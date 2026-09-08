# Launch candidate, 2026-09-08

Candidate branch: `codex/launch-ready-20260907`. Windows and Linux are required
launch platforms. macOS is outside the verified launch scope because no Mac
test machine is available. This Linux machine cannot certify the Windows
desktop or installer.
Linux production packaging and a real signed 0.4.1 → 0.4.2 AppImage upgrade
pass. The final native ext4 serial library suite passed: **4,180 passed, zero failed,
22 ignored**. Exact-session identity checks, native contracts, the real WebKit
engine, CLI/updater/package contracts, and the locked all-target compiler check
are recorded below. Windows desktop and installer acceptance still require a
Windows machine; public release promotion has not occurred.

## ext4 and cleanup follow-up

Native ext4 verification exposed directory link counts changing during normal
Git object/ref maintenance and deleted inode numbers being reused quickly.
Directory identity comparisons now ignore child link counts in both pathname
and retained-handle checks. Unix configured workspace roots and provider
attestations retain their original descriptors so a replacement cannot reuse
the admitted inode. Ordinary Unix rename/unlink remains possible. SSH orphan
cleanup retains a no-follow Linux identity handle and checks the entry kind
before quarantine, preserving an unexpected regular replacement at its path.

A separate cleanup waiter race could report failure after teardown had already
completed. The waiter now observes the notification before looking up the
report. A deterministic regression reproduced the old failure; the correction
passed all 54 teardown tests and 100 repetitions of the 257-cleanup acceptance
case with the process restricted to two CPUs.

CI now gives the full Linux library suite an X11 display. Test temporary roots
are private, canonical directories outside Git checkout ancestry, so a plain
folder fixture cannot accidentally discover the runner's parent repository.
The source checkout retains its own isolated Cargo target. Private preparation
checks verified permissions, symlink rejection, Git ancestry rejection and the
reviewed WASM inputs. Verification on a private kernel-mounted ext4 volume
passed 4,180 library tests (22 ignored), 128 Git tests, six configured-root
checks, 21 native contracts, the real WebKit lifecycle, four exact-session
checks, nine CLI tests, 86 file-service tests (one ignored), 45 updater/package
checks, and the locked all-target compiler check. A further 100 linked-worktree
Stage/Commit cycles passed on two CPUs. No owned harness/compiler remained;
production settings hashes and the installed app PID/start time were unchanged.
The final AppImage and live acceptance are identified in draft PR #2; earlier
package evidence below names its own artifact.

## Linux production package and updater acceptance

The candidate now builds a standard x86-64 AppImage on Ubuntu 24.04. It bundles
both product binaries, GTK/WebKit helpers, TLS and GStreamer modules, and their
runtime libraries. Every packaged ELF uses a relative runtime search path;
provider subprocesses do not inherit a global library-path override. The
builder executes both packaged identities and checks version, protocol, and
payload/binary hashes. Release assembly and signing now include the Linux
AppImage and its updater manifest entry. See `packaging/README-linux.md`.

A whole-image updater verifies the existing release signature and immutable
artifact hash, stages the exact replacement in a private recovery directory,
and exchanges the old/new image atomically after the old host is joined. Its
journal validates the physical old/new pair during recovery; the new matching
host finalizes the handoff. The desktop execs the replacement after native
teardown, and an exec failure restores the previous image. The detached host
launches through the AppImage runtime so its mount survives client close.

Live package acceptance exposed and corrected a rejected first-send lifecycle:
a concurrent Browser mutation can invalidate the send's captured task revision.
A definitive rejection now clears the exact pending send, preserves the draft,
and keeps the healthy host connected so the user can send again. Linux CLI
acceptance also now runs the shared production transport path. Checkout binaries
ignore AppImage environment inherited from a launching IDE, while binaries
inside their own AppImage still require valid package identity.

The native settings now expose Updates, including check, download, and
install/restart, and poll the live service for asynchronous progress. Fresh XDG
config bases are created before canonicalization. Task and terminal menus stay
within the right window edge and bound their scrollable height.

The packaged AppImage passed real WebKit rendering, typing, control-key editing,
clicks, downloads and HTTPS loading. The provider terminal passed physical text,
Ctrl+U editing, exact drag/copy, and wheel scrolling through a 90-line response.
Archiving the task removed its exact provider/guardian processes and running
ledger row. The right-edge task menu stayed entirely within the native window.

The signed upgrade rejected a corrupted download without modifying the installed
image. With valid bytes, the old host exited at 0.52 seconds, image exchange
occurred at 1.47 seconds, the new client started at 2.41 seconds and the new host
at 2.85 seconds. Config/remote hashes were unchanged, the host admission remained
Open, and both recovery journals cleared. Update retirement now uses a tracked
physical acknowledgement and the exact OS process handle; it does not write a
permanent host-close journal. An exclusive profile reservation spans image commit.

