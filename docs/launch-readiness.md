# Launch candidate, 2026-09-07

Candidate branch: `codex/launch-ready-20260907`. Windows and Linux are required
launch platforms. macOS is outside the verified launch scope because no Mac
test machine is available. This Linux machine cannot certify the Windows
desktop or installer.
The candidate is not release-approved.

## Implemented

- Combined `ui-redesign-w4` and `ui-redesign-clean`: panel grid/regrid, narrow
  titles, refusal messages, age refresh, composer hints and pointer fixes.
- Completed pin/unpin, panel swap selection and zoom ownership.
- Iterated on the visible Wayland native shell: corrected stale task-switcher
  state, focus after dismissal and before the first click, shared overlay input
  keyboard registration, shortcut redraws, Escape zoom exit, and Unicode-aware
  word deletion. Added a debug interactive
  fixture mode using the canonical shell and shared controls.
- Verified strict contrast for both default themes and enforced production
  color ownership through the shared token layer.
- Added Claude task-list settings and correlated pending/active/completed
  progress with stable replacement lineage.
- Added host-qualified child conversation tabs, bounded tool output and final
  messages. Child attribution requires explicit current Claude session/agent
  identity; old clients receive the compatible projection. Provider capability
  limits are recorded in `provider-ux-capabilities.md`.
- Rebuilt the pinned Connect WASM artifact and browser/PWA bundle; updated
  compatible browser dependencies to clear the audit findings.
- Corrected Unix terminal teardown, retained Git directory identity comparison
  and process-group reaping. Corrected portable path fixtures and test-owned
  process cleanup without relaxing production authority checks.
- Prepared Windows candidate CI and repaired fresh-checkout WASM preparation
  in both candidate and release workflows. Artifact hashes are verified before
  restoring ignored compiler inputs; each checkout owns its Cargo target.

- Enabled the real Linux local host with same-user authenticated Unix sockets,
  exclusive retained host locks, and parent-bound cleanup using pidfds. The
  Windows path shares the existing handshake, framing, and delivery machinery.
- Updated obsolete protocol fixtures to use host-authorized task creation.
- Enabled bounded Linux provider probes behind an owned exec barrier. One tracer
  thread retains fork/vfork/clone descendants, exact process handles and cleanup;
  task-owned interactive provider runtimes still need this ownership integration.

## Verification evidence

Local logs are sibling files of the isolated worktree:
`/home/robin/Projects/devmanager-launch-20260907-*.log`.

| Check | Observed result |
| Linux supervision behavior | Three passed: exec barrier, detached child/root exit, thread exec, drop cleanup and unrelated-child isolation (`linux-production/process-tests.log`) |
| Provider identity integration on Linux | 41 passed (`linux-production/provider-identity-tests2.log`) |
| Real Linux probe output, environment and tree cleanup | Three passed (`linux-production/probe-tree-tests2.log`) |
| Mismatched executable at Linux exec barrier | Passed (`linux-production/probe-gate-test2.log`) |
| Linux local socket / host lock tests | Two passed (`linux-production/socket-test.log`, `lock-test.log`) |
| Cross-platform local IPC integration | 11 passed (`linux-production/protocol-tests2.log`) |
| Host entry and drain tests on Linux | Nine passed (`linux-production/host-tests.log`) |
| --- | --- |
| Native UI library tests | 998 passed, six existing ignores (`live-ux/ui-tests-final.log`) |
| Correlated child hook/protocol/scope tests | Six passed |
| Provider settings tests | 104 passed |
| Default-theme/source ownership tests | 29 passed |
| Git regression group | 126 passed |
| Browser tests | 606 passed, one existing skip |
| Browser typecheck and production/PWA build | Passed |
| Browser dependency audit | Zero findings |
| Final all-target compiler check | Passed (`launch-evidence/live-ux/all-target-final.log`) |
| Final formatting check | Passed |
| Native-shell integration tests | 21 passed (`native-integration-final.log`) |
| Cleanup, probe, direct CLI and read-only lease regressions | Four passed (`cleanup-final3.log`) |
| Fresh-checkout preparation | Valid restore and four corrupt/escaping-input refusals passed |
| Complete serial Linux library attempt | 4,052 passed, 22 failed, 12 ignored (`lib-full4.log`) |

