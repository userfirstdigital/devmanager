//! Honest, bounded management observations as a sealed pure reducer.
//!
//! This module does not deliver to the kernel outbox or Portal. Those
//! destinations have no observation effect class yet; delivery APIs return
//! [`ObservationDependency::DurableOutbox`].
//!
//! Canonical types reused from the base: [`TaskId`], [`EventId`], [`ClientId`],
//! [`TaskLifecycle`], [`TaskAttention`], [`TaskConnectivity`], [`TaskActivity`],
//! [`AgentRole`], [`AgentSessionLifecycle`], and canonical provider-kind strings.
//! There is no domain `ObservationId`, `DeviceKind`, `HostHealth`, or message
//! origin type; local identifiers and message classes stay specific to this
//! reducer and are not a second provider/task universe.
//!
//! Missing dependencies:
//! - `src/kernel/outbox.rs` has no observation `DestinationClass` / `Effect`
//! - Portal `DevManagerTaskObservation` / telemetry service are out of scope

use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Write};
use std::ops::Bound;
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::domain::agent::{AgentRole, AgentSessionFacts, AgentSessionLifecycle};
use crate::domain::canonical;
use crate::domain::id::{ClientId, EventId, TaskId};
use crate::domain::task::{TaskActivity, TaskAttention, TaskConnectivity, TaskLifecycle};

use super::envelope::{MAX_CONNECT_PAGE_ENCODED_BYTES, MAX_CONNECT_PAGE_ITEMS};
use super::policy::{
    ActiveSessionInterval, GrantBindError, ManagedField, ManagementGrant, ObservationGrantLease,
    ObservationLeaseEpoch, ACTIVE_SESSION_IDLE_LIMIT_MS,
};

#[cfg(test)]
use super::policy::HostTimeAuthority;

pub const OBSERVATION_SCHEMA_REVISION: u32 = 1;
pub const OBSERVATION_STALE_AFTER_MS: u64 = 60 * 60 * 1_000;
pub const MAX_OBSERVATION_TASKS: usize = 128;
pub const MAX_ACTIVITIES_PER_TASK: usize = 256;
pub const MAX_READY_INTERVALS: usize = 64;
pub const MAX_SPECIALISTS: usize = 8;
pub const MAX_USAGE_SOURCE_BYTES: usize = 64;
pub const MAX_USAGE_UNIT_BYTES: usize = 32;
pub const MAX_GIT_BRANCH_BYTES: usize = 255;
pub const MIN_GIT_COMMIT_HEX: usize = 7;
pub const MAX_GIT_COMMIT_HEX: usize = 40;
pub const MAX_GIT_FILE_CHANGES: u32 = 10_000;
pub const MAX_OBSERVATION_DOCUMENT_BYTES: usize = 16_384;
pub const MAX_OBSERVATION_RETENTION_MS: u64 = 24 * 60 * 60 * 1_000;
pub const ACTIVE_SESSION_TIME_LABEL: &str = "active session time";

const MAX_SEEN_EVENTS: usize = MAX_OBSERVATION_TASKS * MAX_ACTIVITIES_PER_TASK;
const MAX_JSON_NESTING: usize = 8;
const MAX_JSON_STRING_BYTES: usize = MAX_GIT_BRANCH_BYTES;
const MAX_JSON_ARRAY_LEN: usize = MAX_SPECIALISTS;
const MAX_JSON_MAP_KEYS: usize = 32;

