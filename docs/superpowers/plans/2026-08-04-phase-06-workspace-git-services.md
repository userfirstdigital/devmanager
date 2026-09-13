# Phase 6: Workspace, Git, Services, and Command Center Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make each Task a complete, safe coding workspace with explicit project configuration, worktree isolation, bounded file/artifact access, Git review/checkpoints, managed and external services, SSH, truthful resources, and an operational Command Center.

**Architecture:** Supported project/folder/command/SSH configuration remains in `config.json` and feeds host-owned services. A Task binds to one workspace choice—Main, Worktree, or explicitly confirmed External. Git and filesystem operations are typed host commands with resolved-path containment, idempotent receipts, and audit events. Services launch through the Phase 3 supervisor; Command Center renders background projections rather than probing the OS.

**Tech Stack:** Rust, Git CLI with non-interactive porcelain formats, existing config/SSH/env/port/process services, GPUI Task Cockpit, Windows credential storage already used by DevManager.

## Global Constraints

- Preserve the user's left-sidebar project/folder/command/SSH configuration from `config.json`; fresh-start applies only to task/session state.
- New AI coding Tasks default to an isolated Git worktree. Main checkout requires an explicit task choice; Ask does not create until answered.
- Git worktrees are the only shipped isolation backend in this program. Do not add Oh My Pi `pi-iso`, ProjFS, block-clone, copy-on-write, or an isolation abstraction during initial implementation; revisit only in a separate design after dirty-state, crash-recovery, cleanup, and Git-equivalence proof.
- Resolve every filesystem/destructive target to an absolute canonical path and prove it stays within the Task workspace or an explicitly selected artifact export target.
- Do not reset, clean, checkout over, overwrite, delete, commit, push, or open a PR without an explicit typed command and preview where consequences are material.
- Existing dirty changes belong to the user. Checkpoints capture state; restore is file/hunk scoped and never uses `git reset --hard`.
- Every dangerous action re-resolves and displays its target, scope, current revision/fingerprint, owner authority, and consequence immediately before execution; an earlier preview cannot authorize a changed target.
- One Git command executor serializes mutating operations per workspace. Reads may coalesce/cache outside hot paths.
- Managed services use Job ownership; external listeners stay blue, observed-only, and cannot be stopped from DevManager.
- Preserve current SSH password auto-injection behavior while adding no new credential exposure. Keys/passwords remain local and are never included in Connect events.
- Command Center reads immutable host snapshots. It must not perform process/port/quota/update/Git probes during layout or paint.

---

## File map

- Create: `src/workspace/model.rs`
- Create: `src/workspace/service.rs`
- Create: `src/workspace/worktree.rs`
- Create: `src/workspace/files.rs`
- Create: `src/workspace/artifacts.rs`
- Create: `src/workspace/checkpoint.rs`
- Refactor: `src/workspace/{mod,editor_ui}.rs`
- Create: `src/git/model.rs`
- Create: `src/git/command.rs`
- Create: `src/git/checkpoint.rs`
- Create: `src/git/review.rs`
- Refactor: `src/git/{mod,git_service,git_ui}.rs`
- Create: `src/services/model.rs`
- Create: `src/services/supervisor.rs`
- Create: `src/services/health.rs`
- Refactor: `src/services/{env_service,pid_file,platform_service,ports_service,scanner_service,session_manager}.rs`
- Create: `src/ssh/mod.rs`
- Create: `src/ssh/{launch,credentials}.rs`
- Create: `src/ui/configuration/{projects,folders,commands,ssh}.rs`
- Create: `src/ui/task_cockpit/{files_panel,changes_panel,services_panel,artifacts_panel,review_panel}.rs`
- Create: `src/ui/command_center/{overview,processes,ports,providers,connect,updates,diagnostics}.rs`
- Modify: `src/config/{model,project_store}.rs`
- Modify: `src/domain/{task,artifact,resource,command,event,snapshot}.rs`
- Modify: `src/host/mod.rs`
- Modify: `src/client/action.rs`
- Modify: `src/ui/task_cockpit/context_dock.rs`
- Create: `tests/workspace_service.rs`
- Create: `tests/worktree_service.rs`
- Create: `tests/file_service.rs`
- Create: `tests/git_service.rs`
- Create: `tests/checkpoint_service.rs`
- Create: `tests/service_supervisor.rs`
- Create: `tests/ssh_launch.rs`
- Create: `tests/command_center.rs`

