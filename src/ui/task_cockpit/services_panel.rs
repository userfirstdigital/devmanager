//! Task Cockpit services panel projection and action affordances.
//!
//! Reads redacted supervisor snapshots only. Never probes ports, processes, or
//! the filesystem. Ownership phrasing keeps "Managed here" in details.

use crate::services::{
    health::{RedactedServiceSnapshot, ServiceState, StatusTone},
    model::ServiceId,
    supervisor::{BoundedServiceLog, SupervisorAction},
};

/// Visual tone mapped from service state for the cockpit strip.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServicePanelTone {
    Gray,
    Orange,
    Green,
    Blue,
    Red,
    Neutral,
}

impl From<StatusTone> for ServicePanelTone {
    fn from(tone: StatusTone) -> Self {
        match tone {
            StatusTone::Gray => Self::Gray,
            StatusTone::Orange => Self::Orange,
            StatusTone::Green => Self::Green,
            StatusTone::Blue => Self::Blue,
            StatusTone::Red => Self::Red,
            StatusTone::Neutral => Self::Neutral,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServicePanelAction {
    Start,
    Stop,
    Restart,
    Logs,
    Health,
    OpenTerminal,
}

impl ServicePanelAction {
    pub const fn as_supervisor_action(self) -> Option<SupervisorAction> {
        match self {
            Self::Start => Some(SupervisorAction::Start),
            Self::Stop => Some(SupervisorAction::Stop),
            Self::Restart => Some(SupervisorAction::Restart),
            // Logs/Health stay unbound until typed host operations exist.
            Self::Logs | Self::Health | Self::OpenTerminal => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceActionAffordance {
    pub action: ServicePanelAction,
    pub enabled: bool,
    pub disabled_reason: Option<&'static str>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServicePanelRow {
    pub service_id: ServiceId,
    pub state: ServiceState,
    pub tone: ServicePanelTone,
    pub ownership_summary: &'static str,
    pub ownership_detail: Option<&'static str>,
    pub port_label: Option<String>,
    pub dependency_summary: String,
    pub health_summary: String,
    pub actions: Vec<ServiceActionAffordance>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ServicesPanelProjection {
    pub rows: Vec<ServicePanelRow>,
    pub selected_logs: Option<BoundedServiceLog>,
}

/// Project one immutable services panel from redacted snapshots.
pub fn project_services_panel(
    snapshots: &[RedactedServiceSnapshot],
    dependency_labels: &[(ServiceId, Vec<ServiceId>)],
) -> ServicesPanelProjection {
    let mut rows = Vec::with_capacity(snapshots.len());
    for snapshot in snapshots {
        let deps = dependency_labels
            .iter()
            .find(|(id, _)| id == &snapshot.service_id)
            .map(|(_, deps)| deps.as_slice())
            .unwrap_or(&[]);
        rows.push(project_row(snapshot, deps));
    }
    ServicesPanelProjection {
        rows,
        selected_logs: None,
    }
}

fn project_row(snapshot: &RedactedServiceSnapshot, dependencies: &[ServiceId]) -> ServicePanelRow {
    let controllable = snapshot.state.is_controllable();
    let active = matches!(
        snapshot.state,
        ServiceState::Starting | ServiceState::Healthy | ServiceState::Unhealthy
    );
    let stopping = matches!(snapshot.state, ServiceState::Stopping);
    let external = matches!(snapshot.state, ServiceState::External);
    let (ownership_summary, ownership_detail) = match snapshot.state {
        ServiceState::External => ("External listener", None),
        ServiceState::Stopped | ServiceState::Failed | ServiceState::Unknown => {
            ("Configured", None)
        }
        _ => ("Managed", Some("Managed here")),
    };
    let port_label = match snapshot.evidence.port {
        crate::services::health::PortEvidenceKind::Owned => Some("managed port".to_owned()),
        crate::services::health::PortEvidenceKind::External => Some("external port".to_owned()),
        crate::services::health::PortEvidenceKind::Free
        | crate::services::health::PortEvidenceKind::Unknown => None,
    };
    let dependency_summary = if dependencies.is_empty() {
        "No dependencies".to_owned()
    } else {
        format!(
            "{} dependenc{}",
            dependencies.len(),
            if dependencies.len() == 1 { "y" } else { "ies" }
        )
    };
    let health_summary = match snapshot.evidence.health {
        crate::services::health::HealthEvidenceKind::Healthy => "Healthy".to_owned(),
        crate::services::health::HealthEvidenceKind::Unhealthy => "Unhealthy".to_owned(),
        crate::services::health::HealthEvidenceKind::Pending => "Waiting for readiness".to_owned(),
        crate::services::health::HealthEvidenceKind::Disabled => {
            "Health checks disabled".to_owned()
        }
        crate::services::health::HealthEvidenceKind::Stale => "Health evidence stale".to_owned(),
        crate::services::health::HealthEvidenceKind::Cancelled => "Health cancelled".to_owned(),
        crate::services::health::HealthEvidenceKind::Crashed => "Process crashed".to_owned(),
        crate::services::health::HealthEvidenceKind::Unknown => "Health unknown".to_owned(),
    };
    let actions = vec![
        affordance(
            ServicePanelAction::Start,
            controllable && !active && !stopping && !external,
            if external {
                Some("External listeners cannot be started from DevManager")
            } else if active || stopping {
                Some("Service is already running")
            } else if !controllable {
                Some("Service state is not controllable")
            } else {
                None
            },
        ),
        affordance(
            ServicePanelAction::Stop,
            controllable && active,
            if external {
                Some("External listeners cannot be stopped from DevManager")
            } else if stopping {
                Some("Service is already stopping")
            } else if !active {
                Some("Service is not running")
            } else if !controllable {
                Some("Service state is not controllable")
            } else {
                None
            },
        ),
        affordance(
            ServicePanelAction::Restart,
            controllable && active,
            if external {
                Some("External listeners cannot be restarted from DevManager")
            } else if stopping {
                Some("Service is already stopping")
            } else if !active {
                Some("Service is not running")
            } else {
                None
            },
        ),
        affordance(
            ServicePanelAction::Logs,
            false,
            Some("Service log query is not available until a typed host operation exists"),
        ),
        affordance(
            ServicePanelAction::Health,
            false,
            Some("Service health query is not available until a typed host operation exists"),
        ),
        // OpenTerminal has no supervisor/host action yet; keep it visible but
        // disabled with a truthful reason rather than advertising a dead path.
        affordance(
            ServicePanelAction::OpenTerminal,
            false,
            Some("Service terminal attach is not available"),
        ),
    ];
    ServicePanelRow {
        service_id: snapshot.service_id.clone(),
        state: snapshot.state,
        tone: ServicePanelTone::from(snapshot.state.tone()),
        ownership_summary,
        ownership_detail,
        port_label,
        dependency_summary,
        health_summary,
        actions,
    }
}

fn affordance(
    action: ServicePanelAction,
    enabled: bool,
    disabled_reason: Option<&'static str>,
) -> ServiceActionAffordance {
    ServiceActionAffordance {
        action,
        enabled,
        disabled_reason: if enabled { None } else { disabled_reason },
    }
}
