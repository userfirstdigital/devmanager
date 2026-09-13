//! Host-owned Task Cockpit service projection contracts.
//!
//! Snapshots are redacted supervisor facts only. They never carry command
//! lines, environment values, secrets, or arbitrary host paths. Control stays
//! on the existing ServiceControl / ConfiguredServiceSupervisor path.

use crate::domain::cockpit::{
    TaskServiceHealth, TaskServiceLogLine, TaskServiceLogs, TaskServiceProjection,
    TaskServiceRuntimeState, TaskServiceScope, TaskServiceSnapshot,
};
use crate::domain::id::ConfiguredServiceId;
use crate::domain::TaskId;
use crate::services::health::{RedactedServiceSnapshot, ServiceState};
use crate::services::model::{ServiceId, ServiceScope, ValidationError};
use crate::services::supervisor::{
    BoundedServiceLog, MAX_SERVICE_LOG_LINES, MAX_SERVICE_LOG_LINE_BYTES,
};

/// Fail-closed visibility rule for one selected Task cockpit.
///
/// Host-scoped (project/workspace) services remain visible. Task-scoped
/// services are included only when their exact TaskId matches. Foreign task
/// scopes are excluded.
pub fn filter_snapshots_for_task(
    snapshots: &[RedactedServiceSnapshot],
    task_id: TaskId,
) -> Vec<RedactedServiceSnapshot> {
    snapshots
        .iter()
        .filter(|snapshot| snapshot_visible_to_task(snapshot, task_id))
        .cloned()
        .collect()
}

pub fn snapshot_visible_to_task(snapshot: &RedactedServiceSnapshot, task_id: TaskId) -> bool {
    match &snapshot.scope {
        ServiceScope::Host => true,
        ServiceScope::Task {
            task_id: scoped_task,
        } => *scoped_task == task_id,
    }
}

/// Host-assembled, redacted service strip for one Task cockpit selection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskServiceCockpitProjection {
    pub task_id: TaskId,
    pub snapshots: Vec<RedactedServiceSnapshot>,
}

impl TaskServiceCockpitProjection {
    pub fn from_host_snapshots(task_id: TaskId, snapshots: &[RedactedServiceSnapshot]) -> Self {
        Self {
            task_id,
            snapshots: filter_snapshots_for_task(snapshots, task_id),
        }
    }
}

/// Convert the dedicated configured-service catalog identity once at the
/// supervisor boundary. This is never [`crate::domain::id::ServiceId`].
pub fn supervisor_service_id(id: &ConfiguredServiceId) -> Result<ServiceId, ValidationError> {
    ServiceId::new(id.as_str())
}

pub fn to_wire_projection(
    projection: TaskServiceCockpitProjection,
) -> Result<TaskServiceProjection, crate::domain::id::ConfiguredServiceIdError> {
    let snapshots = projection
        .snapshots
        .into_iter()
        .map(to_wire_snapshot)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(TaskServiceProjection {
        task_id: projection.task_id,
        snapshots,
    })
}

pub fn to_wire_logs(
    task_id: TaskId,
    service_id: ConfiguredServiceId,
    log: BoundedServiceLog,
) -> Result<TaskServiceLogs, crate::domain::id::ConfiguredServiceIdError> {
    if service_id.as_str() != log.service_id.as_str() {
        return Err(crate::domain::id::ConfiguredServiceIdError::InvalidIdentifier);
    }
    let truncated = log.truncated || log.lines.len() > MAX_SERVICE_LOG_LINES;
    let lines = log
        .lines
        .into_iter()
        .take(MAX_SERVICE_LOG_LINES)
        .map(|line| {
            let text = crate::domain::cockpit::truncate_to_max_bytes(
                &line.text,
                MAX_SERVICE_LOG_LINE_BYTES,
            );
            TaskServiceLogLine {
                observed_at_ms: line.observed_at_ms,
                generation: line.generation,
                text,
            }
        })
        .collect();
    Ok(TaskServiceLogs {
        task_id,
        service_id,
        generation: log.generation,
        lines,
        truncated,
    })
}

pub fn to_wire_health(
    task_id: TaskId,
    snapshot: RedactedServiceSnapshot,
) -> Result<TaskServiceHealth, crate::domain::id::ConfiguredServiceIdError> {
    Ok(TaskServiceHealth {
        task_id,
        snapshot: to_wire_snapshot(snapshot)?,
    })
}

