use crate::models::{
    DependencyStatus, RootScanEntry, RunCommand, ScanResult, ScannedPort, ScannedScript,
};
use regex::Regex;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::LazyLock;

static PORT_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(PORT|.*_PORT)\s*=\s*(\d+)").unwrap());

const ROOT_SCAN_SKIP_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    "dist",
    "build",
    "target",
    ".next",
    ".nuxt",
    "zz-archive",
    "archive",
    "archives",
];

const AUTO_SELECTED_SCRIPT_NAMES: &[&str] = &["dev", "start", "serve", "cargo run", "tauri dev"];

pub fn scan_project(folder_path: &str) -> Result<ScanResult, String> {
    let base = Path::new(folder_path);
    if !base.is_dir() {
        return Err("Folder path is not a directory".to_string());
    }

    let package_json_path = base.join("package.json");
    let cargo_toml_path = base.join("Cargo.toml");
    let has_package_json = package_json_path.exists();
    let has_cargo_toml = cargo_toml_path.exists();

    let mut scripts = Vec::new();
    if has_package_json {
        scripts.extend(read_package_scripts(&package_json_path)?);
    }
    if has_cargo_toml {
        scripts.extend(read_cargo_scripts(&cargo_toml_path)?);
    }
    scripts.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then(left.command.cmp(&right.command))
    });
    scripts.dedup_by(|left, right| left.name == right.name && left.command == right.command);

    let ports = scan_env_ports(base)?;

    Ok(ScanResult {
        scripts,
        ports,
        has_package_json,
        has_cargo_toml,
        has_env_file: default_env_file_for_dir(base).is_some(),
    })
}

pub fn scan_root(root_path: &str) -> Result<Vec<RootScanEntry>, String> {
    let root = Path::new(root_path);
    if !root.is_dir() {
        return Err("Root path is not a directory".to_string());
    }

    let mut entries = Vec::new();
    scan_dir_recursive(root, root, 0, 3, &mut entries)?;
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(entries)
}

pub fn check_dependencies(folder_path: &str) -> Result<DependencyStatus, String> {
    let base = Path::new(folder_path);
    let package_json = base.join("package.json");
    if !package_json.exists() {
        return Ok(DependencyStatus {
            status: "ok".to_string(),
            message: "No package.json found.".to_string(),
        });
    }

    let node_modules = base.join("node_modules");
    if !node_modules.exists() {
        return Ok(DependencyStatus {
            status: "missing".to_string(),
            message: "node_modules directory not found. Run npm install.".to_string(),
        });
    }

    let package_lock_in_modules = node_modules.join(".package-lock.json");
    if package_lock_in_modules.exists() {
        let package_modified = package_json
            .metadata()
            .and_then(|metadata| metadata.modified())
            .map_err(|error| format!("Failed to read package.json mtime: {error}"))?;
        let installed_modified = package_lock_in_modules
            .metadata()
            .and_then(|metadata| metadata.modified())
            .map_err(|error| format!("Failed to read node_modules mtime: {error}"))?;
        if package_modified > installed_modified {
            return Ok(DependencyStatus {
                status: "outdated".to_string(),
                message: "package.json changed after the last install. Run npm install."
                    .to_string(),
            });
        }
    }

    Ok(DependencyStatus {
        status: "ok".to_string(),
        message: "Dependencies are up to date.".to_string(),
    })
}

pub fn read_git_branch(folder_path: &str) -> Result<Option<String>, String> {
    let head_path = Path::new(folder_path).join(".git").join("HEAD");
    match std::fs::read_to_string(&head_path) {
        Ok(contents) => {
            let trimmed = contents.trim();
            if let Some(branch) = trimmed.strip_prefix("ref: refs/heads/") {
                Ok(Some(branch.to_string()))
            } else if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(trimmed.chars().take(8).collect()))
            }
        }
        Err(_) => Ok(None),
    }
}

