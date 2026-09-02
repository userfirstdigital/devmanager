//! Fail-closed managed PTY process creation.

use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::fmt;
#[cfg(test)]
use std::io;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(test)]
use portable_pty::ExitStatus;
use portable_pty::{Child, CommandBuilder, SlavePty};

use crate::domain::id::ResourceId;
use crate::domain::operation::ResourceFence;
use crate::domain::resource::ResourceKind;
use crate::process::identity::{ManagedProcessId, ManagedProcessIdentity, ProcessOwner};
use crate::process::job::ManagedProcessJob;
use crate::process::registry::{
    ProcessDisplayLabel, ProcessRegistry, ProcessRegistryError, RegisteredProcess,
    UnregisterOutcome, MAX_PROCESS_DISPLAY_LABEL_BYTES,
};

#[cfg(test)]
static MANAGED_LAUNCH_COUNT: AtomicUsize = AtomicUsize::new(0);

const MAX_LAUNCH_ARGUMENTS: usize = 256;
const MAX_LAUNCH_ENVIRONMENT_ENTRIES: usize = 256;
const MAX_LAUNCH_ARGUMENT_BYTES: usize = 32 * 1024;
const MAX_LAUNCH_ENVIRONMENT_KEY_BYTES: usize = 256;
const MAX_LAUNCH_ENVIRONMENT_VALUE_BYTES: usize = 32 * 1024;
const MAX_LAUNCH_PATH_BYTES: usize = 32 * 1024;
const MAX_LAUNCH_TOTAL_BYTES: usize = 256 * 1024;

#[cfg(test)]
pub(crate) fn managed_launch_count_for_test() -> usize {
    MANAGED_LAUNCH_COUNT.load(Ordering::SeqCst)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ManagedLaunchStage {
    Unsupported,
    Validation,
    JobCreation,
    ProcessCreation,
    IdentityCapture,
    Registration,
    Resume,
}

#[derive(Debug)]
pub(crate) struct ManagedLaunchError {
    stage: ManagedLaunchStage,
    detail: String,
    #[cfg(test)]
    registry_error: Option<ProcessRegistryError>,
}

impl ManagedLaunchError {
    fn new(stage: ManagedLaunchStage, detail: impl Into<String>) -> Self {
        Self {
            stage,
            detail: detail.into(),
            #[cfg(test)]
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
            #[cfg(test)]
            registry_error: Some(reason),
        }
    }

    #[cfg(test)]
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
pub(crate) struct LaunchIntent {
    pub(crate) resource_id: ResourceId,
    pub(crate) generation: u64,
    pub(crate) owner: ProcessOwner,
    pub(crate) kind: ResourceKind,
    pub(crate) executable: PathBuf,
    pub(crate) args: Vec<OsString>,
    pub(crate) cwd: PathBuf,
    pub(crate) environment: BTreeMap<OsString, OsString>,
    /// When true, clear inherited ambient env and install `environment` exactly
    /// (provider CLIs). Normal terminals leave this false.
    pub(crate) replace_environment: bool,
    pub(crate) display_label: String,
}

struct ValidatedLaunchIntent {
    fence: ResourceFence,
    owner: ProcessOwner,
    kind: ResourceKind,
    executable: PathBuf,
    args: Vec<OsString>,
    cwd: PathBuf,
    environment: BTreeMap<OsString, OsString>,
    replace_environment: bool,
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
        validate_launch_input_bounds(&self)?;
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
        validate_host_os_bound(
            executable.as_os_str(),
            MAX_LAUNCH_PATH_BYTES,
            "canonical executable path",
        )?;
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
        validate_host_os_bound(
            cwd.as_os_str(),
            MAX_LAUNCH_PATH_BYTES,
            "canonical working directory path",
        )?;
        // Canonicalization is what proves the directory exists and resolves
        // every link, so it stays. What CreateProcess is handed afterwards is a
        // separate question: see `launchable_working_directory`.
        #[cfg(windows)]
        let cwd = launchable_working_directory(cwd);
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
            replace_environment: self.replace_environment,
            display_label,
        })
    }
}

/// Windows current-directory limit for a process that is not long-path aware.
#[cfg(windows)]
const MAX_LEGACY_PATH_CHARS: usize = 260;

