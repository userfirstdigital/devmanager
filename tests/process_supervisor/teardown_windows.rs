//! Real Windows Job Object to teardown-coordinator boundary proof.

use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use devmanager::domain::id::{OperationId, ResourceId};
use devmanager::domain::operation::ResourceFence;
use devmanager::process::identity::ProcessOwner;
use devmanager::process::job::{attach_process_to_managed_job, ManagedProcessJob};
use devmanager::process::registry::{
    JobMembership, ManagedProcessFence, ProcessDisplayLabel, ProcessRegistry, RegisteredProcess,
    UnregisterOutcome,
};
use devmanager::process::teardown::{
    AdmissionReceipt, AdmissionState, BoxFuture, ResidueEvidence, StageResult, TeardownAdmission,
    TeardownAdmissionError, TeardownBudgets, TeardownClock, TeardownCompletionKey,
    TeardownCompletionStore, TeardownCoordinator, TeardownDeadline, TeardownEffects,
    TeardownReleaseAuthority, TeardownReport, TeardownScope, TeardownTicket, WaitResult, WaitStage,
};

fn resource_id() -> ResourceId {
    ResourceId::from_bytes([
        0x01, 0x9a, 0x11, 0x22, 0x33, 0x44, 0x70, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x71,
    ])
    .expect("resource id")
}

fn operation_id() -> OperationId {
    OperationId::from_bytes([
        0x01, 0x9a, 0x11, 0x22, 0x33, 0x44, 0x70, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x72,
    ])
    .expect("operation id")
}

#[derive(Debug)]
struct ChildGuard(Option<Child>);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

type SharedJob = Arc<Mutex<Option<ManagedProcessJob>>>;

#[derive(Debug, Clone)]
struct RegistryJob(SharedJob);

impl JobMembership for RegistryJob {
    fn active_process_ids(&self) -> Result<Vec<u32>, String> {
        self.0
            .lock()
            .expect("Windows Job lock")
            .as_ref()
            .ok_or_else(|| "managed Job handle was released too early".to_string())?
            .active_process_ids()
    }

    fn terminate_tree(&self) -> Result<(), String> {
        self.0
            .lock()
            .expect("Windows Job lock")
            .as_ref()
            .ok_or_else(|| "managed Job handle was released before termination".to_string())?
            .terminate_tree()
    }

    fn inspect_process(
        &self,
        pid: u32,
    ) -> Result<devmanager::process::registry::JobMemberInfo, String> {
        self.0
            .lock()
            .expect("Windows Job lock")
            .as_ref()
            .ok_or_else(|| "managed Job handle was released before inspection".to_string())?
            .inspect_process(pid)
    }

    fn bind_completion_fence(&mut self, fence: ManagedProcessFence) -> Result<(), String> {
        self.0
            .lock()
            .expect("Windows Job lock")
            .as_mut()
            .ok_or_else(|| "managed Job handle was released before fence binding".to_string())?
            .bind_completion_fence(fence)
    }
}

#[derive(Debug, Clone)]
struct WindowsAdmission {
    fence: ManagedProcessFence,
}

impl TeardownAdmission for WindowsAdmission {
    fn close_admission(
        &self,
        ticket: &TeardownTicket,
    ) -> Result<AdmissionReceipt, TeardownAdmissionError> {
        if ticket.fence() != &self.fence {
            return Err(TeardownAdmissionError::FenceMismatch);
        }
        Ok(AdmissionReceipt::new(
            ticket.scope(),
            AdmissionState::Closing,
            ticket.action_epoch(),
            self.fence.clone(),
        ))
    }

    fn close_admission_batch(
        &self,
        tickets: &[TeardownTicket],
    ) -> Result<Vec<AdmissionReceipt>, TeardownAdmissionError> {
        if tickets.iter().any(|ticket| ticket.fence() != &self.fence) {
            return Err(TeardownAdmissionError::FenceMismatch);
        }
        Ok(tickets
            .iter()
            .map(|ticket| {
                AdmissionReceipt::new(
                    ticket.scope(),
                    AdmissionState::Closing,
                    ticket.action_epoch(),
                    self.fence.clone(),
                )
            })
            .collect())
    }
}

#[derive(Debug, Default)]
struct WindowsClock;

impl TeardownClock for WindowsClock {
    fn deadline(&self, _timeout: Duration) -> TeardownDeadline {
        TeardownDeadline::new(1)
    }
}

