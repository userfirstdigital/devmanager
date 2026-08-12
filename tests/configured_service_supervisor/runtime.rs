//! Phase 6 Task 6.7 configured-command supervisor source tests.
//!
//! These tests exercise Job/process authority, generation fences, and port
//! claims through a fake managed-launch adapter. They never spawn an OS
//! process or probe a live port.

use std::collections::BTreeMap;

use crate::config::{Nullable, Project, ProjectFolder, RunCommand};
use crate::domain::TaskId;
use crate::process::ports::PortAuthority;
use crate::services::binding::{
    bind_configured_command, bind_configured_services, ConfiguredServiceOwner,
    ConfiguredServiceSource,
};
use crate::services::health::{ProbeOutcome, ServiceState, StatusTone};
use crate::services::model::{
    AdmissionFence, AdmissionRequester, CommandSpec, ExpectedPort, HealthPolicy, HealthSpec,
    HostAuthority, HostId, PortProtocol, ServiceCatalog, ServiceDefinition, ServiceId,
    ServiceScope, StartupPolicy, StopPolicy,
};
use crate::services::supervisor::{
    session_status_for_ui, FakeFailStage, FakeLaunchAuthority, ServiceSupervisor, SupervisorAction,
    SupervisorError, SupervisorEventKind, SupervisorOutcome,
};
use crate::state::SessionStatus;

fn id(value: &str) -> ServiceId {
    ServiceId::new(value).expect("test service id")
}

fn task_a() -> TaskId {
    TaskId::parse("0198b6b0-0000-7000-8000-000000000001").expect("test task id")
}

fn host() -> HostId {
    HostId::new(1)
}

fn fence() -> AdmissionFence {
    AdmissionFence::new(1, 1, 1)
}

fn policy() -> HealthPolicy {
    HealthPolicy {
        startup_deadline_ms: 5_000,
        probe_interval_ms: 1_000,
        max_probe_interval_ms: 4_000,
        backoff_multiplier: 2,
        success_threshold: 2,
        failure_threshold: 2,
        stale_after_ms: 2_500,
    }
}

fn service(
    name: &str,
    scope: ServiceScope,
    dependencies: Vec<ServiceId>,
    port: Option<u16>,
) -> ServiceDefinition {
    ServiceDefinition {
        id: id(name),
        scope,
        command: CommandSpec::new("node")
            .expect("program")
            .with_arg("server.js")
            .expect("arg")
            .with_cwd("apps/api")
            .expect("cwd")
            .with_env_reference("PORT")
            .expect("env"),
        dependencies,
        health: port.map_or(HealthSpec::None, |port| HealthSpec::Tcp {
            port,
            policy: policy(),
        }),
        startup: StartupPolicy::manual(),
        stop: StopPolicy::default(),
        expected_port: port.map(|port| ExpectedPort {
            protocol: PortProtocol::Tcp,
            port,
        }),
    }
}

fn supervisor_from_catalog(
    definitions: Vec<ServiceDefinition>,
    authority: FakeLaunchAuthority,
) -> ServiceSupervisor<FakeLaunchAuthority> {
    let catalog = ServiceCatalog::new(definitions).expect("catalog");
    ServiceSupervisor::from_catalog(
        catalog,
        BTreeMap::new(),
        BTreeMap::new(),
        authority,
        host(),
        1_000,
    )
    .expect("supervisor")
}

fn become_healthy(supervisor: &mut ServiceSupervisor<FakeLaunchAuthority>, name: &str) {
    supervisor
        .apply_probe(&id(name), 1, ProbeOutcome::Success)
        .expect("probe 1");
    supervisor.advance_clock(supervisor.now_ms().saturating_add(1_000));
    supervisor
        .apply_probe(&id(name), 1, ProbeOutcome::Success)
        .expect("probe 2");
}

fn project_fixture() -> (Project, ProjectFolder, RunCommand) {
    let command = RunCommand {
        id: "api".to_owned(),
        label: "API".to_owned(),
        command: "node".to_owned(),
        args: vec!["server.js".to_owned()],
        env: Nullable::Value(BTreeMap::from([("PORT".to_owned(), "8080".to_owned())])),
        port: Nullable::Value(8080),
        auto_restart: Nullable::Value(false),
        ..RunCommand::default()
    };
    let folder = ProjectFolder {
        id: "web".to_owned(),
        name: "web".to_owned(),
        folder_path: "C:/repo/apps/api".to_owned(),
        commands: vec![command.clone()],
        port_variable: Nullable::Value("PORT".to_owned()),
        ..ProjectFolder::default()
    };
    let project = Project {
        id: "proj".to_owned(),
        name: "proj".to_owned(),
        root_path: "C:/repo".to_owned(),
        folders: vec![folder.clone()],
        ..Project::default()
    };
    (project, folder, command)
}