/// The launch form of an already-canonicalized working directory.
///
/// `std::fs::canonicalize` always returns the Windows verbatim device form, and
/// `cmd.exe` refuses one as its current directory: it starts in `%SystemRoot%`
/// instead, silently, with no error and nothing on the terminal. Measured on a
/// real `cmd.exe /Q` launched into a canonicalized temp directory -- its PEB
/// reported `C:\Windows\` for the whole life of the shell, so a plain shell
/// opened somewhere the caller never asked for and every later cwd fact was
/// wrong rather than missing.
///
/// The resolved target is unchanged; only its spelling is. A verbatim path is
/// kept as-is when it is not a plain drive path (UNC and device paths have no
/// equivalent short form) or when the short form would exceed the legacy path
/// limit, where the verbatim form is the only one that works at all.
#[cfg(windows)]
fn launchable_working_directory(canonical: PathBuf) -> PathBuf {
    use std::path::{Component, Prefix};

    let mut components = canonical.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return canonical;
    };
    let Prefix::VerbatimDisk(letter) = prefix.kind() else {
        return canonical;
    };
    let mut rebuilt = PathBuf::from(format!("{}:{}", letter as char, std::path::MAIN_SEPARATOR));
    for component in components {
        if !matches!(component, Component::RootDir) {
            rebuilt.push(component);
        }
    }
    // The limit is on characters the Win32 layer counts, not on the WTF-8
    // bytes the OsStr happens to hold: a path full of non-ASCII would otherwise
    // be judged over the limit while Windows considers it well inside.
    if rebuilt.to_string_lossy().chars().count() >= MAX_LEGACY_PATH_CHARS {
        eprintln!(
            "devmanager: launch cwd {} stays in verbatim form; it exceeds the {MAX_LEGACY_PATH_CHARS}-character legacy limit and a shell that is not long-path aware will not accept it",
            rebuilt.display()
        );
        return canonical;
    }
    rebuilt
}

fn validate_launch_input_bounds(intent: &LaunchIntent) -> Result<(), ManagedLaunchError> {
    if intent.display_label.trim().is_empty() {
        return Err(validation_error("display label must be non-empty"));
    }
    if intent.display_label.len() > MAX_PROCESS_DISPLAY_LABEL_BYTES {
        return Err(validation_error(format!(
            "display label exceeds {MAX_PROCESS_DISPLAY_LABEL_BYTES} bytes"
        )));
    }
    if intent.args.len() > MAX_LAUNCH_ARGUMENTS {
        return Err(validation_error(format!(
            "argument count exceeds {MAX_LAUNCH_ARGUMENTS}"
        )));
    }
    if intent.environment.len() > MAX_LAUNCH_ENVIRONMENT_ENTRIES {
        return Err(validation_error(format!(
            "environment entry count exceeds {MAX_LAUNCH_ENVIRONMENT_ENTRIES}"
        )));
    }

    let mut total = 0usize;
    accumulate_host_os_bound(
        &mut total,
        intent.executable.as_os_str(),
        MAX_LAUNCH_PATH_BYTES,
        "executable path",
    )?;
    accumulate_host_os_bound(
        &mut total,
        intent.cwd.as_os_str(),
        MAX_LAUNCH_PATH_BYTES,
        "working directory path",
    )?;
    accumulate_bytes(&mut total, intent.display_label.len(), "display label")?;
    for (index, argument) in intent.args.iter().enumerate() {
        let bytes = host_os_bytes(argument)?;
        if bytes > MAX_LAUNCH_ARGUMENT_BYTES {
            return Err(validation_error(format!(
                "argument {index} exceeds {MAX_LAUNCH_ARGUMENT_BYTES} host bytes"
            )));
        }
        accumulate_bytes(&mut total, bytes, "argument")?;
    }
    for (key, value) in &intent.environment {
        accumulate_host_os_bound(
            &mut total,
            key,
            MAX_LAUNCH_ENVIRONMENT_KEY_BYTES,
            "environment key",
        )?;
        accumulate_host_os_bound(
            &mut total,
            value,
            MAX_LAUNCH_ENVIRONMENT_VALUE_BYTES,
            "environment value",
        )?;
    }
    Ok(())
}

