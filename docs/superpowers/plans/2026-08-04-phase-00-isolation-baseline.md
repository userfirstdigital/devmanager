# Phase 0: Isolation and Baseline Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Establish a fail-closed replacement-development environment and reproducible evidence proving that builds, tests, preview binaries, browsers, ports, and cleanup cannot touch the installed DevManager.

**Architecture:** All replacement work runs from `.worktrees/native-gpui-kernel` on `codex/native-gpui-kernel`, with the explicit profile `native-next-dev`, instance label `Next`, dedicated target/live directories, and ignored evidence files. Shared PowerShell guards capture and compare production hashes and process identity around every risky gate; Rust path resolution remains fail-closed in tests. Real lifecycle, conformance artifacts, and measurements begin only once Phase 2 has a real host/client surface.

**Tech Stack:** PowerShell 7, Rust 1.94.0, existing `dirs`/persistence code, Windows CIM/process APIs, SHA-256, Git worktrees.

## Global Constraints

- Do not start, stop, restart, install over, attach a debugger to, or send input to the installed DevManager.
- Production storage is the unprofiled `%APPDATA%\com.userfirst.devmanager` tree; tests must never resolve beneath it.
- `config.json` and `remote.json` hashes plus installed PID/start time are the protected invariants. `session.json` is observed only as a path and never read or hashed because the installed app may legitimately update it.
- Use `DEVMANAGER_PROFILE=native-next-dev` and `DEVMANAGER_INSTANCE_LABEL=Next` for every replacement binary.
- Exception for the complete `cargo test --lib` recipe: explicitly remove any inherited `DEVMANAGER_PROFILE`; profile-sensitive unit tests own and restore that process-global variable under the required serial runner.
- Use `target-native-next` and `target-live-native-next`; never copy a development executable into the installed location.
- Every script fails closed on unresolved paths, ambiguous executable identity, malformed evidence, or missing expected profile variables.
- This phase changes development tooling and path policy only; it does not introduce the new kernel, launch lifecycle, or provider/browser work.

---

## Phase entry

- Approved architecture revision `ded903c` is present.
- Main worktree is clean except for committed plan documents.
- Use `superpowers:using-git-worktrees` to create `.worktrees/native-gpui-kernel` on `codex/native-gpui-kernel`.
- Record `git worktree list --porcelain` before and after creation.

## File map

- Create: `src/config/mod.rs` — replacement configuration namespace.
- Create: `src/config/paths.rs` — profile parsing and resolved storage/build identity.
- Modify: `src/lib.rs` — export `config`.
- Modify: `src/persistence/mod.rs` — delegate path calculation to `config::paths` without changing production behavior.
- Create: `tests/development_isolation.rs` — path/profile fail-closed contract.
- Create: `scripts/native-next/Isolation.ps1` — shared path, process, hash, and evidence functions.
- Create: `scripts/native-next/Capture-ProductionBaseline.ps1` — read-only baseline JSON.
- Create: `scripts/native-next/Assert-ProductionUnchanged.ps1` — fail-closed comparator.
- Create: `scripts/native-next/Start-NativeNext.ps1` — Phase 0 ValidateOnly isolation scaffold (real launch deferred to Phase 2).
- Create: `scripts/native-next/Stop-NativeNext.ps1` — Phase 0 ValidateOnly isolation scaffold (real stop deferred to Phase 2; zero-orphan to Phase 3).
- Create: `scripts/native-next/NativeNext.ps1` — shared path/env validation library for the scaffold.
- Create: `scripts/native-next/Invoke-PhaseGate.ps1` — exact recipe execution plus before/after evidence; observe/fail-closed residue (no kill).
- Create: `scripts/native-next/PhaseGate.ps1` — recipe table, observation, and phase-gate helpers.
- Create: `docs/replacement-deletion-ledger.md` — old-path ownership and deletion criteria.
- Modify: `.gitignore` — ignore development output/evidence.

### Task 0.1: Introduce one typed profile/path contract

**Files:**
- Create: `src/config/mod.rs`
- Create: `src/config/paths.rs`
- Modify: `src/lib.rs`
- Modify: `src/persistence/mod.rs`
- Test: `tests/development_isolation.rs`

