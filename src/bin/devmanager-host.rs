//! Development foreground `devmanager-host` entry for Phase 2 ownership.
//!
//! This binary owns one profile lock and one writable command bus executor,
//! accepts authenticated clients concurrently on tracked tasks, and remains
//! bound to its exact parent process. It does not yet own Phase 3 supervised
//! resources.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use devmanager::config::paths::{resolve_app_paths, AppProfile, BuildKind};
use devmanager::domain::ClientId;
use devmanager::host::{
    AcceptHelloConfig, HelloListener, HostConnection, HostLock, HostLockError, HostRequestExecutor,
    HostRequestHandle, HOST_EXIT_ALREADY_RUNNING,
};
use devmanager::kernel::CommandBus;
use devmanager::protocol::{Capability, CapabilitySet, FrameLimits};
use uuid::Uuid;

const MAX_INSTANCE_LABEL_CHARS: usize = 64;
const PARENT_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug)]
struct HostArgs {
    profile: String,
    #[allow(dead_code)]
    instance_label: String,
    parent_pid: u32,
    config_base: PathBuf,
    test_slow_durable_reader_client_id: Option<ClientId>,
}

#[derive(Debug)]
struct PreparedDebugPaths {
    profile_root: PathBuf,
    database: PathBuf,
}

enum HostRunError {
    Message(String),
    AlreadyRunning(String),
}

impl From<String> for HostRunError {
    fn from(value: String) -> Self {
        Self::Message(value)
    }
}

fn main() -> ExitCode {
    let mut argv = std::env::args().skip(1).peekable();
    // Dispatch ctl before foreground-host bootstrap so CLI never parses or uses
    // --parent-pid, --config-base, or HostLock.
    if argv.peek().map(String::as_str) == Some("ctl") {
        argv.next();
        return devmanager::client::dispatch_ctl_from_args(argv);
    }

    match run(argv.collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(HostRunError::AlreadyRunning(message)) => {
            let _ = writeln!(io::stderr(), "devmanager-host: {message}");
            ExitCode::from(HOST_EXIT_ALREADY_RUNNING)
        }
        Err(HostRunError::Message(message)) => {
            let _ = writeln!(io::stderr(), "devmanager-host: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run(raw_args: Vec<String>) -> Result<(), HostRunError> {
    #[cfg(not(debug_assertions))]
    {
        let _ = raw_args;
        return Err(HostRunError::Message(
            "release host startup is deferred until Phase 11".to_string(),
        ));
    }

    #[cfg(not(windows))]
    {
        let _ = raw_args;
        return Err(HostRunError::Message(
            "devmanager-host requires Windows".to_string(),
        ));
    }

    #[cfg(all(windows, debug_assertions))]
    {
        let args = parse_args(raw_args)?;
        let paths = prepare_debug_paths(&args)?;
        let parent = open_and_validate_parent(args.parent_pid)?;
        let host_lock = acquire_lock(&paths.profile_root, &args.profile)?;
        let host_boot_id = host_lock.identity().boot_id;
        let bus = CommandBus::open(&paths.database)
            .map_err(|error| format!("failed to open host command bus: {error}"))?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| format!("failed to build host async runtime: {error}"))?;
        let _ = &args.instance_label;
        runtime.block_on(serve_foreground_host(
            &args.profile,
            parent,
            host_boot_id,
            bus,
            args.test_slow_durable_reader_client_id,
        ))?;
        drop(host_lock);
        Ok(())
    }
}

#[cfg(all(windows, debug_assertions))]
fn parse_args(raw: Vec<String>) -> Result<HostArgs, String> {
    let mut foreground = false;
    let mut profile: Option<String> = None;
    let mut instance_label: Option<String> = None;
    let mut parent_pid: Option<u32> = None;
    let mut config_base: Option<PathBuf> = None;
    let mut test_slow_durable_reader_client_id: Option<ClientId> = None;

    let mut args = raw.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--foreground" => {
                if foreground {
                    return Err("duplicate --foreground".to_string());
                }
                foreground = true;
            }
            "--profile" => {
                if profile.is_some() {
                    return Err("duplicate --profile".to_string());
                }
                let value = args
                    .next()
                    .ok_or_else(|| "missing value for --profile".to_string())?;
                profile = Some(value);
            }
            "--instance-label" => {
                if instance_label.is_some() {
                    return Err("duplicate --instance-label".to_string());
                }
                let value = args
                    .next()
                    .ok_or_else(|| "missing value for --instance-label".to_string())?;
                instance_label = Some(value);
            }
            "--parent-pid" => {
                if parent_pid.is_some() {
                    return Err("duplicate --parent-pid".to_string());
                }
                let value = args
                    .next()
                    .ok_or_else(|| "missing value for --parent-pid".to_string())?;
                let pid = value
                    .parse::<u32>()
                    .map_err(|_| format!("invalid --parent-pid value: {value:?}"))?;
                parent_pid = Some(pid);
            }
            "--config-base" => {
                if config_base.is_some() {
                    return Err("duplicate --config-base".to_string());
                }
                let value = args
                    .next()
                    .ok_or_else(|| "missing value for --config-base".to_string())?;
                config_base = Some(PathBuf::from(value));
            }
            "--test-slow-durable-reader-client-id" => {
                if test_slow_durable_reader_client_id.is_some() {
                    return Err("duplicate --test-slow-durable-reader-client-id".to_string());
                }
                let value = args.next().ok_or_else(|| {
                    "missing value for --test-slow-durable-reader-client-id".to_string()
                })?;
                let client_id = ClientId::parse(&value).map_err(|error| {
                    format!("invalid --test-slow-durable-reader-client-id value: {error}")
                })?;
                test_slow_durable_reader_client_id = Some(client_id);
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown flag: {other}"));
            }
            other => return Err(format!("unexpected argument: {other}")),
        }
    }

    if !foreground {
        return Err("missing required --foreground".to_string());
    }

    let profile_raw = profile.ok_or_else(|| "missing required --profile".to_string())?;
    let profile = validate_debug_profile(&profile_raw)?;

    let instance_label_raw =
        instance_label.ok_or_else(|| "missing required --instance-label".to_string())?;
    let instance_label = validate_instance_label(&instance_label_raw)?;

    let parent_pid = parent_pid.ok_or_else(|| "missing required --parent-pid".to_string())?;
    validate_parent_pid_shape(parent_pid)?;

    let config_base = config_base.ok_or_else(|| "missing required --config-base".to_string())?;
    let config_base = validate_config_base(&config_base)?;

    if test_slow_durable_reader_client_id.is_some() {
        validate_slow_durable_reader_isolation(&instance_label, &profile, &config_base)?;
    }

    Ok(HostArgs {
        profile,
        instance_label,
        parent_pid,
        config_base,
        test_slow_durable_reader_client_id,
    })
}

