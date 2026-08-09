//! Deterministic teardown-coordinator acceptance tests.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use devmanager::domain::id::{OperationId, ResourceId, TaskId};
use devmanager::domain::operation::ResourceFence;
use devmanager::process::identity::{ManagedProcessId, ManagedProcessIdentity, ProcessOwner};
use devmanager::process::registry::{
    ActiveProcessZeroProof, JobCompletionEvent, JobCompletionMessage, JobMembership,
    ManagedProcessFence, ManagedProcessState, ProcessDisplayLabel, ProcessRegistry,
    RegisteredProcess,
};
use devmanager::process::teardown::{
    AdmissionReceipt, AdmissionState, BoxFuture, ResidueEvidence, StageResult, TeardownAdmission,
    TeardownAdmissionError, TeardownClock, TeardownCompletionKey, TeardownCompletionStore,
    TeardownCoordinator, TeardownDeadline, TeardownEffects, TeardownOutcome, TeardownReport,
    TeardownScope, TeardownStage, TeardownTicket, WaitResult, WaitStage,
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

fn operation_id(tail: u8) -> OperationId {
    OperationId::from_bytes(fixed_uuid_v7(tail)).expect("operation id")
}

fn executable() -> PathBuf {
    std::env::current_exe().expect("current test executable")
}

fn identity(pid: u32, creation_time_100ns: u64) -> ManagedProcessIdentity {
    ManagedProcessIdentity::new(
        ManagedProcessId::new(pid, creation_time_100ns).expect("managed process id"),
        executable(),
    )
    .expect("managed process identity")
}

fn ticket(
    resource_tail: u8,
    operation_tail: u8,
    scope: TeardownScope,
    action_epoch: u64,
    pid: u32,
    creation_time_100ns: u64,
) -> TeardownTicket {
    let resource = resource_id(resource_tail);
    let fence = ManagedProcessFence::new(
        ResourceFence::new(resource, 1),
        match scope {
            TeardownScope::Task(task_id) => ProcessOwner::Task(task_id),
            TeardownScope::Host => ProcessOwner::Host,
        },
        identity(pid, creation_time_100ns),
    );
    TeardownTicket::new(operation_id(operation_tail), scope, action_epoch, fence)
        .expect("ticket scope matches process owner")
}

#[derive(Debug, Clone)]
struct AdmissionRule {
    fence: ManagedProcessFence,
    action_epoch: u64,
    state: AdmissionState,
    return_non_closing: bool,
}

#[derive(Debug, Clone, Default)]
struct ExactJob {
    active_process_ids: Arc<Mutex<Vec<u32>>>,
}

impl ExactJob {
    fn with_root(pid: u32) -> Self {
        Self {
            active_process_ids: Arc::new(Mutex::new(vec![pid])),
        }
    }

    fn set_active_process_ids(&self, process_ids: Vec<u32>) {
        *self
            .active_process_ids
            .lock()
            .expect("exact Job membership") = process_ids;
    }
}

impl JobMembership for ExactJob {
    fn active_process_ids(&self) -> Result<Vec<u32>, String> {
        Ok(self
            .active_process_ids
            .lock()
            .expect("exact Job membership")
            .clone())
    }
}

#[derive(Debug, Default, Clone)]
struct FakeAdmission {
    rules: Arc<Mutex<BTreeMap<ResourceId, AdmissionRule>>>,
    events: Arc<Mutex<Vec<String>>>,
}

impl FakeAdmission {
    fn allow(&self, ticket: &TeardownTicket) {
        self.rules.lock().expect("admission rules").insert(
            ticket.resource_id(),
            AdmissionRule {
                fence: ticket.fence().clone(),
                action_epoch: ticket.action_epoch(),
                state: AdmissionState::Open,
                return_non_closing: false,
            },
        );
    }

    fn allow_non_closing(&self, ticket: &TeardownTicket) {
        self.rules.lock().expect("admission rules").insert(
            ticket.resource_id(),
            AdmissionRule {
                fence: ticket.fence().clone(),
                action_epoch: ticket.action_epoch(),
                state: AdmissionState::Open,
                return_non_closing: true,
            },
        );
    }

    fn events(&self) -> Vec<String> {
        self.events.lock().expect("admission events").clone()
    }

    fn launch_or_input_is_rejected(&self, resource: ResourceId) -> bool {
        self.rules
            .lock()
            .expect("admission rules")
            .get(&resource)
            .is_some_and(|rule| rule.state == AdmissionState::Closing)
    }
}

impl TeardownAdmission for FakeAdmission {
    fn close_admission(
        &self,
        ticket: &TeardownTicket,
    ) -> Result<AdmissionReceipt, TeardownAdmissionError> {
        self.events
            .lock()
            .expect("admission events")
            .push(format!("admission:{}", ticket.resource_id()));

        let mut rules = self.rules.lock().expect("admission rules");
        let rule = rules
            .get_mut(&ticket.resource_id())
            .ok_or(TeardownAdmissionError::FenceMismatch)?;
        if rule.action_epoch != ticket.action_epoch() {
            return Err(TeardownAdmissionError::StaleEpoch {
                expected: rule.action_epoch,
                actual: ticket.action_epoch(),
            });
        }
        if rule.fence != *ticket.fence() {
            return Err(TeardownAdmissionError::FenceMismatch);
        }
        if rule.return_non_closing {
            return Ok(AdmissionReceipt::new(
                ticket.scope(),
                AdmissionState::Open,
                ticket.action_epoch(),
                ticket.fence().clone(),
            ));
        }
        rule.state = AdmissionState::Closing;
        Ok(AdmissionReceipt::new(
            ticket.scope(),
            rule.state,
            rule.action_epoch,
            rule.fence.clone(),
        ))
    }
}

#[derive(Debug, Default, Clone)]
struct FakeCompletionStore {
    reports: Arc<Mutex<Vec<(TeardownCompletionKey, TeardownReport)>>>,
}

impl TeardownCompletionStore for FakeCompletionStore {
    fn lookup(&self, key: &TeardownCompletionKey) -> Result<Option<TeardownReport>, String> {
        Ok(self
            .reports
            .lock()
            .expect("completion reports")
            .iter()
            .find(|(stored_key, _)| stored_key == key)
            .map(|(_, report)| report.clone()))
    }

    fn persist(&self, key: &TeardownCompletionKey, report: &TeardownReport) -> Result<(), String> {
        let mut reports = self.reports.lock().expect("completion reports");
        if let Some((_, stored_report)) =
            reports.iter_mut().find(|(stored_key, _)| stored_key == key)
        {
            *stored_report = report.clone();
        } else {
            reports.push((key.clone(), report.clone()));
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct FakeClock {
    next: Arc<AtomicUsize>,
    deadlines: Arc<Mutex<Vec<TeardownDeadline>>>,
}

impl Default for FakeClock {
    fn default() -> Self {
        Self {
            next: Arc::new(AtomicUsize::new(100)),
            deadlines: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl FakeClock {
    fn deadlines(&self) -> Vec<TeardownDeadline> {
        self.deadlines.lock().expect("clock deadlines").clone()
    }
}

impl TeardownClock for FakeClock {
    fn deadline(&self, timeout: Duration) -> TeardownDeadline {
        let deadline = self
            .next
            .fetch_add(timeout.as_millis() as usize, Ordering::SeqCst)
            + timeout.as_millis() as usize;
        let deadline = TeardownDeadline::new(deadline as u64);
        self.deadlines
            .lock()
            .expect("clock deadlines")
            .push(deadline);
        deadline
    }
}

#[derive(Debug, Clone)]
enum WaitPlan {
    TimedOut,
    Zero { proof: ActiveProcessZeroProof },
}

#[derive(Debug, Clone)]
struct BranchScript {
    cooperative: WaitPlan,
    interrupt: WaitPlan,
    termination: WaitPlan,
    residue: Option<ResidueEvidence>,
    detach_after_zero: StageResult,
    reconcile_ports: StageResult,
    persist_settlement: StageResult,
    release_stopped_exact: StageResult,
}

impl BranchScript {
    fn cooperative_zero(ticket: &TeardownTicket) -> Self {
        Self {
            cooperative: WaitPlan::Zero {
                proof: ActiveProcessZeroProof::for_test(ticket.fence().clone()),
            },
            interrupt: WaitPlan::TimedOut,
            termination: WaitPlan::TimedOut,
            residue: None,
            detach_after_zero: StageResult::Completed,
            reconcile_ports: StageResult::Completed,
            persist_settlement: StageResult::Completed,
            release_stopped_exact: StageResult::Completed,
        }
    }

    fn escalation_to_zero(ticket: &TeardownTicket) -> Self {
        Self {
            cooperative: WaitPlan::TimedOut,
            interrupt: WaitPlan::TimedOut,
            termination: WaitPlan::Zero {
                proof: ActiveProcessZeroProof::for_test(ticket.fence().clone()),
            },
            residue: None,
            detach_after_zero: StageResult::Completed,
            reconcile_ports: StageResult::Completed,
            persist_settlement: StageResult::Completed,
            release_stopped_exact: StageResult::Completed,
        }
    }

    fn timeout_with_residue() -> Self {
        Self {
            cooperative: WaitPlan::TimedOut,
            interrupt: WaitPlan::TimedOut,
            termination: WaitPlan::TimedOut,
            residue: Some(ResidueEvidence::new(
                "job\r\nname",
                44,
                4_400,
                "C:\\Users\\alice\\provider.exe\n",
                "provider --token=secret-value\r\nchild",
                "ACTIVE_PROCESS_ZERO\nnot-authoritative",
                vec![TeardownStage::Drain, TeardownStage::TerminateTree],
            )),
            detach_after_zero: StageResult::Completed,
            reconcile_ports: StageResult::Completed,
            persist_settlement: StageResult::Completed,
            release_stopped_exact: StageResult::Completed,
        }
    }
}

#[derive(Debug, Default, Clone)]
struct FakeEffectsState {
    scripts: BTreeMap<ResourceId, BranchScript>,
    events: Vec<String>,
    release_count: BTreeMap<ResourceId, usize>,
    terminate_count: BTreeMap<ResourceId, usize>,
    active: usize,
    max_active: usize,
}

#[derive(Debug, Default, Clone)]
struct FakeEffects {
    state: Arc<Mutex<FakeEffectsState>>,
}

// This fake intentionally covers only the pure core seam. The production
// TerminalService/host adapter remains the Task 3.4 integration boundary.
impl FakeEffects {
    fn install(&self, ticket: &TeardownTicket, script: BranchScript) {
        self.state
            .lock()
            .expect("effects state")
            .scripts
            .insert(ticket.resource_id(), script);
    }

    fn events(&self) -> Vec<String> {
        self.state.lock().expect("effects state").events.clone()
    }

    fn release_count(&self, resource: ResourceId) -> usize {
        self.state
            .lock()
            .expect("effects state")
            .release_count
            .get(&resource)
            .copied()
            .unwrap_or_default()
    }

    fn terminate_count(&self, resource: ResourceId) -> usize {
        self.state
            .lock()
            .expect("effects state")
            .terminate_count
            .get(&resource)
            .copied()
            .unwrap_or_default()
    }

    fn max_active(&self) -> usize {
        self.state.lock().expect("effects state").max_active
    }

    fn record(&self, ticket: &TeardownTicket, event: impl Into<String>) {
        self.state
            .lock()
            .expect("effects state")
            .events
            .push(format!("{}:{}", ticket.resource_id(), event.into()));
    }

    fn script(&self, resource: ResourceId) -> BranchScript {
        self.state
            .lock()
            .expect("effects state")
            .scripts
            .get(&resource)
            .cloned()
            .expect("branch script")
    }
}

impl TeardownEffects for FakeEffects {
    fn drain<'a>(&'a self, ticket: &'a TeardownTicket) -> BoxFuture<'a, StageResult> {
        let effects = self.clone();
        let ticket = ticket.clone();
        Box::pin(async move {
            {
                let mut state = effects.state.lock().expect("effects state");
                state.active += 1;
                state.max_active = state.max_active.max(state.active);
            }
            tokio::task::yield_now().await;
            {
                let mut state = effects.state.lock().expect("effects state");
                state.active -= 1;
            }
            effects.record(&ticket, "drain");
            StageResult::Completed
        })
    }

    fn cooperative_close<'a>(&'a self, ticket: &'a TeardownTicket) -> BoxFuture<'a, StageResult> {
        let effects = self.clone();
        let ticket = ticket.clone();
        Box::pin(async move {
            effects.record(&ticket, "cooperative_close");
            StageResult::Completed
        })
    }

    fn interrupt_or_safe_close<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
    ) -> BoxFuture<'a, StageResult> {
        let effects = self.clone();
        let ticket = ticket.clone();
        Box::pin(async move {
            effects.record(&ticket, "interrupt_or_safe_close");
            StageResult::Completed
        })
    }

    fn terminate_tree<'a>(&'a self, ticket: &'a TeardownTicket) -> BoxFuture<'a, StageResult> {
        let effects = self.clone();
        let ticket = ticket.clone();
        Box::pin(async move {
            effects.record(&ticket, "terminate_tree");
            let mut state = effects.state.lock().expect("effects state");
            *state
                .terminate_count
                .entry(ticket.resource_id())
                .or_default() += 1;
            StageResult::Completed
        })
    }

    fn wait_for_zero<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
        stage: WaitStage,
        _deadline: TeardownDeadline,
    ) -> BoxFuture<'a, WaitResult> {
        let effects = self.clone();
        let ticket = ticket.clone();
        Box::pin(async move {
            effects.record(&ticket, format!("wait:{stage:?}"));
            let script = effects.script(ticket.resource_id());
            let plan = match stage {
                WaitStage::CooperativeGrace => script.cooperative,
                WaitStage::InterruptGrace => script.interrupt,
                WaitStage::Termination => script.termination,
            };
            match plan {
                WaitPlan::TimedOut => WaitResult::TimedOut,
                WaitPlan::Zero { proof } => WaitResult::Zero { proof },
            }
        })
    }

    fn settle_active_process_zero<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
        proof: ActiveProcessZeroProof,
    ) -> BoxFuture<'a, StageResult> {
        let effects = self.clone();
        let ticket = ticket.clone();
        Box::pin(async move {
            effects.record(&ticket, "settle_active_process_zero");
            if proof.fence() == ticket.fence() {
                StageResult::Completed
            } else {
                StageResult::Failed {
                    detail: "registry zero proof fence mismatch".to_string(),
                }
            }
        })
    }

    fn detach_after_zero<'a>(&'a self, ticket: &'a TeardownTicket) -> BoxFuture<'a, StageResult> {
        let effects = self.clone();
        let ticket = ticket.clone();
        Box::pin(async move {
            effects.record(&ticket, "detach_after_zero");
            effects.script(ticket.resource_id()).detach_after_zero
        })
    }

    fn reconcile_ports<'a>(&'a self, ticket: &'a TeardownTicket) -> BoxFuture<'a, StageResult> {
        let effects = self.clone();
        let ticket = ticket.clone();
        Box::pin(async move {
            effects.record(&ticket, "reconcile_ports");
            effects.script(ticket.resource_id()).reconcile_ports
        })
    }

    fn persist_settlement<'a>(&'a self, ticket: &'a TeardownTicket) -> BoxFuture<'a, StageResult> {
        let effects = self.clone();
        let ticket = ticket.clone();
        Box::pin(async move {
            effects.record(&ticket, "persist_settlement");
            effects.script(ticket.resource_id()).persist_settlement
        })
    }

    fn residue<'a>(&'a self, ticket: &'a TeardownTicket) -> BoxFuture<'a, Option<ResidueEvidence>> {
        let effects = self.clone();
        let ticket = ticket.clone();
        Box::pin(async move { effects.script(ticket.resource_id()).residue })
    }

    fn release_stopped_exact<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
    ) -> BoxFuture<'a, StageResult> {
        let effects = self.clone();
        let ticket = ticket.clone();
        Box::pin(async move {
            effects.record(&ticket, "release_stopped_exact");
            let result = effects.script(ticket.resource_id()).release_stopped_exact;
            let mut state = effects.state.lock().expect("effects state");
            *state.release_count.entry(ticket.resource_id()).or_default() += 1;
            result
        })
    }
}

