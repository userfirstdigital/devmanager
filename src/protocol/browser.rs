//! Strict, bounded DTOs for the future host-owned browser surface bridge.
//!
//! These types are deliberately transport-only.  They carry host-issued
//! capabilities and observations, but they do not contain a parking window,
//! teardown proof, WebView controller, or process ownership handle.

use crate::domain::id::{BrowserContextId, ClientId, ResourceId, TaskId};
use serde::de::Error as DeError;
use serde::ser::Error as SerError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::num::NonZeroUsize;

pub const MAX_BROWSER_TEXT_BYTES: usize = 16 * 1024;
pub const MAX_BROWSER_TOKEN_BYTES: usize = 512;
pub const MAX_BROWSER_EXECUTABLE_BYTES: usize = 1_024;
pub const MAX_BROWSER_FRAME_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_BROWSER_DIMENSION: u64 = 32_768;
pub const MAX_BROWSER_DPI: u32 = 3_840;
pub const MAX_BROWSER_CLIENT_SEQUENCE: u64 = 1_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserDtoError {
    Zero(&'static str),
    Empty(&'static str),
    TooLarge {
        field: &'static str,
        bytes: usize,
        maximum: usize,
    },
    Invalid(&'static str),
    Overflow(&'static str),
    OutOfRange(&'static str),
    LocalOriginMustBeZero,
}

impl fmt::Display for BrowserDtoError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Zero(field) => write!(formatter, "{field} must be non-zero"),
            Self::Empty(field) => write!(formatter, "{field} must be non-empty"),
            Self::TooLarge {
                field,
                bytes,
                maximum,
            } => write!(formatter, "{field} is {bytes} bytes; maximum is {maximum}"),
            Self::Invalid(field) => write!(formatter, "invalid {field}"),
            Self::Overflow(field) => write!(formatter, "{field} arithmetic overflow"),
            Self::OutOfRange(field) => write!(formatter, "{field} is outside the supported range"),
            Self::LocalOriginMustBeZero => {
                formatter.write_str("local geometry cannot carry a parent or screen origin")
            }
        }
    }
}

impl std::error::Error for BrowserDtoError {}

fn validate_text(value: &str, field: &'static str, maximum: usize) -> Result<(), BrowserDtoError> {
    if value.is_empty() {
        return Err(BrowserDtoError::Empty(field));
    }
    if value.len() > maximum {
        return Err(BrowserDtoError::TooLarge {
            field,
            bytes: value.len(),
            maximum,
        });
    }
    Ok(())
}

macro_rules! nonzero_u64 {
    ($name:ident, $field:literal) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
        pub struct $name(u64);

        impl $name {
            pub const fn initial() -> Self {
                Self(1)
            }

            pub fn new(value: u64) -> Result<Self, BrowserDtoError> {
                if value == 0 {
                    return Err(BrowserDtoError::Zero($field));
                }
                Ok(Self(value))
            }

            pub const fn value(self) -> u64 {
                self.0
            }

            pub fn next(self) -> Result<Self, BrowserDtoError> {
                self.0
                    .checked_add(1)
                    .map(Self)
                    .ok_or(BrowserDtoError::Overflow($field))
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                if self.0 == 0 {
                    return Err(S::Error::custom(BrowserDtoError::Zero($field)));
                }
                serializer.serialize_u64(self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = u64::deserialize(deserializer)?;
                Self::new(value).map_err(D::Error::custom)
            }
        }
    };
}

nonzero_u64!(BrowserRuntimeGeneration, "runtime generation");
nonzero_u64!(BrowserBoundsEpoch, "bounds epoch");
nonzero_u64!(BrowserFocusEpoch, "focus epoch");

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BrowserHostFence {
    pub boot_epoch: u64,
    pub connection_epoch: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BrowserHostFenceWire {
    boot_epoch: u64,
    connection_epoch: u64,
}

impl BrowserHostFence {
    pub fn new(boot_epoch: u64, connection_epoch: u64) -> Result<Self, BrowserDtoError> {
        if boot_epoch == 0 {
            return Err(BrowserDtoError::Zero("host boot epoch"));
        }
        if connection_epoch == 0 {
            return Err(BrowserDtoError::Zero("host connection epoch"));
        }
        Ok(Self {
            boot_epoch,
            connection_epoch,
        })
    }

    pub fn is_nonzero(self) -> bool {
        self.boot_epoch != 0 && self.connection_epoch != 0
    }
}

impl Serialize for BrowserHostFence {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        Self::new(self.boot_epoch, self.connection_epoch).map_err(S::Error::custom)?;
        BrowserHostFenceWire {
            boot_epoch: self.boot_epoch,
            connection_epoch: self.connection_epoch,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for BrowserHostFence {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = BrowserHostFenceWire::deserialize(deserializer)?;
        Self::new(wire.boot_epoch, wire.connection_epoch).map_err(D::Error::custom)
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BrowserWindowHandle(NonZeroUsize);

impl fmt::Debug for BrowserWindowHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BrowserWindowHandle(<redacted>)")
    }
}

impl BrowserWindowHandle {
    pub fn from_raw(raw: u64) -> Result<Self, BrowserDtoError> {
        let raw = usize::try_from(raw).map_err(|_| BrowserDtoError::OutOfRange("window handle"))?;
        NonZeroUsize::new(raw)
            .map(Self)
            .ok_or(BrowserDtoError::Zero("window handle"))
    }

    pub fn from_wire(wire: impl AsRef<str>) -> Result<Self, BrowserDtoError> {
        let wire = wire.as_ref();
        let raw = wire
            .strip_prefix("hwnd:")
            .ok_or(BrowserDtoError::Invalid("opaque window handle"))?;
        if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(BrowserDtoError::Invalid("opaque window handle"));
        }
        Self::from_raw(
            raw.parse::<u64>()
                .map_err(|_| BrowserDtoError::Invalid("opaque window handle"))?,
        )
    }

    pub fn raw_value(&self) -> u64 {
        self.0.get() as u64
    }

    pub fn wire_value(&self) -> String {
        format!("hwnd:{}", self.0.get())
    }
}

impl fmt::Display for BrowserWindowHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BrowserWindowHandle(<redacted>)")
    }
}

impl Serialize for BrowserWindowHandle {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        Self::from_raw(self.raw_value()).map_err(S::Error::custom)?;
        serializer.serialize_str(&self.wire_value())
    }
}

