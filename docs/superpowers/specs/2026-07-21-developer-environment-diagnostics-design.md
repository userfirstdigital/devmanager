# Developer Environment Diagnostics Design

## Purpose

DevManager should help a developer recover a productive Windows workstation after a clean install without requiring them to remember every CLI, profile edit, or configuration step. The feature adds a dedicated Diagnostics page under Settings, a quiet startup health summary, and safe guided repairs for a curated DevManager development baseline.

The first release is intentionally not a general package manager or arbitrary setup-script runner. It diagnoses known tools and configuration, explains failures in plain language, previews every mutation, and verifies repairs after execution.

## Product experience

### Entry points

- Settings contains a **Developer environment** row with the latest summary and an **Open Diagnostics** action.
- DevManager starts a bounded background scan after the main window is ready. A dismissible banner appears only on first run or when a required check is missing or broken.
- The Diagnostics page has a back action, summary counts, **Rescan**, **Repair recommended**, and grouped check cards.
- Each card shows status, detected version/path when safe, concise guidance, expandable sanitized details, and the actions relevant to that result.
- Repair progress is visible. Only one repair runs at a time; closing the page does not corrupt a repair, and the affected checks refresh when it finishes.

### Status and severity

Checks use these statuses:

- `Healthy`: present and usable.
- `Warning`: usable but misconfigured, outdated relative to an explicitly known local requirement, or producing a recoverable warning.
- `Missing`: a required or recommended component cannot be found.
- `Broken`: the component was found but failed its bounded health probe.
- `Running`: a scan or repair is active.
- `Unavailable`: the check does not apply on this platform or could not safely run.

Catalog importance is independent of status:

- `Required`: Claude CLI, Codex CLI, PowerShell 7, Node.js/npm, and the configured DevManager AI commands.
- `Recommended`: NVM for Windows, PowerShell profile health, the optional `cc` shortcut recipe, Git identity, GitHub CLI/authentication, Windows Package Manager, WebView2 Runtime, and PATH consistency.
- `Optional`: Docker, WSL, Rustup/Cargo, Python, and other informational tools. Missing optional tools do not create warnings.

## Architecture

Add a focused `src/diagnostics/` subsystem rather than embedding process probing and repair logic in the already-large app and workspace modules.

- `mod.rs` exposes the stable domain API.
- `model.rs` owns check identifiers, importance, status, result/detail types, repair plans, repair outcomes, and aggregate summaries.
- `catalog.rs` defines the curated catalog and maps probe results to user-facing guidance.
- `runner.rs` executes bounded child processes without a shell by default, truncates output, and redacts likely secrets.
- `probe.rs` orchestrates checks and contains cross-platform discovery logic.
- `windows.rs` contains Windows-only paths, registry/environment discovery, profile checks, package identifiers, and repair construction.
- `repair.rs` validates and executes typed repair operations, then asks the probe layer to verify the affected checks.
- `profile.rs` parses and safely updates DevManager-owned PowerShell profile blocks.

The GPUI layer owns a diagnostics controller/state object containing the latest snapshot and active operation. Background work runs through the existing async executor and reports immutable snapshots back to the UI. The thin integrations in `src/app/mod.rs` and `src/workspace/mod.rs` open the page, dispatch typed actions, and render results.

## Probe behavior

Every process probe has an explicit timeout, executable plus argument vector, bounded stdout/stderr, and a sanitized display representation. Basic detection never invokes a user shell profile. Commands are resolved using the current process environment and, where appropriate, platform-specific known locations.

The PowerShell profile check has separate phases:

1. Resolve the PowerShell 7 current-user/current-host profile path using `pwsh -NoProfile`.
2. Check whether the file exists and can be parsed by PowerShell's parser without executing it.
3. Run a bounded non-interactive load probe and report warnings or errors. Output such as the PSReadLine redirected-terminal warning becomes a warning with targeted guidance rather than making the whole scan fail.
4. Inspect only the parsed function definition needed to recognize `cc`; do not capture unrelated profile contents in logs or persisted state.

The `cc` recipe is recommended but opt-in. The safe default recipe updates Claude and then launches it normally. A recipe containing `--dangerously-skip-permissions` is classified as high risk, never installed by **Repair recommended**, and requires a separate explicit confirmation describing that it bypasses Claude Code permission prompts.

Git identity checks read configured name/email presence but do not display the complete email in the summary. GitHub authentication checks use a bounded status command and never capture or display tokens. PATH diagnostics report executable names and canonical paths but redact user-home segments in shareable details.

## Repair model and policy

A repair is a typed `RepairPlan`, not a shell string. Operations are limited to:

- install a known package through `winget` with a fixed package identifier and argument vector;
- run a known vendor update command using a directly resolved executable;
- set a DevManager setting;
- create or update a DevManager-owned block in the PowerShell profile;
- open official instructions or reveal a relevant folder;
- copy a fully rendered command for manual execution.

