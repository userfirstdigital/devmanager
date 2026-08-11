//! Deterministic Job membership and completion-notification acceptance tests.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use devmanager::domain::id::{OperationId, ResourceId, TaskId};
use devmanager::domain::operation::ResourceFence;
use devmanager::process::identity::{ManagedProcessId, ManagedProcessIdentity, ProcessOwner};
use devmanager::process::registry::{
    JobCompletionEvent, JobCompletionObservation, JobMemberInfo, JobMembership,
    ManagedProcessFence, ManagedProcessState, OwnershipFault, ProcessClassification,
    ProcessDisplayLabel, ProcessRegistry, RegisteredProcess, UnregisterOutcome,
};
use devmanager::process::teardown::{TeardownScope, TeardownTicket};

fn fixed_uuid_v7(tail: u8) -> [u8; 16] {
    [
        0x01, 0x9a, 0x11, 0x22, 0x33, 0x44, 0x70, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        tail,
    ]
}

fn resource_id(tail: u8) -> ResourceId {
    ResourceId::from_bytes(fixed_uuid_v7(tail)).expect("resource id")
}

fn executable() -> PathBuf {
    std::env::current_exe().expect("current test executable")
}

fn identity(pid: u32, creation_time_100ns: u64, executable: &Path) -> ManagedProcessIdentity {
    ManagedProcessIdentity::new(
        ManagedProcessId::new(pid, creation_time_100ns).expect("managed process id"),
        executable,
    )
    .expect("managed process identity")
}

#[derive(Debug)]
struct ScriptedJobState {
    active_pids: Result<Vec<u32>, String>,
    observations: BTreeMap<u32, Result<JobMemberInfo, String>>,
    bound_fence: Option<ManagedProcessFence>,
}

