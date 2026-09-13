//! Ephemeral host-issued credentials for one logical reconnect handoff.
//!
//! A grant is a random bearer value carried only in ClientHello/ServerHello.
//! The host retains only its SHA-256 digest in the bounded in-memory grant
//! ledger; durable projections and client-facing task data never contain it.

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

const RECONNECT_GRANT_BYTES: usize = 32;

#[derive(Clone, PartialEq, Eq)]
pub struct ReconnectGrant([u8; RECONNECT_GRANT_BYTES]);

impl fmt::Debug for ReconnectGrant {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ReconnectGrant(REDACTED)")
    }
}

impl ReconnectGrant {
    pub(crate) fn issue() -> Result<Self, ()> {
        let mut bytes = [0_u8; RECONNECT_GRANT_BYTES];
        getrandom::fill(&mut bytes).map_err(|_| ())?;
        Ok(Self(bytes))
    }

    pub(crate) fn as_bytes(&self) -> &[u8; RECONNECT_GRANT_BYTES] {
        &self.0
    }

    #[cfg(test)]
    pub(crate) fn from_test_byte(value: u8) -> Self {
        Self([value; RECONNECT_GRANT_BYTES])
    }
}

struct GrantBytesRef<'a>(&'a [u8]);

impl Serialize for GrantBytesRef<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(self.0)
    }
}

struct GrantBytes(Vec<u8>);

impl<'de> Deserialize<'de> for GrantBytes {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct GrantVisitor;

        impl<'de> Visitor<'de> for GrantVisitor {
            type Value = GrantBytes;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("32 opaque reconnect-grant bytes")
            }

            fn visit_bytes<E: de::Error>(self, bytes: &[u8]) -> Result<Self::Value, E> {
                if bytes.len() != RECONNECT_GRANT_BYTES {
                    return Err(de::Error::invalid_length(bytes.len(), &self));
                }
                Ok(GrantBytes(bytes.to_vec()))
            }

            fn visit_byte_buf<E: de::Error>(self, bytes: Vec<u8>) -> Result<Self::Value, E> {
                self.visit_bytes(&bytes)
            }

            fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut bytes = Vec::with_capacity(RECONNECT_GRANT_BYTES);
                while let Some(byte) = seq.next_element::<u8>()? {
                    if bytes.len() == RECONNECT_GRANT_BYTES {
                        return Err(de::Error::invalid_length(RECONNECT_GRANT_BYTES + 1, &self));
                    }
                    bytes.push(byte);
                }
                if bytes.len() != RECONNECT_GRANT_BYTES {
                    return Err(de::Error::invalid_length(bytes.len(), &self));
                }
                Ok(GrantBytes(bytes))
            }
        }

        deserializer.deserialize_bytes(GrantVisitor)
    }
}

impl Serialize for ReconnectGrant {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        GrantBytesRef(&self.0).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ReconnectGrant {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let GrantBytes(bytes) = GrantBytes::deserialize(deserializer)?;
        let bytes: [u8; RECONNECT_GRANT_BYTES] = bytes
            .try_into()
            .map_err(|_| de::Error::custom("reconnect grant must be exactly 32 bytes"))?;
        Ok(Self(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::ReconnectGrant;

    #[test]
    fn grant_roundtrips_as_opaque_bytes_and_never_formats_secret() {
        let grant = ReconnectGrant::from_test_byte(0xa5);
        let encoded = rmp_serde::to_vec_named(&grant).expect("grant encoding");
        let decoded: ReconnectGrant = rmp_serde::from_slice(&encoded).expect("grant decoding");

        assert_eq!(decoded, grant);
        let debug = format!("{grant:?}");
        assert_eq!(debug, "ReconnectGrant(REDACTED)");
        assert!(!debug.contains("a5"));
    }

    #[test]
    fn grant_decoder_rejects_wrong_length() {
        let encoded = rmp_serde::to_vec_named(&[0xa5_u8; 31]).expect("short grant encoding");
        assert!(rmp_serde::from_slice::<ReconnectGrant>(&encoded).is_err());

        let encoded = rmp_serde::to_vec_named(&vec![0xa5_u8; 33]).expect("long grant encoding");
        assert!(rmp_serde::from_slice::<ReconnectGrant>(&encoded).is_err());
    }
}
