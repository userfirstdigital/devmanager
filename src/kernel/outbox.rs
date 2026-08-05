//! Crate-private outbox effect planning types and strict MessagePack codecs.
//!
//! No provider/process control lives here — only durable effect documents and
//! the replay policy/destination columns they must agree with.

use serde::{Deserialize, Serialize};

use crate::domain::command::{CommandReceipt, RejectionCode};
use crate::domain::id::{CommandId, EventId, OperationId, TaskId};
use crate::domain::operation::ResourceFence;
use crate::kernel::store::StoreError;

pub(crate) const RECEIPT_SCHEMA_VERSION: u32 = 1;
pub(crate) const EFFECT_SCHEMA_VERSION: u32 = 1;

/// Stable destination class stored in `outbox.destination_class`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)] // reserved for Task 1.4b+ outbox insert/claim
pub(crate) enum DestinationClass {
    TaskTeardown,
    ResourceRelease,
}

impl DestinationClass {
    #[allow(dead_code)] // reserved for Task 1.4b+ outbox insert/claim
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::TaskTeardown => "task_teardown",
            Self::ResourceRelease => "resource_release",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, StoreError> {
        match value {
            "task_teardown" => Ok(Self::TaskTeardown),
            "resource_release" => Ok(Self::ResourceRelease),
            other => Err(StoreError::CodecMismatch {
                detail: format!("unknown destination_class '{other}'"),
            }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)] // reserved for Task 1.4b+ outbox insert/claim
pub(crate) enum ReplayPolicy {
    RetrySafe,
    ReconcileBeforeRetry,
    NoAutomaticRetry,
}

impl ReplayPolicy {
    #[allow(dead_code)] // reserved for Task 1.4b+ outbox insert/claim
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::RetrySafe => "retry_safe",
            Self::ReconcileBeforeRetry => "reconcile_before_retry",
            Self::NoAutomaticRetry => "no_automatic_retry",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, StoreError> {
        match value {
            "retry_safe" => Ok(Self::RetrySafe),
            "reconcile_before_retry" => Ok(Self::ReconcileBeforeRetry),
            "no_automatic_retry" => Ok(Self::NoAutomaticRetry),
            other => Err(StoreError::CodecMismatch {
                detail: format!("unknown replay_policy '{other}'"),
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
#[allow(dead_code)] // reserved for Task 1.4b+ effect planning
pub(crate) enum Effect {
    BeginTaskTeardown {
        task_id: TaskId,
        action_epoch: u64,
    },
    ReleaseResource {
        task_id: TaskId,
        action_epoch: u64,
        resource_fence: ResourceFence,
    },
}

impl Effect {
    pub(crate) fn destination_class(&self) -> DestinationClass {
        match self {
            Self::BeginTaskTeardown { .. } => DestinationClass::TaskTeardown,
            Self::ReleaseResource { .. } => DestinationClass::ResourceRelease,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // reserved for Task 1.4b+ outbox insert
pub(crate) struct PlannedEffectDocument {
    pub schema_version: u32,
    pub destination_class: DestinationClass,
    pub replay_policy: ReplayPolicy,
    pub effect: Effect,
}

impl PlannedEffectDocument {
    #[allow(dead_code)] // reserved for Task 1.4b+ outbox insert
    pub(crate) fn new(effect: Effect, replay_policy: ReplayPolicy) -> Self {
        let destination_class = effect.destination_class();
        Self {
            schema_version: EFFECT_SCHEMA_VERSION,
            destination_class,
            replay_policy,
            effect,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlannedEffectWire {
    schema_version: u32,
    destination_class: DestinationClass,
    replay_policy: ReplayPolicy,
    effect: Effect,
}

#[allow(dead_code)] // reserved for Task 1.4b+ outbox insert
pub(crate) fn encode_effect_document(doc: &PlannedEffectDocument) -> Result<Vec<u8>, StoreError> {
    if doc.schema_version != EFFECT_SCHEMA_VERSION {
        return Err(StoreError::CodecMismatch {
            detail: format!(
                "effect schema_version {} != {EFFECT_SCHEMA_VERSION}",
                doc.schema_version
            ),
        });
    }
    if doc.destination_class != doc.effect.destination_class() {
        return Err(StoreError::CodecMismatch {
            detail: "effect destination_class disagrees with effect payload".into(),
        });
    }
    let wire = PlannedEffectWire {
        schema_version: doc.schema_version,
        destination_class: doc.destination_class,
        replay_policy: doc.replay_policy,
        effect: doc.effect.clone(),
    };
    rmp_serde::to_vec_named(&wire).map_err(|err| StoreError::CodecMismatch {
        detail: err.to_string(),
    })
}

#[allow(dead_code)] // reserved for Task 1.4b+ outbox claim/decode
pub(crate) fn decode_effect_document(
    payload: &[u8],
    destination_class_column: &str,
    replay_policy_column: &str,
) -> Result<PlannedEffectDocument, StoreError> {
    let wire: PlannedEffectWire =
        rmp_serde::from_slice(payload).map_err(|err| StoreError::CodecMismatch {
            detail: err.to_string(),
        })?;
    if wire.schema_version != EFFECT_SCHEMA_VERSION {
        return Err(StoreError::CodecMismatch {
            detail: format!(
                "effect schema_version {} != {EFFECT_SCHEMA_VERSION}",
                wire.schema_version
            ),
        });
    }
    let expected_destination = DestinationClass::parse(destination_class_column)?;
    let expected_policy = ReplayPolicy::parse(replay_policy_column)?;
    if wire.destination_class != expected_destination {
        return Err(StoreError::CodecMismatch {
            detail: format!(
                "effect destination_class {:?} != column '{destination_class_column}'",
                wire.destination_class
            ),
        });
    }
    if wire.replay_policy != expected_policy {
        return Err(StoreError::CodecMismatch {
            detail: format!(
                "effect replay_policy {:?} != column '{replay_policy_column}'",
                wire.replay_policy
            ),
        });
    }
    if wire.destination_class != wire.effect.destination_class() {
        return Err(StoreError::CodecMismatch {
            detail: "decoded effect destination disagrees with effect payload".into(),
        });
    }
    Ok(PlannedEffectDocument {
        schema_version: wire.schema_version,
        destination_class: wire.destination_class,
        replay_policy: wire.replay_policy,
        effect: wire.effect,
    })
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
enum ReceiptBodyWire {
    Accepted {
        command_id: CommandId,
        operation_id: OperationId,
        task_revision: Option<u64>,
        event_ids: Vec<EventId>,
    },
    Rejected {
        command_id: CommandId,
        code: RejectionCode,
        current_revision: Option<u64>,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiptDocumentWire {
    schema_version: u32,
    receipt: ReceiptBodyWire,
}

#[allow(dead_code)] // reserved for Task 1.4b+ receipt persistence
pub(crate) fn encode_receipt_document(receipt: &CommandReceipt) -> Result<Vec<u8>, StoreError> {
    let body = match receipt {
        CommandReceipt::Accepted {
            command_id,
            operation_id,
            task_revision,
            event_ids,
        } => ReceiptBodyWire::Accepted {
            command_id: *command_id,
            operation_id: *operation_id,
            task_revision: *task_revision,
            event_ids: event_ids.clone(),
        },
        CommandReceipt::Rejected {
            command_id,
            code,
            current_revision,
        } => ReceiptBodyWire::Rejected {
            command_id: *command_id,
            code: *code,
            current_revision: *current_revision,
        },
    };
    let wire = ReceiptDocumentWire {
        schema_version: RECEIPT_SCHEMA_VERSION,
        receipt: body,
    };
    rmp_serde::to_vec_named(&wire).map_err(|err| StoreError::CodecMismatch {
        detail: err.to_string(),
    })
}

#[allow(dead_code)] // reserved for Task 1.4b+ receipt lookup
pub(crate) fn decode_receipt_document(payload: &[u8]) -> Result<CommandReceipt, StoreError> {
    let wire: ReceiptDocumentWire =
        rmp_serde::from_slice(payload).map_err(|err| StoreError::CodecMismatch {
            detail: err.to_string(),
        })?;
    if wire.schema_version != RECEIPT_SCHEMA_VERSION {
        return Err(StoreError::CodecMismatch {
            detail: format!(
                "receipt schema_version {} != {RECEIPT_SCHEMA_VERSION}",
                wire.schema_version
            ),
        });
    }
    Ok(match wire.receipt {
        ReceiptBodyWire::Accepted {
            command_id,
            operation_id,
            task_revision,
            event_ids,
        } => CommandReceipt::Accepted {
            command_id,
            operation_id,
            task_revision,
            event_ids,
        },
        ReceiptBodyWire::Rejected {
            command_id,
            code,
            current_revision,
        } => CommandReceipt::Rejected {
            command_id,
            code,
            current_revision,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::id::{CommandId, EventId, OperationId, ResourceId, TaskId};

    fn fixed_uuid_v7(tail: u8) -> [u8; 16] {
        [
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, tail,
        ]
    }

    #[test]
    fn command_contract_receipt_codec_version_and_unknown_fields() {
        let receipt = CommandReceipt::Accepted {
            command_id: CommandId::from_bytes(fixed_uuid_v7(0x01)).unwrap(),
            operation_id: OperationId::from_bytes(fixed_uuid_v7(0x02)).unwrap(),
            task_revision: Some(3),
            event_ids: vec![EventId::from_bytes(fixed_uuid_v7(0x03)).unwrap()],
        };
        let bytes = encode_receipt_document(&receipt).expect("encode");
        let decoded = decode_receipt_document(&bytes).expect("decode");
        assert_eq!(decoded, receipt);

        #[derive(Serialize)]
        struct BadReceipt {
            schema_version: u32,
            receipt: BadReceiptBody,
        }
        #[derive(Serialize)]
        struct BadReceiptBody {
            status: &'static str,
            command_id: CommandId,
            operation_id: OperationId,
            task_revision: Option<u64>,
            event_ids: Vec<EventId>,
            extra: bool,
        }
        let nested_unknown = rmp_serde::to_vec_named(&BadReceipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            receipt: BadReceiptBody {
                status: "accepted",
                command_id: CommandId::from_bytes(fixed_uuid_v7(0x01)).unwrap(),
                operation_id: OperationId::from_bytes(fixed_uuid_v7(0x02)).unwrap(),
                task_revision: Some(1),
                event_ids: Vec::new(),
                extra: true,
            },
        })
        .unwrap();
        assert!(
            decode_receipt_document(&nested_unknown).is_err(),
            "unknown fields inside receipt body must fail"
        );

        #[derive(Serialize)]
        struct VersionedReceipt {
            schema_version: u32,
            receipt: GoodReceiptBody,
        }
        #[derive(Serialize)]
        struct GoodReceiptBody {
            status: &'static str,
            command_id: CommandId,
            operation_id: OperationId,
            task_revision: Option<u64>,
            event_ids: Vec<EventId>,
        }
        let bad_version = rmp_serde::to_vec_named(&VersionedReceipt {
            schema_version: 99,
            receipt: GoodReceiptBody {
                status: "accepted",
                command_id: CommandId::from_bytes(fixed_uuid_v7(0x01)).unwrap(),
                operation_id: OperationId::from_bytes(fixed_uuid_v7(0x02)).unwrap(),
                task_revision: Some(1),
                event_ids: Vec::new(),
            },
        })
        .unwrap();
        let err = decode_receipt_document(&bad_version).expect_err("bad version");
        assert!(matches!(err, StoreError::CodecMismatch { .. }));
    }

    #[test]
    fn command_contract_effect_codec_checks_columns() {
        let effect = Effect::ReleaseResource {
            task_id: TaskId::from_bytes(fixed_uuid_v7(0x12)).unwrap(),
            action_epoch: 3,
            resource_fence: ResourceFence::new(
                ResourceId::from_bytes(fixed_uuid_v7(0x10)).unwrap(),
                4,
            ),
        };
        let doc = PlannedEffectDocument::new(effect.clone(), ReplayPolicy::NoAutomaticRetry);
        let bytes = encode_effect_document(&doc).expect("encode");
        let decoded = decode_effect_document(
            &bytes,
            DestinationClass::ResourceRelease.as_str(),
            ReplayPolicy::NoAutomaticRetry.as_str(),
        )
        .expect("decode");
        assert_eq!(decoded.effect, effect);
        assert_eq!(decoded.schema_version, EFFECT_SCHEMA_VERSION);

        let mismatch = decode_effect_document(
            &bytes,
            DestinationClass::TaskTeardown.as_str(),
            ReplayPolicy::NoAutomaticRetry.as_str(),
        )
        .expect_err("destination mismatch");
        assert!(matches!(mismatch, StoreError::CodecMismatch { .. }));

        let policy_mismatch = decode_effect_document(
            &bytes,
            DestinationClass::ResourceRelease.as_str(),
            ReplayPolicy::RetrySafe.as_str(),
        )
        .expect_err("policy mismatch");
        assert!(matches!(policy_mismatch, StoreError::CodecMismatch { .. }));

        let teardown = Effect::BeginTaskTeardown {
            task_id: TaskId::from_bytes(fixed_uuid_v7(0x11)).unwrap(),
            action_epoch: 2,
        };
        let teardown_doc = PlannedEffectDocument::new(teardown, ReplayPolicy::ReconcileBeforeRetry);
        let teardown_bytes = encode_effect_document(&teardown_doc).unwrap();
        decode_effect_document(&teardown_bytes, "task_teardown", "reconcile_before_retry")
            .expect("teardown decode");

        #[derive(Serialize)]
        struct BadEffectDoc {
            schema_version: u32,
            destination_class: DestinationClass,
            replay_policy: ReplayPolicy,
            effect: BadReleaseEffect,
        }
        #[derive(Serialize)]
        struct BadReleaseEffect {
            release_resource: BadReleaseBody,
        }
        #[derive(Serialize)]
        struct BadReleaseBody {
            task_id: TaskId,
            action_epoch: u64,
            resource_fence: ResourceFence,
            extra: bool,
        }
        let nested_unknown = rmp_serde::to_vec_named(&BadEffectDoc {
            schema_version: EFFECT_SCHEMA_VERSION,
            destination_class: DestinationClass::ResourceRelease,
            replay_policy: ReplayPolicy::RetrySafe,
            effect: BadReleaseEffect {
                release_resource: BadReleaseBody {
                    task_id: TaskId::from_bytes(fixed_uuid_v7(0x12)).unwrap(),
                    action_epoch: 3,
                    resource_fence: ResourceFence::new(
                        ResourceId::from_bytes(fixed_uuid_v7(0x10)).unwrap(),
                        4,
                    ),
                    extra: true,
                },
            },
        })
        .unwrap();
        assert!(
            decode_effect_document(
                &nested_unknown,
                DestinationClass::ResourceRelease.as_str(),
                ReplayPolicy::RetrySafe.as_str(),
            )
            .is_err(),
            "unknown fields inside effect payload must fail"
        );
    }
}
