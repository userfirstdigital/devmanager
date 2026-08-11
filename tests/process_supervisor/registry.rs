//! Managed process registry acceptance tests.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use devmanager::domain::id::{ResourceId, TaskId};
use devmanager::domain::operation::ResourceFence;
use devmanager::kernel::RuntimeRegistryError;
use devmanager::process::identity::{ManagedProcessId, ManagedProcessIdentity, ProcessOwner};
use devmanager::process::registry::{
    JobMembership, OwnershipFault, ProcessClassification, ProcessDisplayLabel,
    ProcessDisplayLabelError, ProcessRegistry, ProcessRegistryError, RegisteredProcess,
    UnregisterOutcome, MAX_PROCESS_DISPLAY_LABEL_BYTES,
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

fn current_executable() -> PathBuf {
    std::env::current_exe().expect("current test executable")
}

fn other_existing_file() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml")
}

fn identity(pid: u32, creation_time_100ns: u64, executable: &Path) -> ManagedProcessIdentity {
    ManagedProcessIdentity::new(
        ManagedProcessId::new(pid, creation_time_100ns).expect("managed process id"),
        executable,
    )
    .expect("managed process identity")
}

#[derive(Debug)]
struct DropSpy {
    id: u8,
    dropped: Arc<Mutex<Vec<u8>>>,
    membership: Arc<Mutex<Result<Vec<u32>, String>>>,
    root_identity: Option<ManagedProcessIdentity>,
}

impl DropSpy {
    fn member(id: u8, pid: u32, dropped: &Arc<Mutex<Vec<u8>>>) -> Self {
        Self::with_membership(id, dropped, Arc::new(Mutex::new(Ok(vec![pid]))))
    }

    fn without_member(id: u8, dropped: &Arc<Mutex<Vec<u8>>>) -> Self {
        Self::with_membership(id, dropped, Arc::new(Mutex::new(Ok(Vec::new()))))
    }

    fn query_error(id: u8, dropped: &Arc<Mutex<Vec<u8>>>, detail: &str) -> Self {
        Self::with_membership(id, dropped, Arc::new(Mutex::new(Err(detail.to_string()))))
    }

    fn with_membership(
        id: u8,
        dropped: &Arc<Mutex<Vec<u8>>>,
        membership: Arc<Mutex<Result<Vec<u32>, String>>>,
    ) -> Self {
        Self {
            id,
            dropped: Arc::clone(dropped),
            membership,
            root_identity: None,
        }
    }
}

impl JobMembership for DropSpy {
    fn active_process_ids(&self) -> Result<Vec<u32>, String> {
        self.membership.lock().expect("membership state").clone()
    }

    fn inspect_process(
        &self,
        pid: u32,
    ) -> Result<devmanager::process::registry::JobMemberInfo, String> {
        self.root_identity
            .as_ref()
            .filter(|identity| identity.id().pid() == pid)
            .cloned()
            .map(|identity| devmanager::process::registry::JobMemberInfo::new(identity, None))
            .ok_or_else(|| format!("process identity for PID {pid} is inaccessible"))
    }
}

impl Drop for DropSpy {
    fn drop(&mut self) {
        self.dropped.lock().expect("drop ledger").push(self.id);
    }
}

fn registration(
    resource_id: ResourceId,
    generation: u64,
    owner: ProcessOwner,
    root: ManagedProcessIdentity,
    job: DropSpy,
) -> RegisteredProcess<DropSpy> {
    let mut job = job;
    job.root_identity = Some(root.clone());
    RegisteredProcess::new(
        ResourceFence::new(resource_id, generation),
        owner,
        root,
        ProcessDisplayLabel::new("Claude task").expect("display label"),
        job,
    )
}

