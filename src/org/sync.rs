//! Opt-in Portal reconciliation. Caller-driven; never starts itself.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::connect::ConnectHostId;
use crate::domain::id::TaskId;
use crate::org::error::OrgError;
use crate::org::identity::{BoardCardId, ExternalAccount, PortalAccountId, PortalTenantId};
use crate::org::ids::{LocalActionId, ManagedLinkId, OrgPromptChainId, OrgPromptId, OrgPromptVersionId};
use crate::org::local_actions::{
    ActionOutcome, ActionRisk, Admission, LocalActionCatalogEntry, LocalActionKind,
};
use crate::org::managed::{EnrollmentState, ManagedTaskSnapshot};
use crate::org::membership::{
    HostMembership, ManagedMetadataName, MembershipRole, MembershipStatus,
    OrganizationPolicyDocument, RawSharingCeiling,
};
use crate::org::persistence::{PersistedOutboxIntent, OrganizationStateStore};
use crate::org::portal::{
    reject_prohibited_fields, validate_iso_timestamp, validate_opaque_id,
    CanonicalEvidenceImportRequest, HostMembershipDto, HostReconcileRequest,
    HostReconcileResponse,
    LocalActionCatalogDto, LocalActionReceiptRequest, ManagedTaskDto, OrgPromptChainDto,
    OrgPromptDto, OrgPromptVersionDto, OrganizationPolicyDto, PortalActionKind, PortalActionRisk,
    PortalAdapterError, PortalAuthProvider, PortalCredentialHandle, PortalEnrollmentState,
    PortalAdmissionStatus, PortalManagementClient, PortalMembershipStatus, PortalOrgRole,
    PortalOutcomeStatus, PortalPage, PortalPromptStatus, PortalRawSharingCeiling, PortalTransport,
    PublishPromptRequest, TelemetryUploadRequest,
    PORTAL_PAGE_MAX_ITEMS,
};
use crate::org::{
    OrganizationCapabilityDisableReason, OrganizationCapabilityState, OrganizationFact,
    OrganizationProjection, OrganizationSyncState, SyncOutcome,
};
use crate::prompts::{
    OrgPrompt, OrgPromptChain, OrgPromptChainLink, OrgPromptVersion, OrganizationPromptSnapshot,
    PromptLifecycle, ORG_PROMPT_CACHE_TTL_MS,
};