fn coordinator(
    admission: &FakeAdmission,
    effects: &FakeEffects,
    clock: &FakeClock,
) -> TeardownCoordinator {
    TeardownCoordinator::new(
        Arc::new(admission.clone()),
        Arc::new(effects.clone()),
        Arc::new(clock.clone()),
    )
}

fn coordinator_with_limit(
    admission: &FakeAdmission,
    effects: &FakeEffects,
    clock: &FakeClock,
    configured_capacity: usize,
) -> TeardownCoordinator {
    TeardownCoordinator::with_capacity(
        Arc::new(admission.clone()),
        Arc::new(effects.clone()),
        Arc::new(clock.clone()),
        configured_capacity,
    )
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
}

#[test]
fn teardown_admission_closes_before_effects_and_rejects_launch_input() {
    let task = TaskId::new();
    let ticket = ticket(1, 11, TeardownScope::Task(task), 7, 101, 1_001);
    let admission = FakeAdmission::default();
    admission.allow(&ticket);
    let effects = FakeEffects::default();
    effects.install(&ticket, BranchScript::cooperative_zero(&ticket));
    let clock = FakeClock::default();
    let coordinator = coordinator(&admission, &effects, &clock);

    let report = runtime().block_on(
        coordinator
            .request(ticket.clone())
            .expect("admission winner")
            .wait(),
    );

    assert_eq!(report.outcome(), TeardownOutcome::Closed);
    assert!(admission.launch_or_input_is_rejected(ticket.resource_id()));
    assert_eq!(admission.events().len(), 1);
    let events = effects.events();
    assert!(events
        .first()
        .is_some_and(|event| event.ends_with(":drain")));
}