#[test]
fn stale_generation_cannot_unregister_new_process() {
    let dropped = Arc::new(Mutex::new(Vec::new()));
    let resource = resource_id(1);
    let executable = current_executable();
    let mut registry = ProcessRegistry::new();

    let stale_fence = registry
        .register(registration(
            resource,
            1,
            ProcessOwner::Task(TaskId::new()),
            identity(101, 1_000, &executable),
            DropSpy::member(1, 101, &dropped),
        ))
        .expect("first registration");
    let first = match registry
        .rollback_starting_exact(&stale_fence)
        .expect("rollback first provisional generation")
    {
        UnregisterOutcome::Removed(process) => process,
        UnregisterOutcome::Stale => panic!("current generation must unregister"),
    };

    let current_fence = registry
        .register(registration(
            resource,
            2,
            ProcessOwner::Host,
            identity(202, 2_000, &executable),
            DropSpy::member(2, 202, &dropped),
        ))
        .expect("replacement registration");

    assert!(matches!(
        registry
            .rollback_starting_exact(&stale_fence)
            .expect("stale rollback is a disposition"),
        UnregisterOutcome::Stale
    ));
    assert_eq!(
        registry.current(resource).expect("current process").fence(),
        current_fence.resource()
    );
    assert_eq!(
        registry
            .current(resource)
            .expect("current process")
            .job()
            .id,
        2
    );
    assert!(dropped.lock().expect("drop ledger").is_empty());
    drop(first);
}

#[test]
fn live_generation_replacement_preserves_current_job() {
    let dropped = Arc::new(Mutex::new(Vec::new()));
    let resource = resource_id(2);
    let executable = current_executable();
    let mut registry = ProcessRegistry::new();

    let current_fence = registry
        .register(registration(
            resource,
            4,
            ProcessOwner::Host,
            identity(303, 3_000, &executable),
            DropSpy::member(1, 303, &dropped),
        ))
        .expect("current registration");
    let failure = registry
        .register(registration(
            resource,
            5,
            ProcessOwner::Host,
            identity(404, 4_000, &executable),
            DropSpy::member(2, 404, &dropped),
        ))
        .expect_err("live replacement must fail");

    assert!(matches!(
        failure.reason(),
        ProcessRegistryError::ActiveGeneration { current, proposed }
            if *current == current_fence.resource()
                && *proposed == ResourceFence::new(resource, 5)
    ));
    assert_eq!(
        registry
            .current(resource)
            .expect("current process")
            .job()
            .id,
        1
    );
    assert!(dropped.lock().expect("drop ledger").is_empty());

    let (reason, rejected) = failure.into_parts();
    assert!(matches!(
        reason,
        ProcessRegistryError::ActiveGeneration { .. }
    ));
    assert_eq!(rejected.job().id, 2);
    assert!(dropped.lock().expect("drop ledger").is_empty());
    drop(rejected);
    assert_eq!(*dropped.lock().expect("drop ledger"), vec![2]);
}

#[test]
fn retired_generation_cannot_be_reused() {
    let dropped = Arc::new(Mutex::new(Vec::new()));
    let resource = resource_id(3);
    let executable = current_executable();
    let mut registry = ProcessRegistry::new();

    let fence = registry
        .register(registration(
            resource,
            7,
            ProcessOwner::Host,
            identity(505, 5_000, &executable),
            DropSpy::member(1, 505, &dropped),
        ))
        .expect("registration");
    let removed = registry
        .rollback_starting_exact(&fence)
        .expect("rollback current provisional generation");
    drop(removed);
    assert_eq!(*dropped.lock().expect("drop ledger"), vec![1]);

    let failure = registry
        .register(registration(
            resource,
            7,
            ProcessOwner::Host,
            identity(606, 6_000, &executable),
            DropSpy::member(2, 606, &dropped),
        ))
        .expect_err("retired generation must not be reused");
    assert!(matches!(
        failure.reason(),
        ProcessRegistryError::Runtime(RuntimeRegistryError::GenerationNotAdvanced {
            resource_id,
            current_generation: 7,
            proposed_generation: 7,
        }) if *resource_id == resource
    ));
    assert!(registry.current(resource).is_none());
    assert_eq!(*dropped.lock().expect("drop ledger"), vec![1]);

    let (_, rejected) = failure.into_parts();
    drop(rejected);
    assert_eq!(*dropped.lock().expect("drop ledger"), vec![1, 2]);
}

