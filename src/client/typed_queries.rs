//! Shared typed query/command helpers over [`AsyncHostRequestPort`].
//!
//! Envelope construction, capability gates, and reply decode live here once so
//! HostClient and a future fleet adapter do not duplicate them. This is not a
//! transport, CommandBus, or snapshot/replay lease owner.

use std::time::Duration;

use crate::domain::cockpit::{
    AgentConnectionSnapshot, ConfigSidebarSnapshot, TaskCockpitQuery, TaskCockpitResult,
};
use crate::domain::command::{Command, CommandEnvelope, CommandReceipt, ConfirmHostQuitIntent};
use crate::domain::host::HostQuitInspection;
use crate::domain::id::{ClientId, CommandId, RequestId, TaskId};
use crate::domain::query::{
    Query, QueryEnvelope, QueryError, QueryOutcome, QueryReply, QueryResult,
};
use crate::domain::snapshot::TaskSnapshotItem;
use crate::host::{agent_connection_query_timeout, task_cockpit_query_timeout, IpcError};
use crate::prompts::projection::{PromptLibraryQuery, PromptProjectionReply};
use crate::protocol::Capability;

use super::action::{task_cockpit_query, task_show_query};
use super::port::AsyncHostRequestPort;

fn unix_time_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

async fn retire_unexpected<P: AsyncHostRequestPort>(port: &mut P, error: IpcError) -> IpcError {
    port.retire_request_transport().await;
    error
}

fn global_cockpit_query(client_id: ClientId, query: TaskCockpitQuery) -> QueryEnvelope {
    QueryEnvelope {
        request_id: RequestId::new(),
        client_id,
        task_id: None,
        query: Query::TaskCockpit(query),
    }
}

/// Read one Task snapshot through the shared `task.show` query factory.
pub async fn task_snapshot<P: AsyncHostRequestPort>(
    port: &mut P,
    task_id: TaskId,
) -> Result<Result<TaskSnapshotItem, QueryError>, IpcError> {
    let reply = port
        .request_query(
            task_show_query(RequestId::new(), port.client_id(), task_id),
            None,
        )
        .await?;
    match reply.outcome {
        QueryOutcome::Err(error) => Ok(Err(error)),
        QueryOutcome::Ok(QueryResult::TaskSnapshot { snapshot }) if snapshot.task.id == task_id => {
            Ok(Ok(snapshot))
        }
        QueryOutcome::Ok(_) => Err(retire_unexpected(port, IpcError::CorrelationMismatch).await),
    }
}

/// Confirm host quit admission. Requires granted HostShutdown.
pub async fn confirm_host_quit<P: AsyncHostRequestPort>(
    port: &mut P,
    command_id: CommandId,
    inspection_id: u64,
    allow_uninspected_worktrees: bool,
) -> Result<CommandReceipt, IpcError> {
    if !port
        .granted_capabilities()
        .contains(Capability::HostShutdown)
    {
        return Err(IpcError::UnsupportedCapability);
    }
    let envelope = CommandEnvelope {
        command_id,
        client_id: port.client_id(),
        task_id: None,
        issued_at_ms: unix_time_ms(),
        expected_task_revision: None,
        command: Command::ConfirmHostQuit(ConfirmHostQuitIntent {
            inspection_id,
            allow_uninspected_worktrees,
        }),
    };
    port.request_command(envelope).await
}

/// Inspect durable host-quit blockers. Requires granted HostShutdown.
pub async fn inspect_host_quit<P: AsyncHostRequestPort>(
    port: &mut P,
) -> Result<Result<HostQuitInspection, QueryError>, IpcError> {
    if !port
        .granted_capabilities()
        .contains(Capability::HostShutdown)
    {
        return Err(IpcError::UnsupportedCapability);
    }
    let reply = port
        .request_query(
            QueryEnvelope {
                request_id: RequestId::new(),
                client_id: port.client_id(),
                task_id: None,
                query: Query::InspectHostQuit,
            },
            None,
        )
        .await?;
    match reply.outcome {
        QueryOutcome::Err(error) => Ok(Err(error)),
        QueryOutcome::Ok(QueryResult::HostQuitInspection { inspection }) => Ok(Ok(inspection)),
        QueryOutcome::Ok(_) => Err(retire_unexpected(port, IpcError::UnexpectedResponse).await),
    }
}