#[test]
fn teardown_cooperative_exit_requires_matching_zero() {
    let ticket = ticket(2, 12, TeardownScope::Host, 8, 102, 1_002);
    let admission = FakeAdmission::default();
    admission.allow(&ticket);
    let effects = FakeEffects::default();
    let mut script = BranchScript::cooperative_zero(&ticket);
    let mismatched_fence = ManagedProcessFence::new(
        ticket.fence().resource(),
        ticket.fence().owner(),
        identity(ticket.fence().root().id().pid(), 99_999),
    );
    script.cooperative = WaitPlan::Zero {
        proof: ActiveProcessZeroProof::for_test(mismatched_fence),
    };
    script.termination = WaitPlan::Zero {
        proof: ActiveProcessZeroProof::for_test(ticket.fence().clone()),
    };
    effects.install(&ticket, script);
    let clock = FakeClock::default();
    let report = runtime().block_on(
        coordinator(&admission, &effects, &clock)
            .request(ticket.clone())
            .expect("admission winner")
            .wait(),
    );

    assert_eq!(report.outcome(), TeardownOutcome::Closed);
    assert_eq!(effects.terminate_count(ticket.resource_id()), 1);
    assert_eq!(effects.release_count(ticket.resource_id()), 1);
}

#[test]
fn teardown_root_exit_with_live_child_escalates_to_job() {
    let ticket = ticket(3, 13, TeardownScope::Host, 9, 103, 1_003);
    let admission = FakeAdmission::default();
    admission.allow(&ticket);
    let effects = FakeEffects::default();
    effects.install(&ticket, BranchScript::escalation_to_zero(&ticket));
    let clock = FakeClock::default();
    let report = runtime().block_on(
        coordinator(&admission, &effects, &clock)
            .request(ticket.clone())
            .expect("admission winner")
            .wait(),
    );

    assert_eq!(report.outcome(), TeardownOutcome::Closed);
    assert_eq!(effects.terminate_count(ticket.resource_id()), 1);
    assert!(effects
        .events()
        .iter()
        .any(|event| event.ends_with(":wait:Termination")));
}

