use std::fmt;
use std::io::Cursor;

use rmp::Marker;
use serde::de::{self, DeserializeOwned, MapAccess, SeqAccess, Visitor};
use serde::ser::{self, SerializeMap};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use uuid::{Uuid, Variant};

use crate::domain::ClientId;

use super::frame::MAX_PHYSICAL_FRAME_BYTES;
use super::{
    CapabilitySet, FrameLimits, FrameLimitsError, ProtocolVersion, ReconnectGrant,
    VersionNegotiationError,
};

pub const MAX_CLIENT_BUILD_BYTES: u32 = 128;
pub const MAX_MESSAGEPACK_DEPTH: u16 = 32;
pub const MAX_MESSAGEPACK_COLLECTION_ITEMS: u32 = 1_000;
pub const MAX_MESSAGEPACK_VALUES: u32 = 65_536;
const MESSAGEPACK_STACK_SLOTS: usize = MAX_MESSAGEPACK_DEPTH as usize + 1;

/// Domain-separated SHA-256 binding for a normalized named profile.
pub const PROFILE_FINGERPRINT_DOMAIN: &[u8] = b"devmanager.pipe.v1\0";

/// Exact 32-byte profile binding used by pipe endpoints and Hello documents.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProfileFingerprint([u8; 32]);

impl ProfileFingerprint {
    /// Hash an already-normalized named profile segment.
    pub fn hash_normalized(normalized: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(PROFILE_FINGERPRINT_DOMAIN);
        hasher.update(normalized.as_bytes());
        Self(hasher.finalize().into())
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        let mut hex = String::with_capacity(64);
        for byte in self.0 {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
        }
        hex
    }
}

impl fmt::Debug for ProfileFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ProfileFingerprint")
            .field(&self.to_hex())
            .finish()
    }
}

impl Serialize for ProfileFingerprint {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_bytes(&self.0)
    }
}

impl<'de> Deserialize<'de> for ProfileFingerprint {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct FingerprintVisitor;

        impl<'de> Visitor<'de> for FingerprintVisitor {
            type Value = ProfileFingerprint;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a 32-byte profile fingerprint")
            }

            fn visit_bytes<E>(self, value: &[u8]) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                let bytes: [u8; 32] = value
                    .try_into()
                    .map_err(|_| de::Error::invalid_length(value.len(), &self))?;
                Ok(ProfileFingerprint::from_bytes(bytes))
            }

            fn visit_byte_buf<E>(self, value: Vec<u8>) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                self.visit_bytes(&value)
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut bytes = [0u8; 32];
                for (index, slot) in bytes.iter_mut().enumerate() {
                    *slot = seq
                        .next_element()?
                        .ok_or_else(|| de::Error::invalid_length(index, &self))?;
                }
                if seq.next_element::<u8>()?.is_some() {
                    return Err(de::Error::invalid_length(33, &self));
                }
                Ok(ProfileFingerprint::from_bytes(bytes))
            }
        }

        deserializer.deserialize_bytes(FingerprintVisitor)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientBuildError {
    Empty,
    TooLong { declared: u64, maximum: u32 },
}

impl std::fmt::Display for ClientBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "client build identifier must be nonempty"),
            Self::TooLong { declared, maximum } => write!(
                f,
                "client build identifier length {declared} exceeds maximum {maximum}"
            ),
        }
    }
}

impl std::error::Error for ClientBuildError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientHelloError {
    Build(ClientBuildError),
    FrameLimits(FrameLimitsError),
    Version(VersionNegotiationError),
}

impl std::fmt::Display for ClientHelloError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Build(error) => error.fmt(f),
            Self::FrameLimits(error) => error.fmt(f),
            Self::Version(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for ClientHelloError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Build(error) => Some(error),
            Self::FrameLimits(error) => Some(error),
            Self::Version(error) => Some(error),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientHello {
    pub protocol_major: u16,
    pub protocol_minor: u16,
    pub client_build: String,
    pub client_id: ClientId,
    pub profile_fingerprint: ProfileFingerprint,
    pub requested: CapabilitySet,
    pub limits: FrameLimits,
    pub reconnect_grant: Option<ReconnectGrant>,
}

impl Serialize for ClientHello {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate().map_err(ser::Error::custom)?;
        let mut map =
            serializer.serialize_map(Some(7 + usize::from(self.reconnect_grant.is_some())))?;
        map.serialize_entry("protocol_major", &self.protocol_major)?;
        map.serialize_entry("protocol_minor", &self.protocol_minor)?;
        map.serialize_entry("client_build", &self.client_build)?;
        map.serialize_entry("client_id", &self.client_id)?;
        map.serialize_entry("profile_fingerprint", &self.profile_fingerprint)?;
        map.serialize_entry("requested", &self.requested)?;
        map.serialize_entry("limits", &self.limits)?;
        if let Some(grant) = &self.reconnect_grant {
            map.serialize_entry("reconnect_grant", grant)?;
        }
        map.end()
    }
}

