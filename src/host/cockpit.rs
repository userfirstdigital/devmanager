//! Host dispatch for typed Task Cockpit queries.
//!
//! Workspace identity is resolved from the selected Task admission. Client
//! paths are never authoritative. Live Logs/Health use the ProcessManager
//! supervisor singleton. Git and files require a revalidated workspace
//! authorization plus an active resource lease.

use uuid::Uuid;

use crate::config::AppConfig;
use crate::domain::agent_resource::{AgentResourceBinding, AgentResourceBindingError};
use crate::domain::cockpit::{
    cockpit_surface, relative_path_is_safe, workspace_projection, ConfigSidebarFolder,
    ConfigSidebarProject, ConfigSidebarProvider, ConfigSidebarProviderKind, ConfigSidebarServer,
    ConfigSidebarSnapshot, ConfigSidebarSsh, TaskCockpitDeniedReason, TaskCockpitQuery,
    TaskCockpitResult, TaskCockpitSurface, TaskCockpitUnavailableReason, TaskFileEntry,
    TaskFilesListProjection, TaskFilesReadProjection, TaskGitMutateIntent, TaskGitProjection,
    TaskRepositorySelector, TaskSshEndpoint, TaskSshLifecycle, TaskSshProjection,
    TaskSshRuntimeError, TaskSshRuntimeProjection, TaskTerminalProjection, MAX_COCKPIT_FILE_LIST,
    MAX_COCKPIT_READ_BYTES,
};
use crate::domain::id::{ClientId, CommandId, RequestId, TaskId};
use crate::domain::query::{QueryError, QueryOutcome, QueryResult};
use crate::domain::{AgentSessionFacts, ResourceFacts};
use crate::git::command::{
    issue_configured_repository_git_host_binding, issue_git_host_binding, GitCancellation,
    GitConfirmation, GitError, GitRepository,
};
use crate::git::model::{MutationPlan, RepoPath, StatusKind};
use crate::kernel::CommandBus;
use crate::protocol::Capability;
use crate::protocol::CapabilitySet;
use crate::services::model::AdmissionFence;
use crate::services::supervisor::{
    SupervisorAction, SupervisorError, SupervisorOutcome, SupervisorRefusal,
};
use crate::services::ProcessManager;
use crate::ssh::{
    accept_exact_endpoint, SshEndpointDenial, SshLifecycle, SshRuntimeAdapter, SshRuntimeError,
    SshRuntimeSnapshot, SshTaskIdentity,
};
use crate::terminal::service::TerminalService;
use crate::workspace::files::{
    ContentKind, EntryKind, ExpectedRevision, FilePageRequest, FileServiceError, ReadOptions,
    SecretClassification, MAX_CHUNK_BYTES,
};
use crate::workspace::worktree::{revalidate_cockpit_workspace_action, WorkspaceActionContext};
use crate::workspace::{
    issue_file_service, issue_read_file_service, WorkspaceAuthorization, WorkspaceProjectRoots,
    WorkspaceResource, WorkspaceResourceCoordinator, WorkspaceResourceLease, WorkspaceService,
};

pub(crate) struct TaskCockpitDispatch<'a> {
    pub capabilities: CapabilitySet,
    pub envelope_task_id: Option<TaskId>,
    pub client_id: ClientId,
    pub connection_id: Uuid,
    pub request_id: RequestId,
    pub query: &'a TaskCockpitQuery,
    pub bus: &'a CommandBus,
    pub service_runtime: Option<&'a ProcessManager>,
    pub semantic_journal:
        Option<&'a std::sync::Mutex<crate::remote::presentation::SemanticJournalStore>>,
    pub terminal_service: Option<&'a TerminalService>,
    pub ssh_endpoints: Option<&'a [TaskSshEndpoint]>,
    pub ssh_runtime: Option<&'a dyn SshRuntimeAdapter>,
    pub workspace_projects: Option<&'a WorkspaceProjectRoots>,
    pub coordinator: Option<&'a WorkspaceResourceCoordinator>,
    pub action_epoch: Option<u64>,
    pub runtime_generation: Option<u64>,
    /// Host-only canonical config snapshot. It is used only for the redacted
    /// ConfigSnapshot query and is never exposed as a path-bearing AppConfig.
    pub config: Option<&'a AppConfig>,
}

/// Host-side adapter for the exact kernel provider-resource claim.  The
/// Task Cockpit must project the already-validated tuple; it must not derive a
/// provider identity from terminal/process observations.
pub(crate) fn project_agent_resource(
    agent: &AgentSessionFacts,
    resource: &ResourceFacts,
) -> Result<crate::domain::TaskAgentResourceProjection, AgentResourceBindingError> {
    AgentResourceBinding::from_facts(agent, resource)
        .map(crate::domain::task_agent_resource_projection)
}

pub(crate) fn serve_task_cockpit(dispatch: TaskCockpitDispatch<'_>) -> QueryOutcome {
    if !dispatch.capabilities.grants_task_cockpit() {
        return QueryOutcome::Err(QueryError::UnsupportedCapability);
    }
    let surface = cockpit_surface(dispatch.query);
    if matches!(
        dispatch.query,
        TaskCockpitQuery::ConfigCreateProject { .. }
            | TaskCockpitQuery::ConfigUpsertCommand { .. }
            | TaskCockpitQuery::ConfigArchiveCommand { .. }
            | TaskCockpitQuery::ConfigRunCommand { .. }
            | TaskCockpitQuery::ProviderSettings(_)
    ) {
        // Mutation is owned by the exclusive host executor, which re-issues
        // workspace authority before returning a snapshot.
        return QueryOutcome::Err(QueryError::Unavailable {
            reason: "config_mutate",
        });
    }
    if matches!(dispatch.query, TaskCockpitQuery::AgentConnection) {
        return QueryOutcome::Err(QueryError::Unavailable {
            reason: "agent_connection",
        });
    }
    if matches!(dispatch.query, TaskCockpitQuery::ConfigSnapshot) {
        let Some(config) = dispatch.config else {
            return QueryOutcome::Err(QueryError::Unavailable {
                reason: "config_snapshot",
            });
        };
        return QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Config(
            config_sidebar_snapshot(config),
        )));
    }
    if let TaskCockpitQuery::ConfigCommandDetail {
        project_id,
        folder_id,
        command_id,
    } = dispatch.query
    {
        let Some(config) = dispatch.config else {
            return QueryOutcome::Err(QueryError::Unavailable {
                reason: "config_command_detail",
            });
        };
        return match config_command_detail(config, project_id, folder_id, command_id) {
            Some(detail) => QueryOutcome::Ok(QueryResult::TaskCockpit(
                TaskCockpitResult::ConfigCommandDetail(detail),
            )),
            None => QueryOutcome::Err(QueryError::InvalidRequest),
        };
    }
    let Some(task_id) = dispatch.envelope_task_id else {
        return denied(surface, TaskCockpitDeniedReason::MissingTask);
    };
    let snapshot = match dispatch.bus.task_snapshot(task_id) {
        Ok(Some(snapshot)) => snapshot,
        Ok(None) => return denied(surface, TaskCockpitDeniedReason::MissingTask),
        Err(_) => {
            return QueryOutcome::Err(QueryError::Unavailable {
                reason: "task_lookup",
            })
        }
    };
    if snapshot.task.id != task_id {
        return denied(surface, TaskCockpitDeniedReason::Unauthorized);
    }

    match dispatch.query {
        TaskCockpitQuery::ConfigSnapshot
        | TaskCockpitQuery::AgentConnection
        | TaskCockpitQuery::ConfigCreateProject { .. }
        | TaskCockpitQuery::ConfigUpsertCommand { .. }
        | TaskCockpitQuery::ConfigArchiveCommand { .. }
        | TaskCockpitQuery::ConfigRunCommand { .. }
        | TaskCockpitQuery::ConfigCommandDetail { .. }
        | TaskCockpitQuery::ProviderSettings(_) => {
            unreachable!("config snapshot is handled before task-scoped lookup")
        }
        TaskCockpitQuery::BrowserProcessSession => {
            let process_session_id = dispatch
                .service_runtime
                .and_then(|runtime| runtime.live_ai_process_session_for_tab(&task_id.to_string()));
            match process_session_id {
                Some(process_session_id) => QueryOutcome::Ok(QueryResult::TaskCockpit(
                    TaskCockpitResult::BrowserProcessSession(
                        crate::domain::BrowserProcessSessionProjection {
                            task_id,
                            process_session_id,
                        },
                    ),
                )),
                None => unavailable(
                    TaskCockpitSurface::Browser,
                    TaskCockpitUnavailableReason::BrowserProcessSessionUnavailable,
                ),
            }
        }
        TaskCockpitQuery::Conversation { after_sequence } => {
            if !dispatch
                .capabilities
                .contains(Capability::SemanticConversation)
            {
                return QueryOutcome::Err(QueryError::UnsupportedCapability);
            }
            serve_conversation(&dispatch, task_id, *after_sequence)
        }
        TaskCockpitQuery::Terminal => {
            let Some(service) = dispatch.terminal_service else {
                return unavailable(
                    TaskCockpitSurface::Terminal,
                    TaskCockpitUnavailableReason::TerminalUnavailable,
                );
            };
            let terminal = match service.task_terminal_view(task_id) {
                Ok(Some(terminal)) => terminal,
                Ok(None) => {
                    return unavailable(
                        TaskCockpitSurface::Terminal,
                        TaskCockpitUnavailableReason::TerminalUnavailable,
                    )
                }
                Err(_) => {
                    return denied(
                        TaskCockpitSurface::Terminal,
                        TaskCockpitDeniedReason::StaleFence,
                    )
                }
            };
            let Some(primary_agent_id) = snapshot.primary_agent_id else {
                return denied(
                    TaskCockpitSurface::Terminal,
                    TaskCockpitDeniedReason::StaleFence,
                );
            };
            let Some(agent) = snapshot.agents.get(&primary_agent_id) else {
                return denied(
                    TaskCockpitSurface::Terminal,
                    TaskCockpitDeniedReason::StaleFence,
                );
            };
            let matching_resources = snapshot
                .resources
                .values()
                .filter(|resource| {
                    resource.resource_kind == crate::domain::ResourceKind::Terminal
                        && resource.lifecycle == crate::domain::ResourceLifecycle::Active
                })
                .collect::<Vec<_>>();
            let [resource] = matching_resources.as_slice() else {
                return denied(
                    TaskCockpitSurface::Terminal,
                    TaskCockpitDeniedReason::StaleFence,
                );
            };
            if terminal.agent_session_id != primary_agent_id
                || terminal.runtime_generation != agent.runtime_generation
                || terminal.action_epoch == 0
                || terminal.resource_id != resource.id
                || terminal.resource_generation != resource.runtime_generation
            {
                return denied(
                    TaskCockpitSurface::Terminal,
                    TaskCockpitDeniedReason::StaleFence,
                );
            }
            let (screen, text_lines) = compact_terminal_screen_for_wire(terminal.view.screen);
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Terminal(
                TaskTerminalProjection {
                    task_id,
                    terminal_id: terminal.terminal_id,
                    session_id: terminal.session_id,
                    agent_session_id: terminal.agent_session_id,
                    resource_id: terminal.resource_id,
                    runtime_generation: terminal.runtime_generation,
                    resource_generation: terminal.resource_generation,
                    action_epoch: terminal.action_epoch,
                    sequence: terminal.sequence,
                    title: terminal.view.runtime.title,
                    text_lines,
                    screen,
                },
            )))
        }
        TaskCockpitQuery::WorkspaceStatus => QueryOutcome::Ok(QueryResult::TaskCockpit(
            TaskCockpitResult::Workspace(workspace_projection(task_id, &snapshot.task.workspace)),
        )),
        TaskCockpitQuery::GitRepositories => {
            serve_git_repositories(&dispatch, task_id, &snapshot.task)
        }
        TaskCockpitQuery::GitStatus => serve_git_status_targeted(
            &dispatch,
            task_id,
            &snapshot.task,
            &TaskRepositorySelector::Workspace,
        ),
        TaskCockpitQuery::GitStatusTargeted { selector } => {
            serve_git_status_targeted(&dispatch, task_id, &snapshot.task, selector)
        }
        TaskCockpitQuery::GitMutate { intent, confirm } => serve_git_mutate_targeted(
            &dispatch,
            task_id,
            &snapshot.task,
            &TaskRepositorySelector::Workspace,
            intent,
            *confirm,
        ),
        TaskCockpitQuery::GitMutateTargeted {
            selector,
            intent,
            confirm,
        } => serve_git_mutate_targeted(
            &dispatch,
            task_id,
            &snapshot.task,
            selector,
            intent,
            *confirm,
        ),
        TaskCockpitQuery::FilesList {
            relative_directory,
            limit,
        } => {
            if let Some(directory) = relative_directory {
                if !relative_path_is_safe(directory) {
                    return denied(surface, TaskCockpitDeniedReason::PathTraversal);
                }
            }
            if *limit == 0 || *limit > MAX_COCKPIT_FILE_LIST {
                return QueryOutcome::Err(QueryError::InvalidRequest);
            }
            serve_files_list(
                &dispatch,
                task_id,
                &snapshot.task,
                relative_directory.as_deref(),
                *limit,
            )
        }
        TaskCockpitQuery::FilesRead {
            relative_path,
            max_bytes,
        } => {
            if !relative_path_is_safe(relative_path) {
                return denied(surface, TaskCockpitDeniedReason::PathTraversal);
            }
            if *max_bytes == 0 || *max_bytes > MAX_COCKPIT_READ_BYTES {
                return QueryOutcome::Err(QueryError::InvalidRequest);
            }
            serve_files_read(
                &dispatch,
                task_id,
                &snapshot.task,
                relative_path,
                *max_bytes,
            )
        }
        TaskCockpitQuery::FilesWrite {
            relative_path,
            utf8_contents,
            expected_sha256_hex,
            confirm,
        } => {
            if !relative_path_is_safe(relative_path) {
                return denied(surface, TaskCockpitDeniedReason::PathTraversal);
            }
            if utf8_contents.len() > MAX_COCKPIT_READ_BYTES as usize {
                return QueryOutcome::Err(QueryError::InvalidRequest);
            }
            serve_files_write(
                &dispatch,
                task_id,
                &snapshot.task,
                relative_path,
                utf8_contents,
                expected_sha256_hex.as_deref(),
                *confirm,
            )
        }
        TaskCockpitQuery::SshStatus => match dispatch.ssh_endpoints {
            Some(endpoints) => QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Ssh(
                TaskSshProjection {
                    task_id,
                    endpoints: endpoints.to_vec(),
                    runtime: dispatch
                        .ssh_runtime
                        .and_then(|runtime| runtime.status_for_task(task_id))
                        .map(to_ssh_runtime_projection),
                },
            ))),
            None => unavailable(
                TaskCockpitSurface::Ssh,
                TaskCockpitUnavailableReason::SshOperationUnsupported,
            ),
        },
        TaskCockpitQuery::SshAction { endpoint_id } => {
            serve_ssh_action(&dispatch, task_id, &snapshot, endpoint_id)
        }
        TaskCockpitQuery::ServiceSnapshots => match dispatch.service_runtime {
            Some(manager) => match manager.configured_service_snapshots_for_task(task_id) {
                Ok(projection) => match crate::services::cockpit::to_wire_projection(projection) {
                    Ok(services) => QueryOutcome::Ok(QueryResult::TaskCockpit(
                        TaskCockpitResult::Services(services),
                    )),
                    Err(_) => unavailable(
                        TaskCockpitSurface::Services,
                        TaskCockpitUnavailableReason::ServiceSupervisorUnavailable,
                    ),
                },
                Err(_) => unavailable(
                    TaskCockpitSurface::Services,
                    TaskCockpitUnavailableReason::ServiceSupervisorUnavailable,
                ),
            },
            None => unavailable(
                TaskCockpitSurface::Services,
                TaskCockpitUnavailableReason::ServiceSupervisorUnavailable,
            ),
        },
        TaskCockpitQuery::ServiceLogs {
            service_id,
            resource_generation,
            connection_epoch,
            action_epoch,
        } => serve_service_observe(
            &dispatch,
            task_id,
            service_id,
            *resource_generation,
            *connection_epoch,
            *action_epoch,
            SupervisorAction::Logs,
        ),
        TaskCockpitQuery::ServiceHealth {
            service_id,
            resource_generation,
            connection_epoch,
            action_epoch,
        } => serve_service_observe(
            &dispatch,
            task_id,
            service_id,
            *resource_generation,
            *connection_epoch,
            *action_epoch,
            SupervisorAction::Health,
        ),
    }
}