The complete attempt is **not green** and needed intervention for one stopped
probe child. Its Cargo process and harness exited afterward; no compiler or
executable under the isolated target remained. The process helper was built
first, no external `DEVMANAGER_PROFILE` was set, and no concurrent Rust run was
active. These counts do not certify edits made after that binary was built.

The remaining Linux failures include explicitly unavailable browser repair
retention, protected host trust, process identity observation, file mutation,
provider launch and Windows installer inspection. Four provider session tests
also expose the unsupported platform's legacy environment-map codec; three
live port-forward tests exceed the Linux inventory deadline. These paths have
not been declared supported or silently skipped. Fresh Windows and Linux runs must establish the launch-platform verdicts.

The broad run also exposed a pre-exec stopped-child deadlock in the Linux
probe, retained cancellation socket handles, and a read-only fixture requesting
write authority. The earlier cleanup slice refused Linux probes before spawning, released
cancellation-owned sockets, and used the read-only issuer in that fixture.
Linux probes now use the separately tested descendant supervisor above. This does not convert the earlier
full-suite result into a green run.

## Live desktop inspection

The rebuilt native shell now launches with its real sibling host on this Linux
Wayland desktop, using an isolated debug profile. Full canonical synchronization
reached Ready in 1,938 ms on the first recorded launch. The empty workspace and
Add a project dialog were inspected; physical typing appeared in its name field.
The exact parent-bound host exited when the owned app closed. Provider discovery
currently reports an invalid PATH directory; interactive provider runtime ownership also remains a
launch gate. This is not yet provider-terminal acceptance.

The earlier interactive fixture inspection below attaches no host. Current real
host evidence is under `launch-evidence/linux-production/`.

Full-window screenshots at approximately 1,523 × 834 logical pixels were
compared with composition A: board placement, two-panel layout, compact header,
focused-panel outline, and attention colors follow the reference. This is
Linux fixture evidence, not the outstanding Windows production capture.
Physical keyboard and mouse checks verified search before the first click,
Escape followed by zoom, Escape unzoom, whole-word composer deletion, new-task
and rename dialog opening, rename Escape dismissal, and divider drag/release.
Private screenshots and gesture evidence are under the isolated worktree's
`launch-evidence/live-ux/` directory. The debug command is documented in
`native-ui-system.md`.

## Remaining launch gates

- [ ] Complete Linux provider ownership and discovery,
      protected storage, workspace mutation and process reporting.
- [ ] Pass Linux integration/serial library checks, real provider desktop input,
      restart/recovery and native package installation.

- [ ] Obtain a green Windows candidate workflow on the final commit, including
      all-target compilation, serial tests, browser checks and packaging scans.
- [ ] Capture the full rebuilt native shell at reference geometry and compare
      composition, spacing, hierarchy and palette with the approved references.
- [ ] Use the actual rendered provider prompt to verify immediate keyboard text,
      control-key editing, drag/copy and wheel scrolling; exercise divider
      dragging, questions/permissions, exact resume/recovery and restart state.
- [ ] Verify signed packages and isolated install/update behavior on Windows.
- [ ] Push and review the candidate, then complete release promotion.

GitHub authentication is unavailable here: HTTPS has no credentials and SSH
was rejected. No branch, tag, installer or release has been published. The
candidate workflow deliberately has no publication step.

## Isolation

The candidate's source and target are isolated under
`/home/robin/Projects/devmanager-launch-20260907`. The daily checkout's original
index and merge metadata were preserved in private local `launch-evidence`
before normalizing CRLF-only changes; its staged tree was unchanged.

Production `config.json` and `remote.json` hashes match the captured baseline.
No installed DevManager process was present at baseline or after verification.
No installed app, provider conversation or production configuration was changed.
`launch-evidence` contains private local verification and recovery data and is
not part of the candidate commit.