The final packaged pass also exposed readiness-query starvation when another dock
refresh advanced the UI epoch during first Send. Its regression first failed with
zero sends, then passed after the exact task/runtime readiness lease gained its
own reply admission. Connection, runtime and task action-epoch checks remain in
force. The serial library suite, four session-identity checks, 21 native contracts,
real WebKit engine check, nine CLI integrations, 45 updater/package contracts,
and locked all-target compiler check passed. The final rebuilt first-send and
fallback-launch acceptance pass; these paragraphs do not constitute Windows
release approval. All package fixtures and signing keys are
private evidence and must never enter a source commit or release artifact.

The final live iteration also found three lifecycle gaps. An archived primary
conversation identity is retained for exact reopen but no longer counts as a
running host session; active resources remain independent update blockers. The
Linux browser now owns a normally realized GTK window and destroys it after
WebKit, with a regression that waits for native destruction and a packaged close
that records exit code zero. Native browser surfaces park beneath application
menus and dialogs. Closing the last task pane clears its owning selection so a
later canonical refresh cannot reopen it.

First-send readiness uses the host-attested provider launch epoch, which is
independent of the task's durable close/reopen epoch. The regression now uses
a fresh task at epoch zero and a provider at epoch one, alongside an interleaved
dock query. Provider, task, resource, runtime, and connection identity checks
remain in place.

Full native UX acceptance (`43432536dce45846d1fecc09c563786026d74d24e304fe688cc18676098dbeaf`, before the final temporary-directory recovery fix):

- The signed upgrade preserved all 77 durable events byte-for-byte, unchanged
  config/remote hashes, and Open host admission; both recovery journals cleared.
  Physical host exit preceded image exchange (0.52s vs 1.62s); the replacement
  client and host appeared at 2.59s and 3.04s.
- First Send followed immediately by Browser automatically launched the selected
  GPT-5.6 Sol model, delivered exactly one input, received `LINUX FINAL READY`,
  and cleared the draft. No manual terminal launch or send retry was needed.
- Native typing, Ctrl+U, exact drag/copy and wheel up/down passed in the final
  artifact. Browser menus and Settings park the native child and restore it on
  dismissal. Closing with Browser open exits with code zero.
- Warm reopening retained the exact host, provider PID/start time, conversation
  identity, and runtime generation. Process-to-window was 0.756s;
  window-to-first-task-row was at most 0.257s.
- Archive completed durable teardown, removed the exact provider and ledger
  entry, and persisted an empty selection/workspace. Reopening stayed empty.
- Quiet 20-second samples on the software-rendered private X11 display measured
  terminal client/host at 16.53%/1.45% of one core, browser at 5.64%/0.65%.
  These are software-display measurements, not GPU desktop benchmarks.

The serial rerun exposed a Linux file-write denial after the shared temporary
directory accumulated more than 1,024 unrelated entries. Cleanup discovery now
charges its count bound only to verified private cleanup authorities and their
records, while every scanned directory entry remains under the absolute
deadline. The regression covers more than 1,024 ordinary entries, the unchanged
limit on actual recovery authorities, and deadline expiry. The real host
FilesWrite authorization test then passed against the same crowded directory.

The native cleanup acceptance AppImage SHA-256 was
`47792f2e04334aee1265f6ed0f4aac5e0b8489a549c0e80e43a847ddb91066e3`.
Its signed upgrade preserved all 119 durable events byte-for-byte and both
config/remote hashes. The old host exited at 0.52s, image exchange occurred at
1.43s, and the replacement client/host appeared at 2.49s/2.91s. Host admission
remained Open and both journals cleared. First Send followed by Browser delivered
one accepted input exactly once, displayed `SHIPPING LINUX READY.`, and cleared
the composer. Archive removed the exact provider PID and emptied its running
ledger; the packaged client then closed with exit code zero.

The previous installed AppImage SHA-256 was
`bcbe0d447ce59eb8e36129751c2fc9c667d7ff2444cfa563d18e7a4326104f8d`.
It includes the repository's public update verification key, the official GitHub
release feed, and the browser bundle reproduced identically on Windows and Linux.
The final signed fixture upgrade preserved all **161 durable events** byte-for-byte
and both settings files. The old host exited at 0.524s, image exchange occurred at
1.458s, and the replacement client/host appeared at 2.370s/2.838s. Host admission
remained Open and both recovery journals cleared. The fixture client, host,
private display and update server all exited afterward.

