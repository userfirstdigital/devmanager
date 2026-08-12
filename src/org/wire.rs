//! Conversions between host organization types and protocol wire payloads.

use std::collections::BTreeSet;

use uuid::Uuid;

use crate::connect::ConnectHostId;
use crate::domain::id::TaskId;
use crate::domain::task::{TaskAttention, TaskLifecycle};
use crate::org::error::OrgError;
use crate::org::identity::{BoardCardId, PortalAccountId, PortalTenantId};
use crate::org::ids::{ManagedLinkId, OrgPromptChainId, OrgPromptId, OrgPromptVersionId};
use crate::org::local_actions::{
    ActionOutcome, ActionRisk, Admission, LocalActionCatalogEntry, LocalActionKind,
    LocalActionReconcileState, ReplayPolicy,
};
use crate::org::managed::{EnrollmentState, ManagedTaskSnapshot};
use crate::org::membership::{
    LocalActionApprovalRequirement, ManagedMetadataName, MembershipStatus,
    OrganizationPolicyDocument, RawSharingCeiling,
};
use crate::org::persistence::{OutboxDeliveryState, PersistedOutboxIntent};
use crate::org::watcher::{FleetWatcherView, HostReachability, TaskWatcherView};
use crate::org::{OrganizationFact, OrganizationProjection, SyncOutcome};
use crate::prompts::{
    OrgPrompt, OrgPromptChain, OrgPromptChainLink, OrgPromptVersion, OrganizationPromptSnapshot,
    PromptLifecycle,
};
use crate::protocol::{
    validate_organization_payload, OrganizationCodecError, OrganizationEvidenceMetadataWire,
    OrganizationFleetWatcherWire, OrganizationLocalActionCatalogEntryWire,
    OrganizationLocalActionCatalogWire, OrganizationLocalActionStateWire,
    OrganizationManagedTaskWire, OrganizationMembershipWire, OrganizationPolicyWire,
    OrganizationPromptChainLinkWire, OrganizationPromptChainWire, OrganizationPromptSnapshotWire,
    OrganizationPromptVersionWire, OrganizationPromptWire, OrganizationTaskWatcherWire,
    OrganizationTelemetryIntentWire, OrganizationWirePayload, ORGANIZATION_SCHEMA_VERSION,
};

pub fn codec_error(error: OrganizationCodecError) -> OrgError {
    match error {
        OrganizationCodecError::UnknownSchema | OrganizationCodecError::Malformed => {
            OrgError::CorruptState
        }
        OrganizationCodecError::WrongIdentity => OrgError::CrossTenant,
        OrganizationCodecError::ZeroRevision => OrgError::StalePolicy,
        OrganizationCodecError::BoundExceeded => OrgError::BoundExceeded,
        OrganizationCodecError::DuplicateId => OrgError::Replay,
        OrganizationCodecError::RawEvidence => OrgError::ProhibitedField,
    }
}

pub fn organization_fact_from_payload(
    payload: &OrganizationWirePayload,
) -> Result<Option<OrganizationFact>, OrgError> {
    match payload {
        OrganizationWirePayload::Membership(membership) => Ok(Some(OrganizationFact::Membership {
            host_id: ConnectHostId::from_uuid(membership.host_id)
                .map_err(|_| OrgError::EmptyIdentity)?,
            tenant_id: PortalTenantId::parse(&membership.tenant_id)
                .map_err(|_| OrgError::EmptyIdentity)?,
            account_id: PortalAccountId::parse(&membership.account_id)
                .map_err(|_| OrgError::EmptyIdentity)?,
            status: parse_membership_status(&membership.status)?,
            revision: u64::from(membership.policy_revision),
            revoked_at_ms: (membership.status == "revoked").then_some(membership.last_seen_ms),
            expires_at_ms: None,
        })),
        OrganizationWirePayload::ManagedTask(task) => Ok(Some(OrganizationFact::ManagedTask(
            managed_task_from_wire(task)?,
        ))),
        _ => Ok(None),
    }
}

