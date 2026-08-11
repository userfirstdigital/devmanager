//! Background process-tree accounting.
//!
//! Ownership is supplied by the caller (the managed Job-member query). This
//! module only observes the supplied members and never adopts a PID because it
//! happens to be visible in a system process list.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use crate::domain::snapshot::{
    ProcessAccountingMemberSnapshot, ProcessAccountingSnapshot, ProcessMetricStatus,
};
use crate::process::identity::{ManagedProcessId, ManagedProcessIdentity};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SamplerError {
    InvalidLogicalProcessorCount,
    InvalidInterval,
    CounterReset { pid: u32 },
    ConflictingProcessIdentity { pid: u32 },
    WorkBudgetExceeded { attempted: usize, max: usize },
    ObservationFailed { pid: u32, reason: String },
}

impl std::fmt::Display for SamplerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidLogicalProcessorCount => {
                write!(f, "logical processor count was zero")
            }
            Self::InvalidInterval => write!(f, "sampling interval was zero or moved backwards"),
            Self::CounterReset { pid } => write!(f, "CPU counter reset for PID {pid}"),
            Self::ConflictingProcessIdentity { pid } => {
                write!(f, "conflicting process identities for PID {pid}")
            }
            Self::WorkBudgetExceeded { attempted, max } => {
                write!(
                    f,
                    "sampling work budget exceeded ({attempted} > {max} members)"
                )
            }
            Self::ObservationFailed { pid, reason } => {
                write!(f, "process observation failed for PID {pid}: {reason}")
            }
        }
    }
}

impl std::error::Error for SamplerError {}

impl SamplerError {
    pub fn status(&self) -> ProcessMetricStatus {
        match self {
            Self::CounterReset { .. } | Self::ConflictingProcessIdentity { .. } => {
                ProcessMetricStatus::Partial
            }
            Self::InvalidLogicalProcessorCount
            | Self::InvalidInterval
            | Self::WorkBudgetExceeded { .. }
            | Self::ObservationFailed { .. } => ProcessMetricStatus::Failed,
        }
    }
}

/// Hard limits for one background sampling tick. The process monitor never
/// waits for an unbounded number of OS queries; callers choose a shorter
/// deadline for production and a deterministic member cap in tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SamplingWorkCounters {
    pub runtime_sessions: usize,
    pub cached_process_ids: usize,
    pub cached_process_rows: usize,
    pub session_authority_reads: usize,
    pub job_queries: usize,
    pub job_candidates: usize,
    pub identity_inspections: usize,
    pub metric_observations: usize,
    pub metadata_snapshots: usize,
    pub metadata_rows: usize,
    pub projected_rows: usize,
    pub projected_snapshots: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SamplingMemberIdentity {
    Observed {
        creation_time_100ns: u64,
        canonical_executable: PathBuf,
    },
    Inaccessible {
        creation_time_100ns: Option<u64>,
    },
    Conflict,
}

#[derive(Debug, Clone)]
pub struct SamplingBudget {
    deadline: Instant,
    max_members: usize,
    members: BTreeMap<u32, SamplingMemberIdentity>,
    work: SamplingWorkCounters,
}

impl SamplingBudget {
    pub fn new(deadline: Instant, max_members: usize) -> Self {
        Self {
            deadline,
            max_members,
            members: BTreeMap::new(),
            work: SamplingWorkCounters::default(),
        }
    }

    pub fn from_now(max_members: usize, max_duration: Duration) -> Self {
        Self::new(Instant::now() + max_duration, max_members)
    }

    pub fn max_members(&self) -> usize {
        self.max_members
    }

    pub(crate) fn deadline(&self) -> Instant {
        self.deadline
    }

    pub fn claimed_members(&self) -> usize {
        self.members.len()
    }

    pub fn remaining_capacity(&self) -> usize {
        self.max_members.saturating_sub(self.claimed_members())
    }

    /// Remaining exact-member capacity across every Job queried in this tick.
    pub fn remaining_members(&self) -> usize {
        self.remaining_capacity()
    }

    /// Check the common tick deadline without consuming a member slot. Job
    /// enumeration and process-list refreshes use this before doing OS work;
    /// the sampler claims slots only for the observations it actually samples.
    pub fn checkpoint(&self) -> Result<(), SamplerError> {
        if Instant::now() >= self.deadline {
            return Err(SamplerError::WorkBudgetExceeded {
                attempted: self.claimed_members(),
                max: self.max_members,
            });
        }
        Ok(())
    }

    pub fn contains_pid(&self, pid: u32) -> bool {
        self.members.contains_key(&pid)
    }

    pub fn work_counters(&self) -> SamplingWorkCounters {
        self.work
    }

    pub(crate) fn note_job_query(&mut self) {
        self.work.job_queries = self.work.job_queries.saturating_add(1);
    }

    pub(crate) fn note_runtime_session(&mut self) {
        self.work.runtime_sessions = self.work.runtime_sessions.saturating_add(1);
    }

    pub(crate) fn note_cached_process_id(&mut self) {
        self.work.cached_process_ids = self.work.cached_process_ids.saturating_add(1);
    }

