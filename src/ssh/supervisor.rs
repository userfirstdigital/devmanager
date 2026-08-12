//! Host-owned SSH task supervision.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use crate::config::{AppConfig, SSHConnection, SshAuthMode};
use crate::domain::id::{AgentSessionId, ResourceId, TaskId};
use crate::domain::operation::ResourceFence;
use crate::domain::resource::ResourceKind;
use crate::process::identity::ProcessOwner;
use crate::services::supervisor::{ManagedLaunchAuthority, ManagedLaunchSpec};
use crate::ssh::credentials::{
    CredentialError, CredentialKind, CredentialRef, CredentialResolver, CredentialSecret,
    KeyMaterialStore,
};
use crate::ssh::launch::{
    build_ssh_launch_plan, CancellationToken, HostIssuedSshBindingIssuer, LaunchOutcome,
    PromptMatch, PromptMatcher, SshLaunchError, SshLaunchRequest, SshPreSpawn,
};

#[derive(Debug, Clone)]
pub(crate) struct SshTaskIdentity {
    pub task_id: TaskId,
    pub agent_session_id: AgentSessionId,
    pub resource_id: ResourceId,
    pub runtime_generation: u64,
    pub action_epoch: u64,
    pub cwd: PathBuf,
}

pub(crate) trait SshRuntimeAdapter {
    fn status_for_task(&self, task_id: TaskId) -> Option<SshRuntimeSnapshot>;
    fn connect_endpoint(
        &self,
        endpoint_id: &str,
        identity: SshTaskIdentity,
    ) -> Result<SshRuntimeSnapshot, SshRuntimeError>;
}

/// Config-backed resolver for the legacy opaque SSH credential envelope.
/// Modern references remain unavailable until the host credential provider is
/// attached; plaintext is never copied into a launch spec or projection.
pub(crate) struct ConfigCredentialResolver {
    config: AppConfig,
}

impl ConfigCredentialResolver {
    pub(crate) fn new(config: AppConfig) -> Self {
        Self { config }
    }
}

