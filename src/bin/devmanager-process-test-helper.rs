use std::fs;
use std::io::{self, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use devmanager::process::job::ManagedProcessJob;
use serde_json::{json, Value};

const NATURAL_EXIT_BOUND: Duration = Duration::from_secs(20);
const DEFAULT_BOUNDED_DURATION_MS: u64 = 100;
const MAX_BOUNDED_DURATION_MS: u64 = 30_000;
const DEFAULT_OUTPUT_BYTES: usize = 4 * 1024;
const MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024;
const MAX_MEMORY_BYTES: usize = 256 * 1024 * 1024;
const DEFAULT_FORK_CHILDREN: u32 = 1;
const MAX_FORK_CHILDREN: u32 = 1024;

fn write_marker(path: &Path, value: impl AsRef<[u8]>) {
    fs::write(path, value).expect("write process-test marker");
}

fn wait_naturally() {
    std::thread::sleep(NATURAL_EXIT_BOUND);
}

fn spawn_marker_child(marker: &Path) -> Child {
    Command::new(std::env::current_exe().expect("test-helper executable"))
        .arg("mark-wait")
        .arg(marker)
        .spawn()
        .expect("spawn marker child")
}

fn mark_and_wait(marker: &Path) {
    write_marker(marker, b"started");
    wait_naturally();
}

struct ChildTreeGuard {
    children: Vec<Child>,
    jobs: Vec<ManagedProcessJob>,
}

impl ChildTreeGuard {
    fn new() -> Self {
        Self {
            children: Vec::new(),
            jobs: Vec::new(),
        }
    }

    fn push(&mut self, mut child: Child) -> Result<(), String> {
        let job = match devmanager::process::job::attach_process_to_managed_job(child.id()) {
            Ok(job) => job,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "attach child {} to kill-on-close Job Object: {error}",
                    child.id()
                ));
            }
        };
        if let Some(job) = job {
            self.jobs.push(job);
        }
        self.children.push(child);
        Ok(())
    }
}