    pub(crate) fn note_cached_process_row(&mut self) {
        self.work.cached_process_rows = self.work.cached_process_rows.saturating_add(1);
    }

    pub(crate) fn note_session_authority_read(&mut self) {
        self.work.session_authority_reads = self.work.session_authority_reads.saturating_add(1);
    }

    pub(crate) fn note_job_candidate(&mut self) {
        self.work.job_candidates = self.work.job_candidates.saturating_add(1);
    }

    pub(crate) fn note_identity_inspection(&mut self) {
        self.work.identity_inspections = self.work.identity_inspections.saturating_add(1);
    }

    pub(crate) fn note_metric_observation(&mut self) {
        self.work.metric_observations = self.work.metric_observations.saturating_add(1);
    }

    pub(crate) fn note_metadata_snapshot(&mut self) {
        self.work.metadata_snapshots = self.work.metadata_snapshots.saturating_add(1);
    }

    pub(crate) fn note_metadata_row(&mut self) {
        self.work.metadata_rows = self.work.metadata_rows.saturating_add(1);
    }

    pub(crate) fn note_projected_row(&mut self) {
        self.work.projected_rows = self.work.projected_rows.saturating_add(1);
    }

    pub(crate) fn note_projected_snapshot(&mut self) {
        self.work.projected_snapshots = self.work.projected_snapshots.saturating_add(1);
    }

    pub(crate) fn admit_identity(
        &mut self,
        identity: &ManagedProcessIdentity,
    ) -> Result<bool, SamplerError> {
        self.admit_member(
            identity.id().pid(),
            SamplingMemberIdentity::Observed {
                creation_time_100ns: identity.id().creation_time_100ns(),
                canonical_executable: identity.canonical_executable().to_path_buf(),
            },
        )
    }

    pub(crate) fn admit_inaccessible(
        &mut self,
        pid: u32,
        creation_time_100ns: Option<u64>,
    ) -> Result<bool, SamplerError> {
        self.admit_member(
            pid,
            SamplingMemberIdentity::Inaccessible {
                creation_time_100ns,
            },
        )
    }

    fn admit_member(
        &mut self,
        pid: u32,
        observed: SamplingMemberIdentity,
    ) -> Result<bool, SamplerError> {
        self.checkpoint()?;
        if let Some(existing) = self.members.get_mut(&pid) {
            let compatible = match (&*existing, &observed) {
                (
                    SamplingMemberIdentity::Observed {
                        creation_time_100ns: left_creation,
                        canonical_executable: left_executable,
                    },
                    SamplingMemberIdentity::Observed {
                        creation_time_100ns: right_creation,
                        canonical_executable: right_executable,
                    },
                ) => left_creation == right_creation && left_executable == right_executable,
                (
                    SamplingMemberIdentity::Observed {
                        creation_time_100ns,
                        ..
                    },
                    SamplingMemberIdentity::Inaccessible {
                        creation_time_100ns: inaccessible,
                    },
                )
                | (
                    SamplingMemberIdentity::Inaccessible {
                        creation_time_100ns: inaccessible,
                    },
                    SamplingMemberIdentity::Observed {
                        creation_time_100ns,
                        ..
                    },
                ) => inaccessible == &Some(*creation_time_100ns),
                (
                    SamplingMemberIdentity::Inaccessible {
                        creation_time_100ns: left,
                    },
                    SamplingMemberIdentity::Inaccessible {
                        creation_time_100ns: right,
                    },
                ) => left == right,
                (SamplingMemberIdentity::Conflict, _) | (_, SamplingMemberIdentity::Conflict) => {
                    false
                }
            };
            if !compatible {
                *existing = SamplingMemberIdentity::Conflict;
                return Err(SamplerError::ConflictingProcessIdentity { pid });
            }
            return Ok(false);
        }
        if self.members.len() >= self.max_members {
            return Err(SamplerError::WorkBudgetExceeded {
                attempted: self.members.len().saturating_add(1),
                max: self.max_members,
            });
        }
        self.members.insert(pid, observed);
        Ok(true)
    }

    fn admit_observation(
        &mut self,
        member: &ProcessMemberObservation,
    ) -> Result<bool, SamplerError> {
        match member {
            ProcessMemberObservation::Accessible(member) => self.admit_identity(&member.identity),
            ProcessMemberObservation::Inaccessible(member) => {
                self.admit_inaccessible(member.pid, member.creation_time_100ns)
            }
        }
    }
}

const DEFAULT_SAMPLE_MEMBER_BUDGET: usize = 512;
const DEFAULT_SAMPLE_DEADLINE: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CpuMetrics {
    pub raw_core_percent: f64,
    pub machine_cpu_percent: f64,
    pub core_equivalent_percent: f64,
}