#[test]
fn duplicate_root_pid_cannot_gain_second_owner() {
    let dropped = Arc::new(Mutex::new(Vec::new()));
    let executable = current_executable();
    let first_resource = resource_id(4);
    let second_resource = resource_id(5);
    let first_owner = ProcessOwner::Task(TaskId::new());
    let mut registry = ProcessRegistry::new();

    registry
        .register(registration(
            first_resource,
            1,
            first_owner,
            identity(707, 7_000, &executable),
            DropSpy::member(1, 707, &dropped),
        ))
        .expect("first owner");
    let failure = registry
        .register(registration(
            second_resource,
            1,
            ProcessOwner::Host,
            identity(707, 8_000, &executable),
            DropSpy::member(2, 707, &dropped),
        ))
        .expect_err("duplicate active PID must fail");

    assert!(matches!(
        failure.reason(),
        ProcessRegistryError::DuplicateActivePid {
            pid: 707,
            existing_resource,
            proposed_resource,
        } if *existing_resource == first_resource && *proposed_resource == second_resource
    ));
    assert_eq!(
        registry
            .current(first_resource)
            .expect("first registration")
            .owner(),
        first_owner
    );
    assert!(registry.current(second_resource).is_none());
    assert!(dropped.lock().expect("drop ledger").is_empty());

    let (_, rejected) = failure.into_parts();
    assert_eq!(rejected.job().id, 2);
    drop(rejected);
    assert_eq!(*dropped.lock().expect("drop ledger"), vec![2]);
}

#[test]
fn registration_requires_job_membership() {
    let dropped = Arc::new(Mutex::new(Vec::new()));
    let resource = resource_id(12);
    let mut registry = ProcessRegistry::new();

    let failure = registry
        .register(registration(
            resource,
            1,
            ProcessOwner::Host,
            identity(1_313, 16_000, &current_executable()),
            DropSpy::without_member(1, &dropped),
        ))
        .expect_err("a root outside the Job cannot be registered");

    assert!(matches!(
        failure.reason(),
        ProcessRegistryError::NotJobMember { resource_id, pid: 1_313 }
            if *resource_id == resource
    ));
    assert!(registry.current(resource).is_none());
    assert!(dropped.lock().expect("drop ledger").is_empty());

    let (_, rejected) = failure.into_parts();
    assert_eq!(rejected.job().id, 1);
    assert!(dropped.lock().expect("drop ledger").is_empty());
    drop(rejected);
    assert_eq!(*dropped.lock().expect("drop ledger"), vec![1]);
}

#[test]
fn membership_query_failure_returns_rejected_job() {
    let dropped = Arc::new(Mutex::new(Vec::new()));
    let resource = resource_id(13);
    let mut registry = ProcessRegistry::new();

    let failure = registry
        .register(registration(
            resource,
            1,
            ProcessOwner::Host,
            identity(1_414, 17_000, &current_executable()),
            DropSpy::query_error(1, &dropped, "query failed"),
        ))
        .expect_err("an unreadable Job cannot prove ownership");

    assert!(matches!(
        failure.reason(),
        ProcessRegistryError::MembershipQueryFailed { resource_id, detail }
            if *resource_id == resource && detail == "query failed"
    ));
    assert!(registry.current(resource).is_none());
    assert!(dropped.lock().expect("drop ledger").is_empty());

    let (_, rejected) = failure.into_parts();
    drop(rejected);
    assert_eq!(*dropped.lock().expect("drop ledger"), vec![1]);
}

#[test]
fn pid_reuse_is_reconciliation_fault() {
    let dropped = Arc::new(Mutex::new(Vec::new()));
    let executable = current_executable();
    let mut registry = ProcessRegistry::new();
    registry
        .register(registration(
            resource_id(6),
            1,
            ProcessOwner::Host,
            identity(808, 9_000, &executable),
            DropSpy::member(1, 808, &dropped),
        ))
        .expect("registration");

    let observed = identity(808, 10_000, &executable);
    assert!(matches!(
        registry.classify_root(&observed),
        ProcessClassification::ReconciliationFault {
            observed: actual,
            reason: OwnershipFault::IdentityMismatch,
            ..
        } if actual == observed
    ));
}

#[test]
fn external_process_cannot_become_managed() {
    let dropped = Arc::new(Mutex::new(Vec::new()));
    let executable = current_executable();
    let resource = resource_id(7);
    let mut registry = ProcessRegistry::new();
    registry
        .register(registration(
            resource,
            1,
            ProcessOwner::Host,
            identity(909, 11_000, &executable),
            DropSpy::member(1, 909, &dropped),
        ))
        .expect("registration");

    let external = identity(910, 12_000, &executable);
    assert_eq!(
        registry.classify_root(&external),
        ProcessClassification::External
    );
    assert_eq!(registry.len(), 1);
    assert_eq!(
        registry
            .current(resource)
            .expect("managed root")
            .root()
            .id()
            .pid(),
        909
    );
}

