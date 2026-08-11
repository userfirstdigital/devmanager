use crate::browser::BrowserAttachmentSessionBinding;
use crate::process::registry::ManagedProcessFence;
use crate::remote::RemoteActionResult;
use crate::services::process_manager::{ManagedShutdownReport, ProcessManagerInner};
use crate::state::{AiLaunchSpec, ServerLaunchSpec, SessionDimensions, SshLaunchSpec};
use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TrySendError};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    Arc, Mutex, Weak,
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

static NEXT_OP_ID: AtomicU64 = AtomicU64::new(1);
pub(crate) const MAX_PROCESS_OP_BATCH_ITEMS: usize = 256;
const MAX_PROCESS_OP_COMPLETIONS_PER_DRAIN: usize = 256;
const MAX_PENDING_PROCESS_OPS: usize = 256;
const PROCESS_OP_WORKER_JOIN_BUDGET: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessOpKind {
    StartServer,
    StopServer,
    RestartServer,
    KillPortAndRestart,
    StartSsh,
    RestartSsh,
    CloseSsh,
    SpawnAi,
    RestartAi,
    CloseAi,
    StopAll,
    Shutdown,
    KillProcess,
    KillProcessTree,
}

#[derive(Debug, Clone)]
pub struct ProcessOpContext {
    pub message: Option<String>,
    pub session_id: Option<String>,
    pub port: Option<u16>,
    pub focus: bool,
    pub shutdown_report: Option<ManagedShutdownReport>,
}

impl Default for ProcessOpContext {
    fn default() -> Self {
        Self {
            message: None,
            session_id: None,
            port: None,
            focus: false,
            shutdown_report: None,
        }
    }
}

#[derive(Debug)]
pub struct ProcessOpCompletion {
    pub op_id: u64,
    pub kind: ProcessOpKind,
    pub target_id: String,
    pub result: Result<(), String>,
    pub context: ProcessOpContext,
    pub remote_response: Option<Sender<RemoteActionResult>>,
}

#[derive(Debug)]
pub enum ProcessOp {
    StartServer {
        op_id: u64,
        launch: ServerLaunchSpec,
        dimensions: SessionDimensions,
        activate: bool,
        response: Option<Sender<RemoteActionResult>>,
    },
    StopServer {
        op_id: u64,
        command_id: String,
        wait: Duration,
        response: Option<Sender<RemoteActionResult>>,
    },
    RestartServer {
        op_id: u64,
        launch: ServerLaunchSpec,
        dimensions: SessionDimensions,
        banner: String,
        clear_logs: bool,
        response: Option<Sender<RemoteActionResult>>,
    },
    KillPortAndRestart {
        op_id: u64,
        command_id: String,
        port: u16,
        launch: ServerLaunchSpec,
        dimensions: SessionDimensions,
        banner: String,
        response: Option<Sender<RemoteActionResult>>,
    },
    StartSsh {
        op_id: u64,
        launch: SshLaunchSpec,
        session_id: String,
        dimensions: SessionDimensions,
        key_warning: Option<String>,
        response: Option<Sender<RemoteActionResult>>,
    },
    RestartSsh {
        op_id: u64,
        close_session_id: Option<String>,
        launch: SshLaunchSpec,
        session_id: String,
        dimensions: SessionDimensions,
        key_warning: Option<String>,
        response: Option<Sender<RemoteActionResult>>,
    },
    CloseSsh {
        op_id: u64,
        session_id: Option<String>,
        response: Option<Sender<RemoteActionResult>>,
    },
    SpawnAi {
        op_id: u64,
        launch: AiLaunchSpec,
        session_id: String,
        dimensions: SessionDimensions,
        attachment_binding: Option<BrowserAttachmentSessionBinding>,
        response: Option<Sender<RemoteActionResult>>,
    },
    RestartAi {
        op_id: u64,
        close_session_id: Option<String>,
        launch: AiLaunchSpec,
        session_id: String,
        dimensions: SessionDimensions,
        attachment_binding: Option<BrowserAttachmentSessionBinding>,
        response: Option<Sender<RemoteActionResult>>,
    },
    CloseAi {
        op_id: u64,
        session_id: String,
        response: Option<Sender<RemoteActionResult>>,
    },
    StopAll {
        op_id: u64,
        command_ids: Vec<String>,
        wait: Duration,
        response: Option<Sender<RemoteActionResult>>,
    },
    Shutdown {
        op_id: u64,
        timeout: Duration,
    },
    KillProcess {
        op_id: u64,
        session_id: String,
        pid: u32,
        /// PID is a diagnostic row selector only; ownership is the exact
        /// Job/registry fence captured with the monitor snapshot.
        fence: ManagedProcessFence,
        response: Option<Sender<RemoteActionResult>>,
    },
    KillProcessTree {
        op_id: u64,
        session_id: String,
        pid: u32,
        /// PID is a diagnostic row selector only; ownership is the exact
        /// Job/registry fence captured with the monitor snapshot.
        fence: ManagedProcessFence,
        response: Option<Sender<RemoteActionResult>>,
    },
}