impl<'de> Deserialize<'de> for BrowserWindowHandle {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::from_wire(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct BrowserSurfaceNonce([u8; 16]);

impl fmt::Debug for BrowserSurfaceNonce {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BrowserSurfaceNonce(<redacted>)")
    }
}

impl BrowserSurfaceNonce {
    pub(crate) fn new(bytes: [u8; 16]) -> Result<Self, BrowserDtoError> {
        if bytes.iter().all(|byte| *byte == 0) {
            return Err(BrowserDtoError::Zero("surface nonce"));
        }
        Ok(Self(bytes))
    }

    pub fn is_nonzero(self) -> bool {
        self.0.iter().any(|byte| *byte != 0)
    }

    pub fn as_bytes(self) -> [u8; 16] {
        self.0
    }
}

impl Serialize for BrowserSurfaceNonce {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        Self::new(self.0).map_err(S::Error::custom)?;
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for BrowserSurfaceNonce {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(<[u8; 16]>::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BrowserAttachmentLease(String);

impl fmt::Debug for BrowserAttachmentLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BrowserAttachmentLease(<redacted>)")
    }
}

impl BrowserAttachmentLease {
    pub(crate) fn from_bytes(bytes: [u8; 16]) -> Result<Self, BrowserDtoError> {
        if bytes.iter().all(|byte| *byte == 0) {
            return Err(BrowserDtoError::Zero("attachment lease"));
        }
        let mut value = String::with_capacity(32);
        for byte in bytes {
            use fmt::Write as _;
            let _ = write!(value, "{byte:02x}");
        }
        Ok(Self(value))
    }

    pub fn from_wire(wire: impl Into<String>) -> Result<Self, BrowserDtoError> {
        let wire = wire.into();
        if wire.len() != 32
            || !wire.bytes().all(|byte| byte.is_ascii_hexdigit())
            || wire.bytes().all(|byte| byte == b'0')
        {
            return Err(BrowserDtoError::Invalid("opaque attachment lease"));
        }
        Ok(Self(wire.to_ascii_lowercase()))
    }

    pub fn wire_value(&self) -> &str {
        &self.0
    }
}

/// A host-issued capability for mutations that move or close a native view.
/// It is intentionally not serializable or constructible from wire data.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct BrowserHostRequestLease {
    connection_epoch: u64,
    request_epoch: u64,
    token: BrowserAttachmentLease,
}

impl fmt::Debug for BrowserHostRequestLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BrowserHostRequestLease(<redacted>)")
    }
}

impl BrowserHostRequestLease {
    pub(crate) fn from_parts(
        connection_epoch: u64,
        request_epoch: u64,
        bytes: [u8; 16],
    ) -> Result<Self, BrowserDtoError> {
        if connection_epoch == 0 {
            return Err(BrowserDtoError::Zero("browser host connection epoch"));
        }
        if request_epoch == 0 {
            return Err(BrowserDtoError::Zero("browser host request epoch"));
        }
        Ok(Self {
            connection_epoch,
            request_epoch,
            token: BrowserAttachmentLease::from_bytes(bytes)?,
        })
    }

    pub(crate) const fn connection_epoch(&self) -> u64 {
        self.connection_epoch
    }

    pub(crate) const fn request_epoch(&self) -> u64 {
        self.request_epoch
    }
}