pub fn membership_to_wire(membership: &crate::org::HostMembership) -> OrganizationMembershipWire {
    OrganizationMembershipWire {
        schema_version: ORGANIZATION_SCHEMA_VERSION,
        tenant_id: membership.tenant_id.as_str().to_string(),
        account_id: membership.account_id.as_str().to_string(),
        host_id: Uuid::from_bytes(membership.host_id.as_bytes()),
        device_id: membership
            .device_id
            .as_ref()
            .map(|device| device.as_str().to_string())
            .unwrap_or_else(|| "unspecified".to_string()),
        role: match membership.role {
            crate::org::MembershipRole::Owner => "owner",
            crate::org::MembershipRole::Admin => "admin",
            crate::org::MembershipRole::Manager => "manager",
            crate::org::MembershipRole::Member => "member",
            crate::org::MembershipRole::Disabled => "disabled",
        }
        .to_string(),
        status: match membership.status {
            MembershipStatus::PendingLocalConfirm => "pending_local_confirm",
            MembershipStatus::Enrolled => "enrolled",
            MembershipStatus::Revoked => "revoked",
            MembershipStatus::Unenrolled => "unenrolled",
        }
        .to_string(),
        display_name: membership.display_label.clone(),
        policy_revision: membership.policy_revision,
        enrolled_at_ms: membership.enrolled_at_ms.unwrap_or(0),
        last_seen_ms: membership.last_seen_at_ms.unwrap_or(0),
    }
}

pub fn managed_task_to_wire(snapshot: &ManagedTaskSnapshot) -> OrganizationManagedTaskWire {
    OrganizationManagedTaskWire {
        schema_version: ORGANIZATION_SCHEMA_VERSION,
        tenant_id: snapshot.tenant_id.as_str().to_string(),
        host_id: Uuid::from_bytes(snapshot.host_id.as_bytes()),
        local_task_id: Uuid::from_bytes(*snapshot.local_task_id.as_bytes()),
        link_id: Uuid::from_bytes(*snapshot.link_id.as_bytes()),
        board_card_id: snapshot.board_card_id.as_str().to_string(),
        enrollment_state: enrollment_wire(snapshot.enrollment_state),
        portal_revision: snapshot.portal_revision,
        metadata_policy_version: snapshot.metadata_policy_version,
        linked_by: snapshot.linked_by.clone(),
        linked_at: snapshot.linked_at,
        unlinked_at: snapshot.unlinked_at,
        portal_title: snapshot.portal_title.clone(),
    }
}

pub fn policy_to_wire(policy: &OrganizationPolicyDocument) -> OrganizationPolicyWire {
    OrganizationPolicyWire {
        schema_version: ORGANIZATION_SCHEMA_VERSION,
        tenant_id: policy.tenant_id.as_str().to_string(),
        revision: policy.revision,
        allowed_metadata_fields: policy
            .allowed_metadata_fields
            .iter()
            .copied()
            .map(metadata_name_wire)
            .collect(),
        retention_ms: policy.retention_ms,
        idle_interval_ms: policy.idle_interval_ms,
        raw_sharing_ceiling: match policy.raw_sharing_ceiling {
            RawSharingCeiling::None => "none".to_string(),
        },
        local_action_approval: match policy.local_action_approval {
            LocalActionApprovalRequirement::OwnerRequired => "owner_required".to_string(),
        },
        prompt_maintainer_accounts: policy.prompt_maintainer_accounts.iter().cloned().collect(),
        content_hash_hex: policy.content_hash_hex.clone(),
    }
}

pub fn prompt_snapshot_to_wire(
    snapshot: &OrganizationPromptSnapshot,
) -> OrganizationPromptSnapshotWire {
    OrganizationPromptSnapshotWire {
        schema_version: ORGANIZATION_SCHEMA_VERSION,
        tenant_id: snapshot.tenant_id.as_str().to_string(),
        revision: snapshot.revision,
        prompts: snapshot.prompts.iter().map(prompt_to_wire).collect(),
        versions: snapshot
            .versions
            .iter()
            .map(prompt_version_to_wire)
            .collect(),
        chains: snapshot.chains.iter().map(prompt_chain_to_wire).collect(),
    }
}

pub fn fleet_watcher_to_wire(
    tenant_id: &str,
    view: &FleetWatcherView,
) -> OrganizationFleetWatcherWire {
    OrganizationFleetWatcherWire {
        schema_version: ORGANIZATION_SCHEMA_VERSION,
        tenant_id: tenant_id.to_string(),
        host_id: Uuid::from_bytes(view.host_id.as_bytes()),
        reachability: reachability_wire(view.reachability),
        assigned: view.assigned,
        in_progress: view.in_progress,
        waiting: view.waiting,
        blocked: view.blocked,
        review: view.review,
        last_activity_ms: view.last_activity_ms,
        mutation_allowed: false,
        freshness: "observed_at plus completeness/confidence; unavailable stays unavailable"
            .to_string(),
        completeness: "partial".to_string(),
    }
}