### Task 6.1: Preserve and edit project configuration through one host service

**Files:** `src/config/{model,project_store}.rs`, `src/ui/configuration/{projects,folders,commands,ssh}.rs`, `src/domain/{command,event,snapshot}.rs`, `tests/workspace_service.rs`

- [ ] **Step 1: Create sanitized fixtures** representing all current left-sidebar configuration shapes: projects, folders, server/command definitions, default directories, shell options, editor choice, SSH hosts/auth modes, ordering, and unknown supported fields.
- [ ] **Step 2: Write failing tests** for byte-semantics-preserving load/save, stable ordering/IDs, validation, concurrent revision conflict, atomic replace, import/export, and no read/write of `session.json`.
- [ ] **Step 3: Run** `cargo test --test workspace_service config_ -- --nocapture` and record the red result.
- [ ] **Step 4: Add host commands** for create/update/reorder/archive project/folder/command/SSH configuration with expected config revision. Keep a single typed model in `src/config/model.rs`; remove duplicated sidebar models through the deletion ledger.
- [ ] **Step 5: Build GPUI configuration forms** using Phase 5 components, inline validation, safe secret references, cancel/save state, and import/export preview. Never expose raw password/private key values after entry.
- [ ] **Step 6: Load a copied production fixture into `native-next-dev`** and visually compare every left-sidebar item/order without reading the live production file during the UI run.
- [ ] **Step 7: Run** config tests and commit as `feat(config): preserve project sidebar configuration`.

### Task 6.2: Bind each task to one explicit workspace

**Files:** `src/workspace/{model,service}.rs`, `src/domain/task.rs`, `src/ui/task_cockpit/header.rs`, `tests/workspace_service.rs`

- [ ] **Step 1: Write failing tests** for Main, Worktree, Ask, explicit External, nonexistent path, non-repository folder, symlink/junction escape, drive-letter casing, UNC path, and workspace immutability while resources are live.
- [ ] **Step 2: Run** `cargo test --test workspace_service binding_ -- --nocapture` and retain the red output.
- [ ] **Step 3: Define `WorkspaceChoice::{Main, NewWorktree, Ask}`** at Task creation and resolve it to durable `WorkspaceRef`. AI coding Task defaults to `NewWorktree`; general terminal Tasks may use project default.
- [ ] **Step 4: Resolve/canonicalize Windows paths** including junctions and case normalization before comparison. Store a stable repo identity and relative worktree location where possible.
- [ ] **Step 5: Prevent workspace mutation** after a process/file/Git/browser resource starts; moving work requires a new Task or explicit close-and-rebind flow with no live resources.
- [ ] **Step 6: Show branch/worktree/path clearly** in header and task creation; commit as `feat(workspace): bind tasks to explicit workspaces`.

### Task 6.3: Create and remove isolated worktrees safely

**Files:** `src/workspace/worktree.rs`, `src/git/command.rs`, `tests/worktree_service.rs`

- [ ] **Step 1: Write failing tests** for branch naming, existing branch/path, dirty main checkout, nested repo, linked worktree discovery, concurrent creation, cancellation, safe cleanup refusal with dirty/unpushed work, and `only_git_worktree_backend_is_exposed`.
- [ ] **Step 2: Run** `cargo test --test worktree_service -- --nocapture` and save the red result.
- [ ] **Step 3: Execute Git with argument arrays** and `GIT_TERMINAL_PROMPT=0`; parse `git worktree list --porcelain` and `git status --porcelain=v2 -z`, never human-localized output.
- [ ] **Step 4: Generate branches under `codex/` by default** with collision-safe task suffixes and worktrees under the repository-approved worktree root. Record exact branch/path/base commit in Task facts after Git succeeds. Expose `WorkspaceChoice::NewWorktree`, not a generic backend selector or dormant ProjFS flag.
- [ ] **Step 5: On cleanup request**, preview dirty/untracked/unpushed status; refuse destructive removal by default. A confirmed force path targets the exact canonical worktree and remains recoverable where possible.
- [ ] **Step 6: Run** tests against temporary real Git repos and commit as `feat(workspace): manage isolated task worktrees`.

