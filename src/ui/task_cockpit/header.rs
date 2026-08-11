//! Bounded, presentation-only contracts for the task header and top bar.
//!
//! This module deliberately stops at immutable projections.  A canonical
//! shell owns the host connection, observation tick, GPUI tree, and action
//! dispatch.  Nothing here drains a channel, starts a client, or performs
//! synchronous host work from paint/input code.

use std::collections::{BTreeMap, BTreeSet, BinaryHeap, VecDeque};
use std::fmt;
use std::path::PathBuf;

use serde::de::{self, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::client::action::{ActionDescriptor, ActionRisk, TaskRenameArguments, ACTION_TASK_SHOW};
use crate::client::ClientModel;
use crate::diagnostics::runner::redact_secrets;
use crate::domain::agent::AgentSessionLifecycle;
use crate::domain::id::{AgentSessionId, ProjectId, ResourceId, TaskId};
use crate::domain::snapshot::TaskSnapshot;
use crate::domain::task::{VisibleTaskStatus, WorkspaceRef};
use crate::ui::components::{AccessibleRole, ActionRequest};

/// Provider quota values are fresh for at most one hour.
pub const PROVIDER_QUOTA_MAX_AGE_MS: i64 = 60 * 60 * 1_000;
/// Every top-bar observation uses the same freshness boundary.
pub const MAX_OBSERVATION_AGE_MS: i64 = PROVIDER_QUOTA_MAX_AGE_MS;
/// Retain enough specialist identity to support large tasks without an
/// unbounded projection.  Rendering is a smaller keyset window.
pub const MAX_HEADER_SPECIALISTS: usize = 5_000;
pub const MAX_SPECIALIST_VIRTUAL_WINDOW: usize = 128;
pub const MAX_PENDING_HEADER_ACTIONS: usize = 64;
pub const MAX_HEADER_HIGH_WATER_ENTRIES: usize = 4_096;
pub const HEADER_HIGH_WATER_TTL_MS: i64 = PROVIDER_QUOTA_MAX_AGE_MS;
pub const MAX_TOP_BAR_QUOTAS: usize = 8;
pub const MAX_TOP_BAR_QUOTA_CACHE: usize = 64;
pub const MAX_PROVIDER_SESSION_ID_BYTES: usize = 4 * 1_024;

const MAX_LABEL_SCALARS: usize = 160;
const MAX_PROVIDER_SCALARS: usize = 64;
const MAX_ACCESSIBLE_SCALARS: usize = 512;
const MAX_SOURCE_SCALARS: usize = 128;
const MAX_DETAIL_SCALARS: usize = 128;
const MAX_WORKSPACE_PATH_SCALARS: usize = 512;
const MAX_TITLE_SINGLE_LINE_SCALARS: usize = 96;
const MAX_TITLE_LINE_SCALARS: usize = 28;

/// A bounded opaque reference for provider conversation identity.
///
/// The raw provider string is accepted only as a borrowed input, checked for
/// size, and immediately reduced to a fixed digest.  The UI projection never
/// stores, serializes, or formats the provider's raw session id.
#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OpaqueProviderSessionRef([u8; 16]);

pub type ProviderSessionRef = OpaqueProviderSessionRef;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderSessionRefError {
    Empty,
    TooLarge {
        actual_bytes: usize,
        max_bytes: usize,
    },
}

impl fmt::Display for ProviderSessionRefError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("provider session reference must not be empty"),
            Self::TooLarge {
                actual_bytes,
                max_bytes,
            } => write!(
                formatter,
                "provider session reference is {actual_bytes} bytes; maximum is {max_bytes}"
            ),
        }
    }
}

impl std::error::Error for ProviderSessionRefError {}

impl OpaqueProviderSessionRef {
    pub fn from_raw(raw: &str) -> Option<Self> {
        Self::try_from_raw(raw).ok()
    }

    pub fn try_from_raw(raw: &str) -> Result<Self, ProviderSessionRefError> {
        if raw.is_empty() {
            return Err(ProviderSessionRefError::Empty);
        }
        if raw.len() > MAX_PROVIDER_SESSION_ID_BYTES {
            return Err(ProviderSessionRefError::TooLarge {
                actual_bytes: raw.len(),
                max_bytes: MAX_PROVIDER_SESSION_ID_BYTES,
            });
        }
        let digest = Sha256::digest(raw.as_bytes());
        let mut bytes = [0_u8; 16];
        bytes.copy_from_slice(&digest[..16]);
        Ok(Self(bytes))
    }

    pub const fn from_digest(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub const fn as_digest(self) -> [u8; 16] {
        self.0
    }
}

impl fmt::Debug for OpaqueProviderSessionRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("provider-session-ref[opaque]")
    }
}

impl fmt::Display for OpaqueProviderSessionRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("provider-session-ref[opaque]")
    }
}

/// Copy at most `max_scalars` sanitized scalars before invoking the bounded
/// secret redactor.  In particular, this does not first clone or redact an
/// unbounded provider/path/input string.
pub fn presentation_text(value: &str, max_scalars: usize) -> String {
    if max_scalars == 0 {
        return String::new();
    }
    let mut bounded = String::with_capacity(value.len().min(max_scalars.saturating_mul(4)));
    for character in value.chars().take(max_scalars) {
        if character.is_control() && character != '\n' && character != '\t' {
            bounded.push(' ');
        } else {
            bounded.push(character);
        }
    }
    truncate_scalars(&redact_secrets(&bounded), max_scalars)
}

fn truncate_scalars(value: &str, max_scalars: usize) -> String {
    value.chars().take(max_scalars).collect()
}

fn bounded_label(value: &str, max_scalars: usize) -> String {
    let label = presentation_text(value, max_scalars);
    if label.trim().is_empty() {
        "Unavailable".to_string()
    } else {
        label
    }
}

// -------------------------------------------------------------------------
// Shared high-water/tombstone policy

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum AgentResourceField {
    Cpu,
    Memory,
    Network,
}

/// Every independently replayable header field uses this one key space and
/// one monotonic policy.  Session identity is opaque even inside quota keys.
#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
pub enum HeaderFieldKey {
    Host {
        source_id: String,
    },
    Connect {
        source_id: String,
    },
    Update {
        source_id: String,
    },
    Resource {
        resource_id: ResourceId,
        field: AgentResourceField,
    },
    Task(TaskId),
    Agent {
        task_id: TaskId,
        agent_id: AgentSessionId,
    },
    AgentProvider {
        task_id: TaskId,
        agent_id: AgentSessionId,
    },
    AgentResource {
        task_id: TaskId,
        agent_id: AgentSessionId,
        field: AgentResourceField,
    },
    HostResource {
        field: AgentResourceField,
    },
    Quota {
        provider: String,
        provider_session_ref: OpaqueProviderSessionRef,
    },
    Remote {
        source_id: String,
    },
}