impl Drop for ChildTreeGuard {
    fn drop(&mut self) {
        for child in &mut self.children {
            if !matches!(child.try_wait(), Ok(Some(_))) {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

fn spawn_child_and_wait(
    root_marker: &Path,
    child_marker: &Path,
    child_pid: &Path,
) -> Result<(), String> {
    write_marker(root_marker, b"started");
    let mut guard = ChildTreeGuard::new();
    let child = spawn_marker_child(child_marker);
    let child_id = child.id();
    guard.push(child)?;
    write_marker(child_pid, child_id.to_string());
    let child = guard
        .children
        .first_mut()
        .ok_or_else(|| "child guard lost spawned child".to_string())?;
    child
        .wait()
        .map_err(|error| format!("wait marker child: {error}"))?;
    guard.children.clear();
    Ok(())
}

#[cfg(windows)]
fn attempt_breakaway(result: &Path, escaped_marker: &Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;

    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    let spawn = Command::new(std::env::current_exe().expect("test-helper executable"))
        .arg("mark-wait")
        .arg(escaped_marker)
        .creation_flags(CREATE_BREAKAWAY_FROM_JOB)
        .spawn();
    match spawn {
        Ok(escaped) => {
            let escaped_id = escaped.id();
            let mut guard = ChildTreeGuard::new();
            guard.push(escaped)?;
            write_marker(result, format!("escaped:{escaped_id}"));
            let escaped = guard
                .children
                .first_mut()
                .ok_or_else(|| "child guard lost breakaway child".to_string())?;
            let _ = escaped.kill();
            let _ = escaped.wait();
            guard.children.clear();
        }
        Err(error) => write_marker(result, format!("blocked:{:?}", error.kind())),
    }
    wait_naturally();
    Ok(())
}

#[cfg(not(windows))]
fn attempt_breakaway(result: &Path, _escaped_marker: &Path) -> Result<(), String> {
    write_marker(result, b"unsupported");
    Ok(())
}

fn required_path(
    args: &mut impl Iterator<Item = std::ffi::OsString>,
    name: &str,
) -> std::path::PathBuf {
    args.next()
        .map(Into::into)
        .unwrap_or_else(|| panic!("missing {name}"))
}

#[derive(Debug, Clone, Copy)]
struct BoundedOptions {
    duration_ms: u64,
    bytes: usize,
    children: u32,
    port: u16,
    watchdog_ms: Option<u64>,
}

impl Default for BoundedOptions {
    fn default() -> Self {
        Self {
            duration_ms: DEFAULT_BOUNDED_DURATION_MS,
            bytes: DEFAULT_OUTPUT_BYTES,
            children: DEFAULT_FORK_CHILDREN,
            port: 0,
            watchdog_ms: None,
        }
    }
}

fn parse_number<T>(value: &str, name: &str) -> Result<T, String>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    value
        .parse()
        .map_err(|error| format!("invalid {name} '{value}': {error}"))
}

fn parse_bounded_options(
    args: impl Iterator<Item = std::ffi::OsString>,
) -> Result<BoundedOptions, String> {
    let mut options = BoundedOptions::default();
    let mut args = args.peekable();
    while let Some(argument) = args.next() {
        let argument = argument
            .into_string()
            .map_err(|_| "helper arguments must be UTF-8".to_string())?;
        let (name, inline_value) = argument
            .split_once('=')
            .map_or((argument.as_str(), None), |(name, value)| {
                (name, Some(value))
            });
        let value = match name {
            "--duration-ms" => Some(inline_value.map(str::to_owned).unwrap_or_else(|| {
                args.next()
                    .and_then(|value| value.into_string().ok())
                    .unwrap_or_default()
            })),
            "--bytes" => Some(inline_value.map(str::to_owned).unwrap_or_else(|| {
                args.next()
                    .and_then(|value| value.into_string().ok())
                    .unwrap_or_default()
            })),
            "--children" => Some(inline_value.map(str::to_owned).unwrap_or_else(|| {
                args.next()
                    .and_then(|value| value.into_string().ok())
                    .unwrap_or_default()
            })),
            "--port" => Some(inline_value.map(str::to_owned).unwrap_or_else(|| {
                args.next()
                    .and_then(|value| value.into_string().ok())
                    .unwrap_or_default()
            })),
            "--watchdog-ms" => Some(inline_value.map(str::to_owned).unwrap_or_else(|| {
                args.next()
                    .and_then(|value| value.into_string().ok())
                    .unwrap_or_default()
            })),
            other => return Err(format!("unknown bounded helper argument '{other}'")),
        };
        let value = value.expect("bounded helper option value");
        if value.is_empty() {
            return Err(format!("missing value for {name}"));
        }
        match name {
            "--duration-ms" => options.duration_ms = parse_number(&value, "duration-ms")?,
            "--bytes" => options.bytes = parse_number(&value, "bytes")?,
            "--children" => options.children = parse_number(&value, "children")?,
            "--port" => options.port = parse_number(&value, "port")?,
            "--watchdog-ms" => options.watchdog_ms = Some(parse_number(&value, "watchdog-ms")?),
            _ => unreachable!("bounded helper option was validated above"),
        }
    }

    if options.duration_ms == 0 || options.duration_ms > MAX_BOUNDED_DURATION_MS {
        return Err(format!(
            "duration-ms must be between 1 and {MAX_BOUNDED_DURATION_MS}"
        ));
    }
    if options.children == 0 || options.children > MAX_FORK_CHILDREN {
        return Err(format!(
            "children must be between 1 and {MAX_FORK_CHILDREN}"
        ));
    }
    if let Some(watchdog_ms) = options.watchdog_ms {
        if watchdog_ms == 0 || watchdog_ms > MAX_BOUNDED_DURATION_MS {
            return Err(format!(
                "watchdog-ms must be between 1 and {MAX_BOUNDED_DURATION_MS}"
            ));
        }
    }
    Ok(options)
}

fn helper_identity(pid: u32) -> String {
    format!("devmanager-process-test-helper:{pid}")
}

fn emit_event(mode: &str, event: &str, fields: &[(&str, Value)]) {
    let mut object = serde_json::Map::new();
    object.insert("schemaVersion".to_string(), json!(1));
    object.insert("event".to_string(), json!(event));
    object.insert("mode".to_string(), json!(mode));
    object.insert("pid".to_string(), json!(std::process::id()));
    object.insert(
        "identity".to_string(),
        json!(helper_identity(std::process::id())),
    );
    for (key, value) in fields {
        object.insert((*key).to_string(), value.clone());
    }
    println!(
        "{}",
        serde_json::to_string(&Value::Object(object)).expect("serialize helper evidence")
    );
    io::stdout().flush().expect("flush helper evidence");
}

fn emit_error(mode: &str, error: &str) {
    let mut object = serde_json::Map::new();
    object.insert("schemaVersion".to_string(), json!(1));
    object.insert("event".to_string(), json!("error"));
    object.insert("mode".to_string(), json!(mode));
    object.insert("pid".to_string(), json!(std::process::id()));
    object.insert(
        "identity".to_string(),
        json!(helper_identity(std::process::id())),
    );
    object.insert("error".to_string(), json!(error));
    eprintln!(
        "{}",
        serde_json::to_string(&Value::Object(object)).expect("serialize helper error")
    );
}

fn wait_bounded(duration_ms: u64) {
    std::thread::sleep(Duration::from_millis(duration_ms));
}

struct OutputWatchdog {
    cancelled: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl OutputWatchdog {
    fn start(timeout_ms: u64) -> Self {
        let cancelled = Arc::new(AtomicBool::new(false));
        let thread_cancelled = Arc::clone(&cancelled);
        let thread = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_millis(timeout_ms);
            while !thread_cancelled.load(Ordering::Acquire) {
                if Instant::now() >= deadline {
                    eprintln!("large-output watchdog expired after {timeout_ms}ms");
                    std::process::exit(124);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        Self {
            cancelled,
            thread: Some(thread),
        }
    }
}

impl Drop for OutputWatchdog {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run_rapid_fork_exit(options: BoundedOptions) -> Result<(), String> {
    emit_event(
        "rapid-fork-exit",
        "ready",
        &[
            ("durationMs", json!(options.duration_ms)),
            ("maxDurationMs", json!(MAX_BOUNDED_DURATION_MS)),
            ("children", json!(options.children)),
            ("maxChildren", json!(MAX_FORK_CHILDREN)),
        ],
    );
    let mut guard = ChildTreeGuard::new();
    for _ in 0..options.children {
        let child = Command::new(std::env::current_exe().map_err(|error| error.to_string())?)
            .arg("rapid-fork-exit-worker")
            .arg("--duration-ms")
            .arg("1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("spawn rapid-fork worker: {error}"))?;
        guard.push(child)?;
    }
    for child in &mut guard.children {
        let status = child
            .wait()
            .map_err(|error| format!("wait rapid-fork worker: {error}"))?;
        if !status.success() {
            return Err(format!("rapid-fork worker exited as {status}"));
        }
    }
    guard.children.clear();
    wait_bounded(options.duration_ms);
    emit_event(
        "rapid-fork-exit",
        "done",
        &[
            ("exit", json!("natural")),
            ("durationMs", json!(options.duration_ms)),
            ("children", json!(options.children)),
        ],
    );
    Ok(())
}

fn run_rapid_fork_worker(options: BoundedOptions) -> Result<(), String> {
    wait_bounded(options.duration_ms);
    Ok(())
}

fn run_large_output(options: BoundedOptions) -> Result<(), String> {
    if options.bytes == 0 || options.bytes > MAX_OUTPUT_BYTES {
        return Err(format!(
            "bytes must be between 1 and {MAX_OUTPUT_BYTES} for large-output"
        ));
    }
    let watchdog = options.watchdog_ms.map(OutputWatchdog::start);
    emit_event(
        "large-output",
        "ready",
        &[
            ("durationMs", json!(options.duration_ms)),
            ("maxDurationMs", json!(MAX_BOUNDED_DURATION_MS)),
            ("bytes", json!(options.bytes)),
            ("maxBytes", json!(MAX_OUTPUT_BYTES)),
        ],
    );
    let mut output = io::stdout().lock();
    let mut remaining = options.bytes.saturating_sub(1);
    let chunk = [b'x'; 64 * 1024];
    while remaining > 0 {
        let amount = remaining.min(chunk.len());
        output
            .write_all(&chunk[..amount])
            .map_err(|error| format!("write bounded output: {error}"))?;
        remaining -= amount;
    }
    output
        .write_all(b"\n")
        .map_err(|error| format!("terminate bounded output: {error}"))?;
    output
        .flush()
        .map_err(|error| format!("flush bounded output: {error}"))?;
    wait_bounded(options.duration_ms);
    drop(output);
    emit_event(
        "large-output",
        "done",
        &[
            ("exit", json!("natural")),
            ("durationMs", json!(options.duration_ms)),
            ("outputBytes", json!(options.bytes)),
        ],
    );
    drop(watchdog);
    Ok(())
}

fn run_ignored_cooperative_close(options: BoundedOptions) -> Result<(), String> {
    emit_event(
        "ignored-cooperative-close",
        "ready",
        &[
            ("durationMs", json!(options.duration_ms)),
            ("maxDurationMs", json!(MAX_BOUNDED_DURATION_MS)),
            ("cooperativeClose", json!("ignored")),
        ],
    );
    wait_bounded(options.duration_ms);
    emit_event(
        "ignored-cooperative-close",
        "done",
        &[
            ("exit", json!("natural")),
            ("durationMs", json!(options.duration_ms)),
            ("cooperativeClose", json!("ignored")),
        ],
    );
    Ok(())
}

fn run_grandchild_lifetime(options: BoundedOptions) -> Result<(), String> {
    let child = Command::new(std::env::current_exe().map_err(|error| error.to_string())?)
        .arg("grandchild-lifetime-worker")
        .arg("--duration-ms")
        .arg(options.duration_ms.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("spawn grandchild lifetime worker: {error}"))?;
    let child_pid = child.id();
    let child_identity = helper_identity(child_pid);
    let mut guard = ChildTreeGuard::new();
    guard.push(child)?;
    emit_event(
        "grandchild-lifetime",
        "ready",
        &[
            ("durationMs", json!(options.duration_ms)),
            ("maxDurationMs", json!(MAX_BOUNDED_DURATION_MS)),
            ("childPid", json!(child_pid)),
            ("childIdentity", json!(&child_identity)),
        ],
    );
    let status = guard
        .children
        .first_mut()
        .ok_or_else(|| "child guard lost grandchild worker".to_string())?
        .wait()
        .map_err(|error| format!("wait grandchild lifetime worker: {error}"))?;
    if !status.success() {
        return Err(format!("grandchild lifetime worker exited as {status}"));
    }
    emit_event(
        "grandchild-lifetime",
        "done",
        &[
            ("exit", json!("natural")),
            ("durationMs", json!(options.duration_ms)),
            ("childPid", json!(child_pid)),
            ("childIdentity", json!(&child_identity)),
        ],
    );
    guard.children.clear();
    Ok(())
}

fn run_bounded_cpu_load(options: BoundedOptions) -> Result<(), String> {
    emit_event(
        "bounded-cpu-load",
        "ready",
        &[
            ("durationMs", json!(options.duration_ms)),
            ("maxDurationMs", json!(MAX_BOUNDED_DURATION_MS)),
        ],
    );
    let deadline = Instant::now() + Duration::from_millis(options.duration_ms);
    let mut state = 0x9e37_79b9_u64;
    let mut work_units = 0_u64;
    while Instant::now() < deadline {
        state ^= state << 7;
        state ^= state >> 9;
        state = state.wrapping_mul(0x9e37_79b9);
        work_units = work_units.wrapping_add(1);
        if work_units % 4096 == 0 {
            std::hint::black_box(state);
        }
    }
    std::hint::black_box(state);
    emit_event(
        "bounded-cpu-load",
        "done",
        &[
            ("exit", json!("natural")),
            ("durationMs", json!(options.duration_ms)),
            ("workUnits", json!(work_units)),
        ],
    );
    Ok(())
}

fn run_bounded_memory_load(options: BoundedOptions) -> Result<(), String> {
    if options.bytes == 0 || options.bytes > MAX_MEMORY_BYTES {
        return Err(format!(
            "bytes must be between 1 and {MAX_MEMORY_BYTES} for bounded-memory-load"
        ));
    }
    let mut allocation = Vec::new();
    allocation
        .try_reserve_exact(options.bytes)
        .map_err(|error| format!("reserve bounded memory load: {error}"))?;
    allocation.resize(options.bytes, 0_u8);
    for index in (0..allocation.len()).step_by(4096) {
        allocation[index] = (index as u8).wrapping_add(1);
    }
    emit_event(
        "bounded-memory-load",
        "ready",
        &[
            ("durationMs", json!(options.duration_ms)),
            ("maxDurationMs", json!(MAX_BOUNDED_DURATION_MS)),
            ("bytes", json!(options.bytes)),
            ("maxBytes", json!(MAX_MEMORY_BYTES)),
        ],
    );
    wait_bounded(options.duration_ms);
    std::hint::black_box(&allocation);
    emit_event(
        "bounded-memory-load",
        "done",
        &[
            ("exit", json!("natural")),
            ("durationMs", json!(options.duration_ms)),
            ("bytes", json!(options.bytes)),
        ],
    );
    drop(allocation);
    Ok(())
}

fn run_loopback_listener(options: BoundedOptions) -> Result<(), String> {
    let listener = TcpListener::bind(("127.0.0.1", options.port))
        .map_err(|error| format!("bind loopback listener: {error}"))?;
    let port = listener
        .local_addr()
        .map_err(|error| format!("read loopback listener address: {error}"))?
        .port();
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("set loopback listener nonblocking: {error}"))?;
    emit_event(
        "loopback-listener",
        "ready",
        &[
            ("durationMs", json!(options.duration_ms)),
            ("maxDurationMs", json!(MAX_BOUNDED_DURATION_MS)),
            ("address", json!(format!("127.0.0.1:{port}"))),
            ("port", json!(port)),
        ],
    );
    let deadline = Instant::now() + Duration::from_millis(options.duration_ms);
    while Instant::now() < deadline {
        match listener.accept() {
            Ok((stream, _)) => drop(stream),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(format!("accept loopback connection: {error}")),
        }
    }
    drop(listener);
    emit_event(
        "loopback-listener",
        "done",
        &[
            ("exit", json!("natural")),
            ("durationMs", json!(options.duration_ms)),
            ("address", json!(format!("127.0.0.1:{port}"))),
            ("port", json!(port)),
        ],
    );
    Ok(())
}

fn main() {
    let mut args = std::env::args_os().skip(1);
    let mode = args.next().expect("process-test helper mode");
    let mode = mode.to_string_lossy().to_string();
    let result = match mode.as_str() {
        "mark-wait" => {
            mark_and_wait(&required_path(&mut args, "marker path"));
            Ok(())
        }
        "spawn-child" => spawn_child_and_wait(
            &required_path(&mut args, "root marker path"),
            &required_path(&mut args, "child marker path"),
            &required_path(&mut args, "child PID path"),
        ),
        "attempt-breakaway" => attempt_breakaway(
            &required_path(&mut args, "breakaway result path"),
            &required_path(&mut args, "escaped marker path"),
        ),
        "rapid-fork-exit" | "fork-exit" | "rapid-fork" => {
            parse_bounded_options(args).and_then(run_rapid_fork_exit)
        }
        "rapid-fork-exit-worker" => parse_bounded_options(args).and_then(run_rapid_fork_worker),
        "large-output" | "bounded-large-output" => {
            parse_bounded_options(args).and_then(run_large_output)
        }
        "ignored-cooperative-close" | "ignored-close" | "cooperative-close" => {
            parse_bounded_options(args).and_then(run_ignored_cooperative_close)
        }
        "grandchild-lifetime" | "grandchild" => {
            parse_bounded_options(args).and_then(run_grandchild_lifetime)
        }
        "grandchild-lifetime-worker" => parse_bounded_options(args).and_then(run_rapid_fork_worker),
        "bounded-cpu-load" | "cpu-load" => {
            parse_bounded_options(args).and_then(run_bounded_cpu_load)
        }
        "bounded-memory-load" | "memory-load" => {
            parse_bounded_options(args).and_then(run_bounded_memory_load)
        }
        "loopback-listener" | "listen-port" | "port-listener" | "loopback-port" => {
            parse_bounded_options(args).and_then(run_loopback_listener)
        }
        other => Err(format!("unknown process-test helper mode: {other}")),
    };
    if let Err(error) = result {
        emit_error(&mode, &error);
        std::process::exit(2);
    }
}