#[derive(Debug, Default, Clone)]
struct WindowsStore {
    report: Arc<Mutex<Option<(TeardownCompletionKey, TeardownReport)>>>,
}

impl TeardownCompletionStore for WindowsStore {
    fn lookup(&self, key: &TeardownCompletionKey) -> Result<Option<TeardownReport>, String> {
        Ok(self
            .report
            .lock()
            .expect("Windows completion store")
            .as_ref()
            .filter(|(stored_key, _)| stored_key == key)
            .map(|(_, report)| report.clone()))
    }

    fn persist<'a>(
        &'a self,
        key: &'a TeardownCompletionKey,
        report: &'a TeardownReport,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            *self.report.lock().expect("Windows completion store") =
                Some((key.clone(), report.clone()));
            Ok(())
        })
    }
}

#[derive(Debug, Clone)]
struct WindowsEffects {
    registry: Arc<Mutex<ProcessRegistry<RegistryJob>>>,
    shared_job: SharedJob,
    release_authority: Arc<Mutex<Option<TeardownReleaseAuthority>>>,
}

impl TeardownEffects for WindowsEffects {
    fn drain<'a>(&'a self, _ticket: &'a TeardownTicket) -> BoxFuture<'a, StageResult> {
        Box::pin(async { StageResult::Completed })
    }

    fn cooperative_close<'a>(&'a self, _ticket: &'a TeardownTicket) -> BoxFuture<'a, StageResult> {
        Box::pin(async { StageResult::Completed })
    }

    fn interrupt_or_safe_close<'a>(
        &'a self,
        _ticket: &'a TeardownTicket,
    ) -> BoxFuture<'a, StageResult> {
        Box::pin(async { StageResult::Completed })
    }

    fn terminate_tree<'a>(&'a self, _ticket: &'a TeardownTicket) -> BoxFuture<'a, StageResult> {
        let shared_job = Arc::clone(&self.shared_job);
        Box::pin(async move {
            match shared_job
                .lock()
                .expect("Windows Job lock")
                .as_ref()
                .ok_or_else(|| "Job handle released before TerminateJobObject".to_string())
                .and_then(ManagedProcessJob::terminate_tree)
            {
                Ok(()) => StageResult::Completed,
                Err(detail) => StageResult::Failed { detail },
            }
        })
    }

    fn wait_for_zero<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
        stage: WaitStage,
        _deadline: TeardownDeadline,
    ) -> BoxFuture<'a, WaitResult> {
        if stage != WaitStage::Termination {
            return Box::pin(async { WaitResult::TimedOut });
        }
        let registry = Arc::clone(&self.registry);
        let shared_job = Arc::clone(&self.shared_job);
        Box::pin(async move {
            for _ in 0..500 {
                let active = shared_job
                    .lock()
                    .expect("Windows Job lock")
                    .as_ref()
                    .and_then(|job| job.active_process_ids().ok())
                    .unwrap_or_default();
                let has_receiver_zero = {
                    let messages = shared_job
                        .lock()
                        .expect("Windows Job lock")
                        .as_ref()
                        .map(ManagedProcessJob::drain_completion_messages)
                        .unwrap_or_default();
                    let mut registry = registry.lock().expect("Windows registry");
                    for message in messages {
                        registry.apply_job_completion(message);
                    }
                    registry
                        .active_process_zero_proof_exact(ticket.fence())
                        .is_ok()
                };
                if active.is_empty() && has_receiver_zero {
                    return WaitResult::Zero;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            WaitResult::TimedOut
        })
    }

    fn settle_active_process_zero<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
    ) -> BoxFuture<'a, StageResult> {
        let registry = Arc::clone(&self.registry);
        let release_authority = Arc::clone(&self.release_authority);
        let fence = ticket.fence().clone();
        Box::pin(async move {
            let mut registry = registry.lock().expect("Windows registry");
            let proof = match registry.active_process_zero_proof_exact(&fence) {
                Ok(proof) => proof,
                Err(error) => {
                    return StageResult::Failed {
                        detail: error.to_string(),
                    };
                }
            };
            match registry.mint_teardown_release_authority_exact(ticket, proof) {
                Ok(authority) => {
                    *release_authority.lock().expect("Windows release authority") = Some(authority);
                    StageResult::Completed
                }
                Err(error) => StageResult::Failed {
                    detail: error.to_string(),
                },
            }
        })
    }

    fn detach_after_zero<'a>(&'a self, _ticket: &'a TeardownTicket) -> BoxFuture<'a, StageResult> {
        Box::pin(async { StageResult::Completed })
    }

    fn reconcile_ports<'a>(&'a self, _ticket: &'a TeardownTicket) -> BoxFuture<'a, StageResult> {
        Box::pin(async { StageResult::Completed })
    }

    fn persist_settlement<'a>(&'a self, _ticket: &'a TeardownTicket) -> BoxFuture<'a, StageResult> {
        Box::pin(async { StageResult::Completed })
    }

    fn residue<'a>(
        &'a self,
        _ticket: &'a TeardownTicket,
    ) -> BoxFuture<'a, Option<ResidueEvidence>> {
        Box::pin(async { None })
    }

    fn release_stopped_exact<'a>(
        &'a self,
        ticket: &'a TeardownTicket,
    ) -> BoxFuture<'a, StageResult> {
        let registry = Arc::clone(&self.registry);
        let shared_job = Arc::clone(&self.shared_job);
        let release_authority = Arc::clone(&self.release_authority);
        let ticket = ticket.clone();
        Box::pin(async move {
            let authority = release_authority
                .lock()
                .expect("Windows release authority")
                .take()
                .ok_or_else(|| "Windows teardown release authority was not settled".to_string());
            let authority = match authority {
                Ok(authority) => authority,
                Err(detail) => return StageResult::Failed { detail },
            };
            let result = registry
                .lock()
                .expect("Windows registry")
                .release_stopped_with_authority(&ticket, authority);
            match result {
                Ok(UnregisterOutcome::Removed(_)) => {
                    *shared_job.lock().expect("Windows Job lock") = None;
                    StageResult::Completed
                }
                Ok(UnregisterOutcome::Stale) => StageResult::Failed {
                    detail: "Windows registry release became stale".to_string(),
                },
                Err(error) => StageResult::Failed {
                    detail: error.to_string(),
                },
            }
        })
    }
}