#[test]
fn teardown_escalation_order_is_fixed() {
    let ticket = ticket(4, 14, TeardownScope::Host, 10, 104, 1_004);
    let admission = FakeAdmission::default();
    admission.allow(&ticket);
    let effects = FakeEffects::default();
    effects.install(&ticket, BranchScript::escalation_to_zero(&ticket));
    let clock = FakeClock::default();
    runtime().block_on(
        coordinator(&admission, &effects, &clock)
            .request(ticket.clone())
            .expect("admission winner")
            .wait(),
    );

    let labels: Vec<String> = effects
        .events()
        .into_iter()
        .map(|event| {
            event
                .split_once(':')
                .expect("event resource separator")
                .1
                .to_string()
        })
        .collect();
    assert_eq!(
        labels,
        vec![
            "drain",
            "cooperative_close",
            "wait:CooperativeGrace",
            "interrupt_or_safe_close",
            "wait:InterruptGrace",
            "terminate_tree",
            "wait:Termination",
            "settle_active_process_zero",
            "detach_after_zero",
            "reconcile_ports",
            "persist_settlement",
            "release_stopped_exact",
        ]
    );
    assert_eq!(clock.deadlines().len(), 3);
}

#[test]
fn teardown_post_zero_failure_is_reported_with_residue_before_release() {
    let ticket = ticket(17, 37, TeardownScope::Host, 20, 117, 1_017);
    let admission = FakeAdmission::default();
    admission.allow(&ticket);
    let effects = FakeEffects::default();
    let mut script = BranchScript::cooperative_zero(&ticket);
    script.residue = Some(ResidueEvidence::new(
        "post-zero-job",
        117,
        1_017,
        "provider.exe",
        "provider --cleanup",
        "detach_after_zero failed",
        vec![
            TeardownStage::DetachAfterZero,
            TeardownStage::ReleaseStoppedExact,
        ],
    ));
    script.detach_after_zero = StageResult::Failed {
        detail: "terminal detach unavailable".to_string(),
    };
    effects.install(&ticket, script);
    let clock = FakeClock::default();
    let coordinator = coordinator(&admission, &effects, &clock);

    let report = runtime().block_on(
        coordinator
            .request(ticket.clone())
            .expect("admission winner")
            .wait(),
    );

    assert_eq!(report.outcome(), TeardownOutcome::CleanupFailed);
    assert!(
        report.residue().is_some(),
        "post-zero failure retains residue"
    );
    let labels: Vec<String> = effects
        .events()
        .into_iter()
        .map(|event| {
            event
                .split_once(':')
                .expect("event separator")
                .1
                .to_string()
        })
        .collect();
    assert_eq!(
        labels,
        vec![
            "drain",
            "cooperative_close",
            "wait:CooperativeGrace",
            "settle_active_process_zero",
            "detach_after_zero",
            "reconcile_ports",
            "persist_settlement",
            "release_stopped_exact",
        ]
    );
    assert_eq!(effects.release_count(ticket.resource_id()), 1);
}

