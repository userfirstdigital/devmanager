use std::io::{BufRead, BufReader, Read};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::Value;

const HELPER_BOUND: Duration = Duration::from_secs(5);

struct CapturedOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn helper_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_devmanager-process-test-helper"))
}

fn configure_helper(command: &mut Command) {
    command
        .env("DEVMANAGER_PROFILE", "native-next-dev")
        .env_remove("DEVMANAGER_CONFIG_DIR")
        .env_remove("DEVMANAGER_APP_IDENTITY")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
}

fn read_pipe<R: Read + Send + 'static>(mut pipe: R) -> JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut output = Vec::new();
        pipe.read_to_end(&mut output)
            .expect("read process-test helper output");
        output
    })
}

fn finish_helper(mut child: Child, stdout: ChildStdout, stderr: ChildStderr) -> CapturedOutput {
    let stdout_thread = read_pipe(stdout);
    let stderr_thread = read_pipe(stderr);
    let deadline = Instant::now() + HELPER_BOUND;
    let status = loop {
        match child.try_wait().expect("query process-test helper") {
            Some(status) => break status,
            None if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("process-test helper exceeded {:?}", HELPER_BOUND);
            }
        }
    };

    CapturedOutput {
        status,
        stdout: stdout_thread.join().expect("join helper stdout reader"),
        stderr: stderr_thread.join().expect("join helper stderr reader"),
    }
}

fn run_helper(arguments: &[&str]) -> CapturedOutput {
    let mut command = Command::new(helper_path());
    command.args(arguments);
    configure_helper(&mut command);
    let mut child = command.spawn().expect("spawn process-test helper");
    let stdout = child.stdout.take().expect("helper stdout pipe");
    let stderr = child.stderr.take().expect("helper stderr pipe");
    finish_helper(child, stdout, stderr)
}

fn parse_json_lines(output: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(output)
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn assert_ready_and_done(captured: &CapturedOutput, expected_mode: &str) -> (Value, Value) {
    assert!(
        captured.status.success(),
        "{expected_mode} exited unsuccessfully: status={:?}\nstdout={}\nstderr={}",
        captured.status,
        String::from_utf8_lossy(&captured.stdout),
        String::from_utf8_lossy(&captured.stderr)
    );
    let events = parse_json_lines(&captured.stdout);
    let ready = events
        .iter()
        .find(|event| event["event"] == "ready")
        .cloned()
        .unwrap_or_else(|| panic!("{expected_mode} did not emit ready JSON: {events:?}"));
    let done = events
        .iter()
        .find(|event| event["event"] == "done")
        .cloned()
        .unwrap_or_else(|| panic!("{expected_mode} did not emit done JSON: {events:?}"));
    assert_eq!(ready["schemaVersion"], 1);
    assert_eq!(done["schemaVersion"], 1);
    assert_eq!(ready["mode"], expected_mode);
    assert_eq!(done["mode"], expected_mode);
    assert_eq!(ready["pid"], done["pid"]);
    assert_eq!(ready["identity"], done["identity"]);
    assert_eq!(done["exit"], "natural");
    (ready, done)
}

fn spawn_helper_with_ready(
    arguments: &[&str],
) -> (
    Child,
    Receiver<String>,
    JoinHandle<Vec<u8>>,
    JoinHandle<Vec<u8>>,
) {
    let mut command = Command::new(helper_path());
    command.args(arguments);
    configure_helper(&mut command);
    let mut child = command.spawn().expect("spawn process-test helper");
    let stdout = child.stdout.take().expect("helper stdout pipe");
    let stderr = child.stderr.take().expect("helper stderr pipe");
    let (ready_sender, ready_receiver) = mpsc::channel();
    let stdout_thread = thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut first_line = String::new();
        reader
            .read_line(&mut first_line)
            .expect("read helper ready line");
        ready_sender
            .send(first_line.clone())
            .expect("send helper ready line");
        let mut output = first_line.into_bytes();
        reader
            .read_to_end(&mut output)
            .expect("read helper output after ready");
        output
    });
    let stderr_thread = read_pipe(stderr);
    (child, ready_receiver, stdout_thread, stderr_thread)
}

