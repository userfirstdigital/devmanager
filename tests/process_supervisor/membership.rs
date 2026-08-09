//! Deterministic Job membership and completion-notification acceptance tests.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use devmanager::domain::id::{ResourceId, TaskId};
use devmanager::domain::operation::ResourceFence;
use devmanager::process::identity::{ManagedProcessId, ManagedProcessIdentity, ProcessOwner};
use devmanager::process::registry::{
    JobCompletionEvent, JobCompletionMessage, JobMemberInfo, JobMembership, ManagedProcessFence,
    ManagedProcessState, ProcessDisplayLabel, ProcessRegistry, RegisteredProcess,
    UnregisterOutcome,
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
    completions: VecDeque<JobCompletionMessage>,
    bound_fence: Option<ManagedProcessFence>,
}

impl Default for ScriptedJobState {
    fn default() -> Self {
        Self {
            active_pids: Ok(Vec::new()),
            observations: BTreeMap::new(),
            completions: VecDeque::new(),
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

    fn push(&self, message: JobCompletionMessage) {
        self.0
            .lock()
            .expect("scripted Job")
            .completions
            .push_back(message);
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

    fn drain_completion_messages(&self) -> Vec<JobCompletionMessage> {
        self.0
            .lock()
            .expect("scripted Job")
            .completions
            .drain(..)
            .collect()
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
    RegisteredProcess::new(
        ResourceFence::new(resource, generation),
        ProcessOwner::Host,
        root,
        ProcessDisplayLabel::new("membership test").expect("display label"),
        job,
    )
}

fn completion(fence: ManagedProcessFence, event: JobCompletionEvent) -> JobCompletionMessage {
    JobCompletionMessage::new(fence, event)
}

fn member(pid: u32, creation_time_100ns: u64, command_line: &str) -> JobMemberInfo {
    JobMemberInfo::new(
        identity(pid, creation_time_100ns, &executable()),
        Some(command_line.to_string()),
    )
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
    job.push(completion(
        fence.clone(),
        JobCompletionEvent::NewProcess { pid: 1_003 },
    ));
    job.push(completion(
        fence.clone(),
        JobCompletionEvent::ExitProcess { pid: 1_001 },
    ));

    assert_eq!(registry.drain_job_completions(resource), 2);
    registry
        .reconcile_membership(resource)
        .expect("authoritative Job snapshot");
    let current = registry.current(resource).expect("current generation");
    assert_eq!(current.state(), ManagedProcessState::Running);
    assert_eq!(current.known_members(), &[grandchild]);
    assert!(current.unknown_member_pids().is_empty());

    job.push(completion(
        fence.clone(),
        JobCompletionEvent::ActiveProcessZero,
    ));
    assert_eq!(registry.drain_job_completions(resource), 1);
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

    job.push(completion(fence, JobCompletionEvent::ActiveProcessZero));
    assert_eq!(registry.drain_job_completions(resource), 1);
    assert_eq!(
        registry
            .current(resource)
            .expect("current generation")
            .state(),
        ManagedProcessState::Stopped
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
    replacement_job.push(completion(
        replacement_fence.clone(),
        JobCompletionEvent::NewProcess { pid: 4_002 },
    ));
    assert_eq!(registry.drain_job_completions(resource), 1);

    let wrong_owner = ManagedProcessFence::new(
        replacement_fence.resource(),
        ProcessOwner::Task(TaskId::new()),
        replacement_fence.root().clone(),
    );
    assert!(!registry.apply_job_completion(completion(
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
    assert!(!registry.apply_job_completion(completion(
        wrong_root,
        JobCompletionEvent::AbnormalExitProcess { pid: 4_002 },
    )));

    assert!(!registry.apply_job_completion(completion(
        stale_fence.clone(),
        JobCompletionEvent::ExitProcess { pid: 4_001 },
    )));
    assert!(!registry.apply_job_completion(completion(
        stale_fence.clone(),
        JobCompletionEvent::AbnormalExitProcess { pid: 4_001 },
    )));
    assert!(!registry.apply_job_completion(completion(
        stale_fence,
        JobCompletionEvent::ActiveProcessZero,
    )));

    let current = registry.current(resource).expect("replacement generation");
    assert_eq!(current.fence(), replacement_resource_fence);
    assert_eq!(current.state(), ManagedProcessState::Running);
}

#[test]
fn membership_unproved_zero_retries_until_empty_then_stops() {
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
    assert!(registry.apply_job_completion(completion(
        fence.clone(),
        JobCompletionEvent::NewProcess { pid: 9_001 },
    )));

    job.set_membership_error("transient Job query failure");
    assert!(
        registry.apply_job_completion(completion(fence, JobCompletionEvent::ActiveProcessZero,))
    );
    assert_eq!(
        registry.current(resource).expect("current process").state(),
        ManagedProcessState::Leaked
    );
    assert!(registry.reconcile_membership(resource).is_err());
    assert_eq!(
        registry.current(resource).expect("current process").state(),
        ManagedProcessState::Leaked,
        "a repeated query failure must retain the pending zero retry"
    );

    job.set_snapshot(Vec::new(), Vec::new());
    registry
        .reconcile_membership(resource)
        .expect("retry authoritative empty Job query");
    assert_eq!(
        registry.current(resource).expect("current process").state(),
        ManagedProcessState::Stopped
    );
}

#[test]
fn membership_unproved_zero_restores_prior_state_when_members_remain() {
    let resource = resource_id(10);
    let pid = 10_001;
    let root = identity(pid, 100_001, &executable());
    let job = ScriptedJob::with_root(pid);
    let mut registry = ProcessRegistry::new();
    let fence = registry
        .register(registration(resource, 1, root, job.clone()))
        .expect("registration");
    assert!(
        registry.apply_job_completion(completion(fence, JobCompletionEvent::NewProcess { pid },))
    );

    job.set_membership_error("transient Job query failure");
    let bound = job
        .0
        .lock()
        .expect("scripted Job")
        .bound_fence
        .clone()
        .expect("full completion authority bound at registration");
    assert!(
        registry.apply_job_completion(completion(bound, JobCompletionEvent::ActiveProcessZero,))
    );
    assert_eq!(
        registry.current(resource).expect("current process").state(),
        ManagedProcessState::Leaked
    );

    let live_member = member(pid, 100_001, "still-running");
    job.set_snapshot(vec![pid], vec![(pid, Ok(live_member.clone()))]);
    registry
        .reconcile_membership(resource)
        .expect("retry authoritative non-empty Job query");
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
        "a resolved non-empty retry consumes the pending zero marker"
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
    let failed_fence = registry
        .register(registration(
            failed_resource,
            1,
            identity(14_001, 140_001, &executable()),
            ScriptedJob::with_root(14_001),
        ))
        .expect("failed registration");
    assert!(registry.apply_job_completion(completion(
        failed_fence.clone(),
        JobCompletionEvent::AbnormalExitProcess { pid: 14_001 },
    )));
    assert_not_removable(&mut registry, &failed_fence);

    let leaked_resource = resource_id(15);
    let leaked_fence = registry
        .register(registration(
            leaked_resource,
            1,
            identity(15_001, 150_001, &executable()),
            ScriptedJob::with_root(15_001),
        ))
        .expect("leaked registration");
    assert!(registry.apply_job_completion(completion(
        leaked_fence.clone(),
        JobCompletionEvent::MonitorFailed {
            detail: "listener lost".to_string(),
        },
    )));
    assert_not_removable(&mut registry, &leaked_fence);
}

#[test]
fn membership_states_cover_stop_failure_limit_and_listener_loss() {
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
    assert!(registry.apply_job_completion(completion(
        stopping_fence.clone(),
        JobCompletionEvent::NewProcess { pid: 5_001 },
    )));
    assert!(registry.begin_stopping_exact(&stopping_fence));
    assert_eq!(
        registry
            .current(stopping_resource)
            .expect("stopping generation")
            .state(),
        ManagedProcessState::Stopping
    );
    stopping_job.set_snapshot(Vec::new(), Vec::new());
    assert!(registry.apply_job_completion(completion(
        stopping_fence.clone(),
        JobCompletionEvent::ActiveProcessZero,
    )));
    assert_eq!(
        registry
            .current(stopping_resource)
            .expect("stopped generation")
            .state(),
        ManagedProcessState::Stopped
    );

    let failed_resource = resource_id(6);
    let failed_job = ScriptedJob::with_root(6_001);
    let failed_fence = registry
        .register(registration(
            failed_resource,
            1,
            identity(6_001, 60_001, &executable),
            failed_job,
        ))
        .expect("failed registration");
    assert!(registry.apply_job_completion(completion(
        failed_fence,
        JobCompletionEvent::AbnormalExitProcess { pid: 6_001 },
    )));
    assert_eq!(
        registry
            .current(failed_resource)
            .expect("failed generation")
            .state(),
        ManagedProcessState::Failed
    );

    let limited_resource = resource_id(7);
    let limited_job = ScriptedJob::with_root(7_001);
    let limited_fence = registry
        .register(registration(
            limited_resource,
            1,
            identity(7_001, 70_001, &executable),
            limited_job,
        ))
        .expect("limited registration");
    assert!(registry.apply_job_completion(completion(
        limited_fence,
        JobCompletionEvent::Limit {
            message_id: 10,
            pid: Some(7_001),
        },
    )));
    let limited = registry
        .current(limited_resource)
        .expect("limited generation");
    assert_eq!(limited.state(), ManagedProcessState::Failed);
    assert_eq!(limited.last_limit(), Some((10, Some(7_001))));

    let leaked_resource = resource_id(8);
    let leaked_job = ScriptedJob::with_root(8_001);
    let leaked_fence = registry
        .register(registration(
            leaked_resource,
            1,
            identity(8_001, 80_001, &executable),
            leaked_job,
        ))
        .expect("leaked registration");
    assert!(registry.apply_job_completion(completion(
        leaked_fence,
        JobCompletionEvent::MonitorFailed {
            detail: "completion port abandoned".to_string(),
        },
    )));
    assert_eq!(
        registry
            .current(leaked_resource)
            .expect("leaked generation")
            .state(),
        ManagedProcessState::Leaked
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
    let mut child = TestChild(
        Command::new(env!("CARGO_BIN_EXE_devmanager-process-test-helper"))
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
        ManagedProcessState::Stopped
    );

    let removed = registry
        .release_stopped_exact(&fence)
        .expect("release completed Job");
    assert!(matches!(&removed, UnregisterOutcome::Removed(_)));
    drop(removed);
}
