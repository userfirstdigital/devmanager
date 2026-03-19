use crate::services::pid_file;
use crate::state::{AppState, PtySession};
use portable_pty::CommandBuilder;
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
#[cfg(unix)]
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct RuntimePlatformState {
    pub os: String,
    pub user_shell_path: Option<String>,
    pub user_shell_name: Option<String>,
    pub git_bash_path: Option<String>,
    pub login_env: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RuntimePlatformInfo {
    pub os: String,
    #[serde(rename = "userShellPath")]
    pub user_shell_path: Option<String>,
    #[serde(rename = "userShellName")]
    pub user_shell_name: Option<String>,
    #[serde(rename = "gitBashPath")]
    pub git_bash_path: Option<String>,
}

pub fn detect_runtime_platform() -> RuntimePlatformState {
    let os = current_os().to_string();

    #[cfg(target_os = "macos")]
    {
        let user_shell_path = detect_unix_user_shell().or_else(|| Some("/bin/zsh".to_string()));
        let mut login_env = user_shell_path
            .as_deref()
            .and_then(|shell| capture_login_shell_env(shell).ok())
            .unwrap_or_default();

        if let Some(shell) = &user_shell_path {
            login_env
                .entry("SHELL".to_string())
                .or_insert_with(|| shell.clone());
        }

        return RuntimePlatformState {
            os,
            user_shell_name: user_shell_path.as_deref().and_then(shell_name_from_path),
            user_shell_path,
            git_bash_path: None,
            login_env,
        };
    }

    #[cfg(not(target_os = "macos"))]
    {
        RuntimePlatformState {
            os,
            user_shell_name: None,
            user_shell_path: None,
            git_bash_path: detect_windows_git_bash_path(),
            login_env: HashMap::new(),
        }
    }
}

pub fn current_os() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    }
}

pub fn runtime_info(runtime: &RuntimePlatformState) -> RuntimePlatformInfo {
    RuntimePlatformInfo {
        os: runtime.os.clone(),
        user_shell_path: runtime.user_shell_path.clone(),
        user_shell_name: runtime.user_shell_name.clone(),
        git_bash_path: runtime.git_bash_path.clone(),
    }
}

pub fn apply_runtime_env(
    cmd: &mut CommandBuilder,
    runtime: &RuntimePlatformState,
    env: Option<&HashMap<String, String>>,
) {
    if runtime.os == "macos" {
        for (key, value) in &runtime.login_env {
            cmd.env(key, value);
        }
    }

    if let Some(env_vars) = env {
        for (key, value) in env_vars {
            cmd.env(key, value);
        }
    }
}

pub fn stop_all_tracked_processes(state: &AppState) {
    let pids: Vec<u32> = {
        let processes = state.processes.lock().unwrap();
        processes.values().map(|info| info.pid).collect()
    };

    for pid in pids {
        let _ = kill_process_tree(pid);
    }
}

pub fn shutdown_managed_processes(state: &AppState) {
    stop_all_tracked_processes(state);

    {
        let mut sessions = state.pty_sessions.lock().unwrap();
        for (_, mut session) in sessions.drain() {
            kill_pty_session(&mut session);
        }
    }

    {
        let mut processes = state.processes.lock().unwrap();
        processes.clear();
    }
    {
        let mut monitored = state.monitored_processes.lock().unwrap();
        monitored.clear();
    }
    {
        let mut buffers = state.pty_buffers.lock().unwrap();
        buffers.clear();
    }

    pid_file::clear_all();
}

pub fn kill_pty_session(session: &mut PtySession) {
    #[cfg(unix)]
    {
        if let Some(group_leader) = session.master.process_group_leader() {
            let _ = kill_unix_target(group_leader as u32, true);
        } else if let Some(pid) = session.child.process_id() {
            let _ = kill_unix_target(pid, true);
        } else {
            let _ = session.child.kill();
        }
        let _ = session.child.wait();
    }

    #[cfg(windows)]
    {
        if let Some(pid) = session.child.process_id() {
            let _ = kill_process_tree(pid);
        } else {
            let _ = session.child.kill();
        }
        let _ = session.child.wait();
    }
}

