//! The single host-owned organization actor.
//!
//! `OrganizationProjection` is deliberately kept behind this boundary. The
//! native client receives bounded DTOs and sends bounded commands; it never
//! restores or mutates `organization-state.json` itself. Network work is
//! caller-driven and only runs when the persisted opt-in, enrollment, and a
//! transport-backed runtime are all present.

use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::config::PortalConfig;
use crate::connect::ConnectHostId;
use crate::org::{
    LocalActionAdmissionState, LocalActionId, OrgError, OrgPromptVersionId,
    OrganizationCapabilityDisableReason, OrganizationCapabilityState, OrganizationProjection,
    OrganizationStateStore, OrganizationSyncState, PortalReconcileKind, PortalReconcileRequest,
    PortalSyncError, PortalSyncRuntime, PortalTransport,
};

pub const ORGANIZATION_RUNTIME_DEFAULT_REFRESH_INTERVAL_MS: u64 = 60_000;
pub const ORGANIZATION_RUNTIME_MAX_REFRESH_INTERVAL_MS: u64 = 15 * 60_000;

/// Runtime-only options. The `PortalConfig` contains only a vault reference;
/// a bearer token is resolved by the credential provider outside persistence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrganizationRuntimeConfig {
    pub portal: PortalConfig,
    pub refresh_interval_ms: u64,
}

impl Default for OrganizationRuntimeConfig {
    fn default() -> Self {
        Self {
            portal: PortalConfig::default(),
            refresh_interval_ms: ORGANIZATION_RUNTIME_DEFAULT_REFRESH_INTERVAL_MS,
        }
    }
}

