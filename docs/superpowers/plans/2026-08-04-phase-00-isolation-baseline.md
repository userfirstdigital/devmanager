# Phase 0: Isolation and Baseline Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Establish a fail-closed replacement-development environment and reproducible evidence proving that builds, tests, preview binaries, browsers, ports, and cleanup cannot touch the installed DevManager.

**Architecture:** All replacement work runs from `.worktrees/native-gpui-kernel` on `codex/native-gpui-kernel`, with the explicit profile `native-next-dev`, instance label `Next`, dedicated target/live directories, and ignored evidence files. Shared PowerShell guards capture and compare production hashes and process identity around every risky gate; Rust path resolution remains fail-closed in tests. A provider/protocol conformance runner begins here with immutable manifests and append-only traces so every later seam is measured through one reproducible evidence contract.

**Tech Stack:** PowerShell 7, Rust 1.94.0, serde/rmp-serde/serde_json, existing `dirs`/persistence code, Windows CIM/process APIs, SHA-256, Git worktrees.

## Global Constraints

- Do not start, stop, restart, install over, attach a debugger to, or send input to the installed DevManager.
- Production storage is the unprofiled `%APPDATA%\com.userfirst.devmanager` tree; tests must never resolve beneath it.
- `config.json` and `remote.json` hashes plus installed PID/start time are the protected invariants. `session.json` is observed only as a path and never read or hashed because the installed app may legitimately update it.
- Use `DEVMANAGER_PROFILE=native-next-dev` and `DEVMANAGER_INSTANCE_LABEL=Next` for every replacement binary.
- Use `target-native-next` and `target-live-native-next`; never copy a development executable into the installed location.
- Full Rust library verification remains `cargo test --lib -- --test-threads=1` and must be announced before execution.
- Every script fails closed on unresolved paths, ambiguous executable identity, malformed evidence, or missing expected profile variables.
- This phase changes development tooling, path policy, and provider-independent conformance evidence only; it does not introduce the new kernel or launch provider/browser work.

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
- Modify: `Cargo.toml`, `Cargo.lock` — pin UUIDv7 support for conformance run identity.
- Modify: `src/persistence/mod.rs` — delegate path calculation to `config::paths` without changing production behavior.
- Create: `tests/development_isolation.rs` — path/profile fail-closed contract.
- Create: `scripts/native-next/Isolation.ps1` — shared path, process, hash, and evidence functions.
- Create: `scripts/native-next/Capture-ProductionBaseline.ps1` — read-only baseline JSON.
- Create: `scripts/native-next/Assert-ProductionUnchanged.ps1` — fail-closed comparator.
- Create: `scripts/native-next/Start-NativeNext.ps1` — Phase 0 ValidateOnly isolation scaffold (real launch deferred to Phase 2).
- Create: `scripts/native-next/Stop-NativeNext.ps1` — Phase 0 ValidateOnly isolation scaffold (real stop deferred to Phase 2; zero-orphan to Phase 3).
- Create: `scripts/native-next/NativeNext.ps1` — shared path/env validation library for the scaffold.
- Create: `scripts/native-next/Invoke-PhaseGate.ps1` — announced command execution plus before/after evidence and cleanup.
- Create: `scripts/native-next/Capture-PerformanceBaseline.ps1` — read-only machine/installed idle reference; isolated cold-start keys deferred until Phase 2 Start/Stop exist.
- Create: `scripts/native-next/Invoke-Conformance.ps1` — shared case/arm runner under the Phase 0 production guard.
- Create: `src/conformance/mod.rs` — conformance API exports.
- Create: `src/conformance/manifest.rs` — immutable run/case/arm/environment manifest.
- Create: `src/conformance/trace.rs` — bounded append-only `.dmtrace` records.
- Create: `src/conformance/runner.rs` — resumable deterministic case executor.
- Create: `tests/conformance_manifest.rs` — manifest/trace/resume/redaction contract.
- Create: `tests/fixtures/conformance/v1/isolation-case.json` — first deterministic case definition.
- Create: `docs/replacement-deletion-ledger.md` — old-path ownership and deletion criteria.
- Create: `docs/performance-budgets.md` — stable measurement definitions and initial acceptance budgets.
- Modify: `.gitignore` — ignore development output/evidence.
- Modify: `AGENTS.md` — make the replacement isolation command authoritative.

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
- Modify: `scripts/native-next/Isolation.ps1`
- Test: `tests/development_isolation.rs`