const CLIENT_HELLO_FIELDS: &[&str] = &[
    "protocol_major",
    "protocol_minor",
    "client_build",
    "client_id",
    "profile_fingerprint",
    "requested",
    "limits",
    "reconnect_grant",
];

enum ClientHelloField {
    ProtocolMajor,
    ProtocolMinor,
    ClientBuild,
    ClientId,
    ProfileFingerprint,
    Requested,
    Limits,
    ReconnectGrant,
}

impl<'de> Deserialize<'de> for ClientHelloField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct FieldVisitor;

        impl Visitor<'_> for FieldVisitor {
            type Value = ClientHelloField;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a ClientHello field name")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "protocol_major" => Ok(ClientHelloField::ProtocolMajor),
                    "protocol_minor" => Ok(ClientHelloField::ProtocolMinor),
                    "client_build" => Ok(ClientHelloField::ClientBuild),
                    "client_id" => Ok(ClientHelloField::ClientId),
                    "profile_fingerprint" => Ok(ClientHelloField::ProfileFingerprint),
                    "requested" => Ok(ClientHelloField::Requested),
                    "limits" => Ok(ClientHelloField::Limits),
                    "reconnect_grant" => Ok(ClientHelloField::ReconnectGrant),
                    _ => Err(de::Error::unknown_field(value, CLIENT_HELLO_FIELDS)),
                }
            }
        }

        deserializer.deserialize_identifier(FieldVisitor)
    }
}

impl<'de> Deserialize<'de> for ClientHello {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ClientHelloVisitor;

        impl<'de> Visitor<'de> for ClientHelloVisitor {
            type Value = ClientHello;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a named ClientHello map")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut protocol_major = None;
                let mut protocol_minor = None;
                let mut client_build = None;
                let mut client_id = None;
                let mut profile_fingerprint = None;
                let mut requested = None;
                let mut limits = None;
                let mut reconnect_grant = None;

                while let Some(field) = map.next_key()? {
                    match field {
                        ClientHelloField::ProtocolMajor => {
                            if protocol_major.is_some() {
                                return Err(de::Error::duplicate_field("protocol_major"));
                            }
                            protocol_major = Some(map.next_value()?);
                        }
                        ClientHelloField::ProtocolMinor => {
                            if protocol_minor.is_some() {
                                return Err(de::Error::duplicate_field("protocol_minor"));
                            }
                            protocol_minor = Some(map.next_value()?);
                        }
                        ClientHelloField::ClientBuild => {
                            if client_build.is_some() {
                                return Err(de::Error::duplicate_field("client_build"));
                            }
                            client_build = Some(map.next_value()?);
                        }
                        ClientHelloField::ClientId => {
                            if client_id.is_some() {
                                return Err(de::Error::duplicate_field("client_id"));
                            }
                            client_id = Some(map.next_value()?);
                        }
                        ClientHelloField::ProfileFingerprint => {
                            if profile_fingerprint.is_some() {
                                return Err(de::Error::duplicate_field("profile_fingerprint"));
                            }
                            profile_fingerprint = Some(map.next_value()?);
                        }
                        ClientHelloField::Requested => {
                            if requested.is_some() {
                                return Err(de::Error::duplicate_field("requested"));
                            }
                            requested = Some(map.next_value()?);
                        }
                        ClientHelloField::Limits => {
                            if limits.is_some() {
                                return Err(de::Error::duplicate_field("limits"));
                            }
                            limits = Some(map.next_value()?);
                        }
                        ClientHelloField::ReconnectGrant => {
                            if reconnect_grant.is_some() {
                                return Err(de::Error::duplicate_field("reconnect_grant"));
                            }
                            reconnect_grant = Some(map.next_value()?);
                        }
                    }
                }

