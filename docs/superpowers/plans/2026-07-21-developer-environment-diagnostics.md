# Developer Environment Diagnostics Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a Windows-first Settings diagnostics page that detects DevManager's development prerequisites, explains problems, and performs only previewed, typed, verified repairs.

**Architecture:** A focused `src/diagnostics/` subsystem owns immutable results, bounded probes, the curated catalog, PowerShell profile editing, and repair planning/execution. Existing GPUI app/workspace modules only own async orchestration, navigation, and rendering. Tests inject command execution so the catalog is deterministic and does not depend on the developer's workstation.

**Tech Stack:** Rust 2021, GPUI 0.2.2, Tokio process/time, serde, existing DevManager configuration and theme components, tempfile tests.

## Global Constraints

- Windows receives full probes and repairs; macOS must remain compile-safe with portable read-only checks and explicit unavailable states.
- Never run arbitrary user-provided scripts or construct a command through a shell.
- Every process has an explicit timeout and bounded, redacted output.
- Every mutation is represented by a typed repair plan, previewed and confirmed, followed by verification.
- PowerShell profile edits preserve user content, create a backup, modify only a DevManager-owned marked block, and roll back on parse failure.
- `--dangerously-skip-permissions` is high risk and excluded from bulk repair.
- Diagnostics failure must never prevent the rest of DevManager from starting or operating.
- Do not add a package dependency unless the standard library or current dependencies cannot provide the behavior.

---

### Task 1: Diagnostics domain model and summary rules

**Files:**
- Create: `src/diagnostics/mod.rs`
- Create: `src/diagnostics/model.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Produces: `DiagnosticId`, `DiagnosticImportance`, `DiagnosticStatus`, `DiagnosticResult`, `DiagnosticSnapshot`, `RepairRisk`, `RepairOperation`, `RepairPlan`, and `RepairOutcome`.
- `DiagnosticSnapshot::from_results(Vec<DiagnosticResult>)` sorts required failures first and computes counts used by the UI.

- [ ] **Step 1: Write model tests for summary and repair eligibility**

Add tests that build healthy, warning, missing, and optional results and assert required failures sort first, missing optional checks do not increment warnings, and high-risk repairs are absent from `recommended_repairs()`.

- [ ] **Step 2: Run the focused test and confirm the module is missing**

Run: `cargo test --locked diagnostics::model::tests -- --test-threads=1`

Expected: compilation fails because `diagnostics` is not registered yet.

- [ ] **Step 3: Implement the model**

Use owned, cloneable UI-safe values. The core shapes are:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiagnosticImportance { Required, Recommended, Optional }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticStatus { Healthy, Warning, Missing, Broken, Running, Unavailable }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticResult {
    pub id: DiagnosticId,
    pub title: String,
    pub importance: DiagnosticImportance,
    pub status: DiagnosticStatus,
    pub summary: String,
    pub details: Vec<String>,
    pub detected_version: Option<String>,
    pub detected_path: Option<PathBuf>,
    pub repairs: Vec<RepairPlan>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepairOperation {
    RunKnownCommand { program: PathBuf, args: Vec<String> },
    InstallWingetPackage { package_id: String },
    UpdatePowerShellProfile { path: PathBuf, recipe: ProfileRecipe },
    SetDefaultTerminal(DefaultTerminal),
    SetClaudeCommand(String),
    SetCodexCommand(String),
    OpenUrl(String),
    RevealPath(PathBuf),
    CopyCommand(String),
}
```

- [ ] **Step 4: Register the module and pass tests**

Run: `cargo test --locked diagnostics::model::tests -- --test-threads=1`

Expected: all model tests pass.

### Task 2: Bounded command runner and sanitization

**Files:**
- Create: `src/diagnostics/runner.rs`
- Modify: `src/diagnostics/mod.rs`

**Interfaces:**
- Consumes: no UI types.
- Produces: injectable `CommandRunner`, `CommandSpec`, `CommandOutput`, `CommandFailure`, and `TokioCommandRunner`.

- [ ] **Step 1: Write tests for truncation, redaction, and display rendering**

Tests must cover 16 KiB output truncation, case-insensitive redaction for token/password/secret/bearer assignments, home-directory elision, and the rule that the display form never includes environment values.

- [ ] **Step 2: Run the focused tests and confirm failure**

Run: `cargo test --locked diagnostics::runner::tests -- --test-threads=1`

Expected: module or symbols are missing.

- [ ] **Step 3: Implement direct process execution with timeout**

`CommandSpec` contains `program: PathBuf`, `args: Vec<OsString>`, `timeout: Duration`, and an allowlisted environment override map. `TokioCommandRunner::run` uses `tokio::process::Command`, `kill_on_drop(true)`, piped output, and `tokio::time::timeout`; it never invokes `cmd.exe`, `powershell -Command <user text>`, or another shell wrapper.

