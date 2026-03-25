use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::Path;
use std::process::Command;
#[cfg(not(windows))]
use std::thread;
#[cfg(not(windows))]
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

pub fn find_pid_on_port(port: u16) -> Result<Option<u32>, String> {
    #[cfg(windows)]
    {
        match find_pid_on_port_with_netstat(port) {
            Ok(Some(pid)) => return Ok(Some(pid)),
            Ok(None) => {}
            Err(_) => {}
        }
        find_pid_on_port_with_powershell(port)
    }

    #[cfg(not(windows))]
    {
        let output = Command::new("lsof")
            .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-t"])
            .output()
            .map_err(|error| format!("Failed to run lsof: {error}"))?;
        if !output.status.success() {
            return Ok(None);
        }
        let pid = String::from_utf8_lossy(&output.stdout)
            .lines()
            .find_map(|line| line.trim().parse::<u32>().ok());
        Ok(pid)
    }
}

#[cfg(windows)]
fn find_pid_on_port_with_netstat(port: u16) -> Result<Option<u32>, String> {
    let output = Command::new("netstat")
        .args(["-ano", "-p", "tcp"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|error| format!("Failed to run netstat: {error}"))?;
    if !output.status.success() {
        return Err("netstat did not complete successfully".to_string());
    }

    Ok(parse_netstat_pid_on_port(
        &String::from_utf8_lossy(&output.stdout),
        port,
    ))
}

#[cfg(windows)]
fn find_pid_on_port_with_powershell(port: u16) -> Result<Option<u32>, String> {
    let script = format!(
        "$conn = Get-NetTCPConnection -State Listen -LocalPort {port} -ErrorAction SilentlyContinue | Select-Object -First 1; if ($conn) {{ $conn.OwningProcess }}"
    );
    let output = Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &script,
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|error| format!("Failed to run PowerShell port probe: {error}"))?;
    if !output.status.success() {
        return Err("PowerShell port probe did not complete successfully".to_string());
    }

    Ok(parse_pid_output(&String::from_utf8_lossy(&output.stdout)))
}

#[cfg(windows)]
fn parse_netstat_pid_on_port(output: &str, port: u16) -> Option<u32> {
    let needle = format!(":{port}");
    for line in output.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("TCP") {
            continue;
        }
        let columns: Vec<&str> = trimmed.split_whitespace().collect();
        if columns.len() < 5 {
            continue;
        }
        if !columns[1].ends_with(&needle) || !columns[3].eq_ignore_ascii_case("LISTENING") {
            continue;
        }
        if let Ok(pid) = columns[4].parse::<u32>() {
            return Some(pid);
        }
    }
    None
}

#[cfg(windows)]
fn parse_pid_output(output: &str) -> Option<u32> {
    output
        .lines()
        .find_map(|line| line.trim().parse::<u32>().ok())
}

pub fn kill_process_tree(pid: u32) -> Result<(), String> {
    #[cfg(windows)]
    {
        run_taskkill(pid, true)
    }

    #[cfg(not(windows))]
    {
        kill_unix_target(pid, true)
    }
}

pub fn kill_process(pid: u32) -> Result<(), String> {
    #[cfg(windows)]
    {
        run_taskkill(pid, false)
    }

    #[cfg(not(windows))]
    {
        kill_unix_target(pid, false)
    }
}

pub fn is_pid_running(pid: u32) -> bool {
    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    system.process(sysinfo::Pid::from_u32(pid)).is_some()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub started_at_unix_secs: u64,
    pub process_name: Option<String>,
}

pub fn capture_process_identity(pid: u32) -> Option<ProcessIdentity> {
    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    process_identity_with_system(&system, pid)
}

pub fn process_identity_with_system(system: &sysinfo::System, pid: u32) -> Option<ProcessIdentity> {
    let process = system.process(sysinfo::Pid::from_u32(pid))?;
    Some(ProcessIdentity {
        pid,
        started_at_unix_secs: process.start_time(),
        process_name: normalize_process_name(process.name()),
    })
}

pub fn process_matches_identity(
    pid: u32,
    started_at_unix_secs: u64,
    expected_name: Option<&str>,
) -> bool {
    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    process_matches_identity_with_system(&system, pid, started_at_unix_secs, expected_name)
}

