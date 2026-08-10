//! Deterministic teardown-coordinator acceptance tests.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use devmanager::domain::id::{OperationId, ResourceId, TaskId};
use devmanager::domain::operation::ResourceFence;
use devmanager::process::identity::{ManagedProcessId, ManagedProcessIdentity, ProcessOwner};
use devmanager::process::registry::{
    JobCompletionEvent, JobCompletionObservation, JobMembership, ManagedProcessFence,
    ManagedProcessState, ProcessDisplayLabel, ProcessRegistry, RegisteredProcess,
};
use devmanager::process::teardown::{
    AdmissionReceipt, AdmissionState, BoxFuture, ResidueEvidence, StageResult, TeardownAdmission,
    TeardownAdmissionError, TeardownBudgets, TeardownClock, TeardownCompletionStore,
    TeardownCoordinator, TeardownDeadline, TeardownEffects, TeardownOutcome, TeardownScope,
    TeardownStage, TeardownTicket, WaitResult, WaitStage,
};
use tokio::sync::watch;

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

fn indexed_uuid(index: u16) -> [u8; 16] {
    let mut bytes = fixed_uuid_v7(0);
    bytes[14] = (index >> 8) as u8;
    bytes[15] = index as u8;
    bytes
}

fn indexed_resource_id(index: u16) -> ResourceId {
    ResourceId::from_bytes(indexed_uuid(index)).expect("indexed resource id")
}

