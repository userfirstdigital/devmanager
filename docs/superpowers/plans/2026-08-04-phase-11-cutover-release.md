# Phase 11: Cutover, Deletion, and Release Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn the proven replacement into the only DevManager architecture, delete old ownership/session/UI/remote paths and dormant Tauri code, package the GPUI client plus durable host, preserve supported configuration, prompt libraries, task data, and Connect identities through subsequent updates, and ship only after complete compatibility, replay, isolated release, and explicit approval evidence.

**Architecture:** `devmanager.exe` becomes the native GPUI client and `devmanager-host.exe` remains the sole local execution process. The development-only `devmanager-next` identity disappears. The installer deploys both binaries and performs a bounded capability-negotiated update handoff; when safe handoff is impossible it waits for explicit full restart. The one-time old-to-new cutover loads existing `config.json` and `remote.json` directly but begins with an empty Task/prompt database and never imports `session.json` or provider conversations. Every later new-architecture update preserves and transactionally migrates the Task database, including personal prompts, versions, chains, and exact references.

**Tech Stack:** Rust/GPUI/cargo-packager/updater, React PWA, PowerShell release verification, isolated Windows VM/Sandbox test installs, GitHub release metadata/signing workflow already used by the repository.

## Global Constraints

- This is a deletion cutover, not a permanent migration layer. No old/new toggle, compatibility mode, duplicate host, alternate session store, or fallback UI ships.
- Preserve only supported old-architecture user configuration during the one-time cutover: project/folder/command/SSH settings in `config.json`; long-lived pairing/device/host identity and other schema-valid remote fields in `remote.json`; OS-vault credential references. Start the first new-architecture release with an empty new SQLite Task/prompt store.
- On every subsequent new-architecture update, preserve and migrate the SQLite Task/prompt store. Version/checksum fixtures must prove prompts, immutable versions, chains, Task links, operation state, and exact provider-session IDs survive or fail atomically with rollback.
- Do not read/import `session.json`, provider rollout/history directories, old open tabs, or old provider conversation IDs. The user starts fresh Task sessions once.
- Delete superseded code, tests, fixtures, assets, scripts, docs, feature flags, and re-exports in the same cutover. Do not leave an archive inside the source repository.
- Packaging must install `devmanager.exe` and `devmanager-host.exe` together with matching build/protocol identity and accurate Windows product/file descriptions.
- Updater compares signed semantic release versions against the running installed build identity and current release metadata, never development checkout files or stale PWA assets.
- An update never rotates the long-lived pairing code, task invitations, device keys, or host keys, and never overwrites `config.json`/`remote.json`. Pairing and task invitations remain distinct credentials; any schema write is additive/versioned/atomic and covered by before/after fixtures.
- Installer/updater validation runs in a disposable VM/Sandbox/profile first. Do not stop, replace, install over, or mutate the user's daily DevManager without explicit user approval at the release checkpoint.
- All Phase 0 production guards remain active through candidate validation. Long Rust verification is announced and the full library suite runs serially.
- A release cannot be called complete with compile/tests alone: it requires packaged clean-install/update, protocol/provider conformance replay, prompt persistence/access control, desktop/phone/provider/browser/process cleanup, accessibility/performance, signed metadata, and rollback evidence.

---

## File and deletion map

**Final entry/package files:**