                let hello = ClientHello {
                    protocol_major: protocol_major
                        .ok_or_else(|| de::Error::missing_field("protocol_major"))?,
                    protocol_minor: protocol_minor
                        .ok_or_else(|| de::Error::missing_field("protocol_minor"))?,
                    client_build: client_build
                        .ok_or_else(|| de::Error::missing_field("client_build"))?,
                    client_id: client_id.ok_or_else(|| de::Error::missing_field("client_id"))?,
                    profile_fingerprint: profile_fingerprint
                        .ok_or_else(|| de::Error::missing_field("profile_fingerprint"))?,
                    requested: requested.ok_or_else(|| de::Error::missing_field("requested"))?,
                    limits: limits.ok_or_else(|| de::Error::missing_field("limits"))?,
                    reconnect_grant,
                };
                hello.validate().map_err(de::Error::custom)?;
                Ok(hello)
            }
        }

        deserializer.deserialize_map(ClientHelloVisitor)
    }
}

impl ClientHello {
    pub fn new(
        client_build: impl Into<String>,
        client_id: ClientId,
        profile_fingerprint: ProfileFingerprint,
        requested: CapabilitySet,
        limits: FrameLimits,
    ) -> Result<Self, ClientHelloError> {
        let hello = Self {
            protocol_major: super::PROTOCOL_MAJOR,
            protocol_minor: super::PROTOCOL_MINOR,
            client_build: client_build.into(),
            client_id,
            profile_fingerprint,
            requested,
            limits,
            reconnect_grant: None,
        };
        hello.validate()?;
        Ok(hello)
    }

    pub fn new_with_reconnect_grant(
        client_build: impl Into<String>,
        client_id: ClientId,
        profile_fingerprint: ProfileFingerprint,
        requested: CapabilitySet,
        limits: FrameLimits,
        reconnect_grant: Option<ReconnectGrant>,
    ) -> Result<Self, ClientHelloError> {
        let mut hello = Self::new(
            client_build,
            client_id,
            profile_fingerprint,
            requested,
            limits,
        )?;
        hello.reconnect_grant = reconnect_grant;
        Ok(hello)
    }

    pub fn validate(&self) -> Result<(), ClientHelloError> {
        validate_client_build(&self.client_build).map_err(ClientHelloError::Build)?;
        self.limits
            .validate_offer()
            .map_err(ClientHelloError::FrameLimits)
    }

    pub fn negotiate(
        &self,
        supported: CapabilitySet,
        local_limits: FrameLimits,
    ) -> Result<NegotiatedParameters, ClientHelloError> {
        self.validate()?;
        let version = ProtocolVersion::current()
            .negotiate(ProtocolVersion::new(
                self.protocol_major,
                self.protocol_minor,
            ))
            .map_err(ClientHelloError::Version)?;
        let limits = local_limits
            .negotiate(self.limits)
            .map_err(ClientHelloError::FrameLimits)?;
        Ok(NegotiatedParameters {
            version,
            client_id: self.client_id,
            capabilities: supported.intersection(self.requested),
            limits,
        })
    }
}

/// Validated handshake result shared by ClientHello negotiation and ServerHello.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NegotiatedParameters {
    pub version: ProtocolVersion,
    pub client_id: ClientId,
    pub capabilities: CapabilitySet,
    pub limits: FrameLimits,
}

/// Personal prompt library frames ride the existing owner-device session.
/// This is not a Connect persistence or upload DTO.
pub fn personal_prompt_library_granted(granted: CapabilitySet) -> bool {
    granted.grants_personal_prompt_library()
}

pub const MAX_SERVER_BUILD_BYTES: u32 = MAX_CLIENT_BUILD_BYTES;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerBuildError {
    Empty,
    TooLong { declared: u64, maximum: u32 },
}

impl std::fmt::Display for ServerBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "server build identifier must be nonempty"),
            Self::TooLong { declared, maximum } => write!(
                f,
                "server build identifier length {declared} exceeds maximum {maximum}"
            ),
        }
    }
}

impl std::error::Error for ServerBuildError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerHelloError {
    Build(ServerBuildError),
    FrameLimits(FrameLimitsError),
    InvalidUuid { field: &'static str },
}

impl std::fmt::Display for ServerHelloError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Build(error) => error.fmt(f),
            Self::FrameLimits(error) => error.fmt(f),
            Self::InvalidUuid { field } => {
                write!(f, "server hello field {field} must be a UUIDv7")
            }
        }
    }
}

impl std::error::Error for ServerHelloError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Build(error) => Some(error),
            Self::FrameLimits(error) => Some(error),
            Self::InvalidUuid { .. } => None,
        }
    }
}

/// Strict named-map ServerHello returned after a successful ClientHello negotiation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerHello {
    pub protocol_major: u16,
    pub protocol_minor: u16,
    pub server_build: String,
    pub host_boot_id: Uuid,
    pub connection_id: Uuid,
    pub profile_fingerprint: ProfileFingerprint,
    pub granted: CapabilitySet,
    pub limits: FrameLimits,
    pub reconnect_grant: Option<ReconnectGrant>,
}