/// `TerminalScreenSnapshot` keeps both a styled indexed grid and a line grid
/// for local consumers. Serializing either rich representation crosses the
/// named MessagePack decoder's collection/value caps at a normal 100x30
/// provider size. Send bounded strings and let the native client reconstruct
/// default-themed paint cells locally while retaining cursor and mode metadata.
fn compact_terminal_screen_for_wire(
    mut screen: crate::terminal::session::TerminalScreenSnapshot,
) -> (
    crate::terminal::session::TerminalScreenSnapshot,
    Vec<String>,
) {
    let text_lines = screen
        .lines
        .iter()
        .map(|cells| {
            let mut line = String::with_capacity(cells.len());
            for cell in cells {
                if cell.hidden {
                    line.push(' ');
                } else {
                    line.push(cell.character);
                    line.extend(cell.zero_width.iter().copied());
                }
            }
            line
        })
        .collect();
    screen.cells.clear();
    screen.lines.clear();
    (screen, text_lines)
}

const MAX_CONVERSATION_PAGE_ITEMS: usize = 128;
const MAX_CONVERSATION_PAGE_BYTES: usize = 256 * 1024;

fn serve_conversation(
    dispatch: &TaskCockpitDispatch<'_>,
    task_id: TaskId,
    after_sequence: u64,
) -> QueryOutcome {
    use crate::domain::{PrivacyClass, SemanticJournalFact, SemanticJournalPage};
    use crate::remote::presentation::StableSessionKey;

    let Some(store) = dispatch.semantic_journal else {
        return unavailable(
            TaskCockpitSurface::Conversation,
            TaskCockpitUnavailableReason::WorkspaceAuthorityUnavailable,
        );
    };
    let Ok(store) = store.lock() else {
        return QueryOutcome::Err(QueryError::Unavailable {
            reason: "semantic_journal",
        });
    };
    let key = StableSessionKey::from_tab(&task_id.to_string());
    let replay = store
        .capture_replay_after(&key, after_sequence)
        .map(|capture| capture.into_replay());
    drop(store);

    let (high_water, events) = match replay {
        Some(replay) => (replay.through_sequence, replay.events),
        None => (0, Vec::new()),
    };
    // Fixed high-water for this capture. `after_sequence` is a forward exclusive
    // cursor: return the next retained prefix page in ascending sequence order.
    let replaced = events
        .iter()
        .filter_map(|event| event.replaces_sequence)
        .collect::<std::collections::BTreeSet<_>>();
    let retained = events
        .iter()
        .filter(|event| event.sequence > after_sequence)
        .filter(|event| !replaced.contains(&event.sequence))
        .filter(|event| !conversation_omits_ai_raw_output(event))
        .collect::<Vec<_>>();
    let facts = retained
        .iter()
        .take(MAX_CONVERSATION_PAGE_ITEMS)
        .map(|event| SemanticJournalFact {
            id: conversation_event_id(task_id, event.sequence),
            sequence: event.sequence,
            occurred_at_ms: i64::try_from(event.occurred_at_epoch_ms).ok(),
            provider: semantic_provider_name(event.source).to_string(),
            schema_version: 1,
            kind: semantic_payload_kind(&event.kind).to_string(),
            visibility: "task".to_string(),
            privacy_class: PrivacyClass::LocalOnly,
            redacted: false,
            payload: semantic_payload(event),
        })
        .collect::<Vec<_>>();
    let mut page = SemanticJournalPage {
        after_sequence,
        through_sequence: facts
            .last()
            .map(|fact| fact.sequence)
            .unwrap_or(after_sequence),
        high_water,
        encoded_bytes: 0,
        next_sequence: None,
        facts,
    };
    loop {
        let encoded = rmp_serde::to_vec_named(&page).unwrap_or_default();
        if encoded.len() <= MAX_CONVERSATION_PAGE_BYTES || page.facts.is_empty() {
            page.encoded_bytes = u32::try_from(encoded.len()).unwrap_or(u32::MAX);
            break;
        }
        // Trim from the end so the page remains a contiguous forward prefix.
        page.facts.pop();
        page.through_sequence = page
            .facts
            .last()
            .map(|fact| fact.sequence)
            .unwrap_or(after_sequence);
    }
    let page_end = page
        .facts
        .last()
        .map(|fact| fact.sequence)
        .unwrap_or(after_sequence);
    let more_retained = retained.iter().any(|event| event.sequence > page_end);
    page.next_sequence = more_retained.then_some(page_end);
    QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Conversation(
        page,
    )))
}

fn conversation_event_id(task_id: TaskId, sequence: u64) -> crate::domain::EventId {
    let mut bytes = *task_id.as_bytes();
    bytes[8..].copy_from_slice(&sequence.to_be_bytes());
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    crate::domain::EventId::from_bytes(bytes).expect("task-derived event id remains UUIDv7")
}

fn conversation_omits_ai_raw_output(event: &crate::remote::presentation::SemanticEvent) -> bool {
    use crate::remote::presentation::{SemanticEventKind, SemanticSource};
    matches!(
        (event.source, &event.kind),
        (
            SemanticSource::Claude | SemanticSource::Codex,
            SemanticEventKind::Output { .. }
        )
    )
}

fn semantic_provider_name(source: crate::remote::presentation::SemanticSource) -> &'static str {
    use crate::remote::presentation::SemanticSource;
    match source {
        SemanticSource::Claude => "claude_code",
        SemanticSource::Codex => "codex",
        SemanticSource::Shell => "shell",
        SemanticSource::Server => "server",
        SemanticSource::Ssh => "ssh",
        SemanticSource::System => "system",
    }
}

fn semantic_status_is_plan_step(state: &str) -> bool {
    crate::domain::provider_plan_step_lifecycle(state).is_some()
}

fn semantic_payload_kind(kind: &crate::remote::presentation::SemanticEventKind) -> &'static str {
    use crate::remote::presentation::SemanticEventKind;
    match kind {
        SemanticEventKind::UserMessage { .. } => "user_message",
        SemanticEventKind::AssistantMessage { .. } | SemanticEventKind::Output { .. } => {
            "assistant_text"
        }
        SemanticEventKind::Reasoning { .. } => "reasoning_summary",
        SemanticEventKind::Tool { .. } | SemanticEventKind::Diff { .. } => "tool_result",
        SemanticEventKind::Command { .. } => "tool_call",
        SemanticEventKind::Question { .. } => "question",
        SemanticEventKind::Status { state, .. } if semantic_status_is_plan_step(state) => {
            "plan_step"
        }
        SemanticEventKind::Status { .. } | SemanticEventKind::TerminalMode { .. } => {
            "session_state"
        }
        SemanticEventKind::Error { .. } => "error",
    }
}

fn semantic_payload(
    event: &crate::remote::presentation::SemanticEvent,
) -> crate::domain::SemanticJournalPayload {
    use crate::domain::{provider_plan_step_lifecycle, SemanticJournalPayload};
    use crate::remote::presentation::{SemanticEventKind, SemanticToolState};
    match &event.kind {
        SemanticEventKind::UserMessage { text } => {
            SemanticJournalPayload::UserMessage { text: text.clone() }
        }
        SemanticEventKind::AssistantMessage { text, .. }
        | SemanticEventKind::Output { text, .. } => {
            SemanticJournalPayload::AssistantText { text: text.clone() }
        }
        SemanticEventKind::Reasoning { summary, .. } => SemanticJournalPayload::ReasoningSummary {
            text: summary.clone(),
        },
        SemanticEventKind::Tool {
            tool_id,
            name,
            state,
            summary,
        } => match state {
            SemanticToolState::Pending | SemanticToolState::Running => {
                SemanticJournalPayload::ToolCall {
                    tool_name: name.clone(),
                    call_id: tool_id.clone(),
                }
            }
            SemanticToolState::Completed | SemanticToolState::Failed => {
                SemanticJournalPayload::ToolResult {
                    call_id: tool_id.clone(),
                    status: if summary.is_empty() {
                        format!("{state:?}").to_ascii_lowercase()
                    } else {
                        summary.clone()
                    },
                }
            }
        },
        SemanticEventKind::Diff {
            item_id,
            unified_diff,
        } => SemanticJournalPayload::ToolResult {
            call_id: item_id.clone(),
            status: unified_diff.clone(),
        },
        SemanticEventKind::Command {
            command_id,
            text,
            exit_code,
        } => SemanticJournalPayload::ToolResult {
            call_id: command_id.clone(),
            status: exit_code
                .map(|code| format!("{text} (exit {code})"))
                .unwrap_or_else(|| text.clone()),
        },
        SemanticEventKind::Question {
            question_id,
            prompt,
            choices,
        } => SemanticJournalPayload::Question {
            question_id: question_id.clone(),
            prompt: prompt.clone(),
            options: choices.clone(),
        },
        SemanticEventKind::Status { state, detail } if semantic_status_is_plan_step(state) => {
            let lifecycle = provider_plan_step_lifecycle(state)
                .expect("plan-step classification and projection share one typed mapping");
            let identity_sequence = event.replaces_sequence.unwrap_or(event.sequence);
            SemanticJournalPayload::PlanStep {
                step_id: format!("{}:{identity_sequence}", lifecycle.kind.as_str()),
                title: detail.clone().unwrap_or_else(|| match lifecycle.kind {
                    crate::domain::PlanStepKind::Task => "Task".to_string(),
                    crate::domain::PlanStepKind::Subagent => "Subagent".to_string(),
                }),
                status: lifecycle.status.as_str().to_string(),
            }
        }
        SemanticEventKind::Status { state, detail } => SemanticJournalPayload::SessionState {
            state: detail
                .as_ref()
                .map(|detail| format!("{state}: {detail}"))
                .unwrap_or_else(|| state.clone()),
        },
        SemanticEventKind::Error { message } => SemanticJournalPayload::Error {
            code: "provider".to_string(),
            message: message.clone(),
        },
        SemanticEventKind::TerminalMode { raw_required } => SemanticJournalPayload::SessionState {
            state: if *raw_required {
                "raw terminal required"
            } else {
                "semantic conversation active"
            }
            .to_string(),
        },
    }
}

pub(crate) fn config_command_detail(
    config: &AppConfig,
    project_id: &str,
    folder_id: &str,
    command_id: &str,
) -> Option<crate::domain::ConfigCommandDetailProjection> {
    const MAX_LABEL: usize = 96;
    const MAX_COMMAND: usize = 4_096;
    let project = config
        .projects
        .iter()
        .find(|project| project.id == project_id && !is_archived(&project.archived))?;
    let folder = project
        .folders
        .iter()
        .find(|folder| folder.id == folder_id && !is_archived(&folder.archived))?;
    let command = folder
        .commands
        .iter()
        .find(|command| command.id == command_id && !is_archived(&command.archived))?;
    Some(crate::domain::ConfigCommandDetailProjection {
        project_id: bounded_config_text(&project.id, MAX_LABEL),
        folder_id: bounded_config_text(&folder.id, MAX_LABEL),
        command_id: bounded_config_text(&command.id, MAX_LABEL),
        label: bounded_config_text(
            if command.label.trim().is_empty() {
                &command.id
            } else {
                &command.label
            },
            MAX_LABEL,
        ),
        command: bounded_config_text(&command.command, MAX_COMMAND),
    })
}