**Interfaces:**
- Produces: phase evidence with command, arguments, exit code, duration, before/after process inventories, and cleanup result.
- Accepts: `-Phase`, `-Command`, `-Arguments`, `-LongRustRun`, `-AllowDevelopmentProcesses`.
- Never accepts a production profile or broad kill target.

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

The wrapper captures a baseline, prints an explicit warning before `-LongRustRun`, executes one command through `System.Diagnostics.ProcessStartInfo` without a second shell, records the exit code and elapsed milliseconds, waits for Cargo/rustc/test executables whose paths belong to the worktree, runs exact-path development cleanup, compares production state, writes `verification.json`, and exits with the original nonzero result or any guard failure.

- [ ] **Step 4: Self-test the wrapper with a harmless command**

Run:

```powershell
pwsh -NoProfile -File scripts/native-next/Invoke-PhaseGate.ps1 -Phase phase-00-self-test -Command pwsh -Arguments @('-NoProfile','-Command','exit 0')
cargo test --test development_isolation phase_gate_wraps_commands_with_baseline_and_cleanup_checks -- --exact
```

Expected: the command exits 0, evidence files are written, production comparison passes, and process-after evidence has no disposable development processes.

- [ ] **Step 5: Review and commit**

```powershell
git add scripts/native-next tests/development_isolation.rs
git commit -m "chore: add guarded phase verification"
```

### Task 0.5: Capture the replacement inventory and deletion ledger

**Files:**
- Create: `docs/replacement-deletion-ledger.md`
- Modify: `AGENTS.md`

**Interfaces:**
- Produces: one list of current source owners, replacement phase, temporary seam, and deletion proof.
- Consumed by: Phase 11; every temporary re-export added later must be appended in its creating commit.

- [ ] **Step 1: Generate a read-only current inventory**

Run:

```powershell
rg --files src tests web zz-archive | Sort-Object | Set-Content .devmanager-next/evidence/phase-00/source-files.txt
Get-ChildItem src -Recurse -Filter *.rs | Sort-Object Length -Descending | Select-Object FullName,Length | ConvertTo-Json | Set-Content .devmanager-next/evidence/phase-00/rust-file-sizes.json
```

Expected: ignored evidence identifies the largest coupled files without modifying them.

- [ ] **Step 2: Write the ledger with these mandatory owners**

```markdown
| Current path | Current responsibility | Replacement phase | Delete only after |
|---|---|---:|---|
| `src/app/mod.rs` | Window, orchestration, UI, background polling | 2–10 | New GPUI client passes full feature gate |
| `src/services/process_manager.rs` | PTY/provider/server/process monolith | 3–8 | Host services pass zero-orphan gate |
| `src/state/` | Tab/runtime read models | 1–6 | Task projections serve all clients |
| `src/models/SessionState` | Old open-tab persistence | 1, 11 | New host proves empty-start behavior |
| `src/remote/mod.rs` old snapshot bridge | Window-owned remote authority | 2, 9 | Connect protocol/realtime gate passes |
| `src/remote/web/bridge.rs` old bridge | Old web mutation/snapshot transport | 9 | New web client passes reconnect gate |
| `src/sidebar/` | Old GPUI navigation | 5 | Task navigation/configuration passes UI gate |
| `src/workspace/editor_ui.rs` | Old form primitives | 5–6 | New semantic components cover configuration |
| `tests/legacy_loader.rs` | Old config/session migration | 11 | Supported config/remote tests pass without session reader |
| `tests/fixtures/legacy-session.json` | Old tab-state fixture | 11 | Empty task DB cutover test passes |
| `zz-archive/tauri-react-v0.1.11` | Archived old desktop | 11 | Release docs no longer reference it |
```