pub fn process_matches_identity_with_system(
    system: &sysinfo::System,
    pid: u32,
    started_at_unix_secs: u64,
    expected_name: Option<&str>,
) -> bool {
    if started_at_unix_secs == 0 {
        return false;
    }
    let Some(identity) = process_identity_with_system(system, pid) else {
        return false;
    };
    if identity.started_at_unix_secs != started_at_unix_secs {
        return false;
    }
    match expected_name.filter(|name| !name.trim().is_empty()) {
        Some(expected_name) => identity
            .process_name
            .as_deref()
            .map(|actual_name| actual_name.eq_ignore_ascii_case(expected_name))
            .unwrap_or(false),
        None => true,
    }
}

pub fn collect_descendant_process_identities(root_pid: u32) -> Vec<ProcessIdentity> {
    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    collect_descendant_process_identities_with_system(&system, root_pid)
}

pub fn collect_descendant_process_identities_with_system(
    system: &sysinfo::System,
    root_pid: u32,
) -> Vec<ProcessIdentity> {
    let root_pid = sysinfo::Pid::from_u32(root_pid);
    let mut queue = vec![root_pid];
    let mut visited = HashSet::from([root_pid]);
    let mut descendants = Vec::new();
    let mut cursor = 0;

    while cursor < queue.len() {
        let parent_pid = queue[cursor];
        cursor += 1;

        for (candidate_pid, process) in system.processes() {
            if process.parent() == Some(parent_pid) && visited.insert(*candidate_pid) {
                queue.push(*candidate_pid);
                if let Some(identity) = process_identity_with_system(system, candidate_pid.as_u32())
                {
                    descendants.push(identity);
                }
            }
        }
    }

    descendants.sort_by_key(|identity| identity.pid);
    descendants
}

pub fn get_process_name(pid: u32) -> Result<Option<String>, String> {
    #[cfg(windows)]
    {
        let output = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map_err(|error| format!("Failed to run tasklist: {error}"))?;
        if !output.status.success() {
            return Ok(None);
        }
        let line = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if line.is_empty() || line.contains("No tasks are running") {
            return Ok(None);
        }
        let first = line
            .trim_matches('"')
            .split("\",\"")
            .next()
            .map(|value| value.to_string());
        Ok(first.filter(|value| !value.is_empty()))
    }

    #[cfg(not(windows))]
    {
        let output = Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "comm="])
            .output()
            .map_err(|error| format!("Failed to run ps: {error}"))?;
        if !output.status.success() {
            return Ok(None);
        }
        let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok((!name.is_empty()).then_some(name))
    }
}

fn normalize_process_name(name: &OsStr) -> Option<String> {
    let value = name.to_string_lossy().trim().to_string();
    (!value.is_empty()).then_some(value)
}

#[cfg(all(test, windows))]
mod tests {
    use super::{parse_netstat_pid_on_port, parse_pid_output};

    #[test]
    fn parse_netstat_pid_on_port_handles_ipv4_and_ipv6() {
        let output = r#"
  TCP    127.0.0.1:3000         0.0.0.0:0              LISTENING       1111
  TCP    [::1]:5174             [::]:0                 LISTENING       2222
"#;

        assert_eq!(parse_netstat_pid_on_port(output, 3000), Some(1111));
        assert_eq!(parse_netstat_pid_on_port(output, 5174), Some(2222));
        assert_eq!(parse_netstat_pid_on_port(output, 9999), None);
    }

    #[test]
    fn parse_pid_output_ignores_blank_lines() {
        assert_eq!(parse_pid_output("\r\n5174\r\n"), Some(5174));
        assert_eq!(parse_pid_output(""), None);
    }
}