### Task 6.4: Add bounded file access and durable artifacts

**Files:** `src/workspace/{files,artifacts}.rs`, `src/ui/task_cockpit/{files_panel,artifacts_panel}.rs`, `src/domain/artifact.rs`, `tests/file_service.rs`

- [ ] **Step 1: Write failing tests** for listing/reading text and binary metadata, large-file chunking, symlink/junction escape, path traversal, atomic write, expected content hash conflict, deletion preview, artifact import/export, and secret-like file classification.
- [ ] **Step 2: Run** `cargo test --test file_service -- --nocapture` and retain the red result.
- [ ] **Step 3: Define typed file commands** using TaskId, workspace-relative normalized path, expected hash/revision, and bounded chunk sizes. Reject device paths, alternate data streams, traversal, and resolved paths outside the workspace.
- [ ] **Step 4: Write files atomically** when possible; return precise conflict data rather than overwriting changes. Deletion requires a separate preview/confirm receipt and prefers recoverable trash for user-originated files.
- [ ] **Step 5: Store artifact metadata/IDs in SQLite** and contents as files under the isolated Task artifact directory or explicit workspace paths. Content-address/hash every artifact; raw contents are not mirrored to Connect by default.
- [ ] **Step 6: Build virtualized Files/Artifacts panels** with search, open/reveal/copy path/export, status, and safe error states; commit as `feat(workspace): add bounded files and task artifacts`.

### Task 6.5: Add checkpoints and targeted recovery

**Files:** `src/workspace/checkpoint.rs`, `src/git/checkpoint.rs`, `src/domain/{artifact,event}.rs`, `tests/checkpoint_service.rs`

- [ ] **Step 1: Write failing tests** for before/after checkpoints, tracked/untracked metadata, binary files, partial changes, restore preview, external concurrent edits, and prohibition of whole-repo hard reset.
- [ ] **Step 2: Run** `cargo test --test checkpoint_service -- --nocapture` and record the red result.
- [ ] **Step 3: Define `Checkpoint`** with HEAD/tree identity, porcelain status, hashes, optional bounded patch/artifact references, created reason, Task/Agent/turn, and timestamp. Capture before writable agent turns and after accepted completion when the workspace changed.
- [ ] **Step 4: Implement restore as an explicit plan** listing exact files/hunks and conflicts. Apply to a temporary index/worktree preview first; require confirmation for overwrites/deletions and never invoke `git reset --hard` or `git clean -fd`.
- [ ] **Step 5: Detect external modifications** by expected hash and stop rather than erasing them. Record restored/skipped/conflicted files as events.
- [ ] **Step 6: Run** checkpoint tests and commit as `feat(workspace): add safe task checkpoints`.

### Task 6.6: Build Git status, diff, review, commit, push, and PR commands

**Files:** `src/git/{model,command,review}.rs`, `src/ui/task_cockpit/{changes_panel,review_panel}.rs`, `tests/git_service.rs`