#[test]
fn exact_root_is_classified_as_managed() {
    let dropped = Arc::new(Mutex::new(Vec::new()));
    let executable = current_executable();
    let mut registry = ProcessRegistry::new();
    let fence = registry
        .register(registration(
            resource_id(11),
            1,
            ProcessOwner::Host,
            identity(911, 12_500, &executable),
            DropSpy::member(1, 911, &dropped),
        ))
        .expect("registration");

    let observed = identity(911, 12_500, &executable);
    assert_eq!(
        registry.classify_root(&observed),
        ProcessClassification::Managed(fence)
    );
}

#[test]
fn exact_identity_without_job_membership_is_reconciliation_fault() {
    let dropped = Arc::new(Mutex::new(Vec::new()));
    let membership = Arc::new(Mutex::new(Ok(vec![1_515])));
    let executable = current_executable();
    let mut registry = ProcessRegistry::new();
    registry
        .register(registration(
            resource_id(14),
            1,
            ProcessOwner::Host,
            identity(1_515, 18_000, &executable),
            DropSpy::with_membership(1, &dropped, Arc::clone(&membership)),
        ))
        .expect("initial membership proves registration");

    *membership.lock().expect("membership state") = Ok(Vec::new());
    let observed = identity(1_515, 18_000, &executable);
    assert!(matches!(
        registry.classify_root(&observed),
        ProcessClassification::ReconciliationFault {
            observed: actual,
            reason: OwnershipFault::NotJobMember,
            ..
        } if actual == observed
    ));
}

#[test]
fn exact_identity_with_membership_query_error_is_reconciliation_fault() {
    let dropped = Arc::new(Mutex::new(Vec::new()));
    let membership = Arc::new(Mutex::new(Ok(vec![1_616])));
    let executable = current_executable();
    let mut registry = ProcessRegistry::new();
    registry
        .register(registration(
            resource_id(15),
            1,
            ProcessOwner::Host,
            identity(1_616, 19_000, &executable),
            DropSpy::with_membership(1, &dropped, Arc::clone(&membership)),
        ))
        .expect("initial membership proves registration");

    *membership.lock().expect("membership state") = Err("job query failed".to_string());
    let observed = identity(1_616, 19_000, &executable);
    assert!(matches!(
        registry.classify_root(&observed),
        ProcessClassification::ReconciliationFault {
            observed: actual,
            reason: OwnershipFault::MembershipQueryFailed { detail },
            ..
        } if actual == observed && detail == "job query failed"
    ));
}

#[test]
fn canonical_executable_mismatch_is_reconciliation_fault() {
    let dropped = Arc::new(Mutex::new(Vec::new()));
    let executable = current_executable();
    let mut registry = ProcessRegistry::new();
    registry
        .register(registration(
            resource_id(8),
            1,
            ProcessOwner::Host,
            identity(1_010, 13_000, &executable),
            DropSpy::member(1, 1_010, &dropped),
        ))
        .expect("registration");

    let observed = identity(1_010, 13_000, &other_existing_file());
    assert!(matches!(
        registry.classify_root(&observed),
        ProcessClassification::ReconciliationFault {
            observed: actual,
            reason: OwnershipFault::IdentityMismatch,
            ..
        } if actual == observed
    ));
}

#[test]
fn blank_display_label_is_rejected() {
    assert!(matches!(
        ProcessDisplayLabel::new("   "),
        Err(ProcessDisplayLabelError::Empty)
    ));
}

#[test]
fn oversized_display_label_is_rejected() {
    let label = "x".repeat(MAX_PROCESS_DISPLAY_LABEL_BYTES + 1);
    assert!(matches!(
        ProcessDisplayLabel::new(label),
        Err(ProcessDisplayLabelError::TooLong {
            actual,
            max: MAX_PROCESS_DISPLAY_LABEL_BYTES,
        }) if actual == MAX_PROCESS_DISPLAY_LABEL_BYTES + 1
    ));
}
