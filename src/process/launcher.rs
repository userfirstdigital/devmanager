//! Fail-closed managed PTY process creation.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::PathBuf;

use portable_pty::{Child, CommandBuilder, ExitStatus, SlavePty};

use crate::domain::id::ResourceId;
use crate::domain::operation::ResourceFence;
use crate::domain::resource::ResourceKind;
use crate::process::identity::{ManagedProcessId, ManagedProcessIdentity, ProcessOwner};
use crate::process::job::ManagedProcessJob;
use crate::process::registry::{
    ProcessDisplayLabel, ProcessRegistry, ProcessRegistryError, RegisteredProcess,
    UnregisterOutcome,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedLaunchStage {
    Unsupported,
    Validation,
    JobCreation,
    ProcessCreation,
    IdentityCapture,
    Registration,
    Resume,
}

#[derive(Debug)]
pub struct ManagedLaunchError {
    stage: ManagedLaunchStage,
    detail: String,
    registry_error: Option<ProcessRegistryError>,
}

impl ManagedLaunchError {
    fn new(stage: ManagedLaunchStage, detail: impl Into<String>) -> Self {
        Self {
            stage,
            detail: detail.into(),
            registry_error: None,
        }
    }

    fn registration(reason: ProcessRegistryError, cleanup_error: Option<String>) -> Self {
        let mut detail = reason.to_string();
        if let Some(cleanup_error) = cleanup_error {
            detail.push_str("; suspended child cleanup failed: ");
            detail.push_str(&cleanup_error);
        }
        Self {
            stage: ManagedLaunchStage::Registration,
            detail,
            registry_error: Some(reason),
        }
    }

    pub(crate) fn stage(&self) -> ManagedLaunchStage {
        self.stage
    }

    pub(crate) fn registry_error(&self) -> Option<&ProcessRegistryError> {
        self.registry_error.as_ref()
    }
}

impl fmt::Display for ManagedLaunchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "managed launch {:?}: {}",
            self.stage, self.detail
        )
    }
}

impl std::error::Error for ManagedLaunchError {}

#[derive(Debug)]
pub struct LaunchIntent {
    pub resource_id: ResourceId,
    pub generation: u64,
    pub owner: ProcessOwner,
    pub kind: ResourceKind,
    pub executable: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub environment: BTreeMap<OsString, OsString>,
    pub display_label: String,
}

struct ValidatedLaunchIntent {
    fence: ResourceFence,
    owner: ProcessOwner,
    kind: ResourceKind,
    executable: PathBuf,
    args: Vec<OsString>,
    cwd: PathBuf,
    environment: BTreeMap<OsString, OsString>,
    display_label: ProcessDisplayLabel,
}

impl LaunchIntent {
    fn validate(self) -> Result<ValidatedLaunchIntent, ManagedLaunchError> {
        if self.generation == 0 {
            return Err(ManagedLaunchError::new(
                ManagedLaunchStage::Validation,
                "runtime generation must be greater than zero",
            ));
        }
        let executable = std::fs::canonicalize(&self.executable).map_err(|error| {
            ManagedLaunchError::new(
                ManagedLaunchStage::Validation,
                format!(
                    "cannot resolve executable `{}`: {error}",
                    self.executable.display()
                ),
            )
        })?;
        if !executable.is_file() {
            return Err(ManagedLaunchError::new(
                ManagedLaunchStage::Validation,
                format!("executable `{}` is not a file", executable.display()),
            ));
        }
        let cwd = std::fs::canonicalize(&self.cwd).map_err(|error| {
            ManagedLaunchError::new(
                ManagedLaunchStage::Validation,
                format!(
                    "cannot resolve working directory `{}`: {error}",
                    self.cwd.display()
                ),
            )
        })?;
        if !cwd.is_dir() {
            return Err(ManagedLaunchError::new(
                ManagedLaunchStage::Validation,
                format!("working directory `{}` is not a directory", cwd.display()),
            ));
        }
        let display_label = ProcessDisplayLabel::new(self.display_label).map_err(|error| {
            ManagedLaunchError::new(ManagedLaunchStage::Validation, error.to_string())
        })?;

        Ok(ValidatedLaunchIntent {
            fence: ResourceFence::new(self.resource_id, self.generation),
            owner: self.owner,
            kind: self.kind,
            executable,
            args: self.args,
            cwd,
            environment: self.environment,
            display_label,
        })
    }
}

pub(crate) fn is_supported() -> Result<(), ManagedLaunchError> {
    if cfg!(windows) {
        Ok(())
    } else {
        Err(ManagedLaunchError::new(
            ManagedLaunchStage::Unsupported,
            "suspended managed PTY launch is available only on Windows",
        ))
    }
}

