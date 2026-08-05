//! Dispatch lease fencing and opaque claim/permit types.
//!
//! No long-running dispatcher loop lives here — only durable claim/fence types
//! consumed by [`crate::kernel::store::KernelStore`]. Ambiguity, reconciliation,
//! and permit-bound terminal callbacks are deferred to Task 1.4e2.

use crate::domain::id::{OperationId, OutboxId};
use crate::domain::operation::ResourceFence;
use crate::kernel::outbox::{DestinationClass, Effect, PlannedEffectDocument, ReplayPolicy};

/// Opaque dispatch lease claim returned before an external boundary starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchClaim {
    outbox_id: OutboxId,
    lease_generation: i64,
}

impl DispatchClaim {
    pub(crate) fn new(outbox_id: OutboxId, lease_generation: i64) -> Self {
        Self {
            outbox_id,
            lease_generation,
        }
    }

    pub(crate) fn outbox_id(&self) -> OutboxId {
        self.outbox_id
    }

    pub(crate) fn lease_generation(&self) -> i64 {
        self.lease_generation
    }
}

/// Authorizing permit for one in-flight dispatch attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchPermit {
    outbox_id: OutboxId,
    lease_generation: i64,
    operation_id: OperationId,
    effect_index: u32,
    attempt: u64,
    effect: PlannedEffectDocument,
    external_idempotency_key: String,
    action_epoch: Option<u64>,
    resource_fence: Option<ResourceFence>,
}

impl DispatchPermit {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        outbox_id: OutboxId,
        lease_generation: i64,
        operation_id: OperationId,
        effect_index: u32,
        attempt: u64,
        effect: PlannedEffectDocument,
        external_idempotency_key: String,
        action_epoch: Option<u64>,
        resource_fence: Option<ResourceFence>,
    ) -> Self {
        Self {
            outbox_id,
            lease_generation,
            operation_id,
            effect_index,
            attempt,
            effect,
            external_idempotency_key,
            action_epoch,
            resource_fence,
        }
    }

    pub fn attempt(&self) -> u64 {
        self.attempt
    }

    pub fn effect(&self) -> &Effect {
        &self.effect.effect
    }

    pub fn destination_class(&self) -> DestinationClass {
        self.effect.destination_class
    }

    pub fn replay_policy(&self) -> ReplayPolicy {
        self.effect.replay_policy
    }

    pub fn external_idempotency_key(&self) -> &str {
        &self.external_idempotency_key
    }
}

#[cfg(test)]
mod tests {
    use crate::domain::id::OperationId;
    use crate::kernel::outbox::external_idempotency_key;
    use sha2::{Digest, Sha256};

    fn fixed_uuid_v7(tail: u8) -> [u8; 16] {
        [
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, tail,
        ]
    }

    #[test]
    fn versioned_external_idempotency_key_golden_bytes() {
        let operation_id = OperationId::from_bytes(fixed_uuid_v7(0x30)).unwrap();
        let key = external_idempotency_key(operation_id, 0);
        assert_eq!(key, format!("v1:{operation_id}:0"));
        let digest = Sha256::digest(key.as_bytes());
        let mut hex = String::with_capacity(64);
        for b in digest {
            use std::fmt::Write;
            let _ = write!(hex, "{b:02x}");
        }
        assert_eq!(
            hex,
            "eb29abeddb4fef5fd7032b28bcbc6c547ed2f3aea122aca762a3ba96420e519c"
        );
    }
}