impl OrganizationRuntimeConfig {
    pub fn bounded(mut self) -> Self {
        self.refresh_interval_ms = self
            .refresh_interval_ms
            .clamp(1_000, ORGANIZATION_RUNTIME_MAX_REFRESH_INTERVAL_MS);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum OrganizationIpcQuery {
    Snapshot,
    /// `include_body` is explicit because prompt bodies can contain sensitive
    /// operational instructions. Evidence is never represented by this DTO.
    Prompt {
        version_id: OrgPromptVersionId,
        include_body: bool,
        now_ms: i64,
    },
    LocalAction {
        request_id: LocalActionId,
    },
    EvidenceMetadata {
        bundle_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum OrganizationIpcCommand {
    Refresh {
        host_id: ConnectHostId,
        local_confirmation: bool,
        now_ms: i64,
    },
    UnenrollOffline {
        now_ms: i64,
    },
    PutPromptInComposer {
        version_id: OrgPromptVersionId,
        now_ms: i64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum OrganizationIpcReply {
    Snapshot(OrganizationSnapshot),
    Prompt(Option<OrganizationPromptView>),
    LocalAction(Option<LocalActionAdmissionState>),
    EvidenceMetadata(Option<OrganizationEvidenceMetadata>),
    Refreshed(OrganizationRefreshReply),
    Unenrolled(OrganizationSnapshot),
    Composer(OrganizationPromptView),
}

/// Safe-by-default host snapshot. In particular, this has no transcript,
/// media, raw evidence, bearer token, or credential payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OrganizationSnapshot {
    pub sync_state: OrganizationSyncState,
    pub capability: String,
    pub host_id: Option<ConnectHostId>,
    pub tenant_id: Option<String>,
    pub membership_revision: Option<u64>,
    pub policy_revision: Option<u32>,
    pub managed_task_count: u32,
    pub prompt_count: u32,
    pub pending_outbox_count: u32,
    pub last_refresh_ms: Option<i64>,
    pub last_error: Option<String>,
    pub raw_evidence_included: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OrganizationPromptView {
    pub version_id: OrgPromptVersionId,
    pub prompt_id: crate::org::OrgPromptId,
    pub title: String,
    pub tags: Vec<String>,
    pub content_hash_hex: String,
    pub body: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OrganizationEvidenceMetadata {
    pub bundle_id: String,
    pub imported: bool,
    pub raw_content_included: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OrganizationRefreshReply {
    pub attempted: bool,
    pub kind: Option<String>,
    pub applied_facts: u32,
    pub pages_fetched: u32,
    pub outbox_acknowledged: u32,
    pub snapshot: OrganizationSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrganizationRuntimeError {
    Closed,
    InvalidRequest,
    Org(OrgError),
    Sync(PortalSyncError),
}

impl fmt::Display for OrganizationRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => formatter.write_str("organization runtime is shut down"),
            Self::InvalidRequest => formatter.write_str("invalid organization request"),
            Self::Org(error) => error.fmt(formatter),
            Self::Sync(error) => write!(formatter, "organization sync failed: {error:?}"),
        }
    }
}

impl std::error::Error for OrganizationRuntimeError {}

impl From<OrgError> for OrganizationRuntimeError {
    fn from(error: OrgError) -> Self {
        Self::Org(error)
    }
}

impl From<PortalSyncError> for OrganizationRuntimeError {
    fn from(error: PortalSyncError) -> Self {
        Self::Sync(error)
    }
}

trait SyncDriver: Send {
    fn reconcile(
        &mut self,
        projection: &mut OrganizationProjection,
        store: &OrganizationStateStore,
        request: PortalReconcileRequest,
        now_ms: i64,
    ) -> Result<crate::org::PortalReconcileOutcome, PortalSyncError>;
}

impl<C> SyncDriver for PortalSyncRuntime<C>
where
    C: PortalTransport + Send,
{
    fn reconcile(
        &mut self,
        projection: &mut OrganizationProjection,
        store: &OrganizationStateStore,
        request: PortalReconcileRequest,
        now_ms: i64,
    ) -> Result<crate::org::PortalReconcileOutcome, PortalSyncError> {
        PortalSyncRuntime::reconcile(self, projection, store, request, now_ms)
    }
}

struct OrganizationRuntimeState {
    store: OrganizationStateStore,
    projection: OrganizationProjection,
    config: OrganizationRuntimeConfig,
    sync: Option<Box<dyn SyncDriver>>,
    last_refresh_ms: Option<i64>,
    last_error: Option<String>,
    closed: bool,
}

/// Host lifetime owner. Cloning yields a bounded handle, never another
/// projection owner. The binary retains one instance until shutdown.
#[derive(Clone)]
pub struct OrganizationRuntime {
    state: Arc<Mutex<OrganizationRuntimeState>>,
}

impl fmt::Debug for OrganizationRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OrganizationRuntime")
            .field("snapshot", &self.snapshot())
            .finish()
    }
}

impl OrganizationRuntime {
    pub fn open(profile_root: impl AsRef<Path>, config: OrganizationRuntimeConfig) -> Self {
        let restored = OrganizationStateStore::restore_hello(profile_root.as_ref());
        let diagnostic = restored.diagnostic().map(str::to_owned);
        let projection = restored.into_projection();
        Self {
            state: Arc::new(Mutex::new(OrganizationRuntimeState {
                store: OrganizationStateStore::open(profile_root),
                projection,
                config: config.bounded(),
                sync: None,
                last_refresh_ms: None,
                last_error: diagnostic,
                closed: false,
            })),
        }
    }

    pub fn from_app_config(profile_root: impl AsRef<Path>, portal: PortalConfig) -> Self {
        Self::open(
            profile_root,
            OrganizationRuntimeConfig {
                portal,
                ..OrganizationRuntimeConfig::default()
            },
        )
    }

    /// Injects a transport-backed runtime for production credential wiring or
    /// fake transport tests. No transport is created by `open`.
    pub fn attach_sync_runtime<C>(&self, runtime: PortalSyncRuntime<C>)
    where
        C: PortalTransport + Send + 'static,
    {
        if let Ok(mut state) = self.state.lock() {
            if !state.closed {
                state.sync = Some(Box::new(runtime));
            }
        }
    }

    pub fn handle(&self) -> OrganizationRuntimeHandle {
        OrganizationRuntimeHandle {
            state: Arc::clone(&self.state),
        }
    }

    pub fn snapshot(&self) -> OrganizationSnapshot {
        match self.state.lock() {
            Ok(state) => snapshot_for(&state),
            Err(_) => OrganizationSnapshot::closed(),
        }
    }

    pub fn capability(&self) -> OrganizationCapabilityState {
        match self.state.lock() {
            Ok(state) => capability_for(&state),
            Err(_) => {
                OrganizationCapabilityState::Disabled(OrganizationCapabilityDisableReason::Offline)
            }
        }
    }

    /// Drops the only transport owner and marks the actor closed. Persisted
    /// projection state remains recoverable on the next host start.
    pub fn shutdown(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.sync = None;
            state.closed = true;
        }
    }
}

#[derive(Clone)]
pub struct OrganizationRuntimeHandle {
    state: Arc<Mutex<OrganizationRuntimeState>>,
}

impl fmt::Debug for OrganizationRuntimeHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OrganizationRuntimeHandle")
            .field("snapshot", &self.snapshot())
            .finish()
    }
}

impl OrganizationRuntimeHandle {
    pub fn snapshot(&self) -> OrganizationSnapshot {
        match self.state.lock() {
            Ok(state) => snapshot_for(&state),
            Err(_) => OrganizationSnapshot::closed(),
        }
    }

