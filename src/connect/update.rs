//! Bundle/protocol update continuity.
//!
//! A mismatch pauses mutations, preserves local drafts and device keys, and
//! never rewrites remote.json or rotates pairing codes, device keys, or
//! unexpired task invitations.

use serde::{Deserialize, Serialize};

use super::envelope::{CONNECT_PROTOCOL_MAJOR, CONNECT_PROTOCOL_MINOR};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateContinuityError {
    ProtocolIncompatible,
    BundleStale,
    MutationsPaused,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairingContinuity {
    pub pairing_code_generation: u64,
    pub host_identity_fingerprint: String,
    pub device_key_fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateContinuity {
    pub protocol_major: u16,
    pub protocol_minor: u16,
    pub bundle_id: String,
    pub pairing: PairingContinuity,
    pub local_draft: Option<String>,
    pub mutations_paused: bool,
    pub reload_required: bool,
}

impl UpdateContinuity {
    pub fn compatible(bundle_id: impl Into<String>, pairing: PairingContinuity) -> Self {
        Self {
            protocol_major: CONNECT_PROTOCOL_MAJOR,
            protocol_minor: CONNECT_PROTOCOL_MINOR,
            bundle_id: bundle_id.into(),
            pairing,
            local_draft: None,
            mutations_paused: false,
            reload_required: false,
        }
    }

    pub fn preserve_draft(&mut self, draft: impl Into<String>) {
        self.local_draft = Some(draft.into());
    }

    pub fn observe_peer(
        &mut self,
        protocol_major: u16,
        protocol_minor: u16,
        bundle_id: &str,
    ) -> Result<(), UpdateContinuityError> {
        if protocol_major != CONNECT_PROTOCOL_MAJOR {
            self.pause_for_reload();
            return Err(UpdateContinuityError::ProtocolIncompatible);
        }
        if protocol_minor > CONNECT_PROTOCOL_MINOR {
            self.pause_for_reload();
            return Err(UpdateContinuityError::ProtocolIncompatible);
        }
        if bundle_id != self.bundle_id {
            self.pause_for_reload();
            return Err(UpdateContinuityError::BundleStale);
        }
        Ok(())
    }

    pub fn admit_mutation(&self) -> Result<(), UpdateContinuityError> {
        if self.mutations_paused {
            Err(UpdateContinuityError::MutationsPaused)
        } else {
            Ok(())
        }
    }

    pub fn reconnect_same_identity(&self, observed: &PairingContinuity) -> bool {
        self.pairing == *observed
    }

    pub fn rotated_pairing(&self, observed: &PairingContinuity) -> bool {
        self.pairing.pairing_code_generation != observed.pairing_code_generation
            || self.pairing.host_identity_fingerprint != observed.host_identity_fingerprint
    }

    pub fn rotated_device_key(&self, observed: &PairingContinuity) -> bool {
        self.pairing.device_key_fingerprint != observed.device_key_fingerprint
    }

    fn pause_for_reload(&mut self) {
        self.mutations_paused = true;
        self.reload_required = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairing() -> PairingContinuity {
        PairingContinuity {
            pairing_code_generation: 1,
            host_identity_fingerprint: "host".into(),
            device_key_fingerprint: "device".into(),
        }
    }

    #[test]
    fn mismatch_pauses_without_rotating_identity() {
        let mut continuity = UpdateContinuity::compatible("bundle-a", pairing());
        continuity.preserve_draft("keep me");
        assert_eq!(
            continuity.observe_peer(CONNECT_PROTOCOL_MAJOR + 1, 0, "bundle-a"),
            Err(UpdateContinuityError::ProtocolIncompatible)
        );
        assert!(continuity.mutations_paused);
        assert_eq!(continuity.local_draft.as_deref(), Some("keep me"));
        assert!(continuity.reconnect_same_identity(&pairing()));
        assert!(!continuity.rotated_pairing(&pairing()));
        assert!(!continuity.rotated_device_key(&pairing()));
    }
}
