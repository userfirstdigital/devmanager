use std::fmt;
use std::str::FromStr;

use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize, Serializer};
use uuid::{Uuid, Variant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdError {
    InvalidFormat,
    InvalidVersion,
    InvalidVariant,
}

impl fmt::Display for IdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidFormat => write!(f, "invalid UUID format"),
            Self::InvalidVersion => write!(f, "UUID must be version 7"),
            Self::InvalidVariant => write!(f, "UUID must use the RFC 4122/9562 variant"),
        }
    }
}

impl std::error::Error for IdError {}

fn validate_uuid_v7(uuid: Uuid) -> Result<Uuid, IdError> {
    // uuid 1.24 exposes UUIDv7 as Version::SortRand; check the version number.
    if uuid.get_version_num() != 7 {
        return Err(IdError::InvalidVersion);
    }
    // RFC 4122 / RFC 9562 variant (10xx).
    if uuid.get_variant() != Variant::RFC4122 {
        return Err(IdError::InvalidVariant);
    }
    Ok(uuid)
}

macro_rules! define_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }

            /// Wire-valid zero sentinel for a non-optional identity field that
            /// has no identity to carry (a plain shell terminal has no agent
            /// session, yet `TaskTerminalProjection` keeps the field required
            /// for wire compatibility).
            ///
            /// Every payload bit is zero, but the UUID version (7) and RFC 4122
            /// variant bits are set: a true `Uuid::nil()` is version 0 and this
            /// crate's validating deserializer would reject it, so a projection
            /// carrying one could be encoded and never decoded.
            pub const fn nil() -> Self {
                Self(Uuid::from_bytes([
                    0, 0, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 0,
                ]))
            }

            /// True for [`Self::nil`], the "no identity" sentinel.
            pub fn is_nil(&self) -> bool {
                self.0 == Self::nil().0
            }

            pub fn parse(input: &str) -> Result<Self, IdError> {
                let uuid = Uuid::parse_str(input).map_err(|_| IdError::InvalidFormat)?;
                Self::try_from_uuid(uuid)
            }

            pub fn from_bytes(bytes: [u8; 16]) -> Result<Self, IdError> {
                Self::try_from_uuid(Uuid::from_bytes(bytes))
            }

            pub fn as_bytes(&self) -> &[u8; 16] {
                self.0.as_bytes()
            }

            fn try_from_uuid(uuid: Uuid) -> Result<Self, IdError> {
                validate_uuid_v7(uuid).map(Self)
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }

        impl FromStr for $name {
            type Err = IdError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::parse(s)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                // Preserve Uuid's human-readable string / binary-bytes wire shapes.
                self.0.serialize(serializer)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                struct IdVisitor;

                impl<'de> Visitor<'de> for IdVisitor {
                    type Value = $name;

                    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                        formatter.write_str(concat!("a UUIDv7 ", stringify!($name)))
                    }

                    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
                    where
                        E: de::Error,
                    {
                        $name::parse(value).map_err(de::Error::custom)
                    }

                    fn visit_bytes<E>(self, value: &[u8]) -> Result<Self::Value, E>
                    where
                        E: de::Error,
                    {
                        let bytes: [u8; 16] = value
                            .try_into()
                            .map_err(|_| de::Error::custom("UUID must be exactly 16 bytes"))?;
                        $name::from_bytes(bytes).map_err(de::Error::custom)
                    }

                    fn visit_byte_buf<E>(self, value: Vec<u8>) -> Result<Self::Value, E>
                    where
                        E: de::Error,
                    {
                        self.visit_bytes(&value)
                    }

                    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
                    where
                        A: de::SeqAccess<'de>,
                    {
                        let mut bytes = [0u8; 16];
                        for (index, slot) in bytes.iter_mut().enumerate() {
                            *slot = seq
                                .next_element()?
                                .ok_or_else(|| de::Error::invalid_length(index, &self))?;
                        }
                        if seq.next_element::<u8>()?.is_some() {
                            return Err(de::Error::invalid_length(17, &self));
                        }
                        $name::from_bytes(bytes).map_err(de::Error::custom)
                    }
                }

                if deserializer.is_human_readable() {
                    deserializer.deserialize_str(IdVisitor)
                } else {
                    deserializer.deserialize_bytes(IdVisitor)
                }
            }
        }
    };
}