pub(crate) fn config_sidebar_snapshot(config: &AppConfig) -> ConfigSidebarSnapshot {
    const MAX_PROJECTS: usize = 128;
    const MAX_FOLDERS: usize = 64;
    const MAX_SERVERS: usize = 256;
    const MAX_LABEL: usize = 96;
    const MAX_HOST: usize = 160;
    let projects = config
        .projects
        .iter()
        .filter(|project| !is_archived(&project.archived))
        .take(MAX_PROJECTS)
        .map(|project| ConfigSidebarProject {
            config_id: bounded_config_text(&project.id, MAX_LABEL),
            label: bounded_config_text(
                if project.name.trim().is_empty() {
                    &project.id
                } else {
                    &project.name
                },
                MAX_LABEL,
            ),
            root_configured: !project.root_path.trim().is_empty(),
            workspace_id: config
                .workspace_project_ids()
                .get(&project.id)
                .cloned()
                .unwrap_or_default(),
            folders: project
                .folders
                .iter()
                .filter(|folder| !is_archived(&folder.archived))
                .take(MAX_FOLDERS)
                .map(|folder| ConfigSidebarFolder {
                    config_id: bounded_config_text(&folder.id, MAX_LABEL),
                    label: bounded_config_text(
                        if folder.name.trim().is_empty() {
                            &folder.id
                        } else {
                            &folder.name
                        },
                        MAX_LABEL,
                    ),
                    server_count: folder
                        .commands
                        .iter()
                        .filter(|command| !is_archived(&command.archived))
                        .count(),
                })
                .collect(),
        })
        .collect();
    let mut servers = Vec::new();
    for project in config
        .projects
        .iter()
        .filter(|project| !is_archived(&project.archived))
    {
        for folder in project
            .folders
            .iter()
            .filter(|folder| !is_archived(&folder.archived))
            .take(MAX_FOLDERS)
        {
            for command in folder
                .commands
                .iter()
                .filter(|command| !is_archived(&command.archived))
            {
                if servers.len() == MAX_SERVERS {
                    break;
                }
                servers.push(ConfigSidebarServer {
                    project_id: bounded_config_text(&project.id, MAX_LABEL),
                    folder_id: bounded_config_text(&folder.id, MAX_LABEL),
                    command_id: bounded_config_text(&command.id, MAX_LABEL),
                    project_label: bounded_config_text(&project.name, MAX_LABEL),
                    folder_label: bounded_config_text(&folder.name, MAX_LABEL),
                    label: bounded_config_text(
                        if command.label.trim().is_empty() {
                            &command.id
                        } else {
                            &command.label
                        },
                        MAX_LABEL,
                    ),
                    port: command.port.as_ref().copied(),
                });
            }
        }
    }
    let ssh_connections = config
        .ssh_connections
        .iter()
        .filter(|connection| !is_archived(&connection.archived))
        .take(MAX_SERVERS)
        .map(|connection| ConfigSidebarSsh {
            config_id: bounded_config_text(&connection.id, MAX_LABEL),
            label: bounded_config_text(
                if connection.label.trim().is_empty() {
                    &connection.id
                } else {
                    &connection.label
                },
                MAX_LABEL,
            ),
            host: bounded_config_text(&connection.host, MAX_HOST),
            port: connection.port,
            username: bounded_config_text(&connection.username, MAX_LABEL),
        })
        .collect();
    let settings = config.settings();
    let providers = vec![
        ConfigSidebarProvider {
            provider: ConfigSidebarProviderKind::Claude,
            command_configured: settings
                .claude_command
                .as_ref()
                .is_some_and(|command| !command.trim().is_empty()),
        },
        ConfigSidebarProvider {
            provider: ConfigSidebarProviderKind::Codex,
            command_configured: settings
                .codex_command
                .as_ref()
                .is_some_and(|command| !command.trim().is_empty()),
        },
    ];
    ConfigSidebarSnapshot {
        revision: config.revision,
        projects,
        servers,
        ssh_connections,
        providers,
    }
}

fn is_archived(value: &crate::config::Nullable<bool>) -> bool {
    value.as_ref().copied().unwrap_or(false)
}

fn bounded_config_text(value: &str, max: usize) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .take(max)
        .collect()
}

fn serve_service_observe(
    dispatch: &TaskCockpitDispatch<'_>,
    task_id: TaskId,
    service_id: &crate::domain::id::ConfiguredServiceId,
    resource_generation: u64,
    connection_epoch: u64,
    action_epoch: u64,
    action: SupervisorAction,
) -> QueryOutcome {
    let Some(manager) = dispatch.service_runtime else {
        return unavailable(
            TaskCockpitSurface::Services,
            match action {
                SupervisorAction::Logs => TaskCockpitUnavailableReason::LogsUnsupported,
                SupervisorAction::Health => TaskCockpitUnavailableReason::HealthUnsupported,
                SupervisorAction::Start | SupervisorAction::Stop | SupervisorAction::Restart => {
                    TaskCockpitUnavailableReason::ServiceSupervisorUnavailable
                }
            },
        );
    };
    let supervisor_id = match crate::services::cockpit::supervisor_service_id(service_id) {
        Ok(id) => id,
        Err(_) => {
            return denied(
                TaskCockpitSurface::Services,
                TaskCockpitDeniedReason::UnknownService,
            )
        }
    };
    let fence = AdmissionFence::new(resource_generation, connection_epoch, action_epoch);
    match manager.configured_service_observe_for_task(action, &supervisor_id, fence, task_id) {
        Ok(SupervisorOutcome::Logs(log)) => {
            match crate::services::cockpit::to_wire_logs(task_id, service_id.clone(), log) {
                Ok(logs) => QueryOutcome::Ok(QueryResult::TaskCockpit(
                    TaskCockpitResult::ServiceLogs(logs),
                )),
                Err(_) => unavailable(
                    TaskCockpitSurface::Services,
                    TaskCockpitUnavailableReason::ServiceSupervisorUnavailable,
                ),
            }
        }
        Ok(SupervisorOutcome::Health(snapshot)) => {
            match crate::services::cockpit::to_wire_health(task_id, snapshot) {
                Ok(health) => QueryOutcome::Ok(QueryResult::TaskCockpit(
                    TaskCockpitResult::ServiceHealth(health),
                )),
                Err(_) => unavailable(
                    TaskCockpitSurface::Services,
                    TaskCockpitUnavailableReason::ServiceSupervisorUnavailable,
                ),
            }
        }
        Ok(_) => unavailable(
            TaskCockpitSurface::Services,
            TaskCockpitUnavailableReason::ServiceSupervisorUnavailable,
        ),
        Err(error) => map_supervisor_observe_error(error),
    }
}

fn map_supervisor_observe_error(error: SupervisorError) -> QueryOutcome {
    match error {
        SupervisorError::UnknownService(_) => denied(
            TaskCockpitSurface::Services,
            TaskCockpitDeniedReason::UnknownService,
        ),
        SupervisorError::Refused(SupervisorRefusal::StaleFence) => denied(
            TaskCockpitSurface::Services,
            TaskCockpitDeniedReason::StaleFence,
        ),
        SupervisorError::Refused(SupervisorRefusal::Ownership) => denied(
            TaskCockpitSurface::Services,
            TaskCockpitDeniedReason::ForeignScope,
        ),
        SupervisorError::StaleGeneration { .. } => denied(
            TaskCockpitSurface::Services,
            TaskCockpitDeniedReason::StaleFence,
        ),
        _ => unavailable(
            TaskCockpitSurface::Services,
            TaskCockpitUnavailableReason::ServiceSupervisorUnavailable,
        ),
    }
}

fn serve_git_repositories(
    dispatch: &TaskCockpitDispatch<'_>,
    task_id: TaskId,
    task: &crate::domain::task::TaskFacts,
) -> QueryOutcome {
    let Some(projects) = dispatch.workspace_projects else {
        return unavailable(
            TaskCockpitSurface::Git,
            TaskCockpitUnavailableReason::WorkspaceAuthorityUnavailable,
        );
    };
    let (service, authorization, command_id, action_epoch, runtime_generation) =
        match live_mutation_authority(dispatch, task_id, task, TaskCockpitSurface::Git) {
            Ok(authority) => authority,
            Err(outcome) => return outcome,
        };
    let _ = (authorization, command_id, action_epoch, runtime_generation);
    let binding = service.current();
    let workspace_path = binding.map(|bound| bound.path().to_path_buf());
    let workspace_repository = binding.and_then(|bound| bound.repository().cloned());
    let catalog = crate::workspace::repository_targets::build_task_repository_catalog(
        task_id,
        task.project_id,
        &task.workspace,
        workspace_path.as_deref(),
        workspace_repository.as_ref(),
        projects,
    );
    QueryOutcome::Ok(QueryResult::TaskCockpit(
        TaskCockpitResult::GitRepositories(catalog),
    ))
}

fn serve_git_status_targeted(
    dispatch: &TaskCockpitDispatch<'_>,
    task_id: TaskId,
    task: &crate::domain::task::TaskFacts,
    selector: &TaskRepositorySelector,
) -> QueryOutcome {
    match open_git_repository_targeted(dispatch, task_id, task, selector) {
        Ok((repository, resolved)) => git_status_outcome(task_id, &repository, &resolved),
        Err(outcome) => outcome,
    }
}

fn serve_git_mutate_targeted(
    dispatch: &TaskCockpitDispatch<'_>,
    task_id: TaskId,
    task: &crate::domain::task::TaskFacts,
    selector: &TaskRepositorySelector,
    intent: &TaskGitMutateIntent,
    confirm: bool,
) -> QueryOutcome {
    if dispatch.envelope_task_id != Some(task_id) {
        return denied(
            TaskCockpitSurface::Git,
            TaskCockpitDeniedReason::Unauthorized,
        );
    }
    if selector.validate().is_err() {
        return QueryOutcome::Err(QueryError::InvalidRequest);
    }
    let paths = match intent {
        TaskGitMutateIntent::Stage { relative_paths }
        | TaskGitMutateIntent::Unstage { relative_paths } => {
            match cockpit_repo_paths(relative_paths) {
                Ok(paths) => Some(paths),
                Err(outcome) => return outcome,
            }
        }
        TaskGitMutateIntent::Commit { message } => {
            if message.trim().is_empty() || message.len() > MAX_COCKPIT_READ_BYTES as usize {
                return QueryOutcome::Err(QueryError::InvalidRequest);
            }
            None
        }
    };
    let (repository, resolved) =
        match open_git_repository_targeted(dispatch, task_id, task, selector) {
            Ok(opened) => opened,
            Err(outcome) => return outcome,
        };
    if !resolved.mutation_allowed {
        return denied(
            TaskCockpitSurface::Git,
            TaskCockpitDeniedReason::CapabilityDenied,
        );
    }
    if let Err(outcome) = revalidate_git_fence(dispatch, task_id, task) {
        return outcome;
    }
    // Revalidate configured identity immediately before mutation planning.
    if let Err(outcome) = revalidate_configured_target_identity(dispatch, task_id, task, selector) {
        return outcome;
    }
    let planned = match intent {
        TaskGitMutateIntent::Stage { .. } => {
            let paths = paths.expect("stage paths");
            match repository.plan_stage(&paths) {
                Ok(plan) => GitPlannedMutation::Stage(plan),
                Err(error) => {
                    eprintln!("devmanager-host: cockpit Git stage planning failed: {error}");
                    return map_git_error(error);
                }
            }
        }
        TaskGitMutateIntent::Unstage { .. } => {
            let paths = paths.expect("unstage paths");
            match repository.plan_unstage(&paths) {
                Ok(plan) => GitPlannedMutation::Unstage(plan),
                Err(error) => return map_git_error(error),
            }
        }
        TaskGitMutateIntent::Commit { message } => match repository.plan_commit(message) {
            Ok(plan) => GitPlannedMutation::Commit(plan),
            Err(error) => return map_git_error(error),
        },
    };
    if !confirm {
        return git_status_outcome(task_id, &repository, &resolved);
    }
    if let Err(outcome) = revalidate_git_fence(dispatch, task_id, task) {
        return outcome;
    }
    if let Err(outcome) = revalidate_configured_target_identity(dispatch, task_id, task, selector) {
        return outcome;
    }
    let executed = match &planned {
        GitPlannedMutation::Stage(plan) => confirm_git_mutation(&repository, plan)
            .and_then(|confirmation| repository.stage(plan, &confirmation)),
        GitPlannedMutation::Unstage(plan) => confirm_git_mutation(&repository, plan)
            .and_then(|confirmation| repository.unstage(plan, &confirmation)),
        GitPlannedMutation::Commit(plan) => confirm_git_mutation(&repository, plan)
            .and_then(|confirmation| repository.commit(plan, &confirmation)),
    };
    match executed {
        Ok(()) => {
            drop(repository);
            serve_git_status_targeted(dispatch, task_id, task, selector)
        }
        Err(error) => map_git_error(error),
    }
}

enum GitPlannedMutation {
    Stage(crate::git::model::StagePlan),
    Unstage(crate::git::model::UnstagePlan),
    Commit(crate::git::model::CommitPlan),
}

