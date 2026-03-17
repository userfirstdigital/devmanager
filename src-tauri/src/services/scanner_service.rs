use crate::models::config::{ScanResult, ScannedPort, ScannedScript};
use regex::Regex;
use std::path::Path;
use std::sync::LazyLock;

/// Lazily compiled regex for matching port variables in .env files
static PORT_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(PORT|.*_PORT)\s*=\s*(\d+)").unwrap());

/// Read cargo scripts from a Cargo.toml file
pub fn read_cargo_scripts(cargo_toml: &Path) -> Vec<ScannedScript> {
    let content = match std::fs::read_to_string(cargo_toml) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let parsed: toml::Value = match content.parse() {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };

    let mut scripts = vec![
        ScannedScript {
            name: "cargo run".to_string(),
            command: "cargo run".to_string(),
        },
        ScannedScript {
            name: "cargo build".to_string(),
            command: "cargo build".to_string(),
        },
        ScannedScript {
            name: "cargo test".to_string(),
            command: "cargo test".to_string(),
        },
        ScannedScript {
            name: "cargo check".to_string(),
            command: "cargo check".to_string(),
        },
    ];

    // If the crate has [[bin]] targets, add them as cargo run --bin <name>
    if let Some(bins) = parsed.get("bin").and_then(|b| b.as_array()) {
        for bin in bins {
            if let Some(name) = bin.get("name").and_then(|n| n.as_str()) {
                scripts.push(ScannedScript {
                    name: format!("cargo run --bin {}", name),
                    command: format!("cargo run --bin {}", name),
                });
            }
        }
    }

    scripts
}

/// Expand a bare "tauri" npm script into "tauri dev" and "tauri build" entries
pub fn expand_tauri_scripts(scripts: &mut Vec<ScannedScript>) {
    if let Some(idx) = scripts.iter().position(|s| s.name == "tauri") {
        let tauri_cmd = scripts[idx].command.clone();
        scripts.remove(idx);
        scripts.insert(
            idx,
            ScannedScript {
                name: "tauri dev".to_string(),
                command: format!("{} dev", tauri_cmd),
            },
        );
        scripts.insert(
            idx + 1,
            ScannedScript {
                name: "tauri build".to_string(),
                command: format!("{} build", tauri_cmd),
            },
        );
    }
}

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

        // Expand bare "tauri" script into "tauri dev" and "tauri build"
        expand_tauri_scripts(&mut scripts);
    }

    // Check for Cargo.toml and extract cargo scripts
    let cargo_toml_path = base.join("Cargo.toml");
    let has_cargo_toml = cargo_toml_path.exists();

    if has_cargo_toml {
        let cargo_scripts = read_cargo_scripts(&cargo_toml_path);
        scripts.extend(cargo_scripts);
    }

    // Scan .env files for port variables
    let env_files = [".env", ".env.local", ".env.development"];
    let mut ports = Vec::new();
    let mut has_env_file = false;

    for env_file in &env_files {
        let env_path = base.join(env_file);
        if env_path.exists() {
            has_env_file = true;
            let contents = std::fs::read_to_string(&env_path)
                .map_err(|e| format!("Failed to read {}: {}", env_file, e))?;

            for line in contents.lines() {
                if let Some(captures) = PORT_REGEX.captures(line) {
                    if let (Some(var_match), Some(port_match)) = (captures.get(1), captures.get(2))
                    {
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
        has_cargo_toml,
        has_env_file,
    })
}