const AGGREGATE_ALLOWLIST: &[&str] = &[
    "files_changed",
    "insertions",
    "deletions",
    "human_message_count",
    "human_turn_count",
    "tokens",
    "quota_remaining",
    "quota_reset",
    "quoted_cost",
    "local_token_estimate",
    "local_cost_estimate",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservationError {
    Conflict,
    BoundExceeded,
    InvalidUsage,
    InvalidWindow,
    InvalidGit,
    CounterOverflow,
    StaleRevision,
    ProhibitedContent,
    FutureTimestamp,
    Backpressure,
    Unavailable(ObservationDependency),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservationDependency {
    DurableOutbox,
    PortalObservationEffect,
    AuthoritativeSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReduceOutcome {
    Accepted,
    Duplicate,
    Ignored,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservationFreshness {
    Current,
    Stale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationCompleteness {
    Complete,
    Partial,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationConfidence {
    High,
    Low,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservationMessageClass {
    Human,
    Synthetic,
    StatusNotice,
    ProviderInternal,
    Replay,
    CopiedPrompt,
    InheritedContext,
    SpecialistToPrimaryTransfer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageKind {
    Tokens,
    QuotaRemaining,
    QuotaReset,
    MonetaryQuote,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageProvenance {
    ProviderReported,
    ProviderQuoted,
    LocalEstimate,
}

impl UsageProvenance {
    pub const fn as_label(self) -> &'static str {
        match self {
            Self::ProviderReported => "provider_reported",
            Self::ProviderQuoted => "provider_quoted",
            Self::LocalEstimate => "local_estimate",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QualifyingActivity {
    AcceptedHumanCommand {
        task_id: TaskId,
        client_id: ClientId,
        event_id: EventId,
    },
    ForegroundTaskInteraction {
        task_id: TaskId,
        client_id: ClientId,
        event_id: EventId,
    },
}

impl QualifyingActivity {
    fn task_id(self) -> TaskId {
        match self {
            Self::AcceptedHumanCommand { task_id, .. }
            | Self::ForegroundTaskInteraction { task_id, .. } => task_id,
        }
    }

    fn client_id(self) -> ClientId {
        match self {
            Self::AcceptedHumanCommand { client_id, .. }
            | Self::ForegroundTaskInteraction { client_id, .. } => client_id,
        }
    }

    fn event_id(self) -> EventId {
        match self {
            Self::AcceptedHumanCommand { event_id, .. }
            | Self::ForegroundTaskInteraction { event_id, .. } => event_id,
        }
    }

    fn kind_tag(self) -> u8 {
        match self {
            Self::AcceptedHumanCommand { .. } => 1,
            Self::ForegroundTaskInteraction { .. } => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageBudget {
    pub max_items: u16,
    pub max_work: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ObservationCursor {
    task_id: TaskId,
    started_at_ms: u64,
    ended_at_ms: u64,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ObservationPage {
    items: Vec<ObservationRecord>,
    more: bool,
    work_used: u32,
    next_cursor: Option<ObservationCursor>,
}

impl fmt::Debug for ObservationPage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ObservationPage")
            .field("items", &self.items.len())
            .field("more", &self.more)
            .field("work_used", &self.work_used)
            .field("next_cursor", &self.next_cursor)
            .finish()
    }
}

impl ObservationPage {
    pub fn items(&self) -> &[ObservationRecord] {
        &self.items
    }

    pub fn more(&self) -> bool {
        self.more
    }

    pub fn work_used(&self) -> u32 {
        self.work_used
    }

    pub fn next_cursor(&self) -> Option<ObservationCursor> {
        self.next_cursor
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationIdPage {
    ids: Vec<ObservationId>,
    more: bool,
    work_used: u32,
    next_cursor: Option<ObservationCursor>,
}

impl ObservationIdPage {
    pub fn ids(&self) -> &[ObservationId] {
        &self.ids
    }

    pub fn more(&self) -> bool {
        self.more
    }

    pub fn work_used(&self) -> u32 {
        self.work_used
    }

    pub fn next_cursor(&self) -> Option<ObservationCursor> {
        self.next_cursor
    }
}

#[derive(Clone)]
pub struct ObservationAuthority {
    lease: ObservationGrantLease,
}

impl fmt::Debug for ObservationAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ObservationAuthority")
            .field("task_id", &self.lease.task_id())
            .field("policy_revision", &self.lease.policy_revision())
            .finish_non_exhaustive()
    }
}

impl PartialEq for ObservationAuthority {
    fn eq(&self, other: &Self) -> bool {
        self.lease.policy_revision() == other.lease.policy_revision()
            && self.lease.task_id() == other.lease.task_id()
    }
}

impl Eq for ObservationAuthority {}

impl ObservationAuthority {
    pub fn from_grant(grant: &ManagementGrant) -> Result<Self, ObservationError> {
        let lease = grant.observation_lease().map_err(bind_error)?;
        Ok(Self { lease })
    }

    #[cfg(test)]
    pub(crate) fn from_grant_at(
        grant: &ManagementGrant,
        now: HostTimeAuthority,
    ) -> Result<Self, ObservationError> {
        let lease = grant.observation_lease_at(now).map_err(bind_error)?;
        Ok(Self { lease })
    }

    pub fn policy_revision(&self) -> u32 {
        self.lease.policy_revision()
    }

    fn ensure_live(&self) -> Result<(), ObservationError> {
        self.lease.ensure_live().map_err(bind_error)
    }

    fn bound_task_id(&self) -> TaskId {
        self.lease.task_id()
    }

    fn matches_grant(&self, grant: &ManagementGrant) -> bool {
        self.lease.matches_grant(grant)
    }

    fn capture_settlement_epoch(&self) -> Result<ObservationLeaseEpoch, ObservationError> {
        self.lease.capture_epoch().map_err(bind_error)
    }

    fn confirm_settlement_epoch(
        &self,
        epoch: ObservationLeaseEpoch,
    ) -> Result<(), ObservationError> {
        self.lease.confirm_epoch(epoch).map_err(bind_error)
    }

    #[cfg(test)]
    fn revoke_lease_for_test(&self) {
        self.lease.revoke_for_test();
    }
}

fn bind_error(_: GrantBindError) -> ObservationError {
    ObservationError::Unavailable(ObservationDependency::AuthoritativeSource)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObservationId([u8; 32]);

impl ObservationId {
    pub fn to_hex(self) -> String {
        hex_encode(&self.0)
    }

    fn from_hex(value: &str) -> Result<Self, ObservationError> {
        if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(ObservationError::ProhibitedContent);
        }
        let mut bytes = [0_u8; 32];
        for (index, chunk) in value.as_bytes().chunks(2).enumerate() {
            let high = hex_nibble(chunk[0])?;
            let low = hex_nibble(chunk[1])?;
            bytes[index] = (high << 4) | low;
        }
        Ok(Self(bytes))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderObservation {
    kind: String,
    role: AgentRole,
    lifecycle: AgentSessionLifecycle,
    activity: TaskActivity,
}

impl ProviderObservation {
    pub fn try_new(
        kind: impl Into<String>,
        role: AgentRole,
        lifecycle: AgentSessionLifecycle,
        activity: TaskActivity,
    ) -> Result<Self, ObservationError> {
        let kind = AgentSessionFacts::canonicalize_provider_kind(kind)
            .map_err(|_| ObservationError::InvalidUsage)?;
        role.validate()
            .map_err(|_| ObservationError::InvalidUsage)?;
        if kind.len() > MAX_USAGE_SOURCE_BYTES {
            return Err(ObservationError::BoundExceeded);
        }
        Ok(Self {
            kind,
            role,
            lifecycle,
            activity,
        })
    }

    pub fn kind(&self) -> &str {
        &self.kind
    }

    pub fn role(&self) -> &AgentRole {
        &self.role
    }

    pub fn lifecycle(&self) -> AgentSessionLifecycle {
        self.lifecycle
    }

    pub fn activity(&self) -> TaskActivity {
        self.activity
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskObservationFacts {
    task_id: TaskId,
    lifecycle: TaskLifecycle,
    attention: TaskAttention,
    connectivity: TaskConnectivity,
    primary: Option<ProviderObservation>,
    specialists: Vec<ProviderObservation>,
    source_at_ms: u64,
    revision: u64,
}

impl TaskObservationFacts {
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        task_id: TaskId,
        lifecycle: TaskLifecycle,
        attention: TaskAttention,
        connectivity: TaskConnectivity,
        primary: Option<ProviderObservation>,
        specialists: Vec<ProviderObservation>,
        source_at_ms: u64,
        revision: u64,
    ) -> Result<Self, ObservationError> {
        if specialists.len() > MAX_SPECIALISTS {
            return Err(ObservationError::BoundExceeded);
        }
        Ok(Self {
            task_id,
            lifecycle,
            attention,
            connectivity,
            primary,
            specialists,
            source_at_ms,
            revision,
        })
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestrictiveGitSummary {
    branch: Option<String>,
    commit: Option<String>,
    files_changed: u32,
    insertions: u32,
    deletions: u32,
}

impl fmt::Debug for RestrictiveGitSummary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RestrictiveGitSummary")
            .field("present", &true)
            .finish_non_exhaustive()
    }
}

impl RestrictiveGitSummary {
    pub fn try_new(
        branch: Option<&str>,
        commit: Option<&str>,
        files_changed: u32,
        insertions: u32,
        deletions: u32,
    ) -> Result<Self, ObservationError> {
        let branch = match branch {
            Some(value) => {
                let canonical =
                    canonical::canonicalize(value).ok_or(ObservationError::InvalidGit)?;
                if canonical.len() > MAX_GIT_BRANCH_BYTES {
                    return Err(ObservationError::InvalidGit);
                }
                Some(canonical)
            }
            None => None,
        };
        let commit = match commit {
            Some(value) => Some(validate_commit(value)?),
            None => None,
        };
        if files_changed > MAX_GIT_FILE_CHANGES
            || insertions > MAX_GIT_FILE_CHANGES
            || deletions > MAX_GIT_FILE_CHANGES
        {
            return Err(ObservationError::InvalidGit);
        }
        Ok(Self {
            branch,
            commit,
            files_changed,
            insertions,
            deletions,
        })
    }

    pub fn files_changed(&self) -> u32 {
        self.files_changed
    }

    fn payload_hash(&self) -> [u8; 32] {
        hash_parts(&[
            b"git.summary",
            self.branch.as_deref().unwrap_or("").as_bytes(),
            self.commit.as_deref().unwrap_or("").as_bytes(),
            &self.files_changed.to_be_bytes(),
            &self.insertions.to_be_bytes(),
            &self.deletions.to_be_bytes(),
        ])
    }
}

#[derive(Clone, PartialEq, Eq)]
struct GitObservation {
    summary: RestrictiveGitSummary,
    source_event_id: EventId,
    observed_at_ms: u64,
    revision: u64,
    payload_hash: [u8; 32],
}

impl fmt::Debug for GitObservation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GitObservation")
            .field("present", &true)
            .finish_non_exhaustive()
    }
}

impl GitObservation {
    fn try_new(
        summary: RestrictiveGitSummary,
        source_event_id: EventId,
        observed_at_ms: u64,
        revision: u64,
    ) -> Self {
        let payload_hash = summary.payload_hash();
        Self {
            summary,
            source_event_id,
            observed_at_ms,
            revision,
            payload_hash,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct UsageMeasure {
    provider: String,
    source: String,
    kind: UsageKind,
    provenance: UsageProvenance,
    value: Option<u64>,
    unit: String,
    window: Option<(u64, u64)>,
    observed_at_ms: u64,
    revision: u64,
}

impl fmt::Debug for UsageMeasure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

impl UsageMeasure {
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        provider: impl Into<String>,
        source: impl Into<String>,
        kind: UsageKind,
        provenance: UsageProvenance,
        value: Option<u64>,
        unit: impl Into<String>,
        window: Option<(u64, u64)>,
        observed_at_ms: u64,
        revision: u64,
    ) -> Result<Self, ObservationError> {
        let provider = AgentSessionFacts::canonicalize_provider_kind(provider)
            .map_err(|_| ObservationError::InvalidUsage)?;
        let source =
            canonical::canonicalize(source.into()).ok_or(ObservationError::InvalidUsage)?;
        let unit = canonical::canonicalize(unit.into()).ok_or(ObservationError::InvalidUsage)?;
        if source.len() > MAX_USAGE_SOURCE_BYTES || unit.len() > MAX_USAGE_UNIT_BYTES {
            return Err(ObservationError::BoundExceeded);
        }
        let allowed = matches!(
            (kind, provenance),
            (UsageKind::Tokens, UsageProvenance::ProviderReported)
                | (UsageKind::Tokens, UsageProvenance::LocalEstimate)
                | (UsageKind::QuotaRemaining, UsageProvenance::ProviderReported)
                | (UsageKind::QuotaReset, UsageProvenance::ProviderReported)
                | (UsageKind::MonetaryQuote, UsageProvenance::ProviderQuoted)
                | (UsageKind::MonetaryQuote, UsageProvenance::LocalEstimate)
        );
        if !allowed {
            return Err(ObservationError::InvalidUsage);
        }
        if let Some((start, end)) = window {
            if end <= start || start > observed_at_ms || end > observed_at_ms {
                return Err(ObservationError::InvalidWindow);
            }
        }
        Ok(Self {
            provider,
            source,
            kind,
            provenance,
            value,
            unit,
            window,
            observed_at_ms,
            revision,
        })
    }

    pub fn provenance_label(&self) -> &'static str {
        self.provenance.as_label()
    }

    pub fn value(&self) -> Option<u64> {
        self.value
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct UsageSnapshot {
    tokens: Option<UsageMeasure>,
    quota_remaining: Option<UsageMeasure>,
    quota_reset: Option<UsageMeasure>,
    quoted_cost: Option<UsageMeasure>,
    local_token_estimate: Option<UsageMeasure>,
    local_cost_estimate: Option<UsageMeasure>,
}

impl fmt::Debug for UsageSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

impl UsageSnapshot {
    fn insert(&mut self, measure: UsageMeasure) -> Result<ReduceOutcome, ObservationError> {
        let slot = self.slot_mut(measure.kind, measure.provenance);
        if let Some(existing) = slot.as_ref() {
            if measure.revision < existing.revision
                || (measure.revision == existing.revision
                    && measure.observed_at_ms < existing.observed_at_ms)
            {
                return Err(ObservationError::StaleRevision);
            }
            if measure.revision == existing.revision
                && measure.observed_at_ms == existing.observed_at_ms
            {
                return if measure == *existing {
                    Ok(ReduceOutcome::Duplicate)
                } else {
                    Err(ObservationError::Conflict)
                };
            }
        }
        *slot = Some(measure);
        Ok(ReduceOutcome::Accepted)
    }

    fn slot_mut(
        &mut self,
        kind: UsageKind,
        provenance: UsageProvenance,
    ) -> &mut Option<UsageMeasure> {
        match (kind, provenance) {
            (UsageKind::Tokens, UsageProvenance::ProviderReported) => &mut self.tokens,
            (UsageKind::Tokens, UsageProvenance::LocalEstimate) => &mut self.local_token_estimate,
            (UsageKind::QuotaRemaining, UsageProvenance::ProviderReported) => {
                &mut self.quota_remaining
            }
            (UsageKind::QuotaReset, UsageProvenance::ProviderReported) => &mut self.quota_reset,
            (UsageKind::MonetaryQuote, UsageProvenance::ProviderQuoted) => &mut self.quoted_cost,
            (UsageKind::MonetaryQuote, UsageProvenance::LocalEstimate) => {
                &mut self.local_cost_estimate
            }
            _ => unreachable!("constructor rejects mismatched usage"),
        }
    }

    fn iter(&self) -> impl Iterator<Item = &UsageMeasure> {
        [
            self.tokens.as_ref(),
            self.quota_remaining.as_ref(),
            self.quota_reset.as_ref(),
            self.quoted_cost.as_ref(),
            self.local_token_estimate.as_ref(),
            self.local_cost_estimate.as_ref(),
        ]
        .into_iter()
        .flatten()
    }

    pub fn tokens(&self) -> Option<&UsageMeasure> {
        self.tokens.as_ref()
    }

    pub fn quota_remaining(&self) -> Option<&UsageMeasure> {
        self.quota_remaining.as_ref()
    }

    pub fn quota_reset(&self) -> Option<&UsageMeasure> {
        self.quota_reset.as_ref()
    }

    pub fn quoted_cost(&self) -> Option<&UsageMeasure> {
        self.quoted_cost.as_ref()
    }

    pub fn local_token_estimate(&self) -> Option<&UsageMeasure> {
        self.local_token_estimate.as_ref()
    }

    pub fn local_cost_estimate(&self) -> Option<&UsageMeasure> {
        self.local_cost_estimate.as_ref()
    }

    fn has_provider_reported(&self) -> bool {
        self.tokens.is_some() || self.quota_remaining.is_some() || self.quota_reset.is_some()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ObservationRecord {
    observation_id: ObservationId,
    task_id: TaskId,
    lifecycle: TaskLifecycle,
    attention: TaskAttention,
    connectivity: TaskConnectivity,
    primary: Option<ProviderObservation>,
    specialists: Vec<ProviderObservation>,
    git: Option<GitObservation>,
    usage: UsageSnapshot,
    human_message_count: u32,
    human_turn_count: u32,
    active_session: Option<ActiveSessionInterval>,
    source_event_id: EventId,
    observed_at_ms: u64,
    source_at_ms: u64,
    facts_revision: u64,
    completeness: ObservationCompleteness,
    confidence: ObservationConfidence,
    policy_revision: u32,
    schema_revision: u32,
}

impl fmt::Debug for ObservationRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ObservationRecord")
            .field("observation_id", &self.observation_id)
            .field("task_id", &self.task_id)
            .field("lifecycle", &self.lifecycle)
            .field("attention", &self.attention)
            .field("connectivity", &self.connectivity)
            .field("primary", &self.primary.as_ref().map(|item| item.kind()))
            .field(
                "specialists",
                &self
                    .specialists
                    .iter()
                    .map(ProviderObservation::kind)
                    .collect::<Vec<_>>(),
            )
            .field("git", &self.git.as_ref().map(|_| "<redacted>"))
            .field("usage", &"<redacted>")
            .field("human_message_count", &self.human_message_count)
            .field("human_turn_count", &self.human_turn_count)
            .field("active_session", &self.active_session)
            .field("source_event_id", &self.source_event_id)
            .field("observed_at_ms", &self.observed_at_ms)
            .field("source_at_ms", &self.source_at_ms)
            .field("facts_revision", &self.facts_revision)
            .field("completeness", &self.completeness)
            .field("confidence", &self.confidence)
            .field("policy_revision", &self.policy_revision)
            .field("schema_revision", &self.schema_revision)
            .finish()
    }
}

impl ObservationRecord {
    pub fn observation_id(&self) -> ObservationId {
        self.observation_id
    }

    pub fn task_id(&self) -> TaskId {
        self.task_id
    }

    pub fn lifecycle(&self) -> TaskLifecycle {
        self.lifecycle
    }

    pub fn attention(&self) -> TaskAttention {
        self.attention
    }

    pub fn connectivity(&self) -> TaskConnectivity {
        self.connectivity
    }

    pub fn primary(&self) -> Option<&ProviderObservation> {
        self.primary.as_ref()
    }

    pub fn specialists(&self) -> &[ProviderObservation] {
        &self.specialists
    }

    pub fn git(&self) -> Option<&RestrictiveGitSummary> {
        self.git.as_ref().map(|item| &item.summary)
    }

    pub fn usage(&self) -> &UsageSnapshot {
        &self.usage
    }

    pub fn human_message_count(&self) -> u32 {
        self.human_message_count
    }

    pub fn human_turn_count(&self) -> u32 {
        self.human_turn_count
    }

    pub fn active_session(&self) -> Option<ActiveSessionInterval> {
        self.active_session
    }

    pub fn active_time_label(&self) -> &'static str {
        ACTIVE_SESSION_TIME_LABEL
    }

    pub fn source_event_id(&self) -> EventId {
        self.source_event_id
    }

    pub fn observed_at_ms(&self) -> u64 {
        self.observed_at_ms
    }

    pub fn freshness(&self, now_ms: u64) -> ObservationFreshness {
        if now_ms.saturating_sub(self.observed_at_ms) >= OBSERVATION_STALE_AFTER_MS {
            ObservationFreshness::Stale
        } else {
            ObservationFreshness::Current
        }
    }

    pub fn completeness(&self) -> ObservationCompleteness {
        self.completeness
    }

    pub fn confidence(&self) -> ObservationConfidence {
        self.confidence
    }

    pub fn policy_revision(&self) -> u32 {
        self.policy_revision
    }

    pub fn schema_revision(&self) -> u32 {
        self.schema_revision
    }

    pub fn export_managed_field(&self, field: ManagedField) -> Result<(), ObservationError> {
        if field.is_denied_content() {
            return Err(ObservationError::ProhibitedContent);
        }
        if self.supports_managed_field(field) {
            return Ok(());
        }
        Err(ObservationError::Unavailable(
            ObservationDependency::AuthoritativeSource,
        ))
    }

    fn supports_managed_field(&self, field: ManagedField) -> bool {
        match field {
            ManagedField::TaskState
            | ManagedField::TaskAttention
            | ManagedField::SourceTimestamp
            | ManagedField::ObservedTimestamp
            | ManagedField::HumanMessageCount
            | ManagedField::HumanTurnCount => true,
            ManagedField::ProviderKind | ManagedField::ProviderState => self.primary.is_some(),
            ManagedField::ProviderReportedUsage => self.usage.has_provider_reported(),
            ManagedField::ActiveSessionInterval => self.active_session.is_some(),
            ManagedField::GitSummary => self.git.is_some(),
            ManagedField::TaskAssignmentReference
            | ManagedField::HostHealth
            | ManagedField::ApprovedArtifactReference
            | ManagedField::ProviderQuota
            | ManagedField::ProviderCost
            | ManagedField::ProviderEstimate
            | ManagedField::Prompt
            | ManagedField::Response
            | ManagedField::Terminal
            | ManagedField::Browser
            | ManagedField::Recording
            | ManagedField::FileBody
            | ManagedField::FullDiff
            | ManagedField::Credentials
            | ManagedField::EnvironmentValue
            | ManagedField::Unknown => false,
        }
    }

    fn with_content_id(mut self) -> Self {
        self.observation_id = ObservationId(content_hash(&self));
        self
    }

    fn interval_key(&self) -> Option<IntervalKey> {
        self.active_session.map(|interval| {
            (
                self.task_id,
                interval.started_at_ms(),
                interval.ended_at_ms(),
            )
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservationSchema {
    revision: u32,
}

impl ObservationSchema {
    pub const fn current() -> Self {
        Self {
            revision: OBSERVATION_SCHEMA_REVISION,
        }
    }

    pub const fn revision(self) -> u32 {
        self.revision
    }

    pub fn allows_aggregate(self, name: &str) -> bool {
        AGGREGATE_ALLOWLIST.contains(&name)
    }

    pub fn decode(bytes: &[u8]) -> Result<ObservationRecord, ObservationError> {
        if bytes.len() > MAX_OBSERVATION_DOCUMENT_BYTES {
            return Err(ObservationError::BoundExceeded);
        }
        let wire = decode_observation_wire(bytes)?;
        ObservationRecord::from_wire(wire)
    }
}

fn interval_within_retention(ended_at_ms: u64, now_ms: u64) -> bool {
    now_ms.saturating_sub(ended_at_ms) < MAX_OBSERVATION_RETENTION_MS
}

pub(crate) fn reject_encoded_page_bytes(encoded_bytes: usize) -> Result<(), ObservationError> {
    if encoded_bytes > MAX_CONNECT_PAGE_ENCODED_BYTES as usize {
        return Err(ObservationError::BoundExceeded);
    }
    Ok(())
}

pub fn encode_observation(record: &ObservationRecord) -> Result<Vec<u8>, ObservationError> {
    let encoded = encode_observation_wire(&record.to_wire())?;
    if encoded.len() > MAX_OBSERVATION_DOCUMENT_BYTES {
        return Err(ObservationError::BoundExceeded);
    }
    Ok(encoded)
}

#[derive(Debug, Clone, Copy)]
struct ActivityPoint {
    at_ms: u64,
    event_id: EventId,
    #[allow(dead_code)]
    client_id: ClientId,
    #[allow(dead_code)]
    kind_tag: u8,
}

#[derive(Clone)]
struct TaskState {
    facts: Option<TaskObservationFacts>,
    activities: BTreeMap<(u64, EventId), ActivityPoint>,
    usage: UsageSnapshot,
    git: Option<GitObservation>,
    human_message_count: u32,
    human_turn_count: u32,
    last_human_at_ms: Option<u64>,
    source_event_id: Option<EventId>,
}

impl fmt::Debug for TaskState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TaskState")
            .field("has_facts", &self.facts.is_some())
            .field("activities", &self.activities.len())
            .field("usage", &"<redacted>")
            .field("git", &self.git.as_ref().map(|_| "<redacted>"))
            .field("human_message_count", &self.human_message_count)
            .field("human_turn_count", &self.human_turn_count)
            .finish_non_exhaustive()
    }
}

impl TaskState {
    fn new() -> Self {
        Self {
            facts: None,
            activities: BTreeMap::new(),
            usage: UsageSnapshot::default(),
            git: None,
            human_message_count: 0,
            human_turn_count: 0,
            last_human_at_ms: None,
            source_event_id: None,
        }
    }

    fn derived_observed_at(&self) -> u64 {
        let mut observed = 0;
        if let Some(facts) = &self.facts {
            observed = observed.max(facts.source_at_ms);
        }
        for measure in self.usage.iter() {
            observed = observed.max(measure.observed_at_ms);
        }
        if let Some(git) = &self.git {
            observed = observed.max(git.observed_at_ms);
        }
        if let Some(point) = self.activities.values().next_back() {
            observed = observed.max(point.at_ms);
        }
        if let Some(at_ms) = self.last_human_at_ms {
            observed = observed.max(at_ms);
        }
        observed
    }
}

type IntervalKey = (TaskId, u64, u64);

pub struct ObservationReducer {
    now_ms: u64,
    authority: ObservationAuthority,
    tasks: BTreeMap<TaskId, TaskState>,
    seen: BTreeMap<EventId, [u8; 32]>,
    frozen: BTreeMap<IntervalKey, ObservationRecord>,
    #[cfg(test)]
    revoke_before_commit: AtomicBool,
    #[cfg(test)]
    revoke_before_observe_commit: AtomicBool,
}

impl fmt::Debug for ObservationReducer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ObservationReducer")
            .field("now_ms", &self.now_ms)
            .field("authority", &self.authority)
            .field("tasks", &self.tasks.len())
            .field("seen", &self.seen.len())
            .field("frozen", &self.frozen.len())
            .finish()
    }
}

impl ObservationReducer {
    pub fn from_host_time(
        now_ms: u64,
        authority: ObservationAuthority,
    ) -> Result<Self, ObservationError> {
        authority.ensure_live()?;
        Ok(Self {
            now_ms,
            authority,
            tasks: BTreeMap::new(),
            seen: BTreeMap::new(),
            frozen: BTreeMap::new(),
            #[cfg(test)]
            revoke_before_commit: AtomicBool::new(false),
            #[cfg(test)]
            revoke_before_observe_commit: AtomicBool::new(false),
        })
    }

    pub fn now_ms(&self) -> u64 {
        self.now_ms
    }

    fn ensure_live(&self) -> Result<(), ObservationError> {
        self.authority.ensure_live()
    }

    fn begin_record(
        &self,
        grant: &ManagementGrant,
        task_id: TaskId,
    ) -> Result<(), ObservationError> {
        self.ensure_live()?;
        if !self.authority.matches_grant(grant) || task_id != self.authority.bound_task_id() {
            return Err(ObservationError::Unavailable(
                ObservationDependency::AuthoritativeSource,
            ));
        }
        Ok(())
    }

    fn finish_record(&self, grant: &ManagementGrant) -> Result<(), ObservationError> {
        #[cfg(test)]
        if self.revoke_before_commit.swap(false, Ordering::AcqRel) {
            grant.revoke();
        }
        if !self.authority.matches_grant(grant) {
            return Err(ObservationError::Unavailable(
                ObservationDependency::AuthoritativeSource,
            ));
        }
        self.ensure_live()
    }

    fn with_record<R>(
        &mut self,
        grant: &ManagementGrant,
        task_id: TaskId,
        work: impl FnOnce(&mut Self) -> Result<R, ObservationError>,
    ) -> Result<R, ObservationError> {
        self.begin_record(grant, task_id)?;
        let snapshot = RecordSnapshot {
            tasks: self.tasks.clone(),
            seen: self.seen.clone(),
        };
        match work(self) {
            Ok(value) => {
                if let Err(err) = self.finish_record(grant) {
                    self.restore_record_snapshot(snapshot);
                    return Err(err);
                }
                Ok(value)
            }
            Err(err) => {
                self.restore_record_snapshot(snapshot);
                Err(err)
            }
        }
    }

    fn restore_record_snapshot(&mut self, snapshot: RecordSnapshot) {
        self.tasks = snapshot.tasks;
        self.seen = snapshot.seen;
    }

    pub fn observe_at(&mut self, now_ms: u64) -> Result<(), ObservationError> {
        let epoch = self.authority.capture_settlement_epoch()?;
        if now_ms < self.now_ms {
            return Err(ObservationError::StaleRevision);
        }
        if self.would_exceed_ready_bound(now_ms)? {
            return Err(ObservationError::Backpressure);
        }
        let snapshot = ObserveSnapshot {
            frozen: self.frozen.clone(),
            now_ms: self.now_ms,
        };
        self.evict_expired(now_ms);
        self.now_ms = now_ms;
        self.freeze_newly_closed();
        #[cfg(test)]
        if self
            .revoke_before_observe_commit
            .swap(false, Ordering::AcqRel)
        {
            self.authority.revoke_lease_for_test();
        }
        if let Err(err) = self.authority.confirm_settlement_epoch(epoch) {
            self.frozen = snapshot.frozen;
            self.now_ms = snapshot.now_ms;
            return Err(err);
        }
        Ok(())
    }

    pub fn advance(&mut self, duration: Duration) -> Result<(), ObservationError> {
        let millis =
            u64::try_from(duration.as_millis()).map_err(|_| ObservationError::InvalidWindow)?;
        let new_now = self
            .now_ms
            .checked_add(millis)
            .ok_or(ObservationError::CounterOverflow)?;
        self.observe_at(new_now)
    }

    pub fn record_task_facts(
        &mut self,
        grant: &ManagementGrant,
        facts: TaskObservationFacts,
    ) -> Result<ReduceOutcome, ObservationError> {
        let task_id = facts.task_id;
        self.with_record(grant, task_id, |this| {
            if facts.source_at_ms > this.now_ms {
                return Err(ObservationError::FutureTimestamp);
            }
            if let Some(existing) = this.tasks.get(&facts.task_id) {
                if let Some(current) = &existing.facts {
                    if facts.revision < current.revision
                        || (facts.revision == current.revision
                            && facts.source_at_ms < current.source_at_ms)
                    {
                        return Err(ObservationError::StaleRevision);
                    }
                    if facts.revision == current.revision
                        && facts.source_at_ms == current.source_at_ms
                    {
                        return if facts == *current {
                            Ok(ReduceOutcome::Duplicate)
                        } else {
                            Err(ObservationError::Conflict)
                        };
                    }
                }
            } else if this.tasks.len() >= MAX_OBSERVATION_TASKS {
                return Err(ObservationError::BoundExceeded);
            }
            this.tasks
                .entry(task_id)
                .or_insert_with(TaskState::new)
                .facts = Some(facts);
            Ok(ReduceOutcome::Accepted)
        })
    }

    pub fn record_activity(
        &mut self,
        grant: &ManagementGrant,
        activity: QualifyingActivity,
    ) -> Result<ReduceOutcome, ObservationError> {
        self.with_record(grant, activity.task_id(), |this| {
            let hash = this.activity_hash(activity);
            if let Some(existing) = this.seen.get(&activity.event_id()) {
                return if *existing == hash {
                    Ok(ReduceOutcome::Duplicate)
                } else {
                    Err(ObservationError::Conflict)
                };
            }
            if this.seen.len() >= MAX_SEEN_EVENTS {
                return Err(ObservationError::BoundExceeded);
            }
            let now_ms = this.now_ms;
            let task = this.task_mut(activity.task_id())?;
            if task.activities.len() >= MAX_ACTIVITIES_PER_TASK {
                return Err(ObservationError::BoundExceeded);
            }
            let point = ActivityPoint {
                at_ms: now_ms,
                event_id: activity.event_id(),
                client_id: activity.client_id(),
                kind_tag: activity.kind_tag(),
            };
            task.activities.insert((point.at_ms, point.event_id), point);
            if task.source_event_id.is_none() {
                task.source_event_id = Some(activity.event_id());
            }
            this.seen.insert(activity.event_id(), hash);
            Ok(ReduceOutcome::Accepted)
        })
    }

    pub fn record_non_qualifying_provider_cpu(
        &mut self,
        grant: &ManagementGrant,
        task_id: TaskId,
        _event_id: EventId,
    ) -> Result<ReduceOutcome, ObservationError> {
        self.with_record(grant, task_id, |_| Ok(ReduceOutcome::Ignored))
    }

    pub fn record_message(
        &mut self,
        grant: &ManagementGrant,
        task_id: TaskId,
        event_id: EventId,
        class: ObservationMessageClass,
        starts_turn: bool,
    ) -> Result<ReduceOutcome, ObservationError> {
        self.with_record(grant, task_id, |this| {
            if class != ObservationMessageClass::Human {
                return Ok(ReduceOutcome::Ignored);
            }
            let hash = hash_parts(&[
                b"message.human",
                task_id.as_bytes().as_slice(),
                event_id.as_bytes().as_slice(),
                &[u8::from(starts_turn)],
            ]);
            if let Some(existing) = this.seen.get(&event_id) {
                return if *existing == hash {
                    Ok(ReduceOutcome::Duplicate)
                } else {
                    Err(ObservationError::Conflict)
                };
            }
            if this.seen.len() >= MAX_SEEN_EVENTS {
                return Err(ObservationError::BoundExceeded);
            }
            let now_ms = this.now_ms;
            let task = this.task_mut(task_id)?;
            task.human_message_count = task
                .human_message_count
                .checked_add(1)
                .ok_or(ObservationError::CounterOverflow)?;
            if starts_turn {
                task.human_turn_count = task
                    .human_turn_count
                    .checked_add(1)
                    .ok_or(ObservationError::CounterOverflow)?;
            }
            task.last_human_at_ms = Some(now_ms);
            if task.source_event_id.is_none() {
                task.source_event_id = Some(event_id);
            }
            this.seen.insert(event_id, hash);
            Ok(ReduceOutcome::Accepted)
        })
    }

    pub fn record_usage(
        &mut self,
        grant: &ManagementGrant,
        task_id: TaskId,
        measure: UsageMeasure,
    ) -> Result<ReduceOutcome, ObservationError> {
        self.with_record(grant, task_id, |this| {
            if measure.observed_at_ms > this.now_ms {
                return Err(ObservationError::FutureTimestamp);
            }
            this.task_mut(task_id)?.usage.insert(measure)
        })
    }

    pub fn record_git(
        &mut self,
        grant: &ManagementGrant,
        task_id: TaskId,
        summary: RestrictiveGitSummary,
        source_event: EventId,
        observed_at_ms: u64,
        revision: u64,
    ) -> Result<ReduceOutcome, ObservationError> {
        self.with_record(grant, task_id, |this| {
            if observed_at_ms > this.now_ms {
                return Err(ObservationError::FutureTimestamp);
            }
            let incoming = GitObservation::try_new(summary, source_event, observed_at_ms, revision);
            let hash = hash_parts(&[
                b"git.event",
                task_id.as_bytes().as_slice(),
                source_event.as_bytes().as_slice(),
                &incoming.payload_hash,
                &observed_at_ms.to_be_bytes(),
                &revision.to_be_bytes(),
            ]);
            if let Some(existing) = this.seen.get(&source_event) {
                return if *existing == hash {
                    Ok(ReduceOutcome::Duplicate)
                } else {
                    Err(ObservationError::Conflict)
                };
            }
            if this.seen.len() >= MAX_SEEN_EVENTS {
                return Err(ObservationError::BoundExceeded);
            }
            let task = this.task_mut(task_id)?;
            if let Some(current) = &task.git {
                if revision < current.revision
                    || (revision == current.revision && observed_at_ms < current.observed_at_ms)
                {
                    return Err(ObservationError::StaleRevision);
                }
                if revision == current.revision && observed_at_ms == current.observed_at_ms {
                    return if incoming.payload_hash == current.payload_hash {
                        Ok(ReduceOutcome::Duplicate)
                    } else {
                        Err(ObservationError::Conflict)
                    };
                }
            }
            task.git = Some(incoming);
            this.seen.insert(source_event, hash);
            Ok(ReduceOutcome::Accepted)
        })
    }

    pub fn current_observation(
        &self,
        task_id: TaskId,
    ) -> Result<ObservationRecord, ObservationError> {
        self.ensure_live()?;
        if task_id != self.authority.bound_task_id() {
            return Err(ObservationError::Unavailable(
                ObservationDependency::AuthoritativeSource,
            ));
        }
        let task = self
            .tasks
            .get(&task_id)
            .ok_or(ObservationError::Unavailable(
                ObservationDependency::AuthoritativeSource,
            ))?;
        let source = task.source_event_id.ok_or(ObservationError::Unavailable(
            ObservationDependency::AuthoritativeSource,
        ))?;
        let record = self.record_from_task(
            task_id,
            task,
            source,
            None,
            task.derived_observed_at(),
            true,
        );
        self.ensure_live()?;
        Ok(record)
    }

    pub fn ready_page(
        &self,
        budget: PageBudget,
        cursor: Option<ObservationCursor>,
    ) -> Result<ObservationPage, ObservationError> {
        self.ensure_live()?;
        let collected = self.page_frozen(budget, cursor)?;
        let admitted = self.admit_encoded_page(&collected, cursor)?;
        let mut items = Vec::new();
        items
            .try_reserve(admitted.fit)
            .map_err(|_| ObservationError::BoundExceeded)?;
        items.extend(
            collected
                .keys
                .iter()
                .take(admitted.fit)
                .filter_map(|key| self.frozen.get(key).cloned()),
        );
        self.ensure_live()?;
        Ok(ObservationPage {
            items,
            more: admitted.more,
            work_used: admitted.work_used,
            next_cursor: admitted.next_cursor,
        })
    }

    pub fn inspect_pending(
        &self,
        budget: PageBudget,
        cursor: Option<ObservationCursor>,
    ) -> Result<ObservationIdPage, ObservationError> {
        self.ensure_live()?;
        let collected = self.page_frozen(budget, cursor)?;
        let admitted = self.admit_encoded_page(&collected, cursor)?;
        let mut ids = Vec::new();
        ids.try_reserve(admitted.fit)
            .map_err(|_| ObservationError::BoundExceeded)?;
        ids.extend(
            collected
                .keys
                .iter()
                .take(admitted.fit)
                .filter_map(|key| self.frozen.get(key).map(|item| item.observation_id)),
        );
        self.ensure_live()?;
        Ok(ObservationIdPage {
            ids,
            more: admitted.more,
            work_used: admitted.work_used,
            next_cursor: admitted.next_cursor,
        })
    }

    fn admit_encoded_page(
        &self,
        collected: &FrozenPage,
        cursor: Option<ObservationCursor>,
    ) -> Result<EncodedAdmission, ObservationError> {
        let mut encoded_total = 0usize;
        let mut fit = 0usize;
        let mut more = collected.more;
        let mut next_cursor = cursor;
        let mut work_used = 0_u32;
        for key in &collected.keys {
            let Some(record) = self.frozen.get(key) else {
                continue;
            };
            let encoded = encode_observation(record)?;
            let next = encoded_total
                .checked_add(encoded.len())
                .ok_or(ObservationError::CounterOverflow)?;
            if reject_encoded_page_bytes(next).is_err() {
                if fit == 0 {
                    return Err(ObservationError::BoundExceeded);
                }
                more = true;
                break;
            }
            encoded_total = next;
            work_used = work_used
                .checked_add(1)
                .ok_or(ObservationError::CounterOverflow)?;
            next_cursor = Some(ObservationCursor {
                task_id: key.0,
                started_at_ms: key.1,
                ended_at_ms: key.2,
            });
            fit += 1;
        }
        if fit < collected.keys.len() {
            more = true;
        }
        Ok(EncodedAdmission {
            fit,
            more,
            work_used,
            next_cursor,
        })
    }

    pub fn request_delivery(&self, _id: ObservationId) -> Result<ReduceOutcome, ObservationError> {
        self.ensure_live()?;
        Err(ObservationError::Unavailable(
            ObservationDependency::DurableOutbox,
        ))
    }

    pub fn acknowledge(&mut self, _id: ObservationId) -> Result<ReduceOutcome, ObservationError> {
        self.ensure_live()?;
        Err(ObservationError::Unavailable(
            ObservationDependency::DurableOutbox,
        ))
    }

    pub fn request_organization_publication(
        &self,
        _id: ObservationId,
    ) -> Result<ReduceOutcome, ObservationError> {
        self.ensure_live()?;
        Err(ObservationError::Unavailable(
            ObservationDependency::PortalObservationEffect,
        ))
    }

    fn page_frozen(
        &self,
        budget: PageBudget,
        cursor: Option<ObservationCursor>,
    ) -> Result<FrozenPage, ObservationError> {
        let max_items = usize::from(budget.max_items).min(MAX_CONNECT_PAGE_ITEMS as usize);
        if budget.max_work == 0 || max_items == 0 {
            return Ok(FrozenPage {
                keys: Vec::new(),
                more: self.has_after(cursor),
                work_used: 0,
                next_cursor: cursor,
            });
        }
        let mut keys = Vec::new();
        keys.try_reserve(max_items)
            .map_err(|_| ObservationError::BoundExceeded)?;
        let mut work_used = 0_u32;
        let mut more = false;
        let mut next_cursor = cursor;
        for (key, _) in self.frozen.range(after_range(cursor)) {
            if work_used == budget.max_work || keys.len() == max_items {
                more = true;
                break;
            }
            work_used = work_used
                .checked_add(1)
                .ok_or(ObservationError::CounterOverflow)?;
            next_cursor = Some(ObservationCursor {
                task_id: key.0,
                started_at_ms: key.1,
                ended_at_ms: key.2,
            });
            keys.push(*key);
        }
        if !more {
            more = self.has_after(next_cursor);
        }
        Ok(FrozenPage {
            keys,
            more,
            work_used,
            next_cursor,
        })
    }

    fn has_after(&self, cursor: Option<ObservationCursor>) -> bool {
        self.frozen.range(after_range(cursor)).next().is_some()
    }

    fn task_mut(&mut self, task_id: TaskId) -> Result<&mut TaskState, ObservationError> {
        if self.tasks.contains_key(&task_id) {
            return Ok(self.tasks.get_mut(&task_id).expect("checked"));
        }
        if self.tasks.len() >= MAX_OBSERVATION_TASKS {
            return Err(ObservationError::BoundExceeded);
        }
        Ok(self.tasks.entry(task_id).or_insert_with(TaskState::new))
    }

    fn activity_hash(&self, activity: QualifyingActivity) -> [u8; 32] {
        let revision = self
            .tasks
            .get(&activity.task_id())
            .and_then(|task| task.facts.as_ref())
            .map(|facts| facts.revision)
            .unwrap_or(0);
        hash_parts(&[
            b"activity",
            activity.task_id().as_bytes().as_slice(),
            &[activity.kind_tag()],
            activity.client_id().as_bytes().as_slice(),
            activity.event_id().as_bytes().as_slice(),
            &revision.to_be_bytes(),
            &OBSERVATION_SCHEMA_REVISION.to_be_bytes(),
        ])
    }

    fn evict_expired(&mut self, now_ms: u64) {
        self.frozen
            .retain(|key, _| interval_within_retention(key.2, now_ms));
    }

    fn would_exceed_ready_bound(&self, now_ms: u64) -> Result<bool, ObservationError> {
        let retained = self
            .frozen
            .keys()
            .filter(|key| interval_within_retention(key.2, now_ms))
            .count();
        let mut added = 0usize;
        for (task_id, task) in &self.tasks {
            for chunk in closed_chunks(task, now_ms) {
                if !interval_within_retention(chunk.interval.ended_at_ms(), now_ms) {
                    continue;
                }
                let key = (
                    *task_id,
                    chunk.interval.started_at_ms(),
                    chunk.interval.ended_at_ms(),
                );
                if self.frozen.contains_key(&key) && interval_within_retention(key.2, now_ms) {
                    continue;
                }
                added = added
                    .checked_add(1)
                    .ok_or(ObservationError::CounterOverflow)?;
                if retained.saturating_add(added) > MAX_READY_INTERVALS {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    fn freeze_newly_closed(&mut self) {
        let now_ms = self.now_ms;
        let mut pending = Vec::new();
        let _ = pending.try_reserve(MAX_READY_INTERVALS);
        for (task_id, task) in &self.tasks {
            for chunk in closed_chunks(task, now_ms) {
                if !interval_within_retention(chunk.interval.ended_at_ms(), now_ms) {
                    continue;
                }
                let key = (
                    *task_id,
                    chunk.interval.started_at_ms(),
                    chunk.interval.ended_at_ms(),
                );
                if self.frozen.contains_key(&key) {
                    continue;
                }
                pending.push((*task_id, chunk));
            }
        }
        for (task_id, chunk) in pending {
            let Some(task) = self.tasks.get(&task_id) else {
                continue;
            };
            let record = self.record_from_task(
                task_id,
                task,
                chunk.source_event_id,
                Some(chunk.interval),
                task.derived_observed_at(),
                false,
            );
            if let Some(key) = record.interval_key() {
                self.frozen.insert(key, record);
            }
        }
    }

    fn record_from_task(
        &self,
        task_id: TaskId,
        task: &TaskState,
        source_event_id: EventId,
        interval: Option<ActiveSessionInterval>,
        observed_at_ms: u64,
        apply_freshness: bool,
    ) -> ObservationRecord {
        let facts = task.facts.as_ref();
        let git = task.git.clone().filter(|item| {
            !apply_freshness
                || self.now_ms.saturating_sub(item.observed_at_ms) < OBSERVATION_STALE_AFTER_MS
        });
        let usage = if apply_freshness {
            filter_fresh_usage(&task.usage, self.now_ms)
        } else {
            task.usage.clone()
        };
        let (completeness, confidence) = derive_quality(git.as_ref(), &usage);
        ObservationRecord {
            observation_id: ObservationId([0; 32]),
            task_id,
            lifecycle: facts
                .map(|item| item.lifecycle)
                .unwrap_or(TaskLifecycle::Open),
            attention: facts
                .map(|item| item.attention)
                .unwrap_or(TaskAttention::None),
            connectivity: facts
                .map(|item| item.connectivity)
                .unwrap_or(TaskConnectivity::Disconnected),
            primary: facts.and_then(|item| item.primary.clone()),
            specialists: facts
                .map(|item| item.specialists.clone())
                .unwrap_or_default(),
            git,
            usage,
            human_message_count: task.human_message_count,
            human_turn_count: task.human_turn_count,
            active_session: interval,
            source_event_id,
            observed_at_ms,
            source_at_ms: facts
                .map(|item| item.source_at_ms)
                .unwrap_or(observed_at_ms),
            facts_revision: facts.map(|item| item.revision).unwrap_or(0),
            completeness,
            confidence,
            policy_revision: self.authority.policy_revision(),
            schema_revision: OBSERVATION_SCHEMA_REVISION,
        }
        .with_content_id()
    }
}

#[cfg(test)]
impl ObservationReducer {
    pub(crate) fn arm_revoke_before_commit(&self) {
        self.revoke_before_commit.store(true, Ordering::Release);
    }

    pub(crate) fn arm_revoke_before_observe_commit(&self) {
        self.revoke_before_observe_commit
            .store(true, Ordering::Release);
    }

    pub(crate) fn recorded_frozen_ids(&self) -> Vec<ObservationId> {
        self.frozen
            .values()
            .map(|record| record.observation_id)
            .collect()
    }

    pub(crate) fn recorded_facts_revision(&self, task_id: TaskId) -> Option<u64> {
        self.tasks
            .get(&task_id)
            .and_then(|task| task.facts.as_ref())
            .map(|facts| facts.revision)
    }

    pub(crate) fn recorded_activity_len(&self, task_id: TaskId) -> usize {
        self.tasks
            .get(&task_id)
            .map(|task| task.activities.len())
            .unwrap_or(0)
    }

    pub(crate) fn recorded_human_message_count(&self, task_id: TaskId) -> u32 {
        self.tasks
            .get(&task_id)
            .map(|task| task.human_message_count)
            .unwrap_or(0)
    }

    pub(crate) fn recorded_has_usage_tokens(&self, task_id: TaskId) -> bool {
        self.tasks
            .get(&task_id)
            .is_some_and(|task| task.usage.tokens().is_some())
    }

    pub(crate) fn recorded_has_git(&self, task_id: TaskId) -> bool {
        self.tasks
            .get(&task_id)
            .is_some_and(|task| task.git.is_some())
    }
}

struct FrozenPage {
    keys: Vec<IntervalKey>,
    more: bool,
    work_used: u32,
    next_cursor: Option<ObservationCursor>,
}

struct EncodedAdmission {
    fit: usize,
    more: bool,
    work_used: u32,
    next_cursor: Option<ObservationCursor>,
}

struct RecordSnapshot {
    tasks: BTreeMap<TaskId, TaskState>,
    seen: BTreeMap<EventId, [u8; 32]>,
}

struct ObserveSnapshot {
    frozen: BTreeMap<IntervalKey, ObservationRecord>,
    now_ms: u64,
}

fn after_range(cursor: Option<ObservationCursor>) -> (Bound<IntervalKey>, Bound<IntervalKey>) {
    match cursor {
        None => (Bound::Unbounded, Bound::Unbounded),
        Some(cursor) => (
            Bound::Excluded((cursor.task_id, cursor.started_at_ms, cursor.ended_at_ms)),
            Bound::Unbounded,
        ),
    }
}

fn filter_fresh_usage(usage: &UsageSnapshot, now_ms: u64) -> UsageSnapshot {
    let keep = |measure: &UsageMeasure| {
        now_ms.saturating_sub(measure.observed_at_ms) < OBSERVATION_STALE_AFTER_MS
    };
    UsageSnapshot {
        tokens: usage.tokens.clone().filter(keep),
        quota_remaining: usage.quota_remaining.clone().filter(keep),
        quota_reset: usage.quota_reset.clone().filter(keep),
        quoted_cost: usage.quoted_cost.clone().filter(keep),
        local_token_estimate: usage.local_token_estimate.clone().filter(keep),
        local_cost_estimate: usage.local_cost_estimate.clone().filter(keep),
    }
}

fn derive_quality(
    git: Option<&GitObservation>,
    usage: &UsageSnapshot,
) -> (ObservationCompleteness, ObservationConfidence) {
    let completeness = if git.is_some() && usage.has_provider_reported() {
        ObservationCompleteness::Complete
    } else {
        ObservationCompleteness::Partial
    };
    let confidence = if usage.has_provider_reported() {
        ObservationConfidence::High
    } else if usage.local_token_estimate.is_some() || usage.local_cost_estimate.is_some() {
        ObservationConfidence::Low
    } else {
        ObservationConfidence::Unavailable
    };
    (completeness, confidence)
}

#[derive(Debug, Clone, Copy)]
struct ReadyChunk {
    interval: ActiveSessionInterval,
    source_event_id: EventId,
}

fn closed_chunks(task: &TaskState, now_ms: u64) -> impl Iterator<Item = ReadyChunk> + '_ {
    let mut points = task.activities.values().copied().peekable();
    let mut ready = Vec::new();
    while let Some(first) = points.next() {
        let mut last = first;
        while let Some(next) = points.peek().copied() {
            if next.at_ms.saturating_sub(last.at_ms) < ACTIVE_SESSION_IDLE_LIMIT_MS {
                last = next;
                points.next();
            } else {
                break;
            }
        }
        let full_end = last.at_ms.saturating_add(ACTIVE_SESSION_IDLE_LIMIT_MS);
        let mut cursor = first.at_ms;
        while cursor < full_end {
            let chunk_end = full_end.min(cursor.saturating_add(ACTIVE_SESSION_IDLE_LIMIT_MS));
            if now_ms >= chunk_end {
                if let Ok(interval) = ActiveSessionInterval::try_new(cursor, chunk_end) {
                    ready.push(ReadyChunk {
                        interval,
                        source_event_id: first.event_id,
                    });
                }
            }
            cursor = chunk_end;
        }
    }
    ready.into_iter()
}

fn validate_commit(value: &str) -> Result<String, ObservationError> {
    let canonical = canonical::canonicalize(value).ok_or(ObservationError::InvalidGit)?;
    if canonical.len() < MIN_GIT_COMMIT_HEX || canonical.len() > MAX_GIT_COMMIT_HEX {
        return Err(ObservationError::InvalidGit);
    }
    if !canonical
        .bytes()
        .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(ObservationError::InvalidGit);
    }
    Ok(canonical)
}

fn hash_parts(parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for part in parts {
        let len = u64::try_from(part.len()).expect("hash part length fits u64");
        hasher.update(len.to_be_bytes());
        hasher.update(part);
    }
    hasher.finalize().into()
}

fn hash_opt_str(hasher: &mut Sha256, value: Option<&str>) {
    match value {
        Some(value) => {
            hasher.update([1_u8]);
            let len = u64::try_from(value.len()).expect("string length fits u64");
            hasher.update(len.to_be_bytes());
            hasher.update(value.as_bytes());
        }
        None => hasher.update([0_u8]),
    }
}

fn hash_provider(hasher: &mut Sha256, provider: &ProviderObservation) {
    hash_opt_str(hasher, Some(provider.kind.as_str()));
    match &provider.role {
        AgentRole::Primary => hasher.update([0_u8]),
        AgentRole::Specialist { name } => {
            hasher.update([1_u8]);
            hash_opt_str(hasher, Some(name.as_str()));
        }
    }
    hasher.update([lifecycle_tag(provider.lifecycle)]);
    hasher.update([activity_tag(provider.activity)]);
}

fn lifecycle_tag(lifecycle: AgentSessionLifecycle) -> u8 {
    match lifecycle {
        AgentSessionLifecycle::Open => 1,
        AgentSessionLifecycle::Closing => 2,
        AgentSessionLifecycle::Closed => 3,
    }
}

fn activity_tag(activity: TaskActivity) -> u8 {
    match activity {
        TaskActivity::Idle => 1,
        TaskActivity::Working => 2,
        TaskActivity::Settling => 3,
    }
}

fn content_hash(record: &ObservationRecord) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"observation.v1");
    hasher.update(record.task_id.as_bytes());
    hasher.update([match record.lifecycle {
        TaskLifecycle::Open => 1,
        TaskLifecycle::Closing => 2,
        TaskLifecycle::Archived => 3,
    }]);
    hasher.update([match record.attention {
        TaskAttention::None => 0,
        TaskAttention::NeedsAnswer => 1,
        TaskAttention::NeedsApproval => 2,
        TaskAttention::UncertainOutcome => 3,
        TaskAttention::Failed => 4,
    }]);
    hasher.update([match record.connectivity {
        TaskConnectivity::Connected => 1,
        TaskConnectivity::Disconnected => 0,
    }]);
    match &record.primary {
        Some(provider) => {
            hasher.update([1_u8]);
            hash_provider(&mut hasher, provider);
        }
        None => hasher.update([0_u8]),
    }
    let specialist_len = u64::try_from(record.specialists.len()).expect("specialist count");
    hasher.update(specialist_len.to_be_bytes());
    for specialist in &record.specialists {
        hash_provider(&mut hasher, specialist);
    }
    match &record.git {
        Some(git) => {
            hasher.update([1_u8]);
            hash_opt_str(&mut hasher, git.summary.branch.as_deref());
            hash_opt_str(&mut hasher, git.summary.commit.as_deref());
            hasher.update(git.summary.files_changed.to_be_bytes());
            hasher.update(git.summary.insertions.to_be_bytes());
            hasher.update(git.summary.deletions.to_be_bytes());
            hasher.update(git.source_event_id.as_bytes());
            hasher.update(git.observed_at_ms.to_be_bytes());
            hasher.update(git.revision.to_be_bytes());
            hasher.update(git.payload_hash);
        }
        None => hasher.update([0_u8]),
    }
    hash_usage_slot(&mut hasher, record.usage.tokens.as_ref());
    hash_usage_slot(&mut hasher, record.usage.quota_remaining.as_ref());
    hash_usage_slot(&mut hasher, record.usage.quota_reset.as_ref());
    hash_usage_slot(&mut hasher, record.usage.quoted_cost.as_ref());
    hash_usage_slot(&mut hasher, record.usage.local_token_estimate.as_ref());
    hash_usage_slot(&mut hasher, record.usage.local_cost_estimate.as_ref());
    hasher.update(record.human_message_count.to_be_bytes());
    hasher.update(record.human_turn_count.to_be_bytes());
    match record.active_session {
        Some(interval) => {
            hasher.update([1_u8]);
            hasher.update(interval.started_at_ms().to_be_bytes());
            hasher.update(interval.ended_at_ms().to_be_bytes());
        }
        None => hasher.update([0_u8]),
    }
    hasher.update(record.source_event_id.as_bytes());
    hasher.update(record.observed_at_ms.to_be_bytes());
    hasher.update(record.source_at_ms.to_be_bytes());
    hasher.update(record.facts_revision.to_be_bytes());
    hasher.update([match record.completeness {
        ObservationCompleteness::Complete => 2,
        ObservationCompleteness::Partial => 1,
        ObservationCompleteness::Unavailable => 0,
    }]);
    hasher.update([match record.confidence {
        ObservationConfidence::High => 2,
        ObservationConfidence::Low => 1,
        ObservationConfidence::Unavailable => 0,
    }]);
    hasher.update(record.policy_revision.to_be_bytes());
    hasher.update(record.schema_revision.to_be_bytes());
    hasher.finalize().into()
}

fn hash_usage_slot(hasher: &mut Sha256, measure: Option<&UsageMeasure>) {
    let Some(measure) = measure else {
        hasher.update([0_u8]);
        return;
    };
    hasher.update([1_u8]);
    hash_opt_str(hasher, Some(measure.provider.as_str()));
    hash_opt_str(hasher, Some(measure.source.as_str()));
    hasher.update([match measure.kind {
        UsageKind::Tokens => 1,
        UsageKind::QuotaRemaining => 2,
        UsageKind::QuotaReset => 3,
        UsageKind::MonetaryQuote => 4,
    }]);
    hasher.update([match measure.provenance {
        UsageProvenance::ProviderReported => 1,
        UsageProvenance::ProviderQuoted => 2,
        UsageProvenance::LocalEstimate => 3,
    }]);
    match measure.value {
        Some(value) => {
            hasher.update([1_u8]);
            hasher.update(value.to_be_bytes());
        }
        None => hasher.update([0_u8]),
    }
    hash_opt_str(hasher, Some(measure.unit.as_str()));
    match measure.window {
        Some((start, end)) => {
            hasher.update([1_u8]);
            hasher.update(start.to_be_bytes());
            hasher.update(end.to_be_bytes());
        }
        None => hasher.update([0_u8]),
    }
    hasher.update(measure.observed_at_ms.to_be_bytes());
    hasher.update(measure.revision.to_be_bytes());
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn hex_nibble(value: u8) -> Result<u8, ObservationError> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => Err(ObservationError::ProhibitedContent),
    }
}

struct ObservationWire {
    observation_id: String,
    task_id: TaskId,
    lifecycle: TaskLifecycle,
    attention: TaskAttention,
    connectivity: TaskConnectivity,
    primary: Option<ProviderWire>,
    specialists: Vec<ProviderWire>,
    git: Option<GitWire>,
    usage: UsageWire,
    human_message_count: u32,
    human_turn_count: u32,
    active_session: Option<IntervalWire>,
    source_event_id: EventId,
    observed_at_ms: u64,
    source_at_ms: u64,
    facts_revision: u64,
    completeness: ObservationCompleteness,
    confidence: ObservationConfidence,
    policy_revision: u32,
    schema_revision: u32,
}

#[derive(Debug, Clone)]
struct ProviderWire {
    kind: String,
    role: AgentRole,
    lifecycle: AgentSessionLifecycle,
    activity: TaskActivity,
}

#[derive(Debug, Default, Clone)]
struct UsageWire {
    tokens: Option<UsageMeasureWire>,
    quota_remaining: Option<UsageMeasureWire>,
    quota_reset: Option<UsageMeasureWire>,
    quoted_cost: Option<UsageMeasureWire>,
    local_token_estimate: Option<UsageMeasureWire>,
    local_cost_estimate: Option<UsageMeasureWire>,
}

#[derive(Debug, Clone)]
struct UsageMeasureWire {
    provider: String,
    source: String,
    kind: UsageKind,
    provenance: UsageProvenance,
    value: Option<u64>,
    unit: String,
    window: Option<(u64, u64)>,
    observed_at_ms: u64,
    revision: u64,
}

#[derive(Debug, Clone)]
struct GitWire {
    branch: Option<String>,
    commit: Option<String>,
    files_changed: u32,
    insertions: u32,
    deletions: u32,
    source_event_id: EventId,
    observed_at_ms: u64,
    revision: u64,
}

#[derive(Debug, Clone, Copy)]
struct IntervalWire {
    started_at_ms: u64,
    ended_at_ms: u64,
}

impl ObservationRecord {
    fn to_wire(&self) -> ObservationWire {
        ObservationWire {
            observation_id: self.observation_id.to_hex(),
            task_id: self.task_id,
            lifecycle: self.lifecycle,
            attention: self.attention,
            connectivity: self.connectivity,
            primary: self.primary.as_ref().map(ProviderObservation::to_wire),
            specialists: self
                .specialists
                .iter()
                .map(ProviderObservation::to_wire)
                .collect(),
            git: self.git.as_ref().map(|item| GitWire {
                branch: item.summary.branch.clone(),
                commit: item.summary.commit.clone(),
                files_changed: item.summary.files_changed,
                insertions: item.summary.insertions,
                deletions: item.summary.deletions,
                source_event_id: item.source_event_id,
                observed_at_ms: item.observed_at_ms,
                revision: item.revision,
            }),
            usage: UsageWire {
                tokens: self.usage.tokens.as_ref().map(UsageMeasure::to_wire),
                quota_remaining: self
                    .usage
                    .quota_remaining
                    .as_ref()
                    .map(UsageMeasure::to_wire),
                quota_reset: self.usage.quota_reset.as_ref().map(UsageMeasure::to_wire),
                quoted_cost: self.usage.quoted_cost.as_ref().map(UsageMeasure::to_wire),
                local_token_estimate: self
                    .usage
                    .local_token_estimate
                    .as_ref()
                    .map(UsageMeasure::to_wire),
                local_cost_estimate: self
                    .usage
                    .local_cost_estimate
                    .as_ref()
                    .map(UsageMeasure::to_wire),
            },
            human_message_count: self.human_message_count,
            human_turn_count: self.human_turn_count,
            active_session: self.active_session.map(|interval| IntervalWire {
                started_at_ms: interval.started_at_ms(),
                ended_at_ms: interval.ended_at_ms(),
            }),
            source_event_id: self.source_event_id,
            observed_at_ms: self.observed_at_ms,
            source_at_ms: self.source_at_ms,
            facts_revision: self.facts_revision,
            completeness: self.completeness,
            confidence: self.confidence,
            policy_revision: self.policy_revision,
            schema_revision: self.schema_revision,
        }
    }

    fn from_wire(wire: ObservationWire) -> Result<Self, ObservationError> {
        if wire.schema_revision != OBSERVATION_SCHEMA_REVISION {
            return Err(ObservationError::Conflict);
        }
        if wire.specialists.len() > MAX_SPECIALISTS {
            return Err(ObservationError::BoundExceeded);
        }
        let claimed = ObservationId::from_hex(&wire.observation_id)?;
        let interval = match wire.active_session {
            Some(item) => Some(
                ActiveSessionInterval::try_new(item.started_at_ms, item.ended_at_ms)
                    .map_err(|_| ObservationError::InvalidWindow)?,
            ),
            None => None,
        };
        let usage = UsageSnapshot {
            tokens: wire.usage.tokens.map(UsageMeasure::from_wire).transpose()?,
            quota_remaining: wire
                .usage
                .quota_remaining
                .map(UsageMeasure::from_wire)
                .transpose()?,
            quota_reset: wire
                .usage
                .quota_reset
                .map(UsageMeasure::from_wire)
                .transpose()?,
            quoted_cost: wire
                .usage
                .quoted_cost
                .map(UsageMeasure::from_wire)
                .transpose()?,
            local_token_estimate: wire
                .usage
                .local_token_estimate
                .map(UsageMeasure::from_wire)
                .transpose()?,
            local_cost_estimate: wire
                .usage
                .local_cost_estimate
                .map(UsageMeasure::from_wire)
                .transpose()?,
        };
        let git = wire.git.map(GitObservation::from_wire).transpose()?;
        let (completeness, confidence) = derive_quality(git.as_ref(), &usage);
        if wire.completeness != completeness || wire.confidence != confidence {
            return Err(ObservationError::ProhibitedContent);
        }
        let record = Self {
            observation_id: ObservationId([0; 32]),
            task_id: wire.task_id,
            lifecycle: wire.lifecycle,
            attention: wire.attention,
            connectivity: wire.connectivity,
            primary: wire
                .primary
                .map(ProviderObservation::from_wire)
                .transpose()?,
            specialists: wire
                .specialists
                .into_iter()
                .map(ProviderObservation::from_wire)
                .collect::<Result<Vec<_>, _>>()?,
            git,
            usage,
            human_message_count: wire.human_message_count,
            human_turn_count: wire.human_turn_count,
            active_session: interval,
            source_event_id: wire.source_event_id,
            observed_at_ms: wire.observed_at_ms,
            source_at_ms: wire.source_at_ms,
            facts_revision: wire.facts_revision,
            completeness,
            confidence,
            policy_revision: wire.policy_revision,
            schema_revision: wire.schema_revision,
        }
        .with_content_id();
        if record.observation_id != claimed {
            return Err(ObservationError::Conflict);
        }
        Ok(record)
    }
}

impl GitObservation {
    fn from_wire(wire: GitWire) -> Result<Self, ObservationError> {
        let summary = RestrictiveGitSummary::try_new(
            wire.branch.as_deref(),
            wire.commit.as_deref(),
            wire.files_changed,
            wire.insertions,
            wire.deletions,
        )?;
        Ok(Self::try_new(
            summary,
            wire.source_event_id,
            wire.observed_at_ms,
            wire.revision,
        ))
    }
}

impl UsageMeasure {
    fn to_wire(&self) -> UsageMeasureWire {
        UsageMeasureWire {
            provider: self.provider.clone(),
            source: self.source.clone(),
            kind: self.kind,
            provenance: self.provenance,
            value: self.value,
            unit: self.unit.clone(),
            window: self.window,
            observed_at_ms: self.observed_at_ms,
            revision: self.revision,
        }
    }

    fn from_wire(wire: UsageMeasureWire) -> Result<Self, ObservationError> {
        Self::try_new(
            wire.provider,
            wire.source,
            wire.kind,
            wire.provenance,
            wire.value,
            wire.unit,
            wire.window,
            wire.observed_at_ms,
            wire.revision,
        )
    }
}

impl ProviderObservation {
    fn to_wire(&self) -> ProviderWire {
        ProviderWire {
            kind: self.kind.clone(),
            role: self.role.clone(),
            lifecycle: self.lifecycle,
            activity: self.activity,
        }
    }

    fn from_wire(wire: ProviderWire) -> Result<Self, ObservationError> {
        Self::try_new(wire.kind, wire.role, wire.lifecycle, wire.activity)
    }
}

fn decode_observation_wire(bytes: &[u8]) -> Result<ObservationWire, ObservationError> {
    validate_json_bounds(bytes)?;
    serde_json::from_slice(bytes).map_err(|_| ObservationError::ProhibitedContent)
}

fn encode_observation_wire(wire: &ObservationWire) -> Result<Vec<u8>, ObservationError> {
    let mut encoded = BoundedBuf::new(MAX_OBSERVATION_DOCUMENT_BYTES);
    serde_json::to_writer(&mut encoded, wire).map_err(|error| {
        if error.is_io() {
            ObservationError::BoundExceeded
        } else {
            ObservationError::ProhibitedContent
        }
    })?;
    encoded.into_inner()
}

struct BoundedBuf {
    buf: Vec<u8>,
    max: usize,
}

impl BoundedBuf {
    fn new(max: usize) -> Self {
        let mut buf = Vec::new();
        let _ = buf.try_reserve(max.min(MAX_CONNECT_PAGE_ENCODED_BYTES as usize));
        Self { buf, max }
    }

    fn into_inner(self) -> Result<Vec<u8>, ObservationError> {
        if self.buf.len() > self.max {
            return Err(ObservationError::BoundExceeded);
        }
        Ok(self.buf)
    }
}

impl Write for BoundedBuf {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.buf.len().saturating_add(data.len()) > self.max {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "bound"));
        }
        self.buf
            .try_reserve(data.len())
            .map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn validate_json_bounds(bytes: &[u8]) -> Result<(), ObservationError> {
    if bytes.len() > MAX_OBSERVATION_DOCUMENT_BYTES {
        return Err(ObservationError::BoundExceeded);
    }
    let mut index = 0usize;
    parse_json_value(bytes, &mut index, 1)?;
    skip_ws(bytes, &mut index);
    if index != bytes.len() {
        return Err(ObservationError::ProhibitedContent);
    }
    Ok(())
}

fn parse_json_value(bytes: &[u8], index: &mut usize, depth: usize) -> Result<(), ObservationError> {
    if depth > MAX_JSON_NESTING {
        return Err(ObservationError::BoundExceeded);
    }
    skip_ws(bytes, index);
    match bytes.get(*index).copied() {
        Some(b'{') => parse_json_object(bytes, index, depth),
        Some(b'[') => parse_json_array(bytes, index, depth),
        Some(b'"') => parse_json_string(bytes, index).map(|_| ()),
        Some(b't') => parse_literal(bytes, index, b"true"),
        Some(b'f') => parse_literal(bytes, index, b"false"),
        Some(b'n') => parse_literal(bytes, index, b"null"),
        Some(b'-' | b'0'..=b'9') => parse_json_number(bytes, index),
        _ => Err(ObservationError::ProhibitedContent),
    }
}

fn parse_json_object(
    bytes: &[u8],
    index: &mut usize,
    depth: usize,
) -> Result<(), ObservationError> {
    *index += 1;
    skip_ws(bytes, index);
    if bytes.get(*index) == Some(&b'}') {
        *index += 1;
        return Ok(());
    }
    let mut keys = Vec::new();
    loop {
        if keys.len() >= MAX_JSON_MAP_KEYS {
            return Err(ObservationError::BoundExceeded);
        }
        let key = parse_json_string(bytes, index)?;
        if keys.iter().any(|seen| seen == &key) {
            return Err(ObservationError::ProhibitedContent);
        }
        keys.push(key);
        skip_ws(bytes, index);
        if bytes.get(*index) != Some(&b':') {
            return Err(ObservationError::ProhibitedContent);
        }
        *index += 1;
        parse_json_value(bytes, index, depth + 1)?;
        skip_ws(bytes, index);
        match bytes.get(*index) {
            Some(b',') => {
                *index += 1;
                skip_ws(bytes, index);
            }
            Some(b'}') => {
                *index += 1;
                return Ok(());
            }
            _ => return Err(ObservationError::ProhibitedContent),
        }
    }
}

fn parse_json_array(bytes: &[u8], index: &mut usize, depth: usize) -> Result<(), ObservationError> {
    *index += 1;
    skip_ws(bytes, index);
    if bytes.get(*index) == Some(&b']') {
        *index += 1;
        return Ok(());
    }
    let mut count = 0usize;
    loop {
        count = count
            .checked_add(1)
            .ok_or(ObservationError::BoundExceeded)?;
        if count > MAX_JSON_ARRAY_LEN {
            return Err(ObservationError::BoundExceeded);
        }
        parse_json_value(bytes, index, depth + 1)?;
        skip_ws(bytes, index);
        match bytes.get(*index) {
            Some(b',') => {
                *index += 1;
                skip_ws(bytes, index);
            }
            Some(b']') => {
                *index += 1;
                return Ok(());
            }
            _ => return Err(ObservationError::ProhibitedContent),
        }
    }
}

fn parse_json_string(bytes: &[u8], index: &mut usize) -> Result<String, ObservationError> {
    skip_ws(bytes, index);
    if bytes.get(*index) != Some(&b'"') {
        return Err(ObservationError::ProhibitedContent);
    }
    *index += 1;
    let start = *index;
    while *index < bytes.len() {
        match bytes[*index] {
            b'"' => {
                let raw = &bytes[start..*index];
                if raw.len() > MAX_JSON_STRING_BYTES {
                    return Err(ObservationError::BoundExceeded);
                }
                *index += 1;
                return String::from_utf8(raw.to_vec())
                    .map_err(|_| ObservationError::ProhibitedContent);
            }
            b'\\' => {
                *index += 1;
                if *index >= bytes.len() {
                    return Err(ObservationError::ProhibitedContent);
                }
                *index += 1;
            }
            byte if byte < 0x20 => return Err(ObservationError::ProhibitedContent),
            _ => *index += 1,
        }
    }
    Err(ObservationError::ProhibitedContent)
}

fn parse_json_number(bytes: &[u8], index: &mut usize) -> Result<(), ObservationError> {
    if bytes.get(*index) == Some(&b'-') {
        *index += 1;
    }
    let start = *index;
    while matches!(bytes.get(*index), Some(b'0'..=b'9')) {
        *index += 1;
    }
    if *index == start {
        return Err(ObservationError::ProhibitedContent);
    }
    Ok(())
}

fn parse_literal(bytes: &[u8], index: &mut usize, literal: &[u8]) -> Result<(), ObservationError> {
    if bytes
        .get(*index..)
        .is_some_and(|slice| slice.starts_with(literal))
    {
        *index += literal.len();
        Ok(())
    } else {
        Err(ObservationError::ProhibitedContent)
    }
}

fn skip_ws(bytes: &[u8], index: &mut usize) {
    while matches!(bytes.get(*index), Some(b' ' | b'\n' | b'\r' | b'\t')) {
        *index += 1;
    }
}

impl<'de> Deserialize<'de> for ObservationWire {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Clone, Copy, Deserialize)]
        #[serde(field_identifier, rename_all = "snake_case")]
        enum Field {
            ObservationId,
            TaskId,
            Lifecycle,
            Attention,
            Connectivity,
            Primary,
            Specialists,
            Git,
            Usage,
            HumanMessageCount,
            HumanTurnCount,
            ActiveSession,
            SourceEventId,
            ObservedAtMs,
            SourceAtMs,
            FactsRevision,
            Completeness,
            Confidence,
            PolicyRevision,
            SchemaRevision,
        }

        struct ObservationVisitor;

        impl<'de> Visitor<'de> for ObservationVisitor {
            type Value = ObservationWire;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("observation object")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut seen = [false; 20];
                let mut observation_id = None;
                let mut task_id = None;
                let mut lifecycle = None;
                let mut attention = None;
                let mut connectivity = None;
                let mut primary = None;
                let mut specialists = None;
                let mut git = None;
                let mut usage = None;
                let mut human_message_count = None;
                let mut human_turn_count = None;
                let mut active_session = None;
                let mut source_event_id = None;
                let mut observed_at_ms = None;
                let mut source_at_ms = None;
                let mut facts_revision = None;
                let mut completeness = None;
                let mut confidence = None;
                let mut policy_revision = None;
                let mut schema_revision = None;
                let mut count = 0usize;
                while let Some(key) = map.next_key::<Field>()? {
                    count += 1;
                    if count > MAX_JSON_MAP_KEYS {
                        return Err(de::Error::custom("map exceeds bound"));
                    }
                    let index = key as usize;
                    if seen[index] {
                        return Err(de::Error::custom("duplicate observation field"));
                    }
                    seen[index] = true;
                    match key {
                        Field::ObservationId => observation_id = Some(map.next_value()?),
                        Field::TaskId => task_id = Some(map.next_value()?),
                        Field::Lifecycle => lifecycle = Some(map.next_value()?),
                        Field::Attention => attention = Some(map.next_value()?),
                        Field::Connectivity => connectivity = Some(map.next_value()?),
                        Field::Primary => primary = Some(map.next_value()?),
                        Field::Specialists => specialists = Some(map.next_value()?),
                        Field::Git => git = Some(map.next_value()?),
                        Field::Usage => usage = Some(map.next_value()?),
                        Field::HumanMessageCount => human_message_count = Some(map.next_value()?),
                        Field::HumanTurnCount => human_turn_count = Some(map.next_value()?),
                        Field::ActiveSession => active_session = Some(map.next_value()?),
                        Field::SourceEventId => source_event_id = Some(map.next_value()?),
                        Field::ObservedAtMs => observed_at_ms = Some(map.next_value()?),
                        Field::SourceAtMs => source_at_ms = Some(map.next_value()?),
                        Field::FactsRevision => facts_revision = Some(map.next_value()?),
                        Field::Completeness => completeness = Some(map.next_value()?),
                        Field::Confidence => confidence = Some(map.next_value()?),
                        Field::PolicyRevision => policy_revision = Some(map.next_value()?),
                        Field::SchemaRevision => schema_revision = Some(map.next_value()?),
                    }
                }
                Ok(ObservationWire {
                    observation_id: observation_id
                        .ok_or_else(|| de::Error::missing_field("observation_id"))?,
                    task_id: task_id.ok_or_else(|| de::Error::missing_field("task_id"))?,
                    lifecycle: lifecycle.ok_or_else(|| de::Error::missing_field("lifecycle"))?,
                    attention: attention.ok_or_else(|| de::Error::missing_field("attention"))?,
                    connectivity: connectivity
                        .ok_or_else(|| de::Error::missing_field("connectivity"))?,
                    primary: primary.flatten(),
                    specialists: specialists.unwrap_or_default(),
                    git: git.flatten(),
                    usage: usage.unwrap_or_default(),
                    human_message_count: human_message_count
                        .ok_or_else(|| de::Error::missing_field("human_message_count"))?,
                    human_turn_count: human_turn_count
                        .ok_or_else(|| de::Error::missing_field("human_turn_count"))?,
                    active_session: active_session.flatten(),
                    source_event_id: source_event_id
                        .ok_or_else(|| de::Error::missing_field("source_event_id"))?,
                    observed_at_ms: observed_at_ms
                        .ok_or_else(|| de::Error::missing_field("observed_at_ms"))?,
                    source_at_ms: source_at_ms
                        .ok_or_else(|| de::Error::missing_field("source_at_ms"))?,
                    facts_revision: facts_revision
                        .ok_or_else(|| de::Error::missing_field("facts_revision"))?,
                    completeness: completeness
                        .ok_or_else(|| de::Error::missing_field("completeness"))?,
                    confidence: confidence.ok_or_else(|| de::Error::missing_field("confidence"))?,
                    policy_revision: policy_revision
                        .ok_or_else(|| de::Error::missing_field("policy_revision"))?,
                    schema_revision: schema_revision
                        .ok_or_else(|| de::Error::missing_field("schema_revision"))?,
                })
            }
        }

        deserializer.deserialize_map(ObservationVisitor)
    }
}

impl Serialize for ObservationWire {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(20))?;
        map.serialize_entry("observation_id", &self.observation_id)?;
        map.serialize_entry("task_id", &self.task_id)?;
        map.serialize_entry("lifecycle", &self.lifecycle)?;
        map.serialize_entry("attention", &self.attention)?;
        map.serialize_entry("connectivity", &self.connectivity)?;
        map.serialize_entry("primary", &self.primary)?;
        map.serialize_entry("specialists", &self.specialists)?;
        map.serialize_entry("git", &self.git)?;
        map.serialize_entry("usage", &self.usage)?;
        map.serialize_entry("human_message_count", &self.human_message_count)?;
        map.serialize_entry("human_turn_count", &self.human_turn_count)?;
        map.serialize_entry("active_session", &self.active_session)?;
        map.serialize_entry("source_event_id", &self.source_event_id)?;
        map.serialize_entry("observed_at_ms", &self.observed_at_ms)?;
        map.serialize_entry("source_at_ms", &self.source_at_ms)?;
        map.serialize_entry("facts_revision", &self.facts_revision)?;
        map.serialize_entry("completeness", &self.completeness)?;
        map.serialize_entry("confidence", &self.confidence)?;
        map.serialize_entry("policy_revision", &self.policy_revision)?;
        map.serialize_entry("schema_revision", &self.schema_revision)?;
        map.end()
    }
}

impl<'de> Deserialize<'de> for ProviderWire {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Clone, Copy, Deserialize)]
        #[serde(field_identifier, rename_all = "snake_case")]
        enum Field {
            Kind,
            Role,
            Lifecycle,
            Activity,
        }
        struct ProviderVisitor;
        impl<'de> Visitor<'de> for ProviderVisitor {
            type Value = ProviderWire;
            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("provider object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut seen = [false; 4];
                let mut kind = None;
                let mut role = None;
                let mut lifecycle = None;
                let mut activity = None;
                while let Some(key) = map.next_key::<Field>()? {
                    let index = key as usize;
                    if seen[index] {
                        return Err(de::Error::custom("duplicate provider field"));
                    }
                    seen[index] = true;
                    match key {
                        Field::Kind => kind = Some(bounded_string(map.next_value()?)?),
                        Field::Role => role = Some(map.next_value()?),
                        Field::Lifecycle => lifecycle = Some(map.next_value()?),
                        Field::Activity => activity = Some(map.next_value()?),
                    }
                }
                Ok(ProviderWire {
                    kind: kind.ok_or_else(|| de::Error::missing_field("kind"))?,
                    role: role.ok_or_else(|| de::Error::missing_field("role"))?,
                    lifecycle: lifecycle.ok_or_else(|| de::Error::missing_field("lifecycle"))?,
                    activity: activity.ok_or_else(|| de::Error::missing_field("activity"))?,
                })
            }
        }
        deserializer.deserialize_map(ProviderVisitor)
    }
}

impl Serialize for ProviderWire {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(4))?;
        map.serialize_entry("kind", &self.kind)?;
        map.serialize_entry("role", &self.role)?;
        map.serialize_entry("lifecycle", &self.lifecycle)?;
        map.serialize_entry("activity", &self.activity)?;
        map.end()
    }
}

impl<'de> Deserialize<'de> for UsageWire {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Clone, Copy, Deserialize)]
        #[serde(field_identifier, rename_all = "snake_case")]
        enum Field {
            Tokens,
            QuotaRemaining,
            QuotaReset,
            QuotedCost,
            LocalTokenEstimate,
            LocalCostEstimate,
        }
        struct UsageVisitor;
        impl<'de> Visitor<'de> for UsageVisitor {
            type Value = UsageWire;
            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("usage object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut seen = [false; 6];
                let mut usage = UsageWire::default();
                while let Some(key) = map.next_key::<Field>()? {
                    let index = key as usize;
                    if seen[index] {
                        return Err(de::Error::custom("duplicate usage field"));
                    }
                    seen[index] = true;
                    match key {
                        Field::Tokens => usage.tokens = map.next_value()?,
                        Field::QuotaRemaining => usage.quota_remaining = map.next_value()?,
                        Field::QuotaReset => usage.quota_reset = map.next_value()?,
                        Field::QuotedCost => usage.quoted_cost = map.next_value()?,
                        Field::LocalTokenEstimate => {
                            usage.local_token_estimate = map.next_value()?
                        }
                        Field::LocalCostEstimate => usage.local_cost_estimate = map.next_value()?,
                    }
                }
                Ok(usage)
            }
        }
        deserializer.deserialize_map(UsageVisitor)
    }
}

impl Serialize for UsageWire {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(6))?;
        map.serialize_entry("tokens", &self.tokens)?;
        map.serialize_entry("quota_remaining", &self.quota_remaining)?;
        map.serialize_entry("quota_reset", &self.quota_reset)?;
        map.serialize_entry("quoted_cost", &self.quoted_cost)?;
        map.serialize_entry("local_token_estimate", &self.local_token_estimate)?;
        map.serialize_entry("local_cost_estimate", &self.local_cost_estimate)?;
        map.end()
    }
}

