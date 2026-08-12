//! Production managed-launch authority for configured services.
//!
//! Uses the Phase 3 suspended PTY → Job register → resume handoff and exact
//! Job teardown. No raw PID kill path exists here. Service PTY output is
//! drained by a bounded reader into supervisor log projection; waiter exit
//! feeds `report_exit`.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
};

#[cfg(windows)]
use std::{
    ffi::OsString,
    io::{Read, Write},
    path::PathBuf,
    sync::atomic::AtomicBool,
    thread,
};

#[cfg(windows)]
use portable_pty::{native_pty_system, MasterPty, PtySize, SlavePty};

use crate::{
    domain::id::{OperationId, ResourceId},
    process::identity::ProcessOwner,
    process::teardown::TeardownCompletionStore,
    services::supervisor::{
        resolve_configured_service_program, ManagedLaunchAuthority, ManagedLaunchSpec,
        ManagedLaunchStage, SupervisorError,
    },
};

#[cfg(windows)]
use crate::{
    domain::{operation::ResourceFence, resource::ResourceKind},
    process::{
        launcher::{prepare_suspended_pty, PendingManagedLaunch},
        teardown::{
            ManagedTerminalActorHandles, ManagedTerminalIo, ManagedTerminalTeardown,
            TeardownOutcome,
        },
    },
};

#[cfg(not(windows))]
use crate::domain::{operation::ResourceFence, resource::ResourceKind};

const MAX_SERVICE_AUTHORITY_RESOURCES: usize = 256;
#[cfg(windows)]
const MAX_SERVICE_PTY_DRAIN_CHUNK: usize = 4_096;
#[cfg(windows)]
const MAX_SERVICE_LOG_LINE_BYTES: usize = 256;
#[cfg(windows)]
const MAX_SERVICE_OUTPUT_LINES: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct IssuedServiceCapability {
    owner: ProcessOwner,
    kind: ResourceKind,
    resource_id: ResourceId,
    generation: u64,
}

struct ServiceAuthorityState {
    next_action_epoch: u64,
    resources: BTreeMap<String, IssuedServiceCapability>,
    completion_store: Option<TeardownCompletionStore>,
}

/// Host-owned issuer shared with ProcessManager so service launches mint
/// generations through the same completion-store family as terminals.
pub struct ServiceLaunchIssuer {
    state: Mutex<ServiceAuthorityState>,
}

impl ServiceLaunchIssuer {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(ServiceAuthorityState {
                next_action_epoch: 1,
                resources: BTreeMap::new(),
                completion_store: None,
            }),
        }
    }

    /// Admit one exact owner/kind/resource/generation capability. Arbitrary
    /// mismatched specs are rejected; the first admit pins the capability.
    pub fn admit_capability(
        &self,
        session_id: &str,
        owner: ProcessOwner,
        kind: ResourceKind,
        resource_id: ResourceId,
        generation: u64,
    ) -> Result<(u64, TeardownCompletionStore, OperationId), String> {
        if session_id.trim().is_empty() || session_id.len() > 256 {
            return Err("service authority session identity is invalid".to_string());
        }
        if generation == 0 {
            return Err("service runtime generation must be greater than zero".to_string());
        }
        if !matches!(kind, ResourceKind::Service | ResourceKind::Terminal) {
            return Err("managed authority does not admit this resource kind".to_string());
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| "service authority issuer poisoned".to_string())?;
        if state.resources.len() >= MAX_SERVICE_AUTHORITY_RESOURCES
            && !state.resources.contains_key(session_id)
        {
            return Err("service authority retention is full".to_string());
        }
        match state.resources.get(session_id).copied() {
            Some(current) => {
                if current.owner != owner
                    || current.kind != kind
                    || current.resource_id != resource_id
                {
                    return Err("service launch capability mismatch".to_string());
                }
                if generation < current.generation {
                    return Err("service launch generation is stale".to_string());
                }
                state.resources.insert(
                    session_id.to_string(),
                    IssuedServiceCapability {
                        generation,
                        ..current
                    },
                );
            }
            None => {
                state.resources.insert(
                    session_id.to_string(),
                    IssuedServiceCapability {
                        owner,
                        kind,
                        resource_id,
                        generation,
                    },
                );
            }
        }
        let action_epoch = state.next_action_epoch;
        state.next_action_epoch = state
            .next_action_epoch
            .checked_add(1)
            .ok_or_else(|| "service action epoch space is exhausted".to_string())?;
        if state.completion_store.is_none() {
            #[cfg(windows)]
            {
                state.completion_store = Some(TeardownCompletionStore::for_terminal_host()?);
            }
            #[cfg(not(windows))]
            {
                state.completion_store = Some(TeardownCompletionStore::default());
            }
        }
        let completion_store = state
            .completion_store
            .clone()
            .ok_or_else(|| "service teardown completion store missing".to_string())?;
        Ok((action_epoch, completion_store, OperationId::new()))
    }
}

