//! Host dispatch for typed Task Cockpit queries.
//!
//! Workspace identity is resolved from the selected Task admission. Client
//! paths are never authoritative. Live Logs/Health use the ProcessManager
//! supervisor singleton. Git and files require a revalidated workspace
//! authorization plus an active resource lease.

use uuid::Uuid;

use crate::config::AppConfig;
use crate::domain::agent_resource::{
    provider_terminal_resource, AgentResourceBinding, AgentResourceBindingError,
};
use crate::domain::cockpit::{
    cockpit_surface, relative_path_is_safe, workspace_projection, ConfigSidebarFolder,
    ConfigSidebarProject, ConfigSidebarProvider, ConfigSidebarProviderKind, ConfigSidebarServer,
    ConfigSidebarSnapshot, ConfigSidebarSsh, TaskCockpitDeniedReason, TaskCockpitQuery,
    TaskCockpitResult, TaskCockpitSurface, TaskCockpitUnavailableReason, TaskFileEntry,
    TaskFilesListProjection, TaskFilesReadProjection, TaskGitMutateIntent, TaskGitProjection,
    TaskRepositorySelector, TaskSshEndpoint, TaskSshLifecycle, TaskSshProjection,
    TaskSshRuntimeError, TaskSshRuntimeProjection, TaskTerminalChip, TaskTerminalProjection,
    TaskTerminalsProjection, TerminalRuntimeStateWire, MAX_COCKPIT_FILE_LIST,
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
use crate::host_log;
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
    /// Exact restore queue / in-flight hint for TerminalReadiness. Defaults to
    /// Unknown so compatibility callers never invent absence.
    pub provider_launch_hint: ProviderLaunchReadinessHint,
    /// The most recent provider restore/re-pin cause the host holds for this
    /// task, in the words the client should show. A provider terminal that is
    /// unavailable while a cause is known must never reply with the bare
    /// reason alone.
    pub provider_restore_detail: Option<&'a str>,
}

/// Host-owned launch/restore hint passed into TaskCockpitDispatch. Never
/// synthesized from terminal text or PID observations.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum ProviderLaunchReadinessHint {
    #[default]
    Unknown,
    /// Exact task is queued or in-flight for provider restore/start.
    StartPending,
    /// Host proved the task is not in the restore queue or in-flight set.
    NotPending,
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
    serve_task_cockpit_bounded(
        dispatch,
        crate::domain::snapshot::MAX_SNAPSHOT_PAGE_ENCODED_BYTES,
    )
}