/// Calculate the exact Task Manager-style projection from cumulative CPU time.
/// Both CPU and wall time are expressed in 100 ns units, matching Windows
/// `GetProcessTimes` and avoiding a hidden denominator change between samples.
pub fn calculate_cpu_metrics(
    cpu_delta_100ns: u64,
    wall_delta_100ns: u64,
    logical_processors: u32,
) -> Result<CpuMetrics, SamplerError> {
    if logical_processors == 0 || wall_delta_100ns == 0 {
        return Err(if logical_processors == 0 {
            SamplerError::InvalidLogicalProcessorCount
        } else {
            SamplerError::InvalidInterval
        });
    }

    let raw_core_percent = 100.0 * cpu_delta_100ns as f64 / wall_delta_100ns as f64;
    Ok(CpuMetrics {
        raw_core_percent: sanitize_core_percent(raw_core_percent),
        machine_cpu_percent: machine_cpu_percent(raw_core_percent, logical_processors),
        core_equivalent_percent: sanitize_core_percent(raw_core_percent),
    })
}

pub fn calculate_cpu_metrics_for_duration(
    cpu_delta_100ns: u64,
    wall_delta: Duration,
    logical_processors: u32,
) -> Result<CpuMetrics, SamplerError> {
    let wall_delta_100ns = wall_delta.as_nanos() / 100;
    if wall_delta_100ns > u64::MAX as u128 {
        return Err(SamplerError::InvalidInterval);
    }
    calculate_cpu_metrics(cpu_delta_100ns, wall_delta_100ns as u64, logical_processors)
}

/// Convert a core-equivalent percentage to the whole-machine percentage used
/// by the default UI projection.
pub fn machine_cpu_percent(raw_core_percent: f64, logical_processors: u32) -> f64 {
    if logical_processors == 0 || !raw_core_percent.is_finite() || raw_core_percent < 0.0 {
        return 0.0;
    }
    (raw_core_percent / logical_processors as f64).clamp(0.0, 100.0)
}

pub fn require_exact_process_identity(
    expected: &ManagedProcessIdentity,
    observed: &ManagedProcessIdentity,
) -> Result<(), String> {
    if expected.matches_root(observed) {
        Ok(())
    } else {
        Err(format!(
            "process identity changed for PID {}",
            expected.id().pid()
        ))
    }
}

fn sanitize_core_percent(value: f64) -> f64 {
    if value.is_finite() && value >= 0.0 {
        value
    } else {
        0.0
    }
}

const MAX_REDACTED_EXECUTABLE_BYTES: usize = 96;

/// Convert an executable identity into the only form safe for a runtime
/// projection: a short basename with a conservative character allowlist.
/// Canonical paths remain private to identity matching and sampler baselines.
fn redacted_executable_basename(path: &Path) -> Option<String> {
    let basename = path.file_name()?.to_string_lossy();
    let mut output = String::with_capacity(basename.len().min(MAX_REDACTED_EXECUTABLE_BYTES));
    for character in basename.chars() {
        if output.len() >= MAX_REDACTED_EXECUTABLE_BYTES {
            break;
        }
        output.push(
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                character
            } else {
                '_'
            },
        );
    }
    (!output.is_empty()).then_some(output)
}

#[derive(Debug, Clone, PartialEq)]
pub struct AccessibleProcess {
    pub identity: ManagedProcessIdentity,
    pub cpu_time_100ns: u64,
    pub private_memory_bytes: u64,
    pub io_read_bytes: Option<u64>,
    pub io_write_bytes: Option<u64>,
}

impl AccessibleProcess {
    pub fn new(
        identity: ManagedProcessIdentity,
        cpu_time_100ns: u64,
        private_memory_bytes: u64,
    ) -> Self {
        Self {
            identity,
            cpu_time_100ns,
            private_memory_bytes,
            io_read_bytes: None,
            io_write_bytes: None,
        }
    }