    pub fn query(
        &self,
        query: OrganizationIpcQuery,
    ) -> Result<OrganizationIpcReply, OrganizationRuntimeError> {
        let state = self
            .state
            .lock()
            .map_err(|_| OrganizationRuntimeError::Closed)?;
        ensure_open(&state)?;
        match query {
            OrganizationIpcQuery::Snapshot => {
                Ok(OrganizationIpcReply::Snapshot(snapshot_for(&state)))
            }
            OrganizationIpcQuery::Prompt {
                version_id,
                include_body,
                now_ms,
            } => Ok(OrganizationIpcReply::Prompt(prompt_view(
                &state.projection,
                version_id,
                include_body,
                now_ms,
            )?)),
            OrganizationIpcQuery::LocalAction { request_id } => {
                Ok(OrganizationIpcReply::LocalAction(
                    state.projection.local_action_state(request_id)?.cloned(),
                ))
            }
            OrganizationIpcQuery::EvidenceMetadata { bundle_id } => {
                if bundle_id.is_empty() || bundle_id.len() > 256 {
                    return Err(OrganizationRuntimeError::InvalidRequest);
                }
                let imported = state
                    .projection
                    .evidence()
                    .persist_imported_ids()
                    .iter()
                    .any(|id| id == &bundle_id);
                Ok(OrganizationIpcReply::EvidenceMetadata(Some(
                    OrganizationEvidenceMetadata {
                        bundle_id,
                        imported,
                        raw_content_included: false,
                    },
                )))
            }
        }
    }