pub(crate) fn serve_task_cockpit_bounded(
    dispatch: TaskCockpitDispatch<'_>,
    max_response_bytes: u32,
) -> QueryOutcome {
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
            | TaskCockpitQuery::RemoteAccess(_)
    ) {
        // Mutation is owned by the exclusive host executor, which re-issues
        // workspace authority before returning a snapshot.
        return QueryOutcome::Err(QueryError::Unavailable {
            reason: "config_mutate",
        });
    }
    if matches!(dispatch.query, TaskCockpitQuery::OpenShellTerminal { .. }) {
        // Opening a shell resolves a launch and writes a durable resource.
        // Only the exclusive host executor holds the authority to do either,
        // so reaching this read-only serve path is a routing bug.
        return QueryOutcome::Err(QueryError::Unavailable {
            reason: "open_shell_executor",
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
        TaskCockpitQuery::ProviderInputState => {
            if !dispatch.capabilities.contains(Capability::ProviderInput) {
                return QueryOutcome::Err(QueryError::UnsupportedCapability);
            }
            let state =
                crate::domain::cockpit::ProviderInputStateProjection::from_snapshot(&snapshot);
            if state.pending_wait_command_ids.len()
                > crate::domain::cockpit::MAX_PROVIDER_INPUT_STATE_WAITS
            {
                return QueryOutcome::Err(QueryError::Unavailable {
                    reason: "provider_wait_limit",
                });
            }
            QueryOutcome::Ok(QueryResult::TaskCockpit(
                TaskCockpitResult::ProviderInputState(state),
            ))
        }
        TaskCockpitQuery::ConfigSnapshot
        | TaskCockpitQuery::AgentConnection
        | TaskCockpitQuery::ConfigCreateProject { .. }
        | TaskCockpitQuery::ConfigUpsertCommand { .. }
        | TaskCockpitQuery::ConfigArchiveCommand { .. }
        | TaskCockpitQuery::ConfigRunCommand { .. }
        | TaskCockpitQuery::ConfigCommandDetail { .. }
        | TaskCockpitQuery::ProviderSettings(_)
        | TaskCockpitQuery::RemoteAccess(_)
        | TaskCockpitQuery::OpenShellTerminal { .. } => {
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
        TaskCockpitQuery::OpenConversationSubscription { .. }
        | TaskCockpitQuery::ReleaseConversationSubscription { .. } => {
            // Owned by HostRequestExecutor so the subscription registry stays
            // serialized with output lifecycle. Reaching this arm is a routing bug.
            QueryOutcome::Err(QueryError::Unavailable {
                reason: "conversation_subscription_executor",
            })
        }
        TaskCockpitQuery::TaskTerminals => serve_task_terminals(&dispatch, task_id, &snapshot),
        TaskCockpitQuery::Terminal
        | TaskCockpitQuery::TerminalScroll { .. }
        | TaskCockpitQuery::TerminalResize { .. }
        | TaskCockpitQuery::TerminalReadiness
        | TaskCockpitQuery::TerminalFor { .. }
        | TaskCockpitQuery::TerminalScrollFor { .. }
        | TaskCockpitQuery::TerminalResizeFor { .. }
        | TaskCockpitQuery::TerminalReadinessFor { .. } => {
            let readiness_query = matches!(
                dispatch.query,
                TaskCockpitQuery::TerminalReadiness | TaskCockpitQuery::TerminalReadinessFor { .. }
            );
            // `None` keeps the pre-plain-shell selection: the provider slot.
            let selector = terminal_query_resource(dispatch.query);
            let scroll = match dispatch.query {
                TaskCockpitQuery::TerminalScroll { delta_lines }
                | TaskCockpitQuery::TerminalScrollFor { delta_lines, .. } => Some(*delta_lines),
                _ => None,
            };
            let resize = match dispatch.query {
                TaskCockpitQuery::TerminalResize { cols, rows }
                | TaskCockpitQuery::TerminalResizeFor { cols, rows, .. } => Some((*cols, *rows)),
                _ => None,
            };
            // Bounds first: an out-of-range request is refused on its own terms
            // and must not depend on which terminal it happened to name.
            if let Some(delta_lines) = scroll {
                if delta_lines == 0 || delta_lines.unsigned_abs() > 256 {
                    return denied(
                        TaskCockpitSurface::Terminal,
                        TaskCockpitDeniedReason::Unauthorized,
                    );
                }
            }
            let size = match resize {
                Some((cols, rows)) => {
                    match crate::terminal::protocol::TerminalSize::new(cols, rows) {
                        Ok(size) => Some(size),
                        Err(_) => {
                            return denied(
                                TaskCockpitSurface::Terminal,
                                TaskCockpitDeniedReason::Unauthorized,
                            )
                        }
                    }
                }
                None => None,
            };
            if scroll.is_some() || size.is_some() {
                let Some(service) = dispatch.terminal_service else {
                    return unavailable(
                        TaskCockpitSurface::Terminal,
                        TaskCockpitUnavailableReason::TerminalUnavailable,
                    );
                };
                // Authorize BEFORE touching the terminal. Scroll and resize
                // reach the live PTY, so a fence checked afterwards has already
                // let an unauthorized caller move someone else's terminal.
                if let Some(refusal) = terminal_viewport_mutation_refusal(
                    &dispatch, service, task_id, &snapshot, selector,
                ) {
                    return refusal;
                }
                if let Some(delta_lines) = scroll {
                    if service
                        .scroll_task_terminal_for(task_id, selector, delta_lines)
                        .is_err()
                    {
                        return unavailable(
                            TaskCockpitSurface::Terminal,
                            TaskCockpitUnavailableReason::TerminalUnavailable,
                        );
                    }
                }
                if let Some(size) = size {
                    if service
                        .resize_task_terminal_for(task_id, selector, size)
                        .is_err()
                    {
                        return unavailable(
                            TaskCockpitSurface::Terminal,
                            TaskCockpitUnavailableReason::TerminalUnavailable,
                        );
                    }
                }
            }
            serve_task_terminal(
                &dispatch,
                task_id,
                &snapshot,
                selector,
                readiness_query,
                max_response_bytes,
            )
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
        TaskCockpitQuery::GitFileDiffTargeted {
            selector,
            relative_path,
            staged,
        } => serve_git_file_diff_targeted(
            &dispatch,
            task_id,
            &snapshot.task,
            selector,
            relative_path,
            *staged,
        ),
        TaskCockpitQuery::GitHistoryTargeted {
            selector,
            limit,
            skip,
        } => {
            serve_git_history_targeted(&dispatch, task_id, &snapshot.task, selector, *limit, *skip)
        }
        TaskCockpitQuery::GitCommitDiffTargeted {
            selector,
            commit_hash,
        } => serve_git_commit_diff_targeted(
            &dispatch,
            task_id,
            &snapshot.task,
            selector,
            commit_hash,
        ),
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
const MAX_TERMINAL_STYLED_CELLS_FOR_WIRE: usize = 3_000;
const MAX_TERMINAL_CONNECT_STYLED_CELLS: usize = 256;

fn compact_terminal_screen_for_wire(
    mut screen: crate::terminal::session::TerminalScreenSnapshot,
    styled_cell_limit: usize,
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
    screen.cells.retain(|indexed| {
        let cell = &indexed.cell;
        !cell.default_foreground
            || !cell.default_background
            || cell.bold
            || cell.dim
            || cell.italic
            || cell.underline
            || cell.undercurl
            || cell.strike
            || cell.hidden
            || cell.has_hyperlink
            || !cell.zero_width.is_empty()
    });
    if screen.cells.len() > styled_cell_limit {
        // A wide, fully coloured provider TUI can legitimately style more
        // cells than one MessagePack collection may contain. Text remains
        // complete in `text_lines`; keep the newest/bottom prompt region's
        // style overrides so the frame cannot disconnect the client.
        screen
            .cells
            .sort_unstable_by_key(|indexed| (indexed.row, indexed.column));
        let discard = screen.cells.len() - styled_cell_limit;
        screen.cells.drain(..discard);
    }
    screen.lines.clear();
    // The retained-window margin is the one part of a screen that is optional.
    // A Noise transport message has a hard ~64 KiB ceiling and the enclosing
    // envelope still needs headroom, so a remote client trades local scrolling
    // for a frame that fits: it simply gets no margin and falls back to the
    // synchronous scroll, exactly as an older host's client does. The local IPC
    // transport has no such ceiling and keeps the margin.
    if styled_cell_limit <= MAX_TERMINAL_CONNECT_STYLED_CELLS {
        screen.margin_above.clear();
        screen.margin_below.clear();
    }
    (screen, text_lines)
}

const MAX_CONVERSATION_PAGE_ITEMS: usize = 128;
// Keep semantic pages below the smallest supported encrypted carrier frame.
// Noise transport messages have a hard ~64 KiB ceiling and the enclosing
// QueryReply/Connect envelope still needs headroom around this page.
const MAX_CONVERSATION_PAGE_BYTES: usize = 48 * 1024;

/// The durable resource one terminal query addresses, or `None` for the
/// provider slot. The legacy unit variants are always the provider slot.
fn terminal_query_resource(query: &TaskCockpitQuery) -> Option<crate::domain::ResourceId> {
    match query {
        TaskCockpitQuery::TerminalFor { resource_id }
        | TaskCockpitQuery::TerminalScrollFor { resource_id, .. }
        | TaskCockpitQuery::TerminalResizeFor { resource_id, .. }
        | TaskCockpitQuery::TerminalReadinessFor { resource_id } => Some(*resource_id),
        _ => None,
    }
}

/// Project one bounded semantic conversation page for a Task. Shared by the
/// one-shot Conversation query and OpenConversationSubscription initial capture.
fn serve_task_terminal(
    dispatch: &TaskCockpitDispatch<'_>,
    task_id: TaskId,
    snapshot: &crate::domain::TaskSnapshot,
    resource_id: Option<crate::domain::ResourceId>,
    readiness_query: bool,
    max_response_bytes: u32,
) -> QueryOutcome {
    let Some(service) = dispatch.terminal_service else {
        return provider_terminal_unavailable(
            dispatch,
            TaskCockpitUnavailableReason::TerminalUnavailable,
        );
    };
    match service.task_terminal_view_for(task_id, resource_id) {
        Ok(Some(terminal)) if !terminal.is_provider => serve_plain_shell_terminal(
            dispatch,
            service,
            task_id,
            snapshot,
            terminal,
            max_response_bytes,
        ),
        Ok(Some(terminal)) => {
            // The provider slot always carries its complete durable identity;
            // a plain shell never reaches this legacy provider surface.
            let (
                Some(terminal_agent_session_id),
                Some(terminal_runtime_generation),
                Some(terminal_action_epoch),
            ) = (
                terminal.agent_session_id,
                terminal.runtime_generation,
                terminal.action_epoch,
            )
            else {
                return denied(
                    TaskCockpitSurface::Terminal,
                    TaskCockpitDeniedReason::StaleFence,
                );
            };
            // A terminal projection is also the client-specific attachment
            // handshake. Raw input deliberately defaults to read-only until
            // the host has revalidated the exact Task/Agent/Resource tuple
            // for this authenticated client. Without that grant every native
            // and fleet terminal write is rejected as ReadOnly even though the
            // caller holds ProviderInput capability.
            if let Some(refusal) = provider_terminal_fence_refusal(
                dispatch,
                snapshot,
                terminal_agent_session_id,
                terminal_runtime_generation,
                terminal_action_epoch,
                terminal.resource_id,
                terminal.resource_generation,
            ) {
                return refusal;
            }
            if service
                .grant_client(
                    terminal.terminal_id,
                    dispatch.client_id,
                    crate::terminal::protocol::ClientInputGrant::ReadWrite,
                )
                .is_err()
            {
                return denied(
                    TaskCockpitSurface::Terminal,
                    TaskCockpitDeniedReason::StaleFence,
                );
            }
            let styled_cell_limit = if max_response_bytes <= 64 * 1024 {
                MAX_TERMINAL_CONNECT_STYLED_CELLS
            } else {
                MAX_TERMINAL_STYLED_CELLS_FOR_WIRE
            };
            let (screen, text_lines) =
                compact_terminal_screen_for_wire(terminal.view.screen, styled_cell_limit);
            // The fence above already proved this terminal belongs to the
            // primary agent, so re-reading it here cannot pick a different one.
            let Some(agent) = snapshot
                .primary_agent_id
                .and_then(|id| snapshot.agents.get(&id))
            else {
                // Unreachable given the fence above, so this must not panic in
                // a host query path: refuse the way the fence itself would.
                return denied(
                    TaskCockpitSurface::Terminal,
                    TaskCockpitDeniedReason::StaleFence,
                );
            };
            if readiness_query && agent.provider_session_id.is_none() {
                use crate::providers::input::{
                    classify_codex_identityless_startup_readiness,
                    provider_identityless_setup_required, CodexIdentitylessStartupReadiness,
                };
                if provider_identityless_setup_required(agent.provider_kind, &text_lines) {
                    return provider_terminal_unavailable(
                        dispatch,
                        TaskCockpitUnavailableReason::TerminalProviderSetupRequired,
                    );
                }
                if agent.provider_kind == crate::providers::ProviderKind::Codex
                    && matches!(
                        classify_codex_identityless_startup_readiness(&text_lines),
                        CodexIdentitylessStartupReadiness::StartupPending
                    )
                {
                    return provider_terminal_unavailable(
                        dispatch,
                        TaskCockpitUnavailableReason::TerminalStartPending,
                    );
                }
            }
            let projection = TaskTerminalProjection {
                task_id,
                terminal_id: terminal.terminal_id,
                session_id: terminal.session_id,
                agent_session_id: terminal_agent_session_id,
                resource_id: terminal.resource_id,
                runtime_generation: terminal_runtime_generation,
                resource_generation: terminal.resource_generation,
                action_epoch: terminal_action_epoch,
                focus_epoch: terminal.focus_epoch,
                accepted_input_sequence: terminal.accepted_input_sequence,
                accepts_input_without_conversation_id: dispatch.service_runtime.is_some_and(
                    |manager| {
                        manager.accepts_input_without_conversation_id(
                            task_id,
                            terminal_agent_session_id,
                            terminal.resource_id,
                            terminal_runtime_generation,
                            terminal_action_epoch,
                        )
                    },
                ),
                sequence: terminal.sequence,
                title: terminal.view.runtime.title,
                text_lines,
                screen,
                is_provider: terminal.is_provider,
                runtime_state: runtime_state_wire(&terminal.runtime_state),
            };
            let Some(projection) = fit_terminal_projection_for_wire(
                projection,
                dispatch.request_id,
                max_response_bytes,
            ) else {
                return provider_terminal_unavailable(
                    dispatch,
                    TaskCockpitUnavailableReason::TerminalUnavailable,
                );
            };
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Terminal(
                projection,
            )))
        }
        Ok(None) => {
            // Readiness classification is about the provider runtime's start.
            // A shell selector that resolves to nothing is simply gone, and
            // never "the provider has not started".
            if !readiness_query || resource_id.is_some() {
                // Legacy Terminal callers keep the closed Unavailable reason.
                return provider_terminal_unavailable(
                    dispatch,
                    TaskCockpitUnavailableReason::TerminalUnavailable,
                );
            }
            classify_terminal_readiness_absence(dispatch, task_id, snapshot)
        }
        Err(_) => denied(
            TaskCockpitSurface::Terminal,
            TaskCockpitDeniedReason::StaleFence,
        ),
    }
}

/// The provider terminal's complete authorization, or the exact refusal.
///
/// This is the one definition of "may this client act on the provider
/// terminal": the durable Task/Agent/Resource tuple has to still be the one the
/// hosted terminal was attached for, and the client has to hold ProviderInput.
/// It returns `Some(outcome)` for a refusal rather than a bool so the caller
/// cannot flatten `StaleFence` and `CapabilityDenied` into one answer, and so a
/// caller that must authorize BEFORE mutating a terminal runs exactly the same
/// checks as the one that authorizes while projecting it.
fn provider_terminal_fence_refusal(
    dispatch: &TaskCockpitDispatch<'_>,
    snapshot: &crate::domain::TaskSnapshot,
    terminal_agent_session_id: crate::domain::AgentSessionId,
    terminal_runtime_generation: u64,
    terminal_action_epoch: u64,
    terminal_resource_id: crate::domain::ResourceId,
    terminal_resource_generation: u64,
) -> Option<QueryOutcome> {
    let stale = || {
        Some(denied(
            TaskCockpitSurface::Terminal,
            TaskCockpitDeniedReason::StaleFence,
        ))
    };
    let Some(primary_agent_id) = snapshot.primary_agent_id else {
        return stale();
    };
    let Some(agent) = snapshot.agents.get(&primary_agent_id) else {
        return stale();
    };
    let Ok(Some(resource)) = provider_terminal_resource(snapshot, agent) else {
        return stale();
    };
    if terminal_agent_session_id != primary_agent_id
        || terminal_runtime_generation != agent.runtime_generation
        || terminal_action_epoch == 0
        || terminal_resource_id != resource.id
        || terminal_resource_generation != resource.runtime_generation
    {
        return stale();
    }
    if !dispatch.capabilities.contains(Capability::ProviderInput) {
        return Some(denied(
            TaskCockpitSurface::Terminal,
            TaskCockpitDeniedReason::CapabilityDenied,
        ));
    }
    None
}

/// One plain shell's complete authorization, or the exact refusal.
///
/// A shell has no agent session, so its authority is the durable resource
/// itself: present on this task, still a plain shell, Active, and at the
/// generation the hosted attachment was opened for. `ProviderInput` is
/// deliberately not required -- a shell is not the provider's input surface.
fn plain_shell_fence_refusal(
    snapshot: &crate::domain::TaskSnapshot,
    terminal_resource_id: crate::domain::ResourceId,
    terminal_resource_generation: u64,
) -> Option<QueryOutcome> {
    let stale = || {
        Some(denied(
            TaskCockpitSurface::Terminal,
            TaskCockpitDeniedReason::StaleFence,
        ))
    };
    let Some(resource) = snapshot.resources.get(&terminal_resource_id) else {
        return stale();
    };
    if resource.resource_kind != crate::domain::ResourceKind::Terminal
        || !resource.recipe.is_plain_shell()
        || resource.lifecycle != crate::domain::ResourceLifecycle::Active
        || resource.runtime_generation != terminal_resource_generation
    {
        return stale();
    }
    None
}

/// Authorize a viewport mutation (scroll/resize) BEFORE it is applied.
///
/// Scroll and resize change the terminal the moment they are called, and they
/// reach the live PTY. Running them ahead of the fence -- which is what this
/// dispatcher used to do -- means a client with no authority over a terminal
/// resizes the local user's PTY and only then receives its denial. The refusal
/// here is the same one the projection path would have produced, so the two can
/// never disagree about who may act.
fn terminal_viewport_mutation_refusal(
    dispatch: &TaskCockpitDispatch<'_>,
    service: &TerminalService,
    task_id: TaskId,
    snapshot: &crate::domain::TaskSnapshot,
    resource_id: Option<crate::domain::ResourceId>,
) -> Option<QueryOutcome> {
    let fence = match service.task_terminal_fence_for(task_id, resource_id) {
        Ok(Some(fence)) => fence,
        // No terminal to act on is the same closed answer the mutation itself
        // would have produced.
        Ok(None) => {
            return Some(unavailable(
                TaskCockpitSurface::Terminal,
                TaskCockpitUnavailableReason::TerminalUnavailable,
            ))
        }
        Err(_) => {
            return Some(denied(
                TaskCockpitSurface::Terminal,
                TaskCockpitDeniedReason::StaleFence,
            ))
        }
    };
    if !fence.is_provider {
        return plain_shell_fence_refusal(snapshot, fence.resource_id, fence.resource_generation);
    }
    // A provider terminal reached through a resource-addressed query is still
    // the provider terminal: `resource_id` selects by durable resource and does
    // not decide authority.
    let (Some(agent_session_id), Some(runtime_generation), Some(action_epoch)) = (
        fence.agent_session_id,
        fence.runtime_generation,
        fence.action_epoch,
    ) else {
        return Some(denied(
            TaskCockpitSurface::Terminal,
            TaskCockpitDeniedReason::StaleFence,
        ));
    };
    provider_terminal_fence_refusal(
        dispatch,
        snapshot,
        agent_session_id,
        runtime_generation,
        action_epoch,
        fence.resource_id,
        fence.resource_generation,
    )
}

/// One-to-one mapping of the hosted runtime state onto the wire enum.
fn runtime_state_wire(
    state: &crate::terminal::service::TerminalRuntimeState,
) -> TerminalRuntimeStateWire {
    match state {
        crate::terminal::service::TerminalRuntimeState::Running => {
            TerminalRuntimeStateWire::Running
        }
        crate::terminal::service::TerminalRuntimeState::Exited { summary } => {
            TerminalRuntimeStateWire::Exited {
                summary: summary.clone(),
            }
        }
        crate::terminal::service::TerminalRuntimeState::Unknown => {
            TerminalRuntimeStateWire::Unknown
        }
    }
}

/// Serve one plain shell's screen.
///
/// A shell has no agent session, so there is no provider fence to revalidate:
/// its authority is the durable resource itself. The checks are therefore the
/// resource existing on this Task, being a plain shell, being Active, and its
/// runtime generation matching the attachment the service holds. `ProviderInput`
/// is deliberately not required -- a shell is not the provider's input surface
/// -- but the terminal is still granted `ReadWrite` exactly as the provider path
/// grants it, because the projection doubles as the client's attachment.
fn serve_plain_shell_terminal(
    dispatch: &TaskCockpitDispatch<'_>,
    service: &TerminalService,
    task_id: TaskId,
    snapshot: &crate::domain::TaskSnapshot,
    terminal: crate::terminal::service::TaskTerminalView,
    max_response_bytes: u32,
) -> QueryOutcome {
    if let Some(refusal) =
        plain_shell_fence_refusal(snapshot, terminal.resource_id, terminal.resource_generation)
    {
        return refusal;
    }
    if service
        .grant_client(
            terminal.terminal_id,
            dispatch.client_id,
            crate::terminal::protocol::ClientInputGrant::ReadWrite,
        )
        .is_err()
    {
        return denied(
            TaskCockpitSurface::Terminal,
            TaskCockpitDeniedReason::StaleFence,
        );
    }
    let styled_cell_limit = if max_response_bytes <= 64 * 1024 {
        MAX_TERMINAL_CONNECT_STYLED_CELLS
    } else {
        MAX_TERMINAL_STYLED_CELLS_FOR_WIRE
    };
    let runtime_state = runtime_state_wire(&terminal.runtime_state);
    let (screen, text_lines) =
        compact_terminal_screen_for_wire(terminal.view.screen, styled_cell_limit);
    let projection = TaskTerminalProjection {
        task_id,
        terminal_id: terminal.terminal_id,
        session_id: terminal.session_id,
        // A shell has no agent session, no provider runtime generation, and no
        // launch action epoch. These stay required on the wire, so they carry
        // the documented zero sentinels; `is_provider: false` is what tells a
        // client they are sentinels rather than identity.
        agent_session_id: crate::domain::AgentSessionId::nil(),
        resource_id: terminal.resource_id,
        runtime_generation: 0,
        resource_generation: terminal.resource_generation,
        action_epoch: terminal.action_epoch.unwrap_or(0),
        focus_epoch: terminal.focus_epoch,
        accepted_input_sequence: terminal.accepted_input_sequence,
        // Provider conversation identity is meaningless for a shell.
        accepts_input_without_conversation_id: false,
        sequence: terminal.sequence,
        title: terminal.view.runtime.title,
        text_lines,
        screen,
        is_provider: false,
        runtime_state,
    };
    let Some(projection) =
        fit_terminal_projection_for_wire(projection, dispatch.request_id, max_response_bytes)
    else {
        return unavailable(
            TaskCockpitSurface::Terminal,
            TaskCockpitUnavailableReason::TerminalUnavailable,
        );
    };
    QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Terminal(
        projection,
    )))
}

/// The Task's terminal strip: the provider chip first, then the durable order.
///
/// Chips come from the durable facts, never from the hosted runtime map, so a
/// shell whose hosted entry has already been retired (close closes the hosted
/// terminal before `ResourceReleased` clears the strip) still renders, with its
/// recorded exit if one was written and `Unknown` otherwise.
fn serve_task_terminals(
    dispatch: &TaskCockpitDispatch<'_>,
    task_id: TaskId,
    snapshot: &crate::domain::TaskSnapshot,
) -> QueryOutcome {
    let Some(service) = dispatch.terminal_service else {
        return unavailable(
            TaskCockpitSurface::Terminal,
            TaskCockpitUnavailableReason::TerminalUnavailable,
        );
    };
    let Ok(summaries) = service.task_terminal_summaries(task_id) else {
        return denied(
            TaskCockpitSurface::Terminal,
            TaskCockpitDeniedReason::StaleFence,
        );
    };
    let live_by_resource = summaries
        .iter()
        .map(|summary| (summary.resource_id, summary))
        .collect::<std::collections::HashMap<_, _>>();
    // Ambiguity is a refusal, not a chip the strip may quietly omit: a client
    // shown a shells-only strip would read that as "this task has no provider
    // terminal", which is the opposite of what an ambiguous fence means. The
    // serve path denies it, so this does too.
    let provider_resource = match snapshot
        .primary_agent_id
        .and_then(|agent_id| snapshot.agents.get(&agent_id))
        .map(|agent| provider_terminal_resource(snapshot, agent))
    {
        Some(Ok(resource)) => resource.map(|resource| resource.id),
        Some(Err(_)) => {
            return denied(
                TaskCockpitSurface::Terminal,
                TaskCockpitDeniedReason::StaleFence,
            );
        }
        None => None,
    };
    let workspace_root = dispatch
        .workspace_projects
        .and_then(|projects| projects.root_for(snapshot.task.project_id));
    let chip_for = |resource_id: crate::domain::ResourceId, is_provider: bool| {
        let facts = snapshot.terminal_facts.get(&resource_id);
        let exit = facts.and_then(|facts| facts.exit.as_ref());
        let runtime_state = match live_by_resource.get(&resource_id).map(|live| &live.state) {
            Some(state) => runtime_state_wire(state),
            // No hosted entry: the durable exit fact is the only truth there
            // is, and its absence is genuinely unknown -- never a running
            // terminal.
            None => match exit {
                Some(exit) => TerminalRuntimeStateWire::Exited {
                    summary: exit.summary.clone(),
                },
                None => TerminalRuntimeStateWire::Unknown,
            },
        };
        TaskTerminalChip {
            resource_id,
            is_provider,
            title: facts.and_then(|facts| facts.title.clone()),
            label: terminal_label_for(snapshot, resource_id),
            runtime_state,
            live_cwd: facts
                .and_then(|facts| facts.live_cwd.as_ref())
                .map(|cwd| redacted_terminal_cwd(cwd, workspace_root)),
            exit: exit.cloned(),
            created_at_ms: facts.map(|facts| facts.created_at_ms).unwrap_or_default(),
            last_activity_at_ms: facts
                .map(|facts| facts.last_activity_at_ms)
                .unwrap_or_default(),
        }
    };
    let mut terminals = Vec::new();
    if let Some(provider) = provider_resource {
        terminals.push(chip_for(provider, true));
    }
    for resource_id in &snapshot.terminal_strip.order {
        terminals.push(chip_for(*resource_id, false));
    }
    QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::TaskTerminals(
        TaskTerminalsProjection {
            task_id,
            terminals,
            order: snapshot.terminal_strip.order.clone(),
            focused: snapshot.terminal_strip.focused,
        },
    )))
}