impl Serialize for ServerHello {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate().map_err(ser::Error::custom)?;
        let mut map =
            serializer.serialize_map(Some(8 + usize::from(self.reconnect_grant.is_some())))?;
        map.serialize_entry("protocol_major", &self.protocol_major)?;
        map.serialize_entry("protocol_minor", &self.protocol_minor)?;
        map.serialize_entry("server_build", &self.server_build)?;
        map.serialize_entry("host_boot_id", &self.host_boot_id)?;
        map.serialize_entry("connection_id", &self.connection_id)?;
        map.serialize_entry("profile_fingerprint", &self.profile_fingerprint)?;
        map.serialize_entry("granted", &self.granted)?;
        map.serialize_entry("limits", &self.limits)?;
        if let Some(grant) = &self.reconnect_grant {
            map.serialize_entry("reconnect_grant", grant)?;
        }
        map.end()
    }
}

const SERVER_HELLO_FIELDS: &[&str] = &[
    "protocol_major",
    "protocol_minor",
    "server_build",
    "host_boot_id",
    "connection_id",
    "profile_fingerprint",
    "granted",
    "limits",
    "reconnect_grant",
];

enum ServerHelloField {
    ProtocolMajor,
    ProtocolMinor,
    ServerBuild,
    HostBootId,
    ConnectionId,
    ProfileFingerprint,
    Granted,
    Limits,
    ReconnectGrant,
}

impl<'de> Deserialize<'de> for ServerHelloField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct FieldVisitor;

        impl Visitor<'_> for FieldVisitor {
            type Value = ServerHelloField;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a ServerHello field name")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "protocol_major" => Ok(ServerHelloField::ProtocolMajor),
                    "protocol_minor" => Ok(ServerHelloField::ProtocolMinor),
                    "server_build" => Ok(ServerHelloField::ServerBuild),
                    "host_boot_id" => Ok(ServerHelloField::HostBootId),
                    "connection_id" => Ok(ServerHelloField::ConnectionId),
                    "profile_fingerprint" => Ok(ServerHelloField::ProfileFingerprint),
                    "granted" => Ok(ServerHelloField::Granted),
                    "limits" => Ok(ServerHelloField::Limits),
                    "reconnect_grant" => Ok(ServerHelloField::ReconnectGrant),
                    _ => Err(de::Error::unknown_field(value, SERVER_HELLO_FIELDS)),
                }
            }
        }

        deserializer.deserialize_identifier(FieldVisitor)
    }
}

impl<'de> Deserialize<'de> for ServerHello {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ServerHelloVisitor;

        impl<'de> Visitor<'de> for ServerHelloVisitor {
            type Value = ServerHello;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a named ServerHello map")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut protocol_major = None;
                let mut protocol_minor = None;
                let mut server_build = None;
                let mut host_boot_id = None;
                let mut connection_id = None;
                let mut profile_fingerprint = None;
                let mut granted = None;
                let mut limits = None;
                let mut reconnect_grant = None;

                while let Some(field) = map.next_key()? {
                    match field {
                        ServerHelloField::ProtocolMajor => {
                            if protocol_major.is_some() {
                                return Err(de::Error::duplicate_field("protocol_major"));
                            }
                            protocol_major = Some(map.next_value()?);
                        }
                        ServerHelloField::ProtocolMinor => {
                            if protocol_minor.is_some() {
                                return Err(de::Error::duplicate_field("protocol_minor"));
                            }
                            protocol_minor = Some(map.next_value()?);
                        }
                        ServerHelloField::ServerBuild => {
                            if server_build.is_some() {
                                return Err(de::Error::duplicate_field("server_build"));
                            }
                            server_build = Some(map.next_value()?);
                        }
                        ServerHelloField::HostBootId => {
                            if host_boot_id.is_some() {
                                return Err(de::Error::duplicate_field("host_boot_id"));
                            }
                            host_boot_id = Some(map.next_value()?);
                        }
                        ServerHelloField::ConnectionId => {
                            if connection_id.is_some() {
                                return Err(de::Error::duplicate_field("connection_id"));
                            }
                            connection_id = Some(map.next_value()?);
                        }
                        ServerHelloField::ProfileFingerprint => {
                            if profile_fingerprint.is_some() {
                                return Err(de::Error::duplicate_field("profile_fingerprint"));
                            }
                            profile_fingerprint = Some(map.next_value()?);
                        }
                        ServerHelloField::Granted => {
                            if granted.is_some() {
                                return Err(de::Error::duplicate_field("granted"));
                            }
                            granted = Some(map.next_value()?);
                        }
                        ServerHelloField::Limits => {
                            if limits.is_some() {
                                return Err(de::Error::duplicate_field("limits"));
                            }
                            limits = Some(map.next_value()?);
                        }
                        ServerHelloField::ReconnectGrant => {
                            if reconnect_grant.is_some() {
                                return Err(de::Error::duplicate_field("reconnect_grant"));
                            }
                            reconnect_grant = Some(map.next_value()?);
                        }
                    }
                }

