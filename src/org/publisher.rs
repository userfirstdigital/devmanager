//! Authenticated organization publisher/reconciler. HMAC keys stay in memory.

use std::collections::BTreeMap;
use std::fmt;

use hmac::{Hmac, Mac};
use sha2::Sha256;
use uuid::Uuid;

use crate::org::error::OrgError;
use crate::org::evidence::{compute_bundle_hash_bytes, EvidenceBundle};
use crate::org::persistence::{
    OutboxDeliveryState, PersistedOutboxIntent, MAX_ORGANIZATION_OUTBOX_INTENTS,
};
use crate::org::wire::{codec_error, organization_fact_from_payload};
use crate::org::{OrganizationProjection, SyncOutcome};
use crate::protocol::{
    organization_envelope_canonical_bytes, OrganizationEnvelopeWire, OrganizationWirePayload,
    ORGANIZATION_SCHEMA_VERSION,
};

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone, PartialEq, Eq)]
pub struct SignedOrganizationEnvelope {
    pub envelope: OrganizationEnvelopeWire,
    pub mac_hex: String,
}

pub struct OrganizationPublisher {
    key: [u8; 32],
    tenant_id: String,
    account_id: String,
    host_id: Uuid,
    session_id: String,
    last_revision: u64,
    seen_revisions: BTreeMap<u64, [u8; 32]>,
    outbox: BTreeMap<String, PersistedOutboxIntent>,
}

impl fmt::Debug for OrganizationPublisher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OrganizationPublisher")
            .field("key", &"[redacted]")
            .field("tenant_id", &self.tenant_id)
            .field("account_id", &self.account_id)
            .field("host_id", &self.host_id)
            .field("session_id", &self.session_id)
            .field("last_revision", &self.last_revision)
            .field("outbox_len", &self.outbox.len())
            .finish()
    }
}

impl OrganizationPublisher {
    pub fn new(
        key: [u8; 32],
        tenant_id: impl Into<String>,
        account_id: impl Into<String>,
        host_id: Uuid,
        session_id: impl Into<String>,
    ) -> Result<Self, OrgError> {
        let tenant_id = tenant_id.into();
        let account_id = account_id.into();
        let session_id = session_id.into();
        if tenant_id.trim().is_empty()
            || account_id.trim().is_empty()
            || session_id.trim().is_empty()
        {
            return Err(OrgError::EmptyIdentity);
        }
        Ok(Self {
            key,
            tenant_id,
            account_id,
            host_id,
            session_id,
            last_revision: 0,
            seen_revisions: BTreeMap::new(),
            outbox: BTreeMap::new(),
        })
    }

    pub fn sign(
        &self,
        revision: u64,
        payload: OrganizationWirePayload,
    ) -> Result<SignedOrganizationEnvelope, OrgError> {
        let envelope = OrganizationEnvelopeWire {
            schema_version: ORGANIZATION_SCHEMA_VERSION,
            tenant_id: self.tenant_id.clone(),
            account_id: self.account_id.clone(),
            host_id: self.host_id,
            session_id: self.session_id.clone(),
            revision,
            payload,
        };
        let canonical = organization_envelope_canonical_bytes(&envelope).map_err(codec_error)?;
        Ok(SignedOrganizationEnvelope {
            envelope,
            mac_hex: hex_encode(&hmac_sign(&self.key, &canonical)?),
        })
    }

    pub fn verify(
        &self,
        signed: &SignedOrganizationEnvelope,
    ) -> Result<OrganizationWirePayload, OrgError> {
        if signed.envelope.tenant_id != self.tenant_id
            || signed.envelope.account_id != self.account_id
            || signed.envelope.host_id != self.host_id
            || signed.envelope.session_id != self.session_id
        {
            return Err(OrgError::CrossTenant);
        }
        let canonical =
            organization_envelope_canonical_bytes(&signed.envelope).map_err(codec_error)?;
        let expected = hmac_sign(&self.key, &canonical)?;
        let actual = decode_mac_hex(&signed.mac_hex)?;
        if !constant_time_eq(&expected, &actual) {
            return Err(OrgError::TamperedEvidence);
        }
        if signed.envelope.revision == 0 {
            return Err(OrgError::StalePolicy);
        }
        if signed.envelope.revision < self.last_revision {
            return Err(OrgError::StalePolicy);
        }
        if let Some(existing) = self.seen_revisions.get(&signed.envelope.revision) {
            return if existing == &expected {
                Err(OrgError::Replay)
            } else {
                Err(OrgError::LastWriteWinsForbidden)
            };
        }
        Ok(signed.envelope.payload.clone())
    }

