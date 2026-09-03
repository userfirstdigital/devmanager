//! Crate-private outbox effect planning types and strict MessagePack codecs.
//!
//! No provider/process control lives here — only durable effect documents and
//! the replay policy/destination columns they must agree with.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::domain::agent::ProviderSessionId;
use crate::domain::browser::{BrowserDurableFact, BrowserIntegrationHold};
use crate::domain::command::{CommandReceipt, RejectionCode};
use crate::domain::event::Event;
use crate::domain::id::{
    AgentSessionId, ApprovalId, BrowserContextId, BrowserRequestId, ClientId, CommandId, EventId,
    OperationId, QuestionId, ResourceId, TaskId, TurnId,
};
use crate::domain::operation::ResourceFence;
use crate::domain::provider_input::ProviderInputAction;
use crate::domain::resource::{OwnerKind, ResourceLifecycle};
use crate::domain::snapshot::TaskSnapshot;
use crate::kernel::store::StoreError;
use crate::providers::ProviderKind;

pub(crate) const RECEIPT_SCHEMA_VERSION: u32 = 1;
pub(crate) const EFFECT_SCHEMA_VERSION: u32 = 1;
const MAX_EFFECT_DOCUMENT_BYTES: usize = 256 * 1024;

/// Stable destination class stored in `outbox.destination_class`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DestinationClass {
    TaskTeardown,
    ResourceRelease,
    ProviderInput,
    BrowserHost,
}