Also list every old browser/UI/provider compatibility seam discovered by `rg -n "legacy|compat|migrate|SessionState|RemoteWorkspaceSnapshot" src tests`.

- [ ] **Step 3: Strengthen `AGENTS.md` with the exact replacement gate command**

Add one consolidated project rule: all native-kernel implementation and verification uses `scripts/native-next/Invoke-PhaseGate.ps1`; direct long/full Rust runs are prohibited for this program because the wrapper owns production evidence and cleanup.

- [ ] **Step 4: Review the ledger against the approved specification**

Run:

```powershell
rg -n "src/app/mod.rs|process_manager.rs|session.json|remote.json|zz-archive" docs/replacement-deletion-ledger.md
git diff --check
```

Expected: every mandatory owner appears and the diff is clean.

- [ ] **Step 5: Commit**

```powershell
git add docs/replacement-deletion-ledger.md AGENTS.md
git commit -m "docs: define replacement isolation and deletion gates"
```

### Task 0.6: Capture the performance reference and measurement contract

**Files:**
- Create: `scripts/native-next/Capture-PerformanceBaseline.ps1`
- Create: `docs/performance-budgets.md`
- Modify: `tests/development_isolation.rs`

**Interfaces:**
- Produces ignored `performance.json` with reference hardware and installed idle samples; isolated cold-start keys may be defined as unavailable/`pending-phase-2` until real Start/Stop exist.
- Produces committed metric definitions/budgets used unchanged by Phases 3, 5, 7, 8, 9, 10, and 11.
- Never sends input, changes priority/affinity, opens configuration files, restarts the installed process, or launches against the production profile. Phase 0 must not invoke real Start/Stop lifecycle for measurement.

- [ ] **Step 1: Write the failing safety/shape tests**

Add `performance_baseline_is_read_only_and_profile_guarded` and `performance_budget_defines_every_program_metric`. Assert the script exposes only `-InstalledReadOnly`, `-IsolatedColdStart`, `-DurationSeconds`, and `-EvidenceDirectory`; `-IsolatedColdStart` in Phase 0 records the metric contract/availability only (no process launch); the installed mode contains no stop/kill/input/window-message/file-content operations.

- [ ] **Step 2: Run and observe the expected failure**

```powershell
cargo test --test development_isolation performance_ -- --nocapture
```

Expected: missing script/budget contract.

- [ ] **Step 3: Implement read-only measurement**

Record Windows/build, CPU model/logical processor count, physical memory, active display sizes/scales, sample interval, and monotonic timestamps. For an already-running installed DevManager, sample only cumulative CPU time, working/private bytes, handles, and threads for 120 seconds by PID plus creation time. For isolated cold-start measurement, Phase 0 may only define the metric contract and capture the installed reference; it must not claim an isolated start measurement until Phase 2 provides real `Start-NativeNext` lifecycle against `devmanager-host`/`devmanager-next`.

- [ ] **Step 4: Lock metric definitions and initial budgets**

`docs/performance-budgets.md` defines input-to-paint, PTY-byte-to-grid, local/remote command acknowledgement, event propagation, cold/warm startup, task-open, idle CPU/memory, background wakeups, long-session growth, scrollback bounds, provider/browser process counts, 4K frame time, and slow-client isolation. Record baseline values only in ignored evidence; commit measurement methods, reference percentile/sample sizes, and the initial Phase 5/8 targets.

- [ ] **Step 5: Run the baseline without disturbing production**

```powershell
pwsh -NoProfile -File scripts/native-next/Capture-PerformanceBaseline.ps1 -InstalledReadOnly -DurationSeconds 120 -EvidenceDirectory .devmanager-next/evidence/phase-00-performance
pwsh -NoProfile -File scripts/native-next/Capture-PerformanceBaseline.ps1 -IsolatedColdStart -EvidenceDirectory .devmanager-next/evidence/phase-00-performance
cargo test --test development_isolation performance_ -- --nocapture
```

Expected: the installed PID/start time remain unchanged, production files are unopened/unchanged, no isolated process is started, and `performance.json` contains installed samples plus cold-start keys marked unavailable until Phase 2 (no invented zeroes).

- [ ] **Step 6: Commit**