fn cockpit_repo_paths(paths: &[String]) -> Result<Vec<RepoPath>, QueryOutcome> {
    if paths.is_empty() || paths.len() > usize::from(MAX_COCKPIT_FILE_LIST) {
        return Err(QueryOutcome::Err(QueryError::InvalidRequest));
    }
    let mut converted = Vec::with_capacity(paths.len());
    for path in paths {
        if !relative_path_is_safe(path) {
            return Err(denied(
                TaskCockpitSurface::Git,
                TaskCockpitDeniedReason::PathTraversal,
            ));
        }
        if crate::workspace::files::WorkspaceFileService::classify_secret_path(path)
            == SecretClassification::SecretLike
        {
            return Err(denied(
                TaskCockpitSurface::Git,
                TaskCockpitDeniedReason::CapabilityDenied,
            ));
        }
        converted.push(RepoPath::from_bytes(path.as_bytes().to_vec()));
    }
    Ok(converted)
}

fn confirm_git_mutation<P: MutationPlan>(
    repository: &GitRepository,
    plan: &P,
) -> Result<GitConfirmation, GitError> {
    repository.host_confirm(plan)
}

fn map_git_error(error: GitError) -> QueryOutcome {
    match error {
        GitError::InvalidPath { .. } => denied(
            TaskCockpitSurface::Git,
            TaskCockpitDeniedReason::PathTraversal,
        ),
        GitError::InvalidRequest { .. } => QueryOutcome::Err(QueryError::InvalidRequest),
        GitError::CapabilityDenied { .. } => denied(
            TaskCockpitSurface::Git,
            TaskCockpitDeniedReason::CapabilityDenied,
        ),
        GitError::FingerprintMismatch { .. } | GitError::ConfirmationMismatch { .. } => denied(
            TaskCockpitSurface::Git,
            TaskCockpitDeniedReason::RevisionConflict,
        ),
        GitError::WorkspaceMismatch { .. } => denied(
            TaskCockpitSurface::Git,
            TaskCockpitDeniedReason::Unauthorized,
        ),
        GitError::AuthorityUnavailable => unavailable(
            TaskCockpitSurface::Git,
            TaskCockpitUnavailableReason::GitAuthorityNotIssued,
        ),
        _ => unavailable(
            TaskCockpitSurface::Git,
            TaskCockpitUnavailableReason::GitAuthorityNotIssued,
        ),
    }
}

fn git_status_outcome(
    task_id: TaskId,
    repository: &GitRepository,
    resolved: &crate::workspace::repository_targets::ResolvedRepositoryTarget,
) -> QueryOutcome {
    let started = std::time::Instant::now();
    match repository.status_summary() {
        Ok(status) => QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Git(
            TaskGitProjection {
                task_id,
                selector: Some(resolved.selector.clone()),
                label: Some(resolved.label.clone()),
                branch: status.branch.as_ref().map(|name| name.as_str().to_owned()),
                ahead: status.ahead,
                behind: status.behind,
                change_count: status
                    .entries
                    .iter()
                    .filter(|entry| entry.kind != StatusKind::Unknown)
                    .count() as u32,
                detached: status.is_detached,
            },
        ))),
        Err(error) => {
            eprintln!(
                "devmanager-host: cockpit Git status failed after {:?}: {error:?}",
                started.elapsed()
            );
            unavailable(
                TaskCockpitSurface::Git,
                TaskCockpitUnavailableReason::GitAuthorityNotIssued,
            )
        }
    }
}

fn open_git_repository_targeted(
    dispatch: &TaskCockpitDispatch<'_>,
    task_id: TaskId,
    task: &crate::domain::task::TaskFacts,
    selector: &TaskRepositorySelector,
) -> Result<
    (
        GitRepository,
        crate::workspace::repository_targets::ResolvedRepositoryTarget,
    ),
    QueryOutcome,
> {
    if selector.validate().is_err() {
        return Err(QueryOutcome::Err(QueryError::InvalidRequest));
    }
    let Some(projects) = dispatch.workspace_projects else {
        return Err(unavailable(
            TaskCockpitSurface::Git,
            TaskCockpitUnavailableReason::WorkspaceAuthorityUnavailable,
        ));
    };
    let (service, authorization, command_id, action_epoch, runtime_generation) =
        live_mutation_authority(dispatch, task_id, task, TaskCockpitSurface::Git)?;
    let binding = service.current();
    let workspace_path = binding.map(|bound| bound.path().to_path_buf());
    let workspace_repository = binding.and_then(|bound| bound.repository().cloned());
    let resolved = crate::workspace::repository_targets::resolve_repository_target(
        selector,
        task.project_id,
        &task.workspace,
        workspace_path.as_deref(),
        workspace_repository.as_ref(),
        projects,
    )
    .map_err(|error| match error {
        crate::workspace::repository_targets::RepositoryTargetError::InvalidSelector => {
            QueryOutcome::Err(QueryError::InvalidRequest)
        }
        crate::workspace::repository_targets::RepositoryTargetError::UnknownSelector => denied(
            TaskCockpitSurface::Git,
            TaskCockpitDeniedReason::Unauthorized,
        ),
        crate::workspace::repository_targets::RepositoryTargetError::ReadOnly
        | crate::workspace::repository_targets::RepositoryTargetError::Unavailable => denied(
            TaskCockpitSurface::Git,
            TaskCockpitDeniedReason::CapabilityDenied,
        ),
        crate::workspace::repository_targets::RepositoryTargetError::StaleIdentity => {
            denied(TaskCockpitSurface::Git, TaskCockpitDeniedReason::StaleFence)
        }
    })?;
    if !resolved.available {
        return Err(denied(
            TaskCockpitSurface::Git,
            TaskCockpitDeniedReason::CapabilityDenied,
        ));
    }
    let lease = service
        .acquire_task_resource(
            task_id,
            WorkspaceResource::Git,
            dispatch.client_id,
            dispatch.connection_id,
            dispatch.request_id,
            command_id,
            action_epoch,
            runtime_generation,
        )
        .map_err(|error| {
            eprintln!("devmanager-host: cockpit Git lease acquisition failed: {error:?}");
            unavailable(
                TaskCockpitSurface::Git,
                TaskCockpitUnavailableReason::GitAuthorityNotIssued,
            )
        })?;
    revalidate_issued_fence(
        dispatch,
        task_id,
        task,
        &authorization,
        &lease,
        command_id,
        action_epoch,
        runtime_generation,
        WorkspaceResource::Git,
        TaskCockpitSurface::Git,
    )
    .map_err(|outcome| {
        eprintln!("devmanager-host: cockpit Git fence revalidation failed: {outcome:?}");
        outcome
    })?;
    let binding = match &resolved.selector {
        TaskRepositorySelector::Workspace => issue_git_host_binding(
            &authorization,
            lease,
            task_id,
            task.project_id,
            dispatch.client_id,
            dispatch.connection_id,
            dispatch.request_id,
            command_id,
            &task.workspace,
            action_epoch,
            runtime_generation,
        ),
        TaskRepositorySelector::ProjectRoot | TaskRepositorySelector::Folder { .. } => {
            issue_configured_repository_git_host_binding(
                &authorization,
                lease,
                task_id,
                task.project_id,
                dispatch.client_id,
                dispatch.connection_id,
                dispatch.request_id,
                command_id,
                &task.workspace,
                action_epoch,
                runtime_generation,
                &resolved.path,
                &resolved.identity,
            )
        }
    }
    .map_err(|error| {
        match &error {
            GitError::InvalidRepositoryRoot { reason, .. } => {
                eprintln!("devmanager-host: cockpit Git binding failed: {reason}");
            }
            _ => eprintln!("devmanager-host: cockpit Git binding failed: {error}"),
        }
        unavailable(
            TaskCockpitSurface::Git,
            TaskCockpitUnavailableReason::GitAuthorityNotIssued,
        )
    })?;
    let repository =
        GitRepository::from_host_binding(binding, GitCancellation::new()).map_err(|error| {
            eprintln!("devmanager-host: cockpit Git repository open failed: {error:?}");
            unavailable(
                TaskCockpitSurface::Git,
                TaskCockpitUnavailableReason::GitAuthorityNotIssued,
            )
        })?;
    Ok((repository, resolved))
}

fn revalidate_configured_target_identity(
    dispatch: &TaskCockpitDispatch<'_>,
    task_id: TaskId,
    task: &crate::domain::task::TaskFacts,
    selector: &TaskRepositorySelector,
) -> Result<(), QueryOutcome> {
    match selector {
        TaskRepositorySelector::Workspace => Ok(()),
        TaskRepositorySelector::ProjectRoot | TaskRepositorySelector::Folder { .. } => {
            let Some(projects) = dispatch.workspace_projects else {
                return Err(unavailable(
                    TaskCockpitSurface::Git,
                    TaskCockpitUnavailableReason::WorkspaceAuthorityUnavailable,
                ));
            };
            let (service, _, _, _, _) =
                live_mutation_authority(dispatch, task_id, task, TaskCockpitSurface::Git)?;
            let binding = service.current();
            let workspace_path = binding.map(|bound| bound.path().to_path_buf());
            let workspace_repository = binding.and_then(|bound| bound.repository().cloned());
            let resolved = crate::workspace::repository_targets::resolve_repository_target(
                selector,
                task.project_id,
                &task.workspace,
                workspace_path.as_deref(),
                workspace_repository.as_ref(),
                projects,
            )
            .map_err(|_| denied(TaskCockpitSurface::Git, TaskCockpitDeniedReason::StaleFence))?;
            if !resolved.mutation_allowed {
                return Err(denied(
                    TaskCockpitSurface::Git,
                    TaskCockpitDeniedReason::CapabilityDenied,
                ));
            }
            Ok(())
        }
    }
}

fn revalidate_git_fence(
    dispatch: &TaskCockpitDispatch<'_>,
    task_id: TaskId,
    task: &crate::domain::task::TaskFacts,
) -> Result<(), QueryOutcome> {
    remint_and_check_authorization(dispatch, task_id, task, TaskCockpitSurface::Git)
}

fn serve_files_list(
    dispatch: &TaskCockpitDispatch<'_>,
    task_id: TaskId,
    task: &crate::domain::task::TaskFacts,
    relative_directory: Option<&str>,
    limit: u16,
) -> QueryOutcome {
    let files = match live_read_file_service(dispatch, task_id, task) {
        Ok(files) => files,
        Err(outcome) => return outcome,
    };
    match files.list_page(
        relative_directory,
        FilePageRequest {
            offset: 0,
            limit: usize::from(limit),
        },
    ) {
        Ok(page) => {
            let truncated = page.next_offset.is_some();
            let entries = page
                .entries
                .into_iter()
                .filter_map(|entry| {
                    if !relative_path_is_safe(entry.path.as_str()) {
                        return None;
                    }
                    Some(TaskFileEntry {
                        relative_path: entry.path.as_str().to_owned(),
                        is_directory: entry.kind == EntryKind::Directory,
                        secret: entry.secret == SecretClassification::SecretLike,
                    })
                })
                .collect();
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::FilesList(
                TaskFilesListProjection {
                    task_id,
                    entries,
                    truncated,
                },
            )))
        }
        Err(error) => map_file_error(error),
    }
}

fn serve_files_read(
    dispatch: &TaskCockpitDispatch<'_>,
    task_id: TaskId,
    task: &crate::domain::task::TaskFacts,
    relative_path: &str,
    max_bytes: u32,
) -> QueryOutcome {
    let files = match live_read_file_service(dispatch, task_id, task) {
        Ok(files) => files,
        Err(outcome) => return outcome,
    };
    let options = ReadOptions {
        chunk_bytes: (max_bytes as usize).min(MAX_CHUNK_BYTES).max(1),
        total_bytes: max_bytes as usize,
    };
    if crate::workspace::files::WorkspaceFileService::classify_secret_path(relative_path)
        == SecretClassification::SecretLike
    {
        return denied(
            TaskCockpitSurface::Files,
            TaskCockpitDeniedReason::CapabilityDenied,
        );
    }
    match files.read(relative_path, options) {
        Ok(result) => {
            if !relative_path_is_safe(result.path.as_str()) {
                return denied(
                    TaskCockpitSurface::Files,
                    TaskCockpitDeniedReason::PathTraversal,
                );
            }
            if crate::workspace::files::WorkspaceFileService::classify_secret_path(
                result.path.as_str(),
            ) == SecretClassification::SecretLike
            {
                return denied(
                    TaskCockpitSurface::Files,
                    TaskCockpitDeniedReason::CapabilityDenied,
                );
            }
            let mut collected = Vec::new();
            for chunk in &result.chunks {
                let remaining = (max_bytes as usize).saturating_sub(collected.len());
                if remaining == 0 {
                    break;
                }
                collected.extend_from_slice(&chunk.bytes[..chunk.bytes.len().min(remaining)]);
            }
            let utf8_prefix = match result.content_kind {
                ContentKind::Text => {
                    crate::workspace::files::WorkspaceFileService::bounded_utf8_prefix(
                        &collected,
                        max_bytes as usize,
                    )
                }
                ContentKind::Binary => None,
            };
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::FilesRead(
                TaskFilesReadProjection {
                    task_id,
                    relative_path: result.path.as_str().to_owned(),
                    utf8_prefix,
                    byte_len: result.total_bytes.min(u64::from(u32::MAX)) as u32,
                    secret: false,
                },
            )))
        }
        Err(error) => map_file_error(error),
    }
}