fn validate_debug_profile(raw: &str) -> Result<String, String> {
    if raw.is_empty() {
        return Err("profile must be nonempty".to_string());
    }
    if raw.eq_ignore_ascii_case("production") {
        return Err("reserved production profile is forbidden in debug host".to_string());
    }
    match AppProfile::named(raw) {
        Ok(AppProfile::Named(name)) => {
            if name == "production" {
                return Err("reserved production profile is forbidden in debug host".to_string());
            }
            Ok(name)
        }
        Ok(_) => Err(format!("invalid debug profile: {raw:?}")),
        Err(error) => Err(error.to_string()),
    }
}

fn validate_instance_label(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("instance label must be nonempty after trim".to_string());
    }
    if trimmed.chars().count() > MAX_INSTANCE_LABEL_CHARS {
        return Err(format!(
            "instance label exceeds {MAX_INSTANCE_LABEL_CHARS} characters"
        ));
    }
    if !trimmed
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, ' ' | '-' | '_' | '.'))
    {
        return Err(
            "instance label may contain only ASCII alphanumeric, space, '-', '_', '.'".to_string(),
        );
    }
    Ok(trimmed.to_string())
}

#[cfg(all(windows, debug_assertions))]
fn validate_slow_durable_reader_isolation(
    instance_label: &str,
    profile: &str,
    config_base: &Path,
) -> Result<(), String> {
    if instance_label != "Lifecycle Test" {
        return Err(
            "--test-slow-durable-reader-client-id requires instance label exactly 'Lifecycle Test'"
                .to_string(),
        );
    }
    if !profile.starts_with("lifecycle") {
        return Err(
            "--test-slow-durable-reader-client-id requires a profile that begins with 'lifecycle'"
                .to_string(),
        );
    }
    let temp_root = std::env::temp_dir().canonicalize().map_err(|error| {
        format!(
            "failed to canonicalize system temp directory for slow-durable-reader isolation: {error}"
        )
    })?;
    if !path_strictly_beneath(config_base, &temp_root) {
        return Err(format!(
            "--test-slow-durable-reader-client-id requires config base to be a strict descendant of the system temp root: {}",
            config_base.display()
        ));
    }
    Ok(())
}