#[cfg(windows)]
#[must_use = "a pending managed launch must be registered and resumed or dropped to abort"]
#[derive(Debug)]
pub struct PendingManagedLaunch {
    // Keep pending first: ordinary field drop order aborts the child before the
    // final Job handle is closed if a caller abandons this value.
    pending: portable_pty::win::PendingChild,
    job: ManagedProcessJob,
    fence: ResourceFence,
    owner: ProcessOwner,
    kind: ResourceKind,
    root: ManagedProcessIdentity,
    display_label: ProcessDisplayLabel,
}

#[cfg(windows)]
impl PendingManagedLaunch {
    pub(crate) fn process_id(&self) -> u32 {
        self.root.id().pid()
    }

    pub(crate) fn root_identity(&self) -> &ManagedProcessIdentity {
        &self.root
    }

    pub(crate) fn active_process_ids(&self) -> Result<Vec<u32>, String> {
        self.job.active_process_ids()
    }

    pub(crate) fn register_and_resume(
        self,
        registry: &mut ProcessRegistry<ManagedProcessJob>,
    ) -> Result<ManagedPtyChild, ManagedLaunchError> {
        let Self {
            pending,
            job,
            fence,
            owner,
            kind,
            root,
            display_label,
        } = self;
        let process = RegisteredProcess::new(fence, owner, root, display_label, job);
        let managed_fence = match registry.register(process) {
            Ok(fence) => fence,
            Err(failure) => {
                let (reason, rejected) = failure.into_parts();
                let cleanup_error = pending
                    .abort_and_wait()
                    .err()
                    .map(|error| error.to_string());
                drop(rejected);
                return Err(ManagedLaunchError::registration(reason, cleanup_error));
            }
        };

        let child = match pending.resume() {
            Ok(child) => child,
            Err(error) => {
                let cleanup_detail = match registry.rollback_starting_exact(&managed_fence) {
                    Ok(UnregisterOutcome::Removed(process)) => {
                        drop(process);
                        String::new()
                    }
                    Ok(UnregisterOutcome::Stale) => {
                        "; registry cleanup unexpectedly found a stale generation".to_string()
                    }
                    Err(cleanup_error) => format!("; registry cleanup failed: {cleanup_error}"),
                };
                return Err(ManagedLaunchError::new(
                    ManagedLaunchStage::Resume,
                    format!("{error}{cleanup_detail}"),
                ));
            }
        };
        registry
            .commit_resumed_exact(&managed_fence)
            .expect("newly resumed launch retains its exact Starting registry fence");

        Ok(ManagedPtyChild {
            child: Box::new(child),
            fence: managed_fence,
            kind,
        })
    }
}

#[cfg(windows)]
#[derive(Debug)]
pub struct ManagedPtyChild {
    child: Box<dyn Child + Send + Sync>,
    fence: crate::process::registry::ManagedProcessFence,
    kind: ResourceKind,
}

#[cfg(windows)]
impl ManagedPtyChild {
    pub(crate) fn fence(&self) -> &crate::process::registry::ManagedProcessFence {
        &self.fence
    }

    pub(crate) fn process_id(&self) -> u32 {
        self.child
            .process_id()
            .expect("managed Windows PTY child must expose its PID")
    }

    pub(crate) fn kind(&self) -> ResourceKind {
        self.kind
    }

    pub(crate) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    pub(crate) fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait()
    }

    pub(crate) fn into_child(self) -> Box<dyn Child + Send + Sync> {
        self.child
    }
}