#[test]
fn configured_command_binds_to_project_workspace_and_task() {
    let (project, folder, command) = project_fixture();
    let mut workspace_env = BTreeMap::new();
    workspace_env.insert("NODE_ENV".to_owned(), "test".to_owned());
    workspace_env.insert("API_TOKEN".to_owned(), "secret-value".to_owned());

    let task = bind_configured_command(ConfiguredServiceSource {
        project: &project,
        folder: &folder,
        command: &command,
        owner: ConfiguredServiceOwner::Task { task_id: task_a() },
        workspace_cwd: Some("apps/api"),
        workspace_env: &workspace_env,
    })
    .expect("task binding");
    assert!(matches!(
        task.owner,
        ConfiguredServiceOwner::Task { task_id } if task_id == task_a()
    ));
    assert_eq!(task.definition.scope, ServiceScope::task(task_a()));
    assert_eq!(task.definition.command.program().as_str(), "node");
    assert_eq!(
        task.definition
            .command
            .env()
            .iter()
            .map(|reference| reference.name())
            .collect::<Vec<_>>(),
        vec!["API_TOKEN", "NODE_ENV", "PORT"]
    );
    assert!(!format!("{:?}", task.environment).contains("secret-value"));

    let workspace = bind_configured_command(ConfiguredServiceSource {
        project: &project,
        folder: &folder,
        command: &command,
        owner: ConfiguredServiceOwner::Workspace {
            project_id: project.id.clone(),
            folder_id: folder.id.clone(),
        },
        workspace_cwd: Some("apps/api"),
        workspace_env: &BTreeMap::new(),
    })
    .expect("workspace binding");
    assert_eq!(workspace.definition.scope, ServiceScope::Host);

    let host_owned = bind_configured_command(ConfiguredServiceSource {
        project: &project,
        folder: &folder,
        command: &command,
        owner: ConfiguredServiceOwner::Project {
            project_id: project.id.clone(),
        },
        workspace_cwd: None,
        workspace_env: &BTreeMap::new(),
    })
    .expect("project binding");
    assert_eq!(host_owned.definition.scope, ServiceScope::Host);
}

#[test]
fn binding_rejects_secret_arguments_and_absolute_cwd() {
    let (project, folder, mut command) = project_fixture();
    command.args = vec!["--token".to_owned(), "secret-value".to_owned()];
    let error = bind_configured_command(ConfiguredServiceSource {
        project: &project,
        folder: &folder,
        command: &command,
        owner: ConfiguredServiceOwner::Project {
            project_id: project.id.clone(),
        },
        workspace_cwd: None,
        workspace_env: &BTreeMap::new(),
    })
    .expect_err("raw secret");
    assert!(!format!("{error:?}").contains("secret-value"));

    command.args.clear();
    let error = bind_configured_command(ConfiguredServiceSource {
        project: &project,
        folder: &folder,
        command: &command,
        owner: ConfiguredServiceOwner::Task { task_id: task_a() },
        workspace_cwd: Some("C:/abs"),
        workspace_env: &BTreeMap::new(),
    })
    .expect_err("absolute cwd");
    assert!(matches!(
        error,
        crate::services::binding::BindingError::Validation(_)
    ));
}

#[test]
fn launch_layers_environment_and_stays_on_managed_authority() {
    let (project, folder, command) = project_fixture();
    let mut workspace_env = BTreeMap::new();
    workspace_env.insert("API_TOKEN".to_owned(), "secret-value".to_owned());
    let binding = bind_configured_command(ConfiguredServiceSource {
        project: &project,
        folder: &folder,
        command: &command,
        owner: ConfiguredServiceOwner::Task { task_id: task_a() },
        workspace_cwd: Some("apps/api"),
        workspace_env: &workspace_env,
    })
    .expect("binding");
    let authority = FakeLaunchAuthority::new();
    let inspect = authority.clone();
    let mut supervisor = ServiceSupervisor::from_bindings(vec![binding], authority, host(), 1_000)
        .expect("supervisor");
    supervisor
        .handle(
            SupervisorAction::Start,
            &id("api"),
            fence(),
            AdmissionRequester::Task(task_a()),
        )
        .expect("start");
    assert_eq!(inspect.prepared(), 1);
    assert_eq!(inspect.registered(), 1);
    assert_eq!(inspect.resumed(), 1);
    assert_eq!(supervisor.live_count(), 1);
    assert_eq!(supervisor.probe_executions(), 0);
    assert!(inspect.last_env_names().contains(&"API_TOKEN".to_owned()));
    assert!(inspect.last_env_names().contains(&"PORT".to_owned()));
    assert!(!inspect.last_spec_debug().contains("secret-value"));
    assert_eq!(supervisor.state(&id("api")), ServiceState::Starting);
    assert_eq!(
        supervisor.session_status(&id("api")),
        SessionStatus::Starting
    );
}

