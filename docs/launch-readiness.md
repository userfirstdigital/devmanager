# Launch candidate, 2026-09-07

Candidate branch: `codex/launch-ready-20260907`. Windows and Linux are required
launch platforms. macOS is outside the verified launch scope because no Mac
test machine is available. This Linux machine cannot certify the Windows
desktop or installer.
The candidate is not release-approved. The latest complete Linux library run
is green: **4,111 passed, zero failed, 20 ignored** on `d61142e4`. Native
client/host integration (21 tests), exact-resume mismatch (four tests), file
integration (83 tests), and all-target compiler checks passed. Real Windows
desktop/install validation, Linux embedded-browser and package/update support,
Cursor discovery, cross-filesystem file cleanup, historical stranded recovery
receipts, and live remote enrollment still require acceptance.

## Changes panel and portable candidate follow-up

The native Changes pane now lists the host's bounded changed-file entries,
shows exact-repository diffs, and provides Stage/Unstage actions through the
existing task/revision/focus-fenced dispatcher. Read-only repositories keep diff
access and disable mutations. A Review and commit control opens the existing
full Git window. The live Linux pass displayed the provider-created
`launch-check.md`, rendered its added line, staged and unstaged it, and opened
the commit window. The Git index independently confirmed both transitions.

The pass also exposed a saved Changes pane whose initial host refresh still
used the default dock tool. Canonical task follow now restores the pane's tool
before its initial refresh, without adding a periodic query loop. Global
startup status/settings requests are captured before that pane query wave, so
they cannot invalidate its pending replies. The regression exercises the
production reply-admission predicate as well as initial requests.

Debug candidate launchers explicitly select the extracted directory as their
isolated workspace/profile root. They no longer require the original build
machine's source directory. The override is rejected by release builds.
Candidate manifests include hashes for the generated launcher and README.

Evidence: `linux-production/changes-final-*.log` (1,005 UI tests passed, six
existing ignores; all-target check and app/host build passed),
`changes-live-{row,diff,staged,git-window}.png`, and
`candidate-entry-*.log` (two portable-entry tests, all-target check, build).
The final native follow-up passed 365 tests (five existing ignores), the
all-target check and app/host rebuild (`changes-admission-*.log`). A fresh
live process populated the persisted Changes pane without any tab gesture
(`changes-admission-live.png`). Exact app/host shutdown and compiler/harness
cleanup passed; production config/remote hashes remained unchanged.
These are private local evidence files, not release archive inputs.

## Fresh-profile acceptance

An extracted Linux archive launched from `/tmp` with its own profile beneath
the extraction directory, whose name contained spaces. Every manifest file hash
matched after extraction; the original source path was not used as its profile.

That clean-profile pass exposed a blank center canvas with Add project buried
in the board menu. The empty workspace now shows an explicit Add project button
using the shared component and the existing canonical-authority-gated form.
Physical clicking opened the live form with its name field focused. Connecting
and recovery states retain the canonical shell and explain the state in place.
The native suite passed 365 tests (five existing ignores), and the all-target
check and app/host build passed. Logs/captures:
`linux-production/empty-workspace-*`. Owned app/host and automation sessions
were joined after inspection.

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
  task-owned interactive provider runtimes use the cgroup backend below.
- Added a tested Linux native PTY custody backend using a systemd user service
  and delegated cgroup v2 workload. It executes the selected native descriptor,
  acknowledges resume, preserves job control, and cleans up on host/guardian
  loss. Task/session registry integration retains exact completion authority.
- Resolved Linux Claude installation links and Codex npm native distributions,
  retaining the selected native identity through registry revalidation. Enabled
  interactive metadata probes with nonblocking input and bounded cancellation.

## Verification evidence

Local logs are sibling files of the isolated worktree:
`/home/robin/Projects/devmanager-launch-20260907-*.log`.