impl Default for ServiceLaunchIssuer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(windows)]
enum PendingStage {
    Prepared {
        pending: PendingManagedLaunch,
        operation_id: OperationId,
        action_epoch: u64,
        session_id: String,
        completion_store: TeardownCompletionStore,
        io: Arc<ManagedTerminalIo>,
        writer: Arc<Mutex<Box<dyn Write + Send>>>,
        actors: Arc<Mutex<ManagedTerminalActorHandles>>,
        master_slot: Arc<Mutex<Option<Box<dyn MasterPty + Send>>>>,
        output_lines: Arc<Mutex<VecDeque<String>>>,
        exit_code: Arc<Mutex<Option<Option<i32>>>>,
        slave: Box<dyn SlavePty + Send>,
        master: Box<dyn MasterPty + Send>,
        registered: bool,
    },
}

pub struct HostPendingLaunch {
    #[cfg(windows)]
    stage: PendingStage,
    #[cfg(not(windows))]
    _private: (),
}

pub struct HostLiveLaunch {
    #[cfg(windows)]
    teardown: Arc<ManagedTerminalTeardown>,
    #[cfg(windows)]
    fence: ResourceFence,
    #[cfg(windows)]
    output_lines: Arc<Mutex<VecDeque<String>>>,
    #[cfg(windows)]
    exit_code: Arc<Mutex<Option<Option<i32>>>>,
    #[cfg(windows)]
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    #[cfg(not(windows))]
    _private: (),
}

/// Production Phase 3 managed-launch authority for configured services.
pub struct HostManagedLaunchAuthority {
    issuer: Arc<ServiceLaunchIssuer>,
    live: BTreeMap<ResourceId, u64>,
}

impl HostManagedLaunchAuthority {
    pub fn new() -> Self {
        Self {
            issuer: Arc::new(ServiceLaunchIssuer::new()),
            live: BTreeMap::new(),
        }
    }

    pub(crate) fn with_issuer(issuer: Arc<ServiceLaunchIssuer>) -> Self {
        Self {
            issuer,
            live: BTreeMap::new(),
        }
    }

    pub(crate) fn issuer(&self) -> Arc<ServiceLaunchIssuer> {
        Arc::clone(&self.issuer)
    }
}

impl Default for HostManagedLaunchAuthority {
    fn default() -> Self {
        Self::new()
    }
}

impl ManagedLaunchAuthority for HostManagedLaunchAuthority {
    type Pending = HostPendingLaunch;
    type Live = HostLiveLaunch;