impl<'de> Deserialize<'de> for UsageMeasureWire {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Clone, Copy, Deserialize)]
        #[serde(field_identifier, rename_all = "snake_case")]
        enum Field {
            Provider,
            Source,
            Kind,
            Provenance,
            Value,
            Unit,
            Window,
            ObservedAtMs,
            Revision,
        }
        struct MeasureVisitor;
        impl<'de> Visitor<'de> for MeasureVisitor {
            type Value = UsageMeasureWire;
            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("usage measure")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut seen = [false; 9];
                let mut provider = None;
                let mut source = None;
                let mut kind = None;
                let mut provenance = None;
                let mut value = None;
                let mut unit = None;
                let mut window = None;
                let mut observed_at_ms = None;
                let mut revision = None;
                while let Some(key) = map.next_key::<Field>()? {
                    let index = key as usize;
                    if seen[index] {
                        return Err(de::Error::custom("duplicate measure field"));
                    }
                    seen[index] = true;
                    match key {
                        Field::Provider => provider = Some(bounded_string(map.next_value()?)?),
                        Field::Source => source = Some(bounded_string(map.next_value()?)?),
                        Field::Kind => kind = Some(map.next_value()?),
                        Field::Provenance => provenance = Some(map.next_value()?),
                        Field::Value => value = Some(map.next_value()?),
                        Field::Unit => unit = Some(bounded_string(map.next_value()?)?),
                        Field::Window => window = Some(map.next_value()?),
                        Field::ObservedAtMs => observed_at_ms = Some(map.next_value()?),
                        Field::Revision => revision = Some(map.next_value()?),
                    }
                }
                Ok(UsageMeasureWire {
                    provider: provider.ok_or_else(|| de::Error::missing_field("provider"))?,
                    source: source.ok_or_else(|| de::Error::missing_field("source"))?,
                    kind: kind.ok_or_else(|| de::Error::missing_field("kind"))?,
                    provenance: provenance.ok_or_else(|| de::Error::missing_field("provenance"))?,
                    value: value.flatten(),
                    unit: unit.ok_or_else(|| de::Error::missing_field("unit"))?,
                    window: window.flatten(),
                    observed_at_ms: observed_at_ms
                        .ok_or_else(|| de::Error::missing_field("observed_at_ms"))?,
                    revision: revision.ok_or_else(|| de::Error::missing_field("revision"))?,
                })
            }
        }
        deserializer.deserialize_map(MeasureVisitor)
    }
}

