use std::fmt;

use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::ser::{self, SerializeMap, Serializer};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::domain::TransferId;

pub const MAX_CHUNK_BYTES: u32 = 256 * 1024;
pub const MAX_CUMULATIVE_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_CURSOR_BYTES: u32 = 64 * 1024;

pub const MAX_CHUNK_PAYLOAD_BYTES: u32 = MAX_CHUNK_BYTES;
pub const MAX_CHUNK_REASSEMBLY_BYTES: u64 = MAX_CUMULATIVE_BYTES;
pub const MAX_CHUNK_CURSOR_BYTES: u32 = MAX_CURSOR_BYTES;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkLimitField {
    ChunkBytes,
    CumulativeBytes,
    CursorBytes,
}

impl fmt::Display for ChunkLimitField {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ChunkBytes => "max_chunk_bytes",
            Self::CumulativeBytes => "max_cumulative_bytes",
            Self::CursorBytes => "max_cursor_bytes",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkLimitsError {
    Zero {
        field: ChunkLimitField,
    },
    ExceedsHardMaximum {
        field: ChunkLimitField,
        declared: u64,
        maximum: u64,
    },
    ChunkExceedsCumulative {
        chunk: u32,
        cumulative: u64,
    },
}

impl fmt::Display for ChunkLimitsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Zero { field } => write!(formatter, "chunk limit {field} must be nonzero"),
            Self::ExceedsHardMaximum {
                field,
                declared,
                maximum,
            } => write!(
                formatter,
                "chunk limit {field} value {declared} exceeds hard maximum {maximum}"
            ),
            Self::ChunkExceedsCumulative { chunk, cumulative } => write!(
                formatter,
                "chunk limit {chunk} exceeds cumulative limit {cumulative}"
            ),
        }
    }
}

impl std::error::Error for ChunkLimitsError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkError {
    Limits(ChunkLimitsError),
    EmptyPayload,
    ChunkTooLarge { declared: u64, maximum: u32 },
    CumulativeOverflow,
    CumulativeTooLarge { declared: u64, maximum: u64 },
    CursorEmpty,
    CursorTooLarge { declared: u64, maximum: u32 },
    TransferIdMismatch,
    IndexMismatch { expected: u32, received: u32 },
    ResumeCursorMismatch,
    CumulativeHashMismatch,
    AlreadyComplete,
    FinalRequired,
    Poisoned,
}

impl fmt::Display for ChunkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Limits(error) => error.fmt(formatter),
            Self::EmptyPayload => formatter.write_str("chunk payload must be nonempty"),
            Self::ChunkTooLarge { declared, maximum } => write!(
                formatter,
                "chunk payload length {declared} exceeds maximum {maximum}"
            ),
            Self::CumulativeOverflow => {
                formatter.write_str("chunk cumulative byte count overflowed")
            }
            Self::CumulativeTooLarge { declared, maximum } => write!(
                formatter,
                "chunk cumulative bytes {declared} exceeds maximum {maximum}"
            ),
            Self::CursorEmpty => formatter.write_str("chunk resume cursor must be nonempty"),
            Self::CursorTooLarge { declared, maximum } => write!(
                formatter,
                "chunk resume cursor length {declared} exceeds maximum {maximum}"
            ),
            Self::TransferIdMismatch => {
                formatter.write_str("chunk transfer id does not match context")
            }
            Self::IndexMismatch { expected, received } => write!(
                formatter,
                "chunk index {received} is not the expected contiguous index {expected}"
            ),
            Self::ResumeCursorMismatch => {
                formatter.write_str("chunk resume cursor does not match context")
            }
            Self::CumulativeHashMismatch => {
                formatter.write_str("chunk cumulative SHA-256 does not match context")
            }
            Self::AlreadyComplete => formatter.write_str("chunk context is already complete"),
            Self::FinalRequired => formatter.write_str("chunk transfer requires a final chunk"),
            Self::Poisoned => formatter.write_str("chunk context is permanently poisoned"),
        }
    }
}

impl std::error::Error for ChunkError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkLimits {
    pub max_chunk_bytes: u32,
    pub max_cumulative_bytes: u64,
    pub max_cursor_bytes: u32,
}

impl ChunkLimits {
    pub const fn v1_default() -> Self {
        Self {
            max_chunk_bytes: MAX_CHUNK_BYTES,
            max_cumulative_bytes: MAX_CUMULATIVE_BYTES,
            max_cursor_bytes: MAX_CURSOR_BYTES,
        }
    }

