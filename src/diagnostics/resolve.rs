//! PATH / PATHEXT executable resolution for diagnostics probes.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Resolve all PATH hits for `name`, honoring Windows PATHEXT order.
/// Same-directory `.ps1` shims are omitted when a runnable PATHEXT wrapper exists.
pub fn resolve_all_on_path(name: &str, path_var: Option<&OsStr>, path_ext: &str) -> Vec<PathBuf> {
    let Some(path_var) = path_var else {
        return Vec::new();
    };
    let windows = cfg!(windows);
    let names = candidate_names(name, windows, path_ext);
    let mut found = Vec::new();
    for directory in std::env::split_paths(path_var) {
        let mut dir_hits: Vec<PathBuf> = Vec::new();
        for candidate_name in &names {
            let candidate = directory.join(candidate_name);
            if candidate.is_file() {
                dir_hits.push(candidate);
            }
        }
        if let Some(preferred) = prefer_runnable_in_dir(&dir_hits) {
            if !found.iter().any(|p| p == &preferred) {
                found.push(preferred);
            }
        }
    }
    found
}

pub fn resolve_all(name: &str) -> Vec<PathBuf> {
    let path_ext = std::env::var("PATHEXT").unwrap_or_else(|_| {
        if cfg!(windows) {
            ".COM;.EXE;.BAT;.CMD".to_string()
        } else {
            String::new()
        }
    });
    resolve_all_on_path(name, std::env::var_os("PATH").as_deref(), &path_ext)
}

/// Collapse paths that live in the same directory into one installation representative.
pub fn collapse_same_directory_installs(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut by_dir: BTreeMap<PathBuf, PathBuf> = BTreeMap::new();
    for path in paths {
        let dir = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        by_dir
            .entry(dir)
            .and_modify(|existing| {
                if prefer_runnable_pair(existing, path) == std::cmp::Ordering::Greater {
                    *existing = path.clone();
                }
            })
            .or_insert_with(|| path.clone());
    }
    by_dir.into_values().collect()
}

pub fn candidate_names(program: &str, windows: bool, path_ext: &str) -> Vec<String> {
    let mut names = Vec::new();
    if windows && Path::new(program).extension().is_none() {
        for extension in path_ext
            .split(';')
            .filter(|extension| !extension.is_empty())
        {
            let lower = extension.to_ascii_lowercase();
            // Prefer PATHEXT order; skip .ps1 in the primary candidate list so shell
            // shims lose to .cmd/.exe wrappers when both exist.
            if lower == ".ps1" {
                continue;
            }
            names.push(format!("{program}{}", extension.to_ascii_lowercase()));
            names.push(format!("{program}{}", extension.to_ascii_uppercase()));
        }
        // Bare name last; .ps1 only as a last resort when nothing else exists.
        names.push(program.to_string());
        names.push(format!("{program}.ps1"));
        names.push(format!("{program}.PS1"));
    } else {
        names.push(program.to_string());
    }
    names
}

fn prefer_runnable_in_dir(hits: &[PathBuf]) -> Option<PathBuf> {
    if hits.is_empty() {
        return None;
    }
    let mut best = hits[0].clone();
    for hit in &hits[1..] {
        if prefer_runnable_pair(&best, hit) == std::cmp::Ordering::Greater {
            best = hit.clone();
        }
    }
    Some(best)
}

fn prefer_runnable_pair(a: &Path, b: &Path) -> std::cmp::Ordering {
    runnable_rank(a).cmp(&runnable_rank(b))
}

fn runnable_rank(path: &Path) -> u8 {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "com" => 0,
        "exe" => 1,
        "bat" => 2,
        "cmd" => 3,
        "ps1" => 9,
        "" => 5,
        _ => 6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::fs;

    #[test]
    fn pathext_order_prefers_cmd_over_ps1_in_same_dir() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("npm.ps1"), b"").unwrap();
        fs::write(dir.path().join("npm.cmd"), b"").unwrap();
        let path = OsString::from(dir.path());
        let found = resolve_all_on_path("npm", Some(&path), ".COM;.EXE;.BAT;.CMD;.PS1");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].extension().and_then(|s| s.to_str()), Some("cmd"));
    }

    #[test]
    fn same_directory_wrappers_collapse_for_path_consistency() {
        let collapsed = collapse_same_directory_installs(&[
            PathBuf::from(r"C:\nvm\npm.cmd"),
            PathBuf::from(r"C:\nvm\npm.ps1"),
            PathBuf::from(r"C:\Program Files\nodejs\npm.cmd"),
        ]);
        assert_eq!(collapsed.len(), 2);
        assert!(collapsed.iter().any(|p| p.ends_with("npm.cmd")));
        assert!(!collapsed.iter().any(|p| {
            p.extension()
                .and_then(|s| s.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("ps1"))
        }));
    }

    #[test]
    fn candidate_names_skip_ps1_until_last_resort() {
        let names = candidate_names("node", true, ".COM;.EXE;.BAT;.CMD;.PS1");
        let exe = names
            .iter()
            .position(|n| n.eq_ignore_ascii_case("node.exe"));
        let ps1 = names
            .iter()
            .position(|n| n.eq_ignore_ascii_case("node.ps1"));
        assert!(exe.unwrap() < ps1.unwrap());
    }
}