fn serve_files_write(
    dispatch: &TaskCockpitDispatch<'_>,
    task_id: TaskId,
    task: &crate::domain::task::TaskFacts,
    relative_path: &str,
    utf8_contents: &str,
    expected_sha256_hex: Option<&str>,
    confirm: bool,
) -> QueryOutcome {
    if dispatch.envelope_task_id != Some(task_id) {
        return denied(
            TaskCockpitSurface::Files,
            TaskCockpitDeniedReason::Unauthorized,
        );
    }
    if crate::workspace::files::WorkspaceFileService::classify_secret_path(relative_path)
        == SecretClassification::SecretLike
    {
        return denied(
            TaskCockpitSurface::Files,
            TaskCockpitDeniedReason::CapabilityDenied,
        );
    }
    if !confirm {
        return unavailable(
            TaskCockpitSurface::Files,
            TaskCockpitUnavailableReason::WriteUnsupported,
        );
    }
    let files = match live_file_service(dispatch, task_id, task) {
        Ok(files) => files,
        Err(outcome) => return outcome,
    };
    if let Err(outcome) =
        remint_and_check_authorization(dispatch, task_id, task, TaskCockpitSurface::Files)
    {
        return outcome;
    }
    let expected = match expected_revision(relative_path, expected_sha256_hex, &files) {
        Ok(expected) => expected,
        Err(outcome) => return outcome,
    };
    if let Err(outcome) =
        remint_and_check_authorization(dispatch, task_id, task, TaskCockpitSurface::Files)
    {
        return outcome;
    }
    let plan = match files.plan_write(relative_path, utf8_contents.as_bytes().to_vec(), expected) {
        Ok(plan) => plan,
        Err(error) => return map_file_error(error),
    };
    if let Err(outcome) =
        remint_and_check_authorization(dispatch, task_id, task, TaskCockpitSurface::Files)
    {
        return outcome;
    }
    match files.execute_write(plan) {
        Ok(written) => {
            if !relative_path_is_safe(written.path.as_str()) {
                return denied(
                    TaskCockpitSurface::Files,
                    TaskCockpitDeniedReason::PathTraversal,
                );
            }
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::FilesRead(
                TaskFilesReadProjection {
                    task_id,
                    relative_path: written.path.as_str().to_owned(),
                    utf8_prefix: Some(crate::domain::cockpit::truncate_to_max_bytes(
                        utf8_contents,
                        MAX_COCKPIT_READ_BYTES as usize,
                    )),
                    byte_len: written.bytes_written.min(u32::MAX as usize) as u32,
                    secret: false,
                },
            )))
        }
        Err(error) => map_file_error(error),
    }
}

fn expected_revision(
    relative_path: &str,
    expected_sha256_hex: Option<&str>,
    files: &crate::workspace::files::WorkspaceFileService,
) -> Result<ExpectedRevision, QueryOutcome> {
    let Some(hex) = expected_sha256_hex else {
        return Ok(ExpectedRevision::missing());
    };
    let expected_hash =
        parse_sha256_hex(hex).ok_or_else(|| QueryOutcome::Err(QueryError::InvalidRequest))?;
    match files.current_revision(relative_path) {
        Ok(revision) if revision.sha256 == Some(expected_hash) => {
            Ok(ExpectedRevision::exact(revision))
        }
        Ok(_) => Err(denied(
            TaskCockpitSurface::Files,
            TaskCockpitDeniedReason::RevisionConflict,
        )),
        Err(FileServiceError::NotFound { .. }) => Err(denied(
            TaskCockpitSurface::Files,
            TaskCockpitDeniedReason::RevisionConflict,
        )),
        Err(error) => Err(map_file_error(error)),
    }
}

fn parse_sha256_hex(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut digest = [0u8; 32];
    for (index, chunk) in value.as_bytes().chunks_exact(2).enumerate() {
        digest[index] = u8::from_str_radix(std::str::from_utf8(chunk).ok()?, 16).ok()?;
    }
    Some(digest)
}

fn live_file_service(
    dispatch: &TaskCockpitDispatch<'_>,
    task_id: TaskId,
    task: &crate::domain::task::TaskFacts,
) -> Result<crate::workspace::files::WorkspaceFileService, QueryOutcome> {
    live_file_service_with_access(dispatch, task_id, task, FileServiceAccess::ReadWrite)
}

fn live_read_file_service(
    dispatch: &TaskCockpitDispatch<'_>,
    task_id: TaskId,
    task: &crate::domain::task::TaskFacts,
) -> Result<crate::workspace::files::WorkspaceFileService, QueryOutcome> {
    live_file_service_with_access(dispatch, task_id, task, FileServiceAccess::ReadOnly)
}

#[derive(Clone, Copy)]
enum FileServiceAccess {
    ReadOnly,
    ReadWrite,
}

fn live_file_service_with_access(
    dispatch: &TaskCockpitDispatch<'_>,
    task_id: TaskId,
    task: &crate::domain::task::TaskFacts,
    access: FileServiceAccess,
) -> Result<crate::workspace::files::WorkspaceFileService, QueryOutcome> {
    let (service, authorization, command_id, action_epoch, runtime_generation) =
        match live_mutation_authority(dispatch, task_id, task, TaskCockpitSurface::Files) {
            Ok(authority) => authority,
            Err(outcome) => return Err(outcome),
        };
    let lease = service
        .acquire_task_resource(
            task_id,
            WorkspaceResource::File,
            dispatch.client_id,
            dispatch.connection_id,
            dispatch.request_id,
            command_id,
            action_epoch,
            runtime_generation,
        )
        .map_err(|_| {
            unavailable(
                TaskCockpitSurface::Files,
                TaskCockpitUnavailableReason::FileAuthorityNotIssued,
            )
        })?;
    if let Err(outcome) = revalidate_issued_fence(
        dispatch,
        task_id,
        task,
        &authorization,
        &lease,
        command_id,
        action_epoch,
        runtime_generation,
        WorkspaceResource::File,
        TaskCockpitSurface::Files,
    ) {
        return Err(outcome);
    }
    let issue = match access {
        FileServiceAccess::ReadOnly => issue_read_file_service,
        FileServiceAccess::ReadWrite => issue_file_service,
    };
    issue(
        &authorization,
        lease,
        task_id,
        task.project_id,
        dispatch.client_id,
        dispatch.connection_id,
        dispatch.request_id,
        command_id,
        &task.workspace,
        action_epoch,
        runtime_generation,
    )
    .map_err(|_| {
        unavailable(
            TaskCockpitSurface::Files,
            TaskCockpitUnavailableReason::FileAuthorityNotIssued,
        )
    })
}

fn live_workspace_authority(
    dispatch: &TaskCockpitDispatch<'_>,
    task_id: TaskId,
    task: &crate::domain::task::TaskFacts,
) -> Option<(
    WorkspaceService,
    crate::workspace::WorkspaceAuthorization,
    CommandId,
    u64,
    u64,
)> {
    live_mutation_authority(dispatch, task_id, task, TaskCockpitSurface::Workspace).ok()
}

fn live_mutation_authority(
    dispatch: &TaskCockpitDispatch<'_>,
    task_id: TaskId,
    task: &crate::domain::task::TaskFacts,
    surface: TaskCockpitSurface,
) -> Result<
    (
        WorkspaceService,
        WorkspaceAuthorization,
        CommandId,
        u64,
        u64,
    ),
    QueryOutcome,
> {
    if dispatch.envelope_task_id != Some(task_id) {
        return Err(denied(surface, TaskCockpitDeniedReason::Unauthorized));
    }
    let Some(projects) = dispatch.workspace_projects else {
        return Err(unavailable(
            surface,
            TaskCockpitUnavailableReason::WorkspaceAuthorityUnavailable,
        ));
    };
    let Some(coordinator) = dispatch.coordinator else {
        return Err(unavailable(
            surface,
            TaskCockpitUnavailableReason::WorkspaceAuthorityUnavailable,
        ));
    };
    let Some(action_epoch) = dispatch.action_epoch else {
        return Err(unavailable(
            surface,
            TaskCockpitUnavailableReason::WorkspaceAuthorityUnavailable,
        ));
    };
    let Some(runtime_generation) = dispatch.runtime_generation else {
        return Err(unavailable(
            surface,
            TaskCockpitUnavailableReason::WorkspaceAuthorityUnavailable,
        ));
    };
    let command_id = CommandId::from_bytes(*dispatch.request_id.as_bytes()).map_err(|_| {
        unavailable(
            surface,
            TaskCockpitUnavailableReason::WorkspaceAuthorityUnavailable,
        )
    })?;
    let service = WorkspaceService::from_durable_with_task_coordinator(
        task.project_id,
        task_id,
        projects,
        &task.workspace,
        coordinator.clone(),
    )
    .map_err(|error| {
        eprintln!(
            "devmanager-host: cockpit workspace service reconstruction failed for {surface:?}: {error:?}"
        );
        unavailable(
            surface,
            TaskCockpitUnavailableReason::WorkspaceAuthorityUnavailable,
        )
    })?;
    let authorization = service
        .authorize_current_with_generation(
            &task.workspace,
            task_id,
            dispatch.client_id,
            dispatch.connection_id,
            dispatch.request_id,
            command_id,
            action_epoch,
            runtime_generation,
        )
        .map_err(|error| {
            eprintln!(
                "devmanager-host: cockpit workspace authorization rejected for {surface:?}: {error:?}"
            );
            denied(surface, TaskCockpitDeniedReason::StaleFence)
        })?;
    Ok((
        service,
        authorization,
        command_id,
        action_epoch,
        runtime_generation,
    ))
}

fn remint_and_check_authorization(
    dispatch: &TaskCockpitDispatch<'_>,
    task_id: TaskId,
    task: &crate::domain::task::TaskFacts,
    surface: TaskCockpitSurface,
) -> Result<(), QueryOutcome> {
    let (_, authorization, command_id, action_epoch, runtime_generation) =
        live_mutation_authority(dispatch, task_id, task, surface)?;
    if authorization
        .validated_binding(
            task_id,
            task.project_id,
            dispatch.client_id,
            dispatch.connection_id,
            dispatch.request_id,
            command_id,
            &task.workspace,
            action_epoch,
            runtime_generation,
        )
        .is_none()
    {
        return Err(denied(surface, TaskCockpitDeniedReason::StaleFence));
    }
    Ok(())
}

fn revalidate_issued_fence(
    dispatch: &TaskCockpitDispatch<'_>,
    task_id: TaskId,
    task: &crate::domain::task::TaskFacts,
    authorization: &WorkspaceAuthorization,
    lease: &WorkspaceResourceLease,
    command_id: CommandId,
    action_epoch: u64,
    runtime_generation: u64,
    resource: WorkspaceResource,
    surface: TaskCockpitSurface,
) -> Result<(), QueryOutcome> {
    let context = WorkspaceActionContext::new(
        task_id,
        task.project_id,
        dispatch.client_id,
        dispatch.connection_id,
        dispatch.request_id,
        command_id,
        task.workspace.clone(),
        action_epoch,
        runtime_generation,
    );
    revalidate_cockpit_workspace_action(authorization, lease, &context, resource).map_err(|error| {
        match error {
            crate::workspace::worktree::WorktreeError::StaleAuthority => {
                denied(surface, TaskCockpitDeniedReason::StaleFence)
            }
            _ => unavailable(
                surface,
                TaskCockpitUnavailableReason::WorkspaceAuthorityUnavailable,
            ),
        }
    })
}

fn serve_ssh_action(
    dispatch: &TaskCockpitDispatch<'_>,
    task_id: TaskId,
    snapshot: &crate::domain::snapshot::TaskSnapshot,
    endpoint_id: &str,
) -> QueryOutcome {
    let task = &snapshot.task;
    let Some(endpoints) = dispatch.ssh_endpoints else {
        return unavailable(
            TaskCockpitSurface::Ssh,
            TaskCockpitUnavailableReason::SshOperationUnsupported,
        );
    };
    match accept_exact_endpoint(endpoints, endpoint_id) {
        Ok(endpoint) if endpoint.archived => {
            return unavailable(
                TaskCockpitSurface::Ssh,
                TaskCockpitUnavailableReason::SshOperationUnsupported,
            );
        }
        Ok(_) => {}
        Err(SshEndpointDenial::ForeignInput) | Err(SshEndpointDenial::UnknownEndpoint) => {
            return denied(
                TaskCockpitSurface::Ssh,
                TaskCockpitDeniedReason::Unauthorized,
            );
        }
    }
    if dispatch.envelope_task_id != Some(task_id) {
        return denied(
            TaskCockpitSurface::Ssh,
            TaskCockpitDeniedReason::Unauthorized,
        );
    }
    if let Err(outcome) =
        remint_and_check_authorization(dispatch, task_id, task, TaskCockpitSurface::Ssh)
    {
        return outcome;
    }
    let Some(runtime) = dispatch.ssh_runtime else {
        return unavailable(
            TaskCockpitSurface::Ssh,
            TaskCockpitUnavailableReason::SshTaskSupervisorAdapterMissing,
        );
    };
    if task.lifecycle != crate::domain::task::TaskLifecycle::Open {
        return unavailable(
            TaskCockpitSurface::Ssh,
            TaskCockpitUnavailableReason::SshOperationUnsupported,
        );
    }
    let Some(agent_session_id) = snapshot.primary_agent_id else {
        return unavailable(
            TaskCockpitSurface::Ssh,
            TaskCockpitUnavailableReason::SshOperationUnsupported,
        );
    };
    let Some(agent) = snapshot.agents.get(&agent_session_id) else {
        return unavailable(
            TaskCockpitSurface::Ssh,
            TaskCockpitUnavailableReason::SshOperationUnsupported,
        );
    };
    if agent.task_id != task_id
        || agent.lifecycle != crate::domain::agent::AgentSessionLifecycle::Open
        || agent.runtime_generation == 0
    {
        return unavailable(
            TaskCockpitSurface::Ssh,
            TaskCockpitUnavailableReason::SshOperationUnsupported,
        );
    }
    let Some(resource) = snapshot.resources.values().find(|resource| {
        resource.task_id == Some(task_id)
            && resource.owner_kind == crate::domain::resource::OwnerKind::Task
            && resource.resource_kind == crate::domain::resource::ResourceKind::Terminal
            && resource.lifecycle == crate::domain::resource::ResourceLifecycle::Active
            && resource.runtime_generation > 0
            && resource.runtime_generation == agent.runtime_generation
    }) else {
        return unavailable(
            TaskCockpitSurface::Ssh,
            TaskCockpitUnavailableReason::SshOperationUnsupported,
        );
    };
    let Some(cwd) = dispatch
        .workspace_projects
        .and_then(|projects| projects.root_for(task.project_id))
        .map(|path| path.to_path_buf())
    else {
        return unavailable(
            TaskCockpitSurface::Ssh,
            TaskCockpitUnavailableReason::WorkspaceAuthorityUnavailable,
        );
    };
    let Some(action_epoch) = dispatch
        .action_epoch
        .filter(|epoch| *epoch > 0)
        .or_else(|| (task.action_epoch > 0).then_some(task.action_epoch))
    else {
        return unavailable(
            TaskCockpitSurface::Ssh,
            TaskCockpitUnavailableReason::WorkspaceAuthorityUnavailable,
        );
    };
    match runtime.connect_endpoint(
        endpoint_id,
        SshTaskIdentity {
            task_id,
            agent_session_id,
            resource_id: resource.id,
            runtime_generation: resource.runtime_generation,
            action_epoch,
            cwd,
        },
    ) {
        Ok(runtime_snapshot) => QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Ssh(
            TaskSshProjection {
                task_id,
                endpoints: endpoints.to_vec(),
                runtime: Some(to_ssh_runtime_projection(runtime_snapshot)),
            },
        ))),
        Err(SshRuntimeError::UnknownEndpoint | SshRuntimeError::ArchivedEndpoint) => denied(
            TaskCockpitSurface::Ssh,
            TaskCockpitDeniedReason::Unauthorized,
        ),
        Err(_) => unavailable(
            TaskCockpitSurface::Ssh,
            TaskCockpitUnavailableReason::SshOperationUnsupported,
        ),
    }
}