#[test]
fn teardown_completed_cache_evicts_deterministically_without_rerunning_cleanup() {
    let first = ticket(18, 38, TeardownScope::Host, 21, 118, 1_018);
    let second = ticket(19, 39, TeardownScope::Host, 21, 119, 1_019);
    let admission = FakeAdmission::default();
    admission.allow(&first);
    admission.allow(&second);
    let effects = FakeEffects::default();
    effects.install(&first, BranchScript::cooperative_zero(&first));
    effects.install(&second, BranchScript::cooperative_zero(&second));
    let clock = FakeClock::default();
    let store = FakeCompletionStore::default();
    let coordinator = TeardownCoordinator::with_configuration_and_completion_store(
        Arc::new(admission),
        Arc::new(effects.clone()),
        Arc::new(clock),
        2,
        devmanager::process::teardown::TeardownBudgets::default(),
        1,
        Some(Arc::new(store)),
    );

    runtime().block_on(
        coordinator
            .request(first.clone())
            .expect("first admission")
            .wait(),
    );
    runtime().block_on(
        coordinator
            .request(second.clone())
            .expect("second admission")
            .wait(),
    );
    assert_eq!(coordinator.active_operation_count(), 0);
    assert_eq!(coordinator.completed_operation_count(), 1);
    assert_eq!(coordinator.configured_capacity(), 2);

    let retry = runtime().block_on(
        coordinator
            .request(first.clone())
            .expect("durable exact retry")
            .wait(),
    );
    assert_eq!(retry.fence(), first.fence());
    assert_eq!(effects.release_count(first.resource_id()), 1);
}