pub fn default_env_file_for_dir(dir: &Path) -> Option<String> {
    env_file_candidates()
        .iter()
        .find(|candidate| dir.join(candidate).exists())
        .map(|candidate| (*candidate).to_string())
}

pub fn auto_selected_script_names(scripts: &[ScannedScript]) -> Vec<String> {
    let selected: BTreeSet<String> = scripts
        .iter()
        .filter(|script| AUTO_SELECTED_SCRIPT_NAMES.contains(&script.name.as_str()))
        .map(|script| script.name.clone())
        .collect();
    selected.into_iter().collect()
}

pub fn auto_selected_port_variable(ports: &[ScannedPort]) -> Option<String> {
    if let Some(port) = ports
        .iter()
        .find(|port| port.variable.eq_ignore_ascii_case("PORT"))
    {
        return Some(port.variable.clone());
    }

    const PREFERRED_VARIABLES: &[&str] = &[
        "APP_PORT",
        "SERVER_PORT",
        "API_PORT",
        "WEB_PORT",
        "DEV_PORT",
        "VITE_DEV_PORT",
        "FRONTEND_PORT",
        "BACKEND_PORT",
    ];

    for preferred in PREFERRED_VARIABLES {
        if let Some(port) = ports
            .iter()
            .find(|port| port.variable.eq_ignore_ascii_case(preferred))
        {
            return Some(port.variable.clone());
        }
    }

    let primary_ports: Vec<&ScannedPort> = ports
        .iter()
        .filter(|port| !is_auxiliary_port_variable(&port.variable))
        .collect();
    if primary_ports.len() == 1 {
        return primary_ports.first().map(|port| port.variable.clone());
    }

    if ports.len() == 1 {
        ports.first().map(|port| port.variable.clone())
    } else {
        None
    }
}

pub fn port_for_variable(ports: &[ScannedPort], variable: Option<&str>) -> Option<u16> {
    let variable = variable?;
    ports
        .iter()
        .find(|port| port.variable == variable)
        .map(|port| port.port)
}

pub fn build_run_command_from_scanned_script(
    script: &ScannedScript,
    id: String,
    port: Option<u16>,
) -> RunCommand {
    let (program, args) = scanned_script_command_parts(script);
    RunCommand {
        id,
        label: script.name.clone(),
        command: program,
        args,
        env: None,
        port,
        auto_restart: Some(false),
        clear_logs_on_restart: Some(true),
    }
}

pub fn merge_scanned_commands(
    existing: &[RunCommand],
    scripts: &[ScannedScript],
    selected_names: &[String],
    selected_port_variable: Option<&str>,
    id_factory: &mut dyn FnMut() -> String,
) -> Vec<RunCommand> {
    let selected_port = port_for_variable(
        &scripts
            .iter()
            .filter_map(|_| None::<ScannedPort>)
            .collect::<Vec<_>>(),
        selected_port_variable,
    );
    let _ = selected_port;

    let mut commands = existing.to_vec();
    let existing_labels: BTreeSet<String> = commands
        .iter()
        .map(|command| command.label.clone())
        .collect();

    for selected_name in selected_names {
        let Some(script) = scripts.iter().find(|script| &script.name == selected_name) else {
            continue;
        };
        if existing_labels.contains(selected_name) {
            continue;
        }
        commands.push(build_run_command_from_scanned_script(
            script,
            id_factory(),
            None,
        ));
    }

    commands
}

pub fn env_file_candidates() -> &'static [&'static str] {
    &[".env", ".env.local", ".env.development"]
}

pub fn scanned_script_command_parts(script: &ScannedScript) -> (String, Vec<String>) {
    if script.command.starts_with("cargo ") {
        let args = script
            .command
            .split_whitespace()
            .skip(1)
            .map(ToString::to_string)
            .collect();
        return ("cargo".to_string(), args);
    }

    let mut args = vec!["run".to_string()];
    args.extend(script.name.split_whitespace().map(ToString::to_string));
    ("npm".to_string(), args)
}