/// Page the personal prompt library. Requires personal prompt projection grant.
pub async fn query_prompt_library<P: AsyncHostRequestPort>(
    port: &mut P,
    query: PromptLibraryQuery,
) -> Result<Result<PromptProjectionReply, QueryError>, IpcError> {
    if !port.granted_capabilities().grants_personal_prompt_library() {
        return Err(IpcError::UnsupportedCapability);
    }
    let reply = port
        .request_query(
            QueryEnvelope {
                request_id: RequestId::new(),
                client_id: port.client_id(),
                task_id: None,
                query: Query::PromptLibrary(query),
            },
            None,
        )
        .await?;
    match reply.outcome {
        QueryOutcome::Err(error) => Ok(Err(error)),
        QueryOutcome::Ok(QueryResult::PromptLibrary(page)) => Ok(Ok(page)),
        QueryOutcome::Ok(_) => Err(retire_unexpected(port, IpcError::UnexpectedResponse).await),
    }
}

/// Query one Task Cockpit surface with the long cockpit completion deadline.
pub async fn query_task_cockpit<P: AsyncHostRequestPort>(
    port: &mut P,
    task_id: TaskId,
    query: TaskCockpitQuery,
) -> Result<Result<TaskCockpitResult, QueryError>, IpcError> {
    if !port.granted_capabilities().grants_task_cockpit() {
        return Err(IpcError::UnsupportedCapability);
    }
    let reply = port
        .request_query(
            task_cockpit_query(RequestId::new(), port.client_id(), task_id, query),
            Some(task_cockpit_query_timeout()),
        )
        .await?;
    match reply.outcome {
        QueryOutcome::Err(error) => Ok(Err(error)),
        QueryOutcome::Ok(QueryResult::TaskCockpit(result)) => Ok(Ok(result)),
        QueryOutcome::Ok(_) => Err(retire_unexpected(port, IpcError::CorrelationMismatch).await),
    }
}

/// Host-owned redacted configuration projection; no task identity is invented.
pub async fn query_config_sidebar<P: AsyncHostRequestPort>(
    port: &mut P,
) -> Result<Result<ConfigSidebarSnapshot, QueryError>, IpcError> {
    if !port.granted_capabilities().grants_task_cockpit() {
        return Err(IpcError::UnsupportedCapability);
    }
    let reply = port
        .request_query(
            global_cockpit_query(port.client_id(), TaskCockpitQuery::ConfigSnapshot),
            None,
        )
        .await?;
    match reply.outcome {
        QueryOutcome::Err(error) => Ok(Err(error)),
        QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Config(snapshot))) => {
            Ok(Ok(snapshot))
        }
        QueryOutcome::Ok(_) => Err(retire_unexpected(port, IpcError::CorrelationMismatch).await),
    }
}

/// Agent connection snapshot with the long agent-connection deadline.
pub async fn query_agent_connection<P: AsyncHostRequestPort>(
    port: &mut P,
) -> Result<Result<AgentConnectionSnapshot, QueryError>, IpcError> {
    if !port.granted_capabilities().grants_task_cockpit() {
        return Err(IpcError::UnsupportedCapability);
    }
    let reply = port
        .request_query(
            global_cockpit_query(port.client_id(), TaskCockpitQuery::AgentConnection),
            Some(agent_connection_query_timeout()),
        )
        .await?;
    match reply.outcome {
        QueryOutcome::Err(error) => Ok(Err(error)),
        QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::AgentConnection(
            snapshot,
        ))) => Ok(Ok(snapshot)),
        QueryOutcome::Ok(_) => Err(retire_unexpected(port, IpcError::CorrelationMismatch).await),
    }
}

/// Global local-pipe-only remote-access setup; no synthetic task identity.
pub async fn query_remote_access<P: AsyncHostRequestPort>(
    port: &mut P,
    request: crate::host::remote_setup::RemoteSetupRequest,
) -> Result<Result<crate::host::remote_setup::RemoteSetupReply, QueryError>, IpcError> {
    if !port.granted_capabilities().grants_task_cockpit() {
        return Err(IpcError::UnsupportedCapability);
    }
    let reply = port
        .request_query(
            QueryEnvelope {
                request_id: RequestId::new(),
                client_id: port.client_id(),
                task_id: None,
                query: Query::TaskCockpit(TaskCockpitQuery::RemoteAccess(request)),
            },
            Some(task_cockpit_query_timeout()),
        )
        .await?;
    match reply.outcome {
        QueryOutcome::Err(error) => Ok(Err(error)),
        QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::RemoteAccess(reply))) => {
            Ok(Ok(reply))
        }
        QueryOutcome::Ok(_) => Err(retire_unexpected(port, IpcError::CorrelationMismatch).await),
    }
}

/// Provider settings host projection with the cockpit completion deadline.
pub async fn query_provider_settings<P: AsyncHostRequestPort>(
    port: &mut P,
    request: crate::providers::settings::ProviderSettingsHostRequest,
) -> Result<Result<crate::providers::settings::ProviderSettingsReply, QueryError>, IpcError> {
    if !port.granted_capabilities().grants_task_cockpit() {
        return Err(IpcError::UnsupportedCapability);
    }
    let reply = port
        .request_query(
            global_cockpit_query(
                port.client_id(),
                TaskCockpitQuery::ProviderSettings(request),
            ),
            Some(task_cockpit_query_timeout()),
        )
        .await?;
    match reply.outcome {
        QueryOutcome::Err(error) => Ok(Err(error)),
        QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::ProviderSettings(
            settings,
        ))) => Ok(Ok(settings)),
        QueryOutcome::Ok(_) => Err(retire_unexpected(port, IpcError::CorrelationMismatch).await),
    }
}