| Check | Observed result |
| --- | --- |
| Linux cgroup PTY OS integration | Four passed explicitly with `--ignored`: exec/resume gate, expired/abandoned launch, job control, root exit, detached children, connection loss, guardian crash/pause, idle wakeups (`linux-production/cgroup-tests6.log`) |
| Cgroup backend all-target check | Passed (`linux-production/cgroup-final-check6.log`) |
| Linux owner-death and tree cleanup | Four passed (`linux-production/death-owner-tests.log`) |
| New private Linux profiles / existing permissions | Passed (`linux-production/private-profile-test.log`) |
| Startup edge-case all-target check | Passed (`linux-production/private-startup-check.log`) |
| Linux provider discovery identity | 44 passed (`linux-production/discovery-identity-final.log`) |
| Linux registry PATH and override aliases | Passed (`linux-production/discovery-registry-final.log`) |
| Interactive probe exchange, cancellation and full-pipe deadline | Two passed (`linux-production/interactive-probe-tests.log`) |
| Latest Linux all-target check and native binaries | Passed (`linux-production/discovery-check2.log`, `discovery-build2.log`) |
| Linux supervision behavior | Three passed: exec barrier, detached child/root exit, thread exec, drop cleanup and unrelated-child isolation (`linux-production/process-tests.log`) |
| Provider identity integration on Linux | 41 passed (`linux-production/provider-identity-tests2.log`) |
| Real Linux probe output, environment and tree cleanup | Three passed (`linux-production/probe-tree-tests2.log`) |
| Mismatched executable at Linux exec barrier | Passed (`linux-production/probe-gate-test2.log`) |
| Linux local socket / host lock tests | Two passed (`linux-production/socket-test.log`, `lock-test.log`) |
| Cross-platform local IPC integration | 11 passed (`linux-production/protocol-tests2.log`) |
| Host entry and drain tests on Linux | Nine passed (`linux-production/host-tests.log`) |
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

### Latest serial Linux run and Git follow-up

The September 7 `full5` run on `9d2620b1` completed with **4,041 passed,
59 failed, 20 ignored**. The process helper was built first; four correlated
session mismatch tests and all 21 native client/host integration tests passed
before the serial library suite. No external profile or concurrent Rust build
was used. All owned test/build processes exited, and production config/remote
hashes stayed unchanged.

The Git failure cluster was the resolver rejecting the standard merged-/usr
`/bin/git` alias to `/usr/bin/git`. The follow-up accepts only exact system-owned
Linux aliases, retains canonical executable verification, and still rejects
user-controlled directory aliases. All **127 Git tests passed**, including
stage/unstage/commit and replacement/authority regressions. The host cockpit
suite passed **37 of 38**; only the existing Linux file-write authorization
failure remained. The all-target compiler check passed. Logs:
`linux-production/git-alias-{green,host,check}.log`.

This focused result does not replace a fresh complete green run. Remote trust,
file mutation, browser repair retention and Windows installer inspection remain
in the latest failure list. The previous port-forward failures did not recur
in `full5`; their deadline sensitivity still warrants review.

### Linux remote trust storage

The Linux store now holds a nonblocking exclusive `flock` on an owner-only,
no-follow lock descriptor. It validates owner, mode, link count and the held
file's identity before and after acquisition, creates private trust directories,
and refuses writable-by-others layouts. Encrypted record reads and rollback
reads reject final symlinks. Windows retains its existing exclusive share mode.

All **27 native remote-trust tests passed** on Linux, including concurrent device
creation, exact encrypted reload, metadata/cookie tampering, queued cancellation,
admitted-write timeout, sorted rosters and exact forget semantics. Linux tests
no longer silently return on `Unsupported`. New regressions cover exclusive
locking, replacement, symlink/hardlink and permission refusal, and no-follow
transaction rollback. The all-target compiler check passed. Logs:
`linux-production/trust-{green,check}.log`. These unit tests use process-local
custody keys; the separately recorded real wallet acceptance remains applicable.
Production config/remote hashes are unchanged and no owned build/test process
remains. A live remote enrollment/Noise session is still a launch gate.