fn validate_parent_pid_shape(parent_pid: u32) -> Result<(), String> {
    if parent_pid == 0 {
        return Err("parent pid must be nonzero".to_string());
    }
    if parent_pid == std::process::id() {
        return Err("parent pid must not be the host process itself".to_string());
    }
    Ok(())
}

fn installed_production_root() -> Result<PathBuf, String> {
    let config_dir = dirs::config_dir().ok_or_else(|| {
        "unable to resolve normal config directory for production-root policy".to_string()
    })?;
    let config_dir = config_dir.canonicalize().map_err(|error| {
        format!("unable to canonicalize normal config directory {config_dir:?}: {error}")
    })?;
    let lexical = resolve_app_paths(&config_dir, AppProfile::Production, BuildKind::Release)
        .map(|paths| paths.root)
        .map_err(|error| error.to_string())?;
    if lexical.exists() {
        // Canonicalize the production root itself so junctions cannot hide it.
        lexical.canonicalize().map_err(|error| {
            format!(
                "unable to canonicalize installed production root {}: {error}",
                lexical.display()
            )
        })
    } else {
        Ok(lexical)
    }
}

fn path_equals_or_beneath(path: &Path, root: &Path) -> bool {
    path == root || path.starts_with(root)
}

fn path_strictly_beneath(path: &Path, root: &Path) -> bool {
    path.starts_with(root) && path != root
}

fn validate_config_base(config_base: &Path) -> Result<PathBuf, String> {
    if !config_base.is_absolute() {
        return Err(format!(
            "config base must be an absolute path: {}",
            config_base.display()
        ));
    }
    if !config_base.is_dir() {
        return Err(format!(
            "config base must be an existing directory: {}",
            config_base.display()
        ));
    }

    let canonical = config_base.canonicalize().map_err(|error| {
        format!(
            "failed to canonicalize config base {}: {error}",
            config_base.display()
        )
    })?;

    let production_root = installed_production_root()?;
    if path_equals_or_beneath(&canonical, &production_root) {
        return Err(format!(
            "config base must not be the installed production root or a descendant: {}",
            canonical.display()
        ));
    }

    Ok(canonical)
}

#[cfg(all(windows, debug_assertions))]
fn is_reparse_point(path: &Path) -> Result<bool, String> {
    use std::os::windows::fs::MetadataExt;
    use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    // symlink_metadata does not follow junctions/reparse points.
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        format!(
            "failed to read symlink metadata for {}: {error}",
            path.display()
        )
    })?;
    Ok(metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0)
}