- Modify: `src/main.rs` — final native GPUI client and hook relay dispatch
- Keep: `src/bin/devmanager-host.rs` — durable host binary
- Delete: `src/bin/devmanager-next.rs` — development identity ends
- Modify: `Cargo.toml`, `Cargo.lock`, `build.rs`
- Modify: `src/updater/mod.rs`
- Create: `src/updater/handoff.rs`
- Modify/create authoritative packaging scripts/config discovered from current release workflow
- Modify: `.github/workflows/release.yml`
- Modify: `.github/workflows/web-bundle.yml`
- Modify: `README.md`, `Terminal.md`, `AGENTS.md`, `THIRD_PARTY_NOTICES.md`
- Create: `docs/architecture.md`, `docs/connect.md`, `docs/release-checklist.md`
- Finalize/remove: `docs/replacement-deletion-ledger.md`
- Create: `tests/cutover_contract.rs`, `tests/update_contract.rs`, `tests/package_contract.rs`
- Create: `scripts/native-next/Invoke-CutoverAudit.ps1`
- Create: `scripts/native-next/Invoke-ReleaseCandidate.ps1`
- Create: `scripts/native-next/Invoke-PackageMatrix.ps1`
- Create: `scripts/native-next/Invoke-FinalSoak.ps1`

**Delete after moved behavior passes parity:**

- `src/app/` old window-owned application/runtime/process monitor implementation
- `src/state/` old UI/runtime state models
- `src/sidebar/` old terminal-centric sidebar implementation
- `src/models/` duplicated old configuration models after final callers move to `src/config/`
- `src/services/process_manager.rs` and superseded process/session ownership code after `src/process/`/host parity
- `src/ai/codex_rollout.rs` and old provider identity/transcript inference
- Superseded portions/files of `src/ai/codex_cli.rs` and old hook relays after adapters own them
- Superseded `src/remote/web/bridge.rs`, lease/presentation/input/request ownership paths after Connect parity
- Old browser pane/window ownership code after host browser service parity
- Old Git/workspace/UI ownership modules after their final single-source replacements
- Old `web/src/sessions/` routing/models/components that are not Task resources after PWA cutover
- `zz-archive/tauri-react-v0.1.11/` in full
- Obsolete development target/watch scripts, logs, archived screenshots, fixtures, and documentation identified by the deletion ledger

### Task 11.1: Close the parity ledger before deleting anything

**Files:** `docs/replacement-deletion-ledger.md`, `tests/cutover_contract.rs`, `scripts/native-next/Invoke-CutoverAudit.ps1`

- [ ] **Step 1: Turn every ledger row into a verifiable record** with old owner/path, replacement owner/path, focused tests, E2E proof, production contract impact, deletion set, and status. No row may say assumed, partial, or compile-only.
- [ ] **Step 2: Write a failing `cutover_contract` test/script** that enumerates every old module/entry/config/session/remote/browser/provider/process/prompt/UI behavior and fails while any ledger row lacks green evidence or any required replacement file is absent.
- [ ] **Step 3: Run** `cargo test --test cutover_contract parity_ -- --nocapture` and `pwsh scripts/native-next/Invoke-CutoverAudit.ps1 -Mode Parity` and save the red inventory.
- [ ] **Step 4: Re-run the named focused/E2E proof** for every non-green row; fix in its owning earlier phase rather than weakening the cutover assertion.
- [ ] **Step 5: Require explicit green evidence** for configuration/sidebar parity, process/terminal/provider/browser ownership, personal and organization prompt libraries/chains, Connect identity/realtime, update metadata, and every user-visible Task Cockpit/Command Center surface.
- [ ] **Step 6: Commit the completed ledger** as `docs: close native replacement parity ledger` only when the audit passes.

### Task 11.2: Make the new GPUI client the sole product entry

**Files:** `src/main.rs`, `src/bin/devmanager-next.rs`, `src/ui/mod.rs`, `src/host/mod.rs`, `Cargo.toml`, `tests/cutover_contract.rs`

