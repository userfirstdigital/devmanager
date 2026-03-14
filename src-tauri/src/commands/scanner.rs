use crate::models::config::{ScanResult, DependencyStatus, RootScanEntry, ScannedScript, ScannedPort};
use crate::services::scanner_service;
use regex::Regex;
use std::path::Path;

#[tauri::command]
pub fn scan_project(folder_path: String) -> Result<ScanResult, String> {
    scanner_service::scan_directory(&folder_path)
}

#[tauri::command]
pub fn check_dependencies(folder_path: String) -> Result<DependencyStatus, String> {
    let base = Path::new(&folder_path);
    let node_modules = base.join("node_modules");

    if !node_modules.exists() {
        return Ok(DependencyStatus {
            status: "missing".to_string(),
            message: "node_modules directory not found. Run npm install.".to_string(),
        });
    }

    // Compare package.json mtime vs node_modules/.package-lock.json mtime
    let package_json = base.join("package.json");
    let package_lock_in_modules = node_modules.join(".package-lock.json");

    if package_json.exists() && package_lock_in_modules.exists() {
        let pkg_modified = package_json
            .metadata()
            .and_then(|m| m.modified())
            .map_err(|e| format!("Failed to get package.json mtime: {}", e))?;
        let lock_modified = package_lock_in_modules
            .metadata()
            .and_then(|m| m.modified())
            .map_err(|e| format!("Failed to get .package-lock.json mtime: {}", e))?;

        if pkg_modified > lock_modified {
            return Ok(DependencyStatus {
                status: "outdated".to_string(),
                message: "package.json has been modified since last install. Run npm install.".to_string(),
            });
        }
    }

    Ok(DependencyStatus {
        status: "ok".to_string(),
        message: "Dependencies are up to date.".to_string(),
    })
}

#[tauri::command]
pub fn get_git_branch(folder_path: String) -> Result<Option<String>, String> {
    let output = std::process::Command::new("git")
        .args(["-C", &folder_path, "rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .map_err(|e| format!("Failed to run git: {}", e))?;

    if output.status.success() {
        let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(Some(branch))
    } else {
        Ok(None)
    }
}

#[tauri::command]
pub fn scan_root(root_path: String) -> Result<Vec<RootScanEntry>, String> {
    let root = Path::new(&root_path);
    if !root.is_dir() {
        return Err("Root path is not a directory".to_string());
    }

    let skip_dirs = ["node_modules", ".git", "dist", "build", "target", ".next", ".nuxt"];
    let mut entries = Vec::new();

    scan_dir_recursive(root, root, 0, 3, &skip_dirs, &mut entries);

    Ok(entries)
}

fn scan_dir_recursive(
    root: &Path,
    dir: &Path,
    depth: u32,
    max_depth: u32,
    skip_dirs: &[&str],
    entries: &mut Vec<RootScanEntry>,
) {
    if depth > max_depth {
        return;
    }

    // Check if this directory has a package.json
    let package_json = dir.join("package.json");
    if package_json.exists() && dir != root {
        let has_env = dir.join(".env").exists();
        let scripts = read_package_scripts(&package_json);
        let ports = scan_env_ports(dir);
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        entries.push(RootScanEntry {
            path: dir.to_string_lossy().to_string(),
            name,
            has_env,
            scripts,
            ports,
        });
    }

    // Recurse into subdirectories
    if let Ok(read_dir) = std::fs::read_dir(dir) {
        for entry in read_dir.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let dir_name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                if !skip_dirs.contains(&dir_name.as_str()) && !dir_name.starts_with('.') {
                    scan_dir_recursive(root, &path, depth + 1, max_depth, skip_dirs, entries);
                }
            }
        }
    }
}

fn scan_env_ports(dir: &Path) -> Vec<ScannedPort> {
    let env_files = [".env", ".env.local", ".env.development"];
    let port_regex = match Regex::new(r"(?i)^(PORT|.*_PORT)\s*=\s*(\d+)") {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let mut ports = Vec::new();

    for env_file in &env_files {
        let env_path = dir.join(env_file);
        if let Ok(contents) = std::fs::read_to_string(&env_path) {
            for line in contents.lines() {
                if let Some(captures) = port_regex.captures(line) {
                    if let (Some(var_match), Some(port_match)) = (captures.get(1), captures.get(2)) {
                        if let Ok(port) = port_match.as_str().parse::<u16>() {
                            ports.push(ScannedPort {
                                variable: var_match.as_str().to_string(),
                                port,
                                source: env_file.to_string(),
                            });
                        }
                    }
                }
            }
        }
    }

    ports
}

fn read_package_scripts(package_json: &Path) -> Vec<ScannedScript> {
    let content = match std::fs::read_to_string(package_json) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let json: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let scripts = match json.get("scripts").and_then(|s| s.as_object()) {
        Some(s) => s,
        None => return Vec::new(),
    };
    scripts
        .iter()
        .map(|(name, cmd)| ScannedScript {
            name: name.clone(),
            command: cmd.as_str().unwrap_or("").to_string(),
        })
        .collect()
}
