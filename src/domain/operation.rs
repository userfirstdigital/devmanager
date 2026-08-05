use std::marker::PhantomData;

use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, Visitor};
use serde::ser::{self, SerializeMap, Serializer};
use serde::{Deserialize, Serialize};

use crate::domain::canonical;
use crate::domain::id::{CommandId, EventId, OperationId, ResourceId, TaskId};

/// Maximum UTF-8 byte length for durable external reconciliation identities.
pub const MAX_EXTERNAL_IDENTITY_BYTES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationErrorCode {
    SideEffectFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CancellationReason {
    Superseded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationUncertaintyCode {
    AmbiguousDispatch,
}

macro_rules! impl_named_code_deserialize {
    ($type:ident, $expecting:literal, { $($variant:ident => $wire:literal),+ $(,)? }) => {
        impl<'de> Deserialize<'de> for $type {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                struct NamedCodeVisitor;

                impl Visitor<'_> for NamedCodeVisitor {
                    type Value = $type;

                    fn expecting(
                        &self,
                        formatter: &mut std::fmt::Formatter<'_>,
                    ) -> std::fmt::Result {
                        formatter.write_str($expecting)
                    }

                    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
                    where
                        E: de::Error,
                    {
                        match value {
                            $($wire => Ok($type::$variant),)+
                            _ => Err(de::Error::unknown_variant(value, &[$($wire),+])),
                        }
                    }
                }

                deserializer.deserialize_str(NamedCodeVisitor)
            }
        }
    };
}

impl_named_code_deserialize!(OperationErrorCode, "a named OperationErrorCode", {
    SideEffectFailed => "side_effect_failed",
});
impl_named_code_deserialize!(CancellationReason, "a named CancellationReason", {
    Superseded => "superseded",
});
impl_named_code_deserialize!(
    OperationUncertaintyCode,
    "a named OperationUncertaintyCode",
    { AmbiguousDispatch => "ambiguous_dispatch" }
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeFenceError {
    PartialResourceFence,
    InvalidSourceForKind,
    EmptyExternalIdentity,
    ExternalIdentityTooLong,
}

impl std::fmt::Display for OutcomeFenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PartialResourceFence => {
                write!(
                    f,
                    "resource_id and runtime_generation must both be present or both absent"
                )
            }
            Self::InvalidSourceForKind => {
                write!(
                    f,
                    "outcome source is not valid for the requested outcome kind"
                )
            }
            Self::EmptyExternalIdentity => {
                write!(f, "external identity must be non-empty canonical text")
            }
            Self::ExternalIdentityTooLong => {
                write!(
                    f,
                    "external identity exceeds {MAX_EXTERNAL_IDENTITY_BYTES} bytes"
                )
            }
        }
    }
}

impl std::error::Error for OutcomeFenceError {}

pub fn validate_outcome_fence(
    resource_id: Option<ResourceId>,
    runtime_generation: Option<u64>,
) -> Result<(), OutcomeFenceError> {
    match (resource_id, runtime_generation) {
        (None, None) | (Some(_), Some(_)) => Ok(()),
        _ => Err(OutcomeFenceError::PartialResourceFence),
    }
}

fn canonicalize_external_identity(value: impl Into<String>) -> Result<String, OutcomeFenceError> {
    let Some(canonical) = canonical::canonicalize(value.into()) else {
        return Err(OutcomeFenceError::EmptyExternalIdentity);
    };
    if canonical.len() > MAX_EXTERNAL_IDENTITY_BYTES {
        return Err(OutcomeFenceError::ExternalIdentityTooLong);
    }
    Ok(canonical)
}

fn require_canonical_external_identity(value: &str) -> Result<(), OutcomeFenceError> {
    if value.is_empty() || !canonical::is_canonical(value) {
        return Err(OutcomeFenceError::EmptyExternalIdentity);
    }
    if value.len() > MAX_EXTERNAL_IDENTITY_BYTES {
        return Err(OutcomeFenceError::ExternalIdentityTooLong);
    }
    Ok(())
}

/// Paired resource identity and runtime generation fence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceFence {
    pub resource_id: ResourceId,
    pub runtime_generation: u64,
}

impl ResourceFence {
    pub fn new(resource_id: ResourceId, runtime_generation: u64) -> Self {
        Self {
            resource_id,
            runtime_generation,
        }
    }

    pub fn from_parts(
        resource_id: Option<ResourceId>,
        runtime_generation: Option<u64>,
    ) -> Result<Option<Self>, OutcomeFenceError> {
        validate_outcome_fence(resource_id, runtime_generation)?;
        Ok(match (resource_id, runtime_generation) {
            (Some(resource_id), Some(runtime_generation)) => {
                Some(Self::new(resource_id, runtime_generation))
            }
            (None, None) => None,
            _ => unreachable!("validate_outcome_fence rejects partial fences"),
        })
    }