/// Validate borrowed terminal inputs before duplicating them into native
/// launch collections or resolving caller-controlled paths. The reserved
/// environment count covers the fixed defaults added by the terminal bridge.
pub(crate) fn validate_terminal_launch_source_bounds(
    executable: &OsStr,
    cwd: &OsStr,
    display_label: &str,
    args: &[String],
    environment: &HashMap<String, String>,
    reserved_environment_entries: usize,
) -> Result<(), ManagedLaunchError> {
    if display_label.trim().is_empty() {
        return Err(validation_error("display label must be non-empty"));
    }
    if display_label.len() > MAX_PROCESS_DISPLAY_LABEL_BYTES {
        return Err(validation_error(format!(
            "display label exceeds {MAX_PROCESS_DISPLAY_LABEL_BYTES} bytes"
        )));
    }
    if args.len() > MAX_LAUNCH_ARGUMENTS {
        return Err(validation_error(format!(
            "argument count exceeds {MAX_LAUNCH_ARGUMENTS}"
        )));
    }
    let environment_count = environment
        .len()
        .checked_add(reserved_environment_entries)
        .ok_or_else(|| validation_error("environment entry count overflow"))?;
    if environment_count > MAX_LAUNCH_ENVIRONMENT_ENTRIES {
        return Err(validation_error(format!(
            "environment entry count exceeds {MAX_LAUNCH_ENVIRONMENT_ENTRIES}"
        )));
    }

    let mut total = 0usize;
    accumulate_host_os_bound(
        &mut total,
        executable,
        MAX_LAUNCH_PATH_BYTES,
        "executable path",
    )?;
    accumulate_host_os_bound(
        &mut total,
        cwd,
        MAX_LAUNCH_PATH_BYTES,
        "working directory path",
    )?;
    accumulate_bytes(&mut total, display_label.len(), "display label")?;
    for (index, argument) in args.iter().enumerate() {
        let bytes = host_os_bytes(OsStr::new(argument))?;
        if bytes > MAX_LAUNCH_ARGUMENT_BYTES {
            return Err(validation_error(format!(
                "argument {index} exceeds {MAX_LAUNCH_ARGUMENT_BYTES} host bytes"
            )));
        }
        accumulate_bytes(&mut total, bytes, "argument")?;
    }
    for (key, value) in environment {
        accumulate_host_os_bound(
            &mut total,
            OsStr::new(key),
            MAX_LAUNCH_ENVIRONMENT_KEY_BYTES,
            "environment key",
        )?;
        accumulate_host_os_bound(
            &mut total,
            OsStr::new(value),
            MAX_LAUNCH_ENVIRONMENT_VALUE_BYTES,
            "environment value",
        )?;
    }
    Ok(())
}

fn validate_host_os_bound(
    value: &OsStr,
    maximum: usize,
    field: &str,
) -> Result<(), ManagedLaunchError> {
    let bytes = host_os_bytes(value)?;
    if bytes > maximum {
        return Err(validation_error(format!(
            "{field} exceeds {maximum} host bytes"
        )));
    }
    Ok(())
}

fn accumulate_host_os_bound(
    total: &mut usize,
    value: &OsStr,
    maximum: usize,
    field: &str,
) -> Result<(), ManagedLaunchError> {
    let bytes = host_os_bytes(value)?;
    if bytes > maximum {
        return Err(validation_error(format!(
            "{field} exceeds {maximum} host bytes"
        )));
    }
    accumulate_bytes(total, bytes, field)
}

fn accumulate_bytes(
    total: &mut usize,
    bytes: usize,
    field: &str,
) -> Result<(), ManagedLaunchError> {
    *total = total
        .checked_add(bytes)
        .ok_or_else(|| validation_error(format!("{field} overflows the launch byte budget")))?;
    if *total > MAX_LAUNCH_TOTAL_BYTES {
        return Err(validation_error(format!(
            "launch total byte count exceeds {MAX_LAUNCH_TOTAL_BYTES}"
        )));
    }
    Ok(())
}