fn indexed_operation_id(index: u16) -> OperationId {
    OperationId::from_bytes(indexed_uuid(index)).expect("indexed operation id")
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

fn indexed_ticket(index: u16) -> TeardownTicket {
    let resource = indexed_resource_id(index);
    let fence = ManagedProcessFence::new(
        ResourceFence::new(resource, 1),
        ProcessOwner::Host,
        identity(50_000 + u32::from(index), 500_000 + u64::from(index)),
    );
    TeardownTicket::new(indexed_operation_id(index), TeardownScope::Host, 1, fence)
        .expect("indexed ticket scope matches process owner")
}

#[derive(Debug, Clone)]
struct AdmissionRule {
    fence: ManagedProcessFence,
    action_epoch: u64,
    state: AdmissionState,
    return_non_closing: bool,
}

#[derive(Debug, Clone)]
struct ExactJob {
    active_process_ids: Arc<Mutex<Vec<u32>>>,
    root: ManagedProcessIdentity,
}

impl ExactJob {
    fn with_root(root: ManagedProcessIdentity) -> Self {
        Self {
            active_process_ids: Arc::new(Mutex::new(vec![root.id().pid()])),
            root,
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

    fn inspect_process(
        &self,
        pid: u32,
    ) -> Result<devmanager::process::registry::JobMemberInfo, String> {
        if pid == self.root.id().pid()
            && self
                .active_process_ids
                .lock()
                .expect("exact Job membership")
                .contains(&pid)
        {
            Ok(devmanager::process::registry::JobMemberInfo::new(
                self.root.clone(),
                None,
            ))
        } else {
            Err(format!("PID {pid} is not an exact Job member"))
        }
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

    fn close_admission_batch(
        &self,
        tickets: &[TeardownTicket],
    ) -> Result<Vec<AdmissionReceipt>, TeardownAdmissionError> {
        let mut rules = self.rules.lock().expect("admission rules");
        let mut receipts = Vec::with_capacity(tickets.len());
        for ticket in tickets {
            self.events
                .lock()
                .expect("admission events")
                .push(format!("admission:{}", ticket.resource_id()));
            let rule = rules
                .get(&ticket.resource_id())
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
            receipts.push(AdmissionReceipt::new(
                ticket.scope(),
                if rule.return_non_closing {
                    AdmissionState::Open
                } else {
                    AdmissionState::Closing
                },
                rule.action_epoch,
                rule.fence.clone(),
            ));
        }
        for ticket in tickets {
            if let Some(rule) = rules.get_mut(&ticket.resource_id()) {
                if !rule.return_non_closing {
                    rule.state = AdmissionState::Closing;
                }
            }
        }
        Ok(receipts)
    }

    fn rollback_admission_batch(
        &self,
        tickets: &[TeardownTicket],
        receipts: &[AdmissionReceipt],
    ) -> Result<(), TeardownAdmissionError> {
        if tickets.len() != receipts.len() {
            return Err(TeardownAdmissionError::Other {
                detail: "rollback receipt count did not match tickets".to_string(),
            });
        }
        let mut rules = self.rules.lock().expect("admission rules");
        for (ticket, receipt) in tickets.iter().zip(receipts) {
            let rule = rules
                .get(&ticket.resource_id())
                .ok_or(TeardownAdmissionError::FenceMismatch)?;
            if receipt.state() != AdmissionState::Closing
                || receipt.scope() != ticket.scope()
                || receipt.action_epoch() != ticket.action_epoch()
                || receipt.fence() != ticket.fence()
                || rule.action_epoch != ticket.action_epoch()
                || rule.fence != *ticket.fence()
                || rule.state != AdmissionState::Closing
            {
                return Err(TeardownAdmissionError::FenceMismatch);
            }
        }
        for ticket in tickets {
            if let Some(rule) = rules.get_mut(&ticket.resource_id()) {
                rule.state = AdmissionState::Open;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct BlockingAdmission {
    inner: FakeAdmission,
    entered_batch: Arc<AtomicBool>,
    release_batch: Arc<AtomicBool>,
}

impl BlockingAdmission {
    fn new(inner: FakeAdmission) -> Self {
        Self {
            inner,
            entered_batch: Arc::new(AtomicBool::new(false)),
            release_batch: Arc::new(AtomicBool::new(false)),
        }
    }

    fn wait_until_batch_entered(&self) {
        let deadline = Instant::now() + Duration::from_secs(1);
        while !self.entered_batch.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(self.entered_batch.load(Ordering::SeqCst));
    }

    fn release_batch(&self) {
        self.release_batch.store(true, Ordering::SeqCst);
    }
}

impl TeardownAdmission for BlockingAdmission {
    fn close_admission(
        &self,
        ticket: &TeardownTicket,
    ) -> Result<AdmissionReceipt, TeardownAdmissionError> {
        self.inner.close_admission(ticket)
    }

    fn close_admission_batch(
        &self,
        tickets: &[TeardownTicket],
    ) -> Result<Vec<AdmissionReceipt>, TeardownAdmissionError> {
        self.entered_batch.store(true, Ordering::SeqCst);
        while !self.release_batch.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        self.inner.close_admission_batch(tickets)
    }

    fn rollback_admission_batch(
        &self,
        tickets: &[TeardownTicket],
        receipts: &[AdmissionReceipt],
    ) -> Result<(), TeardownAdmissionError> {
        self.inner.rollback_admission_batch(tickets, receipts)
    }
}

#[derive(Debug, Default, Clone)]
struct FakeCompletionStore {
    inner: TeardownCompletionStore,
}

impl FakeCompletionStore {
    fn fail_persist(&self, detail: &str) {
        self.inner.fail_persist_for_test(detail);
    }

    fn store(&self) -> TeardownCompletionStore {
        self.inner.clone()
    }
}

#[derive(Debug, Clone)]
struct BlockingLookupStore {
    inner: TeardownCompletionStore,
}

impl BlockingLookupStore {
    fn new() -> Self {
        let inner = TeardownCompletionStore::default();
        inner.block_lookup_for_test();
        Self { inner }
    }

    fn wait_until_started(&self) {
        let deadline = Instant::now() + Duration::from_secs(1);
        while self.inner.lookup_started_for_test() == 0 && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(self.inner.lookup_started_for_test(), 1);
    }

    fn release(&self) {
        self.inner.release_lookup_for_test();
    }

    fn store(&self) -> TeardownCompletionStore {
        self.inner.clone()
    }
}

#[derive(Debug, Clone)]
struct BlockingPersistStore {
    inner: TeardownCompletionStore,
}

impl BlockingPersistStore {
    fn new() -> Self {
        let inner = TeardownCompletionStore::default();
        inner.block_persist_for_test();
        Self { inner }
    }

    fn max_active(&self) -> usize {
        self.inner.persist_max_active_for_test()
    }

    fn release(&self) {
        self.inner.release_persist_for_test();
    }

    fn store(&self) -> TeardownCompletionStore {
        self.inner.clone()
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
    Zero,
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
    fn cooperative_zero(_ticket: &TeardownTicket) -> Self {
        Self {
            cooperative: WaitPlan::Zero,
            interrupt: WaitPlan::TimedOut,
            termination: WaitPlan::TimedOut,
            residue: None,
            detach_after_zero: StageResult::Completed,
            reconcile_ports: StageResult::Completed,
            persist_settlement: StageResult::Completed,
            release_stopped_exact: StageResult::Completed,
        }
    }

    fn escalation_to_zero(_ticket: &TeardownTicket) -> Self {
        Self {
            cooperative: WaitPlan::TimedOut,
            interrupt: WaitPlan::TimedOut,
            termination: WaitPlan::Zero,
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
                "",
                "provider --token secret-value --authorization Bearer bearer-secret Authorization: Bearer header-secret\r\nchild",
                "ACTIVE_PROCESS_ZERO\nnot-authoritative",
                vec![TeardownStage::Drain, TeardownStage::TerminateTree],
            )),
            detach_after_zero: StageResult::Completed,
            reconcile_ports: StageResult::Completed,
            persist_settlement: StageResult::Completed,
            release_stopped_exact: StageResult::Completed,
        }
    }

    fn timeout_without_residue() -> Self {
        Self {
            cooperative: WaitPlan::TimedOut,
            interrupt: WaitPlan::TimedOut,
            termination: WaitPlan::TimedOut,
            residue: None,
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
    drain_started: Arc<AtomicUsize>,
    drain_gate: Option<watch::Sender<bool>>,
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

    fn drain_started_count(&self) -> usize {
        self.state
            .lock()
            .expect("effects state")
            .drain_started
            .load(Ordering::SeqCst)
    }

    fn install_drain_gate(&self) -> watch::Sender<bool> {
        let (sender, _receiver) = watch::channel(false);
        self.state.lock().expect("effects state").drain_gate = Some(sender.clone());
        sender
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
                state.drain_started.fetch_add(1, Ordering::SeqCst);
            }
            let gate = effects
                .state
                .lock()
                .expect("effects state")
                .drain_gate
                .clone();
            if let Some(gate) = gate {
                let mut released = gate.subscribe();
                while !*released.borrow() {
                    if released.changed().await.is_err() {
                        break;
                    }
                }
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
                WaitPlan::Zero => WaitResult::Zero,
            }
        })
    }

    fn settle_active_process_zero<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
    ) -> BoxFuture<'a, StageResult> {
        let effects = self.clone();
        let ticket = ticket.clone();
        Box::pin(async move {
            effects.record(&ticket, "settle_active_process_zero");
            StageResult::Completed
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FaultStage {
    Drain,
    Wait(WaitStage),
    PersistSettlement,
    PanicDrain,
}

#[derive(Debug, Clone)]
struct FaultyEffects {
    inner: FakeEffects,
    stage: FaultStage,
}

impl FaultyEffects {
    fn hangs(inner: &FakeEffects, stage: FaultStage) -> Self {
        Self {
            inner: inner.clone(),
            stage,
        }
    }

    fn panics(inner: &FakeEffects, stage: FaultStage) -> Self {
        Self {
            inner: inner.clone(),
            stage,
        }
    }
}

impl TeardownEffects for FaultyEffects {
    fn drain<'a>(&'a self, ticket: &'a TeardownTicket) -> BoxFuture<'a, StageResult> {
        if self.stage == FaultStage::PanicDrain {
            return Box::pin(async { panic!("test completion waiter failure") });
        }
        if self.stage == FaultStage::Drain {
            return Box::pin(std::future::pending());
        }
        self.inner.drain(ticket)
    }

    fn cooperative_close<'a>(&'a self, ticket: &'a TeardownTicket) -> BoxFuture<'a, StageResult> {
        self.inner.cooperative_close(ticket)
    }

    fn interrupt_or_safe_close<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
    ) -> BoxFuture<'a, StageResult> {
        self.inner.interrupt_or_safe_close(ticket)
    }

    fn terminate_tree<'a>(&'a self, ticket: &'a TeardownTicket) -> BoxFuture<'a, StageResult> {
        self.inner.terminate_tree(ticket)
    }

    fn wait_for_zero<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
        stage: WaitStage,
        deadline: TeardownDeadline,
    ) -> BoxFuture<'a, WaitResult> {
        if self.stage == FaultStage::Wait(stage) {
            return Box::pin(std::future::pending());
        }
        self.inner.wait_for_zero(ticket, stage, deadline)
    }

    fn settle_active_process_zero<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
    ) -> BoxFuture<'a, StageResult> {
        self.inner.settle_active_process_zero(ticket)
    }

    fn detach_after_zero<'a>(&'a self, ticket: &'a TeardownTicket) -> BoxFuture<'a, StageResult> {
        self.inner.detach_after_zero(ticket)
    }

    fn reconcile_ports<'a>(&'a self, ticket: &'a TeardownTicket) -> BoxFuture<'a, StageResult> {
        self.inner.reconcile_ports(ticket)
    }

    fn persist_settlement<'a>(&'a self, ticket: &'a TeardownTicket) -> BoxFuture<'a, StageResult> {
        if self.stage == FaultStage::PersistSettlement {
            return Box::pin(std::future::pending());
        }
        self.inner.persist_settlement(ticket)
    }

    fn residue<'a>(&'a self, ticket: &'a TeardownTicket) -> BoxFuture<'a, Option<ResidueEvidence>> {
        self.inner.residue(ticket)
    }

    fn release_stopped_exact<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
    ) -> BoxFuture<'a, StageResult> {
        self.inner.release_stopped_exact(ticket)
    }
}