    pub fn command(
        &self,
        command: OrganizationIpcCommand,
    ) -> Result<OrganizationIpcReply, OrganizationRuntimeError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| OrganizationRuntimeError::Closed)?;
        ensure_open(&state)?;
        match command {
            OrganizationIpcCommand::Refresh {
                host_id,
                local_confirmation,
                now_ms,
            } => {
                if state.sync.is_none() {
                    return Ok(OrganizationIpcReply::Refreshed(OrganizationRefreshReply {
                        attempted: false,
                        kind: None,
                        applied_facts: 0,
                        pages_fetched: 0,
                        outbox_acknowledged: 0,
                        snapshot: snapshot_for(&state),
                    }));
                }
                if !state.config.portal.is_opted_in()
                    || state.projection.membership().is_none()
                    || !state
                        .projection
                        .membership()
                        .is_some_and(|m| m.is_enrolled())
                {
                    return Ok(OrganizationIpcReply::Refreshed(OrganizationRefreshReply {
                        attempted: false,
                        kind: None,
                        applied_facts: 0,
                        pages_fetched: 0,
                        outbox_acknowledged: 0,
                        snapshot: snapshot_for(&state),
                    }));
                }
                let outcome = {
                    let sync = state.sync.as_mut().expect("sync checked above");
                    sync.reconcile(
                        &mut state.projection,
                        &state.store,
                        PortalReconcileRequest {
                            host_id,
                            local_confirmation,
                        },
                        now_ms,
                    )
                };
                match outcome {
                    Ok(outcome) => {
                        state.last_refresh_ms = Some(now_ms);
                        state.last_error = None;
                        Ok(OrganizationIpcReply::Refreshed(OrganizationRefreshReply {
                            attempted: true,
                            kind: Some(format!("{:?}", outcome.kind)),
                            applied_facts: outcome.applied_facts,
                            pages_fetched: outcome.pages_fetched,
                            outbox_acknowledged: outcome.outbox_acknowledged,
                            snapshot: snapshot_for(&state),
                        }))
                    }
                    Err(error) => {
                        state.last_error = Some(format!("{error:?}"));
                        Err(error.into())
                    }
                }
            }
            OrganizationIpcCommand::UnenrollOffline { now_ms } => {
                state.projection.unenroll_offline(now_ms)?;
                state.projection.persist_to(&state.store)?;
                state.last_refresh_ms = Some(now_ms);
                Ok(OrganizationIpcReply::Unenrolled(snapshot_for(&state)))
            }
            OrganizationIpcCommand::PutPromptInComposer { version_id, now_ms } => {
                let view = prompt_view(&state.projection, version_id, true, now_ms)?
                    .ok_or(OrganizationRuntimeError::InvalidRequest)?;
                Ok(OrganizationIpcReply::Composer(view))
            }
        }
    }

    /// Bounded periodic refresh gate. Callers can invoke this from a timer;
    /// it never starts a task or performs work before the interval elapses.
    pub fn maybe_refresh(
        &self,
        host_id: ConnectHostId,
        local_confirmation: bool,
        now_ms: i64,
    ) -> Result<Option<OrganizationRefreshReply>, OrganizationRuntimeError> {
        let should_refresh = {
            let state = self
                .state
                .lock()
                .map_err(|_| OrganizationRuntimeError::Closed)?;
            ensure_open(&state)?;
            state.last_refresh_ms.map_or(true, |last| {
                now_ms.saturating_sub(last) >= state.config.refresh_interval_ms as i64
            })
        };
        if !should_refresh {
            return Ok(None);
        }
        match self.command(OrganizationIpcCommand::Refresh {
            host_id,
            local_confirmation,
            now_ms,
        })? {
            OrganizationIpcReply::Refreshed(reply) => Ok(Some(reply)),
            _ => Err(OrganizationRuntimeError::InvalidRequest),
        }
    }
}

fn ensure_open(state: &OrganizationRuntimeState) -> Result<(), OrganizationRuntimeError> {
    if state.closed {
        Err(OrganizationRuntimeError::Closed)
    } else {
        Ok(())
    }
}

fn capability_for(state: &OrganizationRuntimeState) -> OrganizationCapabilityState {
    let projection_capability = state.projection.capability_state();
    if matches!(
        projection_capability,
        OrganizationCapabilityState::Disabled(OrganizationCapabilityDisableReason::Standalone)
    ) {
        return projection_capability;
    }
    if matches!(projection_capability, OrganizationCapabilityState::Enabled)
        && (!state.config.portal.is_opted_in() || state.sync.is_none())
    {
        return OrganizationCapabilityState::Disabled(OrganizationCapabilityDisableReason::Offline);
    }
    projection_capability
}