- [ ] **Step 1: Write failing tests** for porcelain-v2 parsing, staged/unstaged/untracked/rename/submodule/conflict, unified diff parsing, binary/large diff, comments anchored to blob/path/line, selective stage, commit, push failure, no upstream, and PR URL parsing.
- [ ] **Step 2: Run** `cargo test --test git_service -- --nocapture` and save the red output.
- [ ] **Step 3: Run read commands on a coalescing background worker** keyed by workspace and invalidate on filesystem/kernel events. Bound diff bytes and expose truncation honestly.
- [ ] **Step 4: Use typed mutation commands and shared ActionCatalog entries** for stage/unstage/commit/push with expected HEAD/index/worktree fingerprint. Show exact files/message/remote/branch before consequential actions.
- [ ] **Step 5: Keep review comments as Task artifacts/events** anchored to repository-relative path, base blob ID, side, and line; mark stale anchors after changes rather than moving silently.
- [ ] **Step 6: Build Changes/Review panels** with accessible file tree, diff navigation, staged split, comments, checkpoint comparison, and capability/error states.
- [ ] **Step 7: Run** real temporary-repo tests and commit as `feat(git): add safe task review and delivery operations`.

### Task 6.7: Supervise configured commands and services

**Files:** `src/services/{model,supervisor,health}.rs`, `src/services/{session_manager,pid_file,env_service}.rs`, `src/ui/task_cockpit/services_panel.rs`, `tests/service_supervisor.rs`

- [ ] **Step 1: Write failing tests** for configured command launch, environment layering, health startup, crash, manual stop, task close, host-owned service, duplicate start, occupied external port, and dependent services.
- [ ] **Step 2: Run** `cargo test --test service_supervisor -- --nocapture` and retain the red result.
- [ ] **Step 3: Resolve command/environment/cwd from `config.json`** into a validated `LaunchIntent`; redact secret values from logs/events/snapshots. Launch through Phase 3 Job ownership and terminal service.
- [ ] **Step 4: Define service state from facts/probes:** `Stopped`, `Starting`, `Healthy`, `Unhealthy`, `External`, `Stopping`, `Failed`. A process running is not synonymous with health-ready.
- [ ] **Step 5: Implement explicit dependency ordering** with cycle rejection and bounded health waits. Failure does not implicitly kill an external listener or unrelated service.
- [ ] **Step 6: Build Services panel** with start/stop, terminal, health evidence, port status, dependency state, and exact ownership phrased naturally (`Managed here` only in details).
- [ ] **Step 7: Run** service tests and commit as `feat(services): supervise task and host commands`.

### Task 6.8: Preserve SSH behavior under host ownership

**Files:** `src/ssh/{mod,launch,credentials}.rs`, `src/config/model.rs`, `src/ui/configuration/ssh.rs`, `tests/ssh_launch.rs`

- [ ] **Step 1: Port existing sanitized SSH fixtures** and write failing tests for password auto-injection, key path, pasted key secret reference, agent/default auth, host-key prompt, passphrase, cancellation, and no secret in command line/events/Connect snapshots.
- [ ] **Step 2: Run** `cargo test --test ssh_launch -- --nocapture` and record the red result.
- [ ] **Step 3: Build SSH launch specs** from stable config while storing passwords/private key material only through the existing local credential mechanism. Temporary key files use restrictive permissions and guaranteed cleanup.
- [ ] **Step 4: Preserve `maybe_auto_submit_ssh_password` semantics** through a host-side terminal matcher/input path scoped to the exact SSH generation. Never echo or journal injected bytes.
- [ ] **Step 5: Launch SSH terminal/process through the Task-owned supervisor** and apply normal detach/reconnect/close behavior.
- [ ] **Step 6: Run** SSH tests with fake executables/prompts; commit as `refactor(ssh): preserve auth behavior in host runtime`.

### Task 6.9: Build the operational Command Center

**Files:** `src/ui/command_center/*.rs`, `src/client/model.rs`, `src/domain/snapshot.rs`, `tests/command_center.rs`