fn op_preempts_in_flight(op: &ProcessOp) -> bool {
    matches!(
        op,
        ProcessOp::StopServer { .. }
            | ProcessOp::RestartServer { .. }
            | ProcessOp::KillPortAndRestart { .. }
            | ProcessOp::RestartSsh { .. }
            | ProcessOp::CloseSsh { .. }
            | ProcessOp::RestartAi { .. }
            | ProcessOp::CloseAi { .. }
            | ProcessOp::Shutdown { .. }
    )
}

fn validate_process_op_bounds(op: &ProcessOp) -> Result<(), String> {
    if matches!(
        op,
        ProcessOp::StopAll { command_ids, .. }
            if command_ids.len() > MAX_PROCESS_OP_BATCH_ITEMS
    ) {
        return Err(format!(
            "Process operation batch exceeds {MAX_PROCESS_OP_BATCH_ITEMS} entries."
        ));
    }
    Ok(())
}

fn drain_ready_completions(
    rx: &Receiver<ProcessOpCompletion>,
    in_flight: &Mutex<HashMap<String, u64>>,
    pending_ops: &AtomicUsize,
) -> Vec<ProcessOpCompletion> {
    let mut completions = Vec::with_capacity(MAX_PROCESS_OP_COMPLETIONS_PER_DRAIN);
    while completions.len() < MAX_PROCESS_OP_COMPLETIONS_PER_DRAIN {
        let Ok(completion) = rx.try_recv() else {
            break;
        };
        if let Ok(mut in_flight) = in_flight.lock() {
            in_flight.remove(&completion.target_id);
        }
        pending_ops.fetch_sub(1, Ordering::AcqRel);
        completions.push(completion);
    }
    completions
}

impl ProcessOp {
    pub fn op_id(&self) -> u64 {
        match self {
            ProcessOp::StartServer { op_id, .. }
            | ProcessOp::StopServer { op_id, .. }
            | ProcessOp::RestartServer { op_id, .. }
            | ProcessOp::KillPortAndRestart { op_id, .. }
            | ProcessOp::StartSsh { op_id, .. }
            | ProcessOp::RestartSsh { op_id, .. }
            | ProcessOp::CloseSsh { op_id, .. }
            | ProcessOp::SpawnAi { op_id, .. }
            | ProcessOp::RestartAi { op_id, .. }
            | ProcessOp::CloseAi { op_id, .. }
            | ProcessOp::StopAll { op_id, .. }
            | ProcessOp::Shutdown { op_id, .. }
            | ProcessOp::KillProcess { op_id, .. }
            | ProcessOp::KillProcessTree { op_id, .. } => *op_id,
        }
    }