    pub const fn default_v1() -> Self {
        Self::v1_default()
    }

    pub fn try_new(
        max_chunk_bytes: u32,
        max_cumulative_bytes: u64,
        max_cursor_bytes: u32,
    ) -> Result<Self, ChunkLimitsError> {
        let limits = Self {
            max_chunk_bytes,
            max_cumulative_bytes,
            max_cursor_bytes,
        };
        limits.validate()?;
        Ok(limits)
    }

    pub fn validate(self) -> Result<(), ChunkLimitsError> {
        for (field, value, maximum) in [
            (
                ChunkLimitField::ChunkBytes,
                u64::from(self.max_chunk_bytes),
                u64::from(MAX_CHUNK_BYTES),
            ),
            (
                ChunkLimitField::CumulativeBytes,
                self.max_cumulative_bytes,
                MAX_CUMULATIVE_BYTES,
            ),
            (
                ChunkLimitField::CursorBytes,
                u64::from(self.max_cursor_bytes),
                u64::from(MAX_CURSOR_BYTES),
            ),
        ] {
            if value == 0 {
                return Err(ChunkLimitsError::Zero { field });
            }
            if value > maximum {
                return Err(ChunkLimitsError::ExceedsHardMaximum {
                    field,
                    declared: value,
                    maximum,
                });
            }
        }
        if u64::from(self.max_chunk_bytes) > self.max_cumulative_bytes {
            return Err(ChunkLimitsError::ChunkExceedsCumulative {
                chunk: self.max_chunk_bytes,
                cumulative: self.max_cumulative_bytes,
            });
        }
        Ok(())
    }

    pub fn negotiate(self, peer: Self) -> Result<Self, ChunkLimitsError> {
        self.validate()?;
        peer.validate()?;
        Self::try_new(
            self.max_chunk_bytes.min(peer.max_chunk_bytes),
            self.max_cumulative_bytes.min(peer.max_cumulative_bytes),
            self.max_cursor_bytes.min(peer.max_cursor_bytes),
        )
    }

    pub fn validate_chunk(self, cumulative_before: u64, payload: &[u8]) -> Result<u64, ChunkError> {
        if payload.is_empty() {
            return Err(ChunkError::EmptyPayload);
        }
        let declared = u64::try_from(payload.len()).unwrap_or(u64::MAX);
        if declared > u64::from(self.max_chunk_bytes) {
            return Err(ChunkError::ChunkTooLarge {
                declared,
                maximum: self.max_chunk_bytes,
            });
        }
        let cumulative = cumulative_before
            .checked_add(declared)
            .ok_or(ChunkError::CumulativeOverflow)?;
        if cumulative > self.max_cumulative_bytes {
            return Err(ChunkError::CumulativeTooLarge {
                declared: cumulative,
                maximum: self.max_cumulative_bytes,
            });
        }
        Ok(cumulative)
    }

    pub fn validate_cursor_len(self, length: usize) -> Result<(), ChunkError> {
        let declared = u64::try_from(length).unwrap_or(u64::MAX);
        if declared == 0 {
            return Err(ChunkError::CursorEmpty);
        }
        if declared > u64::from(self.max_cursor_bytes) {
            return Err(ChunkError::CursorTooLarge {
                declared,
                maximum: self.max_cursor_bytes,
            });
        }
        Ok(())
    }
}

impl Default for ChunkLimits {
    fn default() -> Self {
        Self::v1_default()
    }
}

impl Serialize for ChunkLimits {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate().map_err(ser::Error::custom)?;
        let mut map = serializer.serialize_map(Some(3))?;
        map.serialize_entry("max_chunk_bytes", &self.max_chunk_bytes)?;
        map.serialize_entry("max_cumulative_bytes", &self.max_cumulative_bytes)?;
        map.serialize_entry("max_cursor_bytes", &self.max_cursor_bytes)?;
        map.end()
    }
}

impl<'de> Deserialize<'de> for ChunkLimits {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct LimitsVisitor;