fn to_ssh_runtime_projection(snapshot: SshRuntimeSnapshot) -> TaskSshRuntimeProjection {
    TaskSshRuntimeProjection {
        task_id: snapshot.task_id,
        agent_session_id: snapshot.agent_session_id,
        resource_id: snapshot.resource_id,
        runtime_generation: snapshot.runtime_generation,
        action_epoch: snapshot.action_epoch,
        endpoint_id: snapshot.endpoint_id,
        lifecycle: match snapshot.lifecycle {
            SshLifecycle::Starting => TaskSshLifecycle::Starting,
            SshLifecycle::Running => TaskSshLifecycle::Running,
            SshLifecycle::Stopping => TaskSshLifecycle::Stopping,
            SshLifecycle::Stopped => TaskSshLifecycle::Stopped,
            SshLifecycle::Failed => TaskSshLifecycle::Failed,
        },
        error: snapshot.error.map(|error| match error {
            SshRuntimeError::CredentialUnavailable => TaskSshRuntimeError::CredentialUnavailable,
            SshRuntimeError::HostKeyPrompt => TaskSshRuntimeError::HostKeyPrompt,
            SshRuntimeError::StaleFence => TaskSshRuntimeError::StaleFence,
            SshRuntimeError::Teardown => TaskSshRuntimeError::Teardown,
            SshRuntimeError::Launch
            | SshRuntimeError::InvalidAdmission
            | SshRuntimeError::UnknownEndpoint
            | SshRuntimeError::ArchivedEndpoint
            | SshRuntimeError::AlreadyRunning
            | SshRuntimeError::NotRunning => TaskSshRuntimeError::Launch,
        }),
    }
}

fn map_file_error(error: FileServiceError) -> QueryOutcome {
    match error {
        FileServiceError::InvalidPath { .. }
        | FileServiceError::ReparseRejected { .. }
        | FileServiceError::HardLinkRejected { .. } => denied(
            TaskCockpitSurface::Files,
            TaskCockpitDeniedReason::PathTraversal,
        ),
        FileServiceError::SecretLikePath => denied(
            TaskCockpitSurface::Files,
            TaskCockpitDeniedReason::CapabilityDenied,
        ),
        FileServiceError::OutsideWorkspace { .. } => denied(
            TaskCockpitSurface::Files,
            TaskCockpitDeniedReason::OutsideWorkspace,
        ),
        FileServiceError::Conflict { .. } | FileServiceError::ChangedDuringRead { .. } => denied(
            TaskCockpitSurface::Files,
            TaskCockpitDeniedReason::RevisionConflict,
        ),
        FileServiceError::AuthorityUnavailable | FileServiceError::RootUnavailable => unavailable(
            TaskCockpitSurface::Files,
            TaskCockpitUnavailableReason::FileAuthorityNotIssued,
        ),
        _ => unavailable(
            TaskCockpitSurface::Files,
            TaskCockpitUnavailableReason::FileAuthorityNotIssued,
        ),
    }
}

fn denied(surface: TaskCockpitSurface, reason: TaskCockpitDeniedReason) -> QueryOutcome {
    QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Denied {
        surface,
        reason,
    }))
}