**Interfaces:**
- Produces: `AppProfile`, `ResolvedAppPaths`, `resolve_app_paths(base, profile, build_kind)`.
- Preserves: unprofiled production path and current named-profile path spelling.
- Consumed by: every later binary, database, browser profile, log, and evidence location.

- [ ] **Step 1: Write the failing path contract tests**

```rust
use devmanager::config::paths::{resolve_app_paths, AppProfile, BuildKind};

#[test]
fn native_next_profile_cannot_alias_production() {
    let base = std::path::Path::new(r"C:\Users\tester\AppData\Roaming");
    let production = resolve_app_paths(base, AppProfile::Production, BuildKind::Release).unwrap();
    let next = resolve_app_paths(
        base,
        AppProfile::named("native-next-dev").unwrap(),
        BuildKind::Debug,
    )
    .unwrap();

    assert_eq!(production.root, base.join("com.userfirst.devmanager"));
    assert_eq!(next.root, base.join("com.userfirst.devmanager-native-next-dev"));
    assert!(!next.root.starts_with(&production.root));
    assert_eq!(next.database, next.root.join("kernel.sqlite3"));
    assert_eq!(next.browser_root, next.root.join("browser"));
}

#[test]
fn named_profile_rejects_empty_or_path_shaped_values() {
    for invalid in ["", "..", r"a\b", "a/b", "native next"] {
        assert!(AppProfile::named(invalid).is_err(), "accepted {invalid:?}");
    }
}
```

- [ ] **Step 2: Run the focused test and observe the intended failure**

Run: `cargo test --test development_isolation -- --test-threads=1`

Expected: compilation fails because `devmanager::config::paths` does not exist.