define_id!(EnvironmentId);
define_id!(ProjectId);
define_id!(TaskId);
define_id!(AgentSessionId);
define_id!(ArtifactId);
define_id!(ResourceId);
define_id!(TerminalId);
define_id!(BrowserContextId);
define_id!(BrowserTabId);
define_id!(BrowserRequestId);
define_id!(BrowserSessionId);
define_id!(ServiceId);
define_id!(ClientId);
define_id!(CommandId);
define_id!(RequestId);
define_id!(OperationId);
define_id!(TransferId);
define_id!(SubscriptionId);
define_id!(SnapshotId);
define_id!(EventId);
define_id!(OutboxId);
define_id!(TurnId);
define_id!(QuestionId);
define_id!(ApprovalId);
define_id!(PromptId);
define_id!(PromptVersionId);
define_id!(PromptChainId);
define_id!(PromptChainLinkId);
define_id!(PromptHistoryId);
define_id!(TaskInviteId);

/// Bounded configured-service catalog identity (for example `api`).
///
/// This is not [`ServiceId`]. `ServiceId` is a UUIDv7 domain resource identity.
/// Host converts [`ConfiguredServiceId`] into `crate::services::model::ServiceId`
/// exactly once at the supervisor boundary.
#[derive(Clone, Debug, Eq, PartialEq, PartialOrd, Ord, Hash)]
pub struct ConfiguredServiceId(String);

pub const MAX_CONFIGURED_SERVICE_ID_BYTES: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfiguredServiceIdError {
    Empty,
    TooLong,
    InvalidIdentifier,
}

impl fmt::Display for ConfiguredServiceIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "configured service id is empty"),
            Self::TooLong => write!(
                f,
                "configured service id exceeds {MAX_CONFIGURED_SERVICE_ID_BYTES} bytes"
            ),
            Self::InvalidIdentifier => {
                write!(f, "configured service id is not a bounded identifier")
            }
        }
    }
}

impl std::error::Error for ConfiguredServiceIdError {}

impl ConfiguredServiceId {
    pub fn new(value: impl Into<String>) -> Result<Self, ConfiguredServiceIdError> {
        let value = value.into();
        if value.is_empty() {
            return Err(ConfiguredServiceIdError::Empty);
        }
        if value.len() > MAX_CONFIGURED_SERVICE_ID_BYTES {
            return Err(ConfiguredServiceIdError::TooLong);
        }
        if value.starts_with('.')
            || value.contains('\0')
            || !value.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
            })
        {
            return Err(ConfiguredServiceIdError::InvalidIdentifier);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ConfiguredServiceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for ConfiguredServiceId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ConfiguredServiceId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::{ConfiguredServiceId, ServiceId};

    #[test]
    fn configured_catalog_id_rejects_empty_and_stays_distinct_from_uuid_service_id() {
        assert!(ConfiguredServiceId::new("").is_err());
        assert!(ConfiguredServiceId::new("api").is_ok());
        assert!(ServiceId::parse("api").is_err());
        let uuid = ServiceId::new();
        let encoded_uuid = serde_json::to_string(&uuid).expect("uuid");
        let encoded_catalog =
            serde_json::to_string(&ConfiguredServiceId::new("api").expect("catalog")).expect("id");
        assert_eq!(encoded_catalog, "\"api\"");
        assert_ne!(encoded_uuid, encoded_catalog);
    }

    #[test]
    fn nil_sentinel_round_trips_through_the_validating_wire_decoder() {
        use super::AgentSessionId;
        let sentinel = AgentSessionId::nil();
        assert!(sentinel.is_nil());
        assert!(!AgentSessionId::new().is_nil());
        assert_eq!(sentinel.to_string(), "00000000-0000-7000-8000-000000000000");
        // Human-readable and binary carriers both revalidate the UUID version.
        let json = serde_json::to_string(&sentinel).expect("encode sentinel");
        let decoded: AgentSessionId = serde_json::from_str(&json).expect("decode sentinel");
        assert_eq!(decoded, sentinel);
        let packed = rmp_serde::to_vec_named(&sentinel).expect("pack sentinel");
        let unpacked: AgentSessionId = rmp_serde::from_slice(&packed).expect("unpack sentinel");
        assert_eq!(unpacked, sentinel);
        // A true all-zero UUID is version 0 and must stay rejected.
        assert!(AgentSessionId::parse("00000000-0000-0000-0000-000000000000").is_err());
    }
}
