//! Pure task-header and global top-bar projections.
//!
//! These models consume only the already assembled [`ClientModel`] and
//! bounded observations supplied by the caller. They do not probe the host,
//! filesystem, network, provider sessions, or process state.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::client::ClientModel;
use crate::domain::agent::{AgentRole, AgentSessionFacts, AgentSessionLifecycle};
use crate::domain::id::{AgentSessionId, ProjectId, TaskId};
use crate::domain::snapshot::TaskSnapshot;
use crate::domain::task::{TaskActivity, VisibleTaskStatus, WorkspaceRef};

/// Quota observations older than this are not shown in the top bar.
pub const PROVIDER_QUOTA_MAX_AGE_MS: i64 = 60 * 60 * 1_000;
/// Keep the header bounded even when a task has many specialist sessions.
pub const MAX_HEADER_SPECIALISTS: usize = 32;
/// Keep the top bar bounded if a caller has not already provider-deduplicated.
pub const MAX_TOP_BAR_QUOTAS: usize = 8;
pub const NARROW_HEADER_WIDTH_PX: u16 = 480;
pub const STANDARD_HEADER_WIDTH_PX: u16 = 720;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskIdentity {
    pub task_id: TaskId,
    pub revision: u64,
    pub action_epoch: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentIdentity {
    pub task_id: TaskId,
    pub agent_id: AgentSessionId,
    pub runtime_generation: u64,
    pub provider_session_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HeaderAction {
    OpenCommandCenter { identity: TaskIdentity },
    OpenAgent { identity: AgentIdentity },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusLink {
    pub label: String,
    pub description: String,
    pub action: HeaderAction,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectProjection {
    pub id: ProjectId,
    /// The client contract has a ProjectId but no project-name projection.
    /// Keep the stable ID as the display label instead of inventing a name.
    pub label: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceProjection {
    Main,
    Worktree { path: PathBuf, branch: String },
    External { path: PathBuf },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentProjection {
    pub identity: AgentIdentity,
    pub role: AgentRole,
    pub provider: String,
    pub lifecycle: AgentSessionLifecycle,
    pub label: String,
    pub accessible_description: String,
    pub action: HeaderAction,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PrimaryAgentProjection {
    Present(AgentProjection),
    Unavailable {
        identity: TaskIdentity,
        label: String,
        description: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnProjection {
    pub activity: TaskActivity,
    pub status: VisibleTaskStatus,
    pub status_link: StatusLink,
    pub label: String,
    pub description: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskHeaderModel {
    pub identity: TaskIdentity,
    pub title: String,
    pub project: ProjectProjection,
    pub workspace: WorkspaceProjection,
    pub primary: PrimaryAgentProjection,
    pub specialists: Vec<AgentProjection>,
    pub specialists_truncated: bool,
    pub turn: TurnProjection,
    pub status: StatusLink,
    pub accessible_description: String,
}

impl TaskHeaderModel {
    pub fn from_model(model: &ClientModel, task_id: TaskId) -> Option<Self> {
        let task = model.task(task_id)?;
        Some(Self::from_snapshot(task_id, task))
    }

    fn from_snapshot(task_id: TaskId, task: &TaskSnapshot) -> Self {
        let identity = TaskIdentity {
            task_id,
            revision: task.task.revision,
            action_epoch: task.task.action_epoch,
        };
        let visible_status = task.visible_status();
        let (status_label, status_description) = visible_status_copy(visible_status);
        let status = StatusLink {
            label: status_label.to_string(),
            description: status_description.to_string(),
            action: HeaderAction::OpenCommandCenter { identity },
        };
        let turn = TurnProjection {
            activity: task.activity,
            status: visible_status,
            status_link: status.clone(),
            label: status.label.clone(),
            description: status.description.clone(),
        };

        let mut primary = PrimaryAgentProjection::Unavailable {
            identity,
            label: "Primary provider unavailable".to_string(),
            description: "Primary provider unavailable. Open Command Center to inspect the task."
                .to_string(),
        };
        let mut specialists = Vec::new();
        for agent in task.agents.values() {
            match &agent.role {
                AgentRole::Primary if task.primary_agent_id == Some(agent.id) => {
                    primary = PrimaryAgentProjection::Present(agent_projection(task_id, agent));
                }
                AgentRole::Specialist { .. } if specialists.len() < MAX_HEADER_SPECIALISTS => {
                    specialists.push(agent_projection(task_id, agent));
                }
                AgentRole::Specialist { .. } => {}
                AgentRole::Primary => {}
            }
        }

        let specialists_truncated = task
            .agents
            .values()
            .filter(|agent| matches!(agent.role, AgentRole::Specialist { .. }))
            .count()
            > specialists.len();
        let project = ProjectProjection {
            id: task.task.project_id,
            label: task.task.project_id.to_string(),
        };
        let workspace = workspace_projection(&task.task.workspace);
        let accessible_description = format_accessible_header(
            &task.task.title,
            &project,
            &workspace,
            &primary,
            &turn,
            specialists.len(),
        );

        Self {
            identity,
            title: task.task.title.clone(),
            project,
            workspace,
            primary,
            specialists,
            specialists_truncated,
            turn,
            status,
            accessible_description,
        }
    }

    /// Return the fields that remain inline at a width and move lower
    /// priority fields into a text-labelled overflow control. The renderer
    /// can use the returned fields without measuring or clipping strings.
    pub fn responsive_layout(&self, width_px: u16) -> HeaderLayout {
        let mut inline = vec![HeaderField::Title, HeaderField::TurnStatus];
        let overflow = if width_px < NARROW_HEADER_WIDTH_PX {
            vec![
                HeaderField::Project,
                HeaderField::Workspace,
                HeaderField::Primary,
                HeaderField::Specialists,
            ]
        } else if width_px < STANDARD_HEADER_WIDTH_PX {
            inline.extend([HeaderField::Project, HeaderField::Primary]);
            vec![HeaderField::Workspace, HeaderField::Specialists]
        } else {
            inline.extend([
                HeaderField::Project,
                HeaderField::Workspace,
                HeaderField::Primary,
                HeaderField::Specialists,
            ]);
            Vec::new()
        };

        let overflow_control = (!overflow.is_empty()).then(|| OverflowControl {
            label: "More task details".to_string(),
            description: "Open More task details to read the project, workspace, Primary, and specialist information.".to_string(),
        });
        let accessible_description = match &overflow_control {
            Some(control) => format!("{}. {}", self.accessible_description, control.description),
            None => self.accessible_description.clone(),
        };

        HeaderLayout {
            inline,
            overflow,
            overflow_control,
            accessible_description,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HeaderField {
    Title,
    Project,
    Workspace,
    Primary,
    Specialists,
    TurnStatus,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OverflowControl {
    pub label: String,
    pub description: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HeaderLayout {
    pub inline: Vec<HeaderField>,
    pub overflow: Vec<HeaderField>,
    pub overflow_control: Option<OverflowControl>,
    pub accessible_description: String,
}

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
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectObservationIdentity {
    pub host_id: String,
    pub connection_epoch: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectObservation {
    pub identity: ConnectObservationIdentity,
    pub state: ConnectState,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateObservationIdentity {
    pub current_version: String,
    pub target_version: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateObservation {
    pub identity: UpdateObservationIdentity,
    pub state: UpdateState,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaObservationIdentity {
    pub provider: String,
    pub provider_session_id: String,
    pub observation_id: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaObservation {
    pub identity: QuotaObservationIdentity,
    /// `None` is an unavailable observation. It is intentionally omitted
    /// from the top bar; Command Center owns the diagnostic explanation.
    pub detail: Option<String>,
    pub observed_at_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostResourceObservation {
    /// The caller supplies the already normalized whole-machine percentage;
    /// this projection never turns a core-scaled sample into user-facing UI.
    pub cpu_percent: Option<f32>,
    pub memory_bytes: Option<u64>,
    pub logical_cpu_count: Option<u32>,
    pub observed_at_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TopBarProjectionInput {
    pub now_ms: i64,
    pub host: Option<HostObservation>,
    pub connect: Option<ConnectObservation>,
    pub update: Option<UpdateObservation>,
    pub quotas: Vec<QuotaObservation>,
    pub resources: Option<HostResourceObservation>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TopBarStatus {
    Host(HostHealth),
    Connect(ConnectState),
    Update(UpdateState),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TopBarAction {
    OpenHostDiagnostics {
        identity: HostObservationIdentity,
    },
    OpenConnectDiagnostics {
        identity: ConnectObservationIdentity,
    },
    OpenUpdate {
        identity: UpdateObservationIdentity,
    },
    OpenQuotaDetails {
        identity: QuotaObservationIdentity,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TopBarStatusLink {
    pub status: TopBarStatus,
    pub label: String,
    pub description: String,
    pub action: TopBarAction,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuotaProjection {
    pub identity: QuotaObservationIdentity,
    pub provider: String,
    pub detail: String,
    pub age_ms: i64,
    pub action: TopBarAction,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CpuProjection {
    /// Normal UI CPU is whole-machine percentage in `0..=100`.
    pub whole_machine_percent: f32,
    /// Optional diagnostics value. Its label explicitly says it is not the
    /// normal CPU percentage shown to users.
    pub diagnostic: Option<CpuDiagnostic>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CpuDiagnostic {
    pub label: String,
    pub core_equivalent: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HostResourceProjection {
    pub cpu: Option<CpuProjection>,
    pub memory_bytes: Option<u64>,
    pub observed_at_ms: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TopBarModel {
    pub host: Option<TopBarStatusLink>,
    pub connect: Option<TopBarStatusLink>,
    pub update: Option<TopBarStatusLink>,
    pub quotas: Vec<QuotaProjection>,
    pub resources: Option<HostResourceProjection>,
    pub accessible_description: String,
}

impl TopBarModel {
    pub fn from_input(input: &TopBarProjectionInput) -> Self {
        let host = input.host.as_ref().map(host_status_link);
        let connect = input.connect.as_ref().map(connect_status_link);
        let update = input.update.as_ref().map(update_status_link);
        let quotas = fresh_quotas(&input.quotas, input.now_ms);
        let resources = input.resources.as_ref().and_then(resource_projection);

        let mut descriptions = Vec::new();
        if let Some(host) = &host {
            descriptions.push(host.description.clone());
        }
        if let Some(connect) = &connect {
            descriptions.push(connect.description.clone());
        }
        if let Some(update) = &update {
            descriptions.push(update.description.clone());
        }
        for quota in &quotas {
            descriptions.push(format!("{} quota: {}.", quota.provider, quota.detail));
        }
        if let Some(resources) = &resources {
            if let Some(cpu) = &resources.cpu {
                descriptions.push(format!(
                    "Whole-machine CPU {:.1}%.",
                    cpu.whole_machine_percent
                ));
            }
            if let Some(memory_bytes) = resources.memory_bytes {
                descriptions.push(format!("Host memory: {memory_bytes} bytes."));
            }
        }

        Self {
            host,
            connect,
            update,
            quotas,
            resources,
            accessible_description: descriptions.join(" "),
        }
    }
}

fn agent_projection(task_id: TaskId, agent: &AgentSessionFacts) -> AgentProjection {
    let identity = AgentIdentity {
        task_id,
        agent_id: agent.id,
        runtime_generation: agent.runtime_generation,
        provider_session_id: agent.provider_session_id.clone(),
    };
    let label = match &agent.role {
        AgentRole::Primary => "Primary".to_string(),
        AgentRole::Specialist { name } => name.clone(),
    };
    let accessible_description = format!(
        "{} provider {} is {:?}.",
        label, agent.provider_kind, agent.lifecycle
    );
    AgentProjection {
        identity: identity.clone(),
        role: agent.role.clone(),
        provider: agent.provider_kind.clone(),
        lifecycle: agent.lifecycle,
        label,
        accessible_description,
        action: HeaderAction::OpenAgent { identity },
    }
}

fn workspace_projection(workspace: &WorkspaceRef) -> WorkspaceProjection {
    match workspace {
        WorkspaceRef::Main => WorkspaceProjection::Main,
        WorkspaceRef::Worktree { path, branch } => WorkspaceProjection::Worktree {
            path: path.clone(),
            branch: branch.clone(),
        },
        WorkspaceRef::External { path } => WorkspaceProjection::External { path: path.clone() },
    }
}

fn workspace_label(workspace: &WorkspaceProjection) -> String {
    match workspace {
        WorkspaceProjection::Main => "main workspace".to_string(),
        WorkspaceProjection::Worktree { path, branch } => {
            format!("worktree {branch} at {}", path.display())
        }
        WorkspaceProjection::External { path } => {
            format!("external workspace at {}", path.display())
        }
    }
}

fn format_accessible_header(
    title: &str,
    project: &ProjectProjection,
    workspace: &WorkspaceProjection,
    primary: &PrimaryAgentProjection,
    turn: &TurnProjection,
    specialist_count: usize,
) -> String {
    let primary = match primary {
        PrimaryAgentProjection::Present(agent) => {
            format!("Primary provider {}", agent.provider)
        }
        PrimaryAgentProjection::Unavailable { label, .. } => label.clone(),
    };
    format!(
        "{title}. Project {}. {}. {}. {}. {} specialist{}.",
        project.label,
        workspace_label(workspace),
        primary,
        turn.description,
        specialist_count,
        if specialist_count == 1 { "" } else { "s" },
    )
}

fn visible_status_copy(status: VisibleTaskStatus) -> (&'static str, &'static str) {
    match status {
        VisibleTaskStatus::Disconnected => (
            "Disconnected",
            "Task is disconnected. Open Command Center to inspect host connectivity.",
        ),
        VisibleTaskStatus::Failed => (
            "Failed",
            "Task failed. Open Command Center to inspect the failure.",
        ),
        VisibleTaskStatus::UncertainOutcome => (
            "Outcome uncertain",
            "Task outcome is uncertain. Open Command Center to inspect and reconcile it.",
        ),
        VisibleTaskStatus::NeedsApproval => (
            "Needs approval",
            "Task needs approval. Open Command Center to review the pending approval.",
        ),
        VisibleTaskStatus::NeedsAnswer => (
            "Needs answer",
            "Task needs an answer. Open Command Center to review the pending question.",
        ),
        VisibleTaskStatus::Working => ("Working", "Task is working."),
        VisibleTaskStatus::Settling => ("Settling", "Task is settling its current turn."),
        VisibleTaskStatus::ReadyForReview => ("Ready for review", "Task is ready for review."),
        VisibleTaskStatus::Idle => ("Idle", "Task is idle."),
    }
}

fn host_status_link(observation: &HostObservation) -> TopBarStatusLink {
    let (label, description) = match observation.health {
        HostHealth::Healthy => (
            "Host healthy",
            "Host is healthy. Open Command Center for host details.",
        ),
        HostHealth::Degraded => (
            "Host degraded",
            "Host is degraded. Open Command Center for host diagnostics.",
        ),
        HostHealth::Unavailable => (
            "Host unavailable",
            "Host is unavailable. Open Command Center for host diagnostics.",
        ),
    };
    TopBarStatusLink {
        status: TopBarStatus::Host(observation.health),
        label: label.to_string(),
        description: description.to_string(),
        action: TopBarAction::OpenHostDiagnostics {
            identity: observation.identity.clone(),
        },
    }
}

fn connect_status_link(observation: &ConnectObservation) -> TopBarStatusLink {
    let (label, description) = match observation.state {
        ConnectState::Connected => (
            "Connected",
            "Host connection is connected. Open Command Center for connection details.",
        ),
        ConnectState::Connecting => (
            "Connecting",
            "Host connection is connecting. Open Command Center for connection details.",
        ),
        ConnectState::Disconnected => (
            "Disconnected",
            "Host connection is disconnected. Open Command Center for connection details.",
        ),
        ConnectState::Failed => (
            "Connection failed",
            "Host connection failed. Open Command Center for connection diagnostics.",
        ),
    };
    TopBarStatusLink {
        status: TopBarStatus::Connect(observation.state),
        label: label.to_string(),
        description: description.to_string(),
        action: TopBarAction::OpenConnectDiagnostics {
            identity: observation.identity.clone(),
        },
    }
}

fn update_status_link(observation: &UpdateObservation) -> TopBarStatusLink {
    let label = match observation.state {
        UpdateState::Disabled => "Updates disabled".to_string(),
        UpdateState::Idle => "Updates idle".to_string(),
        UpdateState::Checking => "Checking for updates".to_string(),
        UpdateState::UpToDate => format!("Up to date · v{}", observation.identity.current_version),
        UpdateState::Available => format!(
            "Update available · {}",
            observation
                .identity
                .target_version
                .as_deref()
                .unwrap_or("new version")
        ),
        UpdateState::Downloading => "Downloading update".to_string(),
        UpdateState::ReadyToInstall => "Update ready to install".to_string(),
        UpdateState::Installing => "Installing update".to_string(),
        UpdateState::Error => "Update check failed".to_string(),
    };
    let description = format!("{label}. Open the update surface for details.");
    TopBarStatusLink {
        status: TopBarStatus::Update(observation.state),
        label,
        description,
        action: TopBarAction::OpenUpdate {
            identity: observation.identity.clone(),
        },
    }
}

fn fresh_quotas(observations: &[QuotaObservation], now_ms: i64) -> Vec<QuotaProjection> {
    let mut latest_by_provider = BTreeMap::<String, &QuotaObservation>::new();
    for observation in observations {
        let replace = latest_by_provider
            .get(&observation.identity.provider)
            .is_none_or(|current| current.observed_at_ms < observation.observed_at_ms);
        if replace {
            latest_by_provider.insert(observation.identity.provider.clone(), observation);
        }
    }

    latest_by_provider
        .into_values()
        .filter_map(|observation| {
            let age_ms = now_ms.saturating_sub(observation.observed_at_ms).max(0);
            let detail = observation.detail.as_ref()?.clone();
            (age_ms <= PROVIDER_QUOTA_MAX_AGE_MS).then(|| QuotaProjection {
                identity: observation.identity.clone(),
                provider: observation.identity.provider.clone(),
                detail,
                age_ms,
                action: TopBarAction::OpenQuotaDetails {
                    identity: observation.identity.clone(),
                },
            })
        })
        .take(MAX_TOP_BAR_QUOTAS)
        .collect()
}

fn resource_projection(observation: &HostResourceObservation) -> Option<HostResourceProjection> {
    let cpu = observation.cpu_percent.and_then(|value| {
        if !value.is_finite() || value < 0.0 {
            return None;
        }
        let whole_machine_percent = value.clamp(0.0, 100.0);
        let diagnostic = observation
            .logical_cpu_count
            .filter(|count| *count > 0)
            .map(|count| CpuDiagnostic {
                label: "Core-equivalent CPU (diagnostic)".to_string(),
                core_equivalent: whole_machine_percent * count as f32 / 100.0,
            });
        Some(CpuProjection {
            whole_machine_percent,
            diagnostic,
        })
    });
    (cpu.is_some() || observation.memory_bytes.is_some()).then(|| HostResourceProjection {
        cpu,
        memory_bytes: observation.memory_bytes,
        observed_at_ms: observation.observed_at_ms,
    })
}
