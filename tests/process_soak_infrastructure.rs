use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::{Digest, Sha256};

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

#[cfg(windows)]
fn sha256_file(path: &Path) -> String {
    let bytes = fs::read(path).expect("read manifest executable for hash");
    let digest = Sha256::digest(bytes);
    format!("{digest:x}")
}

#[cfg(windows)]
fn supervisor_manifest(
    directory: &tempfile::TempDir,
    scenario: &str,
    iterations: u32,
    cycle_deadline_ms: u64,
    stdout_limit: usize,
) -> PathBuf {
    let executable = helper_path();
    let manifest = serde_json::json!({
        "schemaVersion": 1,
        "revision": "phase3.10-supervisor-test-v1",
        "supervisorExecutable": executable,
        "supervisorSha256": sha256_file(&executable),
        "helperExecutable": executable,
        "helperSha256": sha256_file(&executable),
        "cycleExecutable": executable,
        "cycleSha256": sha256_file(&executable),
        "workingDirectory": directory.path(),
        "seed": 3403,
        "iterations": iterations,
        "budgets": {
            "suiteDeadlineMs": 10_000,
            "cycleDeadlineMs": cycle_deadline_ms,
            "cleanupDeadlineMs": 2_000,
            "stdoutBytes": stdout_limit,
            "stderrBytes": 16 * 1024,
            "resultBytes": 16 * 1024
        },
        "scenarioCatalog": [{
            "name": scenario,
            "arguments": ["cycle", scenario],
            "expectedExitCode": 0
        }]
    });
    let path = directory.path().join("manifest.json");
    fs::write(
        &path,
        serde_json::to_vec_pretty(&manifest).expect("serialize supervisor manifest"),
    )
    .expect("write supervisor manifest");
    path
}

#[cfg(windows)]
fn run_supervisor(_directory: &tempfile::TempDir, manifest: &Path) -> std::process::Output {
    let mut command = Command::new(helper_path());
    command.args(["supervise", "--manifest"]);
    command.arg(manifest);
    configure_helper(&mut command);
    command.output().expect("run Rust process supervisor")
}

#[cfg(windows)]
fn supervisor_result(output: &std::process::Output) -> Value {
    let lines = parse_json_lines(&output.stdout);
    assert_eq!(
        lines.len(),
        1,
        "supervisor must emit exactly one JSON result: {output:?}"
    );
    lines.into_iter().next().expect("supervisor JSON result")
}

#[cfg(windows)]
fn temporary_soak_environment() -> tempfile::TempDir {
    let directory = tempfile::tempdir_in(std::env::current_dir().expect("current worktree"))
        .expect("create worktree-local soak test directory");
    directory
}

#[cfg(windows)]
fn configure_soak_environment(command: &mut Command, _directory: &tempfile::TempDir) {
    command
        .env_remove("DEVMANAGER_PROFILE")
        .env_remove("DEVMANAGER_CONFIG_DIR")
        .env_remove("DEVMANAGER_APP_IDENTITY");
}

#[cfg(windows)]
fn configure_single_cargo_path(command: &mut Command) {
    let cargo_directory = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .find(|path| {
            path.join("cargo.exe").is_file()
                && path
                    .to_string_lossy()
                    .to_ascii_lowercase()
                    .contains(".rustup\\toolchains\\")
        })
        .expect("direct rustup cargo directory");
    command.env("PATH", cargo_directory);
}

#[cfg(windows)]
fn run_soak_with_manifest(directory: &tempfile::TempDir, manifest: &Path) -> std::process::Output {
    let mut command = Command::new("pwsh");
    command.args([
        "-NoProfile",
        "-NonInteractive",
        "-File",
        "scripts/native-next/Invoke-ProcessSoak.ps1",
        "-ManifestPath",
    ]);
    command.arg(manifest);
    configure_soak_environment(&mut command, directory);
    command.output().expect("run process soak fixture").into()
}

#[cfg(windows)]
fn run_soak_without_manifest(directory: &tempfile::TempDir) -> std::process::Output {
    let mut command = Command::new("pwsh");
    command.args([
        "-NoProfile",
        "-NonInteractive",
        "-File",
        "scripts/native-next/Invoke-ProcessSoak.ps1",
    ]);
    configure_soak_environment(&mut command, directory);
    command
        .output()
        .expect("run unavailable process soak probe")
}