/// Redact one terminal's live cwd for the wire.
///
/// The Task Cockpit contract is that projections carry no client-authoritative
/// absolute paths, and a shell's cwd is the one field on the strip that would
/// otherwise be one. Inside the task's workspace root it becomes the path
/// relative to that root (empty at the root itself, rendered as `.` so the chip
/// has something to show); anywhere else only the final component survives, so
/// a shell that walked to an unrelated directory discloses its name and not the
/// path that reaches it.
fn redacted_terminal_cwd(
    cwd: &std::path::Path,
    workspace_root: Option<&std::path::Path>,
) -> String {
    if let Some(root) = workspace_root {
        if let Ok(relative) = cwd.strip_prefix(root) {
            let rendered = relative.to_string_lossy().replace('\\', "/");
            return if rendered.is_empty() {
                ".".to_string()
            } else {
                rendered
            };
        }
    }
    cwd.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        // A path with no final component is a bare root; naming the volume is
        // the whole path, so say nothing rather than leak it.
        .unwrap_or_else(|| ".".to_string())
}

/// Fallback display text for one terminal chip: a shell's launch program stem
/// (`pwsh`, `cmd`), or `terminal` for the provider slot. The running child
/// command label is a separate, later concern; this never guesses one.
fn terminal_label_for(
    snapshot: &crate::domain::TaskSnapshot,
    resource_id: crate::domain::ResourceId,
) -> String {
    let recipe = snapshot
        .resources
        .get(&resource_id)
        .map(|resource| &resource.recipe);
    // `is_plain_shell` is the one definition of "carries a launch"; matching the
    // recipe shape by hand here would be a second one, free to drift from it.
    match recipe {
        Some(recipe) if recipe.is_plain_shell() => match recipe {
            crate::domain::resource::ResourceRecipe::Terminal {
                launch: Some(launch),
                ..
            } => launch
                .program
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_else(|| "shell".to_string()),
            _ => "shell".to_string(),
        },
        _ => "terminal".to_string(),
    }
}

/// Reduce only optional styled-cell detail until the complete QueryReply fits
/// the negotiated carrier page. Plain text, cursor, mode, and fence identity
/// remain intact, so browser/LAN clients never lose the whole connection for a
/// richly coloured provider screen.
fn fit_terminal_projection_for_wire(
    mut projection: TaskTerminalProjection,
    request_id: RequestId,
    max_response_bytes: u32,
) -> Option<TaskTerminalProjection> {
    const ENVELOPE_HEADROOM: usize = 2 * 1024;
    let budget = usize::try_from(max_response_bytes)
        .ok()?
        .saturating_sub(ENVELOPE_HEADROOM);
    loop {
        let reply = crate::domain::QueryReply {
            request_id,
            outcome: QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Terminal(
                projection.clone(),
            ))),
        };
        let encoded = rmp_serde::to_vec_named(&reply).ok()?;
        if encoded.len() <= budget {
            return Some(projection);
        }
        if !projection.screen.cells.is_empty() {
            let discard = (projection.screen.cells.len() / 2).max(1);
            projection.screen.cells.drain(..discard);
            continue;
        }
        // Terminal text normally represents only the visible grid. Keep the
        // newest prompt rows if an unusually tall screen still exceeds budget.
        if projection.text_lines.len() > 1 {
            let discard = (projection.text_lines.len() / 4).max(1);
            projection.text_lines.drain(..discard);
            continue;
        }
        return None;
    }
}

/// Opt-in TerminalReadiness classification when no terminal attachment exists.
/// Presence, busy locks, and unknown service never become NotStarted.
fn classify_terminal_readiness_absence(
    dispatch: &TaskCockpitDispatch<'_>,
    task_id: TaskId,
    snapshot: &crate::domain::TaskSnapshot,
) -> QueryOutcome {
    if matches!(
        dispatch.provider_launch_hint,
        ProviderLaunchReadinessHint::StartPending
    ) {
        return provider_terminal_unavailable(
            dispatch,
            TaskCockpitUnavailableReason::TerminalStartPending,
        );
    }
    let Some(primary_agent_id) = snapshot.primary_agent_id else {
        return provider_terminal_unavailable(
            dispatch,
            TaskCockpitUnavailableReason::TerminalUnavailable,
        );
    };
    let Some(agent) = snapshot.agents.get(&primary_agent_id) else {
        return provider_terminal_unavailable(
            dispatch,
            TaskCockpitUnavailableReason::TerminalUnavailable,
        );
    };
    // One rule for "the provider's terminal resource", shared with the serve
    // path and the strip. Before that helper existed this filter admitted plain
    // shells, which are Terminal resources at the same runtime generation, so a
    // task with any open shell classified as TerminalUnavailable instead of
    // reaching this classifier at all.
    let Ok(Some(resource)) = provider_terminal_resource(snapshot, agent) else {
        return provider_terminal_unavailable(
            dispatch,
            TaskCockpitUnavailableReason::TerminalUnavailable,
        );
    };
    let Some(manager) = dispatch.service_runtime else {
        return provider_terminal_unavailable(
            dispatch,
            TaskCockpitUnavailableReason::TerminalUnavailable,
        );
    };
    // Live provider without terminal attachment is not absence. A busy or
    // poisoned runtime book is unknown — never treat None as verified absence.
    match manager.try_has_live_provider_runtime(
        task_id,
        primary_agent_id,
        resource.id,
        agent.runtime_generation,
    ) {
        Some(true) => {
            return provider_terminal_unavailable(
                dispatch,
                TaskCockpitUnavailableReason::TerminalStartPending,
            );
        }
        None => {
            return provider_terminal_unavailable(
                dispatch,
                TaskCockpitUnavailableReason::TerminalUnavailable,
            );
        }
        Some(false) => {}
    }
    match manager.try_classify_persisted_provider_launch(primary_agent_id) {
        Ok(Some(true)) => provider_terminal_unavailable(
            dispatch,
            TaskCockpitUnavailableReason::TerminalStartPending,
        ),
        Ok(Some(false)) => {
            if !matches!(
                dispatch.provider_launch_hint,
                ProviderLaunchReadinessHint::NotPending
            ) || !snapshot.is_unstarted_draft()
            {
                return provider_terminal_unavailable(
                    dispatch,
                    TaskCockpitUnavailableReason::TerminalUnavailable,
                );
            }
            provider_terminal_unavailable(
                dispatch,
                TaskCockpitUnavailableReason::TerminalNotStarted,
            )
        }
        Ok(None) | Err(_) => provider_terminal_unavailable(
            dispatch,
            TaskCockpitUnavailableReason::TerminalUnavailable,
        ),
    }
}

pub(crate) fn serve_conversation(
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
    let capture = store.capture_conversation_after(&key, after_sequence);
    drop(store);
    // Sort retained pointers and resolve replacement links outside the journal
    // mutex; provider token recording must not wait on page projection.
    let replay = capture.map(|capture| capture.into_replay());
    let cursor_rolled_over = replay
        .as_ref()
        .is_some_and(|r| r.cursor_rolled_over || after_sequence > r.through_sequence)
        || (replay.is_none() && after_sequence > 0);

    let (oldest_sequence, high_water, events) = match replay {
        Some(replay) => (
            replay.oldest_sequence,
            replay.through_sequence,
            replay.events,
        ),
        None => (0, 0, Vec::new()),
    };
    let after_sequence = if cursor_rolled_over {
        0
    } else {
        after_sequence
    };
    // Fixed high-water for this capture. `after_sequence` is a forward exclusive
    // cursor: return the next retained prefix page in ascending sequence order.
    let replaced = events
        .iter()
        .filter_map(|event| event.replaces_sequence)
        .collect::<std::collections::BTreeSet<_>>();
    // An upsert receives a new replay sequence, but remains the same visible
    // message. Keep its original identity through retained replacement links;
    // clients can then replace cached partial text instead of appending copies.
    let links = events
        .iter()
        .filter_map(|event| {
            event
                .replaces_sequence
                .map(|previous| (event.sequence, previous))
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut identities = std::collections::BTreeMap::new();
    for event in &events {
        let mut original = event.sequence;
        while let Some(previous) = links.get(&original).copied() {
            if previous == 0 || previous >= original {
                return QueryOutcome::Err(QueryError::Unavailable {
                    reason: "semantic_replacement_lineage",
                });
            }
            original = identities.get(&previous).copied().unwrap_or(previous);
        }
        identities.insert(event.sequence, original);
    }
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
            id: conversation_event_id(task_id, identities[&event.sequence]),
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
        oldest_sequence,
        cursor_rolled_over,
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
        let page_end = page
            .facts
            .last()
            .map(|fact| fact.sequence)
            .unwrap_or(after_sequence);
        let more_retained = retained.iter().any(|event| event.sequence > page_end);
        page.next_sequence = more_retained.then_some(page_end);
        // The final page consumes filtered facts too; otherwise every wake
        // would fetch the same omitted terminal output again.
        page.through_sequence = if more_retained { page_end } else { high_water };
        let Ok(encoded_bytes) = crate::domain::snapshot::canonical_semantic_page_size(&page) else {
            return QueryOutcome::Err(QueryError::Unavailable {
                reason: "semantic_page_encode",
            });
        };
        if encoded_bytes as usize <= MAX_CONVERSATION_PAGE_BYTES {
            page.encoded_bytes = encoded_bytes;
            break;
        }
        // Never emit an empty continuation that cannot advance.
        if page.facts.len() <= 1 {
            return QueryOutcome::Err(QueryError::Unavailable {
                reason: "semantic_fact_too_large",
            });
        }
        page.facts.pop();
    }
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

fn serve_git_file_diff_targeted(
    dispatch: &TaskCockpitDispatch<'_>,
    task_id: TaskId,
    task: &crate::domain::task::TaskFacts,
    selector: &TaskRepositorySelector,
    relative_path: &str,
    staged: bool,
) -> QueryOutcome {
    if relative_path.len() > MAX_COCKPIT_READ_BYTES as usize
        || cockpit_repo_paths(&[relative_path.to_string()]).is_err()
    {
        return denied(
            TaskCockpitSurface::Git,
            TaskCockpitDeniedReason::PathTraversal,
        );
    }
    let (repository, resolved) =
        match open_git_repository_targeted(dispatch, task_id, task, selector) {
            Ok(opened) => opened,
            Err(outcome) => return outcome,
        };
    match crate::git::git_service::diff_file(&repository, relative_path, staged) {
        Ok(diff) => QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::GitFileDiff(
            crate::domain::cockpit::TaskGitFileDiffProjection {
                task_id,
                selector: resolved.selector,
                relative_path: relative_path.to_string(),
                staged,
                diff,
            },
        ))),
        Err(error) => {
            host_log!("devmanager-host: cockpit Git file diff failed: {error}");
            unavailable(
                TaskCockpitSurface::Git,
                TaskCockpitUnavailableReason::GitAuthorityNotIssued,
            )
        }
    }
}

fn serve_git_history_targeted(
    dispatch: &TaskCockpitDispatch<'_>,
    task_id: TaskId,
    task: &crate::domain::task::TaskFacts,
    selector: &TaskRepositorySelector,
    limit: u16,
    skip: u32,
) -> QueryOutcome {
    if limit == 0 || limit > 100 || skip > 10_000 {
        return QueryOutcome::Err(QueryError::InvalidRequest);
    }
    let (repository, resolved) =
        match open_git_repository_targeted(dispatch, task_id, task, selector) {
            Ok(opened) => opened,
            Err(outcome) => return outcome,
        };
    match crate::git::git_service::log(&repository, u32::from(limit), skip) {
        Ok(entries) => QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::GitHistory(
            crate::domain::cockpit::TaskGitHistoryProjection {
                task_id,
                selector: resolved.selector,
                entries,
                skip,
            },
        ))),
        Err(error) => {
            host_log!("devmanager-host: cockpit Git history failed: {error}");
            unavailable(
                TaskCockpitSurface::Git,
                TaskCockpitUnavailableReason::GitAuthorityNotIssued,
            )
        }
    }
}

