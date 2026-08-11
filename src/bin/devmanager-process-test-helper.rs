use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

use devmanager::process::job::ManagedProcessJob;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const NATURAL_EXIT_BOUND: Duration = Duration::from_secs(20);
const DEFAULT_BOUNDED_DURATION_MS: u64 = 100;
const MAX_BOUNDED_DURATION_MS: u64 = 30_000;
const DEFAULT_OUTPUT_BYTES: usize = 4 * 1024;
const MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024;
const MAX_MEMORY_BYTES: usize = 256 * 1024 * 1024;
const DEFAULT_FORK_CHILDREN: u32 = 1;
const MAX_FORK_CHILDREN: u32 = 1024;
const SUPERVISOR_SCHEMA_VERSION: u32 = 1;
const SUPERVISOR_MAX_ITERATIONS: u32 = 100;
const SUPERVISOR_MAX_SCENARIOS: usize = 32;
const SUPERVISOR_MAX_ARGUMENTS: usize = 32;
const SUPERVISOR_MAX_ARGUMENT_BYTES: usize = 512;
const SUPERVISOR_MAX_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
const SUPERVISOR_MAX_RESULT_BYTES: usize = 256 * 1024;

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
        // Dropping the owned Job closes its kill-on-close fence. The Child
        // handles are then joined for deterministic fixture settlement; no
        // raw PID termination is used as a fallback.
        self.jobs.clear();
        for child in &mut self.children {
            let _ = child.wait();
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
            guard.jobs.clear();
            let escaped = guard
                .children
                .first_mut()
                .ok_or_else(|| "child guard lost breakaway child".to_string())?;
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

fn burn_bounded_cpu(duration_ms: u64) {
    let deadline = Instant::now() + Duration::from_millis(duration_ms.min(100));
    let mut state = 0x517c_c1b7_u64;
    while Instant::now() < deadline {
        state ^= state << 11;
        state ^= state >> 7;
        state = state.wrapping_mul(0x9e37_79b9);
        std::hint::black_box(state);
    }
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

fn emit_cycle_result(value: &Value) -> Result<(), String> {
    let encoded =
        serde_json::to_vec(value).map_err(|error| format!("serialize cycle result: {error}"))?;
    io::stdout()
        .write_all(&encoded)
        .map_err(|error| format!("write cycle result: {error}"))?;
    io::stdout()
        .write_all(b"\n")
        .map_err(|error| format!("write cycle result terminator: {error}"))?;
    io::stdout()
        .flush()
        .map_err(|error| format!("flush cycle result: {error}"))
}

fn run_cycle_worker(options: BoundedOptions) -> Result<(), String> {
    wait_bounded(options.duration_ms);
    Ok(())
}

fn run_cycle(arguments: impl Iterator<Item = std::ffi::OsString>) -> Result<(), String> {
    let mut arguments = arguments;
    let scenario = arguments
        .next()
        .ok_or_else(|| "cycle scenario is required".to_string())?
        .into_string()
        .map_err(|_| "cycle scenario must be UTF-8".to_string())?;
    let mut bounded_arguments = Vec::new();
    let mut seed = 0u64;
    let mut iteration = 0u32;
    let mut saw_iteration = false;
    let mut arguments = arguments.peekable();
    while let Some(argument) = arguments.next() {
        let argument = argument
            .into_string()
            .map_err(|_| "cycle arguments must be UTF-8".to_string())?;
        let (name, inline_value) = argument
            .split_once('=')
            .map_or((argument.as_str(), None), |(name, value)| {
                (name, Some(value))
            });
        match name {
            "--seed" => {
                let value = inline_value
                    .map(str::to_owned)
                    .or_else(|| arguments.next().and_then(|value| value.into_string().ok()))
                    .ok_or_else(|| "cycle seed value is required".to_string())?;
                seed = parse_number(&value, "seed")?;
            }
            "--iteration" => {
                let value = inline_value
                    .map(str::to_owned)
                    .or_else(|| arguments.next().and_then(|value| value.into_string().ok()))
                    .ok_or_else(|| "cycle iteration value is required".to_string())?;
                iteration = parse_number(&value, "iteration")?;
                saw_iteration = true;
            }
            _ => bounded_arguments.push(std::ffi::OsString::from(argument)),
        }
    }
    let _options = parse_bounded_options(bounded_arguments.into_iter())?;
    if saw_iteration && iteration == 0 {
        return Err("cycle iteration must be positive".to_string());
    }

    match scenario.as_str() {
        "natural" | "ansi-corpus" => {
            // Keep enough measured CPU work to survive coarse Windows process
            // accounting under a busy host while remaining far below the
            // cycle deadline.
            burn_bounded_cpu(50);
            emit_cycle_result(&json!({
                "schemaVersion": 1,
                "status": "completed",
                "scenario": scenario,
                "seed": seed,
                "iteration": iteration,
                "ansiCorpus": "tests/fixtures/ansi/phase3-v1.json",
            }))
        }
        "wrong-scenario" => emit_cycle_result(&json!({
            "schemaVersion": 1,
            "status": "completed",
            "scenario": "different-scenario",
            "seed": seed,
            "iteration": iteration,
        })),
        "nonzero" => {
            emit_cycle_result(&json!({
                "schemaVersion": 1,
                "status": "failed",
                "scenario": scenario,
                "seed": seed,
                "iteration": iteration,
                "error": "controlled nonzero exit",
            }))?;
            std::process::exit(1)
        }
        "malformed" => {
            io::stdout()
                .write_all(b"not-json\n")
                .map_err(|error| format!("write malformed fixture: {error}"))?;
            io::stdout()
                .flush()
                .map_err(|error| format!("flush malformed fixture: {error}"))
        }
        "multiple" => {
            emit_cycle_result(&json!({
                "schemaVersion": 1,
                "status": "completed",
                "scenario": scenario,
                "seed": seed,
                "iteration": iteration,
            }))?;
            emit_cycle_result(&json!({
                "schemaVersion": 1,
                "status": "completed",
                "scenario": scenario,
                "seed": seed,
                "iteration": iteration,
            }))
        }
        "oversized" => {
            let mut output = io::stdout().lock();
            let chunk = [b'z'; 64 * 1024];
            for _ in 0..128 {
                output
                    .write_all(&chunk)
                    .map_err(|error| format!("write oversized fixture: {error}"))?;
            }
            output
                .flush()
                .map_err(|error| format!("flush oversized fixture: {error}"))
        }
        "stderr-oversized" => {
            let mut output = io::stderr().lock();
            let chunk = [b'e'; 64 * 1024];
            for _ in 0..128 {
                output
                    .write_all(&chunk)
                    .map_err(|error| format!("write oversized stderr fixture: {error}"))?;
            }
            output
                .flush()
                .map_err(|error| format!("flush oversized stderr fixture: {error}"))?;
            emit_cycle_result(&json!({
                "schemaVersion": 1,
                "status": "completed",
                "scenario": scenario,
                "seed": seed,
                "iteration": iteration,
            }))
        }
        "tree-hang" | "interrupt" => {
            let child = Command::new(std::env::current_exe().map_err(|error| error.to_string())?)
                .arg("cycle-worker")
                .arg("--duration-ms")
                .arg("30_000")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|error| format!("spawn controlled tree child: {error}"))?;
            let child_pid = child.id();
            emit_cycle_result(&json!({
                "schemaVersion": 1,
                "status": "running",
                "scenario": scenario,
                "seed": seed,
                "iteration": iteration,
                "childPid": child_pid,
            }))?;
            wait_bounded(30_000);
            Ok(())
        }
        "crash" => {
            emit_cycle_result(&json!({
                "schemaVersion": 1,
                "status": "crashed",
                "scenario": scenario,
                "seed": seed,
                "iteration": iteration,
            }))?;
            std::process::abort()
        }
        "restart-resume" => emit_cycle_result(&json!({
            "schemaVersion": 1,
            "status": "completed",
            "scenario": scenario,
            "seed": seed,
            "iteration": iteration,
            "resume": "new-generation",
        })),
        "environment-probe" => emit_cycle_result(&json!({
            "schemaVersion": 1,
            "status": "completed",
            "scenario": scenario,
            "seed": seed,
            "iteration": iteration,
            "environment": {
                "secretPresent": std::env::var_os("PHASE3_SOAK_SECRET").is_some(),
                "systemRootPresent": std::env::var_os("SystemRoot").is_some(),
                "tempPresent": std::env::var_os("TEMP").is_some(),
                "pathPresent": std::env::var_os("PATH").is_some(),
            },
        })),
        "occupied-port" => {
            let listener = TcpListener::bind("127.0.0.1:0")
                .map_err(|error| format!("bind occupied-port fixture: {error}"))?;
            let address = listener
                .local_addr()
                .map_err(|error| format!("read occupied-port fixture address: {error}"))?;
            let second_bind = TcpListener::bind(address);
            if second_bind.is_ok() {
                return Err("occupied-port fixture unexpectedly allowed a second bind".to_string());
            }
            emit_cycle_result(&json!({
                "schemaVersion": 1,
                "status": "completed",
                "scenario": scenario,
                "seed": seed,
                "iteration": iteration,
                "occupiedPort": address.port(),
                "secondBindRejected": true,
            }))?;
            wait_bounded(100);
            Ok(())
        }
        other => Err(format!("unknown cycle scenario: {other}")),
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SupervisorManifest {
    #[serde(rename = "schemaVersion")]
    schema_version: u32,
    revision: String,
    #[serde(rename = "gitRevision")]
    git_revision: String,
    #[serde(rename = "buildId")]
    build_id: String,
    #[serde(rename = "targetDirectory")]
    target_directory: std::path::PathBuf,
    #[serde(rename = "sourceTreeState")]
    source_tree_state: String,
    #[serde(rename = "supervisorExecutable")]
    supervisor_executable: std::path::PathBuf,
    #[serde(rename = "supervisorSha256")]
    supervisor_sha256: String,
    #[serde(rename = "helperExecutable")]
    helper_executable: std::path::PathBuf,
    #[serde(rename = "helperSha256")]
    helper_sha256: String,
    #[serde(rename = "cycleExecutable")]
    cycle_executable: std::path::PathBuf,
    #[serde(rename = "cycleSha256")]
    cycle_sha256: String,
    #[serde(rename = "workingDirectory")]
    working_directory: std::path::PathBuf,
    #[serde(rename = "evidenceRoot")]
    evidence_root: std::path::PathBuf,
    environment: SupervisorEnvironment,
    #[serde(rename = "ansiCorpus")]
    ansi_corpus: AnsiCorpusReference,
    seed: u64,
    iterations: u32,
    budgets: SupervisorBudgets,
    #[serde(rename = "scenarioCatalog")]
    scenario_catalog: Vec<SupervisorScenario>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
struct SupervisorEnvironment {
    #[serde(rename = "systemRoot")]
    system_root: std::path::PathBuf,
    #[serde(rename = "tempDirectory")]
    temp_directory: std::path::PathBuf,
    #[serde(rename = "pathDirectories")]
    path_directories: Vec<std::path::PathBuf>,
    #[serde(default)]
    allowlist: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
struct AnsiCorpusReference {
    path: std::path::PathBuf,
    sha256: String,
    revision: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SupervisorBudgets {
    #[serde(rename = "suiteDeadlineMs")]
    suite_deadline_ms: u64,
    #[serde(rename = "cycleDeadlineMs")]
    cycle_deadline_ms: u64,
    #[serde(rename = "cleanupDeadlineMs")]
    cleanup_deadline_ms: u64,
    #[serde(rename = "stdoutBytes")]
    stdout_bytes: usize,
    #[serde(rename = "stderrBytes")]
    stderr_bytes: usize,
    #[serde(rename = "resultBytes")]
    result_bytes: usize,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SupervisorScenario {
    name: String,
    arguments: Vec<String>,
    #[serde(rename = "expectedExitCode")]
    expected_exit_code: i32,
}

#[derive(Debug)]
struct CappedOutput {
    bytes: Vec<u8>,
    total_bytes: u64,
    truncated: bool,
}

fn read_capped<R: std::io::Read>(mut reader: R, limit: usize) -> CappedOutput {
    let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
    let mut total_bytes = 0u64;
    let mut truncated = false;
    let mut chunk = [0u8; 16 * 1024];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => {
                total_bytes = total_bytes.saturating_add(count as u64);
                if bytes.len() < limit {
                    let accepted = (limit - bytes.len()).min(count);
                    bytes.extend_from_slice(&chunk[..accepted]);
                    if accepted < count {
                        truncated = true;
                    }
                } else {
                    truncated = true;
                }
            }
            Err(_) => {
                truncated = true;
                break;
            }
        }
    }
    CappedOutput {
        bytes,
        total_bytes,
        truncated,
    }
}

fn spawn_capped_reader<R: std::io::Read + Send + 'static>(
    reader: R,
    limit: usize,
) -> (std::thread::JoinHandle<()>, Receiver<CappedOutput>) {
    let (sender, receiver) = mpsc::sync_channel(1);
    let thread = std::thread::spawn(move || {
        let _ = sender.send(read_capped(reader, limit));
    });
    (thread, receiver)
}

fn receive_capped_reader(
    receiver: &Receiver<CappedOutput>,
    deadline: Instant,
    label: &str,
) -> Result<CappedOutput, String> {
    receiver
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|error| {
            format!("{label} reader did not settle before the absolute deadline: {error}")
        })
}

fn finish_capped_reader(
    receiver: Receiver<CappedOutput>,
    thread: std::thread::JoinHandle<()>,
    deadline: Instant,
    label: &str,
) -> Result<CappedOutput, String> {
    let output = receive_capped_reader(&receiver, deadline, label)?;
    thread
        .join()
        .map_err(|_| format!("{label} reader thread panicked"))?;
    Ok(output)
}

fn sha256_file(path: &std::path::Path) -> Result<String, String> {
    let mut file = std::fs::File::open(path)
        .map_err(|error| format!("open hash target `{}`: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = std::io::Read::read(&mut file, &mut buffer)
            .map_err(|error| format!("hash target `{}`: {error}", path.display()))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn source_tree_digest(root: &std::path::Path) -> Result<String, String> {
    fn collect(
        root: &std::path::Path,
        current: &std::path::Path,
        files: &mut Vec<std::path::PathBuf>,
        total_bytes: &mut usize,
    ) -> Result<(), String> {
        for entry in fs::read_dir(current)
            .map_err(|error| format!("read source tree {}: {error}", current.display()))?
        {
            let entry = entry.map_err(|error| format!("read source tree entry: {error}"))?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if name == ".git"
                || name == ".devmanager-next"
                || name == "target"
                || name == "target-native-next"
                || name.starts_with(".tmp")
            {
                continue;
            }
            let metadata = fs::symlink_metadata(&path)
                .map_err(|error| format!("inspect source tree {}: {error}", path.display()))?;
            if metadata.file_type().is_symlink() {
                return Err(format!(
                    "source tree contains a symbolic-link reparse point: {}",
                    path.display()
                ));
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if metadata.file_attributes() & 0x400 != 0 {
                    return Err(format!(
                        "source tree contains a reparse point: {}",
                        path.display()
                    ));
                }
            }
            if metadata.is_dir() {
                collect(root, &path, files, total_bytes)?;
            } else if metadata.is_file() {
                if files.len() >= 20_000 {
                    return Err("source tree file count exceeds bound".to_string());
                }
                *total_bytes = total_bytes.saturating_add(metadata.len() as usize);
                if *total_bytes > 128 * 1024 * 1024 {
                    return Err("source tree bytes exceed bound".to_string());
                }
                files.push(
                    path.strip_prefix(root)
                        .map_err(|error| format!("source tree relative path: {error}"))?
                        .to_path_buf(),
                );
            }
        }
        Ok(())
    }

    let root = fs::canonicalize(root)
        .map_err(|error| format!("canonicalize source tree root: {error}"))?;
    if !root.is_dir() {
        return Err("source tree root is not a directory".to_string());
    }
    reject_reparse_ancestors(&root, "source tree root")?;
    let mut files = Vec::new();
    let mut total_bytes = 0usize;
    collect(&root, &root, &mut files, &mut total_bytes)?;
    files.sort();
    let mut hasher = Sha256::new();
    for relative in files {
        let bytes = fs::read(root.join(&relative))
            .map_err(|error| format!("read source tree file {}: {error}", relative.display()))?;
        hasher.update(relative.to_string_lossy().replace('\\', "/").as_bytes());
        hasher.update([0u8]);
        hasher.update(bytes);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn find_git_directory(start: &std::path::Path) -> Result<std::path::PathBuf, String> {
    let mut current = if start.is_dir() {
        start.to_path_buf()
    } else {
        start
            .parent()
            .ok_or_else(|| "source path has no parent".to_string())?
            .to_path_buf()
    };
    loop {
        let marker = current.join(".git");
        if marker.is_dir() {
            return Ok(marker);
        }
        if marker.is_file() {
            let contents = fs::read_to_string(&marker)
                .map_err(|error| format!("read worktree git marker: {error}"))?;
            let value = contents
                .strip_prefix("gitdir:")
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "worktree .git marker is malformed".to_string())?;
            let git_directory = std::path::PathBuf::from(value);
            return Ok(if git_directory.is_absolute() {
                git_directory
            } else {
                current.join(git_directory)
            });
        }
        if !current.pop() {
            return Err(format!(
                "could not locate .git metadata above {}",
                start.display()
            ));
        }
    }
}

fn find_worktree_root(start: &std::path::Path) -> Result<std::path::PathBuf, String> {
    let mut current = if start.is_dir() {
        start.to_path_buf()
    } else {
        start
            .parent()
            .ok_or_else(|| "source path has no parent".to_string())?
            .to_path_buf()
    };
    loop {
        let marker = current.join(".git");
        if marker.is_dir() || marker.is_file() {
            return fs::canonicalize(&current)
                .map_err(|error| format!("canonicalize worktree root: {error}"));
        }
        if !current.pop() {
            return Err(format!(
                "could not locate worktree root above {}",
                start.display()
            ));
        }
    }
}

fn current_git_revision(start: &std::path::Path) -> Result<String, String> {
    let git_directory = find_git_directory(start)?;
    let common_directory = match fs::read_to_string(git_directory.join("commondir")) {
        Ok(contents) => {
            let value = contents.trim();
            if value.is_empty() {
                return Err("git commondir is empty".to_string());
            }
            let path = std::path::PathBuf::from(value);
            if path.is_absolute() {
                path
            } else {
                git_directory.join(path)
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => git_directory.clone(),
        Err(error) => return Err(format!("read git commondir: {error}")),
    };
    let head = fs::read_to_string(git_directory.join("HEAD"))
        .map_err(|error| format!("read git HEAD: {error}"))?;
    let revision = if let Some(reference) = head.trim().strip_prefix("ref: ") {
        let reference = reference.trim();
        let direct = [
            git_directory.join(reference),
            common_directory.join(reference),
        ]
        .into_iter()
        .find(|path| path.is_file());
        if let Some(direct) = direct {
            fs::read_to_string(direct)
                .map_err(|error| format!("read git ref {reference}: {error}"))?
                .trim()
                .to_string()
        } else {
            let mut packed_error = None;
            let packed = [
                common_directory.join("packed-refs"),
                git_directory.join("packed-refs"),
            ]
            .into_iter()
            .find_map(|path| match fs::read_to_string(&path) {
                Ok(contents) => Some(contents),
                Err(error) => {
                    packed_error = Some(error);
                    None
                }
            })
            .ok_or_else(|| {
                format!(
                    "read packed git refs: {}",
                    packed_error
                        .map(|error| error.to_string())
                        .unwrap_or_else(|| "packed refs are absent".to_string())
                )
            })?;
            packed
                .lines()
                .filter(|line| !line.starts_with('#') && !line.starts_with('^'))
                .find_map(|line| {
                    let mut fields = line.split_whitespace();
                    let hash = fields.next()?;
                    let name = fields.next()?;
                    (name == reference).then(|| hash.to_string())
                })
                .ok_or_else(|| format!("git reference {reference} is not present"))?
        }
    } else {
        head.trim().to_string()
    };
    if revision.len() != 40
        || !revision
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        return Err(format!(
            "git HEAD is not a full commit identity: {revision}"
        ));
    }
    Ok(revision.to_ascii_lowercase())
}

fn materialize_manifest_attestation(manifest: &mut SupervisorManifest) -> Result<(), String> {
    let revision = current_git_revision(&manifest.working_directory)?;
    let source_root = find_worktree_root(&manifest.working_directory)?;
    if manifest.git_revision == "CURRENT" {
        manifest.git_revision = revision;
    }
    if manifest.source_tree_state == "CURRENT" {
        manifest.source_tree_state = format!("sha256:{}", source_tree_digest(&source_root)?);
    }
    if manifest.build_id == "CURRENT" {
        let executable = canonical_file(&manifest.supervisor_executable, "supervisorExecutable")?;
        manifest.build_id = format!("sha256:{}", sha256_file(&executable)?);
    }
    let executable = canonical_file(&manifest.supervisor_executable, "supervisorExecutable")?;
    let actual_hash = sha256_file(&executable)?;
    for expected in [
        &mut manifest.supervisor_sha256,
        &mut manifest.helper_sha256,
        &mut manifest.cycle_sha256,
    ] {
        if *expected == "CURRENT" {
            *expected = actual_hash.clone();
        }
    }
    Ok(())
}

fn reject_reparse_ancestors(path: &std::path::Path, label: &str) -> Result<(), String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| format!("resolve {label}: {error}"))?
            .join(path)
    };
    let mut current = std::path::PathBuf::new();
    for component in absolute.components() {
        current.push(component.as_os_str());
        if !current.exists() {
            continue;
        }
        let metadata = std::fs::symlink_metadata(&current).map_err(|error| {
            format!("inspect {label} ancestor `{}`: {error}", current.display())
        })?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "{label} contains a symbolic-link reparse point: {}",
                current.display()
            ));
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
            if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return Err(format!(
                    "{label} contains a reparse point: {}",
                    current.display()
                ));
            }
        }
    }
    Ok(())
}

fn resolve_existing_path(
    path: &std::path::Path,
    label: &str,
) -> Result<std::path::PathBuf, String> {
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| format!("resolve {label}: {error}"))?
            .join(path)
    };
    reject_reparse_ancestors(&candidate, label)?;
    std::fs::canonicalize(&candidate)
        .map_err(|error| format!("{label} cannot be canonicalized: {error}"))
}

