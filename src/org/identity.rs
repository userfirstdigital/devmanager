//! External Portal identities. DevManager does not mint tenants, users, or
//! BoardCards; it stores opaque foreign identifiers after local confirmation.

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};

use crate::domain::canonical;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityError {
    Empty,
}

impl std::fmt::Display for IdentityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "external Portal identity must be a non-empty canonical value")
    }
}

impl std::error::Error for IdentityError {}

macro_rules! define_external_id {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        pub struct $name(String);

        impl $name {
            pub fn parse(value: impl Into<String>) -> Result<Self, IdentityError> {
                canonical::canonicalize(value.into())
                    .map(Self)
                    .ok_or(IdentityError::Empty)
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::parse(value).map_err(de::Error::custom)
            }
        }
    };
}

define_external_id!(PortalTenantId);
define_external_id!(PortalAccountId);
define_external_id!(PortalDeviceId);
define_external_id!(BoardCardId);
define_external_id!(BoardId);

/// A Connect-authenticated account that has not enrolled this host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalAccount {
    pub tenant_id: PortalTenantId,
    pub account_id: PortalAccountId,
    pub device_id: Option<PortalDeviceId>,
}

impl ExternalAccount {
    pub fn new(
        tenant_id: PortalTenantId,
        account_id: PortalAccountId,
        device_id: Option<PortalDeviceId>,
    ) -> Self {
        Self {
            tenant_id,
            account_id,
            device_id,
        }
    }
}
