//! Pure task-header and global top-bar projections.
//!
//! These models consume only the already assembled [`ClientModel`] and
//! bounded observations supplied by the caller. They do not probe the host,
//! filesystem, network, provider sessions, or process state.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::client::action::{self, ActionDescriptor, ACTION_HOST_STATUS, ACTION_TASK_SHOW};
use crate::client::ClientModel;
use crate::diagnostics::runner::redact_secrets;
use crate::domain::agent::{AgentRole, AgentSessionFacts, AgentSessionLifecycle};
use crate::domain::id::{AgentSessionId, ProjectId, TaskId};
use crate::domain::snapshot::TaskSnapshot;
use crate::domain::task::{TaskActivity, VisibleTaskStatus, WorkspaceRef};
use crate::ui::actions::{KeyboardShortcut, ShortcutKey};
use crate::ui::components::AccessibleRole;
use crate::ui::shell::PointerButton;

/// Observation values older than this are not shown in the top bar.
pub const PROVIDER_QUOTA_MAX_AGE_MS: i64 = 60 * 60 * 1_000;
/// All top-bar observations use the same canonical freshness window.
pub const MAX_OBSERVATION_AGE_MS: i64 = PROVIDER_QUOTA_MAX_AGE_MS;
/// Keep the header bounded even when a task has many specialist sessions.
pub const MAX_HEADER_SPECIALISTS: usize = 32;
/// Keep the top bar bounded if a caller has not already provider-deduplicated.
pub const MAX_TOP_BAR_QUOTAS: usize = 8;
/// Width at which the title becomes a deterministic single-line ellipsis.
pub const COMPACT_HEADER_WIDTH_PX: u16 = 320;
/// Width at which the title becomes a deterministic two-line projection.
pub const TITLE_WRAP_WIDTH_PX: u16 = 360;
pub const NARROW_HEADER_WIDTH_PX: u16 = 480;
pub const STANDARD_HEADER_WIDTH_PX: u16 = 720;

const MAX_TITLE_SCALARS: usize = 160;
const MAX_PROJECT_SCALARS: usize = 64;
const MAX_PATH_SCALARS: usize = 64;
const MAX_BRANCH_SCALARS: usize = 48;
const MAX_QUOTA_DETAIL_SCALARS: usize = 64;
const MAX_ROLE_SCALARS: usize = 64;
const MAX_ACCESSIBLE_SCALARS: usize = 512;
const COMPACT_TITLE_SCALARS: usize = 28;
const WRAPPED_TITLE_LINE_SCALARS: usize = 28;
const MAX_MACHINE_CPU_INPUT_PERCENT: f64 = 1_000_000.0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskActionContext {
    /// Host/resource generation captured when the row was rendered.
    pub resource_generation: u64,
    /// Connection epoch captured when the row was rendered.
    pub connection_epoch: u64,
    /// Focus epoch captured when the row was rendered.
    pub focus_epoch: u64,
}