    pub fn reconcile(
        &mut self,
        projection: &mut OrganizationProjection,
        signed: &SignedOrganizationEnvelope,
        now_ms: i64,
    ) -> Result<SyncOutcome, OrgError> {
        let payload = self.verify(signed)?;
        let canonical =
            organization_envelope_canonical_bytes(&signed.envelope).map_err(codec_error)?;
        let mac = hmac_sign(&self.key, &canonical)?;
        let outcome = match organization_fact_from_payload(&payload)? {
            Some(fact) => projection.apply_authoritative_fact(fact, now_ms)?,
            None => projection.dispatch_wire_payload(payload, now_ms)?,
        };
        self.seen_revisions.insert(signed.envelope.revision, mac);
        self.last_revision = signed.envelope.revision;
        Ok(outcome)
    }

    pub fn queue_publication(
        &mut self,
        observation_id_hex: impl Into<String>,
        intent: impl Into<String>,
    ) -> Result<PersistedOutboxIntent, OrgError> {
        let queued = PersistedOutboxIntent {
            observation_id_hex: observation_id_hex.into(),
            intent: intent.into(),
            publication_queued: true,
            delivery: OutboxDeliveryState::Queued,
        };
        crate::org::persistence::validate_outbox_intent(&queued)?;
        if let Some(existing) = self.outbox.get(&queued.observation_id_hex) {
            return if existing == &queued {
                Ok(existing.clone())
            } else {
                Err(OrgError::LastWriteWinsForbidden)
            };
        }
        if self.outbox.len() >= MAX_ORGANIZATION_OUTBOX_INTENTS {
            return Err(OrgError::BoundExceeded);
        }
        self.outbox
            .insert(queued.observation_id_hex.clone(), queued.clone());
        Ok(queued)
    }

    pub fn acknowledge_local(
        &mut self,
        observation_id_hex: &str,
    ) -> Result<PersistedOutboxIntent, OrgError> {
        let intent = self
            .outbox
            .get_mut(observation_id_hex)
            .ok_or(OrgError::Unlinked)?;
        intent.delivery = OutboxDeliveryState::LocallyAcknowledged;
        Ok(intent.clone())
    }

    pub fn mark_uncertain(
        &mut self,
        observation_id_hex: &str,
    ) -> Result<PersistedOutboxIntent, OrgError> {
        let intent = self
            .outbox
            .get_mut(observation_id_hex)
            .ok_or(OrgError::Unlinked)?;
        intent.delivery = OutboxDeliveryState::Uncertain;
        Ok(intent.clone())
    }

    pub fn outbox(&self) -> impl Iterator<Item = &PersistedOutboxIntent> {
        self.outbox.values()
    }

    pub fn sign_evidence_hmac(&self, bundle: &EvidenceBundle) -> Result<String, OrgError> {
        let mac = hmac_sign(&self.key, &compute_bundle_hash_bytes(bundle))?;
        Ok(hex_encode(&mac))
    }

    pub fn verify_evidence_hmac(
        &self,
        bundle: &EvidenceBundle,
        mac_hex: &str,
    ) -> Result<(), OrgError> {
        let expected = hmac_sign(&self.key, &compute_bundle_hash_bytes(bundle))?;
        let actual = decode_mac_hex(mac_hex)?;
        if !constant_time_eq(&expected, &actual) {
            return Err(OrgError::TamperedEvidence);
        }
        Ok(())
    }
}