    pub fn with_io_bytes(mut self, read_bytes: u64, write_bytes: u64) -> Self {
        self.io_read_bytes = Some(read_bytes);
        self.io_write_bytes = Some(write_bytes);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InaccessibleProcess {
    pub pid: u32,
    pub creation_time_100ns: Option<u64>,
    pub reason: Option<String>,
}

impl InaccessibleProcess {
    pub fn new(pid: u32, creation_time_100ns: Option<u64>) -> Self {
        Self {
            pid,
            creation_time_100ns,
            reason: None,
        }
    }

    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProcessMemberObservation {
    Accessible(AccessibleProcess),
    Inaccessible(InaccessibleProcess),
}

impl ProcessMemberObservation {
    pub fn pid(&self) -> u32 {
        match self {
            Self::Accessible(member) => member.identity.id().pid(),
            Self::Inaccessible(member) => member.pid,
        }
    }

    pub fn creation_time_100ns(&self) -> Option<u64> {
        match self {
            Self::Accessible(member) => Some(member.identity.id().creation_time_100ns()),
            Self::Inaccessible(member) => member.creation_time_100ns,
        }
    }

    fn has_same_identity(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Accessible(left), Self::Accessible(right)) => {
                left.identity.matches_root(&right.identity)
            }
            (Self::Inaccessible(left), Self::Inaccessible(right)) => {
                left.pid == right.pid && left.creation_time_100ns == right.creation_time_100ns
            }
            _ => false,
        }
    }

    /// An inaccessible query may be a second view of an accessible Job
    /// member, but only when it carries the same known process generation.
    /// A missing generation is not evidence that a PID still refers to the
    /// same process and therefore cannot be merged with an accessible row.
    fn accessible_matches_inaccessible(
        accessible: &AccessibleProcess,
        inaccessible: &InaccessibleProcess,
    ) -> bool {
        inaccessible.creation_time_100ns == Some(accessible.identity.id().creation_time_100ns())
    }
}

/// Return the one deterministic member set used for both totals and baselines.
/// An accessible observation wins over an inaccessible observation for the same
/// PID/creation pair, but duplicate accessible observations are still counted
/// only once.
pub fn unique_members(
    members: impl IntoIterator<Item = ProcessMemberObservation>,
) -> Vec<ProcessMemberObservation> {
    let mut unique = BTreeMap::<u32, ProcessMemberObservation>::new();
    for member in members {
        insert_unique_member(&mut unique, member);
    }
    unique.into_values().collect()
}

fn insert_unique_member(
    unique: &mut BTreeMap<u32, ProcessMemberObservation>,
    member: ProcessMemberObservation,
) {
    let pid = member.pid();
    let Some(existing) = unique.remove(&pid) else {
        unique.insert(pid, member);
        return;
    };

    let replacement = match (&existing, &member) {
        // A repeated observation of one exact process is harmless.
        _ if existing.has_same_identity(&member) => existing,
        // A successful identity wins over an inaccessible query only if the
        // inaccessible query carries the same known generation. Otherwise
        // this is a PID reuse/identity conflict.
        (
            ProcessMemberObservation::Inaccessible(inaccessible),
            ProcessMemberObservation::Accessible(accessible),
        ) if ProcessMemberObservation::accessible_matches_inaccessible(
            accessible,
            inaccessible,
        ) =>
        {
            member
        }
        (
            ProcessMemberObservation::Accessible(accessible),
            ProcessMemberObservation::Inaccessible(inaccessible),
        ) if ProcessMemberObservation::accessible_matches_inaccessible(
            accessible,
            inaccessible,
        ) =>
        {
            existing
        }
        // Two different generations/executables for one PID are a PID
        // reuse/conflict. Keep one explicit inaccessible row rather than
        // silently selecting a generation and granting it a baseline.
        _ => ProcessMemberObservation::Inaccessible(
            InaccessibleProcess::new(pid, None).with_reason("conflicting process identities"),
        ),
    };
    unique.insert(pid, replacement);
}

#[derive(Debug, Clone, Copy)]
struct CpuBaseline {
    cpu_time_100ns: u64,
    io_read_bytes: Option<u64>,
    io_write_bytes: Option<u64>,
}

#[derive(Debug)]
pub struct ProcessSampler {
    clock_start: Instant,
    last_sample_at: Option<Duration>,
    baselines: BTreeMap<ManagedProcessIdentityKey, CpuBaseline>,
    last_snapshot: Option<Arc<ProcessAccountingSnapshot>>,
    generation: u64,
}

impl Default for ProcessSampler {
    fn default() -> Self {
        Self {
            clock_start: Instant::now(),
            last_sample_at: None,
            baselines: BTreeMap::new(),
            last_snapshot: None,
            generation: 0,
        }
    }
}

impl ProcessSampler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn last_snapshot(&self) -> Option<Arc<ProcessAccountingSnapshot>> {
        self.last_snapshot.clone()
    }

    pub fn sample_now(
        &mut self,
        logical_processors: u32,
        members: impl IntoIterator<Item = ProcessMemberObservation>,
    ) -> Result<Arc<ProcessAccountingSnapshot>, SamplerError> {
        self.sample_at(self.clock_start.elapsed(), logical_processors, members)
    }

    pub fn sample_now_with_budget(
        &mut self,
        logical_processors: u32,
        members: impl IntoIterator<Item = ProcessMemberObservation>,
        budget: &mut SamplingBudget,
    ) -> Result<Arc<ProcessAccountingSnapshot>, SamplerError> {
        self.sample_at_with_budget(
            self.clock_start.elapsed(),
            logical_processors,
            members,
            budget,
        )
    }

    pub fn sample_at(
        &mut self,
        sampled_at: Duration,
        logical_processors: u32,
        members: impl IntoIterator<Item = ProcessMemberObservation>,
    ) -> Result<Arc<ProcessAccountingSnapshot>, SamplerError> {
        let mut budget =
            SamplingBudget::from_now(DEFAULT_SAMPLE_MEMBER_BUDGET, DEFAULT_SAMPLE_DEADLINE);
        self.sample_at_with_budget(sampled_at, logical_processors, members, &mut budget)
    }