    pub fn target_id(&self) -> String {
        match self {
            ProcessOp::StartServer { launch, .. } | ProcessOp::RestartServer { launch, .. } => {
                launch.command_id.clone()
            }
            ProcessOp::KillPortAndRestart { command_id, .. } => command_id.clone(),
            ProcessOp::StopServer { command_id, .. } => command_id.clone(),
            ProcessOp::StartSsh { session_id, .. }
            | ProcessOp::RestartSsh { session_id, .. }
            | ProcessOp::SpawnAi { session_id, .. }
            | ProcessOp::RestartAi { session_id, .. }
            | ProcessOp::CloseAi { session_id, .. } => session_id.clone(),
            ProcessOp::CloseSsh { session_id, .. } => {
                session_id.clone().unwrap_or_else(|| "ssh".to_string())
            }
            ProcessOp::StopAll { .. } => "stop-all".to_string(),
            ProcessOp::Shutdown { .. } => "shutdown".to_string(),
            ProcessOp::KillProcess {
                session_id, pid, ..
            }
            | ProcessOp::KillProcessTree {
                session_id, pid, ..
            } => format!("kill:{session_id}:{pid}"),
        }
    }

    pub fn into_failure_completion(self, error: String) -> ProcessOpCompletion {
        let op_id = self.op_id();
        let target_id = self.target_id();
        let (kind, remote_response) = match self {
            ProcessOp::StartServer { response, .. } => (ProcessOpKind::StartServer, response),
            ProcessOp::StopServer { response, .. } => (ProcessOpKind::StopServer, response),
            ProcessOp::RestartServer { response, .. } => (ProcessOpKind::RestartServer, response),
            ProcessOp::KillPortAndRestart { response, .. } => {
                (ProcessOpKind::KillPortAndRestart, response)
            }
            ProcessOp::StartSsh { response, .. } => (ProcessOpKind::StartSsh, response),
            ProcessOp::RestartSsh { response, .. } => (ProcessOpKind::RestartSsh, response),
            ProcessOp::CloseSsh { response, .. } => (ProcessOpKind::CloseSsh, response),
            ProcessOp::SpawnAi { response, .. } => (ProcessOpKind::SpawnAi, response),
            ProcessOp::RestartAi { response, .. } => (ProcessOpKind::RestartAi, response),
            ProcessOp::CloseAi { response, .. } => (ProcessOpKind::CloseAi, response),
            ProcessOp::StopAll { response, .. } => (ProcessOpKind::StopAll, response),
            ProcessOp::Shutdown { .. } => (ProcessOpKind::Shutdown, None),
            ProcessOp::KillProcess { response, .. } => (ProcessOpKind::KillProcess, response),
            ProcessOp::KillProcessTree { response, .. } => {
                (ProcessOpKind::KillProcessTree, response)
            }
        };
        ProcessOpCompletion {
            op_id,
            kind,
            target_id,
            result: Err(error),
            context: ProcessOpContext::default(),
            remote_response,
        }
    }
}

pub fn next_op_id() -> u64 {
    NEXT_OP_ID.fetch_add(1, Ordering::Relaxed)
}

pub struct ProcessOpQueue {
    submit_tx: SyncSender<ProcessOp>,
    completion_rx: Mutex<Receiver<ProcessOpCompletion>>,
    stop: Arc<AtomicBool>,
    worker: Mutex<Option<JoinHandle<()>>>,
    in_flight: Arc<Mutex<HashMap<String, u64>>>,
    admission_serial: Mutex<()>,
    closing: AtomicBool,
    /// Includes queued, executing, and completed-but-undrained operations.
    /// Keeping one shared count proves both channels remain within fixed
    /// allocation bounds without blocking the worker on completion delivery.
    pending_ops: AtomicUsize,
    #[cfg(test)]
    successful_submissions: AtomicU64,
    #[cfg(test)]
    completed_operations: AtomicU64,
}

