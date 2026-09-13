# DevManager Agent Guidance

## Test and installed-app isolation

- Rust tests run generated harness executables from `target\debug\deps`; these
  are expected test processes, not installed DevManager instances. Tell the
  user before a long Rust verification run and confirm no harness, Cargo, or
  Rust compiler process remains afterward.
- The first Rust test in a new worktree may cold-build for several minutes.
  Give it at least a 600-second command timeout. If its wrapper times out, track
  the exact Cargo process tree until it exits before rerunning; a wrapper timeout
  is not proof that Cargo or rustc stopped, and duplicate builds are forbidden.
- A fresh worktree fails `cargo check` until `web/src/connect/wasm/` is copied
  in from the main working copy: that directory is gitignored but its files are
  inputs to `build.rs`'s bundle fingerprint, so a checkout holds 183 of the 188
  files the hash needs. The error's suggested recovery command is not the fix.
- A full `cargo test --lib` run while another Rust build or test run is going
  on this machine means nothing: measured 2026-09-04, the same binary went
  13 red plus a harness crash (STATUS_STACK_BUFFER_OVERRUN) under load, 7 red
  quiet at `--test-threads=4`, 5 red serial, and every red in every run was on
  the pre-existing list. The extra reds under load are cancellation, race and
  pipe tests. Two sessions building at once should take turns for the full
  run, and a red list from a loaded machine is not evidence about the code.
- Give every concurrently active Rust worktree its own `CARGO_TARGET_DIR`.
  Sharing one target directory serializes builds behind Cargo locks, obscures
  which worker owns compiler processes, and can turn a focused task into a false
  timeout. Before the first Cargo command, resolve and print the target path and
  fail if it is not beneath the active isolated worktree or an exact
  `C:\Temp\devmanager-*` root. A repository-level `.scratch` directory is still
  part of the daily checkout and is forbidden even when the worker's source
  cwd is a worktree. Cleanup checks and any termination must be scoped to the
  exact worker descendant tree, worktree, and target directory; never kill
  unrelated global Cargo or rustc processes. If a worker violates this rule,
  stop that exact worker tree, preserve its source diff, and move/clean only the
  verified generated target before continuing.
- If an isolated target exhausts disk space or linker/PDB capacity, first join
  the exact Cargo, rustc, and linker tree and verify its target path. Clean only
  that generated target, then rebuild it with incremental compilation and debug
  symbols disabled when the verification lane does not require them. Never start
  a retry while an orphaned compiler or linker still owns the target.
- A delegated Cursor CLI wrapper is an owned long-lived process until its exact
  wrapper and all descendants have exited. An agent response or source commit
  is not proof that the Cursor wrapper stopped: it can continue executing a
  queued Cargo command and can restart that command after the agent reports
  completion. Before accepting delegated work, join or terminate the exact
  Cursor wrapper tree and verify no descendant still references its worktree or
  target directory.
- Keep `persistence::app_config_dir()` fail-closed under `cfg(test)`. Unit tests
  must resolve beneath the process-unique test root and must never fall back to
  `%APPDATA%\com.userfirst.devmanager`.
- Run the complete Rust library suite as
  `cargo test --lib -- --test-threads=1`. Remote and persistence tests currently
  mutate the process-global `DEVMANAGER_PROFILE` environment variable under
  separate locks, so parallel full-suite execution can race and poison the
  remote profile lock. Focused tests that do not mutate profile state may run
  normally.
- In a fresh isolated target, build `devmanager-process-test-helper` before the
  complete library suite. Process-lifecycle tests launch that exact sibling
  executable; letting the suite start without it creates misleading spawn
  failures that are test-environment defects rather than product regressions.
- Do not set an external `DEVMANAGER_PROFILE` for the complete unit-test suite;
  individual profile-sensitive tests own and restore that variable.
- Every Rust integration train must pass `cargo check --locked --lib --bins
  --tests` in an isolated target before it is considered merge-ready. During a
  large union, land bounded source waves and keep one compiler-tail owner; do
  not defer compiler reconciliation across many independently source-clean
  trains or start parallel all-target checks.