#[test]
fn windows_managed_job_teardown_reaches_receiver_zero_before_registry_release() {
    let child = Command::new("cmd.exe")
        .args(["/C", "ping", "127.0.0.1", "-n", "30"])
        .spawn()
        .expect("spawn Windows Job boundary helper");
    let mut child_guard = ChildGuard(Some(child));
    let pid = child_guard.0.as_ref().expect("child guard").id();
    let job = attach_process_to_managed_job(pid)
        .expect("attach process to managed Job")
        .expect("Windows managed Job");
    let root = job
        .inspect_process(pid)
        .expect("inspect Job root")
        .identity()
        .clone();
    let shared_job = Arc::new(Mutex::new(Some(job)));
    let registry_job = RegistryJob(Arc::clone(&shared_job));
    let resource = ResourceFence::new(resource_id(), 1);
    let mut registry = ProcessRegistry::new();
    let fence = registry
        .register(RegisteredProcess::new(
            resource,
            ProcessOwner::Host,
            root,
            ProcessDisplayLabel::new("Windows teardown boundary").expect("label"),
            registry_job,
        ))
        .expect("register managed Job with receiver fence");
    let registry = Arc::new(Mutex::new(registry));
    let ticket = TeardownTicket::new(operation_id(), TeardownScope::Host, 1, fence.clone())
        .expect("host scope owns the Windows Job");
    let admission = WindowsAdmission { fence };
    let effects = WindowsEffects {
        registry: Arc::clone(&registry),
        shared_job: Arc::clone(&shared_job),
        release_authority: Arc::new(Mutex::new(None)),
    };
    let coordinator = TeardownCoordinator::with_configuration(
        Arc::new(admission),
        Arc::new(effects),
        Arc::new(WindowsClock),
        1,
        TeardownBudgets::new(
            Duration::from_millis(250),
            Duration::from_millis(250),
            Duration::from_secs(5),
        ),
        Arc::new(WindowsStore::default()),
    );

    let report = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("Windows boundary runtime")
        .block_on(coordinator.request(ticket).expect("admission").wait());

    assert_eq!(
        report.outcome(),
        devmanager::process::teardown::TeardownOutcome::Closed,
        "Windows teardown errors: {:?}; residue: {:?}",
        report.errors(),
        report.residue()
    );
    assert!(registry
        .lock()
        .expect("Windows registry")
        .current(resource_id())
        .is_none());
    assert!(shared_job.lock().expect("Windows Job lock").is_none());
    assert!(child_guard
        .0
        .as_mut()
        .expect("child guard")
        .try_wait()
        .expect("wait for terminated Windows helper")
        .is_some());
}
