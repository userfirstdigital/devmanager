use serde::{Deserialize, Serialize};

use super::model::{HealthPolicy, ServiceId, ServiceScope};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceState {
    Stopped,
    Starting,
    Healthy,
    Unhealthy,
    External,
    Stopping,
    Failed,
    Unknown,
}

impl ServiceState {
    pub const fn is_controllable(self) -> bool {
        !matches!(self, Self::External | Self::Unknown)
    }

    pub const fn tone(self) -> StatusTone {
        match self {
            Self::Stopped => StatusTone::Gray,
            Self::Starting => StatusTone::Orange,
            Self::Healthy => StatusTone::Green,
            Self::Unhealthy | Self::Failed => StatusTone::Red,
            Self::External => StatusTone::Blue,
            Self::Stopping | Self::Unknown => StatusTone::Neutral,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusTone {
    Gray,
    Orange,
    Green,
    Blue,
    Red,
    Neutral,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum LifecycleAxis {
    Stopped,
    Starting,
    Running,
    Stopping,
    Failed,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ProcessAxis {
    Absent,
    Running { generation: u64 },
    Exited { exit_code: Option<i32> },
    Crashed { generation: u64 },
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum HealthAxis {
    Disabled,
    Pending { next_probe_at_ms: Option<u64> },
    Healthy { last_probe_at_ms: u64 },
    Unhealthy { last_probe_at_ms: u64 },
    Stale { last_probe_at_ms: Option<u64> },
    Cancelled,
    Crashed,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PortAxis {
    Free,
    Owned { port: u16 },
    External { port: u16, owner_pid: Option<u32> },
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum OwnershipAxis {
    None,
    Task { task_id: String },
    Host,
    External,
    Inconsistent,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceSource {
    Lifecycle,
    ProcessRegistry,
    HealthProbe,
    PortSnapshot,
    Admission,
    FakeProbe,
    Test,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EvidenceProvenance {
    pub source: EvidenceSource,
    pub observed_at_ms: u64,
    pub generation: Option<u64>,
    pub epoch: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ServiceEvidence {
    pub lifecycle: LifecycleAxis,
    pub process: ProcessAxis,
    pub health: HealthAxis,
    pub port: PortAxis,
    pub ownership: OwnershipAxis,
    pub generation: u64,
    pub epoch: u64,
    pub observed_at_ms: u64,
    pub provenance: EvidenceProvenance,
}

pub fn reduce_service(evidence: &ServiceEvidence) -> ServiceState {
    if matches!(evidence.port, PortAxis::External { .. })
        || matches!(evidence.ownership, OwnershipAxis::External)
    {
        return ServiceState::External;
    }
    if matches!(evidence.lifecycle, LifecycleAxis::Stopping) {
        return ServiceState::Stopping;
    }
    let unexpected_exit = matches!(
        evidence.process,
        ProcessAxis::Crashed { .. } | ProcessAxis::Exited { .. }
    ) && !matches!(
        evidence.lifecycle,
        LifecycleAxis::Stopped | LifecycleAxis::Stopping
    );
    if matches!(
        evidence.lifecycle,
        LifecycleAxis::Failed | LifecycleAxis::Unknown
    ) || unexpected_exit
        || matches!(evidence.ownership, OwnershipAxis::Inconsistent)
    {
        if matches!(evidence.lifecycle, LifecycleAxis::Unknown)
            && matches!(evidence.process, ProcessAxis::Absent)
            && matches!(evidence.port, PortAxis::Free)
            && matches!(evidence.ownership, OwnershipAxis::None)
        {
            return ServiceState::Unknown;
        }
        return ServiceState::Failed;
    }
    if matches!(evidence.lifecycle, LifecycleAxis::Unknown)
        || matches!(evidence.process, ProcessAxis::Unknown)
        || matches!(evidence.port, PortAxis::Unknown)
        || matches!(evidence.ownership, OwnershipAxis::Unknown)
        || matches!(
            evidence.health,
            HealthAxis::Unknown | HealthAxis::Stale { .. }
        )
    {
        return ServiceState::Unknown;
    }

    let owned = matches!(
        evidence.ownership,
        OwnershipAxis::Task { .. } | OwnershipAxis::Host
    );
    if matches!(evidence.health, HealthAxis::Unhealthy { .. }) && owned {
        return ServiceState::Unhealthy;
    }
    if matches!(evidence.lifecycle, LifecycleAxis::Stopped)
        && matches!(
            evidence.process,
            ProcessAxis::Absent | ProcessAxis::Exited { .. }
        )
        && matches!(evidence.port, PortAxis::Free)
        && matches!(evidence.ownership, OwnershipAxis::None)
    {
        return ServiceState::Stopped;
    }
    if matches!(evidence.lifecycle, LifecycleAxis::Starting) {
        return ServiceState::Starting;
    }
    if matches!(evidence.lifecycle, LifecycleAxis::Running)
        && matches!(evidence.process, ProcessAxis::Running { .. })
        && owned
    {
        return match evidence.health {
            HealthAxis::Healthy { .. } | HealthAxis::Disabled => ServiceState::Healthy,
            HealthAxis::Unhealthy { .. } => ServiceState::Unhealthy,
            HealthAxis::Pending { .. } => ServiceState::Starting,
            HealthAxis::Cancelled | HealthAxis::Crashed => ServiceState::Failed,
            HealthAxis::Unknown | HealthAxis::Stale { .. } => ServiceState::Unknown,
        };
    }
    ServiceState::Unknown
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ProbeOutcome {
    Success,
    Failure,
    Timeout,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FakeClock {
    now_ms: u64,
}

impl FakeClock {
    pub const fn new(now_ms: u64) -> Self {
        Self { now_ms }
    }

    pub const fn now_ms(self) -> u64 {
        self.now_ms
    }

    pub fn advance_ms(&mut self, amount_ms: u64) {
        self.now_ms = self.now_ms.saturating_add(amount_ms);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProbeSchedule {
    pub next_probe_at_ms: Option<u64>,
    pub interval_ms: u64,
    pub cancelled: bool,
}

impl ProbeSchedule {
    pub const fn is_due(self, now_ms: u64) -> bool {
        if self.cancelled {
            return false;
        }
        match self.next_probe_at_ms {
            Some(next_probe_at_ms) => now_ms >= next_probe_at_ms,
            None => false,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HealthTracker {
    policy: HealthPolicy,
    generation: Option<u64>,
    started_at_ms: Option<u64>,
    last_probe_at_ms: Option<u64>,
    next_probe_at_ms: Option<u64>,
    current_interval_ms: u64,
    consecutive_successes: u8,
    consecutive_failures: u8,
    axis: HealthAxis,
    provenance: Option<EvidenceProvenance>,
}

impl HealthTracker {
    pub fn new(policy: HealthPolicy) -> Self {
        Self {
            current_interval_ms: policy.probe_interval_ms,
            policy,
            generation: None,
            started_at_ms: None,
            last_probe_at_ms: None,
            next_probe_at_ms: None,
            consecutive_successes: 0,
            consecutive_failures: 0,
            axis: HealthAxis::Unknown,
            provenance: None,
        }
    }

    pub fn start(&mut self, now_ms: u64, generation: u64) -> Result<(), HealthError> {
        self.generation = Some(generation);
        self.started_at_ms = Some(now_ms);
        self.last_probe_at_ms = None;
        self.next_probe_at_ms = Some(now_ms);
        self.current_interval_ms = self.policy.probe_interval_ms;
        self.consecutive_successes = 0;
        self.consecutive_failures = 0;
        self.provenance = None;
        self.axis = HealthAxis::Pending {
            next_probe_at_ms: Some(now_ms),
        };
        Ok(())
    }

    pub fn schedule(&self) -> ProbeSchedule {
        ProbeSchedule {
            next_probe_at_ms: self.next_probe_at_ms,
            interval_ms: self.current_interval_ms,
            cancelled: matches!(&self.axis, HealthAxis::Cancelled),
        }
    }

    pub fn axis(&self) -> HealthAxis {
        self.axis.clone()
    }

    pub fn provenance(&self) -> Option<EvidenceProvenance> {
        self.provenance
    }

    pub fn record_probe(
        &mut self,
        now_ms: u64,
        generation: u64,
        outcome: ProbeOutcome,
        source: EvidenceSource,
    ) -> Result<(), HealthError> {
        self.check_generation(generation)?;
        let Some(next_probe_at_ms) = self.next_probe_at_ms else {
            return Err(HealthError::NotScheduled);
        };
        if now_ms < next_probe_at_ms {
            return Err(HealthError::NotDue {
                due_at_ms: next_probe_at_ms,
            });
        }
        if matches!(self.axis, HealthAxis::Cancelled | HealthAxis::Crashed) {
            return Err(HealthError::Cancelled);
        }
        self.last_probe_at_ms = Some(now_ms);
        self.provenance = Some(EvidenceProvenance {
            source,
            observed_at_ms: now_ms,
            generation: Some(generation),
            epoch: None,
        });
        match outcome {
            ProbeOutcome::Success => {
                self.consecutive_successes = self.consecutive_successes.saturating_add(1);
                self.consecutive_failures = 0;
                self.current_interval_ms = self.policy.probe_interval_ms;
                if self.consecutive_successes >= self.policy.success_threshold {
                    self.axis = HealthAxis::Healthy {
                        last_probe_at_ms: now_ms,
                    };
                } else {
                    self.axis = HealthAxis::Pending {
                        next_probe_at_ms: Some(now_ms.saturating_add(self.current_interval_ms)),
                    };
                }
            }
            ProbeOutcome::Failure | ProbeOutcome::Timeout | ProbeOutcome::Unavailable => {
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                self.consecutive_successes = 0;
                self.current_interval_ms = self
                    .current_interval_ms
                    .saturating_mul(u64::from(self.policy.backoff_multiplier))
                    .min(self.policy.max_probe_interval_ms);
                if self.consecutive_failures >= self.policy.failure_threshold {
                    self.axis = HealthAxis::Unhealthy {
                        last_probe_at_ms: now_ms,
                    };
                } else {
                    self.axis = HealthAxis::Pending {
                        next_probe_at_ms: Some(now_ms.saturating_add(self.current_interval_ms)),
                    };
                }
            }
        }
        self.next_probe_at_ms = match &self.axis {
            HealthAxis::Unhealthy { .. } => None,
            HealthAxis::Pending { next_probe_at_ms } => *next_probe_at_ms,
            HealthAxis::Healthy { .. } => self
                .last_probe_at_ms
                .map(|last| last.saturating_add(self.current_interval_ms)),
            _ => None,
        };
        Ok(())
    }

    pub fn advance(&mut self, now_ms: u64, generation: u64) -> Result<(), HealthError> {
        self.check_generation(generation)?;
        if matches!(&self.axis, HealthAxis::Cancelled | HealthAxis::Crashed) {
            return Ok(());
        }
        if let Some(started_at_ms) = self.started_at_ms {
            if self.last_probe_at_ms.is_none()
                && now_ms >= started_at_ms.saturating_add(self.policy.startup_deadline_ms)
            {
                self.axis = HealthAxis::Unhealthy {
                    last_probe_at_ms: started_at_ms,
                };
                self.next_probe_at_ms = None;
                return Ok(());
            }
        }
        if let Some(last_probe_at_ms) = self.last_probe_at_ms {
            if now_ms >= last_probe_at_ms.saturating_add(self.policy.stale_after_ms) {
                self.axis = HealthAxis::Stale {
                    last_probe_at_ms: Some(last_probe_at_ms),
                };
                self.next_probe_at_ms = Some(now_ms);
            }
        }
        Ok(())
    }

    pub fn cancel(&mut self, _now_ms: u64, generation: u64) -> Result<(), HealthError> {
        self.check_generation(generation)?;
        self.axis = HealthAxis::Cancelled;
        self.next_probe_at_ms = None;
        Ok(())
    }

    pub fn process_exit(&mut self, _now_ms: u64, generation: u64) -> Result<(), HealthError> {
        self.check_generation(generation)?;
        self.axis = HealthAxis::Crashed;
        self.next_probe_at_ms = None;
        Ok(())
    }

    fn check_generation(&self, generation: u64) -> Result<(), HealthError> {
        match self.generation {
            Some(expected) if expected == generation => Ok(()),
            Some(expected) => Err(HealthError::StaleGeneration {
                expected,
                received: generation,
            }),
            None => Err(HealthError::NotStarted),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HealthError {
    NotStarted,
    NotScheduled,
    NotDue { due_at_ms: u64 },
    Cancelled,
    StaleGeneration { expected: u64, received: u64 },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RedactedServiceSnapshot {
    pub service_id: ServiceId,
    pub scope: ServiceScope,
    pub state: ServiceState,
    pub observed_at_ms: u64,
    pub generation: u64,
    pub epoch: u64,
    pub evidence: RedactedEvidence,
}

impl RedactedServiceSnapshot {
    pub fn from_evidence(
        service_id: ServiceId,
        scope: ServiceScope,
        evidence: &ServiceEvidence,
    ) -> Self {
        Self {
            service_id,
            scope,
            state: reduce_service(evidence),
            observed_at_ms: evidence.observed_at_ms,
            generation: evidence.generation,
            epoch: evidence.epoch,
            evidence: RedactedEvidence::from_evidence(evidence),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RedactedEvidence {
    pub lifecycle: LifecycleEvidenceKind,
    pub process: ProcessEvidenceKind,
    pub health: HealthEvidenceKind,
    pub port: PortEvidenceKind,
    pub ownership: OwnershipEvidenceKind,
    pub provenance: EvidenceProvenance,
}

impl RedactedEvidence {
    fn from_evidence(evidence: &ServiceEvidence) -> Self {
        Self {
            lifecycle: LifecycleEvidenceKind::from(&evidence.lifecycle),
            process: ProcessEvidenceKind::from(&evidence.process),
            health: HealthEvidenceKind::from(&evidence.health),
            port: PortEvidenceKind::from(&evidence.port),
            ownership: OwnershipEvidenceKind::from(&evidence.ownership),
            provenance: evidence.provenance,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleEvidenceKind {
    Stopped,
    Starting,
    Running,
    Stopping,
    Failed,
    Unknown,
}

impl From<&LifecycleAxis> for LifecycleEvidenceKind {
    fn from(value: &LifecycleAxis) -> Self {
        match value {
            LifecycleAxis::Stopped => Self::Stopped,
            LifecycleAxis::Starting => Self::Starting,
            LifecycleAxis::Running => Self::Running,
            LifecycleAxis::Stopping => Self::Stopping,
            LifecycleAxis::Failed => Self::Failed,
            LifecycleAxis::Unknown => Self::Unknown,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessEvidenceKind {
    Absent,
    Running,
    Exited,
    Crashed,
    Unknown,
}

impl From<&ProcessAxis> for ProcessEvidenceKind {
    fn from(value: &ProcessAxis) -> Self {
        match value {
            ProcessAxis::Absent => Self::Absent,
            ProcessAxis::Running { .. } => Self::Running,
            ProcessAxis::Exited { .. } => Self::Exited,
            ProcessAxis::Crashed { .. } => Self::Crashed,
            ProcessAxis::Unknown => Self::Unknown,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthEvidenceKind {
    Disabled,
    Pending,
    Healthy,
    Unhealthy,
    Stale,
    Cancelled,
    Crashed,
    Unknown,
}

impl From<&HealthAxis> for HealthEvidenceKind {
    fn from(value: &HealthAxis) -> Self {
        match value {
            HealthAxis::Disabled => Self::Disabled,
            HealthAxis::Pending { .. } => Self::Pending,
            HealthAxis::Healthy { .. } => Self::Healthy,
            HealthAxis::Unhealthy { .. } => Self::Unhealthy,
            HealthAxis::Stale { .. } => Self::Stale,
            HealthAxis::Cancelled => Self::Cancelled,
            HealthAxis::Crashed => Self::Crashed,
            HealthAxis::Unknown => Self::Unknown,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortEvidenceKind {
    Free,
    Owned,
    External,
    Unknown,
}

impl From<&PortAxis> for PortEvidenceKind {
    fn from(value: &PortAxis) -> Self {
        match value {
            PortAxis::Free => Self::Free,
            PortAxis::Owned { .. } => Self::Owned,
            PortAxis::External { .. } => Self::External,
            PortAxis::Unknown => Self::Unknown,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnershipEvidenceKind {
    None,
    Task,
    Host,
    External,
    Inconsistent,
    Unknown,
}

impl From<&OwnershipAxis> for OwnershipEvidenceKind {
    fn from(value: &OwnershipAxis) -> Self {
        match value {
            OwnershipAxis::None => Self::None,
            OwnershipAxis::Task { .. } => Self::Task,
            OwnershipAxis::Host => Self::Host,
            OwnershipAxis::External => Self::External,
            OwnershipAxis::Inconsistent => Self::Inconsistent,
            OwnershipAxis::Unknown => Self::Unknown,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticCode {
    ProcessExited,
    HealthStale,
    HealthFailed,
    ExternalPort,
    OwnershipInconsistent,
    EvidenceIncomplete,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RedactedDiagnostic {
    pub service_id: ServiceId,
    pub code: DiagnosticCode,
    pub observed_at_ms: u64,
    pub provenance: EvidenceProvenance,
}
