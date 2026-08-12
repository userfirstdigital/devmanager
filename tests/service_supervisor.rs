//! Phase 6 Task 6.7a pure service contract tests.
//!
//! These tests deliberately stop at validated plans, scheduled observations,
//! and reducer snapshots. They never launch a process or probe a live port.

use std::collections::VecDeque;

use devmanager::domain::TaskId;
use devmanager::services::health::{
    reduce_service, EvidenceProvenance, EvidenceSource, FakeClock, HealthAxis, HealthTracker,
    LifecycleAxis, OwnershipAxis, PortAxis, ProbeOutcome, ProcessAxis, ServiceEvidence,
    ServiceState,
};
use devmanager::services::model::{
    CommandSpec, ExpectedPort, HealthPolicy, HealthSpec, PortProtocol, ServiceCatalog,
    ServiceDefinition, ServiceId, ServiceScope, StartupPolicy, StopPolicy, ValidationError,
    ValidationField, MAX_ARGUMENT_COUNT, MAX_DEPENDENCY_COUNT, MAX_SERVICE_CATALOG_FRAME_BYTES,
    MAX_SERVICE_CATALOG_JSON_DEPTH, MAX_SERVICE_CATALOG_JSON_FIELD_NAME_BYTES, MAX_SERVICE_COUNT,
};