- [ ] **Step 1: Write failing projection tests** for host health, tasks/resources, complete process trees, CPU/memory, ports, provider capabilities/quota freshness, Connect devices, update state, diagnostics, partial metrics, and no-data/error states.
- [ ] **Step 2: Run** `cargo test --test command_center -- --nocapture` and save the red result.
- [ ] **Step 3: Build immutable summary models** on host background projectors and transmit deltas. UI filtering/sorting is client-local; all OS/network/file probes remain scheduled host services.
- [ ] **Step 4: Show clear status semantics:** green managed healthy, orange starting/attention, blue external listener, red failed, gray stopped, explicit unknown. Provide evidence timestamps and drill-down rather than implying certainty.
- [ ] **Step 5: Expose in-app process hierarchy** with honest executable names, task/resource association, complete Job member count, Task Manager CPU, memory, and diagnostic raw core equivalents.
- [ ] **Step 6: Add explicit actions** only where ownership permits them. External process rows offer copy/open Task Manager, not Stop/Kill.
- [ ] **Step 7: Capture large/error/partial-data previews** and commit as `feat(ui): add operational command center`.

### Task 6.10: Prove the complete local coding workspace

**Files:** `scripts/native-next/Invoke-WorkspaceSmoke.ps1`, all Phase 6 integration tests, `tests/fixtures/conformance/workspace/v1/*`, `docs/replacement-deletion-ledger.md`

- [ ] **Step 1: Create a temporary fixture repository** with a configured project, command, service, external listener, dirty file, untracked file, and remote bare repo.
- [ ] **Step 2: Through real host commands**, create a worktree Task, open/read/edit a file, capture checkpoints, launch/health-check service, view terminal/process/port status, inspect diff, stage/commit/push, and close.
- [ ] **Step 3: Verify** main checkout user changes remain untouched, external listener remains alive/blue, pushed work exists in fixture remote, artifacts hash correctly, and every managed Job reaches zero.
- [ ] **Step 4: Reopen the isolated host** and prove `config.json` sidebar configuration persists while task/workspace facts restore from SQLite, not `session.json`.
- [ ] **Step 5: Run shared conformance baseline/variant cases** for worktree creation/cleanup refusal, checkpoint/restore, bounded file/diff access, Git mutation settlement, service health, external-port safety, and zero Job residue. Use temporary repositories/fake services only and record declared latency/result/residue metrics without source bodies.
- [ ] **Step 6: Update the deletion ledger** for old workspace/sidebar/Git/service/SSH ownership and UI paths.
- [ ] **Step 7: Run** smoke plus focused tests; commit as `test(workspace): prove complete safe coding workspace`.

## Phase 6 verification gate

- [ ] Capture production baseline and announce the long isolated workspace/service gate.
- [ ] Run `cargo test --test workspace_service --test worktree_service --test file_service --test git_service --test checkpoint_service --test service_supervisor --test ssh_launch --test command_center -- --nocapture`.
- [ ] Run `pwsh scripts/native-next/Invoke-WorkspaceSmoke.ps1`.
- [ ] Run `cargo fmt --all -- --check` and `cargo clippy --all-targets -- -D warnings`.
- [ ] Search destructive Git/process calls and manually audit every resolved target/confirmation: `rg -n "reset --hard|clean -fd|Remove-Item|TerminateJobObject|kill\(" src/workspace src/git src/services src/ssh`.
- [ ] Visually inspect configuration, Changes, Files, Services, Artifacts, Review, and Command Center in both themes/high DPI.
- [ ] Confirm external listeners and main-checkout fixture changes remain untouched; confirm every managed Job reaches zero.
- [ ] Rebuild the conformance query index and compare workspace/Git/service baseline and variant arms.
- [ ] Confirm no test/Cargo/rustc/helper/development host remains; compare production hashes and installed PID/start time.
- [ ] Review the Phase 6 diff and deletion ledger.

## Phase 6 exit criteria

- The complete left-sidebar configuration survives and is editable through one supported `config.json` contract.
- Every Task has one explicit workspace, with isolated worktree default for AI coding and safe refusal around dirty/unpushed cleanup.
- Files, artifacts, checkpoints, and Git mutations are path-contained, revision checked, and reviewable.
- Managed services/SSH run inside owned Jobs; external listeners are accurately blue and never controlled.
- Command Center reports the host, providers, complete process trees, ports, Connect, updates, and diagnostics from background projections.
- A real temporary-repo smoke completes the coding lifecycle with no orphan and no effect on production DevManager.