    pub fn sample_at_with_budget(
        &mut self,
        sampled_at: Duration,
        logical_processors: u32,
        members: impl IntoIterator<Item = ProcessMemberObservation>,
        budget: &mut SamplingBudget,
    ) -> Result<Arc<ProcessAccountingSnapshot>, SamplerError> {
        if logical_processors == 0 {
            return Err(SamplerError::InvalidLogicalProcessorCount);
        }

        let interval = match self.last_sample_at {
            None => None,
            Some(previous) => {
                let interval = sampled_at
                    .checked_sub(previous)
                    .ok_or(SamplerError::InvalidInterval)?;
                if interval.is_zero() {
                    return Err(SamplerError::InvalidInterval);
                }
                Some(interval)
            }
        };
        let unique = unique_members_with_budget(members, budget)?;
        let wall_delta_100ns = interval.map(|interval| interval.as_nanos() / 100);
        let wall_delta_100ns = wall_delta_100ns
            .filter(|ticks| *ticks > 0 && *ticks <= u64::MAX as u128)
            .map(|ticks| ticks as u64);
        if interval.is_some() && wall_delta_100ns.is_none() {
            return Err(SamplerError::InvalidInterval);
        }

        let mut next_baselines = BTreeMap::new();
        let mut snapshots = Vec::with_capacity(unique.len());
        let mut core_equivalent_percent = 0.0;
        let mut memory_bytes = 0u64;
        let mut io_read_bytes = 0u64;
        let mut io_write_bytes = 0u64;
        let mut has_io_read = false;
        let mut has_io_write = false;
        let mut metrics_unavailable = false;
        let mut has_partial_metrics = false;
        let mut error = None;
        let mut status = if interval.is_some() {
            ProcessMetricStatus::Complete
        } else {
            ProcessMetricStatus::Unknown
        };
        self.generation = self.generation.saturating_add(1);
        let generation = self.generation;

        for member in unique {
            match member {
                ProcessMemberObservation::Accessible(member) => {
                    let key = ManagedProcessIdentityKey::from_identity(&member.identity);
                    let baseline = self.baselines.get(&key).copied();
                    let (cpu_metrics, mut member_status) = match (baseline, wall_delta_100ns) {
                        (None, _) => (None, ProcessMetricStatus::Unknown),
                        (Some(baseline), Some(_wall_delta))
                            if member.cpu_time_100ns < baseline.cpu_time_100ns =>
                        {
                            metrics_unavailable = true;
                            has_partial_metrics = true;
                            status = ProcessMetricStatus::Partial;
                            error = Some("member_cpu_counter_reset".to_string());
                            (None, ProcessMetricStatus::Partial)
                        }
                        (Some(baseline), Some(wall_delta)) => (
                            Some(calculate_cpu_metrics(
                                member.cpu_time_100ns - baseline.cpu_time_100ns,
                                wall_delta,
                                logical_processors,
                            )?),
                            ProcessMetricStatus::Complete,
                        ),
                        (Some(_), None) => (None, ProcessMetricStatus::Unknown),
                    };
                    let io_read_delta = baseline.and_then(|baseline| {
                        counter_delta(baseline.io_read_bytes, member.io_read_bytes)
                    });
                    let io_write_delta = baseline.and_then(|baseline| {
                        counter_delta(baseline.io_write_bytes, member.io_write_bytes)
                    });
                    if baseline.is_some_and(|baseline| {
                        counter_reset(baseline.io_read_bytes, member.io_read_bytes)
                            || counter_reset(baseline.io_write_bytes, member.io_write_bytes)
                    }) {
                        metrics_unavailable = true;
                        has_partial_metrics = true;
                        status = ProcessMetricStatus::Partial;
                        if error.is_none() {
                            error = Some("member_io_counter_reset".to_string());
                        }
                        member_status = ProcessMetricStatus::Partial;
                    }
                    if let Some(delta) = io_read_delta {
                        io_read_bytes = io_read_bytes.saturating_add(delta);
                        has_io_read = true;
                    }
                    if let Some(delta) = io_write_delta {
                        io_write_bytes = io_write_bytes.saturating_add(delta);
                        has_io_write = true;
                    }
                    if let Some(cpu_metrics) = cpu_metrics {
                        core_equivalent_percent += cpu_metrics.core_equivalent_percent;
                    }
                    if !member_status.is_complete() {
                        metrics_unavailable = true;
                        if status == ProcessMetricStatus::Complete {
                            status = member_status;
                        }
                    }
                    memory_bytes = memory_bytes.saturating_add(member.private_memory_bytes);
                    snapshots.push(ProcessAccountingMemberSnapshot {
                        pid: member.identity.id().pid(),
                        creation_time_100ns: Some(member.identity.id().creation_time_100ns()),
                        machine_cpu_percent: cpu_metrics.map(|metrics| metrics.machine_cpu_percent),
                        core_equivalent_percent: cpu_metrics
                            .map(|metrics| metrics.core_equivalent_percent),
                        private_memory_bytes: Some(member.private_memory_bytes),
                        io_read_bytes: io_read_delta,
                        io_write_bytes: io_write_delta,
                        metrics_unavailable: !member_status.is_complete(),
                        status: member_status,
                        // The sampler retains the exact executable only in
                        // its private identity baseline. Its public snapshot
                        // carries a bounded basename, never a canonical path.
                        executable: redacted_executable_basename(
                            member.identity.canonical_executable(),
                        ),
                        generation,
                    });
                    next_baselines.insert(
                        key,
                        CpuBaseline {
                            cpu_time_100ns: member.cpu_time_100ns,
                            io_read_bytes: member.io_read_bytes,
                            io_write_bytes: member.io_write_bytes,
                        },
                    );
                }
                ProcessMemberObservation::Inaccessible(member) => {
                    metrics_unavailable = true;
                    has_partial_metrics = true;
                    status = ProcessMetricStatus::Partial;
                    if error.is_none() {
                        error = Some("member_metrics_unavailable".to_string());
                    }
                    snapshots.push(ProcessAccountingMemberSnapshot {
                        pid: member.pid,
                        creation_time_100ns: member.creation_time_100ns,
                        machine_cpu_percent: None,
                        core_equivalent_percent: None,
                        private_memory_bytes: None,
                        io_read_bytes: None,
                        io_write_bytes: None,
                        metrics_unavailable: true,
                        status: ProcessMetricStatus::Partial,
                        executable: None,
                        generation,
                    });
                }
            }
        }

        let status = if has_partial_metrics {
            ProcessMetricStatus::Partial
        } else {
            status
        };

        let snapshot = Arc::new(ProcessAccountingSnapshot {
            sampled_at,
            interval,
            logical_processors,
            machine_cpu_percent: machine_cpu_percent(core_equivalent_percent, logical_processors),
            core_equivalent_percent: sanitize_core_percent(core_equivalent_percent),
            memory_bytes,
            process_count: snapshots.len() as u32,
            metrics_unavailable,
            status,
            error,
            generation,
            io_read_bytes: has_io_read.then_some(io_read_bytes),
            io_write_bytes: has_io_write.then_some(io_write_bytes),
            members: snapshots,
        });
        self.last_sample_at = Some(sampled_at);
        self.baselines = next_baselines;
        self.last_snapshot = Some(snapshot.clone());
        Ok(snapshot)
    }

