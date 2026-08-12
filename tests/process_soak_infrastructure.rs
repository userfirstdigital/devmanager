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
fn current_git_revision() -> String {
    let output = Command::new("git")
        .args(["rev-parse", "--verify", "HEAD"])
        .output()
        .expect("read current source revision");
    assert!(output.status.success(), "git revision probe: {output:?}");
    String::from_utf8(output.stdout)
        .expect("git revision UTF-8")
        .trim()
        .to_string()
}

#[cfg(windows)]
fn current_build_id() -> String {
    format!("sha256:{}", sha256_file(&helper_path()))
}

#[cfg(windows)]
fn current_source_tree_state() -> String {
    fn collect(root: &Path, current: &Path, files: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(current).expect("read source tree") {
            let entry = entry.expect("read source tree entry");
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
            let metadata = fs::symlink_metadata(&path).expect("inspect source tree entry");
            assert!(
                !metadata.file_type().is_symlink(),
                "source tree reparse point"
            );
            if metadata.is_dir() {
                collect(root, &path, files);
            } else if metadata.is_file() {
                files.push(
                    path.strip_prefix(root)
                        .expect("relative source path")
                        .into(),
                );
            }
        }
    }

    let root = std::env::current_dir().expect("current worktree");
    let mut files = Vec::new();
    collect(&root, &root, &mut files);
    files.sort_by_cached_key(|relative| relative.to_string_lossy().replace('\\', "/"));
    let mut hasher = Sha256::new();
    for relative in files {
        hasher.update(relative.to_string_lossy().replace('\\', "/").as_bytes());
        hasher.update([0u8]);
        hasher.update(fs::read(root.join(relative)).expect("read source tree file"));
    }
    format!("sha256:{:x}", hasher.finalize())
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
    let system_root = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .expect("Windows SystemRoot");
    let corpus = PathBuf::from("tests/fixtures/ansi/phase3-v1.json");
    let manifest = serde_json::json!({
        "schemaVersion": 1,
        "revision": "phase3.10-supervisor-v3",
        "gitRevision": current_git_revision(),
        "buildId": current_build_id(),
        "targetDirectory": "target-native-next",
        "sourceTreeState": current_source_tree_state(),
        "supervisorExecutable": executable,
        "supervisorSha256": sha256_file(&executable),
        "helperExecutable": executable,
        "helperSha256": sha256_file(&executable),
        "cycleExecutable": executable,
        "cycleSha256": sha256_file(&executable),
        "workingDirectory": directory.path(),
        "evidenceRoot": directory.path(),
        "environment": {
            "systemRoot": system_root,
            "tempDirectory": directory.path(),
            "pathDirectories": [system_root.join("System32"), executable.parent().expect("helper directory")],
            "allowlist": {}
        },
        "ansiCorpus": {
            "path": corpus,
            "sha256": sha256_file(&corpus),
            "revision": "phase3.10-ansi-v1"
        },
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
            "expectedExitCode": if scenario == "nonzero" { 1 } else { 0 }
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
fn run_soak(directory: &tempfile::TempDir) -> std::process::Output {
    let mut command = Command::new("pwsh");
    command.args([
        "-NoProfile",
        "-NonInteractive",
        "-File",
        "scripts/native-next/Invoke-ProcessSoak.ps1",
        "-Iterations",
        "2",
        "-Seed",
        "3403",
    ]);
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
        "-ManifestPath",
        ".tmp-missing-phase3-process-soak.manifest.json",
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
    assert!(result["sourceTreeState"].as_str().is_some());
    assert_ne!(
        result["sourceTreeState"],
        "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(result["cycles"][0]["scenario"], "natural");
    assert_eq!(result["cycles"][0]["result"]["seed"], 3403);
    assert_eq!(result["cycles"][0]["result"]["iteration"], 1);
    assert_eq!(result["cycles"][0]["activeProcessZero"], true);
    assert_eq!(result["cycles"][0]["environment"]["secretPresent"], false);
    assert!(result["cycles"][0]["cpu"]["processCpuTime100ns"]
        .as_u64()
        .is_some());
    assert!(result["cycles"][0]["cpu"]["wallTimeMs"].as_u64().is_some());
    assert!(
        result["cycles"][0]["cpu"]["logicalProcessorCount"]
            .as_u64()
            .unwrap_or_default()
            > 0
    );
    assert!(result["cycles"][0]["jobAudit"]["jobMembersAfter"]
        .as_u64()
        .is_some());
    assert!(result["cycles"][0]["jobAudit"]["processHandleCountBefore"]
        .as_u64()
        .is_some());
    assert!(
        result["cycles"][0]["jobAudit"]["hostProcessHandleCountBefore"]
            .as_u64()
            .is_some()
    );
    assert!(
        result["cycles"][0]["jobAudit"]["hostProcessHandleCountAfter"]
            .as_u64()
            .is_some()
    );
    assert!(result["cycles"][0]["jobAudit"]["ownedListenersDuring"].is_array());
    assert!(result["cycles"][0]["jobAudit"]["ownedListenersAfter"].is_array());
    assert_eq!(
        result["cycles"][0]["jobAudit"]["externalListenersUnchanged"],
        true
    );
    assert!(result["cycles"][0]["jobAudit"]["externalListenerBaselineDigest"].is_string());
    assert!(result["cycles"][0]["jobAudit"]["externalListenerAfterDigest"].is_string());
    assert_eq!(result["ansiCorpus"]["revision"], "phase3.10-ansi-v1");
    assert!(result["ansiCorpus"]["sha256"].as_str().is_some());
    assert!(result["ansiCorpus"]["caseHashes"].as_object().is_some());
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
    assert!(!result["cycles"][0]["rootIdentity"]["executablePath"]
        .as_str()
        .unwrap_or_default()
        .contains(':'));
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
fn rust_supervisor_rejects_manifest_revision_or_build_mismatch() {
    for field in ["gitRevision", "buildId", "sourceTreeState"] {
        let directory = temporary_soak_environment();
        let manifest = supervisor_manifest(&directory, "natural", 1, 2_000, 16 * 1024);
        let mut value: Value = serde_json::from_slice(&fs::read(&manifest).expect("read manifest"))
            .expect("parse manifest");
        value[field] = Value::String("wrong".to_string());
        fs::write(
            &manifest,
            serde_json::to_vec(&value).expect("serialize mismatch"),
        )
        .expect("write mismatch");
        let output = run_supervisor(&directory, &manifest);
        assert!(
            !output.status.success(),
            "{field} mismatch must fail closed"
        );
        let result = supervisor_result(&output);
        assert_eq!(result["status"], "rejected");
        assert!(result["error"].as_str().unwrap_or_default().contains(field));
    }
}

#[cfg(windows)]
#[test]
fn rust_supervisor_does_not_inherit_parent_secret_into_cycle() {
    let directory = temporary_soak_environment();
    let manifest = supervisor_manifest(&directory, "environment-probe", 1, 2_000, 16 * 1024);
    let mut command = Command::new(helper_path());
    command.args(["supervise", "--manifest"]);
    command.arg(&manifest);
    command.env("PHASE3_SOAK_SECRET", "must-not-cross-boundary");
    configure_helper(&mut command);
    let output = command.output().expect("run environment probe");
    assert!(output.status.success(), "environment probe: {output:?}");
    let result = supervisor_result(&output);
    assert_eq!(result["status"], "passed");
    assert_eq!(result["cycles"][0]["environment"]["secretPresent"], false);
}

#[cfg(windows)]
#[test]
fn rust_supervisor_rejects_secret_allowlist_entries_before_launch() {
    let directory = temporary_soak_environment();
    let manifest = supervisor_manifest(&directory, "natural", 1, 2_000, 16 * 1024);
    let mut value: Value = serde_json::from_slice(&fs::read(&manifest).expect("read manifest"))
        .expect("parse manifest");
    value["environment"]["allowlist"]["PHASE3_SOAK_SECRET"] =
        Value::String("must-not-be-authorized".to_string());
    fs::write(
        &manifest,
        serde_json::to_vec(&value).expect("serialize secret allowlist"),
    )
    .expect("write secret allowlist");
    let output = run_supervisor(&directory, &manifest);
    assert!(!output.status.success());
    let result = supervisor_result(&output);
    assert_eq!(result["status"], "rejected");
    assert!(result["error"]
        .as_str()
        .unwrap_or_default()
        .contains("allowlist"));
}

#[cfg(windows)]
#[test]
fn rust_supervisor_rejects_evidence_root_outside_worktree() {
    let directory = temporary_soak_environment();
    let outside = tempfile::tempdir().expect("create external evidence fixture");
    let manifest = supervisor_manifest(&directory, "natural", 1, 2_000, 16 * 1024);
    let mut value: Value = serde_json::from_slice(&fs::read(&manifest).expect("read manifest"))
        .expect("parse manifest");
    value["evidenceRoot"] = Value::String(
        outside
            .path()
            .to_str()
            .expect("external evidence path")
            .to_string(),
    );
    value["cycleSha256"] = Value::String("00".repeat(32));
    fs::write(
        &manifest,
        serde_json::to_vec(&value).expect("serialize external evidence manifest"),
    )
    .expect("write external evidence manifest");
    let output = run_supervisor(&directory, &manifest);
    assert!(!output.status.success());
    let result = supervisor_result(&output);
    assert_eq!(result["status"], "rejected");
    assert!(result.get("runDirectory").is_none());
    assert!(!outside.path().join("phase-03-process-soak").exists());
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
    let run_directory = result["runDirectory"]
        .as_str()
        .expect("failure run directory");
    assert!(!Path::new(run_directory).is_absolute());
    assert!(directory
        .path()
        .join(run_directory)
        .join("failure.json")
        .is_file());
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
    assert!(
        result
            .as_object()
            .map(serde_json::Map::len)
            .unwrap_or_default()
            >= 4
    );
    assert!(!Path::new(result["runDirectory"].as_str().unwrap_or_default()).is_absolute());
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
    // Leave enough bounded time for the controlled grandchild to be scheduled
    // and become an observable real Job member before the timeout sample.
    let manifest = supervisor_manifest(&directory, "tree-hang", 1, 500, 16 * 1024);
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
    assert_eq!(
        result["status"], "failed",
        "suite deadline result: {result:?}"
    );
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
    let occupied_manifest = supervisor_manifest(&directory, "occupied-port", 1, 2_000, 16 * 1024);
    let occupied_output = run_supervisor(&directory, &occupied_manifest);
    assert!(
        occupied_output.status.success(),
        "occupied-port supervisor output: {occupied_output:?}"
    );
    let occupied_result = supervisor_result(&occupied_output);
    assert_eq!(
        occupied_result["cycles"][0]["result"]["secondBindRejected"],
        true
    );
    drop(external);
}

#[cfg(windows)]
#[test]
fn process_soak_script_dispatches_immutable_manifest_and_publishes_atomic_report() {
    let directory = temporary_soak_environment();
    let manifest = PathBuf::from("scripts/native-next/phase3-process-soak.manifest.json");
    let before_hash = sha256_file(&manifest);
    let output = run_soak(&directory);
    assert!(output.status.success(), "soak output: {output:?}");
    let summary = parse_json_lines(&output.stdout)
        .into_iter()
        .last()
        .expect("soak summary JSON");
    assert_eq!(summary["status"], "passed", "soak summary: {summary:?}");
    assert_eq!(summary["supervisor"]["status"], "passed");
    assert_eq!(summary["supervisor"]["completedCycles"], 2);
    let run_directory = std::env::current_dir()
        .expect("current worktree")
        .join(".devmanager-next/evidence")
        .join(
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
    assert_eq!(manifest_artifact["revision"], "phase3.10-supervisor-v3");
    assert_eq!(manifest_artifact["scenarioCatalog"][0]["name"], "natural");
    assert_eq!(
        manifest_artifact["binaries"]["cycleSha256"],
        sha256_file(&helper_path())
    );
    assert!(manifest_artifact["binaries"]["cycleExecutable"].is_string());
    assert!(manifest_artifact["binaries"]["cycleExecutable"]
        .as_str()
        .unwrap_or_default()
        .contains("target-native-next"));
    assert!(!Path::new(summary["runDirectory"].as_str().unwrap_or_default()).is_absolute());
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
    assert_eq!(conformance["readerCaps"]["resultBytes"], 262144);
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
fn process_soak_script_uses_bounded_io_and_restored_default_interface() {
    let source = fs::read_to_string("scripts/native-next/Invoke-ProcessSoak.ps1")
        .expect("process soak script");
    let phase_gate =
        fs::read_to_string("scripts/native-next/PhaseGate.ps1").expect("phase gate script");
    assert!(source.contains("Iterations"));
    assert!(source.contains("Seed"));
    assert!(source.contains("phase3-process-soak.manifest.json"));
    assert!(source.contains("WaitForExit("));
    assert!(!source.contains("ReadToEndAsync"));
    assert!(!source.contains(".WaitForExit()"));
    assert!(!source.contains("Get-FileHash"));
    assert!(!source.contains("Publish-AtomicJson"));
    assert!(!source.contains("Get-Content -LiteralPath $manifest"));
    assert!(!source.contains("ManifestPath"));
    assert!(!source.contains("Kill($true)"));
    assert!(!source.contains("target\\debug"));
    assert!(
        phase_gate.find("$deadline =").expect("bounded deadline")
            < phase_gate
                .find("$started = $process.Start()")
                .expect("bounded process start")
    );
}

#[test]
fn phase_gate_supervisor_invocation_uses_bounded_explicit_environment() {
    let source =
        fs::read_to_string("scripts/native-next/Invoke-PhaseGate.ps1").expect("phase gate script");
    assert!(source.contains("$soakInfo = [System.Diagnostics.ProcessStartInfo]::new()"));
    assert!(source.contains("$soakInfo.Environment.Clear()"));
    assert!(source.contains("Invoke-DevManagerPhaseGateBoundedCommand"));
    assert!(source.contains("RedirectStandardOutput = $true"));
    assert!(source.contains("RedirectStandardError = $true"));
    assert!(source.contains("SystemRoot"));
    assert!(source.contains("TEMP"));
    assert!(source.contains("TMP"));
    assert!(source.contains("PATH"));
    assert!(!source.contains("& pwsh -NoProfile -NonInteractive -File $soakScript"));
}

#[test]
fn rust_supervisor_does_not_reset_cleanup_deadline_on_launch_failure() {
    let source = fs::read_to_string("src/bin/devmanager-process-test-helper.rs")
        .expect("read process supervisor source");
    assert!(!source.contains("terminate_and_wait(Instant::now()"));
    assert!(!source.contains("Duration::from_secs(5)"));
    assert!(source.contains("name == \"target\""));
}

#[test]
fn phase3_supervisor_gate_includes_soak_tests_and_fixed_supervisor_entrypoint() {
    let source = fs::read_to_string("scripts/native-next/Invoke-Phase3ProcessSupervisorGate.ps1")
        .expect("phase3 supervisor gate");
    assert!(source.contains("process_soak_infrastructure"));
    assert!(source.contains("Invoke-ProcessSoak.ps1"));
    assert!(source.contains("Capture-ProductionBaseline.ps1"));
    assert!(source.contains("Assert-ProductionUnchanged.ps1"));
    assert!(source.contains("ReadToEndAsync") == false);
    assert!(source.contains("WaitForExit("));
    assert!(!source.contains("Kill($true)"));
    assert!(source.contains("Iterations"));
    assert!(source.contains("Seed"));
    assert!(source.contains(
        "rust_supervisor_100_cycle_summary_is_bounded_and_does_not_retain_listener_tables"
    ));
}

#[cfg(windows)]
#[test]
fn phase_gate_quiet_window_honors_monotonic_elapsed_budget() {
    let script_root = std::env::current_dir()
        .expect("current worktree")
        .join("scripts/native-next");
    let isolation = script_root.join("Isolation.ps1");
    let phase_gate = script_root.join("PhaseGate.ps1");
    let command = format!(
        ". '{}' ; . '{}' ; function Update-DevManagerObservedProcessTree {{ param($ObservedByKey,$TrackedPids,$AttributionFloorUtc,$LineageEndExclusiveByPid,$CimProcesses) }} ; function Get-DevManagerPhaseGateResidueProcesses {{ param($WorktreeRoot,$ObservedByKey,$BeforeProcesses,$CimProcesses); return @() }} ; $observed = New-Object 'System.Collections.Generic.Dictionary[string, object]' ; $tracked = New-Object 'System.Collections.Generic.HashSet[uint32]' ; $lineage = New-Object 'System.Collections.Generic.Dictionary[uint32, DateTime]' ; $watch = [Diagnostics.Stopwatch]::StartNew() ; Wait-DevManagerPhaseGateQuietWindow -WorktreeRoot '{}' -ObservedByKey $observed -TrackedPids $tracked -AttributionFloorUtc ([DateTime]::UtcNow) -LineageEndExclusiveByPid $lineage -BeforeProcesses @() -CimProcesses @([pscustomobject]@{{ name = 'synthetic' }}) -TimeoutMilliseconds 3000 -PollMilliseconds 250 -QuietMilliseconds 1000 | Out-Null ; $watch.Stop() ; Write-Output $watch.ElapsedMilliseconds",
        isolation.display(),
        phase_gate.display(),
        std::env::current_dir().expect("current worktree").display(),
    );
    let output = Command::new("pwsh")
        .args(["-NoProfile", "-NonInteractive", "-Command", &command])
        .output()
        .expect("run quiet-window adversarial probe");
    assert!(output.status.success(), "quiet-window probe: {output:?}");
    let elapsed: u128 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .expect("quiet-window elapsed milliseconds");
    assert!(elapsed >= 1000, "quiet window returned after {elapsed}ms");
}

#[test]
#[cfg(windows)]
fn process_soak_cpu_math_matches_task_manager_denominators() {
    let directory = temporary_soak_environment();
    let manifest = supervisor_manifest(&directory, "natural", 1, 2_000, 16 * 1024);
    let output = run_supervisor(&directory, &manifest);
    assert!(output.status.success(), "measured CPU probe: {output:?}");
    let result = supervisor_result(&output);
    let cpu = &result["cycles"][0]["cpu"];
    let process_time = cpu["processCpuTime100ns"].as_f64().expect("process CPU");
    let wall_ms = cpu["wallTimeMs"].as_f64().expect("wall time");
    let logical = cpu["logicalProcessorCount"].as_f64().expect("logical CPUs");
    assert!(process_time > 0.0);
    assert!(wall_ms > 0.0);
    assert!(logical > 0.0);
    let whole_machine = process_time / (wall_ms * 10_000.0 * logical) * 100.0;
    let core_equivalent = process_time / (wall_ms * 10_000.0) * 100.0;
    assert!((whole_machine - cpu["wholeMachinePercent"].as_f64().unwrap()).abs() < 0.01);
    assert!((core_equivalent - cpu["coreEquivalentPercent"].as_f64().unwrap()).abs() < 0.01);
}

#[test]
fn ansi_corpus_reference_is_versioned_and_contains_split_sequences() {
    let corpus: Value = serde_json::from_str(
        &fs::read_to_string("tests/fixtures/ansi/phase3-v1.json").expect("ANSI corpus"),
    )
    .expect("parse ANSI corpus");
    assert_eq!(corpus["revision"], "phase3.10-ansi-v1");
    assert_eq!(corpus["cases"][0]["bytes"][0], 27);
    assert_eq!(corpus["cases"][0]["bytes"][1], 91);
    assert!(corpus["cases"][3]["chunks"][0].is_array());
    assert_eq!(corpus["cases"][3]["chunks"][0][0], 27);
    assert!(corpus["cases"]
        .as_array()
        .expect("ANSI cases")
        .iter()
        .any(|case| case["name"] == "split-sequence"));
    assert!(corpus["cases"]
        .as_array()
        .expect("ANSI cases")
        .iter()
        .any(|case| case["name"] == "unicode"));
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
fn process_soak_runner_rejects_caller_manifest_override() {
    let directory = temporary_soak_environment();
    let output = run_soak_without_manifest(&directory);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !output.status.success(),
        "override must fail closed: {combined}"
    );
    assert!(
        combined.contains("ManifestPath"),
        "probe output: {combined}"
    );
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

#[cfg(windows)]
#[test]
fn rust_supervisor_rejects_self_attestation_sentinels() {
    let directory = temporary_soak_environment();
    let manifest = supervisor_manifest(&directory, "natural", 1, 2_000, 16 * 1024);
    let mut value: Value = serde_json::from_slice(&fs::read(&manifest).expect("read manifest"))
        .expect("parse manifest");
    for field in [
        "gitRevision",
        "buildId",
        "sourceTreeState",
        "supervisorSha256",
        "helperSha256",
        "cycleSha256",
    ] {
        value[field] = Value::String("CURRENT".to_string());
    }
    fs::write(
        &manifest,
        serde_json::to_vec(&value).expect("serialize self-attestation manifest"),
    )
    .expect("write self-attestation manifest");
    let output = run_supervisor(&directory, &manifest);
    assert!(!output.status.success(), "CURRENT must never self-attest");
    let result = supervisor_result(&output);
    assert_eq!(result["status"], "rejected");
    assert!(result["error"]
        .as_str()
        .unwrap_or_default()
        .contains("external"));
}

#[cfg(windows)]
#[test]
fn rust_supervisor_100_cycle_summary_is_bounded_and_does_not_retain_listener_tables() {
    let directory = temporary_soak_environment();
    let manifest = supervisor_manifest(&directory, "natural", 100, 2_000, 16 * 1024);
    let mut value: Value = serde_json::from_slice(&fs::read(&manifest).expect("read manifest"))
        .expect("parse manifest");
    value["budgets"]["suiteDeadlineMs"] = Value::from(60_000);
    value["budgets"]["resultBytes"] = Value::from(256 * 1024);
    fs::write(
        &manifest,
        serde_json::to_vec(&value).expect("serialize 100-cycle manifest"),
    )
    .expect("write 100-cycle manifest");
    let output = run_supervisor(&directory, &manifest);
    assert!(
        output.status.success(),
        "100-cycle supervisor output: {output:?}"
    );
    let result = supervisor_result(&output);
    assert_eq!(result["status"], "passed");
    assert_eq!(result["completedCycles"], 100);
    let encoded = serde_json::to_vec(&result).expect("encode bounded summary");
    assert!(
        encoded.len() <= 256 * 1024,
        "summary is not bounded: {}",
        encoded.len()
    );
    assert!(result["cycles"]
        .as_array()
        .map(Vec::is_empty)
        .unwrap_or(false));
    assert_eq!(result["cycleAggregate"]["count"], 100);
    assert_eq!(
        result["cycleAggregate"]["conformance"]
            .as_array()
            .map(Vec::len),
        Some(100)
    );
    assert!(result["cycleAggregate"]["digest"].is_string());
    assert_eq!(result["cycleAggregate"]["externalListenersUnchanged"], true);
    assert!(result["cycleAggregate"]["externalListenerBaselineDigest"].is_string());
    assert!(result["cycleAggregate"]["externalListenerLastAfterDigest"].is_string());
}

#[cfg(windows)]
#[test]
fn rust_bounded_supervisor_timeout_reports_owned_job_zero() {
    let directory = temporary_soak_environment();
    let manifest = supervisor_manifest(&directory, "tree-hang", 1, 5_000, 16 * 1024);
    let mut command = Command::new(helper_path());
    command.args(["bounded-supervise", "--manifest"]);
    command.arg(&manifest);
    command.args(["--timeout-ms", "50"]);
    configure_helper(&mut command);
    let output = command.output().expect("run bounded supervisor wrapper");
    assert!(!output.status.success(), "wrapper timeout must fail closed");
    let result = supervisor_result(&output);
    assert_eq!(result["status"], "rejected");
    assert_eq!(result["wrapperTimedOut"], true);
    assert_eq!(result["jobZero"], true);
}

#[test]
fn phase3_gate_executes_full_soak_suite_and_declares_union_hold() {
    let source = fs::read_to_string("scripts/native-next/Invoke-Phase3ProcessSupervisorGate.ps1")
        .expect("phase3 supervisor gate");
    assert!(source.contains("--test-threads=1"));
    assert!(source.contains("--nocapture"));
    assert!(source.contains("dependency"));
    assert!(source.contains("HOLD"));
    assert!(source.contains("100"));
}

#[test]
fn process_soak_wrapper_uses_external_attestation_and_owned_timeout_cleanup() {
    let source = fs::read_to_string("scripts/native-next/Invoke-ProcessSoak.ps1")
        .expect("process soak script");
    assert!(source.contains("bounded-supervise"));
    assert!(source.contains("expected-git-revision"));
    assert!(source.contains("expected-helper-sha256"));
    assert!(source.contains("wrapperTimedOut"));
    assert!(source.contains("jobZero"));
    assert!(source.contains("HOLD"));
}

#[test]
fn process_soak_release_contract_is_real_and_strictly_attested() {
    let helper = fs::read_to_string("src/bin/devmanager-process-test-helper.rs")
        .expect("process supervisor source");
    let soak = fs::read_to_string("scripts/native-next/Invoke-ProcessSoak.ps1")
        .expect("process soak script");
    let gate = fs::read_to_string("scripts/native-next/Invoke-Phase3ProcessSupervisorGate.ps1")
        .expect("phase3 supervisor gate");
    assert!(helper.contains("releaseEligible"));
    assert!(helper.contains("realLifecycle"));
    assert!(
        helper.contains("completedCycles == 100") || helper.contains("completed_cycles == 100")
    );
    assert!(helper.contains("hostExecutable"));
    assert!(helper.contains("clientExecutable"));
    assert!(helper.contains("hostSha256"));
    assert!(helper.contains("clientSha256"));
    assert!(soak.contains("HostExecutable"));
    assert!(soak.contains("HostSha256"));
    assert!(soak.contains("ClientExecutable"));
    assert!(soak.contains("ClientSha256"));
    assert!(gate.contains("hostExecutable") || gate.contains("HostExecutable"));
    assert!(gate.contains("clientExecutable") || gate.contains("ClientExecutable"));
}

#[test]
fn process_soak_powershell_children_have_owned_bounded_cleanup() {
    let phase_gate =
        fs::read_to_string("scripts/native-next/PhaseGate.ps1").expect("phase gate source");
    let soak = fs::read_to_string("scripts/native-next/Invoke-ProcessSoak.ps1")
        .expect("process soak script");
    let helper = fs::read_to_string("src/bin/devmanager-process-test-helper.rs")
        .expect("process supervisor source");
    assert!(phase_gate.contains("AssignProcessToJobObject"));
    assert!(phase_gate.contains("TerminateJobObject"));
    assert!(phase_gate.contains("WaitForExit("));
    assert!(phase_gate.contains("ReadAsync($stdout.buffer"));
    assert!(phase_gate.contains("ReadAsync($stderr.buffer"));
    assert!(soak.contains("Invoke-DevManagerPhaseGateBoundedCommand"));
    assert!(!soak.contains("ReadAsync()"));
    assert!(!soak.contains("ReadToEndAsync"));
    assert!(helper.contains("AF_INET6"));
    assert!(helper.contains("MibTcp6RowOwnerPid"));
}

#[cfg(windows)]
#[test]
fn phase_gate_assigns_a_suspended_child_before_resuming_it() {
    let script = std::env::current_dir()
        .expect("current worktree")
        .join("scripts/native-next/PhaseGate.ps1");
    let command = format!(
        r#"
. '{script}'
Ensure-DevManagerPhaseGateJobType
$pwsh = (Get-Command pwsh -CommandType Application -ErrorAction Stop |
    Where-Object {{ -not [string]::IsNullOrWhiteSpace([string]$_.Source) }} |
    Select-Object -First 1).Source
$info = [Diagnostics.ProcessStartInfo]::new()
$info.FileName = $pwsh
$info.UseShellExecute = $false
$info.CreateNoWindow = $true
$info.RedirectStandardOutput = $true
$info.RedirectStandardError = $true
$info.WorkingDirectory = (Get-Location).Path
$info.Environment.Clear()
$info.Environment['SystemRoot'] = [Environment]::GetEnvironmentVariable('SystemRoot', 'Process')
$info.Environment['PATH'] = Join-Path $info.Environment['SystemRoot'] 'System32'
[void]$info.ArgumentList.Add('-NoProfile')
[void]$info.ArgumentList.Add('-NonInteractive')
[void]$info.ArgumentList.Add('-Command')
[void]$info.ArgumentList.Add('Write-Output suspended-assignment-probe')
$job = [DevManagerPhaseGateJob]::new()
$launch = [DevManagerPhaseGateJob]::StartSuspended($info)
try {{
    if ($launch.Process.HasExited) {{ throw 'probe exited before Job assignment' }}
    $job.Assign($launch.Process)
    $deadline = [Diagnostics.Stopwatch]::GetTimestamp() + [int64](2000 * [Diagnostics.Stopwatch]::Frequency / 1000)
    if ([uint32]$job.ActiveProcessCount($deadline) -ne 1) {{ throw 'suspended root was not a Job member' }}
    $launch.Resume()
    if (-not $launch.Process.WaitForExit(2000)) {{ throw 'resumed probe did not exit' }}
}} finally {{
    if ($null -ne $launch) {{ $launch.Stdout.Dispose(); $launch.Stderr.Dispose() }}
    if ($null -ne $launch) {{ $launch.Dispose() }}
    if ($null -ne $job) {{ $job.Dispose() }}
}}
Write-Output PASS
"#,
        script = script.display().to_string().replace('\'', "''"),
    );
    let output = Command::new("pwsh")
        .args(["-NoProfile", "-NonInteractive", "-Command", &command])
        .output()
        .expect("run suspended assignment probe");
    assert!(
        output.status.success(),
        "suspended assignment probe failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("PASS"));
}

#[cfg(windows)]
#[test]
fn rust_membership_probe_rejects_an_expired_caller_deadline() {
    let captured = run_helper(&["membership-deadline-probe"]);
    assert!(
        captured.status.success(),
        "expired membership probe must fail closed with a typed result: stdout={} stderr={}",
        String::from_utf8_lossy(&captured.stdout),
        String::from_utf8_lossy(&captured.stderr)
    );
    let events = parse_json_lines(&captured.stdout);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["status"], "passed");
    assert_eq!(events[0]["deadlineChecked"], true);
}

#[cfg(windows)]
#[test]
fn rust_reader_cancel_probe_closes_and_joins_before_its_deadline() {
    let started = Instant::now();
    let captured = run_helper(&["reader-cancel-probe"]);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "reader cancellation exceeded its bounded harness: {:?}",
        started.elapsed()
    );
    assert!(
        captured.status.success(),
        "reader cancellation probe failed: stdout={} stderr={}",
        String::from_utf8_lossy(&captured.stdout),
        String::from_utf8_lossy(&captured.stderr)
    );
    let events = parse_json_lines(&captured.stdout);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["status"], "passed");
    assert_eq!(events[0]["readerCancelled"], true);
    assert_eq!(events[0]["readerJoined"], true);
}

#[cfg(windows)]
#[test]
fn final_union_validation_rejects_wrong_schema_or_iteration_count() {
    let phase_gate = std::env::current_dir()
        .expect("current worktree")
        .join("scripts/native-next/PhaseGate.ps1");
    let command = format!(
        r#"
. '{phase_gate}'
function New-UnionResult([object]$schema, [object]$iterations) {{
    [pscustomobject]@{{
        schemaVersion = $schema
        status = 'passed'
        seed = 3403
        jobZero = $true
        releaseEligible = $true
        realLifecycle = $true
        externalListenersUnchanged = $true
        zeroOrphanProcesses = $true
        zeroHelperProcesses = $true
        zeroProviderProcesses = $true
        zeroJobMembers = $true
        zeroOwnedListeners = $true
        zeroNamedPipesExceptDeclaredHost = $true
        pipeReadersSettled = $true
        readerThreadsJoined = $true
        handleGrowthBounded = $true
        memoryGrowthBounded = $true
        orphanProcessCount = 0
        helperProcessCount = 0
        providerProcessCount = 0
        jobMemberCount = 0
        ownedListenerCount = 0
        unexpectedNamedPipeCount = 0
        declaredHostPipeCount = 1
        handleGrowth = 0
        memoryGrowthBytes = 0
        completedCycles = 100
        iterations = $iterations
    }}
}}
foreach ($invalid in @((New-UnionResult 2 100), (New-UnionResult 1 99), (New-UnionResult '1' 100))) {{
    try {{
        Assert-DevManagerPhase3FinalUnionDocument -Document $invalid
        throw 'invalid final union document was accepted'
    }} catch [System.Management.Automation.RuntimeException] {{
        if ($_.Exception.Message -eq 'invalid final union document was accepted') {{ throw }}
    }}
}}
$required = @(
    'schemaVersion', 'status', 'seed', 'iterations', 'completedCycles',
    'jobZero', 'releaseEligible', 'realLifecycle', 'externalListenersUnchanged',
    'zeroOrphanProcesses', 'zeroHelperProcesses', 'zeroProviderProcesses',
    'zeroJobMembers', 'zeroOwnedListeners', 'zeroNamedPipesExceptDeclaredHost',
    'pipeReadersSettled', 'readerThreadsJoined', 'handleGrowthBounded',
    'memoryGrowthBounded', 'orphanProcessCount', 'helperProcessCount',
    'providerProcessCount', 'jobMemberCount', 'ownedListenerCount',
    'unexpectedNamedPipeCount', 'declaredHostPipeCount', 'handleGrowth',
    'memoryGrowthBytes'
)
foreach ($missing in $required) {{
    $invalid = New-UnionResult 1 100
    [void]$invalid.PSObject.Properties.Remove($missing)
    try {{
        Assert-DevManagerPhase3FinalUnionDocument -Document $invalid
        throw "partial final union document was accepted: $missing"
    }} catch [System.Management.Automation.RuntimeException] {{
        if ($_.Exception.Message -eq "partial final union document was accepted: $missing") {{ throw }}
    }}
}}
Assert-DevManagerPhase3FinalUnionDocument -Document (New-UnionResult 1 100)
Write-Output PASS
"#,
        phase_gate = phase_gate.display().to_string().replace('\'', "''"),
    );
    let output = Command::new("pwsh")
        .args(["-NoProfile", "-NonInteractive", "-Command", &command])
        .output()
        .expect("run final union validation probe");
    assert!(
        output.status.success(),
        "final union validation probe failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("PASS"));
}

#[cfg(windows)]
#[test]
fn rust_final_schema_validator_rejects_a_crafted_partial_document() {
    for kind in ["valid", "partial", "wrong-seed", "orphan", "memory-growth"] {
        let captured = run_helper(&["final-schema-probe", kind]);
        assert!(
            captured.status.success(),
            "Rust final schema probe ({kind}) failed: stdout={} stderr={}",
            String::from_utf8_lossy(&captured.stdout),
            String::from_utf8_lossy(&captured.stderr)
        );
        let events = parse_json_lines(&captured.stdout);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["status"], "passed");
        if kind == "valid" {
            assert_eq!(events[0]["validatorAccepted"], true);
        } else {
            assert_eq!(events[0]["validatorRejected"], true);
            assert!(events[0]["error"].as_str().is_some());
        }
    }
}

#[cfg(windows)]
#[test]
fn rust_reader_reaper_probe_keeps_a_timed_out_join_owned() {
    let started = Instant::now();
    let captured = run_helper(&["reader-reaper-probe"]);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "reader reaper probe exceeded its bounded harness: {:?}",
        started.elapsed()
    );
    assert!(
        captured.status.success(),
        "reader reaper probe failed: stdout={} stderr={}",
        String::from_utf8_lossy(&captured.stdout),
        String::from_utf8_lossy(&captured.stderr)
    );
    let events = parse_json_lines(&captured.stdout);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["status"], "passed");
    assert_eq!(events[0]["joinHandleRetainedOnTimeout"], true);
    assert_eq!(events[0]["reaperJoined"], true);
}

#[cfg(windows)]
#[test]
fn outer_phase_gate_publishes_missing_harness_as_a_redacted_typed_hold() {
    let worktree = std::env::current_dir().expect("current worktree");
    let harness_root = tempfile::tempdir_in(&worktree).expect("create empty harness root");
    let script = worktree.join("scripts/native-next/Invoke-Phase3ProcessSupervisorGate.ps1");
    let output = Command::new("pwsh")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-File",
            script.to_str().expect("outer gate script path"),
            "-ListOnly",
        ])
        .env("DEVMANAGER_PHASE3_SOAK_HARNESS_ROOT", harness_root.path())
        .output()
        .expect("run missing harness outer hold");
    assert_eq!(output.status.code(), Some(78));
    let lines = parse_json_lines(&output.stdout);
    assert_eq!(lines.len(), 1, "outer gate must publish one typed HOLD");
    assert_eq!(lines[0]["schemaVersion"], 1);
    assert_eq!(lines[0]["status"], "hold");
    assert_eq!(lines[0]["launched"], false);
    let error = lines[0]["error"].as_str().unwrap_or_default();
    assert!(error.contains("HOLD"));
    assert!(!error.contains("C:\\"));
    assert!(!error.contains("target-native-next"));
}

#[cfg(windows)]
#[test]
fn missing_final_union_binaries_publish_a_typed_hold_without_launching_100_cycles() {
    let script = std::env::current_dir()
        .expect("current worktree")
        .join("scripts/native-next/Invoke-ProcessSoak.ps1");
    let output = Command::new("pwsh")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-File",
            script.to_str().expect("soak script path"),
            "-Iterations",
            "100",
        ])
        .output()
        .expect("run missing dependency soak hold");
    assert_eq!(output.status.code(), Some(78));
    let lines = parse_json_lines(&output.stdout);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["schemaVersion"], 1);
    assert_eq!(lines[0]["status"], "hold");
    assert_eq!(lines[0]["launched"], false);
    let error = lines[0]["error"].as_str().unwrap_or_default();
    assert!(error.contains("HOLD"));
    assert!(!error.contains("C:\\"));
    assert!(!error.contains("target-live-native-next"));
}

