use crate::models::config::{ScanResult, ScannedScript, ScannedPort};
use regex::Regex;
use std::path::Path;

/// Scan a project directory for scripts, ports, and configuration
pub fn scan_directory(folder_path: &str) -> Result<ScanResult, String> {
    let base = Path::new(folder_path);

    // Check for package.json and extract scripts
    let package_json_path = base.join("package.json");
    let has_package_json = package_json_path.exists();
    let mut scripts = Vec::new();

    if has_package_json {
        let contents = std::fs::read_to_string(&package_json_path)
            .map_err(|e| format!("Failed to read package.json: {}", e))?;
        let json: serde_json::Value = serde_json::from_str(&contents)
            .map_err(|e| format!("Failed to parse package.json: {}", e))?;

        if let Some(scripts_obj) = json.get("scripts").and_then(|s| s.as_object()) {
            for (name, command) in scripts_obj {
                if let Some(cmd_str) = command.as_str() {
                    scripts.push(ScannedScript {
                        name: name.clone(),
                        command: cmd_str.to_string(),
                    });
                }
            }
        }
    }

    // Scan .env files for port variables
    let env_files = [".env", ".env.local", ".env.development"];
    let mut ports = Vec::new();
    let mut has_env_file = false;

    let port_regex = Regex::new(r"(?i)^(PORT|.*_PORT)\s*=\s*(\d+)")
        .map_err(|e| format!("Failed to compile regex: {}", e))?;

    for env_file in &env_files {
        let env_path = base.join(env_file);
        if env_path.exists() {
            has_env_file = true;
            let contents = std::fs::read_to_string(&env_path)
                .map_err(|e| format!("Failed to read {}: {}", env_file, e))?;

            for line in contents.lines() {
                if let Some(captures) = port_regex.captures(line) {
                    if let (Some(var_match), Some(port_match)) = (captures.get(1), captures.get(2)) {
                        let variable = var_match.as_str().to_string();
                        if let Ok(port) = port_match.as_str().parse::<u16>() {
                            ports.push(ScannedPort {
                                variable,
                                port,
                                source: env_file.to_string(),
                            });
                        }
                    }
                }
            }
        }
    }

    Ok(ScanResult {
        scripts,
        ports,
        has_package_json,
        has_env_file,
    })
}