impl Default for TaskActionContext {
    fn default() -> Self {
        Self {
            resource_generation: 0,
            connection_epoch: 0,
            focus_epoch: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskIdentity {
    pub task_id: TaskId,
    pub revision: u64,
    pub resource_generation: u64,
    pub connection_epoch: u64,
    pub focus_epoch: u64,
    pub client_epoch: u64,
    pub navigation_epoch: u64,
    pub action_epoch: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentIdentity {
    pub task: TaskIdentity,
    pub agent_id: AgentSessionId,
    pub revision: u64,
    pub resource_generation: u64,
    pub provider_session_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ObservationStamp {
    pub observed_at_ms: i64,
    pub generation: u64,
}

/// The target payload is intentionally separate from the catalog descriptor.
/// It captures the exact current fact needed to reject a stale click while
/// keeping the action id owned by `client::action`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActionTarget {
    Task(TaskIdentity),
    Agent(AgentIdentity),
    Host {
        identity: HostObservationIdentity,
        stamp: ObservationStamp,
    },
    Connect {
        identity: ConnectObservationIdentity,
        stamp: ObservationStamp,
    },
    Update {
        identity: UpdateObservationIdentity,
        stamp: ObservationStamp,
    },
    QuotaSummary {
        stamp: ObservationStamp,
    },
    Quota {
        identity: QuotaObservationIdentity,
        stamp: ObservationStamp,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectedAction {
    descriptor: &'static ActionDescriptor,
    target: ActionTarget,
}

impl ProjectedAction {
    pub fn new(id: &str, target: ActionTarget) -> Self {
        let descriptor = action::descriptor(id)
            .unwrap_or_else(|| panic!("projected action id is not in the shared catalog: {id}"));
        Self { descriptor, target }
    }

    pub fn descriptor(&self) -> &'static ActionDescriptor {
        self.descriptor
    }

    pub fn id(&self) -> &'static str {
        self.descriptor.id
    }

    pub fn target(&self) -> &ActionTarget {
        &self.target
    }
}

/// Compatibility aliases retain the public names from the initial slice but
/// there is now exactly one action representation and one catalog owner.
pub type HeaderAction = ProjectedAction;
pub type TopBarAction = ProjectedAction;

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
    /// Keep a bounded stable ID label instead of inventing a project name.
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
    pub role: AgentRoleProjection,
    /// This is a safe display label, not the provider's raw identity.
    pub provider: String,
    pub lifecycle: AgentSessionLifecycle,
    pub label: String,
    pub accessible_description: String,
    pub action: HeaderAction,
}

/// Bounded public role data. The provider/domain role remains private to the
/// projection boundary; specialist names are sanitized before they can leave
/// this module.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentRoleProjection {
    Primary,
    Specialist { label: String },
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
    pub specialist_total: usize,
    pub specialist_hidden_count: usize,
    pub specialists_truncated: bool,
    pub turn: TurnProjection,
    pub status: StatusLink,
    pub accessible_description: String,
}

impl TaskHeaderModel {
    /// Project a task with the caller's current host/resource, connection,
    /// and focus context. Context is mandatory so actions cannot silently
    /// fall back to an all-zero fence.
    pub fn from_model(
        model: &ClientModel,
        task_id: TaskId,
        context: TaskActionContext,
    ) -> Option<Self> {
        Self::from_model_with_epochs(model, task_id, context, model.last_applied_sequence(), 0)
    }

    /// Compatibility name for callers that already use the explicit context
    /// constructor. New callers should use [`Self::from_model`].
    pub fn from_model_with_context(
        model: &ClientModel,
        task_id: TaskId,
        context: TaskActionContext,
    ) -> Option<Self> {
        Self::from_model(model, task_id, context)
    }

    /// Project a task while capturing all current action-fence epochs. The
    /// client and navigation epochs are supplied by the live Shell rather
    /// than guessed by a pure model projection.
    pub fn from_model_with_epochs(
        model: &ClientModel,
        task_id: TaskId,
        context: TaskActionContext,
        client_epoch: u64,
        navigation_epoch: u64,
    ) -> Option<Self> {
        let task = model.task(task_id)?;
        Some(Self::from_snapshot(
            task_id,
            task,
            context,
            client_epoch,
            navigation_epoch,
        ))
    }

    fn from_snapshot(
        task_id: TaskId,
        task: &TaskSnapshot,
        context: TaskActionContext,
        client_epoch: u64,
        navigation_epoch: u64,
    ) -> Self {
        let identity = TaskIdentity {
            task_id,
            revision: task.task.revision,
            resource_generation: context.resource_generation,
            connection_epoch: context.connection_epoch,
            focus_epoch: context.focus_epoch,
            client_epoch,
            navigation_epoch,
            action_epoch: task.task.action_epoch,
        };
        let visible_status = task.visible_status();
        let (status_label, status_description) = visible_status_copy(visible_status);
        let status = StatusLink {
            label: status_label.to_string(),
            description: status_description.to_string(),
            action: task_action(identity),
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
        if let Some(primary_id) = task.primary_agent_id {
            if let Some(agent) = task.agents.get(&primary_id) {
                if matches!(&agent.role, AgentRole::Primary) {
                    primary = PrimaryAgentProjection::Present(agent_projection(identity, agent));
                }
            }
        }

        let mut specialist_agents: Vec<&AgentSessionFacts> = task
            .agents
            .values()
            .filter(|agent| matches!(&agent.role, AgentRole::Specialist { .. }))
            .collect();
        specialist_agents.sort_by(|left, right| {
            specialist_name(left)
                .cmp(specialist_name(right))
                .then_with(|| left.id.cmp(&right.id))
        });
        let specialist_total = specialist_agents.len();
        let specialists = specialist_agents
            .into_iter()
            .take(MAX_HEADER_SPECIALISTS)
            .map(|agent| agent_projection(identity, agent))
            .collect::<Vec<_>>();
        let specialist_hidden_count = specialist_total.saturating_sub(specialists.len());

        let project = ProjectProjection {
            id: task.task.project_id,
            label: presentation_text(&task.task.project_id.to_string(), MAX_PROJECT_SCALARS),
        };
        let workspace = workspace_projection(&task.task.workspace);
        let title = presentation_text(&task.task.title, MAX_TITLE_SCALARS);
        let accessible_description = format_accessible_header(
            &title,
            &project,
            &workspace,
            &primary,
            &turn,
            specialist_total,
            specialist_hidden_count,
        );

        Self {
            identity,
            title,
            project,
            workspace,
            primary,
            specialists,
            specialist_total,
            specialist_hidden_count,
            specialists_truncated: specialist_hidden_count != 0,
            turn,
            status,
            accessible_description,
        }
    }

    /// A projected header action is accepted only while its exact task or
    /// agent identity is still present in this current snapshot.
    pub fn accepts_action(&self, action: &HeaderAction) -> bool {
        if action.id() != ACTION_TASK_SHOW {
            return false;
        }
        match action.target() {
            ActionTarget::Task(identity) => identity == &self.identity,
            ActionTarget::Agent(identity) => {
                if identity.task != self.identity {
                    return false;
                }
                matches!(&self.primary, PrimaryAgentProjection::Present(agent) if agent.identity == *identity)
                    || self
                        .specialists
                        .iter()
                        .any(|agent| agent.identity == *identity)
            }
            ActionTarget::Host { .. }
            | ActionTarget::Connect { .. }
            | ActionTarget::Update { .. }
            | ActionTarget::QuotaSummary { .. }
            | ActionTarget::Quota { .. } => false,
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
            description: "Open More task details to read additional task information.".to_string(),
            action: task_action(self.identity),
            role: AccessibleRole::Button,
            focusable: true,
            pointer: PointerButton::Primary,
            keyboard: KeyboardShortcut::ctrl(ShortcutKey::Character('m')),
        });
        let accessible_description = match &overflow_control {
            Some(control) => presentation_text(
                &format!("{}. {}", self.accessible_description, control.description),
                MAX_ACCESSIBLE_SCALARS,
            ),
            None => self.accessible_description.clone(),
        };

        HeaderLayout {
            inline,
            overflow,
            overflow_control,
            title: title_layout(&self.title, width_px),
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
pub enum TitleLayout {
    SingleLine(String),
    Truncated(String),
    Wrapped(Vec<String>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OverflowControl {
    pub label: String,
    pub description: String,
    pub action: ProjectedAction,
    pub role: AccessibleRole,
    pub focusable: bool,
    pub pointer: PointerButton,
    pub keyboard: KeyboardShortcut,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HeaderLayout {
    pub inline: Vec<HeaderField>,
    pub overflow: Vec<HeaderField>,
    pub overflow_control: Option<OverflowControl>,
    pub title: TitleLayout,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CpuInputUnit {
    /// Legacy process/resource samples count one fully busy logical CPU as
    /// 100. A value of 125 therefore means 1.25 core equivalents.
    LegacyCoreTotalPercent,
    /// The sample is already Task-Manager-style whole-machine percent.
    MachinePercent,
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
pub struct ConnectObservationIdentity {
    pub host_id: String,
    pub connection_epoch: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectObservation {
    pub identity: ConnectObservationIdentity,
    pub state: ConnectState,
    pub observed_at_ms: Option<i64>,
    pub generation: Option<u64>,
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
    pub observed_at_ms: Option<i64>,
    pub generation: Option<u64>,
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
    pub observed_at_ms: Option<i64>,
    pub generation: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostResourceObservation {
    /// This value is meaningful only with the explicit input unit below.
    pub cpu_percent: Option<f64>,
    pub cpu_input_unit: CpuInputUnit,
    pub memory_bytes: Option<u64>,
    pub logical_cpu_count: Option<u32>,
    pub cpu_observed_at_ms: Option<i64>,
    pub memory_observed_at_ms: Option<i64>,
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
    pub quotas: Vec<QuotaObservation>,
    pub resources: Option<HostResourceObservation>,
}

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
    pub input_unit: CpuInputUnit,
    /// Normal UI CPU is whole-machine percentage in `0..=100`.
    pub whole_machine_percent: f64,
    /// Present only when the input was a raw legacy core-total sample.
    pub diagnostic: Option<CpuDiagnostic>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CpuDiagnostic {
    pub label: String,
    pub core_equivalent: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HostResourceProjection {
    pub cpu: Option<CpuProjection>,
    pub memory_bytes: Option<u64>,
    pub cpu_observed_at_ms: Option<i64>,
    pub memory_observed_at_ms: Option<i64>,
    pub generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TopBarUnavailable {
    HostStatus,
    ConnectionStatus,
    UpdateStatus,
    Cpu,
    Memory,
    Quota,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TopBarModel {
    pub host: Option<TopBarStatusLink>,
    pub connect: Option<TopBarStatusLink>,
    pub update: Option<TopBarStatusLink>,
    pub quotas: Vec<QuotaProjection>,
    pub quota_hidden_count: usize,
    pub quotas_truncated: bool,
    pub quota_overflow_action: Option<TopBarAction>,
    pub resources: Option<HostResourceProjection>,
    pub unavailable: Vec<TopBarUnavailable>,
    pub accessible_description: String,
}

impl TopBarModel {
    pub fn from_input(input: &TopBarProjectionInput) -> Self {
        let host = input.host.as_ref().and_then(|observation| {
            fresh_stamp(observation.observed_at_ms, observation.generation, input)
                .map(|stamp| host_status_link(observation, stamp))
        });
        let connect = input.connect.as_ref().and_then(|observation| {
            fresh_stamp(observation.observed_at_ms, observation.generation, input)
                .map(|stamp| connect_status_link(observation, stamp))
        });
        let update = input.update.as_ref().and_then(|observation| {
            fresh_stamp(observation.observed_at_ms, observation.generation, input)
                .map(|stamp| update_status_link(observation, stamp))
        });
        let quota_result = fresh_quotas(&input.quotas, input);
        let quota_hidden_count = quota_result.hidden_count;
        let quotas = quota_result.visible;
        let quota_overflow_action = (quota_hidden_count != 0).then(|| {
            ProjectedAction::new(
                ACTION_HOST_STATUS,
                ActionTarget::QuotaSummary {
                    stamp: ObservationStamp {
                        observed_at_ms: input.now_ms,
                        generation: input.generation,
                    },
                },
            )
        });
        let resources = input
            .resources
            .as_ref()
            .and_then(|observation| resource_projection(observation, input));

        let mut descriptions = Vec::new();
        let mut unavailable = Vec::new();
        if let Some(host) = &host {
            descriptions.push(host.description.clone());
        } else if input.host.is_some() {
            unavailable.push(TopBarUnavailable::HostStatus);
            descriptions.push("Host status unavailable.".to_string());
        }
        if let Some(connect) = &connect {
            descriptions.push(connect.description.clone());
        } else if input.connect.is_some() {
            unavailable.push(TopBarUnavailable::ConnectionStatus);
            descriptions.push("Host connection status unavailable.".to_string());
        }
        if let Some(update) = &update {
            descriptions.push(update.description.clone());
        } else if input.update.is_some() {
            unavailable.push(TopBarUnavailable::UpdateStatus);
            descriptions.push("Update status unavailable.".to_string());
        }
        if quota_result.unavailable_count != 0 {
            unavailable.push(TopBarUnavailable::Quota);
            descriptions.push(format!(
                "Quota details unavailable for {} provider{}.",
                quota_result.unavailable_count,
                if quota_result.unavailable_count == 1 {
                    ""
                } else {
                    "s"
                }
            ));
        }
        if quota_hidden_count != 0 {
            descriptions.push(format!(
                "{} quotas shown, {} hidden. Open host status for remaining quota details.",
                quotas.len(),
                quota_hidden_count
            ));
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
            } else if input
                .resources
                .as_ref()
                .is_some_and(|observation| observation.cpu_percent.is_some())
            {
                unavailable.push(TopBarUnavailable::Cpu);
                descriptions.push("Host CPU unavailable.".to_string());
            }
            if resources.memory_bytes.is_some() {
                descriptions.push("Host memory is available.".to_string());
            } else if input
                .resources
                .as_ref()
                .is_some_and(|observation| observation.memory_bytes.is_some())
            {
                unavailable.push(TopBarUnavailable::Memory);
                descriptions.push("Host memory unavailable.".to_string());
            }
        } else if let Some(observation) = &input.resources {
            if observation.cpu_percent.is_some() {
                unavailable.push(TopBarUnavailable::Cpu);
                descriptions.push("Host CPU unavailable.".to_string());
            }
            if observation.memory_bytes.is_some() {
                unavailable.push(TopBarUnavailable::Memory);
                descriptions.push("Host memory unavailable.".to_string());
            }
        }

        Self {
            host,
            connect,
            update,
            quotas,
            quota_hidden_count,
            quotas_truncated: quota_hidden_count != 0,
            quota_overflow_action,
            resources,
            unavailable,
            accessible_description: presentation_text(
                &descriptions.join(" "),
                MAX_ACCESSIBLE_SCALARS,
            ),
        }
    }

    /// Accept a top-bar action only if its exact observation identity and
    /// generation still match a currently projected fact.
    pub fn accepts_action(&self, action: &TopBarAction) -> bool {
        if action.id() != ACTION_HOST_STATUS {
            return false;
        }
        self.host
            .as_ref()
            .is_some_and(|link| link.action == *action)
            || self
                .connect
                .as_ref()
                .is_some_and(|link| link.action == *action)
            || self
                .update
                .as_ref()
                .is_some_and(|link| link.action == *action)
            || self
                .quota_overflow_action
                .as_ref()
                .is_some_and(|overflow| overflow == action)
            || self.quotas.iter().any(|quota| quota.action == *action)
    }
}

fn task_action(identity: TaskIdentity) -> ProjectedAction {
    ProjectedAction::new(ACTION_TASK_SHOW, ActionTarget::Task(identity))
}

fn agent_projection(task: TaskIdentity, agent: &AgentSessionFacts) -> AgentProjection {
    let identity = AgentIdentity {
        task,
        agent_id: agent.id,
        revision: agent.revision,
        resource_generation: agent.runtime_generation,
        provider_session_id: agent.provider_session_id.clone(),
    };
    let (role, label) = match &agent.role {
        AgentRole::Primary => (AgentRoleProjection::Primary, "Primary".to_string()),
        AgentRole::Specialist { name } => {
            let label = presentation_text(name, MAX_ROLE_SCALARS);
            (
                AgentRoleProjection::Specialist {
                    label: label.clone(),
                },
                label,
            )
        }
    };
    let provider = provider_label(&agent.provider_kind);
    let accessible_description = presentation_text(
        &format!("{} provider {} is {:?}.", label, provider, agent.lifecycle),
        MAX_ACCESSIBLE_SCALARS,
    );
    AgentProjection {
        identity: identity.clone(),
        role,
        provider,
        lifecycle: agent.lifecycle,
        label,
        accessible_description,
        action: ProjectedAction::new(ACTION_TASK_SHOW, ActionTarget::Agent(identity)),
    }
}

fn specialist_name(agent: &AgentSessionFacts) -> &str {
    match &agent.role {
        AgentRole::Specialist { name } => name,
        AgentRole::Primary => "",
    }
}

fn workspace_projection(workspace: &WorkspaceRef) -> WorkspaceProjection {
    match workspace {
        WorkspaceRef::Main => WorkspaceProjection::Main,
        WorkspaceRef::Worktree { path: _, branch } => WorkspaceProjection::Worktree {
            path: PathBuf::from("workspace"),
            branch: presentation_text(branch, MAX_BRANCH_SCALARS),
        },
        WorkspaceRef::External { path: _ } => WorkspaceProjection::External {
            path: PathBuf::from("workspace"),
        },
    }
}

fn workspace_label(workspace: &WorkspaceProjection) -> String {
    match workspace {
        WorkspaceProjection::Main => "main workspace".to_string(),
        WorkspaceProjection::Worktree { path, branch } => format!(
            "worktree {} at {}",
            presentation_text(branch, MAX_BRANCH_SCALARS),
            shorten_path(path)
        ),
        WorkspaceProjection::External { path } => {
            format!("external workspace at {}", shorten_path(path))
        }
    }
}

fn format_accessible_header(
    title: &str,
    project: &ProjectProjection,
    workspace: &WorkspaceProjection,
    primary: &PrimaryAgentProjection,
    turn: &TurnProjection,
    specialist_total: usize,
    specialist_hidden_count: usize,
) -> String {
    let primary = match primary {
        PrimaryAgentProjection::Present(agent) => {
            format!("Primary provider {}", agent.provider)
        }
        PrimaryAgentProjection::Unavailable { label, .. } => label.clone(),
    };
    presentation_text(
        &format!(
            "{title}. {specialist_total} {} shown, {specialist_hidden_count} hidden. Project {}. {}. {}. {}.",
            if specialist_total == 1 {
                "specialist"
            } else {
                "specialists"
            },
            project.label,
            workspace_label(workspace),
            primary,
            turn.description,
        ),
        MAX_ACCESSIBLE_SCALARS,
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

fn host_status_link(observation: &HostObservation, stamp: ObservationStamp) -> TopBarStatusLink {
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
        action: ProjectedAction::new(
            ACTION_HOST_STATUS,
            ActionTarget::Host {
                identity: observation.identity.clone(),
                stamp,
            },
        ),
    }
}

fn connect_status_link(
    observation: &ConnectObservation,
    stamp: ObservationStamp,
) -> TopBarStatusLink {
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
        action: ProjectedAction::new(
            ACTION_HOST_STATUS,
            ActionTarget::Connect {
                identity: observation.identity.clone(),
                stamp,
            },
        ),
    }
}

fn update_status_link(
    observation: &UpdateObservation,
    stamp: ObservationStamp,
) -> TopBarStatusLink {
    let label = match observation.state {
        UpdateState::Disabled => "Updates disabled",
        UpdateState::Idle => "Updates idle",
        UpdateState::Checking => "Checking for updates",
        UpdateState::UpToDate => "Up to date",
        UpdateState::Available => "Update available",
        UpdateState::Downloading => "Downloading update",
        UpdateState::ReadyToInstall => "Update ready to install",
        UpdateState::Installing => "Installing update",
        UpdateState::Error => "Update check failed",
    };
    let label = label.to_string();
    let description = format!("{label}. Open the update surface for details.");
    TopBarStatusLink {
        status: TopBarStatus::Update(observation.state),
        label,
        description,
        action: ProjectedAction::new(
            ACTION_HOST_STATUS,
            ActionTarget::Update {
                identity: observation.identity.clone(),
                stamp,
            },
        ),
    }
}

struct QuotaProjectionResult {
    visible: Vec<QuotaProjection>,
    hidden_count: usize,
    unavailable_count: usize,
}

fn fresh_quotas<'a>(
    observations: &'a [QuotaObservation],
    input: &TopBarProjectionInput,
) -> QuotaProjectionResult {
    let mut latest_by_provider =
        BTreeMap::<String, (&'a QuotaObservation, ObservationStamp)>::new();
    for observation in observations {
        let Some(stamp) = fresh_stamp(observation.observed_at_ms, observation.generation, input)
        else {
            continue;
        };
        let provider_key = provider_key(&observation.identity.provider);
        let replace = latest_by_provider
            .get(&provider_key)
            .is_none_or(|(current, _)| quota_is_newer(observation, current));
        if replace {
            latest_by_provider.insert(provider_key, (observation, stamp));
        }
    }

    let mut projected = Vec::new();
    let mut unavailable_count = 0;
    for (observation, stamp) in latest_by_provider.into_values() {
        let Some(detail) = observation.detail.as_deref().and_then(safe_quota_detail) else {
            unavailable_count += 1;
            continue;
        };
        let Some(age_ms) = input.now_ms.checked_sub(stamp.observed_at_ms) else {
            unavailable_count += 1;
            continue;
        };
        projected.push(QuotaProjection {
            identity: observation.identity.clone(),
            provider: provider_label(&observation.identity.provider),
            detail,
            age_ms,
            action: ProjectedAction::new(
                ACTION_HOST_STATUS,
                ActionTarget::Quota {
                    identity: observation.identity.clone(),
                    stamp,
                },
            ),
        });
    }

    let hidden_count = projected.len().saturating_sub(MAX_TOP_BAR_QUOTAS);
    projected.truncate(MAX_TOP_BAR_QUOTAS);
    QuotaProjectionResult {
        visible: projected,
        hidden_count,
        unavailable_count,
    }
}

fn quota_is_newer(candidate: &QuotaObservation, current: &QuotaObservation) -> bool {
    (
        candidate.observed_at_ms,
        candidate.identity.observation_id,
        candidate.identity.provider_session_id.as_str(),
        candidate.detail.as_deref().unwrap_or_default(),
    ) > (
        current.observed_at_ms,
        current.identity.observation_id,
        current.identity.provider_session_id.as_str(),
        current.detail.as_deref().unwrap_or_default(),
    )
}

fn resource_projection(
    observation: &HostResourceObservation,
    input: &TopBarProjectionInput,
) -> Option<HostResourceProjection> {
    let cpu_stamp = fresh_stamp(
        observation.cpu_observed_at_ms,
        observation.generation,
        input,
    );
    let memory_stamp = fresh_stamp(
        observation.memory_observed_at_ms,
        observation.generation,
        input,
    );
    let cpu = cpu_stamp.and_then(|_| {
        observation.cpu_percent.and_then(|value| {
            cpu_projection(
                value,
                observation.cpu_input_unit,
                observation.logical_cpu_count,
            )
        })
    });
    let memory_bytes = memory_stamp.and_then(|_| observation.memory_bytes);
    (cpu.is_some() || memory_bytes.is_some()).then(|| HostResourceProjection {
        cpu_observed_at_ms: cpu
            .as_ref()
            .and(cpu_stamp)
            .map(|stamp| stamp.observed_at_ms),
        memory_observed_at_ms: memory_bytes
            .as_ref()
            .and(memory_stamp)
            .map(|stamp| stamp.observed_at_ms),
        cpu,
        memory_bytes,
        generation: input.generation,
    })
}

fn cpu_projection(
    value: f64,
    input_unit: CpuInputUnit,
    logical_cpu_count: Option<u32>,
) -> Option<CpuProjection> {
    if !value.is_finite() || value < 0.0 {
        return None;
    }

    let (whole_machine_percent, diagnostic) = match input_unit {
        CpuInputUnit::LegacyCoreTotalPercent => {
            let logical_cpu_count = logical_cpu_count.filter(|count| *count > 0)?;
            let logical_cpu_count = f64::from(logical_cpu_count);
            // Bound only absurd samples; do not use one-core 100% as the
            // admission ceiling because normalization must happen first.
            let maximum_raw_percent = logical_cpu_count * MAX_MACHINE_CPU_INPUT_PERCENT;
            if !maximum_raw_percent.is_finite() || value > maximum_raw_percent {
                return None;
            }
            let core_equivalent = value / 100.0;
            let whole_machine_percent = value / logical_cpu_count;
            if !core_equivalent.is_finite() || !whole_machine_percent.is_finite() {
                return None;
            }
            (
                whole_machine_percent,
                Some(CpuDiagnostic {
                    label: "Core-equivalent CPU (diagnostic)".to_string(),
                    core_equivalent,
                }),
            )
        }
        CpuInputUnit::MachinePercent => {
            if value > MAX_MACHINE_CPU_INPUT_PERCENT {
                return None;
            }
            (value, None)
        }
    };

    Some(CpuProjection {
        input_unit,
        whole_machine_percent: whole_machine_percent.clamp(0.0, 100.0),
        diagnostic,
    })
}

fn fresh_stamp(
    observed_at_ms: Option<i64>,
    generation: Option<u64>,
    input: &TopBarProjectionInput,
) -> Option<ObservationStamp> {
    let observed_at_ms = observed_at_ms?;
    let generation = generation?;
    if input.now_ms < 0 || observed_at_ms < 0 || generation != input.generation {
        return None;
    }
    let age_ms = input.now_ms.checked_sub(observed_at_ms)?;
    (0..=MAX_OBSERVATION_AGE_MS)
        .contains(&age_ms)
        .then_some(ObservationStamp {
            observed_at_ms,
            generation,
        })
}

fn provider_label(provider: &str) -> String {
    match provider.trim().to_ascii_lowercase().as_str() {
        "claude" | "claude_code" => "Claude".to_string(),
        "codex" => "Codex".to_string(),
        _ => "Provider".to_string(),
    }
}

fn provider_key(provider: &str) -> String {
    match provider.trim().to_ascii_lowercase().as_str() {
        "claude" | "claude_code" => "claude".to_string(),
        "codex" => "codex".to_string(),
        other => other.to_string(),
    }
}

fn safe_quota_detail(detail: &str) -> Option<String> {
    let detail = detail.trim();
    if detail.is_empty() {
        return None;
    }
    for token in
        detail.split(|character: char| character.is_whitespace() || ",;".contains(character))
    {
        let Some(percent) = token.strip_suffix('%') else {
            continue;
        };
        let Ok(percent) = percent.parse::<f64>() else {
            continue;
        };
        if percent.is_finite() && (0.0..=100.0).contains(&percent) {
            return Some(presentation_text(
                &format!("{percent:.0}% remaining"),
                MAX_QUOTA_DETAIL_SCALARS,
            ));
        }
    }
    Some("Quota available".to_string())
}

fn presentation_text(value: &str, max_scalars: usize) -> String {
    let controls_removed = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let redacted = redact_secrets(&controls_removed);
    let input_words = redacted.split_whitespace().collect::<Vec<_>>();
    let mut words = Vec::with_capacity(input_words.len());
    let mut index = 0;
    while index < input_words.len() {
        let word = input_words[index];
        if is_sensitive_presentation_word(word) {
            words.push("[redacted]");
            index += 1;
            continue;
        }
        if is_key_prefix_word(word)
            && input_words
                .get(index + 1)
                .is_some_and(|next| is_key_suffix_word(next))
        {
            words.push("[redacted]");
            index += 2;
            continue;
        }
        words.push(word);
        index += 1;
    }
    let value = words.join(" ");
    if value.is_empty() {
        return "Unavailable".to_string();
    }
    truncate_scalars(&value, max_scalars)
}

fn is_sensitive_presentation_word(word: &str) -> bool {
    let compact = word
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(|character| character.to_lowercase())
        .collect::<String>();
    [
        "secret",
        "token",
        "password",
        "credential",
        "apikey",
        "accesskey",
        "privatekey",
        "authorization",
    ]
    .iter()
    .any(|marker| compact.contains(marker))
}

fn is_key_prefix_word(word: &str) -> bool {
    matches!(
        word.trim_matches(|character: char| !character.is_ascii_alphanumeric())
            .to_ascii_lowercase()
            .as_str(),
        "api" | "access" | "private"
    )
}

fn is_key_suffix_word(word: &str) -> bool {
    word.trim_matches(|character: char| !character.is_ascii_alphanumeric())
        .to_ascii_lowercase()
        .starts_with("key")
}

fn shorten_path(path: &PathBuf) -> String {
    let raw = path.to_string_lossy();
    let parts: Vec<&str> = raw
        .split(['\\', '/'])
        .filter(|part| !part.is_empty())
        .collect();
    if parts.is_empty() {
        return "workspace".to_string();
    }
    if parts.iter().any(|part| {
        let lower = part.to_ascii_lowercase();
        [
            "secret",
            "token",
            "password",
            "credential",
            "apikey",
            "privatekey",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
    }) {
        return "workspace".to_string();
    }
    let suffix = parts.iter().rev().take(2).copied().collect::<Vec<_>>();
    let suffix = suffix.into_iter().rev().collect::<Vec<_>>().join("/");
    presentation_text(&format!("…/{suffix}"), MAX_PATH_SCALARS)
}

fn truncate_scalars(value: &str, max_scalars: usize) -> String {
    if value.chars().count() <= max_scalars {
        return value.to_string();
    }
    let mut truncated = value
        .chars()
        .take(max_scalars.saturating_sub(1))
        .collect::<String>();
    truncated.push('…');
    truncated
}

fn title_layout(title: &str, width_px: u16) -> TitleLayout {
    if width_px <= COMPACT_HEADER_WIDTH_PX {
        return TitleLayout::Truncated(truncate_scalars(title, COMPACT_TITLE_SCALARS));
    }
    if width_px <= TITLE_WRAP_WIDTH_PX {
        let chars: Vec<char> = title.chars().collect();
        let mut lines = Vec::new();
        for index in 0..2 {
            let start = index * WRAPPED_TITLE_LINE_SCALARS;
            if start >= chars.len() {
                break;
            }
            let end = (start + WRAPPED_TITLE_LINE_SCALARS).min(chars.len());
            let mut line = chars[start..end].iter().copied().collect::<String>();
            if index == 1 && end < chars.len() {
                line = truncate_scalars(&line, WRAPPED_TITLE_LINE_SCALARS);
            }
            lines.push(line);
        }
        return TitleLayout::Wrapped(lines);
    }
    TitleLayout::SingleLine(title.to_string())
}