#[derive(Debug, Clone)]
struct BlockingConstructionEffects {
    inner: FakeEffects,
    release: Arc<AtomicUsize>,
    block_residue: bool,
}

impl BlockingConstructionEffects {
    fn new(inner: &FakeEffects) -> Self {
        Self {
            inner: inner.clone(),
            release: Arc::new(AtomicUsize::new(0)),
            block_residue: false,
        }
    }

    fn for_residue(inner: &FakeEffects) -> Self {
        Self {
            inner: inner.clone(),
            release: Arc::new(AtomicUsize::new(0)),
            block_residue: true,
        }
    }

    fn release(&self) {
        self.release.store(1, Ordering::SeqCst);
    }
}

impl TeardownEffects for BlockingConstructionEffects {
    fn drain<'a>(&'a self, ticket: &'a TeardownTicket) -> BoxFuture<'a, StageResult> {
        if !self.block_residue {
            while self.release.load(Ordering::SeqCst) == 0 {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        self.inner.drain(ticket)
    }

    fn cooperative_close<'a>(&'a self, ticket: &'a TeardownTicket) -> BoxFuture<'a, StageResult> {
        self.inner.cooperative_close(ticket)
    }

    fn interrupt_or_safe_close<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
    ) -> BoxFuture<'a, StageResult> {
        self.inner.interrupt_or_safe_close(ticket)
    }

    fn terminate_tree<'a>(&'a self, ticket: &'a TeardownTicket) -> BoxFuture<'a, StageResult> {
        self.inner.terminate_tree(ticket)
    }

    fn wait_for_zero<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
        stage: WaitStage,
        deadline: TeardownDeadline,
    ) -> BoxFuture<'a, WaitResult> {
        self.inner.wait_for_zero(ticket, stage, deadline)
    }

    fn settle_active_process_zero<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
    ) -> BoxFuture<'a, StageResult> {
        self.inner.settle_active_process_zero(ticket)
    }

    fn detach_after_zero<'a>(&'a self, ticket: &'a TeardownTicket) -> BoxFuture<'a, StageResult> {
        self.inner.detach_after_zero(ticket)
    }

    fn reconcile_ports<'a>(&'a self, ticket: &'a TeardownTicket) -> BoxFuture<'a, StageResult> {
        self.inner.reconcile_ports(ticket)
    }

    fn persist_settlement<'a>(&'a self, ticket: &'a TeardownTicket) -> BoxFuture<'a, StageResult> {
        self.inner.persist_settlement(ticket)
    }

    fn residue<'a>(&'a self, ticket: &'a TeardownTicket) -> BoxFuture<'a, Option<ResidueEvidence>> {
        if self.block_residue {
            while self.release.load(Ordering::SeqCst) == 0 {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        self.inner.residue(ticket)
    }

    fn release_stopped_exact<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
    ) -> BoxFuture<'a, StageResult> {
        self.inner.release_stopped_exact(ticket)
    }
}

fn coordinator(
    admission: &FakeAdmission,
    effects: &FakeEffects,
    clock: &FakeClock,
) -> TeardownCoordinator {
    coordinator_with_store(
        admission,
        Arc::new(effects.clone()),
        clock,
        FakeCompletionStore::default().store(),
    )
}

fn coordinator_with_effects_and_budgets(
    admission: &FakeAdmission,
    effects: Arc<dyn TeardownEffects>,
    clock: &FakeClock,
    budgets: TeardownBudgets,
) -> TeardownCoordinator {
    TeardownCoordinator::with_configuration(
        Arc::new(admission.clone()),
        effects,
        Arc::new(clock.clone()),
        4,
        budgets,
        FakeCompletionStore::default().store(),
    )
}