    pub fn into_parts(fence: Option<Self>) -> (Option<ResourceId>, Option<u64>) {
        match fence {
            Some(fence) => (Some(fence.resource_id), Some(fence.runtime_generation)),
            None => (None, None),
        }
    }
}

/// Provenance of an operation outcome. Reconciliation evidence is durable text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutcomeSource {
    Dispatch,
    VerifiedReconciliation {
        effect_index: u32,
        external_identity: String,
    },
}

impl OutcomeSource {
    pub fn verified_reconciliation(
        effect_index: u32,
        external_identity: impl Into<String>,
    ) -> Result<Self, OutcomeFenceError> {
        Ok(Self::VerifiedReconciliation {
            effect_index,
            external_identity: canonicalize_external_identity(external_identity)?,
        })
    }

    pub fn validate(&self) -> Result<(), OutcomeFenceError> {
        match self {
            Self::Dispatch => Ok(()),
            Self::VerifiedReconciliation {
                external_identity, ..
            } => require_canonical_external_identity(external_identity),
        }
    }

    pub fn is_dispatch(&self) -> bool {
        matches!(self, Self::Dispatch)
    }

    pub fn is_verified_reconciliation(&self) -> bool {
        matches!(self, Self::VerifiedReconciliation { .. })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct VerifiedReconciliationWire {
    effect_index: u32,
    external_identity: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum OutcomeSourceWire {
    Dispatch,
    VerifiedReconciliation(VerifiedReconciliationWire),
}

impl Serialize for OutcomeSource {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.validate().map_err(ser::Error::custom)?;
        let wire = match self {
            Self::Dispatch => OutcomeSourceWire::Dispatch,
            Self::VerifiedReconciliation {
                effect_index,
                external_identity,
            } => OutcomeSourceWire::VerifiedReconciliation(VerifiedReconciliationWire {
                effect_index: *effect_index,
                external_identity: external_identity.clone(),
            }),
        };
        wire.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for OutcomeSource {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = OutcomeSourceWire::deserialize(deserializer)?;
        match wire {
            OutcomeSourceWire::Dispatch => Ok(Self::Dispatch),
            OutcomeSourceWire::VerifiedReconciliation(inner) => {
                // Strict wire path: reject non-canonical text instead of trimming.
                require_canonical_external_identity(&inner.external_identity)
                    .map_err(de::Error::custom)?;
                Ok(Self::VerifiedReconciliation {
                    effect_index: inner.effect_index,
                    external_identity: inner.external_identity,
                })
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum OperationOutcomeKind {
    Settled { result_event_ids: Vec<EventId> },
    Failed { code: OperationErrorCode },
    Cancelled { reason: CancellationReason },
    Uncertain { code: OperationUncertaintyCode },
}

impl OperationOutcomeKind {
    pub fn allows_verified_reconciliation(&self) -> bool {
        matches!(self, Self::Settled { .. } | Self::Failed { .. })
    }
}

/// Side-effect outcome observation. Does not duplicate command_id; the store derives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationOutcome {
    pub operation_id: OperationId,
    pub occurred_at_ms: i64,
    pub action_epoch: Option<u64>,
    pub resource_fence: Option<ResourceFence>,
    pub source: OutcomeSource,
    pub kind: OperationOutcomeKind,
}

impl OperationOutcome {
    pub fn new(
        operation_id: OperationId,
        occurred_at_ms: i64,
        action_epoch: Option<u64>,
        resource_fence: Option<ResourceFence>,
        source: OutcomeSource,
        kind: OperationOutcomeKind,
    ) -> Result<Self, OutcomeFenceError> {
        source.validate()?;
        validate_source_for_kind(&source, &kind)?;
        Ok(Self {
            operation_id,
            occurred_at_ms,
            action_epoch,
            resource_fence,
            source,
            kind,
        })
    }

    pub fn validate(&self) -> Result<(), OutcomeFenceError> {
        self.source.validate()?;
        validate_source_for_kind(&self.source, &self.kind)
    }
}

pub fn validate_source_for_kind(
    source: &OutcomeSource,
    kind: &OperationOutcomeKind,
) -> Result<(), OutcomeFenceError> {
    match (source, kind.allows_verified_reconciliation()) {
        (OutcomeSource::Dispatch, _) => Ok(()),
        (OutcomeSource::VerifiedReconciliation { .. }, true) => Ok(()),
        (OutcomeSource::VerifiedReconciliation { .. }, false) => {
            Err(OutcomeFenceError::InvalidSourceForKind)
        }
    }
}

/// Settled/failed durable facts may carry either dispatch or verified-reconciliation source.
pub fn validate_terminal_fact_source(source: &OutcomeSource) -> Result<(), OutcomeFenceError> {
    source.validate()
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OperationOutcomeWire {
    operation_id: OperationId,
    occurred_at_ms: i64,
    action_epoch: Option<u64>,
    resource_fence: Option<ResourceFence>,
    source: OutcomeSource,
    kind: OperationOutcomeKind,
}

impl Serialize for OperationOutcome {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.validate().map_err(ser::Error::custom)?;
        OperationOutcomeWire {
            operation_id: self.operation_id,
            occurred_at_ms: self.occurred_at_ms,
            action_epoch: self.action_epoch,
            resource_fence: self.resource_fence,
            source: self.source.clone(),
            kind: self.kind.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for OperationOutcome {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = OperationOutcomeWire::deserialize(deserializer)?;
        Self::new(
            wire.operation_id,
            wire.occurred_at_ms,
            wire.action_epoch,
            wire.resource_fence,
            wire.source,
            wire.kind,
        )
        .map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperationState {
    Accepted,
    Settled {
        settled_at_ms: i64,
        result_event_ids: Vec<EventId>,
    },
    Failed {
        settled_at_ms: i64,
        code: OperationErrorCode,
    },
    Cancelled {
        settled_at_ms: i64,
        reason: CancellationReason,
    },
    Uncertain {
        observed_at_ms: i64,
        code: OperationUncertaintyCode,
    },
}

struct OperationStatePayloadRef<'a, A: ?Sized, B: ?Sized> {
    first_name: &'static str,
    first: &'a A,
    second_name: &'static str,
    second: &'a B,
}

impl<A, B> Serialize for OperationStatePayloadRef<'_, A, B>
where
    A: Serialize + ?Sized,
    B: Serialize + ?Sized,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry(self.first_name, self.first)?;
        map.serialize_entry(self.second_name, self.second)?;
        map.end()
    }
}

fn serialize_operation_state_variant<S, A, B>(
    serializer: S,
    variant: &'static str,
    first_name: &'static str,
    first: &A,
    second_name: &'static str,
    second: &B,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
    A: Serialize + ?Sized,
    B: Serialize + ?Sized,
{
    let mut map = serializer.serialize_map(Some(1))?;
    map.serialize_entry(
        variant,
        &OperationStatePayloadRef {
            first_name,
            first,
            second_name,
            second,
        },
    )?;
    map.end()
}

impl Serialize for OperationState {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Accepted => serializer.serialize_str("accepted"),
            Self::Settled {
                settled_at_ms,
                result_event_ids,
            } => serialize_operation_state_variant(
                serializer,
                "settled",
                "settled_at_ms",
                settled_at_ms,
                "result_event_ids",
                result_event_ids,
            ),
            Self::Failed {
                settled_at_ms,
                code,
            } => serialize_operation_state_variant(
                serializer,
                "failed",
                "settled_at_ms",
                settled_at_ms,
                "code",
                code,
            ),
            Self::Cancelled {
                settled_at_ms,
                reason,
            } => serialize_operation_state_variant(
                serializer,
                "cancelled",
                "settled_at_ms",
                settled_at_ms,
                "reason",
                reason,
            ),
            Self::Uncertain {
                observed_at_ms,
                code,
            } => serialize_operation_state_variant(
                serializer,
                "uncertain",
                "observed_at_ms",
                observed_at_ms,
                "code",
                code,
            ),
        }
    }
}

enum OperationStateMapVariant {
    Settled,
    Failed,
    Cancelled,
    Uncertain,
}

impl<'de> Deserialize<'de> for OperationStateMapVariant {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct VariantVisitor;

        impl Visitor<'_> for VariantVisitor {
            type Value = OperationStateMapVariant;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("settled, failed, cancelled, or uncertain")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "settled" => Ok(OperationStateMapVariant::Settled),
                    "failed" => Ok(OperationStateMapVariant::Failed),
                    "cancelled" => Ok(OperationStateMapVariant::Cancelled),
                    "uncertain" => Ok(OperationStateMapVariant::Uncertain),
                    _ => Err(de::Error::unknown_variant(
                        value,
                        &["settled", "failed", "cancelled", "uncertain"],
                    )),
                }
            }
        }

        deserializer.deserialize_identifier(VariantVisitor)
    }
}

struct OperationStatePayloadSeed<A, B> {
    first_name: &'static str,
    second_name: &'static str,
    marker: PhantomData<fn() -> (A, B)>,
}

impl<A, B> OperationStatePayloadSeed<A, B> {
    const fn new(first_name: &'static str, second_name: &'static str) -> Self {
        Self {
            first_name,
            second_name,
            marker: PhantomData,
        }
    }
}

impl<'de, A, B> DeserializeSeed<'de> for OperationStatePayloadSeed<A, B>
where
    A: Deserialize<'de>,
    B: Deserialize<'de>,
{
    type Value = (A, B);

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(OperationStatePayloadVisitor {
            first_name: self.first_name,
            second_name: self.second_name,
            marker: PhantomData,
        })
    }
}

struct OperationStatePayloadVisitor<A, B> {
    first_name: &'static str,
    second_name: &'static str,
    marker: PhantomData<fn() -> (A, B)>,
}

impl<'de, A, B> Visitor<'de> for OperationStatePayloadVisitor<A, B>
where
    A: Deserialize<'de>,
    B: Deserialize<'de>,
{
    type Value = (A, B);

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "a named operation-state payload map with {} and {}",
            self.first_name, self.second_name
        )
    }

    fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
    where
        M: MapAccess<'de>,
    {
        let mut first = None;
        let mut second = None;

        while let Some(field) = map.next_key::<String>()? {
            if field == self.first_name {
                if first.is_some() {
                    return Err(de::Error::custom(format_args!(
                        "duplicate field `{}`",
                        self.first_name
                    )));
                }
                first = Some(map.next_value()?);
            } else if field == self.second_name {
                if second.is_some() {
                    return Err(de::Error::custom(format_args!(
                        "duplicate field `{}`",
                        self.second_name
                    )));
                }
                second = Some(map.next_value()?);
            } else {
                return Err(de::Error::custom(format_args!(
                    "unknown field `{field}`, expected `{}` or `{}`",
                    self.first_name, self.second_name
                )));
            }
        }

        Ok((
            first.ok_or_else(|| {
                de::Error::custom(format_args!("missing field `{}`", self.first_name))
            })?,
            second.ok_or_else(|| {
                de::Error::custom(format_args!("missing field `{}`", self.second_name))
            })?,
        ))
    }
}

