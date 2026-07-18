# PowerShell 7 (pwsh) Shell Option Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add PowerShell 7 (`pwsh`) as a selectable default terminal shell on Windows, disabled with a warning when pwsh is not installed.

**Architecture:** Extend the `DefaultTerminal` enum with a `Pwsh` variant, add a cached availability probe (`pwsh_program()`), gate selection in the settings UI, and map the variant to the resolved `pwsh.exe` at launch with a `powershell.exe` fallback.

**Tech Stack:** Rust, serde, GPUI settings UI (existing patterns in `src/workspace/mod.rs`).

## Global Constraints

- Spec: `docs/superpowers/specs/2026-07-17-pwsh-shell-option-design.md`
- Serialized value must be exactly `"pwsh"` (existing `rename_all = "lowercase"`).
- UI label must be exactly `PowerShell 7 (pwsh)`.
- Never silently rewrite the stored setting; never fail a terminal launch because pwsh is missing (fall back to `powershell.exe` with a session warning).
- Run tests with `cargo test` from the repo root; the project must stay clippy-clean (`cargo clippy --all-targets`).

---

### Task 1: `DefaultTerminal::Pwsh` model variant

**Files:**
- Modify: `src/models/config.rs:89-101` (enum `DefaultTerminal`)
- Test: same file, `#[cfg(test)]` module (add one if the file has none near the bottom)