pub fn kill_process_tree(pid: u32) -> Result<(), String> {
    #[cfg(windows)]
    {
        let output = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .output()
            .map_err(|e| format!("Failed to run taskkill: {}", e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("taskkill failed for {}: {}", pid, stderr.trim()));
        }

        Ok(())
    }

    #[cfg(unix)]
    {
        kill_unix_target(pid, true)
    }
}

pub fn kill_process(pid: u32) -> Result<(), String> {
    #[cfg(windows)]
    {
        let output = Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .output()
            .map_err(|e| format!("Failed to run taskkill: {}", e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("taskkill failed for {}: {}", pid, stderr.trim()));
        }

        Ok(())
    }

    #[cfg(unix)]
    {
        kill_unix_target(pid, false)
    }
}

pub fn is_pid_running(pid: u32) -> bool {
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    sys.process(sysinfo::Pid::from_u32(pid)).is_some()
}

pub fn find_pid_on_port(port: u16) -> Result<Option<u32>, String> {
    #[cfg(windows)]
    {
        let output = Command::new("netstat")
            .args(["-ano"])
            .output()
            .map_err(|e| format!("Failed to run netstat: {}", e))?;

        if !output.status.success() || output.stdout.is_empty() {
            return Ok(None);
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let port_suffix = format!(":{}", port);

        for line in stdout.lines() {
            let line = line.trim();
            if !line.contains("LISTENING") {
                continue;
            }

            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 5 {
                continue;
            }

            let local_addr = parts[1];
            if !local_addr.ends_with(&port_suffix) {
                continue;
            }

            if local_addr.rsplit(':').next() == Some(port.to_string().as_str()) {
                if let Ok(pid) = parts[parts.len() - 1].parse::<u32>() {
                    return Ok(Some(pid));
                }
            }
        }

        Ok(None)
    }

    #[cfg(unix)]
    {
        let output = Command::new("lsof")
            .args(["-nP", &format!("-iTCP:{}", port), "-sTCP:LISTEN", "-t"])
            .output()
            .map_err(|e| format!("Failed to run lsof: {}", e))?;

        if !output.status.success() || output.stdout.is_empty() {
            return Ok(None);
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(stdout
            .lines()
            .find_map(|line| line.trim().parse::<u32>().ok()))
    }
}

pub fn get_process_name(pid: u32) -> Result<Option<String>, String> {
    #[cfg(windows)]
    {
        let output = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {}", pid), "/FO", "CSV", "/NH"])
            .output()
            .map_err(|e| format!("Failed to run tasklist: {}", e))?;

        if !output.status.success() {
            return Ok(None);
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let line = stdout.trim();
        if line.is_empty() || line.starts_with("INFO:") {
            return Ok(None);
        }

        Ok(line
            .split(',')
            .next()
            .map(|name| name.trim_matches('"').to_string()))
    }

    #[cfg(unix)]
    {
        let output = Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "comm="])
            .output()
            .map_err(|e| format!("Failed to run ps: {}", e))?;

        if !output.status.success() {
            return Ok(None);
        }

        let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if name.is_empty() {
            Ok(None)
        } else {
            Ok(Some(name))
        }
    }
}

pub fn open_terminal(
    runtime: &RuntimePlatformState,
    folder_path: &str,
    shell_path: Option<&str>,
) -> Result<(), String> {
    let _ = runtime;
    let _ = shell_path;

    #[cfg(windows)]
    {
        let quoted_path = format!("\"{}\"", folder_path);
        let wt_result = Command::new("cmd")
            .args(["/C", "start", "wt", "-d", &quoted_path])
            .output();

        match wt_result {
            Ok(output) if output.status.success() => Ok(()),
            _ => {
                Command::new("cmd")
                    .args([
                        "/C",
                        "start",
                        "cmd",
                        "/K",
                        &format!("cd /d {}", quoted_path),
                    ])
                    .output()
                    .map_err(|e| format!("Failed to open terminal: {}", e))?;
                Ok(())
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        let shell = shell_path
            .filter(|value| !value.trim().is_empty())
            .or(runtime.user_shell_path.as_deref())
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
            .map_err(|e| format!("Failed to open Terminal.app: {}", e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("Failed to open Terminal.app: {}", stderr.trim()));
        }

        Ok(())
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Command::new("xdg-open")
            .arg(folder_path)
            .output()
            .map_err(|e| format!("Failed to open terminal directory: {}", e))?;
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn applescript_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(target_os = "macos")]
fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        "''".to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

fn detect_windows_git_bash_path() -> Option<String> {
    if !cfg!(windows) {
        return None;
    }

    let candidates = [
        "C:/Program Files/Git/bin/bash.exe",
        "C:/Program Files (x86)/Git/bin/bash.exe",
    ];

    for candidate in candidates {
        if Path::new(candidate).exists() {
            return Some(candidate.to_string());
        }
    }

    Some(candidates[0].to_string())
}

#[cfg(target_os = "macos")]
fn shell_name_from_path(path: &str) -> Option<String> {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.to_string())
}

#[cfg(target_os = "macos")]
fn capture_login_shell_env(shell_path: &str) -> Result<HashMap<String, String>, String> {
    let output = Command::new(shell_path)
        .args(["-l", "-c", "env -0"])
        .output()
        .map_err(|e| format!("Failed to capture login shell env: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "Login shell env capture failed for {}: {}",
            shell_path,
            stderr.trim()
        ));
    }

    let mut env = HashMap::new();
    for entry in output.stdout.split(|b| *b == 0) {
        if entry.is_empty() {
            continue;
        }
        let Some(eq_index) = entry.iter().position(|b| *b == b'=') else {
            continue;
        };

        let key = String::from_utf8_lossy(&entry[..eq_index]).to_string();
        let value = String::from_utf8_lossy(&entry[eq_index + 1..]).to_string();
        env.insert(key, value);
    }

    Ok(env)
}

#[cfg(target_os = "macos")]
fn detect_unix_user_shell() -> Option<String> {
    use std::ffi::CStr;

    let passwd_entry = unsafe { libc::getpwuid(libc::geteuid()) };
    if passwd_entry.is_null() {
        return None;
    }

    let shell_ptr = unsafe { (*passwd_entry).pw_shell };
    if shell_ptr.is_null() {
        return None;
    }

    let shell = unsafe { CStr::from_ptr(shell_ptr) };
    let shell = shell.to_str().ok()?.trim();
    if shell.is_empty() {
        None
    } else {
        Some(shell.to_string())
    }
}

#[cfg(unix)]
fn kill_unix_target(pid: u32, as_process_group: bool) -> Result<(), String> {
    use libc::{EPERM, ESRCH, SIGKILL, SIGTERM};

    let target_pid = pid as i32;
    let signal_target = if as_process_group {
        -target_pid
    } else {
        target_pid
    };

    let term_error = send_unix_signal(signal_target, SIGTERM);
    let mut used_group = as_process_group;

    if let Err(err) = term_error {
        match err.raw_os_error() {
            Some(ESRCH) if as_process_group => {
                used_group = false;
                send_unix_signal(target_pid, SIGTERM).map_err(|direct_err| {
                    format!("Failed to terminate process {}: {}", pid, direct_err)
                })?;
            }
            Some(ESRCH) => return Ok(()),
            Some(EPERM) => {}
            _ => {
                return Err(format!("Failed to terminate process {}: {}", pid, err));
            }
        }
    }

    if wait_for_pid_exit(target_pid, Duration::from_secs(2)) {
        return Ok(());
    }

    let kill_target = if used_group { -target_pid } else { target_pid };
    let kill_error = send_unix_signal(kill_target, SIGKILL);
    if let Err(err) = kill_error {
        match err.raw_os_error() {
            Some(ESRCH) => return Ok(()),
            Some(EPERM) => {}
            _ => return Err(format!("Failed to kill process {}: {}", pid, err)),
        }
    }

    if wait_for_pid_exit(target_pid, Duration::from_secs(1)) {
        Ok(())
    } else {
        Err(format!("Process {} did not exit after SIGKILL", pid))
    }
}

#[cfg(unix)]
fn send_unix_signal(target: i32, signal: libc::c_int) -> Result<(), std::io::Error> {
    let result = unsafe { libc::kill(target, signal) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn wait_for_pid_exit(pid: i32, timeout: Duration) -> bool {
    let start = Instant::now();

    while start.elapsed() < timeout {
        if !unix_pid_exists(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    !unix_pid_exists(pid)
}

#[cfg(unix)]
fn unix_pid_exists(pid: i32) -> bool {
    let result = unsafe { libc::kill(pid, 0) };
    if result == 0 {
        true
    } else {
        !matches!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        )
    }
}