static_assertions::assert_not_impl_any!(ServiceCatalog: serde::Deserialize<'static>);

fn id(value: &str) -> ServiceId {
    ServiceId::new(value).expect("test service id")
}

fn task_a() -> TaskId {
    TaskId::parse("0198b6b0-0000-7000-8000-000000000001").expect("test task id")
}

fn task_scope(task_id: TaskId) -> ServiceScope {
    ServiceScope::task(task_id)
}

fn fixture_definition(index: usize) -> serde_json::Value {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/services/valid.json")).unwrap();
    fixture["services"][index].clone()
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

fn command() -> CommandSpec {
    CommandSpec::new("node")
        .expect("valid executable")
        .with_arg("server.js")
        .expect("valid argument")
        .with_cwd("apps/api")
        .expect("valid workspace path")
        .with_env_reference("PORT")
        .expect("valid environment reference")
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
        command: command(),
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

fn provenance(now_ms: u64) -> EvidenceProvenance {
    EvidenceProvenance {
        source: EvidenceSource::FakeProbe,
        observed_at_ms: now_ms,
        generation: Some(7),
        epoch: Some(3),
    }
}

fn evidence(
    lifecycle: LifecycleAxis,
    process: ProcessAxis,
    health: HealthAxis,
    port: PortAxis,
    ownership: OwnershipAxis,
) -> ServiceEvidence {
    ServiceEvidence {
        lifecycle,
        process,
        health,
        port,
        ownership,
        generation: 7,
        epoch: 3,
        observed_at_ms: 1_000,
        provenance: provenance(1_000),
    }
}

#[test]
fn configured_launch_fixture_is_validated_and_launch_intent_is_bounded() {
    let catalog = ServiceCatalog::decode_json(include_bytes!("fixtures/services/valid.json"))
        .expect("valid catalog fixture");

    let intent = catalog
        .launch_intent(&id("api"))
        .expect("api launch intent");
    assert_eq!(intent.service_id(), &id("api"));
    assert_eq!(intent.command().program().as_str(), "node");
    assert_eq!(intent.command().args()[0].as_str(), "server.js");
    assert_eq!(
        intent.command().cwd().map(|path| path.as_str()),
        Some("apps/api")
    );
    assert_eq!(intent.command().env()[0].name(), "PORT");
    assert_eq!(intent.expected_port().unwrap().port, 8080);

    let encoded = serde_json::to_vec(&catalog).unwrap();
    let encoded_value: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(encoded_value["schema_version"], serde_json::json!(1));
    let decoded = ServiceCatalog::decode_json(&encoded).expect("bounded catalog roundtrip");
    assert_eq!(serde_json::to_vec(&decoded).unwrap(), encoded);
}

#[test]
fn validation_rejects_unsafe_paths_raw_secrets_unbounded_args_and_duplicate_ids() {
    assert!(matches!(
        ServiceId::new(""),
        Err(ValidationError::Empty { .. })
    ));

    let mut unsafe_path = fixture_definition(0);
    unsafe_path["command"]["cwd"] = serde_json::json!("..\\secrets");
    assert!(serde_json::from_value::<ServiceDefinition>(unsafe_path).is_err());

    let mut raw_secret = fixture_definition(0);
    raw_secret["command"]["args"] = serde_json::json!(["--api-token=secret-value"]);
    assert!(serde_json::from_value::<ServiceDefinition>(raw_secret).is_err());

    let mut too_many_args = fixture_definition(0);
    too_many_args["command"]["args"] = serde_json::json!(vec!["arg"; MAX_ARGUMENT_COUNT + 1]);
    assert!(serde_json::from_value::<ServiceDefinition>(too_many_args).is_err());

    let duplicate = service("api", task_scope(task_a()), vec![], Some(8080));
    assert!(matches!(
        ServiceCatalog::new(vec![duplicate.clone(), duplicate]),
        Err(ValidationError::DuplicateServiceId { .. })
    ));

    let self_dependency = service("api", task_scope(task_a()), vec![id("api")], Some(8080));
    assert!(matches!(
        ServiceCatalog::new(vec![self_dependency]),
        Err(ValidationError::SelfDependency { .. })
    ));

    let unknown_dependency = service("api", task_scope(task_a()), vec![id("missing")], Some(8080));
    assert!(matches!(
        ServiceCatalog::new(vec![unknown_dependency]),
        Err(ValidationError::UnknownDependency { .. })
    ));
}

#[test]
fn wire_deserializers_validate_ids_catalog_schema_and_definitions() {
    assert!(serde_json::from_str::<ServiceId>(r#"\"\""#).is_err());

    let mut fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/services/valid.json")).unwrap();
    fixture["services"][0]["id"] = serde_json::json!("../escape");
    let definition = serde_json::from_value::<ServiceDefinition>(fixture["services"][0].clone());
    assert!(definition.is_err());

    let mut unknown_field = serde_json::json!({
        "schema_version": 1,
        "services": [],
        "unexpected": true
    });
    assert!(ServiceCatalog::decode_json(&serde_json::to_vec(&unknown_field).unwrap()).is_err());

    unknown_field = serde_json::json!({
        "schema_version": 2,
        "services": []
    });
    assert!(ServiceCatalog::decode_json(&serde_json::to_vec(&unknown_field).unwrap()).is_err());

    let mut unbounded = fixture_definition(0);
    unbounded["command"]["program"] = serde_json::json!("x".repeat(257));
    assert!(serde_json::from_value::<ServiceDefinition>(unbounded).is_err());

    let mut unbounded_dependencies = fixture_definition(0);
    unbounded_dependencies["dependencies"] =
        serde_json::json!(vec!["api"; MAX_DEPENDENCY_COUNT + 1]);
    assert!(serde_json::from_value::<ServiceDefinition>(unbounded_dependencies).is_err());

    let services = vec![fixture_definition(0); MAX_SERVICE_COUNT + 1];
    let unbounded_catalog = serde_json::json!({
        "schema_version": 1,
        "services": services
    });
    assert!(ServiceCatalog::decode_json(&serde_json::to_vec(&unbounded_catalog).unwrap()).is_err());
}

#[test]
fn nested_contract_types_reject_unknown_fields_invalid_values_and_secret_debug_output() {
    let debug = format!("{:?}", command());
    assert!(!debug.contains("secret-value"));
    assert!(serde_json::from_value::<CommandSpec>(serde_json::json!({
        "program": "node",
        "args": [],
        "cwd": null,
        "env": [],
        "unexpected": true
    }))
    .is_err());
    assert!(serde_json::from_value::<CommandSpec>(serde_json::json!({
        "program": "node",
        "args": ["--token", "secret-value"],
        "cwd": null,
        "env": []
    }))
    .is_err());

    assert!(serde_json::from_value::<ServiceScope>(serde_json::json!({
        "Task": { "task_id": "../task" }
    }))
    .is_err());
    assert!(serde_json::from_value::<HealthPolicy>(serde_json::json!({
        "startup_deadline_ms": 0,
        "probe_interval_ms": 1000,
        "max_probe_interval_ms": 1000,
        "backoff_multiplier": 1,
        "success_threshold": 1,
        "failure_threshold": 1,
        "stale_after_ms": 1000,
        "unexpected": true
    }))
    .is_err());
    assert!(serde_json::from_value::<ExpectedPort>(serde_json::json!({
        "protocol": "Tcp",
        "port": 0
    }))
    .is_err());
}

#[test]
fn command_cwd_accepts_only_canonical_workspace_relative_paths() {
    for cwd in ["apps\\api", "apps//api", "apps/./api", " apps/api"] {
        let mut definition = fixture_definition(0);
        definition["command"]["cwd"] = serde_json::json!(cwd);
        assert!(serde_json::from_value::<ServiceDefinition>(definition).is_err());
    }
}

#[test]
fn secret_flags_and_assignments_are_rejected_structurally_but_name_only_refs_are_safe() {
    for args in [
        vec!["--token", "raw-secret"],
        vec!["--api-key=raw-secret"],
        vec!["--secret", "raw-secret"],
        vec!["--password=raw-secret"],
        vec!["--private-key", "raw-secret"],
        vec!["--env=API_TOKEN=raw-secret"],
        vec!["--env", "API_TOKEN=raw-secret"],
        vec!["--set-env=TOKEN=raw-secret"],
        vec!["--set-env", "TOKEN=raw-secret"],
    ] {
        let mut definition = fixture_definition(0);
        definition["command"]["args"] = serde_json::json!(args);
        let error = serde_json::from_value::<ServiceDefinition>(definition)
            .expect_err("raw secret argument must be rejected");
        assert!(!error.to_string().contains("raw-secret"));
    }

    let mut assignment = fixture_definition(0);
    assignment["command"]["env"] = serde_json::json!([{ "name": "API_TOKEN=raw-secret" }]);
    assert!(serde_json::from_value::<ServiceDefinition>(assignment).is_err());

    let mut reference = fixture_definition(0);
    reference["command"]["env"] = serde_json::json!([{ "name": "API_TOKEN" }]);
    assert!(serde_json::from_value::<ServiceDefinition>(reference).is_ok());
}

#[test]
fn inline_secret_option_names_reject_following_raw_values_without_leaking_diagnostics() {
    const RAW_SECRET: &str = "RAW_SECRET_SENTINEL";
    let cases = [
        ("--env=TOKEN", "TOKEN"),
        ("--set-env=TOKEN", "TOKEN"),
        ("--env-var=TOKEN", "TOKEN"),
        ("--set-env-var=TOKEN", "TOKEN"),
        ("--ENV=token", "token"),
        ("--set_env=API_KEY", "API_KEY"),
        ("--ENV_VAR=ACCESS-KEY", "ACCESS-KEY"),
        ("--SET_ENV_VAR=PRIVATE_KEY", "PRIVATE_KEY"),
        ("--set-env_var=api-key", "api-key"),
        ("--env-var=access_key", "access_key"),
        ("--SET-ENV-VAR=private-key", "private-key"),
    ];

    for (option, key) in cases {
        let error = CommandSpec::new("node")
            .expect("valid command")
            .with_args([option.to_string(), RAW_SECRET.to_string()])
            .expect_err("inline secret option assignment must reject its raw value");
        assert!(matches!(
            error,
            ValidationError::RawSecret {
                field: ValidationField::Argument
            }
        ));
        let diagnostic = format!("{error:?} {error}");
        assert!(!diagnostic.contains(key));
        assert!(!diagnostic.contains(RAW_SECRET));
    }
}

#[test]
fn inline_nonsecret_option_names_continue_to_accept_following_values() {
    for args in [
        vec!["--env=PORT", "8080"],
        vec!["--set_env=DEBUG", "true"],
        vec!["--ENV_VAR=APP_MODE", "development"],
        vec!["--SET-ENV-VAR=LOG_LEVEL", "info"],
    ] {
        CommandSpec::new("node")
            .expect("valid command")
            .with_args(args)
            .expect("nonsecret inline option assignment should remain valid");
    }
}

#[test]
fn expected_port_must_match_health_or_be_derived_from_health() {
    let mut mismatch = service("api", task_scope(task_a()), vec![], Some(8080));
    mismatch.expected_port = Some(ExpectedPort {
        protocol: PortProtocol::Tcp,
        port: 9090,
    });
    assert!(ServiceCatalog::new(vec![mismatch]).is_err());

    let mut derived = service("api", task_scope(task_a()), vec![], Some(8080));
    derived.expected_port = None;
    let catalog = ServiceCatalog::new(vec![derived]).unwrap();
    assert_eq!(
        catalog
            .launch_intent(&id("api"))
            .unwrap()
            .expected_port()
            .unwrap()
            .port,
        8080
    );
}

#[test]
fn dependency_plan_is_deterministic_and_cycles_are_explicit() {
    let catalog = ServiceCatalog::new(vec![
        service("api", task_scope(task_a()), vec![id("db")], Some(8080)),
        service("db", task_scope(task_a()), vec![], Some(5432)),
    ])
    .unwrap();
    let plan = catalog.dependency_plan(&id("api")).unwrap();
    assert_eq!(plan.services(), &[id("db"), id("api")]);

    let cycle = ServiceCatalog::new(vec![
        service("api", task_scope(task_a()), vec![id("db")], Some(8080)),
        service("db", task_scope(task_a()), vec![id("api")], Some(5432)),
    ]);
    assert!(matches!(
        cycle,
        Err(ValidationError::DependencyCycle { ref path }) if path == &vec![id("api"), id("db"), id("api")]
    ));
}

#[test]
fn health_tracker_uses_fake_clock_thresholds_backoff_and_stale_evidence() {
    let mut clock = FakeClock::new(1_000);
    let mut tracker = HealthTracker::new(policy());
    tracker.start(clock.now_ms(), 7).unwrap();
    assert!(tracker.schedule().is_due(clock.now_ms()));
    assert!(matches!(tracker.axis(), HealthAxis::Pending { .. }));

    let mut probe = VecDeque::from([ProbeOutcome::Success, ProbeOutcome::Success]);
    tracker
        .record_probe(
            clock.now_ms(),
            7,
            probe.pop_front().unwrap(),
            EvidenceSource::FakeProbe,
        )
        .unwrap();
    assert!(matches!(tracker.axis(), HealthAxis::Pending { .. }));

    clock.advance_ms(1_000);
    tracker
        .record_probe(
            clock.now_ms(),
            7,
            probe.pop_front().unwrap(),
            EvidenceSource::FakeProbe,
        )
        .unwrap();
    assert!(matches!(
        tracker.axis(),
        HealthAxis::Healthy {
            last_probe_at_ms: 2_000
        }
    ));

    clock.advance_ms(2_500);
    tracker.advance(clock.now_ms(), 7).unwrap();
    assert!(matches!(
        tracker.axis(),
        HealthAxis::Stale {
            last_probe_at_ms: Some(2_000)
        }
    ));
}

#[test]
fn health_tracker_counts_consecutive_failures_and_rejects_stale_generation() {
    let mut tracker = HealthTracker::new(policy());
    tracker.start(0, 7).unwrap();
    tracker
        .record_probe(0, 7, ProbeOutcome::Failure, EvidenceSource::FakeProbe)
        .unwrap();
    assert!(matches!(tracker.axis(), HealthAxis::Pending { .. }));

    tracker
        .record_probe(2_000, 7, ProbeOutcome::Failure, EvidenceSource::FakeProbe)
        .unwrap();
    assert!(matches!(
        tracker.axis(),
        HealthAxis::Unhealthy {
            last_probe_at_ms: 2_000
        }
    ));

    assert!(matches!(
        tracker.record_probe(3_000, 6, ProbeOutcome::Success, EvidenceSource::FakeProbe),
        Err(devmanager::services::health::HealthError::StaleGeneration { .. })
    ));
}

#[test]
fn health_tracker_marks_startup_deadline_unhealthy_without_a_probe() {
    let mut tracker = HealthTracker::new(policy());
    tracker.start(100, 7).unwrap();
    tracker.advance(5_100, 7).unwrap();
    assert!(matches!(
        tracker.axis(),
        HealthAxis::Unhealthy {
            last_probe_at_ms: 100
        }
    ));
}

#[test]
fn reducer_keeps_lifecycle_process_health_port_and_ownership_separate() {
    assert_eq!(
        reduce_service(&evidence(
            LifecycleAxis::Running,
            ProcessAxis::Running { generation: 7 },
            HealthAxis::Pending {
                next_probe_at_ms: Some(1_000),
            },
            PortAxis::Owned { port: 8080 },
            OwnershipAxis::Task {
                task_id: "task-a".to_owned(),
            },
        )),
        ServiceState::Starting
    );

    assert_eq!(
        reduce_service(&evidence(
            LifecycleAxis::Running,
            ProcessAxis::Running { generation: 7 },
            HealthAxis::Healthy {
                last_probe_at_ms: 1_000,
            },
            PortAxis::Owned { port: 8080 },
            OwnershipAxis::Task {
                task_id: "task-a".to_owned(),
            },
        )),
        ServiceState::Healthy
    );

    assert_eq!(
        reduce_service(&evidence(
            LifecycleAxis::Running,
            ProcessAxis::Running { generation: 7 },
            HealthAxis::Unhealthy {
                last_probe_at_ms: 1_000,
            },
            PortAxis::Owned { port: 8080 },
            OwnershipAxis::Task {
                task_id: "task-a".to_owned(),
            },
        )),
        ServiceState::Unhealthy
    );

    assert_eq!(
        reduce_service(&evidence(
            LifecycleAxis::Running,
            ProcessAxis::Running { generation: 7 },
            HealthAxis::Healthy {
                last_probe_at_ms: 1_000,
            },
            PortAxis::External {
                port: 8080,
                owner_pid: Some(9001),
            },
            OwnershipAxis::External,
        )),
        ServiceState::External
    );

    assert_eq!(
        reduce_service(&evidence(
            LifecycleAxis::Running,
            ProcessAxis::Crashed { generation: 7 },
            HealthAxis::Unknown,
            PortAxis::Free,
            OwnershipAxis::Task {
                task_id: "task-a".to_owned(),
            },
        )),
        ServiceState::Failed
    );

    assert_eq!(
        reduce_service(&evidence(
            LifecycleAxis::Stopping,
            ProcessAxis::Running { generation: 7 },
            HealthAxis::Cancelled,
            PortAxis::Owned { port: 8080 },
            OwnershipAxis::Task {
                task_id: "task-a".to_owned(),
            },
        )),
        ServiceState::Stopping
    );

    assert_eq!(
        reduce_service(&evidence(
            LifecycleAxis::Stopped,
            ProcessAxis::Exited { exit_code: Some(0) },
            HealthAxis::Cancelled,
            PortAxis::Free,
            OwnershipAxis::None,
        )),
        ServiceState::Stopped
    );
}

#[test]
fn command_debug_redacts_program_cwd_env_names_and_argument_values() {
    let command = command();
    let debug = format!("{command:?}");
    for secret in ["node", "server.js", "apps/api", "PORT"] {
        assert!(!debug.contains(secret), "debug leaked {secret:?}: {debug}");
    }
}

#[test]
fn service_definition_roundtrip_uses_canonical_dependency_order() {
    let definition = service(
        "api",
        task_scope(task_a()),
        vec![id("worker"), id("db")],
        Some(8080),
    );
    let encoded = serde_json::to_value(&definition).unwrap();
    let decoded: ServiceDefinition = serde_json::from_value(encoded).unwrap();
    let canonical = serde_json::to_value(decoded).unwrap();
    assert_eq!(
        canonical["dependencies"],
        serde_json::json!(["db", "worker"])
    );
}

#[test]
fn catalog_wire_is_strict_and_fingerprint_is_order_independent() {
    let first = ServiceCatalog::new(vec![
        service("api", task_scope(task_a()), vec![id("db")], Some(8080)),
        service("db", task_scope(task_a()), vec![], Some(5432)),
    ])
    .unwrap();
    let second = ServiceCatalog::new(vec![
        service("db", task_scope(task_a()), vec![], Some(5432)),
        service("api", task_scope(task_a()), vec![id("db")], Some(8080)),
    ])
    .unwrap();
    assert_eq!(first.fingerprint(), second.fingerprint());

    let omitted = serde_json::json!({ "services": [] });
    assert!(ServiceCatalog::decode_json(&serde_json::to_vec(&omitted).unwrap()).is_err());
    assert!(ServiceCatalog::decode_json(
        br#"{"schema_version":1,"schema_version":1,"services":[]}"#
    )
    .is_err());
}

#[test]
fn catalog_decode_rejects_oversized_frames_and_escaped_strings_before_serde() {
    let oversized_frame = vec![b' '; MAX_SERVICE_CATALOG_FRAME_BYTES + 1];
    assert!(ServiceCatalog::decode_json(&oversized_frame).is_err());

    let escaped = r#"\u0061"#.repeat(2_000);
    let input = format!(
        r#"{{"schema_version":1,"services":[{{"id":"api","scope":"Host","command":{{"program":"node","args":["{escaped}"],"cwd":null,"env":[]}},"dependencies":[],"health":"None","startup":{{"mode":"manual","startup_deadline_ms":1000}},"stop":{{"graceful_timeout_ms":1000,"kill_after_ms":10000}},"expected_port":null}}]}}"#
    );
    let error =
        ServiceCatalog::decode_json(input.as_bytes()).expect_err("escaped oversized string");
    assert!(!format!("{error:?}").contains("aaaa"));
}

#[test]
fn catalog_decode_accepts_the_versioned_valid_fixture() {
    let decoded = ServiceCatalog::decode_json(include_bytes!("fixtures/services/valid.json"))
        .expect("valid catalog fixture should cross the production decode boundary");
    assert_eq!(decoded.definitions().count(), 2);
}

#[test]
fn catalog_decode_preflights_malicious_field_names_arrays_and_depth() {
    let field_name = "x".repeat(MAX_SERVICE_CATALOG_JSON_FIELD_NAME_BYTES + 1);
    let malicious_field = format!(r#"{{"schema_version":1,"services":[],"{field_name}":null}}"#);
    assert!(ServiceCatalog::decode_json(malicious_field.as_bytes()).is_err());

    let too_many_services = format!(
        r#"{{"schema_version":1,"services":[{}]}}"#,
        std::iter::repeat_n("null", 129)
            .collect::<Vec<_>>()
            .join(",")
    );
    assert!(ServiceCatalog::decode_json(too_many_services.as_bytes()).is_err());

    let nested = "[".repeat(MAX_SERVICE_CATALOG_JSON_DEPTH + 1)
        + "null"
        + &"]".repeat(MAX_SERVICE_CATALOG_JSON_DEPTH + 1);
    assert!(ServiceCatalog::decode_json(nested.as_bytes()).is_err());
}

#[test]
fn public_supervisor_api_exposes_production_authority_without_private_errors() {
    use devmanager::services::{
        HostManagedLaunchAuthority, ManagedLaunchAuthority, ManagedLaunchStage, SupervisorError,
        SupervisorRefusal,
    };
    use devmanager::ui::{project_services_panel, ServicePanelTone};

    let authority = HostManagedLaunchAuthority::new();
    assert_eq!(ManagedLaunchAuthority::live_count(&authority), 0);
    assert_eq!(ManagedLaunchAuthority::residue_count(&authority), 0);
    let _ = ManagedLaunchStage::Prepare;
    let refusal = SupervisorError::Refused(SupervisorRefusal::External);
    assert!(!format!("{refusal}").contains("AdmissionRejection"));
    let panel = project_services_panel(&[], &[]);
    assert!(panel.rows.is_empty());
    assert_eq!(
        ServicePanelTone::from(devmanager::services::StatusTone::Green),
        ServicePanelTone::Green
    );
}

#[test]
fn disabled_health_finishes_start_as_healthy_and_allows_stop_restart() {
    use devmanager::services::model::{
        AdmissionFence, AdmissionRequester, CommandSpec, HealthSpec, HostId, ServiceCatalog,
        ServiceDefinition, ServiceScope, StartupPolicy, StopPolicy,
    };
    use devmanager::services::{FakeLaunchAuthority, ServiceSupervisor, SupervisorAction};
    use std::collections::BTreeMap;

    let definition = ServiceDefinition {
        id: id("worker"),
        scope: ServiceScope::Host,
        command: CommandSpec::new("node")
            .unwrap()
            .with_arg("worker.js")
            .unwrap(),
        dependencies: Vec::new(),
        health: HealthSpec::None,
        startup: StartupPolicy::manual(),
        stop: StopPolicy::default(),
        expected_port: None,
    };
    let catalog = ServiceCatalog::new(vec![definition]).unwrap();
    let authority = FakeLaunchAuthority::new();
    let inspect = authority.clone();
    let mut roots = BTreeMap::new();
    roots.insert(id("worker"), "C:/configured/workspace".to_owned());
    let mut supervisor = ServiceSupervisor::from_catalog_with_workspace_roots(
        catalog,
        BTreeMap::new(),
        BTreeMap::new(),
        roots,
        authority,
        HostId::new(1),
        1_000,
    )
    .unwrap();
    supervisor
        .handle(
            SupervisorAction::Start,
            &id("worker"),
            AdmissionFence::new(1, 1, 1),
            AdmissionRequester::Host(devmanager::services::model::HostAuthority::new(
                HostId::new(1),
            )),
        )
        .expect("disabled-health start");
    assert_eq!(supervisor.state(&id("worker")), ServiceState::Healthy);
    let fence = supervisor.fence(&id("worker")).unwrap();
    supervisor
        .handle(
            SupervisorAction::Stop,
            &id("worker"),
            fence,
            AdmissionRequester::Host(devmanager::services::model::HostAuthority::new(
                HostId::new(1),
            )),
        )
        .expect("disabled-health stop");
    assert_eq!(supervisor.state(&id("worker")), ServiceState::Stopped);
    assert_eq!(inspect.torn_down(), 1);
}

#[test]
fn stop_tears_down_live_generation_before_advancing_projected_fence() {
    use devmanager::services::model::{
        AdmissionFence, AdmissionRequester, CommandSpec, ExpectedPort, HealthPolicy, HealthSpec,
        HostAuthority, HostId, PortProtocol, ServiceCatalog, ServiceDefinition, ServiceScope,
        StartupPolicy, StopPolicy,
    };
    use devmanager::services::{FakeLaunchAuthority, ServiceSupervisor, SupervisorAction};
    use std::collections::BTreeMap;

    let policy = HealthPolicy {
        startup_deadline_ms: 5_000,
        probe_interval_ms: 1_000,
        max_probe_interval_ms: 4_000,
        backoff_multiplier: 2,
        success_threshold: 1,
        failure_threshold: 2,
        stale_after_ms: 2_500,
    };
    let definition = ServiceDefinition {
        id: id("api"),
        scope: ServiceScope::Host,
        command: CommandSpec::new("node")
            .unwrap()
            .with_arg("server.js")
            .unwrap(),
        dependencies: Vec::new(),
        health: HealthSpec::Tcp { port: 8080, policy },
        startup: StartupPolicy::manual(),
        stop: StopPolicy::default(),
        expected_port: Some(ExpectedPort {
            protocol: PortProtocol::Tcp,
            port: 8080,
        }),
    };
    let catalog = ServiceCatalog::new(vec![definition]).unwrap();
    let authority = FakeLaunchAuthority::new();
    let inspect = authority.clone();
    let mut roots = BTreeMap::new();
    roots.insert(id("api"), "C:/configured/workspace".to_owned());
    let mut supervisor = ServiceSupervisor::from_catalog_with_workspace_roots(
        catalog,
        BTreeMap::new(),
        BTreeMap::new(),
        roots,
        authority,
        HostId::new(1),
        1_000,
    )
    .unwrap();
    let host = AdmissionRequester::Host(HostAuthority::new(HostId::new(1)));
    supervisor
        .handle(
            SupervisorAction::Start,
            &id("api"),
            AdmissionFence::new(1, 1, 1),
            host.clone(),
        )
        .expect("start");
    let live_fence = supervisor.fence(&id("api")).unwrap();
    assert_eq!(live_fence.resource_generation(), 2);
    supervisor
        .apply_probe(&id("api"), 2, ProbeOutcome::Success)
        .unwrap();
    assert_eq!(supervisor.state(&id("api")), ServiceState::Healthy);
    supervisor
        .handle(SupervisorAction::Stop, &id("api"), live_fence, host)
        .expect("stop must tear down using live generation");
    assert_eq!(inspect.torn_down(), 1);
    assert_eq!(supervisor.live_count(), 0);
    assert_eq!(
        supervisor.fence(&id("api")).unwrap().resource_generation(),
        3
    );
    assert_eq!(supervisor.state(&id("api")), ServiceState::Stopped);
}

#[test]
fn resume_failure_settles_without_orphan_and_projects_failed_fence() {
    use devmanager::services::model::{
        AdmissionFence, AdmissionRequester, CommandSpec, HealthSpec, HostAuthority, HostId,
        ServiceCatalog, ServiceDefinition, ServiceScope, StartupPolicy, StopPolicy,
    };
    use devmanager::services::{
        FakeFailStage, FakeLaunchAuthority, ServiceSupervisor, SupervisorAction, SupervisorError,
    };
    use std::collections::BTreeMap;

    let definition = ServiceDefinition {
        id: id("api"),
        scope: ServiceScope::Host,
        command: CommandSpec::new("node").unwrap(),
        dependencies: Vec::new(),
        health: HealthSpec::None,
        startup: StartupPolicy::manual(),
        stop: StopPolicy::default(),
        expected_port: None,
    };
    let catalog = ServiceCatalog::new(vec![definition]).unwrap();
    let authority = FakeLaunchAuthority::new();
    let inspect = authority.clone();
    inspect.fail_at(FakeFailStage::Resume);
    let mut roots = BTreeMap::new();
    roots.insert(id("api"), "C:/configured/workspace".to_owned());
    let mut supervisor = ServiceSupervisor::from_catalog_with_workspace_roots(
        catalog,
        BTreeMap::new(),
        BTreeMap::new(),
        roots,
        authority,
        HostId::new(1),
        1_000,
    )
    .unwrap();
    let err = supervisor
        .handle(
            SupervisorAction::Start,
            &id("api"),
            AdmissionFence::new(1, 1, 1),
            AdmissionRequester::Host(HostAuthority::new(HostId::new(1))),
        )
        .expect_err("resume failure");
    assert!(matches!(
        err,
        SupervisorError::Launch {
            stage: devmanager::services::ManagedLaunchStage::Resume
        }
    ));
    assert!(inspect.aborted() >= 1);
    assert_eq!(supervisor.live_count(), 0);
    assert_eq!(supervisor.residue_count(), 0);
    assert_eq!(supervisor.state(&id("api")), ServiceState::Failed);
    assert_eq!(
        supervisor.fence(&id("api")).unwrap().resource_generation(),
        2
    );
}

#[test]
fn unknown_and_probe_error_ports_fail_closed_and_port_busy_projects_fence() {
    use devmanager::process::ports::PortAuthority;
    use devmanager::services::model::{
        AdmissionFence, AdmissionRequester, CommandSpec, ExpectedPort, HealthPolicy, HealthSpec,
        HostAuthority, HostId, PortProtocol, ServiceCatalog, ServiceDefinition, ServiceScope,
        StartupPolicy, StopPolicy,
    };
    use devmanager::services::{
        FakeLaunchAuthority, PortClaimView, ServiceSupervisor, SupervisorAction, SupervisorError,
        SupervisorRefusal,
    };
    use std::collections::BTreeMap;

    let policy = HealthPolicy::default();
    let definition = ServiceDefinition {
        id: id("api"),
        scope: ServiceScope::Host,
        command: CommandSpec::new("node").unwrap(),
        dependencies: Vec::new(),
        health: HealthSpec::Tcp { port: 8080, policy },
        startup: StartupPolicy::manual(),
        stop: StopPolicy::default(),
        expected_port: Some(ExpectedPort {
            protocol: PortProtocol::Tcp,
            port: 8080,
        }),
    };
    let catalog = ServiceCatalog::new(vec![definition]).unwrap();
    let mut roots = BTreeMap::new();
    roots.insert(id("api"), "C:/configured/workspace".to_owned());
    let mut supervisor = ServiceSupervisor::from_catalog_with_workspace_roots(
        catalog,
        BTreeMap::new(),
        BTreeMap::new(),
        roots,
        FakeLaunchAuthority::new(),
        HostId::new(1),
        1_000,
    )
    .unwrap();
    supervisor.observe_port(8080, PortAuthority::Unknown, None);
    assert_eq!(
        supervisor.port_claim(8080),
        Some(PortClaimView::Indeterminate)
    );
    assert_eq!(supervisor.state(&id("api")), ServiceState::Unknown);
    let fence_before = supervisor.fence(&id("api")).unwrap();
    let err = supervisor
        .handle(
            SupervisorAction::Start,
            &id("api"),
            fence_before,
            AdmissionRequester::Host(HostAuthority::new(HostId::new(1))),
        )
        .expect_err("indeterminate port fails closed");
    assert!(matches!(
        err,
        SupervisorError::Refused(SupervisorRefusal::EvidenceUnknown)
    ));
    assert_eq!(supervisor.fence(&id("api")).unwrap(), fence_before);

    supervisor.observe_port(8080, PortAuthority::Free, None);
    assert_eq!(supervisor.state(&id("api")), ServiceState::Stopped);
    supervisor.observe_port(8080, PortAuthority::ProbeError, None);
    assert_eq!(
        supervisor.port_claim(8080),
        Some(PortClaimView::Indeterminate)
    );
}

#[test]
fn launch_cwd_resolves_from_configured_workspace_root_not_process_cwd() {
    use devmanager::config::{Nullable, Project, ProjectFolder, RunCommand};
    use devmanager::services::binding::{
        bind_configured_command, ConfiguredServiceOwner, ConfiguredServiceSource,
    };
    use devmanager::services::model::{AdmissionFence, AdmissionRequester, HostAuthority, HostId};
    use devmanager::services::{FakeLaunchAuthority, ServiceSupervisor, SupervisorAction};
    use std::collections::BTreeMap;

    let command = RunCommand {
        id: "api".to_owned(),
        label: "API".to_owned(),
        command: "node".to_owned(),
        args: vec!["server.js".to_owned()],
        env: Nullable::Absent,
        port: Nullable::Absent,
        auto_restart: Nullable::Value(false),
        ..RunCommand::default()
    };
    let folder = ProjectFolder {
        id: "web".to_owned(),
        name: "web".to_owned(),
        folder_path: "apps/api".to_owned(),
        commands: vec![command.clone()],
        ..ProjectFolder::default()
    };
    let project = Project {
        id: "proj".to_owned(),
        name: "proj".to_owned(),
        root_path: "C:/configured/workspace".to_owned(),
        folders: vec![folder.clone()],
        ..Project::default()
    };
    let binding = bind_configured_command(ConfiguredServiceSource {
        project: &project,
        folder: &folder,
        command: &command,
        owner: ConfiguredServiceOwner::Project {
            project_id: project.id.clone(),
        },
        folder_env_file: None,
    })
    .expect("binding");
    assert_eq!(binding.workspace_root, "C:/configured/workspace");
    assert_eq!(
        binding.definition.command.cwd().map(|cwd| cwd.as_str()),
        Some("apps/api")
    );
    let authority = FakeLaunchAuthority::new();
    let inspect = authority.clone();
    let mut supervisor =
        ServiceSupervisor::from_bindings(vec![binding], authority, HostId::new(1), 1_000).unwrap();
    supervisor
        .handle(
            SupervisorAction::Start,
            &id("api"),
            AdmissionFence::new(1, 1, 1),
            AdmissionRequester::Host(HostAuthority::new(HostId::new(1))),
        )
        .expect("start");
    let cwd = inspect.last_cwd().expect("launch cwd");
    assert!(
        cwd.contains("configured") && cwd.contains("apps"),
        "cwd must resolve under workspace root, got {cwd}"
    );
    assert!(
        !cwd.starts_with("apps/api") || cwd.starts_with("C:"),
        "must not use bare relative process cwd"
    );
}

#[test]
fn services_panel_disables_open_terminal_with_truthful_reason() {
    use devmanager::services::health::{
        EvidenceProvenance, EvidenceSource, HealthAxis, LifecycleAxis, OwnershipAxis, PortAxis,
        ProcessAxis, RedactedServiceSnapshot, ServiceEvidence,
    };
    use devmanager::services::model::ServiceScope;
    use devmanager::ui::{project_services_panel, ServicePanelAction};

    let evidence = ServiceEvidence {
        lifecycle: LifecycleAxis::Running,
        process: ProcessAxis::Running { generation: 2 },
        health: HealthAxis::Disabled,
        port: PortAxis::Free,
        ownership: OwnershipAxis::Host,
        generation: 2,
        epoch: 2,
        observed_at_ms: 1_000,
        provenance: EvidenceProvenance {
            source: EvidenceSource::ProcessRegistry,
            observed_at_ms: 1_000,
            generation: Some(2),
            epoch: Some(2),
        },
    };
    let snapshot = RedactedServiceSnapshot::from_evidence(id("api"), ServiceScope::Host, &evidence);
    assert_eq!(snapshot.state, ServiceState::Healthy);
    let panel = project_services_panel(&[snapshot], &[]);
    let open = panel.rows[0]
        .actions
        .iter()
        .find(|action| action.action == ServicePanelAction::OpenTerminal)
        .expect("OpenTerminal affordance");
    assert!(!open.enabled);
    assert_eq!(
        open.disabled_reason,
        Some("Service terminal attach is not available; use Logs")
    );
    assert!(ServicePanelAction::OpenTerminal
        .as_supervisor_action()
        .is_none());
}

#[test]
fn bare_program_resolution_pins_canonical_path_from_path_authority() {
    use devmanager::services::resolve_configured_service_program_with;
    use std::ffi::OsString;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("devmanager-service-path-{stamp}"));
    fs::create_dir_all(&dir).unwrap();
    let exe_name = if cfg!(windows) { "node.EXE" } else { "node" };
    let exe_path = dir.join(exe_name);
    fs::write(&exe_path, b"fake").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&exe_path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&exe_path, perms).unwrap();
    }
    let path_var = OsString::from(dir.as_os_str());
    let resolved = resolve_configured_service_program_with(
        "node",
        Some(path_var.as_os_str()),
        Some(".COM;.EXE;.BAT;.CMD"),
    )
    .expect("bare node resolves through PATH");
    assert!(
        std::path::Path::new(&resolved).is_absolute(),
        "resolved program must be absolute: {resolved}"
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn empty_or_relative_workspace_root_fails_closed_at_bind_and_task_override() {
    use devmanager::config::{Nullable, Project, ProjectFolder, RunCommand};
    use devmanager::services::binding::{
        bind_configured_command, with_task_workspace_root, BindingError, ConfiguredServiceOwner,
        ConfiguredServiceSource,
    };

    let command = RunCommand {
        id: "api".to_owned(),
        label: "API".to_owned(),
        command: "node".to_owned(),
        args: vec![],
        env: Nullable::Absent,
        port: Nullable::Absent,
        auto_restart: Nullable::Value(false),
        ..RunCommand::default()
    };
    let folder = ProjectFolder {
        id: "web".to_owned(),
        name: "web".to_owned(),
        folder_path: "apps/api".to_owned(),
        commands: vec![command.clone()],
        ..ProjectFolder::default()
    };
    let mut project = Project {
        id: "proj".to_owned(),
        name: "proj".to_owned(),
        root_path: "relative/root".to_owned(),
        folders: vec![folder.clone()],
        ..Project::default()
    };
    let err = bind_configured_command(ConfiguredServiceSource {
        project: &project,
        folder: &folder,
        command: &command,
        owner: ConfiguredServiceOwner::Project {
            project_id: project.id.clone(),
        },
        folder_env_file: None,
    })
    .expect_err("relative root");
    assert!(matches!(err, BindingError::InvalidWorkspaceRoot));

    project.root_path = String::new();
    assert!(matches!(
        bind_configured_command(ConfiguredServiceSource {
            project: &project,
            folder: &folder,
            command: &command,
            owner: ConfiguredServiceOwner::Project {
                project_id: project.id.clone(),
            },
            folder_env_file: None,
        }),
        Err(BindingError::InvalidWorkspaceRoot)
    ));

    project.root_path = "C:/configured/workspace".to_owned();
    let binding = bind_configured_command(ConfiguredServiceSource {
        project: &project,
        folder: &folder,
        command: &command,
        owner: ConfiguredServiceOwner::Task { task_id: task_a() },
        folder_env_file: None,
    })
    .expect("absolute project root");
    let overridden =
        with_task_workspace_root(binding, "C:/task/workspace").expect("absolute task root");
    assert_eq!(overridden.workspace_root, "C:/task/workspace");
    assert!(matches!(
        with_task_workspace_root(overridden, "relative/task"),
        Err(BindingError::InvalidWorkspaceRoot)
    ));
}

#[test]
fn pump_io_connects_reader_output_and_waiter_exit_to_logs_and_failed_state() {
    use devmanager::services::model::{
        AdmissionFence, AdmissionRequester, CommandSpec, HealthSpec, HostAuthority, HostId,
        ServiceCatalog, ServiceDefinition, ServiceScope, StartupPolicy, StopPolicy,
    };
    use devmanager::services::{FakeLaunchAuthority, ServiceSupervisor, SupervisorAction};
    use std::collections::BTreeMap;

    let definition = ServiceDefinition {
        id: id("api"),
        scope: ServiceScope::Host,
        command: CommandSpec::new("node").unwrap(),
        dependencies: Vec::new(),
        health: HealthSpec::None,
        startup: StartupPolicy::manual(),
        stop: StopPolicy::default(),
        expected_port: None,
    };
    let catalog = ServiceCatalog::new(vec![definition]).unwrap();
    let authority = FakeLaunchAuthority::new();
    let inspect = authority.clone();
    let mut roots = BTreeMap::new();
    roots.insert(id("api"), "C:/configured/workspace".to_owned());
    let mut supervisor = ServiceSupervisor::from_catalog_with_workspace_roots(
        catalog,
        BTreeMap::new(),
        BTreeMap::new(),
        roots,
        authority,
        HostId::new(1),
        1_000,
    )
    .unwrap();
    supervisor
        .handle(
            SupervisorAction::Start,
            &id("api"),
            AdmissionFence::new(1, 1, 1),
            AdmissionRequester::Host(HostAuthority::new(HostId::new(1))),
        )
        .expect("start");
    assert_eq!(supervisor.state(&id("api")), ServiceState::Healthy);
    let token = inspect.live_token().expect("live token");
    inspect.push_output(token, "hello from service");
    inspect.push_exit(token, Some(7));
    supervisor.pump_io();
    let fence = supervisor.fence(&id("api")).unwrap();
    let logs = supervisor.logs(&id("api"), fence).unwrap();
    assert!(
        logs.lines
            .iter()
            .any(|line| line.text.contains("hello from service")),
        "reader output must reach bounded logs"
    );
    assert_eq!(supervisor.state(&id("api")), ServiceState::Failed);
    assert_eq!(supervisor.live_count(), 0);
    assert_eq!(inspect.torn_down(), 1);
}

#[test]
fn stopping_state_disables_stop_and_restart_affordance() {
    use devmanager::services::health::{
        EvidenceProvenance, EvidenceSource, HealthAxis, LifecycleAxis, OwnershipAxis, PortAxis,
        ProcessAxis, RedactedServiceSnapshot, ServiceEvidence,
    };
    use devmanager::services::model::ServiceScope;
    use devmanager::ui::{project_services_panel, ServicePanelAction};

    let evidence = ServiceEvidence {
        lifecycle: LifecycleAxis::Stopping,
        process: ProcessAxis::Running { generation: 2 },
        health: HealthAxis::Cancelled,
        port: PortAxis::Owned { port: 8080 },
        ownership: OwnershipAxis::Host,
        generation: 2,
        epoch: 2,
        observed_at_ms: 1_000,
        provenance: EvidenceProvenance {
            source: EvidenceSource::ProcessRegistry,
            observed_at_ms: 1_000,
            generation: Some(2),
            epoch: Some(2),
        },
    };
    let snapshot = RedactedServiceSnapshot::from_evidence(id("api"), ServiceScope::Host, &evidence);
    assert_eq!(snapshot.state, ServiceState::Stopping);
    let panel = project_services_panel(&[snapshot], &[]);
    let stop = panel.rows[0]
        .actions
        .iter()
        .find(|action| action.action == ServicePanelAction::Stop)
        .unwrap();
    let restart = panel.rows[0]
        .actions
        .iter()
        .find(|action| action.action == ServicePanelAction::Restart)
        .unwrap();
    assert!(!stop.enabled);
    assert_eq!(stop.disabled_reason, Some("Service is already stopping"));
    assert!(!restart.enabled);
    assert_eq!(restart.disabled_reason, Some("Service is already stopping"));
}

#[test]
fn service_launch_issuer_rejects_mismatched_capability() {
    use devmanager::domain::id::ResourceId;
    use devmanager::domain::resource::ResourceKind;
    use devmanager::process::identity::ProcessOwner;
    use devmanager::services::ServiceLaunchIssuer;

    let issuer = ServiceLaunchIssuer::new();
    let resource = ResourceId::new();
    issuer
        .admit_capability(
            "service:api",
            ProcessOwner::Host,
            ResourceKind::Service,
            resource,
            2,
        )
        .expect("first admit");
    let err = issuer
        .admit_capability(
            "service:api",
            ProcessOwner::Host,
            ResourceKind::Service,
            ResourceId::new(),
            3,
        )
        .expect_err("foreign resource id");
    assert!(err.contains("capability mismatch"));
    let err = issuer
        .admit_capability(
            "service:api",
            ProcessOwner::Host,
            ResourceKind::Service,
            resource,
            1,
        )
        .expect_err("stale generation");
    assert!(err.contains("stale"));
}