#[cfg(windows)]
pub(crate) fn prepare_suspended_pty(
    slave: &dyn SlavePty,
    intent: LaunchIntent,
) -> Result<PendingManagedLaunch, ManagedLaunchError> {
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::io::AsRawHandle;

    use windows::core::PWSTR;
    use windows::Win32::Foundation::{FILETIME, HANDLE};
    use windows::Win32::System::Threading::{
        GetProcessId, GetProcessTimes, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    };

    let intent = intent.validate()?;
    let job = ManagedProcessJob::create()
        .map_err(|error| ManagedLaunchError::new(ManagedLaunchStage::JobCreation, error))?
        .ok_or_else(|| {
            ManagedLaunchError::new(
                ManagedLaunchStage::Unsupported,
                "Windows Job Objects are unavailable",
            )
        })?;

    let mut command = CommandBuilder::new(&intent.executable);
    command.args(&intent.args);
    command.cwd(&intent.cwd);
    for (key, value) in &intent.environment {
        command.env(key, value);
    }
    for key in [
        "DEVMANAGER_TASK_ID",
        "DEVMANAGER_RESOURCE_ID",
        "DEVMANAGER_RESOURCE_KIND",
        "DEVMANAGER_PROCESS_LABEL",
    ] {
        command.env_remove(key);
    }
    command.env(
        "DEVMANAGER_RESOURCE_ID",
        intent.fence.resource_id.to_string(),
    );
    command.env(
        "DEVMANAGER_RESOURCE_KIND",
        match intent.kind {
            ResourceKind::Terminal => "terminal",
            ResourceKind::BrowserContext => "browser",
            ResourceKind::Service => "service",
        },
    );
    command.env("DEVMANAGER_PROCESS_LABEL", intent.display_label.as_str());
    if let ProcessOwner::Task(task_id) = intent.owner {
        command.env("DEVMANAGER_TASK_ID", task_id.to_string());
    }

    let pending = slave
        .spawn_command_suspended_in_job(command, job.borrowed_handle())
        .map_err(|error| {
            ManagedLaunchError::new(ManagedLaunchStage::ProcessCreation, error.to_string())
        })?;
    let process_handle = HANDLE(pending.process_handle().as_raw_handle());
    let pid = unsafe { GetProcessId(process_handle) };
    if pid == 0 || pid != pending.process_id() {
        let observed = pending.process_id();
        return Err(abort_pending(
            ManagedLaunchStage::IdentityCapture,
            format!("GetProcessId returned {pid} for pending PID {observed}"),
            pending,
        ));
    }

    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    if let Err(error) = unsafe {
        GetProcessTimes(
            process_handle,
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    } {
        return Err(abort_pending(
            ManagedLaunchStage::IdentityCapture,
            format!("GetProcessTimes failed: {error}"),
            pending,
        ));
    }
    let creation_time_100ns =
        ((creation.dwHighDateTime as u64) << 32) | creation.dwLowDateTime as u64;

    let mut path_buffer = vec![0u16; 32_768];
    let mut path_len = path_buffer.len() as u32;
    if let Err(error) = unsafe {
        QueryFullProcessImageNameW(
            process_handle,
            PROCESS_NAME_WIN32,
            PWSTR(path_buffer.as_mut_ptr()),
            &mut path_len,
        )
    } {
        return Err(abort_pending(
            ManagedLaunchStage::IdentityCapture,
            format!("QueryFullProcessImageNameW failed: {error}"),
            pending,
        ));
    }
    let observed_executable = PathBuf::from(OsString::from_wide(&path_buffer[..path_len as usize]));
    let process_id = match ManagedProcessId::new(pid, creation_time_100ns) {
        Ok(process_id) => process_id,
        Err(error) => {
            return Err(abort_pending(
                ManagedLaunchStage::IdentityCapture,
                error.to_string(),
                pending,
            ));
        }
    };
    let root = match ManagedProcessIdentity::new(process_id, observed_executable) {
        Ok(root) => root,
        Err(error) => {
            return Err(abort_pending(
                ManagedLaunchStage::IdentityCapture,
                error.to_string(),
                pending,
            ));
        }
    };
    if root.canonical_executable() != intent.executable {
        let observed = root.canonical_executable().display().to_string();
        let expected = intent.executable.display().to_string();
        return Err(abort_pending(
            ManagedLaunchStage::IdentityCapture,
            format!("created executable `{observed}` did not match intent `{expected}`"),
            pending,
        ));
    }

    let members = match job.active_process_ids() {
        Ok(members) => members,
        Err(error) => {
            return Err(abort_pending(
                ManagedLaunchStage::IdentityCapture,
                error,
                pending,
            ));
        }
    };
    if !members.contains(&pid) {
        return Err(abort_pending(
            ManagedLaunchStage::IdentityCapture,
            format!("pending PID {pid} was not assigned to its Job before resume"),
            pending,
        ));
    }

    Ok(PendingManagedLaunch {
        pending,
        job,
        fence: intent.fence,
        owner: intent.owner,
        kind: intent.kind,
        root,
        display_label: intent.display_label,
    })
}

#[cfg(windows)]
fn abort_pending(
    stage: ManagedLaunchStage,
    detail: impl Into<String>,
    pending: portable_pty::win::PendingChild,
) -> ManagedLaunchError {
    let mut detail = detail.into();
    if let Err(cleanup_error) = pending.abort_and_wait() {
        detail.push_str("; suspended child cleanup failed: ");
        detail.push_str(&cleanup_error.to_string());
    }
    ManagedLaunchError::new(stage, detail)
}