```powershell
git add scripts/native-next/Capture-PerformanceBaseline.ps1 docs/performance-budgets.md tests/development_isolation.rs
git commit -m "test: define replacement performance baseline"
```

### Task 0.7: Establish the shared conformance manifest and trace runner

**Files:**
- Create: `src/conformance/{mod,manifest,trace,runner}.rs`
- Modify: `src/lib.rs`
- Modify: `Cargo.toml`, `Cargo.lock`
- Create: `tests/conformance_manifest.rs`
- Create: `tests/fixtures/conformance/v1/isolation-case.json`
- Create: `scripts/native-next/Invoke-Conformance.ps1`
- Modify: `.gitignore`

**Interfaces:**
- Produces: `ConformanceCase`, `ConformanceArm`, `ConformanceRunManifest`, `TraceRecord`, and `ConformanceRunner::resume`.
- Produces: immutable `manifest.json`, append-only `trace.dmtrace`, resumable `cursor.json`, and terminal `result.json` under ignored `.devmanager-next/evidence/<phase>/conformance/<run-id>/`.
- Consumed by: protocol, process, provider, browser, Connect, and release gates. SQLite/search dashboards may index these artifacts later but never become canonical.

- [ ] **Step 1: Write the failing manifest/trace tests**

```rust
#[test]
fn completed_run_is_immutable_and_resumable() {
    let temp = tempfile::tempdir().unwrap();
    let case = fixture_case("isolation-path-contract");
    let run = ConformanceRunner::start(temp.path(), case, ConformanceArm::Baseline).unwrap();
    run.record(TraceRecord::case_started(1)).unwrap();
    drop(run); // simulate interruption

    let resumed = ConformanceRunner::resume(temp.path()).unwrap();
    resumed.record(TraceRecord::assertion_passed(2, "profile_isolated")).unwrap();
    let result = resumed.finish(ConformanceOutcome::Passed).unwrap();

    assert_eq!(result.trace_sequence, 2);
    assert!(ConformanceRunner::resume(temp.path()).is_err());
    assert_manifest_and_trace_hashes_match(temp.path());
}
```

Also add `manifest_rejects_unknown_schema_major`, `trace_rejects_non_monotonic_sequence`, `trace_record_is_bounded_before_write`, `secrets_and_absolute_user_paths_are_rejected`, and `baseline_and_variant_keep_distinct_arm_ids`. Phase 1 adds the separate rebuildable query-index contract after the canonical artifact format exists.

- [ ] **Step 2: Run the focused test and observe the intended failure**

Run: `cargo test --test conformance_manifest -- --nocapture`

Expected: compilation fails because `devmanager::conformance` does not exist.

- [ ] **Step 3: Define the versioned manifest and case types**

```rust
pub enum ConformanceArm {
    Baseline,
    Variant { label: String },
}

pub struct ConformanceRunManifest {
    pub schema_version: u16,
    pub run_id: ConformanceRunId,
    pub case_id: String,
    pub arm: ConformanceArm,
    pub devmanager_revision: String,
    pub adapter_revision: Option<String>,
    pub provider: Option<ProviderEvidence>,
    pub platform: PlatformEvidence,
    pub capabilities: std::collections::BTreeSet<String>,
    pub fixture_sha256: [u8; 32],
    pub trace_schema_version: u16,
    pub started_at_ms: i64,
}

pub struct ProviderEvidence {
    pub kind: String,
    pub executable_sha256: [u8; 32],
    pub version: String,
}

pub struct PlatformEvidence {
    pub os: String,
    pub architecture: String,
    pub logical_processors: u16,
}
```

Add `uuid = { version = "1.24.0", features = ["v7", "serde"] }` and define `ConformanceRunId` as a private-field UUIDv7 newtype. Validate bounded labels/fields, reject credentials/raw prompts/responses and user-profile absolute paths, sort capability/evidence maps deterministically, and write the manifest once through same-directory temporary-file plus atomic rename. A run arm never mutates after the first trace record.

- [ ] **Step 4: Implement bounded append-only traces and resume**