impl DestinationClass {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::TaskTeardown => "task_teardown",
            Self::ResourceRelease => "resource_release",
            Self::ProviderInput => "provider_input",
            Self::BrowserHost => "browser_host",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, StoreError> {
        match value {
            "task_teardown" => Ok(Self::TaskTeardown),
            "resource_release" => Ok(Self::ResourceRelease),
            "provider_input" => Ok(Self::ProviderInput),
            "browser_host" => Ok(Self::BrowserHost),
            other => Err(StoreError::CodecMismatch {
                detail: format!("unknown destination_class '{other}'"),
            }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayPolicy {
    RetrySafe,
    ReconcileBeforeRetry,
    NoAutomaticRetry,
}

impl ReplayPolicy {
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
pub enum Effect {
    BeginTaskTeardown {
        task_id: TaskId,
        action_epoch: u64,
    },
    ReleaseResource {
        task_id: TaskId,
        action_epoch: u64,
        resource_fence: ResourceFence,
    },
    DeliverProviderInput {
        task_id: TaskId,
        operation_id: OperationId,
        command_id: CommandId,
        client_id: ClientId,
        agent_session_id: AgentSessionId,
        provider_kind: ProviderKind,
        provider_session_id: Option<ProviderSessionId>,
        runtime_generation: u64,
        action_epoch: u64,
        turn_id: TurnId,
        question_id: Option<QuestionId>,
        approval_id: Option<ApprovalId>,
        action: ProviderInputAction,
        wait: bool,
    },
    HoldBrowserHost {
        task_id: TaskId,
        action_epoch: u64,
        request_id: BrowserRequestId,
        context_id: BrowserContextId,
        generation: u64,
        hold: BrowserIntegrationHold,
    },
}

impl Effect {
    pub fn browser_host_hold_identity(
        &self,
    ) -> Option<(TaskId, u64, BrowserRequestId, BrowserContextId, u64)> {
        match self {
            Self::HoldBrowserHost {
                task_id,
                action_epoch,
                request_id,
                context_id,
                generation,
                ..
            } => Some((
                *task_id,
                *action_epoch,
                *request_id,
                *context_id,
                *generation,
            )),
            _ => None,
        }
    }
}

/// Accepted-operation fence shared by every planned effect for one command.
///
/// `DeliverProviderInput` intentionally leaves `runtime_generation` empty: the
/// generic operation fence is presentation-only for provider delivery because
/// its resource/generation pair is reserved for resource ownership. The typed
/// provider effect is the durable authority for provider session identity and
/// runtime generation, including ambiguity recovery after replacement. Uncertain
/// outbox rows retain their payload and are never terminal-payload compacted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OperationFence {
    pub action_epoch: Option<u64>,
    pub resource_id: Option<ResourceId>,
    pub runtime_generation: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlannedEffect {
    pub document: PlannedEffectDocument,
    pub fence: OperationFence,
}

/// Deterministic external retry identity. Stable across retries; not the row PK.
#[cfg_attr(not(test), allow(dead_code))] // exercised by dispatch/outcome slices next
pub(crate) fn external_idempotency_key(operation_id: OperationId, effect_index: u32) -> String {
    format!("v1:{operation_id}:{effect_index}")
}

/// Pure planner: maps accepted decision facts + pre-command snapshot to effects.
/// No clock, RNG, SQLite, filesystem, process, or provider calls.
/// Decision facts that settle without host side effects. Keep in lockstep with
/// [`plan_effects`] pure arms (including Browser facts that do not require host
/// settlement).
pub(crate) fn is_pure_slice_decision_fact(event: &Event) -> bool {
    match event {
        Event::TaskCreated { .. }
        | Event::TaskRenamed { .. }
        | Event::TaskAttentionSet { .. }
        | Event::TaskSettled
        | Event::TaskReopened
        | Event::TaskArchived
        | Event::TaskDeleted
        | Event::AgentSessionRegistered { .. }
        | Event::AgentProviderSessionBound { .. }
        | Event::PrimaryAgentSet { .. }
        | Event::UnstartedPrimaryProviderRebound { .. }
        | Event::SpecialistRequested { .. }
        | Event::PrimaryPromoted { .. }
        | Event::SpecialistHandoffRecorded { .. }
        | Event::SpecialistClosed { .. }
        | Event::ArtifactRegistered { .. }
        | Event::ResourceRegistered { .. }
        | Event::ResourceReleased { .. }
        | Event::ProviderQuestionPresented { .. }
        | Event::ProviderApprovalPresented { .. }
        | Event::ProviderWaitSettled { .. }
        // Terminal renames and strip edits are recorded state: they consume a
        // task revision and plan no host effect of their own.
        | Event::TerminalRenamed { .. }
        | Event::TaskTerminalStripSet { .. } => true,
        Event::Browser(fact) => !browser_fact_requires_host_settlement(fact),
        // Never a decision fact: the purge sweep writes it directly
        // over an existing row, so it is never appended by a command
        // and never planned. `is_side_effect_decision_fact` also
        // answers false, so a batch that somehow contained one is
        // rejected rather than planned as a no-op.
        Event::Purged
        | Event::TaskCloseBegun { .. }
        | Event::ResourceReleaseBegun { .. }
        | Event::ProviderInputAccepted { .. }
        | Event::OperationAccepted(_)
        | Event::OperationSettled(_)
        | Event::OperationFailed(_)
        | Event::OperationCancelled(_)
        | Event::OperationUncertain(_)
        | Event::ProviderInputDelivered { .. }
        | Event::HostCloseBegun { .. }
        | Event::HostCleanupBranchCompleted { .. }
        // Host-reported terminal facts carry no task revision, so they can
        // never be a decision fact in a pure command's batch (which requires
        // `task_revision: Some`) — same reason ProviderInputDelivered is here.
        | Event::TerminalCwdReported { .. }
        | Event::TerminalExited { .. }
        | Event::TerminalActivity { .. } => false,
    }
}

/// Decision facts that plan exactly one host side effect. Keep in lockstep with
/// [`plan_effects`] side-effect arms.
pub(crate) fn is_side_effect_decision_fact(event: &Event) -> bool {
    match event {
        Event::TaskCloseBegun { .. }
        | Event::ResourceReleaseBegun { .. }
        | Event::ProviderInputAccepted { .. } => true,
        Event::Browser(fact) => browser_fact_requires_host_settlement(fact),
        _ => false,
    }
}

fn browser_fact_requires_host_settlement(fact: &BrowserDurableFact) -> bool {
    matches!(
        fact,
        BrowserDurableFact::RequestAccepted { action, .. } if action.requires_host_settlement()
    )
}

pub(crate) fn plan_effects(
    snapshot: Option<&TaskSnapshot>,
    task_id: TaskId,
    decision: &[Event],
) -> Result<Vec<PlannedEffect>, StoreError> {
    let mut planned = Vec::new();
    let mut effect_fact_count = 0usize;
    let mut pure_fact_count = 0usize;

    for event in decision {
        match event {
            Event::TaskCloseBegun { action_epoch } => {
                effect_fact_count = effect_fact_count
                    .checked_add(1)
                    .ok_or(StoreError::Corruption)?;
                let Some(snap) = snapshot else {
                    return Err(StoreError::Projection(
                        "task teardown planning requires a pre-command snapshot".into(),
                    ));
                };
                if snap.task.id != task_id {
                    return Err(StoreError::Projection(
                        "task teardown planning task scope mismatch".into(),
                    ));
                }
                if !matches!(
                    snap.task.lifecycle,
                    crate::domain::task::TaskLifecycle::Open
                        | crate::domain::task::TaskLifecycle::Settled
                ) {
                    return Err(StoreError::Projection(
                        "task teardown planning requires Open or Settled lifecycle".into(),
                    ));
                }
                let expected_epoch =
                    snap.task
                        .action_epoch
                        .checked_add(1)
                        .ok_or(StoreError::Projection(
                            "task teardown planning action_epoch overflow".into(),
                        ))?;
                if *action_epoch != expected_epoch {
                    return Err(StoreError::Projection(
                        "task teardown planning action_epoch must be snapshot.action_epoch + 1"
                            .into(),
                    ));
                }
                planned.push(PlannedEffect {
                    document: PlannedEffectDocument::new(
                        Effect::BeginTaskTeardown {
                            task_id,
                            action_epoch: *action_epoch,
                        },
                        ReplayPolicy::RetrySafe,
                    ),
                    fence: OperationFence {
                        action_epoch: Some(*action_epoch),
                        resource_id: None,
                        runtime_generation: None,
                    },
                });
            }
            Event::ResourceReleaseBegun {
                resource_id,
                runtime_generation,
            } => {
                effect_fact_count = effect_fact_count
                    .checked_add(1)
                    .ok_or(StoreError::Corruption)?;
                let Some(snap) = snapshot else {
                    return Err(StoreError::Projection(
                        "resource release planning requires a pre-command snapshot".into(),
                    ));
                };
                if snap.task.id != task_id {
                    return Err(StoreError::Projection(
                        "resource release planning task scope mismatch".into(),
                    ));
                }
                let Some(resource) = snap.resources.get(resource_id) else {
                    return Err(StoreError::Projection(
                        "resource release planning missing pre-command resource".into(),
                    ));
                };
                if resource.owner_kind != OwnerKind::Task
                    || resource.task_id != Some(task_id)
                    || resource.lifecycle != ResourceLifecycle::Active
                    || resource.runtime_generation != *runtime_generation
                {
                    return Err(StoreError::Projection(
                        "resource release planning requires task-owned Active resource with exact generation"
                            .into(),
                    ));
                }
                planned.push(PlannedEffect {
                    document: PlannedEffectDocument::new(
                        Effect::ReleaseResource {
                            task_id,
                            action_epoch: snap.task.action_epoch,
                            resource_fence: ResourceFence::new(*resource_id, *runtime_generation),
                        },
                        ReplayPolicy::ReconcileBeforeRetry,
                    ),
                    fence: OperationFence {
                        action_epoch: Some(snap.task.action_epoch),
                        resource_id: Some(*resource_id),
                        runtime_generation: Some(*runtime_generation),
                    },
                });
            }
            Event::ProviderInputAccepted {
                operation_id,
                command_id,
                client_id,
                agent_session_id,
                provider_kind,
                provider_session_id,
                runtime_generation,
                turn_id,
                action_epoch,
                question_id,
                approval_id,
                action,
                wait,
                ..
            } => {
                effect_fact_count = effect_fact_count
                    .checked_add(1)
                    .ok_or(StoreError::Corruption)?;
                let Some(snap) = snapshot else {
                    return Err(StoreError::Projection(
                        "provider input planning requires a pre-command snapshot".into(),
                    ));
                };
                if snap.task.id != task_id {
                    return Err(StoreError::Projection(
                        "provider input planning task scope mismatch".into(),
                    ));
                }
                let Some(agent) = snap.agents.get(agent_session_id) else {
                    return Err(StoreError::Projection(
                        "provider input planning missing agent session".into(),
                    ));
                };
                if agent.provider_session_id != *provider_session_id
                    || agent.provider_kind != *provider_kind
                    || agent.runtime_generation != *runtime_generation
                {
                    return Err(StoreError::Projection(
                        "provider input planning provider identity mismatch".into(),
                    ));
                }
                planned.push(PlannedEffect {
                    document: PlannedEffectDocument::new(
                        Effect::DeliverProviderInput {
                            task_id,
                            operation_id: *operation_id,
                            command_id: *command_id,
                            client_id: *client_id,
                            agent_session_id: *agent_session_id,
                            provider_kind: provider_kind.clone(),
                            provider_session_id: provider_session_id.clone(),
                            runtime_generation: *runtime_generation,
                            action_epoch: *action_epoch,
                            turn_id: *turn_id,
                            question_id: *question_id,
                            approval_id: *approval_id,
                            action: action.clone(),
                            wait: *wait,
                        },
                        ReplayPolicy::NoAutomaticRetry,
                    ),
                    fence: OperationFence {
                        action_epoch: Some(*action_epoch),
                        resource_id: None,
                        // Provider runtime identity is carried by the typed
                        // provider effect/fence. The generic operation fence
                        // only permits a runtime generation when it is paired
                        // with a resource id.
                        runtime_generation: None,
                    },
                });
            }
            Event::Browser(fact) => match fact {
                crate::domain::browser::BrowserDurableFact::RequestAccepted {
                    request_id,
                    task_id: fact_task,
                    context_id,
                    generation,
                    action,
                    action_epoch,
                    ..
                } if action.requires_host_settlement() => {
                    if *fact_task != task_id {
                        return Err(StoreError::Projection(
                            "browser host HOLD task scope mismatch".into(),
                        ));
                    }
                    if *generation == 0 {
                        return Err(StoreError::Projection(
                            "browser host HOLD generation must be nonzero".into(),
                        ));
                    }
                    effect_fact_count = effect_fact_count
                        .checked_add(1)
                        .ok_or(StoreError::Corruption)?;
                    planned.push(PlannedEffect {
                        document: PlannedEffectDocument::new(
                            Effect::HoldBrowserHost {
                                task_id,
                                action_epoch: *action_epoch,
                                request_id: *request_id,
                                context_id: *context_id,
                                generation: *generation,
                                hold: crate::domain::browser::BrowserIntegrationHold::WebViewSurfaceAbsent,
                            },
                            ReplayPolicy::NoAutomaticRetry,
                        ),
                        fence: OperationFence {
                            action_epoch: Some(*action_epoch),
                            resource_id: None,
                            runtime_generation: None,
                        },
                    });
                }
                _ => {
                    pure_fact_count = pure_fact_count
                        .checked_add(1)
                        .ok_or(StoreError::Corruption)?;
                }
            },
            Event::TaskCreated { .. }
            | Event::TaskRenamed { .. }
            | Event::TaskAttentionSet { .. }
            | Event::TaskSettled
            | Event::TaskReopened
            | Event::TaskArchived
            | Event::TaskDeleted
            | Event::AgentSessionRegistered { .. }
            | Event::AgentProviderSessionBound { .. }
            | Event::PrimaryAgentSet { .. }
            | Event::UnstartedPrimaryProviderRebound { .. }
            | Event::SpecialistRequested { .. }
            | Event::PrimaryPromoted { .. }
            | Event::SpecialistHandoffRecorded { .. }
            | Event::SpecialistClosed { .. }
            | Event::ArtifactRegistered { .. }
            | Event::ResourceRegistered { .. }
            | Event::ResourceReleased { .. }
            | Event::ProviderQuestionPresented { .. }
            | Event::ProviderApprovalPresented { .. }
            | Event::ProviderWaitSettled { .. }
            // Terminal renames and strip edits plan no host effect; keep in
            // lockstep with `is_pure_slice_decision_fact`.
            | Event::TerminalRenamed { .. }
            | Event::TaskTerminalStripSet { .. } => {
                pure_fact_count = pure_fact_count
                    .checked_add(1)
                    .ok_or(StoreError::Corruption)?;
            }
            Event::OperationAccepted(_)
            | Event::OperationSettled(_)
            | Event::OperationFailed(_)
            | Event::OperationCancelled(_)
            | Event::OperationUncertain(_)
            | Event::ProviderInputDelivered { .. }
            | Event::HostCloseBegun { .. }
            | Event::HostCleanupBranchCompleted { .. }
            | Event::TerminalCwdReported { .. }
            | Event::TerminalExited { .. }
            | Event::TerminalActivity { .. } => {
                return Err(StoreError::Projection(
                    "operation outcome facts are not decision inputs for effect planning".into(),
                ));
            }
            // A redacted row, reachable here only if something fed the
            // planner a decoded event log. Loud, not silent: the purge
            // sweep rewrites rows in place and appends nothing.
            Event::Purged => {
                return Err(StoreError::Projection(
                    "event.purged is a redacted row, never a decision fact".into(),
                ));
            }
        }
    }

    if effect_fact_count > 0 && pure_fact_count > 0 {
        return Err(StoreError::Projection(
            "mixed pure and side-effect decision facts cannot be planned together".into(),
        ));
    }
    if effect_fact_count > 1 {
        return Err(StoreError::Projection(
            "current side-effect commands plan exactly one decision fact".into(),
        ));
    }
    if effect_fact_count != planned.len() {
        return Err(StoreError::Projection(
            "planner dropped a required side-effect decision fact".into(),
        ));
    }
    if planned.len() > 1 {
        return Err(StoreError::Projection(
            "current side-effect commands emit exactly one planned effect".into(),
        ));
    }
    if let Some(first) = planned.first() {
        for effect in &planned[1..] {
            if effect.fence != first.fence {
                return Err(StoreError::Projection(
                    "planned effects disagree on accepted operation fence".into(),
                ));
            }
        }
    }
    Ok(planned)
}

impl Effect {
    pub(crate) fn destination_class(&self) -> DestinationClass {
        match self {
            Self::BeginTaskTeardown { .. } => DestinationClass::TaskTeardown,
            Self::ReleaseResource { .. } => DestinationClass::ResourceRelease,
            Self::DeliverProviderInput { .. } => DestinationClass::ProviderInput,
            Self::HoldBrowserHost { .. } => DestinationClass::BrowserHost,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlannedEffectDocument {
    pub schema_version: u32,
    pub destination_class: DestinationClass,
    pub replay_policy: ReplayPolicy,
    pub effect: Effect,
}

impl PlannedEffectDocument {
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
    let payload = rmp_serde::to_vec_named(&wire).map_err(|err| StoreError::CodecMismatch {
        detail: err.to_string(),
    })?;
    if payload.len() > MAX_EFFECT_DOCUMENT_BYTES {
        return Err(StoreError::CodecMismatch {
            detail: format!("effect document exceeds {MAX_EFFECT_DOCUMENT_BYTES} bytes"),
        });
    }
    Ok(payload)
}

pub(crate) fn effect_document_sha256(doc: &PlannedEffectDocument) -> Result<[u8; 32], StoreError> {
    let encoded = encode_effect_document(doc)?;
    let digest = Sha256::digest(encoded);
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    Ok(out)
}

pub(crate) fn decode_effect_document(
    payload: &[u8],
    destination_class_column: &str,
    replay_policy_column: &str,
) -> Result<PlannedEffectDocument, StoreError> {
    if payload.len() > MAX_EFFECT_DOCUMENT_BYTES {
        return Err(StoreError::CodecMismatch {
            detail: format!("effect document exceeds {MAX_EFFECT_DOCUMENT_BYTES} bytes"),
        });
    }
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt_mutation: Option<Vec<u8>>,
    },
    Rejected {
        command_id: CommandId,
        code: RejectionCode,
        current_revision: Option<u64>,
        #[serde(default)]
        resolution: Option<crate::domain::ProviderResolutionWinner>,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiptDocumentWire {
    schema_version: u32,
    receipt: ReceiptBodyWire,
}

pub(crate) fn encode_receipt_document(receipt: &CommandReceipt) -> Result<Vec<u8>, StoreError> {
    let body = match receipt {
        CommandReceipt::Accepted {
            command_id,
            operation_id,
            task_revision,
            event_ids,
            prompt_mutation,
        } => ReceiptBodyWire::Accepted {
            command_id: *command_id,
            operation_id: *operation_id,
            task_revision: *task_revision,
            event_ids: event_ids.clone(),
            prompt_mutation: prompt_mutation
                .as_ref()
                .map(|mutation| {
                    mutation.encode().map_err(|err| StoreError::CodecMismatch {
                        detail: err.to_string(),
                    })
                })
                .transpose()?,
        },
        CommandReceipt::Rejected {
            command_id,
            code,
            current_revision,
            resolution,
        } => ReceiptBodyWire::Rejected {
            command_id: *command_id,
            code: *code,
            current_revision: *current_revision,
            resolution: resolution.clone(),
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
            prompt_mutation,
        } => CommandReceipt::Accepted {
            command_id,
            operation_id,
            task_revision,
            event_ids,
            prompt_mutation: prompt_mutation
                .map(|payload| {
                    crate::prompts::PromptMutationReceipt::decode(&payload).map_err(|err| {
                        StoreError::CodecMismatch {
                            detail: err.to_string(),
                        }
                    })
                })
                .transpose()?,
        },
        ReceiptBodyWire::Rejected {
            command_id,
            code,
            current_revision,
            resolution,
        } => CommandReceipt::Rejected {
            command_id,
            code,
            current_revision,
            resolution,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::id::{AgentSessionId, CommandId, EventId, OperationId, ResourceId, TaskId};

    fn fixed_uuid_v7(tail: u8) -> [u8; 16] {
        [
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, tail,
        ]
    }

    #[test]
    fn terminal_event_decision_classification_is_pinned() {
        let resource_id = ResourceId::from_bytes(fixed_uuid_v7(0x40)).unwrap();
        let pure = [
            Event::TerminalRenamed {
                resource_id,
                title: "build".to_string(),
            },
            Event::TaskTerminalStripSet {
                strip: Default::default(),
            },
        ];
        for event in &pure {
            assert!(
                is_pure_slice_decision_fact(event),
                "{} must be a pure decision fact",
                event.event_type()
            );
            assert!(!is_side_effect_decision_fact(event));
            assert!(event.is_task_mutation());
        }

        let host_facts = [
            Event::TerminalCwdReported {
                resource_id,
                cwd: std::path::PathBuf::from("C:/Code/demo"),
            },
            Event::TerminalExited {
                resource_id,
                code: Some(0),
                summary: "done".to_string(),
            },
            Event::TerminalActivity { resource_id },
        ];
        for event in &host_facts {
            // Host facts carry no task revision, so they can never be a
            // decision fact in a command batch.
            assert!(
                !is_pure_slice_decision_fact(event),
                "{} must not be a pure decision fact",
                event.event_type()
            );
            assert!(!is_side_effect_decision_fact(event));
            assert!(!event.is_task_mutation());
        }
    }

    /// The classification predicates above are pinned, but `plan_effects` has
    /// its own exhaustive match over the same events. Pin that too: the two
    /// mutations plan no host effect at all, and the three host facts are
    /// refused as decision inputs rather than silently planning nothing.
    #[test]
    fn terminal_events_plan_no_effects_and_host_facts_are_refused() {
        let task_id = TaskId::from_bytes(fixed_uuid_v7(0x50)).unwrap();
        let resource_id = ResourceId::from_bytes(fixed_uuid_v7(0x51)).unwrap();

        for event in [
            Event::TerminalRenamed {
                resource_id,
                title: "build".to_string(),
            },
            Event::TaskTerminalStripSet {
                strip: Default::default(),
            },
        ] {
            let planned =
                plan_effects(None, task_id, std::slice::from_ref(&event)).unwrap_or_else(|error| {
                    panic!("{} must plan cleanly, got {error:?}", event.event_type())
                });
            assert!(
                planned.is_empty(),
                "{} must plan no host effect",
                event.event_type()
            );
        }

        for event in [
            Event::TerminalCwdReported {
                resource_id,
                cwd: std::path::PathBuf::from("C:/Code/demo"),
            },
            Event::TerminalExited {
                resource_id,
                code: Some(0),
                summary: "done".to_string(),
            },
            Event::TerminalActivity { resource_id },
        ] {
            match plan_effects(None, task_id, std::slice::from_ref(&event)) {
                Err(StoreError::Projection(detail)) => assert!(
                    detail.contains("not decision inputs"),
                    "{} must be refused as a decision input, got {detail}",
                    event.event_type()
                ),
                other => panic!(
                    "{} must not be plannable, got {other:?}",
                    event.event_type()
                ),
            }
        }
    }

    #[test]
    fn command_contract_receipt_codec_version_and_unknown_fields() {
        let receipt = CommandReceipt::Accepted {
            command_id: CommandId::from_bytes(fixed_uuid_v7(0x01)).unwrap(),
            operation_id: OperationId::from_bytes(fixed_uuid_v7(0x02)).unwrap(),
            task_revision: Some(3),
            event_ids: vec![EventId::from_bytes(fixed_uuid_v7(0x03)).unwrap()],
            prompt_mutation: None,
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

    fn digest_hex(bytes: [u8; 32]) -> String {
        let mut out = String::with_capacity(64);
        for byte in bytes {
            use std::fmt::Write;
            let _ = write!(out, "{byte:02x}");
        }
        out
    }

    #[test]
    fn effect_document_digest_golden_values_guard_compacted_rows() {
        let teardown = PlannedEffectDocument::new(
            Effect::BeginTaskTeardown {
                task_id: TaskId::from_bytes(fixed_uuid_v7(0x11)).unwrap(),
                action_epoch: 2,
            },
            ReplayPolicy::RetrySafe,
        );
        assert_eq!(
            digest_hex(effect_document_sha256(&teardown).expect("teardown digest")),
            "6f88ecb0354de3f3b1938915e61e3eedf5238c3f0001c61301309cf1d9ae42f6"
        );

        let release = PlannedEffectDocument::new(
            Effect::ReleaseResource {
                task_id: TaskId::from_bytes(fixed_uuid_v7(0x12)).unwrap(),
                action_epoch: 3,
                resource_fence: ResourceFence::new(
                    ResourceId::from_bytes(fixed_uuid_v7(0x10)).unwrap(),
                    4,
                ),
            },
            ReplayPolicy::ReconcileBeforeRetry,
        );
        assert_eq!(
            digest_hex(effect_document_sha256(&release).expect("release digest")),
            "71cd5a27f493f539b1fe442bf98463762140b8db790dfe21c2eb3320fb4390d1"
        );
    }

    #[test]
    fn command_side_effect_planner_close_and_release_shapes() {
        use std::collections::BTreeMap;

        use crate::domain::agent::{AgentRole, AgentSessionFacts, AgentSessionLifecycle};
        use crate::domain::id::{EnvironmentId, ProjectId};
        use crate::domain::resource::{
            OwnerKind, ResourceFacts, ResourceKind, ResourceLifecycle, ResourceRecipe,
        };
        use crate::domain::task::{
            ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity,
            TaskFacts, TaskLifecycle, WorkspaceRef,
        };

        let task_id = TaskId::from_bytes(fixed_uuid_v7(0x21)).unwrap();
        let resource_id = ResourceId::from_bytes(fixed_uuid_v7(0x22)).unwrap();

        let mut resources = BTreeMap::new();
        resources.insert(
            resource_id,
            ResourceFacts {
                id: resource_id,
                task_id: Some(task_id),
                owner_kind: OwnerKind::Task,
                resource_kind: ResourceKind::Terminal,
                recipe: ResourceRecipe::terminal(80, 24),
                lifecycle: ResourceLifecycle::Active,
                runtime_generation: 9,
                updated_at_ms: 1,
            },
        );
        let snap = TaskSnapshot {
            task: TaskFacts {
                id: task_id,
                environment_id: EnvironmentId::from_bytes(fixed_uuid_v7(0x23)).unwrap(),
                title: "t".into(),
                description: None,
                project_id: ProjectId::from_bytes(fixed_uuid_v7(0x24)).unwrap(),
                workspace: WorkspaceRef::Main,
                assignment: TaskAssignment::LocalOwner,
                lifecycle: TaskLifecycle::Open,
                action_epoch: 3,
                revision: 1,
                created_at_ms: 1,
            },
            connectivity: TaskConnectivity::Connected,
            attention: TaskAttention::None,
            activity: TaskActivity::Idle,
            review_readiness: ReviewReadiness::NotReady,
            agents: BTreeMap::new(),
            primary_agent_id: None,
            artifacts: BTreeMap::new(),
            resources,
            provider_sessions: BTreeMap::new(),
            browser: crate::domain::browser::BrowserBook::new(),
            terminal_facts: Default::default(),
            terminal_strip: Default::default(),
        };

        let close = plan_effects(
            Some(&snap),
            task_id,
            &[Event::TaskCloseBegun { action_epoch: 4 }],
        )
        .expect("close plan");
        assert_eq!(close.len(), 1);
        assert_eq!(
            close[0].document.effect,
            Effect::BeginTaskTeardown {
                task_id,
                action_epoch: 4
            }
        );
        assert_eq!(close[0].document.replay_policy, ReplayPolicy::RetrySafe);
        assert_eq!(
            close[0].fence,
            OperationFence {
                action_epoch: Some(4),
                resource_id: None,
                runtime_generation: None,
            }
        );

        // Missing snapshot / wrong lifecycle / wrong epoch / duplicate teardown must fail.
        assert!(
            plan_effects(None, task_id, &[Event::TaskCloseBegun { action_epoch: 4 }]).is_err(),
            "TaskCloseBegun requires pre-command snapshot"
        );
        let mut closing = snap.clone();
        closing.task.lifecycle = TaskLifecycle::Closing;
        assert!(
            plan_effects(
                Some(&closing),
                task_id,
                &[Event::TaskCloseBegun { action_epoch: 4 }],
            )
            .is_err(),
            "TaskCloseBegun requires Open lifecycle"
        );
        assert!(
            plan_effects(
                Some(&snap),
                task_id,
                &[Event::TaskCloseBegun { action_epoch: 5 }],
            )
            .is_err(),
            "action_epoch must be snapshot.action_epoch + 1"
        );
        assert!(
            plan_effects(
                Some(&snap),
                task_id,
                &[
                    Event::TaskCloseBegun { action_epoch: 4 },
                    Event::TaskCloseBegun { action_epoch: 4 },
                ],
            )
            .is_err(),
            "duplicate teardown facts must fail"
        );

        let release = plan_effects(
            Some(&snap),
            task_id,
            &[Event::ResourceReleaseBegun {
                resource_id,
                runtime_generation: 9,
            }],
        )
        .expect("release plan");
        assert_eq!(release.len(), 1);
        assert_eq!(
            release[0].document.effect,
            Effect::ReleaseResource {
                task_id,
                action_epoch: 3,
                resource_fence: ResourceFence::new(resource_id, 9),
            }
        );
        assert_eq!(
            release[0].document.replay_policy,
            ReplayPolicy::ReconcileBeforeRetry
        );

        let pure = plan_effects(
            Some(&snap),
            task_id,
            &[Event::TaskRenamed { title: "x".into() }],
        )
        .expect("pure plans nothing");
        assert!(pure.is_empty());

        let agent_session_id = AgentSessionId::from_bytes(fixed_uuid_v7(0x25)).unwrap();
        let provider_session_id =
            crate::domain::ProviderSessionId::new("codex-session-25").unwrap();
        let mut provider_snap = snap.clone();
        provider_snap.agents.insert(
            agent_session_id,
            AgentSessionFacts {
                id: agent_session_id,
                task_id,
                role: AgentRole::Primary,
                provider_kind: crate::providers::ProviderKind::Codex,
                provider_session_id: Some(provider_session_id.clone()),
                lifecycle: AgentSessionLifecycle::Open,
                runtime_generation: 9,
                revision: 0,
            },
        );
        let provider_operation = OperationId::from_bytes(fixed_uuid_v7(0x26)).unwrap();
        let provider_command = CommandId::from_bytes(fixed_uuid_v7(0x27)).unwrap();
        let provider_client = crate::domain::ClientId::from_bytes(fixed_uuid_v7(0x28)).unwrap();
        let provider_event = Event::ProviderInputAccepted {
            command_id: provider_command,
            client_id: provider_client,
            operation_id: provider_operation,
            agent_session_id,
            provider_kind: crate::providers::ProviderKind::Codex,
            provider_session_id: Some(provider_session_id),
            runtime_generation: 9,
            turn_id: crate::domain::TurnId::from_bytes(fixed_uuid_v7(0x29)).unwrap(),
            action_epoch: 3,
            question_id: None,
            approval_id: None,
            action: crate::domain::ProviderInputAction::SendNow {
                text: "sealed adapter seam".into(),
                wait: false,
                images: Vec::new(),
            },
            wait: false,
            delivery: crate::domain::ProviderDeliveryVisibility::hold_until_destination_adapter(),
        };
        let provider = plan_effects(Some(&provider_snap), task_id, &[provider_event])
            .expect("provider input plans a concrete effect");
        assert_eq!(provider.len(), 1);
        assert_eq!(
            provider[0].document.destination_class,
            DestinationClass::ProviderInput
        );
        assert_eq!(
            provider[0].document.replay_policy,
            ReplayPolicy::NoAutomaticRetry
        );
        match &provider[0].document.effect {
            Effect::DeliverProviderInput {
                operation_id: planned_operation,
                agent_session_id: planned_agent,
                ..
            } => {
                assert_eq!(*planned_operation, provider_operation);
                assert_eq!(*planned_agent, agent_session_id);
            }
            other => panic!("unexpected provider effect: {other:?}"),
        }

        assert!(plan_effects(
            Some(&snap),
            task_id,
            &[
                Event::TaskCloseBegun { action_epoch: 4 },
                Event::TaskRenamed {
                    title: "mixed".into()
                },
            ],
        )
        .is_err());

        assert!(
            plan_effects(
                Some(&snap),
                task_id,
                &[
                    Event::TaskCloseBegun { action_epoch: 4 },
                    Event::ResourceReleaseBegun {
                        resource_id,
                        runtime_generation: 9,
                    },
                ],
            )
            .is_err(),
            "multiple side-effect decision facts must fail for current commands"
        );

        let operation_id = OperationId::from_bytes(fixed_uuid_v7(0x30)).unwrap();
        assert_eq!(
            external_idempotency_key(operation_id, 0),
            format!("v1:{operation_id}:0")
        );
    }

    #[test]
    fn decision_fact_classifiers_match_plan_effects_partition() {
        use crate::domain::artifact::PrivacyClass;
        use crate::domain::browser::{BrowserAction, BrowserDurableFact, BrowserPermission};
        use crate::domain::id::{AgentSessionId, BrowserContextId, BrowserRequestId, BrowserTabId};

        let task = TaskId::from_bytes(fixed_uuid_v7(0x41)).unwrap();
        let context = BrowserContextId::from_bytes(fixed_uuid_v7(0x44)).unwrap();
        let pure = [
            Event::TaskArchived,
            Event::ResourceReleased {
                resource_id: ResourceId::from_bytes(fixed_uuid_v7(0x42)).unwrap(),
                runtime_generation: 1,
            },
            Event::SpecialistClosed {
                specialist_id: AgentSessionId::from_bytes(fixed_uuid_v7(0x43)).unwrap(),
                action_epoch: 1,
                runtime_generation: 1,
            },
            Event::Browser(BrowserDurableFact::ContextClosed {
                context_id: context,
                task_id: task,
                generation: 1,
            }),
            Event::Browser(BrowserDurableFact::RequestAccepted {
                request_id: BrowserRequestId::from_bytes(fixed_uuid_v7(0x45)).unwrap(),
                task_id: task,
                context_id: context,
                tab_id: Some(BrowserTabId::from_bytes(fixed_uuid_v7(0x47)).unwrap()),
                generation: 1,
                action: BrowserAction::CreateContext,
                privacy_class: PrivacyClass::Shareable,
                permission: BrowserPermission::CreateContext,
                payload_hash: [0; 32],
                action_epoch: 1,
                command_id: None,
            }),
        ];
        for event in &pure {
            assert!(
                is_pure_slice_decision_fact(event),
                "expected pure: {event:?}"
            );
            assert!(
                !is_side_effect_decision_fact(event),
                "pure must not also be side-effect: {event:?}"
            );
        }

        let side = [
            Event::TaskCloseBegun { action_epoch: 2 },
            Event::ResourceReleaseBegun {
                resource_id: ResourceId::from_bytes(fixed_uuid_v7(0x48)).unwrap(),
                runtime_generation: 3,
            },
            Event::Browser(BrowserDurableFact::RequestAccepted {
                request_id: BrowserRequestId::from_bytes(fixed_uuid_v7(0x49)).unwrap(),
                task_id: task,
                context_id: context,
                tab_id: Some(BrowserTabId::from_bytes(fixed_uuid_v7(0x4B)).unwrap()),
                generation: 1,
                action: BrowserAction::Navigate {
                    url: "https://example.test".into(),
                },
                privacy_class: PrivacyClass::Shareable,
                permission: BrowserPermission::Navigate,
                payload_hash: [1; 32],
                action_epoch: 1,
                command_id: None,
            }),
        ];
        for event in &side {
            assert!(
                is_side_effect_decision_fact(event),
                "expected side-effect: {event:?}"
            );
            assert!(
                !is_pure_slice_decision_fact(event),
                "side-effect must not also be pure: {event:?}"
            );
        }
    }
}