impl ProcessOpQueue {
    pub fn new(inner: Weak<ProcessManagerInner>) -> Arc<Self> {
        let (submit_tx, submit_rx) = mpsc::sync_channel(MAX_PENDING_PROCESS_OPS);
        let (completion_tx, completion_rx) = mpsc::sync_channel(MAX_PENDING_PROCESS_OPS);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_worker = stop.clone();
        let in_flight: Arc<Mutex<HashMap<String, u64>>> = Arc::new(Mutex::new(HashMap::new()));
        let in_flight_worker = in_flight.clone();

        Arc::new_cyclic(move |queue: &Weak<ProcessOpQueue>| {
            let queue = queue.clone();
            let worker = thread::Builder::new()
                .name("process-op-worker".into())
                .spawn(move || {
                    run_worker_loop(
                        inner,
                        queue,
                        submit_rx,
                        completion_tx,
                        stop_worker,
                        in_flight_worker,
                    );
                })
                .expect("spawn process-op worker");

            Self {
                submit_tx,
                completion_rx: Mutex::new(completion_rx),
                stop,
                worker: Mutex::new(Some(worker)),
                in_flight,
                admission_serial: Mutex::new(()),
                closing: AtomicBool::new(false),
                pending_ops: AtomicUsize::new(0),
                #[cfg(test)]
                successful_submissions: AtomicU64::new(0),
                #[cfg(test)]
                completed_operations: AtomicU64::new(0),
            }
        })
    }

    pub fn submit(&self, op: ProcessOp) -> Result<u64, String> {
        let _admission = self
            .admission_serial
            .lock()
            .map_err(|_| "Process operation admission is unavailable.".to_string())?;
        if self.closing.load(Ordering::Acquire) {
            return Err("Process operation queue is shutting down.".to_string());
        }
        validate_process_op_bounds(&op)?;
        if self.pending_ops.load(Ordering::Acquire) >= MAX_PENDING_PROCESS_OPS {
            return Err(format!(
                "Process operation capacity is full ({MAX_PENDING_PROCESS_OPS} pending operations)."
            ));
        }
        let begins_shutdown = matches!(op, ProcessOp::Shutdown { .. });
        if begins_shutdown {
            // Publish the close fence while holding the same gate as every
            // producer. No operation can be admitted behind the shutdown op.
            self.closing.store(true, Ordering::Release);
        }
        let op_id = op.op_id();
        let target_id = op.target_id();
        let tracks_in_flight =
            !matches!(op, ProcessOp::Shutdown { .. } | ProcessOp::StopAll { .. });
        if tracks_in_flight {
            if let Ok(mut in_flight) = self.in_flight.lock() {
                if op_preempts_in_flight(&op) {
                    in_flight.remove(&target_id);
                } else if in_flight.contains_key(&target_id) {
                    return Err(format!("Operation already in progress for `{target_id}`."));
                }
                in_flight.insert(target_id.clone(), op_id);
            }
        }
        self.pending_ops.fetch_add(1, Ordering::AcqRel);
        if let Err(send_error) = self.submit_tx.try_send(op) {
            self.pending_ops.fetch_sub(1, Ordering::AcqRel);
            if tracks_in_flight {
                if let Ok(mut in_flight) = self.in_flight.lock() {
                    if in_flight.get(&target_id) == Some(&op_id) {
                        in_flight.remove(&target_id);
                    }
                }
            }
            if begins_shutdown {
                self.closing.store(false, Ordering::Release);
            }
            return Err(match send_error {
                TrySendError::Full(_) => format!(
                    "Process operation capacity is full ({MAX_PENDING_PROCESS_OPS} pending operations)."
                ),
                TrySendError::Disconnected(_) => {
                    "Process operation queue is unavailable.".to_string()
                }
            });
        }
        #[cfg(test)]
        self.successful_submissions.fetch_add(1, Ordering::SeqCst);
        Ok(op_id)
    }

