use std::fmt;
use std::str::FromStr;

use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize, Serializer};
use uuid::{Uuid, Variant};

use crate::domain::id::IdError;

fn validate_uuid_v7(uuid: Uuid) -> Result<Uuid, IdError> {
    if uuid.get_version_num() != 7 {
        return Err(IdError::InvalidVersion);
    }
    if uuid.get_variant() != Variant::RFC4122 {
        return Err(IdError::InvalidVariant);
    }
    Ok(uuid)
}

macro_rules! define_org_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::now_v7())
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
                }

                deserializer.deserialize_str(IdVisitor)
            }
        }
    };
}

define_org_id!(ManagedLinkId);
define_org_id!(OrgPromptId);
define_org_id!(OrgPromptVersionId);
define_org_id!(OrgPromptChainId);
define_org_id!(LocalActionId);
define_org_id!(EvidenceBundleId);
define_org_id!(TaskDraftId);
define_org_id!(HandoffId);
