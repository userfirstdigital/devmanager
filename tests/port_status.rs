use std::collections::BTreeMap;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use devmanager::domain::id::ResourceId;
use devmanager::domain::operation::ResourceFence;
use devmanager::process::identity::{ManagedProcessId, ManagedProcessIdentity, ProcessOwner};
use devmanager::process::ports::{
    ensure_managed_start_allowed, project_port_status, registered_resource_snapshot,
    ListenerIdentity, ManagedPortHealth, PortInventorySnapshot, PortObservation, PortStartError,
    PortStatusKind, PortTarget,
};
use devmanager::process::registry::{
    JobMembership, ManagedProcessFence, ProcessRegistry, RegisteredProcess,
};
use devmanager::services::ports_service::{kill_port, scan_listener_inventory, PortInventory};

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
fn matching_managed_ready_listener_is_green() {
    let resource = fence(2, 1);
    let (mut registry, managed_fence) = registry_with_root(resource, 11_002, 202);
    registry
        .commit_resumed_exact(&managed_fence)
        .expect("resume generation");
    let managed = registered_resource_snapshot(&registry, resource).expect("current resource");

    let status = project_port_status(
        &target(resource),
        &PortObservation::Listener(listener(11_002, 202)),
        Some(&managed),
    );

    assert_eq!(status.kind(), PortStatusKind::ManagedHealthy);
}

#[test]
fn listener_owned_by_another_resource_is_blue() {
    let resource = fence(3, 1);
    let status = project_port_status(
        &target(resource),
        &PortObservation::Listener(listener(11_003, 303)),
        None,
    );

    assert_eq!(status.kind(), PortStatusKind::External);
    assert_eq!(status.listener(), Some(listener(11_003, 303)));
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
        &PortObservation::Listener(listener(11_006, 999_999)),
        Some(&managed),
    );

    assert_eq!(status.kind(), PortStatusKind::External);
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
        &PortObservation::Listener(listener(11_007, 707)),
        stale_snapshot.as_ref(),
    );

    assert_eq!(status.kind(), PortStatusKind::External);
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
fn occupied_external_start_is_rejected_with_listener_evidence() {
    let occupied = listener(11_009, 909);
    let snapshot = PortInventorySnapshot::new(BTreeMap::from([(
        43001,
        PortObservation::Listener(occupied),
    )]));

    let error = ensure_managed_start_allowed(&snapshot, 43001).expect_err("occupied port");

    assert_eq!(
        error,
        PortStartError::OccupiedExternal {
            port: 43001,
            listener: occupied,
        }
    );
    assert!(error.to_string().contains("PID 11009"));
    assert!(error.to_string().contains("creation 909"));
}

#[test]
fn real_temporary_listener_is_observed_and_never_touched_by_rejection() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("temporary listener");
    let port = listener.local_addr().expect("listener address").port();

    let snapshot = scan_listener_inventory(&[port]).expect("listener inventory");
    let observed = snapshot.observation(port).expect("observed port");
    let observed_listener = match observed {
        PortObservation::Listener(listener) => *listener,
        other => panic!("expected listener observation, got {other:?}"),
    };
    assert_eq!(observed_listener.pid(), std::process::id());

    let error = ensure_managed_start_allowed(&snapshot, port).expect_err("occupied port");
    assert_eq!(
        error,
        PortStartError::OccupiedExternal {
            port,
            listener: observed_listener,
        }
    );

    let kill_error = kill_port(port).expect_err("external listener must never be killed");
    assert!(kill_error.contains("exact managed resource"));

    let still_occupied = TcpListener::bind(("127.0.0.1", port));
    assert!(
        still_occupied.is_err(),
        "rejection must not close the listener"
    );
    drop(listener);
}