#[cfg(windows)]
fn path_is_within(path: &std::path::Path, ancestor: &std::path::Path) -> bool {
    let path = path
        .to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase();
    let ancestor = ancestor
        .to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase();
    path == ancestor
        || path
            .strip_prefix(&ancestor)
            .is_some_and(|remainder| remainder.starts_with('\\'))
}

#[cfg(not(windows))]
fn path_is_within(path: &std::path::Path, ancestor: &std::path::Path) -> bool {
    path.starts_with(ancestor)
}

fn path_equals(left: &std::path::Path, right: &std::path::Path) -> bool {
    path_is_within(left, right) && path_is_within(right, left)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AnsiCorpusFile {
    revision: String,
    cases: Vec<AnsiCorpusCase>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AnsiCorpusCase {
    name: String,
    #[serde(default)]
    bytes: Option<Vec<u8>>,
    #[serde(default)]
    chunks: Option<Vec<Vec<u8>>>,
}

fn load_ansi_corpus(reference: &AnsiCorpusReference) -> Result<Value, String> {
    let path = resolve_existing_path(&reference.path, "ansi corpus")?;
    let bytes = std::fs::read(&path)
        .map_err(|error| format!("read ANSI corpus `{}`: {error}", path.display()))?;
    let actual_hash = sha256_bytes(&bytes);
    if actual_hash != reference.sha256.trim().to_ascii_lowercase() {
        return Err(format!(
            "ANSI corpus SHA-256 mismatch: expected {}, actual {actual_hash}",
            reference.sha256
        ));
    }
    let corpus: AnsiCorpusFile = serde_json::from_slice(&bytes)
        .map_err(|error| format!("ANSI corpus JSON malformed: {error}"))?;
    if corpus.revision != reference.revision || corpus.revision.trim().is_empty() {
        return Err(format!(
            "ANSI corpus revision mismatch: expected {}, actual {}",
            reference.revision, corpus.revision
        ));
    }
    if corpus.cases.is_empty() || corpus.cases.len() > 64 {
        return Err("ANSI corpus case count is outside its bound".to_string());
    }
    let mut names = std::collections::BTreeSet::new();
    let mut case_hashes = BTreeMap::new();
    let mut has_escape = false;
    let mut has_unicode = false;
    for case in corpus.cases {
        if case.name.trim().is_empty() || !names.insert(case.name.clone()) {
            return Err("ANSI corpus case names must be non-empty and unique".to_string());
        }
        let flattened = match (case.bytes, case.chunks) {
            (Some(bytes), None) => bytes,
            (None, Some(chunks)) => chunks.into_iter().flatten().collect(),
            _ => {
                return Err(format!(
                    "ANSI corpus case `{}` must contain bytes or chunks",
                    case.name
                ))
            }
        };
        if flattened.is_empty() {
            return Err(format!("ANSI corpus case `{}` is empty", case.name));
        }
        if !flattened.contains(&0x1b) {
            return Err(format!(
                "ANSI corpus case {} does not contain an ESC byte",
                case.name
            ));
        }
        let decoded = std::str::from_utf8(&flattened)
            .map_err(|error| format!("ANSI corpus case {} is not UTF-8: {error}", case.name))?;
        has_escape = true;
        has_unicode |= decoded.chars().any(|character| !character.is_ascii());
        case_hashes.insert(case.name, sha256_bytes(&flattened));
    }
    if !has_escape || !has_unicode {
        return Err("ANSI corpus must include both ESC and decoded Unicode".to_string());
    }
    Ok(json!({
        "revision": corpus.revision,
        "sha256": actual_hash,
        "caseHashes": case_hashes,
    }))
}

fn canonical_file(path: &std::path::Path, label: &str) -> Result<std::path::PathBuf, String> {
    let canonical = resolve_existing_path(path, label)?;
    if !canonical.is_file() {
        return Err(format!("{label} is not a file: {}", canonical.display()));
    }
    Ok(canonical)
}

fn validate_supervisor_manifest(
    manifest: &SupervisorManifest,
) -> Result<(std::path::PathBuf, std::path::PathBuf, std::path::PathBuf), String> {
    if manifest.schema_version != SUPERVISOR_SCHEMA_VERSION {
        return Err(format!(
            "unsupported supervisor manifest schema {}",
            manifest.schema_version
        ));
    }
    if manifest.git_revision.len() != 40
        || !manifest
            .git_revision
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        return Err(format!(
            "gitRevision must be the current full commit identity, got {}",
            manifest.git_revision
        ));
    }
    let source_tree_digest_value = manifest
        .source_tree_state
        .strip_prefix("sha256:")
        .filter(|digest| {
            digest.len() == 64
                && digest
                    .chars()
                    .all(|character| character.is_ascii_hexdigit())
        })
        .ok_or_else(|| "sourceTreeState must be sha256:<64 hex characters>".to_string())?;
    if manifest.revision.trim().is_empty()
        || manifest.revision.len() > 128
        || !manifest
            .revision
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || ".-_:".contains(character))
    {
        return Err("manifest revision is empty or unsafe".to_string());
    }
    if manifest.iterations == 0 || manifest.iterations > SUPERVISOR_MAX_ITERATIONS {
        return Err(format!(
            "iterations must be between 1 and {SUPERVISOR_MAX_ITERATIONS}"
        ));
    }
    let budgets = &manifest.budgets;
    if budgets.suite_deadline_ms == 0
        || budgets.suite_deadline_ms > 10 * 60 * 1_000
        || budgets.cycle_deadline_ms == 0
        || budgets.cycle_deadline_ms > 60 * 1_000
        || budgets.cleanup_deadline_ms == 0
        || budgets.cleanup_deadline_ms > 60 * 1_000
        || budgets.stdout_bytes == 0
        || budgets.stdout_bytes > SUPERVISOR_MAX_OUTPUT_BYTES
        || budgets.stderr_bytes == 0
        || budgets.stderr_bytes > SUPERVISOR_MAX_OUTPUT_BYTES
        || budgets.result_bytes == 0
        || budgets.result_bytes > SUPERVISOR_MAX_RESULT_BYTES
    {
        return Err("manifest budgets are outside bounded limits".to_string());
    }
    if budgets.cycle_deadline_ms > budgets.suite_deadline_ms {
        return Err("cycle deadline exceeds suite deadline".to_string());
    }
    if manifest.scenario_catalog.is_empty()
        || manifest.scenario_catalog.len() > SUPERVISOR_MAX_SCENARIOS
    {
        return Err(format!(
            "scenario catalog must contain 1..={SUPERVISOR_MAX_SCENARIOS} entries"
        ));
    }

    let supervisor = canonical_file(&manifest.supervisor_executable, "supervisorExecutable")?;
    let helper = canonical_file(&manifest.helper_executable, "helperExecutable")?;
    let cycle = canonical_file(&manifest.cycle_executable, "cycleExecutable")?;
    let current = canonical_file(
        &std::env::current_exe().map_err(|error| format!("resolve current executable: {error}"))?,
        "current executable",
    )?;
    if !path_equals(&supervisor, &current) {
        return Err(format!(
            "supervisor executable identity mismatch: expected `{}`, current `{}`",
            supervisor.display(),
            current.display()
        ));
    }
    if !path_equals(&helper, &current) || !path_equals(&cycle, &current) {
        return Err(format!(
            "helper/cycle executable identity must be the fixed Rust test helper `{}`",
            current.display()
        ));
    }
    for (label, path, expected) in [
        ("supervisor", &supervisor, &manifest.supervisor_sha256),
        ("helper", &helper, &manifest.helper_sha256),
        ("cycle", &cycle, &manifest.cycle_sha256),
    ] {
        let normalized = expected.trim().to_ascii_lowercase();
        if normalized.len() != 64
            || !normalized
                .chars()
                .all(|character| character.is_ascii_hexdigit())
        {
            return Err(format!(
                "{label} SHA-256 must be exactly 64 hexadecimal characters"
            ));
        }
        let actual = sha256_file(path)?;
        if actual != normalized {
            return Err(format!(
                "{label} SHA-256 mismatch: expected {normalized}, actual {actual}"
            ));
        }
    }
    let current_revision = current_git_revision(&manifest.working_directory)?;
    if manifest.git_revision != current_revision {
        return Err(format!(
            "gitRevision mismatch: expected current {}, actual {}",
            current_revision, manifest.git_revision
        ));
    }
    let source_root = find_worktree_root(&manifest.working_directory)?;
    let current_source_tree_state = source_tree_digest(&source_root)?;
    if source_tree_digest_value != current_source_tree_state {
        return Err(format!(
            "sourceTreeState mismatch: expected current sha256:{current_source_tree_state}, actual {}",
            manifest.source_tree_state
        ));
    }
    let expected_build_id = format!("sha256:{}", sha256_file(&supervisor)?);
    if manifest.build_id != expected_build_id {
        return Err(format!(
            "buildId mismatch: expected current binary identity {}, actual {}",
            expected_build_id, manifest.build_id
        ));
    }
    let target_directory = resolve_existing_path(&manifest.target_directory, "targetDirectory")?;
    if !target_directory.is_dir() {
        return Err("targetDirectory is not a directory".to_string());
    }
    if !path_is_within(&target_directory, &source_root) {
        return Err(format!(
            "targetDirectory must remain under the canonical worktree root {}",
            source_root.display()
        ));
    }
    if !path_is_within(&supervisor, &target_directory)
        || !path_is_within(&helper, &target_directory)
        || !path_is_within(&cycle, &target_directory)
    {
        return Err(format!(
            "all helper binaries must be under targetDirectory {}",
            target_directory.display()
        ));
    }
    reject_reparse_ancestors(&manifest.working_directory, "workingDirectory")?;
    let working_directory = std::fs::canonicalize(&manifest.working_directory)
        .map_err(|error| format!("workingDirectory cannot be canonicalized: {error}"))?;
    if !working_directory.is_dir() {
        return Err("workingDirectory is not a directory".to_string());
    }
    reject_reparse_ancestors(&working_directory, "workingDirectory")?;
    if !path_is_within(&working_directory, &source_root) {
        return Err(format!(
            "workingDirectory must remain under the canonical worktree root {}",
            source_root.display()
        ));
    }
    let evidence_root = resolve_existing_path(&manifest.evidence_root, "evidenceRoot")?;
    if !evidence_root.is_dir() {
        return Err("evidenceRoot is not a directory".to_string());
    }
    if !path_is_within(&evidence_root, &source_root) {
        return Err(format!(
            "evidenceRoot must remain under the canonical worktree root {}",
            source_root.display()
        ));
    }
    let system_root =
        resolve_existing_path(&manifest.environment.system_root, "environment.systemRoot")?;
    if !system_root.is_dir() {
        return Err("environment.systemRoot is not a directory".to_string());
    }
    let temp_directory = resolve_existing_path(
        &manifest.environment.temp_directory,
        "environment.tempDirectory",
    )?;
    if !temp_directory.is_dir() {
        return Err("environment.tempDirectory is not a directory".to_string());
    }
    if !path_is_within(&temp_directory, &source_root) {
        return Err(format!(
            "environment.tempDirectory must remain under the canonical worktree root {}",
            source_root.display()
        ));
    }
    if manifest.environment.path_directories.is_empty()
        || manifest.environment.path_directories.len() > 16
    {
        return Err("environment.pathDirectories count is outside its bound".to_string());
    }
    for path in &manifest.environment.path_directories {
        let directory = resolve_existing_path(path, "environment.pathDirectories")?;
        if !directory.is_dir() {
            return Err(format!(
                "environment.pathDirectories entry is not a directory: {}",
                directory.display()
            ));
        }
        if !path_is_within(&directory, &source_root) && !path_is_within(&directory, &system_root) {
            return Err(format!(
                "environment.pathDirectories entry escapes the canonical worktree/system roots: {}",
                directory.display()
            ));
        }
    }
    for (name, value) in &manifest.environment.allowlist {
        if name.is_empty()
            || name.len() > 64
            || !name
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '_')
            || matches!(
                name.to_ascii_uppercase().as_str(),
                "SYSTEMROOT" | "TEMP" | "TMP" | "PATH"
            )
            || name.to_ascii_uppercase().contains("SECRET")
            || name.to_ascii_uppercase().contains("TOKEN")
            || name.to_ascii_uppercase().contains("PASSWORD")
            || name.to_ascii_uppercase().contains("KEY")
            || value.len() > 4096
            || value.chars().any(|character| character == '\0')
        {
            return Err(format!("environment allowlist entry `{name}` is unsafe"));
        }
    }
    let ansi_path = resolve_existing_path(&manifest.ansi_corpus.path, "ansi corpus")?;
    if !path_is_within(&ansi_path, &source_root) {
        return Err(format!(
            "ansi corpus must remain under the canonical worktree root {}",
            source_root.display()
        ));
    }
    let _ = (evidence_root, system_root, temp_directory, target_directory);
    let _ansi_corpus = load_ansi_corpus(&manifest.ansi_corpus)?;
    for scenario in &manifest.scenario_catalog {
        if scenario.name.trim().is_empty()
            || scenario.name.len() > 96
            || !scenario
                .name
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || ".-_:".contains(character))
        {
            return Err("scenario name is empty or unsafe".to_string());
        }
        if scenario.arguments.is_empty()
            || scenario.arguments.len().saturating_add(4) > SUPERVISOR_MAX_ARGUMENTS
        {
            return Err(format!(
                "scenario `{}` arguments must contain 1..={SUPERVISOR_MAX_ARGUMENTS} entries",
                scenario.name
            ));
        }
        if scenario.arguments.first().map(String::as_str) != Some("cycle") {
            return Err(format!(
                "scenario `{}` must invoke the fixed cycle protocol",
                scenario.name
            ));
        }
        for argument in &scenario.arguments {
            if argument.is_empty() || argument.len() > SUPERVISOR_MAX_ARGUMENT_BYTES {
                return Err(format!(
                    "scenario `{}` contains an unbounded argument",
                    scenario.name
                ));
            }
        }
        if scenario.expected_exit_code != 0 && scenario.expected_exit_code != 1 {
            return Err(format!(
                "scenario `{}` expectedExitCode must be 0 or 1",
                scenario.name
            ));
        }
    }
    Ok((supervisor, helper, cycle))
}