- [ ] **Step 1: Write failing entry tests** for ordinary launch, hook-relay subcommands, `devmanager-host ctl` JSON automation, attach-first host startup, exact product/profile identity, second UI client attach, UI detach, explicit full quit, and absence of an old-runtime selector.
- [ ] **Step 2: Run** `cargo test --test cutover_contract entry_ -- --nocapture` and retain the red result.
- [ ] **Step 3: Move the proven `devmanager-next` bootstrap into `src/main.rs`** while preserving hook relay dispatch. Product launch resolves Production only for a signed/release build; debug remains fail-closed to the isolated profile.
- [ ] **Step 4: Delete the `devmanager-next` binary target/name/instance branding** and all old `app::run()` wiring. Keep one `devmanager.exe` desktop path.
- [ ] **Step 5: Search for runtime switches** (`legacy`, `new_ui`, `native_next`, `use_old`, alternate entry dispatch) and remove them unless they are test-only fixture names.
- [ ] **Step 6: Run** entry tests plus a two-client isolated launch/detach/full-quit smoke; commit as `refactor: make task cockpit the only desktop entry`.

### Task 11.3: Delete superseded Rust ownership and state paths

**Files:** deletion paths above, all replacement modules, `src/lib.rs`, `Cargo.toml`, tests, `docs/replacement-deletion-ledger.md`

- [ ] **Step 1: Add source-absence assertions** for old module exports, window-owned PTY/process/provider/browser services, duplicated config types, rollout inference, and old remote mutation bridge. Also assert that no bundled model harness, provider plugin/swarm engine, API-key execution loop, alternate isolation backend, or `pi-iso`/ProjFS runtime dependency entered the shipping product.
- [ ] **Step 2: Run** `cargo test --test cutover_contract old_rust_paths_are_absent -- --nocapture` and retain the expected red list.
- [ ] **Step 3: Move any still-needed leaf algorithm/test fixture** into its single replacement owner with history preserved where practical; run its focused test immediately.
- [ ] **Step 4: Delete entire superseded modules** rather than re-exporting them. Remove unused dependencies/features/build steps and update `src/lib.rs` so dependencies point inward to domain/kernel/host/services/client/UI.
- [ ] **Step 5: Run** `cargo machete` or the repository-approved unused-dependency check, `cargo clippy --all-targets -- -D warnings`, and source scans for old identifiers. Manually review every surviving `unsafe` Windows boundary.
- [ ] **Step 6: Update the ledger with deletion commit/path counts** and commit as `refactor: remove window owned legacy runtime`.

### Task 11.4: Delete legacy session/import/archive/web paths

**Files:** `src/persistence/mod.rs`, `src/ai/*`, `src/remote/web/*`, `web/src/sessions/*`, `zz-archive/`, `.gitignore`, root logs/scripts/docs, tests/fixtures, `tests/cutover_contract.rs`

- [ ] **Step 1: Write failing absence tests/scans** for `session.json` reads, session import/migration, provider rollout inference, old session-tab routes, `zz-archive`, Tauri commands/config, and old dev logs/watch targets.
- [ ] **Step 2: Run** the cutover audit and capture its red file/reference list.
- [ ] **Step 3: Delete the `session.json` loader/writer and old session fixtures/tests.** A negative compatibility test may assert the filename is not opened, but no product code retains a session model/importer.
- [ ] **Step 4: Delete old provider inference and remote bridge/lease/session-tab code** whose Task/command/event/browser/Connect replacements passed Phases 4, 8, and 9. Rebuild the PWA source fingerprint/service-worker cache from Task and Prompt routes.
- [ ] **Step 5: Delete `zz-archive/tauri-react-v0.1.11` in full** plus obsolete archived assets/logs/docs; Git history is the archive.
- [ ] **Step 6: Run** Rust/web absence/build tests and commit as `refactor: delete legacy session and tauri archive`.

### Task 11.5: Package the GPUI client and host as one product

**Files:** `Cargo.toml`, `build.rs`, `packaging/`, `.github/workflows/release.yml`, `.github/workflows/web-bundle.yml`, `tests/package_contract.rs`