This artifact was installed at `/home/robin/Applications/DevManager.AppImage`, with
a desktop launcher and icon, and was left running on the real KDE
NVIDIA desktop. Process-to-window measured 0.615s; the subsequent full 960×640
capture showed the canonical empty workspace with Add project enabled. Eight
embedded browser asset tests and the locked all-target compiler check passed
after the final browser bundle rebuild. The ext4 and cleanup follow-up above is verified separately from this artifact.

The final serial run also includes a deterministic test-only Claude hook
publication fix: SessionStart publication finishes before the test arms its
UserPromptSubmit pause. HTTP admission alone does not prove callback publication.


## Linux native browser startup and interaction

The Browser panel now opens an exact task-owned browser context and resource
through authenticated host authority. Its identity is independent of the
provider conversation and PTY. Opening is revision-fenced and idempotent;
ordinary refresh only reads an existing session. Both production peers negotiate
BrowserProjection, and initial/task-detail snapshots include contexts and tabs,
so a cold restart can attach the canonical browser rather than remain on a
preview. Partial browser snapshots and foreign/duplicate detail fail closed.
The native-only reply resolves its workspace root on the host from the saved
opaque binding; browser file operations never default to the app directory.

The Linux shell pumps the real WebKit child on its owning UI thread, waits for
the native-view receipt before attachment, parks the view while another panel
owns input, and restores its physical bounds after GTK shows it. NVIDIA systems
automatically use the WebKit DMA-BUF fallback unless explicitly overridden.
Navigation shows loading/error feedback, downloads report completion, and the
address field handles Enter through the root input focus path. A tab gesture
selects its exact task before dispatching its first query.

The rendered Linux app passed typing, control-key editing, clicks, wheel
scrolling, navigation, upload (exact selected filename and 31 bytes), and
Browser/Terminal switching. The restored provider prompt accepted typing,
control-key editing, drag selection copied to the private X11 clipboard, and
wheel scrolling away from and back to the current prompt. Failed navigation
remains visible after secret containment through a typed, URL-free load fact. A private X11 display isolates subsequent automation from
the user's desktop. The native engine harness additionally proves awaited JS,
upload bytes, rendered PNG pixels, hide/reparent/show sizing, connection-failure
reporting through containment, cancellation and
native child destruction. This is Linux evidence, not Windows certification.

Focused validation: five native-browser startup/ownership regressions, 26 client
model tests, the NVIDIA override policy, 53 UI projection tests, the production
capability intersection, and the locked all-target compiler check. Evidence is
in private `linux-production/browser-*.log` and `browser-private-*.png` files;
these are excluded from release artifacts. A 20-second idle browser sample
measured 0.95% of one core for the host and 0% for WebKit subprocesses. The
software-rendered private-display debug client used 19.31% (terminal baseline
32.95%); release rendering performance still needs the packaged-app pass. All
nine captured app/host/provider/WebKit processes exited after shutdown, no
owned harness/compiler remained, and production config/remote hashes matched.
The full serial library suite must
be rerun on the final Linux integration, including package/update changes.

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

## Historical verification evidence

The entries below record earlier iterations and their follow-up fixes. The final
Linux verdict and installed artifact are recorded at the top of this document.

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
the isolated temporary directory. Cross-filesystem and bind-mount cleanup now also pass (see below).
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

## Historical launch-gate follow-up

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

## Current launch checklist

- [x] Complete Linux provider ownership and discovery,
      protected storage, workspace mutation and process reporting.
- [x] Pass Linux integration/serial library checks, real provider desktop input,
      restart/recovery and native package installation.

- [ ] Obtain a green Windows candidate workflow on the final commit, including
      all-target compilation, serial tests, browser checks and packaging scans.
- [ ] On Windows, capture the full rebuilt native shell at reference geometry and compare
      composition, spacing, hierarchy and palette with the approved references.
- [ ] On Windows, use the actual rendered provider prompt to verify immediate keyboard text,
      control-key editing, drag/copy and wheel scrolling; exercise divider
      dragging, questions/permissions, exact resume/recovery and restart state.
- [ ] Verify signed packages and isolated install/update behavior on Windows.
- [x] Push the candidate and open draft PR #2.
- [ ] Complete candidate review and release promotion.