Use a `u32` big-endian length followed by MessagePack `TraceRecord`; hard-limit one record to 256 KiB and fsync at case checkpoints. Each record has sequence, monotonic nanoseconds from run start, stable event kind, redacted typed fields, and prior-record hash. `cursor.json` records the last settled case step and trace hash. Resume verifies the manifest, full hash chain, and cursor before appending; a completed `result.json` makes the directory immutable.

- [ ] **Step 5: Add the first deterministic case and guarded script**

`isolation-case.json` runs only the pure path/profile assertions from Task 0.1 and records pass/fail plus durations. `Invoke-Conformance.ps1 -Case isolation-path-contract -Arm Baseline -EvidenceDirectory ...` validates that it is inside `native-next-dev`, calls `Invoke-PhaseGate.ps1`, supports `-ResumeRunId`, and refuses production paths/provider authentication.

- [ ] **Step 6: Run baseline and variant arms**

```powershell
cargo test --test conformance_manifest -- --nocapture
pwsh -NoProfile -File scripts/native-next/Invoke-Conformance.ps1 -Case isolation-path-contract -Arm Baseline -EvidenceDirectory .devmanager-next/evidence/phase-00/conformance
pwsh -NoProfile -File scripts/native-next/Invoke-Conformance.ps1 -Case isolation-path-contract -Arm Variant -ArmLabel repeat -EvidenceDirectory .devmanager-next/evidence/phase-00/conformance
```

Expected: both manifests reference the same case/fixture hash, different arm/run IDs, valid immutable trace/result hashes, and unchanged production evidence.

- [ ] **Step 7: Review and commit**

```powershell
git diff --check
git add Cargo.toml Cargo.lock src/conformance src/lib.rs tests/conformance_manifest.rs tests/fixtures/conformance scripts/native-next/Invoke-Conformance.ps1 .gitignore
git commit -m "test: establish compatibility conformance runner"
```

### Task 0.8: Run the Phase 0 gate

**Files:** none beyond ignored evidence.

- [ ] **Step 1: Announce the long Rust verification and capture a fresh baseline**

Use the wrapper so the user sees that Rust test executables will run and be cleaned up.

- [ ] **Step 2: Run formatting and focused isolation tests**

```powershell
pwsh -NoProfile -File scripts/native-next/Invoke-PhaseGate.ps1 -Phase phase-00-fmt -Command cargo -Arguments @('fmt','--all','--','--check')
pwsh -NoProfile -File scripts/native-next/Invoke-PhaseGate.ps1 -Phase phase-00-isolation -Command cargo -Arguments @('test','--test','development_isolation','--test','conformance_manifest','--','--test-threads=1') -LongRustRun
```

- [ ] **Step 3: Run the complete library baseline serially**

```powershell
pwsh -NoProfile -File scripts/native-next/Invoke-PhaseGate.ps1 -Phase phase-00-lib -Command cargo -Arguments @('test','--lib','--','--test-threads=1') -LongRustRun
```

- [ ] **Step 4: Inspect evidence and repository state**

Run:

```powershell
Get-Content .devmanager-next/evidence/phase-00-lib/verification.json
Get-Content .devmanager-next/evidence/phase-00-lib/processes-after.json
git status --short --branch
```

Expected: commands pass, production comparison is unchanged, no disposable process remains, and the worktree contains no uncommitted source changes.

## Phase 0 exit gate

- Named path tests prove `native-next-dev` cannot alias production.
- Baseline/comparison scripts pass on the real machine without reading `session.json`.
- `-ValidateOnly` start/stop scaffolds start no processes and refuse real lifecycle without the switch.
- Guarded command evidence records exit codes and cleanup.
- Deletion ledger covers every old architecture owner.
- Performance measurement definitions and a read-only installed reference are captured; isolated cold-start measurement is deferred until Phase 2 Start/Stop exist.
- The deterministic conformance case runs baseline/variant arms with immutable manifests, resumable bounded traces, no raw content, and unchanged production evidence.
- Full serial library baseline is green or any pre-existing failure is recorded before Phase 1.
- Installed DevManager PID/start time and production `config.json`/`remote.json` remain unchanged.