- [ ] **Step 4: Pass runner tests**

Run: `cargo test --locked diagnostics::runner::tests -- --test-threads=1`

Expected: all runner tests pass.

### Task 3: Safe PowerShell profile inspection and editing

**Files:**
- Create: `src/diagnostics/profile.rs`
- Modify: `src/diagnostics/mod.rs`

**Interfaces:**
- Produces: `ProfileRecipe::{SafeClaudeShortcut, UnsafeClaudeShortcut}`, `ProfileEditPreview`, `inspect_marked_block`, `preview_profile_edit`, and `apply_profile_edit`.
- `apply_profile_edit` returns the backup path and final file digest; it accepts a parse-verification callback so tests do not need PowerShell.

- [ ] **Step 1: Write temporary-file tests**

Cover a missing file, preservation of text before/after the block, CRLF and LF, idempotent replacement, duplicate/malformed markers, adjacent timestamped backup, rollback after verification failure, and rejection of a symlink/reparse ambiguity where the platform exposes one.

- [ ] **Step 2: Run the focused tests and confirm failure**

Run: `cargo test --locked diagnostics::profile::tests -- --test-threads=1`

Expected: module or symbols are missing.

- [ ] **Step 3: Implement marked-block editing**

The managed block is exactly bounded by `# BEGIN DevManager` and `# END DevManager`. The safe recipe is:

```powershell
function cc {
    try { claude update | Out-Null } catch { }
    claude @args
}
```

The unsafe recipe adds `--dangerously-skip-permissions`, is tagged `RepairRisk::High`, and is never returned from bulk recommendations. Refuse zero or multiple complete blocks, a lone marker, non-file targets, and unresolved canonical parents.

- [ ] **Step 4: Pass profile tests**

Run: `cargo test --locked diagnostics::profile::tests -- --test-threads=1`

Expected: all profile tests pass.

### Task 4: Curated Windows catalog and deterministic probes

**Files:**
- Create: `src/diagnostics/catalog.rs`
- Create: `src/diagnostics/probe.rs`
- Create: `src/diagnostics/windows.rs`
- Modify: `src/diagnostics/mod.rs`
- Reuse: `src/services/pwsh_probe.rs`

**Interfaces:**
- Consumes: `Settings`, `CommandRunner`, and domain types.
- Produces: `DiagnosticProbe::scan(&Settings) -> DiagnosticSnapshot` and `scan_one(DiagnosticId, &Settings)`.

- [ ] **Step 1: Write fake-runner tests for the full catalog**

Fixtures must produce healthy, missing, nonzero, timeout, version-output, multiple-path, profile-parse, profile-load-warning, and GitHub-auth states. Assert missing optional tools stay informational and Windows-only repairs are unavailable on non-Windows builds.

- [ ] **Step 2: Run focused tests and confirm failure**

Run: `cargo test --locked diagnostics::probe::tests diagnostics::catalog::tests -- --test-threads=1`

Expected: modules or symbols are missing.

- [ ] **Step 3: Implement catalog data and portable executable resolution**

Centralize titles, importance, official documentation URLs, fixed winget IDs, executable candidates, and version arguments. Probe Claude/Codex from the configured command first, then their normal executable name. Parse configured command lines with the repository's existing launch-spec parser rather than splitting on spaces.

- [ ] **Step 4: Implement Windows probes**

Detect PowerShell 7, NVM/Node/npm, Git/Git identity, GitHub CLI/auth, winget, WebView2, PATH conflicts, and optional Docker/WSL/Rust/Python. Resolve the current PowerShell profile with `pwsh -NoProfile`, parse it without execution, then perform a separate bounded load probe whose stderr becomes sanitized warning detail.

- [ ] **Step 5: Pass catalog/probe tests**

Run: `cargo test --locked diagnostics:: -- --test-threads=1`

Expected: all diagnostics tests pass.

### Task 5: Typed repair executor and post-repair verification

**Files:**
- Create: `src/diagnostics/repair.rs`
- Modify: `src/diagnostics/mod.rs`

**Interfaces:**
- Consumes: `RepairPlan`, `CommandRunner`, `DiagnosticProbe`, and a small `SettingsRepairSink` trait implemented by the app.
- Produces: `RepairExecutor::execute(plan) -> RepairOutcome` and `execute_recommended(snapshot) -> Vec<RepairOutcome>`.

- [ ] **Step 1: Write executor tests**

Assert the executor rejects unrecognized package IDs and command shapes, excludes high-risk plans from a batch, runs sequentially, stops after the first failure, applies settings only through the sink, and reports failure if verification remains unhealthy.

