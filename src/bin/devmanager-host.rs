//! Development foreground `devmanager-host` entry for Phase 2.1 ownership.
//!
//! This binary proves real host-process identity, HostLock ownership, and
//! parent-process lifetime binding. It does not bind IPC, open the kernel
//! database, or own Phase 3 resources.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use devmanager::config::paths::{resolve_app_paths, AppProfile, BuildKind};
use devmanager::host::{HostLock, HostLockError, HOST_EXIT_ALREADY_RUNNING};

const MAX_INSTANCE_LABEL_CHARS: usize = 64;

#[derive(Debug)]
struct HostArgs {
    profile: String,
    #[allow(dead_code)]
    instance_label: String,
    parent_pid: u32,
    config_base: PathBuf,
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
    match run() {
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

fn run() -> Result<(), HostRunError> {
    #[cfg(not(debug_assertions))]
    {
        return Err(HostRunError::Message(
            "release host startup is deferred until Phase 11".to_string(),
        ));
    }

    #[cfg(not(windows))]
    {
        return Err(HostRunError::Message(
            "devmanager-host requires Windows".to_string(),
        ));
    }

    #[cfg(all(windows, debug_assertions))]
    {
        let args = parse_args(std::env::args().skip(1))?;
        let profile_root = prepare_debug_profile_root(&args)?;
        let parent = open_and_validate_parent(args.parent_pid)?;
        let _lock = acquire_lock(&profile_root, &args.profile)?;
        // Readiness for this ownership-only slice is the valid lock identity.
        let _ = &args.instance_label;
        wait_for_parent(parent)?;
        Ok(())
    }
}

#[cfg(all(windows, debug_assertions))]
fn parse_args<I>(raw: I) -> Result<HostArgs, String>
where
    I: IntoIterator<Item = String>,
{
    let mut foreground = false;
    let mut profile: Option<String> = None;
    let mut instance_label: Option<String> = None;
    let mut parent_pid: Option<u32> = None;
    let mut config_base: Option<PathBuf> = None;

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

    Ok(HostArgs {
        profile,
        instance_label,
        parent_pid,
        config_base,
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
fn prepare_debug_profile_root(args: &HostArgs) -> Result<PathBuf, String> {
    let profile = AppProfile::named(&args.profile).map_err(|error| error.to_string())?;
    let paths = resolve_app_paths(&args.config_base, profile, BuildKind::Debug)
        .map_err(|error| error.to_string())?;
    let root = paths.root;

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

    Ok(canonical_root)
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
fn wait_for_parent(parent: ParentProcess) -> Result<(), String> {
    use windows::Win32::Foundation::{WAIT_FAILED, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{WaitForSingleObject, INFINITE};

    let result = unsafe { WaitForSingleObject(parent.handle, INFINITE) };
    if result == WAIT_OBJECT_0 {
        Ok(())
    } else if result == WAIT_FAILED {
        Err("WaitForSingleObject on parent process failed".to_string())
    } else {
        Err(format!(
            "unexpected WaitForSingleObject result for parent process: {result:?}"
        ))
    }
}
