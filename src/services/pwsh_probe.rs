use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Locates pwsh.exe from an explicit PATH string and ProgramFiles root.
/// Pure core so tests never depend on the host machine.
pub fn find_pwsh(path_var: Option<&OsStr>, program_files: Option<&Path>) -> Option<PathBuf> {
    if let Some(path_var) = path_var {
        for entry in std::env::split_paths(path_var) {
            let candidate = entry.join("pwsh.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    if let Some(program_files) = program_files {
        let conventional = program_files.join("PowerShell").join("7").join("pwsh.exe");
        if conventional.is_file() {
            return Some(conventional);
        }
    }
    None
}

/// Host-facing probe: PATH first, then %ProgramFiles%\PowerShell\7.
pub fn pwsh_program() -> Option<PathBuf> {
    find_pwsh(
        std::env::var_os("PATH").as_deref(),
        std::env::var_os("ProgramFiles")
            .map(PathBuf::from)
            .as_deref(),
    )
}

#[cfg(test)]
mod tests {
    use super::find_pwsh;
    use std::fs;

    #[test]
    fn finds_pwsh_on_path() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("pwsh.exe");
        fs::write(&exe, b"").unwrap();
        let found = find_pwsh(Some(dir.path().as_os_str()), None).unwrap();
        assert_eq!(found, exe);
    }

    #[test]
    fn falls_back_to_program_files() {
        let dir = tempfile::tempdir().unwrap();
        let seven = dir.path().join("PowerShell").join("7");
        fs::create_dir_all(&seven).unwrap();
        let exe = seven.join("pwsh.exe");
        fs::write(&exe, b"").unwrap();
        let found = find_pwsh(None, Some(dir.path())).unwrap();
        assert_eq!(found, exe);
    }

    #[test]
    fn absent_everywhere_is_none() {
        let empty = tempfile::tempdir().unwrap();
        assert!(find_pwsh(Some(empty.path().as_os_str()), Some(empty.path())).is_none());
    }
}