### Linux file mutation flags

Linux recovery now opens directory descriptors read-only, as required by
`openat`/`renameat`/`unlinkat`; requesting `O_RDWR` on a directory had failed
with `EISDIR`. Sibling temporary files now explicitly request `O_RDWR`, fixing
writes failing with `EBADF`. Cleanup directories are created with private mode
from the first syscall. The existing identity/CAS and recovery checks remain.

The real host create/update/conflict regression passed, all **83 file-service
integration tests passed**, and **261 workspace library tests passed** (one
existing ignore). Coverage includes concurrent writes, permission preservation,
root/parent replacement, exact cleanup ownership and deadline recovery. Fixed
portable test cleanup of symlinks and aligned old fixtures with the existing
secret-content read refusal; the production refusal was not relaxed. All-target
check and formatting passed. Logs: `linux-production/files-{host,integration,
workspace,check}.log`. This acceptance used workspaces on the same filesystem as
the isolated temporary directory; cross-filesystem cleanup remains to be checked.
Production config/remote hashes and installed-process baseline remain unchanged.

### Linux browser resource retention

Browser repair evidence now uses a retained exclusive Linux `flock`, with
no-follow opens and owner, writable-mode, link-count and exact inode validation.
The shared runtime and every live retention lease keep that lock alive. Enabled
the portable retention tests on Linux: a separate harness process proves the
lock outlives the store while a lease remains and becomes available after final
drop; crash/reopen proves repair pins are not falsely persisted. Replacement,
symlink/hardlink, exact owner/scope, cross-root and cleanup-isolation checks pass.

All **185 matching browser library tests passed**, including both previously
failing gateway lifecycle cases. All-target check passed. Logs:
`linux-production/browser-linux-{resources,check}.log`. No owned child harness
or compiler remains and production hashes are unchanged. This enables resource
custody only; it does not supply or certify an embedded Linux browser host.

### Portable updater state fixtures

The four remaining updater state-test failures came from a Windows-only fixture
being compared against the actual Linux OS/architecture. The fixture now selects
the current platform and its packager format. All **18 updater library tests**
and the all-target check passed (`linux-production/updater-platform-{fixtures,
check}.log`). No production updater behavior changed. Real Linux package
construction, paired binary names/staging and install/update acceptance remain
separate gates; these state tests do not certify an installer.

### Complete serial Linux verification, full6

On `d61142e4`, helper build, four exact-resume mismatch checks and 21 native
client/host contract checks passed before `cargo test --locked --lib --
--test-threads=1`. The library run completed with **4,111 passed, zero failed,
20 ignored**, in 232.61 seconds. No external `DEVMANAGER_PROFILE` was set and
no other Rust build/test or isolated app was running. All owned compiler,
harness and app processes were absent afterward; production config/remote
hashes matched the baseline. Logs: `linux-production/full6-*.log`.

### Windows and Linux candidate automation

The candidate workflow now includes Ubuntu 24.04 with a systemd user session,
cgroup v2, desktop dependencies, all-target compilation, serial library tests,
and native/file integration checks. Both platforms build debug desktop archives
containing only the two product binaries and explicit shipping resource roots,
plus a source/hash manifest. Archive contents are checked against that allowlist
and each recorded hash. No test harness, profile, conversation or evidence tree
is included. Linux process cleanup auditing is scoped to the isolated target.