- When work concerns persistence, runtime ownership, or process reporting,
  compare the production `config.json` and `remote.json` hashes before and
  after verification and confirm the installed DevManager PID and start time
  are unchanged. Treat `session.json` separately because the running installed
  app may legitimately update its active/open-tab state.

## LLM conversation identity

- Treat provider conversation identity (`providerSessionId`) as distinct from
  disposable PTY identity. Capture it only from correlated, current-generation
  Claude/Codex `SessionStart` hooks, preserve it while its tab remains open, and
  never infer it from cwd, timestamps, or transcript ordering. An exact-resume
  failure must remain visible and must not fall back to a fresh conversation.
  A provider that explicitly reports session identity as unsupported may accept
  first-turn input with no provider session ID only when the exact Task, Agent,
  runtime generation, and action epoch still match. Never synthesize an ID; once
  a correlated current-generation hook binds one, every later write must match it.
  Before full verification, test the production client/host capability
  intersection for conversation queries and deterministic `SessionStart`
  mismatches both before and after live-process publication. A mismatch must
  settle the exact lease, remove the correlated adapter and relay nonce, and
  surface the task failure.

## Host transport ownership and wire truth

- Arm RAII cleanup ownership before the first await that can transfer a resource
  into an executor or registry, and remove the resource immediately when its
  registration acknowledgement cannot be delivered.
- Keep bounded-output admission permits owned through encode, physical write,
  and flush. Cancellation generations may suppress only queued writes that have
  not started; every successful in-flight write still advances the physical
  delivery cursor.
- Finalize resync fields that claim wire delivery in the single writer after any
  earlier in-flight frame settles. Release, resync, retarget, and output shutdown
  must invalidate queued generations and leave no executor-held connection owner.
- A terminal durable fact sent on the critical lane must never cancel, leapfrog,
  or imply delivery of earlier admitted durable facts. Fence each output at its
  captured admitted/physical high-water, let healthy outputs finish independently
  under one absolute deadline, and disconnect a lagging output without sending it
  a sequence-skipping terminal. Use persistent per-stream progress notification;
  do not add a channel allocation to every ordinary durable event.

## Git child-process discipline

- Every host-authorized read-only Git child must set `GIT_OPTIONAL_LOCKS=0`.
  Commands such as `git status` may otherwise refresh the index and make the
  fail-closed repository graph validator observe its own read as a mutation.
  Do not set this variable for authorized mutation children; they must retain
  Git's normal locking guarantees.

## Native live-shell discipline

- A terminal/process wait actor that is synchronously joined during provider
  teardown must never block on a manager lock held by that teardown path. Use
  nonblocking admission and defer reconciliation to the existing bounded retry
  lane. Lifecycle acceptance must verify the exact provider process and durable
  running-process ledger are both gone; a changed task badge or hidden pane is
  not proof that teardown completed.
- Treat first-paint task preview and canonical conversation attachment as one
  recovery contract. A transient snapshot or query failure may fail closed, but
  it must retire and reconnect the exact client, retry canonical synchronization
  off the UI thread, and keep every snapshot request within its bounded IPC
  deadline. Never leave a preview-only inbox permanently disabling task
  selection or the composer, and never weaken durable corruption validation to
  recover. Every fresh native input gesture must also rearm the current focus
  epoch before it mutates the composer.
- Provider discovery and exact-session restore must never monopolize the host's
  local request executor. Prepare the bounded local state synchronously, then
  poll cancellation-owned restore futures alongside inbox and cockpit requests;
  dropping the executor must cancel them. Prove task-list latency while restores
  are still in flight, not only after every provider has connected.
- Browser Connect acceptance must negotiate limits for the physical carrier,
  prove multi-page initial task and semantic-conversation synchronization over
  the encrypted Noise path, and decode MessagePack binary cursors in the exact
  shape emitted by the WASM bridge. Compact optional terminal styling before
  serialization while preserving text, cursor, mode, and fence identity; then
  measure host CPU with a live terminal open so a bounded wire response does not
  hide a hot refresh loop.