#[test]
fn teardown_termination_timeout_retains_handles_and_sanitizes_residue() {
    let ticket = ticket(5, 15, TeardownScope::Host, 11, 105, 1_005);
    let admission = FakeAdmission::default();
    admission.allow(&ticket);
    let effects = FakeEffects::default();
    effects.install(&ticket, BranchScript::timeout_with_residue());
    let clock = FakeClock::default();
    let report = runtime().block_on(
        coordinator(&admission, &effects, &clock)
            .request(ticket.clone())
            .expect("admission winner")
            .wait(),
    );

    assert_eq!(report.outcome(), TeardownOutcome::Leaked);
    assert_eq!(effects.terminate_count(ticket.resource_id()), 1);
    assert_eq!(effects.release_count(ticket.resource_id()), 0);
    let residue = report.residue().expect("sanitized residue");
    assert!(!residue.job_name().contains('\n'));
    assert!(!residue.command_label().contains("secret-value"));
    assert!(!residue.last_lifecycle_event().contains('\n'));
    assert!(residue
        .attempted_stages()
        .contains(&TeardownStage::TerminateTree));
    assert!(effects.events().iter().all(|event| {
        !event.ends_with(":detach_after_zero")
            && !event.ends_with(":reconcile_ports")
            && !event.ends_with(":persist_settlement")
    }));
}