pub fn task_watcher_to_wire(
    tenant_id: &str,
    host_id: ConnectHostId,
    view: &TaskWatcherView,
) -> OrganizationTaskWatcherWire {
    OrganizationTaskWatcherWire {
        schema_version: ORGANIZATION_SCHEMA_VERSION,
        tenant_id: tenant_id.to_string(),
        host_id: Uuid::from_bytes(host_id.as_bytes()),
        task_id: Uuid::from_bytes(*view.task_id.as_bytes()),
        board_card_id: view.board_card_id.as_str().to_string(),
        lifecycle: lifecycle_wire(view.lifecycle),
        attention: attention_wire(view.attention),
        host_reachability: reachability_wire(view.host_reachability),
        usage_source_label: view.usage_source_label.clone(),
        git_summary: view.git_summary.clone(),
        freshness: view.freshness.to_string(),
        completeness: "partial".to_string(),
        mutation_allowed: false,
    }
}

impl OrganizationProjection {
    pub fn dispatch_wire_payload(
        &mut self,
        payload: OrganizationWirePayload,
        now_ms: i64,
    ) -> Result<SyncOutcome, OrgError> {
        let tenant = self
            .membership()
            .map(|membership| membership.tenant_id.as_str().to_string())
            .or_else(|| {
                self.signed_in_account()
                    .map(|account| account.tenant_id.as_str().to_string())
            });
        let host = self
            .membership()
            .map(|membership| membership.host_id.as_uuid());
        validate_organization_payload(&payload, tenant.as_deref(), host).map_err(codec_error)?;
        match payload {
            OrganizationWirePayload::Membership(_) | OrganizationWirePayload::ManagedTask(_) => {
                let fact =
                    organization_fact_from_payload(&payload)?.ok_or(OrgError::CorruptState)?;
                self.apply_authoritative_fact(fact, now_ms)
            }
            OrganizationWirePayload::Policy(policy) => {
                self.apply_policy_document(policy_from_wire(&policy)?)
            }
            OrganizationWirePayload::PromptSnapshot(snapshot) => self.apply_prompt_snapshot(
                prompt_snapshot_from_wire(&snapshot)?,
                now_ms,
                now_ms.saturating_add(1),
            ),
            OrganizationWirePayload::LocalActionCatalog(catalog) => {
                self.bind_local_action_catalog_wire(&catalog)?;
                Ok(SyncOutcome::Applied)
            }
            OrganizationWirePayload::LocalActionState(state) => {
                self.apply_local_action_state_wire(&state)
            }
            OrganizationWirePayload::EvidenceMetadata(evidence) => {
                self.record_evidence_metadata_wire(&evidence)
            }
            OrganizationWirePayload::TelemetryIntent(intent) => {
                self.queue_telemetry_intent_wire(intent)
            }
            OrganizationWirePayload::FleetWatcher(_) | OrganizationWirePayload::TaskWatcher(_) => {
                Err(OrgError::WatcherReadOnly)
            }
        }
    }

    fn bind_local_action_catalog_wire(
        &mut self,
        catalog: &OrganizationLocalActionCatalogWire,
    ) -> Result<(), OrgError> {
        let membership = self.require_enrolled()?;
        if membership.tenant_id.as_str() != catalog.tenant_id {
            return Err(OrgError::CrossTenant);
        }
        if membership.host_id.as_bytes() != catalog.host_id.into_bytes() {
            return Err(OrgError::HostUnenrolled);
        }
        let entries = catalog
            .entries
            .iter()
            .map(catalog_entry_from_wire)
            .collect::<Result<Vec<_>, _>>()?;
        self.local_actions_mut().bind_server_catalog(entries)
    }