fn serve_git_commit_diff_targeted(
    dispatch: &TaskCockpitDispatch<'_>,
    task_id: TaskId,
    task: &crate::domain::task::TaskFacts,
    selector: &TaskRepositorySelector,
    commit_hash: &str,
) -> QueryOutcome {
    let (repository, resolved) =
        match open_git_repository_targeted(dispatch, task_id, task, selector) {
            Ok(opened) => opened,
            Err(outcome) => return outcome,
        };
    match crate::git::git_service::diff_commit(&repository, commit_hash) {
        Ok(diff) => QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::GitCommitDiff(
            crate::domain::cockpit::TaskGitCommitDiffProjection {
                task_id,
                selector: resolved.selector,
                commit_hash: commit_hash.to_string(),
                diff,
            },
        ))),
        Err(error) => {
            host_log!("devmanager-host: cockpit Git commit diff failed: {error}");
            unavailable(
                TaskCockpitSurface::Git,
                TaskCockpitUnavailableReason::GitAuthorityNotIssued,
            )
        }
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
                    host_log!("devmanager-host: cockpit Git stage planning failed: {error}");
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
                entries: status
                    .entries
                    .iter()
                    .take(usize::from(MAX_COCKPIT_FILE_LIST))
                    .map(|entry| crate::domain::cockpit::TaskGitEntryProjection {
                        relative_path: entry.path.display_lossy().into_owned(),
                        original_relative_path: entry
                            .original_path
                            .as_ref()
                            .map(|path| path.display_lossy().into_owned()),
                        status: match entry.kind {
                            StatusKind::Modified => {
                                crate::domain::cockpit::TaskGitEntryStatus::Modified
                            }
                            StatusKind::Added => crate::domain::cockpit::TaskGitEntryStatus::Added,
                            StatusKind::Deleted => {
                                crate::domain::cockpit::TaskGitEntryStatus::Deleted
                            }
                            StatusKind::Renamed => {
                                crate::domain::cockpit::TaskGitEntryStatus::Renamed
                            }
                            StatusKind::Copied => {
                                crate::domain::cockpit::TaskGitEntryStatus::Copied
                            }
                            StatusKind::TypeChanged => {
                                crate::domain::cockpit::TaskGitEntryStatus::TypeChanged
                            }
                            StatusKind::Untracked => {
                                crate::domain::cockpit::TaskGitEntryStatus::Untracked
                            }
                            StatusKind::Conflict => {
                                crate::domain::cockpit::TaskGitEntryStatus::Conflict
                            }
                            StatusKind::Submodule => {
                                crate::domain::cockpit::TaskGitEntryStatus::Submodule
                            }
                            StatusKind::Unknown => {
                                crate::domain::cockpit::TaskGitEntryStatus::Unknown
                            }
                        },
                        staged: entry.is_staged(),
                        unstaged: entry.is_unstaged(),
                    })
                    .collect(),
            },
        ))),
        Err(error) => {
            host_log!(
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
            host_log!("devmanager-host: cockpit Git lease acquisition failed: {error:?}");
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
        host_log!("devmanager-host: cockpit Git fence revalidation failed: {outcome:?}");
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
                host_log!("devmanager-host: cockpit Git binding failed: {reason}");
            }
            _ => host_log!("devmanager-host: cockpit Git binding failed: {error}"),
        }
        unavailable(
            TaskCockpitSurface::Git,
            TaskCockpitUnavailableReason::GitAuthorityNotIssued,
        )
    })?;
    let repository =
        GitRepository::from_host_binding(binding, GitCancellation::new()).map_err(|error| {
            host_log!("devmanager-host: cockpit Git repository open failed: {error:?}");
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
        host_log!(
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
            host_log!(
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
    // `agent.runtime_generation == 0` is refused just above, so the helper's
    // generation clause carries the `> 0` this used to spell out. Taking the
    // shared rule also stops a plain shell from being picked here: this was a
    // `find`, so with an open shell at the agent's generation it could return
    // the shell rather than the provider's terminal.
    let Ok(Some(resource)) = provider_terminal_resource(&snapshot, agent) else {
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
        detail: None,
    }))
}

/// A provider-terminal refusal that carries the host's known cause. The bare
/// reason alone reads as "Terminal unavailable" with nothing to act on, which
/// is exactly what a stale executable pin used to produce forever.
fn provider_terminal_unavailable(
    dispatch: &TaskCockpitDispatch<'_>,
    reason: TaskCockpitUnavailableReason,
) -> QueryOutcome {
    QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Unavailable {
        surface: TaskCockpitSurface::Terminal,
        reason,
        detail: dispatch.provider_restore_detail.map(str::to_string),
    }))
}