#[test]
fn teardown_stale_epoch_and_pid_reuse_cannot_settle_replacement() {
    let current = ticket(6, 16, TeardownScope::Host, 12, 106, 1_006);
    let stale_epoch = ticket(6, 17, TeardownScope::Host, 11, 106, 1_006);
    let reused_pid = ticket(6, 18, TeardownScope::Host, 12, 106, 9_999);
    let admission = FakeAdmission::default();
    admission.allow(&current);
    let effects = FakeEffects::default();
    effects.install(&current, BranchScript::cooperative_zero(&current));
    let clock = FakeClock::default();
    let coordinator = coordinator(&admission, &effects, &clock);

    let stale_error = coordinator
        .request(stale_epoch)
        .expect_err("stale epoch must be rejected");
    let reuse_error = coordinator
        .request(reused_pid)
        .expect_err("PID reuse must be rejected by the full fence");
    assert!(matches!(
        stale_error,
        devmanager::process::teardown::TeardownReject::StaleEpoch { .. }
    ));
    assert!(matches!(
        reuse_error,
        devmanager::process::teardown::TeardownReject::FenceMismatch
    ));
    assert!(effects.events().is_empty());
}

#[test]
fn teardown_caller_cancellation_does_not_cancel_cleanup() {
    let ticket = ticket(7, 19, TeardownScope::Host, 13, 107, 1_007);
    let admission = FakeAdmission::default();
    admission.allow(&ticket);
    let effects = FakeEffects::default();
    effects.install(&ticket, BranchScript::cooperative_zero(&ticket));
    let clock = FakeClock::default();
    let coordinator = coordinator(&admission, &effects, &clock);

    let waiter = coordinator
        .request(ticket.clone())
        .expect("admission winner");
    drop(waiter);
    let replacement_waiter = coordinator
        .request(ticket.clone())
        .expect("duplicate waiter shares owned cleanup");
    let report = runtime().block_on(replacement_waiter.wait());

    assert_eq!(report.outcome(), TeardownOutcome::Closed);
    assert_eq!(effects.release_count(ticket.resource_id()), 1);
}

#[test]
fn teardown_host_shutdown_joins_task_close_by_exact_key() {
    let task_id = TaskId::new();
    let task_ticket = ticket(8, 20, TeardownScope::Task(task_id), 14, 108, 1_008);
    let mismatched = TeardownTicket::new(
        operation_id(21),
        TeardownScope::Host,
        task_ticket.action_epoch(),
        task_ticket.fence().clone(),
    );
    assert!(matches!(
        mismatched,
        Err(devmanager::process::teardown::TeardownTicketError::ScopeOwnerMismatch)
    ));
    let admission = FakeAdmission::default();
    admission.allow(&task_ticket);
    let effects = FakeEffects::default();
    effects.install(&task_ticket, BranchScript::cooperative_zero(&task_ticket));
    let clock = FakeClock::default();
    let coordinator = coordinator(&admission, &effects, &clock);

    let first = coordinator
        .request(task_ticket.clone())
        .expect("task close winner");
    let second = coordinator
        .join(task_ticket.action_epoch(), task_ticket.fence())
        .expect("host close joins task-owned work");
    let (first_report, second_report) =
        runtime().block_on(async { tokio::join!(first.wait(), second.wait()) });

    assert_eq!(first_report, second_report);
    assert_eq!(first_report.operation_id(), operation_id(20));
    assert_eq!(effects.release_count(task_ticket.resource_id()), 1);
    assert_eq!(admission.events().len(), 1);
}

