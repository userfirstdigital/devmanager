//! Bounded wire-format checks used before orchestration data reaches serde.
//!
//! MessagePack's normal deserializer allocates according to lengths advertised by
//! the input.  Provider orchestration payloads are untrusted at this boundary, so
//! the shape is scanned first without constructing any Rust values.

use std::fmt;
use std::io::{Cursor, Read};

use rmp::decode::read_marker;
use rmp::Marker;
use serde::de::DeserializeOwned;
use serde::Serialize;

/// Maximum encoded size accepted for an orchestration MessagePack document.
pub const MAX_ORCHESTRATION_MSGPACK_BYTES: usize = 64 * 1024;
/// Maximum number of scalar/container values in one orchestration document.
pub const MAX_ORCHESTRATION_MSGPACK_NODES: usize = 512;
/// Maximum nesting depth of arrays/maps.
pub const MAX_ORCHESTRATION_MSGPACK_DEPTH: usize = 16;
/// Maximum string or binary value size inspected before serde allocation.
pub const MAX_ORCHESTRATION_MSGPACK_STRING_BYTES: usize = 64 * 1024;
/// Maximum entries in an array or map.
pub const MAX_ORCHESTRATION_MSGPACK_COLLECTION_ITEMS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgPackPreflightError {
    TooManyBytes,
    TooManyNodes,
    TooDeep,
    StringTooLong,
    CollectionTooLong,
    Truncated,
    InvalidMarker,
    ReservedMarker,
    TrailingBytes,
}

impl fmt::Display for MsgPackPreflightError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::TooManyBytes => "MessagePack document exceeds byte bound",
            Self::TooManyNodes => "MessagePack document exceeds node bound",
            Self::TooDeep => "MessagePack document exceeds nesting-depth bound",
            Self::StringTooLong => "MessagePack string or binary value exceeds bound",
            Self::CollectionTooLong => "MessagePack array or map exceeds entry bound",
            Self::Truncated => "MessagePack document is truncated",
            Self::InvalidMarker => "MessagePack document has an invalid marker",
            Self::ReservedMarker => "MessagePack document contains a reserved marker",
            Self::TrailingBytes => "MessagePack document contains trailing bytes",
        };
        f.write_str(message)
    }
}

impl std::error::Error for MsgPackPreflightError {}

/// Scan a MessagePack document without allocating values.
pub fn preflight_msgpack(bytes: &[u8]) -> Result<(), MsgPackPreflightError> {
    if bytes.len() > MAX_ORCHESTRATION_MSGPACK_BYTES {
        return Err(MsgPackPreflightError::TooManyBytes);
    }
    let mut reader = Cursor::new(bytes);
    let mut state = ScanState { nodes: 0 };
    scan_value(&mut reader, 0, &mut state)?;
    if reader.position() != bytes.len() as u64 {
        return Err(MsgPackPreflightError::TrailingBytes);
    }
    Ok(())
}

/// Encode an orchestration value and verify the resulting document shape.
pub fn encode_orchestration_msgpack<T: Serialize>(
    value: &T,
) -> Result<Vec<u8>, OrchestrationCodecError> {
    let bytes = rmp_serde::to_vec(value).map_err(OrchestrationCodecError::Encode)?;
    preflight_msgpack(&bytes).map_err(OrchestrationCodecError::Preflight)?;
    Ok(bytes)
}

/// Preflight an orchestration document before handing it to `rmp_serde`.
pub fn decode_orchestration_msgpack<T: DeserializeOwned>(
    bytes: &[u8],
) -> Result<T, OrchestrationCodecError> {
    preflight_msgpack(bytes).map_err(OrchestrationCodecError::Preflight)?;
    rmp_serde::from_slice(bytes).map_err(OrchestrationCodecError::Decode)
}

#[derive(Debug)]
pub enum OrchestrationCodecError {
    Encode(rmp_serde::encode::Error),
    Decode(rmp_serde::decode::Error),
    Preflight(MsgPackPreflightError),
}

impl fmt::Display for OrchestrationCodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Encode(error) => write!(f, "orchestration MessagePack encode failed: {error}"),
            Self::Decode(error) => write!(f, "orchestration MessagePack decode failed: {error}"),
            Self::Preflight(error) => {
                write!(f, "orchestration MessagePack preflight failed: {error}")
            }
        }
    }
}

impl std::error::Error for OrchestrationCodecError {}

struct ScanState {
    nodes: usize,
}