fn scan_dir_recursive(
    root: &Path,
    dir: &Path,
    depth: u32,
    max_depth: u32,
    entries: &mut Vec<RootScanEntry>,
) -> Result<(), String> {
    if depth > max_depth {
        return Ok(());
    }

    let package_json = dir.join("package.json");
    let cargo_toml = dir.join("Cargo.toml");
    let has_package_json = package_json.exists();
    let has_cargo_toml = cargo_toml.exists();

    if (has_package_json || has_cargo_toml) && dir != root {
        entries.push(build_root_scan_entry(
            dir,
            has_package_json,
            has_cargo_toml,
            &package_json,
            &cargo_toml,
        ));
    }

    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return Ok(());
    };

    for entry in read_dir.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let name = path
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_default();
        if ROOT_SCAN_SKIP_DIRS.contains(&name.as_str()) || name.starts_with('.') {
            continue;
        }
        scan_dir_recursive(root, &path, depth + 1, max_depth, entries)?;
    }

    Ok(())
}

fn build_root_scan_entry(
    dir: &Path,
    has_package_json: bool,
    has_cargo_toml: bool,
    package_json: &Path,
    cargo_toml: &Path,
) -> RootScanEntry {
    let mut scripts = Vec::new();
    if has_package_json {
        if let Ok(found_scripts) = read_package_scripts(package_json) {
            scripts.extend(found_scripts);
        }
    }
    if has_cargo_toml {
        if let Ok(found_scripts) = read_cargo_scripts(cargo_toml) {
            scripts.extend(found_scripts);
        }
    }
    scripts.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then(left.command.cmp(&right.command))
    });
    scripts.dedup_by(|left, right| left.name == right.name && left.command == right.command);

    let name = dir
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_default();
    let project_type = match (has_package_json, has_cargo_toml) {
        (true, true) => "both",
        (false, true) => "rust",
        _ => "node",
    }
    .to_string();

    RootScanEntry {
        path: dir.to_string_lossy().to_string(),
        name,
        has_env: default_env_file_for_dir(dir).is_some(),
        project_type,
        scripts,
        ports: scan_env_ports(dir).unwrap_or_default(),
    }
}

fn is_auxiliary_port_variable(variable: &str) -> bool {
    const AUXILIARY_VARIABLES: &[&str] = &[
        "DATABASE_PORT",
        "PGPORT",
        "POSTGRES_PORT",
        "MYSQL_PORT",
        "MONGO_PORT",
        "REDIS_PORT",
        "SMTP_PORT",
        "MAIL_PORT",
        "SSH_PORT",
        "FTP_PORT",
        "S3_PORT",
        "AMQP_PORT",
        "RABBITMQ_PORT",
    ];
    const AUXILIARY_TOKENS: &[&str] = &[
        "DATABASE", "DB", "POSTGRES", "MYSQL", "MONGO", "REDIS", "SMTP", "MAIL", "SSH", "FTP",
        "S3", "AMQP", "RABBITMQ",
    ];

    let normalized = variable.to_ascii_uppercase();
    AUXILIARY_VARIABLES.contains(&normalized.as_str())
        || normalized
            .split('_')
            .any(|token| AUXILIARY_TOKENS.contains(&token))
}