    fn apply_local_action_state_wire(
        &mut self,
        state: &OrganizationLocalActionStateWire,
    ) -> Result<SyncOutcome, OrgError> {
        let membership = self.require_enrolled()?;
        if membership.tenant_id.as_str() != state.tenant_id {
            return Err(OrgError::CrossTenant);
        }
        if membership.host_id.as_bytes() != state.host_id.into_bytes() {
            return Err(OrgError::HostUnenrolled);
        }
        let request_id = crate::org::LocalActionId::from_bytes(state.request_id.into_bytes())
            .map_err(|_| OrgError::EmptyIdentity)?;
        match self.local_actions().admission_state(request_id) {
            Some(existing)
                if existing.reconcile == parse_reconcile(&state.reconcile)?
                    && format!("{:?}", existing.admission).to_ascii_lowercase()
                        == state.admission =>
            {
                Ok(SyncOutcome::Duplicate)
            }
            Some(_) => Err(OrgError::LastWriteWinsForbidden),
            None => Err(OrgError::Unlinked),
        }
    }

    fn record_evidence_metadata_wire(
        &mut self,
        evidence: &OrganizationEvidenceMetadataWire,
    ) -> Result<SyncOutcome, OrgError> {
        if evidence.raw_content_included {
            return Err(OrgError::ProhibitedField);
        }
        let membership = self.require_enrolled()?;
        if membership.tenant_id.as_str() != evidence.tenant_id {
            return Err(OrgError::CrossTenant);
        }
        Ok(SyncOutcome::Applied)
    }

    fn queue_telemetry_intent_wire(
        &mut self,
        intent: OrganizationTelemetryIntentWire,
    ) -> Result<SyncOutcome, OrgError> {
        let membership = self.require_enrolled()?.clone();
        if membership.tenant_id.as_str() != intent.tenant_id {
            return Err(OrgError::CrossTenant);
        }
        if membership.host_id.as_bytes() != intent.host_id.into_bytes() {
            return Err(OrgError::HostUnenrolled);
        }
        self.queue_outbox_intent(PersistedOutboxIntent {
            observation_id_hex: intent.observation_id_hex,
            intent: intent.intent,
            publication_queued: intent.publication_queued,
            delivery: OutboxDeliveryState::Queued,
        })
    }
}

fn policy_from_wire(wire: &OrganizationPolicyWire) -> Result<OrganizationPolicyDocument, OrgError> {
    let claimed_hash = wire.content_hash_hex.clone();
    let mut allowed_metadata_fields = BTreeSet::new();
    for field in &wire.allowed_metadata_fields {
        allowed_metadata_fields.insert(parse_metadata_name(field)?);
    }
    let document = OrganizationPolicyDocument {
        revision: wire.revision,
        tenant_id: PortalTenantId::parse(&wire.tenant_id).map_err(|_| OrgError::EmptyIdentity)?,
        allowed_metadata_fields,
        retention_ms: wire.retention_ms,
        idle_interval_ms: wire.idle_interval_ms,
        raw_sharing_ceiling: parse_raw_sharing(&wire.raw_sharing_ceiling)?,
        local_action_approval: parse_local_action_approval(&wire.local_action_approval)?,
        prompt_maintainer_accounts: wire.prompt_maintainer_accounts.iter().cloned().collect(),
        content_hash_hex: String::new(),
    }
    .finalize()?;
    if document.content_hash_hex != claimed_hash {
        return Err(OrgError::TamperedEvidence);
    }
    Ok(document)
}

fn prompt_snapshot_from_wire(
    wire: &OrganizationPromptSnapshotWire,
) -> Result<OrganizationPromptSnapshot, OrgError> {
    Ok(OrganizationPromptSnapshot {
        tenant_id: PortalTenantId::parse(&wire.tenant_id).map_err(|_| OrgError::EmptyIdentity)?,
        revision: wire.revision,
        prompts: wire
            .prompts
            .iter()
            .map(prompt_from_wire)
            .collect::<Result<Vec<_>, _>>()?,
        versions: wire
            .versions
            .iter()
            .map(prompt_version_from_wire)
            .collect::<Result<Vec<_>, _>>()?,
        chains: wire
            .chains
            .iter()
            .map(prompt_chain_from_wire)
            .collect::<Result<Vec<_>, _>>()?,
    })
}

fn prompt_to_wire(prompt: &OrgPrompt) -> OrganizationPromptWire {
    OrganizationPromptWire {
        prompt_id: Uuid::from_bytes(*prompt.prompt_id.as_bytes()),
        tenant_id: prompt.tenant_id.as_str().to_string(),
        namespace: prompt.namespace.clone(),
        name: prompt.name.clone(),
        current_version_id: Uuid::from_bytes(*prompt.current_version_id.as_bytes()),
        lifecycle: match prompt.lifecycle {
            PromptLifecycle::Published => "published".to_string(),
            PromptLifecycle::Deprecated => "deprecated".to_string(),
        },
    }
}

