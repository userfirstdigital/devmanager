//! Dispatch lease fencing and opaque claim/permit types.
//!
//! No long-running dispatcher loop lives here — only durable claim/fence types
//! consumed by [`crate::kernel::store::KernelStore`]. Recovery tokens keep
//! ambiguity and reconciliation fenced to the dispatch attempt that produced
//! them; terminal callbacks use the same boundary.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::domain::canonical;
use crate::domain::id::{OperationId, OutboxId};
use crate::domain::operation::{
    CancellationReason, OperationErrorCode, ResourceFence, MAX_EXTERNAL_IDENTITY_BYTES,
};
use crate::kernel::outbox::{DestinationClass, Effect, PlannedEffectDocument, ReplayPolicy};
use crate::kernel::store::StoreError;

const ABSENCE_RECEIPT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ReconciliationVerdict {
    Absent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AbsenceReceiptDocument {
    schema_version: u32,
    outbox_id: OutboxId,
    operation_id: OperationId,
    effect_index: u32,
    completed_attempt: u64,
    lookup_identity: String,
    proved_at_ms: i64,
    finding: ReconciliationVerdict,
}

impl AbsenceReceiptDocument {
    pub(crate) fn new(
        outbox_id: OutboxId,
        operation_id: OperationId,
        effect_index: u32,
        completed_attempt: u64,
        lookup_identity: String,
        proved_at_ms: i64,
    ) -> Result<Self, StoreError> {
        let document = Self {
            schema_version: ABSENCE_RECEIPT_SCHEMA_VERSION,
            outbox_id,
            operation_id,
            effect_index,
            completed_attempt,
            lookup_identity,
            proved_at_ms,
            finding: ReconciliationVerdict::Absent,
        };
        document.validate()?;
        Ok(document)
    }

    pub(crate) fn validate(&self) -> Result<(), StoreError> {
        if self.schema_version != ABSENCE_RECEIPT_SCHEMA_VERSION
            || self.completed_attempt == 0
            || self.lookup_identity.len() > MAX_EXTERNAL_IDENTITY_BYTES
            || !canonical::is_canonical(&self.lookup_identity)
            || self.finding != ReconciliationVerdict::Absent
        {
            return Err(StoreError::CodecMismatch {
                detail: "invalid absence receipt document".into(),
            });
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn authorizes(
        &self,
        outbox_id: OutboxId,
        operation_id: OperationId,
        effect_index: u32,
        completed_attempt: u64,
        lookup_identity: &str,
        dispatch_started_at_ms: i64,
        available_at_ms: i64,
    ) -> bool {
        self.outbox_id == outbox_id
            && self.operation_id == operation_id
            && self.effect_index == effect_index
            && self.completed_attempt == completed_attempt
            && self.lookup_identity == lookup_identity
            && self.proved_at_ms >= dispatch_started_at_ms
            && self.proved_at_ms <= available_at_ms
    }
}

pub(crate) fn encode_absence_receipt(
    document: &AbsenceReceiptDocument,
) -> Result<Vec<u8>, StoreError> {
    document.validate()?;
    rmp_serde::to_vec_named(document).map_err(|error| StoreError::CodecMismatch {
        detail: error.to_string(),
    })
}

pub(crate) fn decode_absence_receipt(payload: &[u8]) -> Result<AbsenceReceiptDocument, StoreError> {
    let document: AbsenceReceiptDocument =
        rmp_serde::from_slice(payload).map_err(|error| StoreError::CodecMismatch {
            detail: error.to_string(),
        })?;
    document.validate()?;
    Ok(document)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmbiguityDisposition {
    RetryScheduled,
    ReconciliationRequired,
    Uncertain,
}

/// Terminal result reported for the exact dispatch attempt authorized by a
/// [`DispatchPermit`]. Identity, fences, provenance, and time are store-owned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchCompletion {
    Settled,
    Failed { code: OperationErrorCode },
    Cancelled { reason: CancellationReason },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconciliationOrigin {
    Accepted,
    Uncertain,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconciliationFinding {
    Absent {
        lookup_identity: String,
        retry_after: Duration,
    },
    Inconclusive {
        lookup_identity: String,
        retry_after: Duration,
    },
    PresentSettled {
        lookup_identity: String,
        external_identity: String,
    },
    PresentFailed {
        lookup_identity: String,
        external_identity: String,
        code: OperationErrorCode,
    },
}

impl ReconciliationFinding {
    pub(crate) fn lookup_identity(&self) -> &str {
        match self {
            Self::Absent {
                lookup_identity, ..
            }
            | Self::Inconclusive {
                lookup_identity, ..
            }
            | Self::PresentSettled {
                lookup_identity, ..
            }
            | Self::PresentFailed {
                lookup_identity, ..
            } => lookup_identity,
        }
    }
}

pub(crate) fn ambiguity_disposition(policy: ReplayPolicy) -> AmbiguityDisposition {
    match policy {
        ReplayPolicy::RetrySafe => AmbiguityDisposition::RetryScheduled,
        ReplayPolicy::ReconcileBeforeRetry => AmbiguityDisposition::ReconciliationRequired,
        ReplayPolicy::NoAutomaticRetry => AmbiguityDisposition::Uncertain,
    }
}

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

    pub(crate) fn outbox_id(&self) -> OutboxId {
        self.outbox_id
    }

    pub(crate) fn lease_generation(&self) -> i64 {
        self.lease_generation
    }

    pub(crate) fn operation_id(&self) -> OperationId {
        self.operation_id
    }

    pub(crate) fn effect_index(&self) -> u32 {
        self.effect_index
    }

    pub(crate) fn action_epoch(&self) -> Option<u64> {
        self.action_epoch
    }

    pub(crate) fn resource_fence(&self) -> Option<ResourceFence> {
        self.resource_fence
    }

    pub(crate) fn document(&self) -> &PlannedEffectDocument {
        &self.effect
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconciliationClaim {
    outbox_id: OutboxId,
    lease_generation: i64,
    operation_id: OperationId,
    effect_index: u32,
    completed_attempt: u64,
    origin: ReconciliationOrigin,
    effect: PlannedEffectDocument,
    lookup_identity: String,
    action_epoch: Option<u64>,
    resource_fence: Option<ResourceFence>,
}

impl ReconciliationClaim {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        outbox_id: OutboxId,
        lease_generation: i64,
        operation_id: OperationId,
        effect_index: u32,
        completed_attempt: u64,
        origin: ReconciliationOrigin,
        effect: PlannedEffectDocument,
        lookup_identity: String,
        action_epoch: Option<u64>,
        resource_fence: Option<ResourceFence>,
    ) -> Self {
        Self {
            outbox_id,
            lease_generation,
            operation_id,
            effect_index,
            completed_attempt,
            origin,
            effect,
            lookup_identity,
            action_epoch,
            resource_fence,
        }
    }

    pub fn completed_attempt(&self) -> u64 {
        self.completed_attempt
    }

    pub fn origin(&self) -> ReconciliationOrigin {
        self.origin
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

    pub fn lookup_identity(&self) -> &str {
        &self.lookup_identity
    }

    pub(crate) fn outbox_id(&self) -> OutboxId {
        self.outbox_id
    }

    pub(crate) fn lease_generation(&self) -> i64 {
        self.lease_generation
    }

    pub(crate) fn operation_id(&self) -> OperationId {
        self.operation_id
    }

    pub(crate) fn effect_index(&self) -> u32 {
        self.effect_index
    }

    pub(crate) fn action_epoch(&self) -> Option<u64> {
        self.action_epoch
    }

    pub(crate) fn resource_fence(&self) -> Option<ResourceFence> {
        self.resource_fence
    }

    pub(crate) fn document(&self) -> &PlannedEffectDocument {
        &self.effect
    }
}

#[cfg(test)]
mod tests {
    use super::{ambiguity_disposition, AmbiguityDisposition};
    use crate::domain::id::OperationId;
    use crate::kernel::outbox::{external_idempotency_key, ReplayPolicy};
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

    #[test]
    fn dispatch_recovery_policy_matrix() {
        assert_eq!(
            ambiguity_disposition(ReplayPolicy::RetrySafe),
            AmbiguityDisposition::RetryScheduled
        );
        assert_eq!(
            ambiguity_disposition(ReplayPolicy::ReconcileBeforeRetry),
            AmbiguityDisposition::ReconciliationRequired
        );
        assert_eq!(
            ambiguity_disposition(ReplayPolicy::NoAutomaticRetry),
            AmbiguityDisposition::Uncertain
        );
    }
}