impl Serialize for BrowserAttachmentLease {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        Self::from_wire(self.0.clone()).map_err(S::Error::custom)?;
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for BrowserAttachmentLease {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::from_wire(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct BrowserHostProcessIdentity {
    pub pid: u32,
    pub creation_time_100ns: u64,
    pub executable: String,
}

impl fmt::Debug for BrowserHostProcessIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BrowserHostProcessIdentity(<redacted>)")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BrowserHostProcessIdentityWire {
    pid: u32,
    creation_time_100ns: u64,
    executable: String,
}

impl BrowserHostProcessIdentity {
    pub fn new(
        pid: u32,
        creation_time_100ns: u64,
        executable: impl Into<String>,
    ) -> Result<Self, BrowserDtoError> {
        let executable = executable.into();
        if pid == 0 {
            return Err(BrowserDtoError::Zero("host process PID"));
        }
        if creation_time_100ns == 0 {
            return Err(BrowserDtoError::Zero("host process creation time"));
        }
        validate_text(&executable, "host executable", MAX_BROWSER_EXECUTABLE_BYTES)?;
        Ok(Self {
            pid,
            creation_time_100ns,
            executable,
        })
    }

    pub fn validate(&self) -> Result<(), BrowserDtoError> {
        Self::new(self.pid, self.creation_time_100ns, self.executable.clone()).map(|_| ())
    }
}

impl Serialize for BrowserHostProcessIdentity {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate().map_err(S::Error::custom)?;
        BrowserHostProcessIdentityWire {
            pid: self.pid,
            creation_time_100ns: self.creation_time_100ns,
            executable: self.executable.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for BrowserHostProcessIdentity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = BrowserHostProcessIdentityWire::deserialize(deserializer)?;
        Self::new(wire.pid, wire.creation_time_100ns, wire.executable).map_err(D::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrowserPhysicalPoint {
    pub x: i32,
    pub y: i32,
}

impl BrowserPhysicalPoint {
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrowserPhysicalBounds {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BrowserPhysicalBoundsWire {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

impl BrowserPhysicalBounds {
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Result<Self, BrowserDtoError> {
        if width == 0 {
            return Err(BrowserDtoError::Zero("physical width"));
        }
        if height == 0 {
            return Err(BrowserDtoError::Zero("physical height"));
        }
        if u64::from(width) > MAX_BROWSER_DIMENSION {
            return Err(BrowserDtoError::OutOfRange("physical width"));
        }
        if u64::from(height) > MAX_BROWSER_DIMENSION {
            return Err(BrowserDtoError::OutOfRange("physical height"));
        }
        let right = i64::from(x)
            .checked_add(i64::from(width))
            .ok_or(BrowserDtoError::Overflow("physical right"))?;
        let bottom = i64::from(y)
            .checked_add(i64::from(height))
            .ok_or(BrowserDtoError::Overflow("physical bottom"))?;
        if right > i64::from(i32::MAX) {
            return Err(BrowserDtoError::OutOfRange("physical right"));
        }
        if bottom > i64::from(i32::MAX) {
            return Err(BrowserDtoError::OutOfRange("physical bottom"));
        }
        Ok(Self {
            x,
            y,
            width,
            height,
        })
    }

    pub const fn x(self) -> i32 {
        self.x
    }

    pub const fn y(self) -> i32 {
        self.y
    }

    pub const fn width(self) -> u32 {
        self.width
    }

    pub const fn height(self) -> u32 {
        self.height
    }

    pub fn contains_local_point(self, x: i32, y: i32) -> bool {
        x >= 0 && y >= 0 && (x as u32) < self.width && (y as u32) < self.height
    }
}

impl Serialize for BrowserPhysicalBounds {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        Self::new(self.x, self.y, self.width, self.height).map_err(S::Error::custom)?;
        BrowserPhysicalBoundsWire {
            x: self.x,
            y: self.y,
            width: self.width,
            height: self.height,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for BrowserPhysicalBounds {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = BrowserPhysicalBoundsWire::deserialize(deserializer)?;
        Self::new(wire.x, wire.y, wire.width, wire.height).map_err(D::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrowserDpi {
    pub horizontal: u32,
    pub vertical: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BrowserDpiWire {
    horizontal: u32,
    vertical: u32,
}

impl BrowserDpi {
    pub fn new(horizontal: u32, vertical: u32) -> Result<Self, BrowserDtoError> {
        if horizontal == 0 {
            return Err(BrowserDtoError::Zero("horizontal DPI"));
        }
        if vertical == 0 {
            return Err(BrowserDtoError::Zero("vertical DPI"));
        }
        if horizontal > MAX_BROWSER_DPI || vertical > MAX_BROWSER_DPI {
            return Err(BrowserDtoError::OutOfRange("DPI"));
        }
        Ok(Self {
            horizontal,
            vertical,
        })
    }
}

impl Serialize for BrowserDpi {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        Self::new(self.horizontal, self.vertical).map_err(S::Error::custom)?;
        BrowserDpiWire {
            horizontal: self.horizontal,
            vertical: self.vertical,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for BrowserDpi {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = BrowserDpiWire::deserialize(deserializer)?;
        Self::new(wire.horizontal, wire.vertical).map_err(D::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BrowserCoordinateSpace {
    Local,
    Parent,
    Screen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrowserLogicalBounds {
    pub x: i64,
    pub y: i64,
    pub width: u64,
    pub height: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BrowserLogicalBoundsWire {
    x: i64,
    y: i64,
    width: u64,
    height: u64,
}

impl BrowserLogicalBounds {
    pub fn new(x: i64, y: i64, width: u64, height: u64) -> Result<Self, BrowserDtoError> {
        if width == 0 {
            return Err(BrowserDtoError::Zero("logical width"));
        }
        if height == 0 {
            return Err(BrowserDtoError::Zero("logical height"));
        }
        if width > MAX_BROWSER_DIMENSION {
            return Err(BrowserDtoError::OutOfRange("logical width"));
        }
        if height > MAX_BROWSER_DIMENSION {
            return Err(BrowserDtoError::OutOfRange("logical height"));
        }
        Ok(Self {
            x,
            y,
            width,
            height,
        })
    }
}

impl Serialize for BrowserLogicalBounds {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        Self::new(self.x, self.y, self.width, self.height).map_err(S::Error::custom)?;
        BrowserLogicalBoundsWire {
            x: self.x,
            y: self.y,
            width: self.width,
            height: self.height,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for BrowserLogicalBounds {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = BrowserLogicalBoundsWire::deserialize(deserializer)?;
        Self::new(wire.x, wire.y, wire.width, wire.height).map_err(D::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrowserGeometryInput {
    pub space: BrowserCoordinateSpace,
    pub bounds: BrowserLogicalBounds,
    pub origin: BrowserPhysicalPoint,
    pub dpi: BrowserDpi,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BrowserGeometryInputWire {
    space: BrowserCoordinateSpace,
    bounds: BrowserLogicalBounds,
    origin: BrowserPhysicalPoint,
    dpi: BrowserDpi,
}

impl BrowserGeometryInput {
    pub fn new(
        space: BrowserCoordinateSpace,
        bounds: BrowserLogicalBounds,
        origin: BrowserPhysicalPoint,
        dpi: BrowserDpi,
    ) -> Result<Self, BrowserDtoError> {
        let bounds = BrowserLogicalBounds::new(bounds.x, bounds.y, bounds.width, bounds.height)?;
        let dpi = BrowserDpi::new(dpi.horizontal, dpi.vertical)?;
        if matches!(space, BrowserCoordinateSpace::Local) && (origin.x != 0 || origin.y != 0) {
            return Err(BrowserDtoError::LocalOriginMustBeZero);
        }
        Ok(Self {
            space,
            bounds,
            origin,
            dpi,
        })
    }
}

impl Serialize for BrowserGeometryInput {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        Self::new(self.space, self.bounds, self.origin, self.dpi).map_err(S::Error::custom)?;
        BrowserGeometryInputWire {
            space: self.space,
            bounds: self.bounds,
            origin: self.origin,
            dpi: self.dpi,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for BrowserGeometryInput {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = BrowserGeometryInputWire::deserialize(deserializer)?;
        Self::new(wire.space, wire.bounds, wire.origin, wire.dpi).map_err(D::Error::custom)
    }
}

pub fn browser_logical_to_physical(
    bounds: BrowserLogicalBounds,
    dpi: BrowserDpi,
    origin: BrowserPhysicalPoint,
    space: BrowserCoordinateSpace,
) -> Result<BrowserPhysicalBounds, BrowserDtoError> {
    let bounds = BrowserLogicalBounds::new(bounds.x, bounds.y, bounds.width, bounds.height)?;
    let dpi = BrowserDpi::new(dpi.horizontal, dpi.vertical)?;
    if matches!(space, BrowserCoordinateSpace::Local) && (origin.x != 0 || origin.y != 0) {
        return Err(BrowserDtoError::LocalOriginMustBeZero);
    }

    fn scaled(value: i64, dpi: u32, field: &'static str) -> Result<i64, BrowserDtoError> {
        let scaled = value
            .checked_mul(i64::from(dpi))
            .ok_or(BrowserDtoError::Overflow(field))?;
        let quotient = scaled / 96;
        let remainder = scaled % 96;
        if scaled < 0 && remainder != 0 {
            quotient
                .checked_sub(1)
                .ok_or(BrowserDtoError::Overflow(field))
        } else {
            Ok(quotient)
        }
    }

    fn scaled_dimension(value: u64, dpi: u32, field: &'static str) -> Result<u32, BrowserDtoError> {
        let value = i64::try_from(value).map_err(|_| BrowserDtoError::OutOfRange(field))?;
        let scaled = value
            .checked_mul(i64::from(dpi))
            .ok_or(BrowserDtoError::Overflow(field))?
            .checked_add(95)
            .ok_or(BrowserDtoError::Overflow(field))?
            .checked_div(96)
            .ok_or(BrowserDtoError::Overflow(field))?;
        u32::try_from(scaled).map_err(|_| BrowserDtoError::OutOfRange(field))
    }

    let x = scaled(bounds.x, dpi.horizontal, "physical x")?
        .checked_add(i64::from(origin.x))
        .ok_or(BrowserDtoError::Overflow("physical x"))?;
    let y = scaled(bounds.y, dpi.vertical, "physical y")?
        .checked_add(i64::from(origin.y))
        .ok_or(BrowserDtoError::Overflow("physical y"))?;
    let x = i32::try_from(x).map_err(|_| BrowserDtoError::OutOfRange("physical x"))?;
    let y = i32::try_from(y).map_err(|_| BrowserDtoError::OutOfRange("physical y"))?;
    let width = scaled_dimension(bounds.width, dpi.horizontal, "physical width")?;
    let height = scaled_dimension(bounds.height, dpi.vertical, "physical height")?;
    BrowserPhysicalBounds::new(x, y, width, height)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrowserSurfaceIdentity {
    pub task_id: TaskId,
    pub context_id: BrowserContextId,
    pub resource_id: ResourceId,
}

#[derive(Clone, PartialEq, Eq)]
pub struct BrowserSurfaceDescriptor {
    pub identity: BrowserSurfaceIdentity,
    pub child_hwnd: BrowserWindowHandle,
    pub host_process: BrowserHostProcessIdentity,
    pub host_fence: BrowserHostFence,
    pub runtime_generation: BrowserRuntimeGeneration,
    pub nonce: BrowserSurfaceNonce,
    pub bounds_epoch: BrowserBoundsEpoch,
    pub focus_epoch: BrowserFocusEpoch,
    pub physical_bounds: BrowserPhysicalBounds,
    pub dpi: BrowserDpi,
}

impl fmt::Debug for BrowserSurfaceDescriptor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BrowserSurfaceDescriptor")
            .field("identity", &self.identity)
            .field("child_hwnd", &self.child_hwnd)
            .field("host_process", &self.host_process)
            .field("host_fence", &self.host_fence)
            .field("runtime_generation", &self.runtime_generation)
            .field("nonce", &self.nonce)
            .field("bounds_epoch", &self.bounds_epoch)
            .field("focus_epoch", &self.focus_epoch)
            .field("physical_bounds", &self.physical_bounds)
            .field("dpi", &self.dpi)
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BrowserSurfaceDescriptorWire {
    identity: BrowserSurfaceIdentity,
    child_hwnd: BrowserWindowHandle,
    host_process: BrowserHostProcessIdentity,
    host_fence: BrowserHostFence,
    runtime_generation: BrowserRuntimeGeneration,
    nonce: BrowserSurfaceNonce,
    bounds_epoch: BrowserBoundsEpoch,
    focus_epoch: BrowserFocusEpoch,
    physical_bounds: BrowserPhysicalBounds,
    dpi: BrowserDpi,
}

impl BrowserSurfaceDescriptor {
    pub fn validate(&self) -> Result<(), BrowserDtoError> {
        BrowserWindowHandle::from_raw(self.child_hwnd.raw_value())?;
        self.host_process.validate()?;
        BrowserHostFence::new(self.host_fence.boot_epoch, self.host_fence.connection_epoch)?;
        BrowserRuntimeGeneration::new(self.runtime_generation.value())?;
        BrowserBoundsEpoch::new(self.bounds_epoch.value())?;
        BrowserFocusEpoch::new(self.focus_epoch.value())?;
        BrowserPhysicalBounds::new(
            self.physical_bounds.x(),
            self.physical_bounds.y(),
            self.physical_bounds.width(),
            self.physical_bounds.height(),
        )?;
        BrowserDpi::new(self.dpi.horizontal, self.dpi.vertical)?;
        BrowserSurfaceNonce::new(self.nonce.as_bytes())?;
        Ok(())
    }
}

impl Serialize for BrowserSurfaceDescriptor {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate().map_err(S::Error::custom)?;
        BrowserSurfaceDescriptorWire {
            identity: self.identity,
            child_hwnd: self.child_hwnd.clone(),
            host_process: self.host_process.clone(),
            host_fence: self.host_fence,
            runtime_generation: self.runtime_generation,
            nonce: self.nonce,
            bounds_epoch: self.bounds_epoch,
            focus_epoch: self.focus_epoch,
            physical_bounds: self.physical_bounds,
            dpi: self.dpi,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for BrowserSurfaceDescriptor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = BrowserSurfaceDescriptorWire::deserialize(deserializer)?;
        let descriptor = Self {
            identity: wire.identity,
            child_hwnd: wire.child_hwnd,
            host_process: wire.host_process,
            host_fence: wire.host_fence,
            runtime_generation: wire.runtime_generation,
            nonce: wire.nonce,
            bounds_epoch: wire.bounds_epoch,
            focus_epoch: wire.focus_epoch,
            physical_bounds: wire.physical_bounds,
            dpi: wire.dpi,
        };
        descriptor.validate().map_err(D::Error::custom)?;
        Ok(descriptor)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase", deny_unknown_fields)]
pub enum BrowserSurfaceLifecycle {
    Parked,
    Attached {
        client_id: ClientId,
    },
    Detached {
        client_id: Option<ClientId>,
        crashed: bool,
    },
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserNativeViewReconciliation {
    Healthy,
    Unknown,
}

#[derive(Clone, PartialEq, Eq)]
pub enum BrowserSurfaceInput {
    TrustedClick {
        x: i32,
        y: i32,
        target_token: String,
    },
    Text {
        text: String,
    },
}

impl fmt::Debug for BrowserSurfaceInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TrustedClick { x, y, .. } => formatter
                .debug_struct("TrustedClick")
                .field("x", x)
                .field("y", y)
                .field("target_token", &"<redacted>")
                .finish(),
            Self::Text { .. } => formatter
                .debug_struct("Text")
                .field("text", &"<redacted>")
                .finish(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
enum BrowserSurfaceInputWire {
    TrustedClick {
        x: i32,
        y: i32,
        target_token: String,
    },
    Text {
        text: String,
    },
}

impl BrowserSurfaceInput {
    pub fn trusted_click(
        x: i32,
        y: i32,
        target_token: impl Into<String>,
    ) -> Result<Self, BrowserDtoError> {
        let target_token = target_token.into();
        validate_text(&target_token, "target token", MAX_BROWSER_TOKEN_BYTES)?;
        Ok(Self::TrustedClick { x, y, target_token })
    }

    pub fn text(text: impl Into<String>) -> Result<Self, BrowserDtoError> {
        let text = text.into();
        validate_text(&text, "text input", MAX_BROWSER_TEXT_BYTES)?;
        Ok(Self::Text { text })
    }

    pub fn validate(&self) -> Result<(), BrowserDtoError> {
        match self {
            Self::TrustedClick { target_token, .. } => {
                validate_text(target_token, "target token", MAX_BROWSER_TOKEN_BYTES)
            }
            Self::Text { text } => validate_text(text, "text input", MAX_BROWSER_TEXT_BYTES),
        }
    }
}

impl Serialize for BrowserSurfaceInput {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate().map_err(S::Error::custom)?;
        let wire = match self {
            Self::TrustedClick { x, y, target_token } => BrowserSurfaceInputWire::TrustedClick {
                x: *x,
                y: *y,
                target_token: target_token.clone(),
            },
            Self::Text { text } => BrowserSurfaceInputWire::Text { text: text.clone() },
        };
        wire.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for BrowserSurfaceInput {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = BrowserSurfaceInputWire::deserialize(deserializer)?;
        let input = match wire {
            BrowserSurfaceInputWire::TrustedClick { x, y, target_token } => {
                Self::TrustedClick { x, y, target_token }
            }
            BrowserSurfaceInputWire::Text { text } => Self::Text { text },
        };
        input.validate().map_err(D::Error::custom)?;
        Ok(input)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrowserAttachRequest {
    pub descriptor: BrowserSurfaceDescriptor,
    pub client_id: ClientId,
}

impl BrowserAttachRequest {
    pub fn new(descriptor: BrowserSurfaceDescriptor, client_id: ClientId) -> Self {
        Self {
            descriptor,
            client_id,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct BrowserHostRequest {
    pub(crate) descriptor: BrowserSurfaceDescriptor,
    pub(crate) request_lease: BrowserHostRequestLease,
}

impl fmt::Debug for BrowserHostRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BrowserHostRequest")
            .field("descriptor", &self.descriptor)
            .field("request_lease", &self.request_lease)
            .finish()
    }
}

impl BrowserHostRequest {
    pub(crate) fn new(
        descriptor: BrowserSurfaceDescriptor,
        request_lease: BrowserHostRequestLease,
    ) -> Self {
        Self {
            descriptor,
            request_lease,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserClientRequest {
    pub descriptor: BrowserSurfaceDescriptor,
    pub client_id: ClientId,
    pub attachment_lease: BrowserAttachmentLease,
    pub client_sequence: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BrowserClientRequestWire {
    descriptor: BrowserSurfaceDescriptor,
    client_id: ClientId,
    attachment_lease: BrowserAttachmentLease,
    client_sequence: u64,
}

impl BrowserClientRequest {
    pub fn new(
        descriptor: BrowserSurfaceDescriptor,
        client_id: ClientId,
        attachment_lease: BrowserAttachmentLease,
    ) -> Self {
        Self {
            descriptor,
            client_id,
            attachment_lease,
            client_sequence: 0,
        }
    }

    pub fn validate(&self) -> Result<(), BrowserDtoError> {
        self.descriptor.validate()?;
        BrowserAttachmentLease::from_wire(self.attachment_lease.wire_value().to_string())?;
        if self.client_sequence > MAX_BROWSER_CLIENT_SEQUENCE {
            return Err(BrowserDtoError::OutOfRange("browser client sequence"));
        }
        Ok(())
    }
}

impl Serialize for BrowserClientRequest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate().map_err(S::Error::custom)?;
        BrowserClientRequestWire {
            descriptor: self.descriptor.clone(),
            client_id: self.client_id,
            attachment_lease: self.attachment_lease.clone(),
            client_sequence: self.client_sequence,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for BrowserClientRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = BrowserClientRequestWire::deserialize(deserializer)?;
        let request = Self {
            descriptor: wire.descriptor,
            client_id: wire.client_id,
            attachment_lease: wire.attachment_lease,
            client_sequence: wire.client_sequence,
        };
        request.validate().map_err(D::Error::custom)?;
        Ok(request)
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct BrowserFrame {
    pub frame_id: u64,
    pub bytes: Vec<u8>,
}

impl fmt::Debug for BrowserFrame {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BrowserFrame")
            .field("frame_id", &self.frame_id)
            .field("byte_length", &self.bytes.len())
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BrowserFrameWire {
    frame_id: u64,
    bytes: Vec<u8>,
}

impl BrowserFrame {
    pub fn new(frame_id: u64, bytes: Vec<u8>) -> Result<Self, BrowserDtoError> {
        if frame_id == 0 {
            return Err(BrowserDtoError::Zero("frame ID"));
        }
        if bytes.len() > MAX_BROWSER_FRAME_BYTES {
            return Err(BrowserDtoError::TooLarge {
                field: "frame bytes",
                bytes: bytes.len(),
                maximum: MAX_BROWSER_FRAME_BYTES,
            });
        }
        Ok(Self { frame_id, bytes })
    }
}

impl Serialize for BrowserFrame {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        Self::new(self.frame_id, self.bytes.clone()).map_err(S::Error::custom)?;
        BrowserFrameWire {
            frame_id: self.frame_id,
            bytes: self.bytes.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for BrowserFrame {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = BrowserFrameWire::deserialize(deserializer)?;
        Self::new(wire.frame_id, wire.bytes).map_err(D::Error::custom)
    }
}

pub const MAX_BROWSER_PROJECTION_FPS: u32 = 8;
pub const MAX_BROWSER_PROJECTION_BYTES_PER_SECOND: u64 = 512 * 1024;
pub const MAX_BROWSER_TITLE_BYTES: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BrowserSecurityState {
    Secure,
    Insecure,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BrowserInteractionMode {
    Observe,
    Interact,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BrowserFrameKind {
    Full,
    Tile,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrowserTabProjection {
    pub tab_id: crate::domain::id::BrowserTabId,
    pub title: String,
    pub url: String,
    pub kind: crate::domain::browser::BrowserTabKind,
    pub security: BrowserSecurityState,
    pub loading: bool,
    pub error: Option<String>,
}

impl BrowserTabProjection {
    pub fn validate(&self) -> Result<(), BrowserDtoError> {
        crate::domain::browser::browser_wire_committed_url(&self.url)
            .map_err(|_| BrowserDtoError::Invalid("shareable tab url"))?;
        if self.title.len() > MAX_BROWSER_TITLE_BYTES {
            return Err(BrowserDtoError::TooLarge {
                field: "tab title",
                bytes: self.title.len(),
                maximum: MAX_BROWSER_TITLE_BYTES,
            });
        }
        if let Some(error) = &self.error {
            validate_text(error, "tab error", MAX_BROWSER_TEXT_BYTES)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserProjectionMeta {
    pub task_id: TaskId,
    pub context_id: BrowserContextId,
    pub generation: BrowserRuntimeGeneration,
    pub bounds_epoch: BrowserBoundsEpoch,
    pub focus_epoch: BrowserFocusEpoch,
    pub frame_id: u64,
    pub selected_tab_id: Option<crate::domain::id::BrowserTabId>,
    pub tabs: Vec<BrowserTabProjection>,
    pub progress: Option<String>,
    pub interaction_mode: BrowserInteractionMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BrowserProjectionMetaWire {
    task_id: TaskId,
    context_id: BrowserContextId,
    generation: BrowserRuntimeGeneration,
    bounds_epoch: BrowserBoundsEpoch,
    focus_epoch: BrowserFocusEpoch,
    frame_id: u64,
    selected_tab_id: Option<crate::domain::id::BrowserTabId>,
    tabs: Vec<BrowserTabProjection>,
    progress: Option<String>,
    interaction_mode: BrowserInteractionMode,
}

impl BrowserProjectionMeta {
    pub fn validate(&self) -> Result<(), BrowserDtoError> {
        BrowserRuntimeGeneration::new(self.generation.value())?;
        BrowserBoundsEpoch::new(self.bounds_epoch.value())?;
        BrowserFocusEpoch::new(self.focus_epoch.value())?;
        if self.frame_id == 0 {
            return Err(BrowserDtoError::Zero("projection frame ID"));
        }
        if self.tabs.len() as u32 > MAX_BROWSER_CLIENT_SEQUENCE.min(32) as u32 {
            return Err(BrowserDtoError::OutOfRange("projection tab count"));
        }
        for tab in &self.tabs {
            tab.validate()?;
        }
        if let Some(selected) = self.selected_tab_id {
            if !self.tabs.iter().any(|tab| tab.tab_id == selected) {
                return Err(BrowserDtoError::Invalid("selected tab"));
            }
        }
        if let Some(progress) = &self.progress {
            validate_text(progress, "projection progress", MAX_BROWSER_TEXT_BYTES)?;
        }
        Ok(())
    }
}

impl Serialize for BrowserProjectionMeta {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate().map_err(S::Error::custom)?;
        BrowserProjectionMetaWire {
            task_id: self.task_id,
            context_id: self.context_id,
            generation: self.generation,
            bounds_epoch: self.bounds_epoch,
            focus_epoch: self.focus_epoch,
            frame_id: self.frame_id,
            selected_tab_id: self.selected_tab_id,
            tabs: self.tabs.clone(),
            progress: self.progress.clone(),
            interaction_mode: self.interaction_mode,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for BrowserProjectionMeta {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = BrowserProjectionMetaWire::deserialize(deserializer)?;
        let meta = Self {
            task_id: wire.task_id,
            context_id: wire.context_id,
            generation: wire.generation,
            bounds_epoch: wire.bounds_epoch,
            focus_epoch: wire.focus_epoch,
            frame_id: wire.frame_id,
            selected_tab_id: wire.selected_tab_id,
            tabs: wire.tabs,
            progress: wire.progress,
            interaction_mode: wire.interaction_mode,
        };
        meta.validate().map_err(D::Error::custom)?;
        Ok(meta)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserProjectionFrame {
    pub frame_id: u64,
    pub kind: BrowserFrameKind,
    pub generation: BrowserRuntimeGeneration,
    pub bounds_epoch: BrowserBoundsEpoch,
    pub bytes: Vec<u8>,
}

impl BrowserProjectionFrame {
    pub fn new(
        frame_id: u64,
        kind: BrowserFrameKind,
        generation: BrowserRuntimeGeneration,
        bounds_epoch: BrowserBoundsEpoch,
        bytes: Vec<u8>,
    ) -> Result<Self, BrowserDtoError> {
        if frame_id == 0 {
            return Err(BrowserDtoError::Zero("projection frame ID"));
        }
        BrowserRuntimeGeneration::new(generation.value())?;
        BrowserBoundsEpoch::new(bounds_epoch.value())?;
        if bytes.len() > MAX_BROWSER_FRAME_BYTES {
            return Err(BrowserDtoError::TooLarge {
                field: "projection frame bytes",
                bytes: bytes.len(),
                maximum: MAX_BROWSER_FRAME_BYTES,
            });
        }
        Ok(Self {
            frame_id,
            kind,
            generation,
            bounds_epoch,
            bytes,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BrowserRemoteInputKind {
    Pointer,
    Touch,
    Keyboard,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserRemoteInput {
    pub frame_id: u64,
    pub generation: BrowserRuntimeGeneration,
    pub bounds_epoch: BrowserBoundsEpoch,
    pub focus_epoch: BrowserFocusEpoch,
    pub kind: BrowserRemoteInputKind,
    pub x: i32,
    pub y: i32,
    pub content_width: u32,
    pub content_height: u32,
    pub scale: u32,
}

impl BrowserRemoteInput {
    pub fn validate(&self) -> Result<(), BrowserDtoError> {
        if self.frame_id == 0 {
            return Err(BrowserDtoError::Zero("input frame ID"));
        }
        BrowserRuntimeGeneration::new(self.generation.value())?;
        BrowserBoundsEpoch::new(self.bounds_epoch.value())?;
        BrowserFocusEpoch::new(self.focus_epoch.value())?;
        if self.content_width == 0 || self.content_height == 0 || self.scale == 0 {
            return Err(BrowserDtoError::Zero("input content scale"));
        }
        if u64::from(self.content_width) > MAX_BROWSER_DIMENSION
            || u64::from(self.content_height) > MAX_BROWSER_DIMENSION
        {
            return Err(BrowserDtoError::OutOfRange("input content bounds"));
        }
        Ok(())
    }

    pub fn mapped_point(&self) -> Result<(i32, i32), BrowserDtoError> {
        self.validate()?;
        let x = i64::from(self.x)
            .checked_mul(i64::from(self.scale))
            .ok_or(BrowserDtoError::Overflow("mapped x"))?
            / 96;
        let y = i64::from(self.y)
            .checked_mul(i64::from(self.scale))
            .ok_or(BrowserDtoError::Overflow("mapped y"))?
            / 96;
        let x = i32::try_from(x).map_err(|_| BrowserDtoError::OutOfRange("mapped x"))?;
        let y = i32::try_from(y).map_err(|_| BrowserDtoError::OutOfRange("mapped y"))?;
        if x < 0
            || y < 0
            || (x as u32) >= self.content_width
            || (y as u32) >= self.content_height
        {
            return Err(BrowserDtoError::OutOfRange("mapped point"));
        }
        Ok((x, y))
    }
}