    /// Observe one PID without changing sampler baselines. A failed query is
    /// deliberately represented as an inaccessible member so the Job member
    /// remains visible in the background snapshot.
    pub fn observe_process(pid: u32) -> ProcessMemberObservation {
        Self::observe_process_with_expected_identity(pid, None)
    }

    pub fn observe_process_with_expected_identity(
        pid: u32,
        expected: Option<&ManagedProcessIdentity>,
    ) -> ProcessMemberObservation {
        #[cfg(windows)]
        {
            observe_windows_process(pid, expected)
        }
        #[cfg(unix)]
        {
            observe_proc_process(pid, expected)
        }
        #[cfg(not(any(windows, unix)))]
        {
            ProcessMemberObservation::Inaccessible(
                InaccessibleProcess::new(pid, None).with_reason("process metrics unsupported"),
            )
        }
    }
}

/// Deduplicate member observations without allowing an unbounded iterator to
/// run past the per-tick accounting budget. The budget counts raw
/// observations, not just unique PIDs, because every observation can carry an
/// OS query result that must be inspected before it is known to be a
/// duplicate.
fn unique_members_with_budget(
    members: impl IntoIterator<Item = ProcessMemberObservation>,
    budget: &mut SamplingBudget,
) -> Result<Vec<ProcessMemberObservation>, SamplerError> {
    let mut unique = BTreeMap::<u32, ProcessMemberObservation>::new();
    let mut new_unique_members = 0usize;
    budget.checkpoint()?;
    for member in members {
        budget.checkpoint()?;
        let pid = member.pid();
        let is_new_local = !unique.contains_key(&pid);
        let is_new_global = !budget.contains_pid(pid);
        if is_new_local && is_new_global && new_unique_members >= budget.remaining_capacity() {
            return Err(SamplerError::WorkBudgetExceeded {
                attempted: budget
                    .claimed_members()
                    .saturating_add(new_unique_members + 1),
                max: budget.max_members(),
            });
        }
        if is_new_local && is_new_global {
            new_unique_members = new_unique_members.saturating_add(1);
        }
        insert_unique_member(&mut unique, member);
    }
    for member in unique.values() {
        // A Job collector has already admitted authoritative identities. A
        // later metrics query may legitimately be inaccessible without
        // revoking that ownership; only a fresh sampler-only budget needs to
        // admit the deduplicated observation here.
        if !budget.contains_pid(member.pid()) {
            budget.admit_observation(member)?;
        }
    }
    Ok(unique.into_values().collect())
}

fn counter_delta(previous: Option<u64>, current: Option<u64>) -> Option<u64> {
    match (previous, current) {
        (Some(previous), Some(current)) if current >= previous => Some(current - previous),
        _ => None,
    }
}

