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
- Keep `persistence::app_config_dir()` fail-closed under `cfg(test)`. Unit tests
  must resolve beneath the process-unique test root and must never fall back to
  `%APPDATA%\com.userfirst.devmanager`.
- Run the complete Rust library suite as
  `cargo test --lib -- --test-threads=1`. Remote and persistence tests currently
  mutate the process-global `DEVMANAGER_PROFILE` environment variable under
  separate locks, so parallel full-suite execution can race and poison the
  remote profile lock. Focused tests that do not mutate profile state may run
  normally.
- Do not set an external `DEVMANAGER_PROFILE` for the complete unit-test suite;
  individual profile-sensitive tests own and restore that variable.
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

## Durable orchestration journals

- Never use a current-state projection row as a resume cursor until it is
  correlated to its exact durable event lineage in the same transaction. Fixed
  multi-step journals must be an exact ordered prefix during normal projection,
  runtime resume, and projection rebuild; duplicates, holes, foreign lineage,
  and out-of-order facts fail closed.
- Before a cleanup journal records an outbox residue count or progresses work,
  validate the complete receipt, event sequence, planned effect, and operation
  fence lineage. Corrupt storage is an execution error, never a synthetic
  cleanup-failure outcome.
