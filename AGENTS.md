# DevManager Agent Guidance

## Test and installed-app isolation

- Rust tests run generated harness executables from `target\debug\deps`; these
  are expected test processes, not installed DevManager instances. Tell the
  user before a long Rust verification run and confirm no harness, Cargo, or
  Rust compiler process remains afterward.
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