    #[cfg(test)]
    pub(crate) fn successful_submissions_for_test(&self) -> u64 {
        self.successful_submissions.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn completed_operations_for_test(&self) -> u64 {
        self.completed_operations.load(Ordering::SeqCst)
    }

    pub fn drain_completions(&self) -> Vec<ProcessOpCompletion> {
        let Ok(rx) = self.completion_rx.lock() else {
            return Vec::new();
        };
        drain_ready_completions(&rx, &self.in_flight, &self.pending_ops)
    }

    pub fn shutdown(&self) {
        let _admission = self
            .admission_serial
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.closing.store(true, Ordering::Release);
        self.stop.store(true, Ordering::SeqCst);
        if let Ok(mut worker) = self.worker.lock() {
            if let Some(handle) = worker.take() {
                if handle.thread().id() == thread::current().id() {
                    // Detaching the queue actor would permit effects after the
                    // shutdown boundary. This state is an invariant failure:
                    // queue shutdown is host-owned and never worker-owned.
                    std::process::abort();
                } else {
                    let deadline = std::time::Instant::now()
                        .checked_add(PROCESS_OP_WORKER_JOIN_BUDGET)
                        .unwrap_or_else(std::time::Instant::now);
                    while !handle.is_finished() && std::time::Instant::now() < deadline {
                        thread::yield_now();
                    }
                    if !handle.is_finished() {
                        // Returning would detach an operation actor that can
                        // still mutate process state after host shutdown.
                        std::process::abort();
                    }
                    let _ = handle.join();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completion(index: usize) -> ProcessOpCompletion {
        ProcessOpCompletion {
            op_id: index as u64,
            kind: ProcessOpKind::CloseAi,
            target_id: format!("session-{index}"),
            result: Ok(()),
            context: ProcessOpContext::default(),
            remote_response: None,
        }
    }

    #[test]
    fn process_operation_batch_and_completion_drain_are_hard_bounded() {
        let oversized = ProcessOp::StopAll {
            op_id: next_op_id(),
            command_ids: (0..=MAX_PROCESS_OP_BATCH_ITEMS)
                .map(|index| format!("server-{index}"))
                .collect(),
            wait: Duration::ZERO,
            response: None,
        };
        assert!(matches!(
            validate_process_op_bounds(&oversized),
            Err(detail) if detail.contains("exceeds 256 entries")
        ));

        let (tx, rx) = mpsc::channel();
        for index in 0..=MAX_PROCESS_OP_COMPLETIONS_PER_DRAIN {
            tx.send(completion(index)).expect("seed completion");
        }
        let in_flight = Mutex::new(HashMap::new());
        let pending = AtomicUsize::new(MAX_PROCESS_OP_COMPLETIONS_PER_DRAIN + 1);
        let first = drain_ready_completions(&rx, &in_flight, &pending);
        assert_eq!(first.len(), MAX_PROCESS_OP_COMPLETIONS_PER_DRAIN);
        let second = drain_ready_completions(&rx, &in_flight, &pending);
        assert_eq!(second.len(), 1, "the next poll retains overflow work");
        assert_eq!(pending.load(Ordering::Acquire), 0);
    }
}

impl Drop for ProcessOpQueue {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run_worker_loop(
    inner: Weak<ProcessManagerInner>,
    queue: Weak<ProcessOpQueue>,
    submit_rx: Receiver<ProcessOp>,
    completion_tx: SyncSender<ProcessOpCompletion>,
    stop: Arc<AtomicBool>,
    in_flight: Arc<Mutex<HashMap<String, u64>>>,
) {
    while !stop.load(Ordering::SeqCst) {
        match submit_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(op) => {
                let Some(_queue_lease) = queue.upgrade() else {
                    break;
                };
                let Some(inner) = inner.upgrade() else {
                    break;
                };
                let completion = execute_process_op(&inner, op);
                #[cfg(test)]
                _queue_lease
                    .completed_operations
                    .fetch_add(1, Ordering::SeqCst);
                if let Ok(mut map) = in_flight.lock() {
                    map.remove(&completion.target_id);
                }
                let _ = completion_tx.send(completion);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn execute_process_op(inner: &Arc<ProcessManagerInner>, op: ProcessOp) -> ProcessOpCompletion {
    crate::services::process_manager::execute_process_op_inner(inner, op)
}
