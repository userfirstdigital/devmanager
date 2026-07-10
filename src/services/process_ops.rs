use crate::remote::RemoteActionResult;
use crate::services::process_manager::{ManagedShutdownReport, ProcessManagerInner};
use crate::state::{AiLaunchSpec, ServerLaunchSpec, SessionDimensions, SshLaunchSpec};
use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

static NEXT_OP_ID: AtomicU64 = AtomicU64::new(1);

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
        response: Option<Sender<RemoteActionResult>>,
    },
    RestartAi {
        op_id: u64,
        close_session_id: Option<String>,
        launch: AiLaunchSpec,
        session_id: String,
        dimensions: SessionDimensions,
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
        response: Option<Sender<RemoteActionResult>>,
    },
    KillProcessTree {
        op_id: u64,
        session_id: String,
        pid: u32,
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
            ProcessOp::CloseSsh { session_id, .. } => session_id
                .clone()
                .unwrap_or_else(|| "ssh".to_string()),
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
}

pub fn next_op_id() -> u64 {
    NEXT_OP_ID.fetch_add(1, Ordering::Relaxed)
}

pub struct ProcessOpQueue {
    submit_tx: Sender<ProcessOp>,
    completion_rx: Mutex<Receiver<ProcessOpCompletion>>,
    stop: Arc<AtomicBool>,
    worker: Mutex<Option<JoinHandle<()>>>,
    in_flight: Arc<Mutex<HashMap<String, u64>>>,
}

impl ProcessOpQueue {
    pub fn new(inner: Arc<ProcessManagerInner>) -> Self {
        let (submit_tx, submit_rx) = mpsc::channel();
        let (completion_tx, completion_rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_worker = stop.clone();
        let in_flight: Arc<Mutex<HashMap<String, u64>>> = Arc::new(Mutex::new(HashMap::new()));
        let in_flight_worker = in_flight.clone();

        let worker = thread::Builder::new()
            .name("process-op-worker".into())
            .spawn(move || {
                run_worker_loop(inner, submit_rx, completion_tx, stop_worker, in_flight_worker);
            })
            .expect("spawn process-op worker");

        Self {
            submit_tx,
            completion_rx: Mutex::new(completion_rx),
            stop,
            worker: Mutex::new(Some(worker)),
            in_flight,
        }
    }

    pub fn submit(&self, op: ProcessOp) -> Result<u64, String> {
        let op_id = op.op_id();
        let target_id = op.target_id();
        if !matches!(op, ProcessOp::Shutdown { .. } | ProcessOp::StopAll { .. }) {
            if let Ok(mut in_flight) = self.in_flight.lock() {
                if op_preempts_in_flight(&op) {
                    in_flight.remove(&target_id);
                } else if in_flight.contains_key(&target_id) {
                    return Err(format!(
                        "Operation already in progress for `{target_id}`."
                    ));
                }
                in_flight.insert(target_id, op_id);
            }
        }
        self.submit_tx
            .send(op)
            .map_err(|_| "Process operation queue is unavailable.".to_string())?;
        Ok(op_id)
    }

    pub fn drain_completions(&self) -> Vec<ProcessOpCompletion> {
        let mut completions = Vec::new();
        let Ok(rx) = self.completion_rx.lock() else {
            return completions;
        };
        while let Ok(completion) = rx.try_recv() {
            if let Ok(mut in_flight) = self.in_flight.lock() {
                in_flight.remove(&completion.target_id);
            }
            completions.push(completion);
        }
        completions
    }

    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Ok(mut worker) = self.worker.lock() {
            if let Some(handle) = worker.take() {
                let _ = handle.join();
            }
        }
    }
}

fn run_worker_loop(
    inner: Arc<ProcessManagerInner>,
    submit_rx: Receiver<ProcessOp>,
    completion_tx: Sender<ProcessOpCompletion>,
    stop: Arc<AtomicBool>,
    in_flight: Arc<Mutex<HashMap<String, u64>>>,
) {
    while !stop.load(Ordering::SeqCst) {
        match submit_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(op) => {
                let completion = execute_process_op(&inner, op);
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