fn coordinator_with_store(
    admission: &FakeAdmission,
    effects: Arc<dyn TeardownEffects>,
    clock: &FakeClock,
    completion_store: TeardownCompletionStore,
) -> TeardownCoordinator {
    TeardownCoordinator::new(
        Arc::new(admission.clone()),
        effects,
        Arc::new(clock.clone()),
        completion_store,
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
        FakeCompletionStore::default().store(),
    )
}

fn coordinator_with_limit_and_budgets(
    admission: &FakeAdmission,
    effects: &FakeEffects,
    clock: &FakeClock,
    configured_capacity: usize,
    budgets: TeardownBudgets,
) -> TeardownCoordinator {
    TeardownCoordinator::with_configuration(
        Arc::new(admission.clone()),
        Arc::new(effects.clone()),
        Arc::new(clock.clone()),
        configured_capacity,
        budgets,
        FakeCompletionStore::default().store(),
    )
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
}

#[test]
fn teardown_coordinator_bounds_a_hung_adapter_stage() {
    let ticket = ticket(21, 41, TeardownScope::Host, 23, 121, 1_021);
    let admission = FakeAdmission::default();
    admission.allow(&ticket);
    let effects = FakeEffects::default();
    effects.install(&ticket, BranchScript::timeout_without_residue());
    let clock = FakeClock::default();
    let coordinator = coordinator_with_effects_and_budgets(
        &admission,
        Arc::new(FaultyEffects::hangs(&effects, FaultStage::Drain)),
        &clock,
        TeardownBudgets::new(
            Duration::from_millis(25),
            Duration::from_millis(25),
            Duration::from_millis(25),
        ),
    );

    let report = runtime().block_on(async {
        let waiter = coordinator.request(ticket).expect("admission winner");
        tokio::time::timeout(Duration::from_millis(500), waiter.wait())
            .await
            .expect("coordinator must settle a hung adapter stage")
    });

    assert_eq!(report.outcome(), TeardownOutcome::CleanupFailed);
    assert!(report.residue().is_some());
    assert!(report
        .errors()
        .iter()
        .any(|error| error.contains("timeout")));
}

#[test]
fn teardown_coordinator_bounds_a_hung_zero_wait() {
    let ticket = ticket(22, 42, TeardownScope::Host, 24, 122, 1_022);
    let admission = FakeAdmission::default();
    admission.allow(&ticket);
    let effects = FakeEffects::default();
    effects.install(&ticket, BranchScript::cooperative_zero(&ticket));
    let clock = FakeClock::default();
    let coordinator = coordinator_with_effects_and_budgets(
        &admission,
        Arc::new(FaultyEffects::hangs(
            &effects,
            FaultStage::Wait(WaitStage::CooperativeGrace),
        )),
        &clock,
        TeardownBudgets::new(
            Duration::from_millis(25),
            Duration::from_millis(25),
            Duration::from_millis(25),
        ),
    );

    let report = runtime().block_on(async {
        let waiter = coordinator.request(ticket).expect("admission winner");
        tokio::time::timeout(Duration::from_millis(500), waiter.wait())
            .await
            .expect("coordinator must settle a hung zero wait")
    });

    assert_eq!(report.outcome(), TeardownOutcome::CleanupFailed);
    assert!(report.residue().is_some());
}

#[test]
fn teardown_coordinator_bounds_a_hung_persistence_stage() {
    let ticket = ticket(23, 43, TeardownScope::Host, 25, 123, 1_023);
    let admission = FakeAdmission::default();
    admission.allow(&ticket);
    let effects = FakeEffects::default();
    effects.install(&ticket, BranchScript::cooperative_zero(&ticket));
    let clock = FakeClock::default();
    let coordinator = coordinator_with_effects_and_budgets(
        &admission,
        Arc::new(FaultyEffects::hangs(
            &effects,
            FaultStage::PersistSettlement,
        )),
        &clock,
        TeardownBudgets::new(
            Duration::from_millis(25),
            Duration::from_millis(25),
            Duration::from_millis(25),
        ),
    );

    let report = runtime().block_on(async {
        let waiter = coordinator.request(ticket).expect("admission winner");
        tokio::time::timeout(Duration::from_millis(500), waiter.wait())
            .await
            .expect("coordinator must settle a hung persistence stage")
    });

    assert_eq!(report.outcome(), TeardownOutcome::CleanupFailed);
    assert!(report.residue().is_some());
}