        impl<'de> Visitor<'de> for LimitsVisitor {
            type Value = ChunkLimits;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a named ChunkLimits map")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut max_chunk_bytes = None;
                let mut max_cumulative_bytes = None;
                let mut max_cursor_bytes = None;
                while let Some(field) = map.next_key::<ChunkLimitsField>()? {
                    match field {
                        ChunkLimitsField::ChunkBytes => {
                            if max_chunk_bytes.is_some() {
                                return Err(de::Error::duplicate_field("max_chunk_bytes"));
                            }
                            max_chunk_bytes = Some(map.next_value()?);
                        }
                        ChunkLimitsField::CumulativeBytes => {
                            if max_cumulative_bytes.is_some() {
                                return Err(de::Error::duplicate_field("max_cumulative_bytes"));
                            }
                            max_cumulative_bytes = Some(map.next_value()?);
                        }
                        ChunkLimitsField::CursorBytes => {
                            if max_cursor_bytes.is_some() {
                                return Err(de::Error::duplicate_field("max_cursor_bytes"));
                            }
                            max_cursor_bytes = Some(map.next_value()?);
                        }
                    }
                }
                ChunkLimits::try_new(
                    max_chunk_bytes.ok_or_else(|| de::Error::missing_field("max_chunk_bytes"))?,
                    max_cumulative_bytes
                        .ok_or_else(|| de::Error::missing_field("max_cumulative_bytes"))?,
                    max_cursor_bytes.ok_or_else(|| de::Error::missing_field("max_cursor_bytes"))?,
                )
                .map_err(de::Error::custom)
            }
        }

        deserializer.deserialize_map(LimitsVisitor)
    }
}

enum ChunkLimitsField {
    ChunkBytes,
    CumulativeBytes,
    CursorBytes,
}

impl<'de> Deserialize<'de> for ChunkLimitsField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct FieldVisitor;

        impl Visitor<'_> for FieldVisitor {
            type Value = ChunkLimitsField;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a ChunkLimits field name")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "max_chunk_bytes" => Ok(ChunkLimitsField::ChunkBytes),
                    "max_cumulative_bytes" => Ok(ChunkLimitsField::CumulativeBytes),
                    "max_cursor_bytes" => Ok(ChunkLimitsField::CursorBytes),
                    _ => Err(de::Error::unknown_field(
                        value,
                        &[
                            "max_chunk_bytes",
                            "max_cumulative_bytes",
                            "max_cursor_bytes",
                        ],
                    )),
                }
            }
        }

        deserializer.deserialize_identifier(FieldVisitor)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkFrame {
    pub transfer_id: TransferId,
    pub index: u32,
    pub final_chunk: bool,
    pub payload: Vec<u8>,
    pub cumulative_sha256: [u8; 32],
    pub resume_cursor: Option<Vec<u8>>,
}

impl ChunkFrame {
    pub const fn new(
        transfer_id: TransferId,
        index: u32,
        final_chunk: bool,
        payload: Vec<u8>,
        cumulative_sha256: [u8; 32],
        resume_cursor: Option<Vec<u8>>,
    ) -> Self {
        Self {
            transfer_id,
            index,
            final_chunk,
            payload,
            cumulative_sha256,
            resume_cursor,
        }
    }

    fn validate_shape(&self) -> Result<(), ChunkError> {
        if self.payload.is_empty() {
            return Err(ChunkError::EmptyPayload);
        }
        let declared = u64::try_from(self.payload.len()).unwrap_or(u64::MAX);
        if declared > u64::from(MAX_CHUNK_BYTES) {
            return Err(ChunkError::ChunkTooLarge {
                declared,
                maximum: MAX_CHUNK_BYTES,
            });
        }
        if let Some(cursor) = self.resume_cursor.as_deref() {
            let declared = u64::try_from(cursor.len()).unwrap_or(u64::MAX);
            if declared == 0 {
                return Err(ChunkError::CursorEmpty);
            }
            if declared > u64::from(MAX_CURSOR_BYTES) {
                return Err(ChunkError::CursorTooLarge {
                    declared,
                    maximum: MAX_CURSOR_BYTES,
                });
            }
        }
        Ok(())
    }

    pub fn validate(&self, limits: ChunkLimits) -> Result<(), ChunkError> {
        limits.validate().map_err(ChunkError::Limits)?;
        self.validate_shape()?;
        limits.validate_chunk(0, &self.payload)?;
        if let Some(cursor) = self.resume_cursor.as_deref() {
            limits.validate_cursor_len(cursor.len())?;
        }
        Ok(())
    }
}

impl Serialize for ChunkFrame {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(6))?;
        map.serialize_entry("transfer_id", &self.transfer_id)?;
        map.serialize_entry("index", &self.index)?;
        map.serialize_entry("final_chunk", &self.final_chunk)?;
        map.serialize_entry("payload", &BinaryRef(&self.payload))?;
        map.serialize_entry("cumulative_sha256", &BinaryRef(&self.cumulative_sha256))?;
        map.serialize_entry(
            "resume_cursor",
            &OptionalBinaryRef(self.resume_cursor.as_deref()),
        )?;
        map.end()
    }
}