fn prompt_from_wire(wire: &OrganizationPromptWire) -> Result<OrgPrompt, OrgError> {
    Ok(OrgPrompt {
        prompt_id: OrgPromptId::from_bytes(wire.prompt_id.into_bytes())
            .map_err(|_| OrgError::EmptyIdentity)?,
        tenant_id: PortalTenantId::parse(&wire.tenant_id).map_err(|_| OrgError::EmptyIdentity)?,
        namespace: wire.namespace.clone(),
        name: wire.name.clone(),
        current_version_id: OrgPromptVersionId::from_bytes(wire.current_version_id.into_bytes())
            .map_err(|_| OrgError::EmptyIdentity)?,
        lifecycle: parse_prompt_lifecycle(&wire.lifecycle)?,
    })
}

fn prompt_version_to_wire(version: &OrgPromptVersion) -> OrganizationPromptVersionWire {
    OrganizationPromptVersionWire {
        prompt_id: Uuid::from_bytes(*version.prompt_id.as_bytes()),
        version_id: Uuid::from_bytes(*version.version_id.as_bytes()),
        author: version.author.as_str().to_string(),
        title: version.title.clone(),
        tags: version.tags.clone(),
        body: version.body.clone(),
        content_hash_hex: version.content_hash_hex.clone(),
        published_at_ms: version.published_at_ms,
    }
}

fn prompt_version_from_wire(
    wire: &OrganizationPromptVersionWire,
) -> Result<OrgPromptVersion, OrgError> {
    Ok(OrgPromptVersion {
        prompt_id: OrgPromptId::from_bytes(wire.prompt_id.into_bytes())
            .map_err(|_| OrgError::EmptyIdentity)?,
        version_id: OrgPromptVersionId::from_bytes(wire.version_id.into_bytes())
            .map_err(|_| OrgError::EmptyIdentity)?,
        author: PortalAccountId::parse(&wire.author).map_err(|_| OrgError::EmptyIdentity)?,
        title: wire.title.clone(),
        tags: wire.tags.clone(),
        body: wire.body.clone(),
        content_hash_hex: wire.content_hash_hex.clone(),
        published_at_ms: wire.published_at_ms,
    })
}

fn prompt_chain_to_wire(chain: &OrgPromptChain) -> OrganizationPromptChainWire {
    OrganizationPromptChainWire {
        chain_id: Uuid::from_bytes(*chain.chain_id.as_bytes()),
        tenant_id: chain.tenant_id.as_str().to_string(),
        revision: chain.revision,
        links: chain
            .links
            .iter()
            .map(|link| OrganizationPromptChainLinkWire {
                position: link.position,
                version_id: Uuid::from_bytes(*link.version_id.as_bytes()),
            })
            .collect(),
    }
}

fn prompt_chain_from_wire(wire: &OrganizationPromptChainWire) -> Result<OrgPromptChain, OrgError> {
    Ok(OrgPromptChain {
        chain_id: OrgPromptChainId::from_bytes(wire.chain_id.into_bytes())
            .map_err(|_| OrgError::EmptyIdentity)?,
        tenant_id: PortalTenantId::parse(&wire.tenant_id).map_err(|_| OrgError::EmptyIdentity)?,
        revision: wire.revision,
        links: wire
            .links
            .iter()
            .map(|link| {
                Ok(OrgPromptChainLink {
                    position: link.position,
                    version_id: OrgPromptVersionId::from_bytes(link.version_id.into_bytes())
                        .map_err(|_| OrgError::EmptyIdentity)?,
                })
            })
            .collect::<Result<Vec<_>, OrgError>>()?,
    })
}

