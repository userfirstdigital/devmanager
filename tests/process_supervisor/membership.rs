//! Deterministic Job membership and completion-notification acceptance tests.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use devmanager::domain::id::ResourceId;
use devmanager::domain::operation::ResourceFence;
use devmanager::process::identity::{ManagedProcessId, ManagedProcessIdentity, ProcessOwner};
use devmanager::process::registry::{
    JobCompletionEvent, JobCompletionMessage, JobMemberInfo, JobMembership, ManagedProcessState,
    ProcessDisplayLabel, ProcessRegistry, RegisteredProcess, UnregisterOutcome,
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

#[derive(Debug, Default)]
struct ScriptedJobState {
    active_pids: Vec<u32>,
    observations: BTreeMap<u32, Result<JobMemberInfo, String>>,
    completions: VecDeque<JobCompletionMessage>,
}

#[derive(Debug, Clone, Default)]
struct ScriptedJob(Arc<Mutex<ScriptedJobState>>);

impl ScriptedJob {
    fn with_root(pid: u32) -> Self {
        let job = Self::default();
        job.0.lock().expect("scripted Job").active_pids = vec![pid];
        job
    }

    fn set_snapshot(
        &self,
        pids: Vec<u32>,
        observations: Vec<(u32, Result<JobMemberInfo, String>)>,
    ) {
        let mut state = self.0.lock().expect("scripted Job");
        state.active_pids = pids;
        state.observations = observations.into_iter().collect();
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
        Ok(self.0.lock().expect("scripted Job").active_pids.clone())
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

fn completion(fence: ResourceFence, event: JobCompletionEvent) -> JobCompletionMessage {
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
    let fence = ResourceFence::new(resource, 1);
    let root = identity(1_001, 10_001, &executable());
    let grandchild = member(1_003, 10_003, "grandchild --work");
    let job = ScriptedJob::with_root(root.id().pid());
    let mut registry = ProcessRegistry::new();
    registry
        .register(registration(resource, 1, root, job.clone()))
        .expect("registration");

    job.set_snapshot(
        vec![grandchild.identity().id().pid()],
        vec![(grandchild.identity().id().pid(), Ok(grandchild.clone()))],
    );
    job.push(completion(
        fence,
        JobCompletionEvent::NewProcess { pid: 1_003 },
    ));
    job.push(completion(
        fence,
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

    job.push(completion(fence, JobCompletionEvent::ActiveProcessZero));
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
    let stale_resource_fence = ResourceFence::new(resource, 1);
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
        .unregister_exact(&stale_fence)
        .expect("retire stale generation");
    assert!(matches!(removed, UnregisterOutcome::Removed(_)));

    registry
        .register(registration(
            resource,
            2,
            identity(4_002, 40_002, &executable()),
            replacement_job.clone(),
        ))
        .expect("replacement registration");
    replacement_job.push(completion(
        replacement_resource_fence,
        JobCompletionEvent::NewProcess { pid: 4_002 },
    ));
    assert_eq!(registry.drain_job_completions(resource), 1);

    assert!(!registry.apply_job_completion(completion(
        stale_resource_fence,
        JobCompletionEvent::ExitProcess { pid: 4_001 },
    )));
    assert!(!registry.apply_job_completion(completion(
        stale_resource_fence,
        JobCompletionEvent::AbnormalExitProcess { pid: 4_001 },
    )));
    assert!(!registry.apply_job_completion(completion(
        stale_resource_fence,
        JobCompletionEvent::ActiveProcessZero,
    )));

    let current = registry.current(resource).expect("replacement generation");
    assert_eq!(current.fence(), replacement_resource_fence);
    assert_eq!(current.state(), ManagedProcessState::Running);
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
        stopping_fence.resource(),
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
        stopping_fence.resource(),
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
        failed_fence.resource(),
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
        limited_fence.resource(),
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
        leaked_fence.resource(),
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