fn unavailable(surface: TaskCockpitSurface, reason: TaskCockpitUnavailableReason) -> QueryOutcome {
    QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Unavailable {
        surface,
        reason,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::command::{Command, CommandEnvelope, CreateTaskRequestIntent};
    use crate::domain::task::{
        ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity,
    };
    use crate::domain::{CommandId, EnvironmentId, ProjectId};
    use crate::protocol::Capability;
    use crate::workspace::{WorkspaceRequest, WorkspaceResourceCoordinator};
    use std::fs;

    #[test]
    fn provider_lifecycle_projects_canonical_plan_status_and_sequence_identity() {
        use crate::remote::presentation::{
            SemanticEvent, SemanticEventKind, SemanticSource, StableSessionKey,
        };

        let event = SemanticEvent {
            stable_session_key: StableSessionKey::from_tab("task"),
            sequence: 9,
            replaces_sequence: Some(4),
            occurred_at_epoch_ms: 1,
            source: SemanticSource::Claude,
            kind: SemanticEventKind::Status {
                state: "taskCompleted".to_string(),
                detail: Some("Run verification".to_string()),
            },
        };

        assert_eq!(semantic_payload_kind(&event.kind), "plan_step");
        assert!(matches!(
            semantic_payload(&event),
            crate::domain::SemanticJournalPayload::PlanStep { step_id, title, status }
                if step_id == "task:4" && title == "Run verification" && status == "completed"
        ));
    }

    #[test]
    fn claude_task_lifecycle_reaches_conversation_as_one_completed_plan_step() {
        use crate::ai::claude_hooks::{ClaudeReducer, ClaudeReducerLimits};
        use crate::remote::presentation::{SemanticJournalStore, StableSessionKey};
        use std::sync::Mutex;

        let (_repository, bus, client_id, task_id, _roots) = create_bound_task();
        let key = StableSessionKey::from_tab(task_id.to_string());
        let mut reducer = ClaudeReducer::new(key, ClaudeReducerLimits::default());
        let mut store = SemanticJournalStore::default();
        for body in [
            br#"{"hook_event_name":"TaskCreated","task_id":"task-7","task_subject":"Verify UX"}"#
                .as_slice(),
            br#"{"hook_event_name":"TaskCompleted","task_id":"task-7","task_subject":"Verify UX"}"#
                .as_slice(),
        ] {
            for draft in reducer.apply_json(body, 10).drafts {
                store.record(draft);
            }
        }
        let journal = Mutex::new(store);
        let query = TaskCockpitQuery::Conversation { after_sequence: 0 };
        let outcome = serve_task_cockpit(TaskCockpitDispatch {
            capabilities: CapabilitySet::from_capabilities([
                Capability::TaskCockpit,
                Capability::SemanticConversation,
            ]),
            envelope_task_id: Some(task_id),
            client_id,
            connection_id: Uuid::now_v7(),
            request_id: RequestId::new(),
            query: &query,
            bus: &bus,
            service_runtime: None,
            semantic_journal: Some(&journal),
            terminal_service: None,
            ssh_endpoints: None,
            ssh_runtime: None,
            workspace_projects: None,
            coordinator: None,
            action_epoch: None,
            runtime_generation: None,
            config: None,
        });
        let QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Conversation(page))) =
            outcome
        else {
            panic!("expected conversation page, got {outcome:?}");
        };

        assert_eq!(page.facts.len(), 1);
        assert!(matches!(
            &page.facts[0].payload,
            crate::domain::SemanticJournalPayload::PlanStep { step_id, title, status }
                if step_id == "task:1" && title == "Verify UX" && status == "completed"
        ));
    }

    fn create_bound_task() -> (
        tempfile::TempDir,
        CommandBus,
        ClientId,
        TaskId,
        WorkspaceProjectRoots,
    ) {
        let repository = tempfile::tempdir().expect("repo");
        let output = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(repository.path())
            .output()
            .expect("git init");
        assert!(output.status.success());
        for (key, value) in [
            ("user.name", "Cockpit Test"),
            ("user.email", "cockpit@example.invalid"),
        ] {
            let configured = std::process::Command::new("git")
                .args(["config", key, value])
                .current_dir(repository.path())
                .output()
                .expect("git config");
            assert!(configured.status.success());
        }
        fs::write(repository.path().join("README.md"), "hello-aé\n").expect("readme");
        fs::write(repository.path().join("blob.bin"), [0u8, 1, 2, 255]).expect("binary");
        fs::write(repository.path().join(".env"), "SECRET=1\n").expect("env");
        let database = repository.path().join("cockpit.sqlite");
        let mut bus = CommandBus::open(&database).expect("bus");
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
            issued_at_ms: 1,
            expected_task_revision: None,
            command: Command::CreateTaskV2(CreateTaskRequestIntent {
                id: task_id,
                environment_id: EnvironmentId::new(),
                title: "Cockpit".into(),
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
        crate::host::connection::dispatch_authenticated_request_with_workspace_projects(
            client_id,
            CapabilitySet::empty(),
            &mut bus,
            &roots,
            crate::protocol::ClientRequest::Command(create),
        )
        .expect("create");
        (repository, bus, client_id, task_id, roots)
    }

    #[test]
    fn ssh_action_requires_task_authority_before_missing_adapter() {
        let (_repository, bus, client_id, task_id, roots) = create_bound_task();
        let endpoints = [TaskSshEndpoint {
            id: "jump".into(),
            label: "Jump".into(),
            archived: false,
            has_credential: true,
        }];
        let granted = CapabilitySet::from_capabilities([Capability::TaskCockpit]);
        let accepted = serve_task_cockpit(TaskCockpitDispatch {
            capabilities: granted,
            envelope_task_id: Some(task_id),
            client_id,
            connection_id: Uuid::now_v7(),
            request_id: RequestId::new(),
            query: &TaskCockpitQuery::SshAction {
                endpoint_id: "jump".into(),
            },
            bus: &bus,
            service_runtime: None,
            semantic_journal: None,
            terminal_service: None,
            ssh_endpoints: Some(&endpoints),
            ssh_runtime: None,
            workspace_projects: Some(&roots),
            coordinator: None,
            action_epoch: None,
            runtime_generation: None,
            config: None,
        });
        assert!(matches!(
            accepted,
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Unavailable {
                surface: TaskCockpitSurface::Ssh,
                reason: TaskCockpitUnavailableReason::WorkspaceAuthorityUnavailable,
            }))
        ));
        let coordinator = WorkspaceResourceCoordinator::new();
        let named = serve_task_cockpit(TaskCockpitDispatch {
            capabilities: granted,
            envelope_task_id: Some(task_id),
            client_id,
            connection_id: Uuid::now_v7(),
            request_id: RequestId::new(),
            query: &TaskCockpitQuery::SshAction {
                endpoint_id: "jump".into(),
            },
            bus: &bus,
            service_runtime: None,
            semantic_journal: None,
            terminal_service: None,
            ssh_endpoints: Some(&endpoints),
            ssh_runtime: None,
            workspace_projects: Some(&roots),
            coordinator: Some(&coordinator),
            action_epoch: Some(1),
            runtime_generation: Some(1),
            config: None,
        });
        assert!(matches!(
            named,
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Unavailable {
                surface: TaskCockpitSurface::Ssh,
                reason: TaskCockpitUnavailableReason::SshTaskSupervisorAdapterMissing,
            }))
        ));
        let foreign = serve_task_cockpit(TaskCockpitDispatch {
            capabilities: granted,
            envelope_task_id: Some(task_id),
            client_id,
            connection_id: Uuid::now_v7(),
            request_id: RequestId::new(),
            query: &TaskCockpitQuery::SshAction {
                endpoint_id: "deploy@secret.example".into(),
            },
            bus: &bus,
            service_runtime: None,
            semantic_journal: None,
            terminal_service: None,
            ssh_endpoints: Some(&endpoints),
            ssh_runtime: None,
            workspace_projects: Some(&roots),
            coordinator: None,
            action_epoch: None,
            runtime_generation: None,
            config: None,
        });
        assert!(matches!(
            foreign,
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Denied {
                surface: TaskCockpitSurface::Ssh,
                reason: TaskCockpitDeniedReason::Unauthorized,
            }))
        ));
    }

    #[test]
    fn files_read_returns_text_omits_binary_and_denies_secret_paths() {
        let (_repository, bus, client_id, task_id, roots) = create_bound_task();
        let granted = CapabilitySet::from_capabilities([Capability::TaskCockpit]);
        let coordinator = WorkspaceResourceCoordinator::new();
        let text = serve_task_cockpit(TaskCockpitDispatch {
            capabilities: granted,
            envelope_task_id: Some(task_id),
            client_id,
            connection_id: Uuid::now_v7(),
            request_id: RequestId::new(),
            query: &TaskCockpitQuery::FilesRead {
                relative_path: "README.md".into(),
                max_bytes: 32,
            },
            bus: &bus,
            service_runtime: None,
            semantic_journal: None,
            terminal_service: None,
            ssh_endpoints: None,
            ssh_runtime: None,
            workspace_projects: Some(&roots),
            coordinator: Some(&coordinator),
            action_epoch: Some(1),
            runtime_generation: Some(1),
            config: None,
        });
        let QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::FilesRead(text))) = text
        else {
            panic!("expected text read, got {text:?}");
        };
        assert_eq!(text.secret, false);
        assert!(text.utf8_prefix.as_deref().unwrap_or("").contains("hello"));
        let binary = serve_task_cockpit(TaskCockpitDispatch {
            capabilities: granted,
            envelope_task_id: Some(task_id),
            client_id,
            connection_id: Uuid::now_v7(),
            request_id: RequestId::new(),
            query: &TaskCockpitQuery::FilesRead {
                relative_path: "blob.bin".into(),
                max_bytes: 32,
            },
            bus: &bus,
            service_runtime: None,
            semantic_journal: None,
            terminal_service: None,
            ssh_endpoints: None,
            ssh_runtime: None,
            workspace_projects: Some(&roots),
            coordinator: Some(&coordinator),
            action_epoch: Some(1),
            runtime_generation: Some(1),
            config: None,
        });
        let QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::FilesRead(binary))) =
            binary
        else {
            panic!("expected binary read, got {binary:?}");
        };
        assert!(binary.utf8_prefix.is_none());
        assert_eq!(binary.secret, false);
        let secret = serve_task_cockpit(TaskCockpitDispatch {
            capabilities: granted,
            envelope_task_id: Some(task_id),
            client_id,
            connection_id: Uuid::now_v7(),
            request_id: RequestId::new(),
            query: &TaskCockpitQuery::FilesRead {
                relative_path: ".env".into(),
                max_bytes: 32,
            },
            bus: &bus,
            service_runtime: None,
            semantic_journal: None,
            terminal_service: None,
            ssh_endpoints: None,
            ssh_runtime: None,
            workspace_projects: Some(&roots),
            coordinator: Some(&coordinator),
            action_epoch: Some(1),
            runtime_generation: Some(1),
            config: None,
        });
        assert!(matches!(
            secret,
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Denied {
                surface: TaskCockpitSurface::Files,
                reason: TaskCockpitDeniedReason::CapabilityDenied,
            }))
        ));
    }

    #[test]
    fn files_list_returns_a_truncated_first_page_for_large_directories() {
        let (repository, bus, client_id, task_id, roots) = create_bound_task();
        for index in 0..70 {
            fs::write(
                repository.path().join(format!("visible-{index:02}.txt")),
                "visible\n",
            )
            .expect("large directory entry");
        }
        let granted = CapabilitySet::from_capabilities([Capability::TaskCockpit]);
        let coordinator = WorkspaceResourceCoordinator::new();
        let listed = serve_task_cockpit(TaskCockpitDispatch {
            capabilities: granted,
            envelope_task_id: Some(task_id),
            client_id,
            connection_id: Uuid::now_v7(),
            request_id: RequestId::new(),
            query: &TaskCockpitQuery::FilesList {
                relative_directory: None,
                limit: MAX_COCKPIT_FILE_LIST,
            },
            bus: &bus,
            service_runtime: None,
            semantic_journal: None,
            terminal_service: None,
            ssh_endpoints: None,
            ssh_runtime: None,
            workspace_projects: Some(&roots),
            coordinator: Some(&coordinator),
            action_epoch: Some(1),
            runtime_generation: Some(1),
            config: None,
        });
        let QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::FilesList(listed))) =
            listed
        else {
            panic!("expected a truncated files page, got {listed:?}");
        };
        assert_eq!(listed.entries.len(), usize::from(MAX_COCKPIT_FILE_LIST));
        assert!(listed.truncated);
    }

    fn granted() -> CapabilitySet {
        CapabilitySet::from_capabilities([Capability::TaskCockpit])
    }

    #[test]
    fn conversation_query_projects_the_task_semantic_journal() {
        use crate::remote::presentation::{
            SemanticEventDraft, SemanticEventKind, SemanticRetention, SemanticSource,
            StableSessionKey,
        };
        use std::sync::Mutex;

        let (_repository, bus, client_id, task_id, _roots) = create_bound_task();
        let journal = Mutex::new(crate::remote::presentation::SemanticJournalStore::default());
        journal.lock().expect("journal").record(SemanticEventDraft {
            stable_session_key: StableSessionKey::from_tab(&task_id.to_string()),
            occurred_at_epoch_ms: 10,
            source: SemanticSource::Claude,
            kind: SemanticEventKind::UserMessage {
                text: "Fix the failing test".to_string(),
            },
            retention: SemanticRetention::Canonical,
            deduplication_key: None,
        });
        let query = TaskCockpitQuery::Conversation { after_sequence: 0 };
        let outcome = serve_task_cockpit(TaskCockpitDispatch {
            capabilities: CapabilitySet::from_capabilities([
                Capability::TaskCockpit,
                Capability::SemanticConversation,
            ]),
            envelope_task_id: Some(task_id),
            client_id,
            connection_id: Uuid::now_v7(),
            request_id: RequestId::new(),
            query: &query,
            bus: &bus,
            service_runtime: None,
            semantic_journal: Some(&journal),
            terminal_service: None,
            ssh_endpoints: None,
            ssh_runtime: None,
            workspace_projects: None,
            coordinator: None,
            action_epoch: None,
            runtime_generation: None,
            config: None,
        });
        let QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Conversation(page))) =
            outcome
        else {
            panic!("expected conversation page, got {outcome:?}");
        };
        assert_eq!(page.facts.len(), 1);
        assert!(matches!(
            &page.facts[0].payload,
            crate::domain::SemanticJournalPayload::UserMessage { text }
                if text == "Fix the failing test"
        ));
        assert!(page.encoded_bytes > 0);
    }

    #[test]
    fn conversation_query_pages_the_complete_local_history_in_sequence_order() {
        use crate::remote::presentation::{
            SemanticEventDraft, SemanticEventKind, SemanticRetention, SemanticSource,
            StableSessionKey,
        };
        use std::sync::Mutex;

        let (_repository, bus, client_id, task_id, _roots) = create_bound_task();
        let journal = Mutex::new(crate::remote::presentation::SemanticJournalStore::default());
        let key = StableSessionKey::from_tab(task_id.to_string());
        for index in 1..=130 {
            journal.lock().expect("journal").record(SemanticEventDraft {
                stable_session_key: key.clone(),
                occurred_at_epoch_ms: index,
                source: SemanticSource::Claude,
                kind: SemanticEventKind::UserMessage {
                    text: format!("message {index}"),
                },
                retention: SemanticRetention::Canonical,
                deduplication_key: None,
            });
        }

        let query_page = |after_sequence| {
            let query = TaskCockpitQuery::Conversation { after_sequence };
            let outcome = serve_task_cockpit(TaskCockpitDispatch {
                capabilities: CapabilitySet::from_capabilities([
                    Capability::TaskCockpit,
                    Capability::SemanticConversation,
                ]),
                envelope_task_id: Some(task_id),
                client_id,
                connection_id: Uuid::now_v7(),
                request_id: RequestId::new(),
                query: &query,
                bus: &bus,
                service_runtime: None,
                semantic_journal: Some(&journal),
                terminal_service: None,
                ssh_endpoints: None,
                ssh_runtime: None,
                workspace_projects: None,
                coordinator: None,
                action_epoch: None,
                runtime_generation: None,
                config: None,
            });
            let QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Conversation(page))) =
                outcome
            else {
                panic!("expected conversation page, got {outcome:?}");
            };
            page
        };

        let first = query_page(0);
        assert_eq!(first.facts.first().map(|fact| fact.sequence), Some(1));
        assert_eq!(first.facts.last().map(|fact| fact.sequence), Some(128));
        assert_eq!(first.next_sequence, Some(128));
        assert_eq!(first.high_water, 130);

        let second = query_page(first.next_sequence.unwrap());
        assert_eq!(
            second
                .facts
                .iter()
                .map(|fact| fact.sequence)
                .collect::<Vec<_>>(),
            vec![129, 130]
        );
        assert_eq!(second.next_sequence, None);
    }

    #[test]
    fn conversation_filters_historical_ai_terminal_output() {
        use crate::remote::presentation::{
            SemanticEventDraft, SemanticEventKind, SemanticRetention, SemanticSource,
            SemanticStream, StableSessionKey,
        };
        use std::sync::Mutex;

        let (_repository, bus, client_id, task_id, _roots) = create_bound_task();
        let journal = Mutex::new(crate::remote::presentation::SemanticJournalStore::default());
        {
            let mut store = journal.lock().expect("journal");
            let key = StableSessionKey::from_tab(&task_id.to_string());
            store.record(SemanticEventDraft {
                stable_session_key: key.clone(),
                occurred_at_epoch_ms: 10,
                source: SemanticSource::Claude,
                kind: SemanticEventKind::AssistantMessage {
                    message_id: "msg-1".to_string(),
                    text: "semantic assistant".to_string(),
                    streaming: false,
                },
                retention: SemanticRetention::Canonical,
                deduplication_key: None,
            });
            store.record(SemanticEventDraft {
                stable_session_key: key,
                occurred_at_epoch_ms: 11,
                source: SemanticSource::Claude,
                kind: SemanticEventKind::Output {
                    stream: SemanticStream::Stdout,
                    text: "raw Claude PTY wall".to_string(),
                },
                retention: SemanticRetention::Verbose,
                deduplication_key: None,
            });
        }
        let query = TaskCockpitQuery::Conversation { after_sequence: 0 };
        let outcome = serve_task_cockpit(TaskCockpitDispatch {
            capabilities: CapabilitySet::from_capabilities([
                Capability::TaskCockpit,
                Capability::SemanticConversation,
            ]),
            envelope_task_id: Some(task_id),
            client_id,
            connection_id: Uuid::now_v7(),
            request_id: RequestId::new(),
            query: &query,
            bus: &bus,
            service_runtime: None,
            semantic_journal: Some(&journal),
            terminal_service: None,
            ssh_endpoints: None,
            ssh_runtime: None,
            workspace_projects: None,
            coordinator: None,
            action_epoch: None,
            runtime_generation: None,
            config: None,
        });
        let QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Conversation(page))) =
            outcome
        else {
            panic!("expected conversation page, got {outcome:?}");
        };
        assert_eq!(
            page.facts.len(),
            1,
            "historical AI Output must not become conversation AssistantText: {page:?}"
        );
        assert!(matches!(
            &page.facts[0].payload,
            crate::domain::SemanticJournalPayload::AssistantText { text }
                if text == "semantic assistant"
        ));
    }

    fn dispatch<'a>(
        bus: &'a CommandBus,
        client_id: ClientId,
        task_id: TaskId,
        query: &'a TaskCockpitQuery,
        roots: Option<&'a WorkspaceProjectRoots>,
        coordinator: Option<&'a WorkspaceResourceCoordinator>,
        action_epoch: Option<u64>,
        runtime_generation: Option<u64>,
    ) -> TaskCockpitDispatch<'a> {
        TaskCockpitDispatch {
            capabilities: granted(),
            envelope_task_id: Some(task_id),
            client_id,
            connection_id: Uuid::now_v7(),
            request_id: RequestId::new(),
            query,
            bus,
            service_runtime: None,
            semantic_journal: None,
            terminal_service: None,
            ssh_endpoints: None,
            ssh_runtime: None,
            workspace_projects: roots,
            coordinator,
            action_epoch,
            runtime_generation,
            config: None,
        }
    }

    #[test]
    fn files_write_denies_traversal_secret_and_stale_expected_revision() {
        let (_repository, bus, client_id, task_id, roots) = create_bound_task();
        let coordinator = WorkspaceResourceCoordinator::new();
        let traversal = serve_task_cockpit(dispatch(
            &bus,
            client_id,
            task_id,
            &TaskCockpitQuery::FilesWrite {
                relative_path: "../secret.env".into(),
                utf8_contents: "nope".into(),
                expected_sha256_hex: None,
                confirm: true,
            },
            Some(&roots),
            Some(&coordinator),
            Some(1),
            Some(1),
        ));
        assert!(matches!(
            traversal,
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Denied {
                surface: TaskCockpitSurface::Files,
                reason: TaskCockpitDeniedReason::PathTraversal,
            }))
        ));
        let secret = serve_task_cockpit(dispatch(
            &bus,
            client_id,
            task_id,
            &TaskCockpitQuery::FilesWrite {
                relative_path: ".env".into(),
                utf8_contents: "SECRET=2\n".into(),
                expected_sha256_hex: None,
                confirm: true,
            },
            Some(&roots),
            Some(&coordinator),
            Some(1),
            Some(1),
        ));
        assert!(matches!(
            secret,
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Denied {
                surface: TaskCockpitSurface::Files,
                reason: TaskCockpitDeniedReason::CapabilityDenied,
            }))
        ));
        let missing = serve_task_cockpit(dispatch(
            &bus,
            client_id,
            task_id,
            &TaskCockpitQuery::FilesWrite {
                relative_path: "notes.txt".into(),
                utf8_contents: "phase6\n".into(),
                expected_sha256_hex: None,
                confirm: true,
            },
            Some(&roots),
            None,
            None,
            None,
        ));
        assert!(matches!(
            missing,
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Unavailable {
                surface: TaskCockpitSurface::Files,
                reason: TaskCockpitUnavailableReason::WorkspaceAuthorityUnavailable,
            }))
        ));
        let unconfirmed = serve_task_cockpit(dispatch(
            &bus,
            client_id,
            task_id,
            &TaskCockpitQuery::FilesWrite {
                relative_path: "notes.txt".into(),
                utf8_contents: "phase6\n".into(),
                expected_sha256_hex: None,
                confirm: false,
            },
            Some(&roots),
            Some(&coordinator),
            Some(1),
            Some(1),
        ));
        assert!(matches!(
            unconfirmed,
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Unavailable {
                surface: TaskCockpitSurface::Files,
                reason: TaskCockpitUnavailableReason::WriteUnsupported,
            }))
        ));
        let created = serve_task_cockpit(dispatch(
            &bus,
            client_id,
            task_id,
            &TaskCockpitQuery::FilesWrite {
                relative_path: "notes.txt".into(),
                utf8_contents: "phase6\n".into(),
                expected_sha256_hex: None,
                confirm: true,
            },
            Some(&roots),
            Some(&coordinator),
            Some(1),
            Some(1),
        ));
        let QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::FilesRead(created))) =
            created
        else {
            panic!("expected create write, got {created:?}");
        };
        assert_eq!(created.relative_path, "notes.txt");
        assert_eq!(created.utf8_prefix.as_deref(), Some("phase6\n"));
        let conflict = serve_task_cockpit(dispatch(
            &bus,
            client_id,
            task_id,
            &TaskCockpitQuery::FilesWrite {
                relative_path: "notes.txt".into(),
                utf8_contents: "changed\n".into(),
                expected_sha256_hex: Some(
                    "0000000000000000000000000000000000000000000000000000000000000000".into(),
                ),
                confirm: true,
            },
            Some(&roots),
            Some(&coordinator),
            Some(1),
            Some(1),
        ));
        assert!(matches!(
            conflict,
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Denied {
                surface: TaskCockpitSurface::Files,
                reason: TaskCockpitDeniedReason::RevisionConflict,
            }))
        ));
    }

    #[test]
    fn git_mutate_denies_traversal_and_stages_when_test_confirm_is_available() {
        let (_repository, bus, client_id, task_id, roots) = create_bound_task();
        let coordinator = WorkspaceResourceCoordinator::new();
        let traversal = serve_task_cockpit(dispatch(
            &bus,
            client_id,
            task_id,
            &TaskCockpitQuery::GitMutate {
                intent: TaskGitMutateIntent::Stage {
                    relative_paths: vec!["../secret".into()],
                },
                confirm: true,
            },
            Some(&roots),
            Some(&coordinator),
            Some(1),
            Some(1),
        ));
        assert!(matches!(
            traversal,
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Denied {
                surface: TaskCockpitSurface::Git,
                reason: TaskCockpitDeniedReason::PathTraversal,
            }))
        ));
        let planned = serve_task_cockpit(dispatch(
            &bus,
            client_id,
            task_id,
            &TaskCockpitQuery::GitMutate {
                intent: TaskGitMutateIntent::Stage {
                    relative_paths: vec!["README.md".into()],
                },
                confirm: false,
            },
            Some(&roots),
            Some(&coordinator),
            Some(1),
            Some(1),
        ));
        let QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Git(_))) = planned else {
            panic!("expected plan-only git projection, got {planned:?}");
        };
        let staged = serve_task_cockpit(dispatch(
            &bus,
            client_id,
            task_id,
            &TaskCockpitQuery::GitMutate {
                intent: TaskGitMutateIntent::Stage {
                    relative_paths: vec!["README.md".into()],
                },
                confirm: true,
            },
            Some(&roots),
            Some(&coordinator),
            Some(1),
            Some(1),
        ));
        let QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Git(staged))) = staged
        else {
            panic!("expected confirmed stage, got {staged:?}");
        };
        assert_eq!(staged.task_id, task_id);
    }

    #[test]
    fn git_mutate_host_issuer_unstages_and_commits_only_through_confirmed_plans() {
        let (repository, bus, client_id, task_id, roots) = create_bound_task();
        let coordinator = WorkspaceResourceCoordinator::new();
        let staged = serve_task_cockpit(dispatch(
            &bus,
            client_id,
            task_id,
            &TaskCockpitQuery::GitMutate {
                intent: TaskGitMutateIntent::Stage {
                    relative_paths: vec!["README.md".into()],
                },
                confirm: true,
            },
            Some(&roots),
            Some(&coordinator),
            Some(1),
            Some(1),
        ));
        let QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Git(_))) = staged else {
            panic!("expected host-confirmed stage, got {staged:?}");
        };
        let unstaged = serve_task_cockpit(dispatch(
            &bus,
            client_id,
            task_id,
            &TaskCockpitQuery::GitMutate {
                intent: TaskGitMutateIntent::Unstage {
                    relative_paths: vec!["README.md".into()],
                },
                confirm: true,
            },
            Some(&roots),
            Some(&coordinator),
            Some(1),
            Some(1),
        ));
        let QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Git(_))) = unstaged else {
            panic!("expected host-confirmed unstage, got {unstaged:?}");
        };
        let restaged = serve_task_cockpit(dispatch(
            &bus,
            client_id,
            task_id,
            &TaskCockpitQuery::GitMutate {
                intent: TaskGitMutateIntent::Stage {
                    relative_paths: vec!["README.md".into()],
                },
                confirm: true,
            },
            Some(&roots),
            Some(&coordinator),
            Some(1),
            Some(1),
        ));
        let QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Git(_))) = restaged else {
            panic!("expected host-confirmed restage, got {restaged:?}");
        };
        let committed = serve_task_cockpit(dispatch(
            &bus,
            client_id,
            task_id,
            &TaskCockpitQuery::GitMutate {
                intent: TaskGitMutateIntent::Commit {
                    message: "cockpit host issuer".into(),
                },
                confirm: true,
            },
            Some(&roots),
            Some(&coordinator),
            Some(1),
            Some(1),
        ));
        let QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Git(committed))) =
            committed
        else {
            panic!("expected host-confirmed commit, got {committed:?}");
        };
        assert_eq!(committed.task_id, task_id);
        let readme_status = std::process::Command::new("git")
            .args(["status", "--porcelain", "--", "README.md"])
            .current_dir(repository.path())
            .output()
            .expect("inspect committed README");
        assert!(readme_status.status.success());
        assert!(readme_status.stdout.is_empty());
    }

    #[test]
    fn mutation_revalidation_rejects_stale_action_epoch() {
        let (_repository, bus, client_id, task_id, roots) = create_bound_task();
        let coordinator = WorkspaceResourceCoordinator::new();
        let snapshot = bus.task_snapshot(task_id).expect("lookup").expect("task");
        let query = TaskCockpitQuery::WorkspaceStatus;
        let prepared = dispatch(
            &bus,
            client_id,
            task_id,
            &query,
            Some(&roots),
            Some(&coordinator),
            Some(1),
            Some(1),
        );
        let (service, authorization, command_id, action_epoch, runtime_generation) =
            live_mutation_authority(
                &prepared,
                task_id,
                &snapshot.task,
                TaskCockpitSurface::Files,
            )
            .expect("authority");
        let lease = service
            .acquire_task_resource(
                task_id,
                WorkspaceResource::File,
                prepared.client_id,
                prepared.connection_id,
                prepared.request_id,
                command_id,
                action_epoch,
                runtime_generation,
            )
            .expect("file lease");
        let current = WorkspaceActionContext::new(
            task_id,
            snapshot.task.project_id,
            prepared.client_id,
            prepared.connection_id,
            prepared.request_id,
            command_id,
            snapshot.task.workspace.clone(),
            action_epoch,
            runtime_generation,
        );
        revalidate_cockpit_workspace_action(
            &authorization,
            &lease,
            &current,
            WorkspaceResource::File,
        )
        .expect("current fence");
        let stale = WorkspaceActionContext::new(
            task_id,
            snapshot.task.project_id,
            prepared.client_id,
            prepared.connection_id,
            prepared.request_id,
            command_id,
            snapshot.task.workspace.clone(),
            action_epoch.saturating_add(3),
            runtime_generation,
        );
        assert!(matches!(
            revalidate_cockpit_workspace_action(
                &authorization,
                &lease,
                &stale,
                WorkspaceResource::File,
            ),
            Err(crate::workspace::worktree::WorktreeError::StaleAuthority)
        ));
    }

    #[test]
    fn host_projection_preserves_the_exact_provider_resource_identity() {
        use crate::domain::agent::{AgentRole, AgentSessionLifecycle, ProviderSessionId};
        use crate::domain::resource::{
            OwnerKind, ResourceFacts, ResourceKind, ResourceLifecycle, ResourceRecipe,
        };
        use crate::domain::{AgentSessionId, ResourceId, TaskId};
        use crate::providers::ProviderKind;

        let task_id = TaskId::new();
        let agent_session_id = AgentSessionId::new();
        let resource_id = ResourceId::new();
        let agent = AgentSessionFacts {
            id: agent_session_id,
            task_id,
            role: AgentRole::Primary,
            provider_kind: ProviderKind::ClaudeCode,
            provider_session_id: Some(ProviderSessionId::new("hook-session").expect("session")),
            lifecycle: AgentSessionLifecycle::Open,
            runtime_generation: 4,
            revision: 0,
        };
        let resource = ResourceFacts {
            id: resource_id,
            task_id: Some(task_id),
            owner_kind: OwnerKind::Task,
            resource_kind: ResourceKind::Terminal,
            recipe: ResourceRecipe::Terminal {
                cols: 120,
                rows: 40,
            },
            lifecycle: ResourceLifecycle::Active,
            runtime_generation: 4,
            updated_at_ms: 1,
        };
        let projection = project_agent_resource(&agent, &resource).expect("projection");
        assert_eq!(projection.task_id, task_id);
        assert_eq!(projection.agent_session_id, agent_session_id);
        assert_eq!(projection.resource_id, resource_id);
        assert_eq!(projection.provider_kind, ProviderKind::ClaudeCode);
        assert_eq!(projection.runtime_generation, 4);
    }

    #[test]
    fn task_cockpit_client_deadline_outlasts_bounded_git_cleanup() {
        assert!(
            crate::host::task_cockpit_query_timeout() >= std::time::Duration::from_secs(15),
            "Task Cockpit must outlast Git's 10s operation bound plus cleanup and reply delivery"
        );
        let source = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/client/host_client.rs"
        ));
        let start = source
            .find("pub async fn query_task_cockpit(")
            .expect("Task Cockpit client query");
        let body = &source[start..];
        let end = body
            .find("pub async fn query_config_sidebar(")
            .expect("config query follows Task Cockpit query");
        let body = &body[..end];
        assert!(
            body.contains("query_with_timeout") && body.contains("task_cockpit_query_timeout"),
            "Task Cockpit must not use the generic 5s request deadline"
        );
    }

    #[test]
    fn legacy_git_status_shim_equals_workspace_targeted_status() {
        let (_repository, bus, client_id, task_id, roots) = create_bound_task();
        let coordinator = WorkspaceResourceCoordinator::new();
        let legacy = serve_task_cockpit(dispatch(
            &bus,
            client_id,
            task_id,
            &TaskCockpitQuery::GitStatus,
            Some(&roots),
            Some(&coordinator),
            Some(1),
            Some(1),
        ));
        let targeted = serve_task_cockpit(dispatch(
            &bus,
            client_id,
            task_id,
            &TaskCockpitQuery::GitStatusTargeted {
                selector: TaskRepositorySelector::Workspace,
            },
            Some(&roots),
            Some(&coordinator),
            Some(1),
            Some(1),
        ));
        let QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Git(legacy_git))) = legacy
        else {
            panic!("legacy git status failed: {legacy:?}");
        };
        let QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Git(targeted_git))) =
            targeted
        else {
            panic!("targeted git status failed: {targeted:?}");
        };
        assert_eq!(legacy_git.task_id, targeted_git.task_id);
        assert_eq!(legacy_git.branch, targeted_git.branch);
        assert_eq!(legacy_git.ahead, targeted_git.ahead);
        assert_eq!(legacy_git.behind, targeted_git.behind);
        assert_eq!(legacy_git.change_count, targeted_git.change_count);
        assert_eq!(legacy_git.detached, targeted_git.detached);
        assert_eq!(
            targeted_git.selector,
            Some(TaskRepositorySelector::Workspace)
        );
    }

    #[test]
    fn main_task_git_repositories_catalog_and_targeted_mutate_share_selector_fence() {
        let (_repository, bus, client_id, task_id, roots) = create_bound_task();
        let coordinator = WorkspaceResourceCoordinator::new();
        let catalog = serve_task_cockpit(dispatch(
            &bus,
            client_id,
            task_id,
            &TaskCockpitQuery::GitRepositories,
            Some(&roots),
            Some(&coordinator),
            Some(1),
            Some(1),
        ));
        let QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::GitRepositories(
            projection,
        ))) = catalog
        else {
            panic!("expected repository catalog, got {catalog:?}");
        };
        assert!(!projection.repositories.is_empty());
        assert!(projection.repositories.iter().any(|entry| {
            matches!(entry.selector, TaskRepositorySelector::Workspace) && entry.available
        }));
        let encoded = serde_json::to_string(&projection).expect("encode catalog");
        assert!(!encoded.contains("C:"));
        assert!(!encoded.contains("folder_path"));

        let planned = serve_task_cockpit(dispatch(
            &bus,
            client_id,
            task_id,
            &TaskCockpitQuery::GitMutateTargeted {
                selector: TaskRepositorySelector::Workspace,
                intent: TaskGitMutateIntent::Stage {
                    relative_paths: vec!["README.md".into()],
                },
                confirm: false,
            },
            Some(&roots),
            Some(&coordinator),
            Some(1),
            Some(1),
        ));
        let QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Git(status))) = planned
        else {
            panic!("expected targeted plan status, got {planned:?}");
        };
        assert_eq!(status.selector, Some(TaskRepositorySelector::Workspace));

        let path_like = serve_task_cockpit(dispatch(
            &bus,
            client_id,
            task_id,
            &TaskCockpitQuery::GitStatusTargeted {
                selector: TaskRepositorySelector::Folder {
                    folder_config_id: "C:/not-a-selector".into(),
                },
            },
            Some(&roots),
            Some(&coordinator),
            Some(1),
            Some(1),
        ));
        assert!(matches!(
            path_like,
            QueryOutcome::Err(QueryError::InvalidRequest)
        ));
    }
}
