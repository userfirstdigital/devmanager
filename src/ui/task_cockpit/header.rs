//! Bounded, presentation-only contracts for the task header and top bar.
//!
//! This module deliberately stops at immutable projections.  A canonical
//! shell owns the host connection, observation tick, GPUI tree, and action
//! dispatch.  Nothing here drains a channel, starts a client, or performs
//! synchronous host work from paint/input code.

use std::collections::{BTreeMap, BTreeSet, BinaryHeap, VecDeque};
use std::fmt;
use std::path::PathBuf;

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
        for observation in observations {
            if bounded.len() >= self.capacity {
                self.needs_full_resync = true;
                return HighWaterDecision::RejectedCapacity;
            }
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
            if let Some(floor) = prior_floors.get(&observation.key).copied() {
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
            } else if candidate.floors.len() >= candidate.capacity {
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
        let mut candidates: BTreeMap<AgentSessionId, AgentProjection> = BTreeMap::new();
        let mut largest_ids = BinaryHeap::new();
        let mut removed = BTreeSet::new();
        let mut total_seen = 0_usize;
        let mut scanned = 0_usize;
        let mut removals_truncated = false;

        for observation in observations {
            scanned = scanned.saturating_add(1);
            if observation.removed {
                if removed.len() < MAX_HEADER_SPECIALISTS || removed.contains(&observation.id) {
                    removed.insert(observation.id);
                } else {
                    removals_truncated = true;
                }
                candidates.remove(&observation.id);
                continue;
            }
            total_seen = total_seen.saturating_add(1);
            removed.remove(&observation.id);
            let candidate = AgentProjection::from_observation(observation);
            if candidates.contains_key(&observation.id) {
                candidates.insert(observation.id, candidate);
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
            } else if let Some(&largest) = largest_ids.peek() {
                if observation.id < largest {
                    largest_ids.pop();
                    candidates.remove(&largest);
                    largest_ids.push(observation.id);
                    candidates.insert(observation.id, candidate);
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
        }
    }

    pub fn retained(&self) -> &[AgentProjection] {
        &self.retained
    }

    pub fn total_seen(&self) -> usize {
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

    pub fn task_rename(identity: TaskIdentity, title: impl Into<String>) -> Self {
        Self::new(
            ActionRequest::TaskRename(TaskRenameArguments {
                task_id: identity.task_id,
                title: title.into(),
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
    pub host_id: String,
    pub state: ConnectState,
    pub revision: u64,
    pub observed_at_ms: Option<i64>,
    pub generation: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateObservation {
    pub source_id: String,
    pub state: UpdateState,
    pub revision: u64,
    pub observed_at_ms: Option<i64>,
    pub generation: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteObservationIdentity {
    pub source_id: String,
    pub revision: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteObservation {
    pub identity: RemoteObservationIdentity,
    pub health: RemoteHealth,
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
            provider: String,
            provider_session_ref: OpaqueProviderSessionRef,
            observation_id: u64,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            identity: Identity,
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
}

impl fmt::Display for TopBarProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidNow => formatter.write_str("top-bar now_ms must be non-negative"),
            Self::InvalidGeneration => formatter.write_str("top-bar generation must be nonzero"),
            Self::TooManyQuotas { actual, max } => {
                write!(formatter, "quota count {actual} exceeds bound {max}")
            }
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
            )
            .map(|stamp| status_link(TopBarStatus::Host(observation.health), "Host", stamp))
        });
        let connect = input.connect.as_ref().and_then(|observation| {
            fresh(
                input.now_ms,
                input.generation,
                observation.observed_at_ms,
                observation.generation,
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
            )
            .map(|stamp| status_link(TopBarStatus::Update(observation.state), "Updates", stamp))
        });
        let remote = input.remote.as_ref().and_then(|observation| {
            fresh(
                input.now_ms,
                input.generation,
                observation.observed_at_ms,
                observation.generation,
            )
            .map(|stamp| {
                let source_id = bounded_label(&observation.identity.source_id, MAX_SOURCE_SCALARS);
                let label = bounded_label(&observation.label, MAX_LABEL_SCALARS);
                let mut accessible_description = String::with_capacity(32 + label.len());
                accessible_description.push_str("Remote ");
                accessible_description.push_str(&label);
                accessible_description.push_str(match observation.health {
                    RemoteHealth::Healthy => " is healthy.",
                    RemoteHealth::Degraded => " is degraded.",
                    RemoteHealth::Unavailable => " is unavailable.",
                });
                RemoteProjection {
                    source_id,
                    health: observation.health,
                    label,
                    age_ms: stamp.observed_at_ms,
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
            ) else {
                continue;
            };
            let key = (
                bounded_label(&observation.identity.provider, MAX_PROVIDER_SCALARS),
                observation.identity.provider_session_ref,
            );
            let replace = selected.get(&key).is_none_or(|current| {
                (
                    observation.generation.unwrap_or_default(),
                    observation.revision,
                    stamp.observed_at_ms,
                ) > (
                    current.generation.unwrap_or_default(),
                    current.revision,
                    current.observed_at_ms.unwrap_or_default(),
                )
            });
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
        quota_values.truncate(MAX_TOP_BAR_QUOTAS);
        let quota_stamp = ObservationStamp {
            observed_at_ms: input.now_ms,
            generation: input.generation,
            revision: 0,
        };
        let quotas = quota_values
            .into_iter()
            .filter_map(|observation| {
                let age_ms = fresh(
                    input.now_ms,
                    input.generation,
                    observation.observed_at_ms,
                    observation.generation,
                )?
                .observed_at_ms;
                Some(QuotaProjection {
                    provider: bounded_label(&observation.identity.provider, MAX_PROVIDER_SCALARS),
                    provider_session_ref: observation.identity.provider_session_ref,
                    detail: bounded_label(&observation.detail, MAX_DETAIL_SCALARS),
                    age_ms,
                    role: AccessibleRole::Status,
                    focusable: false,
                    action: ProjectedAction::new(
                        ActionRequest::HostStatus,
                        ActionTarget::Quota(quota_stamp),
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
            )
            .map(|stamp| ResourceProjection {
                cpu_percent: observation.cpu_percent,
                memory_bytes: observation.memory_bytes,
                age_ms: stamp.observed_at_ms,
            })
        });

        let mut unavailable = Vec::new();
        if input.host.is_some() && host.is_none() {
            unavailable.push(TopBarUnavailable::HostStatus);
        }
        if input.connect.is_some() && connect.is_none() {
            unavailable.push(TopBarUnavailable::ConnectionStatus);
        }
        if input.update.is_some() && update.is_none() {
            unavailable.push(TopBarUnavailable::UpdateStatus);
        }
        if input.remote.is_some() && remote.is_none() {
            unavailable.push(TopBarUnavailable::Remote);
        }
        if !input.quotas.is_empty() && quotas.is_empty() {
            unavailable.push(TopBarUnavailable::Quota);
        }
        if input
            .resources
            .as_ref()
            .is_some_and(|resource| resource.cpu_percent.is_some())
            && resources
                .as_ref()
                .is_none_or(|resource| resource.cpu_percent.is_none())
        {
            unavailable.push(TopBarUnavailable::Cpu);
        }
        if input
            .resources
            .as_ref()
            .is_some_and(|resource| resource.memory_bytes.is_some())
            && resources
                .as_ref()
                .is_none_or(|resource| resource.memory_bytes.is_none())
        {
            unavailable.push(TopBarUnavailable::Memory);
        }
        if host.is_none()
            && connect.is_none()
            && update.is_none()
            && remote.is_none()
            && quotas.is_empty()
            && resources.is_none()
            && unavailable.is_empty()
        {
            unavailable.extend([
                TopBarUnavailable::HostStatus,
                TopBarUnavailable::ConnectionStatus,
                TopBarUnavailable::UpdateStatus,
                TopBarUnavailable::Remote,
                TopBarUnavailable::Quota,
                TopBarUnavailable::Cpu,
                TopBarUnavailable::Memory,
            ]);
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
            append_accessible(
                &mut accessible,
                Some(&format!("{} quota: {}.", quota.provider, quota.detail)),
            );
        }
        if quota_hidden_count != 0 {
            append_accessible(
                &mut accessible,
                Some("Additional provider quotas are available in host status."),
            );
        }
        if accessible.is_empty() {
            accessible = "Host, connection, update, remote, resource, and quota observations are unavailable.".to_string();
        }

        Some(Self {
            host,
            connect,
            update,
            remote,
            quotas,
            quota_hidden_count,
            quotas_truncated: quota_hidden_count != 0,
            quota_overflow_action: (quota_hidden_count != 0).then(|| {
                ProjectedAction::new(ActionRequest::HostStatus, ActionTarget::Quota(quota_stamp))
            }),
            resources,
            unavailable,
            accessible_description: presentation_text(&accessible, MAX_ACCESSIBLE_SCALARS),
        })
        .ok_or(TopBarProjectionError::InvalidGeneration)
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
}

impl TopBarProjectionController {
    pub fn new(input: TopBarProjectionInput) -> Self {
        Self {
            input,
            high_water: HeaderHighWaterLedger::new(
                MAX_HEADER_HIGH_WATER_ENTRIES,
                HEADER_HIGH_WATER_TTL_MS,
            ),
        }
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
            observation.identity.observation_id,
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
            stable_text_fingerprint(&observation.label),
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
        TopBarModel::from_input(&self.input)
    }
}

fn same_quota_identity(left: &QuotaObservation, right: &QuotaObservation) -> bool {
    left.identity.provider == right.identity.provider
        && left.identity.provider_session_ref == right.identity.provider_session_ref
}

fn stable_text_fingerprint(value: &str) -> u64 {
    let digest = Sha256::digest(value.as_bytes());
    u64::from_be_bytes(digest[..8].try_into().expect("fixed digest prefix"))
}

fn fresh(
    now_ms: i64,
    generation: u64,
    observed_at_ms: Option<i64>,
    observation_generation: Option<u64>,
) -> Option<ObservationStamp> {
    let observed_at_ms = observed_at_ms?;
    let observation_generation = observation_generation?;
    let age_ms = now_ms.checked_sub(observed_at_ms)?;
    if observation_generation != generation || !(0..=MAX_OBSERVATION_AGE_MS).contains(&age_ms) {
        return None;
    }
    Some(ObservationStamp {
        observed_at_ms: age_ms,
        generation,
        revision: 0,
    })
}

fn status_link(status: TopBarStatus, label: &str, stamp: ObservationStamp) -> TopBarStatusLink {
    TopBarStatusLink {
        status,
        label: label.to_string(),
        description: format!("{label} status is available."),
        role: AccessibleRole::Status,
        focusable: false,
        action: ProjectedAction::host_status(stamp),
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
        task_id: TaskId,
        snapshot: &TaskSnapshot,
        context: TaskProjectionContext,
    ) -> Self {
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
        let specialists = SpecialistProjection::from_iter(snapshot.agents.values().map(|agent| {
            AgentObservation {
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
            }
        }));
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
                    description: "Specialists are navigated by stable session identity."
                        .to_string(),
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
            path: path.clone(),
            branch: bounded_label(branch, MAX_PROVIDER_SCALARS),
        },
        WorkspaceRef::External { path } => WorkspaceProjection::External { path: path.clone() },
    }
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
        let title = if width_px < 320 {
            TitleLayout::Truncated(truncate_scalars(&model.title, 28))
        } else if width_px < 480 {
            TitleLayout::Wrapped(model.title.split_whitespace().fold(
                Vec::<String>::new(),
                |mut lines, word| {
                    if lines
                        .last()
                        .is_none_or(|line| line.chars().count() + word.chars().count() + 1 > 28)
                    {
                        lines.push(String::new());
                    }
                    if let Some(line) = lines.last_mut() {
                        if !line.is_empty() {
                            line.push(' ');
                        }
                        line.push_str(word);
                    }
                    lines
                },
            ))
        } else {
            TitleLayout::SingleLine(model.title.clone())
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
            accessible_description: model.accessible_description.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticNode {
    pub role: AccessibleRole,
    pub label: String,
    pub description: String,
    pub children: Vec<SemanticNode>,
}