- Automatic cockpit refresh must be idempotent and query only the active dock
  surface. Keep read-only file browsing on the least-authority path, use bounded
  pagination, and validate a handle-relative snapshot at stable root boundaries
  rather than once per child. A client deadline must cover every sequential
  bounded host phase plus cleanup; cancellation of one panel query must not poison
  the shared host transport or erase unrelated panel projections.
- Watch-mode reload must stop only the exact isolated live app and sibling-host
  paths, then verify both processes have exited before copying or launching new
  binaries. A process stop request or unlocked executable alone is not proof that
  the old profile endpoint has been released. Never recursively terminate through
  a `conhost.exe` descendant: stop the exact verified debug-app root and wait for
  its profile-owned sibling host to exit. In PowerShell process scripts, do not
  reuse the reserved automatic `$Host` variable; use a task-specific name such as
  `$hostProc`.

## Durable orchestration journals

- Historical task replay used to reconstruct a later effect fence must consume
  the complete task-scoped event stream. Do not filter replay to events marked
  as task mutations: provider-delivery and operation-terminal facts can clear
  transient state required by a later valid decision. Prove this path with at
  least two provider inputs separated by delivery before validating a later
  resource-release snapshot.
- Never use a current-state projection row as a resume cursor until it is
  correlated to its exact durable event lineage in the same transaction. Fixed
  multi-step journals must be an exact ordered prefix during normal projection,
  runtime resume, and projection rebuild; duplicates, holes, foreign lineage,
  and out-of-order facts fail closed.
- Before a cleanup journal records an outbox residue count or progresses work,
  validate the complete receipt, event sequence, planned effect, and operation
  fence lineage. Corrupt storage is an execution error, never a synthetic
  cleanup-failure outcome.

## Native first-paint acceptance

- Measure native startup as two separate intervals: process-to-window and
  window-to-first-task-row. Open the stable workspace before host bootstrap;
  a task-only preview may populate the inbox but must never install the
  canonical `ClientModel` or enable mutations. Only the completed full
  snapshot/replay handoff may do that.
- On Windows, GPUI headless applications own process-global native state. Run
  each independently named headless test in a fresh copy of its test harness,
  or consolidate its scenarios into one `Application` lifetime. Tests that
  render the idle conversation photo must install an in-memory image; do not
  let a detached network task outlive the GPUI application during teardown.
- A visible native preview must mount the canonical shell and let snapshot and
  replay work advance on GPUI's event loop before capture. Preserve bounded
  settle options across request revalidation, include the explicit settle in
  the preview's one absolute deadline, and use an executor timer rather than a
  blocking thread sleep. For reference-driven visual work, capture the complete
  canonical shell at representative reference geometry and compare its primary
  composition, spacing, hierarchy, and palette against the reference. A small
  crop proves only that the native host can paint; it is not visual acceptance.
  On mixed-DPI Windows desktops, an oversized WGC window may return black below
  the shortest compositor span even when its dimensions validate. Request the
  canonical physical size at the desktop scale, stage the oversized position,
  flush DWM, then issue a real move-only transition farther off-screen and flush
  again before capture; inspect the bottom pixels, not only the PNG dimensions.
- Native terminal input acceptance requires a Computer Use pass against the
  actual rendered provider prompt in the rebuilt live shell. Verify physical
  key text appears immediately, a control-key edit works, drag selection can be
  copied, and wheel scrolling moves and restores the terminal buffer. Focus
  handles, optimistic-echo unit tests, or a visually correct static capture are
  necessary diagnostics but are not live terminal acceptance by themselves.

## Lean phase execution

- Aim for a focused RED/GREEN/review phase slice in roughly 30 minutes. This is
  a planning target, not a hard cutoff. If one slice reaches 60 minutes, stop and
  reassess its ownership, dependencies, test scope, build isolation, recent file
  writes, and process activity. Sixty minutes is a progress checkpoint, not a
  termination signal: let a productive single-writer lane continue for 75–90+
  minutes when it is making verifiable progress. Stop only for a verified stall,
  unsafe overlap, isolation breach, or an exited process. A wrapper timeout is
  not proof that its agent exited; inspect the exact child PID and worktree before
  resuming, and never start a duplicate writer over a live or preserved diff.