fn counter_reset(previous: Option<u64>, current: Option<u64>) -> bool {
    matches!((previous, current), (Some(previous), Some(current)) if current < previous)
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ManagedProcessIdentityKey {
    pid: u32,
    creation_time_100ns: u64,
    executable: PathBuf,
}

impl ManagedProcessIdentityKey {
    fn from_identity(identity: &ManagedProcessIdentity) -> Self {
        Self {
            pid: identity.id().pid(),
            creation_time_100ns: identity.id().creation_time_100ns(),
            executable: identity.canonical_executable().to_path_buf(),
        }
    }
}

#[cfg(windows)]
#[repr(C)]
#[derive(Default)]
struct FileTime {
    low_date_time: u32,
    high_date_time: u32,
}

#[cfg(windows)]
#[repr(C)]
#[derive(Default)]
struct ProcessMemoryCountersEx {
    cb: u32,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
    quota_peak_paged_pool_usage: usize,
    quota_paged_pool_usage: usize,
    quota_peak_non_paged_pool_usage: usize,
    quota_non_paged_pool_usage: usize,
    pagefile_usage: usize,
    peak_pagefile_usage: usize,
    private_committed_bytes: usize,
}

#[cfg(windows)]
#[repr(C)]
#[derive(Default)]
struct IoCounters {
    read_operation_count: u64,
    write_operation_count: u64,
    other_operation_count: u64,
    read_transfer_count: u64,
    write_transfer_count: u64,
    other_transfer_count: u64,
}

#[cfg(windows)]
struct ProcessHandle(*mut std::ffi::c_void);

#[cfg(windows)]
impl Drop for ProcessHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn OpenProcess(
        desired_access: u32,
        inherit_handle: i32,
        process_id: u32,
    ) -> *mut std::ffi::c_void;
    fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
    fn GetProcessTimes(
        process: *mut std::ffi::c_void,
        creation_time: *mut FileTime,
        exit_time: *mut FileTime,
        kernel_time: *mut FileTime,
        user_time: *mut FileTime,
    ) -> i32;
    fn QueryFullProcessImageNameW(
        process: *mut std::ffi::c_void,
        flags: u32,
        executable_name: *mut u16,
        size: *mut u32,
    ) -> i32;
    fn GetProcessIoCounters(process: *mut std::ffi::c_void, io_counters: *mut IoCounters) -> i32;
}

#[cfg(windows)]
#[link(name = "psapi")]
extern "system" {
    fn GetProcessMemoryInfo(
        process: *mut std::ffi::c_void,
        counters: *mut ProcessMemoryCountersEx,
        size: u32,
    ) -> i32;
}

#[cfg(windows)]
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
#[cfg(windows)]
const PROCESS_VM_READ: u32 = 0x0010;

#[cfg(windows)]
fn observe_windows_process(
    pid: u32,
    expected: Option<&ManagedProcessIdentity>,
) -> ProcessMemberObservation {
    let inaccessible = |reason: String, creation_time: Option<u64>| {
        ProcessMemberObservation::Inaccessible(
            InaccessibleProcess::new(pid, creation_time).with_reason(reason),
        )
    };
    if pid == 0 {
        return inaccessible("zero PID cannot be observed".to_string(), None);
    }
    let handle =
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ, 0, pid) };
    if handle.is_null() {
        return inaccessible(
            format!(
                "OpenProcess({pid}) failed: {}",
                std::io::Error::last_os_error()
            ),
            None,
        );
    }
    let handle = ProcessHandle(handle);
    let mut creation = FileTime::default();
    let mut exit = FileTime::default();
    let mut kernel = FileTime::default();
    let mut user = FileTime::default();
    if unsafe { GetProcessTimes(handle.0, &mut creation, &mut exit, &mut kernel, &mut user) } == 0 {
        return inaccessible(
            format!(
                "GetProcessTimes({pid}) failed: {}",
                std::io::Error::last_os_error()
            ),
            None,
        );
    }
    let creation_time_100ns = file_time_value(&creation);
    if creation_time_100ns == 0 {
        return inaccessible("process creation time was zero".to_string(), None);
    }

    let mut executable_buffer = vec![0u16; 32_768];
    let mut executable_length = executable_buffer.len() as u32;
    if unsafe {
        QueryFullProcessImageNameW(
            handle.0,
            0,
            executable_buffer.as_mut_ptr(),
            &mut executable_length,
        )
    } == 0
    {
        return inaccessible(
            format!(
                "QueryFullProcessImageNameW({pid}) failed: {}",
                std::io::Error::last_os_error()
            ),
            Some(creation_time_100ns),
        );
    }
    executable_buffer.truncate(executable_length as usize);
    let executable = PathBuf::from(String::from_utf16_lossy(&executable_buffer));
    let identity = match ManagedProcessIdentity::new(
        ManagedProcessId::new(pid, creation_time_100ns).expect("nonzero process identity"),
        executable,
    ) {
        Ok(identity) => identity,
        Err(error) => {
            return inaccessible(
                format!("could not canonicalize process executable: {error}"),
                Some(creation_time_100ns),
            )
        }
    };
    if let Some(expected) = expected {
        if let Err(reason) = require_exact_process_identity(expected, &identity) {
            return inaccessible(reason, Some(creation_time_100ns));
        }
    }

    let mut memory = ProcessMemoryCountersEx {
        cb: std::mem::size_of::<ProcessMemoryCountersEx>() as u32,
        ..ProcessMemoryCountersEx::default()
    };
    if unsafe {
        GetProcessMemoryInfo(
            handle.0,
            &mut memory,
            std::mem::size_of::<ProcessMemoryCountersEx>() as u32,
        )
    } == 0
    {
        return inaccessible(
            format!(
                "GetProcessMemoryInfo({pid}) failed: {}",
                std::io::Error::last_os_error()
            ),
            Some(creation_time_100ns),
        );
    }
    let mut io = IoCounters::default();
    let io_counters = (unsafe { GetProcessIoCounters(handle.0, &mut io) } != 0)
        .then_some((io.read_transfer_count, io.write_transfer_count));
    ProcessMemberObservation::Accessible(
        AccessibleProcess::new(
            identity,
            file_time_value(&kernel).saturating_add(file_time_value(&user)),
            memory.private_committed_bytes as u64,
        )
        .with_optional_io(io_counters),
    )
}