- [ ] **Step 1: Write a failing package inspection test** that opens the built installer/package manifest and requires `devmanager.exe` and `devmanager-host.exe`, a working `devmanager-host ctl actions --json`, matching semantic/build/protocol identity, correct product/file descriptions, icons, resources, signatures, and no development/legacy binary.
- [ ] **Step 2: Run** `cargo test --test package_contract -- --nocapture` and a dry-run package command; retain the red missing-host result.
- [ ] **Step 3: Configure cargo-packager's explicit binary list** and release build command to produce both binaries once. Place them as siblings so attach-first client startup resolves the exact signed host path.
- [ ] **Step 4: Stamp Windows metadata:** `DevManager` for client and `DevManager Host` for host, same product/version/company, distinct original filenames/descriptions. Do not rename stock provider/helper executables.
- [ ] **Step 5: Include required assets/WebView2/runtime expectations/PWA bundle**, exclude `.worktrees`, target/evidence/test fixtures, `session.json`, dev profiles, Portal proprietary code, and secret/config data. Include SQLite/FTS and prompt-diff functionality through the application binary only; no user prompt database or organization content is packaged.
- [ ] **Step 6: Make CI verify hashes/signatures, package contents, and third-party notices** before generating updater metadata. Require exact license/provenance entries for GPUI, SQLite/FTS use, `similar` 3.1.2, the selected Phase 9 crypto core, and every shipped dependency; run package contract and commit as `build: package desktop and durable host together`.

### Task 11.6: Make update detection and handoff correct and identity-preserving

**Files:** `src/updater/{mod,handoff}.rs`, `src/host/update.rs`, release workflow/scripts, `tests/update_contract.rs`, `tests/fixtures/update/*`

- [ ] **Step 1: Write failing tests** for installed `0.4.1` seeing signed `0.4.2`, prerelease/build metadata ordering, stale cached PWA/metadata, latest endpoint redirect/cache headers, corrupt signature, downgrade, matching version, host/client build mismatch, active-resource handoff, aborted install, stable config/remote hashes/device identities, and a prior new-architecture fixture upgrading with Tasks, operations, provider session IDs, personal prompt versions/chains, organization prompt projection references, and FTS results intact.
- [ ] **Step 2: Run** `cargo test --test update_contract -- --nocapture` and record the red result reproducing the prior missed-update symptom.
- [ ] **Step 3: Derive current version from the running binary package metadata** and parse remote versions with one semantic-version function. Append a cache-busting query/request policy and respect signed content rather than an indefinitely stale local response.
- [ ] **Step 4: Generate `latest.json` only after signed artifacts exist**, with the actual release version, direct artifact URL, signature, platform/architecture, notes, minimum protocol, and immutable artifact hash. Add CI that installs the prior fixture and verifies detection before publishing.
- [ ] **Step 5: Coordinate update with Phase 2 handoff:** inspect active resources, refuse unsafe silent replacement, drain/confirm, install both binaries atomically, start matching host, reconnect clients, snapshot/resync, and roll back/return old host ready on pre-install abort.
- [ ] **Step 6: Hash `config.json`/`remote.json` and compare host/device/pairing records** across the isolated old-to-new update. `session.json` is ignored and no legacy Task import occurs. For new-to-new fixtures, also preserve task-invitation records and compare canonical Task/prompt row hashes and behavior before/after; migration failure must leave the old binaries and database usable.
- [ ] **Step 7: Run** update matrix tests and commit as `fix(updater): detect releases and preserve connect identity`.

### Task 11.7: Rewrite product, architecture, operations, and contributor documentation

**Files:** `README.md`, `Terminal.md`, `AGENTS.md`, `docs/{architecture,connect,release-checklist}.md`, `THIRD_PARTY_NOTICES.md`, obsolete docs deletion