#[cfg(windows)]
fn host_os_bytes(value: &OsStr) -> Result<usize, ManagedLaunchError> {
    value
        .encode_wide()
        .count()
        .checked_mul(std::mem::size_of::<u16>())
        .ok_or_else(|| validation_error("host string byte count overflow"))
}

#[cfg(not(windows))]
fn host_os_bytes(value: &OsStr) -> Result<usize, ManagedLaunchError> {
    Ok(value.as_encoded_bytes().len())
}

fn validation_error(detail: impl Into<String>) -> ManagedLaunchError {
    ManagedLaunchError::new(ManagedLaunchStage::Validation, detail)
}

#[cfg(all(test, not(windows)))]
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
pub(crate) struct PendingManagedLaunch {
    // Keep pending first: ordinary field drop order aborts the child before the
    // final Job handle is closed if a caller abandons this value.
    pending: portable_pty::win::PendingChild,
    job: ManagedProcessJob,
    fence: ResourceFence,
    owner: ProcessOwner,
    root: ManagedProcessIdentity,
    display_label: ProcessDisplayLabel,
}

#[cfg(windows)]
impl PendingManagedLaunch {
    #[cfg(test)]
    pub(crate) fn process_id(&self) -> u32 {
        self.root.id().pid()
    }

    #[cfg(test)]
    pub(crate) fn active_process_ids(&self) -> Result<Vec<u32>, String> {
        self.job.active_process_ids()
    }

    #[cfg(test)]
    pub(crate) fn internal_job_name(&self) -> &str {
        self.job.internal_name()
    }

    pub(crate) fn register_suspended(
        self,
        registry: &mut ProcessRegistry<ManagedProcessJob>,
    ) -> Result<RegisteredPendingManagedLaunch, ManagedLaunchError> {
        let Self {
            pending,
            job,
            fence,
            owner,
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

        Ok(RegisteredPendingManagedLaunch {
            pending,
            fence: managed_fence,
        })
    }

    #[cfg(test)]
    pub(crate) fn register_and_resume(
        self,
        registry: &mut ProcessRegistry<ManagedProcessJob>,
    ) -> Result<ManagedPtyChild, ManagedLaunchError> {
        self.register_suspended(registry)?.resume(registry)
    }
}

/// A suspended root whose exact Job and identity fence are already present in
/// the authoritative registry.  The host can build every fallible teardown
/// adapter around this value before crossing the one-way resume boundary.
#[cfg(windows)]
#[must_use = "a registered suspended launch must be resumed or dropped to abort"]
#[derive(Debug)]
pub(crate) struct RegisteredPendingManagedLaunch {
    pending: portable_pty::win::PendingChild,
    fence: crate::process::registry::ManagedProcessFence,
}

#[cfg(windows)]
impl RegisteredPendingManagedLaunch {
    pub(crate) fn fence(&self) -> &crate::process::registry::ManagedProcessFence {
        &self.fence
    }

    pub(crate) fn resume(
        self,
        registry: &mut ProcessRegistry<ManagedProcessJob>,
    ) -> Result<ManagedPtyChild, ManagedLaunchError> {
        let Self {
            pending,
            fence: managed_fence,
        } = self;
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

        #[cfg(test)]
        MANAGED_LAUNCH_COUNT.fetch_add(1, Ordering::SeqCst);

        Ok(ManagedPtyChild {
            child: Box::new(child),
            fence: managed_fence,
        })
    }
}

#[cfg(windows)]
#[derive(Debug)]
pub(crate) struct ManagedPtyChild {
    child: Box<dyn Child + Send + Sync>,
    fence: crate::process::registry::ManagedProcessFence,
}

#[cfg(windows)]
impl ManagedPtyChild {
    pub(crate) fn fence(&self) -> &crate::process::registry::ManagedProcessFence {
        &self.fence
    }

    #[cfg(test)]
    pub(crate) fn process_id(&self) -> u32 {
        self.child
            .process_id()
            .expect("managed Windows PTY child must expose its PID")
    }