- [ ] **Step 3: Implement the typed path contract**

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppProfile {
    Production,
    Named(String),
    UnitTest(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildKind {
    Debug,
    Release,
    Test,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAppPaths {
    pub root: std::path::PathBuf,
    pub config: std::path::PathBuf,
    pub remote: std::path::PathBuf,
    pub database: std::path::PathBuf,
    pub browser_root: std::path::PathBuf,
    pub logs: std::path::PathBuf,
}
```

`AppProfile::named` accepts only ASCII alphanumeric characters, `-`, and `_`, lowercases them, and rejects rather than rewrites every other character. `resolve_app_paths` must assert that `BuildKind::Test` receives `UnitTest`, and that `Debug` never receives `Production` unless the caller passes an explicit `allow_production_debug` test-only seam unavailable to normal binaries.

Update `persistence::app_config_dir()` to call this contract. Preserve the current production/named directory names exactly.

- [ ] **Step 4: Run focused and persistence tests**

Run:

```powershell
cargo test --test development_isolation -- --test-threads=1
cargo test --lib persistence:: -- --test-threads=1
```

Expected: both commands pass; test paths are beneath a process-unique temp root.

- [ ] **Step 5: Review and commit**

```powershell
git diff --check
git add src/config src/lib.rs src/persistence/mod.rs tests/development_isolation.rs
git commit -m "refactor: centralize isolated app paths"
```

### Task 0.2: Add read-only production baseline and comparison scripts

**Files:**
- Create: `scripts/native-next/Isolation.ps1`
- Create: `scripts/native-next/Capture-ProductionBaseline.ps1`
- Create: `scripts/native-next/Assert-ProductionUnchanged.ps1`
- Test: `tests/development_isolation.rs`

**Interfaces:**
- Produces: `.devmanager-next/evidence/current/baseline.json`.
- Produces: `Get-DevManagerProductionState`, `Write-DevManagerBaseline`, `Assert-DevManagerProductionState`.
- Consumes: exact production root from `dirs`-equivalent Windows Roaming AppData and exact installed executable path discovered through CIM.

- [ ] **Step 1: Add a failing script-contract test**

```rust
#[test]
fn isolation_scripts_protect_only_the_unprofiled_installation() {
    let library = std::fs::read_to_string("scripts/native-next/Isolation.ps1").unwrap();
    assert!(library.contains("Get-DevManagerProductionState"));
    assert!(library.contains("config.json"));
    assert!(library.contains("remote.json"));
    assert!(!library.contains("Get-FileHash $sessionPath"));
    assert!(library.contains("Win32_Process"));
    assert!(library.contains("CreationDate"));
}
```

- [ ] **Step 2: Run the test and confirm it fails because the script is absent**

Run: `cargo test --test development_isolation isolation_scripts_protect_only_the_unprofiled_installation -- --exact`

- [ ] **Step 3: Implement the scripts with fail-closed JSON evidence**

`Get-DevManagerProductionState` returns this exact shape:

```powershell
[pscustomobject]@{
    schemaVersion = 1
    capturedAtUtc = [DateTime]::UtcNow.ToString("o")
    productionRoot = $productionRoot
    config = Get-ProtectedFileState -LiteralPath (Join-Path $productionRoot "config.json")
    remote = Get-ProtectedFileState -LiteralPath (Join-Path $productionRoot "remote.json")
    sessionPath = Join-Path $productionRoot "session.json"
    installedProcesses = @($installedProcesses | ForEach-Object {
        [pscustomobject]@{
            processId = [uint32]$_.ProcessId
            executablePath = [string]$_.ExecutablePath
            creationDate = [string]$_.CreationDate
        }
    })
}
```

`Get-ProtectedFileState` records `exists`, `length`, and SHA-256 when present. It never reads `session.json`. Installed process matching must compare resolved executable paths against Windows installed locations, never process name alone. Comparison fails on any hash, PID/start-time, ambiguity, missing baseline field, or root mismatch.

- [ ] **Step 4: Exercise the scripts without mutating production**

Run:

```powershell
pwsh -NoProfile -File scripts/native-next/Capture-ProductionBaseline.ps1 -OutputPath .devmanager-next/evidence/current/baseline.json
pwsh -NoProfile -File scripts/native-next/Assert-ProductionUnchanged.ps1 -BaselinePath .devmanager-next/evidence/current/baseline.json
cargo test --test development_isolation isolation_scripts_protect_only_the_unprofiled_installation -- --exact
```

Expected: baseline capture reports the protected paths; comparison passes immediately; Rust contract test passes.

- [ ] **Step 5: Review and commit**

```powershell
git add scripts/native-next tests/development_isolation.rs
git commit -m "chore: guard installed DevManager during development"
```

### Task 0.3: Add the isolated validation scaffold (not a real launcher)

**Files:**
- Create: `scripts/native-next/NativeNext.ps1`
- Create: `scripts/native-next/Start-NativeNext.ps1`
- Create: `scripts/native-next/Stop-NativeNext.ps1`
- Modify: `.gitignore`
- Test: `tests/development_isolation.rs`

**Interfaces:**
- Produces: worktree-derived fully-qualified plans for `target-native-next`, `target-live-native-next`, `.devmanager-next/runtime.json` (path only), and evidence roots.
- Sets (plan only): `DEVMANAGER_PROFILE=native-next-dev`, `DEVMANAGER_INSTANCE_LABEL=Next`, `DEVMANAGER_RUNTIME_KIND=native-next`.
- Phase 0 boundary: real build/copy/start/stop/kill/ctl/runtime IO is unavailable until Phase 2; zero-orphan acceptance waits for Phase 3 Job Object proof.

- [ ] **Step 1: Add failing scaffold contract tests**

Assert wrappers expose only `-ValidateOnly`; `NativeNext.ps1` plans exact contained paths/env; speculative process/cargo/runtime helpers are absent; non-ValidateOnly refuses before Capture/Assert; ValidateOnly invokes Capture+Assert only.

- [ ] **Step 2: Run focused tests and observe RED against any overbuilt lifecycle harness**

Run: `cargo test --test development_isolation next_launcher_validation_scaffold_is_lean_and_forbids_lifecycle -- --exact`

- [ ] **Step 3: Implement the ValidateOnly scaffold**

`Start-NativeNext.ps1` / `Stop-NativeNext.ps1`:

1. Without `-ValidateOnly`, throw immediately that real lifecycle is deferred to Phase 2 (before Capture/Assert or other IO).
2. With `-ValidateOnly`, derive/validate the isolated path+env plan, capture protected production state, compare it, and return success.
3. May write only protected evidence under `/.devmanager-next/evidence`.
4. Must never build, copy, start, stop, kill, invoke ctl, or create/read/write/remove `runtime.json`.

Ignore anchors: `/.devmanager-next/`, `/target-native-next/`, `/target-live-native-next/`.

- [ ] **Step 4: Exercise ValidateOnly through Codex-owned machine review** (worker runs synthetic/copied harness only)

Expected: ValidateOnly passes; no process is started; non-ValidateOnly refuses closed.

- [ ] **Step 5: Review and commit**

```powershell
git add .gitignore scripts/native-next tests/development_isolation.rs
git commit -m "chore: add isolated native-next validation scaffold"
```

### Task 0.4: Add one guarded phase-gate runner

**Files:**
- Create: `scripts/native-next/Invoke-PhaseGate.ps1`
- Create: `scripts/native-next/PhaseGate.ps1`
- Modify: `scripts/native-next/Isolation.ps1`
- Test: `tests/development_isolation.rs`

**Interfaces:**
- Produces: unique per-run phase evidence with recipe, exit code, duration, before/after process inventories, and cleanup result.
- Accepts: `-Phase`, `-Recipe`, `-LongRustRun`.
- Never accepts `-Command`/`-Arguments`, a production profile, or broad kill target.
- Phase 0 recipes are an exact closed Cargo set: `cargo-version`, `cargo-fmt-check`, `development-isolation-tests`, and `library-tests-serial`.
- Integration-test recipes force `native-next-dev`; `library-tests-serial` explicitly removes inherited `DEVMANAGER_PROFILE` while retaining the isolated target/instance/runtime environment.

- [ ] **Step 1: Write the failing gate contract test**

```rust
#[test]
fn phase_gate_wraps_commands_with_baseline_and_cleanup_checks() {
    let source = std::fs::read_to_string("scripts/native-next/Invoke-PhaseGate.ps1").unwrap();
    for required in [
        "Capture-ProductionBaseline.ps1",
        "Assert-ProductionUnchanged.ps1",
        "processes-before.json",
        "processes-after.json",
        "verification.json",
        "LongRustRun",
    ] {
        assert!(source.contains(required), "missing {required}");
    }
}
```

- [ ] **Step 2: Run the test and observe the expected failure**

Run: `cargo test --test development_isolation phase_gate_wraps_commands_with_baseline_and_cleanup_checks -- --exact`

- [ ] **Step 3: Implement the phase wrapper**

The wrapper resolves one exact recipe to PATH `cargo.exe` + fixed argument vector, allocates a unique run directory, captures a baseline, prints an explicit warning before `-LongRustRun`, executes through `ProcessStartInfo` (`ArgumentList`, worktree `WorkingDirectory`, isolated env including `CARGO_TARGET_DIR`), observes the admitted process tree without kill authority (quiet window refreshes descendants each poll), always publishes `processes-after.json` after admission (real inventory or bounded `status=unavailable` envelope), always asserts production unchanged after admission, writes immutable atomic `verification.json`, and exits with production/evidence/verification-publication priority over the child exit.

**Phase 0 process boundary:** this gate observes and fails closed; it does not kill. Record `cleanupResult` as `clean` or `residue`. Never use process-name selection, `Stop-Process`, `taskkill`, a PID kill, or a production profile/kill parameter. Phase 3 supplies the first authoritative cleanup gate.

- [ ] **Step 4: Self-test the wrapper with a harmless recipe**

Run:

```powershell
pwsh -NoProfile -File scripts/native-next/Invoke-PhaseGate.ps1 -Phase phase-00-self-test -Recipe cargo-version
cargo test --test development_isolation phase_gate_wraps_commands_with_baseline_and_cleanup_checks -- --exact
```

Expected: the recipe exits 0, a unique run evidence bundle is written, production comparison passes, and process-after evidence has no disposable development processes. Codex alone runs this real harmless self-test and rechecks protected hashes plus installed PID/start time.

- [ ] **Step 5: Review and commit**

```powershell
git add scripts/native-next tests/development_isolation.rs
git commit -m "chore: add guarded phase verification"
```

### Task 0.5: Seed the replacement deletion ledger

**Files:**
- Create: `docs/replacement-deletion-ledger.md`

**Interfaces:**
- Produces: one committed owner map plus an append-only record of temporary seams and their deletion proof.
- Consumed by: every replacement phase and Phase 11; every temporary re-export is appended in its creating commit.

- [ ] **Step 1: Write the lean ledger with these mandatory owners**

```markdown
| Current path | Current responsibility | Replacement phase | Delete only after |
|---|---|---:|---|
| `src/app/mod.rs` | Window, orchestration, UI, background polling | 2–10 | New GPUI client passes full feature gate |
| `src/services/process_manager.rs` | PTY/provider/server/process monolith | 3–8 | Host services pass zero-orphan gate |
| `src/state/` | Tab/runtime read models | 1–6 | Task projections serve all clients |
| `src/models/config.rs` (`SessionState`/`SessionTab`) | Old open-tab persistence data model | 1, 11 | Cutover passes with both symbols absent |
| `src/persistence/mod.rs` session path/read-write | Legacy `session.json` resolution and persistence | 1, 11 | Cutover passes with no session path or reader |
| `src/services/session_manager.rs` | Runtime session save behavior | 1, 11 | Cutover passes after the file is removed |
| `src/remote/mod.rs` old snapshot bridge | Window-owned remote authority | 2, 9 | Connect protocol/realtime gate passes |
| `src/remote/web/bridge.rs` old bridge | Old web mutation/snapshot transport | 9 | New web client passes reconnect gate |
| `src/sidebar/` | Old GPUI navigation | 5 | Task navigation/configuration passes UI gate |
| `src/workspace/editor_ui.rs` | Old form primitives | 5–6 | New semantic components cover configuration |
| `tests/legacy_loader.rs` | Old config/session migration | 11 | Supported config/remote tests pass without session reader |
| `tests/fixtures/legacy-session.json` | Old tab-state fixture | 11 | Empty task DB cutover test passes |
| `zz-archive/tauri-react-v0.1.11` | Archived old desktop | 11 | Release docs no longer reference it |
```

Add one rule beneath the table: a phase that introduces a temporary re-export, bridge, or compatibility seam appends its owner, replacement phase, and deletion proof in the same commit. Do not pre-inventory speculative future seams.

- [ ] **Step 2: Review the ledger against the approved specification**

Run:

```powershell
rg -n "src/app/mod.rs|process_manager.rs|session.json|remote.json|zz-archive" docs/replacement-deletion-ledger.md
git diff --check
```

Expected: every mandatory owner appears and the diff is clean.

- [ ] **Step 3: Commit**

```powershell
git add docs/replacement-deletion-ledger.md
git commit -m "docs: define replacement isolation and deletion gates"
```

### Task 0.6: Run the Phase 0 gate

**Files:** none beyond ignored evidence.

- [ ] **Step 1: Announce the serial Rust verification**

Use the guarded wrapper so the user sees that Rust test executables will run serially and phase evidence will be captured.

- [ ] **Step 2: Run formatting and the focused isolation recipe**

```powershell
pwsh -NoProfile -File scripts/native-next/Invoke-PhaseGate.ps1 -Phase phase-00-fmt -Recipe cargo-fmt-check
pwsh -NoProfile -File scripts/native-next/Invoke-PhaseGate.ps1 -Phase phase-00-isolation -Recipe development-isolation-tests -LongRustRun
```

- [ ] **Step 3: Run the complete library baseline serially**

```powershell
pwsh -NoProfile -File scripts/native-next/Invoke-PhaseGate.ps1 -Phase phase-00-lib -Recipe library-tests-serial -LongRustRun
```

- [ ] **Step 4: Inspect evidence and repository state**

Run:

```powershell
$latestRun = Get-ChildItem .devmanager-next/evidence/phase-00-lib/runs -Directory |
    Sort-Object LastWriteTimeUtc -Descending |
    Select-Object -First 1
Get-Content (Join-Path $latestRun.FullName verification.json)
Get-Content (Join-Path $latestRun.FullName processes-after.json)
git status --short --branch
```

Expected: commands pass, production comparison is unchanged, no disposable process remains, and the worktree contains no uncommitted source changes.

## Phase 0 exit gate

- Named path tests prove `native-next-dev` cannot alias production.
- Baseline/comparison scripts pass on the real machine without reading `session.json`.
- `-ValidateOnly` start/stop scaffolds start no processes and refuse real lifecycle without the switch.
- Guarded command evidence records exit codes and observed residue without kill authority.
- The lean deletion ledger records the mandatory current owners and every temporary seam introduced later.
- `cargo-fmt-check`, `development-isolation-tests`, and the complete serial `library-tests-serial` recipe are green before Phase 1, or a pre-existing failure is recorded.
- Installed DevManager PID/start time and production `config.json`/`remote.json` remain unchanged.