#[test]
fn health_probes_are_off_the_action_hot_path() {
    let authority = FakeLaunchAuthority::new();
    let mut supervisor = supervisor_from_catalog(
        vec![service(
            "api",
            ServiceScope::task(task_a()),
            vec![],
            Some(8080),
        )],
        authority,
    );
    supervisor
        .handle(
            SupervisorAction::Start,
            &id("api"),
            fence(),
            AdmissionRequester::Task(task_a()),
        )
        .expect("start");
    assert_eq!(supervisor.probe_executions(), 0);
    let due = supervisor.due_probes();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].generation, 1);
    become_healthy(&mut supervisor, "api");
    assert_eq!(supervisor.state(&id("api")), ServiceState::Healthy);
    assert_eq!(
        supervisor.session_status(&id("api")),
        SessionStatus::Running
    );
    assert_eq!(supervisor.probe_executions(), 2);
    let health = supervisor
        .handle(
            SupervisorAction::Health,
            &id("api"),
            fence(),
            AdmissionRequester::Task(task_a()),
        )
        .expect("health action");
    assert!(matches!(
        health,
        SupervisorOutcome::Health(snapshot) if snapshot.state == ServiceState::Healthy
    ));
    assert_eq!(supervisor.probe_executions(), 2);
}

#[test]
fn crash_and_manual_stop_and_task_close_leave_no_orphan() {
    let authority = FakeLaunchAuthority::new();
    let inspect = authority.clone();
    let mut supervisor = supervisor_from_catalog(
        vec![
            service("db", ServiceScope::task(task_a()), vec![], Some(5432)),
            service(
                "api",
                ServiceScope::task(task_a()),
                vec![id("db")],
                Some(8080),
            ),
        ],
        authority,
    );
    supervisor
        .handle(
            SupervisorAction::Start,
            &id("api"),
            fence(),
            AdmissionRequester::Task(task_a()),
        )
        .expect("start graph");
    assert_eq!(inspect.resumed(), 2);
    supervisor
        .report_exit(&id("api"), 1, Some(1))
        .expect("crash");
    assert_eq!(supervisor.state(&id("api")), ServiceState::Failed);
    assert_eq!(supervisor.session_status(&id("api")), SessionStatus::Failed);
    supervisor
        .handle(
            SupervisorAction::Stop,
            &id("db"),
            fence(),
            AdmissionRequester::Task(task_a()),
        )
        .expect("manual stop");
    assert_eq!(supervisor.state(&id("db")), ServiceState::Stopped);
    assert_eq!(supervisor.live_count(), 0);
    assert_eq!(inspect.torn_down(), 1);

    let authority = FakeLaunchAuthority::new();
    let inspect = authority.clone();
    let mut supervisor = supervisor_from_catalog(
        vec![service(
            "api",
            ServiceScope::task(task_a()),
            vec![],
            Some(8080),
        )],
        authority,
    );
    supervisor
        .handle(
            SupervisorAction::Start,
            &id("api"),
            fence(),
            AdmissionRequester::Task(task_a()),
        )
        .expect("start");
    become_healthy(&mut supervisor, "api");
    supervisor.close_task(task_a(), 1).expect("task close");
    assert_eq!(supervisor.state(&id("api")), ServiceState::Stopped);
    assert_eq!(inspect.torn_down(), 1);
    assert_eq!(supervisor.residue_count(), 0);
}