The YAML, shell and Python scripts pass parsing checks. The remote CI jobs have
not run here; Windows artifacts and acceptance still require an authenticated
Windows-capable runner. Linux dependencies were checked against the GPUI
upstream's [Linux build guidance](https://zed.dev/docs/development/linux) and
[dependency script](https://github.com/zed-industries/zed/blob/main/script/linux).
These are acceptance archives, not signed release installers.

## Live desktop inspection

The rebuilt native shell now launches with its real sibling host on this Linux
Wayland desktop, using an isolated debug profile. Full canonical synchronization
reached Ready in 1,938 ms on the first recorded launch. The empty workspace and
Add a project dialog were inspected; physical typing appeared in its name field.
The exact parent-bound host exited when the owned app closed. The latest real launch reached canonical Ready in 1,554 ms and populated Claude
and Codex model/usage metadata in the isolated profile. Cursor still reports its
non-native shell launcher as unavailable. Follow-up desktop capture could not
reliably focus the owned window; an observed narrow window also needs sizing
acceptance. Production provider launch and input still need end-to-end acceptance.
This is not yet provider-terminal acceptance.

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

Linux registry integration now retains nested cgroup membership and emits zero-process
authority only after the guardian and wrapper have settled. Completion messages
received at a deadline remain available for the exact generation on retry.
Five live cgroup checks, both registry regression tests, and
`cargo check --locked --lib --bins --tests` passed for this source wave.
No owned harness, Cargo, compiler, guardian or session cgroup remained afterward.

Linux managed ownership now feeds production terminal, provider and configured-service
launch paths. A live PTY test passed input/output, exact generation restart, member
observations and joined actor cleanup. Both installed Claude/Codex stock probes,
32 terminal regressions and the all-target compiler check passed. The rebuilt
X11 app reached canonical Ready in 614 ms; its project picker was exercised with
physical keyboard input. The 960 × 640 minimum preserves the complete dialog under
desktop tiling. Live project creation now persists the selected test folder.
Host-level configuration requests no longer invent task ownership; failed and late
replies settle only the matching dialog. Eight project UI tests, six typed-query
tests, the host persistence test and the all-target check pass. The real-window
regression verifies keyboard routing after the host closes the dialog. The new-task window test also verifies focus transfers to the composer.
Physical typing now appears in the live message box and first Send creates the
canonical task. Provider startup then fails in the Linux launch-environment
persistence codec (`key must be a string`); protected session storage is the next
acceptance gate. The exact rebuilt app and sibling host exited afterward.

## Remaining launch gates

Linux protected storage now uses the desktop Secret Service wallet for immutable
master keys and scope-bound ChaCha20-Poly1305 envelopes for provider settings,
launch environments and Noise custody. The lossless environment codec handles
native Unix bytes. Wallet operations have a three-second deadline; missing,
locked or invalid custody fails visibly without a plaintext fallback. Development
builds use a separate wallet application namespace. Ordinary unit tests use a
process-local test key and never access the user's wallet.

The real Secret Service test created, reopened and removed its unique test key;
AEAD tampering and cancellation cleanup tests passed. Provider settings: 108
passed. Provider sessions: 91 passed, one existing failure remains in the
unimplemented Linux exact-process observer. The Noise profile-binding test and
`cargo check --locked --lib --bins --tests` passed. Evidence is under
`linux-production/custody-*.log`. This does not yet establish provider desktop
input or complete Linux protected-trust support.

The rebuilt app launched an actual Codex runtime and displayed workspace trust.
Its persisted environment is encrypted. Approving exposed a UI handoff defect:
only a readiness refusal had been fetched, leaving no terminal projection for
setup input. The app, sibling host, guardian and provider all exited on the owned
stop, and their cgroup disappeared. The running-process ledger still retains the
stopped provider; process reconciliation and shutdown ledger settlement remain
acceptance gates. Production configuration hashes are unchanged.

### Linux provider handoff and shutdown, 2026-09-07

The desktop pass now completes Codex workspace approval and first-send delivery.
The exact response `linux launch ok` appears in both the real provider terminal
and the canonical Conversation view, with a correlated provider session ID.
Physical typing is immediately visible; Ctrl+W edits the provider prompt, and
physical drag selection plus Ctrl+C copies the exact answer to the desktop
clipboard. Wheel-scroll acceptance remains open: a longer completed provider
answer exposed missing automatic terminal refresh, leaving the visible terminal
on its initial Working frame. The composer also retains the delivered draft.

Setup holds now request the actual terminal screen and suspend readiness polling
while waiting for the user. The previous readiness loop invalidated the screen
reply continuously. The regression runs the production outcome handler, advances
controller work, and proves the screen remains admitted without sending input.
Linux process identity uses retained pidfds and correct clock-tick conversion;
missing, reused, live and unreaped exited processes are distinguished. Internal
provider-manager references no longer prevent last-owner shutdown. Both live
stop passes removed the new provider's exact process tree and running-ledger row.
An older debug-profile row from the pre-fix crash remains separately recorded.

Focused evidence: `handoff-identity-tests.log` (2), `handoff-manager.log` (1),
`handoff-session.log` (92), `handoff-custody.log` (3, including real wallet),
`handoff-trust-final.log` (1), `handoff-check-final.log`, `handoff-build.log`.
The wallet dependency now uses zbus's async-io backend, preserving GPUI's D-Bus
calls outside Tokio; rebuilt desktop logs no longer contain the reactor panic.
During the live terminal pass, a three-second CPU sample measured the native
shell at 1.33% and host at 3.67% of one core; this does not certify the pending
refresh fix. All owned app/host/provider processes were stopped before edits;
Cargo and harness processes were joined. Production configuration hashes match.
Private screenshots and exact identities are under `linux-production/handoff2-*`.

### Live terminal, composer and repeated resume (2026-09-07)

The rebuilt production Linux shell completed first Send, canonical reply,
automatic task naming and draft clearing. Switching from Terminal to Conversation
now disarms hidden PTY input; typing a new composer draft left the provider prompt
empty, and Send delivered that exact draft once. Unchanged host projections keep
selection in a restored draft. Exact owned Send receipts settle even if navigation
or the command's own projection has advanced, while newer text is preserved.

Visible attached terminal panes refresh at a bounded 250 ms cadence with one
in-flight read; hidden/minimised panes and panes hidden by zoom do not refresh.
Unchanged screens do not trigger redundant terminal-strip queries. The actual
provider's delayed 90-line response painted without another input gesture.
The workspace terminal now registers the shared wheel handler. Physical input,
Ctrl+W editing, drag selection with exact clipboard comparison, and wheel-up /
wheel-down restoration passed against the rebuilt native prompt. Five seconds
with the terminal idle measured the host at 2.8% and shell at 7.2% of one core;
this is a bounded refresh, but further shell repaint optimisation remains useful.

Encrypted recovery receipt cleanup now compares the decoded receipt and uses
the exact stored ciphertext for its transactional delete CAS. Re-encryption uses
a fresh nonce and previously deleted zero rows without reporting failure. A new
test reproduces that defect with protected environment data, rejects a stale
receipt, and proves deletion and idempotence. The new live task reopened the same
provider conversation twice (action epochs 1, 2, 3); after each stop, its exact
provider PID, process ledger row and recovery receipt were gone. Explicit terminal
opening now follows the legacy attachment response with bounded readiness reads,
so asynchronous exact restore paints after one click. Earlier debug conversations
with already stranded historical receipts remain preserved; migration/recovery of
those old rows is still a separate gate, not silently discarded evidence.

Validation: 93 provider-session tests passed, restored-draft selection regression
passed, 329 native-shell tests passed (five existing ignores), all-target check,
formatting and native app/host rebuild passed. These focused results do not
replace the pending complete serial library run. Evidence is private under
`linux-production/{receipt-*,resume-*,new-candidate-*,final-terminal-*}`.
Production config/remote hashes are unchanged; all owned app/host/provider and
build/test processes exited after this pass.

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
No installed app or production configuration was changed. Test-owned provider
conversations were created and exercised in the isolated project.
`launch-evidence` contains private local verification and recovery data and is
not part of the candidate commit.