fn managed_task_from_wire(
    task: &OrganizationManagedTaskWire,
) -> Result<ManagedTaskSnapshot, OrgError> {
    Ok(ManagedTaskSnapshot {
        host_id: ConnectHostId::from_uuid(task.host_id).map_err(|_| OrgError::EmptyIdentity)?,
        local_task_id: TaskId::from_bytes(task.local_task_id.into_bytes())
            .map_err(|_| OrgError::EmptyIdentity)?,
        board_card_id: BoardCardId::parse(&task.board_card_id)
            .map_err(|_| OrgError::EmptyIdentity)?,
        enrollment_state: parse_enrollment(&task.enrollment_state)?,
        portal_revision: task.portal_revision,
        metadata_policy_version: task.metadata_policy_version,
        linked_by: task.linked_by.clone(),
        linked_at: task.linked_at,
        unlinked_at: task.unlinked_at,
        link_id: ManagedLinkId::from_bytes(task.link_id.into_bytes())
            .map_err(|_| OrgError::EmptyIdentity)?,
        tenant_id: PortalTenantId::parse(&task.tenant_id).map_err(|_| OrgError::EmptyIdentity)?,
        portal_title: task.portal_title.clone(),
    })
}

fn catalog_entry_from_wire(
    entry: &OrganizationLocalActionCatalogEntryWire,
) -> Result<LocalActionCatalogEntry, OrgError> {
    Ok(LocalActionCatalogEntry {
        kind: parse_action_kind(&entry.kind)?,
        version: entry.version,
        replay_policy: parse_replay(&entry.replay_policy)?,
        risk: parse_risk(&entry.risk)?,
    })
}

fn metadata_name_wire(name: ManagedMetadataName) -> String {
    match name {
        ManagedMetadataName::TaskState => "task_state",
        ManagedMetadataName::TaskAttention => "task_attention",
        ManagedMetadataName::TaskAssignmentReference => "task_assignment_reference",
        ManagedMetadataName::ProviderKind => "provider_kind",
        ManagedMetadataName::ProviderState => "provider_state",
        ManagedMetadataName::SourceTimestamp => "source_timestamp",
        ManagedMetadataName::ObservedTimestamp => "observed_timestamp",
        ManagedMetadataName::ProviderReportedUsage => "provider_reported_usage",
        ManagedMetadataName::HumanMessageCount => "human_message_count",
        ManagedMetadataName::HumanTurnCount => "human_turn_count",
        ManagedMetadataName::ActiveSessionInterval => "active_session_interval",
        ManagedMetadataName::GitSummary => "git_summary",
        ManagedMetadataName::HostHealth => "host_health",
        ManagedMetadataName::ApprovedArtifactReference => "approved_artifact_reference",
    }
    .to_string()
}

fn parse_metadata_name(value: &str) -> Result<ManagedMetadataName, OrgError> {
    match value {
        "task_state" => Ok(ManagedMetadataName::TaskState),
        "task_attention" => Ok(ManagedMetadataName::TaskAttention),
        "task_assignment_reference" => Ok(ManagedMetadataName::TaskAssignmentReference),
        "provider_kind" => Ok(ManagedMetadataName::ProviderKind),
        "provider_state" => Ok(ManagedMetadataName::ProviderState),
        "source_timestamp" => Ok(ManagedMetadataName::SourceTimestamp),
        "observed_timestamp" => Ok(ManagedMetadataName::ObservedTimestamp),
        "provider_reported_usage" => Ok(ManagedMetadataName::ProviderReportedUsage),
        "human_message_count" => Ok(ManagedMetadataName::HumanMessageCount),
        "human_turn_count" => Ok(ManagedMetadataName::HumanTurnCount),
        "active_session_interval" => Ok(ManagedMetadataName::ActiveSessionInterval),
        "git_summary" => Ok(ManagedMetadataName::GitSummary),
        "host_health" => Ok(ManagedMetadataName::HostHealth),
        "approved_artifact_reference" => Ok(ManagedMetadataName::ApprovedArtifactReference),
        _ => Err(OrgError::ProhibitedField),
    }
}

fn parse_raw_sharing(value: &str) -> Result<RawSharingCeiling, OrgError> {
    match value {
        "none" => Ok(RawSharingCeiling::None),
        _ => Err(OrgError::ProhibitedField),
    }
}

fn parse_local_action_approval(value: &str) -> Result<LocalActionApprovalRequirement, OrgError> {
    match value {
        "owner_required" => Ok(LocalActionApprovalRequirement::OwnerRequired),
        _ => Err(OrgError::ProhibitedField),
    }
}

