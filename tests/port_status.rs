use std::collections::{BTreeMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use devmanager::domain::id::ResourceId;
use devmanager::domain::operation::ResourceFence;
use devmanager::process::identity::{ManagedProcessId, ManagedProcessIdentity, ProcessOwner};
use devmanager::process::ports::{
    classify_port_authority, classify_port_authority_from_snapshot, ensure_managed_start_allowed,
    launch_if_port_free, project_port_status, project_port_status_from_snapshot,
    registered_resource_snapshot, registered_resource_snapshot_with_membership, ListenerIdentity,
    ManagedPortHealth, ManagedProcessSnapshotValidity, ManagedResourceSnapshot, PortAuthority,
    PortInventorySnapshot, PortObservation, PortObservationIssue, PortScanError, PortStartError,
    PortStatusKind, PortTarget, RegistryMembershipSnapshot, ScanCancellation, TcpAddressFamily,
    TcpEndpoint, TcpEndpointRecord, TcpProtocol, MAX_SCAN_WAITERS,
};
use devmanager::process::registry::{
    JobMembership, ManagedProcessFence, ManagedProcessState, ProcessRegistry, RegisteredProcess,
};
use devmanager::services::ports_service::{
    kill_port, legacy_statuses_from_snapshot, scan_listener_inventory,
    scan_listener_inventory_with, PortInventory,
};

fn fixed_uuid_v7(tail: u8) -> [u8; 16] {
    [
        0x01, 0x9a, 0x11, 0x22, 0x33, 0x44, 0x70, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        tail,
    ]
}

fn resource_id(tail: u8) -> ResourceId {
    ResourceId::from_bytes(fixed_uuid_v7(tail)).expect("resource id")
}

fn fence(tail: u8, generation: u64) -> ResourceFence {
    ResourceFence::new(resource_id(tail), generation)
}

fn executable() -> PathBuf {
    std::env::current_exe().expect("test executable")
}

fn identity(pid: u32, creation_time_100ns: u64, executable: &Path) -> ManagedProcessIdentity {
    ManagedProcessIdentity::new(
        ManagedProcessId::new(pid, creation_time_100ns).expect("managed process id"),
        executable,
    )
    .expect("managed process identity")
}

fn listener(pid: u32, creation_time_100ns: u64) -> ListenerIdentity {
    ListenerIdentity::new(pid, creation_time_100ns).expect("listener identity")
}

fn single_listener(identity: ListenerIdentity) -> PortObservation {
    PortObservation::from_listeners(vec![identity])
}

fn free_scan(ports: &[u16]) -> PortInventorySnapshot {
    PortInventorySnapshot::new(
        ports
            .iter()
            .copied()
            .map(|port| (port, PortObservation::Free))
            .collect(),
    )
}

#[derive(Debug, Clone)]
struct TestJob {
    root_pid: u32,
}

impl JobMembership for TestJob {
    fn active_process_ids(&self) -> Result<Vec<u32>, String> {
        Ok(vec![self.root_pid])
    }
}

fn registry_with_root(
    resource: ResourceFence,
    root_pid: u32,
    root_creation_time_100ns: u64,
) -> (ProcessRegistry<TestJob>, ManagedProcessFence) {
    let root = identity(root_pid, root_creation_time_100ns, &executable());
    let mut registry = ProcessRegistry::new();
    let registered = RegisteredProcess::new(
        resource,
        ProcessOwner::Host,
        root,
        devmanager::process::registry::ProcessDisplayLabel::new("port test")
            .expect("display label"),
        TestJob { root_pid },
    );
    let managed_fence = registry.register(registered).expect("register process");
    (registry, managed_fence)
}

fn target(resource: ResourceFence) -> PortTarget {
    PortTarget::new(43001, resource, ManagedPortHealth::Ready)
}

#[test]
fn starting_resource_is_orange_even_before_a_listener_appears() {
    let resource = fence(1, 1);
    let (registry, _) = registry_with_root(resource, 11_001, 101);
    let managed = registered_resource_snapshot(&registry, resource).expect("current resource");

    let status = project_port_status(&target(resource), &PortObservation::Free, Some(&managed));

    assert_eq!(status.kind(), PortStatusKind::Starting);
}

#[test]
fn probe_failure_remains_visible_while_managed_resource_is_starting() {
    let resource = fence(10, 1);
    let (registry, _) = registry_with_root(resource, 11_013, 1_013);
    let managed = registered_resource_snapshot(&registry, resource).expect("current resource");

    let status = project_port_status(
        &target(resource),
        &PortObservation::ProbeError("listener table unavailable".to_string()),
        Some(&managed),
    );

    assert_eq!(status.kind(), PortStatusKind::ProbeError);
    assert_eq!(status.error(), Some("listener table unavailable"));
}

#[test]
fn matching_managed_ready_listener_is_green() {
    let resource = fence(2, 1);
    let (mut registry, managed_fence) = registry_with_root(resource, 11_002, 202);
    registry
        .commit_resumed_exact(&managed_fence)
        .expect("resume generation");
    let managed = registered_resource_snapshot(&registry, resource).expect("current resource");

    let status = project_port_status(
        &target(resource),
        &single_listener(listener(11_002, 202)),
        Some(&managed),
    );

    assert_eq!(status.kind(), PortStatusKind::ManagedHealthy);
}

#[test]
fn matching_managed_listener_without_readiness_is_orange_unready() {
    let resource = fence(8, 1);
    let (mut registry, managed_fence) = registry_with_root(resource, 11_008, 808);
    registry
        .commit_resumed_exact(&managed_fence)
        .expect("resume generation");
    let managed = registered_resource_snapshot(&registry, resource).expect("current resource");
    let target = PortTarget::new(43001, resource, ManagedPortHealth::NotReady);

    let status = project_port_status(
        &target,
        &single_listener(listener(11_008, 808)),
        Some(&managed),
    );

    assert_eq!(status.kind(), PortStatusKind::ManagedUnready);
    assert_eq!(status.error(), None);
}

#[test]
fn listener_with_unverified_ownership_is_occupied() {
    let resource = fence(3, 1);
    let status = project_port_status(
        &target(resource),
        &single_listener(listener(11_003, 303)),
        None,
    );

    assert_eq!(status.kind(), PortStatusKind::Occupied);
    assert_eq!(status.listener(), Some(listener(11_003, 303)));
}

#[test]
fn mixed_managed_and_ownership_unverified_listeners_are_preserved_and_fail_closed() {
    let resource = fence(9, 1);
    let (mut registry, managed_fence) = registry_with_root(resource, 11_009, 909);
    registry
        .commit_resumed_exact(&managed_fence)
        .expect("resume generation");
    let managed = registered_resource_snapshot(&registry, resource).expect("current resource");
    let managed_listener = listener(11_009, 909);
    let ownership_unverified_listener = listener(11_010, 910);
    let observation = PortObservation::Listeners(Arc::from(
        vec![managed_listener, ownership_unverified_listener].into_boxed_slice(),
    ));

    let status = project_port_status(&target(resource), &observation, Some(&managed));

    assert_eq!(status.kind(), PortStatusKind::Occupied);
    assert_eq!(status.listener(), None);
    assert_eq!(
        status.listeners(),
        &[managed_listener, ownership_unverified_listener]
    );
}

#[test]
fn valid_registry_snapshot_can_prove_a_listener_external() {
    let resource = fence(18, 1);
    let managed = ManagedResourceSnapshot::new(
        resource,
        ManagedProcessState::Running,
        vec![ManagedProcessId::new(11_018, 1_818).unwrap()],
        RegistryMembershipSnapshot::valid(1, 1, Instant::now(), Duration::from_secs(10)),
    );
    let target = PortTarget::new(43_018, resource, ManagedPortHealth::Ready);
    let snapshot = PortInventorySnapshot::with_endpoints(
        BTreeMap::from([(target.port, single_listener(listener(11_019, 1_919)))]),
        BTreeMap::from([(
            target.port,
            vec![TcpEndpoint::tcp(
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                target.port,
                listener(11_019, 1_919),
            )],
        )]),
    );

    assert_eq!(
        classify_port_authority_from_snapshot(&target, &snapshot, Some(&managed)),
        PortAuthority::ProvenExternal
    );
    assert_eq!(
        project_port_status_from_snapshot(&target, &snapshot, Some(&managed)).kind(),
        PortStatusKind::ProvenExternal
    );
}

#[test]
fn free_port_without_a_launch_is_gray() {
    let resource = fence(4, 1);
    let status = project_port_status(&target(resource), &PortObservation::Free, None);

    assert_eq!(status.kind(), PortStatusKind::Stopped);
    assert_eq!(status.listener(), None);
}

#[test]
fn probe_failure_is_not_treated_as_free() {
    let resource = fence(5, 1);
    let status = project_port_status(
        &target(resource),
        &PortObservation::ProbeError("listener table unavailable".to_string()),
        None,
    );

    assert_eq!(status.kind(), PortStatusKind::ProbeError);
    assert_eq!(status.error(), Some("listener table unavailable"));
}

#[test]
fn pid_reuse_does_not_make_a_reused_pid_managed() {
    let resource = fence(6, 1);
    let (mut registry, managed_fence) = registry_with_root(resource, 11_006, 606);
    registry
        .commit_resumed_exact(&managed_fence)
        .expect("resume generation");
    let managed = registered_resource_snapshot(&registry, resource).expect("current resource");

    let status = project_port_status(
        &target(resource),
        &single_listener(listener(11_006, 999_999)),
        Some(&managed),
    );

    assert_eq!(status.kind(), PortStatusKind::Occupied);
}

#[test]
fn a_stale_resource_generation_cannot_claim_a_current_listener() {
    let current_resource = fence(7, 2);
    let stale_resource = fence(7, 1);
    let (mut registry, managed_fence) = registry_with_root(current_resource, 11_007, 707);
    registry
        .commit_resumed_exact(&managed_fence)
        .expect("resume generation");

    let stale_snapshot = registered_resource_snapshot(&registry, stale_resource);
    let status = project_port_status(
        &target(stale_resource),
        &single_listener(listener(11_007, 707)),
        stale_snapshot.as_ref(),
    );

    assert_eq!(status.kind(), PortStatusKind::Occupied);
}

#[test]
fn direct_projection_requires_the_exact_target_fence() {
    let current_resource = fence(20, 2);
    let stale_resource = fence(20, 1);
    let (mut registry, managed_fence) = registry_with_root(current_resource, 11_020, 2_020);
    registry
        .commit_resumed_exact(&managed_fence)
        .expect("resume generation");
    let current = registered_resource_snapshot(&registry, current_resource).expect("current");

    let status = project_port_status(
        &PortTarget::new(43_020, stale_resource, ManagedPortHealth::Ready),
        &single_listener(listener(11_020, 2_020)),
        Some(&current),
    );
    assert_eq!(status.kind(), PortStatusKind::Occupied);
}

#[test]
fn cached_snapshots_are_read_without_running_a_probe() {
    let inventory = PortInventory::new();
    let before = inventory.cached_snapshot();
    assert!(before.observation(43001).is_none());

    let snapshot = Arc::new(PortInventorySnapshot::new(BTreeMap::from([(
        43001,
        PortObservation::Free,
    )])));
    inventory.publish(snapshot.clone());

    let cached = inventory.cached_snapshot();
    assert!(Arc::ptr_eq(&cached, &snapshot));
    assert_eq!(cached.observation(43001), Some(&PortObservation::Free));
    assert!(Arc::ptr_eq(&cached, &inventory.cached_snapshot()));
}

#[test]
fn occupied_start_is_rejected_with_listener_evidence() {
    let occupied = listener(11_009, 909);
    let snapshot = PortInventorySnapshot::new(BTreeMap::from([(43001, single_listener(occupied))]));

    let error = ensure_managed_start_allowed(&snapshot, 43001).expect_err("occupied port");

    assert_eq!(
        error,
        PortStartError::Occupied {
            port: 43001,
            listener: occupied,
        }
    );
    assert!(error.to_string().contains("PID 11009"));
    assert!(error.to_string().contains("creation 909"));
}

#[test]
fn occupied_ambiguous_start_reports_all_captured_listener_identities() {
    // Distinct identities model IPv4 and IPv6 listener rows after the native
    // inventory has enriched each row with its exact PID and creation time.
    let ipv4_listener = listener(11_013, 1_313);
    let ipv6_listener = listener(11_014, 1_414);
    let listeners: Arc<[ListenerIdentity]> =
        Arc::from(vec![ipv4_listener, ipv6_listener].into_boxed_slice());
    let snapshot = PortInventorySnapshot::new(BTreeMap::from([(
        43001,
        PortObservation::Listeners(listeners.clone()),
    )]));

    let error = ensure_managed_start_allowed(&snapshot, 43001)
        .expect_err("multiple listener identities must be rejected");

    assert_eq!(
        error,
        PortStartError::OccupiedAmbiguous {
            port: 43001,
            listeners,
        }
    );
    let display = error.to_string();
    assert!(display.contains("ownership is unverified"));
    assert!(display.contains("PID 11013 (creation 1313)"));
    assert!(display.contains("PID 11014 (creation 1414)"));
}

#[test]
fn probe_error_cannot_invoke_the_server_launch_callback() {
    let snapshot = PortInventorySnapshot::new(BTreeMap::from([(
        43001,
        PortObservation::ProbeError("listener table unavailable".to_string()),
    )]));
    let launched = std::sync::atomic::AtomicBool::new(false);

    let error = launch_if_port_free(&snapshot, 43001, || {
        launched.store(true, std::sync::atomic::Ordering::SeqCst);
    })
    .expect_err("probe failure must reject launch");

    assert_eq!(
        error,
        PortStartError::ProbeFailed {
            port: 43001,
            detail: "listener table unavailable".to_string(),
        }
    );
    assert!(!launched.load(std::sync::atomic::Ordering::SeqCst));
}

#[test]
fn stale_or_failed_membership_never_proves_managed_ownership() {
    let resource = fence(12, 1);
    let member = ManagedProcessId::new(11_012, 1_212).expect("member identity");
    let stale = ManagedResourceSnapshot::new(
        resource,
        ManagedProcessState::Running,
        vec![member],
        RegistryMembershipSnapshot::stale(7, 9, Instant::now() - Duration::from_secs(10)),
    );
    let stale_authority =
        classify_port_authority(&single_listener(listener(11_012, 1_212)), Some(&stale));
    assert_eq!(stale_authority, PortAuthority::Unknown);

    let failed = ManagedResourceSnapshot::new(
        resource,
        ManagedProcessState::Running,
        vec![member],
        RegistryMembershipSnapshot::failed(7, 10, "membership access denied"),
    );
    let failed_authority =
        classify_port_authority(&single_listener(listener(11_012, 1_212)), Some(&failed));
    assert_eq!(failed_authority, PortAuthority::Unknown);
    assert_eq!(failed.membership_revision(), 7);
    assert_eq!(failed.observation_sequence(), 10);
    assert_eq!(failed.validity(), ManagedProcessSnapshotValidity::Failed);
}

#[test]
fn missing_membership_revision_or_observation_sequence_is_not_fresh() {
    let now = Instant::now();
    assert!(!RegistryMembershipSnapshot::valid(0, 1, now, Duration::from_secs(5)).is_fresh_at(now));
    assert!(!RegistryMembershipSnapshot::valid(1, 0, now, Duration::from_secs(5)).is_fresh_at(now));
}

#[test]
fn stopped_failed_and_leaked_generations_cannot_be_managed() {
    let resource = fence(13, 1);
    let member = ManagedProcessId::new(11_013, 1_313).expect("member identity");
    for state in [
        ManagedProcessState::Stopped,
        ManagedProcessState::Failed,
        ManagedProcessState::Leaked,
    ] {
        let managed = ManagedResourceSnapshot::new(
            resource,
            state,
            vec![member],
            RegistryMembershipSnapshot::valid(1, 1, Instant::now(), Duration::from_secs(10)),
        );
        assert_ne!(
            classify_port_authority(&single_listener(listener(11_013, 1_313)), Some(&managed)),
            PortAuthority::Managed,
            "inactive state {state:?} must not own a listener"
        );
    }
}

#[test]
fn registered_snapshot_requires_the_exact_fence_and_carries_membership_contract() {
    let resource = fence(14, 3);
    let (registry, _) = registry_with_root(resource, 11_014, 1_414);
    let membership =
        RegistryMembershipSnapshot::valid(22, 31, Instant::now(), Duration::from_secs(5));
    let snapshot =
        registered_resource_snapshot_with_membership(&registry, resource, membership.clone())
            .expect("exact current registry generation");

    assert_eq!(snapshot.resource(), resource);
    assert_eq!(snapshot.membership_revision(), 22);
    assert_eq!(snapshot.observation_sequence(), 31);
    assert_eq!(
        snapshot.member_identities(),
        &[ManagedProcessId::new(11_014, 1_414).unwrap()]
    );
    assert!(snapshot.is_fresh_at(Instant::now()));
    assert!(
        registered_resource_snapshot_with_membership(&registry, fence(14, 2), membership,)
            .is_none()
    );
}

#[test]
fn mixed_managed_and_external_endpoints_are_unknown_even_when_the_pid_list_looks_usable() {
    let resource = fence(15, 1);
    let managed_identity = listener(11_015, 1_515);
    let external_identity = listener(11_016, 1_616);
    let managed = ManagedResourceSnapshot::new(
        resource,
        ManagedProcessState::Running,
        vec![ManagedProcessId::new(11_015, 1_515).unwrap()],
        RegistryMembershipSnapshot::valid(1, 1, Instant::now(), Duration::from_secs(10)),
    );
    let port = 43_001;
    let observations = BTreeMap::from([(
        port,
        PortObservation::from_listeners(vec![managed_identity, external_identity]),
    )]);
    let endpoints = BTreeMap::from([(
        port,
        vec![
            TcpEndpoint::tcp(IpAddr::V4(Ipv4Addr::LOCALHOST), port, managed_identity),
            TcpEndpoint::tcp(IpAddr::V6(Ipv6Addr::LOCALHOST), port, external_identity),
        ],
    )]);
    let snapshot = PortInventorySnapshot::with_endpoints(observations, endpoints);

    assert_eq!(
        classify_port_authority_from_snapshot(&target(resource), &snapshot, Some(&managed)),
        PortAuthority::Unknown
    );
}

#[test]
fn listener_table_change_after_identity_capture_is_a_reconciliation_fault() {
    let port = 43_002;
    let first = BTreeMap::from([(
        port,
        vec![TcpEndpointRecord::tcp(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            port,
            11_017,
        )],
    )]);
    let second = BTreeMap::from([(
        port,
        vec![TcpEndpointRecord::tcp(
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port,
            11_017,
        )],
    )]);
    let tables = Arc::new(Mutex::new(VecDeque::from([first, second])));
    let snapshot = scan_listener_inventory_with(
        &[port],
        {
            let tables = tables.clone();
            move |_| {
                tables
                    .lock()
                    .unwrap()
                    .pop_front()
                    .ok_or_else(|| "missing table".to_string())
            }
        },
        |pid| Ok(listener(pid, 1_717)),
    )
    .expect("race result should be published as an explicit fault");

    assert!(matches!(
        snapshot.issue(port),
        Some(PortObservationIssue::ReconciliationFault(_))
    ));
    assert_eq!(
        classify_port_authority_from_snapshot(
            &PortTarget::new(port, fence(16, 1), ManagedPortHealth::Ready),
            &snapshot,
            None,
        ),
        PortAuthority::Unknown
    );
    assert!(matches!(
        ensure_managed_start_allowed(&snapshot, port),
        Err(PortStartError::ProbeFailed { port: observed, .. }) if observed == port
    ));
}

#[test]
fn pid_reuse_during_identity_capture_is_a_reconciliation_fault() {
    let port = 43_006;
    let table = BTreeMap::from([(
        port,
        vec![TcpEndpointRecord::tcp(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            port,
            11_020,
        )],
    )]);
    let capture_count = Arc::new(AtomicUsize::new(0));
    let snapshot = scan_listener_inventory_with(
        &[port],
        {
            let table = table.clone();
            move |_| Ok(table.clone())
        },
        {
            let capture_count = capture_count.clone();
            move |pid| {
                let creation = if capture_count.fetch_add(1, Ordering::SeqCst) == 0 {
                    2_020
                } else {
                    2_021
                };
                Ok(listener(pid, creation))
            }
        },
    )
    .expect("PID reuse result should remain explicit");

    assert!(matches!(
        snapshot.issue(port),
        Some(PortObservationIssue::ReconciliationFault(detail))
            if detail.contains("process identity changed")
    ));
    assert_eq!(
        snapshot
            .observation(port)
            .unwrap()
            .listener()
            .unwrap()
            .creation_time_100ns(),
        2_021
    );
}

#[test]
fn access_denied_identity_capture_is_probe_error_and_never_free_or_external() {
    let port = 43_003;
    let table = BTreeMap::from([(
        port,
        vec![TcpEndpointRecord::tcp(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            port,
            11_018,
        )],
    )]);
    let snapshot = scan_listener_inventory_with(
        &[port],
        {
            let table = table.clone();
            move |_| Ok(table.clone())
        },
        |_| Err("Access is denied".to_string()),
    )
    .expect("per-listener access errors remain in the immutable result");

    assert!(
        matches!(snapshot.observation(port), Some(PortObservation::ProbeError(detail)) if detail.contains("Access is denied"))
    );
    assert_eq!(
        classify_port_authority_from_snapshot(
            &PortTarget::new(port, fence(17, 1), ManagedPortHealth::Ready),
            &snapshot,
            None,
        ),
        PortAuthority::ProbeError
    );
}

#[test]
fn endpoint_observation_preserves_tcp_family_bind_and_dual_stack_rows() {
    let port = 43_004;
    let identity = listener(11_019, 1_919);
    let observations = BTreeMap::from([(port, single_listener(identity))]);
    let endpoints = BTreeMap::from([(
        port,
        vec![
            TcpEndpoint::tcp(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port, identity),
            TcpEndpoint::tcp(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port, identity),
        ],
    )]);
    let snapshot = PortInventorySnapshot::with_endpoints(observations, endpoints);
    let endpoints = snapshot.endpoints(port);

    assert_eq!(endpoints.len(), 2);
    assert!(endpoints
        .iter()
        .all(|endpoint| endpoint.protocol() == TcpProtocol::Tcp));
    assert!(endpoints
        .iter()
        .any(|endpoint| endpoint.family() == TcpAddressFamily::Ipv4));
    assert!(endpoints
        .iter()
        .any(|endpoint| endpoint.family() == TcpAddressFamily::Ipv6));
    assert!(endpoints
        .iter()
        .any(|endpoint| endpoint.is_ipv4() && endpoint.is_wildcard()));
    assert!(endpoints
        .iter()
        .any(|endpoint| endpoint.is_ipv6() && endpoint.is_wildcard()));

    let resource = fence(19, 1);
    let managed = ManagedResourceSnapshot::new(
        resource,
        ManagedProcessState::Running,
        vec![ManagedProcessId::new(identity.pid(), identity.creation_time_100ns()).unwrap()],
        RegistryMembershipSnapshot::valid(1, 1, Instant::now(), Duration::from_secs(10)),
    );
    assert_eq!(
        classify_port_authority_from_snapshot(
            &PortTarget::new(port, resource, ManagedPortHealth::Ready),
            &snapshot,
            Some(&managed),
        ),
        PortAuthority::Managed
    );
}

#[test]
fn launch_admission_rejects_stale_and_partial_free_proofs() {
    let port = 43_005;
    let stale = PortInventorySnapshot::new(BTreeMap::from([(port, PortObservation::Free)]))
        .with_observed_at(Instant::now() - Duration::from_secs(60));
    assert!(matches!(
        ensure_managed_start_allowed(&stale, port),
        Err(PortStartError::StaleProof { port: observed }) if observed == port
    ));

    let partial = PortInventorySnapshot::new(BTreeMap::from([
        (port, PortObservation::Free),
        (port + 1, PortObservation::Free),
    ]));
    assert!(matches!(
        ensure_managed_start_allowed(&partial, port),
        Err(PortStartError::NotExactSnapshot { port: observed }) if observed == port
    ));
}

#[test]
fn port_inventory_coalesces_latest_requests_and_never_runs_two_scans() {
    let active = Arc::new(AtomicUsize::new(0));
    let maximum_active = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(Mutex::new(Vec::<Vec<u16>>::new()));
    let first_started = Arc::new(AtomicBool::new(false));
    let release_first = Arc::new(AtomicBool::new(false));
    let scanner = {
        let active = active.clone();
        let maximum_active = maximum_active.clone();
        let calls = calls.clone();
        let first_started = first_started.clone();
        let release_first = release_first.clone();
        move |ports: &[u16], cancellation: &ScanCancellation| {
            let current = active.fetch_add(1, Ordering::SeqCst) + 1;
            let mut observed_maximum = maximum_active.load(Ordering::SeqCst);
            while current > observed_maximum {
                match maximum_active.compare_exchange(
                    observed_maximum,
                    current,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                ) {
                    Ok(_) => break,
                    Err(next) => observed_maximum = next,
                }
            }
            calls.lock().unwrap().push(ports.to_vec());
            if ports == [43_010] {
                first_started.store(true, Ordering::SeqCst);
                while !release_first.load(Ordering::SeqCst) {
                    assert!(
                        !cancellation.is_cancelled(),
                        "first scan unexpectedly timed out"
                    );
                    thread::sleep(Duration::from_millis(2));
                }
            }
            active.fetch_sub(1, Ordering::SeqCst);
            Ok(free_scan(ports))
        }
    };
    let inventory = PortInventory::with_scanner_and_timeout(scanner, Duration::from_millis(500));

    let first = inventory.request_scan(&[43_010]).expect("first request");
    for _ in 0..200 {
        if first_started.load(Ordering::SeqCst) {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    assert!(first_started.load(Ordering::SeqCst));
    let second = inventory.request_scan(&[43_011]).expect("second request");
    let third = inventory.request_scan(&[43_012]).expect("latest request");
    release_first.store(true, Ordering::SeqCst);

    let first_result = first.wait(Duration::from_secs(2)).expect("first result");
    let second_result = second
        .wait(Duration::from_secs(2))
        .expect("coalesced result");
    let third_result = third.wait(Duration::from_secs(2)).expect("latest result");
    assert_eq!(first_result.requested_ports(), &[43_010]);
    assert_eq!(second_result.requested_ports(), &[43_012]);
    assert!(Arc::ptr_eq(&second_result, &third_result));
    assert_eq!(*calls.lock().unwrap(), vec![vec![43_010], vec![43_012]]);
    assert_eq!(maximum_active.load(Ordering::SeqCst), 1);
    assert_eq!(first_result.publication_sequence(), 1);
    assert_eq!(second_result.publication_sequence(), 2);
    inventory.shutdown();
}

#[test]
fn port_inventory_timeout_cancels_scan_and_shutdown_rejects_new_work() {
    let started = Arc::new(AtomicBool::new(false));
    let cancellation_seen = Arc::new(AtomicBool::new(false));
    let scanner = {
        let started = started.clone();
        let cancellation_seen = cancellation_seen.clone();
        move |_ports: &[u16], cancellation: &ScanCancellation| {
            started.store(true, Ordering::SeqCst);
            while !cancellation.is_cancelled() {
                thread::sleep(Duration::from_millis(2));
            }
            cancellation_seen.store(true, Ordering::SeqCst);
            Err("scanner noticed cancellation".to_string())
        }
    };
    let inventory = PortInventory::with_scanner_and_timeout(scanner, Duration::from_millis(40));
    let request = inventory.request_scan(&[43_013]).expect("scan request");
    for _ in 0..200 {
        if started.load(Ordering::SeqCst) {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    assert!(started.load(Ordering::SeqCst));
    assert_eq!(
        request.wait(Duration::from_secs(1)),
        Err(PortScanError::TimedOut)
    );
    assert!(matches!(
        inventory.cached_snapshot().observation(43_013),
        Some(PortObservation::ProbeError(detail)) if detail.contains("timed out")
    ));
    for _ in 0..200 {
        if cancellation_seen.load(Ordering::SeqCst) {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    assert!(cancellation_seen.load(Ordering::SeqCst));
    inventory.shutdown();
    assert!(matches!(
        inventory.request_scan(&[43_014]),
        Err(PortScanError::Shutdown)
    ));
}

#[test]
fn port_inventory_shutdown_completes_active_and_pending_requests() {
    let started = Arc::new(AtomicBool::new(false));
    let scanner = {
        let started = started.clone();
        move |_ports: &[u16], cancellation: &ScanCancellation| {
            started.store(true, Ordering::SeqCst);
            while !cancellation.is_cancelled() {
                thread::sleep(Duration::from_millis(2));
            }
            Err("scanner stopped".to_string())
        }
    };
    let inventory = PortInventory::with_scanner_and_timeout(scanner, Duration::from_secs(1));
    let active = inventory.request_scan(&[43_015]).expect("active request");
    for _ in 0..200 {
        if started.load(Ordering::SeqCst) {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    assert!(started.load(Ordering::SeqCst));
    let pending = inventory.request_scan(&[43_016]).expect("pending request");
    inventory.shutdown();

    assert_eq!(
        active.wait(Duration::from_secs(1)),
        Err(PortScanError::Shutdown)
    );
    assert_eq!(
        pending.wait(Duration::from_secs(1)),
        Err(PortScanError::Shutdown)
    );
    assert!(matches!(
        inventory.request_scan(&[43_017]),
        Err(PortScanError::Shutdown)
    ));
}

#[test]
fn port_inventory_bounds_waiter_queue() {
    let started = Arc::new(AtomicBool::new(false));
    let scanner = {
        let started = started.clone();
        move |_ports: &[u16], cancellation: &ScanCancellation| {
            started.store(true, Ordering::SeqCst);
            while !cancellation.is_cancelled() {
                thread::sleep(Duration::from_millis(2));
            }
            Err("scanner stopped".to_string())
        }
    };
    let inventory = PortInventory::with_scanner_and_timeout(scanner, Duration::from_secs(1));
    let active = inventory.request_scan(&[43_022]).expect("active request");
    for _ in 0..200 {
        if started.load(Ordering::SeqCst) {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    assert!(started.load(Ordering::SeqCst));

    let mut pending = Vec::new();
    for port in 43_023..43_023 + (MAX_SCAN_WAITERS as u16 - 1) {
        pending.push(
            inventory
                .request_scan(&[port])
                .expect("bounded pending request"),
        );
    }
    assert!(matches!(
        inventory.request_scan(&[43_090]),
        Err(PortScanError::QueueFull {
            actual,
            max: MAX_SCAN_WAITERS,
        }) if actual == MAX_SCAN_WAITERS + 1
    ));
    inventory.shutdown();
    assert_eq!(
        active.wait(Duration::from_secs(1)),
        Err(PortScanError::Shutdown)
    );
    for request in pending {
        assert_eq!(
            request.wait(Duration::from_secs(1)),
            Err(PortScanError::Shutdown)
        );
    }
}

#[test]
fn port_inventory_absolute_timeout_returns_before_an_uncooperative_scanner_finishes() {
    let finished = Arc::new(AtomicBool::new(false));
    let scanner = {
        let finished = finished.clone();
        move |ports: &[u16], _cancellation: &ScanCancellation| {
            thread::sleep(Duration::from_millis(300));
            finished.store(true, Ordering::SeqCst);
            Ok(free_scan(ports))
        }
    };
    let inventory = PortInventory::with_scanner_and_timeout(scanner, Duration::from_millis(50));
    let request = inventory.request_scan(&[43_018]).expect("scan request");
    let started_at = Instant::now();
    assert_eq!(
        request.wait(Duration::from_secs(1)),
        Err(PortScanError::TimedOut)
    );
    assert!(started_at.elapsed() < Duration::from_millis(250));
    assert!(!finished.load(Ordering::SeqCst));
    inventory.shutdown();
    for _ in 0..200 {
        if finished.load(Ordering::SeqCst) {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    assert!(finished.load(Ordering::SeqCst));
}

#[test]
fn stale_late_publication_is_discarded_and_refresh_returns_its_exact_result() {
    let scanner = |_ports: &[u16], _cancellation: &ScanCancellation| Ok(free_scan(&[43_020]));
    let inventory = PortInventory::with_scanner(scanner);
    let stale = Arc::new(free_scan(&[43_019]).with_publication_sequence(1));
    let current = Arc::new(free_scan(&[43_021]).with_publication_sequence(2));
    assert!(inventory.publish_if_newer(stale.clone()));
    assert!(inventory.publish_if_newer(current.clone()));
    assert!(!inventory.publish_if_newer(stale));
    assert!(Arc::ptr_eq(&inventory.cached_snapshot(), &current));

    let refreshed = inventory.refresh(&[43_020]).expect("exact refresh result");
    assert_eq!(refreshed.requested_ports(), &[43_020]);
    assert_eq!(refreshed.observation(43_019), None);
    assert_eq!(refreshed.publication_sequence(), 3);
    assert!(Arc::ptr_eq(&refreshed, &inventory.cached_snapshot()));
    inventory.shutdown();
}

#[test]
fn late_scan_result_cannot_overwrite_the_newest_coalesced_publication() {
    let call_count = Arc::new(AtomicUsize::new(0));
    let first_started = Arc::new(AtomicBool::new(false));
    let scanner = {
        let call_count = call_count.clone();
        let first_started = first_started.clone();
        move |ports: &[u16], _cancellation: &ScanCancellation| {
            if call_count.fetch_add(1, Ordering::SeqCst) == 0 {
                first_started.store(true, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(220));
                Ok(free_scan(&[43_024]))
            } else {
                Ok(free_scan(ports))
            }
        }
    };
    let inventory = PortInventory::with_scanner_and_timeout(scanner, Duration::from_millis(40));
    let old = inventory.request_scan(&[43_024]).expect("old scan");
    for _ in 0..200 {
        if first_started.load(Ordering::SeqCst) {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    assert!(first_started.load(Ordering::SeqCst));
    let newest = inventory.request_scan(&[43_025]).expect("newest scan");
    assert_eq!(
        old.wait(Duration::from_secs(1)),
        Err(PortScanError::TimedOut)
    );
    let newest_result = newest.wait(Duration::from_secs(1)).expect("newest result");
    assert_eq!(newest_result.requested_ports(), &[43_025]);
    assert_eq!(newest_result.publication_sequence(), 2);
    assert!(Arc::ptr_eq(&newest_result, &inventory.cached_snapshot()));
    inventory.shutdown();
}

#[test]
fn ambiguous_legacy_status_does_not_claim_one_managed_pid() {
    let first = listener(11_011, 1_101);
    let second = listener(11_012, 1_102);
    let snapshot = PortInventorySnapshot::new(BTreeMap::from([(
        43001,
        PortObservation::Listeners(Arc::from(vec![first, second].into_boxed_slice())),
    )]));

    let statuses = legacy_statuses_from_snapshot(&snapshot, &[43001]).expect("legacy status");
    let status = statuses.get(&43001).expect("port status");
    assert!(status.in_use);
    assert_eq!(status.pid, None);
}

#[test]
fn real_temporary_listener_is_observed_and_never_touched_by_rejection() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("temporary listener");
    let port = listener.local_addr().expect("listener address").port();

    let snapshot = scan_listener_inventory(&[port]).expect("listener inventory");
    let observed = snapshot.observation(port).expect("observed port");
    let observed_listener = match observed {
        PortObservation::Listeners(listeners) if listeners.len() == 1 => listeners[0],
        other => panic!("expected listener observation, got {other:?}"),
    };
    assert_eq!(observed_listener.pid(), std::process::id());
    let endpoints = snapshot.endpoints(port);
    assert_eq!(endpoints.len(), 1);
    assert_eq!(endpoints[0].identity(), observed_listener);
    assert_eq!(endpoints[0].protocol(), TcpProtocol::Tcp);
    assert_eq!(endpoints[0].family(), TcpAddressFamily::Ipv4);
    assert_eq!(endpoints[0].port(), port);
    assert!(endpoints[0].bind_address().is_loopback());

    let error = ensure_managed_start_allowed(&snapshot, port).expect_err("occupied port");
    assert_eq!(
        error,
        PortStartError::Occupied {
            port,
            listener: observed_listener,
        }
    );

    let kill_error =
        kill_port(port).expect_err("ownership-unverified listener must never be killed");
    assert!(kill_error.contains("exact managed resource"));

    let still_occupied = TcpListener::bind(("127.0.0.1", port));
    assert!(
        still_occupied.is_err(),
        "rejection must not close the listener"
    );
    drop(listener);
}

#[test]
fn real_ipv6_wildcard_and_dual_stack_listeners_preserve_endpoint_rows_when_supported() {
    let ipv6 = match TcpListener::bind(("::1", 0)) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("IPv6 listener unsupported; skipping IPv6 endpoint assertions: {error}");
            return;
        }
    };
    let ipv6_port = ipv6.local_addr().expect("IPv6 listener address").port();
    let ipv6_snapshot = scan_listener_inventory(&[ipv6_port]).expect("IPv6 inventory");
    let ipv6_endpoints = ipv6_snapshot.endpoints(ipv6_port);
    assert!(ipv6_endpoints.iter().any(|endpoint| {
        endpoint.family() == TcpAddressFamily::Ipv6
            && endpoint.bind_address().is_loopback()
            && endpoint.identity().pid() == std::process::id()
    }));

    let wildcard = TcpListener::bind(("0.0.0.0", 0)).expect("IPv4 wildcard listener");
    let wildcard_port = wildcard
        .local_addr()
        .expect("IPv4 wildcard listener address")
        .port();
    let wildcard_snapshot = scan_listener_inventory(&[wildcard_port]).expect("wildcard inventory");
    assert!(wildcard_snapshot
        .endpoints(wildcard_port)
        .iter()
        .any(|endpoint| endpoint.family() == TcpAddressFamily::Ipv4 && endpoint.is_wildcard()));

    let dual_stack = match TcpListener::bind(("::", 0)) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!(
                "IPv6 wildcard listener unsupported; skipping dual-stack assertions: {error}"
            );
            return;
        }
    };
    let dual_port = dual_stack
        .local_addr()
        .expect("IPv6 wildcard listener address")
        .port();
    let dual_ipv4 = TcpListener::bind(("0.0.0.0", dual_port)).ok();
    let dual_snapshot = scan_listener_inventory(&[dual_port]).expect("dual-stack inventory");
    let dual_endpoints = dual_snapshot.endpoints(dual_port);
    assert!(dual_endpoints
        .iter()
        .any(|endpoint| { endpoint.family() == TcpAddressFamily::Ipv6 && endpoint.is_wildcard() }));
    if dual_ipv4.is_some() {
        assert!(dual_endpoints.iter().any(|endpoint| {
            endpoint.family() == TcpAddressFamily::Ipv4 && endpoint.is_wildcard()
        }));
    }
}