impl Default for ScriptedJobState {
    fn default() -> Self {
        Self {
            active_pids: Ok(Vec::new()),
            observations: BTreeMap::new(),
            bound_fence: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct ScriptedJob(Arc<Mutex<ScriptedJobState>>);

impl ScriptedJob {
    fn with_root(pid: u32) -> Self {
        let job = Self::default();
        job.0.lock().expect("scripted Job").active_pids = Ok(vec![pid]);
        job
    }

    fn set_snapshot(
        &self,
        pids: Vec<u32>,
        observations: Vec<(u32, Result<JobMemberInfo, String>)>,
    ) {
        let mut state = self.0.lock().expect("scripted Job");
        state.active_pids = Ok(pids);
        state.observations = observations.into_iter().collect();
    }

    fn set_membership_error(&self, detail: &str) {
        self.0.lock().expect("scripted Job").active_pids = Err(detail.to_string());
    }
}

impl JobMembership for ScriptedJob {
    fn active_process_ids(&self) -> Result<Vec<u32>, String> {
        self.0.lock().expect("scripted Job").active_pids.clone()
    }

    fn inspect_process(&self, pid: u32) -> Result<JobMemberInfo, String> {
        self.0
            .lock()
            .expect("scripted Job")
            .observations
            .get(&pid)
            .cloned()
            .unwrap_or_else(|| Err(format!("PID {pid} is inaccessible")))
    }

    fn bind_completion_fence(&mut self, fence: ManagedProcessFence) -> Result<(), String> {
        self.0.lock().expect("scripted Job").bound_fence = Some(fence);
        Ok(())
    }
}

fn registration(
    resource: ResourceId,
    generation: u64,
    root: ManagedProcessIdentity,
    job: ScriptedJob,
) -> RegisteredProcess<ScriptedJob> {
    job.set_snapshot(
        vec![root.id().pid()],
        vec![(
            root.id().pid(),
            Ok(member_with_identity(root.clone(), "managed root")),
        )],
    );
    RegisteredProcess::new(
        ResourceFence::new(resource, generation),
        ProcessOwner::Host,
        root,
        ProcessDisplayLabel::new("membership test").expect("display label"),
        job,
    )
}

fn completion(fence: ManagedProcessFence, event: JobCompletionEvent) -> JobCompletionObservation {
    JobCompletionObservation::new(fence, event)
}

fn receiver_completion(
    registry: &mut ProcessRegistry<ScriptedJob>,
    _resource: ResourceId,
    _job: &ScriptedJob,
    fence: ManagedProcessFence,
    event: JobCompletionEvent,
) -> bool {
    // ScriptedJob is a public-trait fake. Its observations must never create
    // lifecycle or release authority.
    registry.apply_job_observation(completion(fence, event))
}

fn member(pid: u32, creation_time_100ns: u64, command_line: &str) -> JobMemberInfo {
    JobMemberInfo::new(
        identity(pid, creation_time_100ns, &executable()),
        Some(command_line.to_string()),
    )
}

fn member_with_identity(identity: ManagedProcessIdentity, command_line: &str) -> JobMemberInfo {
    JobMemberInfo::new(identity, Some(command_line.to_string()))
}

#[test]
fn membership_registration_requires_full_root_identity_not_pid_only() {
    let pid = 3_005;
    let root = identity(pid, 30_005, &executable());
    let wrong_executable = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let wrong_members = [
        member_with_identity(identity(pid, 30_006, &executable()), "pid-reused"),
        member_with_identity(identity(pid, 30_005, &wrong_executable), "wrong-image"),
    ];

    for (index, wrong_member) in wrong_members.into_iter().enumerate() {
        let resource = resource_id(40 + index as u8);
        let job = ScriptedJob::with_root(pid);
        job.set_snapshot(vec![pid], vec![(pid, Ok(wrong_member))]);
        let mut registry = ProcessRegistry::new();
        let registered = RegisteredProcess::new(
            ResourceFence::new(resource, 1),
            ProcessOwner::Host,
            root.clone(),
            ProcessDisplayLabel::new("wrong identity").expect("display label"),
            job,
        );

        assert!(
            registry.register(registered).is_err(),
            "PID membership must not admit a reused or wrong-image root"
        );
        assert!(registry.current(resource).is_none());
    }
}

#[test]
fn membership_classification_rejects_pid_match_with_wrong_job_identity() {
    let resource = resource_id(43);
    let pid = 3_006;
    let root = identity(pid, 30_007, &executable());
    let job = ScriptedJob::with_root(pid);
    job.set_snapshot(
        vec![pid],
        vec![(pid, Ok(member_with_identity(root.clone(), "root")))],
    );
    let mut registry = ProcessRegistry::new();
    registry
        .register(registration(resource, 1, root.clone(), job.clone()))
        .expect("matching full identity registers");

    job.set_snapshot(
        vec![pid],
        vec![(
            pid,
            Ok(member_with_identity(
                identity(pid, 30_008, &executable()),
                "pid-reused",
            )),
        )],
    );

    assert!(matches!(
        registry.classify_root(&root),
        ProcessClassification::ReconciliationFault {
            reason: OwnershipFault::IdentityMismatch,
            ..
        }
    ));
}

#[test]
fn membership_root_exit_does_not_stop_a_live_grandchild() {
    let resource = resource_id(1);
    let root = identity(1_001, 10_001, &executable());
    let grandchild = member(1_003, 10_003, "grandchild --work");
    let job = ScriptedJob::with_root(root.id().pid());
    let mut registry = ProcessRegistry::new();
    let fence = registry
        .register(registration(resource, 1, root, job.clone()))
        .expect("registration");

    job.set_snapshot(
        vec![grandchild.identity().id().pid()],
        vec![(grandchild.identity().id().pid(), Ok(grandchild.clone()))],
    );
    assert!(receiver_completion(
        &mut registry,
        resource,
        &job,
        fence.clone(),
        JobCompletionEvent::NewProcess { pid: 1_003 },
    ));
    assert!(receiver_completion(
        &mut registry,
        resource,
        &job,
        fence.clone(),
        JobCompletionEvent::ExitProcess { pid: 1_001 },
    ));
    registry
        .reconcile_membership(resource)
        .expect("authoritative Job snapshot");
    let current = registry.current(resource).expect("current generation");
    assert_eq!(current.state(), ManagedProcessState::Running);
    assert_eq!(current.known_members(), &[grandchild]);
    assert!(current.unknown_member_pids().is_empty());

    assert!(receiver_completion(
        &mut registry,
        resource,
        &job,
        fence.clone(),
        JobCompletionEvent::ActiveProcessZero,
    ));
    assert_eq!(
        registry
            .current(resource)
            .expect("current generation")
            .state(),
        ManagedProcessState::Running,
        "a zero notification cannot override a non-empty authoritative Job query"
    );

    job.set_snapshot(Vec::new(), Vec::new());
    registry
        .reconcile_membership(resource)
        .expect("empty snapshot is observable");
    assert_eq!(
        registry
            .current(resource)
            .expect("current generation")
            .state(),
        ManagedProcessState::Running,
        "an empty query is not the ACTIVE_PROCESS_ZERO notification"
    );

    assert!(receiver_completion(
        &mut registry,
        resource,
        &job,
        fence,
        JobCompletionEvent::ActiveProcessZero,
    ));
    assert_eq!(
        registry
            .current(resource)
            .expect("current generation")
            .state(),
        ManagedProcessState::Running,
        "a public-trait fake cannot mint receiver-owned zero proof"
    );
}

#[test]
fn membership_public_drain_does_not_upgrade_forged_zero_to_release_proof() {
    let resource = resource_id(16);
    let root = identity(16_001, 160_001, &executable());
    let job = ScriptedJob::with_root(root.id().pid());
    let mut registry = ProcessRegistry::new();
    let fence = registry
        .register(registration(resource, 1, root, job.clone()))
        .expect("registration");

    job.set_snapshot(Vec::new(), Vec::new());
    assert!(receiver_completion(
        &mut registry,
        resource,
        &job,
        fence.clone(),
        JobCompletionEvent::ActiveProcessZero,
    ));
    assert!(matches!(
        registry.active_process_zero_proof_exact(&fence),
        Err(devmanager::process::registry::ProcessRegistryError::ActiveProcessZeroUnproved { .. })
    ));
    assert_eq!(
        registry
            .current(resource)
            .expect("caller-owned Job remains registered")
            .state(),
        ManagedProcessState::Starting
    );
}

#[test]
fn membership_rapid_forks_reconcile_known_and_inaccessible_members_without_loss() {
    let resource = resource_id(2);
    let root = identity(2_000, 20_000, &executable());
    let job = ScriptedJob::with_root(root.id().pid());
    let mut registry = ProcessRegistry::new();
    registry
        .register(registration(resource, 1, root, job.clone()))
        .expect("registration");

    let pids: Vec<u32> = (2_001..=2_064).collect();
    let observations = pids
        .iter()
        .copied()
        .map(|pid| {
            if pid % 4 == 0 {
                (pid, Err("access denied".to_string()))
            } else {
                (pid, Ok(member(pid, 20_000 + pid as u64, "worker")))
            }
        })
        .collect();
    job.set_snapshot(pids, observations);

    registry
        .reconcile_membership(resource)
        .expect("rapid-fork snapshot");
    let current = registry.current(resource).expect("current generation");
    assert_eq!(current.known_members().len(), 48);
    assert_eq!(current.unknown_member_pids().len(), 16);
    assert_eq!(current.member_count(), 64);
    assert!(current.unknown_member_pids().contains(&2_004));
    assert!(current.unknown_member_pids().contains(&2_064));
}

#[test]
fn membership_pid_reuse_replaces_the_full_process_identity() {
    let resource = resource_id(3);
    let pid = 3_003;
    let root = identity(pid, 30_001, &executable());
    let job = ScriptedJob::with_root(pid);
    let mut registry = ProcessRegistry::new();
    registry
        .register(registration(resource, 1, root, job.clone()))
        .expect("registration");

    let first = member(pid, 30_001, "worker --first");
    job.set_snapshot(vec![pid], vec![(pid, Ok(first.clone()))]);
    registry
        .reconcile_membership(resource)
        .expect("first identity snapshot");
    assert_eq!(
        registry
            .current(resource)
            .expect("current generation")
            .known_members(),
        &[first]
    );

    let replacement = member(pid, 30_002, "worker --replacement");
    job.set_snapshot(vec![pid], vec![(pid, Ok(replacement.clone()))]);
    registry
        .reconcile_membership(resource)
        .expect("reused PID snapshot");
    assert_eq!(
        registry
            .current(resource)
            .expect("current generation")
            .known_members(),
        &[replacement]
    );
}

#[test]
fn membership_stale_completion_cannot_mutate_replacement_generation() {
    let resource = resource_id(4);
    let replacement_resource_fence = ResourceFence::new(resource, 2);
    let stale_job = ScriptedJob::with_root(4_001);
    let replacement_job = ScriptedJob::with_root(4_002);
    let mut registry = ProcessRegistry::new();

    let stale_fence = registry
        .register(registration(
            resource,
            1,
            identity(4_001, 40_001, &executable()),
            stale_job.clone(),
        ))
        .expect("stale generation registration");
    let removed = registry
        .rollback_starting_exact(&stale_fence)
        .expect("rollback stale provisional generation");
    assert!(matches!(removed, UnregisterOutcome::Removed(_)));

    let replacement_fence = registry
        .register(registration(
            resource,
            2,
            identity(4_002, 40_002, &executable()),
            replacement_job.clone(),
        ))
        .expect("replacement registration");
    registry
        .commit_resumed_exact(&replacement_fence)
        .expect("replacement resume commit");
    assert!(receiver_completion(
        &mut registry,
        resource,
        &replacement_job,
        replacement_fence.clone(),
        JobCompletionEvent::NewProcess { pid: 4_002 },
    ));

    let wrong_owner = ManagedProcessFence::new(
        replacement_fence.resource(),
        ProcessOwner::Task(TaskId::new()),
        replacement_fence.root().clone(),
    );
    assert!(!registry.apply_job_observation(completion(
        wrong_owner,
        JobCompletionEvent::MonitorFailed {
            detail: "wrong owner".to_string(),
        },
    )));
    let wrong_root = ManagedProcessFence::new(
        replacement_fence.resource(),
        replacement_fence.owner(),
        identity(4_002, 49_999, &executable()),
    );
    assert!(!registry.apply_job_observation(completion(
        wrong_root,
        JobCompletionEvent::AbnormalExitProcess { pid: 4_002 },
    )));

    assert!(!registry.apply_job_observation(completion(
        stale_fence.clone(),
        JobCompletionEvent::ExitProcess { pid: 4_001 },
    )));
    assert!(!registry.apply_job_observation(completion(
        stale_fence.clone(),
        JobCompletionEvent::AbnormalExitProcess { pid: 4_001 },
    )));
    assert!(!registry.apply_job_observation(completion(
        stale_fence,
        JobCompletionEvent::ActiveProcessZero,
    )));

    let current = registry.current(resource).expect("replacement generation");
    assert_eq!(current.fence(), replacement_resource_fence);
    assert_eq!(current.state(), ManagedProcessState::Running);
}

#[test]
fn membership_public_zero_observation_does_not_stop_after_empty_reconciliation() {
    let resource = resource_id(9);
    let job = ScriptedJob::with_root(9_001);
    let mut registry = ProcessRegistry::new();
    let fence = registry
        .register(registration(
            resource,
            1,
            identity(9_001, 90_001, &executable()),
            job.clone(),
        ))
        .expect("registration");
    assert!(receiver_completion(
        &mut registry,
        resource,
        &job,
        fence.clone(),
        JobCompletionEvent::NewProcess { pid: 9_001 },
    ));

    job.set_membership_error("transient Job query failure");
    assert!(receiver_completion(
        &mut registry,
        resource,
        &job,
        fence,
        JobCompletionEvent::ActiveProcessZero,
    ));
    assert_eq!(
        registry.current(resource).expect("current process").state(),
        ManagedProcessState::Starting
    );
    assert!(registry.reconcile_membership(resource).is_err());
    assert_eq!(
        registry.current(resource).expect("current process").state(),
        ManagedProcessState::Starting,
        "an untrusted observation must not create a pending zero retry"
    );

    job.set_snapshot(Vec::new(), Vec::new());
    registry
        .reconcile_membership(resource)
        .expect("retry authoritative empty Job query");
    assert_eq!(
        registry.current(resource).expect("current process").state(),
        ManagedProcessState::Starting,
        "an empty query alone is not receiver-owned ACTIVE_PROCESS_ZERO"
    );
}

#[test]
fn membership_public_zero_observation_does_not_create_retry_state() {
    let resource = resource_id(10);
    let pid = 10_001;
    let root = identity(pid, 100_001, &executable());
    let job = ScriptedJob::with_root(pid);
    let mut registry = ProcessRegistry::new();
    let fence = registry
        .register(registration(resource, 1, root, job.clone()))
        .expect("registration");
    assert!(receiver_completion(
        &mut registry,
        resource,
        &job,
        fence.clone(),
        JobCompletionEvent::NewProcess { pid },
    ));

    job.set_membership_error("transient Job query failure");
    assert!(receiver_completion(
        &mut registry,
        resource,
        &job,
        fence,
        JobCompletionEvent::ActiveProcessZero,
    ));
    assert_eq!(
        registry.current(resource).expect("current process").state(),
        ManagedProcessState::Starting
    );

    let live_member = member(pid, 100_001, "still-running");
    job.set_snapshot(vec![pid], vec![(pid, Ok(live_member.clone()))]);
    registry
        .reconcile_membership(resource)
        .expect("authoritative non-empty Job query");
    let current = registry.current(resource).expect("current process");
    assert_eq!(current.state(), ManagedProcessState::Running);
    assert_eq!(current.known_members(), &[live_member]);

    job.set_snapshot(Vec::new(), Vec::new());
    registry
        .reconcile_membership(resource)
        .expect("ordinary empty reconciliation");
    assert_eq!(
        registry.current(resource).expect("current process").state(),
        ManagedProcessState::Running,
        "ordinary empty reconciliation cannot consume a nonexistent zero proof"
    );
}

#[test]
fn membership_live_states_cannot_be_rolled_back_or_released() {
    fn assert_not_removable(
        registry: &mut ProcessRegistry<ScriptedJob>,
        fence: &ManagedProcessFence,
    ) {
        assert!(registry.rollback_starting_exact(fence).is_err());
        assert!(registry.release_stopped_exact(fence).is_err());
        assert!(registry.current(fence.resource().resource_id).is_some());
    }

    let mut registry = ProcessRegistry::new();

    let running_resource = resource_id(12);
    let running_fence = registry
        .register(registration(
            running_resource,
            1,
            identity(12_001, 120_001, &executable()),
            ScriptedJob::with_root(12_001),
        ))
        .expect("running registration");
    registry
        .commit_resumed_exact(&running_fence)
        .expect("resume commit");
    assert_not_removable(&mut registry, &running_fence);

    let stopping_resource = resource_id(13);
    let stopping_fence = registry
        .register(registration(
            stopping_resource,
            1,
            identity(13_001, 130_001, &executable()),
            ScriptedJob::with_root(13_001),
        ))
        .expect("stopping registration");
    registry
        .commit_resumed_exact(&stopping_fence)
        .expect("resume commit");
    assert!(registry.begin_stopping_exact(&stopping_fence));
    assert_not_removable(&mut registry, &stopping_fence);

    let failed_resource = resource_id(14);
    let failed_job = ScriptedJob::with_root(14_001);
    let failed_fence = registry
        .register(registration(
            failed_resource,
            1,
            identity(14_001, 140_001, &executable()),
            failed_job.clone(),
        ))
        .expect("failed registration");
    registry
        .commit_resumed_exact(&failed_fence)
        .expect("failed-state fixture resume commit");
    assert!(registry.begin_stopping_exact(&failed_fence));
    assert_not_removable(&mut registry, &failed_fence);

    let leaked_resource = resource_id(15);
    let leaked_job = ScriptedJob::with_root(15_001);
    let leaked_fence = registry
        .register(registration(
            leaked_resource,
            1,
            identity(15_001, 150_001, &executable()),
            leaked_job.clone(),
        ))
        .expect("leaked registration");
    registry
        .commit_resumed_exact(&leaked_fence)
        .expect("leaked-state fixture resume commit");
    assert!(registry.begin_stopping_exact(&leaked_fence));
    assert_not_removable(&mut registry, &leaked_fence);
}

#[test]
fn membership_public_completion_observations_do_not_mutate_lifecycle() {
    let executable = executable();
    let mut registry = ProcessRegistry::new();

    let stopping_resource = resource_id(5);
    let stopping_job = ScriptedJob::with_root(5_001);
    let stopping_fence = registry
        .register(registration(
            stopping_resource,
            1,
            identity(5_001, 50_001, &executable),
            stopping_job.clone(),
        ))
        .expect("stopping registration");
    assert_eq!(
        registry
            .current(stopping_resource)
            .expect("starting generation")
            .state(),
        ManagedProcessState::Starting
    );
    assert!(receiver_completion(
        &mut registry,
        stopping_resource,
        &stopping_job,
        stopping_fence.clone(),
        JobCompletionEvent::NewProcess { pid: 5_001 },
    ));
    assert!(registry.begin_stopping_exact(&stopping_fence));
    assert_eq!(
        registry
            .current(stopping_resource)
            .expect("stopping generation")
            .state(),
        ManagedProcessState::Stopping
    );
    stopping_job.set_snapshot(Vec::new(), Vec::new());
    assert!(receiver_completion(
        &mut registry,
        stopping_resource,
        &stopping_job,
        stopping_fence.clone(),
        JobCompletionEvent::ActiveProcessZero,
    ));
    assert_eq!(
        registry
            .current(stopping_resource)
            .expect("stopped generation")
            .state(),
        ManagedProcessState::Stopping
    );

    let failed_resource = resource_id(6);
    let failed_job = ScriptedJob::with_root(6_001);
    let failed_fence = registry
        .register(registration(
            failed_resource,
            1,
            identity(6_001, 60_001, &executable),
            failed_job.clone(),
        ))
        .expect("failed registration");
    assert!(receiver_completion(
        &mut registry,
        failed_resource,
        &failed_job,
        failed_fence,
        JobCompletionEvent::AbnormalExitProcess { pid: 6_001 },
    ));
    assert_eq!(
        registry
            .current(failed_resource)
            .expect("failed generation")
            .state(),
        ManagedProcessState::Starting
    );

    let limited_resource = resource_id(7);
    let limited_job = ScriptedJob::with_root(7_001);
    let limited_fence = registry
        .register(registration(
            limited_resource,
            1,
            identity(7_001, 70_001, &executable),
            limited_job.clone(),
        ))
        .expect("limited registration");
    assert!(receiver_completion(
        &mut registry,
        limited_resource,
        &limited_job,
        limited_fence,
        JobCompletionEvent::Limit {
            message_id: 10,
            pid: Some(7_001),
        },
    ));
    let limited = registry
        .current(limited_resource)
        .expect("limited generation");
    assert_eq!(limited.state(), ManagedProcessState::Starting);
    assert_eq!(limited.last_limit(), None);

    let leaked_resource = resource_id(8);
    let leaked_job = ScriptedJob::with_root(8_001);
    let leaked_fence = registry
        .register(registration(
            leaked_resource,
            1,
            identity(8_001, 80_001, &executable),
            leaked_job.clone(),
        ))
        .expect("leaked registration");
    assert!(receiver_completion(
        &mut registry,
        leaked_resource,
        &leaked_job,
        leaked_fence,
        JobCompletionEvent::MonitorFailed {
            detail: "completion port abandoned".to_string(),
        },
    ));
    assert_eq!(
        registry
            .current(leaked_resource)
            .expect("leaked generation")
            .state(),
        ManagedProcessState::Starting
    );
}

#[cfg(windows)]
#[test]
fn membership_windows_job_emits_fenced_new_process_and_active_zero() {
    use std::process::{Child, Command};
    use std::time::{Duration, Instant};

    use devmanager::process::job::ManagedProcessJob;
    use devmanager::services::platform_service::{
        claim_suspended_process, MANAGED_PROCESS_CREATION_FLAGS,
    };
    use std::os::windows::process::CommandExt;

    struct TestChild(Child);

    impl Drop for TestChild {
        fn drop(&mut self) {
            if matches!(self.0.try_wait(), Ok(Some(_))) {
                return;
            }
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let harness = tempfile::tempdir().expect("completion-port harness");
    let marker = harness.path().join("running.marker");
    let helper = std::env::var_os("CARGO_BIN_EXE_devmanager-process-test-helper")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_exe()
                .expect("current test executable")
                .parent()
                .and_then(Path::parent)
                .expect("test target directory")
                .join("devmanager-process-test-helper.exe")
        });
    let mut child = TestChild(
        Command::new(helper)
            .arg("mark-wait")
            .arg(&marker)
            .creation_flags(MANAGED_PROCESS_CREATION_FLAGS)
            .spawn()
            .expect("spawn suspended completion-port child"),
    );
    let pid = child.0.id();
    let job = claim_suspended_process(pid)
        .expect("claim suspended child")
        .expect("Windows managed Job");
    let root = job
        .inspect_process(pid)
        .expect("inspect exact Job root")
        .identity()
        .clone();
    assert!(
        job.inspect_process(std::process::id()).is_err(),
        "a PID outside this Job must remain inaccessible"
    );

    let resource = resource_id(11);
    let mut registry: ProcessRegistry<ManagedProcessJob> = ProcessRegistry::new();
    let fence = registry
        .register(RegisteredProcess::new(
            ResourceFence::new(resource, 1),
            ProcessOwner::Host,
            root,
            ProcessDisplayLabel::new("real completion port").expect("display label"),
            job,
        ))
        .expect("register real Job");

    let event_deadline = Instant::now() + Duration::from_secs(3);
    let mut saw_new_process = false;
    while !saw_new_process && Instant::now() < event_deadline {
        let messages = registry
            .current(resource)
            .expect("current Job")
            .job()
            .drain_completion_messages();
        for message in messages {
            assert_eq!(message.fence(), &fence);
            saw_new_process |= matches!(
                message.event(),
                JobCompletionEvent::NewProcess { pid: observed } if *observed == pid
            );
            assert!(registry.apply_job_completion(message));
        }
        if !saw_new_process {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    assert!(saw_new_process, "real Job did not emit NEW_PROCESS");

    child.0.kill().expect("terminate owned test child");
    child.0.wait().expect("wait for owned test child");
    let zero_deadline = Instant::now() + Duration::from_secs(3);
    let mut saw_active_zero = false;
    while !saw_active_zero && Instant::now() < zero_deadline {
        let messages = registry
            .current(resource)
            .expect("current Job")
            .job()
            .drain_completion_messages();
        for message in messages {
            assert_eq!(message.fence(), &fence);
            saw_active_zero |= matches!(message.event(), JobCompletionEvent::ActiveProcessZero);
            assert!(registry.apply_job_completion(message));
        }
        if !saw_active_zero {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    assert!(saw_active_zero, "real Job did not emit ACTIVE_PROCESS_ZERO");
    assert_eq!(
        registry.current(resource).expect("current Job").state(),
        ManagedProcessState::Running,
        "a completion hint must not publish stopped while the Job is retained"
    );

    assert!(
        registry.release_stopped_exact(&fence).is_err(),
        "a pending completion proof must not release before dedicated settlement"
    );

    let ticket = TeardownTicket::new(
        OperationId::from_bytes(fixed_uuid_v7(12)).expect("operation id"),
        TeardownScope::Host,
        1,
        fence.clone(),
    )
    .expect("host teardown ticket");
    let proof = registry
        .active_process_zero_proof_exact(&fence)
        .expect("receiver-owned zero proof");
    let authority = registry
        .mint_teardown_release_authority_exact(&ticket, proof)
        .expect("authoritative empty membership settlement");
    assert_eq!(
        registry.current(resource).expect("retained Job").state(),
        ManagedProcessState::ZeroSettled,
        "authoritative zero still retains exact Job release authority"
    );

    let removed = registry
        .release_stopped_with_authority(&ticket, &authority)
        .expect("release completed Job");
    assert!(matches!(&removed, UnregisterOutcome::Removed(_)));
    drop(removed);
}