fn parse_prompt_lifecycle(value: &str) -> Result<PromptLifecycle, OrgError> {
    match value {
        "published" => Ok(PromptLifecycle::Published),
        "deprecated" => Ok(PromptLifecycle::Deprecated),
        _ => Err(OrgError::CorruptState),
    }
}

fn parse_membership_status(value: &str) -> Result<MembershipStatus, OrgError> {
    match value {
        "pending_local_confirm" | "pending" => Ok(MembershipStatus::PendingLocalConfirm),
        "enrolled" => Ok(MembershipStatus::Enrolled),
        "revoked" => Ok(MembershipStatus::Revoked),
        "unenrolled" => Ok(MembershipStatus::Unenrolled),
        _ => Err(OrgError::CorruptState),
    }
}

fn parse_enrollment(value: &str) -> Result<EnrollmentState, OrgError> {
    match value {
        "enrolled" => Ok(EnrollmentState::Enrolled),
        "pending_owner_accept" => Ok(EnrollmentState::PendingOwnerAccept),
        "unlinked" => Ok(EnrollmentState::Unlinked),
        "closed" => Ok(EnrollmentState::Closed),
        _ => Err(OrgError::CorruptState),
    }
}

fn enrollment_wire(state: EnrollmentState) -> String {
    match state {
        EnrollmentState::PendingOwnerAccept => "pending_owner_accept",
        EnrollmentState::Enrolled => "enrolled",
        EnrollmentState::Unlinked => "unlinked",
        EnrollmentState::Closed => "closed",
    }
    .to_string()
}

fn parse_action_kind(value: &str) -> Result<LocalActionKind, OrgError> {
    match value {
        "db_schema_introspect" => Ok(LocalActionKind::DbSchemaIntrospect),
        "db_approved_change_apply" => Ok(LocalActionKind::DbApprovedChangeApply),
        "env_diff" => Ok(LocalActionKind::EnvDiff),
        "env_approved_apply" => Ok(LocalActionKind::EnvApprovedApply),
        _ => Err(OrgError::CorruptState),
    }
}

fn parse_replay(value: &str) -> Result<ReplayPolicy, OrgError> {
    match value {
        "idempotent_safe" => Ok(ReplayPolicy::IdempotentSafe),
        "never_assume_retry_safe" => Ok(ReplayPolicy::NeverAssumeRetrySafe),
        _ => Err(OrgError::CorruptState),
    }
}

fn parse_risk(value: &str) -> Result<ActionRisk, OrgError> {
    match value {
        "low" => Ok(ActionRisk::Low),
        "production" => Ok(ActionRisk::Production),
        _ => Err(OrgError::CorruptState),
    }
}

fn parse_reconcile(value: &str) -> Result<LocalActionReconcileState, OrgError> {
    match value {
        "awaiting_host_execution" => Ok(LocalActionReconcileState::AwaitingHostExecution),
        "rejected" => Ok(LocalActionReconcileState::Rejected),
        "uncertain" => Ok(LocalActionReconcileState::Uncertain),
        "settled" => Ok(LocalActionReconcileState::Settled),
        "failed" => Ok(LocalActionReconcileState::Failed),
        "cancelled" => Ok(LocalActionReconcileState::Cancelled),
        _ => Err(OrgError::CorruptState),
    }
}

fn reachability_wire(value: HostReachability) -> String {
    match value {
        HostReachability::Online => "online",
        HostReachability::Stale => "stale",
        HostReachability::Offline => "offline",
    }
    .to_string()
}

fn lifecycle_wire(value: TaskLifecycle) -> String {
    format!("{value:?}").to_ascii_lowercase()
}

fn attention_wire(value: TaskAttention) -> String {
    format!("{value:?}").to_ascii_lowercase()
}

#[allow(dead_code)]
fn parse_admission(value: &str) -> Result<Admission, OrgError> {
    match value {
        "accepted" => Ok(Admission::Accepted),
        "rejected" => Ok(Admission::Rejected),
        _ => Err(OrgError::CorruptState),
    }
}

#[allow(dead_code)]
fn parse_outcome(value: &str) -> Result<ActionOutcome, OrgError> {
    match value {
        "settled" => Ok(ActionOutcome::Settled),
        "failed" => Ok(ActionOutcome::Failed),
        "cancelled" => Ok(ActionOutcome::Cancelled),
        "uncertain" => Ok(ActionOutcome::Uncertain),
        _ => Err(OrgError::CorruptState),
    }
}