The candidate is pushed in [draft PR #2](https://github.com/userfirstdigital/devmanager/pull/2).
Its checks track Windows/Linux compilation, serial tests and candidate archives.
Both platforms passed browser bundle parity and Windows passed package-reference
and dependency-provenance scans. Windows all-target compilation passed. The first serial Windows library run
reported **4,277 passed, 17 failed, 12 ignored**. The failure list includes
short-path aliases in the runner's default temporary directory, CRLF-sensitive
source assertions, and process-lifecycle fixtures. CI now uses a canonical
Windows temporary directory, Rust source checks out with LF, and the Windows
process fixture launches its child directly without shell activation. The
257-cleanup regression now reports the exact failing cleanup for any remaining
failure. Its notification race is now fixed with a deterministic regression and
100 repeated stress runs. The later Windows run at `71e15e31` passed 4,279 tests
with 15 failures, all in the path-alias, source-line-ending and process-fixture
areas corrected for the next candidate. These corrections still require a green
Windows rerun. Linux CI's four initial CLI failures reproduced under shared `/tmp`;
all nine tests pass using the new private temporary directory. Consult the PR
checks for the final platform results. The separate web workflow
now restores the reviewed WASM inputs and installs the native dependencies needed
by its embedded-asset tests. Neither workflow publishes a public release.

## Isolation

The candidate's source and target are isolated under
`/home/robin/Projects/devmanager-launch-20260907`. The daily checkout's original
index and merge metadata were preserved in private local `launch-evidence`
before normalizing CRLF-only changes; its staged tree was unchanged.

Production `config.json` and `remote.json` hashes match the captured baseline.
No installed DevManager process was present at baseline. After isolated
verification, the final Linux AppImage was intentionally installed and launched
for the user; it remains open. Test-owned provider conversations were created
and exercised in the isolated project. Private release fixtures ran only while
the production profile endpoint was unowned: changing XDG paths alone does not
isolate the production abstract Unix socket.
`launch-evidence` contains private local verification and recovery data and is
not part of the candidate commit.

### Linux cross-filesystem cleanup

Overwrite and delete now quarantine private residue on the source mount when
the shared temporary directory is on another mount. Retained descriptor mount
identity also distinguishes bind mounts on the same device. Restart recovery
requires the exact source parent, file identity, and owner-only directory;
foreign replacements and untrusted directories remain untouched and hidden
from file browsing. Empty local quarantine directories are removed.

Validation: 85 file integration tests passed; the separate user/mount-namespace
bind-mount acceptance test passed explicitly; 261 workspace tests passed
(one existing ignored test); `cargo check --locked --lib --bins --tests` passed.
The cross-filesystem overwrite test reproduced the original conflict before
the fix. Production config and remote hashes remain unchanged, and no owned
app, harness, Cargo, compiler or linker remains. Private logs: `crossfs-red.log`,
`crossfs-bind.log`, and `crossfs-final-{integration,workspace,check}.log`.

### Linux Cursor native package discovery

Cursor's recognized Linux installation wrapper now resolves to its sibling
standalone native image after validating the versioned package layout, wrapper
content and runtime manifest. Probe and managed launch retain that native file
identity; neither Bash nor Node is introduced into the launch path. The adapter
recognizes the observed 2026.09.02 interactive command contract while preserving
typed unsupported exact-resume and semantic conversation capabilities. Cursor
chat Send remains unsupported on both platforms; this closes Linux discovery
and native terminal launch, not that separate provider transport limitation.

All 45 provider identity tests and nine Cursor adapter tests passed. An explicit
installed-provider test passed Claude, Codex and Cursor through the production
registry. An explicit cgroup acceptance test launched the installed Cursor into
a native PTY with an isolated HOME, observed its screen, terminated it, and
verified the exact process and cgroup were empty. Both all-target checks passed.
The rebuilt live app displayed Cursor as healthy with version 2026.09.02-c22c1a3.
The app, sibling host and automation exited; production config/remote hashes
were unchanged and no owned Rust process remained. Private evidence:
`cursor-{discovery-green,adapter,stock-probe,custody,custody-check}.log` and
`cursor-provider-list.png`.

### Narrow native workspace

The pane canvas now subtracts the board's painted width. Previously it used a
smaller fitted width while the board retained its saved width, pushing pane
actions past the window edge. Live 960×640 inspection now shows the complete
Changes pane, including View diff, Stage, Done and menu actions. Two selected
tasks fit in stacked panes at 960×640 and remain contained at 1523×834.
Validation: 365 native-shell tests passed (five existing ignored), all-target
check and native build passed; the exact app and host exited. Private evidence:
`narrow-{native,check,build}.log`, `narrow-fixed-live.png`,
`narrow-multiple-panes.png`, and `wide-multiple-panes.png`.

### Historical provider release cleanup

A release receipt stranded after encrypted-row deletion failed can now retire
after its session advances. One write transaction verifies the exact pending
receipt, released and settled historical ownership, and the complete ordered
provider state journal through the current projection. Missing, duplicate,
foreign or unconfirmed history stays visible; no newer claim is released.
The recovery scan has explicit row and byte bounds.

The success and corruption tests passed, along with all 95 provider session
tests and the all-target check. In the rebuilt live isolated profile both old
receipts cleared. The existing 42 journal rows and historical settlement rows
remained unchanged. One previously blocked task then restored the same provider
conversation at action epoch 3, appending five legitimate journal rows. Its
exact provider PID 3752185 and the app/host/guardian all exited. Production
config/remote hashes were unchanged. Private evidence: `historical-*.log`,
`historical-live.png` and `historical-live-result.json`; the pre-run database
backup remains private in launch evidence.


### Linux remote settings and live browser chat

Remote-access InputState fields now keep their own keyboard handling before
terminal routing. Physical Ctrl+A and digit entry changed the port to 43882;
Apply & restart exposed the real listener there, and restoring 43872 also
succeeded. Browser pairing instructions now match Settings → Remote access →
Pair a device. Native tests: 365 passed, five ignored; browser transport,
identity and session tests: 86 passed. Web build and all-target Rust check passed.

The actual listener paired the browser, loaded canonical tasks/history and
accepted `Reply exactly linux remote path verified`. Kernel events 97–100
separately record input acceptance, operation acceptance, physical delivery and
settlement; the expected response appeared in conversation and terminal views.
The provider conversation ID remained unchanged. The browser reconnected after
host restart without another pairing code. A ten-second idle live-terminal sample
measured 1.3% of one core in the host and 4.4% in the native app.

A production browser transport with a negotiated two-item limit loaded all five
tasks over three encrypted snapshot pages, including binary resume cursors.
It exposed a separate conversation limit defect: one page contained 38 facts.
That defect still needs correction before multi-page conversation acceptance.
Private evidence: `linux-production/remote-*`. Owned app, host, guardian,
automation and Rust processes were joined; production configuration hashes did
not change.


### Linux index replacement race

Stage/Unstage in an unborn repository intermittently failed because a Linux
metadata lookup observed Git's replaced index inode with zero links. Exact
Stage in-flight index validation now retries that observation up to three
milliseconds before reading bytes. Hard links, missing paths, persistent
unlinking, other graph files, and strict post-transition validation retain their
rejection behavior. Deterministic metadata tests cover these boundaries; eight
consecutive real Stage/Unstage/Commit runs passed. The broader Git library suite
passed 128 tests and both Git integration suites passed (five tests total).
The host cockpit suite passed all 38 tests, including the original failing flow.


### Encrypted browser conversation pagination

Conversation subscription-open and continuation queries now use the negotiated
item and byte budgets, capped by the existing carrier-safe page limits. The
new executor regression failed with seven facts against a two-item negotiation
before the fix; both independent item- and byte-limit scenarios now traverse
the complete ordered history without duplicates. Conversation-related library
checks passed 69 tests; all-target compiler checking and the live binaries built.

Through the real isolated HTTP pairing and Noise/WASM transport, the production
NativeHostSession negotiated two-item pages, received an assigned client ID,
loaded five tasks in three pages (2/2/1), then loaded 38 unique, ordered
conversation facts in 19 pages. The exact assistant reply remained present.
Snapshot cursors arrived as MessagePack binary arrays and resumed successfully.
After an exact host restart, the same route and retained browser trust recovered
the live five-task/38-fact state without pairing again. No session error remained.
Private evidence: `linux-production/remote-pages-live-result.json` and
`remote-pages-*.log`. Exact app/host and Rust processes were joined; production
configuration hashes remained unchanged.

### Linux embedded-browser runtime

The Linux backend uses WebKitGTK 4.1 through GTK 3 and a retained X11 child
window. Wayland desktops use XWayland; provider processes retain the user's
original desktop environment. Build dependencies include `libgtk-3-dev` and
`libwebkit2gtk-4.1-dev` (Ubuntu names), and a Wayland runtime needs XWayland.
The pinned GPUI crate has a narrow vendored patch for its previously unimplemented
X11 raw handles and explicit X11 application selection.

The real GTK harness passed asynchronous JavaScript, native file upload with
exact content verification, screenshot pixel validation, child reparenting and
parking, and cancellation during teardown. Chromium-only raw CDP is omitted
from the Linux tool list; the typed automation and capture adapters use WebKit.
Resource storage now retains a directory descriptor and rejects directory/lock
replacement, including replacement during an artifact write.

This is engine acceptance. The native task Browser pane still needs its durable
session startup wiring; the completed native-shell follow-up is recorded at
the top of this document.