impl<'de> Deserialize<'de> for OperationState {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct OperationStateVisitor;

        impl<'de> Visitor<'de> for OperationStateVisitor {
            type Value = OperationState;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("accepted or a one-entry named operation-state map")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "accepted" => Ok(OperationState::Accepted),
                    _ => Err(de::Error::unknown_variant(
                        value,
                        &["accepted", "settled", "failed", "cancelled", "uncertain"],
                    )),
                }
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let variant = map
                    .next_key()?
                    .ok_or_else(|| de::Error::custom("OperationState map variant is missing"))?;
                let state = match variant {
                    OperationStateMapVariant::Settled => {
                        let (settled_at_ms, result_event_ids) = map.next_value_seed(
                            OperationStatePayloadSeed::<i64, Vec<EventId>>::new(
                                "settled_at_ms",
                                "result_event_ids",
                            ),
                        )?;
                        OperationState::Settled {
                            settled_at_ms,
                            result_event_ids,
                        }
                    }
                    OperationStateMapVariant::Failed => {
                        let (settled_at_ms, code) =
                            map.next_value_seed(OperationStatePayloadSeed::<
                                i64,
                                OperationErrorCode,
                            >::new(
                                "settled_at_ms", "code"
                            ))?;
                        OperationState::Failed {
                            settled_at_ms,
                            code,
                        }
                    }
                    OperationStateMapVariant::Cancelled => {
                        let (settled_at_ms, reason) =
                            map.next_value_seed(OperationStatePayloadSeed::<
                                i64,
                                CancellationReason,
                            >::new(
                                "settled_at_ms", "reason"
                            ))?;
                        OperationState::Cancelled {
                            settled_at_ms,
                            reason,
                        }
                    }
                    OperationStateMapVariant::Uncertain => {
                        let (observed_at_ms, code) =
                            map.next_value_seed(OperationStatePayloadSeed::<
                                i64,
                                OperationUncertaintyCode,
                            >::new(
                                "observed_at_ms", "code"
                            ))?;
                        OperationState::Uncertain {
                            observed_at_ms,
                            code,
                        }
                    }
                };
                if map.next_key::<de::IgnoredAny>()?.is_some() {
                    return Err(de::Error::custom(
                        "OperationState must contain exactly one variant",
                    ));
                }
                Ok(state)
            }
        }

        deserializer.deserialize_any(OperationStateVisitor)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationFacts {
    pub id: OperationId,
    pub command_id: CommandId,
    pub task_id: Option<TaskId>,
    pub state: OperationState,
    pub accepted_at_ms: i64,
}

impl OperationFacts {
    pub fn accepted(command_id: CommandId, task_id: Option<TaskId>, accepted_at_ms: i64) -> Self {
        Self {
            id: OperationId::new(),
            command_id,
            task_id,
            state: OperationState::Accepted,
            accepted_at_ms,
        }
    }
}