impl<'de> Deserialize<'de> for ChunkFrame {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct FrameVisitor;

        impl<'de> Visitor<'de> for FrameVisitor {
            type Value = ChunkFrame;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a named ChunkFrame map")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut transfer_id = None;
                let mut index = None;
                let mut final_chunk = None;
                let mut payload = None;
                let mut cumulative_sha256 = None;
                let mut resume_cursor = None;

                while let Some(field) = map.next_key::<ChunkFrameField>()? {
                    match field {
                        ChunkFrameField::TransferId => {
                            if transfer_id.is_some() {
                                return Err(de::Error::duplicate_field("transfer_id"));
                            }
                            transfer_id = Some(map.next_value()?);
                        }
                        ChunkFrameField::Index => {
                            if index.is_some() {
                                return Err(de::Error::duplicate_field("index"));
                            }
                            index = Some(map.next_value()?);
                        }
                        ChunkFrameField::FinalChunk => {
                            if final_chunk.is_some() {
                                return Err(de::Error::duplicate_field("final_chunk"));
                            }
                            final_chunk = Some(map.next_value()?);
                        }
                        ChunkFrameField::Payload => {
                            if payload.is_some() {
                                return Err(de::Error::duplicate_field("payload"));
                            }
                            payload = Some(map.next_value::<BinaryBuf>()?.0);
                        }
                        ChunkFrameField::CumulativeSha256 => {
                            if cumulative_sha256.is_some() {
                                return Err(de::Error::duplicate_field("cumulative_sha256"));
                            }
                            let bytes = map.next_value::<BinaryBuf>()?.0;
                            cumulative_sha256 =
                                Some(bytes.try_into().map_err(|bytes: Vec<u8>| {
                                    de::Error::invalid_length(bytes.len(), &"32-byte SHA-256")
                                })?);
                        }
                        ChunkFrameField::ResumeCursor => {
                            if resume_cursor.is_some() {
                                return Err(de::Error::duplicate_field("resume_cursor"));
                            }
                            resume_cursor =
                                Some(map.next_value::<Option<BinaryBuf>>()?.map(|bytes| bytes.0));
                        }
                    }
                }

                let frame = ChunkFrame {
                    transfer_id: transfer_id
                        .ok_or_else(|| de::Error::missing_field("transfer_id"))?,
                    index: index.ok_or_else(|| de::Error::missing_field("index"))?,
                    final_chunk: final_chunk
                        .ok_or_else(|| de::Error::missing_field("final_chunk"))?,
                    payload: payload.ok_or_else(|| de::Error::missing_field("payload"))?,
                    cumulative_sha256: cumulative_sha256
                        .ok_or_else(|| de::Error::missing_field("cumulative_sha256"))?,
                    resume_cursor: resume_cursor
                        .ok_or_else(|| de::Error::missing_field("resume_cursor"))?,
                };
                frame.validate_shape().map_err(de::Error::custom)?;
                Ok(frame)
            }
        }

        deserializer.deserialize_map(FrameVisitor)
    }
}

enum ChunkFrameField {
    TransferId,
    Index,
    FinalChunk,
    Payload,
    CumulativeSha256,
    ResumeCursor,
}

impl<'de> Deserialize<'de> for ChunkFrameField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct FieldVisitor;

        impl Visitor<'_> for FieldVisitor {
            type Value = ChunkFrameField;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a ChunkFrame field name")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "transfer_id" => Ok(ChunkFrameField::TransferId),
                    "index" => Ok(ChunkFrameField::Index),
                    "final_chunk" => Ok(ChunkFrameField::FinalChunk),
                    "payload" => Ok(ChunkFrameField::Payload),
                    "cumulative_sha256" => Ok(ChunkFrameField::CumulativeSha256),
                    "resume_cursor" => Ok(ChunkFrameField::ResumeCursor),
                    _ => Err(de::Error::unknown_field(
                        value,
                        &[
                            "transfer_id",
                            "index",
                            "final_chunk",
                            "payload",
                            "cumulative_sha256",
                            "resume_cursor",
                        ],
                    )),
                }
            }
        }

        deserializer.deserialize_identifier(FieldVisitor)
    }
}