                let hello = ServerHello {
                    protocol_major: protocol_major
                        .ok_or_else(|| de::Error::missing_field("protocol_major"))?,
                    protocol_minor: protocol_minor
                        .ok_or_else(|| de::Error::missing_field("protocol_minor"))?,
                    server_build: server_build
                        .ok_or_else(|| de::Error::missing_field("server_build"))?,
                    host_boot_id: host_boot_id
                        .ok_or_else(|| de::Error::missing_field("host_boot_id"))?,
                    connection_id: connection_id
                        .ok_or_else(|| de::Error::missing_field("connection_id"))?,
                    profile_fingerprint: profile_fingerprint
                        .ok_or_else(|| de::Error::missing_field("profile_fingerprint"))?,
                    granted: granted.ok_or_else(|| de::Error::missing_field("granted"))?,
                    limits: limits.ok_or_else(|| de::Error::missing_field("limits"))?,
                    reconnect_grant,
                };
                hello.validate().map_err(de::Error::custom)?;
                Ok(hello)
            }
        }

        deserializer.deserialize_map(ServerHelloVisitor)
    }
}

impl ServerHello {
    pub fn from_negotiated(
        server_build: impl Into<String>,
        host_boot_id: Uuid,
        profile_fingerprint: ProfileFingerprint,
        negotiated: NegotiatedParameters,
    ) -> Result<Self, ServerHelloError> {
        let hello = Self {
            protocol_major: negotiated.version.major,
            protocol_minor: negotiated.version.minor,
            server_build: server_build.into(),
            host_boot_id,
            connection_id: Uuid::now_v7(),
            profile_fingerprint,
            granted: negotiated.capabilities,
            limits: negotiated.limits,
            reconnect_grant: None,
        };
        hello.validate()?;
        Ok(hello)
    }

    pub fn validate(&self) -> Result<(), ServerHelloError> {
        validate_server_build(&self.server_build).map_err(ServerHelloError::Build)?;
        self.limits
            .validate_offer()
            .map_err(ServerHelloError::FrameLimits)?;
        validate_uuid_v7(self.host_boot_id, "host_boot_id")?;
        validate_uuid_v7(self.connection_id, "connection_id")?;
        Ok(())
    }
}

fn validate_uuid_v7(value: Uuid, field: &'static str) -> Result<(), ServerHelloError> {
    if value.get_version_num() != 7 || value.get_variant() != Variant::RFC4122 {
        return Err(ServerHelloError::InvalidUuid { field });
    }
    Ok(())
}

#[cfg(test)]
mod uuid_v7_tests {
    use super::validate_uuid_v7;
    use uuid::Uuid;

    #[test]
    fn server_hello_uuid_requires_version_7_and_rfc4122_variant() {
        let ok = Uuid::now_v7();
        assert!(validate_uuid_v7(ok, "connection_id").is_ok());
        let nil = Uuid::nil();
        assert!(validate_uuid_v7(nil, "connection_id").is_err());
    }
}

fn validate_client_build(value: &str) -> Result<(), ClientBuildError> {
    let declared = u64::try_from(value.len()).unwrap_or(u64::MAX);
    if declared == 0 {
        return Err(ClientBuildError::Empty);
    }
    if declared > u64::from(MAX_CLIENT_BUILD_BYTES) {
        return Err(ClientBuildError::TooLong {
            declared,
            maximum: MAX_CLIENT_BUILD_BYTES,
        });
    }
    Ok(())
}

fn validate_server_build(value: &str) -> Result<(), ServerBuildError> {
    let declared = u64::try_from(value.len()).unwrap_or(u64::MAX);
    if declared == 0 {
        return Err(ServerBuildError::Empty);
    }
    if declared > u64::from(MAX_SERVER_BUILD_BYTES) {
        return Err(ServerBuildError::TooLong {
            declared,
            maximum: MAX_SERVER_BUILD_BYTES,
        });
    }
    Ok(())
}

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