    fn prepare_suspended(
        &mut self,
        spec: &ManagedLaunchSpec,
    ) -> Result<Self::Pending, SupervisorError> {
        #[cfg(not(windows))]
        {
            let _ = spec;
            Err(SupervisorError::Launch {
                stage: ManagedLaunchStage::Prepare,
            })
        }
        #[cfg(windows)]
        {
            if spec.generation == 0
                || !matches!(spec.kind, ResourceKind::Service | ResourceKind::Terminal)
            {
                return Err(SupervisorError::Launch {
                    stage: ManagedLaunchStage::Prepare,
                });
            }
            let session_id = match spec.kind {
                ResourceKind::Service => format!("service:{}", spec.display_label),
                ResourceKind::Terminal => format!("terminal:{}", spec.resource_id),
                ResourceKind::BrowserContext => unreachable!("browser contexts are not launched"),
            };
            let (action_epoch, completion_store, operation_id) = self
                .issuer
                .admit_capability(
                    &session_id,
                    spec.owner,
                    spec.kind,
                    spec.resource_id,
                    spec.generation,
                )
                .map_err(|_| SupervisorError::Launch {
                    stage: ManagedLaunchStage::Prepare,
                })?;

            let resolved_program = resolve_configured_service_program(&spec.program)?;
            let pty_system = native_pty_system();
            let pair = pty_system
                .openpty(PtySize {
                    rows: 24,
                    cols: 80,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(|_| SupervisorError::Launch {
                    stage: ManagedLaunchStage::Prepare,
                })?;
            let writer = Arc::new(Mutex::new(
                Box::new(std::io::sink()) as Box<dyn std::io::Write + Send>
            ));
            let master_slot = Arc::new(Mutex::new(None));
            let actors = Arc::new(Mutex::new(ManagedTerminalActorHandles::default()));
            let input_admission = Arc::new(AtomicBool::new(false));
            let io = ManagedTerminalIo::new(
                Arc::clone(&writer),
                Arc::clone(&master_slot),
                Arc::clone(&actors),
                Arc::clone(&input_admission),
            );
            let output_lines = Arc::new(Mutex::new(VecDeque::new()));
            let exit_code = Arc::new(Mutex::new(None));

            let mut environment = BTreeMap::new();
            for (key, value) in &spec.environment {
                environment.insert(OsString::from(key), OsString::from(value));
            }
            let intent = crate::process::launcher::LaunchIntent {
                resource_id: spec.resource_id,
                generation: spec.generation,
                owner: spec.owner,
                kind: spec.kind,
                executable: PathBuf::from(resolved_program),
                args: spec.args.iter().cloned().map(OsString::from).collect(),
                cwd: PathBuf::from(&spec.cwd),
                environment,
                display_label: spec.display_label.clone(),
            };
            let pending = prepare_suspended_pty(&*pair.slave, intent).map_err(|_| {
                SupervisorError::Launch {
                    stage: ManagedLaunchStage::Prepare,
                }
            })?;
            Ok(HostPendingLaunch {
                stage: PendingStage::Prepared {
                    pending,
                    operation_id,
                    action_epoch,
                    session_id,
                    completion_store,
                    io,
                    writer,
                    actors,
                    master_slot,
                    output_lines,
                    exit_code,
                    slave: pair.slave,
                    master: pair.master,
                    registered: false,
                },
            })
        }
    }

    fn register_suspended(
        &mut self,
        mut pending: Self::Pending,
    ) -> Result<Self::Pending, SupervisorError> {
        #[cfg(not(windows))]
        {
            let _ = pending;
            Err(SupervisorError::Launch {
                stage: ManagedLaunchStage::Register,
            })
        }
        #[cfg(windows)]
        {
            let PendingStage::Prepared {
                ref mut registered, ..
            } = pending.stage
            else {
                return Err(SupervisorError::Launch {
                    stage: ManagedLaunchStage::Register,
                });
            };
            if *registered {
                return Err(SupervisorError::Launch {
                    stage: ManagedLaunchStage::Register,
                });
            }
            *registered = true;
            Ok(pending)
        }
    }

    fn resume(&mut self, pending: Self::Pending) -> Result<Self::Live, SupervisorError> {
        #[cfg(not(windows))]
        {
            let _ = pending;
            Err(SupervisorError::Launch {
                stage: ManagedLaunchStage::Resume,
            })
        }
        #[cfg(windows)]
        {
            let PendingStage::Prepared {
                pending,
                operation_id,
                action_epoch,
                session_id,
                completion_store,
                io,
                writer,
                actors,
                master_slot,
                output_lines,
                exit_code,
                slave,
                master,
                registered,
            } = pending.stage
            else {
                return Err(SupervisorError::Launch {
                    stage: ManagedLaunchStage::Resume,
                });
            };
            if !registered {
                return Err(SupervisorError::Launch {
                    stage: ManagedLaunchStage::Resume,
                });
            }
            let (teardown, child) = ManagedTerminalTeardown::from_pending_launch(
                pending,
                operation_id,
                action_epoch,
                completion_store,
                session_id,
                Vec::new(),
                Arc::clone(&io),
            )
            .map_err(|_| SupervisorError::Launch {
                stage: ManagedLaunchStage::Resume,
            })?;
            drop(slave);

            let acquired_writer = master.take_writer().map_err(|_| SupervisorError::Launch {
                stage: ManagedLaunchStage::Resume,
            })?;
            {
                let mut writer_slot = writer.lock().map_err(|_| SupervisorError::Launch {
                    stage: ManagedLaunchStage::Resume,
                })?;
                *writer_slot = acquired_writer;
            }
            let reader = master
                .try_clone_reader()
                .map_err(|_| SupervisorError::Launch {
                    stage: ManagedLaunchStage::Resume,
                })?;
            {
                let mut slot = master_slot.lock().map_err(|_| SupervisorError::Launch {
                    stage: ManagedLaunchStage::Resume,
                })?;
                *slot = Some(master);
            }

            let reader_handle = spawn_service_pty_reader(reader, Arc::clone(&output_lines))?;
            let fence = child.fence().resource();
            let waiter_handle =
                spawn_service_pty_waiter(child.into_child(), Arc::clone(&exit_code))?;
            {
                let mut handles = actors.lock().map_err(|_| SupervisorError::Launch {
                    stage: ManagedLaunchStage::Resume,
                })?;
                handles.reader = Some(reader_handle);
                handles.waiter = Some(waiter_handle);
            }
            let _ = io;

            self.live
                .insert(fence.resource_id, fence.runtime_generation);
            Ok(HostLiveLaunch {
                teardown,
                fence,
                output_lines,
                exit_code,
                writer,
            })
        }
    }

    fn teardown(
        &mut self,
        live: &mut Option<Self::Live>,
        fence: ResourceFence,
    ) -> Result<(), SupervisorError> {
        #[cfg(not(windows))]
        {
            let _ = (live, fence);
            Err(SupervisorError::TeardownFailed)
        }
        #[cfg(windows)]
        {
            let Some(handle) = live.take() else {
                return Ok(());
            };
            if handle.fence != fence {
                *live = Some(handle);
                return Err(SupervisorError::TeardownFailed);
            }
            match handle.teardown.close() {
                Ok(report) if report.outcome() == TeardownOutcome::Closed => {
                    self.live.remove(&fence.resource_id);
                    Ok(())
                }
                Ok(_) | Err(_) => {
                    *live = Some(handle);
                    Err(SupervisorError::TeardownFailed)
                }
            }
        }
    }

    fn write_input(
        &mut self,
        live: &Self::Live,
        bytes: &[u8],
        fence: ResourceFence,
    ) -> Result<(), SupervisorError> {
        #[cfg(not(windows))]
        {
            let _ = (live, bytes, fence);
            Err(SupervisorError::Launch {
                stage: ManagedLaunchStage::Resume,
            })
        }
        #[cfg(windows)]
        {
            if live.fence != fence {
                return Err(SupervisorError::TeardownFailed);
            }
            let mut writer = live.writer.lock().map_err(|_| SupervisorError::Launch {
                stage: ManagedLaunchStage::Resume,
            })?;
            writer
                .write_all(bytes)
                .and_then(|_| writer.flush())
                .map_err(|_| SupervisorError::Launch {
                    stage: ManagedLaunchStage::Resume,
                })
        }
    }

    fn drain_output_lines(&self, live: &Self::Live) -> Vec<String> {
        #[cfg(not(windows))]
        {
            let _ = live;
            Vec::new()
        }
        #[cfg(windows)]
        {
            let Ok(mut queue) = live.output_lines.lock() else {
                return Vec::new();
            };
            queue.drain(..).collect()
        }
    }

    fn take_exit(&self, live: &Self::Live) -> Option<Option<i32>> {
        #[cfg(not(windows))]
        {
            let _ = live;
            None
        }
        #[cfg(windows)]
        {
            let Ok(mut slot) = live.exit_code.lock() else {
                return None;
            };
            slot.take()
        }
    }

    fn live_generation(live: &Self::Live) -> u64 {
        #[cfg(not(windows))]
        {
            let _ = live;
            0
        }
        #[cfg(windows)]
        {
            live.fence.runtime_generation
        }
    }

    fn live_count(&self) -> usize {
        self.live.len()
    }

    fn residue_count(&self) -> usize {
        self.live.len()
    }
}

#[cfg(windows)]
fn spawn_service_pty_reader(
    mut reader: Box<dyn Read + Send>,
    output_lines: Arc<Mutex<VecDeque<String>>>,
) -> Result<thread::JoinHandle<()>, SupervisorError> {
    thread::Builder::new()
        .name("service-pty-reader".to_owned())
        .spawn(move || {
            let mut buffer = [0_u8; MAX_SERVICE_PTY_DRAIN_CHUNK];
            let mut pending = String::new();
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => {
                        flush_service_output_line(&output_lines, &mut pending);
                        break;
                    }
                    Ok(count) => {
                        pending.push_str(&String::from_utf8_lossy(&buffer[..count]));
                        while let Some(index) = pending.find('\n') {
                            let mut line = pending[..index].to_owned();
                            pending.drain(..=index);
                            if line.ends_with('\r') {
                                line.pop();
                            }
                            push_service_output_line(&output_lines, line);
                        }
                        if pending.len() > MAX_SERVICE_LOG_LINE_BYTES {
                            let line: String =
                                pending.chars().take(MAX_SERVICE_LOG_LINE_BYTES).collect();
                            pending.clear();
                            push_service_output_line(&output_lines, line);
                        }
                    }
                    Err(_) => {
                        flush_service_output_line(&output_lines, &mut pending);
                        break;
                    }
                }
            }
        })
        .map_err(|_| SupervisorError::Launch {
            stage: ManagedLaunchStage::Resume,
        })
}