#[derive(Debug)]
pub struct ChunkContext {
    transfer_id: TransferId,
    limits: ChunkLimits,
    resume_cursor: Option<Vec<u8>>,
    next_index: u32,
    cumulative_bytes: u64,
    cumulative_hasher: Sha256,
    complete: bool,
    poisoned: bool,
}

impl ChunkContext {
    pub fn new(
        transfer_id: TransferId,
        limits: ChunkLimits,
        resume_cursor: Option<Vec<u8>>,
    ) -> Result<Self, ChunkError> {
        limits.validate().map_err(ChunkError::Limits)?;
        if let Some(cursor) = resume_cursor.as_deref() {
            limits.validate_cursor_len(cursor.len())?;
        }
        Ok(Self {
            transfer_id,
            limits,
            resume_cursor,
            next_index: 0,
            cumulative_bytes: 0,
            cumulative_hasher: Sha256::new(),
            complete: false,
            poisoned: false,
        })
    }

    pub fn try_new(
        transfer_id: TransferId,
        limits: ChunkLimits,
        resume_cursor: Option<Vec<u8>>,
    ) -> Result<Self, ChunkError> {
        Self::new(transfer_id, limits, resume_cursor)
    }

    pub fn accept(&mut self, frame: &ChunkFrame) -> Result<(), ChunkError> {
        if self.poisoned {
            return Err(ChunkError::Poisoned);
        }
        let result = self.accept_inner(frame);
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }

    fn accept_inner(&mut self, frame: &ChunkFrame) -> Result<(), ChunkError> {
        if self.complete {
            return Err(ChunkError::AlreadyComplete);
        }
        frame.validate(self.limits)?;
        if frame.transfer_id != self.transfer_id {
            return Err(ChunkError::TransferIdMismatch);
        }
        if frame.index != self.next_index {
            return Err(ChunkError::IndexMismatch {
                expected: self.next_index,
                received: frame.index,
            });
        }
        if frame.resume_cursor != self.resume_cursor {
            return Err(ChunkError::ResumeCursorMismatch);
        }

        let cumulative = self
            .limits
            .validate_chunk(self.cumulative_bytes, &frame.payload)?;
        let mut candidate_hasher = self.cumulative_hasher.clone();
        candidate_hasher.update(&frame.payload);
        let candidate_hash: [u8; 32] = candidate_hasher.clone().finalize().into();
        if frame.cumulative_sha256 != candidate_hash {
            return Err(ChunkError::CumulativeHashMismatch);
        }

        self.cumulative_hasher = candidate_hasher;
        self.cumulative_bytes = cumulative;
        self.next_index = self
            .next_index
            .checked_add(1)
            .ok_or(ChunkError::CumulativeOverflow)?;
        self.complete = frame.final_chunk;
        Ok(())
    }

    pub const fn is_complete(&self) -> bool {
        self.complete
    }

    pub const fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    pub const fn next_index(&self) -> u32 {
        self.next_index
    }

    pub const fn cumulative_bytes(&self) -> u64 {
        self.cumulative_bytes
    }

    pub fn require_complete(&self) -> Result<(), ChunkError> {
        if self.poisoned {
            return Err(ChunkError::Poisoned);
        }
        if self.complete {
            Ok(())
        } else {
            Err(ChunkError::FinalRequired)
        }
    }
}

struct BinaryRef<'a>(&'a [u8]);

impl Serialize for BinaryRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_bytes(self.0)
    }
}

struct OptionalBinaryRef<'a>(Option<&'a [u8]>);

impl Serialize for OptionalBinaryRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self.0 {
            Some(bytes) => serializer.serialize_some(&BinaryRef(bytes)),
            None => serializer.serialize_none(),
        }
    }
}

struct BinaryBuf(Vec<u8>);

impl<'de> Deserialize<'de> for BinaryBuf {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct BinaryVisitor;

        impl<'de> Visitor<'de> for BinaryVisitor {
            type Value = BinaryBuf;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("MessagePack binary bytes")
            }

            fn visit_bytes<E>(self, value: &[u8]) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(BinaryBuf(value.to_vec()))
            }

            fn visit_byte_buf<E>(self, value: Vec<u8>) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(BinaryBuf(value))
            }

            fn visit_seq<A>(self, _seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                Err(de::Error::invalid_type(de::Unexpected::Seq, &self))
            }
        }

        deserializer.deserialize_bytes(BinaryVisitor)
    }
}