fn scan_value(
    reader: &mut Cursor<&[u8]>,
    depth: usize,
    state: &mut ScanState,
) -> Result<(), MsgPackPreflightError> {
    state.nodes = state
        .nodes
        .checked_add(1)
        .ok_or(MsgPackPreflightError::TooManyNodes)?;
    if state.nodes > MAX_ORCHESTRATION_MSGPACK_NODES {
        return Err(MsgPackPreflightError::TooManyNodes);
    }
    let marker = read_marker(reader).map_err(|error| {
        if reader.position() >= reader.get_ref().len() as u64 {
            MsgPackPreflightError::Truncated
        } else {
            let _ = error;
            MsgPackPreflightError::InvalidMarker
        }
    })?;

    match marker {
        Marker::FixPos(_) | Marker::FixNeg(_) | Marker::Null | Marker::False | Marker::True => {}
        Marker::Reserved => return Err(MsgPackPreflightError::ReservedMarker),
        Marker::FixStr(length) => skip_sized(reader, usize::from(length), true)?,
        Marker::Str8 => skip_length_prefixed(reader, 1, true)?,
        Marker::Str16 => skip_length_prefixed(reader, 2, true)?,
        Marker::Str32 => skip_length_prefixed(reader, 4, true)?,
        Marker::Bin8 => skip_length_prefixed(reader, 1, true)?,
        Marker::Bin16 => skip_length_prefixed(reader, 2, true)?,
        Marker::Bin32 => skip_length_prefixed(reader, 4, true)?,
        Marker::F32 | Marker::U32 | Marker::I32 => skip_sized(reader, 4, false)?,
        Marker::F64 | Marker::U64 | Marker::I64 => skip_sized(reader, 8, false)?,
        Marker::U8 | Marker::I8 => skip_sized(reader, 1, false)?,
        Marker::U16 | Marker::I16 => skip_sized(reader, 2, false)?,
        Marker::FixExt1 => skip_sized(reader, 2, false)?,
        Marker::FixExt2 => skip_sized(reader, 3, false)?,
        Marker::FixExt4 => skip_sized(reader, 5, false)?,
        Marker::FixExt8 => skip_sized(reader, 9, false)?,
        Marker::FixExt16 => skip_sized(reader, 17, false)?,
        Marker::Ext8 => skip_extension(reader, 1)?,
        Marker::Ext16 => skip_extension(reader, 2)?,
        Marker::Ext32 => skip_extension(reader, 4)?,
        Marker::FixArray(length) => scan_array(reader, usize::from(length), depth, state)?,
        Marker::Array16 => {
            let length = read_length(reader, 2)?;
            scan_array(reader, length, depth, state)?;
        }
        Marker::Array32 => {
            let length = read_length(reader, 4)?;
            scan_array(reader, length, depth, state)?;
        }
        Marker::FixMap(length) => scan_map(reader, usize::from(length), depth, state)?,
        Marker::Map16 => {
            let length = read_length(reader, 2)?;
            scan_map(reader, length, depth, state)?;
        }
        Marker::Map32 => {
            let length = read_length(reader, 4)?;
            scan_map(reader, length, depth, state)?;
        }
    }
    Ok(())
}

fn scan_array(
    reader: &mut Cursor<&[u8]>,
    length: usize,
    depth: usize,
    state: &mut ScanState,
) -> Result<(), MsgPackPreflightError> {
    check_collection(length)?;
    let child_depth = depth.checked_add(1).ok_or(MsgPackPreflightError::TooDeep)?;
    if child_depth > MAX_ORCHESTRATION_MSGPACK_DEPTH {
        return Err(MsgPackPreflightError::TooDeep);
    }
    for _ in 0..length {
        scan_value(reader, child_depth, state)?;
    }
    Ok(())
}

fn scan_map(
    reader: &mut Cursor<&[u8]>,
    length: usize,
    depth: usize,
    state: &mut ScanState,
) -> Result<(), MsgPackPreflightError> {
    check_collection(length)?;
    let child_depth = depth.checked_add(1).ok_or(MsgPackPreflightError::TooDeep)?;
    if child_depth > MAX_ORCHESTRATION_MSGPACK_DEPTH {
        return Err(MsgPackPreflightError::TooDeep);
    }
    for _ in 0..length {
        scan_value(reader, child_depth, state)?;
        scan_value(reader, child_depth, state)?;
    }
    Ok(())
}

fn check_collection(length: usize) -> Result<(), MsgPackPreflightError> {
    if length > MAX_ORCHESTRATION_MSGPACK_COLLECTION_ITEMS {
        Err(MsgPackPreflightError::CollectionTooLong)
    } else {
        Ok(())
    }
}

fn read_length(reader: &mut Cursor<&[u8]>, width: usize) -> Result<usize, MsgPackPreflightError> {
    let mut bytes = [0u8; 4];
    reader
        .read_exact(&mut bytes[..width])
        .map_err(|_| MsgPackPreflightError::Truncated)?;
    Ok(match width {
        1 => usize::from(bytes[0]),
        2 => usize::from(u16::from_be_bytes([bytes[0], bytes[1]])),
        4 => usize::try_from(u32::from_be_bytes(bytes))
            .map_err(|_| MsgPackPreflightError::CollectionTooLong)?,
        _ => unreachable!("MessagePack length width is fixed by marker"),
    })
}

fn skip_length_prefixed(
    reader: &mut Cursor<&[u8]>,
    width: usize,
    string: bool,
) -> Result<(), MsgPackPreflightError> {
    let length = read_length(reader, width)?;
    skip_sized(reader, length, string)
}

fn skip_extension(reader: &mut Cursor<&[u8]>, width: usize) -> Result<(), MsgPackPreflightError> {
    let length = read_length(reader, width)?;
    skip_sized(reader, 1, false)?;
    skip_sized(reader, length, true)
}

fn skip_sized(
    reader: &mut Cursor<&[u8]>,
    length: usize,
    bounded_string: bool,
) -> Result<(), MsgPackPreflightError> {
    if bounded_string && length > MAX_ORCHESTRATION_MSGPACK_STRING_BYTES {
        return Err(MsgPackPreflightError::StringTooLong);
    }
    let mut remaining = length;
    let mut scratch = [0u8; 256];
    while remaining > 0 {
        let take = remaining.min(scratch.len());
        reader
            .read_exact(&mut scratch[..take])
            .map_err(|_| MsgPackPreflightError::Truncated)?;
        remaining -= take;
    }
    Ok(())
}