fn snapshot_for(state: &OrganizationRuntimeState) -> OrganizationSnapshot {
    let membership = state.projection.membership();
    OrganizationSnapshot {
        sync_state: state.projection.sync_state(),
        capability: capability_label(capability_for(state)),
        host_id: membership.map(|value| value.host_id),
        tenant_id: membership.map(|value| value.tenant_id.to_string()),
        membership_revision: state.projection.membership_revision(),
        policy_revision: membership.map(|value| value.policy_revision),
        managed_task_count: state.projection.exported_task_count() as u32,
        prompt_count: state
            .projection
            .prompt_snapshot()
            .map(|snapshot| snapshot.prompts.len() as u32)
            .unwrap_or(0),
        pending_outbox_count: state.projection.pending_outbox_intents().count() as u32,
        last_refresh_ms: state.last_refresh_ms,
        last_error: state.last_error.clone(),
        raw_evidence_included: false,
    }
}

fn capability_label(capability: OrganizationCapabilityState) -> String {
    match capability {
        OrganizationCapabilityState::Enabled => "enabled",
        OrganizationCapabilityState::Disabled(reason) => match reason {
            OrganizationCapabilityDisableReason::Standalone => "standalone",
            OrganizationCapabilityDisableReason::Unenrolled => "unenrolled",
            OrganizationCapabilityDisableReason::Revoked => "revoked",
            OrganizationCapabilityDisableReason::Expired => "expired",
            OrganizationCapabilityDisableReason::Offline => "offline",
        },
    }
    .to_string()
}

fn prompt_view(
    projection: &OrganizationProjection,
    version_id: OrgPromptVersionId,
    include_body: bool,
    now_ms: i64,
) -> Result<Option<OrganizationPromptView>, OrganizationRuntimeError> {
    let prompts = projection.prompts()?;
    let Some(version) = prompts.version(version_id) else {
        return Ok(None);
    };
    let body = if include_body {
        Some(prompts.read_cached_body(version_id, now_ms)?.to_owned())
    } else {
        None
    };
    Ok(Some(OrganizationPromptView {
        version_id: version.version_id,
        prompt_id: version.prompt_id,
        title: version.title.clone(),
        tags: version.tags.clone(),
        content_hash_hex: version.content_hash_hex.clone(),
        body,
    }))
}

impl OrganizationSnapshot {
    fn closed() -> Self {
        Self {
            sync_state: OrganizationSyncState::Standalone,
            capability: "offline".to_string(),
            host_id: None,
            tenant_id: None,
            membership_revision: None,
            policy_revision: None,
            managed_task_count: 0,
            prompt_count: 0,
            pending_outbox_count: 0,
            last_refresh_ms: None,
            last_error: Some("organization runtime is closed".to_string()),
            raw_evidence_included: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn standalone_runtime_is_safe_and_does_not_attempt_network() {
        let root = std::env::temp_dir().join(format!(
            "devmanager-org-runtime-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let runtime = OrganizationRuntime::open(&root, OrganizationRuntimeConfig::default());
        let snapshot = runtime.snapshot();
        assert_eq!(snapshot.capability, "standalone");
        assert!(!snapshot.raw_evidence_included);
        assert!(runtime
            .handle()
            .query(OrganizationIpcQuery::EvidenceMetadata {
                bundle_id: "bundle-1".to_string(),
            })
            .is_ok());
        runtime.shutdown();
        assert_eq!(runtime.snapshot().capability, "offline");
    }

    #[test]
    fn opt_in_requires_opaque_credential_reference() {
        let mut portal = PortalConfig {
            enabled: true,
            base_url: Some("https://portal.example.test".to_string()),
            credential_ref: None,
        };
        assert!(!portal.is_opted_in());
        portal.credential_ref = Some(crate::config::PortalCredentialReference {
            vault_ref: "vault:portal".to_string(),
        });
        assert!(portal.validate().is_ok());
        assert!(portal.is_opted_in());
    }
}