Before execution, the UI shows the operation, target, command/arguments or file diff, expected effect, and risk. Mutating plans require explicit confirmation. **Repair recommended** may batch only normal-risk, idempotent repairs; package installs are performed sequentially and stop on the first failure. High-risk repairs are always excluded.

Profile edits must:

- resolve the canonical target and reject directories or symlink ambiguity;
- preserve existing content, encoding where detectable, and line endings;
- create a timestamped adjacent backup before mutation;
- modify only a clearly delimited `# BEGIN DevManager` / `# END DevManager` block;
- be idempotent;
- refuse duplicate or malformed markers and direct the user to manual review;
- write through a temporary file followed by an atomic replacement where Windows permits;
- roll back from the backup when the post-write parse check fails.

Repairs never persist tokens, passwords, complete environment dumps, or raw command output. The page keeps only the current in-memory snapshot and operation details. Configuration persistence is limited to serde-defaulted UX metadata such as banner dismissal or last successful scan time if needed.

## Check catalog and actions

The Windows catalog includes:

| Check | Healthy criteria | Guided actions |
| --- | --- | --- |
| Claude CLI | configured command resolves and `--version` succeeds | official install instructions, known update command, set command path |
| Codex CLI | configured command resolves and `--version` succeeds | official install instructions, package-manager-owned update guidance, set command path |
| PowerShell 7 | `pwsh` resolves and version probe succeeds | `winget` install preview, set default terminal |
| Node.js/npm | both resolve and version probes succeed | NVM guidance or known package preview |
| NVM for Windows | command and managed Node shim are coherent | official instructions or `winget` preview |
| PowerShell profile | file parses; load probe has no errors | create file, install/repair marked block, open file/folder |
| `cc` shortcut | recognized function is callable and matches selected recipe | preview safe recipe; separate high-risk personal recipe |
| Git | command succeeds and identity is configured | install preview, copy configuration commands |
| GitHub CLI | command succeeds; auth reported independently | install preview, copy/open login instructions |
| winget | command succeeds | Microsoft App Installer instructions |
| WebView2 | runtime discoverable | Microsoft runtime instructions |
| PATH consistency | configured commands resolve predictably without conflicting installations | show paths and manual cleanup guidance |
| Optional tools | version/status probe succeeds when installed | official instructions only |

Install/update identifiers and documentation links are centralized data. They must point to official vendor documentation. Failure to validate a package manager owner results in guidance rather than guessing an update command.

## UI integration

Diagnostics is a child Settings surface, not another long inline Settings section. The settings page row remains compact. The diagnostics page uses existing DevManager cards, status tones, buttons, and typography.

Cards are grouped as **Required**, **Recommended**, and **Optional**. Required failures sort first, followed by warnings and healthy items. The summary remains useful during partial scans. Details are selectable/copyable but sanitized. Buttons disable while their check or a conflicting repair is running.

No custom visual mockup is required: this feature extends established Settings components and does not introduce a new visual language.

## Platform behavior

Windows receives the full catalog and repair support. macOS remains compile-safe and may run portable read-only CLI checks, but Windows-only checks return `Unavailable` with clear copy. macOS repairs that lack an explicitly implemented typed operation are not offered.

## Failure handling

- A timed-out probe becomes `Broken` or `Unavailable` with a short timeout explanation; it cannot stall application startup.
- A repair failure retains the original diagnostic, shows sanitized stderr, and offers the preview/manual alternative.
- A canceled or interrupted batch stops before the next operation.
- A profile backup failure prevents the edit.
- A failed verification marks the repair unsuccessful even if the process exited zero.
- Diagnostics failures never prevent normal terminal, browser, project, or Settings use.

## Test strategy

- Unit tests cover aggregation/sorting, command-output truncation and redaction, executable resolution, timeout mapping, platform gating, repair risk/batch selection, and official-link/package catalog invariants.
- Profile tests use temporary files and cover creation, preservation, CRLF/LF, idempotence, malformed/duplicate markers, backup, rollback, and high-risk recipe exclusion.
- Probe tests use a fake command runner so missing, healthy, broken, warning, and timeout states are deterministic.
- App/workspace tests cover opening Diagnostics, rendering summary/check rows, disabling conflicting actions, and returning to Settings.
- Windows integration tests exercise real read-only probes without assuming any optional tool is installed.
- Final gates are `cargo fmt --all -- --check`, `cargo test --locked --all-targets -- --test-threads=1`, Windows release build/package validation, and the existing macOS/Windows CI matrix.

## Acceptance criteria

1. A clean Windows installation can discover the missing required toolchain without blocking DevManager startup.
2. A configured workstation reports detected versions and actionable warnings, including profile-load warnings, without exposing secrets.
3. Users can preview and confirm supported repairs, see backups for profile edits, and receive verified outcomes.
4. **Repair recommended** never installs the high-risk Claude permission-bypass shortcut.
5. Closing/reopening Diagnostics or rescanning cannot leave the UI in a permanently running state.
6. The feature compiles on macOS with clear unsupported repair states.
7. Existing DevManager functionality remains usable when diagnostics or an individual probe fails.
