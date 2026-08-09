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
    MAX_ARGUMENT_COUNT, MAX_DEPENDENCY_COUNT, MAX_SERVICE_COUNT,
};

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
    let catalog: ServiceCatalog =
        serde_json::from_str(include_str!("fixtures/services/valid.json")).unwrap();

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

    let encoded = serde_json::to_value(&catalog).unwrap();
    assert_eq!(encoded["schema_version"], serde_json::json!(1));
    let decoded: ServiceCatalog = serde_json::from_value(encoded.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), encoded);
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
    assert!(serde_json::from_value::<ServiceCatalog>(unknown_field).is_err());

    unknown_field = serde_json::json!({
        "schema_version": 2,
        "services": []
    });
    assert!(serde_json::from_value::<ServiceCatalog>(unknown_field).is_err());

    let mut unbounded = fixture_definition(0);
    unbounded["command"]["program"] = serde_json::json!("x".repeat(257));
    assert!(serde_json::from_value::<ServiceDefinition>(unbounded).is_err());

    let mut unbounded_dependencies = fixture_definition(0);
    unbounded_dependencies["dependencies"] =
        serde_json::json!(vec!["api"; MAX_DEPENDENCY_COUNT + 1]);
    assert!(serde_json::from_value::<ServiceDefinition>(unbounded_dependencies).is_err());

    let services = vec![fixture_definition(0); MAX_SERVICE_COUNT + 1];
    assert!(serde_json::from_value::<ServiceCatalog>(serde_json::json!({
        "schema_version": 1,
        "services": services
    }))
    .is_err());
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
    ] {
        let mut definition = fixture_definition(0);
        definition["command"]["args"] = serde_json::json!(args);
        assert!(serde_json::from_value::<ServiceDefinition>(definition).is_err());
    }

    let mut assignment = fixture_definition(0);
    assignment["command"]["env"] = serde_json::json!([{ "name": "API_TOKEN=raw-secret" }]);
    assert!(serde_json::from_value::<ServiceDefinition>(assignment).is_err());

    let mut reference = fixture_definition(0);
    reference["command"]["env"] = serde_json::json!([{ "name": "API_TOKEN" }]);
    assert!(serde_json::from_value::<ServiceDefinition>(reference).is_ok());
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
    assert!(serde_json::from_value::<ServiceCatalog>(omitted).is_err());
    assert!(serde_json::from_str::<ServiceCatalog>(
        r#"{"schema_version":1,"schema_version":1,"services":[]}"#
    )
    .is_err());
}
