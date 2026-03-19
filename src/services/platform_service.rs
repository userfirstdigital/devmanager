use std::path::Path;
use std::process::Command;

pub fn find_pid_on_port(port: u16) -> Result<Option<u32>, String> {
    #[cfg(windows)]
    {
        let output = Command::new("netstat")
            .args(["-ano", "-p", "tcp"])
            .output()
            .map_err(|error| format!("Failed to run netstat: {error}"))?;
        if !output.status.success() {
            return Err("netstat did not complete successfully".to_string());
        }
        let needle = format!(":{port}");
        for line in String::from_utf8_lossy(&output.stdout).lines() {
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
                return Ok(Some(pid));
            }
        }
        Ok(None)
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

pub fn kill_process(pid: u32) -> Result<(), String> {
    #[cfg(windows)]
    {
        let output = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .output()
            .map_err(|error| format!("Failed to run taskkill: {error}"))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
        }
    }

    #[cfg(not(windows))]
    {
        let output = Command::new("kill")
            .args(["-9", &pid.to_string()])
            .output()
            .map_err(|error| format!("Failed to run kill: {error}"))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
        }
    }
}

pub fn get_process_name(pid: u32) -> Result<Option<String>, String> {
    #[cfg(windows)]
    {
        let output = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
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