impl Serialize for UsageMeasureWire {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(9))?;
        map.serialize_entry("provider", &self.provider)?;
        map.serialize_entry("source", &self.source)?;
        map.serialize_entry("kind", &self.kind)?;
        map.serialize_entry("provenance", &self.provenance)?;
        map.serialize_entry("value", &self.value)?;
        map.serialize_entry("unit", &self.unit)?;
        map.serialize_entry("window", &self.window)?;
        map.serialize_entry("observed_at_ms", &self.observed_at_ms)?;
        map.serialize_entry("revision", &self.revision)?;
        map.end()
    }
}

impl<'de> Deserialize<'de> for GitWire {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Clone, Copy, Deserialize)]
        #[serde(field_identifier, rename_all = "snake_case")]
        enum Field {
            Branch,
            Commit,
            FilesChanged,
            Insertions,
            Deletions,
            SourceEventId,
            ObservedAtMs,
            Revision,
        }
        struct GitVisitor;
        impl<'de> Visitor<'de> for GitVisitor {
            type Value = GitWire;
            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("git summary")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut seen = [false; 8];
                let mut branch = None;
                let mut commit = None;
                let mut files_changed = None;
                let mut insertions = None;
                let mut deletions = None;
                let mut source_event_id = None;
                let mut observed_at_ms = None;
                let mut revision = None;
                while let Some(key) = map.next_key::<Field>()? {
                    let index = key as usize;
                    if seen[index] {
                        return Err(de::Error::custom("duplicate git field"));
                    }
                    seen[index] = true;
                    match key {
                        Field::Branch => branch = Some(map.next_value()?),
                        Field::Commit => commit = Some(map.next_value()?),
                        Field::FilesChanged => files_changed = Some(map.next_value()?),
                        Field::Insertions => insertions = Some(map.next_value()?),
                        Field::Deletions => deletions = Some(map.next_value()?),
                        Field::SourceEventId => source_event_id = Some(map.next_value()?),
                        Field::ObservedAtMs => observed_at_ms = Some(map.next_value()?),
                        Field::Revision => revision = Some(map.next_value()?),
                    }
                }
                Ok(GitWire {
                    branch: branch.flatten(),
                    commit: commit.flatten(),
                    files_changed: files_changed
                        .ok_or_else(|| de::Error::missing_field("files_changed"))?,
                    insertions: insertions.ok_or_else(|| de::Error::missing_field("insertions"))?,
                    deletions: deletions.ok_or_else(|| de::Error::missing_field("deletions"))?,
                    source_event_id: source_event_id
                        .ok_or_else(|| de::Error::missing_field("source_event_id"))?,
                    observed_at_ms: observed_at_ms
                        .ok_or_else(|| de::Error::missing_field("observed_at_ms"))?,
                    revision: revision.ok_or_else(|| de::Error::missing_field("revision"))?,
                })
            }
        }
        deserializer.deserialize_map(GitVisitor)
    }
}