#[cfg(windows)]
fn push_service_output_line(output_lines: &Mutex<VecDeque<String>>, mut line: String) {
    if line.len() > MAX_SERVICE_LOG_LINE_BYTES {
        line.truncate(MAX_SERVICE_LOG_LINE_BYTES);
    }
    if let Ok(mut queue) = output_lines.lock() {
        if queue.len() >= MAX_SERVICE_OUTPUT_LINES {
            queue.pop_front();
        }
        queue.push_back(line);
    }
}

#[cfg(windows)]
fn flush_service_output_line(output_lines: &Mutex<VecDeque<String>>, pending: &mut String) {
    if pending.is_empty() {
        return;
    }
    let line = std::mem::take(pending);
    push_service_output_line(output_lines, line);
}

#[cfg(windows)]
fn spawn_service_pty_waiter(
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
    exit_code: Arc<Mutex<Option<Option<i32>>>>,
) -> Result<thread::JoinHandle<()>, SupervisorError> {
    thread::Builder::new()
        .name("service-pty-waiter".to_owned())
        .spawn(move || {
            let code = match child.wait() {
                Ok(status) => Some(status.exit_code() as i32),
                Err(_) => None,
            };
            if let Ok(mut slot) = exit_code.lock() {
                *slot = Some(code);
            }
        })
        .map_err(|_| SupervisorError::Launch {
            stage: ManagedLaunchStage::Resume,
        })
}