pub const PORTAL_SYNC_PAGE_LIMIT: u32 = 32;
pub const PORTAL_SYNC_MAX_PAGES: u32 = 8;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PortalSyncCursors {
    pub hosts: Option<String>,
    pub tasks: Option<String>,
    pub prompts: Option<String>,
    pub prompt_chains: Option<String>,
    pub actions: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PortalSyncRuntimeRecord {
    pub credential_handle: Option<PortalCredentialHandle>,
    pub cursors: PortalSyncCursors,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortalReconcileRequest {
    pub host_id: ConnectHostId,
    pub local_confirmation: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortalReconcileKind {
    StandaloneNoop,
    PreviewOnly,
    Reconciled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortalReconcileOutcome {
    pub kind: PortalReconcileKind,
    pub applied_facts: u32,
    pub pages_fetched: u32,
    pub outbox_acknowledged: u32,
    pub capability: OrganizationCapabilityState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortalSyncFailureKind {
    Auth,
    Transport,
    Validation,
    Authorization,
    Organization,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortalSyncError {
    Org(OrgError),
    Adapter(PortalAdapterError),
}

impl PortalSyncError {
    pub fn kind(&self) -> PortalSyncFailureKind {
        match self {
            Self::Adapter(PortalAdapterError::Http { status: 401, .. }) => {
                PortalSyncFailureKind::Auth
            }
            Self::Adapter(PortalAdapterError::Http { status: 403, .. }) => {
                PortalSyncFailureKind::Authorization
            }
            Self::Adapter(PortalAdapterError::Transport(_)) => PortalSyncFailureKind::Transport,
            Self::Adapter(PortalAdapterError::Http { .. }) => PortalSyncFailureKind::Authorization,
            Self::Adapter(_) => PortalSyncFailureKind::Validation,
            Self::Org(OrgError::RoleDenied | OrgError::OwnerOnly | OrgError::WatcherReadOnly) => {
                PortalSyncFailureKind::Authorization
            }
            Self::Org(
                OrgError::CrossTenant
                | OrgError::StalePolicy
                | OrgError::StaleGrant
                | OrgError::CorruptState
                | OrgError::ProhibitedField
                | OrgError::LastWriteWinsForbidden
                | OrgError::BoundExceeded
                | OrgError::EmptyIdentity
                | OrgError::TamperedEvidence,
            ) => PortalSyncFailureKind::Validation,
            Self::Org(_) => PortalSyncFailureKind::Organization,
        }
    }
}

impl From<OrgError> for PortalSyncError {
    fn from(error: OrgError) -> Self {
        Self::Org(error)
    }
}

impl From<PortalAdapterError> for PortalSyncError {
    fn from(error: PortalAdapterError) -> Self {
        Self::Adapter(error)
    }
}

pub struct PortalSyncRuntime<C> {
    client: C,
    credential_handle: Option<PortalCredentialHandle>,
    cursors: PortalSyncCursors,
    last_verified: bool,
}

impl<C> PortalSyncRuntime<C> {
    pub fn new(client: C, credential_handle: Option<PortalCredentialHandle>) -> Self {
        Self {
            client,
            credential_handle,
            cursors: PortalSyncCursors::default(),
            last_verified: false,
        }
    }

    pub fn export_record(&self) -> PortalSyncRuntimeRecord {
        PortalSyncRuntimeRecord {
            credential_handle: self.credential_handle.clone(),
            cursors: self.cursors.clone(),
        }
    }

    pub fn cursors(&self) -> &PortalSyncCursors {
        &self.cursors
    }

    pub fn last_cycle_verified(&self) -> bool {
        self.last_verified
    }

    pub fn transport(&self) -> &C {
        &self.client
    }

    pub fn capability(&self, projection: &OrganizationProjection) -> OrganizationCapabilityState {
        match projection.capability_state() {
            OrganizationCapabilityState::Enabled if !self.last_verified => {
                OrganizationCapabilityState::Disabled(OrganizationCapabilityDisableReason::Offline)
            }
            other => other,
        }
    }
}

impl PortalSyncRuntime<PortalManagementClient> {
    pub fn connect(
        base_url: &str,
        provider: &impl PortalAuthProvider,
    ) -> Result<Self, PortalAdapterError> {
        Ok(Self::new(
            PortalManagementClient::new(base_url, provider.bearer_token()?)?,
            provider.handle().cloned(),
        ))
    }
}

impl<C: PortalTransport> PortalSyncRuntime<C> {
    pub fn reconcile(
        &mut self,
        projection: &mut OrganizationProjection,
        store: &OrganizationStateStore,
        request: PortalReconcileRequest,
        now_ms: i64,
    ) -> Result<PortalReconcileOutcome, PortalSyncError> {
        self.last_verified = false;
        if projection.mode().is_standalone() {
            return Ok(PortalReconcileOutcome {
                kind: PortalReconcileKind::StandaloneNoop,
                applied_facts: 0,
                pages_fetched: 0,
                outbox_acknowledged: 0,
                capability: self.capability(projection),
            });
        }

        projection.set_authenticated_online(false);
        let checkpoint = projection.export_state()?;
        let checkpoint_cursors = self.cursors.clone();
        match self.run_cycle(projection, request, now_ms) {
            Ok(mut outcome) => {
                let live = matches!(projection.sync_state(), OrganizationSyncState::Enrolled)
                    && projection
                        .membership()
                        .is_some_and(|membership| membership.is_enrolled());
                projection.set_authenticated_online(live);
                self.last_verified = live;
                if let Err(error) = store.save(projection) {
                    if let Ok(restored) = OrganizationProjection::restore_from_document(checkpoint) {
                        *projection = restored;
                    }
                    self.cursors = checkpoint_cursors;
                    projection.set_authenticated_online(false);
                    self.last_verified = false;
                    return Err(error.into());
                }
                outcome.capability = self.capability(projection);
                Ok(outcome)
            }
            Err(error) => {
                if let Ok(restored) = OrganizationProjection::restore_from_document(checkpoint) {
                    *projection = restored;
                }
                self.cursors = checkpoint_cursors;
                projection.set_authenticated_online(false);
                self.last_verified = false;
                let _ = store.save(projection);
                Err(error)
            }
        }
    }

    pub fn publish_prompt(
        &self,
        projection: &OrganizationProjection,
        request: &PublishPromptRequest,
    ) -> Result<serde_json::Value, PortalSyncError> {
        let _ = scoped_membership(projection, self.last_verified)?;
        Ok(self.client.publish_prompt(request)?)
    }

    pub fn read_prompt(
        &self,
        projection: &OrganizationProjection,
        prompt_id: &str,
    ) -> Result<OrgPromptDto, PortalSyncError> {
        let membership = scoped_membership(projection, self.last_verified)?;
        let prompt = self.client.get_prompt(prompt_id)?;
        if prompt.organization_id != membership.tenant_id.as_str() {
            return Err(OrgError::CrossTenant.into());
        }
        Ok(prompt)
    }

    /// Upload an explicitly supplied, signed canonical EvidenceBundle v1.
    /// Reconciliation never constructs or sends this shape: the normal
    /// outbox path sends only a hash/reference metadata event, keeping raw
    /// transcript and media content opt-in.
    pub fn upload_evidence_bundle(
        &self,
        projection: &OrganizationProjection,
        request: &CanonicalEvidenceImportRequest,
    ) -> Result<serde_json::Value, PortalSyncError> {
        let membership = scoped_membership(projection, self.last_verified)?;
        request.validate()?;
        let device_id = membership
            .device_id
            .as_ref()
            .ok_or(OrgError::HostUnenrolled)?;
        if device_id.as_str() != request.bundle.source.device_id.as_str() {
            return Err(OrgError::CrossTenant.into());
        }
        Ok(self.client.import_evidence(request)?)
    }

    pub fn submit_local_action_receipt(
        &self,
        projection: &OrganizationProjection,
        request_id: &str,
        request: &LocalActionReceiptRequest,
    ) -> Result<crate::org::portal::LocalActionReceiptDto, PortalSyncError> {
        let _ = scoped_membership(projection, self.last_verified)?;
        Ok(self.client.receive_action_receipt(request_id, request)?)
    }

    fn run_cycle(
        &mut self,
        projection: &mut OrganizationProjection,
        request: PortalReconcileRequest,
        now_ms: i64,
    ) -> Result<PortalReconcileOutcome, PortalSyncError> {
        let host_id = request.host_id;
        let host_id_str = uuid_from_host(host_id).to_string();
        let signed_in = signed_in_account(projection).cloned();
        let enrolled = projection.membership().cloned();
        let account = enrolled
            .as_ref()
            .map(|membership| {
                ExternalAccount::new(
                    membership.tenant_id.clone(),
                    membership.account_id.clone(),
                    membership.device_id.clone(),
                )
            })
            .or(signed_in)
            .ok_or(OrgError::HostUnenrolled)?;

        let reconcile = self.client.reconcile_host(&HostReconcileRequest {
            host_id: host_id_str.clone(),
            device_public_id: account
                .device_id
                .as_ref()
                .map(|device| device.as_str().to_string()),
            credential_handle: self
                .credential_handle
                .as_ref()
                .map(|handle| handle.vault_ref.clone()),
            expected_revision: projection.membership_revision(),
            local_confirmed: request.local_confirmation,
        })?;
        if reconcile.membership_revision == 0 {
            return Err(OrgError::StalePolicy.into());
        }
        validate_membership_scope(&account, host_id, &reconcile.membership)?;

        let policy = policy_from_dto(&reconcile.policy, account.tenant_id.clone())?;
        let mut applied = 0u32;
        let mut pages_fetched = 0u32;

        if !matches!(projection.mode(), crate::org::OperatingMode::HostEnrolled { .. }) {
            if !request.local_confirmation {
                apply_membership_fact(projection, host_id, &reconcile, now_ms)?;
                return Ok(PortalReconcileOutcome {
                    kind: PortalReconcileKind::PreviewOnly,
                    applied_facts: 1,
                    pages_fetched: 0,
                    outbox_acknowledged: 0,
                    capability: OrganizationCapabilityState::Disabled(
                        OrganizationCapabilityDisableReason::Unenrolled,
                    ),
                });
            }
            let pending = HostMembership::pending(
                host_id,
                account.clone(),
                map_role(reconcile.membership.role),
                &policy,
                reconcile
                    .membership
                    .display_label
                    .clone()
                    .unwrap_or_else(|| "host".to_string()),
            )?;
            projection.confirm_enrollment(pending, policy.clone(), now_ms)?;
            applied += 1;
        }

        match apply_membership_fact(projection, host_id, &reconcile, now_ms)? {
            SyncOutcome::Applied => applied += 1,
            SyncOutcome::Duplicate => {}
        }
        if matches!(
            projection.sync_state(),
            OrganizationSyncState::Revoked | OrganizationSyncState::Expired
        ) {
            return Ok(PortalReconcileOutcome {
                kind: PortalReconcileKind::Reconciled,
                applied_facts: applied,
                pages_fetched: 0,
                outbox_acknowledged: 0,
                capability: projection.capability_state(),
            });
        }
        if projection.membership().is_none() || !projection.membership().is_some_and(|m| m.is_enrolled())
        {
            return Err(OrgError::EnrollmentNotConfirmed.into());
        }

        match projection.apply_policy_document(policy)? {
            SyncOutcome::Applied => applied += 1,
            SyncOutcome::Duplicate => {}
        }

        let client = &self.client;
        let (tasks, task_pages) = collect_pages(
            |cursor| client.list_tasks_page(cursor, PORTAL_SYNC_PAGE_LIMIT),
            &mut self.cursors.tasks,
        )?;
        pages_fetched += task_pages;
        for task in tasks {
            match apply_managed_task(projection, host_id, &account.tenant_id, task, now_ms)? {
                SyncOutcome::Applied => applied += 1,
                SyncOutcome::Duplicate => {}
            }
        }

        let (prompts, prompt_pages) = collect_pages(
            |cursor| client.list_prompts_page(cursor, PORTAL_SYNC_PAGE_LIMIT),
            &mut self.cursors.prompts,
        )?;
        pages_fetched += prompt_pages;
        let mut versions = Vec::new();
        for prompt in &prompts {
            versions.extend(client.list_prompt_versions(&prompt.id)?);
        }
        let (chains, chain_pages) = collect_pages(
            |cursor| client.list_prompt_chains_page(cursor, PORTAL_SYNC_PAGE_LIMIT),
            &mut self.cursors.prompt_chains,
        )?;
        pages_fetched += chain_pages;
        if let Some(snapshot) =
            prompt_snapshot_from_portal(&account.tenant_id, &prompts, versions, chains)?
        {
            match projection.apply_prompt_snapshot(
                snapshot,
                now_ms,
                now_ms.saturating_add(ORG_PROMPT_CACHE_TTL_MS as i64),
            )? {
                SyncOutcome::Applied => applied += 1,
                SyncOutcome::Duplicate => {}
            }
        }

        let catalog = client.list_action_catalog(&host_id_str)?;
        if !catalog.is_empty() {
            projection.bind_local_action_catalog(catalog_from_dto(catalog)?)?;
            applied += 1;
        }
        let (actions, action_pages) = collect_pages(
            |cursor| {
                client.list_actions_page(Some(&host_id_str), cursor, PORTAL_SYNC_PAGE_LIMIT)
            },
            &mut self.cursors.actions,
        )?;
        pages_fetched += action_pages;
        for action in actions {
            match apply_local_action(projection, host_id, &account.tenant_id, action) {
                Ok(SyncOutcome::Applied) => applied += 1,
                Ok(SyncOutcome::Duplicate) | Err(OrgError::Unlinked) => {}
                Err(error) => return Err(error.into()),
            }
        }

        let outbox_acknowledged = self.flush_outbox(projection)?;
        Ok(PortalReconcileOutcome {
            kind: PortalReconcileKind::Reconciled,
            applied_facts: applied,
            pages_fetched,
            outbox_acknowledged,
            capability: OrganizationCapabilityState::Enabled,
        })
    }

    fn flush_outbox(
        &self,
        projection: &mut OrganizationProjection,
    ) -> Result<u32, PortalSyncError> {
        let pending: Vec<PersistedOutboxIntent> =
            projection.pending_outbox_intents().cloned().collect();
        let mut acknowledged = 0u32;
        for intent in pending {
            let request = TelemetryUploadRequest {
                observation_id: intent.observation_id_hex.clone(),
                intent: intent.intent.clone(),
                content_hash: intent.observation_id_hex.clone(),
                bundle_ref: (intent.intent == "evidence_metadata")
                    .then(|| intent.observation_id_hex.clone()),
                metadata: serde_json::Map::new(),
                raw_content: false,
            };
            request.validate()?;
            let ack = self.client.upload_telemetry(&request)?;
            if !ack.accepted || ack.observation_id != intent.observation_id_hex {
                return Err(PortalAdapterError::InvalidValue {
                    field: "observationId".into(),
                    reason: "Portal did not accept the matching idempotent outbox intent".into(),
                }
                .into());
            }
            match projection.acknowledge_outbox_intent(&intent.observation_id_hex)? {
                SyncOutcome::Applied | SyncOutcome::Duplicate => acknowledged += 1,
            }
        }
        Ok(acknowledged)
    }
}

fn collect_pages<T, F>(
    mut fetch: F,
    cursor_slot: &mut Option<String>,
) -> Result<(Vec<T>, u32), PortalSyncError>
where
    F: FnMut(Option<&str>) -> Result<PortalPage<T>, PortalAdapterError>,
{
    let _ = bound_page_limit(PORTAL_SYNC_PAGE_LIMIT)?;
    let mut items = Vec::new();
    let mut cursor = cursor_slot.clone();
    let mut pages = 0u32;
    loop {
        if pages >= PORTAL_SYNC_MAX_PAGES {
            return Err(OrgError::BoundExceeded.into());
        }
        let page = fetch(cursor.as_deref())?;
        pages += 1;
        if page.items.len() > PORTAL_SYNC_PAGE_LIMIT as usize {
            return Err(PortalAdapterError::InvalidValue {
                field: "items".into(),
                reason: "page exceeds sync bound".into(),
            }
            .into());
        }
        items.extend(page.items);
        match page.next_cursor {
            Some(next) => {
                validate_opaque_id(&next, "nextCursor")?;
                cursor = Some(next);
                *cursor_slot = cursor.clone();
            }
            None => {
                *cursor_slot = None;
                return Ok((items, pages));
            }
        }
    }
}

fn scoped_membership(
    projection: &OrganizationProjection,
    last_verified: bool,
) -> Result<&HostMembership, OrgError> {
    match projection.capability_state() {
        OrganizationCapabilityState::Enabled if last_verified => projection
            .membership()
            .ok_or(OrgError::HostUnenrolled),
        OrganizationCapabilityState::Enabled => Err(OrgError::Offline),
        OrganizationCapabilityState::Disabled(reason) => Err(match reason {
            OrganizationCapabilityDisableReason::Standalone => OrgError::StandaloneMode,
            OrganizationCapabilityDisableReason::Unenrolled => OrgError::HostUnenrolled,
            OrganizationCapabilityDisableReason::Revoked => OrgError::MembershipRevoked,
            OrganizationCapabilityDisableReason::Expired => OrgError::Expired,
            OrganizationCapabilityDisableReason::Offline => OrgError::Offline,
        }),
    }
}

fn signed_in_account(projection: &OrganizationProjection) -> Option<&ExternalAccount> {
    match projection.mode() {
        crate::org::OperatingMode::ConnectSignedIn { account } => Some(account),
        crate::org::OperatingMode::HostEnrolled { .. }
        | crate::org::OperatingMode::AnonymousLocal => None,
    }
}

fn uuid_from_host(host_id: ConnectHostId) -> Uuid {
    host_id.as_uuid()
}

fn parse_host_id(value: &str) -> Result<ConnectHostId, OrgError> {
    let uuid = Uuid::parse_str(value).map_err(|_| OrgError::EmptyIdentity)?;
    ConnectHostId::from_uuid(uuid).map_err(|_| OrgError::EmptyIdentity)
}

fn validate_membership_scope(
    account: &ExternalAccount,
    host_id: ConnectHostId,
    membership: &HostMembershipDto,
) -> Result<(), OrgError> {
    validate_opaque_id(&membership.tenant_id, "tenantId").map_err(|_| OrgError::EmptyIdentity)?;
    if membership.tenant_id != account.tenant_id.as_str()
        || membership.user_id != account.account_id.as_str()
        || membership.organization_id != account.tenant_id.as_str()
    {
        return Err(OrgError::CrossTenant);
    }
    if parse_host_id(&membership.host_id)? != host_id {
        return Err(OrgError::HostUnenrolled);
    }
    Ok(())
}

fn apply_membership_fact(
    projection: &mut OrganizationProjection,
    host_id: ConnectHostId,
    reconcile: &HostReconcileResponse,
    now_ms: i64,
) -> Result<SyncOutcome, OrgError> {
    let tenant_id = PortalTenantId::parse(&reconcile.membership.tenant_id)
        .map_err(|_| OrgError::EmptyIdentity)?;
    let account_id = PortalAccountId::parse(&reconcile.membership.user_id)
        .map_err(|_| OrgError::EmptyIdentity)?;
    projection.apply_authoritative_fact(
        OrganizationFact::Membership {
            host_id,
            tenant_id,
            account_id,
            status: map_status(reconcile.membership.status),
            revision: reconcile.membership_revision,
            revoked_at_ms: reconcile
                .membership
                .revoked_at
                .as_deref()
                .map(iso_timestamp_to_ms)
                .transpose()?,
            expires_at_ms: reconcile
                .grant_expires_at
                .as_deref()
                .or(reconcile.membership.grant_expires_at.as_deref())
                .map(iso_timestamp_to_ms)
                .transpose()?,
        },
        now_ms,
    )
}

fn apply_managed_task(
    projection: &mut OrganizationProjection,
    host_id: ConnectHostId,
    tenant_id: &PortalTenantId,
    task: ManagedTaskDto,
    now_ms: i64,
) -> Result<SyncOutcome, OrgError> {
    if task.link.organization_id != tenant_id.as_str() {
        return Err(OrgError::CrossTenant);
    }
    if parse_host_id(&task.link.host_id)? != host_id {
        return Err(OrgError::HostUnenrolled);
    }
    let local_task_id = parse_task_id(&task.link.local_task_id)?;
    let link_id = ManagedLinkId::parse(&task.link.id).map_err(|_| OrgError::EmptyIdentity)?;
    let metadata_policy_version = u32::try_from(task.link.metadata_policy_version)
        .map_err(|_| OrgError::StalePolicy)?;
    if metadata_policy_version == 0 || task.link.portal_revision == 0 {
        return Err(OrgError::StalePolicy);
    }
    let snapshot = ManagedTaskSnapshot {
        host_id,
        local_task_id,
        board_card_id: BoardCardId::parse(&task.link.board_card_id)
            .map_err(|_| OrgError::EmptyIdentity)?,
        enrollment_state: map_enrollment(task.link.enrollment_state)?,
        portal_revision: task.link.portal_revision,
        metadata_policy_version,
        linked_by: task.link.linked_by,
        linked_at: iso_timestamp_to_ms(&task.link.linked_at)?,
        unlinked_at: task
            .link
            .unlinked_at
            .as_deref()
            .map(iso_timestamp_to_ms)
            .transpose()?,
        link_id,
        tenant_id: tenant_id.clone(),
        portal_title: task
            .board_card
            .as_ref()
            .map(|card| card.title.clone())
            .or(task.local_projection.title),
    };
    projection.apply_authoritative_fact(OrganizationFact::ManagedTask(snapshot), now_ms)
}

fn apply_local_action(
    projection: &mut OrganizationProjection,
    host_id: ConnectHostId,
    tenant_id: &PortalTenantId,
    action: crate::org::portal::LocalActionDto,
) -> Result<SyncOutcome, OrgError> {
    if action.organization_id != tenant_id.as_str() {
        return Err(OrgError::CrossTenant);
    }
    if parse_host_id(&action.host_id)? != host_id {
        return Err(OrgError::HostUnenrolled);
    }
    let request_id = LocalActionId::parse(&action.request_id).map_err(|_| OrgError::EmptyIdentity)?;
    let admission = match action.admission_status {
        PortalAdmissionStatus::Pending | PortalAdmissionStatus::Accepted => Admission::Accepted,
        PortalAdmissionStatus::Rejected => Admission::Rejected,
    };
    let outcome = action.outcome_status.map(|status| match status {
        PortalOutcomeStatus::Settled => ActionOutcome::Settled,
        PortalOutcomeStatus::Failed => ActionOutcome::Failed,
        PortalOutcomeStatus::Cancelled => ActionOutcome::Cancelled,
        PortalOutcomeStatus::Uncertain => ActionOutcome::Uncertain,
    });
    projection.reconcile_local_action_state(request_id, admission, outcome)
}

fn prompt_snapshot_from_portal(
    tenant_id: &PortalTenantId,
    prompts: &[OrgPromptDto],
    versions: Vec<OrgPromptVersionDto>,
    chains: Vec<OrgPromptChainDto>,
) -> Result<Option<OrganizationPromptSnapshot>, OrgError> {
    if prompts.is_empty() && versions.is_empty() && chains.is_empty() {
        return Ok(None);
    }
    let mut converted_prompts = Vec::new();
    for prompt in prompts {
        if prompt.organization_id != tenant_id.as_str() {
            return Err(OrgError::CrossTenant);
        }
        let Some(current_version_id) = prompt.current_version_id.as_deref() else {
            continue;
        };
        converted_prompts.push(OrgPrompt {
            prompt_id: OrgPromptId::parse(&prompt.id).map_err(|_| OrgError::EmptyIdentity)?,
            tenant_id: tenant_id.clone(),
            namespace: prompt.namespace.clone(),
            name: prompt.name.clone(),
            current_version_id: OrgPromptVersionId::parse(current_version_id)
                .map_err(|_| OrgError::EmptyIdentity)?,
            lifecycle: match prompt.status {
                PortalPromptStatus::Published => PromptLifecycle::Published,
                PortalPromptStatus::Deprecated => PromptLifecycle::Deprecated,
            },
        });
    }
    let converted_versions = versions
        .into_iter()
        .map(|version| {
            if version.organization_id != tenant_id.as_str() {
                return Err(OrgError::CrossTenant);
            }
            Ok(OrgPromptVersion {
                prompt_id: OrgPromptId::parse(&version.prompt_id)
                    .map_err(|_| OrgError::EmptyIdentity)?,
                version_id: OrgPromptVersionId::parse(&version.id)
                    .map_err(|_| OrgError::EmptyIdentity)?,
                author: PortalAccountId::parse(&version.author_user_id)
                    .map_err(|_| OrgError::EmptyIdentity)?,
                title: version.title,
                tags: Vec::new(),
                body: version.body,
                content_hash_hex: version.content_hash,
                published_at_ms: iso_timestamp_to_ms(&version.published_at)?,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let converted_chains = chains
        .into_iter()
        .map(|chain| {
            if chain.organization_id != tenant_id.as_str() {
                return Err(OrgError::CrossTenant);
            }
            if chain.revision == 0 {
                return Err(OrgError::StalePolicy);
            }
            Ok(OrgPromptChain {
                chain_id: OrgPromptChainId::parse(&chain.id).map_err(|_| OrgError::EmptyIdentity)?,
                tenant_id: tenant_id.clone(),
                revision: u32::try_from(chain.revision).map_err(|_| OrgError::BoundExceeded)?,
                links: chain
                    .links
                    .into_iter()
                    .map(|link| {
                        if link.organization_id != tenant_id.as_str() {
                            return Err(OrgError::CrossTenant);
                        }
                        Ok(OrgPromptChainLink {
                            position: link.position,
                            version_id: OrgPromptVersionId::parse(&link.prompt_version_id)
                                .map_err(|_| OrgError::EmptyIdentity)?,
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(OrganizationPromptSnapshot {
        tenant_id: tenant_id.clone(),
        revision: 1,
        prompts: converted_prompts,
        versions: converted_versions,
        chains: converted_chains,
    }))
}

fn catalog_from_dto(
    entries: Vec<LocalActionCatalogDto>,
) -> Result<Vec<LocalActionCatalogEntry>, OrgError> {
    entries
        .into_iter()
        .map(|entry| {
            let kind = map_action_kind(entry.action_kind);
            let risk = map_action_risk(entry.risk);
            Ok(LocalActionCatalogEntry {
                kind,
                version: entry.action_version,
                replay_policy: crate::org::LocalActionRegistry::replay_policy(
                    kind,
                    entry.action_version,
                )?,
                risk,
            })
        })
        .collect()
}

fn policy_from_dto(
    dto: &OrganizationPolicyDto,
    tenant_id: PortalTenantId,
) -> Result<OrganizationPolicyDocument, OrgError> {
    let local = dto.to_local_units().map_err(|_| OrgError::CorruptState)?;
    if local.raw_sharing_ceiling != PortalRawSharingCeiling::None {
        return Err(OrgError::ProhibitedField);
    }
    let mut allowed_metadata_fields = BTreeSet::new();
    for field in &local.metadata_fields {
        allowed_metadata_fields.insert(parse_policy_field(field)?);
    }
    OrganizationPolicyDocument {
        revision: local.revision,
        tenant_id,
        allowed_metadata_fields,
        retention_ms: local.retention_ms,
        idle_interval_ms: local.idle_interval_ms,
        raw_sharing_ceiling: RawSharingCeiling::None,
        local_action_approval: crate::org::LocalActionApprovalRequirement::OwnerRequired,
        prompt_maintainer_accounts: BTreeSet::new(),
        content_hash_hex: String::new(),
    }
    .finalize()
}

fn parse_policy_field(value: &str) -> Result<ManagedMetadataName, OrgError> {
    match value {
        "task_state" | "status" => Ok(ManagedMetadataName::TaskState),
        "task_attention" | "attention" => Ok(ManagedMetadataName::TaskAttention),
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

fn map_role(role: PortalOrgRole) -> MembershipRole {
    match role {
        PortalOrgRole::Owner => MembershipRole::Owner,
        PortalOrgRole::Admin => MembershipRole::Admin,
        PortalOrgRole::Manager => MembershipRole::Manager,
        PortalOrgRole::Member | PortalOrgRole::PromptMaintainer => MembershipRole::Member,
    }
}

fn map_status(status: PortalMembershipStatus) -> MembershipStatus {
    match status {
        PortalMembershipStatus::Pending => MembershipStatus::PendingLocalConfirm,
        PortalMembershipStatus::Enrolled => MembershipStatus::Enrolled,
        PortalMembershipStatus::Revoked => MembershipStatus::Revoked,
        PortalMembershipStatus::Unenrolled => MembershipStatus::Unenrolled,
    }
}

fn map_enrollment(state: PortalEnrollmentState) -> Result<EnrollmentState, OrgError> {
    match state {
        PortalEnrollmentState::PendingOwnerAccept | PortalEnrollmentState::PendingHostConfirm => {
            Ok(EnrollmentState::PendingOwnerAccept)
        }
        PortalEnrollmentState::Enrolled => Ok(EnrollmentState::Enrolled),
        PortalEnrollmentState::Unenrolled | PortalEnrollmentState::Personal => {
            Ok(EnrollmentState::Unlinked)
        }
        PortalEnrollmentState::Tombstoned => Ok(EnrollmentState::Closed),
    }
}

fn map_action_kind(kind: PortalActionKind) -> LocalActionKind {
    match kind {
        PortalActionKind::DbSchemaInspect => LocalActionKind::DbSchemaIntrospect,
        PortalActionKind::DbChangeApply => LocalActionKind::DbApprovedChangeApply,
        PortalActionKind::EnvDiff => LocalActionKind::EnvDiff,
        PortalActionKind::EnvApply => LocalActionKind::EnvApprovedApply,
    }
}

fn map_action_risk(risk: PortalActionRisk) -> ActionRisk {
    match risk {
        PortalActionRisk::Low | PortalActionRisk::Standard => ActionRisk::Low,
        PortalActionRisk::Production => ActionRisk::Production,
    }
}

fn parse_task_id(value: &str) -> Result<TaskId, OrgError> {
    let uuid = Uuid::parse_str(value).map_err(|_| OrgError::EmptyIdentity)?;
    TaskId::from_bytes(uuid.into_bytes()).map_err(|_| OrgError::EmptyIdentity)
}

fn iso_timestamp_to_ms(value: &str) -> Result<i64, OrgError> {
    validate_iso_timestamp(value, "timestamp").map_err(|_| OrgError::CorruptState)?;
    let bytes = value.as_bytes();
    let year: i64 = parse_digits(&bytes[0..4])?;
    let month: i64 = parse_digits(&bytes[5..7])?;
    let day: i64 = parse_digits(&bytes[8..10])?;
    let hour: i64 = parse_digits(&bytes[11..13])?;
    let minute: i64 = parse_digits(&bytes[14..16])?;
    let second: i64 = parse_digits(&bytes[17..19])?;
    let mut index = 19;
    let mut millis = 0i64;
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        let start = index;
        while bytes.get(index).is_some_and(|byte| byte.is_ascii_digit()) {
            index += 1;
        }
        let mut frac = std::str::from_utf8(&bytes[start..index])
            .map_err(|_| OrgError::CorruptState)?
            .to_string();
        while frac.len() < 3 {
            frac.push('0');
        }
        millis = frac[..3].parse().map_err(|_| OrgError::CorruptState)?;
    }
    let offset_min = match bytes.get(index) {
        Some(b'Z') => 0,
        Some(b'+') | Some(b'-') => {
            let sign = if bytes[index] == b'+' { 1 } else { -1 };
            let hours = parse_digits(&bytes[index + 1..index + 3])?;
            let minutes = parse_digits(&bytes[index + 4..index + 6])?;
            sign * (hours * 60 + minutes)
        }
        _ => return Err(OrgError::CorruptState),
    };
    let days = days_from_civil(year, month, day)?;
    Ok(days * 86_400_000 + hour * 3_600_000 + minute * 60_000 + second * 1_000 + millis
        - offset_min * 60_000)
}

fn parse_digits(bytes: &[u8]) -> Result<i64, OrgError> {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|value| value.parse().ok())
        .ok_or(OrgError::CorruptState)
}

fn days_from_civil(year: i64, month: i64, day: i64) -> Result<i64, OrgError> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return Err(OrgError::CorruptState);
    }
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Ok(era * 146_097 + doe - 719_468)
}

fn bound_page_limit(limit: u32) -> Result<u32, PortalAdapterError> {
    if limit == 0 || limit > PORTAL_PAGE_MAX_ITEMS {
        return Err(PortalAdapterError::InvalidValue {
            field: "limit".into(),
            reason: format!("must be 1..={PORTAL_PAGE_MAX_ITEMS}"),
        });
    }
    Ok(limit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::org::evidence::{compute_bundle_hash, EvidenceBundle};
    use crate::org::ids::EvidenceBundleId as BundleId;
    use crate::org::persistence::OutboxDeliveryState;
    use crate::org::portal::{MembershipAudit, PortalTransport};
    use crate::org::{
        HostMembership, MembershipRole, OrganizationPolicyDocument, OrganizationProjection,
        PortalAccountId, PortalTenantId,
    };
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::fs;

    #[derive(Default)]
    struct FakePortal {
        inner: RefCell<FakeInner>,
    }

    #[derive(Default)]
    struct FakeInner {
        fail_auth: bool,
        fail_transport: bool,
        infinite_task_pages: bool,
        reconcile: Option<HostReconcileResponse>,
        task_pages: Vec<PortalPage<ManagedTaskDto>>,
        prompt_pages: Vec<PortalPage<OrgPromptDto>>,
        versions: BTreeMap<String, Vec<OrgPromptVersionDto>>,
        chain_pages: Vec<PortalPage<OrgPromptChainDto>>,
        catalog: Vec<LocalActionCatalogDto>,
        action_pages: Vec<PortalPage<crate::org::portal::LocalActionDto>>,
        telemetry: Vec<TelemetryUploadRequest>,
        evidence: Vec<CanonicalEvidenceImportRequest>,
        calls: usize,
        task_page_calls: usize,
        telemetry_calls: usize,
    }

    impl FakePortal {
        fn with_reconcile(response: HostReconcileResponse) -> Self {
            let fake = Self::default();
            fake.inner.borrow_mut().reconcile = Some(response);
            fake
        }

        fn calls(&self) -> usize {
            self.inner.borrow().calls
        }

        fn telemetry_calls(&self) -> usize {
            self.inner.borrow().telemetry_calls
        }

        fn set_fail_auth(&self, value: bool) {
            self.inner.borrow_mut().fail_auth = value;
        }

        fn set_infinite_task_pages(&self, value: bool) {
            self.inner.borrow_mut().infinite_task_pages = value;
        }

        fn set_reconcile(&self, response: HostReconcileResponse) {
            self.inner.borrow_mut().reconcile = Some(response);
        }

        fn set_task_pages(&self, pages: Vec<PortalPage<ManagedTaskDto>>) {
            let mut inner = self.inner.borrow_mut();
            inner.task_pages = pages;
            inner.task_page_calls = 0;
        }

        fn recorded_telemetry(&self) -> Vec<TelemetryUploadRequest> {
            self.inner.borrow().telemetry.clone()
        }

        fn recorded_evidence(&self) -> Vec<CanonicalEvidenceImportRequest> {
            self.inner.borrow().evidence.clone()
        }
    }

    impl PortalTransport for FakePortal {
        fn reconcile_host(
            &self,
            _request: &HostReconcileRequest,
        ) -> Result<HostReconcileResponse, PortalAdapterError> {
            let mut inner = self.inner.borrow_mut();
            inner.calls += 1;
            if inner.fail_auth {
                return Err(PortalAdapterError::Http {
                    status: 401,
                    code: Some("unauthorized".into()),
                    message: "auth failed".into(),
                });
            }
            if inner.fail_transport {
                return Err(PortalAdapterError::Transport("offline".into()));
            }
            inner
                .reconcile
                .clone()
                .ok_or_else(|| PortalAdapterError::Transport("no reconcile fixture".into()))
        }

        fn get_policy(&self) -> Result<OrganizationPolicyDto, PortalAdapterError> {
            self.inner
                .borrow()
                .reconcile
                .as_ref()
                .map(|value| value.policy.clone())
                .ok_or_else(|| PortalAdapterError::Transport("no policy".into()))
        }

        fn enrollment_preview(
            &self,
            _request: &crate::org::portal::EnrollmentPreviewRequest,
        ) -> Result<crate::org::portal::EnrollmentPreviewDto, PortalAdapterError> {
            Err(PortalAdapterError::Transport("unused".into()))
        }

        fn list_tasks_page(
            &self,
            _cursor: Option<&str>,
            _limit: u32,
        ) -> Result<PortalPage<ManagedTaskDto>, PortalAdapterError> {
            let mut inner = self.inner.borrow_mut();
            inner.calls += 1;
            inner.task_page_calls += 1;
            if inner.infinite_task_pages {
                return Ok(PortalPage {
                    items: Vec::new(),
                    next_cursor: Some(format!("cursor-{}", inner.task_page_calls)),
                });
            }
            let index = inner.task_page_calls.saturating_sub(1);
            Ok(inner.task_pages.get(index).cloned().unwrap_or(PortalPage {
                items: Vec::new(),
                next_cursor: None,
            }))
        }

        fn list_prompts_page(
            &self,
            _cursor: Option<&str>,
            _limit: u32,
        ) -> Result<PortalPage<OrgPromptDto>, PortalAdapterError> {
            let inner = self.inner.borrow();
            Ok(inner.prompt_pages.first().cloned().unwrap_or(PortalPage {
                items: Vec::new(),
                next_cursor: None,
            }))
        }

        fn get_prompt(&self, _prompt_id: &str) -> Result<OrgPromptDto, PortalAdapterError> {
            Err(PortalAdapterError::Http {
                status: 404,
                code: None,
                message: "missing".into(),
            })
        }

        fn list_prompt_versions(
            &self,
            prompt_id: &str,
        ) -> Result<Vec<OrgPromptVersionDto>, PortalAdapterError> {
            Ok(self
                .inner
                .borrow()
                .versions
                .get(prompt_id)
                .cloned()
                .unwrap_or_default())
        }

        fn publish_prompt(
            &self,
            _request: &PublishPromptRequest,
        ) -> Result<serde_json::Value, PortalAdapterError> {
            Ok(serde_json::json!({"ok": true}))
        }

        fn list_prompt_chains_page(
            &self,
            _cursor: Option<&str>,
            _limit: u32,
        ) -> Result<PortalPage<OrgPromptChainDto>, PortalAdapterError> {
            Ok(self
                .inner
                .borrow()
                .chain_pages
                .first()
                .cloned()
                .unwrap_or(PortalPage {
                    items: Vec::new(),
                    next_cursor: None,
                }))
        }

        fn list_action_catalog(
            &self,
            _host_id: &str,
        ) -> Result<Vec<LocalActionCatalogDto>, PortalAdapterError> {
            Ok(self.inner.borrow().catalog.clone())
        }

        fn list_actions_page(
            &self,
            _host_id: Option<&str>,
            _cursor: Option<&str>,
            _limit: u32,
        ) -> Result<PortalPage<crate::org::portal::LocalActionDto>, PortalAdapterError> {
            Ok(self
                .inner
                .borrow()
                .action_pages
                .first()
                .cloned()
                .unwrap_or(PortalPage {
                    items: Vec::new(),
                    next_cursor: None,
                }))
        }

        fn receive_action_receipt(
            &self,
            _request_id: &str,
            _request: &LocalActionReceiptRequest,
        ) -> Result<crate::org::portal::LocalActionReceiptDto, PortalAdapterError> {
            Err(PortalAdapterError::Transport("unused".into()))
        }

        fn upload_telemetry(
            &self,
            request: &TelemetryUploadRequest,
        ) -> Result<crate::org::portal::TelemetryUploadAck, PortalAdapterError> {
            request.validate()?;
            let mut inner = self.inner.borrow_mut();
            inner.telemetry_calls += 1;
            inner.telemetry.push(request.clone());
            Ok(crate::org::portal::TelemetryUploadAck {
                observation_id: request.observation_id.clone(),
                accepted: true,
            })
        }

        fn import_evidence(
            &self,
            request: &CanonicalEvidenceImportRequest,
        ) -> Result<serde_json::Value, PortalAdapterError> {
            request.validate()?;
            self.inner.borrow_mut().evidence.push(request.clone());
            Ok(serde_json::json!({"accepted": true}))
        }
    }

    fn tenant() -> PortalTenantId {
        PortalTenantId::parse("acme").expect("tenant")
    }

    fn account() -> ExternalAccount {
        ExternalAccount::new(
            tenant(),
            PortalAccountId::parse("owner-1").expect("account"),
            None,
        )
    }

    fn host_str(host_id: ConnectHostId) -> String {
        host_id.as_uuid().to_string()
    }

    fn policy_dto() -> OrganizationPolicyDto {
        OrganizationPolicyDto {
            revision: "1".into(),
            enrollment_required: true,
            metadata_fields: vec![
                "task_state".into(),
                "task_attention".into(),
                "source_timestamp".into(),
                "observed_timestamp".into(),
            ],
            retention_days: 1,
            idle_interval_minutes: 15,
            raw_sharing_ceiling: PortalRawSharingCeiling::None,
            local_action_approval_required: true,
            auto_accept_managed_tasks: false,
        }
    }

    fn membership_dto(host: &str, status: PortalMembershipStatus) -> HostMembershipDto {
        HostMembershipDto {
            id: "membership-1".into(),
            organization_id: "acme".into(),
            tenant_id: "acme".into(),
            user_id: "owner-1".into(),
            host_id: host.to_string(),
            role: PortalOrgRole::Owner,
            status,
            display_label: Some("owner-host".into()),
            policy_revision: 1,
            capabilities: vec!["organization".into()],
            enrolled_at: Some("2026-08-12T00:00:00Z".into()),
            last_seen_at: Some("2026-08-12T00:00:00Z".into()),
            revoked_at: None,
            grant_expires_at: None,
            audit: MembershipAudit {
                last_event: None,
                last_event_at: None,
            },
        }
    }

    fn reconcile_ok(host: &str) -> HostReconcileResponse {
        HostReconcileResponse {
            membership: membership_dto(host, PortalMembershipStatus::Enrolled),
            policy: policy_dto(),
            membership_revision: 1,
            local_confirmation_required: true,
            grant_expires_at: None,
        }
    }

    fn temp_store() -> (std::path::PathBuf, OrganizationStateStore) {
        let root = std::env::temp_dir().join(format!(
            "devmanager-org-sync-{}",
            Uuid::now_v7().as_simple()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("temp root");
        let store = OrganizationStateStore::open(&root);
        (root, store)
    }

    fn observation(tag: u8) -> String {
        format!("{tag:02x}{}", "ab".repeat(31))
    }

    #[test]
    fn standalone_is_noop_and_disabled() {
        let fake = FakePortal::default();
        let mut runtime = PortalSyncRuntime::new(fake, None);
        let mut projection = OrganizationProjection::standalone();
        let (root, store) = temp_store();
        let host_id = ConnectHostId::new();
        let outcome = runtime
            .reconcile(
                &mut projection,
                &store,
                PortalReconcileRequest {
                    host_id,
                    local_confirmation: true,
                },
                1_000,
            )
            .expect("standalone");
        assert_eq!(outcome.kind, PortalReconcileKind::StandaloneNoop);
        assert_eq!(
            runtime.capability(&projection),
            OrganizationCapabilityState::Disabled(
                OrganizationCapabilityDisableReason::Standalone
            )
        );
        assert_eq!(runtime.transport().calls(), 0);
        assert!(store.load().is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn connect_sign_in_does_not_enroll_without_local_confirmation() {
        let host_id = ConnectHostId::new();
        let fake = FakePortal::with_reconcile(reconcile_ok(&host_str(host_id)));
        let mut runtime = PortalSyncRuntime::new(fake, None);
        let mut projection = OrganizationProjection::standalone();
        assert_eq!(projection.sign_in(account()), 0);
        let (root, store) = temp_store();
        let outcome = runtime
            .reconcile(
                &mut projection,
                &store,
                PortalReconcileRequest {
                    host_id,
                    local_confirmation: false,
                },
                1_000,
            )
            .expect("preview");
        assert_eq!(outcome.kind, PortalReconcileKind::PreviewOnly);
        assert!(matches!(
            projection.mode(),
            crate::org::OperatingMode::ConnectSignedIn { .. }
        ));
        assert_eq!(
            runtime.capability(&projection),
            OrganizationCapabilityState::Disabled(
                OrganizationCapabilityDisableReason::Unenrolled
            )
        );
        assert_eq!(
            runtime.publish_prompt(
                &projection,
                &PublishPromptRequest {
                    namespace: "ops".into(),
                    name: "n".into(),
                    title: "t".into(),
                    body: "b".into(),
                    tags: Vec::new(),
                    expected_current_version_id: None,
                    expected_revision: None,
                }
            ),
            Err(PortalSyncError::Org(OrgError::HostUnenrolled))
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn auth_failure_clears_capability() {
        let host_id = ConnectHostId::new();
        let mut projection = OrganizationProjection::standalone();
        assert_eq!(projection.sign_in(account()), 0);
        let policy = OrganizationPolicyDocument::deny_minimal(tenant()).expect("policy");
        let pending = HostMembership::pending(
            host_id,
            account(),
            MembershipRole::Owner,
            &policy,
            "owner-host",
        )
        .expect("pending");
        projection
            .confirm_enrollment(pending, policy, 1_000)
            .expect("enrolled");
        assert_eq!(
            projection.capability_state(),
            OrganizationCapabilityState::Enabled
        );

        let fake = FakePortal::with_reconcile(reconcile_ok(&host_str(host_id)));
        fake.set_fail_auth(true);
        let mut runtime = PortalSyncRuntime::new(fake, None);
        let (root, store) = temp_store();
        store.save(&projection).expect("save enrolled");
        let error = runtime
            .reconcile(
                &mut projection,
                &store,
                PortalReconcileRequest {
                    host_id,
                    local_confirmation: true,
                },
                2_000,
            )
            .expect_err("auth");
        assert_eq!(error.kind(), PortalSyncFailureKind::Auth);
        assert_eq!(
            runtime.capability(&projection),
            OrganizationCapabilityState::Disabled(OrganizationCapabilityDisableReason::Offline)
        );
        let restored = store.load().expect("load");
        assert!(!restored.authenticated_online());
        assert_eq!(
            restored.capability_state(),
            OrganizationCapabilityState::Disabled(OrganizationCapabilityDisableReason::Offline)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn successful_reconciliation_enables_only_after_verified_response() {
        let host_id = ConnectHostId::new();
        let handle = PortalCredentialHandle::parse("vault:org-portal").expect("handle");
        let fake = FakePortal::with_reconcile(reconcile_ok(&host_str(host_id)));
        let mut runtime = PortalSyncRuntime::new(fake, Some(handle.clone()));
        let mut projection = OrganizationProjection::standalone();
        assert_eq!(projection.sign_in(account()), 0);
        assert_eq!(
            runtime.capability(&projection),
            OrganizationCapabilityState::Disabled(
                OrganizationCapabilityDisableReason::Unenrolled
            )
        );
        let (root, store) = temp_store();
        let outcome = runtime
            .reconcile(
                &mut projection,
                &store,
                PortalReconcileRequest {
                    host_id,
                    local_confirmation: true,
                },
                1_000,
            )
            .expect("reconcile");
        assert_eq!(outcome.kind, PortalReconcileKind::Reconciled);
        assert_eq!(
            runtime.capability(&projection),
            OrganizationCapabilityState::Enabled
        );
        assert!(runtime.last_cycle_verified());
        let restored = store.load().expect("load");
        assert_eq!(restored.sync_state(), OrganizationSyncState::Enrolled);
        let cold = PortalSyncRuntime::new(FakePortal::default(), Some(handle));
        assert_eq!(
            cold.capability(&restored),
            OrganizationCapabilityState::Disabled(OrganizationCapabilityDisableReason::Offline)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn stale_or_wrong_tenant_is_rejected_without_persisting_payload() {
        let host_id = ConnectHostId::new();
        let mut other = reconcile_ok(&host_str(host_id));
        other.membership.tenant_id = "other".into();
        other.membership.organization_id = "other".into();
        let fake = FakePortal::with_reconcile(other);
        let mut runtime = PortalSyncRuntime::new(fake, None);
        let mut projection = OrganizationProjection::standalone();
        assert_eq!(projection.sign_in(account()), 0);
        let (root, store) = temp_store();
        let error = runtime
            .reconcile(
                &mut projection,
                &store,
                PortalReconcileRequest {
                    host_id,
                    local_confirmation: true,
                },
                1_000,
            )
            .expect_err("cross tenant");
        assert_eq!(error.kind(), PortalSyncFailureKind::Validation);
        assert!(projection.membership().is_none());
        let restored = store.load().expect("signed-in only");
        assert!(restored.membership().is_none());
        assert!(restored.persisted_links().next().is_none());
        assert_eq!(restored.sync_state(), OrganizationSyncState::SignedIn);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn wrong_tenant_task_is_rejected_without_persisting_links() {
        let host_id = ConnectHostId::new();
        let host = host_str(host_id);
        let fake = FakePortal::with_reconcile(reconcile_ok(&host));
        let mut runtime = PortalSyncRuntime::new(fake, None);
        let mut projection = OrganizationProjection::standalone();
        assert_eq!(projection.sign_in(account()), 0);
        let (root, store) = temp_store();
        runtime
            .reconcile(
                &mut projection,
                &store,
                PortalReconcileRequest {
                    host_id,
                    local_confirmation: true,
                },
                1_000,
            )
            .expect("enroll");
        runtime.transport().set_task_pages(vec![PortalPage {
            items: vec![ManagedTaskDto {
                link: crate::org::portal::ManagedTaskLink {
                    id: Uuid::now_v7().to_string(),
                    organization_id: "other".into(),
                    host_id: host,
                    local_task_id: Uuid::now_v7().to_string(),
                    board_card_id: "card-1".into(),
                    enrollment_state: PortalEnrollmentState::Enrolled,
                    local_revision: 1,
                    portal_revision: 1,
                    metadata_policy_version: 1,
                    linked_by: "owner-1".into(),
                    linked_at: "2026-08-12T00:00:00Z".into(),
                    unlinked_at: None,
                },
                accepted_at: None,
                title_conflict: None,
                local_projection: crate::org::portal::LocalTaskProjection {
                    title: Some("t".into()),
                    status: None,
                    attention: None,
                    provider_kind: None,
                    provider_state: None,
                    local_revision: 1,
                },
                board_card: None,
            }],
            next_cursor: None,
        }]);
        let error = runtime
            .reconcile(
                &mut projection,
                &store,
                PortalReconcileRequest {
                    host_id,
                    local_confirmation: true,
                },
                2_000,
            )
            .expect_err("cross tenant task");
        assert_eq!(error.kind(), PortalSyncFailureKind::Validation);
        let restored = store.load().expect("previous enrolled state");
        assert!(restored.persisted_links().next().is_none());
        assert_eq!(restored.sync_state(), OrganizationSyncState::Enrolled);
        assert!(!restored.authenticated_online());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn bounded_page_behavior_rejects_unbounded_cursors() {
        let host_id = ConnectHostId::new();
        let fake = FakePortal::with_reconcile(reconcile_ok(&host_str(host_id)));
        fake.set_infinite_task_pages(true);
        let mut runtime = PortalSyncRuntime::new(fake, None);
        let mut projection = OrganizationProjection::standalone();
        assert_eq!(projection.sign_in(account()), 0);
        let (root, store) = temp_store();
        let error = runtime
            .reconcile(
                &mut projection,
                &store,
                PortalReconcileRequest {
                    host_id,
                    local_confirmation: true,
                },
                1_000,
            )
            .expect_err("bound");
        assert_eq!(error, PortalSyncError::Org(OrgError::BoundExceeded));
        let restored = store.load().expect("no enrolled persist");
        assert!(restored.membership().is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn outbox_retry_is_idempotent() {
        let host_id = ConnectHostId::new();
        let fake = FakePortal::with_reconcile(reconcile_ok(&host_str(host_id)));
        let mut runtime = PortalSyncRuntime::new(fake, None);
        let mut projection = OrganizationProjection::standalone();
        assert_eq!(projection.sign_in(account()), 0);
        let (root, store) = temp_store();
        runtime
            .reconcile(
                &mut projection,
                &store,
                PortalReconcileRequest {
                    host_id,
                    local_confirmation: true,
                },
                1_000,
            )
            .expect("enroll");
        let intent = PersistedOutboxIntent {
            observation_id_hex: observation(1),
            intent: "watcher_fleet".into(),
            publication_queued: true,
            delivery: OutboxDeliveryState::Queued,
        };
        assert_eq!(
            projection.queue_outbox_intent(intent.clone()).expect("queue"),
            SyncOutcome::Applied
        );
        assert_eq!(
            projection.queue_outbox_intent(intent).expect("dup"),
            SyncOutcome::Duplicate
        );
        runtime
            .reconcile(
                &mut projection,
                &store,
                PortalReconcileRequest {
                    host_id,
                    local_confirmation: true,
                },
                2_000,
            )
            .expect("flush");
        assert_eq!(runtime.transport().telemetry_calls(), 1);
        runtime
            .reconcile(
                &mut projection,
                &store,
                PortalReconcileRequest {
                    host_id,
                    local_confirmation: true,
                },
                3_000,
            )
            .expect("retry");
        assert_eq!(runtime.transport().telemetry_calls(), 1);
        assert!(!runtime.transport().recorded_telemetry()[0].raw_content);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn secrets_are_not_serialized_on_runtime_record() {
        let handle = PortalCredentialHandle::parse("vault:org-portal").expect("handle");
        let auth = crate::org::portal::StaticPortalAuth::new(handle.clone(), "super-secret-token")
            .expect("auth");
        let runtime = PortalSyncRuntime::new(FakePortal::default(), auth.handle().cloned());
        let json = serde_json::to_string(&runtime.export_record()).expect("json");
        assert!(json.contains("vault:org-portal"));
        assert!(!json.contains("super-secret-token"));
        assert!(!json.to_ascii_lowercase().contains("bearer"));
        assert!(!format!("{auth:?}").contains("super-secret-token"));
    }

    #[test]
    fn raw_evidence_default_remains_off() {
        let host_id = ConnectHostId::new();
        let fake = FakePortal::with_reconcile(reconcile_ok(&host_str(host_id)));
        let mut runtime = PortalSyncRuntime::new(fake, None);
        let mut projection = OrganizationProjection::standalone();
        assert_eq!(projection.sign_in(account()), 0);
        let (root, store) = temp_store();
        runtime
            .reconcile(
                &mut projection,
                &store,
                PortalReconcileRequest {
                    host_id,
                    local_confirmation: true,
                },
                1_000,
            )
            .expect("enroll");
        projection
            .trust_evidence_signer("trusted")
            .expect("trust");
        let bundle_id = BundleId::new();
        let mut bundle = EvidenceBundle {
            manifest_version: crate::org::EVIDENCE_BUNDLE_VERSION,
            bundle_id,
            capture_started_at_ms: 1,
            capture_ended_at_ms: 2,
            timezone: "UTC".into(),
            source_device: "dev".into(),
            source_user: "owner-1".into(),
            transcript_segments: Vec::new(),
            media_refs: Vec::new(),
            proposed_title: "t".into(),
            proposed_summary: "s".into(),
            acceptance_criteria: Vec::new(),
            steps: Vec::new(),
            privacy_labels: Vec::new(),
            redactions: vec!["redacted".into()],
            content_hash_hex: String::new(),
            signature_hex: String::new(),
            signer: "trusted".into(),
        };
        bundle.content_hash_hex = compute_bundle_hash(&bundle);
        bundle.signature_hex = bundle.content_hash_hex.clone();
        let meta = projection.ingest_evidence(&bundle).expect("ingest");
        assert!(!meta.raw_content_included);
        assert_eq!(
            projection.evidence_raw_segments(
                crate::org::EvidenceAccessClass::MetadataOnly,
                &bundle
            ),
            Err(OrgError::ProhibitedField)
        );
        let queued = PersistedOutboxIntent {
            observation_id_hex: bundle.content_hash_hex.clone(),
            intent: "evidence_metadata".into(),
            publication_queued: true,
            delivery: OutboxDeliveryState::Queued,
        };
        projection.queue_outbox_intent(queued).expect("queue");
        runtime
            .reconcile(
                &mut projection,
                &store,
                PortalReconcileRequest {
                    host_id,
                    local_confirmation: true,
                },
                2_000,
            )
            .expect("upload");
        let uploaded = runtime.transport().recorded_telemetry();
        assert_eq!(uploaded.len(), 1);
        assert_eq!(uploaded[0].intent, "evidence_metadata");
        assert_eq!(uploaded[0].bundle_ref.as_deref(), Some(bundle.content_hash_hex.as_str()));
        assert!(!uploaded[0].raw_content);
        assert!(reject_prohibited_fields(&serde_json::json!({"terminal":"raw"})).is_err());
        assert!(TelemetryUploadRequest {
            observation_id: observation(2),
            intent: "raw".into(),
            content_hash: observation(2),
            bundle_ref: None,
            metadata: serde_json::Map::new(),
            raw_content: true,
        }
        .validate()
        .is_err());
        let _ = bundle_id;
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn revoked_membership_rejects_scoped_operations() {
        let host_id = ConnectHostId::new();
        let mut response = reconcile_ok(&host_str(host_id));
        let fake = FakePortal::with_reconcile(response.clone());
        let mut runtime = PortalSyncRuntime::new(fake, None);
        let mut projection = OrganizationProjection::standalone();
        assert_eq!(projection.sign_in(account()), 0);
        let (root, store) = temp_store();
        runtime
            .reconcile(
                &mut projection,
                &store,
                PortalReconcileRequest {
                    host_id,
                    local_confirmation: true,
                },
                1_000,
            )
            .expect("enroll");
        response.membership.status = PortalMembershipStatus::Revoked;
        response.membership.revoked_at = Some("2026-08-12T00:00:01Z".into());
        response.membership_revision = 2;
        runtime.transport().set_reconcile(response);
        runtime
            .reconcile(
                &mut projection,
                &store,
                PortalReconcileRequest {
                    host_id,
                    local_confirmation: true,
                },
                2_000,
            )
            .expect("revoked");
        assert_eq!(
            runtime.capability(&projection),
            OrganizationCapabilityState::Disabled(OrganizationCapabilityDisableReason::Revoked)
        );
        assert_eq!(
            runtime.read_prompt(&projection, "prompt-1").expect_err("revoked"),
            PortalSyncError::Org(OrgError::MembershipRevoked)
        );
        let _ = fs::remove_dir_all(root);
    }
}