pub fn open_terminal(folder_path: &str, shell_path: Option<&str>) -> Result<(), String> {
    let path = Path::new(folder_path);
    if !path.exists() {
        return Err(format!("Directory does not exist: {}", path.display()));
    }

    #[cfg(windows)]
    {
        let quoted_path = format!("\"{}\"", folder_path);
        let wt_result = Command::new("cmd")
            .args(["/C", "start", "wt", "-d", &quoted_path])
            .output();
        match wt_result {
            Ok(output) if output.status.success() => Ok(()),
            _ => {
                let command = match shell_path.filter(|value| !value.trim().is_empty()) {
                    Some(shell) => format!("cd /d {quoted_path} && \"{shell}\""),
                    None => format!("cd /d {quoted_path}"),
                };
                Command::new("cmd")
                    .args(["/C", "start", "cmd", "/K", &command])
                    .output()
                    .map_err(|error| format!("Failed to open terminal: {error}"))?;
                Ok(())
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        let shell = shell_path
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("/bin/zsh");
        let terminal_command = format!(
            "cd {} && exec {} -l",
            shell_quote(folder_path),
            shell_quote(shell)
        );
        let output = Command::new("osascript")
            .args(["-e", "tell application \"Terminal\""])
            .args(["-e", "activate"])
            .args([
                "-e",
                &format!("do script {}", applescript_quote(&terminal_command)),
            ])
            .args(["-e", "end tell"])
            .output()
            .map_err(|error| format!("Failed to open Terminal.app: {error}"))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let output = Command::new("xdg-open")
            .arg(folder_path)
            .output()
            .map_err(|error| format!("Failed to open directory: {error}"))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
        }
    }
}

pub fn open_url(url: &str) -> Result<(), String> {
    #[cfg(windows)]
    {
        Command::new("cmd")
            .args(["/C", "start", "", url])
            .output()
            .map_err(|error| format!("Failed to open URL: {error}"))?;
        Ok(())
    }

    #[cfg(target_os = "macos")]
    {
        let output = Command::new("open")
            .arg(url)
            .output()
            .map_err(|error| format!("Failed to open URL: {error}"))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let output = Command::new("xdg-open")
            .arg(url)
            .output()
            .map_err(|error| format!("Failed to open URL: {error}"))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
        }
    }
}

#[cfg(target_os = "macos")]
fn applescript_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(windows)]
fn run_taskkill(pid: u32, include_tree: bool) -> Result<(), String> {
    let mut command = Command::new("taskkill");
    command.args(["/PID", &pid.to_string(), "/F"]);
    command.creation_flags(CREATE_NO_WINDOW);
    if include_tree {
        command.arg("/T");
    }

    let output = command
        .output()
        .map_err(|error| format!("Failed to run taskkill: {error}"))?;
    if output.status.success() || !is_pid_running(pid) {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

#[cfg(not(windows))]
fn kill_unix_target(pid: u32, as_process_group: bool) -> Result<(), String> {
    let target = pid.to_string();
    let group_target = format!("-{pid}");
    let mut used_group = as_process_group;

    if let Err(error) = send_unix_signal(
        if as_process_group {
            group_target.as_str()
        } else {
            target.as_str()
        },
        "TERM",
    ) {
        if as_process_group {
            used_group = false;
            if is_pid_running(pid) {
                send_unix_signal(target.as_str(), "TERM").map_err(|direct_error| {
                    format!("Failed to terminate process {pid}: {direct_error}")
                })?;
            }
        } else if is_pid_running(pid) {
            return Err(format!("Failed to terminate process {pid}: {error}"));
        } else {
            return Ok(());
        }
    }

    if wait_for_pid_exit(pid, Duration::from_secs(2)) {
        return Ok(());
    }

    let kill_target = if used_group {
        group_target.as_str()
    } else {
        target.as_str()
    };
    if let Err(error) = send_unix_signal(kill_target, "KILL") {
        if is_pid_running(pid) {
            return Err(format!("Failed to kill process {pid}: {error}"));
        }
        return Ok(());
    }

    if wait_for_pid_exit(pid, Duration::from_secs(1)) {
        Ok(())
    } else {
        Err(format!("Process {pid} did not exit after SIGKILL"))
    }
}

#[cfg(not(windows))]
fn send_unix_signal(target: &str, signal: &str) -> Result<(), String> {
    let output = Command::new("kill")
        .args([&format!("-{signal}"), "--", target])
        .output()
        .map_err(|error| format!("Failed to run kill: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

#[cfg(not(windows))]
fn wait_for_pid_exit(pid: u32, timeout: Duration) -> bool {
    let started_at = Instant::now();
    while started_at.elapsed() < timeout {
        if !is_pid_running(pid) {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    !is_pid_running(pid)
}

#[cfg(target_os = "macos")]
fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        "''".to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}