#[test]
fn teardown_waiter_channel_failure_returns_bounded_cleanup_failed_report() {
    let ticket = ticket(24, 44, TeardownScope::Host, 26, 124, 1_024);
    let admission = FakeAdmission::default();
    admission.allow(&ticket);
    let effects = FakeEffects::default();
    effects.install(&ticket, BranchScript::timeout_without_residue());
    let clock = FakeClock::default();
    let coordinator = coordinator_with_effects_and_budgets(
        &admission,
        Arc::new(FaultyEffects::panics(&effects, FaultStage::PanicDrain)),
        &clock,
        TeardownBudgets::new(
            Duration::from_millis(25),
            Duration::from_millis(25),
            Duration::from_millis(25),
        ),
    );

    let report = runtime().block_on(async {
        let waiter = coordinator.request(ticket).expect("admission winner");
        tokio::time::timeout(Duration::from_millis(500), waiter.wait())
            .await
            .expect("waiter channel failure must become a bounded result")
    });

    assert_eq!(report.outcome(), TeardownOutcome::CleanupFailed);
    assert!(report.residue().is_some());
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
fn teardown_zero_result_is_settled_by_the_effect_adapter() {
    let ticket = ticket(2, 12, TeardownScope::Host, 8, 102, 1_002);
    let admission = FakeAdmission::default();
    admission.allow(&ticket);
    let effects = FakeEffects::default();
    let mut script = BranchScript::escalation_to_zero(&ticket);
    script.interrupt = WaitPlan::Zero;
    effects.install(&ticket, script);
    let clock = FakeClock::default();
    let report = runtime().block_on(
        coordinator(&admission, &effects, &clock)
            .request(ticket.clone())
            .expect("admission winner")
            .wait(),
    );

    assert_eq!(report.outcome(), TeardownOutcome::Closed);
    assert_eq!(effects.terminate_count(ticket.resource_id()), 0);
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
    assert_eq!(clock.deadlines().len(), 1);
    assert!(clock
        .deadlines()
        .windows(2)
        .all(|deadlines| deadlines[0] == deadlines[1]));
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
fn teardown_handoff_failure_keeps_idempotent_residue_without_poisoning_other_work() {
    let first = ticket(25, 45, TeardownScope::Host, 27, 125, 1_025);
    let unrelated = ticket(26, 46, TeardownScope::Host, 27, 126, 1_026);
    let admission = FakeAdmission::default();
    admission.allow(&first);
    admission.allow(&unrelated);
    let effects = FakeEffects::default();
    effects.install(&first, BranchScript::cooperative_zero(&first));
    effects.install(&unrelated, BranchScript::cooperative_zero(&unrelated));
    let store = FakeCompletionStore::default();
    store.fail_persist("completion journal unavailable");
    let clock = FakeClock::default();
    let coordinator =
        coordinator_with_store(&admission, Arc::new(effects.clone()), &clock, store.store());

    let first_report = runtime().block_on(
        coordinator
            .request(first.clone())
            .expect("first admission")
            .wait(),
    );
    assert_eq!(first_report.outcome(), TeardownOutcome::CleanupFailed);
    assert!(first_report.residue().is_some());
    assert_eq!(coordinator.active_operation_count(), 0);

    let retry = runtime().block_on(
        coordinator
            .request(first.clone())
            .expect("exact retry uses bounded in-memory settlement")
            .wait(),
    );
    assert_eq!(retry, first_report);
    assert_eq!(effects.release_count(first.resource_id()), 1);

    let unrelated_report = runtime().block_on(
        coordinator
            .request(unrelated.clone())
            .expect("handoff failure must not poison unrelated operations")
            .wait(),
    );
    assert_eq!(unrelated_report.outcome(), TeardownOutcome::CleanupFailed);
    assert_eq!(effects.release_count(unrelated.resource_id()), 1);
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
        store.store(),
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
    assert_eq!(
        residue.executable(),
        ticket
            .fence()
            .root()
            .canonical_executable()
            .to_string_lossy()
    );
    assert!(residue.command_label().contains("--token <redacted>"));
    assert!(residue
        .command_label()
        .contains("--authorization <redacted>"));
    assert!(residue
        .command_label()
        .contains("Authorization: <redacted>"));
    assert!(!residue.command_label().contains("secret-value"));
    assert!(!residue.command_label().contains("bearer-secret"));
    assert!(!residue.command_label().contains("header-secret"));
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
fn teardown_exhausted_cleanup_synthesizes_required_residue_when_adapter_has_none() {
    let ticket = ticket(20, 40, TeardownScope::Host, 22, 120, 1_020);
    let admission = FakeAdmission::default();
    admission.allow(&ticket);
    let effects = FakeEffects::default();
    effects.install(&ticket, BranchScript::timeout_without_residue());
    let clock = FakeClock::default();

    let report = runtime().block_on(
        coordinator(&admission, &effects, &clock)
            .request(ticket.clone())
            .expect("admission winner")
            .wait(),
    );

    assert_eq!(report.outcome(), TeardownOutcome::Leaked);
    let residue = report
        .residue()
        .expect("exhausted cleanup must always retain residue evidence");
    assert_eq!(residue.pid(), ticket.fence().root().id().pid());
    assert_eq!(
        residue.creation_time_100ns(),
        ticket.fence().root().id().creation_time_100ns()
    );
    assert!(!residue.job_name().is_empty());
    assert_eq!(
        residue.executable(),
        ticket
            .fence()
            .root()
            .canonical_executable()
            .to_string_lossy()
    );
    assert_eq!(residue.command_label(), "<unavailable: root command>");
    assert!(residue.last_lifecycle_event().contains("state=Leaked"));
    assert!(residue
        .last_lifecycle_event()
        .contains("last_lifecycle_stage=TerminationWait"));
}

#[test]
fn teardown_executor_drop_settles_active_and_queued_cells_as_failed() {
    let admission = FakeAdmission::default();
    let effects = FakeEffects::default();
    let mut tickets = Vec::new();
    for index in 1..=5u16 {
        let ticket = indexed_ticket(index);
        admission.allow(&ticket);
        effects.install(&ticket, BranchScript::cooperative_zero(&ticket));
        tickets.push(ticket);
    }
    let release_drain = effects.install_drain_gate();
    let clock = FakeClock::default();
    let coordinator = coordinator_with_limit_and_budgets(
        &admission,
        &effects,
        &clock,
        4,
        TeardownBudgets::new(
            Duration::from_secs(30),
            Duration::from_secs(30),
            Duration::from_secs(30),
        ),
    );

    let waiters: Vec<_> = tickets
        .iter()
        .cloned()
        .map(|ticket| coordinator.request(ticket).expect("accepted cleanup"))
        .collect();
    let deadline = Instant::now() + Duration::from_secs(2);
    while effects.drain_started_count() < 4 && Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert_eq!(effects.drain_started_count(), 4);

    let release_thread = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        release_drain.send_replace(true);
    });
    drop(coordinator);
    release_thread.join().expect("release teardown workers");

    let reports = runtime().block_on(async {
        let mut reports = Vec::with_capacity(waiters.len());
        for waiter in waiters {
            reports.push(waiter.wait().await);
        }
        reports
    });
    assert_eq!(
        reports
            .iter()
            .filter(|report| report.outcome() == TeardownOutcome::CleanupFailed)
            .count(),
        5,
        "dropping the coordinator must settle active and queued cleanup as failed cancellation"
    );
    assert_eq!(
        effects.drain_started_count(),
        4,
        "shutdown must not start a queued cleanup after cancellation"
    );
}

#[test]
fn teardown_coordinator_drop_settles_active_and_queued_waiters_promptly() {
    let admission = FakeAdmission::default();
    let effects = FakeEffects::default();
    let mut tickets = Vec::new();
    for index in 1..=5u16 {
        let ticket = indexed_ticket(index);
        admission.allow(&ticket);
        effects.install(&ticket, BranchScript::cooperative_zero(&ticket));
        tickets.push(ticket);
    }
    let release_drain = effects.install_drain_gate();
    let clock = FakeClock::default();
    let coordinator = coordinator_with_limit_and_budgets(
        &admission,
        &effects,
        &clock,
        4,
        TeardownBudgets::new(
            Duration::from_secs(30),
            Duration::from_secs(30),
            Duration::from_secs(30),
        ),
    );

    let waiters: Vec<_> = tickets
        .iter()
        .cloned()
        .map(|ticket| coordinator.request(ticket).expect("accepted cleanup"))
        .collect();
    let deadline = Instant::now() + Duration::from_secs(2);
    while effects.drain_started_count() < 4 && Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert_eq!(effects.drain_started_count(), 4);

    let started = Instant::now();
    drop(coordinator);
    let prompt = runtime().block_on(async {
        tokio::time::timeout(Duration::from_millis(100), async {
            let mut reports = Vec::with_capacity(waiters.len());
            for waiter in waiters.iter().cloned() {
                reports.push(waiter.wait().await);
            }
            reports
        })
        .await
    });

    release_drain.send_replace(true);
    let _ = runtime().block_on(async {
        tokio::time::timeout(Duration::from_secs(2), async {
            for waiter in waiters {
                let _ = waiter.wait().await;
            }
        })
        .await
    });

    assert!(
        prompt.is_ok(),
        "dropping the coordinator must settle active and queued waiters within 100ms (elapsed {:?})",
        started.elapsed()
    );
    let reports = prompt.expect("prompt waiter settlement");
    assert_eq!(reports.len(), 5);
    assert!(reports.iter().all(|report| {
        report.outcome() == TeardownOutcome::CleanupFailed
            && report.residue().is_some()
            && report.errors().iter().any(|error| error.contains("cancel"))
    }));
}

#[test]
fn teardown_stage_future_construction_is_inside_the_deadline() {
    let ticket = ticket(27, 47, TeardownScope::Host, 28, 127, 1_027);
    let admission = FakeAdmission::default();
    admission.allow(&ticket);
    let effects = FakeEffects::default();
    effects.install(&ticket, BranchScript::cooperative_zero(&ticket));
    let blocking = BlockingConstructionEffects::new(&effects);
    let clock = FakeClock::default();
    let coordinator = coordinator_with_effects_and_budgets(
        &admission,
        Arc::new(blocking.clone()),
        &clock,
        TeardownBudgets::new(
            Duration::from_millis(25),
            Duration::from_millis(25),
            Duration::from_millis(25),
        ),
    );
    let waiter = coordinator.request(ticket).expect("admission winner");
    let started = Instant::now();
    let bounded = runtime().block_on(async {
        tokio::time::timeout(Duration::from_millis(100), waiter.clone().wait()).await
    });

    blocking.release();
    let _ = runtime().block_on(waiter.wait());

    assert!(
        bounded.is_ok(),
        "synchronous future construction must not bypass the stage deadline (elapsed {:?})",
        started.elapsed()
    );
    let report = bounded.expect("bounded stage report");
    assert_eq!(report.outcome(), TeardownOutcome::CleanupFailed);
    assert!(report
        .errors()
        .iter()
        .any(|error| error.contains("Drain") && error.contains("timeout")));
}

#[test]
fn teardown_residue_future_construction_is_inside_the_deadline() {
    let ticket = ticket(29, 49, TeardownScope::Host, 30, 129, 1_029);
    let admission = FakeAdmission::default();
    admission.allow(&ticket);
    let effects = FakeEffects::default();
    effects.install(&ticket, BranchScript::timeout_without_residue());
    let blocking = BlockingConstructionEffects::for_residue(&effects);
    let clock = FakeClock::default();
    let coordinator = coordinator_with_effects_and_budgets(
        &admission,
        Arc::new(blocking.clone()),
        &clock,
        TeardownBudgets::new(
            Duration::from_millis(15),
            Duration::from_millis(15),
            Duration::from_millis(15),
        ),
    );
    let waiter = coordinator.request(ticket).expect("admission winner");
    let bounded = runtime().block_on(async {
        tokio::time::timeout(Duration::from_millis(100), waiter.clone().wait()).await
    });

    assert!(
        bounded.is_ok(),
        "synchronous residue construction must not bypass its deadline"
    );
    let report = bounded.expect("bounded residue report");
    assert_eq!(report.outcome(), TeardownOutcome::CleanupFailed);
    assert!(report
        .errors()
        .iter()
        .any(|error| error.contains("Residue") && error.contains("timeout")));

    blocking.release();
    let _ = runtime().block_on(waiter.wait());
}

#[test]
fn teardown_shutdown_rejects_new_admission_without_lookup_or_effects() {
    let ticket = ticket(30, 50, TeardownScope::Host, 31, 130, 1_030);
    let admission = FakeAdmission::default();
    admission.allow(&ticket);
    let effects = FakeEffects::default();
    let store = TeardownCompletionStore::default();
    let coordinator = TeardownCoordinator::with_capacity(
        Arc::new(admission.clone()),
        Arc::new(effects.clone()),
        Arc::new(FakeClock::default()),
        1,
        store.clone(),
    );

    coordinator.shutdown();
    assert!(matches!(
        coordinator.request(ticket),
        Err(devmanager::process::teardown::TeardownReject::ExecutorClosed)
    ));
    assert_eq!(store.lookup_started_for_test(), 0);
    assert!(admission.events().is_empty());
    assert!(effects.events().is_empty());
}

#[test]
fn teardown_shutdown_race_rolls_back_admission_when_submission_closes() {
    let ticket = ticket(31, 51, TeardownScope::Host, 32, 131, 1_031);
    let admission = FakeAdmission::default();
    admission.allow(&ticket);
    let blocking_admission = BlockingAdmission::new(admission.clone());
    let coordinator = Arc::new(TeardownCoordinator::with_capacity(
        Arc::new(blocking_admission.clone()),
        Arc::new(FakeEffects::default()),
        Arc::new(FakeClock::default()),
        1,
        TeardownCompletionStore::default(),
    ));
    let request_coordinator = Arc::clone(&coordinator);
    let request_ticket = ticket.clone();
    let request_thread = std::thread::spawn(move || request_coordinator.request(request_ticket));

    blocking_admission.wait_until_batch_entered();
    coordinator.shutdown();
    blocking_admission.release_batch();

    assert!(matches!(
        request_thread
            .join()
            .expect("admission race request thread"),
        Err(devmanager::process::teardown::TeardownReject::ExecutorClosed)
    ));
    assert!(
        !admission.launch_or_input_is_rejected(ticket.resource_id()),
        "a submission race must reopen the exact admission it did not schedule"
    );
}

#[test]
fn teardown_completion_lookup_is_bounded_at_the_store_boundary() {
    let ticket = ticket(28, 48, TeardownScope::Host, 29, 128, 1_028);
    let admission = FakeAdmission::default();
    admission.allow(&ticket);
    let effects = FakeEffects::default();
    effects.install(&ticket, BranchScript::cooperative_zero(&ticket));
    let clock = FakeClock::default();
    let store = BlockingLookupStore::new();
    let coordinator = TeardownCoordinator::with_configuration(
        Arc::new(admission),
        Arc::new(effects),
        Arc::new(clock),
        1,
        TeardownBudgets::new(
            Duration::from_millis(25),
            Duration::from_millis(25),
            Duration::from_millis(25),
        ),
        store.store(),
    );
    let (sender, receiver) = std::sync::mpsc::channel();
    let request_thread = std::thread::spawn(move || {
        sender
            .send(coordinator.request(ticket))
            .expect("lookup result receiver");
    });
    store.wait_until_started();
    let result = receiver
        .recv_timeout(Duration::from_millis(150))
        .expect("completion lookup must return within its bound");
    assert!(matches!(
        result,
        Err(devmanager::process::teardown::TeardownReject::CompletionLookupFailed { .. })
    ));
    store.release();
    request_thread
        .join()
        .expect("bounded lookup request thread");
}

#[test]
fn teardown_completion_persist_uses_fixed_capacity_under_stuck_store_pressure() {
    const REQUESTS: u16 = 32;
    let admission = FakeAdmission::default();
    let effects = FakeEffects::default();
    let clock = FakeClock::default();
    let store = BlockingPersistStore::new();
    let coordinator = TeardownCoordinator::with_configuration(
        Arc::new(admission.clone()),
        Arc::new(effects.clone()),
        Arc::new(clock),
        4,
        TeardownBudgets::new(
            Duration::from_millis(25),
            Duration::from_millis(25),
            Duration::from_millis(25),
        ),
        store.store(),
    );

    let mut waiters = Vec::with_capacity(REQUESTS as usize);
    for index in 1..=REQUESTS {
        let ticket = indexed_ticket(index);
        admission.allow(&ticket);
        effects.install(&ticket, BranchScript::cooperative_zero(&ticket));
        waiters.push(coordinator.request(ticket).expect("accepted cleanup"));
    }
    let reports = runtime().block_on(async {
        let mut reports = Vec::with_capacity(waiters.len());
        for waiter in waiters {
            reports.push(
                tokio::time::timeout(Duration::from_secs(2), waiter.wait())
                    .await
                    .expect("stuck persistence must remain bounded"),
            );
        }
        reports
    });
    store.release();

    assert_eq!(reports.len(), REQUESTS as usize);
    assert!(reports
        .iter()
        .all(|report| report.outcome() == TeardownOutcome::CleanupFailed));
    assert!(
        store.max_active() <= 4,
        "completion persistence may poison fixed slots but must not create one worker per request"
    );
}

#[test]
fn teardown_completion_store_retention_is_bounded() {
    const REQUESTS: u16 = 300;
    let admission = FakeAdmission::default();
    let effects = FakeEffects::default();
    let store = FakeCompletionStore::default();
    let coordinator = TeardownCoordinator::with_configuration(
        Arc::new(admission.clone()),
        Arc::new(effects.clone()),
        Arc::new(FakeClock::default()),
        4,
        TeardownBudgets::default(),
        store.store(),
    );
    let runtime = runtime();

    for index in 1..=REQUESTS {
        let ticket = indexed_ticket(index);
        admission.allow(&ticket);
        effects.install(&ticket, BranchScript::cooperative_zero(&ticket));
        let _ = runtime.block_on(
            coordinator
                .request(ticket)
                .expect("bounded store retention request")
                .wait(),
        );
    }

    assert!(
        store.inner.retained_count_for_test() <= 256,
        "completion store must retain a bounded exact-retry journal"
    );
}

#[test]
fn teardown_atomic_batches_above_queue_capacity_fail_closed() {
    for count in 257..=260u16 {
        let admission = FakeAdmission::default();
        let effects = FakeEffects::default();
        let mut tickets = Vec::with_capacity(count as usize);
        for index in 1..=count {
            let ticket = indexed_ticket(index);
            admission.allow(&ticket);
            effects.install(&ticket, BranchScript::cooperative_zero(&ticket));
            tickets.push(ticket);
        }
        let clock = FakeClock::default();
        let coordinator = coordinator_with_limit_and_budgets(
            &admission,
            &effects,
            &clock,
            4,
            TeardownBudgets::default(),
        );

        match coordinator.request_batch(tickets) {
            Err(devmanager::process::teardown::TeardownReject::CompletionJournalFull) => {}
            Err(error) => panic!("unexpected oversized batch rejection: {error:?}"),
            Ok(batch) => {
                let reports = runtime().block_on(batch.wait());
                panic!(
                    "atomic batch of {count} unexpectedly admitted {} cleanups",
                    reports.len()
                );
            }
        }
        assert!(
            admission.events().is_empty(),
            "oversized atomic batch {count} must not close admission"
        );
        assert!(effects.events().is_empty());
    }
}

#[test]
fn teardown_257th_distinct_cleanup_starts_and_settles_after_256_completions() {
    let admission = FakeAdmission::default();
    let effects = FakeEffects::default();
    let clock = FakeClock::default();
    let coordinator = coordinator_with_limit(&admission, &effects, &clock, 256);

    let mut reports = Vec::with_capacity(257);
    for index in 1..=257u16 {
        let ticket = indexed_ticket(index);
        admission.allow(&ticket);
        effects.install(&ticket, BranchScript::cooperative_zero(&ticket));
        reports.push(
            runtime().block_on(
                coordinator
                    .request(ticket)
                    .expect("every distinct cleanup remains admissible")
                    .wait(),
            ),
        );
    }

    assert_eq!(reports.len(), 257);
    assert!(reports
        .iter()
        .all(|report| report.outcome() == TeardownOutcome::Closed));
}

#[test]
fn teardown_fixed_executor_handles_concurrent_pressure_and_exact_replay() {
    const REQUESTS: usize = 300;
    const MAX_ADMITTED_WHILE_WORKERS_BLOCK: usize = 260;
    let admission = FakeAdmission::default();
    let effects = FakeEffects::default();
    let mut tickets = Vec::with_capacity(REQUESTS);
    for index in 1..=REQUESTS as u16 {
        let ticket = indexed_ticket(index);
        admission.allow(&ticket);
        effects.install(&ticket, BranchScript::cooperative_zero(&ticket));
        tickets.push(ticket);
    }
    let release_drain = effects.install_drain_gate();
    let clock = FakeClock::default();
    let coordinator = Arc::new(coordinator_with_limit_and_budgets(
        &admission,
        &effects,
        &clock,
        4,
        TeardownBudgets::new(
            Duration::from_secs(30),
            Duration::from_secs(30),
            Duration::from_secs(30),
        ),
    ));
    let returned = Arc::new(AtomicUsize::new(0));
    let (sender, receiver) = std::sync::mpsc::channel();
    let mut handles = Vec::with_capacity(REQUESTS);

    for (index, ticket) in tickets.iter().cloned().enumerate() {
        let coordinator = Arc::clone(&coordinator);
        let returned = Arc::clone(&returned);
        let sender = sender.clone();
        handles.push(std::thread::spawn(move || {
            let result = coordinator.request(ticket);
            returned.fetch_add(1, Ordering::SeqCst);
            sender
                .send((index, result))
                .expect("pressure result receiver");
        }));
    }
    drop(sender);

    let observation_deadline = Instant::now() + Duration::from_secs(2);
    while returned.load(Ordering::SeqCst) < REQUESTS && Instant::now() < observation_deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        returned.load(Ordering::SeqCst) <= MAX_ADMITTED_WHILE_WORKERS_BLOCK,
        "bounded executor admitted unbounded pending cleanup requests"
    );

    release_drain.send_replace(true);
    for handle in handles {
        handle.join().expect("pressure request thread");
    }

    let mut waiters = Vec::with_capacity(REQUESTS);
    for _ in 0..REQUESTS {
        let (_, result) = receiver.recv().expect("pressure result");
        waiters.push(result.expect("every exact request eventually admits"));
    }
    let runtime = runtime();
    let reports = runtime.block_on(async {
        let mut reports = Vec::with_capacity(waiters.len());
        for waiter in waiters {
            reports.push(waiter.wait().await);
        }
        reports
    });
    assert_eq!(reports.len(), REQUESTS);
    assert!(reports
        .iter()
        .all(|report| report.outcome() == TeardownOutcome::Closed));

    let replayed = runtime.block_on(async {
        let mut reports = Vec::with_capacity(tickets.len());
        for ticket in tickets {
            reports.push(
                coordinator
                    .request(ticket)
                    .expect("exact replay remains idempotent")
                    .wait()
                    .await,
            );
        }
        reports
    });
    assert!(replayed
        .iter()
        .all(|report| report.outcome() == TeardownOutcome::Closed));
    assert_eq!(coordinator.active_operation_count(), 0);
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
fn teardown_batch_admission_failure_starts_no_branch() {
    let admitted = ticket(27, 47, TeardownScope::Host, 28, 127, 1_027);
    let rejected = ticket(28, 48, TeardownScope::Host, 28, 128, 1_028);
    let admission = FakeAdmission::default();
    admission.allow(&admitted);
    let effects = FakeEffects::default();
    effects.install(&admitted, BranchScript::cooperative_zero(&admitted));
    let clock = FakeClock::default();
    let coordinator = coordinator(&admission, &effects, &clock);

    let result = runtime().block_on(async {
        let result = coordinator.request_batch(vec![admitted, rejected]);
        tokio::task::yield_now().await;
        result
    });

    assert!(
        result.is_err(),
        "the full-scope admission must fail atomically"
    );
    assert_eq!(
        effects.drain_started_count(),
        0,
        "no branch may drain before every admission is secured"
    );
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
    let job = ExactJob::with_root(ticket.fence().root().clone());
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
    let job = ExactJob::with_root(ticket.fence().root().clone());
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
    assert!(
        registry.apply_job_observation(JobCompletionObservation::new(
            fence.clone(),
            JobCompletionEvent::ActiveProcessZero,
        ))
    );
    assert_eq!(
        registry
            .current(ticket.resource_id())
            .expect("untrusted completion retains process")
            .state(),
        ManagedProcessState::Starting
    );
    assert!(matches!(
        registry.active_process_zero_proof_exact(&fence),
        Err(devmanager::process::registry::ProcessRegistryError::ActiveProcessZeroUnproved { .. })
    ));
    assert!(registry.release_stopped_exact(&fence).is_err());
}

#[cfg(windows)]
#[path = "teardown_windows.rs"]
mod teardown_windows;