#[cfg(windows)]
mod windows_supervisor {
    use super::*;
    use std::ffi::{c_void, OsStr};
    use std::fs::File;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle, OwnedHandle};
    use std::path::{Path, PathBuf};
    use std::ptr;

    type RawHandle = *mut c_void;

    const INVALID_HANDLE_VALUE: RawHandle = -1isize as RawHandle;
    const CREATE_SUSPENDED: u32 = 0x0000_0004;
    const CREATE_UNICODE_ENVIRONMENT: u32 = 0x0000_0400;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const STARTF_USESTDHANDLES: u32 = 0x0000_0100;
    const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;
    const WAIT_OBJECT_0: u32 = 0x0000_0000;
    const WAIT_TIMEOUT: u32 = 0x0000_0102;
    const WAIT_FAILED: u32 = 0xffff_ffff;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const JOB_OBJECT_BASIC_PROCESS_ID_LIST_CLASS: u32 = 3;
    const JOB_OBJECT_ASSOCIATE_COMPLETION_PORT_INFORMATION_CLASS: u32 = 7;
    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS: u32 = 9;
    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;
    const JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO: u32 = 4;
    const ERROR_MORE_DATA: i32 = 234;
    const ERROR_TIMEOUT: i32 = 1460;
    const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
    const FILE_LIST_DIRECTORY: u32 = 0x0001;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const OPEN_EXISTING: u32 = 3;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const AF_INET: u32 = 2;
    const TCP_TABLE_OWNER_PID_LISTENER: u32 = 3;

    #[repr(C)]
    struct SecurityAttributes {
        length: u32,
        security_descriptor: *mut c_void,
        inherit_handle: i32,
    }

    #[repr(C)]
    struct StartupInfoW {
        cb: u32,
        reserved: *mut u16,
        desktop: *mut u16,
        title: *mut u16,
        x: u32,
        y: u32,
        x_size: u32,
        y_size: u32,
        x_count_chars: u32,
        y_count_chars: u32,
        fill_attribute: u32,
        flags: u32,
        show_window: u16,
        reserved2: u16,
        reserved2_ptr: *mut u8,
        std_input: RawHandle,
        std_output: RawHandle,
        std_error: RawHandle,
    }

    #[repr(C)]
    struct ProcessInformation {
        process: RawHandle,
        thread: RawHandle,
        process_id: u32,
        thread_id: u32,
    }

    #[repr(C)]
    struct FileTime {
        low: u32,
        high: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct SystemInfo {
        processor_architecture: u16,
        reserved: u16,
        page_size: u32,
        minimum_application_address: *mut c_void,
        maximum_application_address: *mut c_void,
        active_processor_mask: usize,
        number_of_processors: u32,
        processor_type: u32,
        allocation_granularity: u32,
        processor_level: u16,
        processor_revision: u16,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct MibTcpRowOwnerPid {
        state: u32,
        local_addr: u32,
        local_port: [u8; 4],
        remote_addr: u32,
        remote_port: [u8; 4],
        owning_pid: u32,
    }

    #[repr(C)]
    struct JobObjectAssociateCompletionPort {
        completion_key: *mut c_void,
        completion_port: RawHandle,
    }

    #[repr(C)]
    #[derive(Default)]
    struct JobObjectBasicLimitInformation {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: u32,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct IoCounters {
        read_operation_count: u64,
        write_operation_count: u64,
        other_operation_count: u64,
        read_transfer_count: u64,
        write_transfer_count: u64,
        other_transfer_count: u64,
    }

    #[repr(C)]
    #[derive(Default)]
    struct JobObjectExtendedLimitInformation {
        basic_limit_information: JobObjectBasicLimitInformation,
        io_info: IoCounters,
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn CreatePipe(
            read_pipe: *mut RawHandle,
            write_pipe: *mut RawHandle,
            attributes: *mut SecurityAttributes,
            size: u32,
        ) -> i32;
        fn SetHandleInformation(handle: RawHandle, mask: u32, flags: u32) -> i32;
        fn CreateProcessW(
            application_name: *const u16,
            command_line: *mut u16,
            process_attributes: *mut c_void,
            thread_attributes: *mut c_void,
            inherit_handles: i32,
            creation_flags: u32,
            environment: *mut c_void,
            current_directory: *const u16,
            startup_info: *mut StartupInfoW,
            process_information: *mut ProcessInformation,
        ) -> i32;
        fn CloseHandle(handle: RawHandle) -> i32;
        fn ResumeThread(thread: RawHandle) -> u32;
        fn WaitForSingleObject(handle: RawHandle, milliseconds: u32) -> u32;
        fn GetExitCodeProcess(process: RawHandle, exit_code: *mut u32) -> i32;
        fn TerminateProcess(process: RawHandle, exit_code: u32) -> i32;
        fn CreateJobObjectW(attributes: *mut c_void, name: *const u16) -> RawHandle;
        fn CreateIoCompletionPort(
            file_handle: RawHandle,
            existing_completion_port: RawHandle,
            completion_key: usize,
            number_of_concurrent_threads: u32,
        ) -> RawHandle;
        fn SetInformationJobObject(
            job: RawHandle,
            information_class: u32,
            information: *mut c_void,
            information_length: u32,
        ) -> i32;
        fn AssignProcessToJobObject(job: RawHandle, process: RawHandle) -> i32;
        fn QueryInformationJobObject(
            job: RawHandle,
            information_class: u32,
            information: *mut c_void,
            information_length: u32,
            return_length: *mut u32,
        ) -> i32;
        fn TerminateJobObject(job: RawHandle, exit_code: u32) -> i32;
        fn GetQueuedCompletionStatus(
            completion_port: RawHandle,
            number_of_bytes_transferred: *mut u32,
            completion_key: *mut usize,
            overlapped: *mut *mut c_void,
            milliseconds: u32,
        ) -> i32;
        fn GetProcessTimes(
            process: RawHandle,
            creation_time: *mut FileTime,
            exit_time: *mut FileTime,
            kernel_time: *mut FileTime,
            user_time: *mut FileTime,
        ) -> i32;
        fn QueryFullProcessImageNameW(
            process: RawHandle,
            flags: u32,
            executable_name: *mut u16,
            size: *mut u32,
        ) -> i32;
        fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> RawHandle;
        fn GetCurrentProcess() -> RawHandle;
        fn GetProcessHandleCount(process: RawHandle, handle_count: *mut u32) -> i32;
        fn GetSystemInfo(system_info: *mut SystemInfo);
        fn CreateFileW(
            file_name: *const u16,
            desired_access: u32,
            share_mode: u32,
            security_attributes: *mut c_void,
            creation_disposition: u32,
            flags_and_attributes: u32,
            template_file: RawHandle,
        ) -> RawHandle;
    }

    #[link(name = "iphlpapi")]
    extern "system" {
        fn GetExtendedTcpTable(
            tcp_table: *mut c_void,
            size: *mut u32,
            order: i32,
            address_family: u32,
            table_class: u32,
            reserved: u32,
        ) -> u32;
    }

    #[derive(Debug, Clone)]
    struct ProcessIdentity {
        process_id: u32,
        creation_time_100ns: u64,
        executable_path: PathBuf,
    }

    struct SupervisorJob {
        job: OwnedHandle,
        completion_port: OwnedHandle,
        completion_key: usize,
    }

    impl SupervisorJob {
        fn create() -> Result<Self, String> {
            unsafe {
                let job_raw = CreateJobObjectW(ptr::null_mut(), ptr::null());
                if job_raw.is_null() {
                    return Err(format!(
                        "CreateJobObjectW failed: {}",
                        io::Error::last_os_error()
                    ));
                }
                let job = OwnedHandle::from_raw_handle(job_raw as _);
                let completion_raw =
                    CreateIoCompletionPort(INVALID_HANDLE_VALUE, ptr::null_mut(), 0xD3_10_0001, 1);
                if completion_raw.is_null() {
                    return Err(format!(
                        "CreateIoCompletionPort failed: {}",
                        io::Error::last_os_error()
                    ));
                }
                let completion_port = OwnedHandle::from_raw_handle(completion_raw as _);
                let completion_key = 0xD3_10_0001usize;
                let mut association = JobObjectAssociateCompletionPort {
                    completion_key: completion_key as *mut c_void,
                    completion_port: completion_port.as_raw_handle() as _,
                };
                if SetInformationJobObject(
                    job.as_raw_handle() as _,
                    JOB_OBJECT_ASSOCIATE_COMPLETION_PORT_INFORMATION_CLASS,
                    &mut association as *mut _ as _,
                    std::mem::size_of::<JobObjectAssociateCompletionPort>() as u32,
                ) == 0
                {
                    return Err(format!(
                        "associate Job completion port failed: {}",
                        io::Error::last_os_error()
                    ));
                }
                let mut limits = JobObjectExtendedLimitInformation::default();
                limits.basic_limit_information.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                if SetInformationJobObject(
                    job.as_raw_handle() as _,
                    JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS,
                    &mut limits as *mut _ as _,
                    std::mem::size_of::<JobObjectExtendedLimitInformation>() as u32,
                ) == 0
                {
                    return Err(format!(
                        "set Job kill-on-close policy failed: {}",
                        io::Error::last_os_error()
                    ));
                }
                Ok(Self {
                    job,
                    completion_port,
                    completion_key,
                })
            }
        }

        fn assign(&self, process: RawHandle) -> Result<(), String> {
            let assigned =
                unsafe { AssignProcessToJobObject(self.job.as_raw_handle() as _, process) };
            if assigned == 0 {
                Err(format!(
                    "AssignProcessToJobObject failed: {}",
                    io::Error::last_os_error()
                ))
            } else {
                Ok(())
            }
        }

        fn active_process_ids(&self) -> Result<Vec<u32>, String> {
            let mut capacity = 16usize;
            loop {
                if capacity > 4096 {
                    return Err("Job active member count exceeds 4096".to_string());
                }
                let header = 2 * std::mem::size_of::<u32>();
                let bytes = header
                    .checked_add(capacity * std::mem::size_of::<usize>())
                    .ok_or_else(|| "Job active member buffer overflow".to_string())?;
                let align = std::mem::align_of::<usize>();
                let mut storage = vec![0u8; bytes + align];
                let offset = storage.as_ptr().align_offset(align);
                let range_end = offset
                    .checked_add(bytes)
                    .ok_or_else(|| "Job active member alignment overflow".to_string())?;
                let buffer = storage
                    .get_mut(offset..range_end)
                    .ok_or_else(|| "Job active member buffer out of range".to_string())?;
                let mut returned = 0u32;
                let ok = unsafe {
                    QueryInformationJobObject(
                        self.job.as_raw_handle() as _,
                        JOB_OBJECT_BASIC_PROCESS_ID_LIST_CLASS,
                        buffer.as_mut_ptr() as _,
                        bytes as u32,
                        &mut returned,
                    )
                };
                if ok == 0 {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() == Some(ERROR_MORE_DATA) {
                        let assigned = u32::from_ne_bytes(buffer[..4].try_into().unwrap()) as usize;
                        capacity = capacity.saturating_mul(2).max(assigned).min(4096);
                        continue;
                    }
                    return Err(format!("QueryInformationJobObject failed: {error}"));
                }
                let count = u32::from_ne_bytes(buffer[4..8].try_into().unwrap()) as usize;
                if count > capacity {
                    capacity = count;
                    continue;
                }
                let list = unsafe { buffer.as_ptr().add(header) as *const usize };
                let mut ids = Vec::with_capacity(count);
                for index in 0..count {
                    let pid = unsafe { *list.add(index) as u32 };
                    if pid != 0 {
                        ids.push(pid);
                    }
                }
                ids.sort_unstable();
                ids.dedup();
                return Ok(ids);
            }
        }

        fn wait_active_process_zero(&self, deadline: Instant) -> Result<(), String> {
            // Query the Job membership independently before waiting for the
            // completion-port notification. The notification is a useful
            // edge, but a process may have reached zero before this waiter
            // starts; that state must not be reported as residue merely
            // because its edge was already queued.
            if self.active_process_ids()?.is_empty() {
                return Ok(());
            }
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err("ACTIVE_PROCESS_ZERO deadline exceeded".to_string());
                }
                let timeout = remaining
                    .as_millis()
                    .saturating_add(1)
                    .min(u32::MAX as u128) as u32;
                let mut message = 0u32;
                let mut key = 0usize;
                let mut overlapped = ptr::null_mut();
                let ok = unsafe {
                    GetQueuedCompletionStatus(
                        self.completion_port.as_raw_handle() as _,
                        &mut message,
                        &mut key,
                        &mut overlapped,
                        timeout,
                    )
                };
                if ok == 0 {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() == Some(ERROR_TIMEOUT) {
                        return Err("ACTIVE_PROCESS_ZERO wait timed out".to_string());
                    }
                    return Err(format!("GetQueuedCompletionStatus failed: {error}"));
                }
                if key != self.completion_key {
                    continue;
                }
                if message == JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO {
                    if self.active_process_ids()?.is_empty() {
                        return Ok(());
                    }
                }
            }
        }

        fn terminate_and_wait(&self, deadline: Instant) -> Result<(), String> {
            let initial_members = self.active_process_ids();
            let initial_error = initial_members.as_ref().err().cloned();
            let needs_termination = initial_members
                .as_ref()
                .map(|members| !members.is_empty())
                .unwrap_or(true);
            if needs_termination {
                let terminated = unsafe { TerminateJobObject(self.job.as_raw_handle() as _, 124) };
                if terminated == 0 {
                    let final_members = self.active_process_ids();
                    return Err(format!(
                        "TerminateJobObject failed: {}; initial-members={initial_error:?}; final-members={final_members:?}",
                        io::Error::last_os_error()
                    ));
                }
                if let Err(wait_error) = self.wait_active_process_zero(deadline) {
                    let final_members = self.active_process_ids();
                    return Err(format!(
                        "ACTIVE_PROCESS_ZERO cleanup failed: {wait_error}; initial-members={initial_error:?}; final-members={final_members:?}"
                    ));
                }
            }
            let final_members = self.active_process_ids()?;
            if !final_members.is_empty() {
                return Err(format!(
                    "Job remained non-empty after termination: {final_members:?}"
                ));
            }
            if let Some(initial_error) = initial_error {
                return Err(format!(
                    "initial Job member inspection failed before cleanup: {initial_error}"
                ));
            }
            Ok(())
        }
    }

    impl Drop for SupervisorJob {
        fn drop(&mut self) {
            // The kill-on-close policy is the last-resort fence if a caller is
            // interrupted between an error and explicit Job settlement.
        }
    }

    struct SpawnedChild {
        job: SupervisorJob,
        process: OwnedHandle,
        _thread: OwnedHandle,
        stdout: Option<File>,
        stderr: Option<File>,
        root_identity: ProcessIdentity,
        member_identities: Vec<ProcessIdentity>,
    }

    fn utf16(value: &OsStr) -> Vec<u16> {
        value.encode_wide().chain(std::iter::once(0)).collect()
    }

    fn quote_windows_argument(value: &str) -> String {
        if value.is_empty() {
            return "\"\"".to_string();
        }
        if !value
            .chars()
            .any(|character| character.is_whitespace() || character == '"')
        {
            return value.to_string();
        }
        let mut result = String::from("\"");
        let mut slashes = 0usize;
        for character in value.chars() {
            if character == '\\' {
                slashes += 1;
            } else if character == '"' {
                result.extend(std::iter::repeat_n('\\', slashes * 2 + 1));
                result.push('"');
                slashes = 0;
            } else {
                result.extend(std::iter::repeat_n('\\', slashes));
                result.push(character);
                slashes = 0;
            }
        }
        result.extend(std::iter::repeat_n('\\', slashes * 2));
        result.push('"');
        result
    }

    fn create_process(
        executable: &Path,
        arguments: &[String],
        working_directory: &Path,
        stdout_write: RawHandle,
        stderr_write: RawHandle,
        environment: &[u16],
        cleanup_deadline: Instant,
    ) -> Result<(OwnedHandle, OwnedHandle, u32), String> {
        let executable_wide = utf16(executable.as_os_str());
        let command_line = std::iter::once(executable.to_string_lossy().to_string())
            .chain(arguments.iter().cloned())
            .map(|argument| quote_windows_argument(&argument))
            .collect::<Vec<_>>()
            .join(" ");
        let mut command_line_wide = utf16(OsStr::new(&command_line));
        let working_directory_wide = utf16(working_directory.as_os_str());
        let mut startup = StartupInfoW {
            cb: std::mem::size_of::<StartupInfoW>() as u32,
            reserved: ptr::null_mut(),
            desktop: ptr::null_mut(),
            title: ptr::null_mut(),
            x: 0,
            y: 0,
            x_size: 0,
            y_size: 0,
            x_count_chars: 0,
            y_count_chars: 0,
            fill_attribute: 0,
            flags: STARTF_USESTDHANDLES,
            show_window: 0,
            reserved2: 0,
            reserved2_ptr: ptr::null_mut(),
            std_input: ptr::null_mut(),
            std_output: stdout_write,
            std_error: stderr_write,
        };
        let mut info = ProcessInformation {
            process: ptr::null_mut(),
            thread: ptr::null_mut(),
            process_id: 0,
            thread_id: 0,
        };
        let created = unsafe {
            CreateProcessW(
                executable_wide.as_ptr(),
                command_line_wide.as_mut_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
                1,
                CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
                environment.as_ptr() as *mut c_void,
                working_directory_wide.as_ptr(),
                &mut startup,
                &mut info,
            )
        };
        if created == 0 {
            return Err(format!(
                "CreateProcessW failed: {}",
                io::Error::last_os_error()
            ));
        }
        if info.process.is_null() || info.thread.is_null() || info.process_id == 0 {
            let cleanup = if !info.process.is_null() {
                let process = unsafe { OwnedHandle::from_raw_handle(info.process as _) };
                let terminated = unsafe { TerminateProcess(process.as_raw_handle() as _, 127) };
                let waited = wait_process(process.as_raw_handle() as _, cleanup_deadline);
                match (terminated, waited) {
                    (0, Ok(Some(_))) => Err(format!(
                        "TerminateProcess(incomplete process identity) failed: {}",
                        io::Error::last_os_error()
                    )),
                    (_, Ok(Some(_))) => Ok(()),
                    (_, Ok(None)) => Err(
                        "incomplete process identity did not settle before the absolute deadline"
                            .to_string(),
                    ),
                    (_, Err(error)) => Err(error),
                }
            } else {
                Ok(())
            };
            if !info.thread.is_null() {
                drop(unsafe { OwnedHandle::from_raw_handle(info.thread as _) });
            }
            return Err(format!(
                "CreateProcessW returned incomplete process identity; cleanup={cleanup:?}"
            ));
        }
        Ok(unsafe {
            (
                OwnedHandle::from_raw_handle(info.process as _),
                OwnedHandle::from_raw_handle(info.thread as _),
                info.process_id,
            )
        })
    }

    fn capture_identity(process: RawHandle, pid: u32) -> Result<ProcessIdentity, String> {
        unsafe {
            let mut creation = FileTime { low: 0, high: 0 };
            let mut exit = FileTime { low: 0, high: 0 };
            let mut kernel = FileTime { low: 0, high: 0 };
            let mut user = FileTime { low: 0, high: 0 };
            if GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user) == 0 {
                return Err(format!(
                    "GetProcessTimes({pid}) failed: {}",
                    io::Error::last_os_error()
                ));
            }
            let creation_time_100ns = ((creation.high as u64) << 32) | creation.low as u64;
            let mut buffer = vec![0u16; 32_768];
            let mut length = buffer.len() as u32;
            if QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut length) == 0 {
                return Err(format!(
                    "QueryFullProcessImageNameW({pid}) failed: {}",
                    io::Error::last_os_error()
                ));
            }
            buffer.truncate(length as usize);
            let raw_path = PathBuf::from(String::from_utf16_lossy(&buffer));
            let executable_path = std::fs::canonicalize(&raw_path)
                .map_err(|error| format!("canonicalize process {pid} executable: {error}"))?;
            Ok(ProcessIdentity {
                process_id: pid,
                creation_time_100ns,
                executable_path,
            })
        }
    }

    fn process_cpu_time_100ns(process: RawHandle, pid: u32) -> Result<u64, String> {
        unsafe {
            let mut creation = FileTime { low: 0, high: 0 };
            let mut exit = FileTime { low: 0, high: 0 };
            let mut kernel = FileTime { low: 0, high: 0 };
            let mut user = FileTime { low: 0, high: 0 };
            if GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user) == 0 {
                return Err(format!(
                    "GetProcessTimes({pid}) for CPU accounting failed: {}",
                    io::Error::last_os_error()
                ));
            }
            let kernel_time = ((kernel.high as u64) << 32) | kernel.low as u64;
            let user_time = ((user.high as u64) << 32) | user.low as u64;
            Ok(kernel_time.saturating_add(user_time))
        }
    }

    fn process_handle_count(process: RawHandle, pid: u32) -> Result<u32, String> {
        let mut count = 0u32;
        if unsafe { GetProcessHandleCount(process, &mut count) } == 0 {
            return Err(format!(
                "GetProcessHandleCount({pid}) failed: {}",
                io::Error::last_os_error()
            ));
        }
        Ok(count)
    }

    fn current_process_handle_count() -> Result<u32, String> {
        let mut count = 0u32;
        if unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) } == 0 {
            return Err(format!(
                "GetProcessHandleCount(current process) failed: {}",
                io::Error::last_os_error()
            ));
        }
        Ok(count)
    }

    fn logical_processor_count() -> Result<u32, String> {
        let mut system = SystemInfo::default();
        unsafe { GetSystemInfo(&mut system) };
        if system.number_of_processors == 0 {
            Err("GetSystemInfo returned zero logical processors".to_string())
        } else {
            Ok(system.number_of_processors)
        }
    }

    fn query_tcp_listeners() -> Result<Vec<Value>, String> {
        let mut size = 0u32;
        let first = unsafe {
            GetExtendedTcpTable(
                ptr::null_mut(),
                &mut size,
                1,
                AF_INET,
                TCP_TABLE_OWNER_PID_LISTENER,
                0,
            )
        };
        if first != ERROR_INSUFFICIENT_BUFFER || size < std::mem::size_of::<u32>() as u32 {
            return Err(format!(
                "GetExtendedTcpTable size query failed: status {first}, bytes {size}"
            ));
        }
        if size > 4 * 1024 * 1024 {
            return Err(format!(
                "GetExtendedTcpTable listener table exceeds bounded size: {size}"
            ));
        }
        let mut storage = vec![0u8; size as usize];
        let second = unsafe {
            GetExtendedTcpTable(
                storage.as_mut_ptr() as _,
                &mut size,
                1,
                AF_INET,
                TCP_TABLE_OWNER_PID_LISTENER,
                0,
            )
        };
        if second != 0 {
            return Err(format!(
                "GetExtendedTcpTable listener query failed: status {second}"
            ));
        }
        let bytes = (size as usize).min(storage.len());
        if bytes < std::mem::size_of::<u32>() {
            return Err("GetExtendedTcpTable returned a truncated table".to_string());
        }
        let count = u32::from_ne_bytes(storage[..4].try_into().expect("row count bytes")) as usize;
        let row_size = std::mem::size_of::<MibTcpRowOwnerPid>();
        let available = (bytes - 4) / row_size;
        if count > available {
            return Err(format!(
                "GetExtendedTcpTable row count {count} exceeds available {available}"
            ));
        }
        if count > 4096 {
            return Err(format!(
                "GetExtendedTcpTable listener count exceeds bound: {count}"
            ));
        }
        let mut listeners = Vec::with_capacity(count);
        for index in 0..count {
            let offset = 4 + index * row_size;
            let row = unsafe {
                std::ptr::read_unaligned(storage.as_ptr().add(offset) as *const MibTcpRowOwnerPid)
            };
            let port = u16::from_be_bytes([row.local_port[0], row.local_port[1]]);
            listeners.push(json!({
                "processId": row.owning_pid,
                "address": format!("{}.{}.{}.{}", row.local_addr & 0xff, (row.local_addr >> 8) & 0xff, (row.local_addr >> 16) & 0xff, (row.local_addr >> 24) & 0xff),
                "port": port,
            }));
        }
        Ok(listeners)
    }

    fn filter_owned_listeners(
        listeners: &[Value],
        owned_pids: &[u32],
    ) -> Result<Vec<Value>, String> {
        let mut owned = Vec::new();
        for listener in listeners {
            let pid = listener["processId"]
                .as_u64()
                .ok_or_else(|| "listener row omitted processId".to_string())?
                as u32;
            if owned_pids.contains(&pid) {
                owned.push(listener.clone());
            }
        }
        Ok(owned)
    }

    fn listener_key(listener: &Value) -> Result<String, String> {
        let pid = listener["processId"]
            .as_u64()
            .ok_or_else(|| "listener row omitted processId".to_string())?;
        let address = listener["address"]
            .as_str()
            .ok_or_else(|| "listener row omitted address".to_string())?;
        let port = listener["port"]
            .as_u64()
            .ok_or_else(|| "listener row omitted port".to_string())?;
        Ok(format!("{pid}|{address}|{port}"))
    }

    fn verify_external_listeners_unchanged(
        before: &[Value],
        after: &[Value],
        owned_pids: &[u32],
    ) -> Result<(), String> {
        let after_keys = after
            .iter()
            .map(listener_key)
            .collect::<Result<std::collections::BTreeSet<_>, _>>()?;
        let before_keys = before
            .iter()
            .map(listener_key)
            .collect::<Result<std::collections::BTreeSet<_>, _>>()?;
        for listener in before {
            let pid = listener["processId"]
                .as_u64()
                .ok_or_else(|| "listener row omitted processId".to_string())?
                as u32;
            if !owned_pids.contains(&pid) && !after_keys.contains(&listener_key(listener)?) {
                return Err(format!(
                    "external listener disappeared during cycle: {}",
                    listener_key(listener)?
                ));
            }
        }
        for listener in after {
            let pid = listener["processId"]
                .as_u64()
                .ok_or_else(|| "listener row omitted processId".to_string())?
                as u32;
            if !owned_pids.contains(&pid) && !before_keys.contains(&listener_key(listener)?) {
                return Err(format!(
                    "external listener appeared during cycle: {}",
                    listener_key(listener)?
                ));
            }
        }
        Ok(())
    }

    fn environment_block(environment: &SupervisorEnvironment) -> Result<Vec<u16>, String> {
        let system_root =
            resolve_existing_path(&environment.system_root, "environment.systemRoot")?;
        let temp_directory =
            resolve_existing_path(&environment.temp_directory, "environment.tempDirectory")?;
        let mut entries = environment.allowlist.clone();
        entries.insert(
            "SystemRoot".to_string(),
            system_root.to_string_lossy().into_owned(),
        );
        entries.insert(
            "TEMP".to_string(),
            temp_directory.to_string_lossy().into_owned(),
        );
        entries.insert(
            "TMP".to_string(),
            temp_directory.to_string_lossy().into_owned(),
        );
        let mut path_values = Vec::with_capacity(environment.path_directories.len());
        for path in &environment.path_directories {
            let path = resolve_existing_path(path, "environment.pathDirectories")?;
            path_values.push(path.to_string_lossy().into_owned());
        }
        entries.insert("PATH".to_string(), path_values.join(";"));
        let mut block = Vec::new();
        for (name, value) in entries {
            let entry = format!("{name}={value}");
            block.extend(OsStr::new(&entry).encode_wide());
            block.push(0);
        }
        block.push(0);
        Ok(block)
    }

    fn inspect_member(job: &SupervisorJob, pid: u32) -> Result<ProcessIdentity, String> {
        let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if raw.is_null() {
            return Err(format!(
                "OpenProcess({pid}) failed: {}",
                io::Error::last_os_error()
            ));
        }
        let handle = unsafe { OwnedHandle::from_raw_handle(raw as _) };
        let identity = capture_identity(handle.as_raw_handle() as _, pid)?;
        if !job.active_process_ids()?.contains(&pid) {
            return Err(format!(
                "Job member PID {pid} changed during identity capture"
            ));
        }
        Ok(identity)
    }

    fn open_pipes() -> Result<(OwnedHandle, OwnedHandle, OwnedHandle, OwnedHandle), String> {
        unsafe {
            let mut stdout_read = ptr::null_mut();
            let mut stdout_write = ptr::null_mut();
            let mut stderr_read = ptr::null_mut();
            let mut stderr_write = ptr::null_mut();
            let mut attributes = SecurityAttributes {
                length: std::mem::size_of::<SecurityAttributes>() as u32,
                security_descriptor: ptr::null_mut(),
                inherit_handle: 1,
            };
            if CreatePipe(&mut stdout_read, &mut stdout_write, &mut attributes, 0) == 0 {
                return Err(format!(
                    "CreatePipe(stdout) failed: {}",
                    io::Error::last_os_error()
                ));
            }
            if SetHandleInformation(stdout_read, HANDLE_FLAG_INHERIT, 0) == 0 {
                CloseHandle(stdout_read);
                CloseHandle(stdout_write);
                return Err(format!(
                    "SetHandleInformation(stdout) failed: {}",
                    io::Error::last_os_error()
                ));
            }
            if CreatePipe(&mut stderr_read, &mut stderr_write, &mut attributes, 0) == 0 {
                CloseHandle(stdout_read);
                CloseHandle(stdout_write);
                return Err(format!(
                    "CreatePipe(stderr) failed: {}",
                    io::Error::last_os_error()
                ));
            }
            if SetHandleInformation(stderr_read, HANDLE_FLAG_INHERIT, 0) == 0 {
                CloseHandle(stdout_read);
                CloseHandle(stdout_write);
                CloseHandle(stderr_read);
                CloseHandle(stderr_write);
                return Err(format!(
                    "SetHandleInformation(stderr) failed: {}",
                    io::Error::last_os_error()
                ));
            }
            Ok((
                OwnedHandle::from_raw_handle(stdout_read as _),
                OwnedHandle::from_raw_handle(stdout_write as _),
                OwnedHandle::from_raw_handle(stderr_read as _),
                OwnedHandle::from_raw_handle(stderr_write as _),
            ))
        }
    }

    fn spawn_child(
        executable: &Path,
        arguments: &[String],
        working_directory: &Path,
        expected_executable: &Path,
        environment: &[u16],
        cleanup_deadline: Instant,
    ) -> Result<SpawnedChild, String> {
        let job = SupervisorJob::create()?;
        let (stdout_read, stdout_write, stderr_read, stderr_write) = open_pipes()?;
        let (process, thread, pid) = create_process(
            executable,
            arguments,
            working_directory,
            stdout_write.as_raw_handle() as _,
            stderr_write.as_raw_handle() as _,
            environment,
            cleanup_deadline,
        )?;
        drop(stdout_write);
        drop(stderr_write);
        let mut assigned = false;
        let inspected = (|| {
            job.assign(process.as_raw_handle() as _)?;
            assigned = true;
            let root_identity = capture_identity(process.as_raw_handle() as _, pid)?;
            if root_identity.executable_path != expected_executable {
                return Err(format!(
                    "launched executable identity mismatch: expected `{}`, actual `{}`",
                    expected_executable.display(),
                    root_identity.executable_path.display()
                ));
            }
            let members = job.active_process_ids()?;
            if !members.contains(&pid) {
                return Err(format!("launched PID {pid} was not assigned to its Job"));
            }
            let member_identities = members
                .into_iter()
                .filter(|member| *member != pid)
                .map(|member| inspect_member(&job, member))
                .collect::<Result<Vec<_>, _>>()?;
            let resumed = unsafe { ResumeThread(thread.as_raw_handle() as _) };
            if resumed != 1 {
                return Err(format!(
                    "ResumeThread failed: {}",
                    io::Error::last_os_error()
                ));
            }
            Ok((root_identity, member_identities))
        })();
        let (root_identity, member_identities) = match inspected {
            Ok(value) => value,
            Err(error) => {
                let cleanup = if assigned {
                    job.terminate_and_wait(cleanup_deadline)
                } else {
                    // The process has not entered the Job yet.  Terminate it
                    // through its owned handle, then still prove the Job is
                    // empty before the Job handle is closed.
                    let terminated = unsafe { TerminateProcess(process.as_raw_handle() as _, 127) };
                    let waited = wait_process(process.as_raw_handle() as _, cleanup_deadline);
                    let zero = job.active_process_ids().map(|ids| ids.is_empty());
                    if terminated == 0 {
                        Err(format!(
                            "TerminateProcess(handle) failed: {}",
                            io::Error::last_os_error()
                        ))
                    } else {
                        match waited {
                            Ok(Some(_)) => zero.and_then(|is_zero| {
                                if is_zero {
                                    Ok(())
                                } else {
                                    Err("new Job retained a member after launch failure".to_string())
                                }
                            }),
                            Ok(None) => Err(
                                "incomplete launch process did not settle before the absolute deadline"
                                    .to_string(),
                            ),
                            Err(error) => Err(format!("launch cleanup wait failed: {error}")),
                        }
                    }
                };
                let remaining = job.active_process_ids();
                return Err(format!(
                    "{error}; owned Job cleanup={cleanup:?}; active-members={remaining:?}"
                ));
            }
        };
        Ok(SpawnedChild {
            job,
            process,
            _thread: thread,
            stdout: Some(unsafe { File::from_raw_handle(stdout_read.into_raw_handle()) }),
            stderr: Some(unsafe { File::from_raw_handle(stderr_read.into_raw_handle()) }),
            root_identity,
            member_identities,
        })
    }

    fn wait_process(process: RawHandle, deadline: Instant) -> Result<Option<u32>, String> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(None);
        }
        let timeout = remaining
            .as_millis()
            .saturating_add(1)
            .min(u32::MAX as u128) as u32;
        let result = unsafe { WaitForSingleObject(process, timeout) };
        match result {
            WAIT_OBJECT_0 => {
                let mut exit_code = 0u32;
                if unsafe { GetExitCodeProcess(process, &mut exit_code) } == 0 {
                    Err(format!(
                        "GetExitCodeProcess failed: {}",
                        io::Error::last_os_error()
                    ))
                } else {
                    Ok(Some(exit_code))
                }
            }
            WAIT_TIMEOUT => Ok(None),
            WAIT_FAILED => Err(format!(
                "WaitForSingleObject failed: {}",
                io::Error::last_os_error()
            )),
            other => Err(format!("WaitForSingleObject returned unexpected {other}")),
        }
    }

    fn cycle_result_from_output(
        output: &CappedOutput,
        result_limit: usize,
        expected_scenario: &str,
        expected_seed: u64,
        expected_iteration: u32,
        expected_exit_code: i32,
    ) -> Result<Value, String> {
        if output.truncated {
            return Err(format!(
                "cycle stdout exceeded {} bytes (observed {})",
                result_limit, output.total_bytes
            ));
        }
        let text = std::str::from_utf8(&output.bytes)
            .map_err(|error| format!("cycle stdout is not UTF-8: {error}"))?;
        let lines = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect::<Vec<_>>();
        if lines.len() != 1 {
            return Err(format!(
                "cycle stdout must contain exactly one JSON result (found {})",
                lines.len()
            ));
        }
        if lines[0].len() > result_limit {
            return Err(format!("cycle JSON result exceeds {} bytes", result_limit));
        }
        let value: Value = serde_json::from_str(lines[0])
            .map_err(|error| format!("cycle stdout JSON malformed: {error}"))?;
        if !value.is_object() || value["schemaVersion"] != 1 {
            return Err("cycle JSON result has unsupported schemaVersion".to_string());
        }
        if value["scenario"] != expected_scenario {
            return Err(format!(
                "cycle JSON scenario mismatch: expected {expected_scenario}"
            ));
        }
        let expected_status = if expected_exit_code == 0 {
            "completed"
        } else {
            "failed"
        };
        if value["status"] != expected_status {
            return Err(format!(
                "cycle JSON result status mismatch: expected {expected_status}"
            ));
        }
        if value["seed"] != expected_seed || value["iteration"] != expected_iteration {
            return Err("cycle JSON seed/iteration mismatch".to_string());
        }
        Ok(value)
    }

    fn identity_json(identity: &ProcessIdentity) -> Value {
        let redacted_path = redacted_executable_path(&identity.executable_path);
        json!({
            "processId": identity.process_id,
            "creationTime100ns": identity.creation_time_100ns,
            "executablePath": redacted_path,
            "executablePathHash": sha256_file(&identity.executable_path).unwrap_or_default(),
        })
    }

    fn redacted_executable_path(path: &Path) -> String {
        let executable_path = path.to_string_lossy();
        executable_path
            .find("target-native-next")
            .map(|index| executable_path[index..].replace('\\', "/"))
            .unwrap_or_else(|| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "unknown.exe".to_string())
            })
    }

    fn run_manifest(manifest: &SupervisorManifest) -> Result<Value, Value> {
        let (supervisor, helper, cycle) = match validate_supervisor_manifest(&manifest) {
            Ok(paths) => paths,
            Err(error) => {
                return Err(json!({
                    "schemaVersion": SUPERVISOR_SCHEMA_VERSION,
                    "status": "rejected",
                    "revision": manifest.revision,
                    "launched": false,
                    "error": error,
                }));
            }
        };
        let environment = match environment_block(&manifest.environment) {
            Ok(environment) => environment,
            Err(error) => {
                return Err(json!({
                    "schemaVersion": SUPERVISOR_SCHEMA_VERSION,
                    "status": "rejected",
                    "revision": manifest.revision,
                    "launched": false,
                    "error": error,
                }));
            }
        };
        let ansi_corpus = match load_ansi_corpus(&manifest.ansi_corpus) {
            Ok(corpus) => corpus,
            Err(error) => {
                return Err(json!({
                    "schemaVersion": SUPERVISOR_SCHEMA_VERSION,
                    "status": "rejected",
                    "revision": manifest.revision,
                    "launched": false,
                    "error": error,
                }));
            }
        };
        let _listener_baseline = match query_tcp_listeners() {
            Ok(listeners) => listeners,
            Err(error) => {
                return Err(json!({
                    "schemaVersion": SUPERVISOR_SCHEMA_VERSION,
                    "status": "rejected",
                    "revision": manifest.revision,
                    "launched": false,
                    "error": format!("listener baseline query failed: {error}"),
                }));
            }
        };
        let logical_processor_count = logical_processor_count().map_err(|error| {
            json!({
                "schemaVersion": SUPERVISOR_SCHEMA_VERSION,
                "status": "rejected",
                "revision": manifest.revision,
                "launched": false,
                "error": format!("logical processor audit failed: {error}"),
            })
        })?;
        let suite_deadline = Instant::now()
            .checked_add(Duration::from_millis(manifest.budgets.suite_deadline_ms))
            .ok_or_else(|| json!({"schemaVersion": 1, "status": "rejected", "launched": false, "error": "suite deadline overflow"}))?;
        let mut cycles = Vec::with_capacity(manifest.iterations as usize);
        let mut status = "passed";
        for iteration in 0..manifest.iterations {
            let scenario =
                &manifest.scenario_catalog[iteration as usize % manifest.scenario_catalog.len()];
            let cycle_budget = Duration::from_millis(manifest.budgets.cycle_deadline_ms);
            let launch_now = Instant::now();
            if suite_deadline <= launch_now
                || suite_deadline.saturating_duration_since(launch_now) <= cycle_budget
            {
                status = "failed";
                cycles.push(json!({
                    "iteration": iteration + 1,
                    "scenario": scenario.name,
                    "status": "failed",
                    "outcome": "suite-timeout",
                    "exitCode": null,
                    "durationMs": 0,
                    "stdoutBytes": 0,
                    "stderrBytes": 0,
                    "activeProcessZero": true,
                    "rootIdentity": null,
                    "memberIdentities": [],
                    "result": null,
                    "error": "suite deadline exceeded before launch",
                }));
                break;
            }
            let cycle_deadline = (launch_now + cycle_budget).min(suite_deadline);
            let cleanup_deadline = cycle_deadline
                .checked_add(Duration::from_millis(manifest.budgets.cleanup_deadline_ms))
                .unwrap_or(suite_deadline)
                .min(suite_deadline);
            let started = Instant::now();
            let mut arguments = scenario.arguments.clone();
            arguments.push("--seed".to_string());
            arguments.push(manifest.seed.to_string());
            arguments.push("--iteration".to_string());
            arguments.push((iteration + 1).to_string());
            if arguments.len() > SUPERVISOR_MAX_ARGUMENTS {
                status = "failed";
                cycles.push(json!({
                    "iteration": iteration + 1,
                    "scenario": scenario.name,
                    "status": "failed",
                    "outcome": "invalid-arguments",
                    "exitCode": null,
                    "durationMs": 0,
                    "stdoutBytes": 0,
                    "stderrBytes": 0,
                    "activeProcessZero": false,
                    "rootIdentity": null,
                    "memberIdentities": [],
                    "result": null,
                    "error": "scenario argument count exceeded bound",
                }));
                break;
            }
            let host_process_handle_count_before =
                current_process_handle_count().map_err(|error| {
                    json!({
                        "schemaVersion": SUPERVISOR_SCHEMA_VERSION,
                        "status": "rejected",
                        "revision": manifest.revision,
                        "launched": false,
                        "error": format!("host process handle baseline failed: {error}"),
                    })
                })?;
            let mut child = match spawn_child(
                &cycle,
                &arguments,
                &manifest.working_directory,
                &cycle,
                &environment,
                cleanup_deadline,
            ) {
                Ok(child) => child,
                Err(error) => {
                    status = "failed";
                    cycles.push(json!({
                        "iteration": iteration + 1,
                        "scenario": scenario.name,
                        "status": "failed",
                        "outcome": "launch-failed",
                        "exitCode": null,
                        "durationMs": started.elapsed().as_millis(),
                        "stdoutBytes": 0,
                        "stderrBytes": 0,
                        "activeProcessZero": false,
                        "rootIdentity": null,
                        "memberIdentities": [],
                        "result": null,
                        "error": error,
                    }));
                    break;
                }
            };
            let process_handle_count_before = match process_handle_count(
                child.process.as_raw_handle() as _,
                child.root_identity.process_id,
            ) {
                Ok(count) => count,
                Err(error) => {
                    let cleanup = child.job.terminate_and_wait(cleanup_deadline);
                    let remaining = child.job.active_process_ids();
                    return Err(json!({
                        "schemaVersion": SUPERVISOR_SCHEMA_VERSION,
                        "status": "rejected",
                        "revision": manifest.revision,
                        "launched": true,
                        "error": format!("process handle audit failed: {error}; cleanup={cleanup:?}; active-members={remaining:?}"),
                    }));
                }
            };
            let job_members_before = match child.job.active_process_ids() {
                Ok(members) => members,
                Err(error) => {
                    let cleanup = child.job.terminate_and_wait(cleanup_deadline);
                    let remaining = child.job.active_process_ids();
                    return Err(json!({
                        "schemaVersion": SUPERVISOR_SCHEMA_VERSION,
                        "status": "rejected",
                        "revision": manifest.revision,
                        "launched": true,
                        "error": format!("Job member baseline audit failed: {error}; cleanup={cleanup:?}; active-members={remaining:?}"),
                    }));
                }
            };
            let listeners_before = match query_tcp_listeners() {
                Ok(listeners) => listeners,
                Err(error) => {
                    let cleanup = child.job.terminate_and_wait(cleanup_deadline);
                    let remaining = child.job.active_process_ids();
                    return Err(json!({
                        "schemaVersion": SUPERVISOR_SCHEMA_VERSION,
                        "status": "rejected",
                        "revision": manifest.revision,
                        "launched": true,
                        "error": format!("listener audit failed: {error}; cleanup={cleanup:?}; active-members={remaining:?}"),
                    }));
                }
            };
            let listeners_during = match query_tcp_listeners() {
                Ok(listeners) => listeners,
                Err(error) => {
                    let cleanup = child.job.terminate_and_wait(cleanup_deadline);
                    let remaining = child.job.active_process_ids();
                    return Err(json!({
                        "schemaVersion": SUPERVISOR_SCHEMA_VERSION,
                        "status": "rejected",
                        "revision": manifest.revision,
                        "launched": true,
                        "error": format!("listener during-cycle audit failed: {error}; cleanup={cleanup:?}; active-members={remaining:?}"),
                    }));
                }
            };
            let (stdout_thread, stdout_reader) = spawn_capped_reader(
                {
                    let stdout = child.stdout.take().expect("supervisor stdout pipe");
                    stdout
                },
                manifest.budgets.stdout_bytes,
            );
            let (stderr_thread, stderr_reader) = spawn_capped_reader(
                {
                    let stderr = child.stderr.take().expect("supervisor stderr pipe");
                    stderr
                },
                manifest.budgets.stderr_bytes,
            );
            let _reader_threads = (&stdout_thread, &stderr_thread);
            let mut outcome = "completed";
            let mut active_process_zero = false;
            let mut settlement_error = None::<String>;
            let exit_code = match wait_process(child.process.as_raw_handle() as _, cycle_deadline) {
                Ok(Some(code)) => Some(code),
                Ok(None) => {
                    outcome = "timeout";
                    let inspection = (|| {
                        let member_ids = child.job.active_process_ids()?;
                        for member in member_ids {
                            if member == child.root_identity.process_id
                                || child
                                    .member_identities
                                    .iter()
                                    .any(|identity| identity.process_id == member)
                            {
                                continue;
                            }
                            child
                                .member_identities
                                .push(inspect_member(&child.job, member)?);
                        }
                        Ok::<(), String>(())
                    })();
                    if let Err(error) = inspection {
                        settlement_error = Some(format!(
                            "Job member inspection failed during timeout: {error}"
                        ));
                    }
                    match child.job.terminate_and_wait(cleanup_deadline) {
                        Ok(()) => match child.job.active_process_ids() {
                            Ok(ids) => active_process_zero = ids.is_empty(),
                            Err(error) => {
                                active_process_zero = false;
                                let detail = format!(
                                    "Job member inspection failed after timeout cleanup: {error}"
                                );
                                settlement_error = Some(match settlement_error.take() {
                                    Some(previous) => format!("{previous}; {detail}"),
                                    None => detail,
                                });
                            }
                        },
                        Err(error) => {
                            settlement_error = Some(match settlement_error.take() {
                                Some(previous) => format!("{previous}; cleanup: {error}"),
                                None => error,
                            });
                        }
                    }
                    None
                }
                Err(error) => {
                    outcome = "wait-failed";
                    let cleanup = child.job.terminate_and_wait(cleanup_deadline);
                    let members_after_cleanup = child.job.active_process_ids().map_err(|error| {
                        json!({
                            "schemaVersion": SUPERVISOR_SCHEMA_VERSION,
                            "status": "rejected",
                            "revision": manifest.revision,
                            "launched": true,
                            "error": format!("Job member inspection failed after wait error: {error}"),
                        })
                    })?;
                    active_process_zero = cleanup.is_ok() && members_after_cleanup.is_empty();
                    let error = match cleanup {
                        Ok(()) => error,
                        Err(cleanup_error) => {
                            format!("{error}; Job cleanup failed: {cleanup_error}")
                        }
                    };
                    cycles.push(json!({
                        "iteration": iteration + 1,
                        "scenario": scenario.name,
                        "status": "failed",
                        "outcome": outcome,
                        "exitCode": null,
                        "durationMs": started.elapsed().as_millis(),
                        "stdoutBytes": 0,
                        "stderrBytes": 0,
                        "activeProcessZero": active_process_zero,
                        "rootIdentity": identity_json(&child.root_identity),
                        "memberIdentities": json!(child
                            .member_identities
                            .iter()
                            .map(identity_json)
                            .collect::<Vec<_>>()),
                        "result": null,
                        "error": error,
                    }));
                    status = "failed";
                    finish_capped_reader(stdout_reader, stdout_thread, cleanup_deadline, "stdout")
                        .map_err(|error| {
                            json!({
                                "schemaVersion": SUPERVISOR_SCHEMA_VERSION,
                                "status": "rejected",
                                "revision": manifest.revision,
                                "launched": true,
                                "error": format!("stdout reader failed after wait error: {error}"),
                            })
                        })?;
                    finish_capped_reader(stderr_reader, stderr_thread, cleanup_deadline, "stderr")
                        .map_err(|error| {
                            json!({
                                "schemaVersion": SUPERVISOR_SCHEMA_VERSION,
                                "status": "rejected",
                                "revision": manifest.revision,
                                "launched": true,
                                "error": format!("stderr reader failed after wait error: {error}"),
                            })
                        })?;
                    break;
                }
            };
            if outcome == "completed" {
                match child.job.wait_active_process_zero(cleanup_deadline) {
                    Ok(()) => active_process_zero = true,
                    Err(error) => {
                        outcome = "residue";
                        settlement_error = Some(error);
                        match child.job.terminate_and_wait(cleanup_deadline) {
                            Ok(()) => {
                                active_process_zero = child
                                    .job
                                    .active_process_ids()
                                    .map_err(|error| {
                                        json!({
                                            "schemaVersion": SUPERVISOR_SCHEMA_VERSION,
                                            "status": "rejected",
                                            "revision": manifest.revision,
                                            "launched": true,
                                            "error": format!("Job member inspection failed after cleanup: {error}"),
                                        })
                                    })?
                                    .is_empty();
                            }
                            Err(cleanup_error) => {
                                settlement_error = Some(match settlement_error.take() {
                                    Some(previous) => {
                                        format!("{previous}; cleanup: {cleanup_error}")
                                    }
                                    None => cleanup_error,
                                });
                            }
                        }
                    }
                }
            }
            let stdout =
                finish_capped_reader(stdout_reader, stdout_thread, cleanup_deadline, "stdout")
                    .map_err(|error| {
                        json!({
                            "schemaVersion": SUPERVISOR_SCHEMA_VERSION,
                            "status": "rejected",
                            "revision": manifest.revision,
                            "launched": true,
                            "error": error,
                        })
                    })?;
            let stderr =
                finish_capped_reader(stderr_reader, stderr_thread, cleanup_deadline, "stderr")
                    .map_err(|error| {
                        json!({
                            "schemaVersion": SUPERVISOR_SCHEMA_VERSION,
                            "status": "rejected",
                            "revision": manifest.revision,
                            "launched": true,
                            "error": error,
                        })
                    })?;
            let process_cpu_time = process_cpu_time_100ns(
                child.process.as_raw_handle() as _,
                child.root_identity.process_id,
            )
            .map_err(|error| {
                json!({
                    "schemaVersion": SUPERVISOR_SCHEMA_VERSION,
                    "status": "rejected",
                    "revision": manifest.revision,
                    "launched": true,
                    "error": format!("CPU accounting failed: {error}"),
                })
            })?;
            let wall_time_ms = started.elapsed().as_millis().max(1) as u64;
            let wall_time_100ns = wall_time_ms.saturating_mul(10_000);
            let core_equivalent_percent = if wall_time_100ns == 0 {
                0.0
            } else {
                process_cpu_time as f64 / wall_time_100ns as f64 * 100.0
            };
            let whole_machine_percent =
                core_equivalent_percent / logical_processor_count.max(1) as f64;
            let job_members_after = child.job.active_process_ids().map_err(|error| {
                json!({
                    "schemaVersion": SUPERVISOR_SCHEMA_VERSION,
                    "status": "rejected",
                    "revision": manifest.revision,
                    "launched": true,
                    "error": format!("Job member inspection failed after reader join: {error}"),
                })
            })?;
            if !job_members_after.is_empty() {
                active_process_zero = false;
                settlement_error = Some(format!(
                    "Job retained active members after cleanup: {job_members_after:?}"
                ));
            }
            let process_handle_count_after = process_handle_count(
                child.process.as_raw_handle() as _,
                child.root_identity.process_id,
            )
            .map_err(|error| {
                json!({
                    "schemaVersion": SUPERVISOR_SCHEMA_VERSION,
                    "status": "rejected",
                    "revision": manifest.revision,
                    "launched": true,
                    "error": format!("process handle audit after cleanup failed: {error}"),
                })
            })?;
            let root_identity = child.root_identity.clone();
            let member_identities = child.member_identities.clone();
            drop(child);
            let host_process_handle_count_after =
                current_process_handle_count().map_err(|error| {
                    json!({
                        "schemaVersion": SUPERVISOR_SCHEMA_VERSION,
                        "status": "rejected",
                        "revision": manifest.revision,
                        "launched": true,
                        "error": format!("host process handle audit after cleanup failed: {error}"),
                    })
                })?;
            let listeners_after = query_tcp_listeners().map_err(|error| {
                json!({
                    "schemaVersion": SUPERVISOR_SCHEMA_VERSION,
                    "status": "rejected",
                    "revision": manifest.revision,
                    "launched": true,
                    "error": format!("listener audit after cleanup failed: {error}"),
                })
            })?;
            let mut owned_pids = vec![root_identity.process_id];
            owned_pids.extend(member_identities.iter().map(|identity| identity.process_id));
            owned_pids.sort_unstable();
            owned_pids.dedup();
            let external_listeners_unchanged = match verify_external_listeners_unchanged(
                &listeners_before,
                &listeners_after,
                &owned_pids,
            ) {
                Ok(()) => true,
                Err(error) => {
                    settlement_error = Some(match settlement_error.take() {
                        Some(previous) => format!("{previous}; {error}"),
                        None => error,
                    });
                    false
                }
            };
            let owned_listeners_before = filter_owned_listeners(&listeners_before, &owned_pids)
                .map_err(|error| {
                    json!({
                        "schemaVersion": SUPERVISOR_SCHEMA_VERSION,
                        "status": "rejected",
                        "revision": manifest.revision,
                        "launched": true,
                        "error": format!("owned listener baseline inspection failed: {error}"),
                    })
                })?;
            let owned_listeners_after = filter_owned_listeners(&listeners_after, &owned_pids)
                .map_err(|error| {
                    json!({
                        "schemaVersion": SUPERVISOR_SCHEMA_VERSION,
                        "status": "rejected",
                        "revision": manifest.revision,
                        "launched": true,
                        "error": format!("owned listener after inspection failed: {error}"),
                    })
                })?;
            let parsed = if stderr.truncated {
                Err(format!(
                    "cycle stderr exceeded {} bytes (observed {})",
                    manifest.budgets.stderr_bytes, stderr.total_bytes
                ))
            } else if outcome == "completed"
                && exit_code == Some(scenario.expected_exit_code as u32)
            {
                cycle_result_from_output(
                    &stdout,
                    manifest.budgets.result_bytes,
                    &scenario.name,
                    manifest.seed,
                    iteration + 1,
                    scenario.expected_exit_code,
                )
            } else {
                Err(format!(
                    "cycle exited with {:?}; stderr bytes={}",
                    exit_code, stderr.total_bytes
                ))
            };
            let environment_report = parsed
                .as_ref()
                .ok()
                .and_then(|value| value.get("environment"))
                .cloned()
                .unwrap_or_else(|| {
                    json!({
                        "secretPresent": false,
                        "systemRootPresent": true,
                        "tempPresent": true,
                        "pathPresent": true,
                    })
                });
            let mut cycle_status = "passed";
            let mut error = None;
            if outcome != "completed" {
                cycle_status = "failed";
                error = Some(outcome.to_string());
            } else if parsed.is_err() {
                cycle_status = "failed";
                error = parsed.as_ref().err().cloned();
            } else if scenario.expected_exit_code != 0 {
                cycle_status = "failed";
                error = Some(format!(
                    "cycle exited with expected nonzero code {}",
                    scenario.expected_exit_code
                ));
            }
            if let Some(settlement_error) = settlement_error {
                cycle_status = "failed";
                error = Some(settlement_error);
            }
            if environment_report["secretPresent"] == true {
                cycle_status = "failed";
                error = Some(
                    "cycle received a non-allowlisted secret environment variable".to_string(),
                );
            }
            if cycle_status == "failed" {
                status = "failed";
            }
            let mut cycle_document = serde_json::Map::new();
            cycle_document.insert("iteration".to_string(), json!(iteration + 1));
            cycle_document.insert("scenario".to_string(), json!(scenario.name));
            cycle_document.insert("status".to_string(), json!(cycle_status));
            cycle_document.insert("outcome".to_string(), json!(outcome));
            cycle_document.insert("exitCode".to_string(), json!(exit_code));
            cycle_document.insert("durationMs".to_string(), json!(wall_time_ms));
            cycle_document.insert("stdoutBytes".to_string(), json!(stdout.total_bytes));
            cycle_document.insert("stderrBytes".to_string(), json!(stderr.total_bytes));
            cycle_document.insert("activeProcessZero".to_string(), json!(active_process_zero));
            cycle_document.insert("environment".to_string(), environment_report);
            cycle_document.insert(
                "cpu".to_string(),
                json!({
                    "processCpuTime100ns": process_cpu_time,
                    "wallTimeMs": wall_time_ms,
                    "logicalProcessorCount": logical_processor_count,
                    "coreEquivalentPercent": core_equivalent_percent,
                    "wholeMachinePercent": whole_machine_percent,
                }),
            );
            cycle_document.insert(
                "jobAudit".to_string(),
                json!({
                    "jobMembersBefore": job_members_before.len(),
                    "jobMembersAfter": job_members_after.len(),
                    "processHandleCountBefore": process_handle_count_before,
                    "processHandleCountAfter": process_handle_count_after,
                    "hostProcessHandleCountBefore": host_process_handle_count_before,
                    "hostProcessHandleCountAfter": host_process_handle_count_after,
                    "ownedListenersBefore": owned_listeners_before,
                    "ownedListenersDuring": filter_owned_listeners(&listeners_during, &owned_pids)
                        .map_err(|error| json!({
                            "schemaVersion": SUPERVISOR_SCHEMA_VERSION,
                            "status": "rejected",
                            "revision": manifest.revision,
                            "launched": true,
                            "error": format!("owned listener during inspection failed: {error}"),
                        }))?,
                    "ownedListenersAfter": owned_listeners_after,
                    "externalListenersUnchanged": external_listeners_unchanged,
                    "externalListenersBefore": listeners_before,
                    "externalListenersAfter": listeners_after,
                }),
            );
            cycle_document.insert("rootIdentity".to_string(), identity_json(&root_identity));
            cycle_document.insert(
                "memberIdentities".to_string(),
                json!(member_identities
                    .iter()
                    .map(identity_json)
                    .collect::<Vec<_>>()),
            );
            cycle_document.insert("result".to_string(), Value::Null);
            cycle_document.insert("error".to_string(), Value::Null);
            if let Some(error) = error {
                cycle_document.insert("error".to_string(), json!(error));
            }
            if let Ok(parsed) = parsed {
                cycle_document.insert("result".to_string(), parsed);
            }
            cycles.push(Value::Object(cycle_document));
            if status == "failed" {
                break;
            }
            // The allowlisted cycle executable is intentionally re-resolved and
            // hash-verified once per suite, not imported into this process.
            let _ = (&supervisor, &helper, manifest.seed);
        }
        Ok(json!({
            "schemaVersion": SUPERVISOR_SCHEMA_VERSION,
            "status": status,
            "revision": manifest.revision,
            "gitRevision": manifest.git_revision,
            "buildId": manifest.build_id,
            "sourceTreeState": manifest.source_tree_state,
            "seed": manifest.seed,
            "iterations": manifest.iterations,
            "completedCycles": cycles.len(),
            "ansiCorpus": ansi_corpus,
            "cycles": cycles,
        }))
    }

    fn open_retained_evidence_root(path: &Path) -> Result<(PathBuf, OwnedHandle), String> {
        let root = resolve_existing_path(path, "evidenceRoot")?;
        if !root.is_dir() {
            return Err(format!(
                "evidenceRoot is not a directory: {}",
                root.display()
            ));
        }
        let wide = utf16(root.as_os_str());
        let raw = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_LIST_DIRECTORY,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null_mut(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                ptr::null_mut(),
            )
        };
        if raw.is_null() || raw == INVALID_HANDLE_VALUE {
            return Err(format!(
                "open retained evidenceRoot handle failed: {}",
                io::Error::last_os_error()
            ));
        }
        Ok((root, unsafe { OwnedHandle::from_raw_handle(raw as _) }))
    }

    fn ensure_directory(path: &Path, label: &str) -> Result<(), String> {
        reject_reparse_ancestors(path, label)?;
        match fs::create_dir(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                if path.is_dir() {
                    Ok(())
                } else {
                    Err(format!("{label} exists but is not a directory"))
                }
            }
            Err(error) => Err(format!("create {label} `{}`: {error}", path.display())),
        }
    }

    fn create_exclusive_run_directory(root: &Path) -> Result<(String, PathBuf), String> {
        let phase_root = root.join("phase-03-process-soak");
        let runs_root = phase_root.join("runs");
        ensure_directory(&phase_root, "phase evidence root")?;
        ensure_directory(&runs_root, "phase evidence runs root")?;
        for _ in 0..8 {
            let run_id = Uuid::now_v7().simple().to_string();
            let run_directory = runs_root.join(&run_id);
            reject_reparse_ancestors(&run_directory, "evidence run")?;
            match fs::create_dir(&run_directory) {
                Ok(()) => return Ok((run_id, run_directory)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(format!(
                        "create exclusive evidence run `{}`: {error}",
                        run_directory.display()
                    ));
                }
            }
        }
        Err("could not allocate a unique evidence run directory".to_string())
    }

    fn write_atomic_json(run_directory: &Path, name: &str, value: &Value) -> Result<(), String> {
        if !name.ends_with(".json") || name.contains('\\') || name.contains('/') {
            return Err("unsafe evidence artifact name".to_string());
        }
        reject_reparse_ancestors(run_directory, "evidence run")?;
        let destination = run_directory.join(name);
        if destination.exists() {
            return Err(format!(
                "evidence artifact already exists: {}",
                destination.display()
            ));
        }
        let temporary = run_directory.join(format!(".{name}.{}.tmp", Uuid::now_v7().simple()));
        let bytes =
            serde_json::to_vec(value).map_err(|error| format!("serialize {name}: {error}"))?;
        if bytes.len() > SUPERVISOR_MAX_RESULT_BYTES {
            return Err(format!(
                "evidence artifact {name} exceeds bounded result size"
            ));
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| format!("create temporary evidence artifact: {error}"))?;
        file.write_all(&bytes)
            .map_err(|error| format!("write temporary evidence artifact: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("flush temporary evidence artifact: {error}"))?;
        drop(file);
        if destination.exists() {
            fs::remove_file(&temporary)
                .map_err(|error| format!("remove temporary evidence artifact: {error}"))?;
            return Err(format!(
                "evidence artifact appeared during atomic publish: {}",
                destination.display()
            ));
        }
        fs::rename(&temporary, &destination)
            .map_err(|error| format!("atomically publish evidence artifact {name}: {error}"))
    }

    fn percentile(values: &[u64], fraction: f64) -> u64 {
        if values.is_empty() {
            return 0;
        }
        let index = ((values.len() as f64 * fraction).ceil() as usize).saturating_sub(1);
        values[index.min(values.len() - 1)]
    }

    fn publish_evidence(
        manifest: &SupervisorManifest,
        manifest_bytes: &[u8],
        manifest_sha256: &str,
        result: &mut Value,
    ) -> Result<(), String> {
        let supervisor = canonical_file(&manifest.supervisor_executable, "supervisorExecutable")?;
        let helper = canonical_file(&manifest.helper_executable, "helperExecutable")?;
        let cycle = canonical_file(&manifest.cycle_executable, "cycleExecutable")?;
        let target_directory =
            resolve_existing_path(&manifest.target_directory, "targetDirectory")?;
        let (root, _retained_root_handle) = open_retained_evidence_root(&manifest.evidence_root)?;
        let (run_id, run_directory) = create_exclusive_run_directory(&root)?;
        let ansi_corpus = result["ansiCorpus"].clone();
        let manifest_artifact = json!({
            "schemaVersion": SUPERVISOR_SCHEMA_VERSION,
            "revision": manifest.revision,
            "gitRevision": manifest.git_revision,
            "buildId": manifest.build_id,
            "sourceTreeState": manifest.source_tree_state,
            "targetDirectory": redacted_executable_path(&target_directory),
            "sha256": manifest_sha256,
            "bytes": manifest_bytes.len(),
            "seed": manifest.seed,
            "iterations": manifest.iterations,
            "budgets": manifest.budgets,
            "scenarioCatalog": manifest.scenario_catalog,
            "binaries": {
                "supervisorExecutable": redacted_executable_path(&supervisor),
                "supervisorSha256": manifest.supervisor_sha256,
                "helperExecutable": redacted_executable_path(&helper),
                "helperSha256": manifest.helper_sha256,
                "cycleExecutable": redacted_executable_path(&cycle),
                "cycleSha256": manifest.cycle_sha256,
            },
            "ansiCorpus": ansi_corpus,
        });
        write_atomic_json(&run_directory, "manifest.json", &manifest_artifact)?;

        let cycles = result["cycles"]
            .as_array()
            .ok_or_else(|| "supervisor result cycles are not an array".to_string())?;
        let mut durations = Vec::with_capacity(cycles.len());
        for cycle in cycles {
            durations.push(
                cycle["durationMs"]
                    .as_u64()
                    .ok_or_else(|| "cycle result omitted durationMs".to_string())?,
            );
        }
        durations.sort_unstable();
        let performance = json!({
            "schemaVersion": SUPERVISOR_SCHEMA_VERSION,
            "sampleCount": durations.len(),
            "samplesMs": durations,
            "durationMs": {
                "p50": percentile(&durations, 0.50),
                "p95": percentile(&durations, 0.95),
                "maximum": durations.last().copied().unwrap_or_default(),
            },
            "cpu": {
                "samples": cycles.iter().map(|cycle| cycle["cpu"].clone()).collect::<Vec<_>>(),
                "denominators": "raw child CPU time / monotonic wall interval / logical processors",
            },
        });
        write_atomic_json(&run_directory, "performance.json", &performance)?;
        let conformance = json!({
            "schemaVersion": SUPERVISOR_SCHEMA_VERSION,
            "revision": manifest.revision,
            "gitRevision": manifest.git_revision,
            "buildId": manifest.build_id,
            "manifestSha256": manifest_sha256,
            "ansiCorpus": ansi_corpus,
            "outputProtocol": "exactly-one-json-result",
            "activeProcessZeroRequired": true,
            "identity": "PID + creationTime100ns + canonical executable path",
            "readerCaps": {
                "stdoutBytes": manifest.budgets.stdout_bytes,
                "stderrBytes": manifest.budgets.stderr_bytes,
                "resultBytes": manifest.budgets.result_bytes,
            },
            "deadlinesMs": {
                "suite": manifest.budgets.suite_deadline_ms,
                "cycle": manifest.budgets.cycle_deadline_ms,
                "cleanup": manifest.budgets.cleanup_deadline_ms,
            },
            "scenarios": cycles.iter().map(|cycle| json!({
                "iteration": cycle["iteration"],
                "name": cycle["scenario"],
                "status": cycle["status"],
                "outcome": cycle["outcome"],
            })).collect::<Vec<_>>(),
        });
        write_atomic_json(&run_directory, "conformance.json", &conformance)?;
        result["runId"] = json!(run_id);
        result["runDirectory"] = json!(Path::new("phase-03-process-soak")
            .join("runs")
            .join(&run_id)
            .to_string_lossy()
            .replace('\\', "/"));
        write_atomic_json(&run_directory, "summary.json", result)?;
        let run_document = json!({
            "schemaVersion": SUPERVISOR_SCHEMA_VERSION,
            "status": result["status"],
            "runId": result["runId"],
            "runDirectory": result["runDirectory"],
            "summary": result,
        });
        write_atomic_json(&run_directory, "run.json", &run_document)
    }

    fn publish_failure_evidence(
        manifest: &SupervisorManifest,
        result: &mut Value,
    ) -> Result<(), String> {
        let source_root = find_worktree_root(&manifest.working_directory)?;
        let candidate_root = resolve_existing_path(&manifest.evidence_root, "evidenceRoot")?;
        if !path_is_within(&candidate_root, &source_root) {
            return Err("failure evidence root escapes the canonical worktree root".to_string());
        }
        let (root, _retained_root_handle) = open_retained_evidence_root(&candidate_root)?;
        if !path_is_within(&root, &source_root) {
            return Err("failure evidence root escapes the canonical worktree root".to_string());
        }
        let (run_id, run_directory) = create_exclusive_run_directory(&root)?;
        let relative_directory = Path::new("phase-03-process-soak")
            .join("runs")
            .join(&run_id)
            .to_string_lossy()
            .replace('\\', "/");
        let failure = json!({
            "schemaVersion": SUPERVISOR_SCHEMA_VERSION,
            "status": result["status"],
            "runId": run_id,
            "runDirectory": relative_directory,
            "revision": manifest.revision,
            "gitRevision": manifest.git_revision,
            "buildId": manifest.build_id,
            "sourceTreeState": manifest.source_tree_state,
            "failure": "supervisor did not produce a passing cycle result",
        });
        write_atomic_json(&run_directory, "failure.json", &failure)?;
        result["runId"] = json!(run_id);
        result["runDirectory"] = json!(relative_directory);
        Ok(())
    }

    pub(super) fn run_manifest_file(
        path: &Path,
        iterations_override: Option<u32>,
        seed_override: Option<u64>,
    ) -> (Value, i32) {
        if let Err(error) = reject_reparse_ancestors(path, "manifest") {
            return (
                json!({"schemaVersion": 1, "status": "rejected", "launched": false, "error": error}),
                2,
            );
        }
        let mut manifest_file = match std::fs::OpenOptions::new().read(true).open(path) {
            Ok(file) => file,
            Err(error) => {
                return (
                    json!({"schemaVersion": 1, "status": "rejected", "launched": false, "error": format!("read manifest: {error}")}),
                    2,
                )
            }
        };
        let length = match manifest_file.metadata() {
            Ok(metadata) => metadata.len(),
            Err(error) => {
                return (
                    json!({"schemaVersion": 1, "status": "rejected", "launched": false, "error": format!("inspect manifest: {error}")}),
                    2,
                )
            }
        };
        if length > 1024 * 1024 {
            return (
                json!({"schemaVersion": 1, "status": "rejected", "launched": false, "error": "manifest exceeds 1 MiB"}),
                2,
            );
        }
        let mut bytes = Vec::with_capacity(length as usize);
        if let Err(error) = manifest_file.read_to_end(&mut bytes) {
            return (
                json!({"schemaVersion": 1, "status": "rejected", "launched": false, "error": format!("read manifest: {error}")}),
                2,
            );
        }
        let mut manifest: SupervisorManifest = match serde_json::from_slice(&bytes) {
            Ok(manifest) => manifest,
            Err(error) => {
                return (
                    json!({"schemaVersion": 1, "status": "rejected", "launched": false, "error": format!("manifest JSON malformed: {error}")}),
                    2,
                )
            }
        };
        if let Some(iterations) = iterations_override {
            if iterations == 0 || iterations > SUPERVISOR_MAX_ITERATIONS {
                return (
                    json!({"schemaVersion": SUPERVISOR_SCHEMA_VERSION, "status": "rejected", "launched": false, "error": "iterations override is outside bounded limits"}),
                    2,
                );
            }
            manifest.iterations = iterations;
        }
        if let Some(seed) = seed_override {
            manifest.seed = seed;
        }
        if let Err(error) = materialize_manifest_attestation(&mut manifest) {
            return (
                json!({
                    "schemaVersion": SUPERVISOR_SCHEMA_VERSION,
                    "status": "rejected",
                    "launched": false,
                    "error": format!("source/build attestation failed: {error}"),
                }),
                2,
            );
        }
        let result_limit = manifest.budgets.result_bytes;
        match run_manifest(&manifest) {
            Ok(mut result) => {
                if let Err(error) =
                    publish_evidence(&manifest, &bytes, &sha256_bytes(&bytes), &mut result)
                {
                    return (
                        json!({
                            "schemaVersion": SUPERVISOR_SCHEMA_VERSION,
                            "status": "rejected",
                            "launched": true,
                            "error": format!("evidence publication failed: {error}"),
                        }),
                        2,
                    );
                }
                let encoded_length = serde_json::to_vec(&result)
                    .map(|bytes| bytes.len())
                    .unwrap_or(usize::MAX);
                if encoded_length > result_limit {
                    return (
                        json!({
                            "schemaVersion": SUPERVISOR_SCHEMA_VERSION,
                            "status": "rejected",
                            "launched": false,
                            "error": format!(
                                "supervisor result exceeds {} bytes",
                                result_limit
                            ),
                        }),
                        2,
                    );
                }
                let code = if result["status"] == "passed" { 0 } else { 1 };
                (result, code)
            }
            Err(result) => {
                // Keep every rejected-manifest response on the same bounded,
                // exact protocol shape. Internal validation details are
                // reduced to the safe error string; callers must not need to
                // accept a second rejection schema.
                let error = result["error"]
                    .as_str()
                    .unwrap_or("supervisor manifest rejected")
                    .to_string();
                let mut visible = json!({
                    "schemaVersion": SUPERVISOR_SCHEMA_VERSION,
                    "status": "rejected",
                    "launched": false,
                    "error": error,
                });
                if let Err(publication_error) = publish_failure_evidence(&manifest, &mut visible) {
                    visible["evidencePublicationError"] =
                        json!("failure evidence could not be published");
                    let _ = publication_error;
                }
                (visible, 2)
            }
        }
    }
}

fn main() {
    let mut args = std::env::args_os().skip(1);
    let mode = args.next().expect("process-test helper mode");
    let mode = mode.to_string_lossy().to_string();
    let result = match mode.as_str() {
        "supervise" => {
            #[cfg(windows)]
            {
                let mut manifest: Option<std::path::PathBuf> = None;
                let mut iterations: Option<u32> = None;
                let mut seed: Option<u64> = None;
                while let Some(argument) = args.next() {
                    let argument = argument
                        .into_string()
                        .unwrap_or_else(|_| "<non-UTF8>".to_string());
                    match argument.as_str() {
                        "--manifest" => {
                            let Some(path) = args.next() else {
                                eprintln!("--manifest value is required");
                                std::process::exit(2);
                            };
                            manifest = Some(path.into());
                        }
                        "--iterations" => {
                            let value = args.next().and_then(|value| value.into_string().ok());
                            match value.and_then(|value| value.parse::<u32>().ok()) {
                                Some(value) => iterations = Some(value),
                                None => {
                                    eprintln!("--iterations requires a bounded integer");
                                    std::process::exit(2);
                                }
                            }
                        }
                        "--seed" => {
                            let value = args.next().and_then(|value| value.into_string().ok());
                            match value.and_then(|value| value.parse::<u64>().ok()) {
                                Some(value) => seed = Some(value),
                                None => {
                                    eprintln!("--seed requires an unsigned integer");
                                    std::process::exit(2);
                                }
                            }
                        }
                        other => {
                            eprintln!("unknown supervise argument `{other}`");
                            std::process::exit(2);
                        }
                    }
                }
                if manifest.is_none() {
                    eprintln!("supervise requires --manifest <path>");
                    std::process::exit(2);
                }
                let (result, exit_code) = windows_supervisor::run_manifest_file(
                    &manifest.expect("manifest path"),
                    iterations,
                    seed,
                );
                let encoded = serde_json::to_string(&result).expect("serialize supervisor result");
                println!("{encoded}");
                std::process::exit(exit_code);
            }
            #[cfg(not(windows))]
            {
                eprintln!("supervisor requires Windows Job Objects");
                std::process::exit(78);
            }
        }
        "cycle" => run_cycle(args),
        "cycle-worker" => parse_bounded_options(args).and_then(run_cycle_worker),
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