#[cfg(windows)]
#[test]
fn rust_supervisor_runs_allowlisted_cycle_and_reports_real_identity() {
    let directory = temporary_soak_environment();
    let manifest = supervisor_manifest(&directory, "natural", 1, 2_000, 16 * 1024);
    let output = run_supervisor(&directory, &manifest);
    assert!(output.status.success(), "supervisor output: {output:?}");
    let result = supervisor_result(&output);
    assert_eq!(result["schemaVersion"], 1);
    assert_eq!(result["status"], "passed");
    assert_eq!(result["iterations"], 1);
    assert_eq!(result["cycles"][0]["scenario"], "natural");
    assert_eq!(result["cycles"][0]["result"]["seed"], 3403);
    assert_eq!(result["cycles"][0]["result"]["iteration"], 1);
    assert_eq!(result["cycles"][0]["activeProcessZero"], true);
    assert!(
        result["cycles"][0]["rootIdentity"]["processId"]
            .as_u64()
            .unwrap_or_default()
            > 0
    );
    assert!(
        result["cycles"][0]["rootIdentity"]["creationTime100ns"]
            .as_u64()
            .unwrap_or_default()
            > 0
    );
    assert!(result["cycles"][0]["rootIdentity"]["executablePath"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase()
        .ends_with("devmanager-process-test-helper.exe"));
    assert_eq!(
        result["cycles"][0]["stdoutBytes"]
            .as_u64()
            .expect("stdout byte count")
            <= 16 * 1024,
        true
    );
}

#[cfg(windows)]
#[test]
fn rust_supervisor_rejects_cycle_hash_mismatch_before_launch() {
    let directory = temporary_soak_environment();
    let manifest = supervisor_manifest(&directory, "natural", 1, 2_000, 16 * 1024);
    let mut value: Value = serde_json::from_slice(&fs::read(&manifest).expect("read manifest"))
        .expect("parse manifest");
    value["cycleSha256"] = Value::String("00".repeat(32));
    fs::write(
        &manifest,
        serde_json::to_vec(&value).expect("serialize mismatched manifest"),
    )
    .expect("write mismatched manifest");
    let output = run_supervisor(&directory, &manifest);
    assert!(!output.status.success(), "hash mismatch must fail closed");
    let result = supervisor_result(&output);
    assert_eq!(result["status"], "rejected");
    assert!(result["error"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase()
        .contains("sha"));
    assert_eq!(result["launched"], false);
}

#[cfg(windows)]
#[test]
fn rust_supervisor_rejects_helper_hash_mismatch_before_launch() {
    let directory = temporary_soak_environment();
    let manifest = supervisor_manifest(&directory, "natural", 1, 2_000, 16 * 1024);
    let mut value: Value = serde_json::from_slice(&fs::read(&manifest).expect("read manifest"))
        .expect("parse manifest");
    value["helperSha256"] = Value::String("ff".repeat(32));
    fs::write(
        &manifest,
        serde_json::to_vec(&value).expect("serialize mismatched manifest"),
    )
    .expect("write mismatched manifest");
    let output = run_supervisor(&directory, &manifest);
    assert!(
        !output.status.success(),
        "helper hash mismatch must fail closed"
    );
    let result = supervisor_result(&output);
    assert_eq!(result["status"], "rejected");
    assert!(result["error"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase()
        .contains("helper"));
    assert_eq!(result["launched"], false);
}

#[cfg(windows)]
#[test]
fn rust_supervisor_rejects_helper_identity_mismatch_before_launch() {
    let directory = temporary_soak_environment();
    let manifest = supervisor_manifest(&directory, "natural", 1, 2_000, 16 * 1024);
    let executable = helper_path();
    let copied_helper = directory.path().join("helper-copy.exe");
    fs::copy(&executable, &copied_helper).expect("copy helper identity fixture");
    let mut value: Value = serde_json::from_slice(&fs::read(&manifest).expect("read manifest"))
        .expect("parse manifest");
    value["helperExecutable"] = Value::String(copied_helper.to_string_lossy().into_owned());
    value["helperSha256"] = Value::String(sha256_file(&copied_helper));
    fs::write(
        &manifest,
        serde_json::to_vec(&value).expect("serialize identity-mismatch manifest"),
    )
    .expect("write identity-mismatch manifest");
    let output = run_supervisor(&directory, &manifest);
    assert!(!output.status.success(), "helper identity must fail closed");
    let result = supervisor_result(&output);
    assert_eq!(result["status"], "rejected");
    assert_eq!(result["launched"], false);
    assert_eq!(result.as_object().map(serde_json::Map::len), Some(4));
    assert!(result["error"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase()
        .contains("identity"));
}

#[cfg(windows)]
#[test]
fn rust_supervisor_rejects_malformed_multiple_and_oversized_cycle_output() {
    for (scenario, expected) in [
        ("malformed", "malformed"),
        ("multiple", "exactly one"),
        ("oversized", "exceeded"),
        ("stderr-oversized", "stderr exceeded"),
        ("wrong-scenario", "scenario mismatch"),
    ] {
        let directory = temporary_soak_environment();
        let limit = if scenario == "oversized" {
            128
        } else {
            16 * 1024
        };
        let manifest = supervisor_manifest(&directory, scenario, 1, 2_000, limit);
        let output = run_supervisor(&directory, &manifest);
        assert!(
            !output.status.success(),
            "{scenario} output must fail closed"
        );
        let result = supervisor_result(&output);
        assert_eq!(result["status"], "failed", "{result:?}");
        assert!(
            result["cycles"][0]["error"]
                .as_str()
                .unwrap_or_default()
                .to_ascii_lowercase()
                .contains(expected),
            "unexpected supervisor result: {result:?}"
        );
        assert_eq!(result["cycles"][0]["activeProcessZero"], true);
    }
}

#[cfg(windows)]
#[test]
fn rust_supervisor_rejects_nonzero_and_crash_and_accepts_restart_resume() {
    let nonzero_directory = temporary_soak_environment();
    let nonzero_manifest = supervisor_manifest(&nonzero_directory, "nonzero", 1, 2_000, 16 * 1024);
    let nonzero_output = run_supervisor(&nonzero_directory, &nonzero_manifest);
    assert!(
        !nonzero_output.status.success(),
        "nonzero cycle must not pass"
    );
    let nonzero_result = supervisor_result(&nonzero_output);
    assert_eq!(nonzero_result["status"], "failed");
    assert_eq!(nonzero_result["cycles"][0]["activeProcessZero"], true);
    assert!(nonzero_result["cycles"][0]["error"]
        .as_str()
        .unwrap_or_default()
        .contains("exited"));

    let crash_directory = temporary_soak_environment();
    let crash_manifest = supervisor_manifest(&crash_directory, "crash", 1, 2_000, 16 * 1024);
    let crash_output = run_supervisor(&crash_directory, &crash_manifest);
    assert!(
        !crash_output.status.success(),
        "crashed cycle must not pass"
    );
    let crash_result = supervisor_result(&crash_output);
    assert_eq!(crash_result["status"], "failed");
    assert_eq!(crash_result["cycles"][0]["activeProcessZero"], true);

    let resume_directory = temporary_soak_environment();
    let resume_manifest =
        supervisor_manifest(&resume_directory, "restart-resume", 1, 2_000, 16 * 1024);
    let resume_output = run_supervisor(&resume_directory, &resume_manifest);
    assert!(
        resume_output.status.success(),
        "restart/resume fixture: {resume_output:?}"
    );
    let resume_result = supervisor_result(&resume_output);
    assert_eq!(resume_result["status"], "passed");
    assert_eq!(
        resume_result["cycles"][0]["result"]["resume"],
        "new-generation"
    );
    assert_ne!(
        (
            crash_result["cycles"][0]["rootIdentity"]["processId"].as_u64(),
            crash_result["cycles"][0]["rootIdentity"]["creationTime100ns"].as_u64()
        ),
        (
            resume_result["cycles"][0]["rootIdentity"]["processId"].as_u64(),
            resume_result["cycles"][0]["rootIdentity"]["creationTime100ns"].as_u64()
        ),
        "resume must use a new live process identity"
    );
}

#[cfg(windows)]
#[test]
fn rust_supervisor_timeout_terminates_job_children_and_grandchildren() {
    let directory = temporary_soak_environment();
    let manifest = supervisor_manifest(&directory, "tree-hang", 1, 100, 16 * 1024);
    let output = run_supervisor(&directory, &manifest);
    assert!(!output.status.success(), "timeout must fail closed");
    let result = supervisor_result(&output);
    assert_eq!(result["status"], "failed");
    assert_eq!(result["cycles"][0]["outcome"], "timeout");
    assert_eq!(result["cycles"][0]["activeProcessZero"], true);
    assert!(
        result["cycles"][0]["memberIdentities"]
            .as_array()
            .is_some_and(|members| !members.is_empty()),
        "unexpected timeout result: {result:?}"
    );
}

#[cfg(windows)]
#[test]
fn rust_supervisor_interruption_path_fails_closed_without_false_pass() {
    let directory = temporary_soak_environment();
    let manifest = supervisor_manifest(&directory, "interrupt", 1, 100, 16 * 1024);
    let output = run_supervisor(&directory, &manifest);
    assert!(!output.status.success(), "interrupted cycle must not pass");
    let result = supervisor_result(&output);
    assert_eq!(result["status"], "failed");
    assert_eq!(result["cycles"][0]["outcome"], "timeout");
    assert_eq!(result["cycles"][0]["activeProcessZero"], true);
}

#[cfg(windows)]
#[test]
fn rust_supervisor_suite_deadline_stops_additional_cycles() {
    let directory = temporary_soak_environment();
    let manifest = supervisor_manifest(&directory, "natural", 2, 1, 16 * 1024);
    let mut value: Value = serde_json::from_slice(&fs::read(&manifest).expect("read manifest"))
        .expect("parse manifest");
    value["budgets"]["suiteDeadlineMs"] = Value::from(1);
    fs::write(
        &manifest,
        serde_json::to_vec(&value).expect("serialize suite deadline manifest"),
    )
    .expect("write suite deadline manifest");
    let output = run_supervisor(&directory, &manifest);
    assert!(!output.status.success(), "suite deadline must fail closed");
    let result = supervisor_result(&output);
    assert_eq!(result["status"], "failed");
    assert!(result["completedCycles"].as_u64().unwrap_or_default() <= 2);
}

#[cfg(windows)]
#[test]
fn rust_supervisor_does_not_touch_external_listener() {
    let directory = temporary_soak_environment();
    let (external, address) = TcpListener::bind("127.0.0.1:0")
        .map(|listener| {
            let address = listener.local_addr().expect("external listener address");
            (listener, address)
        })
        .expect("bind external listener");
    let manifest = supervisor_manifest(&directory, "natural", 1, 2_000, 16 * 1024);
    let output = run_supervisor(&directory, &manifest);
    assert!(output.status.success(), "supervisor output: {output:?}");
    assert!(TcpStream::connect_timeout(&address, Duration::from_secs(1)).is_ok());
    drop(external);
}

#[cfg(windows)]
#[test]
fn process_soak_script_dispatches_immutable_manifest_and_publishes_atomic_report() {
    let directory = temporary_soak_environment();
    let manifest = supervisor_manifest(&directory, "natural", 2, 2_000, 16 * 1024);
    let before_hash = sha256_file(&manifest);
    let output = run_soak_with_manifest(&directory, &manifest);
    assert!(output.status.success(), "soak output: {output:?}");
    let summary = parse_json_lines(&output.stdout)
        .into_iter()
        .last()
        .expect("soak summary JSON");
    assert_eq!(summary["status"], "passed", "soak summary: {summary:?}");
    assert_eq!(summary["supervisor"]["status"], "passed");
    assert_eq!(summary["supervisor"]["completedCycles"], 2);
    let run_directory = PathBuf::from(
        summary["runDirectory"]
            .as_str()
            .expect("run directory in summary"),
    );
    for artifact in [
        "manifest.json",
        "summary.json",
        "performance.json",
        "conformance.json",
        "run.json",
    ] {
        assert!(
            run_directory.join(artifact).is_file(),
            "missing artifact {artifact}"
        );
    }
    let manifest_artifact: Value = serde_json::from_slice(
        &fs::read(run_directory.join("manifest.json")).expect("read manifest artifact"),
    )
    .expect("parse manifest artifact");
    assert_eq!(
        manifest_artifact["revision"],
        "phase3.10-supervisor-test-v1"
    );
    assert_eq!(manifest_artifact["scenarioCatalog"][0]["name"], "natural");
    assert_eq!(
        manifest_artifact["binaries"]["cycleSha256"],
        sha256_file(&helper_path())
    );
    assert!(manifest_artifact.get("cycleExecutable").is_none());
    let performance: Value = serde_json::from_slice(
        &fs::read(run_directory.join("performance.json")).expect("read performance artifact"),
    )
    .expect("parse performance artifact");
    assert_eq!(performance["sampleCount"], 2);
    assert_eq!(performance["samplesMs"].as_array().map(Vec::len), Some(2));
    assert!(performance["durationMs"]["p95"].as_u64().is_some());
    let conformance: Value = serde_json::from_slice(
        &fs::read(run_directory.join("conformance.json")).expect("read conformance artifact"),
    )
    .expect("parse conformance artifact");
    assert_eq!(conformance["readerCaps"]["resultBytes"], 16 * 1024);
    assert_eq!(conformance["activeProcessZeroRequired"], true);
    assert_eq!(before_hash, sha256_file(&manifest));
}

#[test]
fn process_soak_contract_documents_task_manager_cpu_and_ansi_corpus() {
    let docs = fs::read_to_string("docs/performance-budgets.md").expect("performance budgets");
    assert!(docs.contains("logical processor"));
    assert!(docs.contains("core-equivalent"));
    assert!(docs.contains("p50"));
    assert!(docs.contains("p95"));
    assert!(docs.contains("ANSI"));
    assert!(docs.contains("ACTIVE_PROCESS_ZERO"));
}

#[test]
fn process_soak_cpu_math_matches_task_manager_denominators() {
    let sample_ms: f64 = 1_000.0;
    let process_ms: f64 = 750.0;
    let logical_processors: f64 = 4.0;
    let whole_machine = process_ms / (sample_ms * logical_processors) * 100.0;
    let core_equivalent = process_ms / sample_ms * 100.0;
    assert!((whole_machine - 18.75).abs() < f64::EPSILON);
    assert!((core_equivalent - 75.0).abs() < f64::EPSILON);
}

#[test]
fn ansi_corpus_reference_is_versioned_and_contains_split_sequences() {
    let corpus: Value = serde_json::from_str(
        &fs::read_to_string("tests/fixtures/ansi/phase3-v1.json").expect("ANSI corpus"),
    )
    .expect("parse ANSI corpus");
    assert_eq!(corpus["revision"], "phase3.10-ansi-v1");
    assert!(corpus["cases"]
        .as_array()
        .expect("ANSI cases")
        .iter()
        .any(|case| case["name"] == "split-sequence"));
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

#[cfg(windows)]
#[test]
fn process_soak_runner_reports_unavailable_without_a_real_cycle_api() {
    let directory = temporary_soak_environment();
    let output = run_soak_without_manifest(&directory);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.status.code(), Some(78), "probe output: {combined}");
    let summary = parse_json_lines(&output.stdout)
        .into_iter()
        .last()
        .expect("unavailable soak result");
    assert_eq!(summary["status"], "unavailable");
    assert_eq!(summary["launched"], false);
    assert!(combined.contains("manifest"), "probe output: {combined}");
}

#[cfg(windows)]
#[test]
fn phase3_process_supervisor_entrypoint_lists_real_tests() {
    let directory = temporary_soak_environment();
    let mut command = Command::new("pwsh");
    command.args([
        "-NoProfile",
        "-NonInteractive",
        "-File",
        "scripts/native-next/Invoke-Phase3ProcessSupervisorGate.ps1",
        "-ListOnly",
    ]);
    configure_soak_environment(&mut command, &directory);
    configure_single_cargo_path(&mut command);
    let output = command.output().expect("run process supervisor list gate");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.status.code(), Some(0), "gate output: {combined}");
    assert!(
        combined.contains("phase-03-process-supervisor"),
        "gate output: {combined}"
    );
    assert!(combined.contains("tests="), "gate output: {combined}");
}