- [ ] **Step 1: Write a documentation link/source scan** that fails on deleted module names, old screenshots, Tauri, session import, visible control leases, provider API keys, old dev binary, automatic prompt-chain execution, pairing/task-invitation conflation, and nonexistent commands.
- [ ] **Step 2: Run** the scan and capture red references.
- [ ] **Step 3: Rewrite README** around Tasks, personal and organization prompts/manual chains, stock subscription CLIs, native GPUI, local host, worktrees/Git/services/browser, direct remote, optional Connect, privacy, installation/update, and troubleshooting.
- [ ] **Step 4: Document architecture/invariants** including process ownership, terminal/browser/provider boundaries, accepted-versus-settled operations, bounded pages/chunks, snapshots/events, client detach, semantic fallback, pairing versus task invitations, Connect privacy, one-time fresh Task/prompt DB versus later DB preservation, and failure/recovery semantics.
- [ ] **Step 5: Update AGENTS.md** with authoritative isolated development commands, host/client/process/browser gates, full serial library test, production invariant checks, exact provider identity, and release safeguards. Merge overlapping rules rather than appending phase history.
- [ ] **Step 6: Update third-party notices/license boundary** and delete obsolete plans/docs only when superseded facts are captured in current architecture/operations docs; retain approved specs/plans as historical design records.
- [ ] **Step 7: Run** link/source scan and commit as `docs: document the task kernel product`.

### Task 11.8: Run every repository's complete final gates once

**Files:** `scripts/native-next/Invoke-ReleaseCandidate.ps1`, each repo's test config, release evidence directory