fn wait_for_success(mut child: Child) -> ExitStatus {
    let deadline = Instant::now() + HELPER_BOUND;
    loop {
        match child.try_wait().expect("query live process-test helper") {
            Some(status) => return status,
            None if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("live process-test helper exceeded {:?}", HELPER_BOUND);
            }
        }
    }
}

#[test]
fn rapid_fork_exit_is_bounded_and_identity_tagged() {
    let captured = run_helper(&["rapid-fork-exit", "--duration-ms", "40", "--children", "3"]);
    let (ready, done) = assert_ready_and_done(&captured, "rapid-fork-exit");
    assert_eq!(ready["children"], 3);
    assert_eq!(done["children"], 3);
}

#[test]
fn large_output_is_bounded_and_identity_tagged() {
    let byte_limit = 4_097usize;
    let captured = run_helper(&[
        "large-output",
        "--duration-ms",
        "20",
        "--bytes",
        &byte_limit.to_string(),
    ]);
    let (ready, done) = assert_ready_and_done(&captured, "large-output");
    assert_eq!(ready["bytes"], byte_limit);
    assert_eq!(done["outputBytes"], byte_limit);

    let ready_end = captured
        .stdout
        .iter()
        .position(|byte| *byte == b'\n')
        .expect("large-output ready line terminator");
    let done_marker = b"{\"schemaVersion\":1,\"event\":\"done\"";
    let done_start = captured.stdout[ready_end + 1..]
        .windows(done_marker.len())
        .rposition(|window| window == done_marker)
        .map(|offset| ready_end + 1 + offset)
        .expect("large-output done line");
    let payload = &captured.stdout[ready_end + 1..done_start];
    assert_eq!(payload.len(), byte_limit);
    assert!(payload[..byte_limit - 1].iter().all(|byte| *byte == b'x'));
    assert_eq!(payload[byte_limit - 1], b'\n');
}

#[test]
fn ignored_cooperative_close_is_bounded_and_identity_tagged() {
    let captured = run_helper(&["ignored-cooperative-close", "--duration-ms", "40"]);
    let (ready, done) = assert_ready_and_done(&captured, "ignored-cooperative-close");
    assert_eq!(ready["cooperativeClose"], "ignored");
    assert_eq!(done["cooperativeClose"], "ignored");
}

#[test]
fn grandchild_lifetime_is_bounded_and_identity_tagged() {
    let captured = run_helper(&["grandchild-lifetime", "--duration-ms", "60"]);
    let (ready, done) = assert_ready_and_done(&captured, "grandchild-lifetime");
    assert!(ready["childPid"].as_u64().unwrap_or_default() > 0);
    assert_eq!(ready["childIdentity"], done["childIdentity"]);
}

#[test]
fn cpu_load_is_bounded_and_identity_tagged() {
    let captured = run_helper(&["bounded-cpu-load", "--duration-ms", "40"]);
    let (ready, done) = assert_ready_and_done(&captured, "bounded-cpu-load");
    assert_eq!(ready["durationMs"], 40);
    assert!(done["workUnits"].as_u64().unwrap_or_default() > 0);
}

#[test]
fn memory_load_is_bounded_and_identity_tagged() {
    let bytes = 1_048_576usize;
    let captured = run_helper(&[
        "bounded-memory-load",
        "--duration-ms",
        "40",
        "--bytes",
        &bytes.to_string(),
    ]);
    let (ready, done) = assert_ready_and_done(&captured, "bounded-memory-load");
    assert_eq!(ready["bytes"], bytes);
    assert_eq!(done["bytes"], bytes);
}