fn scan_env_ports(dir: &Path) -> Result<Vec<ScannedPort>, String> {
    let mut ports = Vec::new();

    for env_file in env_file_candidates() {
        let env_path = dir.join(env_file);
        if !env_path.exists() {
            continue;
        }

        let contents = std::fs::read_to_string(&env_path)
            .map_err(|error| format!("Failed to read {}: {error}", env_path.display()))?;
        for line in contents.lines() {
            if let Some(captures) = PORT_REGEX.captures(line.trim()) {
                let Some(variable_match) = captures.get(1) else {
                    continue;
                };
                let Some(port_match) = captures.get(2) else {
                    continue;
                };
                let Ok(port) = port_match.as_str().parse::<u16>() else {
                    continue;
                };
                ports.push(ScannedPort {
                    variable: variable_match.as_str().to_string(),
                    port,
                    source: (*env_file).to_string(),
                });
            }
        }
    }

    ports.sort_by(|left, right| {
        left.variable
            .cmp(&right.variable)
            .then(left.port.cmp(&right.port))
            .then(left.source.cmp(&right.source))
    });
    ports.dedup_by(|left, right| {
        left.variable == right.variable && left.port == right.port && left.source == right.source
    });
    Ok(ports)
}

fn read_package_scripts(package_json: &Path) -> Result<Vec<ScannedScript>, String> {
    let content = std::fs::read_to_string(package_json)
        .map_err(|error| format!("Failed to read {}: {error}", package_json.display()))?;
    let json: serde_json::Value = serde_json::from_str(&content)
        .map_err(|error| format!("Failed to parse {}: {error}", package_json.display()))?;
    let Some(scripts) = json.get("scripts").and_then(|value| value.as_object()) else {
        return Ok(Vec::new());
    };

    let mut result: Vec<ScannedScript> = scripts
        .iter()
        .map(|(name, command)| ScannedScript {
            name: name.clone(),
            command: command.as_str().unwrap_or("").to_string(),
        })
        .collect();
    expand_tauri_scripts(&mut result);
    Ok(result)
}

fn read_cargo_scripts(cargo_toml: &Path) -> Result<Vec<ScannedScript>, String> {
    let content = std::fs::read_to_string(cargo_toml)
        .map_err(|error| format!("Failed to read {}: {error}", cargo_toml.display()))?;
    let parsed: toml::Value = content
        .parse()
        .map_err(|error| format!("Failed to parse {}: {error}", cargo_toml.display()))?;

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

    if let Some(bins) = parsed.get("bin").and_then(|value| value.as_array()) {
        for bin in bins {
            let Some(name) = bin.get("name").and_then(|value| value.as_str()) else {
                continue;
            };
            scripts.push(ScannedScript {
                name: format!("cargo run --bin {name}"),
                command: format!("cargo run --bin {name}"),
            });
        }
    }

    Ok(scripts)
}

fn expand_tauri_scripts(scripts: &mut Vec<ScannedScript>) {
    let Some(index) = scripts.iter().position(|script| script.name == "tauri") else {
        return;
    };

    let tauri_command = scripts[index].command.clone();
    scripts.remove(index);
    scripts.insert(
        index,
        ScannedScript {
            name: "tauri dev".to_string(),
            command: format!("{tauri_command} dev"),
        },
    );
    scripts.insert(
        index + 1,
        ScannedScript {
            name: "tauri build".to_string(),
            command: format!("{tauri_command} build"),
        },
    );
}

#[cfg(test)]
mod tests {
    use super::{auto_selected_script_names, scanned_script_command_parts, ScannedScript};

    #[test]
    fn auto_selects_common_dev_scripts() {
        let scripts = vec![
            ScannedScript {
                name: "dev".to_string(),
                command: "vite".to_string(),
            },
            ScannedScript {
                name: "lint".to_string(),
                command: "eslint .".to_string(),
            },
            ScannedScript {
                name: "cargo run".to_string(),
                command: "cargo run".to_string(),
            },
        ];

        let selected = auto_selected_script_names(&scripts);
        assert_eq!(selected, vec!["cargo run".to_string(), "dev".to_string()]);
    }

    #[test]
    fn converts_cargo_script_to_program_and_args() {
        let script = ScannedScript {
            name: "cargo run --bin demo".to_string(),
            command: "cargo run --bin demo".to_string(),
        };

        let (program, args) = scanned_script_command_parts(&script);
        assert_eq!(program, "cargo");
        assert_eq!(args, vec!["run", "--bin", "demo"]);
    }
}
