//! Single host-owned CommandBus executor boundary.
//!
//! Transport connection tasks never mutate the bus or projections directly.
//! They submit decoded requests through [`HostRequestHandle`]; one
//! [`HostRequestExecutor`] task exclusively owns [`CommandBus`] and services
//! them in arrival order. The executor also owns the bounded SnapshotSession,
//! EventReplaySession, and ArtifactContentSession registries for paged snapshot,
//! event-replay, and artifact-content queries.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::pin::{pin, Pin};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use futures_util::stream::{FuturesUnordered, StreamExt};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot, watch, Notify, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;
use tokio::time::{interval_at, MissedTickBehavior};
use uuid::Uuid;

use crate::config::{ConfigCommand, ConfigError, ConfigErrorKind, ConfigStore, Nullable, Project};
use crate::domain::agent::{AgentRole, AgentSessionFacts};
use crate::domain::cockpit::{TaskCockpitQuery, TaskCockpitResult};
use crate::domain::command::{
    ArmUpdateInstallIntent, Command, CommandEnvelope, CommandReceipt, ConfirmUpdateDrainIntent,
    CreateTaskIntent, CreateTaskRequestIntent, PrepareUpdateIntent, SetTaskAttentionIntent,
};
use crate::domain::event::DomainEvent;
use crate::domain::id::{
    ArtifactId, OperationId, QuestionId, RequestId, ResourceId, SnapshotId, SubscriptionId, TaskId,
    TerminalId,
};
use crate::domain::query::{
    Query, QueryEnvelope, QueryError, QueryOutcome, QueryReply, QueryResult,
};
use crate::domain::resource::{
    OwnerKind, ResourceFacts, ResourceKind, ResourceLifecycle, ResourceRecipe,
};
use crate::domain::snapshot::{PageLimits, SnapshotSection};
use crate::domain::terminal_facts::{
    HostTerminalFact, TERMINAL_ACTIVITY_COALESCE_MS, TERMINAL_CWD_DEBOUNCE_MS,
};
use crate::domain::ClientId;
use crate::domain::{AgentSessionId, PresentProviderQuestionIntent};
use crate::kernel::{
    ArtifactContentError, ArtifactContentRegistry, CommandBus, EventReplaySession, ReplayError,
    SessionScope, SnapshotError, SnapshotSession, StoreError, TerminalFactOutcome,
};
use crate::protocol::{
    Capability, CapabilitySet, ClientRequest, DetachAck, DetachRequest, FrameLimits,
    NegotiatedParameters, ServerMessage, StreamFrame, StreamKey, UpdateHandoffReply,
};
use crate::state::SessionDimensions;
use crate::terminal::protocol::{CloseReason, TerminalSpec};
use crate::terminal::service::AttachedTerminalRuntime;
use crate::terminal::service::TerminalService;
#[cfg(test)]
use crate::workspace::WorkspaceProjectRootsError;
use crate::workspace::{
    WorkspaceAuthorization, WorkspaceChoice, WorkspaceProjectRoots, WorkspaceRequest,
    WorkspaceResourceCoordinator, WorkspaceService,
};

use super::ipc::IpcError;
use super::shutdown::{
    HostCleanupProgress, HostCleanupWorker, ProcessEmptyTeardown, ProcessEmptyTeardownWorker,
};

/// Fixed capacity for the host request queue.
///
/// When the queue is full, [`HostRequestHandle::execute`] awaits send capacity
/// (bounded backpressure). Requests are never silently dropped.
pub const HOST_REQUEST_QUEUE_CAPACITY: usize = 32;

/// Provider discovery/start may wait on provider-owned startup UI. Keep a
/// small bounded lane so one task waiting for trust cannot head-of-line block
/// an unrelated provider conversation, while still protecting the host from a
/// restart stampede.
const MAX_CONCURRENT_PROVIDER_RESTORES: usize = 2;

/// One unavailable conversation must not head-of-line block newer provider
/// input. Every claimed unit is still individually leased and fenced by the
/// kernel store; this only bounds how many due units the host examines in one
/// maintenance turn.
const MAX_PROVIDER_DISPATCH_UNITS_PER_PASS: usize = 64;

/// Durable provider input is driven immediately on acceptance and again after
/// exact-session restore. The periodic lane is only a recovery fallback; a
/// short idle cadence avoids rewriting the same pre-boundary hold every second.
const PROVIDER_DISPATCH_MAINTENANCE_PERIOD: Duration = Duration::from_secs(5);

fn task_resource_blocks_archive_after_cleanup_error(
    resource: &ResourceFacts,
    task_id: TaskId,
) -> bool {
    resource.owner_kind == OwnerKind::Task
        && resource.task_id == Some(task_id)
        && matches!(
            resource.lifecycle,
            ResourceLifecycle::Active | ResourceLifecycle::Releasing
        )
}

/// Does provider cleanup have to wait for the resource release loop?
///
/// Plain shells are deliberately invisible here. They are closed directly by
/// the release loop rather than through a process-empty durable release, and
/// counting one would flip an ordinary provider task onto the deferred-cleanup
/// branch the moment its owner opened a shell -- a timing change to the
/// provider lane caused by something with no provider in it. Provider cleanup
/// timing must be exactly what it was before plain shells existed.
/// Why one live plain-shell link no longer has a durable terminal behind it,
/// or `None` while it still does.
///
/// A durable terminal disappears exactly when its resource is released: the
/// projector drops the `terminal_facts` row with it. So a `shell_sessions`
/// entry with no row, or whose resource is gone or `Released`, is a live PTY
/// that nothing can reach any more.
///
/// `Releasing` is deliberately NOT orphaned. It is the ordinary transient a
/// close passes through -- the client renders it as a muted "?" chip -- and
/// closing on it would tear down a shell the host is already retiring in
/// order, or one whose release is still in flight.
fn orphaned_shell_reason(
    resource_id: ResourceId,
    snapshot: Option<&crate::domain::snapshot::TaskSnapshot>,
) -> Option<&'static str> {
    // A shell cannot outlive the task that owns it.
    let Some(snapshot) = snapshot else {
        return Some("its task is gone");
    };
    match snapshot.resources.get(&resource_id) {
        None => Some("its resource is gone"),
        Some(resource) if resource.lifecycle == ResourceLifecycle::Released => {
            Some("its resource was released")
        }
        Some(_) if !snapshot.terminal_facts.contains_key(&resource_id) => {
            Some("its durable terminal is gone")
        }
        Some(_) => None,
    }
}

fn provider_cleanup_requires_resource_release_first<'a>(
    resources: impl IntoIterator<Item = &'a ResourceFacts>,
    task_id: TaskId,
) -> bool {
    resources.into_iter().any(|resource| {
        !resource.recipe.is_plain_shell()
            && task_resource_blocks_archive_after_cleanup_error(resource, task_id)
    })
}

fn run_provider_dispatch_pass(
    bus: &mut CommandBus,
    runtime: &crate::providers::dispatch::ProviderDispatchRuntime,
) -> Result<
    Vec<(
        TaskId,
        crate::providers::dispatch::ProviderDispatchHoldReason,
    )>,
    StoreError,
> {
    let mut held = Vec::new();
    for _ in 0..MAX_PROVIDER_DISPATCH_UNITS_PER_PASS {
        match bus.run_provider_dispatch(runtime)? {
            crate::providers::dispatch::ProviderDispatchOutcome::Idle => break,
            crate::providers::dispatch::ProviderDispatchOutcome::Held { task_id, reason } => {
                held.push((task_id, reason));
            }
            crate::providers::dispatch::ProviderDispatchOutcome::Settled { .. }
            | crate::providers::dispatch::ProviderDispatchOutcome::Uncertain { .. }
            | crate::providers::dispatch::ProviderDispatchOutcome::Recovered { .. } => {}
        }
    }
    Ok(held)
}

fn provider_start_requires_existing_binding(
    mode: crate::domain::command::ProviderStartMode,
) -> bool {
    !matches!(
        mode,
        crate::domain::command::ProviderStartMode::NewConversation
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderSemanticIngress {
    Question {
        task_id: TaskId,
        question_id: QuestionId,
    },
}

struct ProviderRestoreOutcome {
    task_id: TaskId,
    action_epoch: u64,
    result: Result<(), String>,
}

type ProviderRestoreFuture = Pin<Box<dyn Future<Output = ProviderRestoreOutcome> + Send + 'static>>;

fn normalize_provider_question_draft(
    draft: &mut crate::remote::presentation::SemanticEventDraft,
) -> Option<ProviderSemanticIngress> {
    let provider_question_id = match &draft.kind {
        crate::remote::presentation::SemanticEventKind::Question { question_id, .. } => {
            question_id.clone()
        }
        _ => return None,
    };
    let task_id = draft
        .stable_session_key
        .as_str()
        .strip_prefix("tab:")
        .and_then(|value| TaskId::parse(value).ok())?;

    let mut hasher = Sha256::new();
    hasher.update(b"devmanager/provider-question/v1\0");
    hasher.update(task_id.as_bytes());
    hasher.update(provider_question_id.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let question_id = QuestionId::from_bytes(bytes).ok()?;
    if let crate::remote::presentation::SemanticEventKind::Question {
        question_id: semantic_question_id,
        ..
    } = &mut draft.kind
    {
        *semantic_question_id = question_id.to_string();
    }
    Some(ProviderSemanticIngress::Question {
        task_id,
        question_id,
    })
}

/// Bounded retry ledger for lost authenticated PrepareUpdate replies.
const MAX_PREPARED_UPDATE_HANDOFFS: usize = 4;

/// Default critical output lane capacity for one duplex connection.
pub(crate) const HOST_CRITICAL_OUTPUT_QUEUE_CAPACITY: usize = 32;

/// Default durable event output lane capacity for one duplex connection.
pub(crate) const HOST_DURABLE_OUTPUT_QUEUE_CAPACITY: usize = 32;

/// Default ephemeral stream output lane capacity for one duplex connection.
pub(crate) const HOST_EPHEMERAL_OUTPUT_QUEUE_CAPACITY: usize = 32;

/// Hard upper bound for every per-connection output lane.  Capacities are
/// host-selected today, but clamping at the constructor keeps a future
/// negotiated or test-provided value from turning into an allocation bomb.
const MAX_OUTPUT_LANE_CAPACITY: usize = 4_096;

const MAX_SNAPSHOT_SESSIONS: usize = 32;
const SNAPSHOT_IDLE_TTL: Duration = Duration::from_secs(30);
const SNAPSHOT_REAPER_PERIOD: Duration = Duration::from_secs(1);

const MAX_EVENT_REPLAY_SESSIONS: usize = 32;
const EVENT_REPLAY_IDLE_TTL: Duration = Duration::from_secs(30);
const EVENT_REPLAY_REAPER_PERIOD: Duration = Duration::from_secs(1);

/// One absolute deadline for all quit-terminal high-water ack waits.
const QUIT_TERMINAL_ACK_TIMEOUT: Duration = Duration::from_secs(5);

fn session_scope(
    negotiated: NegotiatedParameters,
    task_id: Option<TaskId>,
    output_id: Option<ConnectionOutputId>,
) -> SessionScope {
    SessionScope {
        client_id: Some(negotiated.client_id),
        task_id,
        connection_id: output_id.map(ConnectionOutputId::as_uuid),
        // The host currently has no resumable runtime-generation token in the
        // query envelope. Keep the fields explicit and fail closed on a future
        // caller that supplies them rather than silently inferring them.
        action_epoch: None,
        runtime_generation: None,
    }
}

/// Capacity-one supervisor arm request: drop the pending listener before ack.
#[derive(Debug)]
pub struct PhysicalExitArmRequest {
    pub operation_id: OperationId,
    pub action_epoch: u64,
    pub ack: oneshot::Sender<()>,
}

#[cfg(test)]
mod workspace_security_tests {
    use std::process::Command as ProcessCommand;

    use super::{
        dispatch_authenticated_request, dispatch_authenticated_request_with_workspace_projects,
        normalize_provider_question_draft, normalize_task_create_at_host, ProviderSemanticIngress,
    };
    use crate::domain::agent::{AgentRole, AgentSessionFacts};
    use crate::domain::agent_resource::AgentResourceBinding;
    use crate::domain::command::{
        Command, CommandEnvelope, CreateTaskIntent, CreateTaskRequestIntent,
        StartProviderSessionIntent,
    };
    use crate::domain::resource::{OwnerKind, ResourceFacts, ResourceKind, ResourceRecipe};
    use crate::domain::task::{
        ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity,
        WorkspaceRef,
    };
    use crate::domain::{ClientId, CommandId, EnvironmentId, ProjectId, RequestId, TaskId};
    use crate::host::IpcError;
    use crate::kernel::CommandBus;
    use crate::protocol::{CapabilitySet, ClientRequest};
    use crate::providers::ProviderKind;
    use crate::remote::presentation::{
        SemanticEventDraft, SemanticEventKind, SemanticRetention, SemanticSource, StableSessionKey,
    };
    use crate::workspace::{WorkspaceProjectRoots, WorkspaceRequest};
    use uuid::Uuid;

    #[test]
    fn ai_acceptance_semantic_question_gets_stable_durable_identity() {
        let task_id = TaskId::new();
        let draft = || SemanticEventDraft {
            stable_session_key: StableSessionKey::from_tab(task_id.to_string()),
            occurred_at_epoch_ms: 42,
            source: SemanticSource::Claude,
            kind: SemanticEventKind::Question {
                question_id: "toolu-provider-question".to_string(),
                prompt: "Pick a color".to_string(),
                choices: vec!["Blue".to_string(), "Green".to_string()],
            },
            retention: SemanticRetention::Canonical,
            deduplication_key: Some("provider-question".to_string()),
        };
        let mut first = draft();
        let mut duplicate = draft();

        let first_ingress = normalize_provider_question_draft(&mut first);
        let duplicate_ingress = normalize_provider_question_draft(&mut duplicate);

        assert_eq!(first_ingress, duplicate_ingress);
        assert!(matches!(
            first_ingress,
            Some(ProviderSemanticIngress::Question {
                task_id: correlated_task_id,
                question_id,
            }) if correlated_task_id == task_id
                && matches!(first.kind, SemanticEventKind::Question { question_id: ref semantic_id, .. } if semantic_id == &question_id.to_string())
        ));
    }

    #[test]
    fn authenticated_legacy_create_cannot_persist_client_supplied_workspace() {
        let directory = tempfile::tempdir().expect("temporary host database directory");
        let database = directory.path().join("tasks.sqlite");
        let mut bus = CommandBus::open(&database).expect("host command bus");
        let client_id = ClientId::new();
        let task_id = TaskId::new();
        let project_id = ProjectId::new();
        let envelope = CommandEnvelope {
            command_id: CommandId::new(),
            client_id,
            task_id: None,
            issued_at_ms: 1_725_000_000_100,
            expected_task_revision: None,
            command: Command::CreateTask(CreateTaskIntent {
                id: task_id,
                environment_id: EnvironmentId::new(),
                title: "Untrusted workspace".into(),
                description: None,
                project_id,
                workspace: WorkspaceRef::external(r"C:\forged").expect("workspace ref"),
                assignment: TaskAssignment::LocalOwner,
                created_at_ms: 1_725_000_000_100,
                connectivity: TaskConnectivity::Connected,
                attention: TaskAttention::None,
                activity: TaskActivity::Idle,
                review_readiness: ReviewReadiness::NotReady,
            }),
        };

        let result = dispatch_authenticated_request_with_workspace_projects(
            client_id,
            CapabilitySet::empty(),
            &mut bus,
            &WorkspaceProjectRoots::empty(),
            ClientRequest::Command(envelope),
        );

        assert!(
            matches!(result, Err(IpcError::Security(_))),
            "legacy raw CreateTask must fail closed at the authenticated host boundary: {result:?}"
        );
        assert!(
            bus.task_snapshot(task_id)
                .expect("task lookup after rejected request")
                .is_none(),
            "rejected raw task creation must not persist a task"
        );
    }

    #[test]
    fn create_with_primary_provider_binds_generation_one_before_launch() {
        let repository = tempfile::tempdir().expect("repository");
        let output = ProcessCommand::new("git")
            .args(["init", "--quiet"])
            .current_dir(repository.path())
            .output()
            .expect("git init");
        assert!(output.status.success());
        let database = repository.path().join("create-primary.sqlite");
        let mut bus = CommandBus::open(&database).expect("host command bus");
        let client_id = ClientId::new();
        let task_id = TaskId::new();
        let project_id = ProjectId::new();
        let roots =
            WorkspaceProjectRoots::try_from_pairs([(project_id, repository.path().to_path_buf())])
                .expect("roots");
        let create = CommandEnvelope {
            command_id: CommandId::new(),
            client_id,
            task_id: None,
            issued_at_ms: 1_725_000_000_300,
            expected_task_revision: None,
            command: Command::CreateTaskV2(CreateTaskRequestIntent {
                id: task_id,
                environment_id: EnvironmentId::new(),
                title: "Claude primary".into(),
                description: None,
                project_id,
                workspace: WorkspaceRequest::main(),
                primary_provider: Some(ProviderKind::ClaudeCode),
                defer_primary_provider_start: false,
                assignment: TaskAssignment::LocalOwner,
                created_at_ms: 1_725_000_000_300,
                connectivity: TaskConnectivity::Connected,
                attention: TaskAttention::None,
                activity: TaskActivity::Idle,
                review_readiness: ReviewReadiness::NotReady,
            }),
        };
        let (normalized, authorization, request_id) =
            normalize_task_create_at_host(create, Some(&roots), None, Uuid::nil(), None)
                .expect("normalize create");
        let receipt = bus
            .execute_host_authorized(
                normalized,
                authorization,
                request_id.expect("request id"),
                Uuid::nil(),
            )
            .expect("create");
        assert!(matches!(
            receipt,
            crate::domain::command::CommandReceipt::Accepted { .. }
        ));
        let mut agent =
            AgentSessionFacts::new(task_id, AgentRole::Primary, ProviderKind::ClaudeCode, None)
                .expect("agent");
        agent.runtime_generation = 1;
        let mut resource = ResourceFacts::new(
            Some(task_id),
            OwnerKind::Task,
            ResourceKind::Terminal,
            ResourceRecipe::terminal(120, 40),
            1_725_000_000_300,
        )
        .expect("terminal");
        resource.runtime_generation = 1;
        for (expected_revision, command) in [
            (
                1,
                Command::RegisterAgentSession {
                    agent: agent.clone(),
                },
            ),
            (
                2,
                Command::RegisterResource {
                    resource: resource.clone(),
                },
            ),
            (
                3,
                Command::SetPrimaryAgent {
                    agent_session_id: agent.id,
                },
            ),
        ] {
            bus.execute_host_authorized(
                CommandEnvelope {
                    command_id: CommandId::new(),
                    client_id,
                    task_id: Some(task_id),
                    issued_at_ms: 1_725_000_000_300,
                    expected_task_revision: Some(expected_revision),
                    command,
                },
                None,
                RequestId::new(),
                Uuid::nil(),
            )
            .expect("follow-through command");
        }
        let snapshot = bus.task_snapshot(task_id).expect("snapshot").expect("task");
        assert_eq!(snapshot.primary_agent_id, Some(agent.id));
        assert_eq!(snapshot.agents[&agent.id].runtime_generation, 1);
        assert_eq!(
            AgentResourceBinding::from_facts(
                &snapshot.agents[&agent.id],
                &snapshot.resources[&resource.id]
            )
            .expect("binding")
            .runtime_generation,
            1
        );
        let start = StartProviderSessionIntent {
            task_id,
            agent_session_id: agent.id,
            resource_id: resource.id,
            provider_kind: ProviderKind::ClaudeCode,
            mode: crate::domain::command::ProviderStartMode::NewConversation,
            launch_options: crate::providers::adapter::ProviderLaunchOptions::default(),
            expected_task_revision: snapshot.task.revision,
            expected_action_epoch: snapshot.task.action_epoch,
        };
        assert_eq!(start.expected_action_epoch, 0);
        assert_eq!(start.expected_task_revision, snapshot.task.revision);
    }

    #[test]
    fn authenticated_v2_create_resolves_workspace_before_persistence() {
        let repository = tempfile::tempdir().expect("temporary repository");
        let output = ProcessCommand::new("git")
            .args(["init", "--quiet"])
            .current_dir(repository.path())
            .output()
            .expect("git init");
        assert!(output.status.success());

        let database = repository.path().join("tasks.sqlite");
        let mut bus = CommandBus::open(&database).expect("host command bus");
        let client_id = ClientId::new();
        let task_id = TaskId::new();
        let project_id = ProjectId::new();
        let project_roots =
            WorkspaceProjectRoots::try_from_pairs([(project_id, repository.path().to_path_buf())])
                .expect("host project roots");
        let envelope = CommandEnvelope {
            command_id: CommandId::new(),
            client_id,
            task_id: None,
            issued_at_ms: 1_725_000_000_101,
            expected_task_revision: None,
            command: Command::CreateTaskV2(CreateTaskRequestIntent {
                id: task_id,
                environment_id: EnvironmentId::new(),
                title: "Host-resolved workspace".into(),
                description: None,
                project_id,
                workspace: WorkspaceRequest::main(),
                primary_provider: None,
                defer_primary_provider_start: false,
                assignment: TaskAssignment::LocalOwner,
                created_at_ms: 1_725_000_000_101,
                connectivity: TaskConnectivity::Connected,
                attention: TaskAttention::None,
                activity: TaskActivity::Idle,
                review_readiness: ReviewReadiness::NotReady,
            }),
        };

        let compatibility_result = dispatch_authenticated_request(
            client_id,
            CapabilitySet::empty(),
            &mut bus,
            ClientRequest::Command(envelope.clone()),
        );
        assert!(matches!(compatibility_result, Err(IpcError::Security(_))));

        let result = dispatch_authenticated_request_with_workspace_projects(
            client_id,
            CapabilitySet::empty(),
            &mut bus,
            &project_roots,
            ClientRequest::Command(envelope),
        );
        assert!(matches!(
            result,
            Ok(crate::protocol::ServerMessage::CommandReceipt(
                crate::domain::command::CommandReceipt::Accepted { .. }
            ))
        ));
        let snapshot = bus
            .task_snapshot(task_id)
            .expect("task lookup")
            .expect("created task");
        assert!(matches!(
            snapshot.task.workspace,
            crate::domain::task::WorkspaceRef::HostBound { .. }
        ));
    }

    #[test]
    fn authenticated_v2_create_rejects_an_unknown_host_project_id() {
        let repository = tempfile::tempdir().expect("temporary repository");
        let output = ProcessCommand::new("git")
            .args(["init", "--quiet"])
            .current_dir(repository.path())
            .output()
            .expect("git init");
        assert!(output.status.success());

        let database = repository.path().join("tasks.sqlite");
        let mut bus = CommandBus::open(&database).expect("host command bus");
        let client_id = ClientId::new();
        let task_id = TaskId::new();
        let envelope = CommandEnvelope {
            command_id: CommandId::new(),
            client_id,
            task_id: None,
            issued_at_ms: 1_725_000_000_102,
            expected_task_revision: None,
            command: Command::CreateTaskV2(CreateTaskRequestIntent {
                id: task_id,
                environment_id: EnvironmentId::new(),
                title: "Unknown project".into(),
                description: None,
                project_id: ProjectId::new(),
                workspace: WorkspaceRequest::main(),
                primary_provider: None,
                defer_primary_provider_start: false,
                assignment: TaskAssignment::LocalOwner,
                created_at_ms: 1_725_000_000_102,
                connectivity: TaskConnectivity::Connected,
                attention: TaskAttention::None,
                activity: TaskActivity::Idle,
                review_readiness: ReviewReadiness::NotReady,
            }),
        };

        let result = dispatch_authenticated_request_with_workspace_projects(
            client_id,
            CapabilitySet::empty(),
            &mut bus,
            &WorkspaceProjectRoots::empty(),
            ClientRequest::Command(envelope),
        );

        assert!(matches!(result, Err(IpcError::Security(_))));
        assert!(
            bus.task_snapshot(task_id)
                .expect("task lookup after rejected request")
                .is_none(),
            "unknown host project ids must not persist a task"
        );
    }

    #[test]
    fn workspace_security_errors_are_bounded_and_do_not_echo_paths() {
        let repository = tempfile::tempdir().expect("temporary repository");
        let database = repository.path().join("tasks.sqlite");
        let mut bus = CommandBus::open(&database).expect("host command bus");
        let client_id = ClientId::new();
        let task_id = TaskId::new();
        let project_id = ProjectId::new();
        let project_roots =
            WorkspaceProjectRoots::try_from_pairs([(project_id, repository.path().to_path_buf())])
                .expect("host project roots");
        let envelope = CommandEnvelope {
            command_id: CommandId::new(),
            client_id,
            task_id: None,
            issued_at_ms: 1_725_000_000_103,
            expected_task_revision: None,
            command: Command::CreateTaskV2(CreateTaskRequestIntent {
                id: task_id,
                environment_id: EnvironmentId::new(),
                title: "Rejected workspace".into(),
                description: None,
                project_id,
                workspace: WorkspaceRequest::new_worktree(
                    repository.path().join("missing-worktree"),
                    "codex/missing",
                ),
                primary_provider: None,
                defer_primary_provider_start: false,
                assignment: TaskAssignment::LocalOwner,
                created_at_ms: 1_725_000_000_103,
                connectivity: TaskConnectivity::Connected,
                attention: TaskAttention::None,
                activity: TaskActivity::Idle,
                review_readiness: ReviewReadiness::NotReady,
            }),
        };

        let result = dispatch_authenticated_request_with_workspace_projects(
            client_id,
            CapabilitySet::empty(),
            &mut bus,
            &project_roots,
            ClientRequest::Command(envelope),
        );
        let IpcError::Security(message) = result.expect_err("invalid workspace must reject") else {
            panic!("invalid workspace must return a security error");
        };
        assert!(message.len() <= 128, "security error must remain bounded");
        assert!(
            !message.contains(&repository.path().to_string_lossy().to_string()),
            "security error must not echo a filesystem path"
        );
        assert!(bus
            .task_snapshot(task_id)
            .expect("task lookup after rejected request")
            .is_none());
    }

    #[test]
    fn authorization_rejects_command_substitution_without_persisting_an_effect() {
        let repository = tempfile::tempdir().expect("temporary repository");
        let output = ProcessCommand::new("git")
            .args(["init", "--quiet"])
            .current_dir(repository.path())
            .output()
            .expect("git init");
        assert!(output.status.success());

        let database = repository.path().join("substitution.sqlite");
        let mut bus = CommandBus::open(&database).expect("host command bus");
        let client_id = ClientId::new();
        let task_id = TaskId::new();
        let project_id = ProjectId::new();
        let project_roots =
            WorkspaceProjectRoots::try_from_pairs([(project_id, repository.path().to_path_buf())])
                .expect("host project roots");
        let connection_id = Uuid::now_v7();
        let envelope = CommandEnvelope {
            command_id: CommandId::new(),
            client_id,
            task_id: None,
            issued_at_ms: 1_725_000_000_104,
            expected_task_revision: None,
            command: Command::CreateTaskV2(CreateTaskRequestIntent {
                id: task_id,
                environment_id: EnvironmentId::new(),
                title: "substitution guard".into(),
                description: None,
                project_id,
                workspace: WorkspaceRequest::main(),
                primary_provider: None,
                defer_primary_provider_start: false,
                assignment: TaskAssignment::LocalOwner,
                created_at_ms: 1_725_000_000_104,
                connectivity: TaskConnectivity::Connected,
                attention: TaskAttention::None,
                activity: TaskActivity::Idle,
                review_readiness: ReviewReadiness::NotReady,
            }),
        };
        let (normalized, authorization, request_id) = normalize_task_create_at_host(
            envelope,
            Some(&project_roots),
            None,
            connection_id,
            None,
        )
        .expect("host normalization");
        let Some(authorization) = authorization else {
            panic!("CreateTaskV2 must receive host authorization");
        };
        let request_id = request_id.expect("host request nonce");
        let mut substituted = normalized;
        substituted.command_id = CommandId::new();

        assert_eq!(
            bus.execute_host_authorized(
                substituted,
                Some(authorization),
                request_id,
                connection_id,
            ),
            Err(crate::kernel::StoreError::HostAuthorityRequired)
        );
        assert!(bus
            .task_snapshot(task_id)
            .expect("task lookup after substitution")
            .is_none());
    }

    #[test]
    fn task_cockpit_requires_capability_exact_task_and_rejects_path_traversal() {
        let repository = tempfile::tempdir().expect("temporary repository");
        let output = ProcessCommand::new("git")
            .args(["init", "--quiet"])
            .current_dir(repository.path())
            .output()
            .expect("git init");
        assert!(output.status.success());

        let database = repository.path().join("cockpit.sqlite");
        let mut bus = CommandBus::open(&database).expect("host command bus");
        let client_id = ClientId::new();
        let task_id = TaskId::new();
        let project_id = ProjectId::new();
        let project_roots =
            WorkspaceProjectRoots::try_from_pairs([(project_id, repository.path().to_path_buf())])
                .expect("host project roots");
        let create = CommandEnvelope {
            command_id: CommandId::new(),
            client_id,
            task_id: None,
            issued_at_ms: 1_725_000_000_200,
            expected_task_revision: None,
            command: Command::CreateTaskV2(CreateTaskRequestIntent {
                id: task_id,
                environment_id: EnvironmentId::new(),
                title: "Cockpit task".into(),
                description: None,
                project_id,
                workspace: WorkspaceRequest::main(),
                primary_provider: None,
                defer_primary_provider_start: false,
                assignment: TaskAssignment::LocalOwner,
                created_at_ms: 1_725_000_000_200,
                connectivity: TaskConnectivity::Connected,
                attention: TaskAttention::None,
                activity: TaskActivity::Idle,
                review_readiness: ReviewReadiness::NotReady,
            }),
        };
        dispatch_authenticated_request_with_workspace_projects(
            client_id,
            CapabilitySet::empty(),
            &mut bus,
            &project_roots,
            ClientRequest::Command(create),
        )
        .expect("create task");

        let denied = dispatch_authenticated_request_with_workspace_projects(
            client_id,
            CapabilitySet::empty(),
            &mut bus,
            &project_roots,
            ClientRequest::Query(crate::domain::query::QueryEnvelope {
                request_id: crate::domain::RequestId::new(),
                client_id,
                task_id: Some(task_id),
                query: crate::domain::query::Query::TaskCockpit(
                    crate::domain::TaskCockpitQuery::WorkspaceStatus,
                ),
            }),
        )
        .expect("capability denial is a typed query reply");
        let crate::protocol::ServerMessage::QueryReply(reply) = denied else {
            panic!("expected query reply");
        };
        assert!(matches!(
            reply.outcome,
            crate::domain::query::QueryOutcome::Err(
                crate::domain::query::QueryError::UnsupportedCapability
            )
        ));

        let granted = crate::protocol::CapabilitySet::from_capabilities([
            crate::protocol::Capability::TaskCockpit,
        ]);
        let missing = dispatch_authenticated_request_with_workspace_projects(
            client_id,
            granted,
            &mut bus,
            &project_roots,
            ClientRequest::Query(crate::domain::query::QueryEnvelope {
                request_id: crate::domain::RequestId::new(),
                client_id,
                task_id: Some(TaskId::new()),
                query: crate::domain::query::Query::TaskCockpit(
                    crate::domain::TaskCockpitQuery::WorkspaceStatus,
                ),
            }),
        )
        .expect("missing task");
        let crate::protocol::ServerMessage::QueryReply(reply) = missing else {
            panic!("expected query reply");
        };
        assert!(matches!(
            reply.outcome,
            crate::domain::query::QueryOutcome::Ok(crate::domain::query::QueryResult::TaskCockpit(
                crate::domain::TaskCockpitResult::Denied {
                    reason: crate::domain::TaskCockpitDeniedReason::MissingTask,
                    ..
                }
            ))
        ));

        let workspace = dispatch_authenticated_request_with_workspace_projects(
            client_id,
            granted,
            &mut bus,
            &project_roots,
            ClientRequest::Query(crate::domain::query::QueryEnvelope {
                request_id: crate::domain::RequestId::new(),
                client_id,
                task_id: Some(task_id),
                query: crate::domain::query::Query::TaskCockpit(
                    crate::domain::TaskCockpitQuery::WorkspaceStatus,
                ),
            }),
        )
        .expect("workspace");
        let crate::protocol::ServerMessage::QueryReply(reply) = workspace else {
            panic!("expected query reply");
        };
        let crate::domain::query::QueryOutcome::Ok(crate::domain::query::QueryResult::TaskCockpit(
            crate::domain::TaskCockpitResult::Workspace(projection),
        )) = reply.outcome
        else {
            panic!("expected workspace projection, got {:?}", reply.outcome);
        };
        assert_eq!(projection.task_id, task_id);
        assert!(projection.bound);
        let encoded = serde_json::to_string(&projection).expect("encode");
        assert!(
            !encoded.contains(&repository.path().to_string_lossy().to_string()),
            "workspace projection must not leak the filesystem path"
        );

        let traversal = dispatch_authenticated_request_with_workspace_projects(
            client_id,
            granted,
            &mut bus,
            &project_roots,
            ClientRequest::Query(crate::domain::query::QueryEnvelope {
                request_id: crate::domain::RequestId::new(),
                client_id,
                task_id: Some(task_id),
                query: crate::domain::query::Query::TaskCockpit(
                    crate::domain::TaskCockpitQuery::FilesRead {
                        relative_path: "../secret.env".into(),
                        max_bytes: 32,
                    },
                ),
            }),
        )
        .expect("traversal");
        let crate::protocol::ServerMessage::QueryReply(reply) = traversal else {
            panic!("expected query reply");
        };
        assert!(matches!(
            reply.outcome,
            crate::domain::query::QueryOutcome::Ok(crate::domain::query::QueryResult::TaskCockpit(
                crate::domain::TaskCockpitResult::Denied {
                    surface: crate::domain::TaskCockpitSurface::Files,
                    reason: crate::domain::TaskCockpitDeniedReason::PathTraversal,
                    ..
                }
            ))
        ));

        let git = dispatch_authenticated_request_with_workspace_projects(
            client_id,
            granted,
            &mut bus,
            &project_roots,
            ClientRequest::Query(crate::domain::query::QueryEnvelope {
                request_id: crate::domain::RequestId::new(),
                client_id,
                task_id: Some(task_id),
                query: crate::domain::query::Query::TaskCockpit(
                    crate::domain::TaskCockpitQuery::GitStatus,
                ),
            }),
        )
        .expect("git");
        let crate::protocol::ServerMessage::QueryReply(reply) = git else {
            panic!("expected query reply");
        };
        assert!(matches!(
            reply.outcome,
            crate::domain::query::QueryOutcome::Ok(crate::domain::query::QueryResult::TaskCockpit(
                crate::domain::TaskCockpitResult::Unavailable {
                    surface: crate::domain::TaskCockpitSurface::Git,
                    reason:
                        crate::domain::TaskCockpitUnavailableReason::WorkspaceAuthorityUnavailable,
                    ..
                }
            ))
        ));

        let health = dispatch_authenticated_request_with_workspace_projects(
            client_id,
            granted,
            &mut bus,
            &project_roots,
            ClientRequest::Query(crate::domain::query::QueryEnvelope {
                request_id: crate::domain::RequestId::new(),
                client_id,
                task_id: Some(task_id),
                query: crate::domain::query::Query::TaskCockpit(
                    crate::domain::TaskCockpitQuery::ServiceHealth {
                        service_id: crate::domain::id::ConfiguredServiceId::new("api")
                            .expect("catalog"),
                        resource_generation: 1,
                        connection_epoch: 1,
                        action_epoch: 1,
                    },
                ),
            }),
        )
        .expect("health");
        let crate::protocol::ServerMessage::QueryReply(reply) = health else {
            panic!("expected query reply");
        };
        assert!(matches!(
            reply.outcome,
            crate::domain::query::QueryOutcome::Ok(crate::domain::query::QueryResult::TaskCockpit(
                crate::domain::TaskCockpitResult::Unavailable {
                    surface: crate::domain::TaskCockpitSurface::Services,
                    reason: crate::domain::TaskCockpitUnavailableReason::HealthUnsupported,
                    ..
                }
            ))
        ));

        let logs = dispatch_authenticated_request_with_workspace_projects(
            client_id,
            granted,
            &mut bus,
            &project_roots,
            ClientRequest::Query(crate::domain::query::QueryEnvelope {
                request_id: crate::domain::RequestId::new(),
                client_id,
                task_id: Some(task_id),
                query: crate::domain::query::Query::TaskCockpit(
                    crate::domain::TaskCockpitQuery::ServiceLogs {
                        service_id: crate::domain::id::ConfiguredServiceId::new("api")
                            .expect("catalog"),
                        resource_generation: 1,
                        connection_epoch: 1,
                        action_epoch: 1,
                    },
                ),
            }),
        )
        .expect("logs");
        let crate::protocol::ServerMessage::QueryReply(reply) = logs else {
            panic!("expected query reply");
        };
        assert!(matches!(
            reply.outcome,
            crate::domain::query::QueryOutcome::Ok(crate::domain::query::QueryResult::TaskCockpit(
                crate::domain::TaskCockpitResult::Unavailable {
                    surface: crate::domain::TaskCockpitSurface::Services,
                    reason: crate::domain::TaskCockpitUnavailableReason::LogsUnsupported,
                    ..
                }
            ))
        ));

        let ssh = dispatch_authenticated_request_with_workspace_projects(
            client_id,
            granted,
            &mut bus,
            &project_roots,
            ClientRequest::Query(crate::domain::query::QueryEnvelope {
                request_id: crate::domain::RequestId::new(),
                client_id,
                task_id: Some(task_id),
                query: crate::domain::query::Query::TaskCockpit(
                    crate::domain::TaskCockpitQuery::SshAction {
                        endpoint_id: "deploy@secret.example".into(),
                    },
                ),
            }),
        )
        .expect("ssh");
        let crate::protocol::ServerMessage::QueryReply(reply) = ssh else {
            panic!("expected query reply");
        };
        assert!(matches!(
            reply.outcome,
            crate::domain::query::QueryOutcome::Ok(crate::domain::query::QueryResult::TaskCockpit(
                crate::domain::TaskCockpitResult::Unavailable {
                    surface: crate::domain::TaskCockpitSurface::Ssh,
                    reason: crate::domain::TaskCockpitUnavailableReason::SshOperationUnsupported,
                    ..
                }
            ))
        ));
    }
}

/// Typed intentional exit from a supervised [`HostRequestExecutor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostExecutorOutcome {
    Intentional {
        operation_id: OperationId,
        action_epoch: u64,
    },
}

/// Supervised foreground executor: arm channel + join handle with typed outcome.
pub struct SupervisedHostExecutor {
    pub arm_rx: mpsc::Receiver<PhysicalExitArmRequest>,
    pub join: JoinHandle<Result<HostExecutorOutcome, StoreError>>,
}

/// Internal completion routing for one host request job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostRequestCompletionRouting {
    /// Caller (reader / one-shot serve) owns writing the response frame.
    CallerOwned,
    /// Executor may directly admit an accepted ConfirmHostQuit receipt.
    ExecutorOwnsAcceptedHostQuitReceipt,
}

/// Crate-private duplex execute completion: either the reader must write, or the
/// executor already admitted the quit receipt onto the critical lane.
#[derive(Debug)]
pub(crate) enum DuplexExecuteCompletion {
    CallerMustWrite(ServerMessage),
    ExecutorAdmittedQuitReceipt { operation_id: OperationId },
}

struct HostRequestJob {
    negotiated: NegotiatedParameters,
    request: ClientRequest,
    output_id: Option<ConnectionOutputId>,
    routing: HostRequestCompletionRouting,
    reply: oneshot::Sender<Result<DuplexExecuteCompletion, IpcError>>,
}

struct PendingQuitReceiptAck {
    operation_id: OperationId,
    ack: PhysicalWriteAck,
}

#[derive(Clone)]
struct PreparedUpdateReply {
    intent: PrepareUpdateIntent,
    reply: UpdateHandoffReply,
}

enum ExecutorControl {
    RegisterOutput {
        id: ConnectionOutputId,
        output: ConnectionOutputHandle,
        client_id: Option<ClientId>,
        reconnect_from: Option<ConnectionOutputId>,
        ack: oneshot::Sender<()>,
    },
    UnregisterOutput {
        id: ConnectionOutputId,
    },
    AttachTerminal {
        owner: TaskId,
        spec: TerminalSpec,
        runtime: Arc<dyn AttachedTerminalRuntime>,
        ack: oneshot::Sender<Result<TerminalId, String>>,
    },
    BindTerminalIdentity {
        terminal_id: TerminalId,
        agent_session_id: AgentSessionId,
        runtime_generation: u64,
        action_epoch: u64,
        ack: oneshot::Sender<Result<(), String>>,
    },
    InspectHostQuitForUpdate {
        ack: oneshot::Sender<Result<crate::domain::host::HostQuitInspection, String>>,
    },
    PrepareUpdate {
        target_version: String,
        client_build: String,
        host_build: String,
        allow_explicit_confirm_with_active: bool,
        ack: oneshot::Sender<Result<crate::updater::UpdateHandoffToken, String>>,
    },
    ConfirmUpdateDrain {
        token_id: Uuid,
        ack: oneshot::Sender<Result<(), String>>,
    },
    AbortUpdateHandoff {
        ack: oneshot::Sender<Result<(), String>>,
    },
    ArmUpdateInstall {
        token_id: Uuid,
        ack: oneshot::Sender<Result<(), String>>,
    },
    SealUpdateAfterDurableStage {
        ack: oneshot::Sender<Result<(), String>>,
    },
    /// Typed in-process handoff of the UI-owned BrowserGatewayRegistrar into
    /// the host ProcessManager. Sibling-process hosts cannot share this Arc;
    /// they leave the registrar unset and browser tools fail visibly.
    SetBrowserGatewayRegistrar {
        registrar: Option<crate::browser::BrowserGatewayRegistrar>,
        ack: oneshot::Sender<()>,
    },
    #[cfg(test)]
    InspectOutput {
        id: ConnectionOutputId,
        ack: oneshot::Sender<OutputInspection>,
    },
    #[cfg(test)]
    RunMaintenanceOnce {
        ack: oneshot::Sender<Result<(), StoreError>>,
    },
    #[cfg(test)]
    TakePendingQuitReceiptAck {
        id: ConnectionOutputId,
        ack: oneshot::Sender<Option<(OperationId, PhysicalWriteAck)>>,
    },
    #[cfg(test)]
    InstallTestSemanticJournal {
        journal: Arc<Mutex<crate::remote::presentation::SemanticJournalStore>>,
        ack: oneshot::Sender<()>,
    },
    #[cfg(test)]
    RecordTestSemantic {
        draft: crate::remote::presentation::SemanticEventDraft,
        ack: oneshot::Sender<u64>,
    },
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OutputInspection {
    pub(crate) registered: bool,
    pub(crate) live_bound: bool,
}

struct SnapshotRegistryEntry {
    owner: ClientId,
    scope: SessionScope,
    session: SnapshotSession,
    limits: PageLimits,
    last_touch: Instant,
}

struct SnapshotRegistry {
    entries: HashMap<SnapshotId, SnapshotRegistryEntry>,
}

impl SnapshotRegistry {
    fn new() -> Self {
        Self {
            entries: HashMap::with_capacity(MAX_SNAPSHOT_SESSIONS),
        }
    }

    fn reap_idle(&mut self, now: Instant) {
        self.entries
            .retain(|_, entry| now.duration_since(entry.last_touch) < SNAPSHOT_IDLE_TTL);
    }

    fn evict_lru_if_at_capacity(&mut self) {
        while self.entries.len() >= MAX_SNAPSHOT_SESSIONS {
            let Some((&victim_id, _)) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_touch)
            else {
                break;
            };
            self.entries.remove(&victim_id);
        }
    }

    /// Make the bounded map admission decision before opening a SQLite view.
    /// The page/session allocation is therefore never the first operation to
    /// grow this registry.
    fn prepare_insert(&mut self) {
        self.evict_lru_if_at_capacity();
    }

    fn insert(
        &mut self,
        owner: ClientId,
        session: SnapshotSession,
        limits: PageLimits,
        now: Instant,
    ) {
        self.evict_lru_if_at_capacity();
        let snapshot_id = session.snapshot_id();
        self.entries.insert(
            snapshot_id,
            SnapshotRegistryEntry {
                owner,
                scope: session.scope(),
                session,
                limits,
                last_touch: now,
            },
        );
    }

    fn touch(&mut self, snapshot_id: SnapshotId, now: Instant) {
        if let Some(entry) = self.entries.get_mut(&snapshot_id) {
            entry.last_touch = now;
        }
    }

    fn remove(&mut self, snapshot_id: SnapshotId) -> Option<SnapshotRegistryEntry> {
        self.entries.remove(&snapshot_id)
    }

    /// Move authorization for one client from a closed physical connection to
    /// the new connection admitted by a one-shot reconnect grant.  The pinned
    /// session keeps the issuance scope so its HMAC cursor remains resumable;
    /// this registry scope is the current connection gate.
    fn rebind_output(
        &mut self,
        client_id: ClientId,
        old_output: ConnectionOutputId,
        new_output: ConnectionOutputId,
    ) {
        for entry in self.entries.values_mut() {
            if entry.owner == client_id && entry.scope.connection_id == Some(old_output.as_uuid()) {
                entry.scope.connection_id = Some(new_output.as_uuid());
            }
        }
    }

    fn get(
        &self,
        snapshot_id: SnapshotId,
        requester: ClientId,
        scope: SessionScope,
        limits: PageLimits,
        now: Instant,
    ) -> Result<&SnapshotSession, QueryError> {
        let Some(entry) = self.entries.get(&snapshot_id) else {
            return Err(QueryError::NotFound);
        };
        if now.duration_since(entry.last_touch) >= SNAPSHOT_IDLE_TTL {
            return Err(QueryError::NotFound);
        }
        if entry.owner != requester || entry.scope != scope {
            return Err(QueryError::Unauthorized);
        }
        if entry.limits != limits {
            return Err(QueryError::InvalidRequest);
        }
        Ok(&entry.session)
    }
}

pub(crate) struct LiveStreamState {
    /// Bumped on cancel/resync so already-queued durables that have not started
    /// writing are skipped. In-flight frames that complete a physical write still
    /// record their sequence.
    generation: AtomicU64,
    /// Conservative last sequence successfully written on the durable pipe.
    last_physically_written: AtomicU64,
    /// Persistent progress wakeups for quit durable high-water waits.
    progress: Notify,
}

impl LiveStreamState {
    pub(crate) fn new(baseline: u64) -> Arc<Self> {
        Arc::new(Self {
            generation: AtomicU64::new(1),
            last_physically_written: AtomicU64::new(baseline),
            progress: Notify::new(),
        })
    }

    pub(crate) fn current_generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    pub(crate) fn cancel(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    pub(crate) fn last_physically_written(&self) -> u64 {
        self.last_physically_written.load(Ordering::SeqCst)
    }

    pub(crate) fn record_physical_write(&self, sequence: u64) {
        let mut current = self.last_physically_written.load(Ordering::SeqCst);
        while sequence > current {
            match self.last_physically_written.compare_exchange_weak(
                current,
                sequence,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => {
                    // Notify only when the atomic high-water actually advances.
                    self.progress.notify_waiters();
                    return;
                }
                Err(observed) => current = observed,
            }
        }
    }

    /// Wait until [`Self::last_physically_written`] is at least `target`.
    ///
    /// Uses `Notified::enable` plus a recheck so a notify between the atomic
    /// load and the await cannot be lost.
    pub(crate) async fn wait_until_physically_written(&self, target: u64) {
        loop {
            if self.last_physically_written() >= target {
                return;
            }
            let mut notified = pin!(self.progress.notified());
            notified.as_mut().enable();
            if self.last_physically_written() >= target {
                return;
            }
            notified.await;
        }
    }
}

struct LiveTail {
    output_id: ConnectionOutputId,
    last_admitted_sequence: u64,
    stream: Arc<LiveStreamState>,
}

impl LiveTail {
    fn new(output_id: ConnectionOutputId, baseline: u64) -> Self {
        Self {
            output_id,
            last_admitted_sequence: baseline,
            stream: LiveStreamState::new(baseline),
        }
    }
}

struct EventReplayRegistryEntry {
    owner: ClientId,
    scope: SessionScope,
    /// Present only while frozen pages remain. Dropped when frozen replay completes.
    frozen: Option<EventReplaySession>,
    limits: PageLimits,
    last_touch: Instant,
    /// Lightweight live delivery metadata; retained after frozen completion.
    live: Option<LiveTail>,
}

struct EventReplayRegistry {
    entries: HashMap<SubscriptionId, EventReplayRegistryEntry>,
}

impl EventReplayRegistry {
    fn new() -> Self {
        Self {
            entries: HashMap::with_capacity(MAX_EVENT_REPLAY_SESSIONS),
        }
    }

    fn reap_idle(&mut self, now: Instant) {
        // Incomplete frozen replay keeps the bounded TTL. Completed live
        // subscriptions do not expire merely because no events arrive.
        self.entries.retain(|_, entry| match &entry.frozen {
            Some(_) if now.duration_since(entry.last_touch) >= EVENT_REPLAY_IDLE_TTL => {
                if let Some(live) = &entry.live {
                    live.stream.cancel();
                }
                false
            }
            _ => true,
        });
    }

    /// Evict only incomplete frozen entries that have no live binding.
    /// Never silently evict an active live tail; caller must fail closed.
    fn try_evict_inactive_frozen_for_capacity(&mut self) -> bool {
        if self.entries.len() < MAX_EVENT_REPLAY_SESSIONS {
            return true;
        }
        let victim = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.frozen.is_some() && entry.live.is_none())
            .min_by_key(|(_, entry)| entry.last_touch)
            .map(|(id, _)| *id);
        if let Some(victim_id) = victim {
            let _ = self.remove(victim_id);
            true
        } else {
            false
        }
    }

    /// Preflight the bounded registry before opening a read transaction.  A
    /// full live-only registry fails closed; only inactive frozen sessions may
    /// be evicted.
    fn prepare_insert(&mut self) -> bool {
        self.try_evict_inactive_frozen_for_capacity()
    }

    fn insert_open(
        &mut self,
        owner: ClientId,
        session: EventReplaySession,
        limits: PageLimits,
        live: Option<LiveTail>,
        retain_frozen: bool,
        now: Instant,
    ) -> Result<SubscriptionId, IpcError> {
        while self.entries.len() >= MAX_EVENT_REPLAY_SESSIONS {
            if !self.try_evict_inactive_frozen_for_capacity() {
                if let Some(live) = &live {
                    live.stream.cancel();
                }
                return Err(IpcError::Busy);
            }
        }
        let subscription_id = session.subscription_id();
        self.entries.insert(
            subscription_id,
            EventReplayRegistryEntry {
                owner,
                scope: session.scope(),
                frozen: retain_frozen.then_some(session),
                limits,
                last_touch: now,
                live,
            },
        );
        Ok(subscription_id)
    }

    fn touch(&mut self, subscription_id: SubscriptionId, now: Instant) {
        if let Some(entry) = self.entries.get_mut(&subscription_id) {
            entry.last_touch = now;
        }
    }

    fn remove(&mut self, subscription_id: SubscriptionId) -> Option<EventReplayRegistryEntry> {
        let entry = self.entries.remove(&subscription_id)?;
        if let Some(live) = &entry.live {
            live.stream.cancel();
        }
        Some(entry)
    }

    /// Move frozen/live authorization to a freshly admitted physical output.
    /// The immutable replay cursor retains its issuance scope, while live
    /// delivery starts from the last sequence physically admitted before the
    /// old output was lost.
    fn rebind_output(
        &mut self,
        client_id: ClientId,
        old_output: ConnectionOutputId,
        new_output: ConnectionOutputId,
    ) {
        for entry in self.entries.values_mut() {
            if entry.owner != client_id || entry.scope.connection_id != Some(old_output.as_uuid()) {
                continue;
            }
            entry.scope.connection_id = Some(new_output.as_uuid());
            if let Some(live) = entry.live.as_mut() {
                let baseline = live.last_admitted_sequence;
                live.stream.cancel();
                live.output_id = new_output;
                live.stream = LiveStreamState::new(baseline);
            }
        }
    }

    fn remove_for_output(&mut self, output_id: ConnectionOutputId) {
        let mut remove_ids = Vec::new();
        for (subscription_id, entry) in self.entries.iter_mut() {
            let Some(live) = entry.live.as_ref() else {
                continue;
            };
            if live.output_id != output_id {
                continue;
            }
            live.stream.cancel();
            if entry.frozen.is_some() {
                // Incomplete frozen replay keeps its TTL for reconnect; drop only
                // the live binding tied to the closed connection output.
                entry.live = None;
            } else {
                remove_ids.push(*subscription_id);
            }
        }
        for subscription_id in remove_ids {
            self.entries.remove(&subscription_id);
        }
    }

    fn get_frozen(
        &self,
        subscription_id: SubscriptionId,
        requester: ClientId,
        scope: SessionScope,
        limits: PageLimits,
        now: Instant,
    ) -> Result<&EventReplaySession, QueryError> {
        let Some(entry) = self.entries.get(&subscription_id) else {
            return Err(QueryError::NotFound);
        };
        if entry.frozen.is_some() && now.duration_since(entry.last_touch) >= EVENT_REPLAY_IDLE_TTL {
            return Err(QueryError::NotFound);
        }
        if entry.owner != requester || entry.scope != scope {
            return Err(QueryError::Unauthorized);
        }
        if entry.limits != limits {
            return Err(QueryError::InvalidRequest);
        }
        entry.frozen.as_ref().ok_or(QueryError::NotFound)
    }
}

/// Drops unregister the connection output so executor-held handles cannot keep
/// a pipe/writer/reader/task alive after connection shutdown.
pub(crate) struct ConnectionOutputRegistration {
    id: ConnectionOutputId,
    output: ConnectionOutputHandle,
    control_tx: mpsc::Sender<ExecutorControl>,
}

/// Host-owned Connect duplex session.
///
/// Opaque outside the crate: fields are crate-visible only. Drop keeps
/// [`ConnectionOutputRegistration`] ownership until the session ends so
/// shutdown/unregister still runs through the existing guard. This factory
/// does not spawn reader/writer tasks. `client_id` is host-owned registration
/// metadata used to bind the encrypted transport entry — never a network
/// connection id.
pub struct HostConnectDuplex {
    pub(crate) client_id: ClientId,
    pub(crate) requests: HostRequestHandle,
    pub(crate) output: ConnectionOutputHandle,
    pub(crate) ports: ConnectionOutputPorts,
    pub(crate) registration: ConnectionOutputRegistration,
}

impl ConnectionOutputRegistration {
    pub(crate) fn id(&self) -> ConnectionOutputId {
        self.id
    }
}

impl Drop for ConnectionOutputRegistration {
    fn drop(&mut self) {
        self.output.request_shutdown();
        let _ = self
            .control_tx
            .try_send(ExecutorControl::UnregisterOutput { id: self.id });
    }
}

/// Cloneable submit handle for the single host CommandBus executor.
#[derive(Clone, Debug)]
pub struct HostRequestHandle {
    tx: mpsc::Sender<HostRequestJob>,
    control_tx: mpsc::Sender<ExecutorControl>,
    output_id: Option<ConnectionOutputId>,
    update_gate: Arc<crate::host::update::HostUpdateRuntimeGate>,
    host_boot_id: Arc<OnceLock<Uuid>>,
    remote_setup: Arc<OnceLock<super::remote_setup::RemoteSetupHandle>>,
    configured_service_supervisor_ready: bool,
}

impl HostRequestHandle {
    /// Bind the independently owned local setup controller once per host.
    pub fn bind_remote_setup(
        &self,
        handle: super::remote_setup::RemoteSetupHandle,
    ) -> Result<(), super::remote_setup::RemoteSetupHandle> {
        self.remote_setup.set(handle)
    }

    /// Attach an already-owned task terminal to the host terminal service.
    /// This never launches a PTY; the runtime is supplied by the task owner.
    pub(crate) async fn attach_terminal(
        &self,
        owner: TaskId,
        spec: TerminalSpec,
        runtime: Arc<dyn AttachedTerminalRuntime>,
    ) -> Result<TerminalId, IpcError> {
        let (ack, result) = oneshot::channel();
        self.control_tx
            .send(ExecutorControl::AttachTerminal {
                owner,
                spec,
                runtime,
                ack,
            })
            .await
            .map_err(|_| IpcError::Unavailable)?;
        result
            .await
            .map_err(|_| IpcError::Unavailable)?
            .map_err(|_| IpcError::Unavailable)
    }

    /// Bind the durable task identity and generation fence before any input
    /// is admitted for the attached terminal.
    pub(crate) async fn bind_terminal_identity(
        &self,
        terminal_id: TerminalId,
        agent_session_id: AgentSessionId,
        runtime_generation: u64,
        action_epoch: u64,
    ) -> Result<(), IpcError> {
        let (ack, result) = oneshot::channel();
        self.control_tx
            .send(ExecutorControl::BindTerminalIdentity {
                terminal_id,
                agent_session_id,
                runtime_generation,
                action_epoch,
                ack,
            })
            .await
            .map_err(|_| IpcError::Unavailable)?;
        result
            .await
            .map_err(|_| IpcError::Unavailable)?
            .map_err(|_| IpcError::Unavailable)
    }

    /// Shared update admission gate (stop-new-launches while draining/installing).
    pub fn update_runtime_gate(&self) -> Arc<crate::host::update::HostUpdateRuntimeGate> {
        Arc::clone(&self.update_gate)
    }

    /// Whether the host executor successfully bound its one configured
    /// ProcessManager-owned service supervisor at startup.
    pub fn configured_service_supervisor_ready(&self) -> bool {
        self.configured_service_supervisor_ready
    }

    /// Owned Send+'static probe that runs InspectHostQuit on the executor task.
    pub fn owned_update_resource_probe(
        &self,
        host_boot_id: Uuid,
    ) -> crate::host::update::OwnedActiveResourceProbe {
        let handle = self.clone();
        crate::host::update::OwnedActiveResourceProbe::from_fn(move || {
            let deadline = Instant::now() + crate::updater::UPDATE_IPC_DEADLINE;
            let inspection = handle.inspect_host_quit_blocking(deadline)?;
            Ok(crate::host::update::update_inspection_from_host_quit(
                &inspection,
                host_boot_id,
            ))
        })
    }

    /// Bind updater service to this host executor's shared FSM + timed IPC port.
    pub fn bind_updater_runtime(
        &self,
        updater: &crate::updater::UpdaterService,
        host_boot_id: Uuid,
        server_build: &str,
        protocol_major: u16,
        protocol_minor: u16,
    ) {
        let _ = self.host_boot_id.set(host_boot_id);
        updater.bind_live_host_hello(server_build, protocol_major, protocol_minor);
        updater.bind_host_update_runtime(
            self.update_runtime_gate(),
            Box::new(self.clone()),
            Box::new(self.owned_update_resource_probe(host_boot_id)),
        );
    }

    fn control_recv_deadline<T: Send + 'static>(
        &self,
        enqueue: impl FnOnce(oneshot::Sender<T>) -> ExecutorControl,
        deadline: Instant,
    ) -> Result<T, String> {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| "update IPC absolute deadline already elapsed".to_string())?;
        let (ack_tx, ack_rx) = oneshot::channel();
        self.control_tx
            .try_send(enqueue(ack_tx))
            .map_err(|_| "host executor control channel is unavailable".to_string())?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = (|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| format!("failed to build update IPC runtime: {error}"))?;
                runtime.block_on(async {
                    tokio::time::timeout(remaining, ack_rx)
                        .await
                        .map_err(|_| "update IPC timed out before host ack".to_string())?
                        .map_err(|_| "host update IPC ack dropped".to_string())
                })
            })();
            let _ = tx.send(result);
        });
        rx.recv_timeout(remaining.saturating_add(Duration::from_millis(50)))
            .map_err(|_| "update IPC worker disconnected or timed out".to_string())?
    }

    fn inspect_host_quit_blocking(
        &self,
        deadline: Instant,
    ) -> Result<crate::domain::host::HostQuitInspection, String> {
        self.control_recv_deadline(
            |ack| ExecutorControl::InspectHostQuitForUpdate { ack },
            deadline,
        )?
    }

    /// Bind this handle clone to one duplex connection output.
    pub(crate) fn with_output(&self, output_id: ConnectionOutputId) -> Self {
        Self {
            tx: self.tx.clone(),
            control_tx: self.control_tx.clone(),
            output_id: Some(output_id),
            update_gate: Arc::clone(&self.update_gate),
            host_boot_id: Arc::clone(&self.host_boot_id),
            remote_setup: Arc::clone(&self.remote_setup),
            configured_service_supervisor_ready: self.configured_service_supervisor_ready,
        }
    }
}

impl crate::updater::HostUpdateControlPort for HostRequestHandle {
    fn prepare_update(
        &self,
        target_version: &str,
        client_build: &str,
        host_build: &str,
        allow_explicit_confirm_with_active: bool,
        deadline: Instant,
    ) -> Result<crate::updater::UpdateHandoffToken, String> {
        self.control_recv_deadline(
            |ack| ExecutorControl::PrepareUpdate {
                target_version: target_version.to_string(),
                client_build: client_build.to_string(),
                host_build: host_build.to_string(),
                allow_explicit_confirm_with_active,
                ack,
            },
            deadline,
        )?
    }

    fn confirm_drain(&self, token_id: Uuid, deadline: Instant) -> Result<(), String> {
        self.control_recv_deadline(
            |ack| ExecutorControl::ConfirmUpdateDrain { token_id, ack },
            deadline,
        )?
    }

    fn abort_pre_install(&self, deadline: Instant) -> Result<(), String> {
        self.control_recv_deadline(|ack| ExecutorControl::AbortUpdateHandoff { ack }, deadline)?
    }

    fn begin_atomic_install(&self, token_id: Uuid, deadline: Instant) -> Result<(), String> {
        self.control_recv_deadline(
            |ack| ExecutorControl::ArmUpdateInstall { token_id, ack },
            deadline,
        )?
    }

    fn seal_after_durable_stage(&self, deadline: Instant) -> Result<(), String> {
        self.control_recv_deadline(
            |ack| ExecutorControl::SealUpdateAfterDurableStage { ack },
            deadline,
        )?
    }
}

impl HostRequestHandle {
    /// In-process handoff of the UI BrowserGatewayRegistrar into the host
    /// ProcessManager. No-op when the executor has no configured service runtime.
    pub fn set_browser_gateway_registrar(
        &self,
        registrar: Option<crate::browser::BrowserGatewayRegistrar>,
    ) -> Result<(), String> {
        let (ack_tx, ack_rx) = oneshot::channel();
        self.control_tx
            .try_send(ExecutorControl::SetBrowserGatewayRegistrar {
                registrar,
                ack: ack_tx,
            })
            .map_err(|_| "browser gateway registrar handoff queue is unavailable".to_string())?;
        ack_rx
            .blocking_recv()
            .map_err(|_| "browser gateway registrar handoff was cancelled".to_string())
    }

    /// Register dual-lane output for live durable delivery on this connection.
    ///
    /// The returned registration guard is armed before the send/await window so
    /// task cancellation always requests shutdown even if the executor already
    /// inserted the output and the ack is never observed.
    pub(crate) async fn register_output(
        &self,
        output: ConnectionOutputHandle,
    ) -> Result<ConnectionOutputRegistration, IpcError> {
        self.register_output_with_reconnect(output, None, None)
            .await
    }

    pub(crate) async fn register_output_for_connection(
        &self,
        output: ConnectionOutputHandle,
        client_id: ClientId,
        reconnect_from: Option<Uuid>,
    ) -> Result<ConnectionOutputRegistration, IpcError> {
        self.register_output_with_reconnect(
            output,
            Some(client_id),
            reconnect_from.map(ConnectionOutputId::from_uuid),
        )
        .await
    }

    /// Open a bounded host Connect duplex session for `client_id`.
    ///
    /// Allocates a fresh host UUIDv7 output identity (never a network
    /// connection id or `reconnect_from`), registers it through the existing
    /// output queue machinery with cancellation cleanup armed before the first
    /// transferring await, and returns a request handle bound to that output.
    /// Durable replay continues to use canonical event cursors. No tasks are
    /// spawned here.
    pub async fn open_connect_duplex(
        &self,
        client_id: ClientId,
    ) -> Result<HostConnectDuplex, IpcError> {
        let (output, ports) = ConnectionOutputHandle::with_connection_id(
            Uuid::now_v7(),
            HOST_CRITICAL_OUTPUT_QUEUE_CAPACITY,
            HOST_DURABLE_OUTPUT_QUEUE_CAPACITY,
            HOST_EPHEMERAL_OUTPUT_QUEUE_CAPACITY,
        );
        let registration = self
            .register_output_for_connection(output.clone(), client_id, None)
            .await?;
        let requests = self.with_output(registration.id());
        Ok(HostConnectDuplex {
            client_id,
            requests,
            output,
            ports,
            registration,
        })
    }

    async fn register_output_with_reconnect(
        &self,
        output: ConnectionOutputHandle,
        client_id: Option<ClientId>,
        reconnect_from: Option<ConnectionOutputId>,
    ) -> Result<ConnectionOutputRegistration, IpcError> {
        let id = output.id();
        // Arm before any await: cancel must not leave an inserted output without
        // a shutdown owner. Shutdown goes through the handle's synchronized path.
        let registration = ConnectionOutputRegistration {
            id,
            output: output.clone(),
            control_tx: self.control_tx.clone(),
        };
        let (ack_tx, ack_rx) = oneshot::channel();
        self.control_tx
            .send(ExecutorControl::RegisterOutput {
                id,
                output,
                client_id,
                reconnect_from,
                ack: ack_tx,
            })
            .await
            .map_err(|_| IpcError::Unavailable)?;
        ack_rx.await.map_err(|_| IpcError::Unavailable)?;
        Ok(registration)
    }

    #[cfg(test)]
    pub(crate) async fn inspect_output(
        &self,
        id: ConnectionOutputId,
    ) -> Result<OutputInspection, IpcError> {
        let (ack_tx, ack_rx) = oneshot::channel();
        self.control_tx
            .send(ExecutorControl::InspectOutput { id, ack: ack_tx })
            .await
            .map_err(|_| IpcError::Unavailable)?;
        ack_rx.await.map_err(|_| IpcError::Unavailable)
    }

    #[cfg(test)]
    pub(crate) async fn install_test_semantic_journal(
        &self,
        journal: Arc<Mutex<crate::remote::presentation::SemanticJournalStore>>,
    ) -> Result<(), IpcError> {
        let (ack_tx, ack_rx) = oneshot::channel();
        self.control_tx
            .send(ExecutorControl::InstallTestSemanticJournal {
                journal,
                ack: ack_tx,
            })
            .await
            .map_err(|_| IpcError::Unavailable)?;
        ack_rx.await.map_err(|_| IpcError::Unavailable)
    }

    #[cfg(test)]
    pub(crate) async fn record_test_semantic(
        &self,
        draft: crate::remote::presentation::SemanticEventDraft,
    ) -> Result<u64, IpcError> {
        let (ack_tx, ack_rx) = oneshot::channel();
        self.control_tx
            .send(ExecutorControl::RecordTestSemantic { draft, ack: ack_tx })
            .await
            .map_err(|_| IpcError::Unavailable)?;
        ack_rx.await.map_err(|_| IpcError::Unavailable)
    }

    /// Test seam: run exactly one maintenance cleanup/teardown unit on the executor.
    #[cfg(test)]
    pub(crate) async fn run_maintenance_once(&self) -> Result<(), StoreError> {
        let (ack_tx, ack_rx) = oneshot::channel();
        self.control_tx
            .send(ExecutorControl::RunMaintenanceOnce { ack: ack_tx })
            .await
            .map_err(|_| StoreError::Io("executor control channel closed".into()))?;
        ack_rx
            .await
            .map_err(|_| StoreError::Io("maintenance ack dropped".into()))?
    }

    /// Enqueue one authenticated request and await its correlated reply.
    ///
    /// Blocks (with bounded queue backpressure) when the executor queue is full.
    /// Returns [`IpcError::Unavailable`] if the executor has stopped.
    pub async fn execute(
        &self,
        negotiated: NegotiatedParameters,
        request: ClientRequest,
    ) -> Result<ServerMessage, IpcError> {
        match self
            .submit(
                negotiated,
                request,
                HostRequestCompletionRouting::CallerOwned,
            )
            .await?
        {
            DuplexExecuteCompletion::CallerMustWrite(message) => Ok(message),
            DuplexExecuteCompletion::ExecutorAdmittedQuitReceipt { .. } => {
                Err(IpcError::Unavailable)
            }
        }
    }

    /// Duplex path: may return executor-admitted quit receipt (reader must not enqueue).
    pub(crate) async fn execute_for_duplex(
        &self,
        negotiated: NegotiatedParameters,
        request: ClientRequest,
    ) -> Result<DuplexExecuteCompletion, IpcError> {
        self.submit(
            negotiated,
            request,
            HostRequestCompletionRouting::ExecutorOwnsAcceptedHostQuitReceipt,
        )
        .await
    }

    async fn submit(
        &self,
        negotiated: NegotiatedParameters,
        request: ClientRequest,
        routing: HostRequestCompletionRouting,
    ) -> Result<DuplexExecuteCompletion, IpcError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(HostRequestJob {
                negotiated,
                request,
                output_id: self.output_id,
                routing,
                reply: reply_tx,
            })
            .await
            .map_err(|_| IpcError::Unavailable)?;
        reply_rx.await.map_err(|_| IpcError::Unavailable)?
    }

    /// Test seam: take the pending accepted-quit receipt ack for one output, if any.
    #[cfg(test)]
    pub(crate) async fn take_pending_quit_receipt_ack(
        &self,
        id: ConnectionOutputId,
    ) -> Result<Option<(OperationId, PhysicalWriteAck)>, IpcError> {
        let (ack_tx, ack_rx) = oneshot::channel();
        self.control_tx
            .send(ExecutorControl::TakePendingQuitReceiptAck { id, ack: ack_tx })
            .await
            .map_err(|_| IpcError::Unavailable)?;
        ack_rx.await.map_err(|_| IpcError::Unavailable)
    }
}

/// Host-only configuration admission.  The roots are derived once from the
/// sealed ConfigStore issuer; every task admission re-reads the strict store
/// immediately before binding so an external replacement cannot revive stale
/// path authority.
struct HostWorkspaceAdmission {
    store: ConfigStore,
    issuer: crate::config::ConfigWorkspaceIssuer,
    roots: WorkspaceProjectRoots,
    ssh_runtime: HostSshRuntime,
}

/// Host-owned SSH adapter. Config and credential resolution stay behind this
/// boundary; cockpit projections receive only the typed lifecycle snapshot.
struct HostSshRuntime {
    config: crate::config::AppConfig,
    supervisor: Mutex<
        crate::ssh::SshSupervisor<
            crate::services::HostManagedLaunchAuthority,
            crate::ssh::ConfigCredentialResolver,
        >,
    >,
}

impl HostSshRuntime {
    fn new(config: crate::config::AppConfig, key_root: Option<PathBuf>) -> Self {
        let resolver = crate::ssh::ConfigCredentialResolver::new(config.clone());
        let key_store = key_root.and_then(|root| crate::ssh::KeyMaterialStore::new(root).ok());
        let supervisor = crate::ssh::SshSupervisor::with_credentials(
            crate::services::HostManagedLaunchAuthority::new(),
            resolver,
            key_store,
        );
        Self {
            config,
            supervisor: Mutex::new(supervisor),
        }
    }
}

impl crate::ssh::SshRuntimeAdapter for HostSshRuntime {
    fn status_for_task(&self, task_id: TaskId) -> Option<crate::ssh::SshRuntimeSnapshot> {
        self.supervisor
            .lock()
            .ok()
            .and_then(|mut supervisor| supervisor.snapshot_for_task(task_id))
    }

    fn connect_endpoint(
        &self,
        endpoint_id: &str,
        identity: crate::ssh::SshTaskIdentity,
    ) -> Result<crate::ssh::SshRuntimeSnapshot, crate::ssh::SshRuntimeError> {
        let connection = self
            .config
            .ssh_connections
            .iter()
            .find(|connection| connection.id == endpoint_id)
            .cloned()
            .ok_or(crate::ssh::SshRuntimeError::UnknownEndpoint)?;
        let admission = crate::ssh::SshAdmission {
            task_id: identity.task_id,
            agent_session_id: identity.agent_session_id,
            resource_id: identity.resource_id,
            runtime_generation: identity.runtime_generation,
            action_epoch: identity.action_epoch,
            connection,
            cwd: identity.cwd,
        };
        self.supervisor
            .lock()
            .map_err(|_| crate::ssh::SshRuntimeError::Launch)?
            .connect(admission)
    }
}

/// The host's single configured-service authority.  Keeping this alongside
/// the CommandBus executor makes service lifecycle effects share one
/// ProcessManager and prevents a second per-connection supervisor from being
/// constructed.
struct ConfiguredServiceRuntime {
    manager: crate::services::ProcessManager,
    semantic_journal: Arc<Mutex<crate::remote::presentation::SemanticJournalStore>>,
    host_id: crate::services::model::HostId,
    provider_dispatch: crate::providers::dispatch::ProviderDispatchRuntime,
    supervisor_ready: bool,
}

impl ConfiguredServiceRuntime {
    fn initialized_from_admission(
        admission: &HostWorkspaceAdmission,
        provider_session_store_path: Option<std::path::PathBuf>,
        semantic_ingress_tx: mpsc::UnboundedSender<ProviderSemanticIngress>,
        semantic_dirty: Arc<super::conversation_wake::SemanticDirtyBoard>,
    ) -> Option<Self> {
        let manager = provider_session_store_path
            .as_ref()
            .map(|path| {
                crate::services::ProcessManager::new_with_provider_session_store_path(path.clone())
            })
            .unwrap_or_else(crate::services::ProcessManager::new);
        let history_path = provider_session_store_path.as_ref().and_then(|path| {
            path.parent()
                .map(|root| root.join(crate::remote::presentation::CONVERSATION_HISTORY_FILE_NAME))
        });
        let semantic_journal = match history_path {
            Some(path) => match crate::remote::presentation::SemanticJournalStore::open(&path) {
                Ok(store) => Arc::new(Mutex::new(store)),
                Err(error) => {
                    eprintln!(
                        "devmanager-host: conversation history unavailable ({error}); refusing configured service runtime at {}",
                        path.display()
                    );
                    return None;
                }
            },
            None => Arc::new(Mutex::new(
                crate::remote::presentation::SemanticJournalStore::default(),
            )),
        };
        let event_journal = Arc::clone(&semantic_journal);
        let dirty_board = Arc::clone(&semantic_dirty);
        manager.set_remote_session_handler(Some(Arc::new(move |event| {
            let Ok(mut journal) = event_journal.lock() else {
                return;
            };
            let now_ms = unix_time_ms_u64();
            match event {
                crate::services::RemoteSessionEvent::Runtime { runtime, .. } => {
                    journal.observe_runtime(&runtime, &[], now_ms);
                }
                crate::services::RemoteSessionEvent::Output {
                    session_id,
                    bytes,
                    mode,
                    screen,
                } => {
                    journal.observe_output(&session_id, &bytes, screen.as_ref(), now_ms);
                    journal.observe_native_terminal_mode(&session_id, mode, now_ms);
                }
                crate::services::RemoteSessionEvent::Removed { session_id } => {
                    journal.remove_session_binding(&session_id);
                }
                crate::services::RemoteSessionEvent::Semantic { mut draft }
                | crate::services::RemoteSessionEvent::ClaudeSemantic { mut draft, .. }
                | crate::services::RemoteSessionEvent::CodexSemantic { mut draft, .. } => {
                    let ingress = normalize_provider_question_draft(&mut draft);
                    // Record once, then signal the bounded dirty board. Do not
                    // emit per-token DomainEvents or enlarge question ingress.
                    let recorded = journal.record(draft);
                    dirty_board.mark(recorded.stable_session_key.clone(), recorded.sequence);
                    if let Err(error) = journal.flush_if_dirty() {
                        eprintln!(
                            "devmanager-host: conversation history prompt flush failed: {error}"
                        );
                    }
                    drop(journal);
                    if let Some(ingress) = ingress {
                        let _ = semantic_ingress_tx.send(ingress);
                    }
                }
                crate::services::RemoteSessionEvent::AdapterHealth {
                    stable_session_key,
                    health,
                } => {
                    journal.set_adapter_health(&stable_session_key, health);
                }
                crate::services::RemoteSessionEvent::ClaudeAdapterRegistered { .. }
                | crate::services::RemoteSessionEvent::ClaudeAdapterRemoved { .. }
                | crate::services::RemoteSessionEvent::CodexAdapterRegistered { .. }
                | crate::services::RemoteSessionEvent::CodexAdapterRemoved { .. } => {}
            }
        })));
        let host_id = crate::services::model::HostId::new(u64::from(std::process::id()));
        let config = &admission.store.snapshot().config;
        let supervisor_ready = (|| -> Option<()> {
            // Resolve folder env files on the host before handing source references
            // to the binding layer. Values remain in the redacted supervisor
            // overlay; they never enter the action catalog or a client projection.
            // Relative env-file paths resolve under project root + folder path and
            // fail closed on traversal; absolute paths keep absolute behavior.
            let mut env_files = Vec::new();
            for project in &config.projects {
                for folder in &project.folders {
                    let env = match folder.env_file_path.as_ref() {
                        None => None,
                        Some(path) => {
                            // Containment/path-validation errors fail closed and
                            // prevent supervisor initialization. A valid path whose
                            // file is missing or unreadable stays an optional overlay.
                            let resolved = crate::services::resolve_configured_env_file_path(
                                &project.root_path,
                                &folder.folder_path,
                                path,
                            )
                            .ok()?;
                            crate::services::env_service::read_env_map(&resolved)
                                .ok()
                                .map(|values| {
                                    values
                                        .into_iter()
                                        .collect::<std::collections::BTreeMap<_, _>>()
                                })
                        }
                    };
                    env_files.push(env);
                }
            }

            let mut sources = Vec::new();
            let mut env_index = 0;
            for project in &config.projects {
                for folder in &project.folders {
                    for command in &folder.commands {
                        sources.push(crate::services::binding::ConfiguredServiceSource {
                            project,
                            folder,
                            command,
                            owner: crate::services::binding::ConfiguredServiceOwner::Workspace {
                                project_id: project.id.clone(),
                                folder_id: folder.id.clone(),
                            },
                            folder_env_file: env_files.get(env_index).and_then(Option::as_ref),
                        });
                    }
                    env_index += 1;
                }
            }

            manager
                .ensure_configured_service_supervisor(sources, host_id, unix_time_ms_u64())
                .ok()
        })()
        .is_some();
        apply_admitted_notification_sound(&manager, config.settings());
        Some(Self {
            provider_dispatch:
                crate::providers::dispatch::ProviderDispatchRuntime::from_process_manager(
                    manager.clone(),
                ),
            manager,
            semantic_journal,
            host_id,
            supervisor_ready,
        })
    }
}

fn apply_admitted_notification_sound(
    manager: &crate::services::ProcessManager,
    settings: &crate::config::model::Settings,
) {
    manager.set_notification_sound(settings.notification_sound.clone().into_option());
}

fn unix_time_ms_u64() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

impl HostWorkspaceAdmission {
    fn new(
        mut store: ConfigStore,
        action_epoch: u64,
        runtime_generation: u64,
    ) -> Result<Self, ConfigError> {
        let revision = store.snapshot().revision;
        let issuer = store.issue_workspace_authority(revision, action_epoch, runtime_generation)?;
        let roots = WorkspaceProjectRoots::from_config_issuer(&issuer).map_err(|_| {
            ConfigError::new(
                crate::config::ConfigErrorKind::Validation,
                "configured workspace roots are unavailable",
            )
        })?;
        let ssh_runtime = HostSshRuntime::new(
            store.snapshot().config.clone(),
            store.path().parent().map(|parent| parent.join("ssh-keys")),
        );
        Ok(Self {
            store,
            issuer,
            roots,
            ssh_runtime,
        })
    }

    fn validate_current(&self) -> Result<(), ConfigError> {
        self.store.validate_workspace_issuer_current(&self.issuer)
    }

    fn roots(&self) -> &WorkspaceProjectRoots {
        &self.roots
    }

    fn action_epoch(&self) -> u64 {
        self.issuer.action_epoch()
    }

    fn runtime_generation(&self) -> u64 {
        self.issuer.runtime_generation()
    }

    fn redacted_ssh_endpoints(&self) -> Vec<crate::domain::TaskSshEndpoint> {
        crate::ssh::redacted_endpoints(&self.store.snapshot().config.ssh_connections)
    }

    fn create_user_project(&mut self, name: &str, root_path: &str) -> Result<(), ConfigError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(ConfigError::new(
                ConfigErrorKind::Validation,
                "project name is empty",
            ));
        }
        let root = std::path::Path::new(root_path.trim());
        let validated = crate::workspace::service::validate_host_workspace_path(root, true)
            .map_err(|_| {
                ConfigError::new(ConfigErrorKind::Validation, "project folder is unavailable")
            })?;
        let timestamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|duration| duration.as_secs().to_string())
            .unwrap_or_else(|_| "0".to_string());
        let project = Project {
            id: format!("project-{}", Uuid::now_v7()),
            name: name.to_string(),
            root_path: validated.path.to_string_lossy().into_owned(),
            folders: Vec::new(),
            color: Nullable::Null,
            pinned: Nullable::Value(false),
            notes: Nullable::Null,
            save_log_files: Nullable::Value(true),
            created_at: timestamp.clone(),
            updated_at: timestamp,
            archived: Nullable::Null,
            extra: Default::default(),
        };
        let revision = self.store.snapshot().revision;
        self.store
            .execute(revision, ConfigCommand::CreateProject { project })?;
        let revision = self.store.snapshot().revision;
        let issuer = self.store.issue_workspace_authority(
            revision,
            self.issuer.action_epoch(),
            self.issuer.runtime_generation(),
        )?;
        let roots = WorkspaceProjectRoots::from_config_issuer(&issuer).map_err(|_| {
            ConfigError::new(
                ConfigErrorKind::Validation,
                "configured workspace roots are unavailable",
            )
        })?;
        self.issuer = issuer;
        self.roots = roots;
        self.ssh_runtime = HostSshRuntime::new(
            self.store.snapshot().config.clone(),
            self.store
                .path()
                .parent()
                .map(|parent| parent.join("ssh-keys")),
        );
        Ok(())
    }

    fn create_user_project_outcome(&mut self, name: &str, root_path: &str) -> QueryOutcome {
        match self.create_user_project(name, root_path) {
            Ok(()) => QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Config(
                super::cockpit::config_sidebar_snapshot(&self.store.snapshot().config),
            ))),
            Err(error) if error.kind() == ConfigErrorKind::Validation => {
                QueryOutcome::Err(QueryError::InvalidRequest)
            }
            Err(_) => QueryOutcome::Err(QueryError::Unavailable {
                reason: "config_create",
            }),
        }
    }

    fn apply_project_action_outcome(&mut self, query: &TaskCockpitQuery) -> QueryOutcome {
        match self.apply_project_action(query) {
            Ok(()) => QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Config(
                super::cockpit::config_sidebar_snapshot(&self.store.snapshot().config),
            ))),
            Err(error) if error.kind() == ConfigErrorKind::Validation => {
                QueryOutcome::Err(QueryError::InvalidRequest)
            }
            Err(_) => QueryOutcome::Err(QueryError::Unavailable {
                reason: "config_mutate",
            }),
        }
    }

    fn apply_project_action(&mut self, query: &TaskCockpitQuery) -> Result<(), ConfigError> {
        let revision = self.store.snapshot().revision;
        match query {
            TaskCockpitQuery::ConfigUpsertCommand {
                project_id,
                folder_id,
                command_id,
                label,
                command,
            } => {
                let label = label.trim();
                let command_text = command.trim();
                if label.is_empty() || command_text.is_empty() {
                    return Err(ConfigError::new(
                        ConfigErrorKind::Validation,
                        "project action requires a name and command",
                    ));
                }
                let config = self.store.snapshot().config.clone();
                let project = config
                    .projects
                    .iter()
                    .find(|project| project.id == *project_id)
                    .ok_or_else(|| {
                        ConfigError::new(ConfigErrorKind::Validation, "project is unavailable")
                    })?;
                let resolved_folder_id = if folder_id.trim().is_empty() {
                    if let Some(existing) = project.folders.iter().find(|folder| {
                        !matches!(folder.archived, crate::config::Nullable::Value(true))
                    }) {
                        existing.id.clone()
                    } else {
                        let folder = crate::config::ProjectFolder {
                            id: format!("folder-{}", Uuid::now_v7()),
                            name: "Default".to_string(),
                            folder_path: ".".to_string(),
                            commands: Vec::new(),
                            env_file_path: crate::config::Nullable::Null,
                            port_variable: crate::config::Nullable::Null,
                            hidden: crate::config::Nullable::Null,
                            archived: crate::config::Nullable::Null,
                            extra: Default::default(),
                        };
                        let created_id = folder.id.clone();
                        self.store.execute(
                            revision,
                            ConfigCommand::CreateFolder {
                                project_id: project_id.clone(),
                                folder,
                            },
                        )?;
                        created_id
                    }
                } else {
                    folder_id.clone()
                };
                let revision = self.store.snapshot().revision;
                let config = self.store.snapshot().config.clone();
                let project = config
                    .projects
                    .iter()
                    .find(|project| project.id == *project_id)
                    .ok_or_else(|| {
                        ConfigError::new(ConfigErrorKind::Validation, "project is unavailable")
                    })?;
                let folder = project
                    .folders
                    .iter()
                    .find(|folder| folder.id == resolved_folder_id)
                    .ok_or_else(|| {
                        ConfigError::new(
                            ConfigErrorKind::Validation,
                            "project folder is unavailable",
                        )
                    })?;
                if let Some(existing_id) = command_id.as_ref() {
                    let existing = folder
                        .commands
                        .iter()
                        .find(|entry| entry.id == *existing_id)
                        .cloned()
                        .ok_or_else(|| {
                            ConfigError::new(ConfigErrorKind::Validation, "action is unavailable")
                        })?;
                    let mut updated = existing;
                    updated.label = label.to_string();
                    updated.command = command_text.to_string();
                    self.store.execute(
                        revision,
                        ConfigCommand::UpdateCommand {
                            project_id: project_id.clone(),
                            folder_id: resolved_folder_id.clone(),
                            command: updated,
                        },
                    )?;
                } else {
                    let timestamp = SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .map(|duration| duration.as_secs().to_string())
                        .unwrap_or_else(|_| "0".to_string());
                    let created = crate::config::RunCommand {
                        id: format!("command-{}", Uuid::now_v7()),
                        label: label.to_string(),
                        command: command_text.to_string(),
                        args: Vec::new(),
                        env: crate::config::Nullable::Null,
                        port: crate::config::Nullable::Null,
                        auto_restart: crate::config::Nullable::Value(false),
                        clear_logs_on_restart: crate::config::Nullable::Value(false),
                        archived: crate::config::Nullable::Null,
                        extra: Default::default(),
                    };
                    let _ = timestamp;
                    self.store.execute(
                        revision,
                        ConfigCommand::CreateCommand {
                            project_id: project_id.clone(),
                            folder_id: resolved_folder_id,
                            command: created,
                        },
                    )?;
                }
            }
            TaskCockpitQuery::ConfigArchiveCommand {
                project_id,
                folder_id,
                command_id,
            } => {
                self.store.execute(
                    revision,
                    ConfigCommand::ArchiveCommand {
                        project_id: project_id.clone(),
                        folder_id: folder_id.clone(),
                        command_id: command_id.clone(),
                    },
                )?;
            }
            TaskCockpitQuery::ConfigRunCommand { .. } => {
                // Run is admitted as a typed query so the native client can
                // request it, but process launch remains owned by the service
                // runtime. Without a bound service runtime this fails closed.
                return Err(ConfigError::new(
                    ConfigErrorKind::Validation,
                    "project action run requires the host service runtime",
                ));
            }
            _ => {
                return Err(ConfigError::new(
                    ConfigErrorKind::Validation,
                    "unsupported project action",
                ))
            }
        }
        let revision = self.store.snapshot().revision;
        let issuer = self.store.issue_workspace_authority(
            revision,
            self.issuer.action_epoch(),
            self.issuer.runtime_generation(),
        )?;
        let roots = WorkspaceProjectRoots::from_config_issuer(&issuer).map_err(|_| {
            ConfigError::new(
                ConfigErrorKind::Validation,
                "configured workspace roots are unavailable",
            )
        })?;
        self.issuer = issuer;
        self.roots = roots;
        self.ssh_runtime = HostSshRuntime::new(
            self.store.snapshot().config.clone(),
            self.store
                .path()
                .parent()
                .map(|parent| parent.join("ssh-keys")),
        );
        Ok(())
    }
}

/// Exclusive owner of [`CommandBus`]. Runs on one task and drains a bounded queue.
pub struct HostRequestExecutor {
    bus: CommandBus,
    workspace_projects: WorkspaceProjectRoots,
    config_admission: Option<HostWorkspaceAdmission>,
    configured_service_runtime: Option<ConfiguredServiceRuntime>,
    /// One bounded exact-resume enumeration is consumed incrementally after the
    /// request loop is live. It is never retried as a fresh conversation.
    provider_restore_pending: bool,
    provider_restore_queue: VecDeque<crate::domain::command::StartProviderSessionIntent>,
    provider_restore_jobs: FuturesUnordered<ProviderRestoreFuture>,
    /// Tasks that already have a restore/start attempt in flight. Held dispatch
    /// must not enqueue a duplicate StartProviderSessionIntent for the same task.
    provider_restore_in_flight: HashSet<TaskId>,
    /// Exact user-selected launch options for a provider start admitted during
    /// this host lifetime. A held first input can race SessionStart/terminal
    /// publication; reconstructing that retry with defaults would silently
    /// change the selected model, reasoning effort, or access mode.
    provider_launch_options: HashMap<TaskId, crate::providers::adapter::ProviderLaunchOptions>,
    /// Tasks whose currently held input has no durable provider restart facts.
    /// Keep restart lookup and logging edge-triggered until a fresh action
    /// explicitly rearms the task.
    provider_restart_unavailable: HashSet<TaskId>,
    provider_dispatch_maintenance_due: Instant,
    /// A failed restore fence suppresses only the same durable action epoch.
    /// A later user action advances the epoch and may retry intentionally.
    provider_restore_failed_action_epochs: HashMap<TaskId, u64>,
    /// Native profile provider settings + health (exact `--config-base` root).
    provider_settings:
        Option<std::sync::Arc<crate::providers::settings::ProviderSettingsAuthority>>,
    provider_health_jobs: FuturesUnordered<super::provider_health::ProviderHealthFuture>,
    /// Host-owned terminal admission. It only writes to terminals explicitly
    /// attached by the task runtime; an unbound/missing terminal fails closed.
    terminal_service: TerminalService,
    update_gate: Arc<crate::host::update::HostUpdateRuntimeGate>,
    host_boot_id: Arc<OnceLock<Uuid>>,
    remote_setup: Arc<OnceLock<super::remote_setup::RemoteSetupHandle>>,
    rx: mpsc::Receiver<HostRequestJob>,
    control_rx: mpsc::Receiver<ExecutorControl>,
    semantic_ingress_rx: mpsc::UnboundedReceiver<ProviderSemanticIngress>,
    // Keep the queue open for executors without configured provider services.
    _semantic_ingress_guard: mpsc::UnboundedSender<ProviderSemanticIngress>,
    control_closed: bool,
    registry: SnapshotRegistry,
    replay_registry: EventReplayRegistry,
    artifact_content_registry: ArtifactContentRegistry,
    conversation_registry: super::conversation_wake::ConversationSubscriptionRegistry,
    semantic_dirty: Arc<super::conversation_wake::SemanticDirtyBoard>,
    semantic_dirty_rx: watch::Receiver<u64>,
    /// Shared wake when any output frees an ephemeral slot so conversation
    /// dirties can retry without waiting for a new token or maintenance tick.
    ephemeral_capacity_notify: Arc<Notify>,
    /// Test-only in-memory journal when no configured-service runtime is present.
    #[cfg(test)]
    test_semantic_journal: Option<Arc<Mutex<crate::remote::presentation::SemanticJournalStore>>>,
    outputs: HashMap<ConnectionOutputId, ConnectionOutputHandle>,
    /// Latest accepted ConfirmHostQuit receipt ack per output (for terminal drain).
    pending_quit_receipt_acks: HashMap<ConnectionOutputId, PendingQuitReceiptAck>,
    /// Exact PrepareUpdate replies retained for same-command retries after a
    /// client delivery failure or connection-epoch mismatch.
    prepared_update_replies: HashMap<crate::domain::id::CommandId, PreparedUpdateReply>,
    /// Supervised foreground only: capacity-one arm sender to the host supervisor.
    arm_tx: Option<mpsc::Sender<PhysicalExitArmRequest>>,
    /// One host-owned workspace resource coordinator. CreateTask and Task
    /// Cockpit Git/file leases share this instance; queries never mint another.
    workspace_coordinator: WorkspaceResourceCoordinator,
    /// Live plain shell terminals this host spawned, keyed by their durable
    /// resource. The fact pump samples exactly these.
    shell_sessions: HashMap<ResourceId, ShellSessionLink>,
}

/// One live plain shell terminal: the durable identity, the manager session
/// behind it, and the sampling state the fact pump carries between ticks.
struct ShellSessionLink {
    task_id: TaskId,
    session_id: crate::terminal::protocol::TerminalSessionId,
    last_cwd: Option<PathBuf>,
    /// When the sampled cwd last changed. `None` means the current value has
    /// already been recorded (or refused), so it must not be recorded again.
    last_cwd_change: Option<Instant>,
    /// When this link last offered an activity fact. `None` means never.
    last_activity_offer: Option<Instant>,
    /// Hosted-terminal delta sequence at the last offered activity fact.
    /// Activity means the terminal produced output, never that its process is
    /// still alive, so an unchanged sequence offers nothing.
    last_activity_sequence: u64,
    exit_recorded: bool,
}

impl ShellSessionLink {
    /// Fold one cwd sample into the link and answer with the directory that is
    /// now settled enough to become a durable fact, if any.
    ///
    /// A directory the shell is only passing through restarts the timer instead
    /// of being recorded, and a settled directory is answered exactly once: the
    /// durable layer also suppresses an unchanged cwd, but a host that re-asked
    /// every tick would take a store transaction per shell per tick to be told
    /// nothing happened.
    fn note_cwd_sample(
        &mut self,
        observed: Option<PathBuf>,
        now: Instant,
        debounce: Duration,
    ) -> Option<PathBuf> {
        // A sample the manager could not take is not a move. Retracting or
        // re-arming on it would flap a settled directory against the launch
        // directory every time a single read failed.
        let Some(observed) = observed else {
            return None;
        };
        if Some(&observed) != self.last_cwd.as_ref() {
            self.last_cwd = Some(observed.clone());
            self.last_cwd_change = Some(now);
        }
        if !self
            .last_cwd_change
            .is_some_and(|at| now.duration_since(at) >= debounce)
        {
            return None;
        }
        self.last_cwd_change = None;
        Some(observed)
    }

    /// Whether this tick should offer an activity fact.
    ///
    /// Two gates, and the first is the meaning of the fact: activity is real
    /// terminal output, never liveness, so an unchanged hosted-terminal
    /// sequence offers nothing however long the shell has been running. A
    /// heartbeat here would make every idle shell look busy forever.
    ///
    /// The second is cost. The durable rule already coalesces activity to
    /// [`TERMINAL_ACTIVITY_COALESCE_MS`] and this defers to that same window
    /// rather than defining a second one; without it a chatty shell would open
    /// a write transaction every second only to be told the fact is
    /// suppressed. The sequence is recorded only when the offer is actually
    /// made, so output that arrives inside the window is still reported when
    /// the window closes.
    fn should_offer_activity(&mut self, now: Instant, sequence: u64) -> bool {
        if sequence == self.last_activity_sequence {
            return false;
        }
        let window = Duration::from_millis(TERMINAL_ACTIVITY_COALESCE_MS as u64);
        if self
            .last_activity_offer
            .is_some_and(|at| now.duration_since(at) < window)
        {
            return false;
        }
        self.last_activity_offer = Some(now);
        self.last_activity_sequence = sequence;
        true
    }
}

/// One accepted terminal-lifecycle command that owes a host process effect.
#[derive(Clone, Copy)]
enum AcceptedShellEffect {
    /// Retained deliberately, and unreachable today.
    ///
    /// `Command::OpenShellTerminal` is host-authority-only and refused on both
    /// wire lanes, so no accepted client request can carry one. The mapping
    /// stays total anyway: `Close` shares this function and IS client-sendable,
    /// and if the open gate is ever relaxed the process effect must fire with
    /// it rather than leaving a registered resource with no shell behind it.
    /// `only_shell_lifecycle_commands_owe_a_host_process_effect` pins the
    /// mapping either way.
    Open {
        task_id: TaskId,
        resource_id: ResourceId,
    },
    Close {
        resource_id: ResourceId,
    },
}

fn provider_restore_success_attention(
    current: crate::domain::task::TaskAttention,
) -> Option<crate::domain::task::TaskAttention> {
    (current == crate::domain::task::TaskAttention::Failed)
        .then_some(crate::domain::task::TaskAttention::None)
}

/// Exact restore/reattachment failure is a runtime attachment outcome, never a
/// durable task-operation failure. Returning `None` keeps existing attention.
/// Legacy restore-poisoned `Failed` is cleared to `None` when an exact provider
/// session identity is already bound.
fn next_attention_after_provider_restore_failure(
    snapshot: &crate::domain::snapshot::TaskSnapshot,
) -> Option<crate::domain::task::TaskAttention> {
    let has_exact_session = snapshot
        .agents
        .values()
        .any(|agent| agent.provider_session_id.is_some());
    if has_exact_session && snapshot.attention == crate::domain::task::TaskAttention::Failed {
        return Some(crate::domain::task::TaskAttention::None);
    }
    provider_restore_failure_attention(snapshot.attention)
}

fn provider_restore_failure_attention(
    _current: crate::domain::task::TaskAttention,
) -> Option<crate::domain::task::TaskAttention> {
    None
}

impl HostRequestExecutor {
    /// Spawn the single CommandBus executor task.
    ///
    /// The returned handle may be cloned for every connection task. Dropping
    /// every handle closes the queue; the executor then finishes after draining
    /// any already-queued jobs.
    pub fn start(bus: CommandBus) -> (HostRequestHandle, JoinHandle<()>) {
        Self::start_with_workspace_projects(bus, WorkspaceProjectRoots::empty())
    }

    /// Spawn the executor with the host-owned ProjectId-to-root mapping.
    pub(crate) fn start_with_workspace_projects(
        bus: CommandBus,
        workspace_projects: WorkspaceProjectRoots,
    ) -> (HostRequestHandle, JoinHandle<()>) {
        Self::spawn(bus, true, workspace_projects)
    }

    /// Supervised foreground start: arm channel + typed intentional exit outcome.
    ///
    /// Ordinary [`Self::start`] callers are unchanged. The supervisor must drop the
    /// pending accept listener before acknowledging [`PhysicalExitArmRequest`].
    pub fn start_supervised(bus: CommandBus) -> (HostRequestHandle, SupervisedHostExecutor) {
        Self::start_supervised_with_workspace_projects(bus, WorkspaceProjectRoots::empty())
    }

    /// Supervised start with the host-owned ProjectId-to-root mapping.
    pub(crate) fn start_supervised_with_workspace_projects(
        bus: CommandBus,
        workspace_projects: WorkspaceProjectRoots,
    ) -> (HostRequestHandle, SupervisedHostExecutor) {
        let (handle, join, arm_rx) = Self::spawn_supervised(bus, true, workspace_projects);
        (handle, SupervisedHostExecutor { arm_rx, join })
    }

    /// Start the supervised host from a ConfigStore-issued workspace
    /// authority. Raw configured id/root pairs never enter this API.
    pub fn start_supervised_with_config_store(
        bus: CommandBus,
        store: ConfigStore,
        profile_root: &std::path::Path,
    ) -> Result<(HostRequestHandle, SupervisedHostExecutor), ConfigError> {
        Self::start_supervised_with_config_store_at_generation(bus, store, profile_root, 1, 1)
    }

    /// Test-only compatibility seam for the workspace service contract suite.
    /// Production host admission must use [`Self::start_supervised_with_config_store`].
    #[cfg(test)]
    pub(crate) fn start_supervised_with_project_config(
        bus: CommandBus,
        projects: Vec<(String, String)>,
    ) -> Result<(HostRequestHandle, SupervisedHostExecutor), WorkspaceProjectRootsError> {
        let workspace_projects = WorkspaceProjectRoots::try_from_config(projects)?;
        Ok(Self::start_supervised_with_workspace_projects(
            bus,
            workspace_projects,
        ))
    }

    pub(crate) fn start_supervised_with_config_store_at_generation(
        bus: CommandBus,
        store: ConfigStore,
        profile_root: &std::path::Path,
        action_epoch: u64,
        runtime_generation: u64,
    ) -> Result<(HostRequestHandle, SupervisedHostExecutor), ConfigError> {
        let admission = HostWorkspaceAdmission::new(store, action_epoch, runtime_generation)?;
        let workspace_projects = admission.roots().clone();
        let (handle, join, arm_rx) = Self::spawn_supervised_with_admission(
            bus,
            true,
            workspace_projects,
            admission,
            profile_root.join("provider-sessions.sqlite3"),
        );
        Ok((handle, SupervisedHostExecutor { arm_rx, join }))
    }

    /// Test-only: same executor as [`Self::start`], but without the automatic
    /// maintenance timer so explicit [`HostRequestHandle::run_maintenance_once`]
    /// calls are the only cleanup/teardown driver.
    #[cfg(test)]
    fn start_without_automatic_maintenance(bus: CommandBus) -> (HostRequestHandle, JoinHandle<()>) {
        Self::spawn(bus, false, WorkspaceProjectRoots::empty())
    }

    #[cfg(test)]
    fn start_without_automatic_maintenance_with_workspace_projects(
        bus: CommandBus,
        workspace_projects: WorkspaceProjectRoots,
    ) -> (HostRequestHandle, JoinHandle<()>) {
        Self::spawn(bus, false, workspace_projects)
    }

    /// Test-only supervised start without the automatic maintenance timer.
    #[cfg(test)]
    fn start_supervised_without_automatic_maintenance(
        bus: CommandBus,
    ) -> (HostRequestHandle, SupervisedHostExecutor) {
        let (handle, join, arm_rx) =
            Self::spawn_supervised(bus, false, WorkspaceProjectRoots::empty());
        (handle, SupervisedHostExecutor { arm_rx, join })
    }

    fn spawn_supervised(
        bus: CommandBus,
        schedule_automatic_maintenance: bool,
        workspace_projects: WorkspaceProjectRoots,
    ) -> (
        HostRequestHandle,
        JoinHandle<Result<HostExecutorOutcome, StoreError>>,
        mpsc::Receiver<PhysicalExitArmRequest>,
    ) {
        Self::spawn_supervised_inner(
            bus,
            schedule_automatic_maintenance,
            workspace_projects,
            None,
        )
    }

    fn spawn_supervised_with_admission(
        bus: CommandBus,
        schedule_automatic_maintenance: bool,
        workspace_projects: WorkspaceProjectRoots,
        admission: HostWorkspaceAdmission,
        provider_session_store_path: std::path::PathBuf,
    ) -> (
        HostRequestHandle,
        JoinHandle<Result<HostExecutorOutcome, StoreError>>,
        mpsc::Receiver<PhysicalExitArmRequest>,
    ) {
        Self::spawn_supervised_inner(
            bus,
            schedule_automatic_maintenance,
            workspace_projects,
            Some((admission, provider_session_store_path)),
        )
    }

    fn spawn_supervised_inner(
        bus: CommandBus,
        schedule_automatic_maintenance: bool,
        workspace_projects: WorkspaceProjectRoots,
        config_admission: Option<(HostWorkspaceAdmission, std::path::PathBuf)>,
    ) -> (
        HostRequestHandle,
        JoinHandle<Result<HostExecutorOutcome, StoreError>>,
        mpsc::Receiver<PhysicalExitArmRequest>,
    ) {
        let (tx, rx) = mpsc::channel(HOST_REQUEST_QUEUE_CAPACITY);
        let (control_tx, control_rx) = mpsc::channel(HOST_REQUEST_QUEUE_CAPACITY);
        let (semantic_ingress_tx, semantic_ingress_rx) = mpsc::unbounded_channel();
        let (arm_tx, arm_rx) = mpsc::channel(1);
        let update_gate = crate::host::update::HostUpdateRuntimeGate::new();
        let (semantic_dirty, semantic_dirty_rx) =
            super::conversation_wake::SemanticDirtyBoard::new();
        let ephemeral_capacity_notify = Arc::new(Notify::new());
        let configured_service_runtime =
            config_admission
                .as_ref()
                .and_then(|(admission, provider_session_store_path)| {
                    ConfiguredServiceRuntime::initialized_from_admission(
                        admission,
                        Some(provider_session_store_path.clone()),
                        semantic_ingress_tx.clone(),
                        Arc::clone(&semantic_dirty),
                    )
                });
        let configured_service_supervisor_ready = configured_service_runtime
            .as_ref()
            .is_some_and(|runtime| runtime.supervisor_ready);
        let provider_restore_pending = configured_service_runtime.is_some();
        let handle = HostRequestHandle {
            tx,
            control_tx,
            output_id: None,
            update_gate: Arc::clone(&update_gate),
            host_boot_id: Arc::new(OnceLock::new()),
            remote_setup: Arc::new(OnceLock::new()),
            configured_service_supervisor_ready,
        };
        let provider_settings = config_admission.as_ref().and_then(|(_, store_path)| {
            let root = store_path.parent()?;
            match crate::providers::settings::ProviderProfileOwner::open_dir(root) {
                Ok(profile) => Some(std::sync::Arc::new(
                    crate::providers::settings::ProviderSettingsAuthority::from_profile(profile),
                )),
                Err(error) => {
                    eprintln!(
                        "devmanager-host: provider settings unavailable under {}: {error}",
                        root.display()
                    );
                    None
                }
            }
        });
        let host_boot_id = Arc::clone(&handle.host_boot_id);
        let remote_setup = Arc::clone(&handle.remote_setup);
        let mut executor = Self {
            bus,
            workspace_projects,
            config_admission: config_admission.map(|(admission, _)| admission),
            configured_service_runtime,
            provider_restore_pending,
            provider_restore_queue: VecDeque::new(),
            provider_restore_jobs: FuturesUnordered::new(),
            provider_restore_in_flight: HashSet::new(),
            provider_launch_options: HashMap::new(),
            provider_restart_unavailable: HashSet::new(),
            provider_dispatch_maintenance_due: Instant::now(),
            provider_restore_failed_action_epochs: HashMap::new(),
            provider_settings,
            provider_health_jobs: FuturesUnordered::new(),
            terminal_service: TerminalService::new(),
            update_gate,
            host_boot_id,
            remote_setup,
            rx,
            control_rx,
            semantic_ingress_rx,
            _semantic_ingress_guard: semantic_ingress_tx,
            control_closed: false,
            registry: SnapshotRegistry::new(),
            replay_registry: EventReplayRegistry::new(),
            artifact_content_registry: ArtifactContentRegistry::new(),
            conversation_registry: super::conversation_wake::ConversationSubscriptionRegistry::new(
            ),
            semantic_dirty,
            semantic_dirty_rx,
            ephemeral_capacity_notify,
            #[cfg(test)]
            test_semantic_journal: None,
            outputs: HashMap::with_capacity(MAX_SNAPSHOT_SESSIONS),
            pending_quit_receipt_acks: HashMap::with_capacity(MAX_SNAPSHOT_SESSIONS),
            prepared_update_replies: HashMap::with_capacity(MAX_PREPARED_UPDATE_HANDOFFS),
            arm_tx: Some(arm_tx),
            workspace_coordinator: WorkspaceResourceCoordinator::new(),
            shell_sessions: HashMap::new(),
        };
        let join = tokio::spawn(async move {
            executor
                .run_supervised(schedule_automatic_maintenance)
                .await
        });
        (handle, join, arm_rx)
    }

    fn spawn(
        bus: CommandBus,
        schedule_automatic_maintenance: bool,
        workspace_projects: WorkspaceProjectRoots,
    ) -> (HostRequestHandle, JoinHandle<()>) {
        Self::spawn_inner(
            bus,
            schedule_automatic_maintenance,
            workspace_projects,
            None,
        )
    }

    fn spawn_inner(
        bus: CommandBus,
        schedule_automatic_maintenance: bool,
        workspace_projects: WorkspaceProjectRoots,
        config_admission: Option<HostWorkspaceAdmission>,
    ) -> (HostRequestHandle, JoinHandle<()>) {
        let (tx, rx) = mpsc::channel(HOST_REQUEST_QUEUE_CAPACITY);
        let (control_tx, control_rx) = mpsc::channel(HOST_REQUEST_QUEUE_CAPACITY);
        let (semantic_ingress_tx, semantic_ingress_rx) = mpsc::unbounded_channel();
        let update_gate = crate::host::update::HostUpdateRuntimeGate::new();
        let (semantic_dirty, semantic_dirty_rx) =
            super::conversation_wake::SemanticDirtyBoard::new();
        let ephemeral_capacity_notify = Arc::new(Notify::new());
        let configured_service_runtime = config_admission.as_ref().and_then(|admission| {
            ConfiguredServiceRuntime::initialized_from_admission(
                admission,
                None,
                semantic_ingress_tx.clone(),
                Arc::clone(&semantic_dirty),
            )
        });
        let configured_service_supervisor_ready = configured_service_runtime
            .as_ref()
            .is_some_and(|runtime| runtime.supervisor_ready);
        let provider_restore_pending = configured_service_runtime.is_some();
        let handle = HostRequestHandle {
            tx,
            control_tx,
            output_id: None,
            update_gate: Arc::clone(&update_gate),
            host_boot_id: Arc::new(OnceLock::new()),
            remote_setup: Arc::new(OnceLock::new()),
            configured_service_supervisor_ready,
        };
        let host_boot_id = Arc::clone(&handle.host_boot_id);
        let remote_setup = Arc::clone(&handle.remote_setup);
        let mut executor = Self {
            bus,
            workspace_projects,
            config_admission,
            configured_service_runtime,
            provider_restore_pending,
            provider_restore_queue: VecDeque::new(),
            provider_restore_jobs: FuturesUnordered::new(),
            provider_restore_in_flight: HashSet::new(),
            provider_launch_options: HashMap::new(),
            provider_restart_unavailable: HashSet::new(),
            provider_dispatch_maintenance_due: Instant::now(),
            provider_restore_failed_action_epochs: HashMap::new(),
            provider_settings: None,
            provider_health_jobs: FuturesUnordered::new(),
            terminal_service: TerminalService::new(),
            update_gate,
            host_boot_id,
            remote_setup,
            rx,
            control_rx,
            semantic_ingress_rx,
            _semantic_ingress_guard: semantic_ingress_tx,
            control_closed: false,
            registry: SnapshotRegistry::new(),
            replay_registry: EventReplayRegistry::new(),
            artifact_content_registry: ArtifactContentRegistry::new(),
            conversation_registry: super::conversation_wake::ConversationSubscriptionRegistry::new(
            ),
            semantic_dirty,
            semantic_dirty_rx,
            ephemeral_capacity_notify,
            #[cfg(test)]
            test_semantic_journal: None,
            outputs: HashMap::with_capacity(MAX_SNAPSHOT_SESSIONS),
            pending_quit_receipt_acks: HashMap::with_capacity(MAX_SNAPSHOT_SESSIONS),
            prepared_update_replies: HashMap::with_capacity(MAX_PREPARED_UPDATE_HANDOFFS),
            arm_tx: None,
            workspace_coordinator: WorkspaceResourceCoordinator::new(),
            shell_sessions: HashMap::new(),
        };
        let join = tokio::spawn(async move {
            executor.run(schedule_automatic_maintenance).await;
        });
        (handle, join)
    }

    async fn run(&mut self, schedule_automatic_maintenance: bool) {
        // `interval` ticks immediately. Delay the first maintenance pass so
        // startup does not race teardown or provider restoration. Provider
        // restoration is bounded separately so a provider-owned trust screen
        // cannot block an unrelated conversation from starting.
        let period = SNAPSHOT_REAPER_PERIOD.min(EVENT_REPLAY_REAPER_PERIOD);
        let mut reaper = interval_at(tokio::time::Instant::now() + period, period);
        reaper.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                job = self.rx.recv() => {
                    let Some(job) = job else {
                        break;
                    };
                    let accepted_provider_input = accepted_provider_input_task_id(&job.request);
                    let accepted_shell_effect = accepted_shell_terminal_effect(&job.request);
                    let result = if is_agent_connection_query(&job.request) {
                        self.dispatch_agent_connection(job.negotiated, job.request, job.output_id).await
                    } else if is_task_create_with_primary_provider(&job.request) {
                        self.dispatch_task_create_with_primary_provider(job.negotiated, job.request, job.output_id).await
                    } else if is_provider_start_request(&job.request) {
                        self.dispatch_provider_start(job.negotiated, job.request, job.output_id).await
                    } else {
                        self.dispatch_job(job.negotiated, job.request, job.output_id, job.routing)
                    };
                    if let Some(task_id) = accepted_provider_input
                        .filter(|_| command_was_accepted(&result))
                    {
                        self.drive_provider_input_after_acceptance(task_id);
                    }
                    if let Some(effect) =
                        accepted_shell_effect.filter(|_| command_was_accepted(&result))
                    {
                        self.apply_shell_terminal_effect(effect);
                    }
                    // If the connection task went away, drop the reply; do not panic.
                    let _ = job.reply.send(result);
                }
                control = self.control_rx.recv(), if !self.control_closed => {
                    let Some(control) = control else {
                        // Do not busy-spin: stop polling a closed control channel.
                        self.control_closed = true;
                        continue;
                    };
                    self.handle_control(control);
                }
                Some(ingress) = self.semantic_ingress_rx.recv() => {
                    if let Err(error) = self.handle_provider_semantic_ingress(ingress) {
                        eprintln!("devmanager-host: provider semantic ingress rejected: {error}");
                    }
                }
                Ok(()) = self.semantic_dirty_rx.changed() => {
                    self.fan_out_conversation_dirty();
                }
                () = self.ephemeral_capacity_notify.notified() => {
                    self.fan_out_conversation_dirty();
                }
                Some(outcome) = self.provider_restore_jobs.next(), if !self.provider_restore_jobs.is_empty() => {
                    self.handle_provider_restore_outcome(outcome);
                }
                Some(outcome) = self.provider_health_jobs.next(), if !self.provider_health_jobs.is_empty() => {
                    self.handle_provider_health_outcome(outcome);
                }
                _ = reaper.tick(), if schedule_automatic_maintenance => {
                    let now = Instant::now();
                    self.registry.reap_idle(now);
                    self.replay_registry.reap_idle(now);
                    self.artifact_content_registry.reap(now);
                    self.fan_out_conversation_dirty();
                    self.reconcile_configured_services();
                    self.queue_one_provider_restore();
                    self.pump_shell_terminal_facts();
                    self.maybe_schedule_provider_health(false);
                    // Missed unregister try_send must not leave completed live
                    // metadata forever once the connection has requested shutdown.
                    self.reap_shutdown_outputs();
                    // While Open: at most one process-empty teardown per tick.
                    // While Closing: advance exactly one durable host-cleanup unit.
                    // StoreError fails closed so host supervision sees unexpected exit.
                    if self.run_one_cleanup_or_teardown_unit().is_err() {
                        break;
                    }
                }
            }
        }
    }

    async fn run_supervised(
        &mut self,
        schedule_automatic_maintenance: bool,
    ) -> Result<HostExecutorOutcome, StoreError> {
        let period = SNAPSHOT_REAPER_PERIOD.min(EVENT_REPLAY_REAPER_PERIOD);
        let mut reaper = interval_at(tokio::time::Instant::now() + period, period);
        reaper.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                job = self.rx.recv() => {
                    let Some(job) = job else {
                        return Err(StoreError::Io(
                            "supervised executor request queue closed unexpectedly".into(),
                        ));
                    };
                    let accepted_provider_input = accepted_provider_input_task_id(&job.request);
                    let accepted_shell_effect = accepted_shell_terminal_effect(&job.request);
                    let result = if is_agent_connection_query(&job.request) {
                        self.dispatch_agent_connection(job.negotiated, job.request, job.output_id).await
                    } else if is_task_create_with_primary_provider(&job.request) {
                        self.dispatch_task_create_with_primary_provider(job.negotiated, job.request, job.output_id).await
                    } else if is_provider_start_request(&job.request) {
                        self.dispatch_provider_start(job.negotiated, job.request, job.output_id).await
                    } else {
                        self.dispatch_job(job.negotiated, job.request, job.output_id, job.routing)
                    };
                    if let Some(task_id) = accepted_provider_input
                        .filter(|_| command_was_accepted(&result))
                    {
                        self.drive_provider_input_after_acceptance(task_id);
                    }
                    if let Some(effect) =
                        accepted_shell_effect.filter(|_| command_was_accepted(&result))
                    {
                        self.apply_shell_terminal_effect(effect);
                    }
                    let _ = job.reply.send(result);
                }
                control = self.control_rx.recv(), if !self.control_closed => {
                    let Some(control) = control else {
                        self.control_closed = true;
                        continue;
                    };
                    if let Some(outcome) = self.handle_control_supervised(control).await? {
                        return Ok(outcome);
                    }
                }
                Some(ingress) = self.semantic_ingress_rx.recv() => {
                    if let Err(error) = self.handle_provider_semantic_ingress(ingress) {
                        eprintln!("devmanager-host: provider semantic ingress rejected: {error}");
                    }
                }
                Ok(()) = self.semantic_dirty_rx.changed() => {
                    self.fan_out_conversation_dirty();
                }
                () = self.ephemeral_capacity_notify.notified() => {
                    self.fan_out_conversation_dirty();
                }
                Some(outcome) = self.provider_restore_jobs.next(), if !self.provider_restore_jobs.is_empty() => {
                    self.handle_provider_restore_outcome(outcome);
                }
                Some(outcome) = self.provider_health_jobs.next(), if !self.provider_health_jobs.is_empty() => {
                    self.handle_provider_health_outcome(outcome);
                }
                _ = reaper.tick(), if schedule_automatic_maintenance => {
                    let now = Instant::now();
                    self.registry.reap_idle(now);
                    self.replay_registry.reap_idle(now);
                    self.artifact_content_registry.reap(now);
                    self.fan_out_conversation_dirty();
                    self.reconcile_configured_services();
                    self.queue_one_provider_restore();
                    self.pump_shell_terminal_facts();
                    self.maybe_schedule_provider_health(false);
                    self.reap_shutdown_outputs();
                    if let Some(outcome) = self.drive_supervised_maintenance_unit().await? {
                        return Ok(outcome);
                    }
                }
            }
        }
    }

    fn handle_provider_semantic_ingress(
        &mut self,
        ingress: ProviderSemanticIngress,
    ) -> Result<(), StoreError> {
        match ingress {
            ProviderSemanticIngress::Question {
                task_id,
                question_id,
            } => self.present_provider_question(task_id, question_id),
        }
    }

    fn present_provider_question(
        &mut self,
        task_id: TaskId,
        question_id: QuestionId,
    ) -> Result<(), StoreError> {
        let snapshot = self
            .bus
            .task_snapshot(task_id)?
            .ok_or_else(|| StoreError::Projection("semantic question task is missing".into()))?;
        let agent_session_id = snapshot.primary_agent_id.ok_or_else(|| {
            StoreError::Projection("semantic question task has no primary agent".into())
        })?;
        let agent = snapshot.agents.get(&agent_session_id).ok_or_else(|| {
            StoreError::Projection("semantic question primary agent is missing".into())
        })?;
        let session = snapshot
            .provider_sessions
            .get(&agent_session_id)
            .cloned()
            .unwrap_or_default();
        if session.open_question == Some(question_id)
            || session.question_winners.contains_key(&question_id)
        {
            return Ok(());
        }
        if session.open_question.is_some() {
            return Err(StoreError::Projection(
                "a different provider question is already open".into(),
            ));
        }
        let turn_id = session.current_turn.ok_or_else(|| {
            StoreError::Projection("semantic question has no current provider turn".into())
        })?;
        let intent = PresentProviderQuestionIntent::try_new(
            agent_session_id,
            agent.runtime_generation,
            turn_id,
            snapshot.task.action_epoch,
            question_id,
        )
        .map_err(|error| StoreError::Projection(error.to_string()))?;
        let receipt = self.bus.execute_host_authorized(
            CommandEnvelope {
                command_id: crate::domain::CommandId::new(),
                client_id: crate::domain::ClientId::new(),
                task_id: Some(task_id),
                issued_at_ms: unix_time_ms_u64() as i64,
                expected_task_revision: Some(snapshot.task.revision),
                command: Command::PresentProviderQuestion(intent),
            },
            None,
            RequestId::new(),
            Uuid::nil(),
        )?;
        if !matches!(receipt, CommandReceipt::Accepted { .. }) {
            return Err(StoreError::Projection(format!(
                "semantic provider question was rejected: {receipt:?}"
            )));
        }
        self.fan_out_live_durable_events();
        Ok(())
    }

    fn handle_control(&mut self, control: ExecutorControl) {
        match control {
            ExecutorControl::RegisterOutput {
                id,
                output,
                client_id,
                reconnect_from,
                ack,
            } => {
                let replacing = reconnect_from.filter(|old| self.outputs.contains_key(old));
                if self.outputs.len() >= MAX_SNAPSHOT_SESSIONS
                    && !self.outputs.contains_key(&id)
                    && replacing.is_none()
                {
                    output.request_shutdown();
                    return;
                }
                if let (Some(client_id), Some(old_id)) = (client_id, reconnect_from) {
                    self.rebind_connection(client_id, old_id, id);
                }
                output
                    .attach_ephemeral_capacity_notify(Arc::clone(&self.ephemeral_capacity_notify));
                self.outputs.insert(id, output);
                if ack.send(()).is_err() {
                    self.detach_output(id);
                }
            }
            ExecutorControl::UnregisterOutput { id } => {
                self.detach_output(id);
            }
            ExecutorControl::AttachTerminal {
                owner,
                spec,
                runtime,
                ack,
            } => {
                let result = self
                    .terminal_service
                    .attach(owner, spec, runtime)
                    .map_err(|error| error.to_string());
                let _ = ack.send(result);
            }
            ExecutorControl::BindTerminalIdentity {
                terminal_id,
                agent_session_id,
                runtime_generation,
                action_epoch,
                ack,
            } => {
                let result = self
                    .terminal_service
                    .bind_task_identity(
                        terminal_id,
                        agent_session_id,
                        runtime_generation,
                        action_epoch,
                    )
                    .map_err(|error| error.to_string());
                let _ = ack.send(result);
            }
            ExecutorControl::InspectHostQuitForUpdate { ack } => {
                let result = self
                    .bus
                    .inspect_host_quit()
                    .map_err(|error| format!("InspectHostQuit failed: {error}"));
                let _ = ack.send(result);
            }
            ExecutorControl::PrepareUpdate {
                target_version,
                client_build,
                host_build,
                allow_explicit_confirm_with_active,
                ack,
            } => {
                let result = (|| {
                    let inspection = self
                        .bus
                        .inspect_host_quit()
                        .map_err(|error| format!("InspectHostQuit failed: {error}"))?;
                    let mapped = crate::host::update::update_inspection_from_host_quit(
                        &inspection,
                        Uuid::nil(),
                    );
                    let mut probe = crate::updater::FixedActiveResourceProbe { inspection: mapped };
                    self.update_gate
                        .prepare_update(
                            &mut probe,
                            &target_version,
                            &client_build,
                            &host_build,
                            SystemTime::now(),
                            allow_explicit_confirm_with_active,
                        )
                        .map_err(|error| error.to_string())
                })();
                let _ = ack.send(result);
            }
            ExecutorControl::ConfirmUpdateDrain { token_id, ack } => {
                let result = self
                    .update_gate
                    .confirm_drain(token_id, SystemTime::now())
                    .map_err(|error| error.to_string());
                let _ = ack.send(result);
            }
            ExecutorControl::AbortUpdateHandoff { ack } => {
                let result = self
                    .update_gate
                    .abort_pre_install()
                    .map(|_| ())
                    .map_err(|error| error.to_string());
                let _ = ack.send(result);
            }
            ExecutorControl::ArmUpdateInstall { token_id, ack } => {
                let result = self
                    .update_gate
                    .begin_atomic_install(token_id, SystemTime::now())
                    .map_err(|error| error.to_string());
                let _ = ack.send(result);
            }
            ExecutorControl::SealUpdateAfterDurableStage { ack } => {
                let result = self
                    .update_gate
                    .seal_after_durable_stage()
                    .map_err(|error| error.to_string());
                let _ = ack.send(result);
            }
            ExecutorControl::SetBrowserGatewayRegistrar { registrar, ack } => {
                if let Some(runtime) = self.configured_service_runtime.as_ref() {
                    runtime.manager.set_browser_gateway_registrar(registrar);
                }
                let _ = ack.send(());
            }
            #[cfg(test)]
            ExecutorControl::InspectOutput { id, ack } => {
                let registered = self.outputs.contains_key(&id);
                let live_bound = self
                    .replay_registry
                    .entries
                    .values()
                    .any(|entry| entry.live.as_ref().is_some_and(|live| live.output_id == id));
                let _ = ack.send(OutputInspection {
                    registered,
                    live_bound,
                });
            }
            #[cfg(test)]
            ExecutorControl::RunMaintenanceOnce { ack } => {
                let result = self.run_one_cleanup_or_teardown_unit();
                let _ = ack.send(result);
            }
            #[cfg(test)]
            ExecutorControl::TakePendingQuitReceiptAck { id, ack } => {
                let taken = self
                    .pending_quit_receipt_acks
                    .remove(&id)
                    .map(|pending| (pending.operation_id, pending.ack));
                let _ = ack.send(taken);
            }
            #[cfg(test)]
            ExecutorControl::InstallTestSemanticJournal { journal, ack } => {
                self.test_semantic_journal = Some(journal);
                let _ = ack.send(());
            }
            #[cfg(test)]
            ExecutorControl::RecordTestSemantic { draft, ack } => {
                let sequence = if let Some(journal) = self.test_semantic_journal.as_ref() {
                    let mut store = journal.lock().expect("test semantic journal");
                    let recorded = store.record(draft);
                    self.semantic_dirty
                        .mark(recorded.stable_session_key.clone(), recorded.sequence);
                    recorded.sequence
                } else if let Some(runtime) = self.configured_service_runtime.as_ref() {
                    let mut store = runtime.semantic_journal.lock().expect("semantic journal");
                    let recorded = store.record(draft);
                    self.semantic_dirty
                        .mark(recorded.stable_session_key.clone(), recorded.sequence);
                    recorded.sequence
                } else {
                    0
                };
                let _ = ack.send(sequence);
            }
        }
    }

    /// A fresh durable provider input is an explicit retry signal. Attempt one
    /// bounded outbox pass immediately; if the exact provider runtime is absent,
    /// schedule its persisted restart now instead of waiting for the periodic
    /// maintenance tick. Discovery and process launch remain on the
    /// cancellation-owned restore-future lane.
    fn drive_provider_input_after_acceptance(&mut self, task_id: TaskId) {
        self.provider_restore_failed_action_epochs.remove(&task_id);
        self.provider_restart_unavailable.remove(&task_id);
        self.provider_dispatch_maintenance_due =
            Instant::now() + PROVIDER_DISPATCH_MAINTENANCE_PERIOD;
        let held_restarts = if let Some(runtime) = self.configured_service_runtime.as_mut() {
            match run_provider_dispatch_pass(&mut self.bus, &runtime.provider_dispatch) {
                Ok(held) => held,
                Err(error) => {
                    eprintln!("devmanager-host: immediate provider dispatch failed: {error}");
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };
        for (held_task_id, reason) in held_restarts {
            // The launch that just completed already owns this task's exact
            // runtime. SessionStart and the terminal attachment arrive
            // asynchronously, so dispatch can briefly report SessionNotBound
            // or RuntimeAuthorityAbsent here. Scheduling another start in that
            // window races the live generation and permanently records a false
            // restore failure. Other held tasks still need their own restart.
            if held_task_id != task_id {
                self.queue_held_provider_restart(held_task_id, reason);
            }
        }
        self.queue_one_provider_restore();
        self.fan_out_live_durable_events();
    }

    /// Pump the host-owned configured supervisor on the maintenance lane. The
    /// service panel and action path therefore observe reconciled process/port
    /// evidence without doing probes or process work in request dispatch.
    fn reconcile_configured_services(&mut self) {
        // Capture dispatch/flush/failure results under the runtime borrow, then
        // release it before mutating restore queues owned by `self`. Disjoint
        // field borrows (`bus` + `configured_service_runtime`) keep this compiling.
        let dispatch_due = Instant::now() >= self.provider_dispatch_maintenance_due;
        if dispatch_due {
            self.provider_dispatch_maintenance_due =
                Instant::now() + PROVIDER_DISPATCH_MAINTENANCE_PERIOD;
        }
        let (held_restarts, provider_failures) = if let Some(runtime) =
            self.configured_service_runtime.as_mut()
        {
            let bus = &mut self.bus;
            runtime.manager.reconcile_provider_terminal_exits_now();
            let held_restarts = if dispatch_due {
                match run_provider_dispatch_pass(bus, &runtime.provider_dispatch) {
                    Ok(held) => held,
                    Err(error) => {
                        eprintln!("devmanager-host: provider dispatch maintenance failed: {error}");
                        Vec::new()
                    }
                }
            } else {
                Vec::new()
            };
            if let Ok(mut journal) = runtime.semantic_journal.lock() {
                if let Err(error) = journal.flush_if_dirty() {
                    eprintln!("devmanager-host: conversation history flush failed: {error}");
                }
            }
            let _ = runtime.manager.configured_service_snapshots();
            let failures = runtime.manager.drain_provider_session_failures();
            (held_restarts, failures)
        } else {
            (Vec::new(), Vec::new())
        };
        for (task_id, reason) in held_restarts {
            self.queue_held_provider_restart(task_id, reason);
        }
        for failure in provider_failures {
            eprintln!(
                "devmanager-host: provider session failed task={} agent={} provider={:?}: {:?}",
                failure.task_id, failure.agent_session_id, failure.provider_kind, failure.failure,
            );
            if let Ok(Some(snapshot)) = self.bus.task_snapshot(failure.task_id) {
                self.provider_restore_failed_action_epochs
                    .insert(failure.task_id, snapshot.task.action_epoch);
            } else {
                self.provider_restore_failed_action_epochs
                    .insert(failure.task_id, 0);
            }
            self.mark_provider_restore_failed(failure.task_id);
        }
        if let Err(error) = self.sync_provider_session_identities() {
            eprintln!("devmanager-host: provider session identity sync failed: {error}");
        }
    }

    fn queue_held_provider_restart(
        &mut self,
        task_id: TaskId,
        reason: crate::providers::dispatch::ProviderDispatchHoldReason,
    ) {
        if self.provider_restore_in_flight.contains(&task_id)
            || self
                .provider_restore_queue
                .iter()
                .any(|intent| intent.task_id == task_id)
        {
            return;
        }
        if self.provider_restart_unavailable.contains(&task_id) {
            return;
        }
        match self.bus.provider_hold_restart_intent(task_id) {
            Ok(Some(mut intent)) => {
                self.provider_restart_unavailable.remove(&task_id);
                if self
                    .provider_restore_failed_action_epochs
                    .get(&task_id)
                    .is_some_and(|failed_epoch| *failed_epoch == intent.expected_action_epoch)
                {
                    return;
                }
                restore_admitted_provider_launch_options(
                    &mut intent,
                    &self.provider_launch_options,
                );
                eprintln!(
                    "devmanager-host: provider input held task={} reason={reason:?}; scheduling {:?}",
                    task_id, intent.mode
                );
                push_unique_provider_restore_intent(
                    &mut self.provider_restore_queue,
                    &self.provider_restore_in_flight,
                    intent,
                );
            }
            Ok(None) => {
                if self.provider_restart_unavailable.insert(task_id) {
                    eprintln!(
                        "devmanager-host: provider input held task={task_id} reason={reason:?}; no restart facts"
                    );
                }
            }
            Err(error) => {
                eprintln!(
                    "devmanager-host: provider hold restart selection failed task={task_id}: {error}"
                );
            }
        }
    }

    fn queue_one_provider_restore(&mut self) {
        if self.provider_restore_pending {
            match self.bus.restorable_provider_starts(64) {
                Ok(starts) => {
                    // Only clear pending after successful enumeration. Failed
                    // or not-yet-run enumeration keeps Unknown on readiness.
                    self.provider_restore_pending = false;
                    for intent in starts {
                        push_unique_provider_restore_intent(
                            &mut self.provider_restore_queue,
                            &self.provider_restore_in_flight,
                            intent,
                        );
                    }
                }
                Err(error) => {
                    eprintln!("devmanager-host: provider restore enumeration failed: {error}");
                    return;
                }
            }
        }
        if self.provider_restore_jobs.len() >= MAX_CONCURRENT_PROVIDER_RESTORES {
            return;
        }
        let Some(intent) = self.provider_restore_queue.pop_front() else {
            return;
        };
        let Some(runtime) = self.configured_service_runtime.as_ref() else {
            self.handle_provider_restore_outcome(ProviderRestoreOutcome {
                task_id: intent.task_id,
                action_epoch: intent.expected_action_epoch,
                result: Err("configured provider runtime is unavailable".to_string()),
            });
            return;
        };
        let manager = runtime.manager.clone();
        let prepared = (|| {
            let (binding, agent, _) = self
                .bus
                .prepare_provider_start(&intent)
                .map_err(|error| error.to_string())?;
            let loaded = self
                .bus
                .load_task_runtime(intent.task_id, &self.workspace_projects)
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "provider restore task runtime is missing".to_string())?;
            let cwd = loaded
                .workspace
                .runtime_working_directory()
                .map_err(|error| error.to_string())?;
            let mode = match intent.mode {
                crate::domain::command::ProviderStartMode::Open => {
                    crate::providers::session::ProviderSessionStartMode::Open
                }
                crate::domain::command::ProviderStartMode::NewConversation => {
                    crate::providers::session::ProviderSessionStartMode::NewConversation
                }
                crate::domain::command::ProviderStartMode::ResumeExact => {
                    crate::providers::session::ProviderSessionStartMode::ResumeExact
                }
            };
            let launch_options = intent.launch_options.clone();
            Ok::<_, String>((binding, agent, cwd, mode, launch_options))
        })();
        let (binding, agent, cwd, mode, launch_options) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                self.handle_provider_restore_outcome(ProviderRestoreOutcome {
                    task_id: intent.task_id,
                    action_epoch: intent.expected_action_epoch,
                    result: Err(error),
                });
                return;
            }
        };
        let task_id = intent.task_id;
        let action_epoch = intent.expected_action_epoch;
        let provider_kind = intent.provider_kind;
        let legacy_launch = match manager.peek_persisted_provider_launch_spec(agent.id) {
            Ok(launch) => launch,
            Err(error) => {
                self.handle_provider_restore_outcome(ProviderRestoreOutcome {
                    task_id,
                    action_epoch,
                    result: Err(format!("persisted launch peek failed: {error}")),
                });
                return;
            }
        };
        self.provider_restore_in_flight.insert(task_id);
        let Some(owner) = self
            .provider_settings
            .as_ref()
            .map(|authority| std::sync::Arc::clone(authority.profile()))
        else {
            self.handle_provider_restore_outcome(ProviderRestoreOutcome {
                task_id,
                action_epoch,
                result: Err("provider settings profile unavailable".into()),
            });
            return;
        };
        self.provider_restore_jobs.push(Box::pin(async move {
            let result = async {
                let resolved = super::provider_launch::resolve_host_provider_launch(
                    owner.as_ref(),
                    task_id,
                    provider_kind,
                    launch_options,
                    provider_start_requires_existing_binding(intent.mode),
                    legacy_launch.as_ref(),
                )?;
                let observation = super::agent_connection::observe_with_trusted_auth(
                    manager.provider_host().registry(),
                    provider_kind,
                    &resolved.discovery,
                )
                .await
                .map_err(|error| error.to_string())?;
                let environment = resolved.environment;
                let launch_options = resolved.launch_options;
                let deferred_binding = resolved.deferred_binding;
                tokio::task::spawn_blocking(move || {
                    manager
                        .start_production_stock_provider_session_with_options(
                            binding,
                            agent,
                            &observation,
                            None,
                            cwd,
                            environment,
                            mode,
                            launch_options,
                        )
                        .map_err(|error| error.to_string())?;
                    if let Some(deferred) = deferred_binding.as_ref() {
                        super::provider_launch::commit_deferred_provider_binding(
                            owner.as_ref(),
                            deferred,
                        )?;
                    }
                    Ok::<(), String>(())
                })
                .await
                .map_err(|error| format!("provider restore launch worker failed: {error}"))??;
                Ok(())
            }
            .await;
            ProviderRestoreOutcome {
                task_id,
                action_epoch,
                result,
            }
        }));
    }

    fn handle_provider_health_outcome(
        &mut self,
        outcome: super::provider_health::ProviderHealthJobOutcome,
    ) {
        // Finish ownership lives inside the future DropGuard. Stale completions
        // are ignored here; only surface unexpected probe-lane failures.
        if let Some(error) = outcome.error.as_ref() {
            if let Some(authority) = self.provider_settings.as_ref() {
                if authority
                    .health_job()
                    .is_stale_generation(outcome.generation)
                    || authority
                        .health_job()
                        .is_stale_config(outcome.config_revision)
                {
                    return;
                }
            }
            eprintln!("devmanager-host: provider health refresh failed: {error}");
        }
    }

    fn maybe_schedule_provider_health(&mut self, force: bool) {
        let Some(authority) = self.provider_settings.clone() else {
            return;
        };
        let manager = self
            .configured_service_runtime
            .as_ref()
            .map(|runtime| runtime.manager.clone());
        if let Some((_, future)) =
            super::provider_health::try_begin_health_job(&authority, manager, force)
        {
            self.provider_health_jobs.push(future);
        }
    }

    fn dispatch_provider_settings_query(
        &mut self,
        request: &crate::providers::settings::ProviderSettingsHostRequest,
    ) -> QueryOutcome {
        use crate::providers::settings::{
            ProviderSettingsHostRequest, ProviderSettingsQuery, ProviderSettingsReply,
        };
        let Some(authority) = self.provider_settings.clone() else {
            return QueryOutcome::Err(QueryError::Unavailable {
                reason: "provider_settings",
            });
        };
        let reply = match request {
            ProviderSettingsHostRequest::Snapshot => {
                match authority.query(ProviderSettingsQuery::Snapshot) {
                    Ok(reply) => reply,
                    Err(error) => ProviderSettingsReply::Error {
                        message: error.to_string(),
                    },
                }
            }
            ProviderSettingsHostRequest::Refresh { force } => {
                let manager = self
                    .configured_service_runtime
                    .as_ref()
                    .map(|runtime| runtime.manager.clone());
                match super::provider_health::try_begin_health_job(&authority, manager, *force) {
                    Some((generation, future)) => {
                        self.provider_health_jobs.push(future);
                        ProviderSettingsReply::RefreshStarted { generation }
                    }
                    None => ProviderSettingsReply::RefreshBusy,
                }
            }
            ProviderSettingsHostRequest::Mutate(mutation) => {
                match authority.mutate(mutation.clone()) {
                    Ok(reply) => reply,
                    Err(error) => ProviderSettingsReply::Error {
                        message: error.to_string(),
                    },
                }
            }
        };
        QueryOutcome::Ok(QueryResult::TaskCockpit(
            TaskCockpitResult::ProviderSettings(reply),
        ))
    }

    fn handle_provider_restore_outcome(&mut self, outcome: ProviderRestoreOutcome) {
        self.provider_restore_in_flight.remove(&outcome.task_id);
        if let Err(error) = outcome.result {
            eprintln!(
                "devmanager-host: exact provider restore failed task={}: {error}",
                outcome.task_id
            );
            self.provider_restore_failed_action_epochs
                .insert(outcome.task_id, outcome.action_epoch);
            self.mark_provider_restore_failed(outcome.task_id);
        } else {
            self.provider_restore_failed_action_epochs
                .remove(&outcome.task_id);
            if let Err(error) = self.attach_provider_terminal(outcome.task_id) {
                eprintln!(
                    "devmanager-host: provider terminal attachment failed task={}: {error}",
                    outcome.task_id
                );
            }
            let task_id = outcome.task_id;
            self.drive_provider_input_after_acceptance(task_id);
        }
    }

    fn attach_provider_terminal(&mut self, task_id: TaskId) -> Result<(), String> {
        let runtime = self
            .configured_service_runtime
            .as_ref()
            .ok_or_else(|| "configured provider runtime is unavailable".to_string())?;
        let snapshot = self
            .bus
            .task_snapshot(task_id)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "provider task projection is unavailable".to_string())?;
        let primary_agent_id = snapshot
            .primary_agent_id
            .ok_or_else(|| "provider task has no primary agent".to_string())?;
        let agent = snapshot
            .agents
            .get(&primary_agent_id)
            .ok_or_else(|| "provider terminal agent projection is unavailable".to_string())?;
        // Shared rule: plain shells are Active Terminal resources registered at
        // the task's runtime generation too, so the older "exactly one Active
        // Terminal" filter here counted them and refused a healthy provider on
        // any task with an open shell.
        let resource = crate::domain::agent_resource::provider_terminal_resource(&snapshot, agent)
            .map_err(|error| format!("provider task terminal resource is ambiguous: {error}"))?
            .ok_or_else(|| {
                "provider task does not have one exact active terminal resource".to_string()
            })?;
        let binding = runtime
            .manager
            .provider_terminal_binding_diagnostic(
                task_id,
                primary_agent_id,
                resource.id,
                agent.runtime_generation,
            )
            .map_err(|error| {
                format!("provider runtime did not publish its terminal binding: {error}")
            })?;
        if binding.task_id != task_id
            || binding.agent_session_id != primary_agent_id
            || binding.resource_id != resource.id
            || agent.runtime_generation != binding.runtime_generation
            || resource.runtime_generation != binding.runtime_generation
            || binding.action_epoch == 0
        {
            return Err("provider terminal binding conflicts with durable task facts".to_string());
        }
        if let Some(existing) = self
            .terminal_service
            .task_terminal_view(task_id)
            .map_err(|error| error.to_string())?
        {
            return if existing.agent_session_id == Some(binding.agent_session_id)
                && existing.resource_id == binding.resource_id
                && existing.runtime_generation == Some(binding.runtime_generation)
                && existing.action_epoch == Some(binding.action_epoch)
            {
                Ok(())
            } else {
                Err("provider terminal already has a conflicting attachment".to_string())
            };
        }
        let view = binding
            .runtime
            .session_view()
            .ok_or_else(|| "provider terminal session view is unavailable".to_string())?;
        let size = crate::terminal::protocol::TerminalSize::new(
            view.runtime.dimensions.cols,
            view.runtime.dimensions.rows,
        )
        .map_err(|error| error.to_string())?;
        let mut spec = TerminalSpec::new(binding.terminal_session_id, size)
            .map_err(|error| error.to_string())?;
        spec.title = view.runtime.title;
        let attached: Arc<dyn AttachedTerminalRuntime> = binding.runtime;
        self.terminal_service
            .attach_bound_task_runtime(
                task_id,
                spec,
                attached,
                binding.agent_session_id,
                binding.runtime_generation,
                binding.action_epoch,
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    fn mark_provider_restore_succeeded(&mut self, task_id: TaskId) {
        self.provider_restart_unavailable.remove(&task_id);
        let Some(snapshot) = self.bus.task_snapshot(task_id).ok().flatten() else {
            return;
        };
        let Some(attention) = provider_restore_success_attention(snapshot.attention) else {
            return;
        };
        let _ = self.bus.execute_host_authorized(
            CommandEnvelope {
                command_id: crate::domain::CommandId::new(),
                client_id: crate::domain::ClientId::new(),
                task_id: Some(task_id),
                issued_at_ms: unix_time_ms_u64() as i64,
                expected_task_revision: Some(snapshot.task.revision),
                command: Command::SetTaskAttention(SetTaskAttentionIntent { attention }),
            },
            None,
            RequestId::new(),
            Uuid::nil(),
        );
        self.fan_out_live_durable_events();
    }

    fn mark_provider_restore_failed(&mut self, task_id: TaskId) {
        let Some(snapshot) = self.bus.task_snapshot(task_id).ok().flatten() else {
            return;
        };
        let Some(attention) = next_attention_after_provider_restore_failure(&snapshot) else {
            return;
        };
        let _ = self.bus.execute_host_authorized(
            CommandEnvelope {
                command_id: crate::domain::CommandId::new(),
                client_id: crate::domain::ClientId::new(),
                task_id: Some(task_id),
                issued_at_ms: unix_time_ms_u64() as i64,
                expected_task_revision: Some(snapshot.task.revision),
                command: Command::SetTaskAttention(SetTaskAttentionIntent { attention }),
            },
            None,
            RequestId::new(),
            Uuid::nil(),
        );
        self.fan_out_live_durable_events();
    }

    fn apply_shell_terminal_effect(&mut self, effect: AcceptedShellEffect) {
        match effect {
            AcceptedShellEffect::Open {
                task_id,
                resource_id,
            } => self.open_shell_terminal_after_accept(task_id, resource_id),
            AcceptedShellEffect::Close { resource_id } => self.close_shell_terminal(resource_id),
        }
    }

    /// Serve one host-authority `terminal.open_shell` request.
    ///
    /// The client names a Task, an optional working directory, and the exact
    /// revision it saw. Everything else — the resolved directory, the shell
    /// executable, its arguments, and the runtime generation — is the host's
    /// to choose, which is why `Command::OpenShellTerminal` is refused on the
    /// client `execute` path entirely.
    ///
    /// A named refusal writes one sentence to both the host log and the reply's
    /// `detail`: the wire reason is a closed enum, so "that directory does not
    /// exist", "that path is relative" and "no shell is installed" would
    /// otherwise be indistinguishable on the client's side.
    ///
    /// `Ok(())` means the durable resource was registered and the process
    /// effect has been started; the caller answers with the Task's strip.
    fn open_shell_terminal_request(
        &mut self,
        client_id: ClientId,
        task_id: Option<TaskId>,
        cwd: Option<&str>,
        expected_task_revision: u64,
        request_id: RequestId,
        connection_id: Uuid,
    ) -> Result<(), QueryOutcome> {
        let Some(task_id) = task_id else {
            return Err(shell_open_denied(
                crate::domain::TaskCockpitDeniedReason::MissingTask,
            ));
        };
        let snapshot = match self.bus.task_snapshot(task_id) {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => {
                return Err(shell_open_denied(
                    crate::domain::TaskCockpitDeniedReason::MissingTask,
                ))
            }
            Err(error) => {
                eprintln!("devmanager-host: open shell snapshot failed task={task_id}: {error}");
                return Err(QueryOutcome::Err(QueryError::Unavailable {
                    reason: "task_lookup",
                }));
            }
        };
        let cwd = self.resolve_shell_terminal_cwd(task_id, cwd)?;
        let (default_terminal, mac_profile, shell_integration_enabled) = self.terminal_settings();
        let launch = match crate::terminal::session::resolve_plain_shell_launch(
            Some(&default_terminal),
            mac_profile.as_ref(),
            shell_integration_enabled,
            &cwd,
        ) {
            Ok(launch) => launch,
            Err(reason) => {
                return Err(shell_open_unavailable_named(
                    task_id,
                    crate::domain::TaskCockpitUnavailableReason::TerminalUnavailable,
                    reason,
                ));
            }
        };
        // The shell rides the Task's live runtime generation so teardown fences
        // it exactly like the provider terminal. A Task that has not started a
        // provider yet has no agent to read one from; one is the first valid
        // generation, and zero is refused outright downstream.
        let runtime_generation = snapshot
            .primary_agent_id
            .and_then(|agent_id| snapshot.agents.get(&agent_id))
            .map(|agent| agent.runtime_generation)
            .filter(|generation| *generation > 0)
            .unwrap_or(1);
        // One clock read for one operation. The resource's `created_at` and its
        // command's `issued_at_ms` describe the same instant; two reads let the
        // durable record disagree with the envelope that carried it.
        let now_ms = unix_time_ms_u64() as i64;
        let mut resource = match crate::domain::resource::ResourceFacts::new(
            Some(task_id),
            crate::domain::resource::OwnerKind::Task,
            crate::domain::resource::ResourceKind::Terminal,
            crate::domain::resource::ResourceRecipe::Terminal {
                cols: 120,
                rows: 40,
                launch: Some(crate::domain::resource::TerminalLaunch {
                    cwd: cwd.clone(),
                    program: launch.program.clone(),
                    args: launch.args.clone(),
                }),
                title: None,
            },
            now_ms,
        ) {
            Ok(resource) => resource,
            Err(error) => {
                eprintln!(
                    "devmanager-host: open shell recipe rejected task={task_id} program={} cwd={}: {error}",
                    launch.program.display(),
                    cwd.display()
                );
                return Err(shell_open_unavailable(
                    crate::domain::TaskCockpitUnavailableReason::TerminalUnavailable,
                ));
            }
        };
        resource.runtime_generation = runtime_generation;
        let resource_id = resource.id;
        let envelope = CommandEnvelope {
            command_id: crate::domain::CommandId::new(),
            client_id,
            task_id: Some(task_id),
            issued_at_ms: now_ms,
            expected_task_revision: Some(expected_task_revision),
            command: Command::OpenShellTerminal(crate::domain::command::OpenShellTerminalIntent {
                resource,
            }),
        };
        match self
            .bus
            .execute_host_authorized(envelope, None, request_id, connection_id)
        {
            Ok(CommandReceipt::Accepted { .. }) => {}
            Ok(CommandReceipt::Rejected { code, .. }) => {
                eprintln!(
                    "devmanager-host: open shell rejected task={task_id} revision={expected_task_revision}: {code:?}"
                );
                return Err(shell_open_denied(match code {
                    crate::domain::command::RejectionCode::RevisionConflict => {
                        crate::domain::TaskCockpitDeniedReason::RevisionConflict
                    }
                    crate::domain::command::RejectionCode::OwnershipConflict => {
                        crate::domain::TaskCockpitDeniedReason::Unauthorized
                    }
                    _ => crate::domain::TaskCockpitDeniedReason::StaleFence,
                }));
            }
            Err(error) => {
                eprintln!("devmanager-host: open shell execute failed task={task_id}: {error}");
                return Err(QueryOutcome::Err(QueryError::Unavailable {
                    reason: "open_shell_execute",
                }));
            }
        }
        // A host-issued command has no client request behind it, so nothing
        // else publishes its events; every other host-authorized write in this
        // executor fans out for the same reason.
        self.fan_out_live_durable_events();
        // Registration and the real process are two steps, and this is the only
        // path that takes the second one. `Command::OpenShellTerminal` is
        // refused on both wire lanes, so the accepted-request effect lane never
        // sees an open; a host-issued command has to start the process itself.
        self.open_shell_terminal_after_accept(task_id, resource_id);
        Ok(())
    }

    /// Resolve the working directory one shell will start in.
    ///
    /// A client-supplied directory must be absolute and must exist right now:
    /// a relative path would resolve against the host process's directory,
    /// which is not the client's, and a missing one produces a shell that dies
    /// on spawn with nothing to attribute it to.
    fn resolve_shell_terminal_cwd(
        &self,
        task_id: TaskId,
        cwd: Option<&str>,
    ) -> Result<PathBuf, QueryOutcome> {
        if let Some(requested) = cwd {
            let path = PathBuf::from(requested);
            // Two different mistakes, two different fixes: a relative path is
            // the caller sending the wrong thing, a missing directory is the
            // world having changed. One shared message made them one bug report.
            if !path.is_absolute() {
                return Err(shell_open_denied_named(
                    task_id,
                    crate::domain::TaskCockpitDeniedReason::OutsideWorkspace,
                    format!("cwd must be absolute: {}", path.display()),
                ));
            }
            if !path.is_dir() {
                return Err(shell_open_denied_named(
                    task_id,
                    crate::domain::TaskCockpitDeniedReason::OutsideWorkspace,
                    format!("cwd is not a directory: {}", path.display()),
                ));
            }
            return Ok(path);
        }
        let runtime = match self
            .bus
            .load_task_runtime(task_id, &self.workspace_projects)
        {
            Ok(Some(runtime)) => runtime,
            Ok(None) => {
                return Err(shell_open_denied(
                    crate::domain::TaskCockpitDeniedReason::MissingTask,
                ))
            }
            Err(error) => {
                eprintln!(
                    "devmanager-host: open shell workspace unavailable task={task_id}: {error}"
                );
                return Err(shell_open_unavailable(
                    crate::domain::TaskCockpitUnavailableReason::WorkspaceAuthorityUnavailable,
                ));
            }
        };
        match runtime.workspace.runtime_working_directory() {
            Ok(path) if path.is_dir() => Ok(path),
            Ok(path) => Err(shell_open_denied_named(
                task_id,
                crate::domain::TaskCockpitDeniedReason::OutsideWorkspace,
                format!("cwd is not a directory: {}", path.display()),
            )),
            Err(error) => {
                eprintln!(
                    "devmanager-host: open shell working directory unavailable task={task_id}: {error}"
                );
                Err(shell_open_unavailable(
                    crate::domain::TaskCockpitUnavailableReason::WorkspaceAuthorityUnavailable,
                ))
            }
        }
    }

    /// The three terminal settings a plain shell needs, defaulted when this
    /// host has no admitted config yet.
    ///
    /// The durable config and the terminal layer carry two structurally
    /// identical copies of these enums (`config::model` and `models::config`);
    /// the one conversion between them lives in `crate::persistence` and is
    /// shared with `strict_settings_to_legacy`.
    fn terminal_settings(
        &self,
    ) -> (
        crate::models::DefaultTerminal,
        Option<crate::models::MacTerminalProfile>,
        bool,
    ) {
        let Some(admission) = self.config_admission.as_ref() else {
            return (crate::models::DefaultTerminal::default(), None, false);
        };
        let snapshot = admission.store.snapshot();
        let settings = snapshot.config.settings();
        (
            settings.default_terminal.clone().into(),
            settings
                .mac_terminal_profile
                .clone()
                .into_option()
                .map(crate::models::MacTerminalProfile::from),
            settings.shell_integration_enabled,
        )
    }

    /// Start the host process effect for one accepted `OpenShellTerminal`.
    ///
    /// Every refusal records an exit fact rather than returning quietly: to a
    /// client, a shell that never opened and a shell that opened and died look
    /// identical, and only the summary can tell them apart.
    fn open_shell_terminal_after_accept(&mut self, task_id: TaskId, resource_id: ResourceId) {
        let Some(manager) = self
            .configured_service_runtime
            .as_ref()
            .map(|runtime| runtime.manager.clone())
        else {
            self.refuse_shell_terminal(
                task_id,
                resource_id,
                "host has no configured service runtime to spawn a shell in".to_string(),
            );
            return;
        };
        let snapshot = match self.bus.task_snapshot(task_id) {
            Ok(Some(snapshot)) => snapshot,
            // No task means nothing to attribute a refusal to.
            Ok(None) => return,
            Err(error) => {
                self.refuse_shell_terminal(
                    task_id,
                    resource_id,
                    format!("task snapshot unavailable: {error}"),
                );
                return;
            }
        };
        // The command that just registered this resource was accepted, so an
        // absent resource means it was released again underneath us. There is
        // no terminal left to carry a fact, hence log-only.
        let Some(resource) = snapshot.resources.get(&resource_id) else {
            eprintln!(
                "devmanager-host: shell terminal {resource_id} was released before it could spawn"
            );
            return;
        };
        let ResourceRecipe::Terminal {
            cols,
            rows,
            launch: Some(launch),
            ..
        } = &resource.recipe
        else {
            self.refuse_shell_terminal(
                task_id,
                resource_id,
                "resource is not a plain shell terminal".to_string(),
            );
            return;
        };
        let (cols, rows, launch) = (*cols, *rows, launch.clone());
        let runtime_generation = resource.runtime_generation;
        if runtime_generation == 0 {
            self.refuse_shell_terminal(
                task_id,
                resource_id,
                "registered without a runtime generation".to_string(),
            );
            return;
        }
        // The launch authority fences teardown on the task's action epoch, and
        // a task that has taken no durable action yet is still at zero, which
        // the issuer refuses outright.
        let action_epoch = snapshot.task.action_epoch.max(1);
        let dimensions = SessionDimensions {
            cols,
            rows,
            cell_width: 8,
            cell_height: 16,
        };

        let session_id = match manager.spawn_task_shell_session(
            task_id,
            resource_id,
            runtime_generation,
            action_epoch,
            &launch,
            dimensions,
        ) {
            Ok(session_id) => session_id,
            Err(error) => {
                self.refuse_shell_terminal(task_id, resource_id, format!("spawn failed: {error}"));
                return;
            }
        };

        let abandon = |executor: &mut Self, reason: String| {
            let _ = manager.close_task_shell_session(session_id);
            executor.refuse_shell_terminal(task_id, resource_id, reason);
        };

        let attached = match manager.task_shell_runtime(session_id) {
            Ok(session) => session,
            Err(error) => {
                abandon(self, format!("shell runtime unavailable: {error}"));
                return;
            }
        };
        let size = match crate::terminal::protocol::TerminalSize::new(cols, rows) {
            Ok(size) => size,
            Err(error) => {
                abandon(self, format!("shell size rejected: {error}"));
                return;
            }
        };
        let spec = match TerminalSpec::new(session_id, size) {
            Ok(spec) => spec,
            Err(error) => {
                abandon(self, format!("shell spec rejected: {error}"));
                return;
            }
        };
        let attached: Arc<dyn AttachedTerminalRuntime> = attached;
        // `attach_plain_shell` verifies the runtime's own attachment fence
        // rather than installing one, so this can only succeed because the
        // spawn above issued its authority for the same resource/generation.
        if let Err(error) = self.terminal_service.attach_plain_shell(
            task_id,
            resource_id,
            runtime_generation,
            spec,
            attached,
        ) {
            abandon(self, format!("shell attach failed: {error}"));
            return;
        }

        self.shell_sessions.insert(
            resource_id,
            ShellSessionLink {
                task_id,
                session_id,
                last_cwd: None,
                last_cwd_change: None,
                last_activity_offer: None,
                last_activity_sequence: 0,
                exit_recorded: false,
            },
        );
    }

    /// Refuse one plain shell and say why, in both channels.
    ///
    /// A registered terminal whose shell never started is indistinguishable
    /// from one that started and died unless the reason is written down: the
    /// exit fact is what a client can see, and the log line is what survives a
    /// store that is itself refusing writes.
    fn refuse_shell_terminal(&mut self, task_id: TaskId, resource_id: ResourceId, reason: String) {
        eprintln!("devmanager-host: shell terminal {resource_id} refused: {reason}");
        self.record_shell_exit(
            task_id,
            resource_id,
            None,
            format!("spawn refused: {reason}"),
        );
    }

    fn record_shell_exit(
        &mut self,
        task_id: TaskId,
        resource_id: ResourceId,
        code: Option<i32>,
        summary: String,
    ) {
        let _ = self.record_shell_fact(
            task_id,
            resource_id,
            HostTerminalFact::Exit { code, summary },
        );
    }

    /// Write one host observation as a durable terminal fact.
    ///
    /// `Suppressed` is the ordinary answer for a repeated observation and stays
    /// silent -- except for a cwd refused for not being absolute, which means
    /// the sampler itself is wrong and would otherwise go on being wrong
    /// forever without a word. `UnknownTerminal` means the durable resource was
    /// released under us: stop sampling it and close the shell behind it.
    fn record_shell_fact(
        &mut self,
        task_id: TaskId,
        resource_id: ResourceId,
        fact: HostTerminalFact,
    ) -> Option<TerminalFactOutcome> {
        let refused_cwd = match &fact {
            HostTerminalFact::Cwd(cwd) if !cwd.is_absolute() => Some(cwd.clone()),
            _ => None,
        };
        match self
            .bus
            .record_terminal_fact(task_id, resource_id, fact, unix_time_ms_u64() as i64)
        {
            Ok(TerminalFactOutcome::Recorded) => {
                self.fan_out_live_durable_events();
                Some(TerminalFactOutcome::Recorded)
            }
            Ok(TerminalFactOutcome::Suppressed) => {
                if let Some(cwd) = refused_cwd {
                    eprintln!(
                        "devmanager-host: terminal {resource_id} cwd sample `{}` is not absolute and can never become a fact",
                        cwd.display()
                    );
                }
                Some(TerminalFactOutcome::Suppressed)
            }
            Ok(TerminalFactOutcome::UnknownTerminal) => {
                self.close_shell_terminal(resource_id);
                Some(TerminalFactOutcome::UnknownTerminal)
            }
            Err(error) => {
                eprintln!("devmanager-host: terminal {resource_id} fact was not written: {error}");
                None
            }
        }
    }

    /// One sampling pass over every live plain shell, at the reaper cadence.
    ///
    /// A shell whose exit is already recorded is skipped entirely: it has no
    /// further cwd to report, and an activity fact for a dead shell would read
    /// as a live one. The link stays so an explicit close can still reach the
    /// manager session.
    fn pump_shell_terminal_facts(&mut self) {
        if self.shell_sessions.is_empty() {
            return;
        }
        self.close_orphaned_shell_terminals();
        if self.shell_sessions.is_empty() {
            return;
        }
        let Some(manager) = self
            .configured_service_runtime
            .as_ref()
            .map(|runtime| runtime.manager.clone())
        else {
            return;
        };
        let now = Instant::now();
        let resource_ids = self.shell_sessions.keys().copied().collect::<Vec<_>>();
        for resource_id in resource_ids {
            let Some(link) = self.shell_sessions.get(&resource_id) else {
                continue;
            };
            let task_id = link.task_id;
            let session_id = link.session_id;
            let last_sequence = link.last_activity_sequence;
            if link.exit_recorded {
                continue;
            }
            if let Some((code, summary)) = manager.shell_session_exit(session_id) {
                if let Some(link) = self.shell_sessions.get_mut(&resource_id) {
                    link.exit_recorded = true;
                }
                self.record_shell_exit(task_id, resource_id, code, summary);
                continue;
            }

            let observed = manager.shell_session_cwd(session_id);
            // A sequence this pump cannot read is not new output. `NotFound`
            // is the ordinary answer once the hosted entry has retired.
            let sequence = self
                .terminal_service
                .shell_output_sequence(resource_id)
                .unwrap_or(last_sequence);
            let debounce = Duration::from_millis(TERMINAL_CWD_DEBOUNCE_MS as u64);
            let Some(link) = self.shell_sessions.get_mut(&resource_id) else {
                continue;
            };
            let settled_cwd = link.note_cwd_sample(observed, now, debounce);
            let offer_activity = link.should_offer_activity(now, sequence);
            let mut terminal_is_gone = false;
            if let Some(cwd) = settled_cwd {
                terminal_is_gone = matches!(
                    self.record_shell_fact(task_id, resource_id, HostTerminalFact::Cwd(cwd)),
                    Some(TerminalFactOutcome::UnknownTerminal)
                );
            }
            // The cwd write already told us the durable terminal is gone and
            // closed the shell; a second write in the same tick would only
            // re-learn it.
            if offer_activity && !terminal_is_gone {
                let _ = self.record_shell_fact(task_id, resource_id, HostTerminalFact::Activity);
            }
        }
    }

    /// Close every live shell whose durable terminal is gone.
    ///
    /// The framework-level guard behind the release path above, and the reason
    /// a future release path cannot leak a shell by forgetting to close one.
    /// A durable terminal disappears exactly when its resource is released --
    /// the projector drops the `terminal_facts` row with it -- so a
    /// `shell_sessions` entry with no row, or whose resource is `Released`, is
    /// a live PTY nothing can reach any more.
    ///
    /// A task whose snapshot has gone entirely counts too: the shell cannot
    /// outlive the task that owns it.
    ///
    /// A store error is NOT a reason to close anything. It says the sweep could
    /// not look, and closing a live shell on the strength of a failed read
    /// would destroy work; the log line is what makes that visible.
    fn close_orphaned_shell_terminals(&mut self) {
        let links: Vec<(ResourceId, TaskId)> = self
            .shell_sessions
            .iter()
            .map(|(resource_id, link)| (*resource_id, link.task_id))
            .collect();
        let mut orphaned = Vec::new();
        for (resource_id, task_id) in links {
            let snapshot = match self.bus.task_snapshot(task_id) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    eprintln!(
                        "devmanager-host: shell terminal {resource_id} reconciliation could not \
                         read task {task_id}, leaving it open: {error}"
                    );
                    continue;
                }
            };
            if let Some(reason) = orphaned_shell_reason(resource_id, snapshot.as_ref()) {
                orphaned.push((resource_id, reason));
            }
        }
        for (resource_id, reason) in orphaned {
            eprintln!(
                "devmanager-host: shell terminal {resource_id} closed by reconciliation: {reason}"
            );
            self.close_shell_terminal(resource_id);
        }
    }

    /// Release one plain shell: hosted view first, then the manager session.
    ///
    /// The durable `ReleaseResource` effect does **not** reach a plain shell.
    /// `Command::CloseTerminal` emits only `ResourceReleaseBegun`
    /// (`src/domain/command.rs:1675`), and the sole host caller of
    /// `settle_next_resource_release` is the task-close/archive loop
    /// (`src/host/connection.rs:5687`); that settlement records the outbox row
    /// as **process-empty** and performs no process effect at all
    /// (`src/kernel/command_bus.rs:712`, whose own doc says it exists for
    /// "failed launches [that] register a task-owned terminal without creating
    /// an OS process"). So this method owns the whole teardown.
    ///
    /// Order matters and is safe in exactly this direction.
    /// `TerminalService::close` verifies the runtime's attachment fence and
    /// tears the process down through it, which is what retires the view so
    /// clients receive the Exit delta.
    /// `TerminalSession::close_managed_process_exact`
    /// (`src/terminal/session.rs:1074`) does not clear the teardown slot: it
    /// sets `retired` and returns `Ok` on a repeat. The manager's
    /// `close_exact_session_owner` therefore still finds the fence it requires
    /// (`src/services/process_manager.rs:7533`), takes the idempotent path, and
    /// removes the session from its store. Manager-first would work equally,
    /// but only this order guarantees the client-visible Exit delta.
    fn close_shell_terminal(&mut self, resource_id: ResourceId) {
        let Some(link) = self.shell_sessions.remove(&resource_id) else {
            return;
        };
        match self.terminal_service.shell_terminal_id(resource_id) {
            Ok(Some(terminal_id)) => {
                if let Err(error) = self
                    .terminal_service
                    .close(terminal_id, CloseReason::ExplicitServiceClose)
                {
                    eprintln!(
                        "devmanager-host: shell terminal {resource_id} view close failed: {error:?}"
                    );
                }
            }
            Ok(None) => {}
            Err(error) => eprintln!(
                "devmanager-host: shell terminal {resource_id} view lookup failed: {error:?}"
            ),
        }
        // Nothing else retires a hosted entry, so the closed shell's retained
        // deltas and attached runtime would live for the host's lifetime.
        // Afterwards a query for this resource answers `None`, which is the
        // correct end state: the durable strip still renders it from facts.
        if let Err(error) = self.terminal_service.remove_closed(resource_id) {
            eprintln!("devmanager-host: shell terminal {resource_id} retire failed: {error:?}");
        }
        if let Some(runtime) = self.configured_service_runtime.as_ref() {
            if let Err(error) = runtime.manager.close_task_shell_session(link.session_id) {
                eprintln!("devmanager-host: shell terminal {resource_id} close failed: {error}");
            }
        }
    }

    fn sync_provider_session_identities(&mut self) -> Result<(), StoreError> {
        let Some(runtime) = self.configured_service_runtime.as_ref() else {
            return Ok(());
        };
        let bindings = runtime.manager.provider_session_bindings();
        for binding in bindings {
            let Some(snapshot) = self.bus.task_snapshot(binding.task_id)? else {
                continue;
            };
            let Some(agent) = snapshot.agents.get(&binding.agent_session_id) else {
                continue;
            };
            if agent.provider_kind != binding.provider_kind
                || agent.runtime_generation != binding.runtime_generation
            {
                continue;
            }
            match self
                .bus
                .durable_provider_binding(binding.agent_session_id)?
            {
                Some((provider_session_id, resource_id))
                    if provider_session_id == binding.provider_session_id
                        && resource_id == binding.resource_id =>
                {
                    self.mark_provider_restore_succeeded(binding.task_id);
                    continue;
                }
                Some(_) => {
                    return Err(StoreError::Projection(
                        "live provider session identity conflicts with durable agent binding"
                            .into(),
                    ));
                }
                None if agent.provider_session_id.is_some() => {
                    return Err(StoreError::Corruption);
                }
                None => {}
            }
            let receipt = self.bus.execute_host_authorized(
                CommandEnvelope {
                    command_id: crate::domain::CommandId::new(),
                    client_id: crate::domain::ClientId::new(),
                    task_id: Some(binding.task_id),
                    issued_at_ms: unix_time_ms_u64() as i64,
                    expected_task_revision: Some(snapshot.task.revision),
                    command: Command::BindProviderSession {
                        agent_session_id: binding.agent_session_id,
                        resource_id: binding.resource_id,
                        provider_session_id: binding.provider_session_id,
                        expected_runtime_generation: binding.runtime_generation,
                    },
                },
                None,
                RequestId::new(),
                Uuid::nil(),
            )?;
            if !matches!(receipt, CommandReceipt::Accepted { .. }) {
                return Err(StoreError::Projection(
                    "provider session identity binding was rejected".into(),
                ));
            }
            self.fan_out_live_durable_events();
            self.mark_provider_restore_succeeded(binding.task_id);
        }
        Ok(())
    }

    fn rebind_connection(
        &mut self,
        client_id: ClientId,
        old_id: ConnectionOutputId,
        new_id: ConnectionOutputId,
    ) {
        if old_id == new_id {
            return;
        }
        self.registry.rebind_output(client_id, old_id, new_id);
        self.replay_registry
            .rebind_output(client_id, old_id, new_id);
        self.artifact_content_registry
            .rebind_output(client_id, old_id.as_uuid(), new_id.as_uuid());
        // Conversation subscriptions are ephemeral and never rebound: drop the
        // old output's subscriptions so reconnect must open a fresh lease.
        let old_handle = self.outputs.get(&old_id).cloned();
        if let Some(ref old_output) = old_handle {
            self.invalidate_conversation_subscriptions_for_output(old_id, old_output);
        } else {
            for (_subscription_id, entry) in self
                .conversation_registry
                .remove_for_output(old_id.as_uuid())
            {
                self.semantic_dirty.untrack(&entry.session_key);
            }
        }
        self.pending_quit_receipt_acks.remove(&old_id);
        if let Some(old_output) = self.outputs.remove(&old_id) {
            old_output.request_shutdown();
        }
    }

    async fn handle_control_supervised(
        &mut self,
        control: ExecutorControl,
    ) -> Result<Option<HostExecutorOutcome>, StoreError> {
        match control {
            #[cfg(test)]
            ExecutorControl::RunMaintenanceOnce { ack } => {
                match self.drive_supervised_maintenance_unit().await {
                    Ok(Some(outcome)) => {
                        let _ = ack.send(Ok(()));
                        Ok(Some(outcome))
                    }
                    Ok(None) => {
                        let _ = ack.send(Ok(()));
                        Ok(None)
                    }
                    Err(error) => {
                        let _ = ack.send(Err(error.clone()));
                        Err(error)
                    }
                }
            }
            other => {
                self.handle_control(other);
                Ok(None)
            }
        }
    }

    /// Advance one Open/Closing unit; on supervised ReadyToExit, arm+settle+exit.
    async fn drive_supervised_maintenance_unit(
        &mut self,
    ) -> Result<Option<HostExecutorOutcome>, StoreError> {
        let closing = self.bus.host_admission_is_closing()?;
        if !closing {
            match ProcessEmptyTeardownWorker::run_once(&mut self.bus)? {
                ProcessEmptyTeardown::Idle => Ok(None),
                ProcessEmptyTeardown::Settled { .. } => {
                    self.fan_out_live_durable_events();
                    Ok(None)
                }
            }
        } else {
            match HostCleanupWorker::run_once(&mut self.bus)? {
                HostCleanupProgress::Idle => Ok(None),
                HostCleanupProgress::ReadyToExit {
                    operation_id,
                    action_epoch,
                } => self
                    .arm_and_complete_intentional_quit(operation_id, action_epoch)
                    .await
                    .map(Some),
                HostCleanupProgress::Progressed { .. }
                | HostCleanupProgress::BranchCompleted { .. }
                | HostCleanupProgress::Failed { .. } => {
                    self.fan_out_live_durable_events();
                    Ok(None)
                }
            }
        }
    }

    async fn arm_and_complete_intentional_quit(
        &mut self,
        operation_id: OperationId,
        action_epoch: u64,
    ) -> Result<HostExecutorOutcome, StoreError> {
        let arm_tx = self.arm_tx.as_ref().ok_or_else(|| {
            StoreError::Io("supervised executor missing physical-exit arm sender".into())
        })?;
        let (ack_tx, ack_rx) = oneshot::channel();
        arm_tx
            .send(PhysicalExitArmRequest {
                operation_id,
                action_epoch,
                ack: ack_tx,
            })
            .await
            .map_err(|_| {
                StoreError::Io("physical-exit arm request rejected by supervisor".into())
            })?;
        ack_rx.await.map_err(|_| {
            StoreError::Io("physical-exit arm acknowledgement dropped by supervisor".into())
        })?;

        self.quiesce_intake();
        // Fail closed on receipt lineage before any durable settle can persist Closed.
        self.reap_shutdown_outputs();
        let high_water = std::mem::take(&mut self.pending_quit_receipt_acks);
        for pending in high_water.values() {
            if pending.operation_id != operation_id {
                return Err(StoreError::Corruption);
            }
        }

        let settlement = HostCleanupWorker::settle_success(&mut self.bus)?;
        if settlement.operation_id != operation_id || settlement.action_epoch != action_epoch {
            return Err(StoreError::Corruption);
        }

        self.deliver_terminal_and_await_high_water(
            settlement.terminal_event,
            operation_id,
            high_water,
        )
        .await?;

        for output in self.outputs.values() {
            output.request_shutdown();
        }

        Ok(HostExecutorOutcome::Intentional {
            operation_id,
            action_epoch,
        })
    }

    fn quiesce_intake(&mut self) {
        self.rx.close();
        self.control_rx.close();
        self.control_closed = true;
        while let Ok(job) = self.rx.try_recv() {
            let _ = job.reply.send(Err(IpcError::Unavailable));
        }
        while let Ok(control) = self.control_rx.try_recv() {
            match control {
                ExecutorControl::RegisterOutput { output, ack, .. } => {
                    output.request_shutdown();
                    drop(ack);
                }
                ExecutorControl::UnregisterOutput { id } => {
                    self.detach_output(id);
                }
                ExecutorControl::AttachTerminal { ack, .. } => {
                    let _ = ack.send(Err(
                        "terminal attachment rejected after quit intake quiesce".into(),
                    ));
                }
                ExecutorControl::BindTerminalIdentity { ack, .. } => {
                    let _ = ack.send(Err(
                        "terminal identity binding rejected after quit intake quiesce".into(),
                    ));
                }
                ExecutorControl::InspectHostQuitForUpdate { ack } => {
                    let _ = ack.send(Err(
                        "InspectHostQuit rejected after quit intake quiesce".into()
                    ));
                }
                ExecutorControl::PrepareUpdate { ack, .. } => {
                    let _ = ack.send(Err(
                        "update handoff rejected after quit intake quiesce".into()
                    ));
                }
                ExecutorControl::ConfirmUpdateDrain { ack, .. }
                | ExecutorControl::AbortUpdateHandoff { ack }
                | ExecutorControl::ArmUpdateInstall { ack, .. }
                | ExecutorControl::SealUpdateAfterDurableStage { ack } => {
                    let _ = ack.send(Err(
                        "update handoff rejected after quit intake quiesce".into()
                    ));
                }
                ExecutorControl::SetBrowserGatewayRegistrar { ack, .. } => {
                    let _ = ack.send(());
                }
                #[cfg(test)]
                ExecutorControl::InspectOutput { ack, .. } => {
                    drop(ack);
                }
                #[cfg(test)]
                ExecutorControl::RunMaintenanceOnce { ack } => {
                    let _ = ack.send(Err(StoreError::Io(
                        "maintenance rejected after quit intake quiesce".into(),
                    )));
                }
                #[cfg(test)]
                ExecutorControl::TakePendingQuitReceiptAck { ack, .. } => {
                    let _ = ack.send(None);
                }
                #[cfg(test)]
                ExecutorControl::InstallTestSemanticJournal { ack, .. } => {
                    let _ = ack.send(());
                }
                #[cfg(test)]
                ExecutorControl::RecordTestSemantic { ack, .. } => {
                    let _ = ack.send(0);
                }
            }
        }
    }

    async fn deliver_terminal_and_await_high_water(
        &mut self,
        terminal_event: DomainEvent,
        quit_operation_id: OperationId,
        mut high_water: HashMap<ConnectionOutputId, PendingQuitReceiptAck>,
    ) -> Result<(), StoreError> {
        self.reap_shutdown_outputs();
        let deadline = Instant::now() + QUIT_TERMINAL_ACK_TIMEOUT;

        // Snapshot each live tail's stream + last admitted sequence, grouped by
        // output. Subscription IDs are sorted for deterministic terminal order.
        let mut live_bindings: Vec<(ConnectionOutputId, SubscriptionId)> = self
            .replay_registry
            .entries
            .iter()
            .filter_map(|(subscription_id, entry)| {
                entry
                    .live
                    .as_ref()
                    .map(|live| (live.output_id, *subscription_id))
            })
            .collect();
        live_bindings.sort_unstable();

        let mut by_output: BTreeMap<
            ConnectionOutputId,
            Vec<(SubscriptionId, Arc<LiveStreamState>, u64)>,
        > = BTreeMap::new();
        for (output_id, subscription_id) in live_bindings {
            let Some(live) = self
                .replay_registry
                .entries
                .get(&subscription_id)
                .and_then(|entry| entry.live.as_ref())
            else {
                continue;
            };
            by_output.entry(output_id).or_default().push((
                subscription_id,
                Arc::clone(&live.stream),
                live.last_admitted_sequence,
            ));
        }

        let mut pending_outputs: HashMap<ConnectionOutputId, Vec<SubscriptionId>> = HashMap::new();
        let mut fences = FuturesUnordered::new();
        for (output_id, tails) in by_output {
            let subscription_ids: Vec<SubscriptionId> = tails
                .iter()
                .map(|(subscription_id, _, _)| *subscription_id)
                .collect();
            pending_outputs.insert(output_id, subscription_ids.clone());
            fences.push(async move {
                for (_, stream, target) in tails {
                    stream.wait_until_physically_written(target).await;
                }
                (output_id, subscription_ids)
            });
        }

        while !fences.is_empty() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, fences.next()).await {
                Ok(Some((output_id, subscription_ids))) => {
                    pending_outputs.remove(&output_id);
                    // Durable high-water reached: cancel only these live tails, then
                    // admit ordered terminal CRITICAL (never after skipped history).
                    for subscription_id in &subscription_ids {
                        self.replay_registry.remove(*subscription_id);
                    }
                    let Some(output) = self.outputs.get(&output_id).cloned() else {
                        high_water.remove(&output_id);
                        continue;
                    };
                    let mut last_terminal_ack = None;
                    let mut admit_ok = true;
                    for subscription_id in &subscription_ids {
                        match output.try_enqueue_critical_tracked(ServerMessage::DurableEvent {
                            subscription_id: *subscription_id,
                            event: terminal_event.clone(),
                        }) {
                            Ok(ack) => last_terminal_ack = Some(ack),
                            Err(_) => {
                                admit_ok = false;
                                break;
                            }
                        }
                    }
                    if !admit_ok {
                        if let Some(output) = self.outputs.get(&output_id) {
                            output.request_shutdown();
                        }
                        high_water.remove(&output_id);
                        continue;
                    }
                    if let Some(ack) = last_terminal_ack {
                        high_water.insert(
                            output_id,
                            PendingQuitReceiptAck {
                                operation_id: quit_operation_id,
                                ack,
                            },
                        );
                    } else {
                        high_water.remove(&output_id);
                    }
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }

        // Shared deadline expired or fences dropped: never settle after skipped history.
        for (output_id, subscription_ids) in pending_outputs.drain() {
            self.abort_quit_output_chain(output_id, &subscription_ids, &mut high_water);
        }
        drop(fences);

        // Receipt-only / final-terminal high-waters use only the remainder of the
        // same absolute deadline — no per-client fresh timeout.
        let remaining = deadline.saturating_duration_since(Instant::now());
        if !remaining.is_zero() {
            let _ = tokio::time::timeout(remaining, async {
                for (_, pending) in high_water {
                    let _ = pending.ack.wait().await;
                }
            })
            .await;
        }
        Ok(())
    }

    fn abort_quit_output_chain(
        &mut self,
        output_id: ConnectionOutputId,
        subscription_ids: &[SubscriptionId],
        high_water: &mut HashMap<ConnectionOutputId, PendingQuitReceiptAck>,
    ) {
        for subscription_id in subscription_ids {
            self.replay_registry.remove(*subscription_id);
        }
        if let Some(output) = self.outputs.get(&output_id) {
            output.request_shutdown();
        }
        high_water.remove(&output_id);
    }

    /// Advance exactly one Open teardown or Closing cleanup unit and fan out on progress.
    fn run_one_cleanup_or_teardown_unit(&mut self) -> Result<(), StoreError> {
        let closing = self.bus.host_admission_is_closing()?;
        let fan_out = if closing {
            match HostCleanupWorker::run_once(&mut self.bus)? {
                HostCleanupProgress::Idle | HostCleanupProgress::ReadyToExit { .. } => false,
                HostCleanupProgress::Progressed { .. }
                | HostCleanupProgress::BranchCompleted { .. }
                | HostCleanupProgress::Failed { .. } => true,
            }
        } else {
            match ProcessEmptyTeardownWorker::run_once(&mut self.bus)? {
                ProcessEmptyTeardown::Idle => false,
                ProcessEmptyTeardown::Settled { .. } => true,
            }
        };
        if fan_out {
            self.fan_out_live_durable_events();
        }
        Ok(())
    }

    fn detach_output(&mut self, id: ConnectionOutputId) {
        self.pending_quit_receipt_acks.remove(&id);
        if let Some(output) = self.outputs.remove(&id) {
            self.invalidate_conversation_subscriptions_for_output(id, &output);
            output.request_shutdown();
        } else {
            for (_subscription_id, entry) in
                self.conversation_registry.remove_for_output(id.as_uuid())
            {
                self.semantic_dirty.untrack(&entry.session_key);
            }
        }
        self.replay_registry.remove_for_output(id);
    }

    /// Remove one output registration for an acknowledged detach without
    /// requesting shutdown yet (ack must be physically written first).
    fn release_output_for_detach(
        &mut self,
        id: ConnectionOutputId,
    ) -> Option<ConnectionOutputHandle> {
        self.pending_quit_receipt_acks.remove(&id);
        let output = self.outputs.remove(&id);
        if let Some(ref handle) = output {
            self.invalidate_conversation_subscriptions_for_output(id, handle);
        } else {
            for (_subscription_id, entry) in
                self.conversation_registry.remove_for_output(id.as_uuid())
            {
                self.semantic_dirty.untrack(&entry.session_key);
            }
        }
        self.replay_registry.remove_for_output(id);
        output
    }

    fn invalidate_conversation_subscriptions_for_output(
        &mut self,
        id: ConnectionOutputId,
        output: &ConnectionOutputHandle,
    ) {
        for (subscription_id, entry) in self.conversation_registry.remove_for_output(id.as_uuid()) {
            self.semantic_dirty.untrack(&entry.session_key);
            output.release_reserved_ephemeral_key(EphemeralKey::Conversation(subscription_id));
        }
    }

    fn serve_detach(
        &mut self,
        negotiated: NegotiatedParameters,
        request: DetachRequest,
        output_id: Option<ConnectionOutputId>,
    ) -> Result<ServerMessage, IpcError> {
        if !negotiated.capabilities.contains(Capability::ExplicitDetach) {
            return Err(IpcError::UnsupportedCapability);
        }
        if request.client_id != negotiated.client_id {
            return Err(IpcError::Unauthorized);
        }
        let Some(registered_id) = output_id else {
            return Err(IpcError::Unauthorized);
        };
        let requested_id = ConnectionOutputId::from_uuid(request.connection_id);
        if requested_id != registered_id {
            return Err(IpcError::Unauthorized);
        }
        if self.release_output_for_detach(registered_id).is_none() {
            return Err(IpcError::Unauthorized);
        }
        Ok(ServerMessage::Detached(DetachAck {
            request_id: request.request_id,
            connection_id: request.connection_id,
        }))
    }

    fn reap_shutdown_outputs(&mut self) {
        let dead = self
            .outputs
            .iter()
            .filter(|(_, output)| output.is_shutdown_requested())
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for id in dead {
            self.detach_output(id);
        }
    }

    fn dispatch_job(
        &mut self,
        negotiated: NegotiatedParameters,
        request: ClientRequest,
        output_id: Option<ConnectionOutputId>,
        routing: HostRequestCompletionRouting,
    ) -> Result<DuplexExecuteCompletion, IpcError> {
        let is_confirm_host_quit = matches!(
            &request,
            ClientRequest::Command(envelope)
                if matches!(envelope.command, Command::ConfirmHostQuit(_))
        );
        let response = self.dispatch(negotiated, request, output_id)?;
        if !matches!(
            routing,
            HostRequestCompletionRouting::ExecutorOwnsAcceptedHostQuitReceipt
        ) {
            return Ok(DuplexExecuteCompletion::CallerMustWrite(response));
        }
        // lookup_receipt is command_id-keyed: a ConfirmHostQuit-shaped request can
        // surface a prior non-quit Accepted. Own only the durable host-admission
        // receipt shape (task_revision None, exactly one event_id).
        let operation_id = match (&response, is_confirm_host_quit) {
            (
                ServerMessage::CommandReceipt(CommandReceipt::Accepted {
                    operation_id,
                    task_revision: None,
                    event_ids,
                    ..
                }),
                true,
            ) if event_ids.len() == 1 => *operation_id,
            _ => return Ok(DuplexExecuteCompletion::CallerMustWrite(response)),
        };
        let Some(output_id) = output_id else {
            return Err(IpcError::Unavailable);
        };
        let Some(output) = self.outputs.get(&output_id) else {
            return Err(IpcError::Unavailable);
        };
        if self.pending_quit_receipt_acks.len() >= MAX_SNAPSHOT_SESSIONS {
            return Err(IpcError::Busy);
        }
        let ack = output.try_enqueue_critical_tracked(response)?;
        self.pending_quit_receipt_acks
            .insert(output_id, PendingQuitReceiptAck { operation_id, ack });
        Ok(DuplexExecuteCompletion::ExecutorAdmittedQuitReceipt { operation_id })
    }

    async fn dispatch_task_create_with_primary_provider(
        &mut self,
        negotiated: NegotiatedParameters,
        request: ClientRequest,
        output_id: Option<ConnectionOutputId>,
    ) -> Result<DuplexExecuteCompletion, IpcError> {
        let ClientRequest::Command(envelope) = request else {
            return Err(IpcError::Unavailable);
        };
        if envelope.client_id != negotiated.client_id {
            return Err(IpcError::Unauthorized);
        }
        let (primary_provider, defer_primary_provider_start) = match &envelope.command {
            Command::CreateTaskV2(intent) => {
                (intent.primary_provider, intent.defer_primary_provider_start)
            }
            _ => return Err(IpcError::Unavailable),
        };
        if matches!(
            primary_provider,
            Some(crate::providers::ProviderKind::Cursor)
        ) && !defer_primary_provider_start
        {
            return Err(IpcError::Unavailable);
        }
        validate_authenticated_command_capability(negotiated.capabilities, &envelope.command)?;
        if command_starts_new_launch(&envelope.command) && self.update_gate.stops_new_launches() {
            eprintln!("devmanager-host: provider start blocked by update gate");
            return Err(IpcError::Unavailable);
        }
        let connection_id = output_id
            .map(ConnectionOutputId::as_uuid)
            .unwrap_or(Uuid::nil());
        let issued_at_ms = envelope.issued_at_ms;
        let (normalized, authorization, request_id) = normalize_task_create_at_host(
            envelope,
            Some(&self.workspace_projects),
            self.config_admission.as_ref(),
            connection_id,
            Some(&self.workspace_coordinator),
        )?;
        let task_id = match &normalized.command {
            Command::CreateTask(intent) => intent.id,
            _ => return Err(IpcError::Unavailable),
        };
        let receipt = self
            .bus
            .execute_host_authorized(
                normalized,
                authorization,
                request_id.unwrap_or_else(RequestId::new),
                connection_id,
            )
            .map_err(map_store_error)?;
        self.fan_out_live_durable_events();
        let provider_kind = primary_provider.ok_or(IpcError::Unavailable)?;
        let mut agent = AgentSessionFacts::new(task_id, AgentRole::Primary, provider_kind, None)
            .map_err(|_| IpcError::Unavailable)?;
        agent.runtime_generation = 1;
        let mut resource = ResourceFacts::new(
            Some(task_id),
            OwnerKind::Task,
            ResourceKind::Terminal,
            ResourceRecipe::terminal(120, 40),
            issued_at_ms,
        )
        .map_err(|_| IpcError::Unavailable)?;
        resource.runtime_generation = 1;

        let client_id = negotiated.client_id;
        let mut follow_through = |command: Command, expected_revision: u64, label: &str| -> bool {
            match self.bus.execute_host_authorized(
                CommandEnvelope {
                    command_id: crate::domain::CommandId::new(),
                    client_id,
                    task_id: Some(task_id),
                    issued_at_ms,
                    expected_task_revision: Some(expected_revision),
                    command,
                },
                None,
                RequestId::new(),
                connection_id,
            ) {
                Ok(crate::domain::command::CommandReceipt::Accepted { .. }) => true,
                Ok(rejected) => {
                    eprintln!("devmanager-host: provider start {label} rejected: {rejected:?}");
                    false
                }
                Err(error) => {
                    eprintln!("devmanager-host: provider start {label} failed: {error}");
                    false
                }
            }
        };
        if !follow_through(
            Command::RegisterAgentSession {
                agent: agent.clone(),
            },
            1,
            "register agent",
        ) {
            return Ok(DuplexExecuteCompletion::CallerMustWrite(
                ServerMessage::CommandReceipt(receipt),
            ));
        }
        if !follow_through(
            Command::RegisterResource {
                resource: resource.clone(),
            },
            2,
            "register resource",
        ) {
            return Ok(DuplexExecuteCompletion::CallerMustWrite(
                ServerMessage::CommandReceipt(receipt),
            ));
        }
        if !follow_through(
            Command::SetPrimaryAgent {
                agent_session_id: agent.id,
            },
            3,
            "set primary agent",
        ) {
            return Ok(DuplexExecuteCompletion::CallerMustWrite(
                ServerMessage::CommandReceipt(receipt),
            ));
        }
        self.fan_out_live_durable_events();
        let snapshot = self
            .bus
            .task_snapshot(task_id)
            .map_err(map_store_error)?
            .ok_or(IpcError::Unavailable)?;

        if defer_primary_provider_start {
            return Ok(DuplexExecuteCompletion::CallerMustWrite(
                ServerMessage::CommandReceipt(receipt),
            ));
        }

        let start_intent = crate::domain::command::StartProviderSessionIntent {
            task_id,
            agent_session_id: agent.id,
            resource_id: resource.id,
            provider_kind,
            mode: crate::domain::command::ProviderStartMode::NewConversation,
            launch_options: crate::providers::adapter::ProviderLaunchOptions::default(),
            expected_task_revision: snapshot.task.revision,
            expected_action_epoch: snapshot.task.action_epoch,
        };
        // Durable create facts are already committed. Schedule observation/launch
        // onto the cancellation-owned FuturesUnordered lane and return the create
        // receipt immediately so task-list queries stay responsive.
        if let Err(error) = self.start_provider_session_intent(&start_intent) {
            eprintln!("devmanager-host: provider start schedule failed after create: {error}");
            if let Some(revision) = self
                .bus
                .task_snapshot(task_id)
                .ok()
                .flatten()
                .map(|snapshot| snapshot.task.revision)
            {
                let _ = self
                    .bus
                    .execute_host_authorized(
                        CommandEnvelope {
                            command_id: crate::domain::CommandId::new(),
                            client_id: negotiated.client_id,
                            task_id: Some(task_id),
                            issued_at_ms,
                            expected_task_revision: Some(revision),
                            command: Command::SetTaskAttention(SetTaskAttentionIntent {
                                attention: crate::domain::task::TaskAttention::Failed,
                            }),
                        },
                        None,
                        RequestId::new(),
                        connection_id,
                    )
                    .map_err(map_store_error);
                self.fan_out_live_durable_events();
            }
        }
        Ok(DuplexExecuteCompletion::CallerMustWrite(
            ServerMessage::CommandReceipt(receipt),
        ))
    }

    /// Authenticated stock-provider effect. The durable bus supplies the
    /// exact task/resource join; the live registry supplies the attested
    /// executable/capability observation immediately before launch.
    async fn dispatch_provider_start(
        &mut self,
        negotiated: NegotiatedParameters,
        request: ClientRequest,
        _output_id: Option<ConnectionOutputId>,
    ) -> Result<DuplexExecuteCompletion, IpcError> {
        let ClientRequest::Command(envelope) = request else {
            return Err(IpcError::Unavailable);
        };
        if envelope.client_id != negotiated.client_id {
            return Err(IpcError::Unauthorized);
        }
        if let Err(error) =
            validate_authenticated_command_capability(negotiated.capabilities, &envelope.command)
        {
            eprintln!("devmanager-host: provider start capability rejected: {error}");
            return Err(error);
        }
        let Command::StartProviderSession(intent) = envelope.command else {
            return Err(IpcError::Unavailable);
        };
        if envelope.task_id != Some(intent.task_id)
            || envelope.expected_task_revision != Some(intent.expected_task_revision)
        {
            eprintln!("devmanager-host: provider start envelope fence mismatch");
            return Err(IpcError::Security(
                "provider start envelope fence mismatch".into(),
            ));
        }
        let task_revision = self.start_provider_session_intent(&intent)?;
        Ok(DuplexExecuteCompletion::CallerMustWrite(
            ServerMessage::CommandReceipt(CommandReceipt::Accepted {
                command_id: envelope.command_id,
                operation_id: OperationId::new(),
                task_revision: Some(task_revision),
                event_ids: Vec::new(),
                prompt_mutation: None,
            }),
        ))
    }

    fn start_provider_session_intent(
        &mut self,
        intent: &crate::domain::command::StartProviderSessionIntent,
    ) -> Result<u64, IpcError> {
        let runtime = self.configured_service_runtime.as_ref().ok_or_else(|| {
            eprintln!("devmanager-host: provider start has no configured service runtime");
            IpcError::Unavailable
        })?;
        let manager = runtime.manager.clone();
        let mut intent = intent.clone();
        if self.provider_restore_in_flight.contains(&intent.task_id)
            || self
                .provider_restore_queue
                .iter()
                .any(|queued| queued.task_id == intent.task_id)
        {
            return Err(IpcError::Unavailable);
        }
        {
            let snapshot = self
                .bus
                .task_snapshot(intent.task_id)
                .map_err(|error| {
                    eprintln!("devmanager-host: provider start snapshot failed: {error}");
                    map_store_error(error)
                })?
                .ok_or_else(|| {
                    eprintln!("devmanager-host: provider start task missing");
                    IpcError::Unavailable
                })?;
            let agent = snapshot
                .agents
                .get(&intent.agent_session_id)
                .ok_or_else(|| {
                    eprintln!("devmanager-host: provider start agent missing");
                    IpcError::Unavailable
                })?;
            // Validate the caller's original task/agent/resource fences before
            // journaling any provider change. The kind is the only draft field
            // being replaced; all other identity must match the existing claim.
            let mut original_claim = intent.clone();
            original_claim.provider_kind = agent.provider_kind;
            self.bus
                .prepare_provider_start(&original_claim)
                .map_err(map_store_error)?;
            // Same-kind NewConversation must not double-launch when a live or
            // pending provider already owns the exact agent/resource claim.
            if intent.mode == crate::domain::command::ProviderStartMode::NewConversation {
                match manager.try_has_live_provider_runtime(
                    intent.task_id,
                    intent.agent_session_id,
                    intent.resource_id,
                    agent.runtime_generation,
                ) {
                    Some(true) => {
                        eprintln!(
                            "devmanager-host: NewConversation refused; live provider generation present"
                        );
                        return Err(IpcError::Unavailable);
                    }
                    None => {
                        eprintln!(
                            "devmanager-host: NewConversation refused; live provider book busy/unknown"
                        );
                        return Err(IpcError::Unavailable);
                    }
                    Some(false) => {}
                }
                match manager.try_classify_persisted_provider_launch(intent.agent_session_id) {
                    Ok(Some(true)) => {
                        eprintln!(
                            "devmanager-host: NewConversation refused; persisted launch graph present"
                        );
                        return Err(IpcError::Unavailable);
                    }
                    Ok(Some(false)) => {}
                    Ok(None) | Err(()) => {
                        eprintln!(
                            "devmanager-host: NewConversation refused; persisted launch classification unknown"
                        );
                        return Err(IpcError::Unavailable);
                    }
                }
            }
            if agent.provider_kind != intent.provider_kind {
                if intent.mode != crate::domain::command::ProviderStartMode::NewConversation {
                    eprintln!(
                        "devmanager-host: provider rebind refused for non-NewConversation start"
                    );
                    return Err(IpcError::Unavailable);
                }
                if !snapshot.is_unstarted_draft() {
                    eprintln!("devmanager-host: provider start kind mismatch on non-draft task");
                    return Err(IpcError::Unavailable);
                }
                // Running/launching without SessionStart must not rebind once a
                // live provider terminal or persisted launch graph exists.
                match manager.try_has_live_provider_runtime(
                    intent.task_id,
                    intent.agent_session_id,
                    intent.resource_id,
                    agent.runtime_generation,
                ) {
                    Some(true) => {
                        eprintln!(
                            "devmanager-host: provider rebind refused; live provider generation present"
                        );
                        return Err(IpcError::Unavailable);
                    }
                    None => {
                        eprintln!(
                            "devmanager-host: provider rebind refused; live provider book busy/unknown"
                        );
                        return Err(IpcError::Unavailable);
                    }
                    Some(false) => {}
                }
                match manager.try_classify_persisted_provider_launch(intent.agent_session_id) {
                    Ok(Some(true)) => {
                        eprintln!(
                            "devmanager-host: provider rebind refused; persisted launch graph present"
                        );
                        return Err(IpcError::Unavailable);
                    }
                    Ok(Some(false)) => {}
                    Ok(None) | Err(()) => {
                        eprintln!(
                            "devmanager-host: provider rebind refused; persisted launch classification unknown"
                        );
                        return Err(IpcError::Unavailable);
                    }
                }
                let receipt = self
                    .bus
                    .execute_host_authorized(
                        CommandEnvelope {
                            command_id: crate::domain::CommandId::new(),
                            client_id: crate::domain::ClientId::new(),
                            task_id: Some(intent.task_id),
                            issued_at_ms: unix_time_ms_u64() as i64,
                            expected_task_revision: Some(snapshot.task.revision),
                            command: Command::RebindUnstartedPrimaryProvider {
                                agent_session_id: intent.agent_session_id,
                                provider_kind: intent.provider_kind,
                            },
                        },
                        None,
                        RequestId::new(),
                        Uuid::nil(),
                    )
                    .map_err(|error| {
                        eprintln!("devmanager-host: draft provider rebind failed: {error}");
                        map_store_error(error)
                    })?;
                if !matches!(receipt, CommandReceipt::Accepted { .. }) {
                    eprintln!("devmanager-host: draft provider rebind rejected: {receipt:?}");
                    return Err(IpcError::Unavailable);
                }
                self.fan_out_live_durable_events();
                let rebound = self
                    .bus
                    .task_snapshot(intent.task_id)
                    .map_err(map_store_error)?
                    .ok_or(IpcError::Unavailable)?;
                intent.expected_task_revision = rebound.task.revision;
                intent.expected_action_epoch = rebound.task.action_epoch;
            }
        }
        let (binding, agent, snapshot) =
            self.bus.prepare_provider_start(&intent).map_err(|error| {
                eprintln!("devmanager-host: provider start prepare failed: {error}");
                map_store_error(error)
            })?;
        let loaded = self
            .bus
            .load_task_runtime(intent.task_id, &self.workspace_projects)
            .map_err(|error| {
                eprintln!("devmanager-host: provider start load runtime failed: {error}");
                IpcError::Unavailable
            })?
            .ok_or_else(|| {
                eprintln!("devmanager-host: provider start task runtime missing");
                IpcError::Unavailable
            })?;
        let cwd = loaded
            .workspace
            .runtime_working_directory()
            .map_err(|error| {
                eprintln!("devmanager-host: provider start cwd failed: {error}");
                IpcError::Unavailable
            })?;
        let mode = match intent.mode {
            crate::domain::command::ProviderStartMode::Open => {
                crate::providers::session::ProviderSessionStartMode::Open
            }
            crate::domain::command::ProviderStartMode::NewConversation => {
                crate::providers::session::ProviderSessionStartMode::NewConversation
            }
            crate::domain::command::ProviderStartMode::ResumeExact => {
                crate::providers::session::ProviderSessionStartMode::ResumeExact
            }
        };
        let task_id = intent.task_id;
        let action_epoch = intent.expected_action_epoch;
        let provider_kind = intent.provider_kind;
        let launch_options = intent.launch_options.clone();
        let revision = snapshot.task.revision;
        self.provider_launch_options
            .insert(task_id, launch_options.clone());
        self.provider_restart_unavailable.remove(&task_id);
        self.provider_restore_in_flight.insert(task_id);
        let Some(owner) = self
            .provider_settings
            .as_ref()
            .map(|authority| std::sync::Arc::clone(authority.profile()))
        else {
            self.handle_provider_restore_outcome(ProviderRestoreOutcome {
                task_id,
                action_epoch,
                result: Err("provider settings profile unavailable".into()),
            });
            return Err(IpcError::Unavailable);
        };
        // Sync prepare is complete. Enqueue discovery/launch onto the same
        // cancellation-owned FuturesUnordered lane used by provider restore so
        // the request executor stays free for task-list and cockpit queries.
        self.provider_restore_jobs.push(Box::pin(async move {
            let result = async {
                let resolved = super::provider_launch::resolve_host_provider_launch(
                    owner.as_ref(),
                    task_id,
                    provider_kind,
                    launch_options,
                    provider_start_requires_existing_binding(intent.mode),
                    None,
                )
                .map_err(|error| {
                    format!("devmanager-host: provider start settings failed: {error}")
                })?;
                let observation = super::agent_connection::observe_with_trusted_auth(
                    manager.provider_host().registry(),
                    provider_kind,
                    &resolved.discovery,
                )
                .await
                .map_err(|error| {
                    format!("devmanager-host: provider start observe failed: {error}")
                })?;
                let environment = resolved.environment;
                let launch_options = resolved.launch_options;
                let deferred_binding = resolved.deferred_binding;
                tokio::task::spawn_blocking(move || {
                    manager
                        .start_production_stock_provider_session_with_options(
                            binding,
                            agent,
                            &observation,
                            None,
                            cwd,
                            environment,
                            mode,
                            launch_options,
                        )
                        .map_err(|error| {
                            format!("devmanager-host: provider start launch failed: {error}")
                        })?;
                    if let Some(deferred) = deferred_binding.as_ref() {
                        super::provider_launch::commit_deferred_provider_binding(
                            owner.as_ref(),
                            deferred,
                        )
                        .map_err(|error| {
                            format!("devmanager-host: provider binding commit failed: {error}")
                        })?;
                    }
                    Ok::<(), String>(())
                })
                .await
                .map_err(|error| {
                    format!("devmanager-host: provider start launch worker failed: {error}")
                })??;
                Ok(())
            }
            .await;
            ProviderRestoreOutcome {
                task_id,
                action_epoch,
                result,
            }
        }));
        Ok(revision)
    }

    /// Archive one task after releasing its task-owned resources.
    ///
    /// `BeginCloseTask` alone cannot archive a failed +Claude/+Codex start:
    /// that path registers an Active terminal resource with no OS process, and
    /// process-empty teardown refuses live resources. Release, settle, close,
    /// then archive in this host effect.
    fn close_provider_task_for_archive(
        &mut self,
        task_id: TaskId,
        command_id: crate::domain::id::CommandId,
        current_revision: u64,
    ) -> Result<Option<CommandReceipt>, IpcError> {
        let Some(runtime) = self.configured_service_runtime.as_ref() else {
            return Ok(None);
        };
        if let Err(error) = runtime.manager.close_provider_task(task_id) {
            eprintln!("devmanager-host: task close provider session stop failed: {error}");
            // The domain resource ledger is the authority for live process
            // custody. A stale provider-state row (for example after a CLI
            // upgrade changes its executable hash) must not strand a task
            // whose task-owned resources are already durably Released.
            // Conversely, never archive through a cleanup error while any
            // owned resource may still be live or settling.
            let cleanup_snapshot = self
                .bus
                .task_snapshot(task_id)
                .map_err(map_store_error)?
                .ok_or(IpcError::Unavailable)?;
            if provider_cleanup_requires_resource_release_first(
                cleanup_snapshot.resources.values(),
                task_id,
            ) {
                return Ok(Some(CommandReceipt::Rejected {
                    command_id,
                    code: crate::domain::command::RejectionCode::InvalidTransition,
                    current_revision: Some(current_revision),
                    resolution: None,
                }));
            }
            eprintln!(
                "devmanager-host: task close continuing after stale provider cleanup failure; all task-owned resources are released"
            );
        }
        Ok(None)
    }

    fn dispatch_task_close(
        &mut self,
        negotiated: NegotiatedParameters,
        envelope: CommandEnvelope,
        output_id: Option<ConnectionOutputId>,
    ) -> Result<ServerMessage, IpcError> {
        if envelope.client_id != negotiated.client_id {
            return Err(IpcError::Unauthorized);
        }
        if !matches!(envelope.command, Command::BeginCloseTask) {
            return Err(IpcError::Unavailable);
        }
        let task_id = envelope.task_id.ok_or(IpcError::Unavailable)?;
        let connection_id = output_id
            .map(ConnectionOutputId::as_uuid)
            .unwrap_or(Uuid::nil());
        let issued_at_ms = envelope.issued_at_ms;
        let snapshot = self
            .bus
            .task_snapshot(task_id)
            .map_err(map_store_error)?
            .ok_or(IpcError::Unavailable)?;
        if envelope.expected_task_revision != Some(snapshot.task.revision) {
            eprintln!(
                "devmanager-host: task close stale fence task={task_id} expected={:?} actual={}",
                envelope.expected_task_revision, snapshot.task.revision
            );
            return Err(IpcError::Unavailable);
        }
        if snapshot.task.lifecycle == crate::domain::task::TaskLifecycle::Archived {
            return Ok(ServerMessage::CommandReceipt(CommandReceipt::Rejected {
                command_id: envelope.command_id,
                code: crate::domain::command::RejectionCode::InvalidTransition,
                current_revision: Some(snapshot.task.revision),
                resolution: None,
            }));
        }
        let defer_provider_cleanup =
            provider_cleanup_requires_resource_release_first(snapshot.resources.values(), task_id);
        if !defer_provider_cleanup {
            if let Some(receipt) = self.close_provider_task_for_archive(
                task_id,
                envelope.command_id,
                snapshot.task.revision,
            )? {
                return Ok(ServerMessage::CommandReceipt(receipt));
            }
        }

        let mut released = 0usize;
        loop {
            if released >= 32 {
                eprintln!("devmanager-host: task close exceeded resource release bound");
                return Err(IpcError::Unavailable);
            }
            let snapshot = self
                .bus
                .task_snapshot(task_id)
                .map_err(map_store_error)?
                .ok_or(IpcError::Unavailable)?;
            let Some(resource) = snapshot.resources.values().find(|resource| {
                resource.owner_kind == OwnerKind::Task
                    && resource.task_id == Some(task_id)
                    && matches!(
                        resource.lifecycle,
                        ResourceLifecycle::Active | ResourceLifecycle::Releasing
                    )
            }) else {
                break;
            };
            // `ReleaseResource` is process-empty for a plain shell by
            // construction (`settle_next_resource_release` performs no process
            // effect), so releasing one without this leaves a live PTY, a
            // `shell_sessions` entry and a `HostedTerminal` unreachable until
            // the host exits -- and on Windows the shell holds the task's
            // worktree directory open. `close_shell_terminal` owns the ordered
            // teardown and is a no-op for a resource this host never spawned.
            if resource.recipe.is_plain_shell() {
                self.close_shell_terminal(resource.id);
            }
            if resource.lifecycle == ResourceLifecycle::Active {
                let receipt = self
                    .bus
                    .execute_host_authorized(
                        CommandEnvelope {
                            command_id: crate::domain::id::CommandId::new(),
                            client_id: negotiated.client_id,
                            task_id: Some(task_id),
                            issued_at_ms,
                            expected_task_revision: Some(snapshot.task.revision),
                            command: Command::ReleaseResource {
                                resource_id: resource.id,
                            },
                        },
                        None,
                        RequestId::new(),
                        connection_id,
                    )
                    .map_err(|error| {
                        eprintln!("devmanager-host: task close release resource failed: {error}");
                        map_store_error(error)
                    })?;
                if !matches!(receipt, CommandReceipt::Accepted { .. }) {
                    eprintln!("devmanager-host: task close release resource rejected: {receipt:?}");
                    return Ok(ServerMessage::CommandReceipt(receipt));
                }
                self.fan_out_live_durable_events();
            }
            if self
                .bus
                .settle_next_resource_release(Duration::from_secs(30))
                .map_err(|error| {
                    eprintln!("devmanager-host: task close settle release failed: {error}");
                    map_store_error(error)
                })?
                .is_none()
            {
                eprintln!("devmanager-host: task close settle release found no outbox row");
                return Err(IpcError::Unavailable);
            }
            self.fan_out_live_durable_events();
            released += 1;
        }

        if defer_provider_cleanup {
            let cleanup_revision = self
                .bus
                .task_snapshot(task_id)
                .map_err(map_store_error)?
                .ok_or(IpcError::Unavailable)?
                .task
                .revision;
            if let Some(receipt) = self.close_provider_task_for_archive(
                task_id,
                envelope.command_id,
                cleanup_revision,
            )? {
                return Ok(ServerMessage::CommandReceipt(receipt));
            }
        }

        let snapshot = self
            .bus
            .task_snapshot(task_id)
            .map_err(map_store_error)?
            .ok_or(IpcError::Unavailable)?;
        let receipt = self
            .bus
            .execute_host_authorized(
                CommandEnvelope {
                    command_id: envelope.command_id,
                    client_id: negotiated.client_id,
                    task_id: Some(task_id),
                    issued_at_ms,
                    expected_task_revision: Some(snapshot.task.revision),
                    command: Command::BeginCloseTask,
                },
                None,
                RequestId::new(),
                connection_id,
            )
            .map_err(|error| {
                eprintln!("devmanager-host: task close begin failed: {error}");
                map_store_error(error)
            })?;
        self.fan_out_live_durable_events();
        if matches!(receipt, CommandReceipt::Accepted { .. }) {
            if let Err(error) = self
                .bus
                .settle_next_process_empty_task_teardown(Duration::from_secs(30))
            {
                eprintln!("devmanager-host: task close teardown settle failed: {error}");
                return Err(map_store_error(error));
            }
            self.fan_out_live_durable_events();
        }
        Ok(ServerMessage::CommandReceipt(receipt))
    }

    async fn dispatch_agent_connection(
        &mut self,
        negotiated: NegotiatedParameters,
        request: ClientRequest,
        _output_id: Option<ConnectionOutputId>,
    ) -> Result<DuplexExecuteCompletion, IpcError> {
        let ClientRequest::Query(envelope) = request else {
            return Err(IpcError::Unavailable);
        };
        if envelope.client_id != negotiated.client_id {
            return Err(IpcError::Unauthorized);
        }
        if !negotiated.capabilities.grants_task_cockpit() {
            return Ok(DuplexExecuteCompletion::CallerMustWrite(
                ServerMessage::QueryReply(QueryReply {
                    request_id: envelope.request_id,
                    outcome: QueryOutcome::Err(QueryError::UnsupportedCapability),
                }),
            ));
        }
        let Query::TaskCockpit(TaskCockpitQuery::AgentConnection) = envelope.query else {
            return Err(IpcError::Unavailable);
        };

        let restore_failed_task_ids = self
            .provider_restore_failed_action_epochs
            .keys()
            .copied()
            .collect();
        // Schedule health before borrowing authority for the sync projection.
        if self.provider_settings.is_some() {
            self.maybe_schedule_provider_health(false);
        }
        let agents = if let Some(authority) = self.provider_settings.as_ref() {
            // Health work runs on the cancellation-owned future lane; never
            // await sequential CLI probes on this exclusive executor.
            super::agent_connection::project_agent_connection_from_authority(
                authority,
                restore_failed_task_ids,
            )
        } else {
            crate::domain::AgentConnectionSnapshot {
                agents: vec![
                    crate::domain::AgentConnectionRow {
                        provider: crate::domain::ConfigSidebarProviderKind::Claude,
                        presence: crate::domain::AgentPresence::NotSignedIn,
                    },
                    crate::domain::AgentConnectionRow {
                        provider: crate::domain::ConfigSidebarProviderKind::Codex,
                        presence: crate::domain::AgentPresence::NotSignedIn,
                    },
                ],
                restore_failed_task_ids,
            }
        };
        Ok(DuplexExecuteCompletion::CallerMustWrite(
            ServerMessage::QueryReply(QueryReply {
                request_id: envelope.request_id,
                outcome: QueryOutcome::Ok(QueryResult::TaskCockpit(
                    TaskCockpitResult::AgentConnection(agents),
                )),
            }),
        ))
    }

    fn dispatch(
        &mut self,
        negotiated: NegotiatedParameters,
        request: ClientRequest,
        output_id: Option<ConnectionOutputId>,
    ) -> Result<ServerMessage, IpcError> {
        match request {
            ClientRequest::Command(envelope) => {
                if envelope.client_id != negotiated.client_id {
                    return Err(IpcError::Unauthorized);
                }
                if matches!(envelope.command, Command::ConfirmHostQuit(_))
                    && !negotiated.capabilities.contains(Capability::HostShutdown)
                {
                    return Err(IpcError::UnsupportedCapability);
                }
                validate_authenticated_command_capability(
                    negotiated.capabilities,
                    &envelope.command,
                )?;
                if matches!(
                    envelope.command,
                    Command::PromptLibrary(_) | Command::PromptChain(_)
                ) && !negotiated.capabilities.grants_personal_prompt_library()
                {
                    return Err(IpcError::UnsupportedCapability);
                }
                if command_starts_new_launch(&envelope.command)
                    && self.update_gate.stops_new_launches()
                {
                    return Err(IpcError::Unavailable);
                }
                if let Some(message) =
                    self.try_dispatch_update_handoff_command(&negotiated, &envelope)?
                {
                    return Ok(message);
                }
                if matches!(envelope.command, Command::BeginCloseTask) {
                    return self.dispatch_task_close(negotiated, envelope, output_id);
                }
                let connection_id = output_id
                    .map(ConnectionOutputId::as_uuid)
                    .unwrap_or(Uuid::nil());
                let (envelope, authorization, request_id) = normalize_task_create_at_host(
                    envelope,
                    Some(&self.workspace_projects),
                    self.config_admission.as_ref(),
                    connection_id,
                    Some(&self.workspace_coordinator),
                )?;
                let receipt_request_id =
                    request_id.unwrap_or_else(|| command_receipt_request_id(&envelope));
                let receipt = self
                    .bus
                    .execute_host_authorized(
                        envelope,
                        authorization,
                        receipt_request_id,
                        connection_id,
                    )
                    .map_err(map_store_error)?;
                self.fan_out_live_durable_events();
                Ok(ServerMessage::CommandReceipt(receipt))
            }
            ClientRequest::Query(envelope) => {
                if envelope.client_id != negotiated.client_id {
                    return Err(IpcError::Unauthorized);
                }
                let reply = self.dispatch_query(negotiated, envelope, output_id)?;
                Ok(ServerMessage::QueryReply(reply))
            }
            ClientRequest::TerminalInput(request) => {
                if request.client_id != negotiated.client_id {
                    return Err(IpcError::Unauthorized);
                }
                if !negotiated.capabilities.contains(Capability::ProviderInput) {
                    return Err(IpcError::UnsupportedCapability);
                }
                let input_id = request.input_id;
                let ack = self
                    .terminal_service
                    .write_task_input(request)
                    .map_err(|_| IpcError::Unavailable)?;
                Ok(ServerMessage::TerminalInputAck(
                    crate::terminal::protocol::TerminalInputAck { input_id, ack },
                ))
            }
            ClientRequest::Detach(request) => self.serve_detach(negotiated, request, output_id),
        }
    }

    fn dispatch_query(
        &mut self,
        negotiated: NegotiatedParameters,
        envelope: QueryEnvelope,
        output_id: Option<ConnectionOutputId>,
    ) -> Result<QueryReply, IpcError> {
        let task_id = envelope.task_id;
        match envelope.query {
            Query::CommandReceiptStatus { command } => Ok(QueryReply {
                request_id: envelope.request_id,
                outcome: command_receipt_status_outcome(
                    &self.bus,
                    negotiated.client_id,
                    envelope.client_id,
                    task_id,
                    &command,
                ),
            }),
            Query::SnapshotPage {
                section,
                snapshot_id,
                resume_cursor,
            } => {
                // The kernel snapshot is global. A task-scoped envelope must
                // not be allowed to borrow that view until a task-filtered
                // snapshot implementation exists.
                if task_id.is_some() {
                    return Ok(QueryReply {
                        request_id: envelope.request_id,
                        outcome: QueryOutcome::Err(QueryError::InvalidRequest),
                    });
                }
                if !negotiated.capabilities.contains(Capability::PagedSnapshots) {
                    return Ok(QueryReply {
                        request_id: envelope.request_id,
                        outcome: QueryOutcome::Err(QueryError::UnsupportedCapability),
                    });
                }
                let outcome = self.serve_snapshot_page(
                    negotiated,
                    task_id,
                    section,
                    snapshot_id,
                    resume_cursor,
                    output_id,
                )?;
                Ok(QueryReply {
                    request_id: envelope.request_id,
                    outcome,
                })
            }
            Query::ReleaseSnapshot { snapshot_id } => {
                if !negotiated.capabilities.contains(Capability::PagedSnapshots) {
                    return Ok(QueryReply {
                        request_id: envelope.request_id,
                        outcome: QueryOutcome::Err(QueryError::UnsupportedCapability),
                    });
                }
                let outcome =
                    self.serve_release_snapshot(negotiated, task_id, snapshot_id, output_id);
                Ok(QueryReply {
                    request_id: envelope.request_id,
                    outcome,
                })
            }
            Query::OpenEventReplay { after_sequence } => {
                if task_id.is_some() {
                    return Ok(QueryReply {
                        request_id: envelope.request_id,
                        outcome: QueryOutcome::Err(QueryError::InvalidRequest),
                    });
                }
                if !negotiated.capabilities.contains(Capability::EventReplay) {
                    return Ok(QueryReply {
                        request_id: envelope.request_id,
                        outcome: QueryOutcome::Err(QueryError::UnsupportedCapability),
                    });
                }
                let outcome =
                    self.serve_open_event_replay(negotiated, after_sequence, output_id)?;
                Ok(QueryReply {
                    request_id: envelope.request_id,
                    outcome,
                })
            }
            Query::ContinueEventReplay {
                subscription_id,
                resume_cursor,
            } => {
                if task_id.is_some() {
                    return Ok(QueryReply {
                        request_id: envelope.request_id,
                        outcome: QueryOutcome::Err(QueryError::InvalidRequest),
                    });
                }
                if !negotiated.capabilities.contains(Capability::EventReplay) {
                    return Ok(QueryReply {
                        request_id: envelope.request_id,
                        outcome: QueryOutcome::Err(QueryError::UnsupportedCapability),
                    });
                }
                let outcome = self.serve_continue_event_replay(
                    negotiated,
                    subscription_id,
                    resume_cursor,
                    output_id,
                )?;
                Ok(QueryReply {
                    request_id: envelope.request_id,
                    outcome,
                })
            }
            Query::ReleaseEventReplay { subscription_id } => {
                if task_id.is_some() {
                    return Ok(QueryReply {
                        request_id: envelope.request_id,
                        outcome: QueryOutcome::Err(QueryError::InvalidRequest),
                    });
                }
                if !negotiated.capabilities.contains(Capability::EventReplay) {
                    return Ok(QueryReply {
                        request_id: envelope.request_id,
                        outcome: QueryOutcome::Err(QueryError::UnsupportedCapability),
                    });
                }
                let outcome =
                    self.serve_release_event_replay(negotiated, subscription_id, output_id);
                Ok(QueryReply {
                    request_id: envelope.request_id,
                    outcome,
                })
            }
            Query::OpenArtifactContent { artifact_id } => {
                let Some(task_id) = task_id else {
                    return Ok(QueryReply {
                        request_id: envelope.request_id,
                        outcome: QueryOutcome::Err(QueryError::InvalidRequest),
                    });
                };
                if !negotiated.capabilities.contains(Capability::ChunkResume) {
                    return Ok(QueryReply {
                        request_id: envelope.request_id,
                        outcome: QueryOutcome::Err(QueryError::UnsupportedCapability),
                    });
                }
                let outcome = self.serve_open_artifact_content(
                    negotiated,
                    envelope.request_id,
                    task_id,
                    artifact_id,
                    output_id,
                )?;
                Ok(QueryReply {
                    request_id: envelope.request_id,
                    outcome,
                })
            }
            Query::ContinueArtifactContent {
                subscription_id,
                resume_cursor,
            } => {
                let Some(task_id) = task_id else {
                    return Ok(QueryReply {
                        request_id: envelope.request_id,
                        outcome: QueryOutcome::Err(QueryError::InvalidRequest),
                    });
                };
                if !negotiated.capabilities.contains(Capability::ChunkResume) {
                    return Ok(QueryReply {
                        request_id: envelope.request_id,
                        outcome: QueryOutcome::Err(QueryError::UnsupportedCapability),
                    });
                }
                let outcome = self.serve_continue_artifact_content(
                    negotiated,
                    task_id,
                    subscription_id,
                    resume_cursor,
                    output_id,
                )?;
                Ok(QueryReply {
                    request_id: envelope.request_id,
                    outcome,
                })
            }
            Query::ReleaseArtifactContent { subscription_id } => {
                let Some(task_id) = task_id else {
                    return Ok(QueryReply {
                        request_id: envelope.request_id,
                        outcome: QueryOutcome::Err(QueryError::InvalidRequest),
                    });
                };
                if !negotiated.capabilities.contains(Capability::ChunkResume) {
                    return Ok(QueryReply {
                        request_id: envelope.request_id,
                        outcome: QueryOutcome::Err(QueryError::UnsupportedCapability),
                    });
                }
                let outcome = self.serve_release_artifact_content(
                    negotiated,
                    task_id,
                    subscription_id,
                    output_id,
                )?;
                Ok(QueryReply {
                    request_id: envelope.request_id,
                    outcome,
                })
            }
            Query::InspectHostQuit => {
                if task_id.is_some() {
                    return Ok(QueryReply {
                        request_id: envelope.request_id,
                        outcome: QueryOutcome::Err(QueryError::InvalidRequest),
                    });
                }
                if !negotiated.capabilities.contains(Capability::HostShutdown) {
                    return Ok(QueryReply {
                        request_id: envelope.request_id,
                        outcome: QueryOutcome::Err(QueryError::UnsupportedCapability),
                    });
                }
                self.bus.query(envelope).map_err(map_store_error)
            }
            Query::PromptLibrary(_) => {
                if !negotiated.capabilities.grants_personal_prompt_library() {
                    return Ok(QueryReply {
                        request_id: envelope.request_id,
                        outcome: QueryOutcome::Err(QueryError::UnsupportedCapability),
                    });
                }
                self.bus
                    .query_with_capabilities(
                        negotiated.capabilities,
                        negotiated.limits.max_physical_frame_bytes,
                        envelope,
                    )
                    .map_err(map_store_error)
            }
            Query::TaskCockpit(query) => {
                if let TaskCockpitQuery::OpenConversationSubscription { after_sequence } = &query {
                    let outcome = self.serve_open_conversation_subscription(
                        negotiated,
                        envelope.task_id,
                        envelope.client_id,
                        envelope.request_id,
                        *after_sequence,
                        output_id,
                    );
                    return Ok(QueryReply {
                        request_id: envelope.request_id,
                        outcome,
                    });
                }
                if let TaskCockpitQuery::ReleaseConversationSubscription { subscription_id } =
                    &query
                {
                    let outcome = self.serve_release_conversation_subscription(
                        negotiated,
                        envelope.task_id,
                        *subscription_id,
                        output_id,
                    );
                    return Ok(QueryReply {
                        request_id: envelope.request_id,
                        outcome,
                    });
                }
                // Opening a shell is a host-authority mutation admitted as a
                // typed query. Serve it here, then answer with the Task's
                // refreshed strip so one round trip both opens and shows it.
                let query = if let TaskCockpitQuery::OpenShellTerminal {
                    cwd,
                    expected_task_revision,
                } = &query
                {
                    if !negotiated.capabilities.grants_task_cockpit() {
                        return Ok(QueryReply {
                            request_id: envelope.request_id,
                            outcome: QueryOutcome::Err(QueryError::UnsupportedCapability),
                        });
                    }
                    // The envelope's client id is written into the durable
                    // command, so it must be this connection's own. Reads may
                    // carry a foreign id harmlessly; a write may not.
                    if envelope.client_id != negotiated.client_id {
                        return Ok(QueryReply {
                            request_id: envelope.request_id,
                            outcome: QueryOutcome::Err(QueryError::Unauthorized),
                        });
                    }
                    let open_connection_id = output_id
                        .map(ConnectionOutputId::as_uuid)
                        .unwrap_or(Uuid::nil());
                    match self.open_shell_terminal_request(
                        envelope.client_id,
                        envelope.task_id,
                        cwd.as_deref(),
                        *expected_task_revision,
                        envelope.request_id,
                        open_connection_id,
                    ) {
                        Ok(()) => TaskCockpitQuery::TaskTerminals,
                        Err(outcome) => {
                            return Ok(QueryReply {
                                request_id: envelope.request_id,
                                outcome,
                            })
                        }
                    }
                } else {
                    query
                };
                if let TaskCockpitQuery::RemoteAccess(request) = &query {
                    let outcome = if !negotiated.capabilities.grants_task_cockpit() {
                        QueryOutcome::Err(QueryError::UnsupportedCapability)
                    } else if envelope.task_id.is_some() {
                        QueryOutcome::Err(QueryError::InvalidRequest)
                    } else if let Some(setup) = self.remote_setup.get() {
                        QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::RemoteAccess(
                            setup.request(request.clone()),
                        )))
                    } else {
                        QueryOutcome::Err(QueryError::Unavailable {
                            reason: "remote_setup",
                        })
                    };
                    return Ok(QueryReply {
                        request_id: envelope.request_id,
                        outcome,
                    });
                }
                if let TaskCockpitQuery::ProviderSettings(request) = &query {
                    if !negotiated.capabilities.grants_task_cockpit() {
                        return Ok(QueryReply {
                            request_id: envelope.request_id,
                            outcome: QueryOutcome::Err(QueryError::UnsupportedCapability),
                        });
                    }
                    let outcome = self.dispatch_provider_settings_query(request);
                    return Ok(QueryReply {
                        request_id: envelope.request_id,
                        outcome,
                    });
                }
                if let TaskCockpitQuery::ConfigCreateProject { name, root_path } = &query {
                    if !negotiated.capabilities.grants_task_cockpit() {
                        return Ok(QueryReply {
                            request_id: envelope.request_id,
                            outcome: QueryOutcome::Err(QueryError::UnsupportedCapability),
                        });
                    }
                    let outcome = match self.config_admission.as_mut() {
                        Some(admission) => {
                            let outcome = admission.create_user_project_outcome(name, root_path);
                            if matches!(outcome, QueryOutcome::Ok(_)) {
                                self.workspace_projects = admission.roots().clone();
                            }
                            outcome
                        }
                        None => QueryOutcome::Err(QueryError::Unavailable {
                            reason: "config_store",
                        }),
                    };
                    return Ok(QueryReply {
                        request_id: envelope.request_id,
                        outcome,
                    });
                }
                if matches!(
                    &query,
                    TaskCockpitQuery::ConfigUpsertCommand { .. }
                        | TaskCockpitQuery::ConfigArchiveCommand { .. }
                        | TaskCockpitQuery::ConfigRunCommand { .. }
                ) {
                    if !negotiated.capabilities.grants_task_cockpit() {
                        return Ok(QueryReply {
                            request_id: envelope.request_id,
                            outcome: QueryOutcome::Err(QueryError::UnsupportedCapability),
                        });
                    }
                    let outcome = match self.config_admission.as_mut() {
                        Some(admission) => admission.apply_project_action_outcome(&query),
                        None => QueryOutcome::Err(QueryError::Unavailable {
                            reason: "config_store",
                        }),
                    };
                    return Ok(QueryReply {
                        request_id: envelope.request_id,
                        outcome,
                    });
                }
                let ssh_endpoints = self
                    .config_admission
                    .as_ref()
                    .map(HostWorkspaceAdmission::redacted_ssh_endpoints);
                let (action_epoch, runtime_generation) = self
                    .config_admission
                    .as_ref()
                    .and_then(|admission| {
                        admission.validate_current().ok()?;
                        Some((admission.action_epoch(), admission.runtime_generation()))
                    })
                    .map(|(action_epoch, runtime_generation)| {
                        (Some(action_epoch), Some(runtime_generation))
                    })
                    .unwrap_or((None, None));
                let connection_id = output_id
                    .map(ConnectionOutputId::as_uuid)
                    .unwrap_or(Uuid::nil());
                if matches!(
                    query,
                    TaskCockpitQuery::Terminal | TaskCockpitQuery::TerminalReadiness
                ) {
                    if let Some(runtime) = self.configured_service_runtime.as_ref() {
                        runtime.manager.reconcile_provider_terminal_exits_now();
                    }
                    if provider_terminal_query_may_attach(&query) {
                        if let Some(task_id) = envelope.task_id {
                            // Attachment is idempotent. Retry at the user-visible
                            // query boundary so a provider-native trust/setup prompt
                            // becomes available as soon as the PTY exists, even if
                            // the asynchronous launch completion raced first paint.
                            if let Err(error) = self.attach_provider_terminal(task_id) {
                                let restore_already_pending =
                                    self.provider_restore_in_flight.contains(&task_id)
                                        || self
                                            .provider_restore_queue
                                            .iter()
                                            .any(|queued| queued.task_id == task_id);
                                if !restore_already_pending {
                                    eprintln!(
                                        "devmanager-host: provider terminal attachment deferred task={task_id}: {error}"
                                    );
                                }
                                // Opening Terminal is an explicit request for the
                                // task's exact provider runtime. Restore it lazily
                                // instead of keeping every settled conversation
                                // alive in the background. This also makes an
                                // earlier transient restore failure retryable by a
                                // fresh user gesture.
                                self.provider_restore_failed_action_epochs.remove(&task_id);
                                self.queue_held_provider_restart(
                                    task_id,
                                    crate::providers::dispatch::ProviderDispatchHoldReason::SessionNotBound,
                                );
                                self.queue_one_provider_restore();
                            }
                        }
                    }
                }
                let provider_launch_hint = match envelope.task_id {
                    Some(task_id)
                        if self.provider_restore_in_flight.contains(&task_id)
                            || self
                                .provider_restore_queue
                                .iter()
                                .any(|queued| queued.task_id == task_id) =>
                    {
                        super::cockpit::ProviderLaunchReadinessHint::StartPending
                    }
                    Some(_) if self.provider_restore_pending => {
                        // Startup restore enumeration not yet proved — never
                        // publish NotPending / NotStarted from this host.
                        super::cockpit::ProviderLaunchReadinessHint::Unknown
                    }
                    Some(_) => super::cockpit::ProviderLaunchReadinessHint::NotPending,
                    None => super::cockpit::ProviderLaunchReadinessHint::Unknown,
                };
                let outcome = super::cockpit::serve_task_cockpit_bounded(
                    super::cockpit::TaskCockpitDispatch {
                        capabilities: negotiated.capabilities,
                        envelope_task_id: envelope.task_id,
                        client_id: envelope.client_id,
                        connection_id,
                        request_id: envelope.request_id,
                        query: &query,
                        bus: &self.bus,
                        service_runtime: self
                            .configured_service_runtime
                            .as_ref()
                            .map(|runtime| &runtime.manager),
                        semantic_journal: self.semantic_journal_mutex(),
                        terminal_service: Some(&self.terminal_service),
                        ssh_endpoints: ssh_endpoints.as_deref(),
                        ssh_runtime: self.config_admission.as_ref().map(|admission| {
                            &admission.ssh_runtime as &dyn crate::ssh::SshRuntimeAdapter
                        }),
                        workspace_projects: Some(&self.workspace_projects),
                        coordinator: Some(&self.workspace_coordinator),
                        action_epoch,
                        runtime_generation,
                        config: self
                            .config_admission
                            .as_ref()
                            .map(|admission| &admission.store.snapshot().config),
                        provider_launch_hint,
                    },
                    negotiated.limits.max_page_encoded_bytes,
                );
                Ok(QueryReply {
                    request_id: envelope.request_id,
                    outcome,
                })
            }
            Query::OperationStatus { .. } | Query::TaskSnapshot => {
                self.bus.query(envelope).map_err(map_store_error)
            }
        }
    }

    fn serve_snapshot_page(
        &mut self,
        negotiated: NegotiatedParameters,
        task_id: Option<TaskId>,
        section: SnapshotSection,
        snapshot_id: Option<SnapshotId>,
        resume_cursor: Option<Vec<u8>>,
        output_id: Option<ConnectionOutputId>,
    ) -> Result<QueryOutcome, IpcError> {
        match (snapshot_id, resume_cursor) {
            (None, None) => self.open_snapshot_page(negotiated, task_id, section, output_id),
            (Some(snapshot_id), None) => {
                self.begin_snapshot_section(negotiated, task_id, section, snapshot_id, output_id)
            }
            (Some(snapshot_id), Some(resume_cursor)) => self.resume_snapshot_page(
                negotiated,
                task_id,
                section,
                snapshot_id,
                resume_cursor,
                output_id,
            ),
            (None, Some(_)) => Ok(QueryOutcome::Err(QueryError::InvalidRequest)),
        }
    }

    fn open_snapshot_page(
        &mut self,
        negotiated: NegotiatedParameters,
        task_id: Option<TaskId>,
        section: SnapshotSection,
        output_id: Option<ConnectionOutputId>,
    ) -> Result<QueryOutcome, IpcError> {
        let now = Instant::now();
        self.registry.reap_idle(now);
        let limits = page_limits_from_negotiated(negotiated)?;
        self.registry.prepare_insert();
        let scope = session_scope(negotiated, task_id, output_id);
        let session = self
            .bus
            .begin_snapshot_scoped(limits, scope)
            .map_err(map_snapshot_error_transport)?;
        let page = match session.page(section, None) {
            Ok(page) => page,
            Err(error) => return map_snapshot_section_error(section, error),
        };
        // Retain the pinned session for every valid open page, including empty
        // or single-page first sections, until explicit release / TTL / eviction.
        self.registry
            .insert(negotiated.client_id, session, limits, Instant::now());
        Ok(QueryOutcome::Ok(QueryResult::SnapshotPage { page }))
    }

    fn begin_snapshot_section(
        &mut self,
        negotiated: NegotiatedParameters,
        task_id: Option<TaskId>,
        section: SnapshotSection,
        snapshot_id: SnapshotId,
        output_id: Option<ConnectionOutputId>,
    ) -> Result<QueryOutcome, IpcError> {
        let now = Instant::now();
        self.registry.reap_idle(now);
        if let Some(entry) = self.registry.entries.get(&snapshot_id) {
            if now.duration_since(entry.last_touch) >= SNAPSHOT_IDLE_TTL {
                self.registry.remove(snapshot_id);
                return Ok(QueryOutcome::Err(QueryError::NotFound));
            }
        }
        let limits = page_limits_from_negotiated(negotiated)?;
        let scope = session_scope(negotiated, task_id, output_id);
        let session = match self
            .registry
            .get(snapshot_id, negotiated.client_id, scope, limits, now)
        {
            Ok(session) => session,
            Err(error) => return Ok(QueryOutcome::Err(error)),
        };
        let page = match session.page(section, None) {
            Ok(page) => page,
            Err(error) => return map_snapshot_section_error(section, error),
        };
        self.registry.touch(snapshot_id, Instant::now());
        Ok(QueryOutcome::Ok(QueryResult::SnapshotPage { page }))
    }

    fn resume_snapshot_page(
        &mut self,
        negotiated: NegotiatedParameters,
        task_id: Option<TaskId>,
        section: SnapshotSection,
        snapshot_id: SnapshotId,
        resume_cursor: Vec<u8>,
        output_id: Option<ConnectionOutputId>,
    ) -> Result<QueryOutcome, IpcError> {
        let now = Instant::now();
        self.registry.reap_idle(now);
        // Expire idle entries before serving so TTL maps to NotFound.
        if let Some(entry) = self.registry.entries.get(&snapshot_id) {
            if now.duration_since(entry.last_touch) >= SNAPSHOT_IDLE_TTL {
                self.registry.remove(snapshot_id);
                return Ok(QueryOutcome::Err(QueryError::NotFound));
            }
        }
        let limits = page_limits_from_negotiated(negotiated)?;
        let scope = session_scope(negotiated, task_id, output_id);
        let session = match self
            .registry
            .get(snapshot_id, negotiated.client_id, scope, limits, now)
        {
            Ok(session) => session,
            Err(error) => return Ok(QueryOutcome::Err(error)),
        };
        let page = match session.page(section, Some(resume_cursor.as_slice())) {
            Ok(page) => page,
            Err(error) => {
                // Cursor/shape failures leave a valid retained session intact.
                return map_snapshot_section_error(section, error);
            }
        };
        // Finished sections stay pinned; only release / TTL / eviction drops them.
        self.registry.touch(snapshot_id, Instant::now());
        Ok(QueryOutcome::Ok(QueryResult::SnapshotPage { page }))
    }

    fn serve_release_snapshot(
        &mut self,
        negotiated: NegotiatedParameters,
        task_id: Option<TaskId>,
        snapshot_id: SnapshotId,
        output_id: Option<ConnectionOutputId>,
    ) -> QueryOutcome {
        let now = Instant::now();
        self.registry.reap_idle(now);
        match self.registry.entries.get(&snapshot_id) {
            None => QueryOutcome::Ok(QueryResult::SnapshotReleased { snapshot_id }),
            Some(entry)
                if entry.owner != negotiated.client_id
                    || entry.scope != session_scope(negotiated, task_id, output_id) =>
            {
                QueryOutcome::Err(QueryError::Unauthorized)
            }
            Some(_) => {
                self.registry.remove(snapshot_id);
                QueryOutcome::Ok(QueryResult::SnapshotReleased { snapshot_id })
            }
        }
    }

    fn serve_open_event_replay(
        &mut self,
        negotiated: NegotiatedParameters,
        after_sequence: u64,
        output_id: Option<ConnectionOutputId>,
    ) -> Result<QueryOutcome, IpcError> {
        let now = Instant::now();
        self.replay_registry.reap_idle(now);
        let limits = page_limits_from_negotiated(negotiated)?;
        if !self.replay_registry.prepare_insert() {
            return Err(IpcError::Busy);
        }
        let session = match self.bus.begin_event_replay_scoped(
            after_sequence,
            limits,
            session_scope(negotiated, None, output_id),
        ) {
            Ok(session) => session,
            Err(error) => return map_replay_error(error),
        };
        let subscription_id = session.subscription_id();
        let page = match session.page(None) {
            Ok(page) => page,
            Err(error) => return map_replay_error(error),
        };
        let retain_frozen = page.next_cursor.is_some();
        let live = output_id.map(|output_id| LiveTail::new(output_id, page.through_sequence));
        if retain_frozen || live.is_some() {
            self.replay_registry.insert_open(
                negotiated.client_id,
                session,
                limits,
                live,
                retain_frozen,
                Instant::now(),
            )?;
            if output_id.is_some() {
                self.catch_up_subscription(subscription_id);
            }
        }
        Ok(QueryOutcome::Ok(QueryResult::EventReplayPage {
            subscription_id,
            page,
        }))
    }

    fn serve_continue_event_replay(
        &mut self,
        negotiated: NegotiatedParameters,
        subscription_id: SubscriptionId,
        resume_cursor: Vec<u8>,
        output_id: Option<ConnectionOutputId>,
    ) -> Result<QueryOutcome, IpcError> {
        let now = Instant::now();
        self.replay_registry.reap_idle(now);
        if let Some(entry) = self.replay_registry.entries.get(&subscription_id) {
            if entry.frozen.is_some()
                && now.duration_since(entry.last_touch) >= EVENT_REPLAY_IDLE_TTL
            {
                self.replay_registry.remove(subscription_id);
                return Ok(QueryOutcome::Err(QueryError::NotFound));
            }
        }
        let limits = page_limits_from_negotiated(negotiated)?;
        let session = match self.replay_registry.get_frozen(
            subscription_id,
            negotiated.client_id,
            session_scope(negotiated, None, output_id),
            limits,
            now,
        ) {
            Ok(session) => session,
            Err(error) => return Ok(QueryOutcome::Err(error)),
        };
        let page = match session.page(Some(resume_cursor.as_slice())) {
            Ok(page) => page,
            Err(error) => {
                // Cursor/shape failures leave a valid retained session intact.
                return map_replay_error(error);
            }
        };
        let through_sequence = page.through_sequence;
        let finished = page.next_cursor.is_none();
        if finished {
            // Drop the SQLite read view but retain lightweight live metadata.
            if let Some(entry) = self.replay_registry.entries.get_mut(&subscription_id) {
                entry.frozen = None;
                entry.last_touch = Instant::now();
                Self::bind_live_preserving_admitted(entry, output_id, through_sequence);
            }
            if self
                .replay_registry
                .entries
                .get(&subscription_id)
                .is_some_and(|entry| entry.live.is_some())
            {
                self.catch_up_subscription(subscription_id);
            } else {
                self.replay_registry.remove(subscription_id);
            }
        } else {
            self.replay_registry.touch(subscription_id, Instant::now());
            if let Some(entry) = self.replay_registry.entries.get_mut(&subscription_id) {
                Self::bind_live_preserving_admitted(entry, output_id, through_sequence);
            }
        }
        Ok(QueryOutcome::Ok(QueryResult::EventReplayPage {
            subscription_id,
            page,
        }))
    }

    /// Preserve an existing live admitted cursor on the same output. When the
    /// output identity changes, cancel the old stream and attach a fresh live
    /// tail on the new output from the frozen baseline. Attach a reconnecting
    /// output at the frozen baseline when no live binding remains.
    fn bind_live_preserving_admitted(
        entry: &mut EventReplayRegistryEntry,
        output_id: Option<ConnectionOutputId>,
        frozen_through: u64,
    ) {
        let Some(output_id) = output_id else {
            return;
        };
        match entry.live.as_ref().map(|live| live.output_id == output_id) {
            Some(true) => {
                // Keep last_admitted_sequence / stream progress intact.
            }
            Some(false) => {
                if let Some(old) = entry.live.take() {
                    old.stream.cancel();
                }
                entry.live = Some(LiveTail::new(output_id, frozen_through));
            }
            None => {
                entry.live = Some(LiveTail::new(output_id, frozen_through));
            }
        }
    }

    fn serve_release_event_replay(
        &mut self,
        negotiated: NegotiatedParameters,
        subscription_id: SubscriptionId,
        output_id: Option<ConnectionOutputId>,
    ) -> QueryOutcome {
        let now = Instant::now();
        self.replay_registry.reap_idle(now);
        match self.replay_registry.entries.get(&subscription_id) {
            None => QueryOutcome::Ok(QueryResult::EventReplayReleased { subscription_id }),
            Some(entry)
                if entry.owner != negotiated.client_id
                    || entry.scope != session_scope(negotiated, None, output_id) =>
            {
                QueryOutcome::Err(QueryError::Unauthorized)
            }
            Some(_) => {
                // remove() cancels the live stream generation so queued durables
                // for this subscription are skipped after the release reply.
                self.replay_registry.remove(subscription_id);
                QueryOutcome::Ok(QueryResult::EventReplayReleased { subscription_id })
            }
        }
    }

    fn semantic_journal_mutex(
        &self,
    ) -> Option<&std::sync::Mutex<crate::remote::presentation::SemanticJournalStore>> {
        #[cfg(test)]
        if let Some(journal) = self.test_semantic_journal.as_ref() {
            return Some(journal.as_ref());
        }
        self.configured_service_runtime
            .as_ref()
            .map(|runtime| runtime.semantic_journal.as_ref())
    }

    fn fan_out_conversation_dirty(&mut self) {
        let dirties = self.semantic_dirty.snapshot_watched();
        for (session_key, high_water) in dirties {
            let targets = self.conversation_registry.subscriptions_for(&session_key);
            let mut delivered = true;
            let mut attempted = false;
            for (
                subscription_id,
                task_id,
                generation,
                generation_flag,
                connection_id,
                last_notified,
            ) in targets
            {
                if high_water <= last_notified {
                    continue;
                }
                attempted = true;
                let Some(output) = self
                    .outputs
                    .get(&ConnectionOutputId::from_uuid(connection_id))
                    .cloned()
                else {
                    delivered = false;
                    continue;
                };
                match output.try_admit_conversation_dirty(
                    subscription_id,
                    task_id,
                    generation,
                    generation_flag,
                    high_water,
                ) {
                    EphemeralAdmitResult::Queued | EphemeralAdmitResult::Coalesced => {
                        self.conversation_registry
                            .note_notified(subscription_id, high_water);
                    }
                    EphemeralAdmitResult::CapacityDrop
                    | EphemeralAdmitResult::StaleGeneration
                    | EphemeralAdmitResult::ShutdownRequested => {
                        delivered = false;
                    }
                }
            }
            if !attempted || delivered {
                self.semantic_dirty
                    .clear_if_at_most(&session_key, high_water);
            }
        }
    }

    fn serve_open_conversation_subscription(
        &mut self,
        negotiated: NegotiatedParameters,
        task_id: Option<TaskId>,
        client_id: ClientId,
        request_id: RequestId,
        after_sequence: u64,
        output_id: Option<ConnectionOutputId>,
    ) -> QueryOutcome {
        if !negotiated.capabilities.grants_task_cockpit()
            || !negotiated
                .capabilities
                .contains(Capability::SemanticConversation)
        {
            return QueryOutcome::Err(QueryError::UnsupportedCapability);
        }
        let Some(task_id) = task_id else {
            return QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Denied {
                surface: crate::domain::cockpit::TaskCockpitSurface::Conversation,
                reason: crate::domain::cockpit::TaskCockpitDeniedReason::MissingTask,
                detail: None,
            }));
        };
        let Some(output_id) = output_id else {
            return QueryOutcome::Err(QueryError::InvalidRequest);
        };
        if !self.outputs.contains_key(&output_id) {
            return QueryOutcome::Err(QueryError::Unavailable {
                reason: "conversation_output",
            });
        }
        if client_id != negotiated.client_id {
            return QueryOutcome::Err(QueryError::Unauthorized);
        }

        let connection_id = output_id.as_uuid();
        let session_key =
            super::conversation_wake::ConversationSubscriptionRegistry::session_key_for_task(
                task_id,
            );
        if !self.conversation_registry.prepare_insert(connection_id) {
            return QueryOutcome::Err(QueryError::Unavailable {
                reason: "conversation_subscription_capacity",
            });
        }

        // Track before capture so producer marks during the open window are
        // retained; journal metadata catch-up closes any remaining race.
        self.semantic_dirty.track(session_key.clone());

        let dispatch = super::cockpit::TaskCockpitDispatch {
            capabilities: negotiated.capabilities,
            envelope_task_id: Some(task_id),
            client_id: negotiated.client_id,
            connection_id,
            request_id,
            query: &TaskCockpitQuery::Conversation { after_sequence },
            bus: &self.bus,
            service_runtime: self
                .configured_service_runtime
                .as_ref()
                .map(|runtime| &runtime.manager),
            semantic_journal: self.semantic_journal_mutex(),
            terminal_service: Some(&self.terminal_service),
            ssh_endpoints: None,
            ssh_runtime: None,
            workspace_projects: Some(&self.workspace_projects),
            coordinator: Some(&self.workspace_coordinator),
            action_epoch: None,
            runtime_generation: None,
            config: None,
            provider_launch_hint: super::cockpit::ProviderLaunchReadinessHint::Unknown,
        };
        let page = match super::cockpit::serve_conversation(&dispatch, task_id, after_sequence) {
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Conversation(page))) => {
                page
            }
            other => {
                self.semantic_dirty.untrack(&session_key);
                return other;
            }
        };

        let scope = session_scope(negotiated, Some(task_id), Some(output_id));
        let subscription_id = match self.conversation_registry.insert(
            negotiated.client_id,
            task_id,
            session_key.clone(),
            connection_id,
            scope,
            page.high_water,
        ) {
            Ok(id) => id,
            Err(()) => {
                self.semantic_dirty.untrack(&session_key);
                return QueryOutcome::Err(QueryError::Unavailable {
                    reason: "conversation_subscription_capacity",
                });
            }
        };

        if let Some(output) = self.outputs.get(&output_id) {
            output.reserve_ephemeral_key(EphemeralKey::Conversation(subscription_id));
        } else {
            let _ = self.conversation_registry.remove(subscription_id);
            self.semantic_dirty.untrack(&session_key);
            return QueryOutcome::Err(QueryError::Unavailable {
                reason: "conversation_output",
            });
        }

        let journal_high_water = self.semantic_journal_mutex().and_then(|journal| {
            let Ok(store) = journal.lock() else {
                return None;
            };
            store
                .metadata(&session_key)
                .map(|meta| meta.latest_sequence)
        });
        let catch_up = super::conversation_wake::race_catch_up_high_water(
            page.high_water,
            self.semantic_dirty.peek(&session_key),
            journal_high_water,
        );
        if catch_up > page.high_water {
            let output = self.outputs.get(&output_id).cloned();
            let admit = self
                .conversation_registry
                .get(subscription_id)
                .map(|entry| {
                    (
                        entry.task_id,
                        entry.current_generation(),
                        Arc::clone(&entry.generation),
                    )
                });
            if let (Some(output), Some((task_id, generation, flag))) = (output, admit) {
                match output.try_admit_conversation_dirty(
                    subscription_id,
                    task_id,
                    generation,
                    flag,
                    catch_up,
                ) {
                    EphemeralAdmitResult::Queued | EphemeralAdmitResult::Coalesced => {
                        self.conversation_registry
                            .note_notified(subscription_id, catch_up);
                    }
                    EphemeralAdmitResult::CapacityDrop
                    | EphemeralAdmitResult::StaleGeneration
                    | EphemeralAdmitResult::ShutdownRequested => {
                        // Retain dirty / journal high-water for later fan-out.
                    }
                }
            }
        }
        self.fan_out_conversation_dirty();

        QueryOutcome::Ok(QueryResult::TaskCockpit(
            TaskCockpitResult::ConversationSubscription {
                subscription_id,
                page,
            },
        ))
    }

    fn serve_release_conversation_subscription(
        &mut self,
        negotiated: NegotiatedParameters,
        task_id: Option<TaskId>,
        subscription_id: SubscriptionId,
        output_id: Option<ConnectionOutputId>,
    ) -> QueryOutcome {
        if !negotiated.capabilities.grants_task_cockpit()
            || !negotiated
                .capabilities
                .contains(Capability::SemanticConversation)
        {
            return QueryOutcome::Err(QueryError::UnsupportedCapability);
        }
        let Some(task_id) = task_id else {
            return QueryOutcome::Err(QueryError::InvalidRequest);
        };
        let scope = session_scope(negotiated, Some(task_id), output_id);
        match self
            .conversation_registry
            .release(subscription_id, negotiated.client_id, scope)
        {
            Ok(entry) => {
                self.semantic_dirty.untrack(&entry.session_key);
                if let Some(output) = self
                    .outputs
                    .get(&ConnectionOutputId::from_uuid(entry.connection_id))
                {
                    output.release_reserved_ephemeral_key(EphemeralKey::Conversation(
                        subscription_id,
                    ));
                }
                QueryOutcome::Ok(QueryResult::TaskCockpit(
                    TaskCockpitResult::ConversationSubscriptionReleased { subscription_id },
                ))
            }
            Err(super::conversation_wake::ReleaseError::NotFound) => {
                QueryOutcome::Ok(QueryResult::TaskCockpit(
                    TaskCockpitResult::ConversationSubscriptionReleased { subscription_id },
                ))
            }
            Err(super::conversation_wake::ReleaseError::Unauthorized) => {
                QueryOutcome::Err(QueryError::Unauthorized)
            }
        }
    }

    fn serve_open_artifact_content(
        &mut self,
        negotiated: NegotiatedParameters,
        request_id: RequestId,
        task_id: TaskId,
        artifact_id: ArtifactId,
        output_id: Option<ConnectionOutputId>,
    ) -> Result<QueryOutcome, IpcError> {
        let now = Instant::now();
        self.artifact_content_registry.reap(now);
        let limits = page_limits_from_negotiated(negotiated)?;
        let scope = session_scope(negotiated, Some(task_id), output_id);
        let session = match self.bus.begin_artifact_content_scoped(
            scope,
            request_id,
            artifact_id,
            limits,
            negotiated.limits.max_reassembled_message_bytes,
            negotiated.limits.max_physical_frame_bytes,
        ) {
            Ok(session) => session,
            Err(error) => return map_artifact_content_error(error),
        };
        let subscription_id = session.subscription_id();
        let page = match session.page(None) {
            Ok(page) => page,
            Err(error) => return map_artifact_content_error(error),
        };
        self.artifact_content_registry
            .insert(session, Instant::now());
        Ok(QueryOutcome::Ok(QueryResult::ArtifactContentPage {
            subscription_id,
            page,
        }))
    }

    fn serve_continue_artifact_content(
        &mut self,
        negotiated: NegotiatedParameters,
        task_id: TaskId,
        subscription_id: SubscriptionId,
        resume_cursor: Vec<u8>,
        output_id: Option<ConnectionOutputId>,
    ) -> Result<QueryOutcome, IpcError> {
        let now = Instant::now();
        self.artifact_content_registry.reap(now);
        let limits = page_limits_from_negotiated(negotiated)?;
        let session = match self.artifact_content_registry.get_scoped(
            subscription_id,
            session_scope(negotiated, Some(task_id), output_id),
            limits,
            negotiated.limits.max_reassembled_message_bytes,
            negotiated.limits.max_physical_frame_bytes,
            now,
        ) {
            Ok(session) => session,
            Err(error) => return map_artifact_content_error(error),
        };
        let page = match session.page(Some(resume_cursor.as_slice())) {
            Ok(page) => page,
            Err(error) => {
                // Cursor/shape failures leave a valid retained session intact.
                return map_artifact_content_error(error);
            }
        };
        self.artifact_content_registry
            .touch(subscription_id, Instant::now());
        Ok(QueryOutcome::Ok(QueryResult::ArtifactContentPage {
            subscription_id,
            page,
        }))
    }

    fn serve_release_artifact_content(
        &mut self,
        negotiated: NegotiatedParameters,
        task_id: TaskId,
        subscription_id: SubscriptionId,
        output_id: Option<ConnectionOutputId>,
    ) -> Result<QueryOutcome, IpcError> {
        let now = Instant::now();
        self.artifact_content_registry.reap(now);
        match self.artifact_content_registry.release_scoped(
            subscription_id,
            session_scope(negotiated, Some(task_id), output_id),
        ) {
            Ok(()) => Ok(QueryOutcome::Ok(QueryResult::ArtifactContentReleased {
                subscription_id,
            })),
            Err(error) => map_artifact_content_error(error),
        }
    }

    fn fan_out_live_durable_events(&mut self) {
        let subscription_ids = self
            .replay_registry
            .entries
            .iter()
            .filter_map(|(id, entry)| entry.live.as_ref().map(|_| *id))
            .collect::<Vec<_>>();
        for subscription_id in subscription_ids {
            self.catch_up_subscription(subscription_id);
        }
    }

    fn catch_up_subscription(&mut self, subscription_id: SubscriptionId) {
        let (output_id, mut after_sequence, limits, stream) = {
            let Some(entry) = self.replay_registry.entries.get(&subscription_id) else {
                return;
            };
            let Some(live) = entry.live.as_ref() else {
                return;
            };
            (
                live.output_id,
                live.last_admitted_sequence,
                entry.limits,
                Arc::clone(&live.stream),
            )
        };
        let Some(output) = self.outputs.get(&output_id).cloned() else {
            self.replay_registry.remove(subscription_id);
            return;
        };
        if output.is_shutdown_requested() {
            self.replay_registry.remove(subscription_id);
            return;
        }

        loop {
            let session = match self.bus.begin_event_replay(after_sequence, limits) {
                Ok(session) => session,
                Err(error) => {
                    self.fail_live_replay(subscription_id, &output, &stream, after_sequence, error);
                    return;
                }
            };
            let page = match session.page(None) {
                Ok(page) => page,
                Err(error) => {
                    drop(session);
                    self.fail_live_replay(subscription_id, &output, &stream, after_sequence, error);
                    return;
                }
            };
            drop(session);

            if page.events.is_empty() {
                return;
            }

            let newest_sequence = page.through_sequence;
            for event in page.events {
                let sequence = event.sequence;
                match output.try_enqueue_durable_event(
                    subscription_id,
                    event,
                    &stream,
                    newest_sequence,
                ) {
                    DurableAdmitResult::Admitted => {
                        after_sequence = sequence;
                        if let Some(entry) = self.replay_registry.entries.get_mut(&subscription_id)
                        {
                            if let Some(live) = entry.live.as_mut() {
                                live.last_admitted_sequence = sequence;
                            }
                        }
                    }
                    DurableAdmitResult::ResyncAdmitted { .. } => {
                        self.replay_registry.remove(subscription_id);
                        return;
                    }
                    DurableAdmitResult::ShutdownRequested => {
                        self.replay_registry.remove(subscription_id);
                        return;
                    }
                }
            }

            if page.next_cursor.is_none() {
                return;
            }
        }
    }

    fn fail_live_replay(
        &mut self,
        subscription_id: SubscriptionId,
        output: &ConnectionOutputHandle,
        stream: &Arc<LiveStreamState>,
        last_admitted: u64,
        error: ReplayError,
    ) {
        let newest_sequence = newest_sequence_hint_from_replay_error(
            &error,
            last_admitted,
            stream.last_physically_written(),
        );
        let _ = output.force_live_resync(subscription_id, stream, newest_sequence);
        self.replay_registry.remove(subscription_id);
    }
}

fn push_unique_provider_restore_intent(
    queue: &mut VecDeque<crate::domain::command::StartProviderSessionIntent>,
    in_flight: &HashSet<TaskId>,
    intent: crate::domain::command::StartProviderSessionIntent,
) -> bool {
    if in_flight.contains(&intent.task_id)
        || queue.iter().any(|queued| queued.task_id == intent.task_id)
    {
        return false;
    }
    queue.push_back(intent);
    true
}

fn restore_admitted_provider_launch_options(
    intent: &mut crate::domain::command::StartProviderSessionIntent,
    admitted: &HashMap<TaskId, crate::providers::adapter::ProviderLaunchOptions>,
) {
    if let Some(options) = admitted.get(&intent.task_id) {
        intent.launch_options = options.clone();
    }
}

fn provider_terminal_query_may_attach(query: &TaskCockpitQuery) -> bool {
    // Readiness is a classification probe used by first-send. Starting a
    // provider here races the explicit StartProviderSession command and loses
    // the user's captured model/reasoning/access choices. The legacy Terminal
    // query represents an explicit request to open/restore the runtime.
    matches!(query, TaskCockpitQuery::Terminal)
}

/// Conservative newest-sequence hint for ResyncRequired after a live replay error.
fn newest_sequence_hint_from_replay_error(
    error: &ReplayError,
    last_admitted: u64,
    last_physically_written: u64,
) -> u64 {
    let floor = last_admitted.max(last_physically_written);
    match error {
        ReplayError::ReplayUnavailable {
            newest_sequence, ..
        } => (*newest_sequence).max(floor),
        ReplayError::InvalidRange {
            through_sequence, ..
        } => (*through_sequence).max(floor),
        ReplayError::PageItemTooLarge { sequence, .. } => (*sequence).max(floor),
        ReplayError::Store(_)
        | ReplayError::InvalidLimits(_)
        | ReplayError::EntropyUnavailable
        | ReplayError::InvalidCursor
        | ReplayError::CursorContextMismatch
        | ReplayError::PageEnvelopeTooLarge { .. } => floor,
    }
}

fn page_limits_from_negotiated(negotiated: NegotiatedParameters) -> Result<PageLimits, IpcError> {
    PageLimits::new(
        negotiated.limits.max_page_items,
        negotiated.limits.max_page_encoded_bytes,
    )
    .map_err(|_| IpcError::Unavailable)
}

/// One refused `terminal.open_shell` the client is allowed to see the shape of.
///
/// The wire reason is a closed enum, so it cannot say *which* refusal this was:
/// `OutsideWorkspace` covers both "you gave me a relative path" and "that
/// directory is gone". Where a named cause exists, use
/// [`shell_open_denied_named`] instead — it writes one string to both the host
/// log and the client's `detail`, so the two can never drift.
fn shell_open_denied(reason: crate::domain::TaskCockpitDeniedReason) -> QueryOutcome {
    QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Denied {
        surface: crate::domain::TaskCockpitSurface::Terminal,
        reason,
        detail: None,
    }))
}

fn shell_open_unavailable(reason: crate::domain::TaskCockpitUnavailableReason) -> QueryOutcome {
    QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Unavailable {
        surface: crate::domain::TaskCockpitSurface::Terminal,
        reason,
        detail: None,
    }))
}

/// Log one named refusal and answer with the same sentence.
///
/// One `detail` string, two destinations: the operator's host log and the
/// client that asked. Writing them separately is what let the two named
/// messages exist only in stderr, where nobody using the client can read them.
fn shell_open_denied_named(
    task_id: TaskId,
    reason: crate::domain::TaskCockpitDeniedReason,
    detail: String,
) -> QueryOutcome {
    eprintln!("devmanager-host: open shell refused task={task_id}: {detail}");
    QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Denied {
        surface: crate::domain::TaskCockpitSurface::Terminal,
        reason,
        detail: Some(detail),
    }))
}

/// See [`shell_open_denied_named`].
fn shell_open_unavailable_named(
    task_id: TaskId,
    reason: crate::domain::TaskCockpitUnavailableReason,
    detail: String,
) -> QueryOutcome {
    eprintln!("devmanager-host: open shell refused task={task_id}: {detail}");
    QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Unavailable {
        surface: crate::domain::TaskCockpitSurface::Terminal,
        reason,
        detail: Some(detail),
    }))
}

fn map_store_error(error: StoreError) -> IpcError {
    match error {
        StoreError::Busy => IpcError::Busy,
        _ => IpcError::Unavailable,
    }
}

/// CommandEnvelope has no transport request ID. Its logical receipt request
/// therefore uses the already validated command UUID, not a fresh UUID on each
/// retry. Workspace-authorized commands retain their explicit grant request ID.
/// Physical connection scope remains independent and is never bypassed here.
fn command_receipt_request_id(envelope: &CommandEnvelope) -> RequestId {
    RequestId::from_bytes(*envelope.command_id.as_bytes()).expect("validated command UUIDv7")
}

fn command_receipt_status_outcome(
    bus: &CommandBus,
    authenticated_client: ClientId,
    query_client: ClientId,
    task_id: Option<TaskId>,
    command: &CommandEnvelope,
) -> QueryOutcome {
    if query_client != authenticated_client || command.client_id != authenticated_client {
        return QueryOutcome::Err(QueryError::Unauthorized);
    }
    if task_id != command.task_id {
        return QueryOutcome::Err(QueryError::Conflict);
    }
    match bus.command_receipt_status(authenticated_client, command) {
        Ok(receipt) => QueryOutcome::Ok(QueryResult::CommandReceiptStatus { receipt }),
        Err(StoreError::CommandIdConflict) => QueryOutcome::Err(QueryError::Conflict),
        Err(_) => QueryOutcome::Err(QueryError::Unavailable {
            reason: "receipt_recovery",
        }),
    }
}

fn command_starts_new_launch(command: &Command) -> bool {
    match command {
        Command::CreateTaskV2(intent) => !intent.defer_primary_provider_start,
        Command::CreateTask(_)
        | Command::RegisterAgentSession { .. }
        | Command::RegisterArtifact { .. }
        | Command::RegisterResource { .. }
        | Command::SetPrimaryAgent { .. }
        | Command::StartProviderSession(_) => true,
        _ => false,
    }
}

fn is_provider_start_request(request: &ClientRequest) -> bool {
    matches!(
        request,
        ClientRequest::Command(envelope) if matches!(
            &envelope.command,
            Command::StartProviderSession(_)
        )
    )
}

fn accepted_provider_input_task_id(request: &ClientRequest) -> Option<TaskId> {
    match request {
        ClientRequest::Command(envelope)
            if matches!(&envelope.command, Command::SubmitProviderInput(_)) =>
        {
            envelope.task_id
        }
        _ => None,
    }
}

/// The host process effect owed by one accepted terminal-lifecycle command.
///
/// A plain shell's durable registration and its real process are two separate
/// steps; this is the join between them, read from the request before the
/// envelope is consumed by dispatch.
fn accepted_shell_terminal_effect(request: &ClientRequest) -> Option<AcceptedShellEffect> {
    let ClientRequest::Command(envelope) = request else {
        return None;
    };
    match &envelope.command {
        Command::OpenShellTerminal(intent) => Some(AcceptedShellEffect::Open {
            task_id: intent.resource.task_id?,
            resource_id: intent.resource.id,
        }),
        Command::CloseTerminal { resource_id } => Some(AcceptedShellEffect::Close {
            resource_id: *resource_id,
        }),
        _ => None,
    }
}

fn command_was_accepted(result: &Result<DuplexExecuteCompletion, IpcError>) -> bool {
    matches!(
        result,
        Ok(DuplexExecuteCompletion::CallerMustWrite(
            ServerMessage::CommandReceipt(CommandReceipt::Accepted { .. })
        ))
    )
}

fn is_agent_connection_query(request: &ClientRequest) -> bool {
    matches!(
        request,
        ClientRequest::Query(envelope) if matches!(
            &envelope.query,
            Query::TaskCockpit(TaskCockpitQuery::AgentConnection)
        )
    )
}

impl HostRequestExecutor {
    fn try_dispatch_update_handoff_command(
        &mut self,
        negotiated: &NegotiatedParameters,
        envelope: &CommandEnvelope,
    ) -> Result<Option<ServerMessage>, IpcError> {
        let now = SystemTime::now();
        let receipt = match &envelope.command {
            Command::PrepareUpdate(intent) => {
                if !negotiated.capabilities.contains(Capability::HostShutdown)
                    || !negotiated.capabilities.contains(Capability::UpdateHandoff)
                {
                    return Err(IpcError::UnsupportedCapability);
                }
                if let Some(previous) = self.prepared_update_replies.get(&envelope.command_id) {
                    if previous.intent != *intent {
                        return Err(IpcError::CorrelationMismatch);
                    }
                    if previous.reply.token.is_expired_at(now) {
                        self.prepared_update_replies.remove(&envelope.command_id);
                        let _ = self.update_gate.abort_pre_install();
                    } else {
                        return Ok(Some(ServerMessage::UpdateHandoff(previous.reply.clone())));
                    }
                }
                if self.prepared_update_replies.len() >= MAX_PREPARED_UPDATE_HANDOFFS {
                    return Err(IpcError::Busy);
                }
                let host_boot_id = self.host_boot_id.get().copied().ok_or_else(|| {
                    IpcError::Security("host boot identity is not bound".to_string())
                })?;
                let inspection = self.bus.inspect_host_quit().map_err(|error| {
                    IpcError::Security(format!("InspectHostQuit failed: {error}"))
                })?;
                let mapped = crate::host::update::update_inspection_from_host_quit(
                    &inspection,
                    host_boot_id,
                );
                let mut probe = crate::updater::FixedActiveResourceProbe { inspection: mapped };
                let token = self
                    .update_gate
                    .prepare_update(
                        &mut probe,
                        &intent.target_version,
                        &intent.client_build,
                        &intent.host_build,
                        now,
                        intent.allow_explicit_confirm_with_active,
                    )
                    .map_err(|error| IpcError::Security(error.to_string()))?;
                let reply = UpdateHandoffReply {
                    command_id: envelope.command_id,
                    token,
                };
                self.prepared_update_replies.insert(
                    envelope.command_id,
                    PreparedUpdateReply {
                        intent: intent.clone(),
                        reply: reply.clone(),
                    },
                );
                return Ok(Some(ServerMessage::UpdateHandoff(reply)));
            }
            Command::ConfirmUpdateDrain(intent) => {
                if !negotiated.capabilities.contains(Capability::HostShutdown) {
                    return Err(IpcError::UnsupportedCapability);
                }
                self.update_gate
                    .confirm_drain(intent.token_id, now)
                    .map_err(|error| IpcError::Security(error.to_string()))?;
                CommandReceipt::Accepted {
                    command_id: envelope.command_id,
                    operation_id: crate::domain::id::OperationId::new(),
                    task_revision: None,
                    event_ids: Vec::new(),
                    prompt_mutation: None,
                }
            }
            Command::AbortUpdateHandoff => {
                if !negotiated.capabilities.contains(Capability::HostShutdown) {
                    return Err(IpcError::UnsupportedCapability);
                }
                self.update_gate
                    .abort_pre_install()
                    .map_err(|error| IpcError::Security(error.to_string()))?;
                self.prepared_update_replies.clear();
                CommandReceipt::Accepted {
                    command_id: envelope.command_id,
                    operation_id: crate::domain::id::OperationId::new(),
                    task_revision: None,
                    event_ids: Vec::new(),
                    prompt_mutation: None,
                }
            }
            Command::ArmUpdateInstall(intent) => {
                if !negotiated.capabilities.contains(Capability::HostShutdown) {
                    return Err(IpcError::UnsupportedCapability);
                }
                self.update_gate
                    .begin_atomic_install(intent.token_id, now)
                    .map_err(|error| IpcError::Security(error.to_string()))?;
                self.prepared_update_replies
                    .retain(|_, prepared| prepared.reply.token.token_id != intent.token_id);
                CommandReceipt::Accepted {
                    command_id: envelope.command_id,
                    operation_id: crate::domain::id::OperationId::new(),
                    task_revision: None,
                    event_ids: Vec::new(),
                    prompt_mutation: None,
                }
            }
            Command::ServiceControl(intent) => {
                if !negotiated
                    .capabilities
                    .contains(Capability::ServiceSupervisor)
                {
                    return Err(IpcError::UnsupportedCapability);
                }
                if intent.resource_generation == 0
                    || intent.connection_epoch == 0
                    || intent.action_epoch == 0
                {
                    return Err(IpcError::Security(
                        "service control requires a nonzero admission fence".into(),
                    ));
                }
                let runtime = self
                    .configured_service_runtime
                    .as_mut()
                    .ok_or(IpcError::Unavailable)?;
                let service_id = crate::services::supervisor_service_id(&intent.service_id)
                    .map_err(|error| IpcError::Security(error.to_string()))?;
                let scope = runtime
                    .manager
                    .configured_service_scope(&service_id)
                    .map_err(|error| IpcError::Security(error.to_string()))?;
                let requester = match scope {
                    crate::services::model::ServiceScope::Host => {
                        crate::services::model::AdmissionRequester::Host(
                            crate::services::model::HostAuthority::new(runtime.host_id),
                        )
                    }
                    crate::services::model::ServiceScope::Task { task_id } => {
                        match envelope.task_id {
                            Some(request_task) if request_task == task_id => {
                                crate::services::model::AdmissionRequester::Task(task_id)
                            }
                            _ => {
                                return Err(IpcError::Security(
                                    "service control task scope mismatch".into(),
                                ));
                            }
                        }
                    }
                };
                let action = match intent.action {
                    crate::domain::command::ServiceControlAction::Start => {
                        crate::services::supervisor::SupervisorAction::Start
                    }
                    crate::domain::command::ServiceControlAction::Stop => {
                        crate::services::supervisor::SupervisorAction::Stop
                    }
                    crate::domain::command::ServiceControlAction::Restart => {
                        crate::services::supervisor::SupervisorAction::Restart
                    }
                };
                runtime
                    .manager
                    .configured_service_control(
                        action,
                        &service_id,
                        crate::services::model::AdmissionFence::new(
                            intent.resource_generation,
                            intent.connection_epoch,
                            intent.action_epoch,
                        ),
                        requester,
                    )
                    .map_err(|error| IpcError::Security(error.to_string()))?;
                CommandReceipt::Accepted {
                    command_id: envelope.command_id,
                    operation_id: crate::domain::id::OperationId::new(),
                    task_revision: None,
                    event_ids: Vec::new(),
                    prompt_mutation: None,
                }
            }
            _ => return Ok(None),
        };
        Ok(Some(ServerMessage::CommandReceipt(receipt)))
    }
}

fn is_task_create_with_primary_provider(request: &ClientRequest) -> bool {
    matches!(
        request,
        ClientRequest::Command(CommandEnvelope {
            command: Command::CreateTaskV2(CreateTaskRequestIntent {
                primary_provider: Some(
                    crate::providers::ProviderKind::ClaudeCode
                        | crate::providers::ProviderKind::Codex
                ),
                ..
            }),
            ..
        })
    )
}

/// Normalize request-shaped task creation only after authentication and
/// before the command enters the durable kernel. Raw V1 `CreateTaskIntent`
/// requests are rejected because their client-supplied WorkspaceRef has not
/// been resolved against a host-owned project root.
fn normalize_task_create_at_host(
    envelope: CommandEnvelope,
    workspace_projects: Option<&WorkspaceProjectRoots>,
    config_admission: Option<&HostWorkspaceAdmission>,
    connection_id: Uuid,
    coordinator: Option<&WorkspaceResourceCoordinator>,
) -> Result<
    (
        CommandEnvelope,
        Option<WorkspaceAuthorization>,
        Option<RequestId>,
    ),
    IpcError,
> {
    let CommandEnvelope {
        command,
        command_id,
        client_id,
        task_id,
        issued_at_ms,
        expected_task_revision,
    } = envelope;

    let (command, authorization, request_id) = match command {
        Command::CreateTask(_) => {
            return Err(IpcError::Security(
                "raw CreateTask is not accepted at the authenticated host boundary".into(),
            ));
        }
        Command::CreateTaskV2(CreateTaskRequestIntent {
            id,
            environment_id,
            title,
            description,
            project_id,
            workspace,
            primary_provider,
            assignment,
            created_at_ms,
            connectivity,
            attention,
            activity,
            review_readiness,
            defer_primary_provider_start: _,
        }) => {
            if matches!(
                primary_provider,
                Some(crate::providers::ProviderKind::Cursor)
            ) {
                return Err(IpcError::Unavailable);
            }
            let workspace_projects = workspace_projects.ok_or_else(|| {
                IpcError::Security(
                    "task.create.v2 is unavailable on the compatibility transport".into(),
                )
            })?;
            let (action_epoch, runtime_generation) = if let Some(admission) = config_admission {
                admission
                    .validate_current()
                    .map_err(|_| IpcError::Security("workspace configuration is stale".into()))?;
                (admission.action_epoch(), admission.runtime_generation())
            } else {
                (0, 0)
            };
            let project_root = workspace_projects.root_for(project_id).ok_or_else(|| {
                IpcError::Security("project is not configured for this host".into())
            })?;
            let workspace = match workspace.choice {
                // External is a creation-time choice, not a caller-owned root.
                // The authenticated host keeps the configured project root as
                // the only authority available to durable task creation.
                WorkspaceChoice::External => WorkspaceRequest::confirmed_external(project_root),
                _ => workspace,
            };
            let request_id = RequestId::new();
            let coordinator = coordinator
                .cloned()
                .unwrap_or_else(WorkspaceResourceCoordinator::new);
            let mut service = WorkspaceService::with_task_coordinator(
                project_id,
                id,
                workspace_projects,
                coordinator,
            )
            .map_err(|_| IpcError::Security("configured project is unavailable".into()))?;
            let (binding, authorization) = service
                .bind_authorized_with_generation(
                    workspace,
                    id,
                    client_id,
                    connection_id,
                    request_id,
                    command_id,
                    action_epoch,
                    runtime_generation,
                )
                .map_err(|_| IpcError::Security("workspace request rejected".into()))?;
            (
                Command::CreateTask(CreateTaskIntent {
                    id,
                    environment_id,
                    title,
                    description,
                    project_id,
                    workspace: binding.durable_ref().clone(),
                    assignment,
                    created_at_ms,
                    connectivity,
                    attention,
                    activity,
                    review_readiness,
                }),
                Some(authorization),
                Some(request_id),
            )
        }
        command => (command, None, None),
    };

    Ok((
        CommandEnvelope {
            command_id,
            client_id,
            task_id,
            issued_at_ms,
            expected_task_revision,
            command,
        },
        authorization,
        request_id,
    ))
}

fn map_snapshot_error_transport(error: SnapshotError) -> IpcError {
    eprintln!("devmanager-host: snapshot open failed: {error:?}");
    match error {
        SnapshotError::Store(StoreError::Busy) => IpcError::Busy,
        SnapshotError::InvalidCursor | SnapshotError::CursorContextMismatch => {
            // Open path should not produce cursor errors; treat as unavailable.
            IpcError::Unavailable
        }
        _ => IpcError::Unavailable,
    }
}

fn map_snapshot_section_error(
    section: SnapshotSection,
    error: SnapshotError,
) -> Result<QueryOutcome, IpcError> {
    eprintln!("devmanager-host: snapshot section {section:?} page failed: {error:?}");
    match error {
        SnapshotError::InvalidCursor | SnapshotError::CursorContextMismatch => {
            Ok(QueryOutcome::Err(QueryError::InvalidRequest))
        }
        SnapshotError::Store(StoreError::Busy) => Err(IpcError::Busy),
        SnapshotError::Store(_)
        | SnapshotError::InvalidLimits(_)
        | SnapshotError::EntropyUnavailable
        | SnapshotError::PageEnvelopeTooLarge { .. }
        | SnapshotError::PageItemTooLarge { .. } => Err(IpcError::Unavailable),
    }
}

fn map_replay_error(error: ReplayError) -> Result<QueryOutcome, IpcError> {
    match error {
        ReplayError::ReplayUnavailable {
            oldest_sequence,
            newest_sequence,
        } => Ok(QueryOutcome::Err(QueryError::ReplayUnavailable {
            oldest_sequence,
            newest_sequence,
        })),
        ReplayError::InvalidRange { .. }
        | ReplayError::InvalidCursor
        | ReplayError::CursorContextMismatch => Ok(QueryOutcome::Err(QueryError::InvalidRequest)),
        ReplayError::Store(StoreError::Busy) => Err(IpcError::Busy),
        ReplayError::Store(_)
        | ReplayError::InvalidLimits(_)
        | ReplayError::EntropyUnavailable
        | ReplayError::PageEnvelopeTooLarge { .. }
        | ReplayError::PageItemTooLarge { .. } => Err(IpcError::Unavailable),
    }
}

fn map_artifact_content_error(error: ArtifactContentError) -> Result<QueryOutcome, IpcError> {
    match error {
        ArtifactContentError::NotFound => Ok(QueryOutcome::Err(QueryError::NotFound)),
        ArtifactContentError::Unauthorized => Ok(QueryOutcome::Err(QueryError::Unauthorized)),
        ArtifactContentError::InvalidRequest
        | ArtifactContentError::InvalidCursor
        | ArtifactContentError::CursorContextMismatch
        | ArtifactContentError::ContentDigestMismatch
        | ArtifactContentError::BodyTooLarge { .. } => {
            Ok(QueryOutcome::Err(QueryError::InvalidRequest))
        }
        ArtifactContentError::Store(StoreError::Busy) => Err(IpcError::Busy),
        ArtifactContentError::Store(_)
        | ArtifactContentError::InvalidLimits(_)
        | ArtifactContentError::EntropyUnavailable
        | ArtifactContentError::PageEnvelopeTooLarge { .. } => Err(IpcError::Unavailable),
    }
}

/// Authenticated host request seam used by tests and the compatibility path.
pub fn dispatch_host_request(
    authenticated_client_id: ClientId,
    capabilities: CapabilitySet,
    bus: &mut CommandBus,
    request: ClientRequest,
) -> Result<ServerMessage, IpcError> {
    dispatch_authenticated_request(authenticated_client_id, capabilities, bus, request)
}

/// Authenticated client_id check plus CommandBus execute/query dispatch.
///
/// Used by the exclusive [`super::ipc::HostConnection::serve_request`]
/// compatibility path. Registry-backed snapshot and event-replay queries are
/// unsupported here; the single executor owns those registries.
///
/// `capabilities` are the negotiated grant set from Hello; capability-gated
/// bus queries (currently [`Query::InspectHostQuit`],
/// [`Query::PromptLibrary`], and [`Query::TaskCockpit`]) fail closed here
/// the same way [`HostRequestExecutor`] does.
pub(crate) fn dispatch_authenticated_request(
    authenticated_client_id: ClientId,
    capabilities: CapabilitySet,
    bus: &mut CommandBus,
    request: ClientRequest,
) -> Result<ServerMessage, IpcError> {
    dispatch_authenticated_request_inner(authenticated_client_id, capabilities, bus, None, request)
}

/// Compatibility dispatch with the host-owned ProjectId-to-root mapping.
pub(crate) fn dispatch_authenticated_request_with_workspace_projects(
    authenticated_client_id: ClientId,
    capabilities: CapabilitySet,
    bus: &mut CommandBus,
    workspace_projects: &WorkspaceProjectRoots,
    request: ClientRequest,
) -> Result<ServerMessage, IpcError> {
    dispatch_authenticated_request_inner(
        authenticated_client_id,
        capabilities,
        bus,
        Some(workspace_projects),
        request,
    )
}

fn dispatch_authenticated_request_inner(
    authenticated_client_id: ClientId,
    capabilities: CapabilitySet,
    bus: &mut CommandBus,
    workspace_projects: Option<&WorkspaceProjectRoots>,
    request: ClientRequest,
) -> Result<ServerMessage, IpcError> {
    match request {
        ClientRequest::Command(envelope) => {
            if envelope.client_id != authenticated_client_id {
                return Err(IpcError::Unauthorized);
            }
            if matches!(envelope.command, Command::ConfirmHostQuit(_))
                && !capabilities.contains(Capability::HostShutdown)
            {
                return Err(IpcError::UnsupportedCapability);
            }
            validate_authenticated_command_capability(capabilities, &envelope.command)?;
            // The compatibility seam has no ProcessManager-owned configured
            // supervisor. Never route service control through the durable bus
            // or create a second lifecycle owner here.
            if matches!(envelope.command, Command::ServiceControl(_)) {
                return Err(IpcError::Unavailable);
            }
            if matches!(
                envelope.command,
                Command::PromptLibrary(_) | Command::PromptChain(_)
            ) && !capabilities.grants_personal_prompt_library()
            {
                return Err(IpcError::UnsupportedCapability);
            }
            // The compatibility transport has no resumable connection
            // identity. Keep its receipt unbound so a later registered output
            // can claim the exact same receipt once.
            let connection_id = Uuid::nil();
            let (envelope, authorization, request_id) = normalize_task_create_at_host(
                envelope,
                workspace_projects,
                None,
                connection_id,
                None,
            )?;
            let receipt_request_id =
                request_id.unwrap_or_else(|| command_receipt_request_id(&envelope));
            let receipt = bus
                .execute_host_authorized(envelope, authorization, receipt_request_id, connection_id)
                .map_err(map_store_error)?;
            Ok(ServerMessage::CommandReceipt(receipt))
        }
        ClientRequest::Query(envelope) => {
            if envelope.client_id != authenticated_client_id {
                return Err(IpcError::Unauthorized);
            }
            match &envelope.query {
                Query::CommandReceiptStatus { command } => {
                    return Ok(ServerMessage::QueryReply(QueryReply {
                        request_id: envelope.request_id,
                        outcome: command_receipt_status_outcome(
                            bus,
                            authenticated_client_id,
                            envelope.client_id,
                            envelope.task_id,
                            command,
                        ),
                    }));
                }
                Query::SnapshotPage { .. }
                | Query::ReleaseSnapshot { .. }
                | Query::OpenEventReplay { .. }
                | Query::ContinueEventReplay { .. }
                | Query::ReleaseEventReplay { .. }
                | Query::OpenArtifactContent { .. }
                | Query::ContinueArtifactContent { .. }
                | Query::ReleaseArtifactContent { .. } => {
                    return Ok(ServerMessage::QueryReply(QueryReply {
                        request_id: envelope.request_id,
                        outcome: QueryOutcome::Err(QueryError::UnsupportedCapability),
                    }));
                }
                Query::InspectHostQuit => {
                    if envelope.task_id.is_some() {
                        return Ok(ServerMessage::QueryReply(QueryReply {
                            request_id: envelope.request_id,
                            outcome: QueryOutcome::Err(QueryError::InvalidRequest),
                        }));
                    }
                    if !capabilities.contains(Capability::HostShutdown) {
                        return Ok(ServerMessage::QueryReply(QueryReply {
                            request_id: envelope.request_id,
                            outcome: QueryOutcome::Err(QueryError::UnsupportedCapability),
                        }));
                    }
                }
                Query::PromptLibrary(_) => {
                    if !capabilities.grants_personal_prompt_library() {
                        return Ok(ServerMessage::QueryReply(QueryReply {
                            request_id: envelope.request_id,
                            outcome: QueryOutcome::Err(QueryError::UnsupportedCapability),
                        }));
                    }
                    let reply = bus
                        .query_with_capabilities(
                            capabilities,
                            FrameLimits::v1_default().max_physical_frame_bytes,
                            envelope,
                        )
                        .map_err(map_store_error)?;
                    return Ok(ServerMessage::QueryReply(reply));
                }
                Query::TaskCockpit(query) => {
                    let outcome =
                        super::cockpit::serve_task_cockpit(super::cockpit::TaskCockpitDispatch {
                            capabilities,
                            envelope_task_id: envelope.task_id,
                            client_id: envelope.client_id,
                            connection_id: Uuid::nil(),
                            request_id: envelope.request_id,
                            query,
                            bus,
                            service_runtime: None,
                            semantic_journal: None,
                            terminal_service: None,
                            ssh_endpoints: None,
                            ssh_runtime: None,
                            workspace_projects,
                            coordinator: None,
                            action_epoch: None,
                            runtime_generation: None,
                            config: None,
                            provider_launch_hint:
                                super::cockpit::ProviderLaunchReadinessHint::Unknown,
                        });
                    return Ok(ServerMessage::QueryReply(QueryReply {
                        request_id: envelope.request_id,
                        outcome,
                    }));
                }
                Query::OperationStatus { .. } | Query::TaskSnapshot => {}
            }
            let reply = bus.query(envelope).map_err(map_store_error)?;
            Ok(ServerMessage::QueryReply(reply))
        }
        ClientRequest::TerminalInput(request) => {
            if request.client_id != authenticated_client_id {
                return Err(IpcError::Unauthorized);
            }
            if !capabilities.contains(Capability::ProviderInput) {
                return Err(IpcError::UnsupportedCapability);
            }
            // The compatibility executor has no host-owned TerminalService;
            // never create or infer a PTY from an input request.
            Err(IpcError::Unavailable)
        }
        ClientRequest::Detach(_) => Err(IpcError::Unavailable),
    }
}

/// Capability/source gate shared by both host command dispatch paths.
/// Journal-only provider facts are never accepted from an authenticated client;
/// provider input itself requires the negotiated capability bit.
fn validate_authenticated_command_capability(
    capabilities: CapabilitySet,
    command: &Command,
) -> Result<(), IpcError> {
    match command {
        Command::ConfirmHostQuit(_) if !capabilities.contains(Capability::HostShutdown) => {
            Err(IpcError::UnsupportedCapability)
        }
        Command::PrepareUpdate(_) if !capabilities.contains(Capability::HostShutdown) => {
            Err(IpcError::UnsupportedCapability)
        }
        Command::PrepareUpdate(_) if !capabilities.contains(Capability::UpdateHandoff) => {
            Err(IpcError::UnsupportedCapability)
        }
        // `OpenShellTerminal` carries a fully formed `ResourceFacts` with a
        // program, args and cwd of the sender's choosing, and the decider has
        // no program allowlist. It is host-issuance only: the host resolves
        // both from sealed settings and executes it through
        // `open_shell_terminal_request`, which never passes through here (this
        // gate runs only on client-sent envelopes). A client asks for a shell
        // with `TaskCockpitQuery::OpenShellTerminal`, never with this command.
        Command::PresentProviderQuestion(_)
        | Command::PresentProviderApproval(_)
        | Command::SettleProviderWait(_)
        | Command::BindProviderSession { .. }
        | Command::OpenShellTerminal(_)
        | Command::RebindUnstartedPrimaryProvider { .. } => Err(IpcError::UnsupportedCapability),
        Command::SubmitProviderInput(_) if !capabilities.contains(Capability::ProviderInput) => {
            Err(IpcError::UnsupportedCapability)
        }
        Command::ServiceControl(_) if !capabilities.contains(Capability::ServiceSupervisor) => {
            Err(IpcError::UnsupportedCapability)
        }
        Command::StartProviderSession(_) if !capabilities.contains(Capability::ProviderInput) => {
            Err(IpcError::UnsupportedCapability)
        }
        _ => Ok(()),
    }
}

/// Stable id for one duplex connection's executor-facing output handle.
///
/// Production registrations use the wire [`ServerHello::connection_id`] so host
/// and client share one identity. Unit tests may generate ids via [`Self::new`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ConnectionOutputId(Uuid);

impl ConnectionOutputId {
    /// Test constructor: allocate a fresh UUIDv7 identity.
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self(Uuid::now_v7())
    }

    pub(crate) fn from_uuid(id: Uuid) -> Self {
        Self(id)
    }

    pub(crate) fn as_uuid(self) -> Uuid {
        self.0
    }
}

/// Result of admitting one durable event onto a connection output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DurableAdmitResult {
    Admitted,
    ResyncAdmitted {
        last_delivered_sequence: u64,
        newest_sequence: u64,
    },
    ShutdownRequested,
}

/// Outcome observed when polling a [`PhysicalWriteAck`] without awaiting.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PhysicalWriteAckStatus {
    Pending,
    Succeeded,
    Aborted,
}

/// Per-frame, non-Clone wait handle for one successful physical write.
///
/// Success is reported only after [`PrioritizedOutbound::after_successful_write`].
/// Dropping the outbound acknowledger (encode/write/cancel/drop without success)
/// reports aborted.
#[derive(Debug)]
pub(crate) struct PhysicalWriteAck {
    rx: oneshot::Receiver<()>,
}

impl PhysicalWriteAck {
    pub(crate) async fn wait(self) -> Result<(), ()> {
        self.rx.await.map_err(|_| ())
    }

    #[cfg(test)]
    pub(crate) fn status(&mut self) -> PhysicalWriteAckStatus {
        match self.rx.try_recv() {
            Ok(()) => PhysicalWriteAckStatus::Succeeded,
            Err(oneshot::error::TryRecvError::Empty) => PhysicalWriteAckStatus::Pending,
            Err(oneshot::error::TryRecvError::Closed) => PhysicalWriteAckStatus::Aborted,
        }
    }
}

/// Private sender half; drop aborts the paired [`PhysicalWriteAck`].
struct PhysicalWriteAcknowledger {
    tx: oneshot::Sender<()>,
}

impl PhysicalWriteAcknowledger {
    fn pair() -> (PhysicalWriteAck, Self) {
        let (tx, rx) = oneshot::channel();
        (PhysicalWriteAck { rx }, Self { tx })
    }

    fn acknowledge(self) {
        let _ = self.tx.send(());
    }
}

/// Critical outbound keeps the owned semaphore permit alive until dropped after
/// the physical write returns (success or failure).
pub(crate) struct CriticalOutbound {
    message: ServerMessage,
    _permit: OwnedSemaphorePermit,
    /// Live resync only: finalize `last_delivered_sequence` immediately before
    /// encode/write so an earlier in-flight durable can advance the baseline.
    live_resync: Option<LiveResyncMaterialization>,
    /// Explicit detach: request connection shutdown only after the ack write.
    shutdown_after_successful_write: Option<ConnectionOutputHandle>,
    write_ack: Option<PhysicalWriteAcknowledger>,
}

struct LiveResyncMaterialization {
    stream: Arc<LiveStreamState>,
    newest_sequence_hint: u64,
}

impl CriticalOutbound {
    fn prepare_for_write(&mut self) {
        let Some(materialize) = self.live_resync.take() else {
            return;
        };
        let last_delivered_sequence = materialize.stream.last_physically_written();
        let newest_sequence = materialize
            .newest_sequence_hint
            .max(last_delivered_sequence);
        if let ServerMessage::ResyncRequired {
            last_delivered_sequence: last,
            newest_sequence: newest,
            ..
        } = &mut self.message
        {
            *last = last_delivered_sequence;
            *newest = newest_sequence;
        }
    }
}

/// Durable outbound carries a live-stream generation so cancel/resync can skip
/// already-queued events without poisoning unrelated subscriptions.
pub(crate) struct DurableOutbound {
    message: ServerMessage,
    stream: Arc<LiveStreamState>,
    generation: u64,
    sequence: u64,
}

impl DurableOutbound {
    fn is_current(&self) -> bool {
        self.generation == self.stream.current_generation()
    }

    fn commit_physical_write(self) {
        // Always record: generation only suppresses queued frames that never
        // started writing. An in-flight cancel must not erase a completed write.
        self.stream.record_physical_write(self.sequence);
    }
}

/// Writer-facing prioritized outbound with RAII admission lifetime.
pub(crate) enum PrioritizedOutbound {
    Critical(CriticalOutbound),
    Durable(DurableOutbound),
    Ephemeral(EphemeralOutbound),
}

impl PrioritizedOutbound {
    pub(crate) fn message(&self) -> &ServerMessage {
        match self {
            Self::Critical(outbound) => &outbound.message,
            Self::Durable(outbound) => &outbound.message,
            Self::Ephemeral(outbound) => outbound
                .message
                .as_ref()
                .expect("message() requires should_write"),
        }
    }

    pub(crate) fn should_write(&self) -> bool {
        match self {
            Self::Critical(_) => true,
            Self::Durable(outbound) => outbound.is_current(),
            Self::Ephemeral(outbound) => outbound.should_write(),
        }
    }

    /// Finalize any write-time fields (live ResyncRequired baseline) before encode.
    pub(crate) fn prepare_for_write(&mut self) {
        match self {
            Self::Critical(outbound) => outbound.prepare_for_write(),
            Self::Durable(_) | Self::Ephemeral(_) => {}
        }
    }

    pub(crate) fn after_successful_write(self) {
        match self {
            Self::Critical(outbound) => {
                if let Some(handle) = outbound.shutdown_after_successful_write {
                    handle.request_shutdown();
                }
                if let Some(ack) = outbound.write_ack {
                    ack.acknowledge();
                }
            }
            Self::Durable(outbound) => outbound.commit_physical_write(),
            Self::Ephemeral(mut outbound) => outbound.commit_successful_write(),
        }
    }
}

/// Cloneable materializer invoked only when the writer drains an ephemeral token.
pub(crate) type EphemeralMaterializer =
    Arc<dyn Fn() -> Option<ServerMessage> + Send + Sync + 'static>;

/// Stream-frame materializer preserved for existing callers; wrapped into
/// [`EphemeralMaterializer`] at admission.
pub(crate) type StreamMaterializer = Arc<dyn Fn() -> Option<StreamFrame> + Send + Sync + 'static>;

/// Typed ephemeral coalescer key. Stream frames and conversation dirties share
/// one bounded lane with identical generation/dirty-revision semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum EphemeralKey {
    ResourceStream(StreamKey),
    Conversation(SubscriptionId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EphemeralAdmitResult {
    Queued,
    Coalesced,
    StaleGeneration,
    CapacityDrop,
    ShutdownRequested,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EphemeralPhase {
    Queued,
    InFlight {
        taken_generation: u64,
        taken_dirty_revision: u64,
    },
}

struct EphemeralSlot {
    generation: u64,
    dirty_revision: u64,
    materializer: EphemeralMaterializer,
    phase: EphemeralPhase,
}

struct EphemeralLaneInner {
    capacity: usize,
    /// Conversation subscription keys with a reserved coalescer slot so stream
    /// traffic cannot starve the final dirty notice.
    reserved: HashSet<EphemeralKey>,
    slots: HashMap<EphemeralKey, EphemeralSlot>,
    pending: VecDeque<EphemeralKey>,
}

struct EphemeralControl {
    shutdown: bool,
    lane: EphemeralLaneInner,
    /// Executor-shared wake when a slot frees (capacity return without new token).
    capacity_notify: Option<Arc<Notify>>,
}

impl EphemeralLaneInner {
    fn occupied(&self) -> usize {
        self.slots.len()
    }

    fn clear(&mut self) {
        self.slots.clear();
        self.pending.clear();
        self.reserved.clear();
    }

    fn reserve_key(&mut self, key: EphemeralKey) {
        self.reserved.insert(key);
    }

    fn release_reserved_key(&mut self, key: EphemeralKey) -> bool {
        self.reserved.remove(&key);
        let removed = self.slots.remove(&key).is_some();
        self.pending.retain(|pending| *pending != key);
        removed
    }

    fn invalidate_key(&mut self, key: EphemeralKey) -> bool {
        let removed = self.slots.remove(&key).is_some();
        self.pending.retain(|pending| *pending != key);
        removed
    }

    fn can_admit_new(&self, key: &EphemeralKey) -> bool {
        if self.reserved.contains(key) {
            // Reserved conversation slot: never blocked by stream fill.
            return true;
        }
        let non_reserved = self
            .slots
            .keys()
            .filter(|occupied| !self.reserved.contains(occupied))
            .count();
        non_reserved < self.capacity
    }

    fn admit(
        &mut self,
        key: EphemeralKey,
        generation: u64,
        materializer: EphemeralMaterializer,
    ) -> (EphemeralAdmitResult, bool) {
        if let Some(slot) = self.slots.get_mut(&key) {
            if generation < slot.generation {
                return (EphemeralAdmitResult::StaleGeneration, false);
            }
            slot.generation = generation;
            slot.dirty_revision = slot.dirty_revision.saturating_add(1);
            slot.materializer = materializer;
            let wake = matches!(slot.phase, EphemeralPhase::Queued)
                && !self.pending.iter().any(|pending| *pending == key);
            if wake {
                self.pending.push_back(key);
            }
            return (EphemeralAdmitResult::Coalesced, wake);
        }
        if !self.can_admit_new(&key) {
            return (EphemeralAdmitResult::CapacityDrop, false);
        }
        self.slots.insert(
            key,
            EphemeralSlot {
                generation,
                dirty_revision: 1,
                materializer,
                phase: EphemeralPhase::Queued,
            },
        );
        self.pending.push_back(key);
        (EphemeralAdmitResult::Queued, true)
    }

    fn take_pending(&mut self) -> Option<(EphemeralKey, u64, u64, EphemeralMaterializer)> {
        while let Some(key) = self.pending.pop_front() {
            let Some(slot) = self.slots.get_mut(&key) else {
                continue;
            };
            if !matches!(slot.phase, EphemeralPhase::Queued) {
                continue;
            }
            let taken_generation = slot.generation;
            let taken_dirty_revision = slot.dirty_revision;
            slot.phase = EphemeralPhase::InFlight {
                taken_generation,
                taken_dirty_revision,
            };
            return Some((
                key,
                taken_generation,
                taken_dirty_revision,
                Arc::clone(&slot.materializer),
            ));
        }
        None
    }

    /// Returns `(requeue, freed_capacity)`.
    fn finish(
        &mut self,
        key: EphemeralKey,
        taken_generation: u64,
        taken_dirty_revision: u64,
    ) -> (bool, bool) {
        let Some(slot) = self.slots.get_mut(&key) else {
            return (false, false);
        };
        let EphemeralPhase::InFlight {
            taken_generation: phase_generation,
            taken_dirty_revision: phase_dirty,
        } = slot.phase
        else {
            return (false, false);
        };
        if phase_generation != taken_generation || phase_dirty != taken_dirty_revision {
            return (false, false);
        }
        if slot.dirty_revision > taken_dirty_revision {
            slot.phase = EphemeralPhase::Queued;
            if !self.pending.iter().any(|pending| *pending == key) {
                self.pending.push_back(key);
            }
            (true, false)
        } else {
            self.slots.remove(&key);
            (false, true)
        }
    }
}

fn wake_ephemeral(tx: &mpsc::Sender<()>) -> Result<(), ()> {
    match tx.try_send(()) {
        Ok(()) => Ok(()),
        Err(mpsc::error::TrySendError::Full(_)) => Ok(()),
        Err(mpsc::error::TrySendError::Closed(_)) => Err(()),
    }
}

fn notify_ephemeral_capacity(control: &EphemeralControl) {
    if let Some(notify) = control.capacity_notify.as_ref() {
        // Store a permit so a capacity return between select polls is not lost.
        notify.notify_one();
    }
}

fn materialize_ephemeral_message(
    key: EphemeralKey,
    taken_generation: u64,
    materializer: &EphemeralMaterializer,
) -> Option<ServerMessage> {
    match (key, materializer()) {
        (EphemeralKey::ResourceStream(stream), Some(ServerMessage::Stream(frame)))
            if frame.stream == stream && frame.generation == taken_generation =>
        {
            Some(ServerMessage::Stream(frame))
        }
        (
            EphemeralKey::Conversation(subscription_id),
            Some(ServerMessage::ConversationDirty {
                subscription_id: dirty_id,
                task_id,
                high_water,
            }),
        ) if dirty_id == subscription_id => Some(ServerMessage::ConversationDirty {
            subscription_id,
            task_id,
            high_water,
        }),
        _ => None,
    }
}

/// Ephemeral outbound token: materializes at drain time, frees/requeues on completion.
pub(crate) struct EphemeralOutbound {
    message: Option<ServerMessage>,
    key: EphemeralKey,
    taken_generation: u64,
    taken_dirty_revision: u64,
    control: Arc<Mutex<EphemeralControl>>,
    wake_tx: mpsc::Sender<()>,
    shutdown: watch::Sender<bool>,
    completed: bool,
}

impl EphemeralOutbound {
    pub(crate) fn should_write(&self) -> bool {
        self.message.is_some()
    }

    pub(crate) fn message(&self) -> &ServerMessage {
        self.message
            .as_ref()
            .expect("message() requires should_write")
    }

    fn commit_successful_write(&mut self) {
        self.completed = true;
        self.release_or_requeue();
    }

    fn release_or_requeue(&self) {
        let (requeue, freed) = {
            let mut control = self.control.lock().expect("ephemeral control");
            if control.shutdown {
                control.lane.clear();
                (false, false)
            } else {
                control
                    .lane
                    .finish(self.key, self.taken_generation, self.taken_dirty_revision)
            }
        };
        if freed {
            let control = self.control.lock().expect("ephemeral control");
            notify_ephemeral_capacity(&control);
        }
        if requeue && wake_ephemeral(&self.wake_tx).is_err() {
            let mut control = self.control.lock().expect("ephemeral control");
            control.shutdown = true;
            control.lane.clear();
            let _ = self.shutdown.send_replace(true);
        }
    }
}

impl Drop for EphemeralOutbound {
    fn drop(&mut self) {
        if !self.completed {
            self.release_or_requeue();
        }
    }
}

/// Dual-lane host→client output for one duplex connection.
#[derive(Clone)]
pub(crate) struct ConnectionOutputHandle {
    id: ConnectionOutputId,
    critical_slots: Arc<Semaphore>,
    critical_tx: mpsc::UnboundedSender<CriticalOutbound>,
    durable_tx: mpsc::Sender<DurableOutbound>,
    ephemeral: Arc<Mutex<EphemeralControl>>,
    ephemeral_wake_tx: mpsc::Sender<()>,
    shutdown: watch::Sender<bool>,
}

/// Writer-side receivers for one connection output.
pub(crate) struct ConnectionOutputPorts {
    critical_rx: mpsc::UnboundedReceiver<CriticalOutbound>,
    durable_rx: mpsc::Receiver<DurableOutbound>,
    ephemeral: Arc<Mutex<EphemeralControl>>,
    ephemeral_wake_rx: mpsc::Receiver<()>,
    ephemeral_wake_tx: mpsc::Sender<()>,
    shutdown: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
}

impl ConnectionOutputHandle {
    /// Allocate an output with a generated connection identity (unit tests).
    #[cfg(test)]
    pub(crate) fn new(
        critical_capacity: usize,
        durable_capacity: usize,
        ephemeral_capacity: usize,
    ) -> (Self, ConnectionOutputPorts) {
        Self::with_connection_id(
            ConnectionOutputId::new().as_uuid(),
            critical_capacity,
            durable_capacity,
            ephemeral_capacity,
        )
    }

    /// Allocate an output whose id is the wire `ServerHello.connection_id`.
    pub(crate) fn with_connection_id(
        connection_id: Uuid,
        critical_capacity: usize,
        durable_capacity: usize,
        ephemeral_capacity: usize,
    ) -> (Self, ConnectionOutputPorts) {
        let critical_capacity = critical_capacity.max(1).min(MAX_OUTPUT_LANE_CAPACITY);
        let durable_capacity = durable_capacity.max(1).min(MAX_OUTPUT_LANE_CAPACITY);
        let ephemeral_capacity = ephemeral_capacity.max(1).min(MAX_OUTPUT_LANE_CAPACITY);
        let (critical_tx, critical_rx) = mpsc::unbounded_channel();
        let (durable_tx, durable_rx) = mpsc::channel(durable_capacity);
        let (ephemeral_wake_tx, ephemeral_wake_rx) = mpsc::channel(1);
        let (shutdown, shutdown_rx) = watch::channel(false);
        let ephemeral = Arc::new(Mutex::new(EphemeralControl {
            shutdown: false,
            lane: EphemeralLaneInner {
                capacity: ephemeral_capacity,
                reserved: HashSet::new(),
                slots: HashMap::new(),
                pending: VecDeque::new(),
            },
            capacity_notify: None,
        }));
        let handle = Self {
            id: ConnectionOutputId::from_uuid(connection_id),
            critical_slots: Arc::new(Semaphore::new(critical_capacity)),
            critical_tx,
            durable_tx,
            ephemeral: Arc::clone(&ephemeral),
            ephemeral_wake_tx: ephemeral_wake_tx.clone(),
            shutdown: shutdown.clone(),
        };
        (
            handle,
            ConnectionOutputPorts {
                critical_rx,
                durable_rx,
                ephemeral,
                ephemeral_wake_rx,
                ephemeral_wake_tx,
                shutdown,
                shutdown_rx,
            },
        )
    }

    pub(crate) fn id(&self) -> ConnectionOutputId {
        self.id
    }

    pub(crate) fn subscribe_shutdown(&self) -> watch::Receiver<bool> {
        self.shutdown.subscribe()
    }

    pub(crate) fn is_shutdown_requested(&self) -> bool {
        let control = self.ephemeral.lock().expect("ephemeral control");
        control.shutdown || *self.shutdown.borrow()
    }

    pub(crate) fn request_shutdown(&self) {
        {
            let mut control = self.ephemeral.lock().expect("ephemeral control");
            control.shutdown = true;
            control.lane.clear();
        }
        let _ = self.shutdown.send_replace(true);
        let _ = wake_ephemeral(&self.ephemeral_wake_tx);
    }

    #[cfg(test)]
    pub(crate) fn critical_permits_available(&self) -> usize {
        self.critical_slots.available_permits()
    }

    #[cfg(test)]
    pub(crate) fn ephemeral_slots_occupied(&self) -> usize {
        self.ephemeral
            .lock()
            .expect("ephemeral control")
            .lane
            .occupied()
    }

    #[cfg(test)]
    pub(crate) fn ephemeral_pending_len(&self) -> usize {
        self.ephemeral
            .lock()
            .expect("ephemeral control")
            .lane
            .pending
            .len()
    }

    #[cfg(test)]
    pub(crate) fn registration_guard_for_test(&self) -> ConnectionOutputRegistration {
        ConnectionOutputRegistration {
            id: self.id,
            output: self.clone(),
            control_tx: {
                let (tx, _rx) = mpsc::channel(1);
                tx
            },
        }
    }

    pub(crate) fn try_enqueue_critical(&self, message: ServerMessage) -> Result<(), IpcError> {
        self.try_enqueue_critical_outbound(message, None, false, false)
            .map(|_| ())
    }

    /// Admit a critical message that requests shutdown only after a successful write.
    pub(crate) fn try_enqueue_critical_shutdown_after_write(
        &self,
        message: ServerMessage,
    ) -> Result<(), IpcError> {
        self.try_enqueue_critical_outbound(message, None, true, false)
            .map(|_| ())
    }

    /// Tracked critical admission: returns a [`PhysicalWriteAck`] while preserving
    /// synchronous nonblocking permit/channel admission.
    pub(crate) fn try_enqueue_critical_tracked(
        &self,
        message: ServerMessage,
    ) -> Result<PhysicalWriteAck, IpcError> {
        self.try_enqueue_critical_outbound(message, None, false, true)?
            .ok_or(IpcError::Unavailable)
    }

    fn try_enqueue_critical_outbound(
        &self,
        message: ServerMessage,
        live_resync: Option<LiveResyncMaterialization>,
        shutdown_after_successful_write: bool,
        tracked: bool,
    ) -> Result<Option<PhysicalWriteAck>, IpcError> {
        if self.is_shutdown_requested() {
            return Err(IpcError::Unavailable);
        }
        let permit = self
            .critical_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| {
                self.request_shutdown();
                IpcError::Unavailable
            })?;
        let shutdown_after_successful_write = shutdown_after_successful_write.then(|| self.clone());
        let (ack, write_ack) = if tracked {
            let (ack, acknowledger) = PhysicalWriteAcknowledger::pair();
            (Some(ack), Some(acknowledger))
        } else {
            (None, None)
        };
        self.critical_tx
            .send(CriticalOutbound {
                message,
                _permit: permit,
                live_resync,
                shutdown_after_successful_write,
                write_ack,
            })
            .map_err(|_| {
                self.request_shutdown();
                IpcError::Unavailable
            })?;
        Ok(ack)
    }

    /// Cancel the live stream generation and attempt one critical ResyncRequired.
    ///
    /// The provisional baseline is snapshotted for admission results; the writer
    /// finalizes `last_delivered_sequence` via [`PrioritizedOutbound::prepare_for_write`]
    /// immediately before encoding so an in-flight durable can advance it.
    pub(crate) fn force_live_resync(
        &self,
        subscription_id: SubscriptionId,
        stream: &Arc<LiveStreamState>,
        newest_sequence: u64,
    ) -> DurableAdmitResult {
        if self.is_shutdown_requested() {
            return DurableAdmitResult::ShutdownRequested;
        }
        stream.cancel();
        let last_delivered_sequence = stream.last_physically_written();
        let newest_sequence = newest_sequence.max(last_delivered_sequence);
        let resync = ServerMessage::ResyncRequired {
            subscription_id,
            last_delivered_sequence,
            newest_sequence,
        };
        match self.try_enqueue_critical_outbound(
            resync,
            Some(LiveResyncMaterialization {
                stream: Arc::clone(stream),
                newest_sequence_hint: newest_sequence,
            }),
            false,
            false,
        ) {
            Ok(_) => DurableAdmitResult::ResyncAdmitted {
                last_delivered_sequence,
                newest_sequence,
            },
            Err(_) => DurableAdmitResult::ShutdownRequested,
        }
    }

    pub(crate) fn try_enqueue_durable_event(
        &self,
        subscription_id: SubscriptionId,
        event: crate::domain::event::DomainEvent,
        stream: &Arc<LiveStreamState>,
        newest_sequence: u64,
    ) -> DurableAdmitResult {
        if self.is_shutdown_requested() {
            return DurableAdmitResult::ShutdownRequested;
        }
        let sequence = event.sequence;
        let generation = stream.current_generation();
        let outbound = DurableOutbound {
            message: ServerMessage::DurableEvent {
                subscription_id,
                event,
            },
            stream: Arc::clone(stream),
            generation,
            sequence,
        };
        match self.durable_tx.try_send(outbound) {
            Ok(()) => DurableAdmitResult::Admitted,
            Err(mpsc::error::TrySendError::Full(_)) | Err(mpsc::error::TrySendError::Closed(_)) => {
                self.force_live_resync(subscription_id, stream, newest_sequence)
            }
        }
    }

    pub(crate) fn try_admit_ephemeral_stream(
        &self,
        stream: StreamKey,
        generation: u64,
        materializer: StreamMaterializer,
    ) -> EphemeralAdmitResult {
        self.try_admit_ephemeral(
            EphemeralKey::ResourceStream(stream),
            generation,
            Arc::new(move || materializer().map(ServerMessage::Stream)),
        )
    }

    pub(crate) fn try_admit_ephemeral(
        &self,
        key: EphemeralKey,
        generation: u64,
        materializer: EphemeralMaterializer,
    ) -> EphemeralAdmitResult {
        let mut control = self.ephemeral.lock().expect("ephemeral control");
        if control.shutdown {
            return EphemeralAdmitResult::ShutdownRequested;
        }
        let (result, wake) = control.lane.admit(key, generation, materializer);
        if wake {
            if wake_ephemeral(&self.ephemeral_wake_tx).is_err() {
                control.shutdown = true;
                control.lane.clear();
                drop(control);
                let _ = self.shutdown.send_replace(true);
                return EphemeralAdmitResult::ShutdownRequested;
            }
        }
        result
    }

    pub(crate) fn invalidate_ephemeral_key(&self, key: EphemeralKey) {
        let mut control = self.ephemeral.lock().expect("ephemeral control");
        if control.lane.invalidate_key(key) {
            notify_ephemeral_capacity(&control);
        }
    }

    pub(crate) fn attach_ephemeral_capacity_notify(&self, notify: Arc<Notify>) {
        let mut control = self.ephemeral.lock().expect("ephemeral control");
        control.capacity_notify = Some(notify);
    }

    pub(crate) fn reserve_ephemeral_key(&self, key: EphemeralKey) {
        let mut control = self.ephemeral.lock().expect("ephemeral control");
        control.lane.reserve_key(key);
    }

    pub(crate) fn release_reserved_ephemeral_key(&self, key: EphemeralKey) {
        let mut control = self.ephemeral.lock().expect("ephemeral control");
        if control.lane.release_reserved_key(key) {
            notify_ephemeral_capacity(&control);
        }
    }

    pub(crate) fn try_admit_conversation_dirty(
        &self,
        subscription_id: SubscriptionId,
        task_id: TaskId,
        generation: u64,
        generation_flag: Arc<AtomicU64>,
        high_water: u64,
    ) -> EphemeralAdmitResult {
        let materializer: EphemeralMaterializer = Arc::new(move || {
            if generation_flag.load(Ordering::SeqCst) != generation {
                return None;
            }
            Some(ServerMessage::ConversationDirty {
                subscription_id,
                task_id,
                high_water,
            })
        });
        self.try_admit_ephemeral(
            EphemeralKey::Conversation(subscription_id),
            generation,
            materializer,
        )
    }
}

impl ConnectionOutputPorts {
    fn shutdown_requested(&self) -> bool {
        let control = self.ephemeral.lock().expect("ephemeral control");
        control.shutdown || *self.shutdown_rx.borrow()
    }

    fn take_ephemeral_outbound(&mut self) -> Option<EphemeralOutbound> {
        let (key, taken_generation, taken_dirty_revision, materializer) = {
            let mut control = self.ephemeral.lock().expect("ephemeral control");
            if control.shutdown {
                return None;
            }
            control.lane.take_pending()?
        };
        let message = materialize_ephemeral_message(key, taken_generation, &materializer);
        Some(EphemeralOutbound {
            message,
            key,
            taken_generation,
            taken_dirty_revision,
            control: Arc::clone(&self.ephemeral),
            wake_tx: self.ephemeral_wake_tx.clone(),
            shutdown: self.shutdown.clone(),
            completed: false,
        })
    }

    /// Prefer critical, then durable, then ephemeral; never blocks.
    #[cfg(test)]
    pub(crate) fn try_recv_prioritized(&mut self) -> Option<PrioritizedOutbound> {
        if let Ok(outbound) = self.critical_rx.try_recv() {
            return Some(PrioritizedOutbound::Critical(outbound));
        }
        if let Ok(outbound) = self.durable_rx.try_recv() {
            return Some(PrioritizedOutbound::Durable(outbound));
        }
        self.take_ephemeral_outbound()
            .map(PrioritizedOutbound::Ephemeral)
    }

    /// Count coalesced wake tokens without permanently consuming them.
    #[cfg(test)]
    pub(crate) fn ephemeral_wake_pending_count(&mut self) -> usize {
        match self.ephemeral_wake_rx.try_recv() {
            Ok(()) => {
                let _ = self.ephemeral_wake_tx.try_send(());
                1
            }
            Err(_) => 0,
        }
    }

    /// Blocking receive that prefers critical then durable then ephemeral.
    pub(crate) async fn recv_prioritized(&mut self) -> Option<PrioritizedOutbound> {
        loop {
            if self.shutdown_requested() {
                return None;
            }
            if let Ok(outbound) = self.critical_rx.try_recv() {
                return Some(PrioritizedOutbound::Critical(outbound));
            }
            if let Ok(outbound) = self.durable_rx.try_recv() {
                return Some(PrioritizedOutbound::Durable(outbound));
            }
            if let Some(outbound) = self.take_ephemeral_outbound() {
                return Some(PrioritizedOutbound::Ephemeral(outbound));
            }
            tokio::select! {
                biased;
                changed = self.shutdown_rx.changed() => {
                    if changed.is_err() || self.shutdown_requested() {
                        return None;
                    }
                }
                critical = self.critical_rx.recv() => {
                    return critical.map(PrioritizedOutbound::Critical);
                }
                durable = self.durable_rx.recv() => {
                    return durable.map(PrioritizedOutbound::Durable);
                }
                wake = self.ephemeral_wake_rx.recv() => {
                    if wake.is_none() {
                        return None;
                    }
                }
            }
        }
    }

    /// Debug-only: drain critical traffic only; never consume durable or ephemeral.
    #[cfg(debug_assertions)]
    pub(crate) async fn recv_critical_only(&mut self) -> Option<PrioritizedOutbound> {
        loop {
            if self.shutdown_requested() {
                return None;
            }
            if let Ok(outbound) = self.critical_rx.try_recv() {
                return Some(PrioritizedOutbound::Critical(outbound));
            }
            tokio::select! {
                biased;
                changed = self.shutdown_rx.changed() => {
                    if changed.is_err() || self.shutdown_requested() {
                        return None;
                    }
                }
                critical = self.critical_rx.recv() => {
                    return critical.map(PrioritizedOutbound::Critical);
                }
            }
        }
    }
}

#[cfg(test)]
mod output_tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use super::{
        accepted_shell_terminal_effect, provider_restore_success_attention,
        provider_start_requires_existing_binding, AcceptedShellEffect, ConnectionOutputHandle,
        ConnectionOutputId, DuplexExecuteCompletion, DurableAdmitResult, EphemeralAdmitResult,
        EphemeralKey, EventReplayRegistry, HostRequestExecutor, HostRequestHandle, LiveStreamState,
        LiveTail, PhysicalWriteAckStatus, PrioritizedOutbound, ShellSessionLink,
        StreamMaterializer, MAX_CONCURRENT_PROVIDER_RESTORES,
    };
    use crate::domain::cockpit::{TaskCockpitQuery, TaskCockpitResult};
    use crate::domain::command::{Command, CommandEnvelope, CreateTaskIntent};
    use crate::domain::event::{DomainEvent, Event};
    use crate::domain::id::{
        CommandId, EnvironmentId, EventId, ProjectId, RequestId, ResourceId, SubscriptionId, TaskId,
    };
    use crate::domain::query::{QueryOutcome, QueryReply};
    use crate::domain::resource::{
        OwnerKind, ResourceFacts, ResourceKind, ResourceLifecycle, ResourceRecipe,
    };
    use crate::domain::snapshot::PageLimits;
    use crate::domain::task::{
        ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity,
        WorkspaceRef,
    };
    use crate::domain::terminal_facts::{TERMINAL_ACTIVITY_COALESCE_MS, TERMINAL_CWD_DEBOUNCE_MS};
    use crate::domain::ClientId;
    use crate::kernel::SessionScope;
    use crate::protocol::{
        Capability, CapabilitySet, ClientRequest, NegotiatedParameters, ServerMessage, StreamFrame,
        StreamKey, StreamPayloadKind,
    };
    use std::sync::atomic::AtomicU64;
    use std::sync::{Arc, Mutex};
    use tokio::sync::Notify;
    use uuid::Uuid;

    #[test]
    fn successful_provider_restore_clears_only_stale_failed_attention() {
        assert_eq!(
            provider_restore_success_attention(TaskAttention::Failed),
            Some(TaskAttention::None)
        );
        assert_eq!(
            provider_restore_success_attention(TaskAttention::NeedsAnswer),
            None
        );
        assert_eq!(
            provider_restore_success_attention(TaskAttention::None),
            None
        );
    }

    fn sample_event(sequence: u64) -> DomainEvent {
        DomainEvent {
            id: EventId::new(),
            task_id: None,
            sequence,
            task_revision: None,
            occurred_at_ms: 1_725_000_000_000,
            payload: Event::TaskRenamed {
                title: format!("seq-{sequence}"),
            },
        }
    }

    fn sample_reply() -> ServerMessage {
        ServerMessage::QueryReply(QueryReply {
            request_id: RequestId::new(),
            outcome: QueryOutcome::Err(crate::domain::query::QueryError::NotFound),
        })
    }

    fn test_repository_root() -> PathBuf {
        let current = std::env::current_dir().expect("test current directory");
        current
            .ancestors()
            .find(|candidate| candidate.join(".git").is_dir())
            .unwrap_or(current.as_path())
            .to_path_buf()
    }

    #[test]
    fn output_lane_capacities_are_clamped_before_channel_allocation() {
        let (handle, _ports) = ConnectionOutputHandle::with_connection_id(
            Uuid::now_v7(),
            usize::MAX,
            usize::MAX,
            usize::MAX,
        );

        assert!(handle.critical_permits_available() <= super::MAX_OUTPUT_LANE_CAPACITY);
    }

    #[cfg(debug_assertions)]
    #[tokio::test]
    async fn critical_only_receiver_never_consumes_durable_or_ephemeral() {
        let subscription_id = SubscriptionId::new();
        let stream = LiveStreamState::new(0);
        let (handle, mut ports) = ConnectionOutputHandle::new(2, 1, 1);

        assert!(matches!(
            handle.try_enqueue_durable_event(subscription_id, sample_event(1), &stream, 1),
            DurableAdmitResult::Admitted
        ));
        let ephemeral_stream = sample_stream_key(0x70);
        assert!(matches!(
            handle.try_admit_ephemeral_stream(
                ephemeral_stream,
                1,
                Arc::new(move || Some(sample_stream_frame(ephemeral_stream, 1, 1, 7))),
            ),
            EphemeralAdmitResult::Queued | EphemeralAdmitResult::Coalesced
        ));
        handle
            .try_enqueue_critical(sample_reply())
            .expect("critical must admit while durable and ephemeral are held");

        let first = tokio::time::timeout(Duration::from_secs(1), ports.recv_critical_only())
            .await
            .expect("critical-only recv stayed bounded")
            .expect("critical outbound");
        assert!(matches!(first, PrioritizedOutbound::Critical(_)));

        let still_waiting =
            tokio::time::timeout(Duration::from_millis(50), ports.recv_critical_only()).await;
        assert!(
            still_waiting.is_err(),
            "critical-only must not surface durable or ephemeral while waiting"
        );
        let durable = ports
            .try_recv_prioritized()
            .expect("durable must remain queued");
        assert!(matches!(durable, PrioritizedOutbound::Durable(_)));
        let ephemeral = ports
            .try_recv_prioritized()
            .expect("ephemeral must remain queued");
        assert!(matches!(ephemeral, PrioritizedOutbound::Ephemeral(_)));
    }

    #[cfg(debug_assertions)]
    #[tokio::test]
    async fn critical_only_receiver_completes_none_on_shutdown() {
        let (handle, mut ports) = ConnectionOutputHandle::new(1, 1, 1);
        let pending = tokio::spawn(async move { ports.recv_critical_only().await });
        // Yield until the spawned receive is pending on critical/shutdown.
        for _ in 0..16 {
            if pending.is_finished() {
                break;
            }
            tokio::task::yield_now().await;
        }
        handle.request_shutdown();
        let outcome = tokio::time::timeout(Duration::from_secs(1), pending)
            .await
            .expect("critical-only shutdown wakeup stayed bounded")
            .expect("critical-only shutdown join");
        assert!(
            outcome.is_none(),
            "shutdown must complete critical-only receive with None"
        );
    }

    #[test]
    fn full_durable_lane_preserves_critical_admission_resync_and_connection_local_shutdown() {
        let subscription_id = SubscriptionId::new();
        let stream = LiveStreamState::new(0);
        // Two critical slots: one remains held as RAII through a simulated write
        // while overflow resync still admits on the second slot.
        let (alpha, mut alpha_ports) = ConnectionOutputHandle::new(2, 1, 1);
        let (beta, mut beta_ports) = ConnectionOutputHandle::new(1, 1, 1);

        assert!(matches!(
            alpha.try_enqueue_durable_event(subscription_id, sample_event(1), &stream, 2),
            DurableAdmitResult::Admitted
        ));

        alpha
            .try_enqueue_critical(sample_reply())
            .expect("full durable must not consume or block critical admission");
        let held = alpha_ports
            .try_recv_prioritized()
            .expect("critical reply must be dequeued as RAII outbound");
        assert!(matches!(held.message(), ServerMessage::QueryReply(_)));
        assert_eq!(
            alpha.critical_permits_available(),
            1,
            "held RAII outbound must keep its slot until dropped after write completion"
        );

        assert!(matches!(
            alpha.try_enqueue_durable_event(subscription_id, sample_event(2), &stream, 2),
            DurableAdmitResult::ResyncAdmitted {
                last_delivered_sequence: 0,
                newest_sequence: 2,
            }
        ));
        assert_eq!(alpha.critical_permits_available(), 0);

        assert!(
            alpha.try_enqueue_critical(sample_reply()).is_err(),
            "critical exhaustion must fail closed for this connection"
        );
        assert!(
            alpha.is_shutdown_requested(),
            "critical exhaustion must request only this connection's shutdown"
        );
        drop(held);

        let mut saw_resync = false;
        let mut saw_stale_durable = false;
        while let Some(outbound) = alpha_ports.try_recv_prioritized() {
            match outbound {
                PrioritizedOutbound::Critical(critical) => match &critical.message {
                    ServerMessage::ResyncRequired {
                        subscription_id: got_sub,
                        last_delivered_sequence,
                        newest_sequence,
                    } => {
                        assert_eq!(*got_sub, subscription_id);
                        assert_eq!(*last_delivered_sequence, 0);
                        assert_eq!(*newest_sequence, 2);
                        saw_resync = true;
                    }
                    other => panic!("expected ResyncRequired, got {other:?}"),
                },
                PrioritizedOutbound::Durable(durable) => {
                    assert!(!durable.is_current());
                    saw_stale_durable = true;
                }
                PrioritizedOutbound::Ephemeral(_) => {
                    panic!("unexpected ephemeral outbound in durable/critical test")
                }
            }
        }
        assert!(saw_resync);
        assert!(saw_stale_durable);

        beta.try_enqueue_critical(sample_reply())
            .expect("peer connection must remain writable");
        assert!(!beta.is_shutdown_requested());
        assert!(beta_ports.try_recv_prioritized().is_some());
    }

    #[test]
    fn durable_overflow_resync_uses_physical_baseline_and_suppresses_queued_event() {
        let subscription_id = SubscriptionId::new();
        let stream = LiveStreamState::new(10);
        let (handle, mut ports) = ConnectionOutputHandle::new(1, 1, 1);

        assert!(matches!(
            handle.try_enqueue_durable_event(subscription_id, sample_event(11), &stream, 12),
            DurableAdmitResult::Admitted
        ));
        assert!(matches!(
            handle.try_enqueue_durable_event(subscription_id, sample_event(12), &stream, 12),
            DurableAdmitResult::ResyncAdmitted {
                last_delivered_sequence: 10,
                newest_sequence: 12,
            }
        ));

        let mut saw_stale = false;
        let mut saw_resync = false;
        while let Some(outbound) = ports.try_recv_prioritized() {
            match outbound {
                PrioritizedOutbound::Durable(durable) => {
                    assert!(
                        !durable.is_current(),
                        "queued durable must be suppressed after resync cancel"
                    );
                    saw_stale = true;
                }
                PrioritizedOutbound::Critical(critical) => match &critical.message {
                    ServerMessage::ResyncRequired {
                        last_delivered_sequence,
                        newest_sequence,
                        ..
                    } => {
                        assert_eq!(*last_delivered_sequence, 10);
                        assert_eq!(*newest_sequence, 12);
                        saw_resync = true;
                    }
                    other => panic!("unexpected critical {other:?}"),
                },
                PrioritizedOutbound::Ephemeral(_) => {
                    panic!("unexpected ephemeral outbound in durable resync test")
                }
            }
        }
        assert!(
            saw_resync,
            "priority resync must be present on critical lane"
        );
        assert!(
            saw_stale,
            "stale queued durable must still be observable as cancelled"
        );
        assert_eq!(stream.last_physically_written(), 10);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_wakes_idle_output_receive_without_sleep() {
        let (handle, mut ports) = ConnectionOutputHandle::new(1, 1, 1);
        let recv = tokio::spawn(async move { ports.recv_prioritized().await });
        tokio::task::yield_now().await;
        handle.request_shutdown();
        let result = tokio::time::timeout(Duration::from_millis(200), recv)
            .await
            .expect("shutdown must wake promptly")
            .expect("join");
        assert!(result.is_none());
    }

    #[test]
    fn shutdown_send_replace_retains_when_receivers_already_dropped() {
        let (handle, ports) = ConnectionOutputHandle::new(1, 1, 1);
        drop(ports);
        handle.request_shutdown();
        assert!(
            handle.is_shutdown_requested(),
            "send_replace must retain shutdown for executor reaper observation"
        );
    }

    #[test]
    fn force_live_resync_cancels_queued_frames_and_reports_physical_baseline() {
        let subscription_id = SubscriptionId::new();
        let stream = LiveStreamState::new(3);
        let (handle, mut ports) = ConnectionOutputHandle::new(1, 1, 1);
        assert!(matches!(
            handle.try_enqueue_durable_event(subscription_id, sample_event(4), &stream, 9),
            DurableAdmitResult::Admitted
        ));
        assert!(matches!(
            handle.force_live_resync(subscription_id, &stream, 9),
            DurableAdmitResult::ResyncAdmitted {
                last_delivered_sequence: 3,
                newest_sequence: 9,
            }
        ));

        let mut saw_stale = false;
        let mut saw_resync = false;
        while let Some(outbound) = ports.try_recv_prioritized() {
            match outbound {
                PrioritizedOutbound::Durable(durable) => {
                    assert!(!durable.is_current());
                    saw_stale = true;
                }
                PrioritizedOutbound::Critical(critical) => match &critical.message {
                    ServerMessage::ResyncRequired {
                        last_delivered_sequence,
                        newest_sequence,
                        ..
                    } => {
                        assert_eq!(*last_delivered_sequence, 3);
                        assert_eq!(*newest_sequence, 9);
                        saw_resync = true;
                    }
                    other => panic!("unexpected critical {other:?}"),
                },
                PrioritizedOutbound::Ephemeral(_) => {
                    panic!("unexpected ephemeral outbound in force_live_resync test")
                }
            }
        }
        assert!(saw_resync);
        assert!(saw_stale);
    }

    #[test]
    fn in_flight_durable_write_advances_prepared_resync_baseline() {
        let subscription_id = SubscriptionId::new();
        let stream = LiveStreamState::new(3);
        let (handle, mut ports) = ConnectionOutputHandle::new(1, 1, 1);

        assert!(matches!(
            handle.try_enqueue_durable_event(subscription_id, sample_event(4), &stream, 9),
            DurableAdmitResult::Admitted
        ));
        let in_flight = ports
            .try_recv_prioritized()
            .expect("dequeue durable as if write already started");
        let PrioritizedOutbound::Durable(durable) = in_flight else {
            panic!("expected durable outbound held in flight");
        };
        assert!(durable.is_current());

        assert!(matches!(
            handle.force_live_resync(subscription_id, &stream, 9),
            DurableAdmitResult::ResyncAdmitted {
                last_delivered_sequence: 3,
                newest_sequence: 9,
            }
        ));
        assert!(
            !durable.is_current(),
            "resync cancel must bump generation while durable is in flight"
        );

        // Physical write completed after cancel raced mid-flight.
        PrioritizedOutbound::Durable(durable).after_successful_write();
        assert_eq!(stream.last_physically_written(), 4);

        let mut resync = ports
            .try_recv_prioritized()
            .expect("critical resync must remain queued");
        resync.prepare_for_write();
        match resync.message() {
            ServerMessage::ResyncRequired {
                last_delivered_sequence,
                newest_sequence,
                ..
            } => {
                assert_eq!(*last_delivered_sequence, 4);
                assert_eq!(*newest_sequence, 9);
            }
            other => panic!("expected prepared ResyncRequired, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn live_stream_wait_until_physically_written_returns_immediately_when_already_advanced() {
        let stream = LiveStreamState::new(10);
        assert_eq!(stream.last_physically_written(), 10);
        tokio::time::timeout(
            Duration::from_millis(50),
            stream.wait_until_physically_written(10),
        )
        .await
        .expect("already-advanced wait must complete without blocking");
        tokio::time::timeout(
            Duration::from_millis(50),
            stream.wait_until_physically_written(7),
        )
        .await
        .expect("lower target must also complete immediately");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn live_stream_wait_until_physically_written_observes_progress_without_lost_wakeup() {
        let stream = LiveStreamState::new(1);
        let waiter_stream = Arc::clone(&stream);
        let waiter = tokio::spawn(async move {
            waiter_stream.wait_until_physically_written(5).await;
        });
        // Yield so the waiter can register Notified::enable before progress.
        tokio::task::yield_now().await;
        stream.record_physical_write(3);
        assert_eq!(stream.last_physically_written(), 3);
        assert!(
            !waiter.is_finished(),
            "waiter must remain pending below target"
        );
        stream.record_physical_write(5);
        tokio::time::timeout(Duration::from_millis(200), waiter)
            .await
            .expect("progress notify must wake waiter promptly")
            .expect("join");
        assert_eq!(stream.last_physically_written(), 5);

        // Stale / equal sequences must not notify (high-water does not advance).
        let stalled = Arc::clone(&stream);
        let stalled_waiter = tokio::spawn(async move {
            stalled.wait_until_physically_written(6).await;
        });
        tokio::task::yield_now().await;
        stream.record_physical_write(4);
        stream.record_physical_write(5);
        assert!(
            !stalled_waiter.is_finished(),
            "non-advancing writes must not wake a higher target waiter"
        );
        stalled_waiter.abort();
    }

    fn sample_stream_key(tail: u8) -> StreamKey {
        StreamKey::from(
            ResourceId::from_bytes([
                0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, tail,
            ])
            .expect("resource"),
        )
    }

    fn sample_stream_frame(
        stream: StreamKey,
        generation: u64,
        sequence: u64,
        marker: u8,
    ) -> StreamFrame {
        StreamFrame {
            subscription_id: SubscriptionId::new(),
            stream,
            generation,
            sequence,
            payload_kind: StreamPayloadKind::new(1).expect("kind"),
            schema_version: 1,
            payload: vec![marker],
        }
    }

    #[test]
    fn ephemeral_many_dirty_notifications_occupy_one_slot_and_first_drain_materializes_latest() {
        // Catches: repeated dirtiness for one stream must coalesce to a single
        // queued/in-flight slot and materialize only the latest state on drain.
        let stream = sample_stream_key(0x01);
        let (handle, mut ports) = ConnectionOutputHandle::new(1, 1, 1);
        let markers = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0));
        for marker in [1u8, 2, 3, 4, 5] {
            markers.store(marker, std::sync::atomic::Ordering::SeqCst);
            let markers = Arc::clone(&markers);
            let materializer: StreamMaterializer = Arc::new(move || {
                Some(sample_stream_frame(
                    stream,
                    1,
                    u64::from(markers.load(std::sync::atomic::Ordering::SeqCst)),
                    markers.load(std::sync::atomic::Ordering::SeqCst),
                ))
            });
            let result = handle.try_admit_ephemeral_stream(stream, 1, materializer);
            if marker == 1 {
                assert!(matches!(result, EphemeralAdmitResult::Queued));
            } else {
                assert!(matches!(result, EphemeralAdmitResult::Coalesced));
            }
        }
        assert_eq!(handle.ephemeral_slots_occupied(), 1);

        let outbound = ports
            .try_recv_prioritized()
            .expect("one coalesced ephemeral token must drain");
        let PrioritizedOutbound::Ephemeral(ephemeral) = outbound else {
            panic!("expected ephemeral outbound");
        };
        assert!(ephemeral.should_write());
        match ephemeral.message() {
            ServerMessage::Stream(frame) => {
                assert_eq!(frame.stream, stream);
                assert_eq!(frame.generation, 1);
                assert_eq!(frame.payload, vec![5]);
            }
            other => panic!("expected Stream frame, got {other:?}"),
        }
        assert!(ports.try_recv_prioritized().is_none());
        PrioritizedOutbound::Ephemeral(ephemeral).after_successful_write();
        assert_eq!(handle.ephemeral_slots_occupied(), 0);
    }

    #[test]
    fn ephemeral_dirty_during_in_flight_write_requeues_exactly_once_for_new_state() {
        // Catches: dirtiness while a materialized frame is in flight must requeue
        // exactly one token after successful write so the next drain regenerates.
        let stream = sample_stream_key(0x02);
        let (handle, mut ports) = ConnectionOutputHandle::new(1, 1, 1);
        let markers = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(1));
        let markers_admit = Arc::clone(&markers);
        assert!(matches!(
            handle.try_admit_ephemeral_stream(
                stream,
                1,
                Arc::new(move || {
                    Some(sample_stream_frame(
                        stream,
                        1,
                        1,
                        markers_admit.load(std::sync::atomic::Ordering::SeqCst),
                    ))
                }),
            ),
            EphemeralAdmitResult::Queued
        ));
        let first = ports.try_recv_prioritized().expect("first ephemeral drain");
        let PrioritizedOutbound::Ephemeral(first) = first else {
            panic!("expected ephemeral");
        };
        assert!(
            matches!(first.message(), ServerMessage::Stream(frame) if frame.payload == vec![1])
        );

        markers.store(9, std::sync::atomic::Ordering::SeqCst);
        let markers_second = Arc::clone(&markers);
        assert!(matches!(
            handle.try_admit_ephemeral_stream(
                stream,
                1,
                Arc::new(move || {
                    Some(sample_stream_frame(
                        stream,
                        1,
                        2,
                        markers_second.load(std::sync::atomic::Ordering::SeqCst),
                    ))
                }),
            ),
            EphemeralAdmitResult::Coalesced
        ));
        assert!(
            ports.try_recv_prioritized().is_none(),
            "in-flight stream must not queue a second token before write completion"
        );
        assert_eq!(handle.ephemeral_slots_occupied(), 1);

        PrioritizedOutbound::Ephemeral(first).after_successful_write();
        let second = ports
            .try_recv_prioritized()
            .expect("exactly one requeue after in-flight dirtiness");
        let PrioritizedOutbound::Ephemeral(second) = second else {
            panic!("expected ephemeral requeue");
        };
        assert!(
            matches!(second.message(), ServerMessage::Stream(frame) if frame.payload == vec![9])
        );
        assert!(ports.try_recv_prioritized().is_none());
        PrioritizedOutbound::Ephemeral(second).after_successful_write();
        assert_eq!(handle.ephemeral_slots_occupied(), 0);
    }

    #[test]
    fn ephemeral_capacity_drops_overflow_without_blocking_critical_or_durable() {
        // Catches: distinct-stream capacity is hard-bounded; overflow drops only
        // ephemeral work while critical/durable remain admissible and prioritized.
        let (handle, mut ports) = ConnectionOutputHandle::new(1, 1, 1);
        let stream_a = sample_stream_key(0x10);
        let stream_b = sample_stream_key(0x11);
        assert!(matches!(
            handle.try_admit_ephemeral_stream(
                stream_a,
                1,
                Arc::new(move || Some(sample_stream_frame(stream_a, 1, 1, 1))),
            ),
            EphemeralAdmitResult::Queued
        ));
        assert!(matches!(
            handle.try_admit_ephemeral_stream(
                stream_b,
                1,
                Arc::new(move || Some(sample_stream_frame(stream_b, 1, 1, 2))),
            ),
            EphemeralAdmitResult::CapacityDrop
        ));
        assert_eq!(handle.ephemeral_slots_occupied(), 1);
        assert!(!handle.is_shutdown_requested());

        let stream = LiveStreamState::new(0);
        let subscription_id = SubscriptionId::new();
        assert!(matches!(
            handle.try_enqueue_durable_event(subscription_id, sample_event(1), &stream, 1),
            DurableAdmitResult::Admitted
        ));
        handle
            .try_enqueue_critical(sample_reply())
            .expect("ephemeral capacity must not consume critical/durable capacity");

        let first = ports.try_recv_prioritized().expect("critical first");
        assert!(matches!(first, PrioritizedOutbound::Critical(_)));
        let second = ports.try_recv_prioritized().expect("durable second");
        assert!(matches!(second, PrioritizedOutbound::Durable(_)));
        let third = ports.try_recv_prioritized().expect("ephemeral third");
        assert!(matches!(third, PrioritizedOutbound::Ephemeral(_)));
    }

    #[test]
    fn ephemeral_stale_generation_cannot_replace_newer_source() {
        // Catches: a lower generation must be rejected as stale and must not
        // replace a newer materializer/generation already occupying the slot.
        let stream = sample_stream_key(0x20);
        let (handle, mut ports) = ConnectionOutputHandle::new(1, 1, 1);
        assert!(matches!(
            handle.try_admit_ephemeral_stream(
                stream,
                5,
                Arc::new(move || Some(sample_stream_frame(stream, 5, 1, 5))),
            ),
            EphemeralAdmitResult::Queued
        ));
        assert!(matches!(
            handle.try_admit_ephemeral_stream(
                stream,
                4,
                Arc::new(move || Some(sample_stream_frame(stream, 4, 1, 4))),
            ),
            EphemeralAdmitResult::StaleGeneration
        ));
        let outbound = ports.try_recv_prioritized().expect("drain");
        let PrioritizedOutbound::Ephemeral(ephemeral) = outbound else {
            panic!("expected ephemeral");
        };
        match ephemeral.message() {
            ServerMessage::Stream(frame) => {
                assert_eq!(frame.generation, 5);
                assert_eq!(frame.payload, vec![5]);
            }
            other => panic!("expected stream, got {other:?}"),
        }
    }

    #[test]
    fn ephemeral_none_or_mismatched_materialization_emits_nothing_and_frees_capacity() {
        // Catches: None/mismatched/stale materialization must emit no frame,
        // must not busy-loop, and must release capacity consistently.
        let stream = sample_stream_key(0x30);
        let other = sample_stream_key(0x31);
        let (handle, mut ports) = ConnectionOutputHandle::new(1, 1, 2);

        assert!(matches!(
            handle.try_admit_ephemeral_stream(stream, 1, Arc::new(|| None)),
            EphemeralAdmitResult::Queued
        ));
        let none_out = ports.try_recv_prioritized().expect("drain none token");
        let PrioritizedOutbound::Ephemeral(none_out) = none_out else {
            panic!("expected ephemeral");
        };
        assert!(!none_out.should_write());
        drop(none_out);
        assert_eq!(handle.ephemeral_slots_occupied(), 0);
        assert!(ports.try_recv_prioritized().is_none());

        assert!(matches!(
            handle.try_admit_ephemeral_stream(
                stream,
                2,
                Arc::new(move || Some(sample_stream_frame(other, 2, 1, 1))),
            ),
            EphemeralAdmitResult::Queued
        ));
        let mismatch = ports.try_recv_prioritized().expect("drain mismatch");
        let PrioritizedOutbound::Ephemeral(mismatch) = mismatch else {
            panic!("expected ephemeral");
        };
        assert!(!mismatch.should_write());
        drop(mismatch);
        assert_eq!(handle.ephemeral_slots_occupied(), 0);

        assert!(matches!(
            handle.try_admit_ephemeral_stream(
                stream,
                3,
                Arc::new(move || Some(sample_stream_frame(stream, 99, 1, 1))),
            ),
            EphemeralAdmitResult::Queued
        ));
        let stale = ports.try_recv_prioritized().expect("drain stale frame");
        let PrioritizedOutbound::Ephemeral(stale) = stale else {
            panic!("expected ephemeral");
        };
        assert!(!stale.should_write());
        drop(stale);
        assert_eq!(handle.ephemeral_slots_occupied(), 0);
        assert!(ports.try_recv_prioritized().is_none());
    }

    #[test]
    fn ephemeral_wake_notification_stays_one_slot_under_repeated_admit_drain() {
        // Catches: unbounded ephemeral wake tokens must not accumulate across
        // repeated admissions and eager try_recv drains.
        let (handle, mut ports) = ConnectionOutputHandle::new(1, 1, 8);
        for tail in 0..8u8 {
            let stream = sample_stream_key(tail);
            assert!(matches!(
                handle.try_admit_ephemeral_stream(
                    stream,
                    1,
                    Arc::new(move || Some(sample_stream_frame(stream, 1, 1, tail))),
                ),
                EphemeralAdmitResult::Queued
            ));
        }
        let wake_pending = ports.ephemeral_wake_pending_count();
        assert!(
            wake_pending <= 1,
            "wake notifications must coalesce to at most one pending token, got {wake_pending}"
        );
        for _ in 0..8 {
            let outbound = ports
                .try_recv_prioritized()
                .expect("drain queued ephemeral");
            let PrioritizedOutbound::Ephemeral(ephemeral) = outbound else {
                panic!("expected ephemeral");
            };
            PrioritizedOutbound::Ephemeral(ephemeral).after_successful_write();
        }
        for _ in 0..64 {
            let stream = sample_stream_key(0x40);
            let _ = handle.try_admit_ephemeral_stream(
                stream,
                2,
                Arc::new(move || Some(sample_stream_frame(stream, 2, 1, 7))),
            );
        }
        assert!(handle.ephemeral_slots_occupied() <= 8);
        assert!(handle.ephemeral_pending_len() <= 8);
        let wake_pending = ports.ephemeral_wake_pending_count();
        assert!(
            wake_pending <= 1,
            "repeated coalesce admits must not grow the wake queue, got {wake_pending}"
        );
        while let Some(outbound) = ports.try_recv_prioritized() {
            if let PrioritizedOutbound::Ephemeral(ephemeral) = outbound {
                PrioritizedOutbound::Ephemeral(ephemeral).after_successful_write();
            }
        }
        assert_eq!(handle.ephemeral_slots_occupied(), 0);
        assert_eq!(handle.ephemeral_pending_len(), 0);
        assert!(ports.ephemeral_wake_pending_count() <= 1);
    }

    #[test]
    fn ephemeral_shutdown_linearizes_admission_and_clears_slots() {
        // Catches: shutdown and ephemeral admission must share one sync point so
        // post-shutdown admits return ShutdownRequested and leave zero slots.
        let (handle, _ports) = ConnectionOutputHandle::new(1, 1, 2);
        let stream = sample_stream_key(0x50);
        assert!(matches!(
            handle.try_admit_ephemeral_stream(
                stream,
                1,
                Arc::new(move || Some(sample_stream_frame(stream, 1, 1, 1))),
            ),
            EphemeralAdmitResult::Queued
        ));
        assert_eq!(handle.ephemeral_slots_occupied(), 1);

        let registration = handle.registration_guard_for_test();
        drop(registration);

        assert!(
            handle.is_shutdown_requested(),
            "registration drop must request synchronized shutdown"
        );
        assert_eq!(
            handle.ephemeral_slots_occupied(),
            0,
            "shutdown must clear ephemeral slots/pending"
        );
        assert_eq!(handle.ephemeral_pending_len(), 0);
        assert!(matches!(
            handle.try_admit_ephemeral_stream(
                sample_stream_key(0x51),
                1,
                Arc::new(|| Some(sample_stream_frame(sample_stream_key(0x51), 1, 1, 2))),
            ),
            EphemeralAdmitResult::ShutdownRequested
        ));
        assert_eq!(handle.ephemeral_slots_occupied(), 0);
    }

    #[test]
    fn ephemeral_closed_wake_on_in_flight_dirty_completion_requests_shutdown() {
        // Catches: finish-requeue after dirty in-flight must not strand a slot when
        // the wake receiver is already closed; Closed wake must synchronize shutdown.
        let (handle, mut ports) = ConnectionOutputHandle::new(1, 1, 1);
        let stream = sample_stream_key(0x60);
        assert!(matches!(
            handle.try_admit_ephemeral_stream(
                stream,
                1,
                Arc::new(move || Some(sample_stream_frame(stream, 1, 1, 1))),
            ),
            EphemeralAdmitResult::Queued
        ));
        let in_flight = ports
            .try_recv_prioritized()
            .expect("drain token into in-flight");
        let PrioritizedOutbound::Ephemeral(in_flight) = in_flight else {
            panic!("expected ephemeral");
        };
        assert!(matches!(
            handle.try_admit_ephemeral_stream(
                stream,
                1,
                Arc::new(move || Some(sample_stream_frame(stream, 1, 2, 2))),
            ),
            EphemeralAdmitResult::Coalesced
        ));
        drop(ports);

        PrioritizedOutbound::Ephemeral(in_flight).after_successful_write();

        assert!(
            handle.is_shutdown_requested(),
            "closed wake after requeue must request synchronized shutdown"
        );
        assert_eq!(handle.ephemeral_slots_occupied(), 0);
        assert_eq!(handle.ephemeral_pending_len(), 0);
        assert!(matches!(
            handle.try_admit_ephemeral_stream(
                sample_stream_key(0x61),
                1,
                Arc::new(|| Some(sample_stream_frame(sample_stream_key(0x61), 1, 1, 3))),
            ),
            EphemeralAdmitResult::ShutdownRequested
        ));
    }

    #[test]
    fn replay_error_newest_hint_uses_error_fields_and_admitted_floor() {
        use super::newest_sequence_hint_from_replay_error;
        use crate::kernel::ReplayError;

        assert_eq!(
            newest_sequence_hint_from_replay_error(
                &ReplayError::ReplayUnavailable {
                    oldest_sequence: 2,
                    newest_sequence: 11,
                },
                5,
                4
            ),
            11
        );
        assert_eq!(
            newest_sequence_hint_from_replay_error(
                &ReplayError::InvalidRange {
                    after_sequence: 20,
                    through_sequence: 7,
                },
                5,
                4
            ),
            7
        );
        assert_eq!(
            newest_sequence_hint_from_replay_error(
                &ReplayError::PageItemTooLarge {
                    sequence: 6,
                    encoded_bytes: 100,
                    max_encoded_bytes: 50,
                },
                5,
                4
            ),
            6
        );
        assert_eq!(
            newest_sequence_hint_from_replay_error(&ReplayError::InvalidCursor, 5, 8),
            8
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn detach_removes_exact_output_and_live_binding_before_ack_shutdown() {
        use super::{ConnectionOutputId, HostRequestExecutor, OutputInspection};
        use crate::domain::command::{Command, CommandEnvelope, CreateTaskRequestIntent};
        use crate::domain::id::{CommandId, EnvironmentId, ProjectId, RequestId, TaskId};
        use crate::domain::query::{Query, QueryEnvelope};
        use crate::domain::task::{
            ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity,
        };
        use crate::domain::ClientId;
        use crate::kernel::CommandBus;
        use crate::protocol::{
            Capability, CapabilitySet, ClientRequest, DetachRequest, FrameLimits,
            NegotiatedParameters, ProtocolVersion, ServerMessage,
        };
        use crate::workspace::{WorkspaceProjectRoots, WorkspaceRequest};
        use uuid::Uuid;

        let repository = tempfile::tempdir().expect("repository");
        let git_init = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(repository.path())
            .output()
            .expect("git init");
        assert!(git_init.status.success());
        let dir = tempfile::tempdir().expect("tempdir");
        let bus = CommandBus::open(&dir.path().join("detach.db")).expect("bus");
        let project_id = ProjectId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xd7,
        ])
        .expect("project");
        let project_roots =
            WorkspaceProjectRoots::try_from_pairs([(project_id, repository.path().to_path_buf())])
                .expect("project roots");
        let (requests, executor) =
            HostRequestExecutor::start_with_workspace_projects(bus, project_roots);

        let id_a = Uuid::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xd1,
        ]);
        let id_b = Uuid::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xd2,
        ]);
        let (out_a, mut ports_a) = ConnectionOutputHandle::with_connection_id(id_a, 2, 4, 1);
        let (out_b, _ports_b) = ConnectionOutputHandle::with_connection_id(id_b, 2, 4, 1);
        let shutdown_a = out_a.subscribe_shutdown();
        let reg_a = requests
            .register_output(out_a.clone())
            .await
            .expect("register a");
        let reg_b = requests
            .register_output(out_b.clone())
            .await
            .expect("register b");
        assert_eq!(reg_a.id().as_uuid(), id_a);
        assert_eq!(reg_b.id().as_uuid(), id_b);

        let client = ClientId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xd3,
        ])
        .expect("client");
        let negotiated = NegotiatedParameters {
            version: ProtocolVersion::current(),
            client_id: client,
            capabilities: CapabilitySet::from_capabilities([
                Capability::PagedSnapshots,
                Capability::EventReplay,
                Capability::OperationSettlement,
                Capability::ExplicitDetach,
            ]),
            limits: FrameLimits::v1_default(),
        };
        let handle_a = requests.with_output(reg_a.id());
        let handle_b = requests.with_output(reg_b.id());

        let task_id = TaskId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xd4,
        ])
        .expect("task");
        let create = CommandEnvelope {
            command_id: CommandId::from_bytes([
                0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0xd5,
            ])
            .expect("command"),
            client_id: client,
            task_id: None,
            issued_at_ms: 1,
            expected_task_revision: None,
            command: Command::CreateTaskV2(CreateTaskRequestIntent {
                id: task_id,
                environment_id: EnvironmentId::from_bytes([
                    0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0xd6,
                ])
                .expect("env"),
                title: "detach live".into(),
                description: None,
                project_id,
                workspace: WorkspaceRequest::main(),
                primary_provider: None,
                defer_primary_provider_start: false,
                assignment: TaskAssignment::LocalOwner,
                created_at_ms: 1,
                connectivity: TaskConnectivity::Connected,
                attention: TaskAttention::None,
                activity: TaskActivity::Idle,
                review_readiness: ReviewReadiness::NotReady,
            }),
        };
        handle_a
            .execute(negotiated, ClientRequest::Command(create))
            .await
            .expect("create task");

        let open = handle_a
            .execute(
                negotiated,
                ClientRequest::Query(QueryEnvelope {
                    request_id: RequestId::from_bytes([
                        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00,
                        0x00, 0x00, 0x00, 0xd8,
                    ])
                    .expect("open req"),
                    client_id: client,
                    task_id: None,
                    query: Query::OpenEventReplay { after_sequence: 0 },
                }),
            )
            .await
            .expect("open replay");
        assert!(matches!(open, ServerMessage::QueryReply(_)));

        let before = requests
            .inspect_output(ConnectionOutputId::from_uuid(id_a))
            .await
            .expect("inspect before");
        assert_eq!(
            before,
            OutputInspection {
                registered: true,
                live_bound: true,
            }
        );

        let denied = handle_a
            .execute(
                NegotiatedParameters {
                    capabilities: CapabilitySet::from_capabilities([
                        Capability::PagedSnapshots,
                        Capability::EventReplay,
                        Capability::OperationSettlement,
                    ]),
                    ..negotiated
                },
                ClientRequest::Detach(DetachRequest {
                    request_id: RequestId::from_bytes([
                        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00,
                        0x00, 0x00, 0x00, 0xcf,
                    ])
                    .expect("denied req"),
                    client_id: client,
                    connection_id: id_a,
                }),
            )
            .await;
        assert!(matches!(
            denied,
            Err(super::super::ipc::IpcError::UnsupportedCapability)
        ));
        assert_eq!(
            requests
                .inspect_output(ConnectionOutputId::from_uuid(id_a))
                .await
                .expect("inspect after deny"),
            before,
            "unsupported detach must leave output and live binding intact"
        );

        let sibling_before = requests
            .inspect_output(ConnectionOutputId::from_uuid(id_b))
            .await
            .expect("inspect b");
        assert!(sibling_before.registered);

        let request_id = RequestId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xd9,
        ])
        .expect("detach req");
        let ack_message = handle_a
            .execute(
                negotiated,
                ClientRequest::Detach(DetachRequest {
                    request_id,
                    client_id: client,
                    connection_id: id_a,
                }),
            )
            .await
            .expect("detach");
        assert_eq!(
            ack_message,
            ServerMessage::Detached(crate::protocol::DetachAck {
                request_id,
                connection_id: id_a,
            })
        );

        let after = requests
            .inspect_output(ConnectionOutputId::from_uuid(id_a))
            .await
            .expect("inspect after");
        assert_eq!(
            after,
            OutputInspection {
                registered: false,
                live_bound: false,
            },
            "detach must remove output and live binding before ack write"
        );
        let sibling_after = requests
            .inspect_output(ConnectionOutputId::from_uuid(id_b))
            .await
            .expect("inspect b after");
        assert!(
            sibling_after.registered,
            "sibling output must remain usable"
        );

        assert!(
            !*shutdown_a.borrow(),
            "shutdown must not run before ack write"
        );
        out_a
            .try_enqueue_critical_shutdown_after_write(ack_message.clone())
            .expect("admit detach ack");
        assert!(!*shutdown_a.borrow());
        let outbound = ports_a
            .try_recv_prioritized()
            .expect("detach ack on critical lane");
        assert_eq!(outbound.message(), &ack_message);
        outbound.after_successful_write();
        assert!(
            *shutdown_a.borrow(),
            "successful ack write must request shutdown"
        );
        assert!(
            matches!(
                out_a.try_enqueue_critical(ServerMessage::QueryReply(
                    crate::domain::query::QueryReply {
                        request_id: RequestId::new(),
                        outcome: crate::domain::query::QueryOutcome::Err(
                            crate::domain::query::QueryError::NotFound
                        ),
                    }
                )),
                Err(super::super::ipc::IpcError::Unavailable)
            ),
            "no later critical traffic after detach shutdown"
        );

        let stale = handle_b
            .execute(
                negotiated,
                ClientRequest::Detach(DetachRequest {
                    request_id: RequestId::new(),
                    client_id: client,
                    connection_id: id_a,
                }),
            )
            .await;
        assert!(
            matches!(stale, Err(super::super::ipc::IpcError::Unauthorized)),
            "stale connection identity must not detach the sibling"
        );
        assert!(
            requests
                .inspect_output(ConnectionOutputId::from_uuid(id_b))
                .await
                .expect("b still registered")
                .registered
        );

        let wrong_client = ClientId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xda,
        ])
        .expect("foreign");
        let wrong = handle_b
            .execute(
                negotiated,
                ClientRequest::Detach(DetachRequest {
                    request_id: RequestId::new(),
                    client_id: wrong_client,
                    connection_id: id_b,
                }),
            )
            .await;
        assert!(matches!(
            wrong,
            Err(super::super::ipc::IpcError::Unauthorized)
        ));
        assert!(
            requests
                .inspect_output(ConnectionOutputId::from_uuid(id_b))
                .await
                .expect("b survives wrong client")
                .registered
        );

        handle_b
            .execute(
                negotiated,
                ClientRequest::Query(QueryEnvelope {
                    request_id: RequestId::new(),
                    client_id: client,
                    task_id: Some(task_id),
                    query: Query::TaskSnapshot,
                }),
            )
            .await
            .expect("sibling remains usable after failed detaches");

        drop(reg_a);
        drop(reg_b);
        drop(requests);
        executor.abort();
        let _ = executor.await;
        // Keep TempDir fixtures alive through executor teardown.
        let _ = (repository, dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn register_output_cancel_while_awaiting_ack_still_requests_shutdown() {
        use super::HostRequestExecutor;
        use crate::kernel::CommandBus;

        let dir = tempfile::tempdir().expect("tempdir");
        let bus = CommandBus::open(&dir.path().join("reg-cancel.db")).expect("bus");
        let (requests, executor) = HostRequestExecutor::start(bus);
        let (output, _ports) = ConnectionOutputHandle::new(1, 1, 1);
        let shutdown_rx = output.subscribe_shutdown();

        {
            let register = requests.register_output(output);
            tokio::pin!(register);
            // Poll registration first (biased) so the RAII guard is armed, then
            // cancel by dropping the future while send/ack may still be pending.
            tokio::select! {
                biased;
                result = &mut register => {
                    let registration = result.expect("register");
                    drop(registration);
                }
                _ = tokio::task::yield_now() => {
                    drop(register);
                }
            }
        }
        assert!(
            *shutdown_rx.borrow(),
            "cancel before ack observation must still request output shutdown"
        );

        drop(requests);
        executor.abort();
        let _ = executor.await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn open_connect_duplex_allocates_unique_host_uuidv7_outputs() {
        use super::HostRequestExecutor;
        use crate::kernel::CommandBus;
        use uuid::Version;

        let dir = tempfile::tempdir().expect("tempdir");
        let bus = CommandBus::open(&dir.path().join("duplex-uuid.db")).expect("bus");
        let (requests, executor) = HostRequestExecutor::start_without_automatic_maintenance(bus);
        let client_a = ClientId::new();
        let client_b = ClientId::new();

        let duplex_a1 = requests
            .open_connect_duplex(client_a)
            .await
            .expect("duplex a1");
        let duplex_a2 = requests
            .open_connect_duplex(client_a)
            .await
            .expect("duplex a2");
        let duplex_b = requests
            .open_connect_duplex(client_b)
            .await
            .expect("duplex b");

        let id_a1 = duplex_a1.registration.id().as_uuid();
        let id_a2 = duplex_a2.registration.id().as_uuid();
        let id_b = duplex_b.registration.id().as_uuid();
        assert_ne!(id_a1, id_a2);
        assert_ne!(id_a1, id_b);
        assert_ne!(id_a2, id_b);
        for id in [id_a1, id_a2, id_b] {
            assert_eq!(
                id.get_version(),
                Some(Version::SortRand),
                "host must mint UUIDv7 outputs, got {id}"
            );
        }

        drop(duplex_a1);
        drop(duplex_a2);
        drop(duplex_b);
        drop(requests);
        executor.abort();
        let _ = executor.await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn open_connect_duplex_bound_requests_open_event_replay_live_bound() {
        use super::{HostRequestExecutor, OutputInspection};
        use crate::domain::id::RequestId;
        use crate::domain::query::{Query, QueryEnvelope};
        use crate::kernel::CommandBus;
        use crate::protocol::{ClientRequest, ServerMessage};

        let dir = tempfile::tempdir().expect("tempdir");
        let bus = CommandBus::open(&dir.path().join("duplex-replay.db")).expect("bus");
        let (requests, executor) = HostRequestExecutor::start_without_automatic_maintenance(bus);
        let client = ClientId::new();
        let negotiated = event_replay_negotiated(client);

        let duplex = requests
            .open_connect_duplex(client)
            .await
            .expect("open duplex");
        let output_id = duplex.registration.id();
        let opened = duplex
            .requests
            .execute(
                negotiated,
                ClientRequest::Query(QueryEnvelope {
                    request_id: RequestId::new(),
                    client_id: client,
                    task_id: None,
                    query: Query::OpenEventReplay { after_sequence: 0 },
                }),
            )
            .await
            .expect("open replay");
        assert!(matches!(opened, ServerMessage::QueryReply(_)));
        assert_eq!(
            requests.inspect_output(output_id).await.expect("inspect"),
            OutputInspection {
                registered: true,
                live_bound: true,
            }
        );

        drop(duplex);
        drop(requests);
        executor.abort();
        let _ = executor.await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn open_connect_duplex_drop_unregisters_only_its_output() {
        use super::{HostRequestExecutor, OutputInspection};
        use crate::kernel::CommandBus;

        let dir = tempfile::tempdir().expect("tempdir");
        let bus = CommandBus::open(&dir.path().join("duplex-drop.db")).expect("bus");
        let (requests, executor) = HostRequestExecutor::start_without_automatic_maintenance(bus);
        let client_a = ClientId::new();
        let client_b = ClientId::new();

        let duplex_a = requests
            .open_connect_duplex(client_a)
            .await
            .expect("duplex a");
        let duplex_b = requests
            .open_connect_duplex(client_b)
            .await
            .expect("duplex b");
        let id_a = duplex_a.registration.id();
        let id_b = duplex_b.registration.id();

        drop(duplex_a);
        assert_eq!(
            requests.inspect_output(id_a).await.expect("inspect a"),
            OutputInspection {
                registered: false,
                live_bound: false,
            }
        );
        assert_eq!(
            requests.inspect_output(id_b).await.expect("inspect b"),
            OutputInspection {
                registered: true,
                live_bound: false,
            },
            "dropping one session must not unregister another client's output"
        );

        drop(duplex_b);
        assert_eq!(
            requests
                .inspect_output(id_b)
                .await
                .expect("inspect b after"),
            OutputInspection {
                registered: false,
                live_bound: false,
            }
        );

        drop(requests);
        executor.abort();
        let _ = executor.await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn open_connect_duplex_cancel_while_awaiting_ack_still_requests_shutdown() {
        use super::HostRequestExecutor;
        use crate::kernel::CommandBus;

        let dir = tempfile::tempdir().expect("tempdir");
        let bus = CommandBus::open(&dir.path().join("duplex-cancel.db")).expect("bus");
        let (requests, executor) = HostRequestExecutor::start(bus);
        let client = ClientId::new();

        {
            let open = requests.open_connect_duplex(client);
            tokio::pin!(open);
            // Poll open first (biased) so register_output_with_reconnect arms the
            // RAII guard, then cancel by dropping the future while send/ack may
            // still be pending — same contract as register_output_cancel_while_awaiting_ack.
            tokio::select! {
                biased;
                result = &mut open => {
                    let duplex = result.expect("open duplex");
                    let shutdown_rx = duplex.output.subscribe_shutdown();
                    let id = duplex.registration.id();
                    drop(duplex);
                    assert!(
                        *shutdown_rx.borrow(),
                        "completed session drop must request output shutdown"
                    );
                    assert!(
                        !requests
                            .inspect_output(id)
                            .await
                            .expect("inspect after drop")
                            .registered,
                        "drop must unregister the exact output"
                    );
                }
                _ = tokio::task::yield_now() => {
                    drop(open);
                }
            }
        }

        // Cancel or drop must leave no wedged retained owner: a fresh session opens.
        let duplex = requests
            .open_connect_duplex(client)
            .await
            .expect("reopen after cancel");
        let id = duplex.registration.id();
        assert!(
            requests
                .inspect_output(id)
                .await
                .expect("inspect reopen")
                .registered
        );
        let shutdown_rx = duplex.output.subscribe_shutdown();
        drop(duplex);
        assert!(*shutdown_rx.borrow());
        assert!(
            !requests
                .inspect_output(id)
                .await
                .expect("cleared")
                .registered
        );

        drop(requests);
        executor.abort();
        let _ = executor.await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn open_connect_duplex_dequeue_never_auto_acks_physical_write() {
        use super::{HostRequestExecutor, PhysicalWriteAckStatus};
        use crate::kernel::CommandBus;

        let dir = tempfile::tempdir().expect("tempdir");
        let bus = CommandBus::open(&dir.path().join("duplex-ack.db")).expect("bus");
        let (requests, executor) = HostRequestExecutor::start_without_automatic_maintenance(bus);
        let client = ClientId::new();
        let mut duplex = requests
            .open_connect_duplex(client)
            .await
            .expect("open duplex");

        let mut ack = duplex
            .output
            .try_enqueue_critical_tracked(sample_reply())
            .expect("tracked critical admit");
        assert_eq!(ack.status(), PhysicalWriteAckStatus::Pending);
        let outbound = duplex
            .ports
            .try_recv_prioritized()
            .expect("dequeue tracked critical");
        assert_eq!(
            ack.status(),
            PhysicalWriteAckStatus::Pending,
            "dequeuing must not call after_successful_write automatically"
        );
        outbound.after_successful_write();
        assert!(ack.wait().await.is_ok());

        drop(duplex);
        drop(requests);
        executor.abort();
        let _ = executor.await;
    }

    #[test]
    fn dispatch_authenticated_inspect_host_quit_without_host_shutdown_rejects_before_bus_query() {
        use super::dispatch_authenticated_request;
        use crate::domain::id::{RequestId, TaskId};
        use crate::domain::query::{Query, QueryEnvelope, QueryError, QueryOutcome, QueryReply};
        use crate::domain::ClientId;
        use crate::kernel::CommandBus;
        use crate::protocol::{Capability, CapabilitySet, ClientRequest, ServerMessage};

        let dir = tempfile::tempdir().expect("tempdir");
        let mut bus = CommandBus::open(&dir.path().join("inspect-auth-gate.db")).expect("bus");
        let client = ClientId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xe0,
        ])
        .expect("client");
        let request_id = RequestId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xe1,
        ])
        .expect("request");
        let inspect_envelope = || {
            ClientRequest::Query(QueryEnvelope {
                request_id,
                client_id: client,
                task_id: None,
                query: Query::InspectHostQuit,
            })
        };

        // Control: with HostShutdown the compatibility path reaches bus.query and succeeds.
        let granted = dispatch_authenticated_request(
            client,
            CapabilitySet::from_capabilities([Capability::HostShutdown]),
            &mut bus,
            inspect_envelope(),
        )
        .expect("granted inspect transport");
        assert!(
            matches!(
                granted,
                ServerMessage::QueryReply(QueryReply {
                    request_id: rid,
                    outcome: QueryOutcome::Ok(_),
                }) if rid == request_id
            ),
            "HostShutdown must still allow InspectHostQuit on the auth path; got {granted:?}"
        );

        // Regression: missing HostShutdown must fail closed before bus.query.
        // The same envelope just succeeded above, so UnsupportedCapability here
        // cannot be a bus-level failure — the capability gate returned first.
        let denied = dispatch_authenticated_request(
            client,
            CapabilitySet::from_capabilities([Capability::OperationSettlement]),
            &mut bus,
            inspect_envelope(),
        )
        .expect("denied inspect transport");
        assert_eq!(
            denied,
            ServerMessage::QueryReply(QueryReply {
                request_id,
                outcome: QueryOutcome::Err(QueryError::UnsupportedCapability),
            })
        );

        // Global-only scope still wins before capability when task_id is set.
        let scoped = dispatch_authenticated_request(
            client,
            CapabilitySet::from_capabilities([Capability::OperationSettlement]),
            &mut bus,
            ClientRequest::Query(QueryEnvelope {
                request_id,
                client_id: client,
                task_id: Some(
                    TaskId::from_bytes([
                        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00,
                        0x00, 0x00, 0x00, 0xe2,
                    ])
                    .expect("task"),
                ),
                query: Query::InspectHostQuit,
            }),
        )
        .expect("scoped inspect transport");
        assert_eq!(
            scoped,
            ServerMessage::QueryReply(QueryReply {
                request_id,
                outcome: QueryOutcome::Err(QueryError::InvalidRequest),
            })
        );
    }

    #[test]
    fn dispatch_authenticated_confirm_host_quit_capability_and_scope_gates() {
        use super::dispatch_authenticated_request;
        use crate::domain::command::{
            Command, CommandEnvelope, CommandReceipt, ConfirmHostQuitIntent, RejectionCode,
        };
        use crate::domain::id::{CommandId, TaskId};
        use crate::domain::ClientId;
        use crate::kernel::CommandBus;
        use crate::protocol::{Capability, CapabilitySet, ClientRequest, ServerMessage};

        let dir = tempfile::tempdir().expect("tempdir");
        let mut bus = CommandBus::open(&dir.path().join("confirm-auth-gate.db")).expect("bus");
        let client = ClientId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xf0,
        ])
        .expect("client");
        let events_before: i64 = {
            let conn =
                rusqlite::Connection::open(dir.path().join("confirm-auth-gate.db")).expect("raw");
            conn.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
                .expect("count")
        };

        let denied = dispatch_authenticated_request(
            client,
            CapabilitySet::from_capabilities([Capability::OperationSettlement]),
            &mut bus,
            ClientRequest::Command(CommandEnvelope {
                command_id: CommandId::from_bytes([
                    0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0xf1,
                ])
                .expect("cmd"),
                client_id: client,
                task_id: None,
                issued_at_ms: 1_725_000_000_400,
                expected_task_revision: None,
                command: Command::ConfirmHostQuit(ConfirmHostQuitIntent {
                    inspection_id: 0,
                    allow_uninspected_worktrees: true,
                }),
            }),
        );
        assert!(matches!(
            denied,
            Err(crate::host::IpcError::UnsupportedCapability)
        ));
        let events_after_deny: i64 = {
            let conn =
                rusqlite::Connection::open(dir.path().join("confirm-auth-gate.db")).expect("raw");
            conn.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
                .expect("count")
        };
        assert_eq!(events_after_deny, events_before, "denied must not mutate");

        let inspection = bus.inspect_host_quit().expect("inspect");
        let granted = dispatch_authenticated_request(
            client,
            CapabilitySet::from_capabilities([Capability::HostShutdown]),
            &mut bus,
            ClientRequest::Command(CommandEnvelope {
                command_id: CommandId::from_bytes([
                    0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0xf2,
                ])
                .expect("cmd"),
                client_id: client,
                task_id: None,
                issued_at_ms: 1_725_000_000_401,
                expected_task_revision: None,
                command: Command::ConfirmHostQuit(ConfirmHostQuitIntent {
                    inspection_id: inspection.inspection_id,
                    allow_uninspected_worktrees: true,
                }),
            }),
        )
        .expect("granted confirm");
        assert!(
            matches!(
                granted,
                ServerMessage::CommandReceipt(CommandReceipt::Accepted { .. })
            ),
            "granted ConfirmHostQuit must Accept, got {granted:?}"
        );

        // Fresh Open store for task-scope invalidation without Closing interference.
        let mut scoped_bus =
            CommandBus::open(&dir.path().join("confirm-scope-gate.db")).expect("scoped bus");
        let scoped_inspection = scoped_bus.inspect_host_quit().expect("scoped inspect");
        let scoped = dispatch_authenticated_request(
            client,
            CapabilitySet::from_capabilities([Capability::HostShutdown]),
            &mut scoped_bus,
            ClientRequest::Command(CommandEnvelope {
                command_id: CommandId::from_bytes([
                    0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0xf3,
                ])
                .expect("cmd"),
                client_id: client,
                task_id: Some(
                    TaskId::from_bytes([
                        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00,
                        0x00, 0x00, 0x00, 0xf4,
                    ])
                    .expect("task"),
                ),
                issued_at_ms: 1_725_000_000_402,
                expected_task_revision: None,
                command: Command::ConfirmHostQuit(ConfirmHostQuitIntent {
                    inspection_id: scoped_inspection.inspection_id,
                    allow_uninspected_worktrees: true,
                }),
            }),
        )
        .expect("scoped confirm transport");
        assert!(
            matches!(
                scoped,
                ServerMessage::CommandReceipt(CommandReceipt::Rejected {
                    code: RejectionCode::InvalidTransition,
                    ..
                })
            ),
            "task scope must InvalidTransition via CommandBus, got {scoped:?}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn host_request_executor_confirm_host_quit_capability_gate() {
        use super::HostRequestExecutor;
        use crate::domain::command::{
            Command, CommandEnvelope, CommandReceipt, ConfirmHostQuitIntent,
        };
        use crate::domain::id::CommandId;
        use crate::domain::ClientId;
        use crate::kernel::CommandBus;
        use crate::protocol::{
            Capability, CapabilitySet, ClientRequest, FrameLimits, NegotiatedParameters,
            ProtocolVersion, ServerMessage,
        };

        let dir = tempfile::tempdir().expect("tempdir");
        let bus = CommandBus::open(&dir.path().join("confirm-exec-gate.db")).expect("bus");
        let (requests, executor) = HostRequestExecutor::start(bus);
        let client = ClientId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xf5,
        ])
        .expect("client");
        let denied_negotiated = NegotiatedParameters {
            version: ProtocolVersion::current(),
            client_id: client,
            capabilities: CapabilitySet::from_capabilities([Capability::OperationSettlement]),
            limits: FrameLimits::v1_default(),
        };
        let denied = requests
            .execute(
                denied_negotiated,
                ClientRequest::Command(CommandEnvelope {
                    command_id: CommandId::from_bytes([
                        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00,
                        0x00, 0x00, 0x00, 0xf6,
                    ])
                    .expect("cmd"),
                    client_id: client,
                    task_id: None,
                    issued_at_ms: 1_725_000_000_410,
                    expected_task_revision: None,
                    command: Command::ConfirmHostQuit(ConfirmHostQuitIntent {
                        inspection_id: 0,
                        allow_uninspected_worktrees: true,
                    }),
                }),
            )
            .await;
        assert!(matches!(
            denied,
            Err(crate::host::IpcError::UnsupportedCapability)
        ));

        let granted_negotiated = NegotiatedParameters {
            version: ProtocolVersion::current(),
            client_id: client,
            capabilities: CapabilitySet::from_capabilities([Capability::HostShutdown]),
            limits: FrameLimits::v1_default(),
        };
        // Empty host: COALESCE(MAX(sequence),0) == 0 matches inspection_id 0.
        let granted = requests
            .execute(
                granted_negotiated,
                ClientRequest::Command(CommandEnvelope {
                    command_id: CommandId::from_bytes([
                        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00,
                        0x00, 0x00, 0x00, 0xf7,
                    ])
                    .expect("cmd"),
                    client_id: client,
                    task_id: None,
                    issued_at_ms: 1_725_000_000_411,
                    expected_task_revision: None,
                    command: Command::ConfirmHostQuit(ConfirmHostQuitIntent {
                        inspection_id: 0,
                        allow_uninspected_worktrees: true,
                    }),
                }),
            )
            .await
            .expect("granted confirm");
        assert!(
            matches!(
                granted,
                ServerMessage::CommandReceipt(CommandReceipt::Accepted { .. })
            ),
            "HostRequestExecutor granted ConfirmHostQuit must Accept, got {granted:?}"
        );

        drop(requests);
        executor.abort();
        let _ = executor.await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn host_cleanup_one_unit_per_maintenance_tick_with_two_registered_outputs() {
        use super::{ConnectionOutputId, HostRequestExecutor, OutputInspection};
        use crate::domain::command::{
            Command, CommandEnvelope, CommandReceipt, ConfirmHostQuitIntent,
        };
        use crate::domain::host::HostCleanupBranch;
        use crate::domain::id::CommandId;
        use crate::domain::ClientId;
        use crate::kernel::CommandBus;
        use crate::protocol::{
            Capability, CapabilitySet, ClientRequest, FrameLimits, NegotiatedParameters,
            ProtocolVersion, ServerMessage,
        };
        use rusqlite::Connection;
        use std::time::Duration;
        use uuid::Uuid;

        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("cleanup-tick.db");
        let bus = CommandBus::open(&db_path).expect("bus");
        let (requests, executor) = HostRequestExecutor::start_without_automatic_maintenance(bus);

        let id_a = Uuid::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xe1,
        ]);
        let id_b = Uuid::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xe2,
        ]);
        let (out_a, _ports_a) = ConnectionOutputHandle::with_connection_id(id_a, 2, 4, 1);
        let (out_b, _ports_b) = ConnectionOutputHandle::with_connection_id(id_b, 2, 4, 1);
        let _reg_a = requests.register_output(out_a).await.expect("register a");
        let _reg_b = requests.register_output(out_b).await.expect("register b");
        let output_id_a = ConnectionOutputId::from_uuid(id_a);
        let output_id_b = ConnectionOutputId::from_uuid(id_b);
        assert_ne!(output_id_a, output_id_b);

        let client = ClientId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xe3,
        ])
        .expect("client");
        let negotiated = NegotiatedParameters {
            version: ProtocolVersion::current(),
            client_id: client,
            capabilities: CapabilitySet::from_capabilities([Capability::HostShutdown]),
            limits: FrameLimits::v1_default(),
        };
        let confirm = requests
            .execute(
                negotiated,
                ClientRequest::Command(CommandEnvelope {
                    command_id: CommandId::from_bytes([
                        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00,
                        0x00, 0x00, 0x00, 0xe4,
                    ])
                    .expect("cmd"),
                    client_id: client,
                    task_id: None,
                    issued_at_ms: 1_725_000_000_500,
                    expected_task_revision: None,
                    command: Command::ConfirmHostQuit(ConfirmHostQuitIntent {
                        inspection_id: 0,
                        allow_uninspected_worktrees: true,
                    }),
                }),
            )
            .await
            .expect("confirm");
        assert!(matches!(
            confirm,
            ServerMessage::CommandReceipt(CommandReceipt::Accepted { .. })
        ));

        fn cleanup_branches(path: &std::path::Path) -> Vec<String> {
            let conn =
                Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                    .expect("readonly");
            let mut stmt = conn
                .prepare(
                    "SELECT branch FROM host_cleanup_branches
                     ORDER BY
                       CASE branch
                         WHEN 'agent_sessions' THEN 0
                         WHEN 'resources' THEN 1
                         WHEN 'outstanding_effects' THEN 2
                         WHEN 'task_teardowns' THEN 3
                         ELSE 99
                       END",
                )
                .expect("prepare");
            stmt.query_map([], |row| row.get::<_, String>(0))
                .expect("query")
                .map(|row| row.expect("row"))
                .collect()
        }

        assert!(cleanup_branches(&db_path).is_empty());
        requests.run_maintenance_once().await.expect("tick 1");
        assert_eq!(
            cleanup_branches(&db_path),
            vec![HostCleanupBranch::AgentSessions.as_str().to_string()]
        );
        requests.run_maintenance_once().await.expect("tick 2");
        assert_eq!(
            cleanup_branches(&db_path),
            vec![
                HostCleanupBranch::AgentSessions.as_str().to_string(),
                HostCleanupBranch::Resources.as_str().to_string(),
            ]
        );
        requests.run_maintenance_once().await.expect("tick 3");
        requests.run_maintenance_once().await.expect("tick 4");
        assert_eq!(
            cleanup_branches(&db_path),
            HostCleanupBranch::ORDER
                .iter()
                .map(|branch| branch.as_str().to_string())
                .collect::<Vec<_>>()
        );
        requests.run_maintenance_once().await.expect("idle tick");
        assert_eq!(cleanup_branches(&db_path).len(), 4);

        // Wait longer than the production maintenance period without scheduling
        // automatic ticks; only explicit invocations may advance rows.
        tokio::time::sleep(super::SNAPSHOT_REAPER_PERIOD + Duration::from_millis(250)).await;
        assert_eq!(cleanup_branches(&db_path).len(), 4);

        let inspect_a = requests
            .inspect_output(output_id_a)
            .await
            .expect("inspect a");
        let inspect_b = requests
            .inspect_output(output_id_b)
            .await
            .expect("inspect b");
        assert_eq!(
            inspect_a,
            OutputInspection {
                registered: true,
                live_bound: false,
            }
        );
        assert_eq!(
            inspect_b,
            OutputInspection {
                registered: true,
                live_bound: false,
            }
        );

        drop(requests);
        executor.abort();
        let _ = executor.await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn host_cleanup_executor_maintenance_fans_out_cleanup_failed() {
        use super::{
            ConnectionOutputId, HostRequestExecutor, OutputInspection, PrioritizedOutbound,
        };
        use crate::domain::agent::{AgentRole, AgentSessionFacts, AgentSessionLifecycle};
        use crate::domain::command::{
            Command, CommandEnvelope, CommandReceipt, ConfirmHostQuitIntent, CreateTaskIntent,
        };
        use crate::domain::event::Event;
        use crate::domain::id::{
            AgentSessionId, CommandId, EnvironmentId, ProjectId, RequestId, TaskId,
        };
        use crate::domain::operation::OperationErrorCode;
        use crate::domain::query::{Query, QueryEnvelope};
        use crate::domain::task::{
            ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity,
            TaskLifecycle, WorkspaceRef,
        };
        use crate::domain::ClientId;
        use crate::kernel::CommandBus;
        use crate::protocol::{
            Capability, CapabilitySet, ClientRequest, FrameLimits, NegotiatedParameters,
            ProtocolVersion, ServerMessage,
        };
        use crate::providers::ProviderKind;
        use uuid::Uuid;

        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("cleanup-failed-fanout.db");
        {
            let mut bus = CommandBus::open(&db_path).expect("seed");
            let task = TaskId::from_bytes([
                0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x41,
            ])
            .expect("task");
            bus.execute_for_test(CommandEnvelope {
                command_id: CommandId::from_bytes([
                    0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x42,
                ])
                .expect("create cmd"),
                client_id: ClientId::from_bytes([
                    0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x20,
                ])
                .expect("client"),
                task_id: None,
                issued_at_ms: 1_725_000_000_100,
                expected_task_revision: None,
                command: Command::CreateTask(CreateTaskIntent {
                    id: task,
                    environment_id: EnvironmentId::from_bytes([
                        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00,
                        0x00, 0x00, 0x00, 0x21,
                    ])
                    .expect("env"),
                    title: "cleanup failed fanout".into(),
                    description: None,
                    project_id: ProjectId::from_bytes([
                        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00,
                        0x00, 0x00, 0x00, 0x22,
                    ])
                    .expect("project"),
                    workspace: WorkspaceRef::Main,
                    assignment: TaskAssignment::LocalOwner,
                    created_at_ms: 1_725_000_000_000,
                    connectivity: TaskConnectivity::Connected,
                    attention: TaskAttention::None,
                    activity: TaskActivity::Idle,
                    review_readiness: ReviewReadiness::NotReady,
                }),
            })
            .expect("create");
            bus.execute(CommandEnvelope {
                command_id: CommandId::from_bytes([
                    0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x43,
                ])
                .expect("agent cmd"),
                client_id: ClientId::from_bytes([
                    0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x20,
                ])
                .expect("client"),
                task_id: Some(task),
                issued_at_ms: 1_725_000_000_100,
                expected_task_revision: Some(1),
                command: Command::RegisterAgentSession {
                    agent: AgentSessionFacts {
                        id: AgentSessionId::from_bytes([
                            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00,
                            0x00, 0x00, 0x00, 0xa1,
                        ])
                        .expect("agent"),
                        task_id: task,
                        role: AgentRole::Primary,
                        provider_kind: ProviderKind::ClaudeCode,
                        provider_session_id: Some(
                            "session-fanout".parse().expect("provider session"),
                        ),
                        lifecycle: AgentSessionLifecycle::Open,
                        runtime_generation: 0,
                        revision: 0,
                    },
                },
            })
            .expect("register agent");
        }

        let bus = CommandBus::open(&db_path).expect("bus");
        let (requests, executor) = HostRequestExecutor::start_without_automatic_maintenance(bus);
        let id = Uuid::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xf1,
        ]);
        let (out, mut ports) = ConnectionOutputHandle::with_connection_id(id, 4, 8, 1);
        let reg = requests.register_output(out).await.expect("register");
        let client = ClientId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xf2,
        ])
        .expect("client");
        let negotiated = NegotiatedParameters {
            version: ProtocolVersion::current(),
            client_id: client,
            capabilities: CapabilitySet::from_capabilities([
                Capability::HostShutdown,
                Capability::EventReplay,
                Capability::PagedSnapshots,
            ]),
            limits: FrameLimits::v1_default(),
        };
        let handle = requests.with_output(reg.id());
        let open = handle
            .execute(
                negotiated.clone(),
                ClientRequest::Query(QueryEnvelope {
                    request_id: RequestId::from_bytes([
                        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00,
                        0x00, 0x00, 0x00, 0xf4,
                    ])
                    .expect("req"),
                    client_id: client,
                    task_id: None,
                    query: Query::OpenEventReplay { after_sequence: 0 },
                }),
            )
            .await
            .expect("open replay");
        assert!(matches!(open, ServerMessage::QueryReply(_)));

        let inspect = handle
            .execute(
                negotiated.clone(),
                ClientRequest::Query(QueryEnvelope {
                    request_id: RequestId::from_bytes([
                        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00,
                        0x00, 0x00, 0x00, 0xf5,
                    ])
                    .expect("inspect req"),
                    client_id: client,
                    task_id: None,
                    query: Query::InspectHostQuit,
                }),
            )
            .await
            .expect("inspect quit");
        let inspection_id = match inspect {
            ServerMessage::QueryReply(reply) => match reply.outcome {
                crate::domain::query::QueryOutcome::Ok(
                    crate::domain::query::QueryResult::HostQuitInspection { inspection },
                ) => inspection.inspection_id,
                other => panic!("expected HostQuitInspection, got {other:?}"),
            },
            other => panic!("expected QueryReply, got {other:?}"),
        };

        let confirm = handle
            .execute(
                negotiated,
                ClientRequest::Command(CommandEnvelope {
                    command_id: CommandId::from_bytes([
                        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00,
                        0x00, 0x00, 0x00, 0xf3,
                    ])
                    .expect("cmd"),
                    client_id: client,
                    task_id: None,
                    issued_at_ms: 1_725_000_000_500,
                    expected_task_revision: None,
                    command: Command::ConfirmHostQuit(ConfirmHostQuitIntent {
                        inspection_id,
                        allow_uninspected_worktrees: true,
                    }),
                }),
            )
            .await
            .expect("confirm");
        let quit_op = match confirm {
            ServerMessage::CommandReceipt(CommandReceipt::Accepted { operation_id, .. }) => {
                operation_id
            }
            other => panic!("expected Accepted, got {other:?}"),
        };

        for _ in 0..4 {
            requests.run_maintenance_once().await.expect("branch tick");
        }
        while let Some(outbound) = ports.try_recv_prioritized() {
            let _ = outbound;
        }

        requests
            .run_maintenance_once()
            .await
            .expect("failure terminal tick");
        let mut saw_failed = false;
        while let Some(outbound) = ports.try_recv_prioritized() {
            if matches!(&outbound, PrioritizedOutbound::Durable(_)) {
                if let ServerMessage::DurableEvent { event, .. } = outbound.message() {
                    if let Event::OperationFailed(fact) = &event.payload {
                        assert_eq!(fact.operation_id, quit_op);
                        assert_eq!(fact.code, OperationErrorCode::CleanupFailed);
                        assert_eq!(fact.action_epoch, Some(1));
                        saw_failed = true;
                    }
                }
            }
        }
        assert!(
            saw_failed,
            "Failed terminalization must fan out OperationFailed"
        );

        let inspect = requests
            .inspect_output(ConnectionOutputId::from_uuid(id))
            .await
            .expect("inspect");
        assert_eq!(
            inspect,
            OutputInspection {
                registered: true,
                live_bound: true,
            }
        );

        requests
            .run_maintenance_once()
            .await
            .expect("post-terminal idle");
        assert!(
            ports.try_recv_prioritized().is_none(),
            "idempotent Idle must not invent additional durables"
        );

        drop(requests);
        executor.abort();
        let _ = executor.await;
        let _ = TaskLifecycle::Open;
    }

    fn host_shutdown_negotiated(client: ClientId) -> NegotiatedParameters {
        use crate::protocol::{Capability, CapabilitySet, FrameLimits, ProtocolVersion};
        NegotiatedParameters {
            version: ProtocolVersion::current(),
            client_id: client,
            capabilities: CapabilitySet::from_capabilities([Capability::HostShutdown]),
            limits: FrameLimits::v1_default(),
        }
    }

    fn inspect_quit_request(client: ClientId) -> ClientRequest {
        use crate::domain::id::RequestId;
        use crate::domain::query::{Query, QueryEnvelope};
        ClientRequest::Query(QueryEnvelope {
            request_id: RequestId::new(),
            client_id: client,
            task_id: None,
            query: Query::InspectHostQuit,
        })
    }

    fn confirm_quit_request(
        client: ClientId,
        command_id: crate::domain::id::CommandId,
        inspection_id: u64,
    ) -> ClientRequest {
        use crate::domain::command::{Command, CommandEnvelope, ConfirmHostQuitIntent};
        ClientRequest::Command(CommandEnvelope {
            command_id,
            client_id: client,
            task_id: None,
            issued_at_ms: 1_725_000_000_700,
            expected_task_revision: None,
            command: Command::ConfirmHostQuit(ConfirmHostQuitIntent {
                inspection_id,
                allow_uninspected_worktrees: true,
            }),
        })
    }

    async fn inspection_id_for(
        handle: &HostRequestHandle,
        negotiated: NegotiatedParameters,
        client: ClientId,
    ) -> u64 {
        match handle
            .execute(negotiated, inspect_quit_request(client))
            .await
            .expect("inspect")
        {
            ServerMessage::QueryReply(reply) => match reply.outcome {
                crate::domain::query::QueryOutcome::Ok(
                    crate::domain::query::QueryResult::HostQuitInspection { inspection },
                ) => inspection.inspection_id,
                other => panic!("expected HostQuitInspection, got {other:?}"),
            },
            other => panic!("expected QueryReply, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tracked_critical_ack_pending_until_after_successful_write_and_aborts_on_drop() {
        let (handle, mut ports) = ConnectionOutputHandle::new(2, 1, 1);
        let mut ack = handle
            .try_enqueue_critical_tracked(sample_reply())
            .expect("tracked critical admit");
        assert_eq!(ack.status(), PhysicalWriteAckStatus::Pending);
        let outbound = ports
            .try_recv_prioritized()
            .expect("dequeue tracked critical");
        assert_eq!(ack.status(), PhysicalWriteAckStatus::Pending);
        outbound.after_successful_write();
        assert!(ack.wait().await.is_ok());

        let mut aborted = handle
            .try_enqueue_critical_tracked(sample_reply())
            .expect("second tracked critical");
        drop(ports.try_recv_prioritized().expect("dequeue second"));
        assert_eq!(aborted.status(), PhysicalWriteAckStatus::Aborted);
    }

    #[test]
    fn tracked_critical_full_and_closed_fail_closed_without_ack() {
        let (handle, mut ports) = ConnectionOutputHandle::new(1, 1, 1);
        handle.try_enqueue_critical(sample_reply()).expect("fill");
        assert!(handle.try_enqueue_critical_tracked(sample_reply()).is_err());
        assert!(handle.is_shutdown_requested());
        let _ = ports.try_recv_prioritized();

        let (closed, ports) = ConnectionOutputHandle::new(1, 1, 1);
        drop(ports);
        closed.request_shutdown();
        assert!(closed.try_enqueue_critical_tracked(sample_reply()).is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ordinary_execute_accepted_confirm_host_quit_returns_server_message() {
        use crate::domain::command::CommandReceipt;
        use crate::domain::id::CommandId;
        use crate::kernel::CommandBus;

        let dir = tempfile::tempdir().expect("tempdir");
        let bus = CommandBus::open(&dir.path().join("ordinary-quit.db")).expect("bus");
        let (requests, executor) = HostRequestExecutor::start_without_automatic_maintenance(bus);
        let (out, mut ports) = ConnectionOutputHandle::new(4, 8, 1);
        let reg = requests.register_output(out).await.expect("register");
        let client = ClientId::new();
        let negotiated = host_shutdown_negotiated(client);
        let handle = requests.with_output(reg.id());
        let inspection_id = inspection_id_for(&handle, negotiated.clone(), client).await;

        let confirm = handle
            .execute(
                negotiated,
                confirm_quit_request(client, CommandId::new(), inspection_id),
            )
            .await
            .expect("ordinary execute");
        assert!(matches!(
            confirm,
            ServerMessage::CommandReceipt(CommandReceipt::Accepted { .. })
        ));
        assert!(ports.try_recv_prioritized().is_none());
        assert!(requests
            .take_pending_quit_receipt_ack(reg.id())
            .await
            .expect("take")
            .is_none());

        drop(reg);
        drop(requests);
        executor.abort();
        let _ = executor.await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn create_with_primary_provider_keeps_bindings_when_launch_fails() {
        use std::process::Command as ProcessCommand;

        use crate::domain::command::{Command, CommandEnvelope, CreateTaskRequestIntent};
        use crate::domain::id::{CommandId, EnvironmentId, ProjectId, TaskId};
        use crate::domain::task::{
            ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity,
        };
        use crate::host::IpcError;
        use crate::kernel::CommandBus;
        use crate::protocol::{Capability, CapabilitySet, FrameLimits, ProtocolVersion};
        use crate::providers::ProviderKind;
        use crate::workspace::{WorkspaceProjectRoots, WorkspaceRequest};

        let repository = tempfile::tempdir().expect("repository");
        let output = ProcessCommand::new("git")
            .args(["init", "--quiet"])
            .current_dir(repository.path())
            .output()
            .expect("git init");
        assert!(output.status.success());
        let database = repository.path().join("create-primary-executor.sqlite");
        let project_id = ProjectId::new();
        let project_roots =
            WorkspaceProjectRoots::try_from_pairs([(project_id, repository.path().to_path_buf())])
                .expect("project roots");
        let client = ClientId::new();
        let negotiated = NegotiatedParameters {
            version: ProtocolVersion::current(),
            client_id: client,
            capabilities: CapabilitySet::from_capabilities([Capability::ProviderInput]),
            limits: FrameLimits::v1_default(),
        };
        let task_id = TaskId::new();
        let bus = CommandBus::open(&database).expect("bus");
        let (requests, executor) =
            HostRequestExecutor::start_without_automatic_maintenance_with_workspace_projects(
                bus,
                project_roots.clone(),
            );
        let (out, _ports) = ConnectionOutputHandle::new(4, 8, 1);
        let registration = requests.register_output(out).await.expect("register");
        let handle = requests.with_output(registration.id());
        let result = handle
            .execute(
                negotiated.clone(),
                ClientRequest::Command(CommandEnvelope {
                    command_id: CommandId::new(),
                    client_id: client,
                    task_id: None,
                    issued_at_ms: 1_725_000_000_800,
                    expected_task_revision: None,
                    command: Command::CreateTaskV2(CreateTaskRequestIntent {
                        id: task_id,
                        environment_id: EnvironmentId::new(),
                        title: "Claude primary".into(),
                        description: None,
                        project_id,
                        workspace: WorkspaceRequest::main(),
                        primary_provider: Some(ProviderKind::ClaudeCode),
                        defer_primary_provider_start: false,
                        assignment: TaskAssignment::LocalOwner,
                        created_at_ms: 1_725_000_000_800,
                        connectivity: TaskConnectivity::Connected,
                        attention: TaskAttention::None,
                        activity: TaskActivity::Idle,
                        review_readiness: ReviewReadiness::NotReady,
                    }),
                }),
            )
            .await;
        assert!(matches!(
            result,
            Ok(ServerMessage::CommandReceipt(
                crate::domain::command::CommandReceipt::Accepted { .. }
            ))
        ));
        drop(registration);
        drop(requests);
        executor.abort();
        let _ = executor.await;

        let bus = CommandBus::open(&database).expect("reopen bus");
        let snapshot = bus
            .task_snapshot(task_id)
            .expect("snapshot")
            .expect("created task");
        let agent_id = snapshot.primary_agent_id.expect("primary agent");
        assert_eq!(snapshot.agents[&agent_id].runtime_generation, 1);
        let resource = snapshot
            .resources
            .values()
            .find(|resource| resource.task_id == Some(task_id))
            .expect("primary resource");
        assert_eq!(resource.runtime_generation, 1);
        assert_eq!(snapshot.attention, TaskAttention::Failed);
        bus.load_task_runtime(task_id, &project_roots)
            .expect("load runtime after failed launch")
            .expect("persisted primary task")
            .workspace
            .runtime_working_directory()
            .expect("reopen cwd after failed launch");
        drop(bus);

        let cursor_task_id = TaskId::new();
        let bus = CommandBus::open(&database).expect("bus for cursor rejection");
        let (requests, executor) =
            HostRequestExecutor::start_without_automatic_maintenance_with_workspace_projects(
                bus,
                project_roots,
            );
        let cursor = requests
            .execute(
                negotiated,
                ClientRequest::Command(CommandEnvelope {
                    command_id: CommandId::new(),
                    client_id: client,
                    task_id: None,
                    issued_at_ms: 1_725_000_000_801,
                    expected_task_revision: None,
                    command: Command::CreateTaskV2(CreateTaskRequestIntent {
                        id: cursor_task_id,
                        environment_id: EnvironmentId::new(),
                        title: "Cursor primary".into(),
                        description: None,
                        project_id,
                        workspace: WorkspaceRequest::main(),
                        primary_provider: Some(ProviderKind::Cursor),
                        defer_primary_provider_start: false,
                        assignment: TaskAssignment::LocalOwner,
                        created_at_ms: 1_725_000_000_801,
                        connectivity: TaskConnectivity::Connected,
                        attention: TaskAttention::None,
                        activity: TaskActivity::Idle,
                        review_readiness: ReviewReadiness::NotReady,
                    }),
                }),
            )
            .await;
        assert!(matches!(cursor, Err(IpcError::Unavailable)));
        drop(requests);
        executor.abort();
        let _ = executor.await;
        let bus = CommandBus::open(&database).expect("reopen cursor bus");
        assert!(bus
            .task_snapshot(cursor_task_id)
            .expect("cursor task lookup")
            .is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn closing_failed_primary_provider_task_archives_it() {
        use std::process::Command as ProcessCommand;

        use crate::domain::command::{Command, CommandEnvelope, CreateTaskRequestIntent};
        use crate::domain::id::{CommandId, EnvironmentId, ProjectId, TaskId};
        use crate::domain::resource::ResourceLifecycle;
        use crate::domain::task::{
            ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity,
            TaskLifecycle,
        };
        use crate::kernel::CommandBus;
        use crate::protocol::{Capability, CapabilitySet, FrameLimits, ProtocolVersion};
        use crate::providers::ProviderKind;
        use crate::workspace::{WorkspaceProjectRoots, WorkspaceRequest};

        let repository = tempfile::tempdir().expect("repository");
        let output = ProcessCommand::new("git")
            .args(["init", "--quiet"])
            .current_dir(repository.path())
            .output()
            .expect("git init");
        assert!(output.status.success());
        let database = repository.path().join("close-failed-primary.sqlite");
        let project_id = ProjectId::new();
        let project_roots =
            WorkspaceProjectRoots::try_from_pairs([(project_id, repository.path().to_path_buf())])
                .expect("project roots");
        let client = ClientId::new();
        let negotiated = NegotiatedParameters {
            version: ProtocolVersion::current(),
            client_id: client,
            capabilities: CapabilitySet::from_capabilities([Capability::ProviderInput]),
            limits: FrameLimits::v1_default(),
        };
        let task_id = TaskId::new();
        let bus = CommandBus::open(&database).expect("bus");
        let (requests, executor) =
            HostRequestExecutor::start_without_automatic_maintenance_with_workspace_projects(
                bus,
                project_roots.clone(),
            );
        let (out, _ports) = ConnectionOutputHandle::new(4, 8, 1);
        let registration = requests.register_output(out).await.expect("register");
        let handle = requests.with_output(registration.id());
        handle
            .execute(
                negotiated.clone(),
                ClientRequest::Command(CommandEnvelope {
                    command_id: CommandId::new(),
                    client_id: client,
                    task_id: None,
                    issued_at_ms: 1_725_000_000_800,
                    expected_task_revision: None,
                    command: Command::CreateTaskV2(CreateTaskRequestIntent {
                        id: task_id,
                        environment_id: EnvironmentId::new(),
                        title: "Claude primary".into(),
                        description: None,
                        project_id,
                        workspace: WorkspaceRequest::main(),
                        primary_provider: Some(ProviderKind::ClaudeCode),
                        defer_primary_provider_start: false,
                        assignment: TaskAssignment::LocalOwner,
                        created_at_ms: 1_725_000_000_800,
                        connectivity: TaskConnectivity::Connected,
                        attention: TaskAttention::None,
                        activity: TaskActivity::Idle,
                        review_readiness: ReviewReadiness::NotReady,
                    }),
                }),
            )
            .await
            .expect("create task");
        drop(registration);
        drop(requests);
        executor.abort();
        let _ = executor.await;

        let bus = CommandBus::open(&database).expect("reopen bus");
        let snapshot = bus
            .task_snapshot(task_id)
            .expect("snapshot")
            .expect("created task");
        assert_eq!(snapshot.attention, TaskAttention::Failed);
        let revision = snapshot.task.revision;
        drop(bus);

        let bus = CommandBus::open(&database).expect("bus for close");
        let (requests, executor) =
            HostRequestExecutor::start_without_automatic_maintenance_with_workspace_projects(
                bus,
                project_roots,
            );
        let (out, _ports) = ConnectionOutputHandle::new(4, 8, 1);
        let registration = requests.register_output(out).await.expect("register");
        let handle = requests.with_output(registration.id());
        let close = handle
            .execute(
                negotiated,
                ClientRequest::Command(CommandEnvelope {
                    command_id: CommandId::new(),
                    client_id: client,
                    task_id: Some(task_id),
                    issued_at_ms: 1_725_000_000_801,
                    expected_task_revision: Some(revision),
                    command: Command::BeginCloseTask,
                }),
            )
            .await
            .expect("close task");
        assert!(
            matches!(
                close,
                ServerMessage::CommandReceipt(
                    crate::domain::command::CommandReceipt::Accepted { .. }
                )
            ),
            "close must be accepted: {close:?}"
        );
        drop(registration);
        drop(requests);
        executor.abort();
        let _ = executor.await;

        let bus = CommandBus::open(&database).expect("reopen after close");
        let snapshot = bus
            .task_snapshot(task_id)
            .expect("snapshot")
            .expect("closed task");
        assert_eq!(snapshot.task.lifecycle, TaskLifecycle::Archived);
        assert!(
            snapshot
                .resources
                .values()
                .all(|resource| resource.lifecycle == ResourceLifecycle::Released),
            "archive must release the failed-start terminal resource"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn duplex_accepted_confirm_host_quit_executor_admits_tracked_critical_receipt() {
        use crate::domain::command::CommandReceipt;
        use crate::domain::id::CommandId;
        use crate::kernel::CommandBus;

        let dir = tempfile::tempdir().expect("tempdir");
        let bus = CommandBus::open(&dir.path().join("duplex-quit-admit.db")).expect("bus");
        let (requests, executor) = HostRequestExecutor::start_without_automatic_maintenance(bus);
        let (out, mut ports) = ConnectionOutputHandle::new(4, 8, 1);
        let reg = requests.register_output(out).await.expect("register");
        let client = ClientId::new();
        let negotiated = host_shutdown_negotiated(client);
        let handle = requests.with_output(reg.id());
        let inspection_id = inspection_id_for(&handle, negotiated.clone(), client).await;

        let completion = handle
            .execute_for_duplex(
                negotiated,
                confirm_quit_request(client, CommandId::new(), inspection_id),
            )
            .await
            .expect("duplex quit");
        let DuplexExecuteCompletion::ExecutorAdmittedQuitReceipt { operation_id } = completion
        else {
            panic!("expected ExecutorAdmittedQuitReceipt, got {completion:?}");
        };
        let outbound = ports.try_recv_prioritized().expect("one critical receipt");
        match outbound.message() {
            ServerMessage::CommandReceipt(CommandReceipt::Accepted {
                operation_id: wired,
                task_revision: None,
                event_ids,
                ..
            }) => {
                assert_eq!(*wired, operation_id);
                assert_eq!(event_ids.len(), 1);
            }
            other => panic!("expected host-admission Accepted, got {other:?}"),
        }
        assert!(ports.try_recv_prioritized().is_none());

        let (stored_op, mut stored_ack) = requests
            .take_pending_quit_receipt_ack(reg.id())
            .await
            .expect("take")
            .expect("stored ack");
        assert_eq!(stored_op, operation_id);
        assert_eq!(stored_ack.status(), PhysicalWriteAckStatus::Pending);
        outbound.after_successful_write();
        assert_eq!(stored_ack.status(), PhysicalWriteAckStatus::Succeeded);

        drop(reg);
        drop(requests);
        executor.abort();
        let _ = executor.await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn duplex_non_quit_rejected_quit_and_command_id_collision_remain_caller_owned() {
        use crate::domain::command::{
            Command, CommandEnvelope, CommandReceipt, CreateTaskRequestIntent, RejectionCode,
        };
        use crate::domain::id::{CommandId, EnvironmentId, ProjectId, TaskId};
        use crate::domain::task::{
            ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity,
        };
        use crate::kernel::CommandBus;
        use crate::workspace::{WorkspaceProjectRoots, WorkspaceRequest};

        let repository = tempfile::tempdir().expect("repository");
        let git_init = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(repository.path())
            .output()
            .expect("git init");
        assert!(git_init.status.success());
        let dir = tempfile::tempdir().expect("tempdir");
        let bus = CommandBus::open(&dir.path().join("duplex-caller-owned.db")).expect("bus");
        let project_id = ProjectId::new();
        let project_roots =
            WorkspaceProjectRoots::try_from_pairs([(project_id, repository.path().to_path_buf())])
                .expect("project roots");
        let (requests, executor) =
            HostRequestExecutor::start_without_automatic_maintenance_with_workspace_projects(
                bus,
                project_roots,
            );
        let (out, mut ports) = ConnectionOutputHandle::new(4, 8, 1);
        let reg = requests.register_output(out).await.expect("register");
        let client = ClientId::new();
        let negotiated = host_shutdown_negotiated(client);
        let handle = requests.with_output(reg.id());

        let inspect = handle
            .execute_for_duplex(negotiated.clone(), inspect_quit_request(client))
            .await
            .expect("inspect");
        assert!(matches!(
            inspect,
            DuplexExecuteCompletion::CallerMustWrite(ServerMessage::QueryReply(_))
        ));
        assert!(ports.try_recv_prioritized().is_none());

        let rejected = handle
            .execute_for_duplex(
                negotiated.clone(),
                ClientRequest::Command(CommandEnvelope {
                    command_id: CommandId::new(),
                    client_id: client,
                    task_id: Some(TaskId::new()),
                    issued_at_ms: 1_725_000_000_701,
                    expected_task_revision: None,
                    command: Command::ConfirmHostQuit(
                        crate::domain::command::ConfirmHostQuitIntent {
                            inspection_id: 0,
                            allow_uninspected_worktrees: true,
                        },
                    ),
                }),
            )
            .await
            .expect("rejected");
        assert!(matches!(
            rejected,
            DuplexExecuteCompletion::CallerMustWrite(ServerMessage::CommandReceipt(
                CommandReceipt::Rejected {
                    code: RejectionCode::InvalidTransition,
                    ..
                }
            ))
        ));

        let reused_command_id = CommandId::new();
        let created = handle
            .execute(
                negotiated.clone(),
                ClientRequest::Command(CommandEnvelope {
                    command_id: reused_command_id,
                    client_id: client,
                    task_id: None,
                    issued_at_ms: 1_725_000_000_702,
                    expected_task_revision: None,
                    command: Command::CreateTaskV2(CreateTaskRequestIntent {
                        id: TaskId::new(),
                        environment_id: EnvironmentId::new(),
                        title: "collision".into(),
                        description: None,
                        project_id,
                        workspace: WorkspaceRequest::main(),
                        primary_provider: None,
                        defer_primary_provider_start: false,
                        assignment: TaskAssignment::LocalOwner,
                        created_at_ms: 1_725_000_000_000,
                        connectivity: TaskConnectivity::Connected,
                        attention: TaskAttention::None,
                        activity: TaskActivity::Idle,
                        review_readiness: ReviewReadiness::NotReady,
                    }),
                }),
            )
            .await
            .expect("create task");
        let ServerMessage::CommandReceipt(CommandReceipt::Accepted {
            task_revision: Some(_),
            ..
        }) = created
        else {
            panic!("expected task Accepted with revision, got {created:?}");
        };

        let collision = handle
            .execute_for_duplex(
                negotiated,
                confirm_quit_request(client, reused_command_id, 0),
            )
            .await
            .expect("collision duplex");
        match collision {
            DuplexExecuteCompletion::CallerMustWrite(ServerMessage::CommandReceipt(
                CommandReceipt::Rejected {
                    code: RejectionCode::IdempotencyConflict,
                    ..
                },
            )) => {}
            other => panic!(
                "collision must stay caller-owned non-quit IdempotencyConflict, got {other:?}"
            ),
        }
        assert!(ports.try_recv_prioritized().is_none());
        assert!(requests
            .take_pending_quit_receipt_ack(reg.id())
            .await
            .expect("take")
            .is_none());

        drop(reg);
        drop(requests);
        executor.abort();
        let _ = executor.await;
        // Keep TempDir fixtures alive through executor teardown.
        let _ = (repository, dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn duplex_accepted_quit_missing_then_retry_admits_on_healthy_output() {
        use crate::domain::id::CommandId;
        use crate::kernel::CommandBus;

        let dir = tempfile::tempdir().expect("tempdir");
        let bus = CommandBus::open(&dir.path().join("duplex-quit-missing.db")).expect("bus");
        let (requests, executor) = HostRequestExecutor::start_without_automatic_maintenance(bus);
        let client = ClientId::new();
        let negotiated = host_shutdown_negotiated(client);
        let inspection_id = inspection_id_for(&requests, negotiated.clone(), client).await;
        let command_id = CommandId::new();

        let missing = requests
            .execute_for_duplex(
                negotiated.clone(),
                confirm_quit_request(client, command_id, inspection_id),
            )
            .await;
        assert!(matches!(missing, Err(crate::host::IpcError::Unavailable)));

        let (out, mut ports) = ConnectionOutputHandle::new(4, 8, 1);
        let reg = requests.register_output(out).await.expect("register");
        let output_id = reg.id();
        let handle = requests.with_output(output_id);
        let retry = handle
            .execute_for_duplex(
                negotiated,
                confirm_quit_request(client, command_id, inspection_id),
            )
            .await
            .expect("retry");
        let DuplexExecuteCompletion::ExecutorAdmittedQuitReceipt { operation_id } = retry else {
            panic!("expected ExecutorAdmittedQuitReceipt, got {retry:?}");
        };
        let frame = ports.try_recv_prioritized().expect("one critical");
        match frame.message() {
            ServerMessage::CommandReceipt(crate::domain::command::CommandReceipt::Accepted {
                operation_id: wired,
                ..
            }) => assert_eq!(*wired, operation_id),
            other => panic!("expected Accepted, got {other:?}"),
        }
        assert!(ports.try_recv_prioritized().is_none());
        let (stored_op, mut ack) = requests
            .take_pending_quit_receipt_ack(output_id)
            .await
            .expect("take")
            .expect("pending ack after admit");
        assert_eq!(stored_op, operation_id);
        assert_eq!(ack.status(), PhysicalWriteAckStatus::Pending);

        drop(requests);
        executor.abort();
        let _ = executor.await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn duplex_accepted_quit_full_then_retry_on_fresh_output_and_detach_clears_ack() {
        use crate::domain::id::CommandId;
        use crate::kernel::CommandBus;

        let dir = tempfile::tempdir().expect("tempdir");
        let bus = CommandBus::open(&dir.path().join("duplex-quit-full.db")).expect("bus");
        let (requests, executor) = HostRequestExecutor::start_without_automatic_maintenance(bus);
        let (out, mut ports) = ConnectionOutputHandle::new(1, 8, 1);
        let output_id = out.id();
        let reg = requests
            .register_output(out.clone())
            .await
            .expect("register");
        let client = ClientId::new();
        let negotiated = host_shutdown_negotiated(client);
        let handle = requests.with_output(reg.id());
        out.try_enqueue_critical(sample_reply()).expect("fill");
        let inspection_id = inspection_id_for(&handle, negotiated.clone(), client).await;
        let command_id = CommandId::new();

        let full = handle
            .execute_for_duplex(
                negotiated.clone(),
                confirm_quit_request(client, command_id, inspection_id),
            )
            .await;
        assert!(matches!(full, Err(crate::host::IpcError::Unavailable)));
        assert!(out.is_shutdown_requested());
        assert!(requests
            .take_pending_quit_receipt_ack(output_id)
            .await
            .expect("take")
            .is_none());
        assert!(matches!(
            ports.try_recv_prioritized().expect("filler").message(),
            ServerMessage::QueryReply(_)
        ));
        drop(reg);

        let (healthy, mut healthy_ports) = ConnectionOutputHandle::new(4, 8, 1);
        let healthy_id = healthy.id();
        let reg = requests
            .register_output(healthy)
            .await
            .expect("register healthy");
        let handle = requests.with_output(reg.id());
        let retry = handle
            .execute_for_duplex(
                negotiated,
                confirm_quit_request(client, command_id, inspection_id),
            )
            .await;
        assert!(matches!(retry, Err(crate::host::IpcError::Unavailable)));
        assert!(healthy_ports.try_recv_prioritized().is_none());
        // Detach clears the pending map without requiring a take first.
        drop(reg);
        assert!(requests
            .take_pending_quit_receipt_ack(healthy_id)
            .await
            .expect("cleared")
            .is_none());

        drop(requests);
        executor.abort();
        let _ = executor.await;
    }

    fn conversation_wake_negotiated(client: ClientId) -> NegotiatedParameters {
        use crate::protocol::{Capability, CapabilitySet, FrameLimits, ProtocolVersion};
        NegotiatedParameters {
            version: ProtocolVersion::current(),
            client_id: client,
            capabilities: CapabilitySet::from_capabilities([
                Capability::TaskCockpit,
                Capability::SemanticConversation,
            ]),
            limits: FrameLimits::v1_default(),
        }
    }

    fn create_task_for_conversation_wake(
        bus: &mut crate::kernel::CommandBus,
        client: ClientId,
    ) -> TaskId {
        use crate::domain::command::{Command, CommandEnvelope, CommandReceipt, CreateTaskIntent};
        use crate::domain::id::{EnvironmentId, ProjectId};
        use crate::domain::task::{
            ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity,
            WorkspaceRef,
        };

        let task_id = TaskId::new();
        // These tests exercise subscription/dirty delivery, not workspace
        // admission. Seed their durable task through the test-only kernel
        // authority before the authenticated host executor starts; raw V1
        // CreateTask must remain rejected at that production boundary.
        let receipt = bus
            .execute_for_test(CommandEnvelope {
                command_id: crate::domain::id::CommandId::new(),
                client_id: client,
                task_id: None,
                issued_at_ms: 1_725_000_000_000,
                expected_task_revision: None,
                command: Command::CreateTask(CreateTaskIntent {
                    id: task_id,
                    environment_id: EnvironmentId::new(),
                    title: "conversation wake".into(),
                    description: None,
                    project_id: ProjectId::new(),
                    workspace: WorkspaceRef::Main,
                    assignment: TaskAssignment::LocalOwner,
                    created_at_ms: 1_725_000_000_000,
                    connectivity: TaskConnectivity::Connected,
                    attention: TaskAttention::None,
                    activity: TaskActivity::Idle,
                    review_readiness: ReviewReadiness::NotReady,
                }),
            })
            .expect("seed task");
        assert!(matches!(receipt, CommandReceipt::Accepted { .. }));
        task_id
    }

    fn sample_semantic_draft(
        task_id: TaskId,
        index: u64,
    ) -> crate::remote::presentation::SemanticEventDraft {
        use crate::remote::presentation::{
            SemanticEventDraft, SemanticEventKind, SemanticRetention, SemanticSource,
            StableSessionKey,
        };
        SemanticEventDraft {
            stable_session_key: StableSessionKey::from_tab(task_id.to_string()),
            occurred_at_epoch_ms: 1_725_000_000_000 + index,
            source: SemanticSource::Codex,
            kind: SemanticEventKind::AssistantMessage {
                message_id: format!("msg-{index}"),
                text: format!("token-{index}"),
                streaming: false,
            },
            retention: SemanticRetention::Canonical,
            deduplication_key: Some(format!("dedupe-{index}")),
        }
    }

    async fn open_conversation_subscription(
        duplex: &super::HostConnectDuplex,
        negotiated: NegotiatedParameters,
        client: ClientId,
        task_id: TaskId,
        after_sequence: u64,
    ) -> (SubscriptionId, crate::domain::snapshot::SemanticJournalPage) {
        use crate::domain::query::{Query, QueryEnvelope, QueryOutcome, QueryResult};

        let reply = duplex
            .requests
            .execute(
                negotiated,
                ClientRequest::Query(QueryEnvelope {
                    request_id: RequestId::new(),
                    client_id: client,
                    task_id: Some(task_id),
                    query: Query::TaskCockpit(TaskCockpitQuery::OpenConversationSubscription {
                        after_sequence,
                    }),
                }),
            )
            .await
            .expect("open conversation subscription");
        match reply {
            ServerMessage::QueryReply(crate::domain::query::QueryReply {
                outcome:
                    QueryOutcome::Ok(QueryResult::TaskCockpit(
                        TaskCockpitResult::ConversationSubscription {
                            subscription_id,
                            page,
                        },
                    )),
                ..
            }) => (subscription_id, page),
            other => panic!("expected ConversationSubscription, got {other:?}"),
        }
    }

    #[test]
    fn ephemeral_conversation_dirty_materializer_coalesces_newest_high_water() {
        let subscription_id = SubscriptionId::new();
        let task_id = TaskId::new();
        let (handle, mut ports) = ConnectionOutputHandle::new(1, 1, 1);
        handle.reserve_ephemeral_key(EphemeralKey::Conversation(subscription_id));
        let generation = Arc::new(AtomicU64::new(1));
        for high_water in [1u64, 50, 200] {
            let flag = Arc::clone(&generation);
            let result =
                handle.try_admit_conversation_dirty(subscription_id, task_id, 1, flag, high_water);
            if high_water == 1 {
                assert!(matches!(result, EphemeralAdmitResult::Queued));
            } else {
                assert!(matches!(result, EphemeralAdmitResult::Coalesced));
            }
        }
        assert_eq!(handle.ephemeral_slots_occupied(), 1);
        let outbound = ports.try_recv_prioritized().expect("one dirty token");
        let PrioritizedOutbound::Ephemeral(ephemeral) = outbound else {
            panic!("expected ephemeral");
        };
        assert!(matches!(
            ephemeral.message(),
            ServerMessage::ConversationDirty {
                subscription_id: id,
                task_id: dirty_task,
                high_water: 200,
            } if *id == subscription_id && *dirty_task == task_id
        ));
        PrioritizedOutbound::Ephemeral(ephemeral).after_successful_write();
        assert_eq!(handle.ephemeral_slots_occupied(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ephemeral_capacity_return_notifies_so_conversation_dirty_recovers() {
        use std::pin::pin;

        let subscription_id = SubscriptionId::new();
        let task_id = TaskId::new();
        let notify = Arc::new(Notify::new());
        let (handle, mut ports) = ConnectionOutputHandle::new(1, 1, 1);
        handle.attach_ephemeral_capacity_notify(Arc::clone(&notify));
        let stream = sample_stream_key(0x55);
        assert!(matches!(
            handle.try_admit_ephemeral_stream(
                stream,
                1,
                Arc::new(move || Some(sample_stream_frame(stream, 1, 1, 9))),
            ),
            EphemeralAdmitResult::Queued
        ));
        // Unreserved conversation competes for the single stream budget → drop.
        let generation = Arc::new(AtomicU64::new(1));
        assert!(matches!(
            handle.try_admit_conversation_dirty(
                subscription_id,
                task_id,
                1,
                Arc::clone(&generation),
                42,
            ),
            EphemeralAdmitResult::CapacityDrop
        ));

        let stream_out = ports.try_recv_prioritized().expect("stream token");
        let mut notified = pin!(notify.notified());
        notified.as_mut().enable();
        stream_out.after_successful_write();
        notified.await;

        handle.reserve_ephemeral_key(EphemeralKey::Conversation(subscription_id));
        assert!(matches!(
            handle.try_admit_conversation_dirty(
                subscription_id,
                task_id,
                1,
                Arc::clone(&generation),
                42,
            ),
            EphemeralAdmitResult::Queued
        ));
        let dirty = ports
            .try_recv_prioritized()
            .expect("dirty after capacity return");
        let PrioritizedOutbound::Ephemeral(ephemeral) = dirty else {
            panic!("expected ephemeral dirty");
        };
        assert!(matches!(
            ephemeral.message(),
            ServerMessage::ConversationDirty {
                subscription_id: id,
                task_id: dirty_task,
                high_water: 42,
            } if *id == subscription_id && *dirty_task == task_id
        ));
        PrioritizedOutbound::Ephemeral(ephemeral).after_successful_write();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn remote_setup_queries_require_global_scope_and_bound_local_controller() {
        use crate::domain::query::{Query, QueryEnvelope, QueryError};
        let directory = tempfile::tempdir().expect("tempdir");
        let bus = crate::kernel::CommandBus::open(&directory.path().join("remote-setup-query.db"))
            .expect("bus");
        let (requests, executor) = HostRequestExecutor::start_without_automatic_maintenance(bus);
        let client = ClientId::new();
        let negotiated = conversation_wake_negotiated(client);
        for (task_id, expected) in [
            (
                None,
                QueryError::Unavailable {
                    reason: "remote_setup",
                },
            ),
            (Some(TaskId::new()), QueryError::InvalidRequest),
        ] {
            let reply = requests
                .execute(
                    negotiated,
                    ClientRequest::Query(QueryEnvelope {
                        request_id: RequestId::new(),
                        client_id: client,
                        task_id,
                        query: Query::TaskCockpit(TaskCockpitQuery::RemoteAccess(
                            super::super::remote_setup::RemoteSetupRequest::Snapshot,
                        )),
                    }),
                )
                .await
                .expect("bounded host reply");
            assert!(matches!(reply, ServerMessage::QueryReply(QueryReply {
                outcome: QueryOutcome::Err(error), ..
            }) if error == expected));
        }
        drop(requests);
        executor.abort();
        let _ = executor.await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn conversation_subscription_executor_race_record_before_and_after_register() {
        use crate::domain::query::{Query, QueryEnvelope, QueryOutcome, QueryResult};
        use crate::kernel::CommandBus;
        use crate::remote::presentation::SemanticJournalStore;
        use std::sync::Arc;

        let dir = tempfile::tempdir().expect("tempdir");
        let mut bus = CommandBus::open(&dir.path().join("conv-race.db")).expect("bus");
        let client = ClientId::new();
        let task_id = create_task_for_conversation_wake(&mut bus, client);
        let (requests, executor) = HostRequestExecutor::start_without_automatic_maintenance(bus);
        let negotiated = conversation_wake_negotiated(client);
        let journal = Arc::new(Mutex::new(SemanticJournalStore::default()));
        requests
            .install_test_semantic_journal(Arc::clone(&journal))
            .await
            .expect("install journal");

        let mut duplex = requests
            .open_connect_duplex(client)
            .await
            .expect("open duplex");

        // Order A: producer marks before open/register — catch-up must surface.
        let before = requests
            .record_test_semantic(sample_semantic_draft(task_id, 1))
            .await
            .expect("record before");
        assert_eq!(before, 1);
        let (subscription_a, page_a) =
            open_conversation_subscription(&duplex, negotiated, client, task_id, 0).await;
        assert!(page_a.high_water >= 1);
        // Drain any catch-up dirty from open.
        while let Some(outbound) = duplex.ports.try_recv_prioritized() {
            if matches!(outbound.message(), ServerMessage::ConversationDirty { .. }) {
                outbound.after_successful_write();
            } else {
                break;
            }
        }

        // Order B: register first, then hundreds of records coalesce to newest.
        let release = duplex
            .requests
            .execute(
                negotiated,
                ClientRequest::Query(QueryEnvelope {
                    request_id: RequestId::new(),
                    client_id: client,
                    task_id: Some(task_id),
                    query: Query::TaskCockpit(TaskCockpitQuery::ReleaseConversationSubscription {
                        subscription_id: subscription_a,
                    }),
                }),
            )
            .await
            .expect("release");
        assert!(matches!(
            release,
            ServerMessage::QueryReply(crate::domain::query::QueryReply {
                outcome: QueryOutcome::Ok(QueryResult::TaskCockpit(
                    TaskCockpitResult::ConversationSubscriptionReleased { .. }
                )),
                ..
            })
        ));

        let (subscription_b, page_b) =
            open_conversation_subscription(&duplex, negotiated, client, task_id, page_a.high_water)
                .await;
        assert_eq!(page_b.high_water, page_a.high_water);
        let mut highest = page_b.high_water;
        for index in 2..=201u64 {
            highest = requests
                .record_test_semantic(sample_semantic_draft(task_id, index))
                .await
                .expect("record burst");
        }
        // Allow executor dirty fan-out.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        let mut saw_dirty = None;
        for _ in 0..32 {
            if let Some(outbound) = duplex.ports.try_recv_prioritized() {
                match outbound.message() {
                    ServerMessage::ConversationDirty {
                        subscription_id,
                        task_id: dirty_task,
                        high_water,
                    } if *subscription_id == subscription_b && *dirty_task == task_id => {
                        saw_dirty = Some(*high_water);
                        outbound.after_successful_write();
                        break;
                    }
                    _ => outbound.after_successful_write(),
                }
            } else {
                tokio::task::yield_now().await;
            }
        }
        let dirty_hw = saw_dirty.expect("coalesced ConversationDirty after burst");
        assert_eq!(dirty_hw, highest);

        let page_reply = duplex
            .requests
            .execute(
                negotiated,
                ClientRequest::Query(QueryEnvelope {
                    request_id: RequestId::new(),
                    client_id: client,
                    task_id: Some(task_id),
                    query: Query::TaskCockpit(TaskCockpitQuery::Conversation {
                        after_sequence: page_b.high_water,
                    }),
                }),
            )
            .await
            .expect("page after dirty");
        match page_reply {
            ServerMessage::QueryReply(crate::domain::query::QueryReply {
                outcome:
                    QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Conversation(page))),
                ..
            }) => {
                assert_eq!(page.high_water, highest);
                assert!(
                    !page.facts.is_empty(),
                    "page after coalesced wake must recover recorded facts"
                );
            }
            other => panic!("expected Conversation page, got {other:?}"),
        }

        drop(duplex);
        drop(requests);
        executor.abort();
        let _ = executor.await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn conversation_subscription_release_detach_and_foreign_scope_fail() {
        use crate::domain::query::{Query, QueryEnvelope, QueryOutcome, QueryResult};
        use crate::kernel::CommandBus;
        use crate::remote::presentation::SemanticJournalStore;

        let dir = tempfile::tempdir().expect("tempdir");
        let mut bus = CommandBus::open(&dir.path().join("conv-release.db")).expect("bus");
        let client = ClientId::new();
        let task_id = create_task_for_conversation_wake(&mut bus, client);
        let (requests, executor) = HostRequestExecutor::start_without_automatic_maintenance(bus);
        let negotiated = conversation_wake_negotiated(client);
        requests
            .install_test_semantic_journal(Arc::new(Mutex::new(SemanticJournalStore::default())))
            .await
            .expect("install journal");
        let mut duplex = requests
            .open_connect_duplex(client)
            .await
            .expect("open duplex");
        let output_id = duplex.registration.id();
        let (subscription_id, _) =
            open_conversation_subscription(&duplex, negotiated, client, task_id, 0).await;

        // Stall stream lane: fill non-reserved capacity. Reserved conversation
        // coalescer must still admit the final dirty without waiting for the
        // stream write to complete.
        let stream = sample_stream_key(0x44);
        assert!(matches!(
            duplex.output.try_admit_ephemeral_stream(
                stream,
                1,
                Arc::new(move || Some(sample_stream_frame(stream, 1, 1, 1))),
            ),
            EphemeralAdmitResult::Queued
        ));
        let highest = requests
            .record_test_semantic(sample_semantic_draft(task_id, 1))
            .await
            .expect("dirty while stream stalled");
        let (saw_dirty, stalled_stream) = tokio::time::timeout(Duration::from_secs(3), async {
            let mut stalled_stream = None;
            loop {
                let outbound = duplex
                    .ports
                    .recv_prioritized()
                    .await
                    .expect("live output while subscription is attached");
                match outbound.message() {
                    ServerMessage::ConversationDirty {
                        subscription_id: id,
                        task_id: dirty_task,
                        high_water,
                    } if *id == subscription_id && *dirty_task == task_id => {
                        let delivered_high_water = *high_water;
                        outbound.after_successful_write();
                        return (delivered_high_water, stalled_stream);
                    }
                    ServerMessage::Stream(_) => {
                        // Hold the preexisting stream in-flight, but keep
                        // polling until the executor consumes the dirty watch
                        // edge. Its record ack precedes that select turn.
                        assert!(stalled_stream.is_none(), "one stalled stream");
                        stalled_stream = Some(outbound);
                    }
                    _ => outbound.after_successful_write(),
                }
            }
        })
        .await
        .expect("reserved conversation dirty before stream capacity returns");
        assert_eq!(
            saw_dirty, highest,
            "reserved conversation slot must deliver final notice while stream lane is stalled"
        );
        assert!(stalled_stream.is_some(), "stream stayed in-flight");
        drop(stalled_stream);

        // Release must invalidate queued conversation generation even if a notice
        // was coalesced onto the lane.
        let released = duplex
            .requests
            .execute(
                negotiated,
                ClientRequest::Query(QueryEnvelope {
                    request_id: RequestId::new(),
                    client_id: client,
                    task_id: Some(task_id),
                    query: Query::TaskCockpit(TaskCockpitQuery::ReleaseConversationSubscription {
                        subscription_id,
                    }),
                }),
            )
            .await
            .expect("release");
        assert!(matches!(
            released,
            ServerMessage::QueryReply(crate::domain::query::QueryReply {
                outcome: QueryOutcome::Ok(QueryResult::TaskCockpit(
                    TaskCockpitResult::ConversationSubscriptionReleased { .. }
                )),
                ..
            })
        ));

        // Foreign owner / wrong task scope fails closed.
        let foreign = ClientId::new();
        let foreign_neg = conversation_wake_negotiated(foreign);
        let (subscription_id, _) =
            open_conversation_subscription(&duplex, negotiated, client, task_id, 0).await;
        let unauthorized = duplex
            .requests
            .execute(
                foreign_neg,
                ClientRequest::Query(QueryEnvelope {
                    request_id: RequestId::new(),
                    client_id: foreign,
                    task_id: Some(task_id),
                    query: Query::TaskCockpit(TaskCockpitQuery::ReleaseConversationSubscription {
                        subscription_id,
                    }),
                }),
            )
            .await
            .expect("foreign release");
        assert!(matches!(
            unauthorized,
            ServerMessage::QueryReply(crate::domain::query::QueryReply {
                outcome: QueryOutcome::Err(crate::domain::query::QueryError::Unauthorized),
                ..
            })
        ));

        // Detach removes subscriptions for the exact output.
        let detach = duplex
            .requests
            .execute(
                NegotiatedParameters {
                    version: crate::protocol::ProtocolVersion::current(),
                    client_id: client,
                    capabilities: CapabilitySet::from_capabilities([
                        Capability::TaskCockpit,
                        Capability::SemanticConversation,
                        Capability::ExplicitDetach,
                    ]),
                    limits: crate::protocol::FrameLimits::v1_default(),
                },
                ClientRequest::Detach(crate::protocol::DetachRequest {
                    request_id: RequestId::new(),
                    client_id: client,
                    connection_id: output_id.as_uuid(),
                }),
            )
            .await
            .expect("detach");
        assert!(matches!(detach, ServerMessage::Detached(_)));

        drop(duplex);
        drop(requests);
        executor.abort();
        let _ = executor.await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn conversation_subscription_stalled_writer_recovers_on_capacity_return() {
        use crate::kernel::CommandBus;
        use crate::remote::presentation::SemanticJournalStore;

        let dir = tempfile::tempdir().expect("tempdir");
        let mut bus = CommandBus::open(&dir.path().join("conv-capacity.db")).expect("bus");
        let client = ClientId::new();
        let task_id = create_task_for_conversation_wake(&mut bus, client);
        let (requests, executor) = HostRequestExecutor::start_without_automatic_maintenance(bus);
        let negotiated = conversation_wake_negotiated(client);
        requests
            .install_test_semantic_journal(Arc::new(Mutex::new(SemanticJournalStore::default())))
            .await
            .expect("install journal");
        let mut duplex = requests
            .open_connect_duplex(client)
            .await
            .expect("open duplex");
        let (subscription_id, _) =
            open_conversation_subscription(&duplex, negotiated, client, task_id, 0).await;

        let stream = sample_stream_key(0x66);
        assert!(matches!(
            duplex.output.try_admit_ephemeral_stream(
                stream,
                1,
                Arc::new(move || Some(sample_stream_frame(stream, 1, 1, 3))),
            ),
            EphemeralAdmitResult::Queued
        ));
        let first = requests
            .record_test_semantic(sample_semantic_draft(task_id, 1))
            .await
            .expect("first dirty");
        // Hold the stream in-flight and await the dirty watch edge, rather
        // than assuming the record-control ack also completed fanout.
        let stream_out = tokio::time::timeout(Duration::from_secs(3), async {
            let mut stream_out = None;
            loop {
                let outbound = duplex
                    .ports
                    .recv_prioritized()
                    .await
                    .expect("live output while subscription is attached");
                match outbound.message() {
                    ServerMessage::ConversationDirty {
                        subscription_id: id,
                        task_id: dirty_task,
                        high_water,
                    } if *id == subscription_id
                        && *dirty_task == task_id
                        && *high_water == first =>
                    {
                        outbound.after_successful_write();
                        return stream_out;
                    }
                    ServerMessage::Stream(_) => {
                        assert!(stream_out.is_none(), "one stalled stream");
                        stream_out = Some(outbound);
                    }
                    _ => outbound.after_successful_write(),
                }
            }
        })
        .await
        .expect("reserved conversation dirty before stream capacity returns")
        .expect("stream stays in-flight until reserved dirty delivery");

        // Completing the stalled stream frees capacity and notifies the executor.
        stream_out.after_successful_write();

        let second = requests
            .record_test_semantic(sample_semantic_draft(task_id, 2))
            .await
            .expect("second dirty after capacity return");
        let saw_second = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let outbound = duplex
                    .ports
                    .recv_prioritized()
                    .await
                    .expect("live output while subscription is attached");
                match outbound.message() {
                    ServerMessage::ConversationDirty {
                        subscription_id: id,
                        task_id: dirty_task,
                        high_water,
                    } if *id == subscription_id && *dirty_task == task_id => {
                        let delivered_high_water = *high_water;
                        outbound.after_successful_write();
                        return delivered_high_water;
                    }
                    _ => outbound.after_successful_write(),
                }
            }
        })
        .await
        .expect("capacity-return dirty delivery");
        assert_eq!(
            saw_second, second,
            "capacity-return notify must let the next token deliver promptly"
        );

        drop(duplex);
        drop(requests);
        executor.abort();
        let _ = executor.await;
    }

    fn event_replay_negotiated(client: ClientId) -> NegotiatedParameters {
        use crate::protocol::{Capability, CapabilitySet, FrameLimits, ProtocolVersion};
        NegotiatedParameters {
            version: ProtocolVersion::current(),
            client_id: client,
            capabilities: CapabilitySet::from_capabilities([
                Capability::HostShutdown,
                Capability::EventReplay,
                Capability::OperationSettlement,
            ]),
            limits: FrameLimits::v1_default(),
        }
    }

    fn count_settled(path: &std::path::Path) -> i64 {
        let conn =
            rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .expect("ro");
        conn.query_row(
            "SELECT COUNT(*) FROM events WHERE event_type = 'operation.settled'",
            [],
            |row| row.get(0),
        )
        .expect("count")
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reconnect_handoff_rebinds_live_replay_to_new_output_without_reusing_id() {
        use crate::domain::query::{Query, QueryEnvelope, QueryError, QueryOutcome, QueryResult};
        use crate::kernel::CommandBus;

        let dir = tempfile::tempdir().expect("reconnect tempdir");
        let bus = CommandBus::open(&dir.path().join("reconnect-replay.db")).expect("bus");
        let (requests, executor) = HostRequestExecutor::start_without_automatic_maintenance(bus);
        let client = ClientId::new();
        let negotiated = event_replay_negotiated(client);

        let (old_output, mut old_ports) = ConnectionOutputHandle::new(4, 8, 1);
        let old_id = old_output.id();
        let old_registration = requests
            .register_output_for_connection(old_output, client, None)
            .await
            .expect("old registration");
        let old_handle = requests.with_output(old_id);
        let opened = old_handle
            .execute(
                negotiated.clone(),
                ClientRequest::Query(QueryEnvelope {
                    request_id: RequestId::new(),
                    client_id: client,
                    task_id: None,
                    query: Query::OpenEventReplay { after_sequence: 0 },
                }),
            )
            .await
            .expect("open replay");
        let subscription_id = match opened {
            ServerMessage::QueryReply(reply) => match reply.outcome {
                QueryOutcome::Ok(QueryResult::EventReplayPage {
                    subscription_id, ..
                }) => subscription_id,
                other => panic!("expected replay page, got {other:?}"),
            },
            other => panic!("expected query reply, got {other:?}"),
        };
        while let Some(outbound) = old_ports.try_recv_prioritized() {
            outbound.after_successful_write();
        }

        let (new_output, _new_ports) = ConnectionOutputHandle::new(4, 8, 1);
        let new_id = new_output.id();
        assert_ne!(old_id, new_id, "reconnect must never reuse physical IDs");
        let new_registration = requests
            .register_output_for_connection(new_output, client, Some(old_id.as_uuid()))
            .await
            .expect("new registration");
        assert_eq!(new_registration.id(), new_id);
        assert!(
            !requests
                .inspect_output(old_id)
                .await
                .expect("old inspection")
                .registered
        );
        assert!(
            requests
                .inspect_output(new_id)
                .await
                .expect("new inspection")
                .live_bound
        );

        let old_release = old_handle
            .execute(
                negotiated.clone(),
                ClientRequest::Query(QueryEnvelope {
                    request_id: RequestId::new(),
                    client_id: client,
                    task_id: None,
                    query: Query::ReleaseEventReplay { subscription_id },
                }),
            )
            .await
            .expect("old release response");
        assert!(matches!(
            old_release,
            ServerMessage::QueryReply(crate::domain::query::QueryReply {
                outcome: QueryOutcome::Err(QueryError::Unauthorized),
                ..
            })
        ));

        let new_release = requests
            .with_output(new_id)
            .execute(
                negotiated,
                ClientRequest::Query(QueryEnvelope {
                    request_id: RequestId::new(),
                    client_id: client,
                    task_id: None,
                    query: Query::ReleaseEventReplay { subscription_id },
                }),
            )
            .await
            .expect("new release response");
        assert!(matches!(
            new_release,
            ServerMessage::QueryReply(crate::domain::query::QueryReply {
                outcome: QueryOutcome::Ok(QueryResult::EventReplayReleased { .. }),
                ..
            })
        ));

        drop(old_registration);
        drop(new_registration);
        drop(requests);
        executor.abort();
        let _ = executor.await;
    }

    #[test]
    fn reconnect_handoff_preserves_frozen_replay_cursor_only_in_new_scope() {
        use crate::domain::command::CommandReceipt;
        use crate::kernel::CommandBus;

        let dir = tempfile::tempdir().expect("reconnect frozen tempdir");
        let mut bus = CommandBus::open(&dir.path().join("reconnect-frozen.db")).expect("bus");
        let client = ClientId::new();
        let project_id = ProjectId::new();
        for index in 0..2_u8 {
            let task_id = TaskId::new();
            let receipt = bus
                .execute_for_test(CommandEnvelope {
                    command_id: CommandId::new(),
                    client_id: client,
                    task_id: None,
                    issued_at_ms: 1_725_000_000_900 + i64::from(index),
                    expected_task_revision: None,
                    command: Command::CreateTask(CreateTaskIntent {
                        id: task_id,
                        environment_id: EnvironmentId::new(),
                        title: format!("frozen replay {index}"),
                        description: None,
                        project_id,
                        workspace: WorkspaceRef::Main,
                        assignment: TaskAssignment::LocalOwner,
                        created_at_ms: 1_725_000_000_900 + i64::from(index),
                        connectivity: TaskConnectivity::Connected,
                        attention: TaskAttention::None,
                        activity: TaskActivity::Idle,
                        review_readiness: ReviewReadiness::NotReady,
                    }),
                })
                .expect("create replay fixture task");
            assert!(matches!(receipt, CommandReceipt::Accepted { .. }));
        }

        let limits = PageLimits::new(1, 512 * 1024).expect("replay limits");
        let old_output = ConnectionOutputId::new();
        let new_output = ConnectionOutputId::new();
        let old_scope = SessionScope {
            client_id: Some(client),
            task_id: None,
            connection_id: Some(old_output.as_uuid()),
            action_epoch: None,
            runtime_generation: None,
        };
        let new_scope = SessionScope {
            connection_id: Some(new_output.as_uuid()),
            ..old_scope
        };
        let session = bus
            .begin_event_replay_scoped(0, limits, old_scope)
            .expect("frozen replay");
        let cursor = session
            .page(None)
            .expect("first page")
            .next_cursor
            .expect("bounded fixture should have a cursor");
        let subscription_id = session.subscription_id();

        let mut registry = EventReplayRegistry::new();
        registry
            .insert_open(
                client,
                session,
                limits,
                Some(LiveTail::new(old_output, 0)),
                true,
                std::time::Instant::now(),
            )
            .expect("registry admission");
        // A dropped old output removes only its live writer binding; the
        // frozen cursor remains reachable until the authenticated successor
        // explicitly rebinds it.
        registry.remove_for_output(old_output);
        registry.rebind_output(client, old_output, new_output);

        assert!(matches!(
            registry.get_frozen(
                subscription_id,
                client,
                old_scope,
                limits,
                std::time::Instant::now(),
            ),
            Err(crate::domain::query::QueryError::Unauthorized)
        ));
        let resumed = registry
            .get_frozen(
                subscription_id,
                client,
                new_scope,
                limits,
                std::time::Instant::now(),
            )
            .expect("new connection scope");
        assert!(!resumed
            .page(Some(&cursor))
            .expect("cursor continuation")
            .events
            .is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn supervised_ready_does_not_settle_until_arm_ack_then_exits_intentional() {
        use crate::domain::host::HostCleanupBranch;
        use crate::domain::id::CommandId;
        use crate::kernel::CommandBus;
        use crate::protocol::ServerMessage;

        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("arm-before-settle.db");
        let bus = CommandBus::open(&db_path).expect("bus");
        let (requests, mut supervised) =
            HostRequestExecutor::start_supervised_without_automatic_maintenance(bus);
        let (out, mut ports) = ConnectionOutputHandle::new(8, 8, 1);
        let reg = requests.register_output(out).await.expect("register");
        let client = ClientId::new();
        let negotiated = event_replay_negotiated(client);
        let handle = requests.with_output(reg.id());
        let inspection_id = inspection_id_for(&handle, negotiated.clone(), client).await;

        let completion = handle
            .execute_for_duplex(
                negotiated.clone(),
                confirm_quit_request(client, CommandId::new(), inspection_id),
            )
            .await
            .expect("duplex quit");
        let DuplexExecuteCompletion::ExecutorAdmittedQuitReceipt { operation_id } = completion
        else {
            panic!("expected admitted quit receipt");
        };
        // Complete receipt write so high-water can succeed.
        ports
            .try_recv_prioritized()
            .expect("receipt critical")
            .after_successful_write();

        // Open a live subscription so terminal CRITICAL fanout has a target.
        let open = handle
            .execute(
                negotiated,
                ClientRequest::Query(crate::domain::query::QueryEnvelope {
                    request_id: RequestId::new(),
                    client_id: client,
                    task_id: None,
                    query: crate::domain::query::Query::OpenEventReplay { after_sequence: 0 },
                }),
            )
            .await
            .expect("open replay");
        let subscription_id = match open {
            ServerMessage::QueryReply(reply) => match reply.outcome {
                crate::domain::query::QueryOutcome::Ok(
                    crate::domain::query::QueryResult::EventReplayPage {
                        subscription_id, ..
                    },
                ) => subscription_id,
                other => panic!("expected EventReplayPage, got {other:?}"),
            },
            other => panic!("expected QueryReply, got {other:?}"),
        };
        // Drain any catch-up durables from open.
        while let Some(outbound) = ports.try_recv_prioritized() {
            outbound.after_successful_write();
        }

        for _ in HostCleanupBranch::ORDER {
            requests.run_maintenance_once().await.expect("branch");
            while let Some(outbound) = ports.try_recv_prioritized() {
                outbound.after_successful_write();
            }
        }
        assert_eq!(count_settled(&db_path), 0);

        // Kick ReadyToExit → arm without acking yet.
        let maintenance = tokio::spawn({
            let requests = requests.clone();
            async move { requests.run_maintenance_once().await }
        });
        let arm = supervised
            .arm_rx
            .recv()
            .await
            .expect("arm request before settle");
        assert_eq!(arm.operation_id, operation_id);
        assert_eq!(arm.action_epoch, 1);
        assert_eq!(
            count_settled(&db_path),
            0,
            "must not settle before arm acknowledgement"
        );

        // Writer drain task so terminal high-water can complete after settle.
        let drain = tokio::spawn(async move {
            while let Some(outbound) = ports.recv_prioritized().await {
                outbound.after_successful_write();
            }
        });

        arm.ack.send(()).expect("ack arm");
        maintenance
            .await
            .expect("maintenance join")
            .expect("maintenance ok");
        let outcome = supervised.join.await.expect("join").expect("intentional");
        assert_eq!(
            outcome,
            super::HostExecutorOutcome::Intentional {
                operation_id,
                action_epoch: 1,
            }
        );
        assert_eq!(count_settled(&db_path), 1);
        let _ = drain.await;
        let _ = subscription_id;
        drop(reg);
        drop(requests);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn supervised_terminal_critical_fanout_high_water_receipt_only_and_live_watcher() {
        use crate::domain::command::CommandReceipt;
        use crate::domain::event::Event;
        use crate::domain::host::HostCleanupBranch;
        use crate::domain::id::CommandId;
        use crate::kernel::CommandBus;
        use crate::protocol::ServerMessage;
        use std::collections::BTreeSet;

        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("terminal-fanout.db");
        let bus = CommandBus::open(&db_path).expect("bus");
        let (requests, mut supervised) =
            HostRequestExecutor::start_supervised_without_automatic_maintenance(bus);

        // Receipt-only initiator: dequeue but do not physically complete until after arm.
        let (receipt_out, mut receipt_ports) = ConnectionOutputHandle::new(8, 8, 1);
        let receipt_reg = requests
            .register_output(receipt_out)
            .await
            .expect("register receipt");
        let receipt_client = ClientId::new();
        let receipt_neg = host_shutdown_negotiated(receipt_client);
        let receipt_handle = requests.with_output(receipt_reg.id());
        let inspection_id =
            inspection_id_for(&receipt_handle, receipt_neg.clone(), receipt_client).await;
        let completion = receipt_handle
            .execute_for_duplex(
                receipt_neg,
                confirm_quit_request(receipt_client, CommandId::new(), inspection_id),
            )
            .await
            .expect("quit");
        let DuplexExecuteCompletion::ExecutorAdmittedQuitReceipt { operation_id } = completion
        else {
            panic!("expected admitted quit");
        };
        let receipt_frame = receipt_ports
            .try_recv_prioritized()
            .expect("receipt critical");
        assert!(matches!(
            receipt_frame.message(),
            ServerMessage::CommandReceipt(CommandReceipt::Accepted { .. })
        ));
        // Hold receipt_frame until after arm (pending high-water).

        // Live-only watcher with TWO subscriptions on one output.
        let (live_out, mut live_ports) = ConnectionOutputHandle::new(8, 8, 1);
        let live_reg = requests
            .register_output(live_out)
            .await
            .expect("register live");
        let live_client = ClientId::new();
        let live_neg = event_replay_negotiated(live_client);
        let live_handle = requests.with_output(live_reg.id());
        let mut live_subs = Vec::new();
        for _ in 0..2 {
            let open = live_handle
                .execute(
                    live_neg.clone(),
                    ClientRequest::Query(crate::domain::query::QueryEnvelope {
                        request_id: RequestId::new(),
                        client_id: live_client,
                        task_id: None,
                        query: crate::domain::query::Query::OpenEventReplay { after_sequence: 0 },
                    }),
                )
                .await
                .expect("open");
            let sub = match open {
                ServerMessage::QueryReply(reply) => match reply.outcome {
                    crate::domain::query::QueryOutcome::Ok(
                        crate::domain::query::QueryResult::EventReplayPage {
                            subscription_id, ..
                        },
                    ) => subscription_id,
                    other => panic!("unexpected {other:?}"),
                },
                other => panic!("unexpected {other:?}"),
            };
            live_subs.push(sub);
            while let Some(outbound) = live_ports.try_recv_prioritized() {
                outbound.after_successful_write();
            }
        }
        assert_eq!(live_subs.len(), 2);
        assert_ne!(live_subs[0], live_subs[1]);

        for _ in HostCleanupBranch::ORDER {
            requests.run_maintenance_once().await.expect("branch");
            while let Some(outbound) = live_ports.try_recv_prioritized() {
                outbound.after_successful_write();
            }
        }
        assert!(receipt_ports.try_recv_prioritized().is_none());

        let maintenance = tokio::spawn({
            let requests = requests.clone();
            async move { requests.run_maintenance_once().await }
        });
        let arm = supervised.arm_rx.recv().await.expect("arm");
        assert_eq!(arm.operation_id, operation_id);

        let expected_subs: BTreeSet<_> = live_subs.iter().copied().collect();
        let receipt_drain = tokio::spawn(async move {
            let mut saw_terminal = false;
            // Complete the held receipt after arm (receipt-only high-water).
            receipt_frame.after_successful_write();
            while let Some(outbound) = receipt_ports.recv_prioritized().await {
                if matches!(
                    outbound.message(),
                    ServerMessage::DurableEvent {
                        event: DomainEvent {
                            payload: Event::OperationSettled(_),
                            ..
                        },
                        ..
                    }
                ) {
                    saw_terminal = true;
                }
                outbound.after_successful_write();
            }
            saw_terminal
        });
        let live_drain = tokio::spawn(async move {
            let mut terminal_subs = BTreeSet::new();
            while let Some(outbound) = live_ports.recv_prioritized().await {
                if let ServerMessage::DurableEvent {
                    subscription_id,
                    event:
                        DomainEvent {
                            payload: Event::OperationSettled(fact),
                            ..
                        },
                } = outbound.message()
                {
                    assert_eq!(fact.operation_id, operation_id);
                    assert_eq!(fact.action_epoch, Some(1));
                    terminal_subs.insert(*subscription_id);
                }
                outbound.after_successful_write();
            }
            terminal_subs
        });

        arm.ack.send(()).expect("ack");
        maintenance.await.expect("join").expect("ok");
        let outcome = supervised.join.await.expect("exec").expect("intentional");
        assert_eq!(
            outcome,
            super::HostExecutorOutcome::Intentional {
                operation_id,
                action_epoch: 1,
            }
        );
        assert!(
            !receipt_drain.await.expect("receipt drain"),
            "receipt-only output must not receive a terminal CRITICAL"
        );
        assert_eq!(
            live_drain.await.expect("live drain"),
            expected_subs,
            "two live subscriptions require two distinct terminal CRITICAL frames"
        );
        drop(receipt_reg);
        drop(live_reg);
        drop(requests);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn supervised_terminal_flushes_ordered_durables_before_settlement_with_slow_output_isolation(
    ) {
        use crate::domain::event::Event;
        use crate::domain::host::HostCleanupBranch;
        use crate::domain::id::CommandId;
        use crate::kernel::CommandBus;
        use crate::protocol::ServerMessage;

        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("ordered-durable-fence.db");
        let bus = CommandBus::open(&db_path).expect("bus");
        let (requests, mut supervised) =
            HostRequestExecutor::start_supervised_without_automatic_maintenance(bus);

        let (healthy_out, mut healthy_ports) = ConnectionOutputHandle::new(8, 8, 1);
        let healthy_reg = requests
            .register_output(healthy_out)
            .await
            .expect("register healthy");
        let healthy_client = ClientId::new();
        let healthy_neg = event_replay_negotiated(healthy_client);
        let healthy_handle = requests.with_output(healthy_reg.id());
        let inspection_id =
            inspection_id_for(&healthy_handle, healthy_neg.clone(), healthy_client).await;
        let completion = healthy_handle
            .execute_for_duplex(
                healthy_neg.clone(),
                confirm_quit_request(healthy_client, CommandId::new(), inspection_id),
            )
            .await
            .expect("quit");
        let DuplexExecuteCompletion::ExecutorAdmittedQuitReceipt { operation_id } = completion
        else {
            panic!("expected admitted quit");
        };
        healthy_ports
            .try_recv_prioritized()
            .expect("receipt")
            .after_successful_write();

        let (slow_out, mut slow_ports) = ConnectionOutputHandle::new(8, 8, 1);
        let slow_probe = slow_out.clone();
        let slow_reg = requests
            .register_output(slow_out)
            .await
            .expect("register slow");
        let slow_client = ClientId::new();
        let slow_neg = event_replay_negotiated(slow_client);
        let slow_handle = requests.with_output(slow_reg.id());

        let open_healthy = healthy_handle
            .execute(
                healthy_neg,
                ClientRequest::Query(crate::domain::query::QueryEnvelope {
                    request_id: RequestId::new(),
                    client_id: healthy_client,
                    task_id: None,
                    query: crate::domain::query::Query::OpenEventReplay { after_sequence: 0 },
                }),
            )
            .await
            .expect("open healthy");
        assert!(matches!(open_healthy, ServerMessage::QueryReply(_)));
        while let Some(outbound) = healthy_ports.try_recv_prioritized() {
            outbound.after_successful_write();
        }

        let open_slow = slow_handle
            .execute(
                slow_neg,
                ClientRequest::Query(crate::domain::query::QueryEnvelope {
                    request_id: RequestId::new(),
                    client_id: slow_client,
                    task_id: None,
                    query: crate::domain::query::Query::OpenEventReplay { after_sequence: 0 },
                }),
            )
            .await
            .expect("open slow");
        assert!(matches!(open_slow, ServerMessage::QueryReply(_)));
        while let Some(outbound) = slow_ports.try_recv_prioritized() {
            outbound.after_successful_write();
        }

        // Admit four HostCleanupBranchCompleted durables; leave them queued (unacked).
        for _ in HostCleanupBranch::ORDER {
            requests.run_maintenance_once().await.expect("branch");
        }

        let (settled_tx, settled_rx) = tokio::sync::oneshot::channel();
        let healthy_drain = tokio::spawn(async move {
            let mut branch_sequences = Vec::new();
            let mut saw_settled = false;
            let mut settled_signal = Some(settled_tx);
            while let Some(outbound) = healthy_ports.recv_prioritized().await {
                match outbound.message() {
                    ServerMessage::DurableEvent {
                        event:
                            DomainEvent {
                                sequence,
                                payload: Event::HostCleanupBranchCompleted { .. },
                                ..
                            },
                        ..
                    } => {
                        assert!(
                            !saw_settled,
                            "HostCleanupBranchCompleted must precede OperationSettled"
                        );
                        branch_sequences.push(*sequence);
                    }
                    ServerMessage::DurableEvent {
                        event:
                            DomainEvent {
                                payload: Event::OperationSettled(fact),
                                ..
                            },
                        ..
                    } => {
                        assert_eq!(fact.operation_id, operation_id);
                        assert_eq!(fact.action_epoch, Some(1));
                        saw_settled = true;
                        if let Some(tx) = settled_signal.take() {
                            let _ = tx.send(());
                        }
                    }
                    _ => {}
                }
                outbound.after_successful_write();
            }
            (branch_sequences, saw_settled)
        });

        let started = std::time::Instant::now();
        let maintenance = tokio::spawn({
            let requests = requests.clone();
            async move { requests.run_maintenance_once().await }
        });
        let arm = supervised.arm_rx.recv().await.expect("arm");
        assert_eq!(arm.operation_id, operation_id);
        arm.ack.send(()).expect("ack");

        // Healthy output must settle promptly; slow output must still be fencing.
        tokio::time::timeout(Duration::from_secs(2), settled_rx)
            .await
            .expect("healthy OperationSettled must arrive well before the 5s slow-output deadline")
            .expect("settled signal");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "healthy settlement must not wait on the stalled output"
        );
        assert!(
            !maintenance.is_finished(),
            "executor/maintenance path must still be pending while slow output fences"
        );
        assert!(
            !supervised.join.is_finished(),
            "supervised executor must still be pending while slow output fences"
        );

        maintenance.await.expect("join").expect("ok");
        let outcome = supervised.join.await.expect("exec").expect("intentional");
        assert_eq!(
            outcome,
            super::HostExecutorOutcome::Intentional {
                operation_id,
                action_epoch: 1,
            }
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(6),
            "host/executor must exit within the one global terminal bound"
        );

        let (branch_sequences, saw_settled) = healthy_drain.await.expect("healthy drain");
        assert_eq!(
            branch_sequences.len(),
            HostCleanupBranch::ORDER.len(),
            "healthy output must physically write all admitted branch durables"
        );
        assert!(
            branch_sequences.windows(2).all(|w| w[0] < w[1]),
            "branch durables must be written in increasing sequence: {branch_sequences:?}"
        );
        assert!(
            saw_settled,
            "healthy output must receive OperationSettled CRITICAL after ordered durables"
        );

        assert!(
            slow_probe.is_shutdown_requested(),
            "slow output must be shut down after durable fence deadline"
        );
        let mut slow_saw_settled = false;
        while let Some(outbound) = slow_ports.try_recv_prioritized() {
            if matches!(
                outbound.message(),
                ServerMessage::DurableEvent {
                    event: DomainEvent {
                        payload: Event::OperationSettled(_),
                        ..
                    },
                    ..
                }
            ) {
                slow_saw_settled = true;
            }
        }
        assert!(
            !slow_saw_settled,
            "slow output must never receive OperationSettled after skipped durable history"
        );

        drop(healthy_reg);
        drop(slow_reg);
        drop(requests);
    }

    #[test]
    fn authenticated_command_gate_rejects_journal_source_and_requires_provider_capability() {
        use crate::domain::command::{Command, SubmitProviderInputIntent};
        use crate::domain::provider_input::SettleProviderWaitIntent;
        use crate::domain::{
            AgentSessionId, ApprovalId, CommandId, PresentProviderApprovalIntent,
            PresentProviderQuestionIntent, ProviderInputAction, ProviderWaitFence, QuestionId,
            TaskId, TurnId,
        };
        use crate::protocol::{Capability, CapabilitySet};

        let question = Command::PresentProviderQuestion(
            PresentProviderQuestionIntent::try_new(
                AgentSessionId::new(),
                1,
                TurnId::new(),
                1,
                QuestionId::new(),
            )
            .expect("question intent"),
        );
        let approval = Command::PresentProviderApproval(
            PresentProviderApprovalIntent::try_new(
                AgentSessionId::new(),
                1,
                TurnId::new(),
                1,
                ApprovalId::new(),
            )
            .expect("approval intent"),
        );
        let wait = Command::SettleProviderWait(
            SettleProviderWaitIntent::try_new(ProviderWaitFence::new(
                CommandId::new(),
                TaskId::new(),
                1,
                AgentSessionId::new(),
                1,
                TurnId::new(),
                None,
                None,
            ))
            .expect("wait intent"),
        );
        for command in [&question, &approval, &wait] {
            assert!(matches!(
                super::validate_authenticated_command_capability(
                    CapabilitySet::from_capabilities([Capability::ProviderInput]),
                    command,
                ),
                Err(crate::host::IpcError::UnsupportedCapability)
            ));
        }

        let provider = Command::SubmitProviderInput(
            SubmitProviderInputIntent::try_new(
                AgentSessionId::new(),
                1,
                TurnId::new(),
                1,
                None,
                None,
                ProviderInputAction::SendNow {
                    text: "input".into(),
                    wait: false,
                    images: Vec::new(),
                },
            )
            .expect("provider intent"),
        );
        assert!(matches!(
            super::validate_authenticated_command_capability(CapabilitySet::empty(), &provider),
            Err(crate::host::IpcError::UnsupportedCapability)
        ));
        assert!(super::validate_authenticated_command_capability(
            CapabilitySet::from_capabilities([Capability::ProviderInput]),
            &provider,
        )
        .is_ok());
    }

    /// Behavioural coverage of the `terminal.open_shell` host path.
    ///
    /// Everything below goes through a real `HostRequestExecutor` and a real
    /// `TaskCockpitQuery::OpenShellTerminal` request, because the branches that
    /// matter (capability, client identity, cwd resolution, revision fencing)
    /// are all inside `dispatch_query` and `open_shell_terminal_request`, and a
    /// unit test of the resolver cannot reach any of them.
    ///
    /// No shell is spawned: the fixture has no `configured_service_runtime`, so
    /// an accepted open registers the durable resource and then records a
    /// "spawn refused" exit fact. The assertions stop at that boundary — the
    /// registered recipe — which is exactly what this task is responsible for.
    #[tokio::test(flavor = "current_thread")]
    async fn open_shell_terminal_query_resolves_cwd_and_fences_every_refusal() {
        use std::process::Command as ProcessCommand;

        use crate::domain::cockpit::TaskCockpitQuery;
        use crate::domain::command::{Command, CommandEnvelope, CreateTaskRequestIntent};
        use crate::domain::id::{CommandId, EnvironmentId, ProjectId, RequestId, TaskId};
        use crate::domain::query::{
            Query, QueryEnvelope, QueryError, QueryOutcome, QueryReply, QueryResult,
        };
        use crate::domain::resource::ResourceRecipe;
        use crate::domain::task::{
            ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity,
        };
        use crate::domain::{
            TaskCockpitDeniedReason, TaskCockpitResult, TaskCockpitSurface,
            TaskCockpitUnavailableReason,
        };
        use crate::kernel::CommandBus;
        use crate::protocol::{Capability, CapabilitySet, FrameLimits, ProtocolVersion};
        use crate::workspace::{WorkspaceProjectRoots, WorkspaceRequest};

        let repository = tempfile::tempdir().expect("repository");
        let output = ProcessCommand::new("git")
            .args(["init", "--quiet"])
            .current_dir(repository.path())
            .output()
            .expect("git init");
        assert!(output.status.success());
        let database = repository.path().join("open-shell-executor.sqlite");
        let project_id = ProjectId::new();
        let project_roots =
            WorkspaceProjectRoots::try_from_pairs([(project_id, repository.path().to_path_buf())])
                .expect("project roots");
        let client = ClientId::new();
        let stranger = ClientId::new();
        let cockpit = |capabilities| NegotiatedParameters {
            version: ProtocolVersion::current(),
            client_id: client,
            capabilities,
            limits: FrameLimits::v1_default(),
        };
        let negotiated = cockpit(CapabilitySet::from_capabilities([Capability::TaskCockpit]));
        let task_id = TaskId::new();
        let bus = CommandBus::open(&database).expect("bus");
        let (requests, executor) =
            HostRequestExecutor::start_without_automatic_maintenance_with_workspace_projects(
                bus,
                project_roots.clone(),
            );
        let (out, _ports) = ConnectionOutputHandle::new(8, 16, 1);
        let registration = requests.register_output(out).await.expect("register");
        let handle = requests.with_output(registration.id());

        // A task with no primary provider: the shell must not need one, and
        // this keeps the fixture free of a provider launch attempt.
        let created = handle
            .execute(
                negotiated.clone(),
                ClientRequest::Command(CommandEnvelope {
                    command_id: CommandId::new(),
                    client_id: client,
                    task_id: None,
                    issued_at_ms: 1_725_000_000_900,
                    expected_task_revision: None,
                    command: Command::CreateTaskV2(CreateTaskRequestIntent {
                        id: task_id,
                        environment_id: EnvironmentId::new(),
                        title: "Shell host".into(),
                        description: None,
                        project_id,
                        workspace: WorkspaceRequest::main(),
                        primary_provider: None,
                        defer_primary_provider_start: true,
                        assignment: TaskAssignment::LocalOwner,
                        created_at_ms: 1_725_000_000_900,
                        connectivity: TaskConnectivity::Connected,
                        attention: TaskAttention::None,
                        activity: TaskActivity::Idle,
                        review_readiness: ReviewReadiness::NotReady,
                    }),
                }),
            )
            .await;
        assert!(
            matches!(
                created,
                Ok(ServerMessage::CommandReceipt(
                    crate::domain::command::CommandReceipt::Accepted { .. }
                ))
            ),
            "fixture task must be created; got {created:?}"
        );

        let open = |negotiated: NegotiatedParameters,
                    client_id: ClientId,
                    cwd: Option<String>,
                    expected_task_revision: u64| {
            let request_id = RequestId::new();
            let request = ClientRequest::Query(QueryEnvelope {
                request_id,
                client_id,
                task_id: Some(task_id),
                query: Query::TaskCockpit(TaskCockpitQuery::OpenShellTerminal {
                    cwd,
                    expected_task_revision,
                }),
            });
            (request_id, negotiated, request)
        };
        let outcome = |reply: Result<ServerMessage, crate::host::IpcError>,
                       request_id: RequestId| match reply {
            Ok(ServerMessage::QueryReply(QueryReply {
                request_id: replied,
                outcome,
            })) => {
                assert_eq!(
                    replied, request_id,
                    "reply must answer the request it was sent"
                );
                outcome
            }
            other => panic!("expected a query reply; got {other:?}"),
        };

        // 1. Missing Capability::TaskCockpit is refused before any resolution.
        let (rid, neg, request) = open(cockpit(CapabilitySet::empty()), client, None, 1);
        assert_eq!(
            outcome(handle.execute(neg, request).await, rid),
            QueryOutcome::Err(QueryError::UnsupportedCapability)
        );

        // 2. A foreign client_id is refused: that id is written into the durable
        //    command, so a read may carry someone else's, a write may not.
        //    The executor's own connection-scope check fires first and refuses
        //    at the transport, ahead of dispatch_query's identity guard; either
        //    refusal is acceptable, silently proceeding is not.
        let (_rid, neg, request) = open(negotiated.clone(), stranger, None, 1);
        match handle.execute(neg, request).await {
            Err(crate::host::IpcError::Unauthorized) => {}
            Ok(ServerMessage::QueryReply(QueryReply {
                outcome: QueryOutcome::Err(QueryError::Unauthorized),
                ..
            })) => {}
            other => panic!("a foreign client_id must be refused; got {other:?}"),
        }

        // 3. A relative cwd names itself.
        let (rid, neg, request) = open(negotiated.clone(), client, Some("relative/dir".into()), 1);
        match outcome(handle.execute(neg, request).await, rid) {
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Denied {
                surface,
                reason,
                detail,
            })) => {
                assert_eq!(surface, TaskCockpitSurface::Terminal);
                assert_eq!(reason, TaskCockpitDeniedReason::OutsideWorkspace);
                assert_eq!(
                    detail.as_deref(),
                    Some("cwd must be absolute: relative/dir"),
                    "the client must be told which of the two cwd mistakes it made"
                );
            }
            other => panic!("relative cwd must be denied; got {other:?}"),
        }

        // 4. An absolute cwd that is not a directory names the path.
        let missing = repository.path().join("no-such-directory");
        let (rid, neg, request) = open(
            negotiated.clone(),
            client,
            Some(missing.to_string_lossy().to_string()),
            1,
        );
        match outcome(handle.execute(neg, request).await, rid) {
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Denied {
                reason,
                detail,
                ..
            })) => {
                assert_eq!(reason, TaskCockpitDeniedReason::OutsideWorkspace);
                let detail = detail.expect("a named cwd refusal must carry its detail");
                assert!(
                    detail.starts_with("cwd is not a directory: "),
                    "unexpected detail: {detail}"
                );
                assert!(
                    detail.contains("no-such-directory"),
                    "the detail must name the path it rejected: {detail}"
                );
            }
            other => panic!("a missing cwd must be denied; got {other:?}"),
        }

        // 5. A stale expected_task_revision is fenced. Revision 1 is the task's
        //    creation; anything below it is a client that has fallen behind.
        let (rid, neg, request) = open(negotiated.clone(), client, None, 0);
        match outcome(handle.execute(neg, request).await, rid) {
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Denied {
                reason,
                ..
            })) => assert_eq!(reason, TaskCockpitDeniedReason::RevisionConflict),
            other => panic!("a stale revision must be fenced; got {other:?}"),
        }

        // 6. cwd omitted: the workspace runtime working directory is used.
        //    On a machine with no resolvable shell this is a named
        //    TerminalUnavailable instead; both outcomes are asserted so the
        //    test cannot pass by accident on either.
        let revision = {
            let bus = CommandBus::open(&database).expect("revision bus");
            let revision = bus
                .task_snapshot(task_id)
                .expect("snapshot")
                .expect("created task")
                .task
                .revision;
            drop(bus);
            revision
        };
        let (rid, neg, request) = open(negotiated.clone(), client, None, revision);
        let fallback = outcome(handle.execute(neg, request).await, rid);
        let shell_resolved = match &fallback {
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::TaskTerminals(strip))) => {
                assert_eq!(strip.task_id, task_id);
                assert_eq!(strip.order.len(), 1, "one shell opened: {strip:?}");
                true
            }
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Unavailable {
                reason,
                detail,
                ..
            })) => {
                assert_eq!(*reason, TaskCockpitUnavailableReason::TerminalUnavailable);
                let detail = detail.as_deref().expect("a shell refusal must be named");
                assert!(
                    detail.starts_with("no shell found; tried: "),
                    "unexpected detail: {detail}"
                );
                false
            }
            other => panic!("workspace fallback must open or name why it did not; got {other:?}"),
        };

        drop(registration);
        drop(requests);
        executor.abort();
        let _ = executor.await;

        if shell_resolved {
            let bus = CommandBus::open(&database).expect("reopen bus");
            let snapshot = bus
                .task_snapshot(task_id)
                .expect("snapshot")
                .expect("created task");
            let resource = snapshot
                .resources
                .values()
                .find(|resource| {
                    matches!(resource.recipe, ResourceRecipe::Terminal { ref launch, .. } if launch.is_some())
                })
                .expect("the accepted shell must be registered");
            let ResourceRecipe::Terminal {
                launch: Some(launch),
                ..
            } = &resource.recipe
            else {
                panic!("shell resource must carry a launch: {resource:?}");
            };
            let expected = bus
                .load_task_runtime(task_id, &project_roots)
                .expect("load runtime")
                .expect("persisted task")
                .workspace
                .runtime_working_directory()
                .expect("workspace cwd");
            assert_eq!(
                launch.cwd, expected,
                "an omitted cwd must fall back to the Task's workspace directory"
            );
            assert!(
                !launch
                    .program
                    .to_string_lossy()
                    .to_ascii_lowercase()
                    .replace('/', "\\")
                    .ends_with("windowspowershell\\v1.0\\powershell.exe"),
                "the excluded shell must never be recorded: {launch:?}"
            );
        }
    }

    /// An authenticated client may not send `Command::OpenShellTerminal`.
    ///
    /// The command carries a client-chosen program/args/cwd and the kernel
    /// decider has no allowlist, so accepting one from the wire would let any
    /// authenticated client spawn an arbitrary binary on the host. This asserts
    /// the refusal through the real dispatch path, not just the gate function:
    /// `CommandBus::execute`'s own `HostAuthorityRequired` refusal is defence in
    /// depth behind it, and the live executor uses `execute_host_authorized`,
    /// which does not refuse it.
    #[test]
    fn dispatch_refuses_a_client_sent_open_shell_terminal_command() {
        use super::dispatch_authenticated_request;
        use crate::domain::command::{Command, CommandEnvelope, OpenShellTerminalIntent};
        use crate::domain::id::{CommandId, ResourceId, TaskId};
        use crate::domain::resource::{
            OwnerKind, ResourceFacts, ResourceKind, ResourceLifecycle, ResourceRecipe,
            TerminalLaunch,
        };
        use crate::domain::ClientId;
        use crate::kernel::CommandBus;
        use crate::protocol::{Capability, CapabilitySet, ClientRequest};

        let dir = tempfile::tempdir().expect("tempdir");
        let mut bus = CommandBus::open(&dir.path().join("open-shell-auth-gate.db")).expect("bus");
        let client = ClientId::new();
        let task_id = TaskId::new();
        let envelope = |command| {
            ClientRequest::Command(CommandEnvelope {
                command_id: CommandId::new(),
                client_id: client,
                task_id: Some(task_id),
                issued_at_ms: 1_725_000_000_000,
                expected_task_revision: Some(1),
                command,
            })
        };
        let open = Command::OpenShellTerminal(OpenShellTerminalIntent {
            resource: ResourceFacts {
                id: ResourceId::new(),
                task_id: Some(task_id),
                owner_kind: OwnerKind::Task,
                resource_kind: ResourceKind::Terminal,
                recipe: ResourceRecipe::Terminal {
                    cols: 120,
                    rows: 40,
                    launch: Some(TerminalLaunch {
                        cwd: std::path::PathBuf::from(if cfg!(windows) {
                            "C:/code"
                        } else {
                            "/code"
                        }),
                        program: std::path::PathBuf::from("attacker.exe"),
                        args: vec!["--payload".to_string()],
                    }),
                    title: None,
                },
                lifecycle: ResourceLifecycle::Active,
                runtime_generation: 1,
                updated_at_ms: 1_725_000_000_000,
            },
        });

        // Every capability a client can hold, including the full set: this is a
        // source refusal, not a missing bit.
        for capabilities in [
            CapabilitySet::empty(),
            CapabilitySet::from_capabilities([
                Capability::TaskCockpit,
                Capability::ProviderInput,
                Capability::HostShutdown,
                Capability::ServiceSupervisor,
            ]),
        ] {
            let refusal = dispatch_authenticated_request(
                client,
                capabilities,
                &mut bus,
                envelope(open.clone()),
            );
            assert!(
                matches!(refusal, Err(crate::host::IpcError::UnsupportedCapability)),
                "client-sent OpenShellTerminal must be refused; got {refusal:?}"
            );
        }

        // The gate function itself, so a future dispatch path cannot lose it.
        assert!(matches!(
            super::validate_authenticated_command_capability(
                CapabilitySet::from_capabilities([Capability::TaskCockpit]),
                &open,
            ),
            Err(crate::host::IpcError::UnsupportedCapability)
        ));

        // Control: a sibling terminal mutation on the same lane is NOT refused
        // by the gate, so the refusal above is specific to OpenShellTerminal
        // and not the whole terminal family failing closed.
        assert!(super::validate_authenticated_command_capability(
            CapabilitySet::empty(),
            &Command::CloseTerminal {
                resource_id: ResourceId::new(),
            },
        )
        .is_ok());
    }

    #[test]
    fn task_create_releases_request_lane_before_provider_observe() {
        let source = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/host/connection.rs"
        ));
        let start = source
            .find("async fn dispatch_task_create_with_primary_provider(")
            .expect("create-with-primary dispatch");
        let body = &source[start..];
        let end = body
            .find("/// Authenticated stock-provider effect.")
            .or_else(|| body.find("async fn dispatch_provider_start("))
            .expect("provider start follows create");
        let body = &body[..end];
        assert!(
            !body.contains("dispatch_provider_start("),
            "CreateTask must schedule provider launch off the request lane instead of awaiting dispatch_provider_start"
        );
        assert!(
            body.contains("start_provider_session_intent("),
            "CreateTask must schedule through start_provider_session_intent after durable facts"
        );
    }

    #[test]
    fn provider_start_schedules_observe_off_request_lane() {
        let source = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/host/connection.rs"
        ));
        let start = source
            .find("\n    fn start_provider_session_intent(")
            .expect("start_provider_session_intent");
        let body = &source[start..];
        let end = body
            .find("/// Archive one task after releasing")
            .or_else(|| body.find("fn dispatch_task_close("))
            .expect("task close follows provider start intent");
        let body = &body[..end];
        let schedule_at = body.find("provider_restore_jobs.push").expect(
            "direct provider-start must release the request lane after sync prepare/schedule",
        );
        let observe_at = body
            .find("observe_with_trusted_auth")
            .expect("provider start must still observe with trusted auth before launch");
        let blocking_worker_at = body
            .find("tokio::task::spawn_blocking")
            .expect("synchronous provider launch must run on the blocking worker lane");
        let launch_at = body
            .find("start_production_stock_provider_session")
            .expect("provider start must still launch the prepared session");
        assert!(
            observe_at > schedule_at,
            "trusted auth observe must run inside the scheduled future, not on the request-lane hot path"
        );
        assert!(
            blocking_worker_at > observe_at && launch_at > blocking_worker_at,
            "provider process launch must run inside spawn_blocking after async discovery"
        );
    }

    #[test]
    fn agent_connection_dispatch_is_cache_projection_not_probe_await() {
        let source = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/host/connection.rs"
        ));
        let start = source
            .find("async fn dispatch_agent_connection(")
            .expect("dispatch_agent_connection");
        let body = &source[start..];
        let end = body
            .find("\n    fn dispatch(")
            .expect("dispatch follows agent connection");
        let body = &body[..end];
        assert!(
            body.contains("project_agent_connection_from_authority"),
            "AgentConnection must project from provider-settings cache"
        );
        assert!(
            body.contains("maybe_schedule_provider_health"),
            "AgentConnection may schedule health futures, not await them"
        );
        assert!(
            !body.contains("probe_agents("),
            "production AgentConnection must not call probe_agents"
        );
        assert!(
            !body.contains("observe_with_trusted_auth"),
            "AgentConnection must not await observe_with_trusted_auth on the exclusive executor"
        );
        assert!(
            !body.contains("ProcessManager::new"),
            "AgentConnection must not construct a ProcessManager fallback"
        );
    }

    #[test]
    fn provider_start_rebind_requires_new_conversation_unstarted_and_no_live_launch() {
        let source = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/host/connection.rs"
        ));
        let start = source
            .find("\n    fn start_provider_session_intent(")
            .expect("start_provider_session_intent");
        let body = &source[start..];
        let end = body
            .find("\n    fn dispatch_task_close(")
            .or_else(|| body.find("/// Archive one task after releasing"))
            .expect("end of start_provider_session_intent");
        let body = &body[..end];
        assert!(
            body.contains("provider_restore_in_flight.contains"),
            "duplicate queued/inflight restore must guard before starts"
        );
        assert!(
            body.contains("original_claim"),
            "read-only original_claim must prepare before any rebind"
        );
        assert!(
            body.contains("prepare_provider_start(&original_claim)"),
            "original fences must validate before journaling provider change"
        );
        assert!(
            body.contains("ProviderStartMode::NewConversation"),
            "rebind refused unless NewConversation"
        );
        assert!(
            body.contains("is_unstarted_draft()"),
            "rebind refused unless projection is unstarted draft"
        );
        assert!(
            body.contains("try_has_live_provider_runtime("),
            "live provider book must block rebind even without SessionStart id"
        );
        assert!(
            body.contains("try_classify_persisted_provider_launch"),
            "persisted launch graph must use bounded classification"
        );
        assert!(
            body.contains("execute_host_authorized"),
            "rebind must use host-authorized execute, not public bus"
        );
        assert!(
            body.contains("Command::RebindUnstartedPrimaryProvider"),
            "rebind command must be the host-only rebound variant"
        );
    }

    fn shell_link_for_test() -> ShellSessionLink {
        ShellSessionLink {
            task_id: TaskId::new(),
            session_id: crate::terminal::protocol::TerminalSessionId::new(),
            last_cwd: None,
            last_cwd_change: None,
            last_activity_offer: None,
            last_activity_sequence: 0,
            exit_recorded: false,
        }
    }

    fn cwd_debounce() -> std::time::Duration {
        std::time::Duration::from_millis(TERMINAL_CWD_DEBOUNCE_MS as u64)
    }

    #[test]
    fn a_cwd_becomes_a_fact_only_after_the_shell_has_stayed_in_it() {
        let mut link = shell_link_for_test();
        let start = std::time::Instant::now();
        let first = std::path::PathBuf::from("/code/one");
        let second = std::path::PathBuf::from("/code/two");

        // A newly observed directory is not a fact yet.
        assert_eq!(
            link.note_cwd_sample(Some(first.clone()), start, cwd_debounce()),
            None
        );
        // Moving on before the window elapses restarts it: a directory the
        // shell only passed through never becomes a durable fact.
        let mid = start + cwd_debounce() / 2;
        assert_eq!(
            link.note_cwd_sample(Some(second.clone()), mid, cwd_debounce()),
            None
        );
        assert_eq!(
            link.note_cwd_sample(Some(second.clone()), start + cwd_debounce(), cwd_debounce()),
            None,
            "the window runs from the change, not from the first sample"
        );

        let settled = mid + cwd_debounce();
        assert_eq!(
            link.note_cwd_sample(Some(second.clone()), settled, cwd_debounce()),
            Some(second.clone())
        );
        // And exactly once: re-asking the store every tick to be told nothing
        // changed costs a transaction per shell per tick.
        assert_eq!(
            link.note_cwd_sample(Some(second), settled + cwd_debounce(), cwd_debounce()),
            None
        );
    }

    #[test]
    fn activity_reports_terminal_output_not_a_living_process() {
        let mut link = shell_link_for_test();
        let start = std::time::Instant::now();
        let window = std::time::Duration::from_millis(TERMINAL_ACTIVITY_COALESCE_MS as u64);

        // Two ticks over a silent shell. A heartbeat here would make every idle
        // terminal read as busy for as long as it stays open.
        assert!(!link.should_offer_activity(start, 0));
        assert!(!link.should_offer_activity(start + window * 2, 0));

        // Output moves the hosted delta sequence, and that is the activity.
        assert!(link.should_offer_activity(start + window * 2, 7));

        // Silent again afterwards: nothing more to report.
        assert!(!link.should_offer_activity(start + window * 4, 7));

        // Output arriving inside the coalescing window is held, not dropped:
        // the sequence is banked only when the offer is actually made.
        assert!(!link.should_offer_activity(start + window * 2 + window / 2, 9));
        assert!(link.should_offer_activity(start + window * 4, 9));
    }

    #[test]
    fn a_lost_cwd_sample_neither_retracts_nor_re_offers_the_settled_directory() {
        let mut link = shell_link_for_test();
        let start = std::time::Instant::now();
        let a = std::path::PathBuf::from("/code/one");

        // `a` settles and is offered exactly once.
        assert_eq!(
            link.note_cwd_sample(Some(a.clone()), start, cwd_debounce()),
            None
        );
        let settled = start + cwd_debounce();
        assert_eq!(
            link.note_cwd_sample(Some(a.clone()), settled, cwd_debounce()),
            Some(a.clone())
        );

        // The manager could not take this sample -- a denied OpenProcess, a
        // directory deleted under the shell. That is not a move.
        let lost = settled + cwd_debounce();
        assert_eq!(link.note_cwd_sample(None, lost, cwd_debounce()), None);
        assert_eq!(link.last_cwd.as_ref(), Some(&a), "the last cwd must stand");

        // `a` coming back is not a change either, so no second fact is offered
        // for a directory the shell never left.
        let recovered = lost + cwd_debounce() * 2;
        assert_eq!(
            link.note_cwd_sample(Some(a.clone()), recovered, cwd_debounce()),
            None
        );
        assert_eq!(link.last_cwd.as_ref(), Some(&a));
    }

    fn shell_request(command: Command) -> crate::protocol::ClientRequest {
        crate::protocol::ClientRequest::Command(CommandEnvelope {
            command_id: CommandId::new(),
            client_id: ClientId::new(),
            task_id: None,
            issued_at_ms: 1_725_000_000_000,
            expected_task_revision: None,
            command,
        })
    }

    #[test]
    fn only_shell_lifecycle_commands_owe_a_host_process_effect() {
        let task_id = TaskId::new();
        let resource_id = ResourceId::new();
        let mut resource = ResourceFacts {
            id: resource_id,
            task_id: Some(task_id),
            owner_kind: OwnerKind::Task,
            resource_kind: ResourceKind::Terminal,
            recipe: ResourceRecipe::Terminal {
                cols: 100,
                rows: 30,
                launch: Some(crate::domain::resource::TerminalLaunch {
                    cwd: std::path::PathBuf::from(if cfg!(windows) { "C:/code" } else { "/code" }),
                    program: std::path::PathBuf::from("cmd.exe"),
                    args: Vec::new(),
                }),
                title: None,
            },
            lifecycle: ResourceLifecycle::Active,
            runtime_generation: 1,
            updated_at_ms: 1_725_000_000_000,
        };

        let open = shell_request(Command::OpenShellTerminal(
            crate::domain::command::OpenShellTerminalIntent {
                resource: resource.clone(),
            },
        ));
        assert!(matches!(
            accepted_shell_terminal_effect(&open),
            Some(AcceptedShellEffect::Open { task_id: t, resource_id: r })
                if t == task_id && r == resource_id
        ));

        let close = shell_request(Command::CloseTerminal { resource_id });
        assert!(matches!(
            accepted_shell_terminal_effect(&close),
            Some(AcceptedShellEffect::Close { resource_id: r }) if r == resource_id
        ));

        // A rename changes only durable text; it owes no process effect.
        let rename = shell_request(Command::RenameTerminal {
            resource_id,
            title: "build".to_string(),
        });
        assert!(accepted_shell_terminal_effect(&rename).is_none());

        // A host-owned terminal has no task to spawn under, so there is no
        // effect to attribute rather than one attributed to the wrong task.
        resource.task_id = None;
        let unowned = shell_request(Command::OpenShellTerminal(
            crate::domain::command::OpenShellTerminalIntent { resource },
        ));
        assert!(accepted_shell_terminal_effect(&unowned).is_none());
    }

    #[test]
    fn both_executor_loops_start_and_sample_host_shells() {
        let source = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/host/connection.rs"
        ));
        // Each needle is assembled so this assertion's own source does not
        // count as one of the call sites it is measuring.
        let capture = format!("accepted_shell_terminal_effect{}", "(&job.request)");
        assert_eq!(
            source.matches(capture.as_str()).count(),
            2,
            "both executor loops must capture the shell effect before the request is moved"
        );
        let apply = format!("self.apply_shell_terminal_effect{}", "(effect);");
        assert_eq!(
            source.matches(apply.as_str()).count(),
            2,
            "both executor loops must run the accepted shell effect"
        );
        let pump = format!("self.pump_shell_terminal_facts{}", "();");
        assert_eq!(
            source.matches(pump.as_str()).count(),
            2,
            "both reaper ticks must sample live shells; an unsampled shell reports \
             no cwd, no activity and never observes its own exit"
        );
    }

    #[test]
    fn provider_restore_pending_keeps_readiness_unknown_until_enumeration() {
        let source = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/host/connection.rs"
        ));
        let queue = source
            .find("fn queue_one_provider_restore(")
            .expect("queue_one_provider_restore");
        let body = &source[queue..];
        let end = body
            .find("\n    fn handle_provider_restore_outcome(")
            .or_else(|| body.find("\n    async fn "))
            .unwrap_or(body.len().min(2500));
        let body = &body[..end];
        assert!(
            body.contains("self.provider_restore_pending = false"),
            "pending must clear only after successful enumeration"
        );
        let clear_at = body
            .find("self.provider_restore_pending = false")
            .expect("clear");
        let enum_at = body
            .find("restorable_provider_starts")
            .expect("enumeration");
        assert!(
            enum_at < clear_at,
            "must not publish NotPending before enumeration succeeds"
        );
        assert!(
            source.contains("Some(_) if self.provider_restore_pending =>"),
            "readiness hint must stay Unknown while restore enumeration is pending/failed"
        );
    }

    #[test]
    fn accepted_provider_input_drives_delivery_and_exact_restore_immediately() {
        let source = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/host/connection.rs"
        ));
        assert_eq!(
            source
                .matches("accepted_provider_input_task_id(&job.request)")
                .count(),
            3,
            "both executor loops must capture provider-input ownership before moving the request"
        );
        assert_eq!(
            source
                .matches("drive_provider_input_after_acceptance(task_id)")
                .count(),
            4,
            "both executor loops and successful exact restore must drive the provider outbox"
        );

        let start = source
            .find("fn drive_provider_input_after_acceptance(")
            .expect("immediate provider-input driver");
        let body = &source[start..];
        let end = body
            .find("\n    fn reconcile_configured_services(")
            .expect("maintenance reconciler follows immediate driver");
        let body = &body[..end];
        assert!(
            body.contains("provider_restore_failed_action_epochs.remove(&task_id)"),
            "a fresh accepted input must explicitly rearm exact restore after a prior transient failure"
        );
        assert!(
            body.contains("run_provider_dispatch"),
            "accepted input must attempt one bounded delivery pass immediately"
        );
        assert!(
            body.contains("queue_held_provider_restart"),
            "a held delivery must schedule the exact persisted provider restart"
        );
        assert!(
            body.contains("queue_one_provider_restore"),
            "the exact restore must start without waiting for periodic maintenance"
        );
    }

    #[test]
    fn provider_restore_lane_prevents_startup_prompt_head_of_line_blocking() {
        let source = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/host/connection.rs"
        ));
        let start = source
            .find("fn queue_one_provider_restore(")
            .expect("provider restore scheduler");
        let body = &source[start..];
        let end = body
            .find("\n    fn handle_provider_restore_outcome(")
            .expect("provider restore outcome follows scheduler");
        let body = &body[..end];
        assert!(
            body.contains("self.provider_restore_jobs.len() >= MAX_CONCURRENT_PROVIDER_RESTORES"),
            "one provider-owned startup prompt must not serialize every provider restore"
        );
        assert!(MAX_CONCURRENT_PROVIDER_RESTORES >= 2);
    }

    #[test]
    fn provider_launch_binding_policy_distinguishes_new_conversation_from_resume() {
        use crate::domain::command::ProviderStartMode;

        assert!(!provider_start_requires_existing_binding(
            ProviderStartMode::NewConversation
        ));
        assert!(provider_start_requires_existing_binding(
            ProviderStartMode::ResumeExact
        ));
        assert!(provider_start_requires_existing_binding(
            ProviderStartMode::Open
        ));
    }

    #[test]
    fn same_kind_new_conversation_refuses_live_or_persisted_provider() {
        let source = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/host/connection.rs"
        ));
        let start = source
            .find("\n    fn start_provider_session_intent(")
            .expect("start_provider_session_intent");
        let body = &source[start..];
        let end = body
            .find("\n    fn dispatch_task_close(")
            .or_else(|| body.find("/// Archive one task after releasing"))
            .expect("end of start_provider_session_intent");
        let body = &body[..end];
        let same_kind = body
            .find("Same-kind NewConversation must not double-launch")
            .expect("same-kind NewConversation guard");
        let kind_mismatch = body
            .find("if agent.provider_kind != intent.provider_kind")
            .expect("kind-mismatch rebind branch");
        assert!(
            same_kind < kind_mismatch,
            "same-kind live/persisted refusal must run before kind-mismatch rebind"
        );
        let guard = &body[same_kind..kind_mismatch];
        assert!(
            guard.contains("try_has_live_provider_runtime"),
            "same-kind guard must refuse live provider runtime"
        );
        assert!(
            guard.contains("try_classify_persisted_provider_launch"),
            "same-kind guard must use bounded persisted classification"
        );
        assert!(
            !guard.contains("peek_persisted_provider_launch_spec"),
            "same-kind guard must not call blocking persisted peek"
        );
        assert!(
            guard.contains("NewConversation refused; live provider generation present")
                || guard.contains("live provider generation present"),
            "refusal must stay Unavailable — never fake success"
        );
    }

    #[test]
    fn public_command_bus_denies_rebind_unstarted_primary_provider() {
        let bus = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/kernel/command_bus.rs"
        ));
        assert!(
            bus.contains("Command::RebindUnstartedPrimaryProvider")
                && bus.contains("HostAuthorityRequired"),
            "public CommandBus.execute must deny RebindUnstartedPrimaryProvider"
        );
        let permissions = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/connect/permissions.rs"
        ));
        assert!(
            permissions.contains("Command::RebindUnstartedPrimaryProvider"),
            "connect permissions must enumerate RebindUnstartedPrimaryProvider"
        );
    }

    #[test]
    fn projector_rebound_validates_busy_projection_not_only_null_session_id() {
        let source = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/kernel/projector.rs"
        ));
        let start = source
            .find("Event::UnstartedPrimaryProviderRebound")
            .expect("rebound event arm");
        let body = &source[start..];
        let end = body
            .find("Event::SpecialistRequested")
            .expect("next event arm");
        let body = &body[..end];
        assert!(
            body.contains("current_turn.is_some()")
                && body.contains("open_question.is_some()")
                && body.contains("last_settlement.is_some()"),
            "projector must refuse rebound when provider projection is busy"
        );
        assert!(
            body.contains("provider_session_id IS NULL"),
            "SQL still requires null SessionStart id"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{
        apply_admitted_notification_sound, next_attention_after_provider_restore_failure,
        orphaned_shell_reason, provider_cleanup_requires_resource_release_first,
        provider_restore_failure_attention, provider_restore_success_attention,
        provider_terminal_query_may_attach, push_unique_provider_restore_intent,
        restore_admitted_provider_launch_options, task_resource_blocks_archive_after_cleanup_error,
    };
    use crate::domain::agent::{
        AgentRole, AgentSessionFacts, AgentSessionLifecycle, ProviderSessionId,
    };
    use crate::domain::command::{
        Command, CommandEnvelope, CreateTaskIntent, ProviderStartMode, SetTaskAttentionIntent,
    };
    use crate::domain::id::{
        AgentSessionId, ClientId, CommandId, EnvironmentId, ProjectId, RequestId, ResourceId,
        TaskId,
    };
    use crate::domain::resource::{
        OwnerKind, ResourceFacts, ResourceKind, ResourceLifecycle, ResourceRecipe,
    };
    use crate::domain::task::{
        ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity,
        WorkspaceRef,
    };
    use crate::kernel::CommandBus;
    use crate::providers::ProviderKind;
    use crate::remote::presentation::{
        SemanticEventDraft, SemanticEventKind, SemanticJournalStore, SemanticRetention,
        SemanticSource, StableSessionKey,
    };
    use std::collections::{HashMap, HashSet, VecDeque};
    use uuid::Uuid;

    /// Fixtures for the two F1 rules: one task, one provider terminal, one
    /// plain shell, and every field the rules actually read.
    fn resource_fixture(
        task_id: TaskId,
        launch: Option<crate::domain::resource::TerminalLaunch>,
        lifecycle: ResourceLifecycle,
    ) -> ResourceFacts {
        ResourceFacts {
            id: ResourceId::new(),
            task_id: Some(task_id),
            owner_kind: OwnerKind::Task,
            resource_kind: ResourceKind::Terminal,
            recipe: ResourceRecipe::Terminal {
                cols: 100,
                rows: 30,
                launch,
                title: None,
            },
            lifecycle,
            runtime_generation: 1,
            updated_at_ms: 1,
        }
    }

    fn shell_launch() -> crate::domain::resource::TerminalLaunch {
        crate::domain::resource::TerminalLaunch {
            cwd: std::path::PathBuf::from("C:/workspace"),
            program: std::path::PathBuf::from("C:/Windows/System32/cmd.exe"),
            args: Vec::new(),
        }
    }

    /// Opening a shell must not change when provider cleanup runs.
    ///
    /// `provider_cleanup_requires_resource_release_first` decides whether the
    /// provider is closed before or after the resource release loop. Plain
    /// shells are released by that loop directly rather than through a
    /// process-empty durable release, so counting one would move an ordinary
    /// provider task onto the deferred branch the moment its owner opened a
    /// shell -- a timing change to the provider lane caused by something with
    /// no provider in it.
    #[test]
    fn a_plain_shell_does_not_change_when_provider_cleanup_runs() {
        let task_id = TaskId::new();
        let provider = resource_fixture(task_id, None, ResourceLifecycle::Active);
        let shell = resource_fixture(task_id, Some(shell_launch()), ResourceLifecycle::Active);

        // The branch a provider alone takes is the branch it must still take
        // with a shell beside it.
        let with_provider_alone =
            provider_cleanup_requires_resource_release_first([&provider], task_id);
        assert!(
            with_provider_alone,
            "a live provider resource defers cleanup; this fixture is the control"
        );
        assert_eq!(
            provider_cleanup_requires_resource_release_first([&provider, &shell], task_id),
            with_provider_alone,
            "a shell beside the provider must not move the cleanup branch"
        );

        // And a task whose only live resources are shells takes the
        // undeferred branch, exactly as a task with nothing open does.
        assert!(
            !provider_cleanup_requires_resource_release_first([&shell], task_id),
            "shells alone must not defer provider cleanup"
        );
        let releasing_shell =
            resource_fixture(task_id, Some(shell_launch()), ResourceLifecycle::Releasing);
        assert!(
            !provider_cleanup_requires_resource_release_first([&releasing_shell], task_id),
            "a releasing shell must not defer provider cleanup either"
        );
        assert!(
            !provider_cleanup_requires_resource_release_first([], task_id),
            "the empty case is the branch every shell-only task must share"
        );
    }

    /// The reconciliation sweep's rule, in both directions.
    ///
    /// This is the framework-level guard behind the release path: a future
    /// release path that forgets to close a shell leaks a live PTY, and this
    /// is what catches it one tick later. A rule that answered "orphaned" for
    /// a healthy shell would be far worse than the leak, so the negative cases
    /// carry as much weight as the positive ones -- particularly `Releasing`,
    /// which is the ordinary transient a close passes through and which the
    /// client renders as a muted "?" chip.
    #[test]
    fn only_a_shell_with_no_durable_terminal_left_is_swept() {
        use crate::domain::terminal_facts::TerminalFacts;
        use std::collections::BTreeMap;

        let task_id = TaskId::new();
        let snapshot_with = |lifecycle: Option<ResourceLifecycle>, facts: bool| {
            let shell = resource_fixture(
                task_id,
                Some(shell_launch()),
                lifecycle.unwrap_or(ResourceLifecycle::Active),
            );
            let shell_id = shell.id;
            let mut snapshot = crate::domain::snapshot::TaskSnapshot {
                connectivity: TaskConnectivity::Connected,
                attention: TaskAttention::None,
                activity: TaskActivity::Idle,
                review_readiness: ReviewReadiness::NotReady,
                task: crate::domain::task::TaskFacts::new(
                    EnvironmentId::new(),
                    "sweep",
                    None,
                    ProjectId::new(),
                    WorkspaceRef::Main,
                    TaskAssignment::LocalOwner,
                    1,
                )
                .expect("task facts"),
                agents: BTreeMap::new(),
                primary_agent_id: None,
                artifacts: BTreeMap::new(),
                resources: BTreeMap::new(),
                provider_sessions: BTreeMap::new(),
                browser: crate::domain::browser::BrowserBook::new(),
                terminal_facts: BTreeMap::new(),
                terminal_strip: crate::domain::terminal_facts::TaskTerminalStrip::default(),
            };
            if lifecycle.is_some() {
                snapshot.resources.insert(shell_id, shell);
            }
            if facts {
                snapshot
                    .terminal_facts
                    .insert(shell_id, TerminalFacts::registered(shell_id, None, 1));
            }
            (snapshot, shell_id)
        };

        // Still live: the sweep must leave every one of these alone.
        for lifecycle in [ResourceLifecycle::Active, ResourceLifecycle::Releasing] {
            let (snapshot, shell_id) = snapshot_with(Some(lifecycle), true);
            assert_eq!(
                orphaned_shell_reason(shell_id, Some(&snapshot)),
                None,
                "a {lifecycle:?} shell with durable facts is still reachable and must not be closed"
            );
        }

        // Gone, in each of the three ways it can go.
        let (snapshot, shell_id) = snapshot_with(Some(ResourceLifecycle::Released), false);
        assert_eq!(
            orphaned_shell_reason(shell_id, Some(&snapshot)),
            Some("its resource was released")
        );
        let (snapshot, shell_id) = snapshot_with(None, false);
        assert_eq!(
            orphaned_shell_reason(shell_id, Some(&snapshot)),
            Some("its resource is gone")
        );
        // The projection can also lose the terminal without the resource row:
        // that is what a failed boot rebuild leaves behind.
        let (snapshot, shell_id) = snapshot_with(Some(ResourceLifecycle::Active), false);
        assert_eq!(
            orphaned_shell_reason(shell_id, Some(&snapshot)),
            Some("its durable terminal is gone")
        );
        assert_eq!(
            orphaned_shell_reason(ResourceId::new(), None),
            Some("its task is gone")
        );
    }

    #[test]
    fn stale_provider_cleanup_only_blocks_archive_for_live_task_owned_resources() {
        let task_id = TaskId::new();
        let other_task_id = TaskId::new();
        let mut owned = ResourceFacts::new(
            Some(task_id),
            OwnerKind::Task,
            ResourceKind::Terminal,
            ResourceRecipe::terminal(80, 24),
            1,
        )
        .expect("task resource");
        assert!(task_resource_blocks_archive_after_cleanup_error(
            &owned, task_id
        ));

        owned.lifecycle = ResourceLifecycle::Releasing;
        assert!(task_resource_blocks_archive_after_cleanup_error(
            &owned, task_id
        ));

        owned.lifecycle = ResourceLifecycle::Released;
        assert!(!task_resource_blocks_archive_after_cleanup_error(
            &owned, task_id
        ));

        owned.lifecycle = ResourceLifecycle::Active;
        assert!(!task_resource_blocks_archive_after_cleanup_error(
            &owned,
            other_task_id
        ));

        let host = ResourceFacts::new(
            None,
            OwnerKind::Host,
            ResourceKind::Service,
            ResourceRecipe::service("host service").expect("service recipe"),
            1,
        )
        .expect("host resource");
        assert!(!task_resource_blocks_archive_after_cleanup_error(
            &host, task_id
        ));
    }

    #[test]
    fn active_task_resource_defers_provider_cleanup_until_release_settles() {
        let task_id = TaskId::new();
        let mut owned = ResourceFacts::new(
            Some(task_id),
            OwnerKind::Task,
            ResourceKind::Terminal,
            ResourceRecipe::terminal(80, 24),
            1,
        )
        .expect("task resource");
        let resources = HashMap::from([(owned.id, owned.clone())]);
        assert!(provider_cleanup_requires_resource_release_first(
            resources.values(),
            task_id
        ));

        owned.lifecycle = ResourceLifecycle::Released;
        let resources = HashMap::from([(owned.id, owned)]);
        assert!(!provider_cleanup_requires_resource_release_first(
            resources.values(),
            task_id
        ));
    }

    #[test]
    fn terminal_readiness_is_observational_and_never_starts_a_provider() {
        assert!(provider_terminal_query_may_attach(
            &crate::domain::cockpit::TaskCockpitQuery::Terminal
        ));
        assert!(!provider_terminal_query_may_attach(
            &crate::domain::cockpit::TaskCockpitQuery::TerminalReadiness
        ));
    }

    #[test]
    fn provider_dispatch_pass_does_not_stop_at_the_first_held_conversation() {
        let source = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/host/connection.rs"
        ));
        let start = source
            .find("fn run_provider_dispatch_pass(")
            .expect("bounded provider dispatch pass");
        let body = &source[start..];
        let end = body
            .find("\n}\n\nfn provider_start_requires_existing_binding")
            .expect("dispatch pass boundary");
        let body = &body[..end];
        assert!(body.contains("0..MAX_PROVIDER_DISPATCH_UNITS_PER_PASS"));
        assert!(body.contains("ProviderDispatchOutcome::Idle => break"));
        assert!(body.contains("held.push((task_id, reason))"));
        assert!(!body.contains("ProviderDispatchOutcome::Held { task_id, reason } => break"));
    }

    struct ProviderRestoreHarness {
        bus: CommandBus,
        task_id: TaskId,
        client_id: ClientId,
        agent_session_id: AgentSessionId,
        resource_id: ResourceId,
        provider_session_id: ProviderSessionId,
        journal: SemanticJournalStore,
        _directory: tempfile::TempDir,
    }

    struct RestartRestoreIntentView {
        provider_session_id: Option<ProviderSessionId>,
        mode: ProviderStartMode,
    }

    impl ProviderRestoreHarness {
        fn with_bound_session(expected: ProviderSessionId) -> Self {
            Self::seed(expected, true)
        }

        fn with_history(session: &str) -> Self {
            let expected = ProviderSessionId::new(session).expect("provider session");
            let mut harness = Self::seed(expected, true);
            harness.seed_history_facts(2);
            harness
        }

        fn seed(provider_session_id: ProviderSessionId, bind: bool) -> Self {
            let directory = tempfile::tempdir().expect("provider restore harness directory");
            let mut bus =
                CommandBus::open(&directory.path().join("tasks.sqlite")).expect("command bus");
            let task_id = TaskId::new();
            let client_id = ClientId::new();
            let project_id = ProjectId::new();
            let created = bus
                .execute_for_test(CommandEnvelope {
                    command_id: CommandId::new(),
                    client_id,
                    task_id: None,
                    issued_at_ms: 1_725_000_000_000,
                    expected_task_revision: None,
                    command: Command::CreateTask(CreateTaskIntent {
                        id: task_id,
                        environment_id: EnvironmentId::new(),
                        title: "Restore harness".into(),
                        description: None,
                        project_id,
                        workspace: WorkspaceRef::Main,
                        assignment: TaskAssignment::LocalOwner,
                        created_at_ms: 1_725_000_000_000,
                        connectivity: TaskConnectivity::Connected,
                        attention: TaskAttention::None,
                        // Startup only restores conversations that were
                        // durably working (or still own undelivered input).
                        // Idle history is intentionally lazy.
                        activity: TaskActivity::Working,
                        review_readiness: ReviewReadiness::NotReady,
                    }),
                })
                .expect("create task");
            let mut revision = match created {
                crate::domain::command::CommandReceipt::Accepted {
                    task_revision: Some(revision),
                    ..
                } => revision,
                other => panic!("expected accepted create, got {other:?}"),
            };
            let agent_session_id = AgentSessionId::new();
            revision = accepted_revision(
                bus.execute(task_envelope(
                    client_id,
                    task_id,
                    revision,
                    Command::RegisterAgentSession {
                        agent: AgentSessionFacts {
                            id: agent_session_id,
                            task_id,
                            role: AgentRole::Primary,
                            provider_kind: ProviderKind::Codex,
                            provider_session_id: None,
                            lifecycle: AgentSessionLifecycle::Open,
                            runtime_generation: 3,
                            revision: 0,
                        },
                    },
                ))
                .expect("register agent"),
            );
            revision = accepted_revision(
                bus.execute(task_envelope(
                    client_id,
                    task_id,
                    revision,
                    Command::SetPrimaryAgent { agent_session_id },
                ))
                .expect("set primary"),
            );
            let resource_id = ResourceId::new();
            revision = accepted_revision(
                bus.execute(task_envelope(
                    client_id,
                    task_id,
                    revision,
                    Command::RegisterResource {
                        resource: ResourceFacts {
                            id: resource_id,
                            task_id: Some(task_id),
                            owner_kind: OwnerKind::Task,
                            resource_kind: ResourceKind::Terminal,
                            recipe: ResourceRecipe::terminal(120, 40),
                            lifecycle: ResourceLifecycle::Active,
                            runtime_generation: 3,
                            updated_at_ms: 1_725_000_000_100,
                        },
                    },
                ))
                .expect("register resource"),
            );
            if bind {
                let _ = accepted_revision(host_execute(
                    &mut bus,
                    task_envelope(
                        client_id,
                        task_id,
                        revision,
                        Command::BindProviderSession {
                            agent_session_id,
                            resource_id,
                            provider_session_id: provider_session_id.clone(),
                            expected_runtime_generation: 3,
                        },
                    ),
                ));
            }
            Self {
                bus,
                task_id,
                client_id,
                agent_session_id,
                resource_id,
                provider_session_id,
                journal: SemanticJournalStore::default(),
                _directory: directory,
            }
        }

        fn seed_history_facts(&mut self, count: usize) {
            for index in 0..count {
                let kind = if index % 2 == 0 {
                    SemanticEventKind::UserMessage {
                        text: format!("user-{index}"),
                    }
                } else {
                    SemanticEventKind::AssistantMessage {
                        message_id: format!("msg-{index}"),
                        text: format!("assistant-{index}"),
                        streaming: false,
                    }
                };
                self.journal.record(SemanticEventDraft {
                    stable_session_key: StableSessionKey::from_tab(self.task_id.to_string()),
                    occurred_at_epoch_ms: 1_725_000_000_000 + index as u64,
                    source: SemanticSource::Codex,
                    kind,
                    retention: SemanticRetention::Canonical,
                    deduplication_key: Some(format!("history-{index}")),
                });
            }
        }

        fn restart_restore_intent(&mut self) -> RestartRestoreIntentView {
            let starts = self
                .bus
                .restorable_provider_starts(64)
                .expect("restorable starts");
            assert_eq!(starts.len(), 1, "expected one restorable conversation");
            let intent = &starts[0];
            assert_eq!(intent.task_id, self.task_id);
            assert_eq!(intent.agent_session_id, self.agent_session_id);
            assert_eq!(intent.resource_id, self.resource_id);
            let (_binding, agent, _) = self
                .bus
                .prepare_provider_start(intent)
                .expect("prepare exact restore");
            let durable = self
                .bus
                .durable_provider_binding(self.agent_session_id)
                .expect("durable binding")
                .map(|(provider_session_id, _)| provider_session_id);
            assert_eq!(
                durable.as_ref(),
                Some(&self.provider_session_id),
                "hand-derived durable value must match the stored provider session"
            );
            assert_eq!(
                agent.provider_session_id.as_ref(),
                Some(&self.provider_session_id),
                "prepared agent must carry the exact stored provider session"
            );
            RestartRestoreIntentView {
                provider_session_id: agent.provider_session_id,
                mode: intent.mode,
            }
        }

        fn complete_restore_with_error(&mut self, _reason: &str) {
            let snapshot = self
                .bus
                .task_snapshot(self.task_id)
                .expect("snapshot")
                .expect("task present");
            if let Some(attention) = next_attention_after_provider_restore_failure(&snapshot) {
                let _ = host_execute(
                    &mut self.bus,
                    task_envelope(
                        self.client_id,
                        self.task_id,
                        snapshot.task.revision,
                        Command::SetTaskAttention(SetTaskAttentionIntent { attention }),
                    ),
                );
            }
        }

        fn task_attention(&self) -> Option<TaskAttention> {
            self.bus
                .task_snapshot(self.task_id)
                .expect("snapshot")
                .map(|snapshot| snapshot.attention)
        }

        fn persisted_history_len(&self) -> usize {
            let key = StableSessionKey::from_tab(self.task_id.to_string());
            self.journal
                .replay_after(&key, 0)
                .map(|replay| replay.events.len())
                .unwrap_or(0)
        }
    }

    fn accepted_revision(receipt: crate::domain::command::CommandReceipt) -> u64 {
        match receipt {
            crate::domain::command::CommandReceipt::Accepted {
                task_revision: Some(revision),
                ..
            } => revision,
            other => panic!("expected accepted task receipt, got {other:?}"),
        }
    }

    fn task_envelope(
        client_id: ClientId,
        task_id: TaskId,
        revision: u64,
        command: Command,
    ) -> CommandEnvelope {
        CommandEnvelope {
            command_id: CommandId::new(),
            client_id,
            task_id: Some(task_id),
            issued_at_ms: 1_725_000_100_000,
            expected_task_revision: Some(revision),
            command,
        }
    }

    fn host_execute(
        bus: &mut CommandBus,
        envelope: CommandEnvelope,
    ) -> crate::domain::command::CommandReceipt {
        bus.execute_host_authorized(envelope, None, RequestId::new(), Uuid::now_v7())
            .expect("host-authorized command")
    }

    #[test]
    fn restart_restore_uses_the_durable_provider_session_id_without_new_conversation_fallback() {
        let expected = ProviderSessionId::new("conversation-42").unwrap();
        let mut harness = ProviderRestoreHarness::with_bound_session(expected.clone());

        let intent = harness.restart_restore_intent();

        assert_eq!(intent.provider_session_id.as_ref(), Some(&expected));
        assert_eq!(intent.mode, ProviderStartMode::ResumeExact);
    }

    #[test]
    fn failed_exact_restore_does_not_mark_an_existing_task_failed() {
        let mut harness = ProviderRestoreHarness::with_history("conversation-42");

        harness.complete_restore_with_error("ExactResumeUnavailable");

        assert_ne!(harness.task_attention(), Some(TaskAttention::Failed));
        assert_eq!(harness.persisted_history_len(), 2);
    }

    #[test]
    fn provider_restore_failure_keeps_existing_attention() {
        assert_eq!(
            provider_restore_failure_attention(TaskAttention::None),
            None
        );
        assert_eq!(
            provider_restore_failure_attention(TaskAttention::NeedsAnswer),
            None
        );
        let _ = provider_restore_success_attention(TaskAttention::Failed);
    }

    #[test]
    fn startup_restore_does_not_duplicate_a_held_restart_for_the_same_task() {
        let task_id = TaskId::new();
        let intent = crate::domain::command::StartProviderSessionIntent {
            task_id,
            agent_session_id: crate::domain::AgentSessionId::new(),
            resource_id: crate::domain::ResourceId::new(),
            provider_kind: crate::domain::ProviderKind::Codex,
            mode: crate::domain::command::ProviderStartMode::ResumeExact,
            launch_options: crate::providers::adapter::ProviderLaunchOptions::default(),
            expected_task_revision: 7,
            expected_action_epoch: 0,
        };
        let mut queue = VecDeque::new();
        let in_flight = HashSet::new();

        assert!(push_unique_provider_restore_intent(
            &mut queue,
            &in_flight,
            intent.clone(),
        ));
        assert!(!push_unique_provider_restore_intent(
            &mut queue, &in_flight, intent,
        ));
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn held_first_input_preserves_admitted_provider_launch_options() {
        let task_id = TaskId::new();
        let mut retry = crate::domain::command::StartProviderSessionIntent {
            task_id,
            agent_session_id: crate::domain::AgentSessionId::new(),
            resource_id: crate::domain::ResourceId::new(),
            provider_kind: crate::domain::ProviderKind::Codex,
            mode: crate::domain::command::ProviderStartMode::NewConversation,
            launch_options: crate::providers::ProviderLaunchOptions::default(),
            expected_task_revision: 7,
            expected_action_epoch: 1,
        };
        let selected = crate::providers::ProviderLaunchOptions {
            model: crate::providers::ProviderModel::CodexTerra,
            reasoning_effort: crate::providers::ProviderReasoningEffort::High,
            access: crate::providers::ProviderAccessMode::ReadOnly,
            ..crate::providers::ProviderLaunchOptions::default()
        };
        let admitted = HashMap::from([(task_id, selected.clone())]);

        restore_admitted_provider_launch_options(&mut retry, &admitted);

        assert_eq!(retry.launch_options, selected);
    }

    #[test]
    fn configured_runtime_applies_admitted_notification_sound() {
        use crate::config::model::{Nullable, Settings};

        // Same boundary as ConfiguredServiceRuntime::initialized_from_admission:
        // admitted settings.notification_sound → ProcessManager.
        let mut settings = Settings::default();
        settings.notification_sound = Nullable::Value("chime".into());
        let manager = crate::services::ProcessManager::new();
        apply_admitted_notification_sound(&manager, &settings);
        assert_eq!(manager.notification_sound().as_deref(), Some("chime"));

        // Replay/stale generations must not invent a second sound source; the
        // ProcessManager retains exactly one configured id until replaced.
        manager.set_notification_sound(None);
        assert_eq!(manager.notification_sound(), None);
        manager.set_notification_sound(Some("glass".into()));
        assert_eq!(manager.notification_sound().as_deref(), Some("glass"));
    }

    #[test]
    fn legacy_failed_attention_clears_when_exact_session_restore_fails() {
        let mut harness = ProviderRestoreHarness::with_history("conversation-42");
        let snapshot = harness
            .bus
            .task_snapshot(harness.task_id)
            .expect("snapshot")
            .expect("task");
        let _ = host_execute(
            &mut harness.bus,
            task_envelope(
                harness.client_id,
                harness.task_id,
                snapshot.task.revision,
                Command::SetTaskAttention(SetTaskAttentionIntent {
                    attention: TaskAttention::Failed,
                }),
            ),
        );
        assert_eq!(harness.task_attention(), Some(TaskAttention::Failed));

        harness.complete_restore_with_error("ExactResumeUnavailable");

        assert_ne!(harness.task_attention(), Some(TaskAttention::Failed));
        assert_eq!(harness.persisted_history_len(), 2);
    }
}