#[test]
fn teardown_max_concurrency_and_report_order_are_deterministic() {
    let tickets = [
        ticket(10, 30, TeardownScope::Host, 16, 110, 1_010),
        ticket(9, 31, TeardownScope::Host, 16, 109, 1_009),
        ticket(12, 32, TeardownScope::Host, 16, 112, 1_012),
        ticket(11, 33, TeardownScope::Host, 16, 111, 1_011),
    ];
    let admission = FakeAdmission::default();
    let effects = FakeEffects::default();
    for ticket in &tickets {
        admission.allow(ticket);
        effects.install(ticket, BranchScript::cooperative_zero(ticket));
    }
    let clock = FakeClock::default();
    let coordinator = coordinator_with_limit(&admission, &effects, &clock, 2);
    let batch = coordinator
        .request_batch(tickets.to_vec())
        .expect("batch admission");
    let reports = runtime().block_on(batch.wait());

    let resources: Vec<ResourceId> = reports.iter().map(|report| report.resource_id()).collect();
    let mut sorted = resources.clone();
    sorted.sort();
    assert_eq!(resources, sorted);
    assert!(effects.max_active() <= 2);
    assert_eq!(reports.len(), tickets.len());
}

#[test]
fn teardown_non_closing_scope_is_rejected() {
    let ticket = ticket(13, 34, TeardownScope::Host, 17, 113, 1_013);
    let admission = FakeAdmission::default();
    admission.allow_non_closing(&ticket);
    let effects = FakeEffects::default();
    effects.install(&ticket, BranchScript::cooperative_zero(&ticket));
    let clock = FakeClock::default();

    let error = coordinator(&admission, &effects, &clock)
        .request(ticket)
        .expect_err("open scope must not start cleanup");
    assert!(matches!(
        error,
        devmanager::process::teardown::TeardownReject::NonClosingScope
    ));
    assert!(effects.events().is_empty());
}

#[test]
fn teardown_registry_empty_membership_without_zero_event_does_not_stop() {
    let ticket = ticket(15, 36, TeardownScope::Task(TaskId::new()), 19, 115, 1_015);
    let job = ExactJob::with_root(ticket.fence().root().id().pid());
    let mut registry = ProcessRegistry::new();
    let registered = RegisteredProcess::new(
        ticket.fence().resource(),
        ticket.fence().owner(),
        ticket.fence().root().clone(),
        ProcessDisplayLabel::new("missing zero event").expect("display label"),
        job.clone(),
    );
    let fence = registry.register(registered).expect("register exact Job");

    job.set_active_process_ids(Vec::new());
    assert!(matches!(
        registry.active_process_zero_proof_exact(&fence),
        Err(devmanager::process::registry::ProcessRegistryError::ActiveProcessZeroUnproved { .. })
    ));
    assert_eq!(
        registry
            .current(ticket.resource_id())
            .expect("current process")
            .state(),
        ManagedProcessState::Starting
    );
}

#[test]
fn teardown_registry_requires_exact_fence_and_authoritative_zero_before_release() {
    let ticket = ticket(14, 35, TeardownScope::Host, 18, 114, 1_014);
    let job = ExactJob::with_root(ticket.fence().root().id().pid());
    let mut registry = ProcessRegistry::new();
    let registered = RegisteredProcess::new(
        ticket.fence().resource(),
        ticket.fence().owner(),
        ticket.fence().root().clone(),
        ProcessDisplayLabel::new("teardown registry").expect("display label"),
        job.clone(),
    );
    let fence = registry.register(registered).expect("register exact Job");

    assert!(registry.exact_fence_matches(&fence));
    let reused_pid = ManagedProcessFence::new(
        fence.resource(),
        fence.owner(),
        identity(fence.root().id().pid(), 99_999),
    );
    assert!(!registry.exact_fence_matches(&reused_pid));
    assert!(matches!(
        registry.active_process_zero_proof_exact(&fence),
        Err(devmanager::process::registry::ProcessRegistryError::ActiveProcessZeroUnproved { .. })
    ));
    assert_eq!(
        registry
            .current(ticket.resource_id())
            .expect("current process")
            .state(),
        ManagedProcessState::Starting
    );

    job.set_active_process_ids(Vec::new());
    assert!(registry.apply_job_completion(JobCompletionMessage::new(
        fence.clone(),
        JobCompletionEvent::ActiveProcessZero,
    )));
    let proof = registry
        .active_process_zero_proof_exact(&fence)
        .expect("matching completion proof");
    assert!(registry
        .settle_active_process_zero_exact(proof)
        .expect("authoritative empty Job query"));
    assert_eq!(
        registry
            .current(ticket.resource_id())
            .expect("stopped process")
            .state(),
        ManagedProcessState::Stopped
    );
    assert!(registry.release_stopped_exact(&fence).is_ok());
}