#[test]
fn host_owned_service_starts_through_host_requester() {
    let authority = FakeLaunchAuthority::new();
    let mut supervisor = supervisor_from_catalog(
        vec![service("host-db", ServiceScope::Host, vec![], Some(5432))],
        authority,
    );
    supervisor
        .handle(
            SupervisorAction::Start,
            &id("host-db"),
            fence(),
            AdmissionRequester::Host(HostAuthority::new(host())),
        )
        .expect("host start");
    assert_eq!(supervisor.state(&id("host-db")), ServiceState::Starting);
    assert!(matches!(
        supervisor.handle(
            SupervisorAction::Start,
            &id("host-db"),
            fence(),
            AdmissionRequester::Task(task_a()),
        ),
        Err(SupervisorError::Refused(_))
    ));
}

#[test]
fn duplicate_start_coalesces_without_a_second_launch() {
    let authority = FakeLaunchAuthority::new();
    let inspect = authority.clone();
    let mut supervisor = supervisor_from_catalog(
        vec![service(
            "api",
            ServiceScope::task(task_a()),
            vec![],
            Some(8080),
        )],
        authority,
    );
    supervisor
        .handle(
            SupervisorAction::Start,
            &id("api"),
            fence(),
            AdmissionRequester::Task(task_a()),
        )
        .expect("start");
    let again = supervisor
        .handle(
            SupervisorAction::Start,
            &id("api"),
            fence(),
            AdmissionRequester::Task(task_a()),
        )
        .expect("duplicate");
    assert!(matches!(again, SupervisorOutcome::Coalesced { .. }));
    assert_eq!(inspect.prepared(), 1);
    assert_eq!(inspect.resumed(), 1);
}

#[test]
fn occupied_external_port_is_blue_and_uncontrolled() {
    let authority = FakeLaunchAuthority::new();
    let inspect = authority.clone();
    let mut supervisor = supervisor_from_catalog(
        vec![service(
            "api",
            ServiceScope::task(task_a()),
            vec![],
            Some(8080),
        )],
        authority,
    );
    supervisor.observe_port(8080, PortAuthority::ProvenExternal, Some(9001));
    assert_eq!(supervisor.state(&id("api")), ServiceState::External);
    assert_eq!(supervisor.state(&id("api")).tone(), StatusTone::Blue);
    assert_eq!(
        supervisor.session_status(&id("api")),
        SessionStatus::Stopped
    );
    assert!(matches!(
        supervisor.handle(
            SupervisorAction::Start,
            &id("api"),
            fence(),
            AdmissionRequester::Task(task_a()),
        ),
        Err(SupervisorError::Refused(_))
    ));
    assert_eq!(inspect.prepared(), 0);
    assert!(matches!(
        supervisor.handle(
            SupervisorAction::Stop,
            &id("api"),
            fence(),
            AdmissionRequester::Task(task_a()),
        ),
        Err(SupervisorError::Refused(_))
    ));
}

#[test]
fn dependent_failure_does_not_kill_an_unrelated_or_external_service() {
    let authority = FakeLaunchAuthority::new();
    let inspect = authority.clone();
    let mut supervisor = supervisor_from_catalog(
        vec![
            service("db", ServiceScope::task(task_a()), vec![], Some(5432)),
            service(
                "api",
                ServiceScope::task(task_a()),
                vec![id("db")],
                Some(8080),
            ),
            service("worker", ServiceScope::task(task_a()), vec![], Some(9090)),
        ],
        authority,
    );
    supervisor.observe_port(9090, PortAuthority::ProvenExternal, Some(42));
    supervisor
        .handle(
            SupervisorAction::Start,
            &id("db"),
            fence(),
            AdmissionRequester::Task(task_a()),
        )
        .expect("start db");
    become_healthy(&mut supervisor, "db");
    assert_eq!(supervisor.state(&id("db")), ServiceState::Healthy);
    inspect.fail_at(FakeFailStage::Resume);
    assert!(supervisor
        .handle(
            SupervisorAction::Start,
            &id("api"),
            fence(),
            AdmissionRequester::Task(task_a()),
        )
        .is_err());
    assert_eq!(supervisor.state(&id("api")), ServiceState::Failed);
    assert_eq!(supervisor.state(&id("db")), ServiceState::Healthy);
    assert_eq!(supervisor.state(&id("worker")), ServiceState::External);
    assert_eq!(supervisor.live_count(), 1);
    assert!(inspect.aborted() >= 1);
}