impl CredentialResolver for ConfigCredentialResolver {
    fn resolve(&self, reference: &CredentialRef) -> Result<CredentialSecret, CredentialError> {
        for connection in &self.config.ssh_connections {
            let Some(auth) = connection.auth.as_ref() else {
                continue;
            };
            let Some(configured_reference) = auth.credential_ref.as_ref() else {
                continue;
            };
            if configured_reference != reference.as_str() {
                continue;
            }
            let Some((password, private_key)) =
                crate::config::model::decode_legacy_ssh_credential(configured_reference)
                    .map_err(|_| CredentialError::InvalidSecretMaterial)?
            else {
                return Err(CredentialError::MissingReference(reference.clone()));
            };
            let (kind, material) = match auth.mode {
                SshAuthMode::Password => (CredentialKind::Password, password),
                SshAuthMode::PrivateKey => (CredentialKind::PrivateKey, private_key),
                SshAuthMode::Default | SshAuthMode::Agent => {
                    return Err(CredentialError::MissingReference(reference.clone()));
                }
            };
            let Some(material) = material else {
                return Err(CredentialError::MissingReference(reference.clone()));
            };
            return CredentialSecret::from_bytes(kind, material.as_bytes());
        }
        Err(CredentialError::MissingReference(reference.clone()))
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SshAdmission {
    pub task_id: TaskId,
    pub agent_session_id: AgentSessionId,
    pub resource_id: ResourceId,
    pub runtime_generation: u64,
    pub action_epoch: u64,
    pub connection: SSHConnection,
    pub cwd: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SshLifecycle {
    Starting,
    Running,
    Stopping,
    Stopped,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SshRuntimeSnapshot {
    pub task_id: TaskId,
    pub agent_session_id: AgentSessionId,
    pub resource_id: ResourceId,
    pub runtime_generation: u64,
    pub action_epoch: u64,
    pub endpoint_id: String,
    pub lifecycle: SshLifecycle,
    pub error: Option<SshRuntimeError>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SshRuntimeError {
    InvalidAdmission,
    UnknownEndpoint,
    ArchivedEndpoint,
    CredentialUnavailable,
    HostKeyPrompt,
    Launch,
    StaleFence,
    AlreadyRunning,
    NotRunning,
    Teardown,
}

pub(crate) struct EmptyCredentialResolver;

impl CredentialResolver for EmptyCredentialResolver {
    fn resolve(&self, reference: &CredentialRef) -> Result<CredentialSecret, CredentialError> {
        Err(CredentialError::MissingReference(reference.clone()))
    }
}

struct SshSession<L> {
    admission: SshAdmission,
    fence: ResourceFence,
    pre_spawn: SshPreSpawn,
    matcher: PromptMatcher,
    live: Option<L>,
    lifecycle: SshLifecycle,
    error: Option<SshRuntimeError>,
}

pub(crate) struct SshSupervisor<A: ManagedLaunchAuthority, C = EmptyCredentialResolver> {
    authority: A,
    credentials: C,
    key_store: Option<KeyMaterialStore>,
    sessions: BTreeMap<ResourceId, SshSession<A::Live>>,
}

impl<A: ManagedLaunchAuthority> SshSupervisor<A, EmptyCredentialResolver> {
    pub(crate) fn new(authority: A) -> Self {
        Self {
            authority,
            credentials: EmptyCredentialResolver,
            key_store: None,
            sessions: BTreeMap::new(),
        }
    }
}

impl<A: ManagedLaunchAuthority, C: CredentialResolver> SshSupervisor<A, C> {
    pub(crate) fn with_credentials(
        authority: A,
        credentials: C,
        key_store: Option<KeyMaterialStore>,
    ) -> Self {
        Self {
            authority,
            credentials,
            key_store,
            sessions: BTreeMap::new(),
        }
    }

    pub(crate) fn connect(
        &mut self,
        admission: SshAdmission,
    ) -> Result<SshRuntimeSnapshot, SshRuntimeError> {
        validate_admission(&admission)?;
        if admission
            .connection
            .archived
            .as_ref()
            .copied()
            .unwrap_or(false)
        {
            return Err(SshRuntimeError::ArchivedEndpoint);
        }
        if let Some(existing) = self.sessions.get(&admission.resource_id) {
            if matches!(
                existing.lifecycle,
                SshLifecycle::Starting | SshLifecycle::Running | SshLifecycle::Stopping
            ) {
                return Err(SshRuntimeError::AlreadyRunning);
            }
        }
        self.sessions.remove(&admission.resource_id);

        let binding = HostIssuedSshBindingIssuer::issue_for_task(
            admission.task_id,
            admission.agent_session_id,
            admission.resource_id,
            admission.runtime_generation,
            admission.action_epoch,
            *uuid::Uuid::now_v7().as_bytes(),
        )
        .map_err(|_| SshRuntimeError::InvalidAdmission)?;
        let request = SshLaunchRequest::from_config(
            binding,
            &admission.connection,
            Duration::from_secs(crate::ssh::launch::MAX_RUNTIME_SECONDS),
        )
        .map_err(map_launch_error)?;
        let cancellation = CancellationToken::new();
        let mut plan = match build_ssh_launch_plan(
            &request,
            &self.credentials,
            self.key_store.as_ref(),
            &cancellation,
        )
        .map_err(map_launch_error)?
        {
            LaunchOutcome::Ready(plan) => plan,
            LaunchOutcome::Cancelled => return Err(SshRuntimeError::Launch),
        };
        let matcher = plan.prompt_matcher();
        let pre_spawn = plan.pre_spawn().map_err(map_launch_error)?;
        let command = pre_spawn.command();
        let spec = ManagedLaunchSpec {
            resource_id: admission.resource_id,
            generation: admission.runtime_generation,
            owner: ProcessOwner::Task(admission.task_id),
            kind: ResourceKind::Terminal,
            program: command.program().to_owned(),
            args: command.args().to_vec(),
            cwd: admission.cwd.to_string_lossy().into_owned(),
            environment: command.env().clone(),
            display_label: format!("ssh:{}", admission.connection.id),
        };
        let pending = self
            .authority
            .prepare_suspended(&spec)
            .map_err(|_| SshRuntimeError::Launch)?;
        let pending = self
            .authority
            .register_suspended(pending)
            .map_err(|_| SshRuntimeError::Launch)?;
        let live = self
            .authority
            .resume(pending)
            .map_err(|_| SshRuntimeError::Launch)?;
        if A::live_generation(&live) != admission.runtime_generation {
            let mut live = Some(live);
            let _ = self.authority.teardown(
                &mut live,
                ResourceFence::new(admission.resource_id, admission.runtime_generation),
            );
            return Err(SshRuntimeError::StaleFence);
        }
        let fence = ResourceFence::new(admission.resource_id, admission.runtime_generation);
        let snapshot = SshRuntimeSnapshot {
            task_id: admission.task_id,
            agent_session_id: admission.agent_session_id,
            resource_id: admission.resource_id,
            runtime_generation: admission.runtime_generation,
            action_epoch: admission.action_epoch,
            endpoint_id: admission.connection.id.clone(),
            lifecycle: SshLifecycle::Running,
            error: None,
        };
        self.sessions.insert(
            admission.resource_id,
            SshSession {
                admission,
                fence,
                pre_spawn,
                matcher,
                live: Some(live),
                lifecycle: SshLifecycle::Running,
                error: None,
            },
        );
        Ok(snapshot)
    }

    pub(crate) fn poll(&mut self) -> Vec<SshRuntimeSnapshot> {
        let ids = self.sessions.keys().copied().collect::<Vec<_>>();
        for resource_id in ids {
            self.poll_one(resource_id);
        }
        self.sessions.values().map(snapshot).collect()
    }

    pub(crate) fn snapshots(&mut self) -> Vec<SshRuntimeSnapshot> {
        self.poll()
    }

    pub(crate) fn snapshot_for_task(&mut self, task_id: TaskId) -> Option<SshRuntimeSnapshot> {
        self.poll()
            .into_iter()
            .find(|snapshot| snapshot.task_id == task_id)
    }

    pub(crate) fn stop(
        &mut self,
        task_id: TaskId,
        agent_session_id: AgentSessionId,
        resource_id: ResourceId,
        runtime_generation: u64,
        action_epoch: u64,
    ) -> Result<SshRuntimeSnapshot, SshRuntimeError> {
        let session = self
            .sessions
            .get_mut(&resource_id)
            .ok_or(SshRuntimeError::NotRunning)?;
        if session.admission.task_id != task_id
            || session.admission.agent_session_id != agent_session_id
            || session.admission.runtime_generation != runtime_generation
            || session.admission.action_epoch != action_epoch
        {
            return Err(SshRuntimeError::StaleFence);
        }
        if session.live.is_none() {
            return Err(SshRuntimeError::NotRunning);
        }
        session.lifecycle = SshLifecycle::Stopping;
        if self
            .authority
            .teardown(&mut session.live, session.fence)
            .is_err()
        {
            session.lifecycle = SshLifecycle::Failed;
            session.error = Some(SshRuntimeError::Teardown);
        } else {
            session.lifecycle = SshLifecycle::Stopped;
            session.error = None;
        }
        Ok(snapshot(session))
    }

    fn poll_one(&mut self, resource_id: ResourceId) {
        let Some(mut session) = self.sessions.remove(&resource_id) else {
            return;
        };
        if let Some(live) = session.live.as_ref() {
            let lines = self.authority.drain_output_lines(live);
            let mut failure = None;
            for line in lines {
                let result = session.matcher.observe(line.as_bytes());
                match result {
                    Ok(PromptMatch::Ignore) => {}
                    Ok(PromptMatch::HostKey(_)) => {
                        session.error = Some(SshRuntimeError::HostKeyPrompt);
                    }
                    Ok(PromptMatch::Input(input)) => {
                        let delivery =
                            match input.resolve(&self.credentials, session.pre_spawn.binding()) {
                                Ok(delivery) => delivery,
                                Err(error) => {
                                    failure = Some(map_launch_error(error));
                                    break;
                                }
                            };
                        if self
                            .authority
                            .write_input(live, delivery.bytes(), session.fence)
                            .is_err()
                        {
                            failure = Some(SshRuntimeError::Launch);
                            break;
                        }
                    }
                    Err(error) => {
                        failure = Some(map_launch_error(error));
                        break;
                    }
                }
            }
            if let Some(error) = failure {
                session.lifecycle = SshLifecycle::Failed;
                session.error = Some(error);
                let _ = self.authority.teardown(&mut session.live, session.fence);
            } else if self.authority.take_exit(live).is_some() {
                let mut live = session.live.take();
                let exit = self.authority.teardown(&mut live, session.fence);
                session.live = live;
                if exit.is_err() {
                    session.lifecycle = SshLifecycle::Failed;
                    session.error = Some(SshRuntimeError::Teardown);
                } else {
                    session.lifecycle = SshLifecycle::Stopped;
                    session.error = None;
                }
            }
        }
        self.sessions.insert(resource_id, session);
    }
}

fn validate_admission(admission: &SshAdmission) -> Result<(), SshRuntimeError> {
    if admission.runtime_generation == 0
        || admission.action_epoch == 0
        || admission.connection.id.trim().is_empty()
        || admission.connection.host.trim().is_empty()
        || admission.connection.username.trim().is_empty()
        || !admission.cwd.is_absolute()
    {
        return Err(SshRuntimeError::InvalidAdmission);
    }
    Ok(())
}

fn map_launch_error(error: SshLaunchError) -> SshRuntimeError {
    match error {
        SshLaunchError::Credential(_) => SshRuntimeError::CredentialUnavailable,
        SshLaunchError::InvalidField(_) => SshRuntimeError::InvalidAdmission,
        SshLaunchError::DeadlineExpired
        | SshLaunchError::DeadlineTooFar
        | SshLaunchError::Cancelled
        | SshLaunchError::StaleBinding
        | SshLaunchError::AlreadyConsumed
        | SshLaunchError::PreSpawnConsumed
        | SshLaunchError::AttemptLimit
        | SshLaunchError::CapacityExceeded
        | SshLaunchError::CancellationLedgerUnavailable
        | SshLaunchError::UnsupportedRuntime
        | SshLaunchError::ArgumentTooLarge { .. }
        | SshLaunchError::EnvironmentTooLarge { .. }
        | SshLaunchError::PromptTooLarge { .. } => SshRuntimeError::Launch,
    }
}

fn snapshot<L>(session: &SshSession<L>) -> SshRuntimeSnapshot {
    SshRuntimeSnapshot {
        task_id: session.admission.task_id,
        agent_session_id: session.admission.agent_session_id,
        resource_id: session.admission.resource_id,
        runtime_generation: session.admission.runtime_generation,
        action_epoch: session.admission.action_epoch,
        endpoint_id: session.admission.connection.id.clone(),
        lifecycle: session.lifecycle,
        error: session.error.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Nullable, SshAuth, SshAuthMode};
    use crate::domain::id::{AgentSessionId, ResourceId, TaskId};
    use crate::services::supervisor::FakeLaunchAuthority;
    use crate::ssh::credentials::{
        CredentialError, CredentialKind, CredentialRef, CredentialSecret,
    };

    struct TestCredentialResolver;

    impl CredentialResolver for TestCredentialResolver {
        fn resolve(&self, reference: &CredentialRef) -> Result<CredentialSecret, CredentialError> {
            assert_eq!(reference.as_str(), "credential:fixture");
            CredentialSecret::from_bytes(CredentialKind::Password, b"fixture-password")
        }
    }

    fn admission() -> SshAdmission {
        SshAdmission {
            task_id: TaskId::new(),
            agent_session_id: AgentSessionId::new(),
            resource_id: ResourceId::new(),
            runtime_generation: 3,
            action_epoch: 5,
            connection: SSHConnection {
                id: "fixture-ssh".into(),
                label: "Fixture SSH".into(),
                host: "fixture.invalid".into(),
                port: 22,
                username: "deploy".into(),
                auth: Nullable::Value(SshAuth {
                    mode: SshAuthMode::Agent,
                    credential_ref: Nullable::Absent,
                    extra: Default::default(),
                }),
                archived: Nullable::Value(false),
                extra: Default::default(),
            },
            cwd: std::env::current_dir().expect("cwd"),
        }
    }

    #[test]
    fn connects_configured_endpoint_through_managed_authority() {
        let authority = FakeLaunchAuthority::new();
        let mut supervisor = SshSupervisor::new(authority.clone());

        let result = supervisor.connect(admission()).expect("connect");

        assert_eq!(result.lifecycle, SshLifecycle::Running);
        assert_eq!(result.runtime_generation, 3);
        assert_eq!(result.action_epoch, 5);
        assert_eq!(authority.prepared(), 1);
        assert_eq!(authority.registered(), 1);
        assert_eq!(authority.resumed(), 1);
        assert_eq!(authority.live_count(), 1);
    }

    #[test]
    fn stop_requires_the_exact_task_agent_resource_generation() {
        let authority = FakeLaunchAuthority::new();
        let mut supervisor = SshSupervisor::new(authority.clone());
        let admission = admission();
        let running = supervisor.connect(admission.clone()).expect("connect");

        let stale = supervisor.stop(
            admission.task_id,
            admission.agent_session_id,
            admission.resource_id,
            admission.runtime_generation.saturating_sub(1),
            admission.action_epoch,
        );
        assert_eq!(stale, Err(SshRuntimeError::StaleFence));
        assert_eq!(authority.torn_down(), 0);

        let stopped = supervisor
            .stop(
                running.task_id,
                running.agent_session_id,
                running.resource_id,
                running.runtime_generation,
                running.action_epoch,
            )
            .expect("exact stop");
        assert_eq!(stopped.lifecycle, SshLifecycle::Stopped);
        assert_eq!(authority.torn_down(), 1);
        assert_eq!(authority.live_count(), 0);
    }

    #[test]
    fn prompt_input_is_delivered_only_through_the_managed_authority() {
        let authority = FakeLaunchAuthority::new();
        let mut admission = admission();
        admission.connection.auth = Nullable::Value(SshAuth {
            mode: SshAuthMode::Password,
            credential_ref: Nullable::Value("credential:fixture".into()),
            extra: Default::default(),
        });
        let mut supervisor =
            SshSupervisor::with_credentials(authority.clone(), TestCredentialResolver, None);
        supervisor.connect(admission).expect("connect");
        let token = authority.live_token().expect("live fixture");
        authority.push_output(token, "deploy@fixture.invalid's password:");

        let snapshots = supervisor.poll();
        assert_eq!(snapshots[0].lifecycle, SshLifecycle::Running);
        assert_eq!(authority.input_writes(), 1);
        assert!(!authority.last_spec_debug().contains("fixture-password"));
    }
}