fn unavailable(surface: TaskCockpitSurface, reason: TaskCockpitUnavailableReason) -> QueryOutcome {
    QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Unavailable {
        surface,
        reason,
        detail: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::command::{
        Command, CommandEnvelope, CommandReceipt, CreateTaskRequestIntent,
    };
    use crate::domain::task::{
        ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity,
    };
    use crate::domain::{CommandId, EnvironmentId, ProjectId};
    use crate::protocol::Capability;
    use crate::workspace::{WorkspaceRequest, WorkspaceResourceCoordinator};
    use std::fs;
    use std::path::Path;

    #[test]
    fn compact_terminal_wire_projection_preserves_sparse_ansi_styles() {
        use crate::terminal::session::{
            TerminalCellSnapshot, TerminalIndexedCellSnapshot, TerminalScreenSnapshot,
        };

        let plain = TerminalCellSnapshot {
            character: 'x',
            zero_width: Vec::new(),
            foreground: 0,
            background: 0,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            undercurl: false,
            strike: false,
            hidden: false,
            has_hyperlink: false,
            default_background: true,
            default_foreground: true,
        };
        let styled = TerminalCellSnapshot {
            character: '!',
            foreground: 0xffb000,
            bold: true,
            default_foreground: false,
            ..plain.clone()
        };
        let screen = TerminalScreenSnapshot {
            cols: 2,
            rows: 1,
            lines: vec![vec![plain.clone(), styled.clone()]],
            cells: vec![
                TerminalIndexedCellSnapshot {
                    row: 0,
                    column: 0,
                    cell: plain,
                },
                TerminalIndexedCellSnapshot {
                    row: 0,
                    column: 1,
                    cell: styled.clone(),
                },
            ],
            ..TerminalScreenSnapshot::default()
        };

        let (wire, text_lines) =
            compact_terminal_screen_for_wire(screen, MAX_TERMINAL_STYLED_CELLS_FOR_WIRE);

        assert_eq!(text_lines, vec!["x!".to_string()]);
        assert!(wire.lines.is_empty());
        assert_eq!(wire.cells.len(), 1, "only styled overrides cross IPC");
        assert_eq!(wire.cells[0].row, 0);
        assert_eq!(wire.cells[0].column, 1);
        assert_eq!(wire.cells[0].cell, styled);
    }

    #[test]
    fn normal_styled_terminal_screen_fits_the_messagepack_wire_contract() {
        use crate::protocol::{FrameLimits, MessagePackCodec};
        use crate::terminal::session::{
            TerminalCellSnapshot, TerminalIndexedCellSnapshot, TerminalScreenSnapshot,
        };

        let styled = TerminalCellSnapshot {
            character: 'x',
            zero_width: Vec::new(),
            foreground: 0x00ff00,
            background: 0,
            bold: true,
            dim: false,
            italic: false,
            underline: false,
            undercurl: false,
            strike: false,
            hidden: false,
            has_hyperlink: false,
            default_background: true,
            default_foreground: false,
        };
        let rows = 15;
        let cols = 77;
        let lines = vec![vec![styled.clone(); cols]; rows];
        let cells = lines
            .iter()
            .enumerate()
            .flat_map(|(row, line)| {
                line.iter()
                    .cloned()
                    .enumerate()
                    .map(move |(column, cell)| TerminalIndexedCellSnapshot { row, column, cell })
            })
            .collect();
        let screen = TerminalScreenSnapshot {
            cells,
            lines,
            rows,
            cols,
            ..TerminalScreenSnapshot::default()
        };

        let (screen, text_lines) =
            compact_terminal_screen_for_wire(screen, MAX_TERMINAL_STYLED_CELLS_FOR_WIRE);
        assert_eq!(screen.cells.len(), 1_155);
        let codec = MessagePackCodec::from_limits(FrameLimits::v1_default()).expect("codec");
        codec
            .encode(&TaskCockpitResult::Terminal(TaskTerminalProjection {
                task_id: TaskId::new(),
                terminal_id: crate::domain::TerminalId::new(),
                session_id: crate::terminal::protocol::TerminalSessionId::new(),
                agent_session_id: crate::domain::AgentSessionId::new(),
                resource_id: crate::domain::ResourceId::new(),
                runtime_generation: 1,
                resource_generation: 1,
                action_epoch: 1,
                focus_epoch: crate::terminal::protocol::FocusEpoch::initial(),
                accepted_input_sequence: 0,
                accepts_input_without_conversation_id: false,
                sequence: 1,
                title: Some("Codex".to_string()),
                text_lines,
                screen,
                is_provider: true,
                runtime_state: crate::domain::cockpit::TerminalRuntimeStateWire::Running,
            }))
            .expect("a normal styled provider screen must remain encodable");
    }

    #[test]
    fn oversized_styled_terminal_keeps_the_newest_prompt_region_within_wire_limits() {
        use crate::protocol::{FrameLimits, MessagePackCodec};
        use crate::terminal::session::{
            TerminalCellSnapshot, TerminalIndexedCellSnapshot, TerminalScreenSnapshot,
        };

        let styled = TerminalCellSnapshot {
            character: 'x',
            zero_width: Vec::new(),
            foreground: 0x00ff00,
            background: 0,
            bold: true,
            dim: false,
            italic: false,
            underline: false,
            undercurl: false,
            strike: false,
            hidden: false,
            has_hyperlink: false,
            default_background: true,
            default_foreground: false,
        };
        let rows = 48;
        let cols = 100;
        let lines = vec![vec![styled.clone(); cols]; rows];
        let cells = lines
            .iter()
            .enumerate()
            .flat_map(|(row, line)| {
                line.iter()
                    .cloned()
                    .enumerate()
                    .map(move |(column, cell)| TerminalIndexedCellSnapshot { row, column, cell })
            })
            .collect();
        let screen = TerminalScreenSnapshot {
            cells,
            lines,
            rows,
            cols,
            ..TerminalScreenSnapshot::default()
        };

        let connect_screen = screen.clone();
        let (screen, text_lines) =
            compact_terminal_screen_for_wire(screen, MAX_TERMINAL_STYLED_CELLS_FOR_WIRE);
        assert_eq!(screen.cells.len(), MAX_TERMINAL_STYLED_CELLS_FOR_WIRE);
        assert_eq!(screen.cells.first().map(|cell| cell.row), Some(18));
        assert_eq!(screen.cells.last().map(|cell| cell.row), Some(47));
        let (connect_screen, connect_lines) =
            compact_terminal_screen_for_wire(connect_screen, MAX_TERMINAL_CONNECT_STYLED_CELLS);
        assert_eq!(
            connect_screen.cells.len(),
            MAX_TERMINAL_CONNECT_STYLED_CELLS
        );
        assert_eq!(connect_screen.cells.last().map(|cell| cell.row), Some(47));
        assert_eq!(connect_lines.len(), rows);
        let projection = TaskTerminalProjection {
            task_id: TaskId::new(),
            terminal_id: crate::domain::TerminalId::new(),
            session_id: crate::terminal::protocol::TerminalSessionId::new(),
            agent_session_id: crate::domain::AgentSessionId::new(),
            resource_id: crate::domain::ResourceId::new(),
            runtime_generation: 1,
            resource_generation: 1,
            action_epoch: 1,
            focus_epoch: crate::terminal::protocol::FocusEpoch::initial(),
            accepted_input_sequence: 0,
            accepts_input_without_conversation_id: false,
            sequence: 1,
            title: Some("Codex".to_string()),
            text_lines,
            screen,
            is_provider: true,
            runtime_state: crate::domain::cockpit::TerminalRuntimeStateWire::Running,
        };
        let codec = MessagePackCodec::from_limits(FrameLimits::v1_default()).expect("codec");
        codec
            .encode(&TaskCockpitResult::Terminal(projection.clone()))
            .expect("an oversized styled provider screen must remain encodable");

        let request_id = RequestId::new();
        let bounded = fit_terminal_projection_for_wire(projection, request_id, 48 * 1024)
            .expect("connect carrier projection");
        assert_eq!(bounded.text_lines.len(), rows);
        assert!(bounded.screen.cells.len() < MAX_TERMINAL_STYLED_CELLS_FOR_WIRE);
        let reply = crate::domain::QueryReply {
            request_id,
            outcome: QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Terminal(
                bounded,
            ))),
        };
        assert!(rmp_serde::to_vec_named(&reply).unwrap().len() <= 46 * 1024);
    }

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
            provider_launch_hint: ProviderLaunchReadinessHint::Unknown,
            provider_restore_detail: None,
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
            provider_launch_hint: ProviderLaunchReadinessHint::Unknown,
            provider_restore_detail: None,
        });
        assert!(matches!(
            accepted,
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Unavailable {
                surface: TaskCockpitSurface::Ssh,
                reason: TaskCockpitUnavailableReason::WorkspaceAuthorityUnavailable,
                ..
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
            provider_launch_hint: ProviderLaunchReadinessHint::Unknown,
            provider_restore_detail: None,
        });
        assert!(matches!(
            named,
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Unavailable {
                surface: TaskCockpitSurface::Ssh,
                reason: TaskCockpitUnavailableReason::SshTaskSupervisorAdapterMissing,
                ..
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
            provider_launch_hint: ProviderLaunchReadinessHint::Unknown,
            provider_restore_detail: None,
        });
        assert!(matches!(
            foreign,
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Denied {
                surface: TaskCockpitSurface::Ssh,
                reason: TaskCockpitDeniedReason::Unauthorized,
                ..
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
            provider_launch_hint: ProviderLaunchReadinessHint::Unknown,
            provider_restore_detail: None,
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
            provider_launch_hint: ProviderLaunchReadinessHint::Unknown,
            provider_restore_detail: None,
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
            provider_launch_hint: ProviderLaunchReadinessHint::Unknown,
            provider_restore_detail: None,
        });
        assert!(matches!(
            secret,
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Denied {
                surface: TaskCockpitSurface::Files,
                reason: TaskCockpitDeniedReason::CapabilityDenied,
                ..
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
            provider_launch_hint: ProviderLaunchReadinessHint::Unknown,
            provider_restore_detail: None,
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
            provider_launch_hint: ProviderLaunchReadinessHint::Unknown,
            provider_restore_detail: None,
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
                provider_launch_hint: ProviderLaunchReadinessHint::Unknown,
                provider_restore_detail: None,
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
        assert_eq!(
            second.encoded_bytes as usize,
            rmp_serde::to_vec_named(&second).unwrap().len()
        );

        let reset = query_page(900);
        assert!(reset.cursor_rolled_over);
        assert_eq!(reset.after_sequence, 0);
        assert_eq!(reset.oldest_sequence, 1);
        assert_eq!(reset.facts.first().unwrap().sequence, 1);
        assert_eq!(reset.next_sequence, Some(128));
        assert_eq!(
            reset.encoded_bytes as usize,
            rmp_serde::to_vec_named(&reset).unwrap().len()
        );
    }

    #[test]
    fn conversation_query_pages_fit_the_encrypted_connect_carrier() {
        use crate::remote::presentation::{
            SemanticEventDraft, SemanticEventKind, SemanticJournalStore, SemanticRetention,
            SemanticSource, StableSessionKey,
        };

        let (_repository, bus, client_id, task_id, _) = create_bound_task();
        let journal = std::sync::Mutex::new(SemanticJournalStore::default());
        let key = StableSessionKey::from_tab(task_id.to_string());
        for index in 0..40 {
            journal.lock().unwrap().record(SemanticEventDraft {
                stable_session_key: key.clone(),
                occurred_at_epoch_ms: index,
                source: SemanticSource::Claude,
                kind: SemanticEventKind::UserMessage {
                    text: format!("message {index}: {}", "x".repeat(4 * 1024)),
                },
                retention: SemanticRetention::Canonical,
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
            provider_launch_hint: ProviderLaunchReadinessHint::Unknown,
            provider_restore_detail: None,
        });
        let QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Conversation(page))) =
            outcome
        else {
            panic!("conversation page expected")
        };

        assert!(page.encoded_bytes as usize <= MAX_CONVERSATION_PAGE_BYTES);
        assert_eq!(
            page.encoded_bytes as usize,
            rmp_serde::to_vec_named(&page).unwrap().len()
        );
        assert!(page.next_sequence.is_some(), "large history must paginate");
    }

    #[test]
    fn conversation_incremental_upserts_preserve_original_message_identity() {
        use crate::remote::presentation::{
            SemanticEventDraft, SemanticEventKind, SemanticJournalStore, SemanticRetention,
            SemanticSource, StableSessionKey,
        };
        let (_repository, bus, client_id, task_id, _) = create_bound_task();
        let journal = std::sync::Mutex::new(SemanticJournalStore::default());
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
                provider_launch_hint: ProviderLaunchReadinessHint::Unknown,
                provider_restore_detail: None,
            });
            let QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Conversation(page))) =
                outcome
            else {
                panic!("conversation page expected")
            };
            page
        };
        let mut first_id = None;
        for (index, text) in ["Hel", "Hello", "Hello world"].into_iter().enumerate() {
            journal.lock().unwrap().record(SemanticEventDraft {
                stable_session_key: StableSessionKey::from_tab(task_id.to_string()),
                occurred_at_epoch_ms: index as u64 + 1,
                source: SemanticSource::Claude,
                kind: SemanticEventKind::UserMessage { text: text.into() },
                retention: SemanticRetention::Canonical,
                deduplication_key: Some("stable-message".into()),
            });
            let page = query_page(index as u64);
            assert_eq!(page.facts.len(), 1);
            let id = page.facts[0].id;
            assert_eq!(id, *first_id.get_or_insert(id));
            assert_eq!(page.facts[0].sequence, index as u64 + 1);
        }
        let page = query_page(0);
        assert_eq!(page.facts.len(), 1);
        assert_eq!(Some(page.facts[0].id), first_id);
    }

    #[test]
    fn provider_input_state_is_exact_primary_agent_and_capability_gated() {
        use crate::domain::agent::{AgentRole, AgentSessionFacts, ProviderSessionId};
        use crate::domain::cockpit::ProviderInputStateProjection;
        use crate::domain::provider_input::ProviderSessionProjection;
        use crate::domain::resource::{OwnerKind, ResourceFacts, ResourceKind, ResourceRecipe};
        use crate::providers::ProviderKind;

        let (_repository, bus, client_id, task_id, _roots) = create_bound_task();
        let mut snapshot = bus.task_snapshot(task_id).unwrap().unwrap();
        let empty = ProviderInputStateProjection::from_snapshot(&snapshot);
        assert_eq!(empty.task_id, task_id);
        assert!(empty.agent_session_id.is_none());
        let mut agent = AgentSessionFacts::new(
            task_id,
            AgentRole::Primary,
            ProviderKind::ClaudeCode,
            Some(ProviderSessionId::new("exact-provider-conversation").unwrap()),
        )
        .unwrap();
        agent.runtime_generation = 7;
        snapshot.primary_agent_id = Some(agent.id);
        snapshot.agents.insert(agent.id, agent.clone());
        let mut resource = ResourceFacts::new(
            Some(task_id),
            OwnerKind::Task,
            ResourceKind::Terminal,
            ResourceRecipe::terminal(120, 40),
            1,
        )
        .unwrap();
        resource.runtime_generation = 7;
        snapshot.resources.insert(resource.id, resource.clone());
        let question = crate::domain::QuestionId::new();
        snapshot.provider_sessions.insert(
            agent.id,
            ProviderSessionProjection {
                open_question: Some(question),
                ..Default::default()
            },
        );
        let state = ProviderInputStateProjection::from_snapshot(&snapshot);
        assert_eq!(state.runtime_generation, Some(7));
        assert_eq!(state.agent_session_id, Some(agent.id));
        assert_eq!(state.resource_id, Some(resource.id));
        assert_eq!(state.provider_session_id, agent.provider_session_id);
        assert_eq!(state.open_question, Some(question));
        // Even a malformed foreign primary reference cannot grant input authority.
        snapshot.agents.get_mut(&agent.id).unwrap().task_id = TaskId::new();
        assert!(ProviderInputStateProjection::from_snapshot(&snapshot)
            .agent_session_id
            .is_none());

        for allowed in [false, true] {
            let caps = if allowed {
                CapabilitySet::from_capabilities([
                    Capability::TaskCockpit,
                    Capability::ProviderInput,
                ])
            } else {
                CapabilitySet::from_capabilities([Capability::TaskCockpit])
            };
            let outcome = serve_task_cockpit(TaskCockpitDispatch {
                capabilities: caps,
                envelope_task_id: Some(task_id),
                client_id,
                connection_id: Uuid::now_v7(),
                request_id: RequestId::new(),
                query: &TaskCockpitQuery::ProviderInputState,
                bus: &bus,
                service_runtime: None,
                semantic_journal: None,
                terminal_service: None,
                ssh_endpoints: None,
                ssh_runtime: None,
                workspace_projects: None,
                coordinator: None,
                action_epoch: None,
                runtime_generation: None,
                config: None,
                provider_launch_hint: ProviderLaunchReadinessHint::Unknown,
                provider_restore_detail: None,
            });
            if allowed {
                assert!(matches!(
                    outcome,
                    QueryOutcome::Ok(QueryResult::TaskCockpit(
                        TaskCockpitResult::ProviderInputState(_)
                    ))
                ));
            } else {
                assert!(matches!(
                    outcome,
                    QueryOutcome::Err(QueryError::UnsupportedCapability)
                ));
            }
        }
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
            provider_launch_hint: ProviderLaunchReadinessHint::Unknown,
            provider_restore_detail: None,
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
            provider_launch_hint: ProviderLaunchReadinessHint::Unknown,
            provider_restore_detail: None,
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
                ..
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
                ..
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
                ..
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
                ..
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
                ..
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
                ..
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
            recipe: ResourceRecipe::terminal(120, 40),
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
            body.contains("typed_queries::query_task_cockpit(self, task_id, query)"),
            "Task Cockpit must use the typed request path whose timeout is verified by cockpit_and_agent_keep_custom_timeouts"
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
    fn workspace_targeted_git_status_survives_external_sibling_worktree() {
        let (repository, bus, client_id, task_id, roots) = create_bound_task();
        // create_bound_task leaves an unborn HEAD; seed one commit so
        // `git worktree add --detach` has a valid reference (local to this test).
        std::fs::write(repository.path().join("seed.txt"), "seed\n").expect("seed file");
        let add = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=Cockpit Test",
                "-c",
                "user.email=cockpit@example.invalid",
            ])
            .args(["add", "seed.txt"])
            .current_dir(repository.path())
            .output()
            .expect("git add seed");
        assert!(
            add.status.success(),
            "{}",
            String::from_utf8_lossy(&add.stderr)
        );
        let commit = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=Cockpit Test",
                "-c",
                "user.email=cockpit@example.invalid",
            ])
            .args(["commit", "-m", "seed"])
            .current_dir(repository.path())
            .output()
            .expect("git commit seed");
        assert!(
            commit.status.success(),
            "{}",
            String::from_utf8_lossy(&commit.stderr)
        );
        let sibling_root = Path::new(r"C:\Temp");
        fs::create_dir_all(sibling_root).expect("sibling temp root");
        let sibling_parent = tempfile::Builder::new()
            .prefix("devmanager-cockpit-sibling-wt-")
            .tempdir_in(sibling_root)
            .expect("sibling parent");
        let sibling = sibling_parent.path().join("sibling");
        let added = std::process::Command::new("git")
            .args([
                "worktree",
                "add",
                "--detach",
                sibling.to_str().expect("sibling path"),
            ])
            .current_dir(repository.path())
            .output()
            .expect("add external sibling worktree");
        assert!(
            added.status.success(),
            "git worktree add failed: {}",
            String::from_utf8_lossy(&added.stderr)
        );
        fs::write(repository.path().join("cockpit-change.txt"), "visible\n")
            .expect("write current worktree change");

        let coordinator = WorkspaceResourceCoordinator::new();
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
        let QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Git(status))) = targeted
        else {
            panic!(
                "Workspace GitStatusTargeted must succeed with an external sibling: {targeted:?}"
            );
        };
        assert_eq!(status.selector, Some(TaskRepositorySelector::Workspace));
        assert!(
            status.change_count > 0,
            "current worktree changes must populate without authorizing the sibling backlink"
        );
        assert!(
            status
                .entries
                .iter()
                .any(|entry| entry.relative_path == "cockpit-change.txt"),
            "the native Git window must receive bounded repository-relative status rows"
        );
        assert!(
            status.entries.iter().all(|entry| !entry
                .relative_path
                .contains(repository.path().to_string_lossy().as_ref())),
            "Git status rows must never disclose the authorized repository root"
        );
        assert!(
            !sibling.join("cockpit-change.txt").exists(),
            "status must not mutate or require the sibling checkout"
        );

        let file_diff = serve_task_cockpit(dispatch(
            &bus,
            client_id,
            task_id,
            &TaskCockpitQuery::GitFileDiffTargeted {
                selector: TaskRepositorySelector::Workspace,
                relative_path: "cockpit-change.txt".into(),
                staged: false,
            },
            Some(&roots),
            Some(&coordinator),
            Some(1),
            Some(1),
        ));
        let QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::GitFileDiff(file_diff))) =
            file_diff
        else {
            panic!("targeted file diff failed: {file_diff:?}");
        };
        assert_eq!(file_diff.relative_path, "cockpit-change.txt");
        assert!(file_diff.diff.hunks.iter().any(|hunk| {
            hunk.lines.iter().any(|line| {
                line.kind == crate::git::git_service::DiffLineKind::Add && line.content == "visible"
            })
        }));

        let history = serve_task_cockpit(dispatch(
            &bus,
            client_id,
            task_id,
            &TaskCockpitQuery::GitHistoryTargeted {
                selector: TaskRepositorySelector::Workspace,
                limit: 25,
                skip: 0,
            },
            Some(&roots),
            Some(&coordinator),
            Some(1),
            Some(1),
        ));
        let QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::GitHistory(history))) =
            history
        else {
            panic!("targeted Git history failed: {history:?}");
        };
        let seed = history.entries.first().expect("seed history entry");
        assert_eq!(seed.subject, "seed");

        let commit_diff = serve_task_cockpit(dispatch(
            &bus,
            client_id,
            task_id,
            &TaskCockpitQuery::GitCommitDiffTargeted {
                selector: TaskRepositorySelector::Workspace,
                commit_hash: seed.full_hash.clone(),
            },
            Some(&roots),
            Some(&coordinator),
            Some(1),
            Some(1),
        ));
        let QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::GitCommitDiff(
            commit_diff,
        ))) = commit_diff
        else {
            panic!("targeted commit diff failed: {commit_diff:?}");
        };
        assert!(commit_diff.diff.hunks.iter().any(|hunk| {
            hunk.lines.iter().any(|line| {
                line.kind == crate::git::git_service::DiffLineKind::Add && line.content == "seed"
            })
        }));
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

    fn create_unstarted_draft_with_terminal_claim() -> (
        tempfile::TempDir,
        CommandBus,
        ClientId,
        TaskId,
        WorkspaceProjectRoots,
        crate::domain::AgentSessionId,
        crate::domain::ResourceId,
    ) {
        use crate::domain::agent::{AgentRole, AgentSessionFacts};
        use crate::domain::resource::{OwnerKind, ResourceFacts, ResourceKind, ResourceRecipe};
        use crate::providers::ProviderKind;

        let (repository, mut bus, client_id, task_id, roots) = create_bound_task();
        let mut revision = bus
            .task_snapshot(task_id)
            .expect("lookup")
            .expect("task")
            .task
            .revision;
        let mut agent =
            AgentSessionFacts::new(task_id, AgentRole::Primary, ProviderKind::Codex, None)
                .expect("agent");
        agent.runtime_generation = 1;
        let agent_id = agent.id;
        let receipt = bus
            .execute(CommandEnvelope {
                command_id: CommandId::new(),
                client_id,
                task_id: Some(task_id),
                issued_at_ms: 2,
                expected_task_revision: Some(revision),
                command: Command::RegisterAgentSession { agent },
            })
            .expect("register agent");
        let CommandReceipt::Accepted {
            task_revision: Some(next),
            ..
        } = receipt
        else {
            panic!("register agent: {receipt:?}");
        };
        revision = next;
        let mut resource = ResourceFacts::new(
            Some(task_id),
            OwnerKind::Task,
            ResourceKind::Terminal,
            ResourceRecipe::terminal(120, 40),
            2,
        )
        .expect("resource");
        resource.runtime_generation = 1;
        let resource_id = resource.id;
        let receipt = bus
            .execute(CommandEnvelope {
                command_id: CommandId::new(),
                client_id,
                task_id: Some(task_id),
                issued_at_ms: 3,
                expected_task_revision: Some(revision),
                command: Command::RegisterResource { resource },
            })
            .expect("register resource");
        let CommandReceipt::Accepted {
            task_revision: Some(next),
            ..
        } = receipt
        else {
            panic!("register resource: {receipt:?}");
        };
        revision = next;
        let receipt = bus
            .execute(CommandEnvelope {
                command_id: CommandId::new(),
                client_id,
                task_id: Some(task_id),
                issued_at_ms: 4,
                expected_task_revision: Some(revision),
                command: Command::SetPrimaryAgent {
                    agent_session_id: agent_id,
                },
            })
            .expect("set primary");
        assert!(
            matches!(receipt, CommandReceipt::Accepted { .. }),
            "{receipt:?}"
        );
        (
            repository,
            bus,
            client_id,
            task_id,
            roots,
            agent_id,
            resource_id,
        )
    }

    /// One plain shell registered on `task_id`, already carrying the runtime
    /// generation the host requires before it will spawn anything for it.
    /// `Command::OpenShellTerminal` is host-authority-only: the launch recipe
    /// is the host's to choose, so these fixtures execute it the way the host
    /// executor does rather than through the client `execute` path.
    fn host_open_shell(
        bus: &mut crate::kernel::CommandBus,
        envelope: CommandEnvelope,
    ) -> Result<CommandReceipt, crate::kernel::StoreError> {
        bus.execute_host_authorized(
            envelope,
            None,
            crate::domain::RequestId::new(),
            uuid::Uuid::now_v7(),
        )
    }

    fn plain_shell_resource(task_id: TaskId, program: &str) -> crate::domain::ResourceFacts {
        use crate::domain::resource::{
            OwnerKind, ResourceFacts, ResourceKind, ResourceRecipe, TerminalLaunch,
        };
        let mut resource = ResourceFacts::new(
            Some(task_id),
            OwnerKind::Task,
            ResourceKind::Terminal,
            ResourceRecipe::Terminal {
                cols: 80,
                rows: 24,
                launch: Some(TerminalLaunch {
                    cwd: std::path::PathBuf::from(SHELL_CWD),
                    program: std::path::PathBuf::from(program),
                    args: Vec::new(),
                }),
                title: None,
            },
            5,
        )
        .expect("plain shell resource");
        resource.runtime_generation = 1;
        resource
    }

    const SHELL_CWD: &str = if cfg!(windows) { "C:/Code" } else { "/code" };
    const SHELL_A_PROGRAM: &str = if cfg!(windows) {
        "C:/Windows/System32/WindowsPowerShell/v1.0/powershell.exe"
    } else {
        "/bin/bash"
    };
    const SHELL_B_PROGRAM: &str = if cfg!(windows) {
        "C:/Windows/System32/cmd.exe"
    } else {
        "/bin/sh"
    };

    fn shell_terminal_spec() -> crate::terminal::protocol::TerminalSpec {
        crate::terminal::protocol::TerminalSpec::new(
            crate::terminal::protocol::TerminalSessionId::new(),
            crate::terminal::protocol::TerminalSize::new(80, 24).expect("size"),
        )
        .expect("spec")
    }

    /// A task carrying its provider terminal plus two attached plain shells,
    /// with the strip deliberately ordered against registration order so a
    /// projection that echoed registration order would fail.
    #[allow(clippy::type_complexity)]
    fn task_with_provider_and_two_shells() -> (
        tempfile::TempDir,
        CommandBus,
        ClientId,
        TaskId,
        WorkspaceProjectRoots,
        crate::terminal::service::TerminalService,
        crate::domain::ResourceId,
        crate::domain::ResourceId,
        crate::domain::ResourceId,
    ) {
        use crate::domain::command::OpenShellTerminalIntent;
        use crate::terminal::service::{MockAttachedRuntime, TerminalService};

        let (repository, mut bus, client_id, task_id, roots, agent_id, provider_resource) =
            create_unstarted_draft_with_terminal_claim();
        let mut revision = bus
            .task_snapshot(task_id)
            .expect("lookup")
            .expect("task")
            .task
            .revision;
        let shell_a = plain_shell_resource(task_id, SHELL_A_PROGRAM);
        let shell_b = plain_shell_resource(task_id, SHELL_B_PROGRAM);
        for shell in [shell_a.clone(), shell_b.clone()] {
            let receipt = host_open_shell(
                &mut bus,
                CommandEnvelope {
                    command_id: CommandId::new(),
                    client_id,
                    task_id: Some(task_id),
                    issued_at_ms: 6,
                    expected_task_revision: Some(revision),
                    command: Command::OpenShellTerminal(OpenShellTerminalIntent {
                        resource: shell,
                    }),
                },
            )
            .expect("open shell");
            let CommandReceipt::Accepted {
                task_revision: Some(next),
                ..
            } = receipt
            else {
                panic!("open shell: {receipt:?}");
            };
            revision = next;
        }
        let receipt = bus
            .execute(CommandEnvelope {
                command_id: CommandId::new(),
                client_id,
                task_id: Some(task_id),
                issued_at_ms: 7,
                expected_task_revision: Some(revision),
                command: Command::SetTerminalStrip(
                    crate::domain::terminal_facts::TaskTerminalStrip {
                        order: vec![shell_b.id, shell_a.id],
                        focused: Some(shell_a.id),
                    },
                ),
            })
            .expect("set strip");
        assert!(
            matches!(receipt, CommandReceipt::Accepted { .. }),
            "{receipt:?}"
        );

        let size = crate::terminal::protocol::TerminalSize::new(80, 24).expect("size");
        let terminals = TerminalService::new();
        terminals
            .attach_bound_task_runtime(
                task_id,
                shell_terminal_spec(),
                // The hosted terminal takes its resource identity from the
                // runtime's own attachment fence, so the mock has to present
                // the task's durable provider resource.
                MockAttachedRuntime::with_resource_fence(size, provider_resource),
                agent_id,
                1,
                1,
            )
            .expect("provider attach");
        for shell in [shell_a.id, shell_b.id] {
            terminals
                .attach_plain_shell(
                    task_id,
                    shell,
                    1,
                    shell_terminal_spec(),
                    MockAttachedRuntime::with_resource_fence(size, shell),
                )
                .expect("shell attach");
        }
        (
            repository,
            bus,
            client_id,
            task_id,
            roots,
            terminals,
            provider_resource,
            shell_a.id,
            shell_b.id,
        )
    }

    #[test]
    fn task_terminals_query_lists_provider_first_then_strip_order() {
        let (
            _repository,
            bus,
            client_id,
            task_id,
            roots,
            terminals,
            provider_resource,
            shell_a,
            shell_b,
        ) = task_with_provider_and_two_shells();

        let outcome = serve_task_cockpit(TaskCockpitDispatch {
            capabilities: CapabilitySet::from_capabilities([Capability::TaskCockpit]),
            envelope_task_id: Some(task_id),
            client_id,
            connection_id: Uuid::now_v7(),
            request_id: RequestId::new(),
            query: &TaskCockpitQuery::TaskTerminals,
            bus: &bus,
            service_runtime: None,
            semantic_journal: None,
            terminal_service: Some(&terminals),
            ssh_endpoints: None,
            ssh_runtime: None,
            workspace_projects: Some(&roots),
            coordinator: None,
            action_epoch: None,
            runtime_generation: None,
            config: None,
            provider_launch_hint: ProviderLaunchReadinessHint::Unknown,
            provider_restore_detail: None,
        });
        let QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::TaskTerminals(
            projection,
        ))) = outcome
        else {
            panic!("expected the terminal strip, got {outcome:?}");
        };
        assert_eq!(projection.task_id, task_id);
        assert_eq!(projection.terminals.len(), 3);
        assert!(projection.terminals[0].is_provider);
        assert_eq!(projection.terminals[0].resource_id, provider_resource);
        // The strip order is durable, not registration order.
        assert_eq!(projection.terminals[1].resource_id, shell_b);
        assert_eq!(projection.terminals[2].resource_id, shell_a);
        assert!(!projection.terminals[1].is_provider);
        assert_eq!(projection.order, vec![shell_b, shell_a]);
        assert_eq!(projection.focused, Some(shell_a));
        assert_eq!(
            projection.terminals[0].runtime_state,
            TerminalRuntimeStateWire::Running
        );
        // ResourceRegistered writes TerminalFacts for every Terminal resource,
        // the provider's included -- only the strip push is gated on
        // `is_plain_shell`. So the provider chip's timestamps are real facts,
        // not the zero a chip built without them would carry.
        let durable = bus
            .task_snapshot(task_id)
            .expect("lookup")
            .expect("task")
            .terminal_facts
            .get(&provider_resource)
            .cloned()
            .expect("the provider terminal has durable facts");
        assert!(durable.created_at_ms > 0, "{durable:?}");
        assert_eq!(projection.terminals[0].created_at_ms, durable.created_at_ms);
        assert_eq!(
            projection.terminals[0].last_activity_at_ms,
            durable.last_activity_at_ms
        );
        // Label is the launch program's file stem; the provider slot has none.
        assert_eq!(projection.terminals[0].label, "terminal");
        assert_eq!(
            projection.terminals[1].label,
            if cfg!(windows) { "cmd" } else { "sh" }
        );
        assert_eq!(
            projection.terminals[2].label,
            if cfg!(windows) { "powershell" } else { "bash" }
        );
    }

    #[test]
    fn task_terminals_renders_a_retired_shell_from_durable_facts() {
        let (_repository, bus, client_id, task_id, roots, terminals, _provider, shell_a, _shell_b) =
            task_with_provider_and_two_shells();
        // Close semantics: the hosted entry is closed and retired before the
        // durable resource is released, so the chip must survive on facts.
        let terminal_id = terminals
            .shell_terminal_id(shell_a)
            .expect("lookup")
            .expect("hosted shell");
        terminals
            .close(
                terminal_id,
                crate::terminal::protocol::CloseReason::ExplicitServiceClose,
            )
            .expect("close shell");
        assert!(
            terminals.remove_closed(shell_a).expect("retire"),
            "closed entry retires"
        );
        assert!(
            !terminals.remove_closed(shell_a).expect("retire again"),
            "retiring twice is a no-op"
        );

        let outcome = serve_task_cockpit(TaskCockpitDispatch {
            capabilities: CapabilitySet::from_capabilities([Capability::TaskCockpit]),
            envelope_task_id: Some(task_id),
            client_id,
            connection_id: Uuid::now_v7(),
            request_id: RequestId::new(),
            query: &TaskCockpitQuery::TaskTerminals,
            bus: &bus,
            service_runtime: None,
            semantic_journal: None,
            terminal_service: Some(&terminals),
            ssh_endpoints: None,
            ssh_runtime: None,
            workspace_projects: Some(&roots),
            coordinator: None,
            action_epoch: None,
            runtime_generation: None,
            config: None,
            provider_launch_hint: ProviderLaunchReadinessHint::Unknown,
            provider_restore_detail: None,
        });
        let QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::TaskTerminals(
            projection,
        ))) = outcome
        else {
            panic!("expected the terminal strip, got {outcome:?}");
        };
        assert_eq!(projection.terminals.len(), 3);
        let retired = projection
            .terminals
            .iter()
            .find(|chip| chip.resource_id == shell_a)
            .expect("retired shell still in the strip");
        // No hosted entry and no durable exit fact is genuinely unknown; it
        // must never read as a running terminal.
        assert_eq!(retired.runtime_state, TerminalRuntimeStateWire::Unknown);
    }

    #[test]
    fn task_terminals_reports_a_recorded_exit_after_the_hosted_entry_is_gone() {
        use crate::domain::terminal_facts::HostTerminalFact;

        let (_repository, mut bus, client_id, task_id, roots, terminals, _provider, shell_a, _b) =
            task_with_provider_and_two_shells();
        let terminal_id = terminals
            .shell_terminal_id(shell_a)
            .expect("lookup")
            .expect("hosted shell");
        terminals
            .close(
                terminal_id,
                crate::terminal::protocol::CloseReason::ExplicitServiceClose,
            )
            .expect("close shell");
        assert!(terminals.remove_closed(shell_a).expect("retire"));
        // The host fact pump records the exit durably; the hosted entry that
        // could have answered "Exited" is already gone by then.
        let outcome = bus
            .record_terminal_fact(
                task_id,
                shell_a,
                HostTerminalFact::Exit {
                    code: Some(3),
                    summary: "exit 3".to_string(),
                },
                9_000,
            )
            .expect("record exit");
        assert!(
            matches!(outcome, crate::kernel::TerminalFactOutcome::Recorded),
            "{outcome:?}"
        );

        let served = serve_task_cockpit(TaskCockpitDispatch {
            capabilities: CapabilitySet::from_capabilities([Capability::TaskCockpit]),
            envelope_task_id: Some(task_id),
            client_id,
            connection_id: Uuid::now_v7(),
            request_id: RequestId::new(),
            query: &TaskCockpitQuery::TaskTerminals,
            bus: &bus,
            service_runtime: None,
            semantic_journal: None,
            terminal_service: Some(&terminals),
            ssh_endpoints: None,
            ssh_runtime: None,
            workspace_projects: Some(&roots),
            coordinator: None,
            action_epoch: None,
            runtime_generation: None,
            config: None,
            provider_launch_hint: ProviderLaunchReadinessHint::Unknown,
            provider_restore_detail: None,
        });
        let QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::TaskTerminals(
            projection,
        ))) = served
        else {
            panic!("expected the terminal strip, got {served:?}");
        };
        let chip = projection
            .terminals
            .iter()
            .find(|chip| chip.resource_id == shell_a)
            .expect("exited shell still in the strip");
        assert_eq!(
            chip.runtime_state,
            TerminalRuntimeStateWire::Exited {
                summary: "exit 3".to_string()
            }
        );
        assert_eq!(
            chip.exit.as_ref().map(|exit| exit.code),
            Some(Some(3)),
            "{chip:?}"
        );
    }

    #[test]
    fn task_terminals_redacts_the_shell_cwd_against_the_workspace_root() {
        use crate::domain::terminal_facts::HostTerminalFact;

        let (
            _repository,
            mut bus,
            client_id,
            task_id,
            roots,
            terminals,
            _provider,
            shell_a,
            shell_b,
        ) = task_with_provider_and_two_shells();
        let root = roots
            .root_for(
                bus.task_snapshot(task_id)
                    .expect("lookup")
                    .expect("task")
                    .task
                    .project_id,
            )
            .expect("workspace root")
            .to_path_buf();
        // One shell inside the workspace, one deliberately outside it.
        let inside = root.join("crates").join("api");
        std::fs::create_dir_all(&inside).expect("inside dir");
        let outside = std::env::temp_dir().join("devmanager-cwd-redaction-probe");
        std::fs::create_dir_all(&outside).expect("outside dir");
        for (resource_id, cwd) in [(shell_a, inside.clone()), (shell_b, outside.clone())] {
            let outcome = bus
                .record_terminal_fact(task_id, resource_id, HostTerminalFact::Cwd(cwd), 9_100)
                .expect("record cwd");
            assert!(
                matches!(outcome, crate::kernel::TerminalFactOutcome::Recorded),
                "{outcome:?}"
            );
        }

        let served = serve_task_cockpit(TaskCockpitDispatch {
            capabilities: CapabilitySet::from_capabilities([Capability::TaskCockpit]),
            envelope_task_id: Some(task_id),
            client_id,
            connection_id: Uuid::now_v7(),
            request_id: RequestId::new(),
            query: &TaskCockpitQuery::TaskTerminals,
            bus: &bus,
            service_runtime: None,
            semantic_journal: None,
            terminal_service: Some(&terminals),
            ssh_endpoints: None,
            ssh_runtime: None,
            workspace_projects: Some(&roots),
            coordinator: None,
            action_epoch: None,
            runtime_generation: None,
            config: None,
            provider_launch_hint: ProviderLaunchReadinessHint::Unknown,
            provider_restore_detail: None,
        });
        let QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::TaskTerminals(
            projection,
        ))) = served
        else {
            panic!("expected the terminal strip, got {served:?}");
        };
        let cwd_of = |resource_id| {
            projection
                .terminals
                .iter()
                .find(|chip| chip.resource_id == resource_id)
                .and_then(|chip| chip.live_cwd.clone())
                .expect("chip carries a cwd")
        };
        assert_eq!(cwd_of(shell_a), "crates/api");
        assert_eq!(
            cwd_of(shell_b),
            outside
                .file_name()
                .expect("final component")
                .to_string_lossy()
        );
        // The contract is that no absolute host path reaches the wire at all.
        let root_text = root.to_string_lossy().to_string();
        for chip in &projection.terminals {
            if let Some(cwd) = &chip.live_cwd {
                assert!(!cwd.contains(&root_text), "leaked workspace root: {cwd}");
                assert!(
                    !std::path::Path::new(cwd).is_absolute(),
                    "leaked absolute path: {cwd}"
                );
            }
        }
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn terminal_for_serves_a_shell_without_provider_input_capability() {
        let (_repository, bus, client_id, task_id, roots, terminals, _provider, shell_a, _shell_b) =
            task_with_provider_and_two_shells();

        let dispatch_shell = |query: &TaskCockpitQuery| {
            serve_task_cockpit(TaskCockpitDispatch {
                // Deliberately no ProviderInput: a shell is not the provider's
                // input surface, so it must not be gated on that capability.
                capabilities: CapabilitySet::from_capabilities([Capability::TaskCockpit]),
                envelope_task_id: Some(task_id),
                client_id,
                connection_id: Uuid::now_v7(),
                request_id: RequestId::new(),
                query,
                bus: &bus,
                service_runtime: None,
                semantic_journal: None,
                terminal_service: Some(&terminals),
                ssh_endpoints: None,
                ssh_runtime: None,
                workspace_projects: Some(&roots),
                coordinator: None,
                action_epoch: None,
                runtime_generation: None,
                config: None,
                provider_launch_hint: ProviderLaunchReadinessHint::Unknown,
                provider_restore_detail: None,
            })
        };

        let outcome = dispatch_shell(&TaskCockpitQuery::TerminalFor {
            resource_id: shell_a,
        });
        let QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Terminal(projection))) =
            outcome
        else {
            panic!("expected a shell terminal screen, got {outcome:?}");
        };
        assert!(!projection.is_provider);
        assert_eq!(projection.resource_id, shell_a);
        assert!(projection.agent_session_id.is_nil());
        assert_eq!(projection.runtime_generation, 0);
        assert_eq!(projection.resource_generation, 1);
        assert!(!projection.accepts_input_without_conversation_id);
        assert_eq!(projection.runtime_state, TerminalRuntimeStateWire::Running);

        // Scroll and resize address the same shell, and leave the provider
        // terminal's own grid untouched.
        assert!(matches!(
            dispatch_shell(&TaskCockpitQuery::TerminalResizeFor {
                resource_id: shell_a,
                cols: 100,
                rows: 30,
            }),
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Terminal(_)))
        ));
        let resized = terminals
            .task_terminal_view_for(task_id, Some(shell_a))
            .expect("view")
            .expect("present");
        assert_eq!(resized.view.screen.cols, 100);
        let provider = terminals
            .task_terminal_view_for(task_id, None)
            .expect("view")
            .expect("present");
        assert_eq!(provider.view.screen.cols, 80);
        assert!(matches!(
            dispatch_shell(&TaskCockpitQuery::TerminalScrollFor {
                resource_id: shell_a,
                delta_lines: 2,
            }),
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Terminal(_)))
        ));

        // An unknown resource is absence, never the provider terminal.
        let unknown = dispatch_shell(&TaskCockpitQuery::TerminalFor {
            resource_id: crate::domain::ResourceId::new(),
        });
        assert!(
            matches!(
                unknown,
                QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Unavailable {
                    surface: TaskCockpitSurface::Terminal,
                    reason: TaskCockpitUnavailableReason::TerminalUnavailable,
                    ..
                }))
            ),
            "{unknown:?}"
        );
    }

    /// Scroll and resize reach the live PTY the instant they are called, so the
    /// dispatcher must authorize before it acts. It used to apply them first
    /// and fence afterwards, and `TerminalScrollFor`/`TerminalResizeFor` select
    /// by durable resource -- including the PROVIDER's resource. A Connect
    /// client holding only TaskCockpit could therefore resize the local user's
    /// provider PTY and only then be told CapabilityDenied.
    #[test]
    fn resource_addressed_viewport_ops_are_fenced_before_they_touch_the_terminal() {
        let (
            _repository,
            bus,
            client_id,
            task_id,
            roots,
            terminals,
            provider_resource,
            shell_a,
            _shell_b,
        ) = task_with_provider_and_two_shells();

        let dispatch_with = |capabilities: CapabilitySet, query: &TaskCockpitQuery| {
            serve_task_cockpit(TaskCockpitDispatch {
                capabilities,
                envelope_task_id: Some(task_id),
                client_id,
                connection_id: Uuid::now_v7(),
                request_id: RequestId::new(),
                query,
                bus: &bus,
                service_runtime: None,
                semantic_journal: None,
                terminal_service: Some(&terminals),
                ssh_endpoints: None,
                ssh_runtime: None,
                workspace_projects: Some(&roots),
                coordinator: None,
                action_epoch: None,
                runtime_generation: None,
                config: None,
                provider_launch_hint: ProviderLaunchReadinessHint::Unknown,
                provider_restore_detail: None,
            })
        };
        let provider_cols = || {
            terminals
                .task_terminal_view_for(task_id, None)
                .expect("view")
                .expect("present")
                .view
                .screen
                .cols
        };
        let read_only = CapabilitySet::from_capabilities([Capability::TaskCockpit]);
        let before = provider_cols();
        assert_eq!(before, 80);

        // Addressing the provider's own resource does not turn a provider
        // terminal into a shell: the capability gate still applies.
        for query in [
            TaskCockpitQuery::TerminalResizeFor {
                resource_id: provider_resource,
                cols: 200,
                rows: 50,
            },
            TaskCockpitQuery::TerminalScrollFor {
                resource_id: provider_resource,
                delta_lines: 5,
            },
            TaskCockpitQuery::TerminalFor {
                resource_id: provider_resource,
            },
        ] {
            let outcome = dispatch_with(read_only, &query);
            assert!(
                matches!(
                    outcome,
                    QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Denied {
                        surface: TaskCockpitSurface::Terminal,
                        reason: TaskCockpitDeniedReason::CapabilityDenied,
                        ..
                    }))
                ),
                "{query:?} must be denied for a TaskCockpit-only client, got {outcome:?}"
            );
            assert_eq!(
                provider_cols(),
                before,
                "{query:?} must not have touched the provider terminal"
            );
        }

        // The same legacy shape is fenced the same way, and equally must not
        // have resized anything on its way to the denial.
        let legacy = dispatch_with(
            read_only,
            &TaskCockpitQuery::TerminalResize {
                cols: 200,
                rows: 50,
            },
        );
        assert!(
            matches!(
                legacy,
                QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Denied {
                    surface: TaskCockpitSurface::Terminal,
                    reason: TaskCockpitDeniedReason::CapabilityDenied,
                    ..
                }))
            ),
            "{legacy:?}"
        );
        assert_eq!(provider_cols(), before);

        // The same client may resize its own shell: a shell is not the
        // provider's input surface and never required ProviderInput.
        let shell = dispatch_with(
            read_only,
            &TaskCockpitQuery::TerminalResizeFor {
                resource_id: shell_a,
                cols: 132,
                rows: 43,
            },
        );
        assert!(
            matches!(
                shell,
                QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Terminal(_)))
            ),
            "{shell:?}"
        );
        assert_eq!(
            terminals
                .task_terminal_view_for(task_id, Some(shell_a))
                .expect("view")
                .expect("present")
                .view
                .screen
                .cols,
            132
        );
        assert_eq!(provider_cols(), before, "the shell resize is scoped");

        // And a client that does hold ProviderInput still resizes the provider.
        let granted =
            CapabilitySet::from_capabilities([Capability::TaskCockpit, Capability::ProviderInput]);
        let allowed = dispatch_with(
            granted,
            &TaskCockpitQuery::TerminalResizeFor {
                resource_id: provider_resource,
                cols: 100,
                rows: 30,
            },
        );
        assert!(
            matches!(
                allowed,
                QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Terminal(_)))
            ),
            "{allowed:?}"
        );
        assert_eq!(provider_cols(), 100);
    }

    /// A shell that is releasing is no longer a terminal a client may move.
    /// The refusal has to land before the viewport does: the durable close and
    /// an in-flight resize race by construction, because closing a tab is
    /// exactly when a client is still sending geometry for it.
    #[test]
    fn a_releasing_shell_refuses_the_resize_without_applying_it() {
        use crate::domain::command::OpenShellTerminalIntent;
        use crate::terminal::service::{MockAttachedRuntime, TerminalService};

        let (_repository, mut bus, client_id, task_id, roots, _agent_id, _provider) =
            create_unstarted_draft_with_terminal_claim();
        let revision = bus
            .task_snapshot(task_id)
            .expect("lookup")
            .expect("task")
            .task
            .revision;
        let shell = plain_shell_resource(task_id, SHELL_B_PROGRAM);
        let receipt = host_open_shell(
            &mut bus,
            CommandEnvelope {
                command_id: CommandId::new(),
                client_id,
                task_id: Some(task_id),
                issued_at_ms: 6,
                expected_task_revision: Some(revision),
                command: Command::OpenShellTerminal(OpenShellTerminalIntent {
                    resource: shell.clone(),
                }),
            },
        )
        .expect("open shell");
        let CommandReceipt::Accepted {
            task_revision: Some(revision),
            ..
        } = receipt
        else {
            panic!("open shell: {receipt:?}");
        };

        let size = crate::terminal::protocol::TerminalSize::new(80, 24).expect("size");
        let terminals = TerminalService::new();
        terminals
            .attach_plain_shell(
                task_id,
                shell.id,
                1,
                shell_terminal_spec(),
                MockAttachedRuntime::with_resource_fence(size, shell.id),
            )
            .expect("attach shell");

        // The hosted terminal stays open here on purpose: this is the window
        // where the durable resource has left Active but the PTY is still
        // resizable, which is precisely what the fence has to cover.
        let receipt = bus
            .execute(CommandEnvelope {
                command_id: CommandId::new(),
                client_id,
                task_id: Some(task_id),
                issued_at_ms: 7,
                expected_task_revision: Some(revision),
                command: Command::CloseTerminal {
                    resource_id: shell.id,
                },
            })
            .expect("close terminal");
        assert!(
            matches!(receipt, CommandReceipt::Accepted { .. }),
            "{receipt:?}"
        );
        assert_ne!(
            bus.task_snapshot(task_id)
                .expect("lookup")
                .expect("task")
                .resources[&shell.id]
                .lifecycle,
            crate::domain::ResourceLifecycle::Active,
            "the durable shell must have left Active for this test to mean anything"
        );

        let outcome = serve_task_cockpit(TaskCockpitDispatch {
            capabilities: CapabilitySet::from_capabilities([Capability::TaskCockpit]),
            envelope_task_id: Some(task_id),
            client_id,
            connection_id: Uuid::now_v7(),
            request_id: RequestId::new(),
            query: &TaskCockpitQuery::TerminalResizeFor {
                resource_id: shell.id,
                cols: 200,
                rows: 50,
            },
            bus: &bus,
            service_runtime: None,
            semantic_journal: None,
            terminal_service: Some(&terminals),
            ssh_endpoints: None,
            ssh_runtime: None,
            workspace_projects: Some(&roots),
            coordinator: None,
            action_epoch: None,
            runtime_generation: None,
            config: None,
            provider_launch_hint: ProviderLaunchReadinessHint::Unknown,
            provider_restore_detail: None,
        });
        assert!(
            matches!(
                outcome,
                QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Denied {
                    surface: TaskCockpitSurface::Terminal,
                    reason: TaskCockpitDeniedReason::StaleFence,
                    ..
                }))
            ),
            "{outcome:?}"
        );
        assert_eq!(
            terminals
                .task_terminal_view_for(task_id, Some(shell.id))
                .expect("view")
                .expect("present")
                .view
                .screen
                .cols,
            80,
            "a refused resize must not have been applied"
        );
    }

    #[test]
    fn legacy_terminal_query_still_reaches_the_provider_beside_open_shells() {
        let (_repository, bus, client_id, task_id, roots, terminals, provider_resource, _a, _b) =
            task_with_provider_and_two_shells();

        let outcome = serve_task_cockpit(TaskCockpitDispatch {
            capabilities: CapabilitySet::from_capabilities([
                Capability::TaskCockpit,
                Capability::ProviderInput,
            ]),
            envelope_task_id: Some(task_id),
            client_id,
            connection_id: Uuid::now_v7(),
            request_id: RequestId::new(),
            query: &TaskCockpitQuery::Terminal,
            bus: &bus,
            service_runtime: None,
            semantic_journal: None,
            terminal_service: Some(&terminals),
            ssh_endpoints: None,
            ssh_runtime: None,
            workspace_projects: Some(&roots),
            coordinator: None,
            action_epoch: None,
            runtime_generation: None,
            config: None,
            provider_launch_hint: ProviderLaunchReadinessHint::Unknown,
            provider_restore_detail: None,
        });
        let QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Terminal(projection))) =
            outcome
        else {
            panic!("open shells must not hide the provider terminal, got {outcome:?}");
        };
        assert!(projection.is_provider);
        assert_eq!(projection.resource_id, provider_resource);
        assert!(!projection.agent_session_id.is_nil());
    }

    /// The readiness classifier used to collect every Active Terminal resource
    /// at the agent's generation, which a task's plain shells satisfy too. One
    /// open shell made `[resource]` fail and the classifier answered
    /// `TerminalUnavailable` for every task that had one -- the same defect the
    /// provider serve path carried, one function along.
    #[test]
    fn terminal_readiness_classification_survives_an_open_plain_shell() {
        use crate::domain::command::OpenShellTerminalIntent;
        use crate::services::ProcessManager;
        use crate::terminal::service::TerminalService;

        let (_repository, mut bus, client_id, task_id, roots, agent_id, _provider_resource) =
            create_unstarted_draft_with_terminal_claim();
        let revision = bus
            .task_snapshot(task_id)
            .expect("lookup")
            .expect("task")
            .task
            .revision;
        let shell = plain_shell_resource(task_id, SHELL_B_PROGRAM);
        let receipt = host_open_shell(
            &mut bus,
            CommandEnvelope {
                command_id: CommandId::new(),
                client_id,
                task_id: Some(task_id),
                issued_at_ms: 6,
                expected_task_revision: Some(revision),
                command: Command::OpenShellTerminal(OpenShellTerminalIntent {
                    resource: shell.clone(),
                }),
            },
        )
        .expect("open shell");
        assert!(
            matches!(receipt, CommandReceipt::Accepted { .. }),
            "{receipt:?}"
        );
        let snapshot = bus.task_snapshot(task_id).expect("lookup").expect("task");
        let agent = snapshot.agents.get(&agent_id).expect("agent");
        assert_eq!(
            snapshot
                .resources
                .values()
                .filter(|resource| {
                    resource.resource_kind == crate::domain::ResourceKind::Terminal
                        && resource.lifecycle == crate::domain::ResourceLifecycle::Active
                        && resource.runtime_generation == agent.runtime_generation
                })
                .count(),
            2,
            "the shell and the provider terminal are both Active at this generation, \
             which is exactly the case the old filter could not tell apart"
        );

        // No hosted attachment at all, so this reaches the absence classifier.
        let terminals = TerminalService::new();
        let store_dir = tempfile::tempdir().expect("provider store");
        let manager = ProcessManager::new_with_provider_session_store_path(
            store_dir.path().join("provider-sessions.sqlite"),
        );
        let outcome = serve_task_cockpit(TaskCockpitDispatch {
            capabilities: CapabilitySet::from_capabilities([Capability::TaskCockpit]),
            envelope_task_id: Some(task_id),
            client_id,
            connection_id: Uuid::now_v7(),
            request_id: RequestId::new(),
            query: &TaskCockpitQuery::TerminalReadiness,
            bus: &bus,
            service_runtime: Some(&manager),
            semantic_journal: None,
            terminal_service: Some(&terminals),
            ssh_endpoints: None,
            ssh_runtime: None,
            workspace_projects: Some(&roots),
            coordinator: None,
            action_epoch: None,
            runtime_generation: None,
            config: None,
            provider_launch_hint: ProviderLaunchReadinessHint::NotPending,
            provider_restore_detail: None,
        });
        assert!(
            matches!(
                outcome,
                QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Unavailable {
                    surface: TaskCockpitSurface::Terminal,
                    reason: TaskCockpitUnavailableReason::TerminalNotStarted,
                    ..
                }))
            ),
            "an open shell must not collapse readiness to TerminalUnavailable, got {outcome:?}"
        );
    }

    #[test]
    fn a_provider_terminal_refusal_carries_the_host_known_cause() {
        use crate::services::ProcessManager;
        use crate::terminal::service::TerminalService;

        let (_repository, bus, client_id, task_id, roots, _agent_id, _resource) =
            create_unstarted_draft_with_terminal_claim();
        let granted = CapabilitySet::from_capabilities([Capability::TaskCockpit]);
        let terminals = TerminalService::new();
        let store_dir = tempfile::tempdir().expect("provider store");
        let manager = ProcessManager::new_with_provider_session_store_path(
            store_dir.path().join("provider-sessions.sqlite"),
        );
        let cause = "Claude Code was updated since this session started;                      resuming with the new build...";

        let with_cause = serve_task_cockpit(TaskCockpitDispatch {
            capabilities: granted,
            envelope_task_id: Some(task_id),
            client_id,
            connection_id: Uuid::now_v7(),
            request_id: RequestId::new(),
            query: &TaskCockpitQuery::Terminal,
            bus: &bus,
            service_runtime: Some(&manager),
            semantic_journal: None,
            terminal_service: Some(&terminals),
            ssh_endpoints: None,
            ssh_runtime: None,
            workspace_projects: Some(&roots),
            coordinator: None,
            action_epoch: None,
            runtime_generation: None,
            config: None,
            provider_launch_hint: ProviderLaunchReadinessHint::NotPending,
            provider_restore_detail: Some(cause),
        });
        let QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Unavailable {
            surface,
            reason,
            detail,
        })) = with_cause
        else {
            panic!("the provider terminal is unavailable here");
        };
        assert_eq!(surface, TaskCockpitSurface::Terminal);
        assert_eq!(reason, TaskCockpitUnavailableReason::TerminalUnavailable);
        assert_eq!(
            detail.as_deref(),
            Some(cause),
            "a known cause must never be dropped in favour of the bare reason"
        );

        let without_cause = serve_task_cockpit(TaskCockpitDispatch {
            capabilities: granted,
            envelope_task_id: Some(task_id),
            client_id,
            connection_id: Uuid::now_v7(),
            request_id: RequestId::new(),
            query: &TaskCockpitQuery::Terminal,
            bus: &bus,
            service_runtime: Some(&manager),
            semantic_journal: None,
            terminal_service: Some(&terminals),
            ssh_endpoints: None,
            ssh_runtime: None,
            workspace_projects: Some(&roots),
            coordinator: None,
            action_epoch: None,
            runtime_generation: None,
            config: None,
            provider_launch_hint: ProviderLaunchReadinessHint::NotPending,
            provider_restore_detail: None,
        });
        let QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Unavailable {
            detail,
            ..
        })) = without_cause
        else {
            panic!("the provider terminal is unavailable here");
        };
        assert_eq!(detail, None, "no cause known, nothing invented");
    }

    #[test]
    fn terminal_readiness_classifies_pending_unknown_and_not_started() {
        use crate::services::ProcessManager;
        use crate::terminal::service::TerminalService;

        let (_repository, bus, client_id, task_id, roots, agent_id, _resource) =
            create_unstarted_draft_with_terminal_claim();
        let granted = CapabilitySet::from_capabilities([Capability::TaskCockpit]);
        let terminals = TerminalService::new();
        let store_dir = tempfile::tempdir().expect("provider store");
        // Genuinely cold manager: provider_sessions stays None; missing store
        // file is proved absence without peek/init.
        let manager = ProcessManager::new_with_provider_session_store_path(
            store_dir.path().join("provider-sessions.sqlite"),
        );
        assert!(
            matches!(
                manager.try_classify_persisted_provider_launch(agent_id),
                Ok(Some(false))
            ),
            "cold missing store must classify absence without initializing the manager"
        );

        let no_service = serve_task_cockpit(TaskCockpitDispatch {
            capabilities: granted,
            envelope_task_id: Some(task_id),
            client_id,
            connection_id: Uuid::now_v7(),
            request_id: RequestId::new(),
            query: &TaskCockpitQuery::TerminalReadiness,
            bus: &bus,
            service_runtime: None,
            semantic_journal: None,
            terminal_service: None,
            ssh_endpoints: None,
            ssh_runtime: None,
            workspace_projects: Some(&roots),
            coordinator: None,
            action_epoch: None,
            runtime_generation: None,
            config: None,
            provider_launch_hint: ProviderLaunchReadinessHint::NotPending,
            provider_restore_detail: None,
        });
        assert!(matches!(
            no_service,
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Unavailable {
                surface: TaskCockpitSurface::Terminal,
                reason: TaskCockpitUnavailableReason::TerminalUnavailable,
                ..
            }))
        ));

        let pending = serve_task_cockpit(TaskCockpitDispatch {
            capabilities: granted,
            envelope_task_id: Some(task_id),
            client_id,
            connection_id: Uuid::now_v7(),
            request_id: RequestId::new(),
            query: &TaskCockpitQuery::TerminalReadiness,
            bus: &bus,
            service_runtime: Some(&manager),
            semantic_journal: None,
            terminal_service: Some(&terminals),
            ssh_endpoints: None,
            ssh_runtime: None,
            workspace_projects: Some(&roots),
            coordinator: None,
            action_epoch: None,
            runtime_generation: None,
            config: None,
            provider_launch_hint: ProviderLaunchReadinessHint::StartPending,
            provider_restore_detail: None,
        });
        assert!(matches!(
            pending,
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Unavailable {
                surface: TaskCockpitSurface::Terminal,
                reason: TaskCockpitUnavailableReason::TerminalStartPending,
                ..
            }))
        ));

        let unknown = serve_task_cockpit(TaskCockpitDispatch {
            capabilities: granted,
            envelope_task_id: Some(task_id),
            client_id,
            connection_id: Uuid::now_v7(),
            request_id: RequestId::new(),
            query: &TaskCockpitQuery::TerminalReadiness,
            bus: &bus,
            service_runtime: Some(&manager),
            semantic_journal: None,
            terminal_service: Some(&terminals),
            ssh_endpoints: None,
            ssh_runtime: None,
            workspace_projects: Some(&roots),
            coordinator: None,
            action_epoch: None,
            runtime_generation: None,
            config: None,
            provider_launch_hint: ProviderLaunchReadinessHint::Unknown,
            provider_restore_detail: None,
        });
        assert!(matches!(
            unknown,
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Unavailable {
                surface: TaskCockpitSurface::Terminal,
                reason: TaskCockpitUnavailableReason::TerminalUnavailable,
                ..
            }))
        ));

        let legacy = serve_task_cockpit(TaskCockpitDispatch {
            capabilities: granted,
            envelope_task_id: Some(task_id),
            client_id,
            connection_id: Uuid::now_v7(),
            request_id: RequestId::new(),
            query: &TaskCockpitQuery::Terminal,
            bus: &bus,
            service_runtime: Some(&manager),
            semantic_journal: None,
            terminal_service: Some(&terminals),
            ssh_endpoints: None,
            ssh_runtime: None,
            workspace_projects: Some(&roots),
            coordinator: None,
            action_epoch: None,
            runtime_generation: None,
            config: None,
            provider_launch_hint: ProviderLaunchReadinessHint::StartPending,
            provider_restore_detail: None,
        });
        assert!(
            matches!(
                legacy,
                QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Unavailable {
                    surface: TaskCockpitSurface::Terminal,
                    reason: TaskCockpitUnavailableReason::TerminalUnavailable,
                    ..
                }))
            ),
            "legacy Terminal must keep the closed Unavailable reason"
        );

        let not_started = serve_task_cockpit(TaskCockpitDispatch {
            capabilities: granted,
            envelope_task_id: Some(task_id),
            client_id,
            connection_id: Uuid::now_v7(),
            request_id: RequestId::new(),
            query: &TaskCockpitQuery::TerminalReadiness,
            bus: &bus,
            service_runtime: Some(&manager),
            semantic_journal: None,
            terminal_service: Some(&terminals),
            ssh_endpoints: None,
            ssh_runtime: None,
            workspace_projects: Some(&roots),
            coordinator: None,
            action_epoch: None,
            runtime_generation: None,
            config: None,
            provider_launch_hint: ProviderLaunchReadinessHint::NotPending,
            provider_restore_detail: None,
        });
        assert!(matches!(
            not_started,
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Unavailable {
                surface: TaskCockpitSurface::Terminal,
                reason: TaskCockpitUnavailableReason::TerminalNotStarted,
                ..
            }))
        ));
    }

    #[test]
    fn terminal_readiness_corrupt_store_stays_unknown_within_bound() {
        use crate::services::ProcessManager;
        use crate::terminal::service::TerminalService;
        use std::time::Instant;

        let (_repository, bus, client_id, task_id, roots, agent_id, _resource) =
            create_unstarted_draft_with_terminal_claim();
        let store_dir = tempfile::tempdir().expect("provider store");
        let store_path = store_dir.path().join("provider-sessions.sqlite");
        std::fs::write(&store_path, b"not-a-sqlite-database").expect("corrupt");
        let manager = ProcessManager::new_with_provider_session_store_path(store_path);
        let started = Instant::now();
        let classified = manager.try_classify_persisted_provider_launch(agent_id);
        assert!(
            started.elapsed() < std::time::Duration::from_millis(500),
            "corrupt probe must stay bounded"
        );
        assert!(
            matches!(classified, Ok(None)),
            "corrupt store must be unknown, got {classified:?}"
        );
        let terminals = TerminalService::new();
        let outcome = serve_task_cockpit(TaskCockpitDispatch {
            capabilities: CapabilitySet::from_capabilities([Capability::TaskCockpit]),
            envelope_task_id: Some(task_id),
            client_id,
            connection_id: Uuid::now_v7(),
            request_id: RequestId::new(),
            query: &TaskCockpitQuery::TerminalReadiness,
            bus: &bus,
            service_runtime: Some(&manager),
            semantic_journal: None,
            terminal_service: Some(&terminals),
            ssh_endpoints: None,
            ssh_runtime: None,
            workspace_projects: Some(&roots),
            coordinator: None,
            action_epoch: None,
            runtime_generation: None,
            config: None,
            provider_launch_hint: ProviderLaunchReadinessHint::NotPending,
            provider_restore_detail: None,
        });
        assert!(matches!(
            outcome,
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Unavailable {
                surface: TaskCockpitSurface::Terminal,
                reason: TaskCockpitUnavailableReason::TerminalUnavailable,
                ..
            }))
        ));
    }

    #[test]
    fn terminal_readiness_live_binding_without_attachment_is_pending_not_absence() {
        let source = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/host/cockpit.rs"));
        let start = source
            .find("fn classify_terminal_readiness_absence(")
            .expect("classify_terminal_readiness_absence");
        let body = &source[start..];
        let end = body
            .find("\npub(crate) fn serve_conversation(")
            .expect("serve_conversation follows classify");
        let body = &body[..end];
        assert!(
            !body.contains("provider_terminal_binding("),
            "no-attachment classification must not call blocking provider_terminal_binding"
        );
        assert!(
            body.contains("try_has_live_provider_runtime("),
            "live runtime book must be tri-state checked"
        );
        assert!(
            body.contains("Some(false)"),
            "NotStarted requires proved Some(false) live absence"
        );
        assert!(
            body.contains("try_classify_persisted_provider_launch("),
            "persisted launch classification must stay non-blocking"
        );
        assert!(
            !body.contains("peek_persisted_provider_launch_spec("),
            "readiness must not call the blocking persisted peek"
        );
    }
}