#[test]
fn launch_failure_and_supervisor_drop_leave_no_orphan() {
    let authority = FakeLaunchAuthority::new();
    let inspect = authority.clone();
    inspect.fail_at(FakeFailStage::Register);
    {
        let mut supervisor = supervisor_from_catalog(
            vec![service(
                "api",
                ServiceScope::task(task_a()),
                vec![],
                Some(8080),
            )],
            authority,
        );
        assert!(supervisor
            .handle(
                SupervisorAction::Start,
                &id("api"),
                fence(),
                AdmissionRequester::Task(task_a()),
            )
            .is_err());
        assert_eq!(supervisor.live_count(), 0);
        assert!(inspect.aborted() >= 1);
    }
    assert_eq!(inspect.residue_count(), 0);

    let authority = FakeLaunchAuthority::new();
    let inspect = authority.clone();
    {
        let mut supervisor = supervisor_from_catalog(
            vec![service(
                "api",
                ServiceScope::task(task_a()),
                vec![],
                Some(8080),
            )],
            authority,
        );
        supervisor
            .handle(
                SupervisorAction::Start,
                &id("api"),
                fence(),
                AdmissionRequester::Task(task_a()),
            )
            .expect("start");
        assert_eq!(inspect.resumed(), 1);
    }
    assert_eq!(inspect.live_count(), 0);
    assert_eq!(inspect.torn_down(), 1);
}

#[test]
fn stale_generation_is_ignored_and_logs_stay_redacted() {
    let (project, folder, command) = project_fixture();
    let mut workspace_env = BTreeMap::new();
    workspace_env.insert("API_TOKEN".to_owned(), "secret-value".to_owned());
    let binding = bind_configured_command(ConfiguredServiceSource {
        project: &project,
        folder: &folder,
        command: &command,
        owner: ConfiguredServiceOwner::Task { task_id: task_a() },
        workspace_cwd: Some("apps/api"),
        workspace_env: &workspace_env,
    })
    .expect("binding");
    let mut supervisor =
        ServiceSupervisor::from_bindings(vec![binding], FakeLaunchAuthority::new(), host(), 1_000)
            .expect("supervisor");
    supervisor
        .handle(
            SupervisorAction::Start,
            &id("api"),
            fence(),
            AdmissionRequester::Task(task_a()),
        )
        .expect("start");
    assert!(matches!(
        supervisor.apply_probe(&id("api"), 99, ProbeOutcome::Success),
        Err(SupervisorError::StaleGeneration {
            expected: 1,
            received: 99
        })
    ));
    assert!(matches!(
        supervisor.handle(
            SupervisorAction::Start,
            &id("api"),
            AdmissionFence::new(2, 1, 1),
            AdmissionRequester::Task(task_a()),
        ),
        Err(SupervisorError::Refused(_))
    ));
    let logs = match supervisor
        .handle(
            SupervisorAction::Logs,
            &id("api"),
            fence(),
            AdmissionRequester::Task(task_a()),
        )
        .expect("logs")
    {
        SupervisorOutcome::Logs(logs) => logs,
        other => panic!("expected logs, got {other:?}"),
    };
    let rendered = format!("{logs:?}");
    assert!(!rendered.contains("secret-value"));
    assert!(supervisor
        .events()
        .any(|event| event.kind == SupervisorEventKind::Started));
}

#[test]
fn ui_session_status_preserves_existing_server_semantics() {
    assert_eq!(
        session_status_for_ui(ServiceState::Stopped),
        SessionStatus::Stopped
    );
    assert_eq!(
        session_status_for_ui(ServiceState::Starting),
        SessionStatus::Starting
    );
    assert_eq!(
        session_status_for_ui(ServiceState::Healthy),
        SessionStatus::Running
    );
    assert_eq!(
        session_status_for_ui(ServiceState::Unhealthy),
        SessionStatus::Running
    );
    assert_eq!(
        session_status_for_ui(ServiceState::External),
        SessionStatus::Stopped
    );
    assert_eq!(
        session_status_for_ui(ServiceState::Stopping),
        SessionStatus::Stopping
    );
    assert_eq!(
        session_status_for_ui(ServiceState::Failed),
        SessionStatus::Failed
    );
}

#[test]
fn bind_configured_services_rejects_duplicates() {
    let (project, folder, command) = project_fixture();
    let env = BTreeMap::new();
    let source = ConfiguredServiceSource {
        project: &project,
        folder: &folder,
        command: &command,
        owner: ConfiguredServiceOwner::Project {
            project_id: project.id.clone(),
        },
        workspace_cwd: Some("apps/api"),
        workspace_env: &env,
    };
    assert!(bind_configured_services([source.clone(), source]).is_err());
}