#[cfg(windows)]
#[test]
fn outer_phase_gate_publishes_missing_binary_hold_before_any_production_guard() {
    let worktree = std::env::current_dir().expect("current worktree");
    let live_root = worktree.join("target-live-native-next");
    if live_root.join("devmanager-host.exe").is_file()
        && live_root.join("devmanager-next.exe").is_file()
    {
        eprintln!("skipping missing-binary outer-gate probe: live inputs are present");
        return;
    }
    let script = worktree.join("scripts/native-next/Invoke-Phase3ProcessSupervisorGate.ps1");
    let output = Command::new("pwsh")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-File",
            script.to_str().expect("outer gate script path"),
            "-Iterations",
            "100",
        ])
        .output()
        .expect("run outer missing dependency hold");
    assert_eq!(output.status.code(), Some(78));
    let lines = parse_json_lines(&output.stdout);
    assert_eq!(lines.len(), 1, "outer gate must publish one typed HOLD");
    assert_eq!(lines[0]["schemaVersion"], 1);
    assert_eq!(lines[0]["status"], "hold");
    assert_eq!(lines[0]["launched"], false);
    let error = lines[0]["error"].as_str().unwrap_or_default();
    assert!(error.contains("HOLD"));
    assert!(!error.contains("C:\\"));
    assert!(!error.contains("target-live-native-next"));
}
