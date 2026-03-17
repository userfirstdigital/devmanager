use crate::models::config::{
    DependencyStatus, RootScanEntry, ScanResult, ScannedPort, ScannedScript,
};
use crate::services::scanner_service;
use crate::state::AppState;
use regex::Regex;
use std::path::Path;
use std::sync::LazyLock;
use tauri::{AppHandle, Emitter, State};

/// Lazily compiled regex for matching port variables in .env files
static PORT_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(PORT|.*_PORT)\s*=\s*(\d+)").unwrap());

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
                message: "package.json has been modified since last install. Run npm install."
                    .to_string(),
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
    read_git_branch(&folder_path)
}

/// Read git branch by directly reading .git/HEAD (no subprocess)
pub fn read_git_branch(folder_path: &str) -> Result<Option<String>, String> {
    let git_head = Path::new(folder_path).join(".git").join("HEAD");
    match std::fs::read_to_string(&git_head) {
        Ok(content) => {
            let content = content.trim();
            if let Some(branch) = content.strip_prefix("ref: refs/heads/") {
                Ok(Some(branch.to_string()))
            } else {
                // Detached HEAD — return short hash
                Ok(Some(content.chars().take(8).collect()))
            }
        }
        Err(_) => Ok(None),
    }
}

#[tauri::command]
pub fn watch_git_branches(
    folder_paths: Vec<String>,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    use notify::{event::ModifyKind, Config, EventKind, RecursiveMode, Watcher};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    // Stop existing watcher
    {
        let mut watcher_guard = state.git_watcher.lock().map_err(|e| e.to_string())?;
        *watcher_guard = None;
    }

    if folder_paths.is_empty() {
        return Ok(());
    }

    // Build map of .git/HEAD path -> folder_path, and read initial branches
    let head_to_folder: Arc<Mutex<HashMap<std::path::PathBuf, String>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let last_branches: Arc<Mutex<HashMap<String, Option<String>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    let mut paths_to_watch = Vec::new();

    for folder_path in &folder_paths {
        let git_dir = Path::new(folder_path).join(".git");
        if git_dir.is_dir() {
            let head_path = git_dir.join("HEAD");
            head_to_folder
                .lock()
                .unwrap()
                .insert(head_path, folder_path.clone());
            paths_to_watch.push(git_dir);

            // Read initial branch
            let branch = read_git_branch(folder_path).unwrap_or(None);
            last_branches
                .lock()
                .unwrap()
                .insert(folder_path.clone(), branch);
        }
    }

    if paths_to_watch.is_empty() {
        return Ok(());
    }

    let h2f = head_to_folder.clone();
    let lb = last_branches.clone();
    let app_clone = app.clone();

    let mut watcher =
        notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
            if let Ok(event) = res {
                // Only care about data modifications
                match event.kind {
                    EventKind::Modify(ModifyKind::Data(_)) | EventKind::Modify(ModifyKind::Any) => {
                    }
                    _ => return,
                }

                let map = h2f.lock().unwrap();
                for path in &event.paths {
                    // Match on the HEAD file itself
                    let head_path = if path.ends_with("HEAD") {
                        path.clone()
                    } else {
                        continue;
                    };

                    if let Some(folder_path) = map.get(&head_path) {
                        let new_branch = read_git_branch(folder_path).unwrap_or(None);
                        let mut branches = lb.lock().unwrap();
                        let old_branch = branches.get(folder_path).cloned().flatten();

                        if new_branch != old_branch {
                            branches.insert(folder_path.clone(), new_branch.clone());
                            drop(branches);

                            #[derive(Clone, serde::Serialize)]
                            struct GitBranchChanged {
                                folder_path: String,
                                branch: Option<String>,
                            }

                            let _ = app_clone.emit(
                                "git-branch-changed",
                                GitBranchChanged {
                                    folder_path: folder_path.clone(),
                                    branch: new_branch,
                                },
                            );
                        }
                    }
                }
            }
        })
        .map_err(|e| format!("Failed to create file watcher: {}", e))?;

    watcher
        .configure(Config::default())
        .map_err(|e| format!("Failed to configure watcher: {}", e))?;

    for git_dir in &paths_to_watch {
        watcher
            .watch(git_dir, RecursiveMode::NonRecursive)
            .map_err(|e| format!("Failed to watch {}: {}", git_dir.display(), e))?;
    }

    // Store watcher to keep it alive
    let mut watcher_guard = state.git_watcher.lock().map_err(|e| e.to_string())?;
    *watcher_guard = Some(watcher);

    Ok(())
}

#[tauri::command]
pub fn unwatch_git_branches(state: State<'_, AppState>) -> Result<(), String> {
    let mut watcher_guard = state.git_watcher.lock().map_err(|e| e.to_string())?;
    *watcher_guard = None;
    Ok(())
}

#[tauri::command]
pub fn scan_root(root_path: String) -> Result<Vec<RootScanEntry>, String> {
    let root = Path::new(&root_path);
    if !root.is_dir() {
        return Err("Root path is not a directory".to_string());
    }

    let skip_dirs = [
        "node_modules",
        ".git",
        "dist",
        "build",
        "target",
        ".next",
        ".nuxt",
    ];
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

    // Check if this directory has a package.json or Cargo.toml
    let package_json = dir.join("package.json");
    let cargo_toml = dir.join("Cargo.toml");
    let has_pkg = package_json.exists();
    let has_cargo = cargo_toml.exists();

    if (has_pkg || has_cargo) && dir != root {
        let has_env = dir.join(".env").exists();
        let mut scripts = if has_pkg {
            read_package_scripts(&package_json)
        } else {
            Vec::new()
        };
        if has_cargo {
            scripts.extend(scanner_service::read_cargo_scripts(&cargo_toml));
        }
        let ports = scan_env_ports(dir);
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let project_type = match (has_pkg, has_cargo) {
            (true, true) => "both",
            (false, true) => "rust",
            _ => "node",
        }
        .to_string();

        entries.push(RootScanEntry {
            path: dir.to_string_lossy().to_string(),
            name,
            has_env,
            project_type,
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
    let mut ports = Vec::new();

    for env_file in &env_files {
        let env_path = dir.join(env_file);
        if let Ok(contents) = std::fs::read_to_string(&env_path) {
            for line in contents.lines() {
                if let Some(captures) = PORT_REGEX.captures(line) {
                    if let (Some(var_match), Some(port_match)) = (captures.get(1), captures.get(2))
                    {
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
    let mut result: Vec<ScannedScript> = scripts
        .iter()
        .map(|(name, cmd)| ScannedScript {
            name: name.clone(),
            command: cmd.as_str().unwrap_or("").to_string(),
        })
        .collect();

    // Expand bare "tauri" script into "tauri dev" and "tauri build"
    scanner_service::expand_tauri_scripts(&mut result);

    result
}