- [ ] **Step 2: Run the focused tests and confirm failure**

Run: `cargo test --locked diagnostics::repair::tests -- --test-threads=1`

Expected: module or symbols are missing.

- [ ] **Step 3: Implement allowlisted execution and verification**

Match every `RepairOperation` explicitly. Render a stable preview before execution. For winget use the fixed shape `install --id <ID> --exact --accept-package-agreements --accept-source-agreements`; for profile edits call the safe profile module; for settings use the sink; for URLs/copy/reveal return a non-mutating outcome for the GPUI layer to perform.

- [ ] **Step 4: Pass executor and full diagnostics tests**

Run: `cargo test --locked diagnostics:: -- --test-threads=1`

Expected: all diagnostics tests pass.

### Task 6: GPUI diagnostics page and Settings entry point

**Files:**
- Modify: `src/workspace/mod.rs`
- Modify: `src/app/mod.rs`
- Modify: `src/models/config.rs`

**Interfaces:**
- Consumes: `DiagnosticSnapshot`, `DiagnosticResult`, and `RepairPlan`.
- Produces: `EditorPanel::Diagnostics(DiagnosticsDraft)` and typed `EditorAction` variants for open/back/rescan/expand/preview/confirm/cancel/bulk repair.

- [ ] **Step 1: Add UI-model tests**

Add preview/model tests asserting Diagnostics has the correct title, required failures render before optional checks, unavailable repairs have no confirm action, a running operation disables conflicting actions, and Back returns to Settings without discarding settings edits.

- [ ] **Step 2: Run focused workspace tests and confirm failure**

Run: `cargo test --locked workspace::tests -- --test-threads=1`

Expected: missing diagnostics panel/actions.

- [ ] **Step 3: Add the thin Settings and page models**

`DiagnosticsDraft` contains only display state:

```rust
pub struct DiagnosticsDraft {
    pub snapshot: DiagnosticSnapshot,
    pub expanded: HashSet<DiagnosticId>,
    pub pending_repair: Option<RepairPlan>,
    pub active_operation: Option<String>,
    pub last_error: Option<String>,
}
```

Add a compact Developer environment Settings row with summary and Open Diagnostics. Render the page with existing form cards, surface tones, buttons, typography, and scroll behavior.

- [ ] **Step 4: Add async app orchestration**

Open Diagnostics immediately with a running snapshot, spawn the scan on the background executor, update only if the panel still exists, and restore an idle/error state on every completion path. Confirmation dispatches one repair; bulk repair uses only `recommended_repairs`. Settings mutations update both the draft and persisted config through existing save paths.

- [ ] **Step 5: Keep diagnostics on demand**

Do not schedule diagnostics scanning or process probing during application startup. Show **Not scanned yet** before the first scan. Opening Diagnostics starts a fresh bounded scan unless a repair is active, and the page's **Rescan** action starts the same scan explicitly. Do not persist first-run or startup-scan metadata.

- [ ] **Step 6: Pass workspace/app tests**

Run: `cargo test --locked workspace::tests app::tests -- --test-threads=1`

Expected: all targeted tests pass.

### Task 7: Complete-diff review, platform gates, and documentation

**Files:**
- Modify: `README.md` only if it has a Settings/features section suitable for a short Diagnostics entry.
- Modify: implementation files only to correct review findings.

**Interfaces:**
- Consumes: completed implementation.
- Produces: release-ready verified feature.

- [ ] **Step 1: Review the complete diff**

Check for shell-string execution, unbounded child processes, raw environment/output logging, profile replacement outside the marked block, high-risk bulk inclusion, UI states that can remain `Running`, and accidental changes outside diagnostics integration.

- [ ] **Step 2: Run formatting and lint-oriented checks**

Run: `cargo fmt --all -- --check`

Expected: exit 0.

Run: `git diff --check`

Expected: exit 0.

- [ ] **Step 3: Run the locked test suite once**

Run: `cargo test --locked --all-targets -- --test-threads=1`

Expected: exit 0 with no failed tests.

- [ ] **Step 4: Run the Windows release build**

Run: `cargo build --locked --release`

Expected: exit 0 and `target/release/devmanager.exe` exists.

- [ ] **Step 5: Manually validate the running app**

Open Settings → Developer environment → Diagnostics. Confirm the local machine detects Claude, Codex, PowerShell 7, NVM, Node/npm, Git, GitHub CLI, Docker, WSL, Rust, and Python as appropriate; confirm the known non-interactive PSReadLine warning is readable but sanitized; preview the `cc` profile edit without applying it; rescan; navigate back to Settings; and confirm terminal/browser use remains normal.