fn to_wire_snapshot(
    snapshot: RedactedServiceSnapshot,
) -> Result<TaskServiceSnapshot, crate::domain::id::ConfiguredServiceIdError> {
    Ok(TaskServiceSnapshot {
        service_id: ConfiguredServiceId::new(snapshot.service_id.as_str())?,
        scope: match snapshot.scope {
            ServiceScope::Host => TaskServiceScope::Host,
            ServiceScope::Task { task_id } => TaskServiceScope::Task { task_id },
        },
        state: match snapshot.state {
            ServiceState::Stopped => TaskServiceRuntimeState::Stopped,
            ServiceState::Starting => TaskServiceRuntimeState::Starting,
            ServiceState::Healthy => TaskServiceRuntimeState::Healthy,
            ServiceState::Unhealthy => TaskServiceRuntimeState::Unhealthy,
            ServiceState::External => TaskServiceRuntimeState::External,
            ServiceState::Stopping => TaskServiceRuntimeState::Stopping,
            ServiceState::Failed => TaskServiceRuntimeState::Failed,
            ServiceState::Unknown => TaskServiceRuntimeState::Unknown,
        },
        generation: snapshot.generation,
        epoch: snapshot.epoch,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::id::ServiceId as DomainServiceId;
    use crate::services::health::{
        EvidenceProvenance, EvidenceSource, HealthEvidenceKind, LifecycleEvidenceKind,
        OwnershipEvidenceKind, PortEvidenceKind, ProcessEvidenceKind, RedactedEvidence,
    };

    #[test]
    fn supervisor_id_conversion_stays_on_the_configured_string() {
        let configured = ConfiguredServiceId::new("api").expect("catalog id");
        let supervisor = supervisor_service_id(&configured).expect("supervisor id");
        assert_eq!(supervisor.as_str(), "api");
        assert!(DomainServiceId::parse(configured.as_str()).is_err());
    }

    #[test]
    fn filter_keeps_host_and_selected_task_and_drops_foreign_task() {
        let selected = TaskId::new();
        let foreign = TaskId::new();
        let snapshots = [
            redacted("api", ServiceScope::Host),
            redacted("worker", ServiceScope::Task { task_id: selected }),
            redacted("other", ServiceScope::Task { task_id: foreign }),
        ];
        let visible = filter_snapshots_for_task(&snapshots, selected);
        assert_eq!(visible.len(), 2);
        assert_eq!(visible[0].service_id.as_str(), "api");
        assert_eq!(visible[1].service_id.as_str(), "worker");
        let wire = to_wire_projection(TaskServiceCockpitProjection::from_host_snapshots(
            selected, &snapshots,
        ))
        .expect("wire");
        assert_eq!(wire.task_id, selected);
        assert_eq!(wire.snapshots.len(), 2);
        assert_eq!(wire.snapshots[0].service_id.as_str(), "api");
    }

    #[test]
    fn wire_logs_reuse_supervisor_bounds_and_exact_configured_id() {
        use crate::services::supervisor::RedactedLogLine;

        let task_id = TaskId::new();
        let service_id = ConfiguredServiceId::new("api").expect("catalog");
        let mut lines = Vec::new();
        for index in 0..(MAX_SERVICE_LOG_LINES + 4) {
            lines.push(RedactedLogLine {
                observed_at_ms: index as u64,
                generation: 1,
                text: "x".repeat(MAX_SERVICE_LOG_LINE_BYTES + 8),
            });
        }
        let logs = to_wire_logs(
            task_id,
            service_id.clone(),
            BoundedServiceLog {
                service_id: ServiceId::new("api").expect("id"),
                generation: 7,
                lines,
                truncated: false,
            },
        )
        .expect("wire logs");
        assert_eq!(logs.task_id, task_id);
        assert_eq!(logs.service_id.as_str(), "api");
        assert_eq!(logs.lines.len(), MAX_SERVICE_LOG_LINES);
        assert!(logs.truncated);
        assert!(logs
            .lines
            .iter()
            .all(|line| line.text.len() <= MAX_SERVICE_LOG_LINE_BYTES));
        let unicode = to_wire_logs(
            task_id,
            service_id.clone(),
            BoundedServiceLog {
                service_id: ServiceId::new("api").expect("id"),
                generation: 1,
                lines: vec![RedactedLogLine {
                    observed_at_ms: 1,
                    generation: 1,
                    text: "é".repeat(200),
                }],
                truncated: false,
            },
        )
        .expect("unicode logs");
        assert!(unicode.lines[0].text.len() <= MAX_SERVICE_LOG_LINE_BYTES);
        assert!(unicode.lines[0]
            .text
            .is_char_boundary(unicode.lines[0].text.len()));
        assert!(to_wire_logs(
            task_id,
            ConfiguredServiceId::new("other").expect("other"),
            BoundedServiceLog {
                service_id: ServiceId::new("api").expect("id"),
                generation: 1,
                lines: Vec::new(),
                truncated: false,
            },
        )
        .is_err());
    }

    fn redacted(id: &str, scope: ServiceScope) -> RedactedServiceSnapshot {
        RedactedServiceSnapshot {
            service_id: ServiceId::new(id).expect("id"),
            scope,
            state: ServiceState::Stopped,
            observed_at_ms: 1,
            generation: 1,
            epoch: 1,
            evidence: RedactedEvidence {
                lifecycle: LifecycleEvidenceKind::Stopped,
                process: ProcessEvidenceKind::Absent,
                health: HealthEvidenceKind::Disabled,
                port: PortEvidenceKind::Free,
                ownership: OwnershipEvidenceKind::None,
                provenance: EvidenceProvenance {
                    source: EvidenceSource::Test,
                    observed_at_ms: 1,
                    generation: Some(1),
                    epoch: Some(1),
                },
            },
        }
    }
}
