use std::fs;
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

#[cfg(windows)]
fn temporary_soak_environment() -> tempfile::TempDir {
    let directory = tempfile::tempdir_in(std::env::current_dir().expect("current worktree"))
        .expect("create worktree-local soak test directory");
    let production_root = directory
        .path()
        .join("appdata")
        .join("com.userfirst.devmanager");
    let install_root = directory.path().join("install");
    fs::create_dir_all(&production_root).expect("create temporary production root");
    fs::create_dir_all(&install_root).expect("create temporary install root");
    fs::write(production_root.join("config.json"), b"test-config").expect("write test config");
    fs::write(production_root.join("remote.json"), b"test-remote").expect("write test remote");
    directory
}

#[cfg(windows)]
fn write_cycle_api(directory: &tempfile::TempDir, contents: &str) -> PathBuf {
    let path = directory.path().join("cycle-api.ps1");
    fs::write(&path, contents).expect("write cycle API fixture");
    path
}

#[cfg(windows)]
fn configure_soak_environment(command: &mut Command, directory: &tempfile::TempDir) {
    let app_data = directory.path().join("appdata");
    let install_root = directory.path().join("install");
    command
        .env("APPDATA", app_data)
        .env("LOCALAPPDATA", &install_root)
        .env("ProgramFiles", &install_root)
        .env_remove("ProgramFiles(x86)")
        .env("DEVMANAGER_PROFILE", "native-next-dev")
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
fn run_soak_with_api(
    directory: &tempfile::TempDir,
    api_path: &Path,
    iterations: u32,
    seed: u32,
) -> std::process::Output {
    let mut command = Command::new("pwsh");
    command.args([
        "-NoProfile",
        "-NonInteractive",
        "-File",
        "scripts/native-next/Invoke-ProcessSoak.ps1",
        "-Iterations",
        &iterations.to_string(),
        "-Seed",
        &seed.to_string(),
        "-CycleApiScript",
    ]);
    command.arg(api_path);
    configure_soak_environment(&mut command, directory);
    command.output().expect("run process soak fixture").into()
}

#[cfg(windows)]
fn run_soak_without_api(
    directory: &tempfile::TempDir,
    iterations: u32,
    seed: u32,
) -> std::process::Output {
    let mut command = Command::new("pwsh");
    command.args([
        "-NoProfile",
        "-NonInteractive",
        "-File",
        "scripts/native-next/Invoke-ProcessSoak.ps1",
        "-Iterations",
        &iterations.to_string(),
        "-Seed",
        &seed.to_string(),
    ]);
    configure_soak_environment(&mut command, directory);
    command
        .output()
        .expect("run unavailable process soak probe")
}

#[cfg(windows)]
fn summary_from_soak_output(output: &std::process::Output) -> Value {
    parse_json_lines(&output.stdout)
        .into_iter()
        .rev()
        .find(|value| value["phase"] == "phase-03-process-soak")
        .unwrap_or_else(|| {
            panic!(
                "process soak summary missing: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        })
}

#[cfg(windows)]
fn valid_cycle_api_script(extra: &str) -> String {
    let script = r#"
function New-TestSoakIdentity {
    param([int]$ProcessId, [string]$Name)
    return [pscustomobject][ordered]@{
        processId = $ProcessId
        executablePath = ('C:\soak\' + $Name + '.exe')
        creationDate = '2026-08-09T00:00:00.0000000Z'
    }
}

function Invoke-DevManagerProcessSoakCycle {
    param(
        [int]$Iteration,
        [int]$Seed,
        [object]$Scenario,
        [string]$WorktreeRoot
    )

    $reportedCycle = $Iteration
    $reportedSeed = $Seed
    $jobMemberCount = 0
    $jobMemberCountAuthoritative = $true
    $ownedListeners = @()
    $ownedNamedPipes = @()
    $leakedPtyHandles = @()
    $leakedJobHandles = @()
    __EXTRA__

    return [pscustomobject][ordered]@{
        schemaVersion = 1
        status = 'completed'
        cycle = $reportedCycle
        seed = $reportedSeed
        host = [pscustomobject][ordered]@{
            identity = (New-TestSoakIdentity -ProcessId 10001 -Name 'host')
            generation = 'generation-1'
        }
        client = [pscustomobject][ordered]@{
            identity = (New-TestSoakIdentity -ProcessId 10002 -Name 'client')
            generation = 'generation-1'
        }
        terminal = [pscustomobject][ordered]@{
            terminalId = 'terminal-1'
            resourceId = 'pty-1'
            generation = 'generation-1'
        }
        operations = [pscustomobject][ordered]@{
            launch = [pscustomobject][ordered]@{ operationId = 'operation-launch-1' }
            firstOutput = [pscustomobject][ordered]@{ operationId = 'operation-first-output-1' }
            inputAck = [pscustomobject][ordered]@{ operationId = 'operation-input-ack-1' }
            closeSettlement = [pscustomobject][ordered]@{ operationId = 'operation-close-settlement-1' }
        }
        managedRoot = [pscustomobject][ordered]@{
            identity = (New-TestSoakIdentity -ProcessId 10003 -Name 'managed-root')
            job = [pscustomobject][ordered]@{
                handleId = 'job-1'
                memberCount = $jobMemberCount
                memberCountAuthoritative = $jobMemberCountAuthoritative
            }
        }
        ownedProcessIdentities = [pscustomobject][ordered]@{
            helper = @()
            provider = @()
            hostChildren = @()
        }
        resources = [pscustomobject][ordered]@{
            listeners = [pscustomobject][ordered]@{ observed = @(); owned = $ownedListeners }
            namedPipes = [pscustomobject][ordered]@{ observed = @(); owned = $ownedNamedPipes }
            ptyHandles = [pscustomobject][ordered]@{ observed = @('pty-1'); leaked = $leakedPtyHandles }
            jobHandles = [pscustomobject][ordered]@{ observed = @('job-1'); leaked = $leakedJobHandles }
            delta = [pscustomobject][ordered]@{
                privateBytes = [pscustomobject][ordered]@{ before = 100000; after = 100000; delta = 0; budget = 16777216 }
                handles = [pscustomobject][ordered]@{ before = 10; after = 10; delta = 0; budget = 32 }
                listeners = [pscustomobject][ordered]@{ before = 0; after = 0; delta = 0; budget = 0 }
                namedPipes = [pscustomobject][ordered]@{ before = 0; after = 0; delta = 0; budget = 0 }
                ptyHandles = [pscustomobject][ordered]@{ before = 0; after = 0; delta = 0; budget = 0 }
                jobHandles = [pscustomobject][ordered]@{ before = 0; after = 0; delta = 0; budget = 0 }
            }
        }
        timing = [pscustomobject][ordered]@{
            launchMs = 1
            firstOutputMs = 1
            inputAckMs = 1
            closeSettlementMs = 1
            totalMs = 4
        }
    }
}
"#;
    script.replace("__EXTRA__", extra)
}

#[cfg(windows)]
fn process_is_alive(pid: u32) -> bool {
    let mut system = sysinfo::System::new();
    let process_id = sysinfo::Pid::from_u32(pid);
    system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[process_id]), true);
    system.process(process_id).is_some()
}

#[cfg(windows)]
fn force_terminate_process(pid: u32) {
    use std::ffi::c_void;

    const PROCESS_TERMINATE: u32 = 0x0001;
    unsafe extern "system" {
        fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> *mut c_void;
        fn TerminateProcess(process: *mut c_void, exit_code: u32) -> i32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }

    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if !handle.is_null() {
            let _ = TerminateProcess(handle, 1);
            let _ = CloseHandle(handle);
        }
    }
}

#[cfg(windows)]
struct ProcessCleanupGuard {
    pids: Vec<u32>,
}

#[cfg(windows)]
impl Drop for ProcessCleanupGuard {
    fn drop(&mut self) {
        for pid in self.pids.iter().copied() {
            if process_is_alive(pid) {
                force_terminate_process(pid);
            }
        }
    }
}

#[cfg(windows)]
#[test]
fn process_soak_rejects_bare_and_partial_completed_cycle_results() {
    for contents in [
        "function Invoke-DevManagerProcessSoakCycle { param($Iteration, $Seed, $Scenario, $WorktreeRoot) [pscustomobject]@{ status = 'completed' } }",
        "function Invoke-DevManagerProcessSoakCycle { param($Iteration, $Seed, $Scenario, $WorktreeRoot) [pscustomobject]@{ schemaVersion = 1; status = 'completed'; cycle = $Iteration; seed = $Seed } }",
    ] {
        let directory = temporary_soak_environment();
        let api_path = write_cycle_api(&directory, contents);
        let output = run_soak_with_api(&directory, &api_path, 1, 3403);
        assert_eq!(output.status.code(), Some(1), "runner output: {output:?}");
        let summary = summary_from_soak_output(&output);
        assert_eq!(summary["status"], "failed");
        assert!(summary["failure"].as_str().unwrap_or_default().contains("schema"));
    }
}

#[cfg(windows)]
#[test]
fn process_soak_rejects_wrong_cycle_and_seed_evidence() {
    for extra in [
        "$reportedCycle = $Iteration + 1",
        "$reportedSeed = $Seed + 1",
    ] {
        let directory = temporary_soak_environment();
        let api_path = write_cycle_api(&directory, &valid_cycle_api_script(extra));
        let output = run_soak_with_api(&directory, &api_path, 1, 3403);
        assert_eq!(output.status.code(), Some(1), "runner output: {output:?}");
        let summary = summary_from_soak_output(&output);
        assert_eq!(summary["status"], "failed");
        assert!(
            summary["failure"]
                .as_str()
                .unwrap_or_default()
                .contains("cycle")
                || summary["failure"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("seed")
        );
    }
}

#[cfg(windows)]
#[test]
fn process_soak_rejects_residue_evidence_before_claiming_completion() {
    let directory = temporary_soak_environment();
    let api_path = write_cycle_api(
        &directory,
        &valid_cycle_api_script("$jobMemberCount = 1\n    $ownedListeners = @('listener-1')"),
    );
    let output = run_soak_with_api(&directory, &api_path, 1, 3403);
    assert_eq!(output.status.code(), Some(1), "runner output: {output:?}");
    let summary = summary_from_soak_output(&output);
    assert_eq!(summary["status"], "failed");
    assert!(
        summary["failure"]
            .as_str()
            .unwrap_or_default()
            .contains("residue")
            || summary["failure"]
                .as_str()
                .unwrap_or_default()
                .contains("member")
    );
}

#[cfg(windows)]
#[test]
fn process_soak_baseline_predates_extension_and_catches_production_side_effects() {
    let directory = temporary_soak_environment();
    let api_path = write_cycle_api(
        &directory,
        &valid_cycle_api_script(
            "Set-Content -LiteralPath (Join-Path $env:APPDATA 'com.userfirst.devmanager\\config.json') -Value 'extension-side-effect'",
        ),
    );
    let output = run_soak_with_api(&directory, &api_path, 1, 3403);
    assert_eq!(output.status.code(), Some(1), "runner output: {output:?}");
    let summary = summary_from_soak_output(&output);
    assert_eq!(summary["status"], "failed");
    assert_eq!(summary["productionAssert"], "failed");
    assert!(summary["failure"]
        .as_str()
        .unwrap_or_default()
        .contains("config.json"));
}

#[cfg(windows)]
#[test]
fn process_soak_stops_at_failing_cycle_and_persists_sanitized_cycle_evidence() {
    let directory = temporary_soak_environment();
    let marker_path = directory.path().join("iterations.txt");
    let marker_text = marker_path.to_string_lossy().replace('\'', "''");
    let extra = format!(
        "Add-Content -LiteralPath '{marker_text}' -Value ([string]$Iteration)\n    if ($Iteration -eq 2) {{ throw 'cycle failure secret=top-secret' }}"
    );
    let api_path = write_cycle_api(&directory, &valid_cycle_api_script(&extra));
    let output = run_soak_with_api(&directory, &api_path, 3, 3403);
    assert_eq!(output.status.code(), Some(1), "runner output: {output:?}");
    let summary = summary_from_soak_output(&output);
    assert_eq!(summary["status"], "failed");
    assert_eq!(summary["completedCycles"], 1);
    assert_eq!(summary["cycles"][0]["cycle"], 1);
    assert_eq!(summary["cycles"][1]["status"], "failed");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("top-secret"));
    assert_eq!(
        fs::read_to_string(marker_path).expect("read cycle marker"),
        "1\r\n2\r\n"
    );
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

#[cfg(windows)]
#[test]
fn large_output_watchdog_settles_when_parent_never_reads_stdout() {
    let mut command = Command::new(helper_path());
    command.args([
        "large-output",
        "--duration-ms",
        "1",
        "--bytes",
        "67108864",
        "--watchdog-ms",
        "250",
    ]);
    configure_helper(&mut command);
    let mut child = command.spawn().expect("spawn unread large-output helper");
    let helper_pid = child.id();
    let _cleanup = ProcessCleanupGuard {
        pids: vec![helper_pid],
    };
    let deadline = Instant::now() + Duration::from_secs(3);
    let status = loop {
        match child.try_wait().expect("query unread helper") {
            Some(status) => break status,
            None if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("unread large-output helper exceeded watchdog deadline");
            }
        }
    };
    assert_eq!(
        status.code(),
        Some(124),
        "unexpected unread helper status: {status:?}"
    );
    assert!(
        !process_is_alive(helper_pid),
        "watchdog left helper PID {helper_pid} alive"
    );
}

#[cfg(windows)]
#[test]
fn forced_parent_close_cleans_spawned_child_identity() {
    let directory = tempfile::tempdir().expect("create child cleanup directory");
    let root_marker = directory.path().join("root.marker");
    let child_marker = directory.path().join("child.marker");
    let child_pid_path = directory.path().join("child.pid");
    let root_marker_text = root_marker.to_string_lossy().into_owned();
    let child_marker_text = child_marker.to_string_lossy().into_owned();
    let child_pid_text = child_pid_path.to_string_lossy().into_owned();

    let mut command = Command::new(helper_path());
    command.args([
        "spawn-child",
        &root_marker_text,
        &child_marker_text,
        &child_pid_text,
    ]);
    configure_helper(&mut command);
    let mut child = command.spawn().expect("spawn child-tree helper");
    let root_pid = child.id();
    let mut cleanup_pids = vec![root_pid];
    let child_pid_deadline = Instant::now() + Duration::from_secs(2);
    let child_pid = loop {
        if let Ok(contents) = fs::read_to_string(&child_pid_path) {
            break contents.trim().parse::<u32>().expect("child PID marker");
        }
        if Instant::now() >= child_pid_deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("child PID marker was not written");
        }
        thread::sleep(Duration::from_millis(10));
    };
    cleanup_pids.push(child_pid);
    let _cleanup = ProcessCleanupGuard { pids: cleanup_pids };

    let _ = child.kill();
    let _ = child.wait();
    thread::sleep(Duration::from_millis(100));
    assert!(
        !process_is_alive(child_pid),
        "forced parent close left child identity {child_pid} alive"
    );
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
    let output = run_soak_without_api(&directory, 100, 3403);
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
    let summary = summary_from_soak_output(&output);
    assert_eq!(summary["status"], "unavailable");
    assert_eq!(summary["iterations"], 100);
    assert_eq!(summary["completedCycles"], 0);
    assert!(summary["baselinePath"]
        .as_str()
        .unwrap_or_default()
        .ends_with("baseline.json"));
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