    #[cfg(test)]
    pub(crate) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    pub(crate) fn into_child(self) -> Box<dyn Child + Send + Sync> {
        self.child
    }
}

#[cfg(windows)]
pub(crate) fn validate_process_image_length(
    reported_length: u32,
    buffer_capacity: usize,
) -> Result<usize, String> {
    let reported_length = usize::try_from(reported_length)
        .map_err(|_| "process image length does not fit usize".to_string())?;
    if reported_length > buffer_capacity {
        return Err(format!(
            "process image length {reported_length} exceeds buffer capacity {buffer_capacity}"
        ));
    }
    Ok(reported_length)
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
    let job = ManagedProcessJob::create_for_resource(intent.owner, intent.fence)
        .map_err(|error| ManagedLaunchError::new(ManagedLaunchStage::JobCreation, error))?;

    let mut command = CommandBuilder::new(&intent.executable);
    command.args(&intent.args);
    command.cwd(&intent.cwd);
    if intent.replace_environment {
        command.env_clear();
        for (key, value) in &intent.environment {
            command.env(key, value);
        }
    } else {
        for (key, value) in &intent.environment {
            command.env(key, value);
        }
    }
    if !intent.replace_environment {
        command.env_remove("NO_COLOR");
        command.env_remove("NODE_DISABLE_COLORS");
    }
    // Only host-owned resource metadata is added after the provider environment
    // is sealed. Never alter provider authentication/configuration variables here.
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
    let path_len = match validate_process_image_length(path_len, path_buffer.len()) {
        Ok(path_len) => path_len,
        Err(error) => {
            return Err(abort_pending(
                ManagedLaunchStage::IdentityCapture,
                error,
                pending,
            ));
        }
    };
    let observed_executable = PathBuf::from(OsString::from_wide(&path_buffer[..path_len]));
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

#[cfg(all(test, windows))]
mod tests {
    use std::path::{Component, Prefix};

    #[test]
    fn a_canonical_launch_directory_is_handed_over_in_the_form_cmd_accepts() {
        let canonical = std::env::temp_dir()
            .canonicalize()
            .expect("canonical temp dir");
        assert!(
            matches!(
                canonical.components().next(),
                Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::VerbatimDisk(_))
            ),
            "canonicalize must produce the verbatim form this rewrite exists for: {canonical:?}"
        );

        let launchable = super::launchable_working_directory(canonical.clone());
        assert!(
            matches!(
                launchable.components().next(),
                Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_))
            ),
            "cmd.exe silently ignores a verbatim current directory: {launchable:?}"
        );
        assert_eq!(
            launchable.canonicalize().expect("round trip"),
            canonical,
            "only the spelling may change, never the resolved target"
        );
    }

    #[test]
    fn a_path_with_no_short_form_is_left_exactly_as_resolved() {
        // A verbatim UNC path has no equivalent drive form, and a path past the
        // legacy limit only works in the verbatim form: both must survive.
        let unc = std::path::PathBuf::from(format!(
            "{sep}{sep}?{sep}UNC{sep}server{sep}share{sep}dir",
            sep = std::path::MAIN_SEPARATOR
        ));
        assert_eq!(super::launchable_working_directory(unc.clone()), unc);

        let long = std::path::PathBuf::from(format!(
            "{sep}{sep}?{sep}C:{sep}{}",
            "segment".repeat(60),
            sep = std::path::MAIN_SEPARATOR
        ));
        assert_eq!(super::launchable_working_directory(long.clone()), long);
    }

    /// The limit is on characters Windows counts, not on the WTF-8 bytes the
    /// OsStr holds. Counting bytes would keep a non-ASCII path in the verbatim
    /// form that `cmd.exe` silently ignores, at roughly a third of the length
    /// Windows actually allows.
    #[test]
    fn the_legacy_limit_counts_characters_not_encoded_bytes() {
        // 200 three-byte characters: well inside 260 characters, well past 260
        // encoded bytes.
        let segment = "中".repeat(200);
        let verbatim = std::path::PathBuf::from(format!(
            "{sep}{sep}?{sep}C:{sep}{segment}",
            sep = std::path::MAIN_SEPARATOR
        ));
        assert!(
            verbatim.as_os_str().len() > 260,
            "the byte length must exceed the limit or this test proves nothing"
        );
        let launchable = super::launchable_working_directory(verbatim);
        assert!(
            matches!(
                launchable.components().next(),
                Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_))
            ),
            "a path inside the character limit must still be rewritten: {launchable:?}"
        );
    }
}