fn hmac_sign(key: &[u8; 32], bytes: &[u8]) -> Result<[u8; 32], OrgError> {
    let mut mac = HmacSha256::new_from_slice(key).map_err(|_| OrgError::EmptyIdentity)?;
    mac.update(bytes);
    Ok(mac.finalize().into_bytes().into())
}

fn decode_mac_hex(hex: &str) -> Result<[u8; 32], OrgError> {
    if hex.len() != 64 {
        return Err(OrgError::TamperedEvidence);
    }
    let mut out = [0u8; 32];
    for (index, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let value = std::str::from_utf8(chunk)
            .ok()
            .and_then(|part| u8::from_str_radix(part, 16).ok())
            .ok_or(OrgError::TamperedEvidence)?;
        out[index] = value;
    }
    Ok(out)
}

fn hex_encode(bytes: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn constant_time_eq(left: &[u8; 32], right: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{OrganizationMembershipWire, ORGANIZATION_SCHEMA_VERSION};

    fn new_publisher() -> OrganizationPublisher {
        OrganizationPublisher::new([7u8; 32], "acme", "owner-1", Uuid::now_v7(), "session-1")
            .expect("publisher")
    }

    fn membership_payload(host_id: Uuid) -> OrganizationWirePayload {
        OrganizationWirePayload::Membership(OrganizationMembershipWire {
            schema_version: ORGANIZATION_SCHEMA_VERSION,
            tenant_id: "acme".to_string(),
            account_id: "owner-1".to_string(),
            host_id,
            device_id: "device-1".to_string(),
            role: "owner".to_string(),
            status: "enrolled".to_string(),
            display_name: "owner-host".to_string(),
            policy_revision: 1,
            enrolled_at_ms: 1_000,
            last_seen_ms: 1_000,
        })
    }

    #[test]
    fn tamper_and_replay_are_rejected() {
        let publisher = new_publisher();
        let mut signed = publisher
            .sign(1, membership_payload(publisher.host_id))
            .expect("sign");
        let verified = publisher.verify(&signed).expect("verify");
        assert!(matches!(verified, OrganizationWirePayload::Membership(_)));
        signed.mac_hex.replace_range(0..2, "00");
        assert_eq!(
            publisher.verify(&signed).expect_err("tamper"),
            OrgError::TamperedEvidence
        );

        let mut publisher = new_publisher();
        let signed = publisher
            .sign(1, membership_payload(publisher.host_id))
            .expect("sign");
        publisher
            .seen_revisions
            .insert(1, decode_mac_hex(&signed.mac_hex).expect("mac"));
        publisher.last_revision = 1;
        assert_eq!(
            publisher.verify(&signed).expect_err("replay"),
            OrgError::Replay
        );
        let stale = publisher
            .sign(1, membership_payload(publisher.host_id))
            .expect("stale sign");
        assert_eq!(
            publisher.verify(&stale).expect_err("stale or replay"),
            OrgError::Replay
        );
    }

    #[test]
    fn outbox_is_bounded_and_never_claims_delivery() {
        let mut publisher = new_publisher();
        let hex = "ab".repeat(32);
        let queued = publisher
            .queue_publication(hex.clone(), "request_delivery")
            .expect("queue");
        assert_eq!(queued.delivery, OutboxDeliveryState::Queued);
        assert!(queued.publication_queued);
        let duplicate = publisher
            .queue_publication(hex.clone(), "request_delivery")
            .expect("exact duplicate");
        assert_eq!(duplicate, queued);
        let ack = publisher.acknowledge_local(&hex).expect("ack");
        assert_eq!(ack.delivery, OutboxDeliveryState::LocallyAcknowledged);
        assert_eq!(
            publisher.queue_publication(hex.clone(), "request_delivery"),
            Err(OrgError::LastWriteWinsForbidden)
        );
        assert_eq!(
            publisher.queue_publication(hex, "other-intent"),
            Err(OrgError::LastWriteWinsForbidden)
        );
        assert_eq!(
            publisher.queue_publication("cd".repeat(32), "   "),
            Err(OrgError::EmptyIdentity)
        );
    }
}