impl Serialize for GitWire {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(8))?;
        map.serialize_entry("branch", &self.branch)?;
        map.serialize_entry("commit", &self.commit)?;
        map.serialize_entry("files_changed", &self.files_changed)?;
        map.serialize_entry("insertions", &self.insertions)?;
        map.serialize_entry("deletions", &self.deletions)?;
        map.serialize_entry("source_event_id", &self.source_event_id)?;
        map.serialize_entry("observed_at_ms", &self.observed_at_ms)?;
        map.serialize_entry("revision", &self.revision)?;
        map.end()
    }
}

impl<'de> Deserialize<'de> for IntervalWire {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Clone, Copy, Deserialize)]
        #[serde(field_identifier, rename_all = "snake_case")]
        enum Field {
            StartedAtMs,
            EndedAtMs,
        }
        struct IntervalVisitor;
        impl<'de> Visitor<'de> for IntervalVisitor {
            type Value = IntervalWire;
            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("interval")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut seen = [false; 2];
                let mut started_at_ms = None;
                let mut ended_at_ms = None;
                while let Some(key) = map.next_key::<Field>()? {
                    let index = key as usize;
                    if seen[index] {
                        return Err(de::Error::custom("duplicate interval field"));
                    }
                    seen[index] = true;
                    match key {
                        Field::StartedAtMs => started_at_ms = Some(map.next_value()?),
                        Field::EndedAtMs => ended_at_ms = Some(map.next_value()?),
                    }
                }
                Ok(IntervalWire {
                    started_at_ms: started_at_ms
                        .ok_or_else(|| de::Error::missing_field("started_at_ms"))?,
                    ended_at_ms: ended_at_ms
                        .ok_or_else(|| de::Error::missing_field("ended_at_ms"))?,
                })
            }
        }
        deserializer.deserialize_map(IntervalVisitor)
    }
}

impl Serialize for IntervalWire {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("started_at_ms", &self.started_at_ms)?;
        map.serialize_entry("ended_at_ms", &self.ended_at_ms)?;
        map.end()
    }
}

fn bounded_string<E: de::Error>(value: String) -> Result<String, E> {
    if value.len() > MAX_JSON_STRING_BYTES {
        Err(E::custom("string exceeds bound"))
    } else {
        Ok(value)
    }
}