impl fmt::Debug for HeaderFieldKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Host { .. } => formatter.write_str("Host([redacted])"),
            Self::Connect { .. } => formatter.write_str("Connect([redacted])"),
            Self::Update { .. } => formatter.write_str("Update([redacted])"),
            Self::Resource { field, .. } => formatter
                .debug_struct("Resource")
                .field("field", field)
                .finish(),
            Self::Task(_) => formatter.write_str("Task([redacted])"),
            Self::Agent { .. } => formatter.write_str("Agent([redacted])"),
            Self::AgentProvider { .. } => formatter.write_str("AgentProvider([redacted])"),
            Self::AgentResource { field, .. } => formatter
                .debug_struct("AgentResource")
                .field("field", field)
                .finish(),
            Self::HostResource { field } => formatter
                .debug_struct("HostResource")
                .field("field", field)
                .finish(),
            Self::Quota { .. } => formatter.write_str("Quota([redacted])"),
            Self::Remote { .. } => formatter.write_str("Remote([redacted])"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HeaderObservation {
    pub key: HeaderFieldKey,
    pub generation: u64,
    pub revision: u64,
    pub observed_at_ms: i64,
    pub fingerprint: u64,
    pub removed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HighWaterDecision {
    Accepted,
    IgnoredStale,
    RejectedConflict,
    /// Incremental state cannot be accepted after a detailed mark expired or
    /// the bounded ledger reached capacity.  The caller must attach a newer
    /// authoritative full-resync epoch.
    NeedsFullResync,
    RejectedCapacity,
    RejectedInvalid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HeaderWaterMark {
    generation: u64,
    revision: u64,
    observed_at_ms: i64,
    fingerprint: u64,
    removed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MonotonicFloor {
    generation: u64,
    revision: u64,
    fingerprint: u64,
    removed: bool,
    retirement_epoch: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RetirementFloor {
    generation: u64,
    revision: u64,
    retirement_epoch: u64,
}

/// Bounded detailed marks plus a bounded monotonic floor.  Expiring a
/// detailed mark never deletes its floor; future incremental observations for
/// that key are fail-closed until a newer authoritative full-resync epoch.
#[derive(Clone)]
pub struct HeaderHighWaterLedger {
    marks: BTreeMap<HeaderFieldKey, HeaderWaterMark>,
    floors: BTreeMap<HeaderFieldKey, MonotonicFloor>,
    /// One bounded fallback floor protects keys retired when a full resync
    /// replaces more detailed entries than the configured capacity allows.
    retirement_floor: Option<RetirementFloor>,
    capacity: usize,
    ttl_ms: i64,
    last_full_resync_epoch: u64,
    needs_full_resync: bool,
}

impl HeaderHighWaterLedger {
    pub fn new(capacity: usize, ttl_ms: i64) -> Self {
        Self {
            marks: BTreeMap::new(),
            floors: BTreeMap::new(),
            retirement_floor: None,
            capacity: capacity.clamp(1, MAX_HEADER_HIGH_WATER_ENTRIES),
            ttl_ms: ttl_ms.max(1),
            last_full_resync_epoch: 0,
            needs_full_resync: false,
        }
    }

    pub fn observe(
        &mut self,
        key: HeaderFieldKey,
        generation: u64,
        revision: u64,
        observed_at_ms: i64,
        fingerprint: u64,
        removed: bool,
    ) -> HighWaterDecision {
        if generation == 0 || observed_at_ms < 0 {
            return HighWaterDecision::RejectedInvalid;
        }
        let Some(key) = bounded_header_key(&key) else {
            return HighWaterDecision::RejectedInvalid;
        };
        self.expire(observed_at_ms);

        if let Some(floor) = self.floors.get(&key).copied() {
            match compare_stamp(generation, revision, floor.generation, floor.revision) {
                std::cmp::Ordering::Less => return HighWaterDecision::IgnoredStale,
                std::cmp::Ordering::Equal => {
                    return if floor.fingerprint == fingerprint && floor.removed == removed {
                        HighWaterDecision::IgnoredStale
                    } else {
                        HighWaterDecision::RejectedConflict
                    };
                }
                std::cmp::Ordering::Greater => {}
            }
            // A floor without a detailed mark is intentionally not enough to
            // admit a delta: the bounded projection may have missed a change.
            if !self.marks.contains_key(&key) || self.needs_full_resync {
                self.needs_full_resync = true;
                return HighWaterDecision::NeedsFullResync;
            }
        } else {
            if let Some(retirement_floor) = self.retirement_floor {
                match compare_stamp(
                    generation,
                    revision,
                    retirement_floor.generation,
                    retirement_floor.revision,
                ) {
                    std::cmp::Ordering::Less | std::cmp::Ordering::Equal => {
                        return HighWaterDecision::IgnoredStale;
                    }
                    std::cmp::Ordering::Greater => {
                        self.needs_full_resync = true;
                        return HighWaterDecision::NeedsFullResync;
                    }
                }
            }
            if self.needs_full_resync {
                return HighWaterDecision::NeedsFullResync;
            }
        }

        if self.marks.len() >= self.capacity && !self.marks.contains_key(&key) {
            self.needs_full_resync = true;
            return HighWaterDecision::NeedsFullResync;
        }
        if self.floors.len() >= self.capacity && !self.floors.contains_key(&key) {
            self.needs_full_resync = true;
            return HighWaterDecision::NeedsFullResync;
        }

        if let Some(mark) = self.marks.get(&key).copied() {
            match compare_stamp(generation, revision, mark.generation, mark.revision) {
                std::cmp::Ordering::Less => return HighWaterDecision::IgnoredStale,
                std::cmp::Ordering::Equal => {
                    return if mark.fingerprint == fingerprint && mark.removed == removed {
                        HighWaterDecision::IgnoredStale
                    } else {
                        HighWaterDecision::RejectedConflict
                    };
                }
                std::cmp::Ordering::Greater => {}
            }
        }

        let mark = HeaderWaterMark {
            generation,
            revision,
            observed_at_ms,
            fingerprint,
            removed,
        };
        self.marks.insert(key.clone(), mark);
        self.floors.insert(
            key,
            MonotonicFloor {
                generation,
                revision,
                fingerprint,
                removed,
                retirement_epoch: self.last_full_resync_epoch,
            },
        );
        HighWaterDecision::Accepted
    }

    /// Apply one bounded authoritative snapshot.  The epoch must advance, the
    /// snapshot must fit the detailed bound, and floors still reject equal
    /// revision payload conflicts or older generations.
    pub fn apply_full_resync<I>(
        &mut self,
        full_resync_epoch: u64,
        generation: u64,
        observations: I,
    ) -> HighWaterDecision
    where
        I: IntoIterator<Item = HeaderObservation>,
    {
        if full_resync_epoch == 0
            || generation == 0
            || full_resync_epoch <= self.last_full_resync_epoch
        {
            return HighWaterDecision::RejectedInvalid;
        }
        let mut bounded = Vec::with_capacity(self.capacity.min(64));
        for mut observation in observations {
            if bounded.len() >= self.capacity {
                self.needs_full_resync = true;
                return HighWaterDecision::RejectedCapacity;
            }
            let Some(key) = bounded_header_key(&observation.key) else {
                return HighWaterDecision::RejectedInvalid;
            };
            observation.key = key;
            if observation.generation != generation || observation.observed_at_ms < 0 {
                return HighWaterDecision::RejectedInvalid;
            }
            bounded.push(observation);
        }

        let incoming_keys: BTreeSet<_> =
            bounded.iter().map(|observation| &observation.key).collect();
        let mut candidate = self.clone();
        let prior_floors = candidate.floors.clone();
        for (key, floor) in &prior_floors {
            if !incoming_keys.contains(key) {
                candidate.retire_floor(*floor, full_resync_epoch);
            }
        }
        candidate.marks.clear();
        candidate.floors.clear();
        candidate.last_full_resync_epoch = full_resync_epoch;
        candidate.needs_full_resync = false;

        for observation in bounded {
            if let Some(floor) = candidate.floors.get(&observation.key).copied() {
                match compare_stamp(
                    observation.generation,
                    observation.revision,
                    floor.generation,
                    floor.revision,
                ) {
                    std::cmp::Ordering::Less => continue,
                    std::cmp::Ordering::Equal => {
                        if floor.fingerprint != observation.fingerprint
                            || floor.removed != observation.removed
                        {
                            return HighWaterDecision::RejectedConflict;
                        }
                        continue;
                    }
                    std::cmp::Ordering::Greater => {}
                }
            } else if let Some(floor) = prior_floors.get(&observation.key).copied() {
                match compare_stamp(
                    observation.generation,
                    observation.revision,
                    floor.generation,
                    floor.revision,
                ) {
                    std::cmp::Ordering::Less => {
                        // Keep the prior floor even when the authoritative
                        // bundle itself is stale for this field.  Dropping
                        // it would turn a bounded resync into a revival hole.
                        candidate.floors.insert(observation.key, floor);
                        continue;
                    }
                    std::cmp::Ordering::Equal => {
                        if floor.fingerprint != observation.fingerprint
                            || floor.removed != observation.removed
                        {
                            return HighWaterDecision::RejectedConflict;
                        }
                    }
                    std::cmp::Ordering::Greater => {}
                }
            } else if let Some(retirement_floor) = candidate.retirement_floor {
                match compare_stamp(
                    observation.generation,
                    observation.revision,
                    retirement_floor.generation,
                    retirement_floor.revision,
                ) {
                    std::cmp::Ordering::Less | std::cmp::Ordering::Equal => {
                        return HighWaterDecision::IgnoredStale;
                    }
                    std::cmp::Ordering::Greater => {}
                }
            }
            if !candidate.floors.contains_key(&observation.key)
                && candidate.floors.len() >= candidate.capacity
            {
                candidate.needs_full_resync = true;
                return HighWaterDecision::RejectedCapacity;
            }

            candidate.floors.insert(
                observation.key.clone(),
                MonotonicFloor {
                    generation: observation.generation,
                    revision: observation.revision,
                    fingerprint: observation.fingerprint,
                    removed: observation.removed,
                    retirement_epoch: full_resync_epoch,
                },
            );
            candidate.marks.insert(
                observation.key,
                HeaderWaterMark {
                    generation: observation.generation,
                    revision: observation.revision,
                    observed_at_ms: observation.observed_at_ms,
                    fingerprint: observation.fingerprint,
                    removed: observation.removed,
                },
            );
        }
        *self = candidate;
        HighWaterDecision::Accepted
    }

    /// Explicitly age detailed values.  Floors and tombstones remain.
    pub fn expire(&mut self, now_ms: i64) {
        let ttl_ms = self.ttl_ms;
        let before = self.marks.len();
        self.marks
            .retain(|_, mark| now_ms.saturating_sub(mark.observed_at_ms) < ttl_ms);
        if self.marks.len() != before {
            self.needs_full_resync = true;
        }
    }

    pub fn len(&self) -> usize {
        self.marks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.marks.is_empty()
    }

    pub fn floor_len(&self) -> usize {
        self.floors.len()
    }

    pub fn tombstone_count(&self) -> usize {
        self.floors.values().filter(|floor| floor.removed).count()
    }

    pub fn contains_tombstone(&self, key: &HeaderFieldKey) -> bool {
        self.floors.get(key).is_some_and(|floor| floor.removed)
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn ttl_ms(&self) -> i64 {
        self.ttl_ms
    }

    pub fn last_full_resync_epoch(&self) -> u64 {
        self.last_full_resync_epoch
    }

    pub fn requires_full_resync(&self) -> bool {
        self.needs_full_resync
    }

    fn retire_floor(&mut self, floor: MonotonicFloor, retirement_epoch: u64) {
        let candidate = RetirementFloor {
            generation: floor.generation,
            revision: floor.revision,
            retirement_epoch,
        };
        let replace = self.retirement_floor.is_none_or(|current| {
            compare_stamp(
                candidate.generation,
                candidate.revision,
                current.generation,
                current.revision,
            ) == std::cmp::Ordering::Greater
        });
        if replace {
            self.retirement_floor = Some(candidate);
        }
    }
}

impl fmt::Debug for HeaderHighWaterLedger {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HeaderHighWaterLedger")
            .field("detailed_entries", &self.marks.len())
            .field("floor_entries", &self.floors.len())
            .field("tombstones", &self.tombstone_count())
            .field("capacity", &self.capacity)
            .field("ttl_ms", &self.ttl_ms)
            .field("last_full_resync_epoch", &self.last_full_resync_epoch)
            .field("has_retirement_floor", &self.retirement_floor.is_some())
            .field("requires_full_resync", &self.needs_full_resync)
            .finish()
    }
}

impl fmt::Display for HeaderHighWaterLedger {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("header-high-water-ledger[bounded]")
    }
}

fn compare_stamp(
    left_generation: u64,
    left_revision: u64,
    right_generation: u64,
    right_revision: u64,
) -> std::cmp::Ordering {
    left_generation
        .cmp(&right_generation)
        .then_with(|| left_revision.cmp(&right_revision))
}

fn bounded_key_text(value: &str, max_scalars: usize) -> Option<String> {
    if value.trim().is_empty() || value.chars().count() > max_scalars {
        return None;
    }
    Some(value.to_owned())
}

fn bounded_header_key(key: &HeaderFieldKey) -> Option<HeaderFieldKey> {
    Some(match key {
        HeaderFieldKey::Host { source_id } => HeaderFieldKey::Host {
            source_id: bounded_key_text(source_id, MAX_SOURCE_SCALARS)?,
        },
        HeaderFieldKey::Connect { source_id } => HeaderFieldKey::Connect {
            source_id: bounded_key_text(source_id, MAX_SOURCE_SCALARS)?,
        },
        HeaderFieldKey::Update { source_id } => HeaderFieldKey::Update {
            source_id: bounded_key_text(source_id, MAX_SOURCE_SCALARS)?,
        },
        HeaderFieldKey::Resource { resource_id, field } => HeaderFieldKey::Resource {
            resource_id: *resource_id,
            field: *field,
        },
        HeaderFieldKey::Task(task_id) => HeaderFieldKey::Task(*task_id),
        HeaderFieldKey::Agent { task_id, agent_id } => HeaderFieldKey::Agent {
            task_id: *task_id,
            agent_id: *agent_id,
        },
        HeaderFieldKey::AgentProvider { task_id, agent_id } => HeaderFieldKey::AgentProvider {
            task_id: *task_id,
            agent_id: *agent_id,
        },
        HeaderFieldKey::AgentResource {
            task_id,
            agent_id,
            field,
        } => HeaderFieldKey::AgentResource {
            task_id: *task_id,
            agent_id: *agent_id,
            field: *field,
        },
        HeaderFieldKey::HostResource { field } => HeaderFieldKey::HostResource { field: *field },
        HeaderFieldKey::Quota {
            provider,
            provider_session_ref,
        } => HeaderFieldKey::Quota {
            provider: bounded_key_text(provider, MAX_PROVIDER_SCALARS)?,
            provider_session_ref: *provider_session_ref,
        },
        HeaderFieldKey::Remote { source_id } => HeaderFieldKey::Remote {
            source_id: bounded_key_text(source_id, MAX_SOURCE_SCALARS)?,
        },
    })
}

// -------------------------------------------------------------------------
// Bounded specialist retention and keyset windows

/// A borrowed observation adapter.  It lets the canonical runtime pass
/// domain facts without cloning provider/session strings into the projection.
#[derive(Clone, Copy)]
pub struct AgentObservation<'a> {
    pub id: AgentSessionId,
    pub task_id: TaskId,
    pub label: &'a str,
    pub provider: &'a str,
    pub provider_session_id: Option<&'a str>,
    pub lifecycle: AgentSessionLifecycle,
    pub runtime_generation: u64,
    pub revision: u64,
    pub removed: bool,
}

impl fmt::Debug for AgentObservation<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentObservation")
            .field("id", &self.id)
            .field("task_id", &self.task_id)
            .field("label", &"[bounded]")
            .field("provider", &"[bounded]")
            .field(
                "provider_session_id",
                &self.provider_session_id.map(|_| "[redacted]"),
            )
            .field("lifecycle", &self.lifecycle)
            .field("runtime_generation", &self.runtime_generation)
            .field("revision", &self.revision)
            .field("removed", &self.removed)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentProjection {
    pub id: AgentSessionId,
    pub task_id: TaskId,
    pub label: String,
    pub provider: String,
    pub provider_session_ref: Option<OpaqueProviderSessionRef>,
    pub lifecycle: AgentSessionLifecycle,
    pub runtime_generation: u64,
    pub revision: u64,
}

impl AgentProjection {
    fn from_observation(observation: AgentObservation<'_>) -> Self {
        Self {
            id: observation.id,
            task_id: observation.task_id,
            label: bounded_label(observation.label, MAX_LABEL_SCALARS),
            provider: bounded_label(observation.provider, MAX_PROVIDER_SCALARS),
            provider_session_ref: observation
                .provider_session_id
                .and_then(|raw| OpaqueProviderSessionRef::try_from_raw(raw).ok()),
            lifecycle: observation.lifecycle,
            runtime_generation: observation.runtime_generation,
            revision: observation.revision,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpecialistWindow {
    /// The exclusive keyset anchor supplied by the caller.
    pub window_after_id: Option<AgentSessionId>,
    pub next_after_id: Option<AgentSessionId>,
    pub items: Vec<AgentProjection>,
    pub removed_ids: Vec<AgentSessionId>,
}

/// Retained specialist projection.  The scan stores at most 5,000 candidate
/// rows and sorts only stable `AgentSessionId` values; labels never determine
/// identity or page order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpecialistProjection {
    retained: Vec<AgentProjection>,
    removed_ids: Vec<AgentSessionId>,
    total_seen: usize,
    scanned: usize,
    removals_truncated: bool,
    overflowed: bool,
    source_available: bool,
    unique_count_exact: bool,
    conflicts_rejected: usize,
}

impl SpecialistProjection {
    pub fn from_observations(observations: &[AgentObservation<'_>]) -> Self {
        Self::from_iter(observations.iter().copied())
    }

    /// Scan a borrowed stream without first materializing all source rows.
    /// Only the bounded top-k candidates and bounded removal report remain in
    /// the returned projection.
    pub fn from_iter<'a, I>(observations: I) -> Self
    where
        I: IntoIterator<Item = AgentObservation<'a>>,
    {
        Self::from_iter_with_source(observations, true)
    }

    /// The source flag is explicit because an empty page and an unavailable
    /// page are different states.  A caller must not turn a transport failure
    /// into an apparently empty specialist list.
    pub fn from_iter_with_source<'a, I>(observations: I, source_available: bool) -> Self
    where
        I: IntoIterator<Item = AgentObservation<'a>>,
    {
        let mut candidates: BTreeMap<AgentSessionId, AgentProjection> = BTreeMap::new();
        let mut largest_ids = BinaryHeap::new();
        let mut removed = BTreeSet::new();
        let mut watermarks: BTreeMap<AgentSessionId, AgentWaterMark> = BTreeMap::new();
        let mut seen_ids = BTreeSet::new();
        let mut total_seen = 0_usize;
        let mut scanned = 0_usize;
        let mut removals_truncated = false;
        let mut overflowed = false;
        let mut source_available = source_available;
        let mut unique_count_exact = true;
        let mut conflicts_rejected = 0_usize;
        let mut last_id = None;
        let mut ordered = true;

        for observation in observations {
            scanned = scanned.saturating_add(1);
            let previous_id = last_id;
            if previous_id.is_some_and(|last| observation.id < last) {
                ordered = false;
            }
            last_id = Some(observation.id);

            if previous_id == Some(observation.id) || seen_ids.contains(&observation.id) {
                // The bounded set covers the IDs that can affect the retained
                // window.  IDs beyond it remain countable when the source is
                // already in stable key order.
            } else if seen_ids.len() < MAX_HEADER_SPECIALISTS {
                seen_ids.insert(observation.id);
                total_seen = total_seen.saturating_add(1);
            } else if ordered {
                // A monotonic keyset stream proves this is a new identity
                // without retaining all 100k keys.
                total_seen = total_seen.saturating_add(1);
            } else {
                // Keep scanning without retaining the entire unordered key
                // set.  The count remains useful for a normal unique source,
                // while the exactness bit makes duplicate ambiguity explicit.
                overflowed = true;
                unique_count_exact = false;
                total_seen = total_seen.saturating_add(1);
            }
            if total_seen > MAX_HEADER_SPECIALISTS {
                overflowed = true;
            }

            let fingerprint = agent_observation_fingerprint(observation);
            if let Some(previous) = watermarks.get(&observation.id).copied() {
                match compare_stamp(
                    observation.runtime_generation,
                    observation.revision,
                    previous.runtime_generation,
                    previous.revision,
                ) {
                    std::cmp::Ordering::Less => continue,
                    std::cmp::Ordering::Equal => {
                        if previous.fingerprint != fingerprint
                            || previous.removed != observation.removed
                        {
                            conflicts_rejected = conflicts_rejected.saturating_add(1);
                            source_available = false;
                            candidates.remove(&observation.id);
                            removed.remove(&observation.id);
                        }
                        continue;
                    }
                    std::cmp::Ordering::Greater => {}
                }
            }

            if observation.removed {
                if removed.len() < MAX_HEADER_SPECIALISTS || removed.contains(&observation.id) {
                    removed.insert(observation.id);
                } else {
                    removals_truncated = true;
                    overflowed = true;
                }
                candidates.remove(&observation.id);
                if watermarks.len() < MAX_HEADER_SPECIALISTS
                    || watermarks.contains_key(&observation.id)
                {
                    watermarks.insert(
                        observation.id,
                        AgentWaterMark {
                            runtime_generation: observation.runtime_generation,
                            revision: observation.revision,
                            fingerprint,
                            removed: true,
                        },
                    );
                } else {
                    overflowed = true;
                }
                continue;
            }
            removed.remove(&observation.id);
            let candidate = AgentProjection::from_observation(observation);
            if candidates.contains_key(&observation.id) {
                candidates.insert(observation.id, candidate);
                watermarks.insert(
                    observation.id,
                    AgentWaterMark {
                        runtime_generation: observation.runtime_generation,
                        revision: observation.revision,
                        fingerprint,
                        removed: false,
                    },
                );
                continue;
            }
            while largest_ids
                .peek()
                .is_some_and(|id: &AgentSessionId| !candidates.contains_key(id))
            {
                largest_ids.pop();
            }
            if candidates.len() < MAX_HEADER_SPECIALISTS {
                largest_ids.push(observation.id);
                candidates.insert(observation.id, candidate);
                watermarks.insert(
                    observation.id,
                    AgentWaterMark {
                        runtime_generation: observation.runtime_generation,
                        revision: observation.revision,
                        fingerprint,
                        removed: false,
                    },
                );
            } else if let Some(&largest) = largest_ids.peek() {
                if observation.id < largest {
                    largest_ids.pop();
                    candidates.remove(&largest);
                    watermarks.remove(&largest);
                    largest_ids.push(observation.id);
                    candidates.insert(observation.id, candidate);
                    watermarks.insert(
                        observation.id,
                        AgentWaterMark {
                            runtime_generation: observation.runtime_generation,
                            revision: observation.revision,
                            fingerprint,
                            removed: false,
                        },
                    );
                }
            }
        }

        let mut retained: Vec<_> = candidates.into_values().collect();
        retained.sort_by_key(|candidate| candidate.id);
        let mut removed_ids: Vec<_> = removed.into_iter().collect();
        removed_ids.truncate(MAX_HEADER_SPECIALISTS);

        Self {
            retained,
            removed_ids,
            total_seen,
            scanned,
            removals_truncated,
            overflowed,
            source_available,
            unique_count_exact,
            conflicts_rejected,
        }
    }

    pub fn retained(&self) -> &[AgentProjection] {
        &self.retained
    }

    pub fn total_seen(&self) -> usize {
        self.total_seen
    }

    pub fn unique_count(&self) -> usize {
        self.total_seen
    }

    pub fn scanned(&self) -> usize {
        self.scanned
    }

    pub fn removed_ids(&self) -> &[AgentSessionId] {
        &self.removed_ids
    }

    pub fn removals_truncated(&self) -> bool {
        self.removals_truncated
    }

    pub fn overflowed(&self) -> bool {
        self.overflowed
    }

    pub fn source_available(&self) -> bool {
        self.source_available
    }

    pub fn unique_count_exact(&self) -> bool {
        self.unique_count_exact
    }

    pub fn conflicts_rejected(&self) -> usize {
        self.conflicts_rejected
    }

    /// Return a bounded exclusive keyset window.  Adding or relabelling an
    /// id before `anchor` cannot change the identity of rows after `anchor`.
    pub fn window_after_id(
        &self,
        anchor: Option<AgentSessionId>,
        limit: usize,
    ) -> SpecialistWindow {
        let limit = limit.min(MAX_SPECIALIST_VIRTUAL_WINDOW);
        let mut items = Vec::with_capacity(limit);
        let mut next_after_id = anchor;
        for candidate in &self.retained {
            if anchor.is_some_and(|anchor| candidate.id <= anchor) {
                continue;
            }
            if items.len() == limit {
                break;
            }
            next_after_id = Some(candidate.id);
            items.push(candidate.clone());
        }
        SpecialistWindow {
            window_after_id: anchor,
            next_after_id,
            items,
            removed_ids: self.removed_ids.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AgentWaterMark {
    runtime_generation: u64,
    revision: u64,
    fingerprint: u64,
    removed: bool,
}

fn agent_observation_fingerprint(observation: AgentObservation<'_>) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(observation.id.as_bytes());
    hasher.update(observation.task_id.as_bytes());
    hasher.update(observation.label.as_bytes());
    hasher.update(observation.provider.as_bytes());
    if let Some(session) = observation.provider_session_id {
        // Session refs are opaque and size-checked before they are retained;
        // the raw value is only fed into the digest and never formatted.
        if let Ok(reference) = OpaqueProviderSessionRef::try_from_raw(session) {
            hasher.update(reference.as_digest());
        } else {
            hasher.update([0_u8; 16]);
        }
    }
    hasher.update([match observation.lifecycle {
        AgentSessionLifecycle::Open => 0,
        AgentSessionLifecycle::Closing => 1,
        AgentSessionLifecycle::Closed => 2,
    }]);
    hasher.update(observation.runtime_generation.to_be_bytes());
    hasher.update(observation.revision.to_be_bytes());
    hasher.update([u8::from(observation.removed)]);
    u64::from_be_bytes(
        hasher.finalize()[..8]
            .try_into()
            .expect("fixed digest prefix"),
    )
}

// -------------------------------------------------------------------------
// Immutable action captures and bounded tick queue

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskIdentity {
    pub task_id: TaskId,
    pub revision: u64,
    pub resource_generation: u64,
    pub connection_epoch: u64,
    pub focus_epoch: u64,
    pub client_epoch: u64,
    pub navigation_epoch: u64,
    /// Request epoch fences a capture to the observation request that created
    /// it; action epoch fences task mutations within that request.
    pub request_epoch: u64,
    pub action_epoch: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ObservationStamp {
    pub observed_at_ms: i64,
    pub generation: u64,
    pub revision: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActionTarget {
    Task(TaskIdentity),
    Host(ObservationStamp),
    Remote(ObservationStamp),
    Quota(ObservationStamp),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectedAction {
    request: ActionRequest,
    target: ActionTarget,
}

impl ProjectedAction {
    pub fn new(request: ActionRequest, target: ActionTarget) -> Self {
        Self {
            request: sanitize_action_request(request),
            target,
        }
    }

    pub fn task_show(identity: TaskIdentity) -> Self {
        Self::new(
            ActionRequest::TaskShow {
                task_id: identity.task_id,
            },
            ActionTarget::Task(identity),
        )
    }

    pub fn task_rename(identity: TaskIdentity, title: impl AsRef<str>) -> Self {
        Self::new(
            ActionRequest::TaskRename(TaskRenameArguments {
                task_id: identity.task_id,
                // Bound the borrowed caller text before creating the owned
                // action payload.  This is the only public constructor that
                // accepts untrusted rename text directly.
                title: presentation_text(title.as_ref(), MAX_LABEL_SCALARS),
            }),
            ActionTarget::Task(identity),
        )
    }

    pub fn host_status(stamp: ObservationStamp) -> Self {
        Self::new(ActionRequest::HostStatus, ActionTarget::Host(stamp))
    }

    pub fn request(&self) -> &ActionRequest {
        &self.request
    }

    pub fn target(&self) -> &ActionTarget {
        &self.target
    }

    pub fn descriptor(&self) -> &'static ActionDescriptor {
        self.request.descriptor()
    }

    pub fn id(&self) -> &'static str {
        self.request.id()
    }

    fn safely_coalescible(&self) -> bool {
        self.descriptor().risk == ActionRisk::ReadOnly && self.id() == ACTION_TASK_SHOW
    }
}

fn sanitize_action_request(mut request: ActionRequest) -> ActionRequest {
    if let ActionRequest::TaskRename(arguments) = &mut request {
        arguments.title = presentation_text(&arguments.title, MAX_LABEL_SCALARS);
    }
    request
}

pub type HeaderAction = ProjectedAction;
pub type TopBarAction = ProjectedAction;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PendingHeaderActionOutcome {
    Queued,
    Coalesced,
    Full,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PendingHeaderActionError {
    Full,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingHeaderActionQueue {
    actions: VecDeque<ProjectedAction>,
    capacity: usize,
}

impl PendingHeaderActionQueue {
    pub fn new(capacity: usize) -> Self {
        Self {
            actions: VecDeque::new(),
            capacity: capacity.clamp(1, MAX_PENDING_HEADER_ACTIONS),
        }
    }

    /// Coalesce only an exact, read-only task-show capture.  Mutating actions
    /// remain ordered even when their target epochs match.
    pub fn push(&mut self, action: ProjectedAction) -> PendingHeaderActionOutcome {
        if action.safely_coalescible() && self.actions.iter().any(|pending| pending == &action) {
            return PendingHeaderActionOutcome::Coalesced;
        }
        if self.actions.len() >= self.capacity {
            return PendingHeaderActionOutcome::Full;
        }
        self.actions.push_back(action);
        PendingHeaderActionOutcome::Queued
    }

    pub fn try_push(
        &mut self,
        action: ProjectedAction,
    ) -> Result<PendingHeaderActionOutcome, PendingHeaderActionError> {
        match self.push(action) {
            PendingHeaderActionOutcome::Full => Err(PendingHeaderActionError::Full),
            outcome => Ok(outcome),
        }
    }

    pub fn len(&self) -> usize {
        self.actions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Drain only the caller's bounded canonical-runtime tick budget.
    pub fn drain_for_tick(&mut self, limit: usize) -> Vec<ProjectedAction> {
        let count = limit.min(self.actions.len());
        self.actions.drain(..count).collect()
    }
}

// -------------------------------------------------------------------------
// Top-bar observations and one quota projection source

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostHealth {
    Healthy,
    Degraded,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectState {
    Connected,
    Connecting,
    Disconnected,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateState {
    Disabled,
    Idle,
    Checking,
    UpToDate,
    Available,
    Downloading,
    ReadyToInstall,
    Installing,
    Error,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteHealth {
    Healthy,
    Degraded,
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostObservationIdentity {
    #[serde(deserialize_with = "deserialize_source_string")]
    pub host_id: String,
    pub revision: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostObservation {
    pub identity: HostObservationIdentity,
    pub health: HostHealth,
    pub observed_at_ms: Option<i64>,
    pub generation: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectObservation {
    #[serde(deserialize_with = "deserialize_source_string")]
    pub host_id: String,
    pub state: ConnectState,
    pub revision: u64,
    pub observed_at_ms: Option<i64>,
    pub generation: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateObservation {
    #[serde(deserialize_with = "deserialize_source_string")]
    pub source_id: String,
    pub state: UpdateState,
    pub revision: u64,
    pub observed_at_ms: Option<i64>,
    pub generation: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteObservationIdentity {
    #[serde(deserialize_with = "deserialize_source_string")]
    pub source_id: String,
    pub revision: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteObservation {
    pub identity: RemoteObservationIdentity,
    pub health: RemoteHealth,
    #[serde(deserialize_with = "deserialize_label_string")]
    pub label: String,
    pub observed_at_ms: Option<i64>,
    pub generation: Option<u64>,
}

impl RemoteObservation {
    pub fn new(
        source_id: &str,
        health: RemoteHealth,
        label: &str,
        generation: u64,
        revision: u64,
        observed_at_ms: i64,
    ) -> Self {
        Self {
            identity: RemoteObservationIdentity {
                source_id: bounded_label(source_id, MAX_SOURCE_SCALARS),
                revision,
            },
            health,
            label: bounded_label(label, MAX_LABEL_SCALARS),
            observed_at_ms: Some(observed_at_ms),
            generation: Some(generation),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaObservationIdentity {
    #[serde(deserialize_with = "deserialize_provider_string")]
    pub provider: String,
    /// Deliberately opaque: this field cannot deserialize a raw provider
    /// conversation id.
    pub provider_session_ref: OpaqueProviderSessionRef,
    pub observation_id: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuotaObservation {
    pub identity: QuotaObservationIdentity,
    pub detail: String,
    pub observed_at_ms: Option<i64>,
    pub generation: Option<u64>,
    pub revision: u64,
}

impl Serialize for QuotaObservation {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        #[serde(deny_unknown_fields)]
        struct Identity {
            provider: String,
            provider_session_ref: OpaqueProviderSessionRef,
            observation_id: u64,
        }
        #[derive(Serialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            identity: Identity,
            detail: String,
            observed_at_ms: Option<i64>,
            generation: Option<u64>,
            revision: u64,
        }
        let sanitized = sanitize_quota_observation(self);
        Wire {
            identity: Identity {
                provider: sanitized.identity.provider,
                provider_session_ref: sanitized.identity.provider_session_ref,
                observation_id: sanitized.identity.observation_id,
            },
            detail: sanitized.detail,
            observed_at_ms: sanitized.observed_at_ms,
            generation: sanitized.generation,
            revision: sanitized.revision,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for QuotaObservation {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Identity {
            #[serde(deserialize_with = "deserialize_provider_string")]
            provider: String,
            provider_session_ref: OpaqueProviderSessionRef,
            observation_id: u64,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            identity: Identity,
            #[serde(deserialize_with = "deserialize_detail_string")]
            detail: String,
            observed_at_ms: Option<i64>,
            generation: Option<u64>,
            revision: u64,
        }
        let wire = Wire::deserialize(deserializer)?;
        validate_bounded_wire(
            "quota.provider",
            &wire.identity.provider,
            MAX_PROVIDER_SCALARS,
        )
        .map_err(serde::de::Error::custom)?;
        validate_bounded_wire("quota.detail", &wire.detail, MAX_DETAIL_SCALARS)
            .map_err(serde::de::Error::custom)?;
        Ok(Self {
            identity: QuotaObservationIdentity {
                provider: wire.identity.provider,
                provider_session_ref: wire.identity.provider_session_ref,
                observation_id: wire.identity.observation_id,
            },
            detail: wire.detail,
            observed_at_ms: wire.observed_at_ms,
            generation: wire.generation,
            revision: wire.revision,
        })
    }
}

fn deserialize_bounded_string<'de, D>(
    deserializer: D,
    field: &'static str,
    max_scalars: usize,
) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct BoundedTextVisitor {
        field: &'static str,
        max_scalars: usize,
    }

    impl<'de> Visitor<'de> for BoundedTextVisitor {
        type Value = String;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(formatter, "a bounded {} string", self.field)
        }

        fn visit_borrowed_str<E>(self, value: &'de str) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            self.visit_str(value)
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            validate_bounded_wire(self.field, value, self.max_scalars).map_err(E::custom)?;
            Ok(value.to_owned())
        }

        fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            validate_bounded_wire(self.field, &value, self.max_scalars).map_err(E::custom)?;
            Ok(value)
        }
    }

    deserializer.deserialize_str(BoundedTextVisitor { field, max_scalars })
}

fn deserialize_source_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_string(deserializer, "source", MAX_SOURCE_SCALARS)
}

fn deserialize_provider_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_string(deserializer, "provider", MAX_PROVIDER_SCALARS)
}

fn deserialize_detail_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_string(deserializer, "detail", MAX_DETAIL_SCALARS)
}

fn deserialize_label_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_string(deserializer, "label", MAX_LABEL_SCALARS)
}

fn deserialize_bounded_quotas<'de, D>(deserializer: D) -> Result<Vec<QuotaObservation>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct QuotasVisitor;

    impl<'de> Visitor<'de> for QuotasVisitor {
        type Value = Vec<QuotaObservation>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a bounded quota sequence")
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let mut quotas = Vec::with_capacity(MAX_TOP_BAR_QUOTA_CACHE);
            while let Some(quota) = sequence.next_element::<QuotaObservation>()? {
                if quotas.len() >= MAX_TOP_BAR_QUOTA_CACHE {
                    return Err(de::Error::custom(format!(
                        "quota count exceeds {}",
                        MAX_TOP_BAR_QUOTA_CACHE
                    )));
                }
                quotas.push(quota);
            }
            Ok(quotas)
        }
    }

    deserializer.deserialize_seq(QuotasVisitor)
}

fn validate_bounded_wire(
    field: &'static str,
    value: &str,
    max_scalars: usize,
) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("{field} must not be blank"));
    }
    let actual = value.chars().count();
    if actual > max_scalars {
        return Err(format!("{field} exceeds {max_scalars} scalars ({actual})"));
    }
    Ok(())
}

impl QuotaObservation {
    pub fn new(
        provider: &str,
        provider_session_ref: OpaqueProviderSessionRef,
        generation: u64,
        revision: u64,
        observed_at_ms: i64,
        detail: &str,
    ) -> Self {
        Self {
            identity: QuotaObservationIdentity {
                provider: bounded_label(provider, MAX_PROVIDER_SCALARS),
                provider_session_ref,
                observation_id: revision,
            },
            detail: bounded_label(detail, MAX_DETAIL_SCALARS),
            observed_at_ms: Some(observed_at_ms),
            generation: Some(generation),
            revision,
        }
    }

    pub fn from_raw_session(
        provider: &str,
        raw_provider_session_id: &str,
        generation: u64,
        revision: u64,
        observed_at_ms: i64,
        detail: &str,
    ) -> Result<Self, ProviderSessionRefError> {
        let session = OpaqueProviderSessionRef::try_from_raw(raw_provider_session_id)?;
        Ok(Self::new(
            provider,
            session,
            generation,
            revision,
            observed_at_ms,
            detail,
        ))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HostResourceObservation {
    pub cpu_percent: Option<f64>,
    pub memory_bytes: Option<u64>,
    pub revision: u64,
    pub observed_at_ms: Option<i64>,
    pub generation: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TopBarProjectionInput {
    pub now_ms: i64,
    pub generation: u64,
    pub host: Option<HostObservation>,
    pub connect: Option<ConnectObservation>,
    pub update: Option<UpdateObservation>,
    pub remote: Option<RemoteObservation>,
    /// The host-owned adapter supplies this one list.  No app-level quota
    /// poller is consulted by this component.
    #[serde(deserialize_with = "deserialize_bounded_quotas")]
    pub quotas: Vec<QuotaObservation>,
    pub resources: Option<HostResourceObservation>,
}

impl Default for TopBarProjectionInput {
    fn default() -> Self {
        Self {
            now_ms: 0,
            generation: 0,
            host: None,
            connect: None,
            update: None,
            remote: None,
            quotas: Vec::new(),
            resources: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TopBarProjectionError {
    InvalidNow,
    InvalidGeneration,
    TooManyQuotas { actual: usize, max: usize },
    QuotaConflict,
}

impl fmt::Display for TopBarProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidNow => formatter.write_str("top-bar now_ms must be non-negative"),
            Self::InvalidGeneration => formatter.write_str("top-bar generation must be nonzero"),
            Self::TooManyQuotas { actual, max } => {
                write!(formatter, "quota count {actual} exceeds bound {max}")
            }
            Self::QuotaConflict => formatter.write_str("equal-stamp quota observations conflict"),
        }
    }
}

impl std::error::Error for TopBarProjectionError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TopBarStatus {
    Host(HostHealth),
    Connect(ConnectState),
    Update(UpdateState),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TopBarStatusLink {
    pub status: TopBarStatus,
    pub label: String,
    pub description: String,
    pub role: AccessibleRole,
    pub focusable: bool,
    pub action: TopBarAction,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuotaProjection {
    pub provider: String,
    pub provider_session_ref: OpaqueProviderSessionRef,
    pub detail: String,
    pub age_ms: i64,
    pub role: AccessibleRole,
    pub focusable: bool,
    pub action: TopBarAction,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteProjection {
    pub source_id: String,
    pub health: RemoteHealth,
    pub label: String,
    pub age_ms: i64,
    pub role: AccessibleRole,
    pub focusable: bool,
    pub accessible_description: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResourceProjection {
    pub cpu_percent: Option<f64>,
    pub memory_bytes: Option<u64>,
    pub age_ms: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TopBarUnavailable {
    HostStatus,
    ConnectionStatus,
    UpdateStatus,
    Remote,
    Quota,
    Cpu,
    Memory,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TopBarModel {
    pub host: Option<TopBarStatusLink>,
    pub connect: Option<TopBarStatusLink>,
    pub update: Option<TopBarStatusLink>,
    pub remote: Option<RemoteProjection>,
    pub quotas: Vec<QuotaProjection>,
    pub quota_hidden_count: usize,
    pub quota_overflow_count: usize,
    pub quotas_truncated: bool,
    pub quota_overflow_action: Option<TopBarAction>,
    pub resources: Option<ResourceProjection>,
    pub unavailable: Vec<TopBarUnavailable>,
    pub accessible_description: String,
}

impl TopBarModel {
    pub fn unavailable() -> Self {
        Self {
            host: None,
            connect: None,
            update: None,
            remote: None,
            quotas: Vec::new(),
            quota_hidden_count: 0,
            quota_overflow_count: 0,
            quotas_truncated: false,
            quota_overflow_action: None,
            resources: None,
            unavailable: vec![
                TopBarUnavailable::HostStatus,
                TopBarUnavailable::ConnectionStatus,
                TopBarUnavailable::UpdateStatus,
                TopBarUnavailable::Remote,
                TopBarUnavailable::Quota,
                TopBarUnavailable::Cpu,
                TopBarUnavailable::Memory,
            ],
            accessible_description: "Top-bar observations are unavailable.".to_string(),
        }
    }

    pub fn try_from_input(input: &TopBarProjectionInput) -> Result<Self, TopBarProjectionError> {
        if input.now_ms < 0 {
            return Err(TopBarProjectionError::InvalidNow);
        }
        if input.generation == 0 {
            return Err(TopBarProjectionError::InvalidGeneration);
        }
        if input.quotas.len() > MAX_TOP_BAR_QUOTA_CACHE {
            return Err(TopBarProjectionError::TooManyQuotas {
                actual: input.quotas.len(),
                max: MAX_TOP_BAR_QUOTA_CACHE,
            });
        }

        let host = input.host.as_ref().and_then(|observation| {
            fresh(
                input.now_ms,
                input.generation,
                observation.observed_at_ms,
                observation.generation,
                observation.identity.revision,
            )
            .map(|stamp| status_link(TopBarStatus::Host(observation.health), "Host", stamp))
        });
        let connect = input.connect.as_ref().and_then(|observation| {
            fresh(
                input.now_ms,
                input.generation,
                observation.observed_at_ms,
                observation.generation,
                observation.revision,
            )
            .map(|stamp| {
                status_link(
                    TopBarStatus::Connect(observation.state),
                    "Connection",
                    stamp,
                )
            })
        });
        let update = input.update.as_ref().and_then(|observation| {
            fresh(
                input.now_ms,
                input.generation,
                observation.observed_at_ms,
                observation.generation,
                observation.revision,
            )
            .map(|stamp| status_link(TopBarStatus::Update(observation.state), "Updates", stamp))
        });
        let remote = input.remote.as_ref().and_then(|observation| {
            fresh(
                input.now_ms,
                input.generation,
                observation.observed_at_ms,
                observation.generation,
                observation.identity.revision,
            )
            .map(|stamp| {
                let source_id = bounded_label(&observation.identity.source_id, MAX_SOURCE_SCALARS);
                let label = bounded_label(&observation.label, MAX_LABEL_SCALARS);
                let accessible_description = format!(
                    "Remote {label} is {}.",
                    remote_health_label(observation.health)
                );
                RemoteProjection {
                    source_id,
                    health: observation.health,
                    label,
                    age_ms: stamp.age_ms(input.now_ms),
                    role: AccessibleRole::Status,
                    focusable: false,
                    accessible_description: presentation_text(
                        &accessible_description,
                        MAX_ACCESSIBLE_SCALARS,
                    ),
                }
            })
        });

        let mut selected: BTreeMap<(String, OpaqueProviderSessionRef), QuotaObservation> =
            BTreeMap::new();
        for observation in &input.quotas {
            let Some(stamp) = fresh(
                input.now_ms,
                input.generation,
                observation.observed_at_ms,
                observation.generation,
                observation.revision,
            ) else {
                continue;
            };
            let key = (
                bounded_label(&observation.identity.provider, MAX_PROVIDER_SCALARS),
                observation.identity.provider_session_ref,
            );
            let replace = match selected.get(&key) {
                None => true,
                Some(current) => {
                    let current_stamp = quota_stamp_for(current).expect("selected quota is fresh");
                    match compare_observation_stamps(stamp, current_stamp) {
                        std::cmp::Ordering::Greater => true,
                        std::cmp::Ordering::Less => false,
                        std::cmp::Ordering::Equal => {
                            if quota_fingerprint(observation) != quota_fingerprint(current) {
                                return Err(TopBarProjectionError::QuotaConflict);
                            }
                            false
                        }
                    }
                }
            };
            if replace {
                selected.insert(key, sanitize_quota_observation(observation));
            }
        }
        let mut quota_values: Vec<_> = selected.into_values().collect();
        quota_values.sort_by(|left, right| {
            left.identity
                .provider
                .cmp(&right.identity.provider)
                .then_with(|| {
                    left.identity
                        .provider_session_ref
                        .cmp(&right.identity.provider_session_ref)
                })
        });
        let quota_hidden_count = quota_values.len().saturating_sub(MAX_TOP_BAR_QUOTAS);
        let quota_action_stamp = quota_values
            .iter()
            .filter_map(quota_stamp_for)
            .max_by(|left, right| compare_observation_stamps(*left, *right));
        quota_values.truncate(MAX_TOP_BAR_QUOTAS);
        let quotas = quota_values
            .into_iter()
            .filter_map(|observation| {
                let stamp = quota_stamp_for(&observation)?;
                Some(QuotaProjection {
                    provider: bounded_label(&observation.identity.provider, MAX_PROVIDER_SCALARS),
                    provider_session_ref: observation.identity.provider_session_ref,
                    detail: bounded_label(&observation.detail, MAX_DETAIL_SCALARS),
                    age_ms: stamp.age_ms(input.now_ms),
                    role: AccessibleRole::Status,
                    focusable: false,
                    action: ProjectedAction::new(
                        ActionRequest::HostStatus,
                        ActionTarget::Quota(stamp),
                    ),
                })
            })
            .collect::<Vec<_>>();

        let resources = input.resources.as_ref().and_then(|observation| {
            fresh(
                input.now_ms,
                input.generation,
                observation.observed_at_ms,
                observation.generation,
                observation.revision,
            )
            .map(|stamp| ResourceProjection {
                cpu_percent: valid_cpu_percent(observation.cpu_percent),
                memory_bytes: observation.memory_bytes,
                age_ms: stamp.age_ms(input.now_ms),
            })
        });

        let mut unavailable = Vec::new();
        if host.is_none() {
            unavailable.push(TopBarUnavailable::HostStatus);
        }
        if connect.is_none() {
            unavailable.push(TopBarUnavailable::ConnectionStatus);
        }
        if update.is_none() {
            unavailable.push(TopBarUnavailable::UpdateStatus);
        }
        if remote.is_none() {
            unavailable.push(TopBarUnavailable::Remote);
        }
        if quotas.is_empty() {
            unavailable.push(TopBarUnavailable::Quota);
        }
        if resources
            .as_ref()
            .and_then(|resource| resource.cpu_percent)
            .is_none()
        {
            unavailable.push(TopBarUnavailable::Cpu);
        }
        if resources
            .as_ref()
            .and_then(|resource| resource.memory_bytes)
            .is_none()
        {
            unavailable.push(TopBarUnavailable::Memory);
        }

        let mut accessible = String::new();
        append_accessible(
            &mut accessible,
            host.as_ref().map(|status| status.description.as_str()),
        );
        append_accessible(
            &mut accessible,
            connect.as_ref().map(|status| status.description.as_str()),
        );
        append_accessible(
            &mut accessible,
            update.as_ref().map(|status| status.description.as_str()),
        );
        append_accessible(
            &mut accessible,
            remote
                .as_ref()
                .map(|remote| remote.accessible_description.as_str()),
        );
        for quota in &quotas {
            let description = format!("{} quota: {}.", quota.provider, quota.detail);
            append_accessible(&mut accessible, Some(&description));
        }
        if let Some(resource) = resources.as_ref() {
            let cpu = resource
                .cpu_percent
                .map_or_else(|| "unavailable".to_string(), |value| format!("{value:.1}%"));
            let memory = resource
                .memory_bytes
                .map_or_else(|| "unavailable".to_string(), |value| value.to_string());
            let description = format!("CPU {cpu}; memory {memory} bytes.");
            append_accessible(&mut accessible, Some(&description));
        }
        if quota_hidden_count != 0 {
            append_accessible(
                &mut accessible,
                Some("Additional provider quotas are available in host status."),
            );
        }
        for missing in &unavailable {
            append_accessible(&mut accessible, Some(unavailable_description(*missing)));
        }
        if accessible.is_empty() {
            accessible = "Top-bar observations are unavailable.".to_string();
        }

        Ok(Self {
            host,
            connect,
            update,
            remote,
            quotas,
            quota_hidden_count,
            quota_overflow_count: 0,
            quotas_truncated: quota_hidden_count != 0,
            quota_overflow_action: quota_action_stamp.map(|stamp| {
                ProjectedAction::new(ActionRequest::HostStatus, ActionTarget::Quota(stamp))
            }),
            resources,
            unavailable,
            accessible_description: presentation_text(&accessible, MAX_ACCESSIBLE_SCALARS),
        })
    }

    pub fn from_input(input: &TopBarProjectionInput) -> Self {
        Self::try_from_input(input).unwrap_or_else(|_| Self::unavailable())
    }
}

/// Host-owned observation adapter seam.  It retains one bounded quota source
/// and applies the same field ledger used by task/nested projections; it does
/// not poll, drain, dispatch, or contact a provider synchronously.
#[derive(Clone)]
pub struct TopBarProjectionController {
    input: TopBarProjectionInput,
    high_water: HeaderHighWaterLedger,
    quota_overflow_count: usize,
}

impl TopBarProjectionController {
    pub fn new(input: TopBarProjectionInput) -> Self {
        let quota_overflow_count = input.quotas.len().saturating_sub(MAX_TOP_BAR_QUOTA_CACHE);
        let input = normalize_top_bar_input(input);
        let mut controller = Self {
            input,
            high_water: HeaderHighWaterLedger::new(
                MAX_HEADER_HIGH_WATER_ENTRIES,
                HEADER_HIGH_WATER_TTL_MS,
            ),
            quota_overflow_count,
        };
        controller.seed_high_water();
        controller
    }

    pub fn input(&self) -> &TopBarProjectionInput {
        &self.input
    }

    pub fn high_water(&self) -> &HeaderHighWaterLedger {
        &self.high_water
    }

    pub fn observe_quota(&mut self, observation: QuotaObservation) -> HighWaterDecision {
        let Some(generation) = observation.generation else {
            return HighWaterDecision::RejectedInvalid;
        };
        let Some(observed_at_ms) = observation.observed_at_ms else {
            return HighWaterDecision::RejectedInvalid;
        };
        let provider = bounded_label(&observation.identity.provider, MAX_PROVIDER_SCALARS);
        let key = HeaderFieldKey::Quota {
            provider: provider.clone(),
            provider_session_ref: observation.identity.provider_session_ref,
        };
        let same_identity = self.input.quotas.iter().any(|current| {
            current.identity.provider == provider
                && current.identity.provider_session_ref
                    == observation.identity.provider_session_ref
        });
        if !same_identity && self.input.quotas.len() >= MAX_TOP_BAR_QUOTA_CACHE {
            return HighWaterDecision::NeedsFullResync;
        }
        let decision = self.high_water.observe(
            key,
            generation,
            observation.revision,
            observed_at_ms,
            quota_fingerprint(&observation),
            false,
        );
        if decision == HighWaterDecision::Accepted {
            let sanitized = sanitize_quota_observation(&observation);
            self.input
                .quotas
                .retain(|current| !same_quota_identity(current, &sanitized));
            self.input.quotas.push(sanitized);
        }
        decision
    }

    pub fn observe_host(&mut self, observation: HostObservation) -> HighWaterDecision {
        let Some(generation) = observation.generation else {
            return HighWaterDecision::RejectedInvalid;
        };
        let Some(observed_at_ms) = observation.observed_at_ms else {
            return HighWaterDecision::RejectedInvalid;
        };
        let source_id = bounded_label(&observation.identity.host_id, MAX_SOURCE_SCALARS);
        let decision = self.high_water.observe(
            HeaderFieldKey::Host {
                source_id: source_id.clone(),
            },
            generation,
            observation.identity.revision,
            observed_at_ms,
            host_fingerprint(&observation),
            false,
        );
        if decision == HighWaterDecision::Accepted {
            self.input.host = Some(HostObservation {
                identity: HostObservationIdentity {
                    host_id: source_id,
                    revision: observation.identity.revision,
                },
                health: observation.health,
                observed_at_ms: Some(observed_at_ms),
                generation: Some(generation),
            });
        }
        decision
    }

    pub fn observe_connect(&mut self, observation: ConnectObservation) -> HighWaterDecision {
        let Some(generation) = observation.generation else {
            return HighWaterDecision::RejectedInvalid;
        };
        let Some(observed_at_ms) = observation.observed_at_ms else {
            return HighWaterDecision::RejectedInvalid;
        };
        let source_id = bounded_label(&observation.host_id, MAX_SOURCE_SCALARS);
        let decision = self.high_water.observe(
            HeaderFieldKey::Connect {
                source_id: source_id.clone(),
            },
            generation,
            observation.revision,
            observed_at_ms,
            connect_fingerprint(&observation),
            false,
        );
        if decision == HighWaterDecision::Accepted {
            self.input.connect = Some(ConnectObservation {
                host_id: source_id,
                state: observation.state,
                revision: observation.revision,
                observed_at_ms: Some(observed_at_ms),
                generation: Some(generation),
            });
        }
        decision
    }

    pub fn observe_update(&mut self, observation: UpdateObservation) -> HighWaterDecision {
        let Some(generation) = observation.generation else {
            return HighWaterDecision::RejectedInvalid;
        };
        let Some(observed_at_ms) = observation.observed_at_ms else {
            return HighWaterDecision::RejectedInvalid;
        };
        let source_id = bounded_label(&observation.source_id, MAX_SOURCE_SCALARS);
        let decision = self.high_water.observe(
            HeaderFieldKey::Update {
                source_id: source_id.clone(),
            },
            generation,
            observation.revision,
            observed_at_ms,
            update_observation_fingerprint(&observation),
            false,
        );
        if decision == HighWaterDecision::Accepted {
            self.input.update = Some(UpdateObservation {
                source_id,
                state: observation.state,
                revision: observation.revision,
                observed_at_ms: Some(observed_at_ms),
                generation: Some(generation),
            });
        }
        decision
    }

    pub fn observe_resource(&mut self, observation: HostResourceObservation) -> HighWaterDecision {
        let Some(generation) = observation.generation else {
            return HighWaterDecision::RejectedInvalid;
        };
        let Some(observed_at_ms) = observation.observed_at_ms else {
            return HighWaterDecision::RejectedInvalid;
        };
        let fingerprint = resource_fingerprint(&observation);
        let cpu = self.high_water.observe(
            HeaderFieldKey::HostResource {
                field: AgentResourceField::Cpu,
            },
            generation,
            observation.revision,
            observed_at_ms,
            fingerprint,
            false,
        );
        let memory = self.high_water.observe(
            HeaderFieldKey::HostResource {
                field: AgentResourceField::Memory,
            },
            generation,
            observation.revision,
            observed_at_ms,
            fingerprint,
            false,
        );
        let decision = combine_high_water_decisions(cpu, memory);
        if matches!(decision, HighWaterDecision::Accepted) {
            self.input.resources = Some(HostResourceObservation {
                cpu_percent: valid_cpu_percent(observation.cpu_percent),
                memory_bytes: observation.memory_bytes,
                revision: observation.revision,
                observed_at_ms: Some(observed_at_ms),
                generation: Some(generation),
            });
        }
        decision
    }

    pub fn observe_remote(&mut self, observation: RemoteObservation) -> HighWaterDecision {
        let Some(generation) = observation.generation else {
            return HighWaterDecision::RejectedInvalid;
        };
        let Some(observed_at_ms) = observation.observed_at_ms else {
            return HighWaterDecision::RejectedInvalid;
        };
        let key = HeaderFieldKey::Remote {
            source_id: bounded_label(&observation.identity.source_id, MAX_SOURCE_SCALARS),
        };
        let decision = self.high_water.observe(
            key,
            generation,
            observation.identity.revision,
            observed_at_ms,
            remote_fingerprint(&observation),
            false,
        );
        if decision == HighWaterDecision::Accepted {
            self.input.remote = Some(RemoteObservation::new(
                &observation.identity.source_id,
                observation.health,
                &observation.label,
                generation,
                observation.identity.revision,
                observed_at_ms,
            ));
        }
        decision
    }

    pub fn model(&self) -> TopBarModel {
        let mut model = TopBarModel::from_input(&self.input);
        model.quota_overflow_count = self.quota_overflow_count;
        if self.quota_overflow_count != 0 {
            model.quota_hidden_count = model
                .quota_hidden_count
                .saturating_add(self.quota_overflow_count);
            model.quotas_truncated = true;
            if model.quota_overflow_action.is_none() {
                model.quota_overflow_action =
                    model.quotas.first().map(|quota| quota.action.clone());
            }
        }
        model
    }

    fn seed_high_water(&mut self) {
        if let Some(host) = self.input.host.as_ref() {
            if let (Some(generation), Some(observed_at_ms)) = (host.generation, host.observed_at_ms)
            {
                let _ = self.high_water.observe(
                    HeaderFieldKey::Host {
                        source_id: bounded_label(&host.identity.host_id, MAX_SOURCE_SCALARS),
                    },
                    generation,
                    host.identity.revision,
                    observed_at_ms,
                    host_fingerprint(host),
                    false,
                );
            }
        }
        if let Some(connect) = self.input.connect.as_ref() {
            if let (Some(generation), Some(observed_at_ms)) =
                (connect.generation, connect.observed_at_ms)
            {
                let _ = self.high_water.observe(
                    HeaderFieldKey::Connect {
                        source_id: bounded_label(&connect.host_id, MAX_SOURCE_SCALARS),
                    },
                    generation,
                    connect.revision,
                    observed_at_ms,
                    connect_fingerprint(connect),
                    false,
                );
            }
        }
        if let Some(update) = self.input.update.as_ref() {
            if let (Some(generation), Some(observed_at_ms)) =
                (update.generation, update.observed_at_ms)
            {
                let _ = self.high_water.observe(
                    HeaderFieldKey::Update {
                        source_id: bounded_label(&update.source_id, MAX_SOURCE_SCALARS),
                    },
                    generation,
                    update.revision,
                    observed_at_ms,
                    update_observation_fingerprint(update),
                    false,
                );
            }
        }
        if let Some(remote) = self.input.remote.as_ref() {
            if let (Some(generation), Some(observed_at_ms)) =
                (remote.generation, remote.observed_at_ms)
            {
                let _ = self.high_water.observe(
                    HeaderFieldKey::Remote {
                        source_id: bounded_label(&remote.identity.source_id, MAX_SOURCE_SCALARS),
                    },
                    generation,
                    remote.identity.revision,
                    observed_at_ms,
                    remote_fingerprint(remote),
                    false,
                );
            }
        }
        for quota in &self.input.quotas {
            if let (Some(generation), Some(observed_at_ms)) =
                (quota.generation, quota.observed_at_ms)
            {
                let _ = self.high_water.observe(
                    HeaderFieldKey::Quota {
                        provider: bounded_label(&quota.identity.provider, MAX_PROVIDER_SCALARS),
                        provider_session_ref: quota.identity.provider_session_ref,
                    },
                    generation,
                    quota.revision,
                    observed_at_ms,
                    quota_fingerprint(quota),
                    false,
                );
            }
        }
        if let Some(resource) = self.input.resources.as_ref() {
            if let (Some(generation), Some(observed_at_ms)) =
                (resource.generation, resource.observed_at_ms)
            {
                let fingerprint = resource_fingerprint(resource);
                for field in [AgentResourceField::Cpu, AgentResourceField::Memory] {
                    let _ = self.high_water.observe(
                        HeaderFieldKey::HostResource { field },
                        generation,
                        resource.revision,
                        observed_at_ms,
                        fingerprint,
                        false,
                    );
                }
            }
        }
    }
}

fn normalize_top_bar_input(mut input: TopBarProjectionInput) -> TopBarProjectionInput {
    input.host = input.host.map(|observation| HostObservation {
        identity: HostObservationIdentity {
            host_id: bounded_label(&observation.identity.host_id, MAX_SOURCE_SCALARS),
            revision: observation.identity.revision,
        },
        health: observation.health,
        observed_at_ms: observation.observed_at_ms,
        generation: observation.generation,
    });
    input.connect = input.connect.map(|observation| ConnectObservation {
        host_id: bounded_label(&observation.host_id, MAX_SOURCE_SCALARS),
        state: observation.state,
        revision: observation.revision,
        observed_at_ms: observation.observed_at_ms,
        generation: observation.generation,
    });
    input.update = input.update.map(|observation| UpdateObservation {
        source_id: bounded_label(&observation.source_id, MAX_SOURCE_SCALARS),
        state: observation.state,
        revision: observation.revision,
        observed_at_ms: observation.observed_at_ms,
        generation: observation.generation,
    });
    input.remote = input.remote.map(|observation| RemoteObservation {
        identity: RemoteObservationIdentity {
            source_id: bounded_label(&observation.identity.source_id, MAX_SOURCE_SCALARS),
            revision: observation.identity.revision,
        },
        health: observation.health,
        label: bounded_label(&observation.label, MAX_LABEL_SCALARS),
        observed_at_ms: observation.observed_at_ms,
        generation: observation.generation,
    });
    let mut quotas: Vec<_> = input
        .quotas
        .into_iter()
        .map(|observation| sanitize_quota_observation(&observation))
        .collect();
    quotas.sort_by(|left, right| {
        left.identity
            .provider
            .cmp(&right.identity.provider)
            .then_with(|| {
                left.identity
                    .provider_session_ref
                    .cmp(&right.identity.provider_session_ref)
            })
    });
    quotas.truncate(MAX_TOP_BAR_QUOTA_CACHE);
    input.quotas = quotas;
    if let Some(resource) = input.resources.as_mut() {
        resource.cpu_percent = valid_cpu_percent(resource.cpu_percent);
    }
    input
}

fn combine_high_water_decisions(
    left: HighWaterDecision,
    right: HighWaterDecision,
) -> HighWaterDecision {
    use HighWaterDecision::*;
    if left == RejectedConflict || right == RejectedConflict {
        return RejectedConflict;
    }
    if left == RejectedInvalid || right == RejectedInvalid {
        return RejectedInvalid;
    }
    if left == NeedsFullResync || right == NeedsFullResync {
        return NeedsFullResync;
    }
    if left == RejectedCapacity || right == RejectedCapacity {
        return RejectedCapacity;
    }
    if left == IgnoredStale || right == IgnoredStale {
        return IgnoredStale;
    }
    Accepted
}

fn same_quota_identity(left: &QuotaObservation, right: &QuotaObservation) -> bool {
    left.identity.provider == right.identity.provider
        && left.identity.provider_session_ref == right.identity.provider_session_ref
}

fn update_fingerprint(state: UpdateState) -> u8 {
    match state {
        UpdateState::Disabled => 0,
        UpdateState::Idle => 1,
        UpdateState::Checking => 2,
        UpdateState::UpToDate => 3,
        UpdateState::Available => 4,
        UpdateState::Downloading => 5,
        UpdateState::ReadyToInstall => 6,
        UpdateState::Installing => 7,
        UpdateState::Error => 8,
    }
}

fn host_health_fingerprint(health: HostHealth) -> u8 {
    match health {
        HostHealth::Healthy => 0,
        HostHealth::Degraded => 1,
        HostHealth::Unavailable => 2,
    }
}

fn connect_state_fingerprint(state: ConnectState) -> u8 {
    match state {
        ConnectState::Connected => 0,
        ConnectState::Connecting => 1,
        ConnectState::Disconnected => 2,
        ConnectState::Failed => 3,
    }
}

fn remote_health_label(health: RemoteHealth) -> &'static str {
    match health {
        RemoteHealth::Healthy => "healthy",
        RemoteHealth::Degraded => "degraded",
        RemoteHealth::Unavailable => "unavailable",
    }
}

fn remote_health_fingerprint(health: RemoteHealth) -> u8 {
    match health {
        RemoteHealth::Healthy => 0,
        RemoteHealth::Degraded => 1,
        RemoteHealth::Unavailable => 2,
    }
}

fn hash_option_i64(hasher: &mut Sha256, value: Option<i64>) {
    match value {
        Some(value) => {
            hasher.update([1]);
            hasher.update(value.to_be_bytes());
        }
        None => hasher.update([0]),
    }
}

fn hash_option_u64(hasher: &mut Sha256, value: Option<u64>) {
    match value {
        Some(value) => {
            hasher.update([1]);
            hasher.update(value.to_be_bytes());
        }
        None => hasher.update([0]),
    }
}

fn hash_digest_prefix(hasher: Sha256) -> u64 {
    u64::from_be_bytes(
        hasher.finalize()[..8]
            .try_into()
            .expect("fixed digest prefix"),
    )
}

fn quota_fingerprint(observation: &QuotaObservation) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(observation.identity.provider.as_bytes());
    hasher.update(observation.identity.provider_session_ref.as_digest());
    hasher.update(observation.identity.observation_id.to_be_bytes());
    hasher.update(observation.detail.as_bytes());
    hash_option_i64(&mut hasher, observation.observed_at_ms);
    hash_option_u64(&mut hasher, observation.generation);
    hasher.update(observation.revision.to_be_bytes());
    hash_digest_prefix(hasher)
}

fn remote_fingerprint(observation: &RemoteObservation) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(observation.identity.source_id.as_bytes());
    hasher.update([remote_health_fingerprint(observation.health)]);
    hasher.update(observation.label.as_bytes());
    hasher.update(observation.identity.revision.to_be_bytes());
    hash_option_i64(&mut hasher, observation.observed_at_ms);
    hash_option_u64(&mut hasher, observation.generation);
    hash_digest_prefix(hasher)
}

fn host_fingerprint(observation: &HostObservation) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(observation.identity.host_id.as_bytes());
    hasher.update([host_health_fingerprint(observation.health)]);
    hasher.update(observation.identity.revision.to_be_bytes());
    hash_option_i64(&mut hasher, observation.observed_at_ms);
    hash_option_u64(&mut hasher, observation.generation);
    hash_digest_prefix(hasher)
}

fn connect_fingerprint(observation: &ConnectObservation) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(observation.host_id.as_bytes());
    hasher.update([connect_state_fingerprint(observation.state)]);
    hasher.update(observation.revision.to_be_bytes());
    hash_option_i64(&mut hasher, observation.observed_at_ms);
    hash_option_u64(&mut hasher, observation.generation);
    hash_digest_prefix(hasher)
}

fn update_observation_fingerprint(observation: &UpdateObservation) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(observation.source_id.as_bytes());
    hasher.update([update_fingerprint(observation.state)]);
    hasher.update(observation.revision.to_be_bytes());
    hash_option_i64(&mut hasher, observation.observed_at_ms);
    hash_option_u64(&mut hasher, observation.generation);
    hash_digest_prefix(hasher)
}

fn resource_fingerprint(observation: &HostResourceObservation) -> u64 {
    let mut hasher = Sha256::new();
    match observation.cpu_percent {
        Some(value) => {
            hasher.update([1]);
            hasher.update(value.to_bits().to_be_bytes());
        }
        None => hasher.update([0]),
    }
    hash_option_u64(&mut hasher, observation.memory_bytes);
    hasher.update(observation.revision.to_be_bytes());
    hash_option_i64(&mut hasher, observation.observed_at_ms);
    hash_option_u64(&mut hasher, observation.generation);
    hash_digest_prefix(hasher)
}

fn quota_stamp_for(observation: &QuotaObservation) -> Option<ObservationStamp> {
    Some(ObservationStamp {
        observed_at_ms: observation.observed_at_ms?,
        generation: observation.generation?,
        revision: observation.revision,
    })
}

fn compare_observation_stamps(
    left: ObservationStamp,
    right: ObservationStamp,
) -> std::cmp::Ordering {
    compare_stamp(
        left.generation,
        left.revision,
        right.generation,
        right.revision,
    )
    .then_with(|| left.observed_at_ms.cmp(&right.observed_at_ms))
}

fn valid_cpu_percent(value: Option<f64>) -> Option<f64> {
    value.filter(|value| value.is_finite() && (0.0..=100.0).contains(value))
}

fn unavailable_description(unavailable: TopBarUnavailable) -> &'static str {
    match unavailable {
        TopBarUnavailable::HostStatus => "Host status is unavailable.",
        TopBarUnavailable::ConnectionStatus => "Connection status is unavailable.",
        TopBarUnavailable::UpdateStatus => "Update status is unavailable.",
        TopBarUnavailable::Remote => "Remote status is unavailable.",
        TopBarUnavailable::Quota => "Quota status is unavailable.",
        TopBarUnavailable::Cpu => "CPU usage is unavailable.",
        TopBarUnavailable::Memory => "Memory usage is unavailable.",
    }
}

fn fresh(
    now_ms: i64,
    generation: u64,
    observed_at_ms: Option<i64>,
    observation_generation: Option<u64>,
    revision: u64,
) -> Option<ObservationStamp> {
    let observed_at_ms = observed_at_ms?;
    let observation_generation = observation_generation?;
    let age_ms = now_ms.checked_sub(observed_at_ms)?;
    if observation_generation != generation || !(0..=MAX_OBSERVATION_AGE_MS).contains(&age_ms) {
        return None;
    }
    Some(ObservationStamp {
        observed_at_ms,
        generation,
        revision,
    })
}

impl ObservationStamp {
    fn age_ms(self, now_ms: i64) -> i64 {
        now_ms.saturating_sub(self.observed_at_ms)
    }
}

fn status_link(status: TopBarStatus, label: &str, stamp: ObservationStamp) -> TopBarStatusLink {
    TopBarStatusLink {
        status,
        label: label.to_string(),
        description: presentation_text(
            &format!("{label} status is {}.", status_label(status)),
            MAX_ACCESSIBLE_SCALARS,
        ),
        role: AccessibleRole::Status,
        focusable: false,
        action: ProjectedAction::host_status(stamp),
    }
}

fn status_label(status: TopBarStatus) -> &'static str {
    match status {
        TopBarStatus::Host(health) => match health {
            HostHealth::Healthy => "healthy",
            HostHealth::Degraded => "degraded",
            HostHealth::Unavailable => "unavailable",
        },
        TopBarStatus::Connect(state) => match state {
            ConnectState::Connected => "connected",
            ConnectState::Connecting => "connecting",
            ConnectState::Disconnected => "disconnected",
            ConnectState::Failed => "failed",
        },
        TopBarStatus::Update(state) => match state {
            UpdateState::Disabled => "disabled",
            UpdateState::Idle => "idle",
            UpdateState::Checking => "checking",
            UpdateState::UpToDate => "up to date",
            UpdateState::Available => "available",
            UpdateState::Downloading => "downloading",
            UpdateState::ReadyToInstall => "ready to install",
            UpdateState::Installing => "installing",
            UpdateState::Error => "error",
        },
    }
}

fn append_accessible(output: &mut String, value: Option<&str>) {
    let Some(value) = value else { return };
    if output.len() >= MAX_ACCESSIBLE_SCALARS.saturating_mul(4) {
        return;
    }
    if !output.is_empty() {
        output.push(' ');
    }
    let remaining = MAX_ACCESSIBLE_SCALARS.saturating_sub(output.chars().count());
    output.push_str(&presentation_text(value, remaining));
}

fn sanitize_quota_observation(observation: &QuotaObservation) -> QuotaObservation {
    QuotaObservation {
        identity: QuotaObservationIdentity {
            provider: bounded_label(&observation.identity.provider, MAX_PROVIDER_SCALARS),
            provider_session_ref: observation.identity.provider_session_ref,
            observation_id: observation.identity.observation_id,
        },
        detail: bounded_label(&observation.detail, MAX_DETAIL_SCALARS),
        observed_at_ms: observation.observed_at_ms,
        generation: observation.generation,
        revision: observation.revision,
    }
}

// -------------------------------------------------------------------------
// Task header projection and renderer-facing semantic layout

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskProjectionContext {
    pub resource_generation: u64,
    pub connection_epoch: u64,
    pub focus_epoch: u64,
    pub client_epoch: u64,
    pub navigation_epoch: u64,
    pub request_epoch: u64,
}

impl Default for TaskProjectionContext {
    fn default() -> Self {
        Self {
            resource_generation: 0,
            connection_epoch: 0,
            focus_epoch: 0,
            client_epoch: 0,
            navigation_epoch: 0,
            request_epoch: 0,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskHeaderModel {
    pub identity: TaskIdentity,
    pub title: String,
    pub project: ProjectProjection,
    pub workspace: WorkspaceProjection,
    pub specialists: SpecialistProjection,
    pub status: VisibleTaskStatus,
    pub accessible_description: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectProjection {
    pub id: ProjectId,
    pub label: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceProjection {
    Main,
    Worktree { path: PathBuf, branch: String },
    External { path: PathBuf },
}

impl TaskHeaderModel {
    pub fn from_model(
        model: &ClientModel,
        task_id: TaskId,
        context: TaskProjectionContext,
    ) -> Option<Self> {
        let snapshot = model.tasks().get(&task_id)?;
        Some(Self::from_snapshot(task_id, snapshot, context))
    }

    pub fn from_snapshot(
        _task_id: TaskId,
        snapshot: &TaskSnapshot,
        context: TaskProjectionContext,
    ) -> Self {
        let task_id = snapshot.task.id;
        let identity = TaskIdentity {
            task_id,
            revision: snapshot.task.revision,
            resource_generation: context.resource_generation,
            connection_epoch: context.connection_epoch,
            focus_epoch: context.focus_epoch,
            client_epoch: context.client_epoch,
            navigation_epoch: context.navigation_epoch,
            request_epoch: context.request_epoch,
            action_epoch: snapshot.task.action_epoch,
        };
        let specialists = SpecialistProjection::from_iter(
            snapshot
                .agents
                .values()
                .filter(|agent| agent.task_id == task_id)
                .map(|agent| AgentObservation {
                    id: agent.id,
                    task_id: agent.task_id,
                    label: match &agent.role {
                        crate::domain::agent::AgentRole::Primary => "Primary",
                        crate::domain::agent::AgentRole::Specialist { name } => name.as_str(),
                    },
                    provider: agent.provider_kind.as_str(),
                    provider_session_id: agent.provider_session_id.as_deref(),
                    lifecycle: agent.lifecycle,
                    runtime_generation: agent.runtime_generation,
                    revision: agent.revision,
                    removed: false,
                }),
        );
        let project = ProjectProjection {
            id: snapshot.task.project_id,
            label: format!("project-{}", snapshot.task.project_id),
        };
        let workspace = workspace_projection(&snapshot.task.workspace);
        let title = bounded_label(&snapshot.task.title, MAX_LABEL_SCALARS);
        let status = snapshot.visible_status();
        let accessible_description = presentation_text(
            &format!("Task {}. {}. Status {:?}.", title, project.label, status),
            MAX_ACCESSIBLE_SCALARS,
        );
        Self {
            identity,
            title,
            project,
            workspace,
            specialists,
            status,
            accessible_description,
        }
    }

    pub fn task_show_action(&self) -> ProjectedAction {
        ProjectedAction::task_show(self.identity)
    }

    pub fn layout(&self, width_px: u16) -> HeaderLayout {
        HeaderLayout::for_model(self, width_px)
    }

    pub fn accessibility_tree(&self) -> SemanticNode {
        let specialist_description = if !self.specialists.source_available() {
            "Specialist observations are unavailable."
        } else if self.specialists.overflowed() && !self.specialists.unique_count_exact() {
            "Specialist list is truncated and its count is approximate."
        } else if self.specialists.overflowed() {
            "Specialist list is truncated; stable identity pagination is available."
        } else {
            "Specialists are navigated by stable session identity."
        };
        SemanticNode {
            role: AccessibleRole::Region,
            label: "Task header".to_string(),
            description: self.accessible_description.clone(),
            children: vec![
                SemanticNode {
                    role: AccessibleRole::Status,
                    label: self.title.clone(),
                    description: self.accessible_description.clone(),
                    children: Vec::new(),
                },
                SemanticNode {
                    role: AccessibleRole::Status,
                    label: format!("{} specialist sessions", self.specialists.total_seen()),
                    description: specialist_description.to_string(),
                    children: Vec::new(),
                },
            ],
        }
    }
}

fn workspace_projection(workspace: &WorkspaceRef) -> WorkspaceProjection {
    match workspace {
        WorkspaceRef::Main => WorkspaceProjection::Main,
        WorkspaceRef::Worktree { path, branch } => WorkspaceProjection::Worktree {
            path: bounded_path(path),
            branch: bounded_label(branch, MAX_PROVIDER_SCALARS),
        },
        WorkspaceRef::External { path } => WorkspaceProjection::External {
            path: bounded_path(path),
        },
    }
}

fn bounded_path(path: &std::path::Path) -> PathBuf {
    PathBuf::from(presentation_text(
        &path.to_string_lossy(),
        MAX_WORKSPACE_PATH_SCALARS,
    ))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HeaderField {
    Title,
    Project,
    Workspace,
    Specialists,
    Status,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TitleLayout {
    SingleLine(String),
    Wrapped(Vec<String>),
    Truncated(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HeaderLayout {
    pub inline: Vec<HeaderField>,
    pub overflow: Vec<HeaderField>,
    pub title: TitleLayout,
    pub overflow_label: Option<String>,
    pub accessible_description: String,
}

impl HeaderLayout {
    pub fn for_model(model: &TaskHeaderModel, width_px: u16) -> Self {
        let bounded_title = presentation_text(&model.title, MAX_LABEL_SCALARS);
        let title = if width_px < 320 {
            TitleLayout::Truncated(truncate_with_ellipsis(
                &bounded_title,
                MAX_TITLE_LINE_SCALARS,
            ))
        } else if width_px < 480 {
            TitleLayout::Wrapped(wrap_title(&bounded_title, MAX_TITLE_LINE_SCALARS))
        } else if bounded_title.chars().count() > MAX_TITLE_SINGLE_LINE_SCALARS {
            TitleLayout::Truncated(truncate_with_ellipsis(
                &bounded_title,
                MAX_TITLE_SINGLE_LINE_SCALARS,
            ))
        } else {
            TitleLayout::SingleLine(bounded_title)
        };
        let (inline, overflow) = if width_px < 400 {
            (
                vec![HeaderField::Title, HeaderField::Status],
                vec![
                    HeaderField::Project,
                    HeaderField::Workspace,
                    HeaderField::Specialists,
                ],
            )
        } else if width_px < 640 {
            (
                vec![
                    HeaderField::Title,
                    HeaderField::Project,
                    HeaderField::Status,
                ],
                vec![HeaderField::Workspace, HeaderField::Specialists],
            )
        } else {
            (
                vec![
                    HeaderField::Title,
                    HeaderField::Project,
                    HeaderField::Workspace,
                    HeaderField::Status,
                ],
                vec![HeaderField::Specialists],
            )
        };
        let overflow_label =
            (!overflow.is_empty()).then(|| format!("{} more task header fields", overflow.len()));
        Self {
            inline,
            overflow,
            title,
            overflow_label,
            accessible_description: presentation_text(
                &model.accessible_description,
                MAX_ACCESSIBLE_SCALARS,
            ),
        }
    }
}

fn truncate_with_ellipsis(value: &str, max_scalars: usize) -> String {
    if value.chars().count() <= max_scalars {
        return value.to_string();
    }
    if max_scalars <= 1 {
        return "…".chars().take(max_scalars).collect();
    }
    let mut truncated: String = value.chars().take(max_scalars - 1).collect();
    truncated.push('…');
    truncated
}

fn wrap_title(value: &str, max_line_scalars: usize) -> Vec<String> {
    if max_line_scalars == 0 {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in value.split_whitespace() {
        let word_len = word.chars().count();
        if word_len > max_line_scalars {
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
            }
            let mut chunk = String::new();
            for character in word.chars() {
                chunk.push(character);
                if chunk.chars().count() == max_line_scalars {
                    lines.push(std::mem::take(&mut chunk));
                }
            }
            if !chunk.is_empty() {
                current = chunk;
            }
            continue;
        }
        let proposed = current.chars().count() + usize::from(!current.is_empty()) + word_len;
        if proposed > max_line_scalars && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticNode {
    pub role: AccessibleRole,
    pub label: String,
    pub description: String,
    pub children: Vec<SemanticNode>,
}