- [ ] **Step 1: Make the release script fail closed** unless worktrees are clean, versions match, production baseline is captured, isolated profile/target paths are active, release signing inputs are available without printing them, and no conflicting dev process is running.
- [ ] **Step 2: Tell the user before the long Rust run** that generated test executables will appear only under the isolated target directory.
- [ ] **Step 3: Run focused Phase 0–11 tests**, then the complete Rust library suite exactly once as `cargo test --lib -- --test-threads=1`, all integration tests, `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, and release builds for both binaries.
- [ ] **Step 4: Run DevManager PWA** test/typecheck/build and bundle/source-fingerprint tests.
- [ ] **Step 5: Run Portal API/web and DevAgent complete applicable test/typecheck/lint/build gates** in their isolated worktrees; report any baseline/unrelated failure separately but do not call the release green until required owned gates pass.
- [ ] **Step 6: Run schema/protocol/crypto golden-fixture checks and the complete immutable conformance corpus.** Rebuild the conformance index from manifests/hash-chained traces, resume at least one interrupted case from its cursor without duplicate settlement, execute every supported provider-version fixture and generic unknown-event fixture, then run disposable migration dry runs, prompt version/diff/FTS fixtures, license/advisory scans, source-absence audit, and package contract. These checks measure DevManager protocol/lifecycle/rendering/cleanup—not model answer quality or worker behavior.
- [ ] **Step 7: Confirm no harness/Cargo/rustc/client/host/provider/browser/helper/Portal dev server remains** and compare production config/remote hashes plus installed PID/start time.
- [ ] **Step 8: Review complete diffs once per repo**, correct issues in one batch, rerun affected focused tests, then rerun each final gate only if its inputs changed.

### Task 11.9: Validate clean install and update in disposable Windows environments

**Files:** `scripts/native-next/Invoke-PackageMatrix.ps1`, `docs/release-checklist.md`, package/update fixtures/evidence

- [ ] **Step 1: Provision disposable Windows VM/Sandbox snapshots** with supported OS/WebView2/DPI combinations and no access to the user's production AppData or installed process.
- [ ] **Step 2: Clean-install candidate** and verify client/host signatures/descriptions, first-run empty Task/prompt DB, project configuration creation, stable Connect identity after restart, stock provider discovery/auth guidance, personal prompt create/version/diff/chain/search, organization prompt access control, browser fixture, uninstall, and zero leftovers outside documented user-data retention.
- [ ] **Step 3: Install the last old-architecture release in the disposable environment**, seed sanitized `config.json` and `remote.json` fixtures with a stable pairing code and paired device, leave legacy `session.json`, then publish/serve candidate update metadata locally.
- [ ] **Step 4: Verify the old version detects the candidate**, downloads/verifies, handles active-resource policy, installs both binaries, reconnects, preserves config/remote/device/pairing byte semantics, ignores legacy session state, and starts an empty Task/prompt database.
- [ ] **Step 5: Run a separate new-to-new upgrade fixture** containing open/closed Tasks, exact provider-session IDs, pending/settled/uncertain operations with replay policies, personal prompt versions and inserted chain links, organization prompt references/cache, an unexpired task invitation, and search index rows. Prove canonical data/behavior survives, then exercise interrupted download, corrupt signature, installer cancel, host incompatibility, schema migration failure, and power/process interruption with rollback/retry.
- [ ] **Step 6: Run uninstall/reinstall** with explicit retain/remove-user-data choices and prove behavior/documentation match.
- [ ] **Step 7: Retain package hashes, signatures, screenshots, process inventories, file hashes, logs, and pass/fail matrix**; do not install on the daily machine yet.

### Task 11.10: Run the final quality, security, and zero-orphan soak

**Files:** `scripts/native-next/Invoke-FinalSoak.ps1`, evidence matrix, `docs/release-checklist.md`

- [ ] **Step 1: Run a minimum 8-hour isolated soak** with fixture providers by default and repeated Task create/open/close, personal prompt edit/version/diff/search, manual chain insertion and composer placement, organization prompt refresh/revocation, semantic/raw switching, worktrees/Git/services, browser automation/recording/replay, desktop/phone alternation, relay restart, sleep/wake, update check, and client detach/reconnect. Run selected authenticated Claude/Codex/Cursor segments only after explicit operator authority with an explicit provider allowlist; never treat “available” as permission to launch all subscription CLIs.
- [ ] **Step 2: Inject deterministic failures** for provider crash, renderer crash, host crash/recovery, network loss, stale cursor, interrupted chunk/page, slow client, occupied external port, denied approval, cancelled/pending operation, prompt-index rebuild, organization membership revoke, and disk/store error fixtures.
- [ ] **Step 3: Track host/client/provider/browser/helper PIDs plus creation times, Job membership, handles, threads, memory, CPU, ports, terminal/browser queue depth, SQLite size/integrity, relay routes, and event lag.** Fail on unexplained monotonic growth or any orphan.
- [ ] **Step 4: Run performance budgets** for startup, attach, task switch, typing, timeline/terminal/browser updates, idle CPU, resource probe cadence, and phone network use; compare to Phase 5/8 budgets.
- [ ] **Step 5: Run accessibility/visual matrix** for themes, density, high contrast, reduced motion, 100–200% DPI, keyboard-only, screen reader inspection, narrow/wide desktop, and phone/tablet orientations.
- [ ] **Step 6: Run privacy/security negatives** for cross-Task IDs, cross-tenant organization prompts, personal-prompt upload, task-invite use as a host pairing credential, revoked device, Watcher mutation, relay plaintext, protocol downgrade, replay, malformed/oversized/reassembled frame, path escape, secret leakage, external-process control, analytics raw/behavior fields, and update signature.
- [ ] **Step 7: End with explicit full quit** and require zero owned processes/Jobs/helpers/listeners/routes, valid SQLite integrity/replay, no unhandled errors, and unchanged production invariants.

### Task 11.11: Obtain release approval, integrate, publish, and remove development scaffolding

**Files:** branch/worktrees, release workflow/metadata, `scripts/native-next/*`, docs/deletion ledger

- [ ] **Step 1: Prepare an approval packet** with architecture/parity summary, complete gates, conformance index and replay results, package/update matrix, provider/browser/prompt/Connect proofs, security review, accessibility/performance, soak/resource graphs, known limitations, rollback, config/remote preservation, one-time fresh Task/prompt-state behavior, later database preservation, and exact candidate hashes/version.
- [ ] **Step 2: Stop and request explicit user approval** to merge/push/tag/publish/install. This plan does not grant those production/repository actions by itself.
- [ ] **Step 3: After approval**, use `superpowers:finishing-a-development-branch`; merge each repository in dependency order, rerun the actual merged-tree focused/release gates, and push only approved branches/tags.
- [ ] **Step 4: Publish signed artifacts and `latest.json` atomically**, verify public URLs/signatures/cache behavior from a clean client, and monitor update detection without installing on the user's daily machine unless separately requested.
- [ ] **Step 5: Remove development-only launch identity/profile scripts** that are no longer useful, or rename narrowly useful isolation scripts to their permanent contributor names. Delete the completed deletion ledger only after all rows are represented by final architecture/release docs and Git history.
- [ ] **Step 6: Remove stale worktrees/branches only after merged-tree verification** and only for exact resolved paths. Confirm main worktrees are clean and no dev/test process remains.
- [ ] **Step 7: Create release notes** that clearly state the one-time fresh Task/session/personal-prompt start, preserved configuration/pairing, distinction between device pairing and Task invitations, subsequent-update Task/prompt preservation contract, provider subscription requirements, new host process, Connect/privacy boundaries, and rollback/support path.

## Phase 11 verification gate

- [ ] `Invoke-CutoverAudit.ps1` reports no legacy runtime, session importer, old UI/remote ownership, Tauri archive, compatibility switch, or development product binary.
- [ ] All Rust focused/integration tests and `cargo test --lib -- --test-threads=1` pass from the final merged tree.
- [ ] Rust formatting/clippy/release builds and DevManager web test/typecheck/build pass.
- [ ] Required Portal/DevAgent gates and disposable migration tests pass for shipped Connect features.
- [ ] Package contains exactly the intended signed binaries/resources and no production/dev secrets/data.
- [ ] Clean-install and old-to-new update matrices pass with stable `config.json`, `remote.json`, host/device/pairing identity, and an intentionally empty first Task/prompt database; new-to-new update fixtures preserve Tasks, provider-session IDs, operations, personal prompts/chains, organization prompt references, and Task invitations.
- [ ] Every immutable conformance case and supported provider-version fixture replays or resumes to the expected hash/result with no duplicate effect; ambiguous non-idempotent delivery becomes visible `Uncertain`, and unknown optional data uses bounded generic fallback.
- [ ] Real stock-provider, exact prompt insertion, and LLM-controlled-browser scenarios pass on the packaged candidate without benchmarking model intelligence.
- [ ] Direct/hosted desktop-phone realtime, E2E opacity, roles/revocation, and update-without-re-pair pass.
- [ ] Accessibility/visual/performance/security matrices and the 8-hour soak pass.
- [ ] Explicit full quit leaves zero Jobs/processes/helpers/listeners/routes; no test/Cargo/rustc process remains.
- [ ] The user's installed DevManager PID/start time and production hashes remain unchanged until separately approved installation.
- [ ] The user explicitly approves integration/publication/installation scope before those actions occur.

## Phase 11 exit criteria

- Only one desktop architecture ships: native GPUI client plus durable Rust host.
- All old window-owned runtime/session/UI/remote/browser ownership paths, migration shims, feature switches, duplicated models, old web session routes, and Tauri archive are deleted.
- Existing left-sidebar configuration and Connect host/device/pairing identity survive direct load and update; task invitations remain distinct and stable according to their own expiry/revocation policy; old session/conversation state is intentionally ignored.
- The old-to-new cutover starts with an empty Task/personal-prompt database exactly once; all later updates preserve and migrate Task, operation, provider-session, prompt-version, chain, search, and organization-projection state atomically.
- Signed update metadata reliably exposes a newer release and installs matching client/host binaries without silently sacrificing live work.
- Packaged real-provider, prompt, browser, local workspace, direct/hosted Connect, and management flows pass with correct security/privacy/cleanup.
- Release evidence demonstrates compatibility/conformance replay, accessibility, performance, E2E opacity, store integrity, and zero orphaned resources without model-quality or employee-behavior scoring.
- Merge/push/tag/publish/install occur only after explicit approval and final merged-tree verification.