**Interfaces:**
- Produces: `DefaultTerminal::Pwsh`, serialized `"pwsh"`. All later tasks match on this variant.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod default_terminal_tests {
    use super::DefaultTerminal;

    #[test]
    fn pwsh_round_trips_as_lowercase() {
        let json = serde_json::to_string(&DefaultTerminal::Pwsh).unwrap();
        assert_eq!(json, "\"pwsh\"");
        let back: DefaultTerminal = serde_json::from_str("\"pwsh\"").unwrap();
        assert_eq!(back, DefaultTerminal::Pwsh);
    }

    #[test]
    fn legacy_values_still_deserialize() {
        for (raw, expected) in [
            ("\"bash\"", DefaultTerminal::Bash),
            ("\"powershell\"", DefaultTerminal::Powershell),
            ("\"cmd\"", DefaultTerminal::Cmd),
        ] {
            let parsed: DefaultTerminal = serde_json::from_str(raw).unwrap();
            assert_eq!(parsed, expected);
        }
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test default_terminal_tests`
Expected: compile error — `Pwsh` not found in `DefaultTerminal`.

- [ ] **Step 3: Add the variant**

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DefaultTerminal {
    Bash,
    Powershell,
    Pwsh,
    Cmd,
}
```

Fix every non-UI exhaustive `match` the compiler reports by treating `Pwsh` like `Powershell` **only where behavior is identical**; leave `src/workspace/mod.rs` and `src/services/process_manager.rs` matches for Tasks 3-4 (add a temporary `DefaultTerminal::Pwsh => ...` arm mirroring `Powershell` so the build passes; Tasks 3-4 replace them).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test default_terminal_tests`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add src/models/config.rs src/workspace/mod.rs src/services/process_manager.rs
git commit -m "feat: add DefaultTerminal::Pwsh model variant"
```

---

### Task 2: pwsh availability probe

**Files:**
- Create: `src/services/pwsh_probe.rs`
- Modify: `src/services/mod.rs` (add `pub mod pwsh_probe;`)
- Test: inline `#[cfg(test)]` in the new file

**Interfaces:**
- Produces: `pub fn pwsh_program() -> Option<std::path::PathBuf>` — resolved `pwsh.exe`, or `None` when not installed. Also `pub fn find_pwsh(path_var: Option<&std::ffi::OsStr>, program_files: Option<&std::path::Path>) -> Option<std::path::PathBuf>` (pure, testable core).

- [ ] **Step 1: Write the failing tests**

```rust
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
```

(If `tempfile` is not already a dev-dependency in `Cargo.toml`, add `tempfile = "3"` under `[dev-dependencies]`; check first — the codex_bridge tests likely already use it.)

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test pwsh_probe`
Expected: compile error — module/function not found.

- [ ] **Step 3: Implement**

```rust
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
        std::env::var_os("ProgramFiles").map(PathBuf::from).as_deref(),
    )
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test pwsh_probe`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add src/services/pwsh_probe.rs src/services/mod.rs Cargo.toml
git commit -m "feat: add pwsh availability probe"
```

---

### Task 3: settings UI — option, warning, selection gating

**Files:**
- Modify: `src/workspace/mod.rs:1594-1608` (`next_default_terminal`, `default_terminal_label`), the options list near `src/workspace/mod.rs:1757`, and the settings-row render near `src/workspace/mod.rs:1822-1830`
- Test: existing test module in `src/workspace/mod.rs`

**Interfaces:**
- Consumes: `crate::services::pwsh_probe::pwsh_program()` (Task 2), `DefaultTerminal::Pwsh` (Task 1).
- Produces: `pub fn next_default_terminal_with_availability(current: DefaultTerminal, pwsh_available: bool) -> DefaultTerminal` and `pub fn default_terminal_label(value: &DefaultTerminal) -> &'static str` returning `"PowerShell 7 (pwsh)"` for `Pwsh`. The editor state caches `pwsh_available: bool`, computed once when the settings editor opens.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn cycle_includes_pwsh_when_available() {
    assert_eq!(
        next_default_terminal_with_availability(DefaultTerminal::Powershell, true),
        DefaultTerminal::Pwsh
    );
    assert_eq!(
        next_default_terminal_with_availability(DefaultTerminal::Pwsh, true),
        DefaultTerminal::Cmd
    );
}

#[test]
fn cycle_skips_pwsh_when_unavailable() {
    assert_eq!(
        next_default_terminal_with_availability(DefaultTerminal::Powershell, false),
        DefaultTerminal::Cmd
    );
}

#[test]
fn pwsh_label() {
    assert_eq!(default_terminal_label(&DefaultTerminal::Pwsh), "PowerShell 7 (pwsh)");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test cycle_includes_pwsh cycle_skips_pwsh pwsh_label`
Expected: compile error — function not found.

- [ ] **Step 3: Implement**

```rust
pub fn next_default_terminal_with_availability(
    current: DefaultTerminal,
    pwsh_available: bool,
) -> DefaultTerminal {
    let next = match current {
        DefaultTerminal::Bash => DefaultTerminal::Powershell,
        DefaultTerminal::Powershell => DefaultTerminal::Pwsh,
        DefaultTerminal::Pwsh => DefaultTerminal::Cmd,
        DefaultTerminal::Cmd => DefaultTerminal::Bash,
    };
    if next == DefaultTerminal::Pwsh && !pwsh_available {
        DefaultTerminal::Cmd
    } else {
        next
    }
}
```

Keep `next_default_terminal(current)` as a thin wrapper calling the new function with `pwsh_available: true` if other callers exist; update the settings editor call site to pass the cached availability. Update `default_terminal_label` with `DefaultTerminal::Pwsh => "PowerShell 7 (pwsh)"`.

Then wire the UI following the file's existing row patterns:
1. When constructing the settings editor state (where `dependency_status`-style fields live, `src/workspace/mod.rs:1298` area), add `pwsh_available: bool`, initialized `crate::services::pwsh_probe::pwsh_program().is_some()` on editor open.
2. In the option list at `src/workspace/mod.rs:1757`, add `DefaultTerminal::Pwsh` between `Powershell` and `Cmd`. Render it disabled (existing disabled-row styling used elsewhere in the settings pane) with warning copy `"PowerShell 7 is not installed"` when `!pwsh_available`.
3. In the `EditorAction::SelectDefaultTerminal`/`CycleDefaultTerminal` handlers (`src/workspace/mod.rs:1460-1465` actions), reject `SelectDefaultTerminal(DefaultTerminal::Pwsh)` when `!pwsh_available`: do not change the draft; surface the same warning copy via the pane's existing inline notice mechanism. Use `next_default_terminal_with_availability` for the cycle handler.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test cycle_includes_pwsh cycle_skips_pwsh pwsh_label`
Expected: PASS. Also run `cargo clippy --all-targets` — no new warnings.

- [ ] **Step 5: Commit**

```bash
git add src/workspace/mod.rs
git commit -m "feat: settings option for PowerShell 7 with install check"
```

---

### Task 4: launch mapping + fallback

**Files:**
- Modify: `src/services/process_manager.rs:4533-4545` (`build_interactive_shell_command`)
- Test: existing test module in `src/services/process_manager.rs`

**Interfaces:**
- Consumes: `crate::services::pwsh_probe::pwsh_program()` (Task 2).
- Produces: `fn build_interactive_shell_command(settings: &Settings) -> (String, Vec<String>)` mapping `Pwsh` → resolved pwsh path or `powershell.exe` fallback. Extract the testable core as `fn windows_shell_for(terminal: &DefaultTerminal, shell_integration: bool, pwsh: Option<PathBuf>) -> (String, Vec<String>)`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn pwsh_maps_to_resolved_path() {
    let (program, args) = windows_shell_for(
        &crate::models::DefaultTerminal::Pwsh,
        false,
        Some(std::path::PathBuf::from(r"C:\Program Files\PowerShell\7\pwsh.exe")),
    );
    assert_eq!(program, r"C:\Program Files\PowerShell\7\pwsh.exe");
    assert!(args.is_empty());
}

#[test]
fn pwsh_missing_falls_back_to_windows_powershell() {
    let (program, _) = windows_shell_for(&crate::models::DefaultTerminal::Pwsh, false, None);
    assert_eq!(program, "powershell.exe");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test pwsh_maps pwsh_missing`
Expected: compile error — `windows_shell_for` not found.

- [ ] **Step 3: Implement**

```rust
fn windows_shell_for(
    terminal: &crate::models::DefaultTerminal,
    shell_integration: bool,
    pwsh: Option<PathBuf>,
) -> (String, Vec<String>) {
    match terminal {
        crate::models::DefaultTerminal::Powershell => ("powershell.exe".to_string(), Vec::new()),
        crate::models::DefaultTerminal::Pwsh => match pwsh {
            Some(path) => (path.to_string_lossy().into_owned(), Vec::new()),
            // Selected pwsh but it is gone (uninstalled, hand-edited config):
            // degrade to Windows PowerShell rather than failing the launch.
            None => ("powershell.exe".to_string(), Vec::new()),
        },
        crate::models::DefaultTerminal::Cmd => ("cmd.exe".to_string(), Vec::new()),
        crate::models::DefaultTerminal::Bash => (
            preferred_windows_bash_program(),
            bash_shell_args(shell_integration),
        ),
    }
}
```

In `build_interactive_shell_command`, replace the Windows match with `return windows_shell_for(&settings.default_terminal, settings.shell_integration_enabled, crate::services::pwsh_probe::pwsh_program());`. When the fallback branch is taken, log the warning through the same mechanism the function's callers use for session status (search for how launch warnings surface near `src/services/process_manager.rs:4509`; if none exists, `eprintln!` is not acceptable — attach the warning to the returned session state the way `state.shell_program` is recorded at `:2275`).

Also update the non-Windows `match settings.default_terminal` at `src/services/process_manager.rs:4555-4564`: `Pwsh` falls into the existing `_ => resolve_shell_path(settings)` arm — verify no exhaustive-match break.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test pwsh_maps pwsh_missing` then the full suite `cargo test`
Expected: PASS, no regressions.

- [ ] **Step 5: Commit**

```bash
git add src/services/process_manager.rs
git commit -m "feat: launch pwsh terminals with fallback to Windows PowerShell"
```

---

### Task 5: manual QA

- [ ] **Step 1:** `cargo run`, open Settings → verify "PowerShell 7 (pwsh)" appears and is selectable (pwsh is installed on this machine); select it, open a terminal, run `$PSVersionTable.PSVersion` → major version 7.
- [ ] **Step 2:** Simulate absence: temporarily rename `pwsh.exe` out of PATH scope is impractical — instead verify the disabled path via the unit tests plus a manual run with `find_pwsh` forced to `None` (temporary local edit, not committed), confirming the option renders disabled with "PowerShell 7 is not installed" and selection is rejected.
- [ ] **Step 3:** Confirm a config hand-edited to `"defaultTerminal": "pwsh"` on a machine without pwsh still opens terminals (fallback) — covered by `pwsh_missing_falls_back_to_windows_powershell`, spot-check by launching with the temporary `None` edit from Step 2.