#[cfg(windows)]
impl AccessibleProcess {
    fn with_optional_io(mut self, io: Option<(u64, u64)>) -> Self {
        if let Some((read_bytes, write_bytes)) = io {
            self.io_read_bytes = Some(read_bytes);
            self.io_write_bytes = Some(write_bytes);
        }
        self
    }
}

#[cfg(windows)]
fn file_time_value(value: &FileTime) -> u64 {
    ((value.high_date_time as u64) << 32) | value.low_date_time as u64
}

#[cfg(unix)]
fn observe_proc_process(
    pid: u32,
    expected: Option<&ManagedProcessIdentity>,
) -> ProcessMemberObservation {
    let inaccessible = |reason: String, creation_time: Option<u64>| {
        ProcessMemberObservation::Inaccessible(
            InaccessibleProcess::new(pid, creation_time).with_reason(reason),
        )
    };
    let stat_path = format!("/proc/{pid}/stat");
    let stat = match std::fs::read_to_string(&stat_path) {
        Ok(stat) => stat,
        Err(error) => return inaccessible(format!("read {stat_path} failed: {error}"), None),
    };
    let Some(after_name) = stat.rfind(") ").map(|index| &stat[index + 2..]) else {
        return inaccessible("malformed /proc stat".to_string(), None);
    };
    let fields: Vec<&str> = after_name.split_whitespace().collect();
    let Some(start_time_ticks) = fields.get(19).and_then(|value| value.parse::<u64>().ok()) else {
        return inaccessible("missing process start time".to_string(), None);
    };
    let Some(user_ticks) = fields.get(11).and_then(|value| value.parse::<u64>().ok()) else {
        return inaccessible(
            "missing process user time".to_string(),
            Some(start_time_ticks),
        );
    };
    let Some(kernel_ticks) = fields.get(12).and_then(|value| value.parse::<u64>().ok()) else {
        return inaccessible(
            "missing process kernel time".to_string(),
            Some(start_time_ticks),
        );
    };
    let executable = match std::fs::canonicalize(format!("/proc/{pid}/exe")) {
        Ok(executable) => executable,
        Err(error) => {
            return inaccessible(
                format!("read process executable failed: {error}"),
                Some(start_time_ticks),
            )
        }
    };
    let identity = match ManagedProcessIdentity::new(
        ManagedProcessId::new(pid, start_time_ticks).expect("nonzero proc identity"),
        executable,
    ) {
        Ok(identity) => identity,
        Err(error) => {
            return inaccessible(
                format!("could not canonicalize process executable: {error}"),
                Some(start_time_ticks),
            )
        }
    };
    if let Some(expected) = expected {
        if let Err(reason) = require_exact_process_identity(expected, &identity) {
            return inaccessible(reason, Some(start_time_ticks));
        }
    }
    let private_memory_bytes = match read_proc_private_memory(pid) {
        Some(bytes) => bytes,
        None => {
            return inaccessible(
                "private memory metrics were inaccessible".to_string(),
                Some(start_time_ticks),
            )
        }
    };
    let io = read_proc_io(pid);
    ProcessMemberObservation::Accessible(
        AccessibleProcess::new(
            identity,
            user_ticks
                .saturating_add(kernel_ticks)
                .saturating_mul(100_000),
            private_memory_bytes,
        )
        .with_optional_io(io),
    )
}

#[cfg(unix)]
impl AccessibleProcess {
    fn with_optional_io(mut self, io: Option<(u64, u64)>) -> Self {
        if let Some((read_bytes, write_bytes)) = io {
            self.io_read_bytes = Some(read_bytes);
            self.io_write_bytes = Some(write_bytes);
        }
        self
    }
}

#[cfg(unix)]
fn read_proc_private_memory(pid: u32) -> Option<u64> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).ok()?;
    let mut total_kib = 0u64;
    for line in text.lines() {
        let Some(value) = line
            .strip_prefix("Private_Clean:")
            .or_else(|| line.strip_prefix("Private_Dirty:"))
        else {
            continue;
        };
        total_kib = total_kib.saturating_add(value.split_whitespace().next()?.parse().ok()?);
    }
    Some(total_kib.saturating_mul(1024))
}

#[cfg(unix)]
fn read_proc_io(pid: u32) -> Option<(u64, u64)> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/io")).ok()?;
    let mut read_bytes = None;
    let mut write_bytes = None;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("read_bytes:") {
            read_bytes = value.trim().parse().ok();
        } else if let Some(value) = line.strip_prefix("write_bytes:") {
            write_bytes = value.trim().parse().ok();
        }
    }
    Some((read_bytes?, write_bytes?))
}