#[test]
fn loopback_listener_is_temporary_bounded_and_identity_tagged() {
    let (child, ready_receiver, stdout_thread, stderr_thread) =
        spawn_helper_with_ready(&["loopback-listener", "--duration-ms", "300"]);
    let ready_line = ready_receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("loopback listener ready evidence");
    let ready: Value = serde_json::from_str(ready_line.trim()).expect("loopback ready JSON");
    assert_eq!(ready["event"], "ready");
    assert_eq!(ready["mode"], "loopback-listener");
    let port = ready["port"].as_u64().expect("loopback listener port") as u16;
    assert!(port > 0);
    TcpStream::connect_timeout(
        &(format!("127.0.0.1:{port}"))
            .parse()
            .expect("loopback address"),
        Duration::from_secs(1),
    )
    .expect("loopback listener accepts a connection");

    let status = wait_for_success(child);
    let stdout = stdout_thread.join().expect("join loopback stdout reader");
    let stderr = stderr_thread.join().expect("join loopback stderr reader");
    assert!(
        status.success(),
        "loopback listener failed: stdout={} stderr={}",
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr)
    );
    let events = parse_json_lines(&stdout);
    let done = events
        .iter()
        .find(|event| event["event"] == "done")
        .expect("loopback listener done evidence");
    assert_eq!(done["identity"], ready["identity"]);
    assert_eq!(done["port"], port);
}

#[test]
fn process_soak_runner_contract_is_fail_closed_and_deterministic() {
    let source = std::fs::read_to_string("scripts/native-next/Invoke-ProcessSoak.ps1")
        .expect("read process soak runner");
    for required in [
        "Set-StrictMode -Version Latest",
        "$ErrorActionPreference = 'Stop'",
        "[int]$Iterations",
        "[int]$Seed",
        "Invoke-DevManagerProcessSoakCycle",
        "Capture-ProductionBaseline.ps1",
        "Assert-ProductionUnchanged.ps1",
        "Get-DevManagerProcessInventory",
        "orphan",
        "UNAVAILABLE",
        "exit 78",
    ] {
        assert!(
            source.contains(required),
            "runner contract missing {required:?}"
        );
    }
    for forbidden in ["Stop-Process", "taskkill", ".Kill()"] {
        assert!(
            !source.contains(forbidden),
            "runner must not kill {forbidden:?}"
        );
    }
}

#[cfg(windows)]
#[test]
fn process_soak_runner_reports_unavailable_without_a_real_cycle_api() {
    let output = Command::new("pwsh")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-File",
            "scripts/native-next/Invoke-ProcessSoak.ps1",
            "-Iterations",
            "1",
            "-Seed",
            "3403",
        ])
        .env("DEVMANAGER_PROFILE", "native-next-dev")
        .env_remove("DEVMANAGER_CONFIG_DIR")
        .env_remove("DEVMANAGER_APP_IDENTITY")
        .output()
        .expect("run process soak unavailable probe");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.status.code(), Some(78), "probe output: {combined}");
    assert!(combined.contains("UNAVAILABLE"), "probe output: {combined}");
    assert!(
        combined.contains("no real host/client cycle was run"),
        "probe output: {combined}"
    );
}

#[test]
fn phase_gate_supervisor_recipe_is_nonempty_and_fail_closed() {
    let source = std::fs::read_to_string("scripts/native-next/Invoke-PhaseGate.ps1")
        .expect("read phase gate runner");
    for required in [
        "phase-03-process-supervisor",
        "--list",
        "zero tests",
        "process_supervisor",
        "Set-DevManagerPhaseGateProcessEnvironment",
    ] {
        assert!(
            source.contains(required),
            "phase gate contract missing {required:?}"
        );
    }
    assert!(!source.contains("supervisor::"));
    for forbidden in ["Stop-Process", "taskkill", ".Kill()"] {
        assert!(
            !source.contains(forbidden),
            "phase gate must not kill {forbidden:?}"
        );
    }
}

#[test]
fn performance_budget_document_declares_provisional_phase_three_limits() {
    let path = Path::new("docs/performance-budgets.md");
    let source = std::fs::read_to_string(path).expect("read performance budgets");
    for required in [
        "Provisional engineering budgets",
        "Test-helper close latency",
        "PTY first-output/input acknowledgement",
        "10 MB output",
        "100-cycle memory/handle growth",
        "real evidence",
    ] {
        assert!(
            source.contains(required),
            "budget contract missing {required:?}"
        );
    }
}