/// Bounded path-redacted repository catalog for one Task.
pub async fn query_git_repositories<P: AsyncHostRequestPort>(
    port: &mut P,
    task_id: TaskId,
) -> Result<Result<crate::domain::TaskGitRepositoriesProjection, QueryError>, IpcError> {
    match query_task_cockpit(port, task_id, TaskCockpitQuery::GitRepositories).await? {
        Ok(TaskCockpitResult::GitRepositories(catalog)) => Ok(Ok(catalog)),
        Ok(_) => Err(retire_unexpected(port, IpcError::UnexpectedResponse).await),
        Err(error) => Ok(Err(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::id::EnvironmentId;
    use crate::domain::snapshot::TaskSnapshotItem;
    use crate::domain::task::{
        ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity, TaskFacts,
        TaskLifecycle, WorkspaceRef,
    };
    use crate::domain::ClientId;
    use crate::protocol::{Capability, CapabilitySet};
    use async_trait::async_trait;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[derive(Clone)]
    struct FakeAsyncPort {
        client_id: ClientId,
        capabilities: CapabilitySet,
        inner: Arc<Mutex<FakeAsyncPortInner>>,
    }

    struct FakeAsyncPortInner {
        queries: Vec<(QueryEnvelope, Option<Duration>)>,
        commands: Vec<CommandEnvelope>,
        retired: usize,
        next_query: Option<Result<QueryReply, IpcError>>,
        next_command: Option<Result<CommandReceipt, IpcError>>,
    }

    impl FakeAsyncPort {
        fn new(client_id: ClientId, capabilities: CapabilitySet) -> Self {
            Self {
                client_id,
                capabilities,
                inner: Arc::new(Mutex::new(FakeAsyncPortInner {
                    queries: Vec::new(),
                    commands: Vec::new(),
                    retired: 0,
                    next_query: None,
                    next_command: None,
                })),
            }
        }

        async fn set_query(&self, reply: Result<QueryReply, IpcError>) {
            self.inner.lock().await.next_query = Some(reply);
        }

        async fn queries(&self) -> Vec<(QueryEnvelope, Option<Duration>)> {
            self.inner.lock().await.queries.clone()
        }

        async fn retired(&self) -> usize {
            self.inner.lock().await.retired
        }
    }

    #[async_trait]
    impl AsyncHostRequestPort for FakeAsyncPort {
        fn client_id(&self) -> ClientId {
            self.client_id
        }

        fn granted_capabilities(&self) -> CapabilitySet {
            self.capabilities.clone()
        }

        async fn request_query(
            &mut self,
            envelope: QueryEnvelope,
            timeout: Option<Duration>,
        ) -> Result<QueryReply, IpcError> {
            let mut inner = self.inner.lock().await;
            inner.queries.push((envelope, timeout));
            inner
                .next_query
                .take()
                .unwrap_or(Err(IpcError::Unavailable))
        }

        async fn request_command(
            &mut self,
            envelope: CommandEnvelope,
        ) -> Result<CommandReceipt, IpcError> {
            let mut inner = self.inner.lock().await;
            inner.commands.push(envelope);
            inner
                .next_command
                .take()
                .unwrap_or(Err(IpcError::Unavailable))
        }

        async fn retire_request_transport(&mut self) {
            self.inner.lock().await.retired += 1;
        }
    }

    fn fixed_client() -> ClientId {
        ClientId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xa1,
        ])
        .expect("client")
    }

    fn fixed_task() -> TaskId {
        TaskId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xa2,
        ])
        .expect("task")
    }

    #[tokio::test]
    async fn task_snapshot_uses_exact_task_and_client_ids() {
        let client_id = fixed_client();
        let task_id = fixed_task();
        let mut port = FakeAsyncPort::new(client_id, CapabilitySet::empty());
        let snapshot = TaskSnapshotItem {
            task: TaskFacts {
                id: task_id,
                environment_id: EnvironmentId::from_bytes([
                    0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0xa3,
                ])
                .expect("env"),
                title: "t".into(),
                description: None,
                project_id: crate::domain::id::ProjectId::from_bytes([
                    0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0xa4,
                ])
                .expect("project"),
                workspace: WorkspaceRef::Main,
                assignment: TaskAssignment::LocalOwner,
                lifecycle: TaskLifecycle::Open,
                action_epoch: 0,
                revision: 1,
                created_at_ms: 1,
            },
            connectivity: TaskConnectivity::Connected,
            attention: TaskAttention::None,
            activity: TaskActivity::Idle,
            review_readiness: ReviewReadiness::NotReady,
            primary_agent_id: None,
        };
        port.set_query(Ok(QueryReply {
            request_id: RequestId::new(),
            outcome: QueryOutcome::Ok(QueryResult::TaskSnapshot {
                snapshot: snapshot.clone(),
            }),
        }))
        .await;
        let got = task_snapshot(&mut port, task_id).await.expect("ok");
        assert_eq!(got.expect("snapshot").task.id, task_id);
        let queries = port.queries().await;
        assert_eq!(queries.len(), 1);
        assert_eq!(queries[0].0.client_id, client_id);
        assert_eq!(queries[0].0.task_id, Some(task_id));
        assert!(queries[0].1.is_none());
    }

    #[tokio::test]
    async fn missing_capability_rejects_before_io() {
        let mut port = FakeAsyncPort::new(fixed_client(), CapabilitySet::empty());
        let err = inspect_host_quit(&mut port).await.expect_err("cap");
        assert!(matches!(err, IpcError::UnsupportedCapability));
        assert!(port.queries().await.is_empty());
        assert_eq!(port.retired().await, 0);
    }

    #[tokio::test]
    async fn wrong_result_retires_exact_port() {
        let mut port = FakeAsyncPort::new(
            fixed_client(),
            CapabilitySet::from_capabilities([Capability::HostShutdown]),
        );
        port.set_query(Ok(QueryReply {
            request_id: RequestId::new(),
            outcome: QueryOutcome::Ok(QueryResult::TaskSnapshot {
                snapshot: TaskSnapshotItem {
                    task: TaskFacts {
                        id: fixed_task(),
                        environment_id: EnvironmentId::from_bytes([
                            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00,
                            0x00, 0x00, 0x00, 0xb3,
                        ])
                        .expect("env"),
                        title: "x".into(),
                        description: None,
                        project_id: crate::domain::id::ProjectId::from_bytes([
                            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00,
                            0x00, 0x00, 0x00, 0xb4,
                        ])
                        .expect("project"),
                        workspace: WorkspaceRef::Main,
                        assignment: TaskAssignment::LocalOwner,
                        lifecycle: TaskLifecycle::Open,
                        action_epoch: 0,
                        revision: 1,
                        created_at_ms: 1,
                    },
                    connectivity: TaskConnectivity::Connected,
                    attention: TaskAttention::None,
                    activity: TaskActivity::Idle,
                    review_readiness: ReviewReadiness::NotReady,
                    primary_agent_id: None,
                },
            }),
        }))
        .await;
        let err = inspect_host_quit(&mut port).await.expect_err("wrong");
        assert!(matches!(err, IpcError::UnexpectedResponse));
        assert_eq!(port.retired().await, 1);
    }

    #[tokio::test]
    async fn cockpit_and_agent_keep_custom_timeouts() {
        let mut port = FakeAsyncPort::new(
            fixed_client(),
            CapabilitySet::from_capabilities([Capability::TaskCockpit]),
        );
        assert!(port.granted_capabilities().grants_task_cockpit());
        port.set_query(Ok(QueryReply {
            request_id: RequestId::new(),
            outcome: QueryOutcome::Err(QueryError::Unauthorized),
        }))
        .await;
        let _ = query_task_cockpit(&mut port, fixed_task(), TaskCockpitQuery::ConfigSnapshot)
            .await
            .expect("transport ok");
        let queries = port.queries().await;
        assert_eq!(queries[0].1, Some(task_cockpit_query_timeout()));

        port.set_query(Ok(QueryReply {
            request_id: RequestId::new(),
            outcome: QueryOutcome::Err(QueryError::Unauthorized),
        }))
        .await;
        let _ = query_agent_connection(&mut port)
            .await
            .expect("transport ok");
        let queries = port.queries().await;
        assert_eq!(
            queries.last().unwrap().1,
            Some(agent_connection_query_timeout())
        );
    }

    #[tokio::test]
    async fn remote_access_envelope_has_no_task() {
        let mut port = FakeAsyncPort::new(
            fixed_client(),
            CapabilitySet::from_capabilities([Capability::TaskCockpit]),
        );
        port.set_query(Ok(QueryReply {
            request_id: RequestId::new(),
            outcome: QueryOutcome::Err(QueryError::Unauthorized),
        }))
        .await;
        let _ = query_remote_access(
            &mut port,
            crate::host::remote_setup::RemoteSetupRequest::Snapshot,
        )
        .await
        .expect("transport");
        let queries = port.queries().await;
        assert!(queries[0].0.task_id.is_none());
        assert_eq!(queries[0].1, Some(task_cockpit_query_timeout()));
    }
}
