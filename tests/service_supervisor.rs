//! Phase 6 Task 6.7a pure service contract tests.
//!
//! These tests deliberately stop at validated plans, scheduled observations,
//! and reducer snapshots. They never launch a process or probe a live port.

use std::collections::VecDeque;

use devmanager::services::health::{
    reduce_service, EvidenceProvenance, EvidenceSource, FakeClock, HealthAxis, HealthTracker,
    LifecycleAxis, OwnershipAxis, PortAxis, ProbeOutcome, ProcessAxis, RedactedServiceSnapshot,
    ServiceEvidence, ServiceState,
};
use devmanager::services::model::{
    ActiveOperation, AdmissionDecision, AdmissionFence, AdmissionRejection, AdmissionRequest,
    AdmissionSnapshot, CommandSpec, ExpectedPort, HealthPolicy, HealthSpec, LaunchIntent,
    PortProtocol, RuntimeOwnership, RuntimeRecord, ServiceAction, ServiceCatalog,
    ServiceDefinition, ServiceId, ServiceScope, StartupPolicy, StopPolicy, ValidationError,
    MAX_ARGUMENT_COUNT,
};
use serde::Deserialize;

fn id(value: &str) -> ServiceId {
    ServiceId::new(value).expect("test service id")
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
        .with_arg("server.js")
        .with_cwd("apps/api")
        .with_env_reference("PORT")
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

fn record(
    state: ServiceState,
    fence: AdmissionFence,
    ownership: RuntimeOwnership,
    operation: Option<ActiveOperation>,
) -> RuntimeRecord {
    RuntimeRecord {
        state,
        fence,
        ownership,
        operation,
    }
}

#[derive(Deserialize)]
struct ServiceFixture {
    services: Vec<ServiceDefinition>,
}

#[test]
fn configured_launch_fixture_is_validated_and_launch_intent_is_bounded() {
    let fixture: ServiceFixture =
        serde_json::from_str(include_str!("fixtures/services/valid.json")).unwrap();
    let catalog = ServiceCatalog::new(fixture.services).expect("valid service fixture");

    let intent = catalog
        .launch_intent(&id("api"))
        .expect("api launch intent");
    assert_eq!(intent.service_id, id("api"));
    assert_eq!(intent.command.program, "node");
    assert_eq!(intent.command.args, vec!["server.js"]);
    assert_eq!(intent.command.cwd.as_deref(), Some("apps/api"));
    assert_eq!(intent.command.env[0].name, "PORT");
    assert_eq!(intent.expected_port.unwrap().port, 8080);
}

#[test]
fn validation_rejects_unsafe_paths_raw_secrets_unbounded_args_and_duplicate_ids() {
    assert!(matches!(
        ServiceId::new(""),
        Err(ValidationError::Empty { .. })
    ));

    let mut unsafe_path = service("api", ServiceScope::task("task-a"), vec![], Some(8080));
    unsafe_path.command.cwd = Some("..\\secrets".to_owned());
    assert!(matches!(
        ServiceCatalog::new(vec![unsafe_path]),
        Err(ValidationError::UnsafePath { .. })
    ));

    let mut raw_secret = service("api", ServiceScope::task("task-a"), vec![], Some(8080));
    raw_secret.command.args = vec!["--api-token=secret-value".to_owned()];
    assert!(matches!(
        ServiceCatalog::new(vec![raw_secret]),
        Err(ValidationError::RawSecret { .. })
    ));

    let mut too_many_args = service("api", ServiceScope::task("task-a"), vec![], Some(8080));
    too_many_args.command.args = vec!["arg".to_owned(); MAX_ARGUMENT_COUNT + 1];
    assert!(matches!(
        ServiceCatalog::new(vec![too_many_args]),
        Err(ValidationError::TooMany { .. })
    ));

    let duplicate = service("api", ServiceScope::task("task-a"), vec![], Some(8080));
    assert!(matches!(
        ServiceCatalog::new(vec![duplicate.clone(), duplicate]),
        Err(ValidationError::DuplicateServiceId { .. })
    ));

    let self_dependency = service(
        "api",
        ServiceScope::task("task-a"),
        vec![id("api")],
        Some(8080),
    );
    assert!(matches!(
        ServiceCatalog::new(vec![self_dependency]),
        Err(ValidationError::SelfDependency { .. })
    ));

    let unknown_dependency = service(
        "api",
        ServiceScope::task("task-a"),
        vec![id("missing")],
        Some(8080),
    );
    assert!(matches!(
        ServiceCatalog::new(vec![unknown_dependency]),
        Err(ValidationError::UnknownDependency { .. })
    ));
}

#[test]
fn wire_deserializers_validate_ids_definitions_and_launch_intents() {
    assert!(serde_json::from_str::<ServiceId>(r#"\"\""#).is_err());

    let mut fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/services/valid.json")).unwrap();
    fixture["services"][0]["id"] = serde_json::json!("../escape");
    let definition = serde_json::from_value::<ServiceDefinition>(fixture["services"][0].clone());
    assert!(definition.is_err());

    let invalid_intent = serde_json::json!({
        "service_id": "../escape",
        "scope": { "Task": { "task_id": "task-a" } },
        "command": {
            "program": "node",
            "args": ["server.js"],
            "cwd": "apps/api",
            "env": [{ "name": "PORT" }]
        },
        "dependencies": [],
        "expected_port": { "protocol": "Tcp", "port": 8080 }
    });
    assert!(serde_json::from_value::<LaunchIntent>(invalid_intent).is_err());
}

#[test]
fn nested_contract_types_reject_unknown_fields_invalid_values_and_secret_debug_output() {
    let invalid_command = CommandSpec {
        program: "node".to_owned(),
        args: vec!["--token".to_owned(), "secret-value".to_owned()],
        cwd: None,
        env: Vec::new(),
    };
    let debug = format!("{invalid_command:?}");
    assert!(!debug.contains("secret-value"));
    assert!(serde_json::to_value(&invalid_command).is_err());
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
        let mut definition = service("api", ServiceScope::task("task-a"), vec![], Some(8080));
        definition.command.cwd = Some(cwd.to_owned());
        assert!(
            ServiceCatalog::new(vec![definition]).is_err(),
            "non-canonical cwd should be rejected: {cwd:?}"
        );
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
        let mut definition = service("api", ServiceScope::task("task-a"), vec![], Some(8080));
        definition.command.args = args.into_iter().map(str::to_owned).collect();
        assert!(matches!(
            ServiceCatalog::new(vec![definition]),
            Err(ValidationError::RawSecret { .. })
        ));
    }

    let mut assignment = service("api", ServiceScope::task("task-a"), vec![], Some(8080));
    assignment.command.env = vec![devmanager::services::model::EnvReference::new(
        "API_TOKEN=raw-secret",
    )];
    assert!(matches!(
        ServiceCatalog::new(vec![assignment]),
        Err(ValidationError::RawSecret { .. })
    ));

    let mut reference = service("api", ServiceScope::task("task-a"), vec![], Some(8080));
    reference.command.env = vec![devmanager::services::model::EnvReference::new("API_TOKEN")];
    assert!(ServiceCatalog::new(vec![reference]).is_ok());
}

#[test]
fn expected_port_must_match_health_or_be_derived_from_health() {
    let mut mismatch = service("api", ServiceScope::task("task-a"), vec![], Some(8080));
    mismatch.expected_port = Some(ExpectedPort {
        protocol: PortProtocol::Tcp,
        port: 9090,
    });
    assert!(ServiceCatalog::new(vec![mismatch]).is_err());

    let mut derived = service("api", ServiceScope::task("task-a"), vec![], Some(8080));
    derived.expected_port = None;
    let catalog = ServiceCatalog::new(vec![derived]).unwrap();
    assert_eq!(
        catalog
            .launch_intent(&id("api"))
            .unwrap()
            .expected_port
            .unwrap()
            .port,
        8080
    );
}

#[test]
fn dependency_plan_is_deterministic_and_cycles_are_explicit() {
    let catalog = ServiceCatalog::new(vec![
        service(
            "api",
            ServiceScope::task("task-a"),
            vec![id("db")],
            Some(8080),
        ),
        service("db", ServiceScope::task("task-a"), vec![], Some(5432)),
    ])
    .unwrap();
    let plan = catalog.dependency_plan(&id("api")).unwrap();
    assert_eq!(plan.ordered, vec![id("db"), id("api")]);

    let cycle = ServiceCatalog::new(vec![
        service(
            "api",
            ServiceScope::task("task-a"),
            vec![id("db")],
            Some(8080),
        ),
        service(
            "db",
            ServiceScope::task("task-a"),
            vec![id("api")],
            Some(5432),
        ),
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
fn admission_orders_dependencies_coalesces_duplicate_start_and_blocks_failures() {
    let catalog = ServiceCatalog::new(vec![
        service(
            "api",
            ServiceScope::task("task-a"),
            vec![id("db")],
            Some(8080),
        ),
        service("db", ServiceScope::task("task-a"), vec![], Some(5432)),
    ])
    .unwrap();
    let fence = AdmissionFence::new(4, 9);
    let mut snapshot = AdmissionSnapshot::default();
    snapshot.set_service(
        id("api"),
        record(ServiceState::Stopped, fence, RuntimeOwnership::None, None),
    );
    snapshot.set_service(
        id("db"),
        record(ServiceState::Stopped, fence, RuntimeOwnership::None, None),
    );

    let decision = catalog.admit(
        AdmissionRequest::new(ServiceAction::Start, id("api"), fence),
        &snapshot,
    );
    let AdmissionDecision::Start(plan) = decision else {
        panic!("expected start plan");
    };
    assert_eq!(
        plan.ordered
            .iter()
            .map(|intent| intent.service_id.clone())
            .collect::<Vec<_>>(),
        vec![id("db"), id("api")]
    );

    snapshot.set_service(
        id("api"),
        record(
            ServiceState::Starting,
            fence,
            RuntimeOwnership::Task {
                task_id: "task-a".to_owned(),
            },
            Some(ActiveOperation {
                id: 55,
                action: ServiceAction::Start,
            }),
        ),
    );
    assert!(matches!(
        catalog.admit(
            AdmissionRequest::new(ServiceAction::Start, id("api"), fence),
            &snapshot
        ),
        AdmissionDecision::Coalesced {
            operation_id: 55,
            action: ServiceAction::Start,
            ..
        }
    ));

    snapshot.set_service(
        id("db"),
        record(ServiceState::Failed, fence, RuntimeOwnership::None, None),
    );
    snapshot.set_service(
        id("api"),
        record(ServiceState::Stopped, fence, RuntimeOwnership::None, None),
    );
    assert!(matches!(
        catalog.admit(
            AdmissionRequest::new(ServiceAction::Start, id("api"), fence),
            &snapshot
        ),
        AdmissionDecision::Refused(AdmissionRejection::DependencyNotReady {
            dependency,
            state: ServiceState::Failed,
            ..
        }) if dependency == id("db")
    ));
}

#[test]
fn admission_rejects_external_stop_stale_fences_and_limits_task_close_to_owned_task() {
    let catalog = ServiceCatalog::new(vec![
        service("task-api", ServiceScope::task("task-a"), vec![], Some(8080)),
        service(
            "other-task",
            ServiceScope::task("task-b"),
            vec![],
            Some(8081),
        ),
        service("host-db", ServiceScope::Host, vec![], Some(5432)),
    ])
    .unwrap();
    let fence = AdmissionFence::new(4, 9);
    let mut snapshot = AdmissionSnapshot::default();
    snapshot.set_service(
        id("task-api"),
        record(
            ServiceState::External,
            fence,
            RuntimeOwnership::External,
            None,
        ),
    );
    assert!(matches!(
        catalog.admit(
            AdmissionRequest::new(ServiceAction::Stop, id("task-api"), fence),
            &snapshot
        ),
        AdmissionDecision::Refused(AdmissionRejection::ExternalNotControllable { .. })
    ));

    snapshot.set_service(
        id("task-api"),
        record(
            ServiceState::Healthy,
            fence,
            RuntimeOwnership::Task {
                task_id: "task-a".to_owned(),
            },
            None,
        ),
    );
    assert!(matches!(
        catalog.admit(
            AdmissionRequest::new(
                ServiceAction::Stop,
                id("task-api"),
                AdmissionFence::new(3, 9),
            ),
            &snapshot
        ),
        AdmissionDecision::Refused(AdmissionRejection::StaleFence { .. })
    ));

    snapshot.set_task_epoch("task-a", 9);
    snapshot.mark_task_closing("task-a");
    snapshot.set_service(
        id("other-task"),
        record(
            ServiceState::Healthy,
            fence,
            RuntimeOwnership::Task {
                task_id: "task-b".to_owned(),
            },
            None,
        ),
    );
    snapshot.set_service(
        id("host-db"),
        record(ServiceState::Healthy, fence, RuntimeOwnership::Host, None),
    );
    let close = catalog.admit_task_close("task-a", 9, &snapshot).unwrap();
    assert_eq!(
        close
            .ordered
            .iter()
            .map(|item| item.service_id.clone())
            .collect::<Vec<_>>(),
        vec![id("task-api")]
    );
    assert!(!close
        .ordered
        .iter()
        .any(|item| item.service_id == id("other-task")));
    assert!(!close
        .ordered
        .iter()
        .any(|item| item.service_id == id("host-db")));
}

#[test]
fn task_close_requires_closing_barrier_and_rejects_member_operations() {
    let catalog = ServiceCatalog::new(vec![
        service("api", ServiceScope::task("task-a"), vec![], Some(8080)),
        service("db", ServiceScope::task("task-a"), vec![], Some(5432)),
    ])
    .unwrap();
    let fence = AdmissionFence::new(4, 9);
    let owned = RuntimeOwnership::Task {
        task_id: "task-a".to_owned(),
    };
    let mut snapshot = AdmissionSnapshot::default();
    snapshot.set_task_epoch("task-a", 9);
    snapshot.set_service(
        id("api"),
        record(
            ServiceState::Healthy,
            fence,
            owned.clone(),
            Some(ActiveOperation {
                id: 91,
                action: ServiceAction::Start,
            }),
        ),
    );
    snapshot.set_service(
        id("db"),
        record(ServiceState::Healthy, fence, owned.clone(), None),
    );

    assert!(catalog.admit_task_close("task-a", 9, &snapshot).is_err());
    snapshot.mark_task_closing("task-a");
    assert!(catalog.admit_task_close("task-a", 9, &snapshot).is_err());

    snapshot.set_service(id("api"), record(ServiceState::Healthy, fence, owned, None));
    let close = catalog.admit_task_close("task-a", 9, &snapshot).unwrap();
    assert_eq!(
        close
            .ordered
            .iter()
            .map(|item| item.service_id.clone())
            .collect::<Vec<_>>(),
        vec![id("db"), id("api")]
    );
    assert!(matches!(
        &close.ordered[0].fence.ownership,
        RuntimeOwnership::Task { task_id } if task_id == "task-a"
    ));
    assert!(close.revalidate(&snapshot).is_ok());
}

#[test]
fn operations_require_exact_scope_ownership_and_none_is_only_initial_start_claim() {
    let catalog = ServiceCatalog::new(vec![
        service("task-api", ServiceScope::task("task-a"), vec![], Some(8080)),
        service("host-db", ServiceScope::Host, vec![], Some(5432)),
    ])
    .unwrap();
    let fence = AdmissionFence::new(4, 9);
    let mut snapshot = AdmissionSnapshot::default();
    snapshot.set_service(
        id("task-api"),
        record(
            ServiceState::Healthy,
            fence,
            RuntimeOwnership::Task {
                task_id: "task-b".to_owned(),
            },
            None,
        ),
    );
    assert!(matches!(
        catalog.admit(
            AdmissionRequest::new(ServiceAction::Stop, id("task-api"), fence),
            &snapshot
        ),
        AdmissionDecision::Refused(_)
    ));

    snapshot.set_service(
        id("task-api"),
        record(ServiceState::Healthy, fence, RuntimeOwnership::None, None),
    );
    assert!(matches!(
        catalog.admit(
            AdmissionRequest::new(ServiceAction::Stop, id("task-api"), fence),
            &snapshot
        ),
        AdmissionDecision::Refused(_)
    ));

    snapshot.set_service(
        id("task-api"),
        record(ServiceState::Stopped, fence, RuntimeOwnership::None, None),
    );
    assert!(matches!(
        catalog.admit(
            AdmissionRequest::new(ServiceAction::Start, id("task-api"), fence),
            &snapshot
        ),
        AdmissionDecision::Start(_)
    ));

    snapshot.set_service(
        id("host-db"),
        record(
            ServiceState::Healthy,
            fence,
            RuntimeOwnership::Task {
                task_id: "task-a".to_owned(),
            },
            None,
        ),
    );
    assert!(matches!(
        catalog.admit(
            AdmissionRequest::new(ServiceAction::Stop, id("host-db"), fence),
            &snapshot
        ),
        AdmissionDecision::Refused(_)
    ));
}

#[test]
fn plan_members_capture_exact_fences_and_support_atomic_revalidation() {
    let catalog = ServiceCatalog::new(vec![
        service(
            "api",
            ServiceScope::task("task-a"),
            vec![id("db")],
            Some(8080),
        ),
        service("db", ServiceScope::task("task-a"), vec![], Some(5432)),
    ])
    .unwrap();
    let fence = AdmissionFence::new(4, 9);
    let mut snapshot = AdmissionSnapshot::default();
    snapshot.set_service(
        id("api"),
        record(ServiceState::Stopped, fence, RuntimeOwnership::None, None),
    );
    snapshot.set_service(
        id("db"),
        record(ServiceState::Stopped, fence, RuntimeOwnership::None, None),
    );
    let AdmissionDecision::Start(plan) = catalog.admit(
        AdmissionRequest::new(ServiceAction::Start, id("api"), fence),
        &snapshot,
    ) else {
        panic!("expected start plan");
    };
    assert_eq!(plan.ordered[0].fence.generation, 4);
    assert_eq!(plan.ordered[0].fence.epoch, 9);
    assert_eq!(plan.ordered[0].fence.ownership, RuntimeOwnership::None);
    assert!(plan.revalidate(&snapshot).is_ok());

    snapshot.set_service(
        id("db"),
        record(
            ServiceState::Stopped,
            AdmissionFence::new(5, 9),
            RuntimeOwnership::None,
            None,
        ),
    );
    assert!(plan.revalidate(&snapshot).is_err());
}

#[test]
fn admission_stop_reverses_dependencies_and_restart_returns_typed_plans() {
    let catalog = ServiceCatalog::new(vec![
        service(
            "api",
            ServiceScope::task("task-a"),
            vec![id("db")],
            Some(8080),
        ),
        service("db", ServiceScope::task("task-a"), vec![], Some(5432)),
    ])
    .unwrap();
    let fence = AdmissionFence::new(4, 9);
    let owned = RuntimeOwnership::Task {
        task_id: "task-a".to_owned(),
    };
    let mut snapshot = AdmissionSnapshot::default();
    snapshot.set_service(
        id("api"),
        record(ServiceState::Healthy, fence, owned.clone(), None),
    );
    snapshot.set_service(id("db"), record(ServiceState::Healthy, fence, owned, None));

    let stop = catalog.admit(
        AdmissionRequest::new(ServiceAction::Stop, id("db"), fence),
        &snapshot,
    );
    assert!(matches!(
        stop,
        AdmissionDecision::Stop(plan)
            if plan
                .ordered
                .iter()
                .map(|item| item.service_id.clone())
                .collect::<Vec<_>>() == vec![id("api"), id("db")]
    ));

    let AdmissionDecision::Stop(stop_plan) = catalog.admit(
        AdmissionRequest::new(ServiceAction::Stop, id("db"), fence),
        &snapshot,
    ) else {
        panic!("expected stop plan");
    };
    assert!(stop_plan.revalidate(&snapshot).is_ok());
    snapshot.set_service(
        id("api"),
        record(
            ServiceState::Healthy,
            fence,
            RuntimeOwnership::Task {
                task_id: "task-a".to_owned(),
            },
            Some(ActiveOperation {
                id: 93,
                action: ServiceAction::Start,
            }),
        ),
    );
    assert!(stop_plan.revalidate(&snapshot).is_err());
    snapshot.set_service(
        id("api"),
        record(
            ServiceState::Healthy,
            fence,
            RuntimeOwnership::Task {
                task_id: "task-a".to_owned(),
            },
            None,
        ),
    );

    let restart = catalog.admit(
        AdmissionRequest::new(ServiceAction::Restart, id("api"), fence),
        &snapshot,
    );
    let AdmissionDecision::Restart(restart_plan) = restart else {
        panic!("expected restart plan");
    };
    assert!(restart_plan.revalidate(&snapshot).is_ok());
    assert_eq!(
        restart_plan
            .stop
            .ordered
            .iter()
            .map(|item| item.service_id.clone())
            .collect::<Vec<_>>(),
        vec![id("api")]
    );
    assert_eq!(
        restart_plan
            .start
            .ordered
            .iter()
            .map(|item| item.service_id.clone())
            .collect::<Vec<_>>(),
        vec![id("db"), id("api")]
    );
}

#[test]
fn snapshots_are_redacted_and_manual_stop_or_crash_has_explicit_evidence() {
    let stopped = evidence(
        LifecycleAxis::Stopped,
        ProcessAxis::Absent,
        HealthAxis::Cancelled,
        PortAxis::Free,
        OwnershipAxis::None,
    );
    assert_eq!(reduce_service(&stopped), ServiceState::Stopped);

    let snapshot =
        RedactedServiceSnapshot::from_evidence(id("api"), ServiceScope::task("task-a"), &stopped);
    let serialized = serde_json::to_string(&snapshot).unwrap();
    assert!(!serialized.contains("node"));
    assert!(!serialized.contains("server.js"));
    assert!(!serialized.contains("secret-value"));
    assert!(serialized.contains("observed_at_ms"));
    assert!(serialized.contains("fake_probe"));

    let mut tracker = HealthTracker::new(policy());
    tracker.start(0, 7).unwrap();
    tracker.cancel(100, 7).unwrap();
    assert_eq!(tracker.axis(), HealthAxis::Cancelled);
    tracker.process_exit(200, 7).unwrap();
    assert!(matches!(tracker.axis(), HealthAxis::Crashed));
}