#[cfg(all(windows, debug_assertions))]
fn prepare_debug_paths(args: &HostArgs) -> Result<PreparedDebugPaths, String> {
    let profile = AppProfile::named(&args.profile).map_err(|error| error.to_string())?;
    let paths = resolve_app_paths(&args.config_base, profile, BuildKind::Debug)
        .map_err(|error| error.to_string())?;
    let root = paths.root.clone();

    if root
        .parent()
        .map(|parent| parent != args.config_base.as_path())
        .unwrap_or(true)
    {
        return Err(format!(
            "resolved profile root must be a direct child of config base: {}",
            root.display()
        ));
    }

    match fs::symlink_metadata(&root) {
        Ok(_) => {
            if is_reparse_point(&root)? {
                return Err(format!(
                    "resolved profile root must not be a reparse point/junction: {}",
                    root.display()
                ));
            }
            if !root.is_dir() {
                return Err(format!(
                    "resolved profile root exists and is not a directory: {}",
                    root.display()
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(&root).map_err(|create_error| {
                format!(
                    "failed to create profile root {}: {create_error}",
                    root.display()
                )
            })?;
        }
        Err(error) => {
            return Err(format!(
                "failed to inspect profile root {}: {error}",
                root.display()
            ));
        }
    }

    let canonical_root = root.canonicalize().map_err(|error| {
        format!(
            "failed to canonicalize profile root {}: {error}",
            root.display()
        )
    })?;

    if !path_equals_or_beneath(&canonical_root, &args.config_base)
        || canonical_root == args.config_base
    {
        return Err(format!(
            "canonical profile root must remain a strict descendant of config base: {}",
            canonical_root.display()
        ));
    }

    let production_root = installed_production_root()?;
    if path_equals_or_beneath(&canonical_root, &production_root) {
        return Err(format!(
            "resolved profile root must not equal or lie beneath production root: {}",
            canonical_root.display()
        ));
    }

    Ok(PreparedDebugPaths {
        profile_root: canonical_root,
        database: paths.database,
    })
}

#[cfg(all(windows, debug_assertions))]
fn acquire_lock(profile_root: &Path, profile: &str) -> Result<HostLock, HostRunError> {
    match HostLock::acquire(profile_root, profile) {
        Ok(lock) => Ok(lock),
        Err(HostLockError::AlreadyRunning { .. }) => Err(HostRunError::AlreadyRunning(
            "another host already holds this profile lock".to_string(),
        )),
        Err(error) => Err(HostRunError::Message(error.to_string())),
    }
}

#[cfg(all(windows, debug_assertions))]
struct ParentProcess {
    handle: windows::Win32::Foundation::HANDLE,
}

#[cfg(all(windows, debug_assertions))]
impl Drop for ParentProcess {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

#[cfg(all(windows, debug_assertions))]
fn actual_parent_pid() -> Result<u32, String> {
    use sysinfo::{Pid, ProcessesToUpdate, System};

    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::All, true);
    let self_pid = Pid::from_u32(std::process::id());
    let process = system
        .process(self_pid)
        .ok_or_else(|| "unable to resolve current process via sysinfo".to_string())?;
    let parent = process
        .parent()
        .ok_or_else(|| "unable to resolve actual parent PID via sysinfo".to_string())?;
    Ok(parent.as_u32())
}

#[cfg(all(windows, debug_assertions))]
fn filetime_to_ticks(time: windows::Win32::Foundation::FILETIME) -> u64 {
    (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
}

#[cfg(all(windows, debug_assertions))]
fn creation_ticks(handle: windows::Win32::Foundation::HANDLE) -> Result<u64, String> {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::GetProcessTimes;

    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) }
        .map_err(|error| format!("GetProcessTimes failed: {error}"))?;
    let ticks = filetime_to_ticks(creation);
    if ticks == 0 {
        return Err("process creation FILETIME ticks unavailable".to_string());
    }
    Ok(ticks)
}

#[cfg(all(windows, debug_assertions))]
fn open_and_validate_parent(parent_pid: u32) -> Result<ParentProcess, String> {
    use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows::Win32::System::Threading::{
        GetCurrentProcess, GetProcessId, OpenProcess, WaitForSingleObject,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    };

    let actual = actual_parent_pid()?;
    if parent_pid != actual {
        return Err(format!(
            "supplied --parent-pid {parent_pid} does not match actual parent PID {actual}"
        ));
    }

    let access = PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION;
    let handle = unsafe { OpenProcess(access, false, parent_pid) }.map_err(|error| {
        format!("failed to open parent pid {parent_pid} with sync/query rights: {error}")
    })?;
    if handle.is_invalid() || handle == HANDLE::default() {
        return Err(format!(
            "failed to open parent pid {parent_pid}: invalid process handle"
        ));
    }
    let parent = ParentProcess { handle };

    let handle_pid = unsafe { GetProcessId(parent.handle) };
    if handle_pid != parent_pid {
        return Err(format!(
            "GetProcessId on parent handle returned {handle_pid}, expected {parent_pid}"
        ));
    }

    let parent_ticks = creation_ticks(parent.handle)?;
    let self_ticks = creation_ticks(unsafe { GetCurrentProcess() })?;
    if parent_ticks > self_ticks {
        return Err(format!(
            "parent creation ticks {parent_ticks} are after host creation ticks {self_ticks}"
        ));
    }

    let wait = unsafe { WaitForSingleObject(parent.handle, 0) };
    if wait == WAIT_OBJECT_0 {
        return Err("parent process has already exited".to_string());
    }
    if wait != WAIT_TIMEOUT {
        return Err(format!(
            "unexpected WaitForSingleObject(0) result while probing parent: {wait:?}"
        ));
    }

    Ok(parent)
}

#[cfg(all(windows, debug_assertions))]
fn parent_has_exited(parent: &ParentProcess) -> Result<bool, String> {
    use windows::Win32::Foundation::{WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows::Win32::System::Threading::WaitForSingleObject;

    let result = unsafe { WaitForSingleObject(parent.handle, 0) };
    if result == WAIT_OBJECT_0 {
        Ok(true)
    } else if result == WAIT_TIMEOUT {
        Ok(false)
    } else if result == WAIT_FAILED {
        Err("WaitForSingleObject on parent process failed".to_string())
    } else {
        Err(format!(
            "unexpected WaitForSingleObject result for parent process: {result:?}"
        ))
    }
}

#[cfg(all(windows, debug_assertions))]
async fn wait_for_parent_exit(parent: &ParentProcess) -> Result<(), String> {
    loop {
        if parent_has_exited(parent)? {
            return Ok(());
        }
        tokio::time::sleep(PARENT_POLL_INTERVAL).await;
    }
}

#[cfg(all(windows, debug_assertions))]
fn join_error_message(context: &str, error: tokio::task::JoinError) -> String {
    if error.is_panic() {
        format!("{context} panicked")
    } else if error.is_cancelled() {
        format!("{context} was cancelled")
    } else {
        format!("{context} failed: {error}")
    }
}

#[cfg(all(windows, debug_assertions))]
async fn abort_and_drain_connection_tasks(
    tasks: &mut tokio::task::JoinSet<()>,
) -> Result<(), String> {
    tasks.abort_all();
    let mut first_error = None;
    while let Some(result) = tasks.join_next().await {
        match result {
            Ok(()) => {}
            Err(error) if error.is_cancelled() => {}
            Err(error) if first_error.is_none() => {
                first_error = Some(join_error_message("connection task", error));
            }
            Err(_) => {}
        }
    }
    first_error.map_or(Ok(()), Err)
}

#[cfg(all(windows, debug_assertions))]
async fn stop_request_executor(executor_task: tokio::task::JoinHandle<()>) -> Result<(), String> {
    // Cancel the executor at its next await point, then reap it before the host
    // releases ownership. Synchronous dispatch remains atomic on this runtime.
    executor_task.abort();
    match executor_task.await {
        Ok(()) => Ok(()),
        Err(error) if error.is_cancelled() => Ok(()),
        Err(error) => Err(join_error_message("command-bus executor", error)),
    }
}

#[cfg(all(windows, debug_assertions))]
enum HostLoopExit {
    Parent(Result<(), String>),
    Listener(String),
    Executor(Result<(), tokio::task::JoinError>),
    Connection(tokio::task::JoinError),
}

#[cfg(all(windows, debug_assertions))]
async fn finish_supervised_host(
    exit: HostLoopExit,
    connection_tasks: &mut tokio::task::JoinSet<()>,
    request_handle: HostRequestHandle,
    executor_task: tokio::task::JoinHandle<()>,
) -> Result<(), String> {
    let executor_already_joined = matches!(&exit, HostLoopExit::Executor(_));
    let mut errors = Vec::new();
    match exit {
        HostLoopExit::Parent(Ok(())) => {}
        HostLoopExit::Parent(Err(error)) | HostLoopExit::Listener(error) => errors.push(error),
        HostLoopExit::Executor(Ok(())) => {
            errors.push("command-bus executor exited unexpectedly".to_string())
        }
        HostLoopExit::Executor(Err(error)) => {
            errors.push(join_error_message("command-bus executor", error))
        }
        HostLoopExit::Connection(error) => {
            errors.push(join_error_message("connection task", error))
        }
    }

    if let Err(error) = abort_and_drain_connection_tasks(connection_tasks).await {
        errors.push(error);
    }
    // Connection tasks are now gone, so dropping the root handle closes the
    // request queue without leaving any sender clone alive.
    drop(request_handle);
    if executor_already_joined {
        drop(executor_task);
    } else if let Err(error) = stop_request_executor(executor_task).await {
        errors.push(error);
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

#[cfg(all(windows, debug_assertions))]
fn spawn_connection_task(
    tasks: &mut tokio::task::JoinSet<()>,
    connection: HostConnection,
    requests: HostRequestHandle,
    slow_durable_reader_client_id: Option<ClientId>,
) {
    tasks.spawn(async move {
        // Duplex serve owns reader+writer halves until disconnect; abort/drain
        // of this JoinSet task reaps both halves with the connection lifecycle.
        let matched_slow = slow_durable_reader_client_id == Some(connection.client_id());
        if matched_slow {
            let _ = connection
                .serve_duplex_for_test_slow_durable_reader(requests)
                .await;
        } else {
            let _ = connection.serve_duplex(requests).await;
        }
    });
}

#[cfg(all(windows, debug_assertions))]
async fn serve_foreground_host(
    profile: &str,
    parent: ParentProcess,
    host_boot_id: Uuid,
    bus: CommandBus,
    slow_durable_reader_client_id: Option<ClientId>,
) -> Result<(), String> {
    let hello_config = AcceptHelloConfig {
        host_boot_id,
        server_build: format!("devmanager-host/{}", env!("CARGO_PKG_VERSION")),
        supported: CapabilitySet::from_capabilities([
            Capability::PagedSnapshots,
            Capability::EventReplay,
            Capability::OperationSettlement,
            Capability::ChunkResume,
            Capability::ExplicitDetach,
        ]),
        local_limits: FrameLimits::v1_default(),
    };

    // The first instance proves no pre-existing pipe server is present. Each
    // later instance is created by this same lock owner before the connected
    // instance is handed to a connection task, so clients never depend on a
    // close/rebind gap.
    let listener = HelloListener::bind(profile, hello_config)
        .map_err(|error| format!("failed to bind host pipe: {error}"))?;

    // One CommandBus executor owns all mutate/query dispatch. Transport tasks
    // only submit through HostRequestHandle.
    let (request_handle, mut executor_task) = HostRequestExecutor::start(bus);
    let mut connection_tasks = tokio::task::JoinSet::new();
    // `accept_with_successor` owns its listener. Keep the future pinned across
    // unrelated task-completion branches so a normal client disconnect never
    // cancels and drops the sole pending listener.
    let mut accept_task = Box::pin(listener.accept_with_successor());

    let exit = loop {
        tokio::select! {
            parent_result = wait_for_parent_exit(&parent) => {
                break HostLoopExit::Parent(parent_result);
            }
            executor_result = &mut executor_task => {
                break HostLoopExit::Executor(executor_result);
            }
            connection_result = connection_tasks.join_next(), if !connection_tasks.is_empty() => {
                match connection_result {
                    Some(Ok(())) => {}
                    Some(Err(error)) => break HostLoopExit::Connection(error),
                    None => {}
                }
            }
            accepted_result = &mut accept_task => {
                let (accepted, next_listener) = match accepted_result {
                    Ok(pair) => pair,
                    Err(error) => {
                        break HostLoopExit::Listener(format!(
                            "failed to preserve host pipe listener: {error}"
                        ));
                    }
                };
                accept_task = Box::pin(next_listener.accept_with_successor());
                // Handshake failures belong only to the attempted connection.
                // The secured successor remains pending for another client.
                if let Ok(connection) = accepted {
                    spawn_connection_task(
                        &mut connection_tasks,
                        connection,
                        request_handle.clone(),
                        slow_durable_reader_client_id,
                    );
                }
            }
        }
    };

    finish_supervised_host(exit, &mut connection_tasks, request_handle, executor_task).await
}

#[cfg(all(windows, debug_assertions, test))]
mod tests {
    use super::{
        parse_args, path_strictly_beneath, validate_slow_durable_reader_isolation, HostArgs,
    };
    use tempfile::TempDir;

    fn lifecycle_base_args(config_base: &std::path::Path, profile: &str) -> Vec<String> {
        let parent_pid = match std::process::id() {
            1 => 2,
            other => other.saturating_sub(1).max(1),
        };
        vec![
            "--foreground".into(),
            "--profile".into(),
            profile.into(),
            "--instance-label".into(),
            "Lifecycle Test".into(),
            "--parent-pid".into(),
            parent_pid.to_string(),
            "--config-base".into(),
            config_base.display().to_string(),
        ]
    }

    #[test]
    fn validate_slow_durable_reader_isolation_accepts_temp_child() {
        let config_base = TempDir::new().expect("process-unique config base");
        let canonical = config_base
            .path()
            .canonicalize()
            .expect("canonicalize temp child");
        validate_slow_durable_reader_isolation("Lifecycle Test", "lifecyclefixture", &canonical)
            .expect("temp child must be accepted");
        assert!(path_strictly_beneath(
            &canonical,
            &std::env::temp_dir().canonicalize().unwrap()
        ));
    }

    #[test]
    fn validate_slow_durable_reader_isolation_rejects_wrong_label() {
        let config_base = TempDir::new().expect("process-unique config base");
        let canonical = config_base
            .path()
            .canonicalize()
            .expect("canonicalize temp child");
        let error =
            validate_slow_durable_reader_isolation("Other Label", "lifecyclefixture", &canonical)
                .expect_err("wrong label must fail");
        assert!(
            error.contains("Lifecycle Test"),
            "error must name the required label: {error}"
        );
    }

    #[test]
    fn validate_slow_durable_reader_isolation_rejects_wrong_profile() {
        let config_base = TempDir::new().expect("process-unique config base");
        let canonical = config_base
            .path()
            .canonicalize()
            .expect("canonicalize temp child");
        let error =
            validate_slow_durable_reader_isolation("Lifecycle Test", "otherprofile", &canonical)
                .expect_err("wrong profile must fail");
        assert!(
            error.contains("lifecycle"),
            "error must name the required profile prefix: {error}"
        );
    }

    #[test]
    fn validate_slow_durable_reader_isolation_rejects_temp_root_itself() {
        let temp_root = std::env::temp_dir()
            .canonicalize()
            .expect("canonicalize system temp root");
        let error = validate_slow_durable_reader_isolation(
            "Lifecycle Test",
            "lifecyclefixture",
            &temp_root,
        )
        .expect_err("temp root itself must fail");
        assert!(
            error.contains("strict descendant"),
            "error must require a strict descendant: {error}"
        );
    }

    #[test]
    fn parse_args_rejects_missing_slow_durable_reader_client_id_value() {
        let config_base = TempDir::new().expect("process-unique config base");
        let mut args = lifecycle_base_args(config_base.path(), "lifecycleparse");
        args.push("--test-slow-durable-reader-client-id".into());
        let error = parse_args(args).expect_err("missing flag value must fail");
        assert!(
            error.contains("missing value for --test-slow-durable-reader-client-id"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn parse_args_rejects_invalid_slow_durable_reader_client_id() {
        let config_base = TempDir::new().expect("process-unique config base");
        let mut args = lifecycle_base_args(config_base.path(), "lifecycleparse");
        args.push("--test-slow-durable-reader-client-id".into());
        args.push("not-a-uuidv7".into());
        let error = parse_args(args).expect_err("invalid UUIDv7 must fail");
        assert!(
            error.contains("invalid --test-slow-durable-reader-client-id"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn parse_args_rejects_duplicate_slow_durable_reader_client_id() {
        let config_base = TempDir::new().expect("process-unique config base");
        let client_id = "018f60b0-9c1a-7001-8000-0000000000c1";
        let mut args = lifecycle_base_args(config_base.path(), "lifecycleparse");
        args.push("--test-slow-durable-reader-client-id".into());
        args.push(client_id.into());
        args.push("--test-slow-durable-reader-client-id".into());
        args.push(client_id.into());
        let error = parse_args(args).expect_err("duplicate flag must fail");
        assert!(
            error.contains("duplicate --test-slow-durable-reader-client-id"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn parse_args_accepts_slow_durable_reader_client_id_under_lifecycle_shape() {
        let config_base = TempDir::new().expect("process-unique config base");
        let client_id = "018f60b0-9c1a-7001-8000-0000000000c1";
        let mut args = lifecycle_base_args(config_base.path(), "lifecycleparse");
        args.push("--test-slow-durable-reader-client-id".into());
        args.push(client_id.into());
        let parsed: HostArgs = parse_args(args).expect("valid lifecycle shape must parse");
        assert_eq!(
            parsed
                .test_slow_durable_reader_client_id
                .expect("flag retained")
                .to_string(),
            client_id
        );
        assert!(parsed.config_base.is_absolute());
    }
}
