# Task 5C Report: Sequential checkpoints

## Checkpoint 10: Typed locator failure, retained evidence, and stable pause

### Status and scope

Checkpoint 10 started from the approved checkpoint-9 evidence head `f9f1657b04cff4153c0402dbfb38a7d57a632e34`. The design and implementation lineage is complete through independently approved Task 3 head `0be24d9ba2453d9a4076ffcc23f366e9de35791c`, and Task-4 evidence was committed at `e541996c03ee133ceebe3d941faef8976260354a`. The `0be24d9` approval covers Task 3 implementation, not the full checkpoint evidence package. The controller freezes the clean current `f9f1657b04cff4153c0402dbfb38a7d57a632e34..HEAD` artifact for final independent review; full checkpoint-10 approval remains pending that re-review.

This checkpoint adds only typed locator-failure classification, exact live-runtime evidence retention, private repair state and host capture authority, and an executor that remains alive in a stable pause. It does not add repair preview, highlight overlays, candidate selection, confirmation, recipe mutation, locator overrides in use, repository approval/write behavior, the exact `browser_workflow` MCP group, native repair controls, provider lifecycle wiring, a second replay owner, whole-PC control, Playwright, Node sidecars, or external Chrome mode. Checkpoints 11 and 12 remain pending and untouched.

### Contract delivered

- `BrowserError::LocatorNotFound` carries only fixed `Primary`, `Source`, or `Destination` target kinds. The injected Windows boundary accepts a missing-target code only from a private failure created for and consumed by the current invocation; page-controlled exceptions, reserved-looking messages, and retained genuine failures collapse to the fixed `automation_failed` crash path. Secret target disappearance/change maps only to `Primary`.
- `BrowserReplayRepairInstance` binds the exact workspace/replay/repair IDs to pointer-identical coordinator scope. One active replay owns at most one private repair, one non-Clone retention lease, one private old locator/resume cursor, and one value-free watch generation. Safe projection exposes only validated IDs, exact slot/index, tab ID, revision, dedicated owner-scoped resource handles, and fixed phase.
- Every store opened on one canonical project-resource root shares one process-global root runtime while that runtime is live. Windows holds one OS-backed exclusive direct-regular root lock; same-process stores reuse exact immutable limits, external contention returns fixed path-free failure, and final runtime drop releases the lock. Repair metadata persists `pinned: false`; the exact live lease supplies effective pinning, and crash/drop leaves no persistent repair-retention token.
- Dedicated `ReplayRepairSnapshot` and `ReplayRepairScreenshot` resources can be created only through an exact Agent replay sidecar. The sidecar validates coordinator scope, owner, replay, repair, root, command, tab, revision, kind, MIME/response shape, sequence, one-shot receipt, cancellation, and revocation. Ordinary capture, manual pin, duplicate kind, cross-root, and cross-coordinator substitution fail closed.
- On an eligible typed action/wait/assertion failure, the executor maps the exact primary/optional/source/destination/action-wait/step-wait/assertion slot and resume cursor, reserves the lease, reads a fresh workspace revision, captures and validates semantic snapshot before viewport screenshot, then atomically publishes the paused projection. Any boundary failure rolls back the whole lease and terminates with fixed `StepFailed`; absent/hidden semantics and impossible target kinds do not become repairs.
- The executor retains the original execution handle and secret store while paused. It checks exact coordinator state before and after each watch wait, so early signals, sender drop, cancellation, replacement, workspace interruption, controller interruption, and late responses cannot strand or rearm the old authority. Nested wait/assertion resume tests prove only the failed phase retries and a successful mutation is not duplicated.
- Independent review found one test-only exactness gap: a replay-scoped resume helper could let stale repair N resume repair N+1 in the same replay. The inspection helper now returns the exact `BrowserReplayRepairInstance` plus slot/cursor, and resume accepts that instance and verifies exact active paused-repair equality before taking state or signaling. No production/public resume path was added.

### Strict RED-to-GREEN chronology

#### Design hardening

- `3afcb722b2ef74742e566725e9208f3a5519154a` (`docs(browser): design locator repair`) established the single-owner lifecycle.
- `bcb39f84cf9bde048b12c3ce12c749c8425288a9` (`docs(browser): harden locator repair races`) closed watch, overlay, and apply race claims.
- `aae0877b33d3ac64e8ecd18077b0f3d16f8ae16f` (`docs(browser): close locator repair ownership gaps`) fixed resource/runtime and ownership boundaries.
- `09abecdf095c9fe2e84555623b0e1758a449de7f` (`docs(browser): define recipe repair digest`) fixed the later checkpoint-11 digest contract without starting preview/apply.

#### Task 1: typed host locator failure

- RED: `cargo test -j 1 --test browser_host locator_failure_errors_are_typed_value_free_and_distinct_from_crashes` failed to compile because `BrowserLocatorFailureTarget` and `BrowserError::LocatorNotFound` did not exist.
- Initial GREEN: `77033cdee0c47b6f6663bf6d24dec81960a68b88` (`feat(browser): add typed locator failures`).
- Review RED reproduced page-controlled reserved-message collisions; `1f35ead29d19969e6648b941be6d151001827e50` (`fix(browser): authenticate locator failure codes`) introduced private nominal failure provenance.
- A second review RED retained a genuine failure and rethrew it from a later invocation; `f8c05aac5c6c263437cd28f2570d199ff0a33829` (`fix(browser): bind locator failures to invocation`) bound and consumed an exact invocation ticket. Fresh current action/secret failures remain typed while arbitrary or stale failures remain fixed crashes.

#### Task 2: exact retention, coordinator state, and host sidecar

- The resource RED failed before production changes because the exact repair-retention authority and metadata-failure seam did not exist; the public integration RED also lacked fixed root-unavailable mapping. `5a449346940a42584f105ea1a9953c13ee795a29` (`feat(browser): add repair resource retention`) delivered the shared live-root runtime, Windows lock, collision-safe unpinned publication, exact lease, cleanup retry, and crash behavior.
- Coordinator identity/state/trait/terminal REDs preceded `f49a4f017391b0cd4bc3ffac56419d0e1ba57b15` (`feat(browser): add exact repair coordinator state`), which added exact repair IDs, projection, capture phases, one live repair, watch generation, private lease/locator/cursor, and terminal release.
- Sidecar/host REDs preceded `96733e2de90240cd931416362913287b82c3b02a` (`feat(browser): retain exact repair evidence through host`), which added private request authority, dedicated capture kinds, exact validation/receipt, fixed error containment, and real Windows storage. Independent review approved the complete Task-2 slice before executor work.

#### Task 3: executor capture, stable pause, and stale-generation repair

- Focused executor tests established exact locator-slot mapping, eligible timeout semantics, snapshot-then-screenshot capture, all rollback boundaries, pause/wake races, terminal retention, secret containment, and phase-aware retry before the implementation was completed in `0be24d9ba2453d9a4076ffcc23f366e9de35791c` (`feat(browser): pause replay with locator evidence`).
- Independent review rejected the replay-scoped test resume helper. Exact RED command: `cargo test --locked --lib executor_test_resume_rejects_a_stale_repair_generation -j 1 -- --test-threads=1`; result was 0 passed and 1 failed because stale repair N returned `Ok` and resumed repair N+1 instead of returning `InvalidRepairEvidence`.
- The same command passed 1/1 after exact active repair equality was required. The executor-level regression then reserves and resumes N, publishes N+1 in the same replay, proves stale N leaves N+1 paused with no browser request, resumes exact N+1, and completes only the failed phase. Final independent review approved `0be24d9`.

### Checkpoint-10 verification

Every Cargo command ran one at a time with one Cargo build job and one test thread.

- `cargo test --locked --lib browser::replay_executor::tests -j 1 -- --test-threads=1`: 11 passed, 0 failed.
- `cargo test --locked --test browser_replay_executor -j 1 -- --test-threads=1`: 23 passed, 0 failed.
- `cargo test --locked --test browser_replay_repair -j 1 -- --test-threads=1`: 5 passed, 0 failed.
- `cargo test --locked --test browser_replay_secrets -j 1 -- --test-threads=1`: 12 passed, 0 failed.
- `cargo test --locked --test browser_host -j 1 -- --test-threads=1`: 101 passed, 0 failed.
- `cargo test --locked --test browser_workflow_coordinator -j 1 -- --test-threads=1`: 24 passed, 0 failed.
- `cargo test --locked --lib browser::resources::tests -j 1 -- --test-threads=1`: 10 passed, 0 failed; nested child-helper processes also passed.
- `cargo test --locked --lib browser::commands::secure_command_tests -j 1 -- --test-threads=1`: 12 passed, 0 failed. An initial `browser::commands::tests` filter ran 0 tests and was not counted; inspecting the source identified the real module name before this required rerun.
- `cargo test --locked --test browser_replay -j 1 -- --test-threads=1`: 21 passed, 0 failed.
- Focused total: 219 passed, 0 failed.
- The literal aggregate ordering `cargo test --locked browser -- --test-threads=1 -j 1` did not reach tests: Cargo help confirms arguments after `--` belong to the test binary, so Cargo compiled targets at its CPU-count default and rustc exhausted the Windows paging file (`os error 1455`). No source failure was reported and no Cargo/rustc process remained afterward.
- Corrected single-job aggregate `cargo test --locked -j 1 browser -- --test-threads=1`: 187 top-level matching tests passed, 0 failed (135 library and 52 integration-target matches). The full-output recovery run exited 0 in 379.1 seconds; a warm case-sensitive target-boundary rerun independently reproduced exit 0 and the exact count without counting nested child helpers.
- `cargo check --locked --all-targets -j 1`: passed in 55.53 seconds. It reported only the expected dormant checkpoint-11 dead-code warnings for paused projection payload, private old locator/resume cursor, and the unused internal status accessor.
- `cargo fmt --all -- --check`: passed.
- `git diff --check`: passed; Git emitted only informational LF-to-CRLF working-copy notices for the four documentation files.

### Leakage audit

- An executable base-through-working-docs audit over `git diff f9f1657b04cff4153c0402dbfb38a7d57a632e34`, the exact projection/error/target source blocks, negative trait assertions, fixed Windows/MCP codes, and the sentinel search passed: 12 allowlisted projection fields with 0 forbidden hits, 4 fixed repair-error variants, 3 fixed locator target variants, 0 added journal-sensitive hits, and exactly 1 test-only sentinel occurrence.
- `BrowserReplayRepairProjection` serializes only workspace/replay/repair IDs, validated recipe/step IDs, step index, fixed locator slot, tab ID, revision, dedicated owner-scoped handles, and fixed phase. It has no locator, selector, page text, path, input value, secret, callback message, candidate, or old locator. The resource bodies intentionally contain the captured page evidence, but only existing owner-scoped handles leave the private state.
- Repair-domain errors are fixed payload-free variants. Host `LocatorNotFound` carries only the fixed target enum; Windows and MCP journal/error conversion emits only `locator_not_found`. The repair capture error boundary is a closed allowlist and collapses all path/message-bearing storage/host errors to fixed `ResourceRootUnavailable`.
- `BrowserReplayRepairInstance`, authority, sidecar, capture receipt/evidence, retention lease, private repair state, and secret lease are non-serde and non-`Debug`; compile-time negative assertions cover the externally relevant private carriers. The only serde/Debug repair value is the safe projection/fixed enum surface above.
- The exact checkpoint secret sentinel scan `rg -n --glob '!target/**' "EXECUTOR_REPAIR_SECRET_9F2C" .` returns one occurrence: `src/browser/replay_executor.rs:2995`, inside the module beginning at `#[cfg(test)]` on line 1963. It does not occur in production source, MCP, resources, journal code, or documentation. Its focused test proves command/context and terminal projections exclude the value.
- Secret-store lifetime remains private across pause: focused replay/executor tests retain the lease while paused, close it on cancel/replace/interruption/terminal return, reject late sidecar use, and never place plaintext in repair evidence or safe state.

### Platform and acceptance limits

- Windows is the functional platform for typed callback provenance, exclusive resource-root locking, retained capture storage, and injected secret behavior. Unsupported hosts reject the validated capture route with a fixed platform error before storage.
- No interactive real WebView2 repair session was run. Evidence is executable state-machine, store/OS-lock, fake-controller, Node callback, Windows source-contract, and integration coverage plus independent immutable review. Real-provider/interactive acceptance remains Task 6.
- Checkpoint 10 deliberately stops at a stable pause. Preview/apply/repository mutation and same-step production resume remain checkpoint 11; the exact MCP/native/provider lifecycle bridge remains checkpoint 12.

### Commits and immutable-review scope

Exact approved-base-through-evidence lineage:

1. `f9f1657b04cff4153c0402dbfb38a7d57a632e34` — approved base.
2. `3afcb722b2ef74742e566725e9208f3a5519154a` — locator-repair design.
3. `bcb39f84cf9bde048b12c3ce12c749c8425288a9` — design race hardening.
4. `aae0877b33d3ac64e8ecd18077b0f3d16f8ae16f` — design ownership hardening.
5. `09abecdf095c9fe2e84555623b0e1758a449de7f` — deterministic digest design.
6. `77033cdee0c47b6f6663bf6d24dec81960a68b88` — typed failure implementation.
7. `1f35ead29d19969e6648b941be6d151001827e50` — fixed-code authentication.
8. `f8c05aac5c6c263437cd28f2570d199ff0a33829` — current-invocation binding.
9. `5a449346940a42584f105ea1a9953c13ee795a29` — live-runtime repair retention.
10. `f49a4f017391b0cd4bc3ffac56419d0e1ba57b15` — exact repair coordinator state.
11. `96733e2de90240cd931416362913287b82c3b02a` — exact host evidence sidecar.
12. `0be24d9ba2453d9a4076ffcc23f366e9de35791c` — executor evidence capture, stable pause, and stale-generation fix.
13. `e541996c03ee133ceebe3d941faef8976260354a` — checkpoint-10 Task-4 evidence.

Task-4 evidence was committed at `e541996c03ee133ceebe3d941faef8976260354a`. The controller freezes the clean current path-scoped `f9f1657b04cff4153c0402dbfb38a7d57a632e34..HEAD` artifact for final independent review. Raw-byte freeze metadata—byte count, SHA-256, raw stable patch ID, and byte-identical regeneration—is recorded by the controller/reviewer outside this artifact so the artifact does not self-reference. Full checkpoint-10 approval remains pending re-review. No checkpoint-11 or checkpoint-12 implementation or evidence file has been started.

## Checkpoint 9: Memory-only replay secrets

### Status and scope

Checkpoint 9 started from the independently approved checkpoint-8 head `7f4afb3637b6e23435ccf6146bc901eb8d79c192`. Its implementation is complete through `4d6123b464f6be2ba0977144852ebff2dd601fad`; the evidence-only commit that contains this report is the frozen checkpoint-9 review head recorded in the handoff.

This checkpoint adds only memory-only replay secrets and the masked pane contract. It does not add locator-failure/repair state, recipe repair or repository mutation, the exact `browser_workflow` MCP group, replay lifecycle UI wiring, a second replay owner, whole-PC control, Playwright, Node sidecars, or external Chrome mode. The NativeShell prompt installation seam remains intentionally dormant until checkpoint 12 owns the final workflow lifecycle.

### Contract delivered

- Each replay owns one exact workspace/coordinator-scope/instance `BrowserReplaySecretStore`. The non-Clone, non-Debug, non-serde submission and lease authority retain bounded `Zeroizing<String>` values only; terminalization, replacement, interruption, cancellation, coordinator drop, and executor teardown synchronously close the store.
- Public browser state sees only `BrowserCommand::SecretType { tab_id, target, input_name }`. Plaintext crosses a private non-Debug/non-serde sidecar as an exact replay lease. Ordinary controller calls cannot forge the sidecar; the secure controller method accepts only Agent execution and exact workspace/replay/input authority.
- Windows sends the marker through the existing per-tab Agent queue, runtime target inspection, approval, cancellation, recording, and journal paths. Exposure begins before the plaintext callback and remains in-flight until the fixed callback boundary. Document taint, native navigation identity, and fail-closed missing state prevent page-controlled values from re-entering safe projections during or after secret execution.
- Recording reserves Secret and File input name/kind ownership transactionally in global source order before asynchronous inspection. It retains only an unset Secret declaration and recipe `Type` input reference; password text, upload paths, CDP params/results, and callback output never enter recorder state.
- The executor sends ordinary Text through `BrowserAction::Type` and only Secret-kind recipe `Type` through the private secure request. It accepts the exact already-Running instance produced by prompt submission, requires the standard exactly-one-action response, fences status around every await, and closes the store on every return.
- NativeShell owns the volatile prompt vault directly, outside `BrowserPaneTransient`, `BrowserPaneModel`, `AppState`, persistence, remote snapshots, resources, and journals. Values are preallocated to the 16-KiB bound and never reallocate while filling; rendering receives only safe names/focus/`is_set` and always displays exactly eight bullets. Submit consumes the vault once; cancellation, replacement, route/tab/conversation changes, destructive browser actions, pane collapse, modal/editor entry, and window close zeroize it.
- Prompt key routing consumes printable keys, Backspace, navigation, Enter, Escape, and modified clipboard gestures before terminal/composer/annotation/remote paths. While the prompt is active, the annotation editor remains preserved but non-visible, non-focused, and unable to read the clipboard or mutate its draft.
- There is no MCP operation or argument that can submit a replay secret. The existing tool schemas/resources expose no `SecretType`, prompt, secret-value, or replay-secret wire seam.

### Strict RED-to-GREEN chronology

#### Task 1: exact-instance zeroizing store

- RED: `cargo test --locked --test browser_replay_secrets -- --test-threads=1` failed to compile because the secret submission/store/lease/error and coordinator APIs did not exist.
- GREEN implementation: `e7367498799ece57fbc9a5873cd31785741dd755` (`feat(browser): add memory-only replay secret store`).
- Independent review exposed a missing source-order input-capacity fence. The focused RED accepted an over-capacity unresolved Secret set; `11937fbf7db7ea570ffb9320edc686111bef2ab7` moved the fixed 32-input limit to replay compilation/start before a store can be installed. Final replay-secret and replay suites include the exact-capacity, invalid-set atomicity, one-shot, stale/foreign, terminal-close, and retained-lease zeroization cases.

#### Task 2: private controller sidecar

- RED: `cargo test --locked --test browser_replay_secrets secure_command -- --test-threads=1` failed to compile because `SecretType` and the secure request method did not exist.
- GREEN implementation: `df43754454352ade18516429b49f28bfa85956d8` (`feat(browser): add private secret command lane`).
- Independent review reproduced same numeric replay IDs from different coordinator scopes reaching the sidecar boundary. `aba7ea121c31023e13684afc9b4fbdc299b9aca3` bound the sidecar to the exact opaque replay scope and made generic, wrong-command, wrong-input, wrong-workspace, stale, cancelled, revoked, and unsupported routes fail closed. Public marker/context/status serialization and Debug assertions remain value-free.

#### Task 3: Windows queue, approval, recording, and injected typing

- RED began at `deea001cc4ba002111c2f0e730623371d64f32d6` with absent queue phases, injected `typeSecret`, approval/recording mapping, and Node harness behavior. The initial GREEN typed through the existing Agent lane, then three strict review-remediation rounds landed at `7a482f2487f9cf8fca147b28ce7fb01241e48568`, `2d7fe4d3ed2b379e65ed4f4f077a6894a2e98e65`, and `27e9c2a2df58fdba39f43dd1240bf569546f7543`.
- The final remediation's missing-state REDs were three exact 0/1 failures (close tab, workspace reset, and profile clear) in which queued URL/title/download/diagnostic values survived state removal. GREEN retains tri-state document authority and treats missing as contained before any projection or recorder installation.
- Exposure REDs were exact 0/1 failures for navigation completion before callback, callback invalidation of an earlier navigation candidate, duplicate finish, immediate schedule error, and the production source lease boundary. GREEN begins one idempotent exposure lease before `with_exposed`, blocks navigation clearing while in flight, finishes before fixed result mapping/queueing, and deliberately remains fail closed when an accepted evaluation never calls back.
- Recorder source-order REDs were five exact 0/1 failures: upload-then-secret completion order, secret-then-upload ownership, explicit collision, capacity before exposure, and cancelled-earlier-secret/later-upload naming. GREEN preclaims both kinds under one source-order lock, rolls back failed begin atomically, and releases ownership on cancellation/stop/discard/restart.
- Final Task-3 verification was 142/142 across host 96, recording 10, replay secrets 12, and coordinator 24; 32/32 adjacent integrations; 14/14 focused native state tests; one exact post-success coordinator regression; locked all-target check; format; and diff checks. Independent review approved with 0 Critical and 0 Important findings. The theoretical `u64` exposure-generation/in-flight counter saturation after approximately `2^64` transitions remains one Minor final-review note.

#### Task 4: executor and masked prompt

- A pre-existing CRLF-sensitive executor source assertion was normalized in test code before feature work; its exact repaired baseline was 1/1. Strict executor and prompt RED commands failed to compile for the absent prompt vault/event/projection and secure executor route; the exact NativeShell ownership and pane-obscuring lifecycle tests then failed 0/1 before their seams existed. No failed command was a zero-test filter.
- The first executor GREEN attempt timed out because prompt submission correctly transitions `NeedsUserSecret` to `Running` while the executor accepted only `Pending`. Exact-instance acceptance of the submitted Running replay made the three secret executor tests pass. `3dd7a9f811ded698a229f9c1f0e6ba4641721a19` delivered the contract; final implementation suites were executor 23/23, prompt 3/3, pane 32/32, replay secrets 12/12, replay 21/21, workflow coordinator 24/24, host 96/96, recording 10/10, workflow review UI 14/14, attachment lifecycle 3/3, and terminal annotations 6/6.
- Independent review rejected two Important boundaries. Allocation RED failed to compile before a dedicated preallocated value owner existed; GREEN fills 16 KiB in 257-byte increments with unchanged pointer/capacity and rejects a one-byte overflow without allocation movement. Annotation ownership RED first lacked pure visibility/action seams, then failed the source-order guard and behavioral `annotation_focused` assertion; GREEN removes the editor/key handlers while preserving its draft and consumes stale annotation actions before mutation or clipboard access.
- `4d6123b464f6be2ba0977144852ebff2dd601fad` is the independently approved remediation. Its final gates were allocation invariant 1/1, prompt 3/3, pane 32/32, executor 23/23, review UI 14/14, annotations 5/5, locked all-target check, format, diff, and secret/logging scans. Fresh re-review reported 0 Critical, 0 Important, and 0 Minor findings.

### Checkpoint-9 verification

All Cargo commands used `CARGO_BUILD_JOBS=1` where compilation was involved.

- `cargo test --locked --test browser_replay_secrets -- --test-threads=1`: 12 passed, 0 failed.
- `cargo test --locked --test browser_secret_prompt -- --test-threads=1`: 3 passed, 0 failed.
- `cargo test --locked --test browser_replay_executor -- --test-threads=1`: 23 passed, 0 failed.
- `cargo test --locked --test browser_host -- --test-threads=1`: 96 passed, 0 failed.
- `cargo test --locked --test browser_workflow_coordinator -- --test-threads=1`: 24 passed, 0 failed.
- `cargo test --locked --test browser_recording -- --test-threads=1`: 10 passed, 0 failed.
- Focused total: 168 passed, 0 failed.
- `cargo test --locked browser -- --test-threads=1`: 153 matching tests passed across library and integration targets, 0 failed.
- `cargo check --locked --all-targets`: passed in 29.21 seconds.
- `cargo build --release --locked`: passed natively on Windows in 8 minutes 24 seconds with `GPUI_FXC_PATH=C:\Program Files (x86)\Windows Kits\10\bin\10.0.22621.0\x64\fxc.exe` and one Cargo job.
- `cargo fmt --all -- --check`: passed.
- `git diff --check`: passed; Git emitted only an informational Windows autocrlf warning before the fixture prerequisite normalized its payload.

The first aggregate attempt was not green: it reached 101/101 matching library tests and 17/17 `browser_core` tests, then `browser_fixture` failed 0/1 because the tracked LF payload had been checked out as CRLF under system `core.autocrlf=true`. Raw-byte diagnosis showed the index ended in `0A` (37 bytes), the worktree ended in `0D 0A` (38 bytes), and the test directly used `read_to_string`; an exact rerun repeated the failure. The separate strict-TDD and independently approved prerequisite `429dbc86dbd5a8b89ee9daa0c7220a7d64dfb9d5` adds only `.gitattributes` byte-stability for browser fixture payloads. After it, `git ls-files --eol` reported `i/lf w/lf`, the exact fixture test passed 1/1, and the required aggregate rerun passed 153/153. This prerequisite is not checkpoint-9 implementation and is excluded from its artifact.

The first release attempt was also not green: after 783.3 seconds, third-party `gpui 0.2.2` stopped because `fxc.exe` was not on PATH. The installed Windows 10 SDK contained the x64 compiler at the path above, GPUI's build script explicitly supports `GPUI_FXC_PATH`, and the source/configuration remained unchanged. The environment-only retry is the release evidence.

### Leakage audit

- The sentinel-bearing focused suites exercise command/event/status serialization, Debug projections, pane/persisted/remote snapshots, journal/resource source ownership, recorder JSON, injected-page console/network/IPC/snapshot/performance output, callback mapping, executor requests, and the controlled DOM value. Their 168/168 result permits the plaintext only inside test source and the controlled DOM assertion.
- An exact tracked-source scan for the eight checkpoint sentinel/private-value patterns found 11 occurrences and 0 unexpected production occurrences: every hit is in an integration test or after the `#[cfg(test)]` module boundary in `commands.rs`, `replay.rs`, or `host/windows.rs`.
- MCP and resource production sources contain 0 checkpoint sentinel hits and 0 `SecretType`, replay-secret, or secret-prompt wire seams. Compile-time negative trait assertions prove submissions, leases, exposure authorities, editor values, and vaults are not Debug/serde/Clone as required; safe event/projection types contain only names, focus, and booleans.
- Added-line logging/input-sink scans found only three test assertions proving secret key handling precedes and never calls clipboard access. There are no added production `println!`, `eprintln!`, `dbg!`, tracing/logging, terminal write, composer write, or clipboard-read sinks for plaintext.
- The native release outputs `target/release/devmanager.exe`, `.pdb`, and `.d` contain 0 hits for all eight checkpoint sentinel/private-value patterns.
- Public `SecretType` JSON has only `type`, `tabId`, semantic `target`, and validated `inputName`; no `text` or `value`. Recording JSON has only an unset Secret input and value reference. Error messages, approvals, resources, journals, callback results, and safe projections use fixed codes/summaries and contain no caller data.

### Platform and acceptance limits

- Windows is the functional platform for this checkpoint. The native Windows all-target check, host/unsupported behavior tests, and release build pass.
- The unsupported/macOS adapter rejects the validated secure ingress with the typed platform error before exposure. A Windows-hosted `aarch64-apple-darwin` cross-check cannot reach project typechecking because third-party `ring`/`aws-lc-sys` requires an Apple-target C compiler unavailable in this environment; native macOS CI remains the authoritative compile gate.
- No interactive real WebView2 session was run during checkpoint 9. Evidence is executable state-machine/unit/integration/Node tests, release compilation, source and binary leakage scans, and independent immutable review. Real-provider/interactive acceptance remains Task 6.

### Commits and immutable-review scope

Checkpoint implementation history, excluding the unrelated Codex/Pwsh documentation commits interleaved on master:

- `cac4ae8` design and `e736749` implementation of the store;
- `11937fb` capacity hardening;
- `df43754` private lane and `aba7ea1` exact replay binding;
- `deea001` Windows typing plus `7a482f2`, `2d7fe4d`, and `27e9c2a` containment hardening;
- `3dd7a9f` executor/prompt plus `4d6123b` memory/focus hardening;
- the evidence-only commit containing this section.

Prior immutable artifact identities retained for audit:

- Task-3 third remediation `41224333884bb5fdfaedceaf766618763e28bc11..27e9c2a2df58fdba39f43dd1240bf569546f7543`: 53,459 bytes; SHA-256 `9c76a4fa1f76b84ab2170ff6413904476255e496268a342f7000b8fc437f6d5f`; raw-byte stable patch ID `8803139e48c1678111b0db1d3c03ef2424fe5b62`; independent regeneration byte-identical.
- Task-4 original `27e9c2a2df58fdba39f43dd1240bf569546f7543..3dd7a9f811ded698a229f9c1f0e6ba4641721a19`: 84,972 bytes; SHA-256 `1605c431cdc4bef641e4d6464a1e1ab0fe61d3b7b271b9ff2d04c69639ebc96a`; authoritative raw-byte stable patch ID `4c18c2346bc105b03082e2dc0585c30c618fa76a`; independent regeneration byte-identical. The previously reported `a01e4fa5bf4d958a9de9251d4bbb524afc174ee7` was produced by PowerShell text transcoding; the frozen artifact itself never changed.
- Task-4 remediation `3dd7a9f811ded698a229f9c1f0e6ba4641721a19..4d6123b464f6be2ba0977144852ebff2dd601fad`: 14,087 bytes; SHA-256 `48e83409abe60fd985780385e49ef34106fe3b40095bb6a8f108085c9aff751f`; raw-byte stable patch ID `c4923954cdf5882f572e90e904e9f5d1f20f863d`; independent regeneration byte-identical.

The final ignored artifact is `.superpowers/sdd/browser-checkpoint9-review.diff`, generated deterministically from base `7f4afb3637b6e23435ccf6146bc901eb8d79c192` through the evidence commit using exactly these 26 pathspecs:

1. `Cargo.lock`
2. `Cargo.toml`
3. `docs/superpowers/plans/2026-07-17-browser-replay-secrets.md`
4. `docs/superpowers/specs/2026-07-17-browser-replay-secrets-design.md`
5. `src/app/mod.rs`
6. `src/browser/commands.rs`
7. `src/browser/host/initialization.rs`
8. `src/browser/host/mod.rs`
9. `src/browser/host/unsupported.rs`
10. `src/browser/host/windows.rs`
11. `src/browser/mod.rs`
12. `src/browser/pane.rs`
13. `src/browser/recording.rs`
14. `src/browser/recording_coordinator.rs`
15. `src/browser/replay.rs`
16. `src/browser/replay_executor.rs`
17. `src/browser/replay_secrets.rs`
18. `tests/browser_host.rs`
19. `tests/browser_replay.rs`
20. `tests/browser_replay_executor.rs`
21. `tests/browser_replay_secrets.rs`
22. `tests/browser_secret_prompt.rs`
23. `tests/browser_workflow_coordinator.rs`
24. `.superpowers/sdd/browser-task-5c-checkpoints.md`
25. `.superpowers/sdd/browser-task-5c-report.md`
26. `.superpowers/sdd/progress.md`

This path scope deliberately excludes unrelated commits `c2f0626` and `4122433` and their Codex-hooks/Pwsh plan/design files, plus prerequisite `429dbc8` and `.gitattributes`. The final head, byte count, SHA-256, raw-byte stable patch ID, and byte-identical independent regeneration are reported in the immutable handoff after the evidence commit exists; embedding the artifact's own identity in this included report would make the package self-referential.

Checkpoint 9 stops here for independent review. Checkpoints 10 through 12 remain pending and no locator-repair or `browser_workflow` MCP/lifecycle work has started.

## Checkpoint 8: Replay through the existing controller/queue/approval/journal

### Status

Checkpoint 8 started from the approved checkpoint-7 hardening head `c57cfd6f0c1c80caf00f2439550a15655ea7c12e`. Implementation and the public runbook are frozen at `29291e6`; the final evidence-only commit containing this report is the independent-review head recorded in the checkpoint handoff. It adds only replay execution through the existing `BrowserController`, the typed host waits required by strict recipes, portable initial-tab recording/alias validation, public architecture documentation, and focused evidence.

Checkpoints 9 through 12 are not implemented. There is no memory-only secret-value carrier or prompt, locator failure/repair state, repair preview/apply, `browser_workflow` MCP group, workflow lifecycle UI, or new browser transport/operation queue/approval/journal.

### Contract decisions

- `BrowserReplayExecutionHandle` is a distinct non-`Clone`, non-`Debug`, non-`Serialize` exact-instance carrier for the one shared immutable plan and cancellation lease. The public cloneable cancellation lease retains authority only, not the plan; terminalization plus dropping the execution handle releases value-bearing plan state.
- Recording start seeds the selected runtime tab as logical `tab-1`. Compilation validates the full bounded alias lifecycle before any browser work. Normal recipes bind the fresh setup tab to `tab-1`; legacy recipes that explicitly `CreateTab tab-1` leave the setup tab implicit until the exact successful create response. Ambient tabs are never inferred as aliases, closed aliases are removed, and aliases are never reused.
- Execution receives the exact coordinator, instance, execution handle, actor, and authenticated canonical local project root. Root verification runs before the first browser command. Setup then awaits exact `CreateTab(None)`, viewport update, and start-URL navigation responses on one fresh tab.
- Every setup operation, recipe action, optional wait, and assertion uses a fresh invocation context with the caller actor, unique operation ID, fixed value-free intent, and `Normal` risk except classified upload. The executor calls only existing controller request methods, so the established operation queue, runtime target inspection, approvals, cancellation epochs, resources, and agent journal remain authoritative.
- Every strict action maps to one existing typed command and exact response family. Semantic download is one `Act::Click`; `CdpMarker` uses its validated method, an empty object, fixed rationale, and exact `Cdp` response. There is no arbitrary JavaScript or direct download-filesystem path.
- Step waits cover Duration, URL exact/contains, Load, NetworkIdle, Element present/visible/hidden, and Text present/absent. Assertions compile to short typed URL, Title, Text present/absent, Element present/absent/visible/hidden, or exact ElementValue waits. Action, optional wait, and assertions run strictly in order; the coordinator advances only after all succeed and stops on the first failure.
- Ordinary `matched: false` is `StepFailed`; assertion `matched: false` is `AssertionFailed`; transport, host, response-shape, value-resolution, alias, and snapshot-proof failures collapse to `StepFailed`. Raw values, canonical paths, and host errors never enter new Debug/Serialize status/error surfaces.
- Upload File inputs resolve at execution time. Relative paths join the verified root; the existing classifier canonicalizes candidates, follows symlinks/reparse redirects, verifies a regular file, and returns exact `Normal` or `OutsideWorkspaceFile` risk. The existing authenticated-root upload request/approval path receives the canonical file.
- Cancellation authority and exact coordinator status are checked around every awaited response and before transitions. Cancellation, replacement, workspace interruption, and `BrowserError::Interrupted` return the retained old `Cancelled` projection. Late responses cannot mutate aliases, advance/fail/complete the old instance, or affect a replacement. Begin/advance/fail/complete races fall back only to the exact retained terminal projection.

### RED-to-GREEN evidence

1. Alias portability: the compiler initially accepted select/close-before-create and alias reuse, while recording did not seed the selected tab. Focused compiler/coordinator tests drove lifecycle validation and deterministic `tab-1` recording to green.
2. Execution ownership: the first shared-plan slice incorrectly made the public cloneable cancellation lease retain the plan. A failing weak-plan-retention regression led to the distinct authority-only lease plus single execution handle; replay tests finished 20/20 green.
3. Typed waits: strict replay conditions were not representable in the host. Focused serialization/injection tests drove NetworkIdle, Title, ElementAbsent, and ElementValue support without a JavaScript-predicate wire.
4. Setup: the executor initially handled only Reload. A real controller-channel test drove canonical-root preflight, fresh-tab setup, viewport/start navigation, exact workspace proofs, one awaited request at a time, unique contexts, and late setup-response cancellation fencing.
5. Actions and uploads: a table covering every non-upload action drove exact command/response mapping. Upload tests drove execution-time relative/absolute resolution, canonical in-root `Normal`, outside-root and Windows junction escape `OutsideWorkspaceFile`, missing-file failure, authenticated-root propagation, and path-free output.
6. Wait/assertion ordering: failing tests drove action -> optional wait -> all assertions -> advance, every wait variant, every assertion variant, ordinary/assertion false-result distinction, wrong response variants, first-failure stop, and no later work.
7. Adversarial audit: cancellation/replacement across in-flight action, wait, and assertion responses exposed transition-race handling gaps. Checked setup requests and retained-terminal fallbacks made all six race cases green. Additional regressions cover host interruption, hostile path/value-bearing host errors, exact create/select/close snapshot mismatches, and legacy `CreateTab tab-1` runtime mapping.

### Verification

- Focused checkpoint targets: `browser_automation` 12, `browser_host` 87, `browser_recipes` 16, `browser_recording` 10, `browser_replay` 20, and `browser_replay_executor` 14: 159 passed, 0 failed.
- Aggregate `cargo test browser`: 122 matching tests passed across library/integration targets, 0 failed.
- ProcessManager surrounding protocol gate: 70 passed, 0 failed.
- `cargo check --locked --all-targets`: passed.
- Native Windows `cargo build --release --locked`: passed.
- `cargo fmt --all -- --check` and exact-range `git diff --check`: passed.
- `cargo check --lib --target aarch64-apple-darwin` was attempted from Windows and reached third-party `aws-lc-sys`, then stopped because no Apple-target C compiler (`cc`) is installed. The shared unsupported-host module and typed macOS-unavailable behavior compile and pass on the native Windows all-target/host-test surface; native Apple compilation remains environment-limited.
- Strict `cargo clippy --test browser_replay_executor -- -D warnings` is not a clean repository gate: it stopped on broad pre-existing unrelated lint debt beginning in `src/app/mod.rs` and `src/ai`. No checkpoint-8-specific lint failure was observed before that baseline stopped the command.

### Commits

- `b12edeb` design specification
- `eb75b96` implementation plan
- `552c14a` portable tab-alias validation and recording seed
- `8560fad`, corrected by `a0cad2e`, shared plan/execution authority
- `52e5374` typed replay waits
- `73b879f` sequential setup/root preflight
- `63b2713` complete executor, containment, assertions, and adversarial tests
- `29291e6` public architecture/runbook documentation

### Files

- `docs/browser-automation.md`
- `docs/superpowers/plans/2026-07-17-browser-replay-executor.md`
- `docs/superpowers/specs/2026-07-17-browser-replay-executor-design.md`
- `src/browser/automation.rs`
- `src/browser/commands.rs`
- `src/browser/host/initialization.rs`
- `src/browser/host/windows.rs`
- `src/browser/mod.rs`
- `src/browser/recording_coordinator.rs`
- `src/browser/replay.rs`
- `src/browser/replay_executor.rs`
- `tests/browser_host.rs`
- `tests/browser_replay.rs`
- `tests/browser_replay_executor.rs`
- `tests/browser_workflow_coordinator.rs`

## Checkpoint 7: Replay compiler/status/cancellation lease

### Status

Checkpoint 7 started from the approved clean checkpoint-6 hardening head `0f35ff6552faadf7fa0226d4e59359030848562c`. It implements only the platform-neutral replay compiler, value-free lifecycle projection, exact workspace/instance fencing, bounded terminal cleanup, and one replay-lifetime cancellation lease. The immutable final head, stable patch ID, and review package range are recorded by the checkpoint handoff after the commit exists.

Checkpoints 8 through 12 are not implemented. This checkpoint executes no browser action and adds no host, controller, operation-queue, approval, journal, filesystem, UI, MCP, runtime secret-value store/prompt, locator-failure payload, or repair preview/apply integration.

### Contract decisions

- `compile_browser_replay` validates the complete strict `BrowserRecipeV1` before public inputs. It caps 64 inputs, 256 ordered steps, 128-byte safe names, 64-KiB Text, 8-KiB URL, and 32-KiB File candidates; rejects duplicate, unknown, missing, mismatched, credential-bearing, and every public Secret submission with closed value-free errors; and applies only validated Text/URL defaults.
- File values remain bounded nonblank control-free opaque candidates. Replay does not normalize, canonicalize, inspect existence, or touch the filesystem. Secret values cannot enter the public compiler; only safe declared Secret names enter the unresolved-name list and cause `NeedsUserSecret`.
- `BrowserReplayPlan`, public input carriers, and cancellation leases implement neither `Debug` nor `Serialize`. The immutable plan retains the start URL, viewport, ordered steps, and declaration-ordered non-secret bindings only for checkpoint-8 execution. Status/error/Debug/Serialize output contains no recipe name/description/start URL, locator, literal, default, public value, file path, or arbitrary failure message.
- `BrowserReplayStatus` is exactly `Pending | Running | NeedsUserSecret | PausedLocatorRepair | Completed | Failed | Cancelled`. Legal edges are `Pending -> Running|Cancelled`, internal value-free `NeedsUserSecret -> Running` or `Cancelled`, `Running -> PausedLocatorRepair|Completed|Failed|Cancelled`, and `PausedLocatorRepair -> Running|Failed|Cancelled`. Completion requires every ordered step; terminals are immutable.
- Each coordinator has one opaque process-local scope shared by clones and distinct across independently constructed coordinators. Every instance is fenced by that scope plus exact workspace and checked monotonic local ID, preventing foreign-coordinator collisions even when workspace and numeric ID match. A second ordinary start fails; explicit replacement cancels/archives the old exact instance before installing the new plan.
- One `Arc`-backed immutable cancellation authority is minted at replay start. Every lease clone shares its identity and atomic flag across status reads, step gaps, secret wait, and locator pause; no transition rearms or replaces it. Cancel, replacement, and workspace interruption synchronously invalidate it. Completed and Failed replays are not relabelled as cancellation.
- Active state alone owns the value-bearing plan. Terminal transitions drop it and retain only a safe projection in a configurable at-least-one bounded oldest-first deque. Evicted identities become stale. Mutex poisoning is recovered without exposing values; checked ID overflow fails closed without installing a plan.

### RED to GREEN evidence

1. Compiler: RED failed with unresolved `compile_browser_replay`, `BrowserReplayError`, `BrowserReplayPlan`, and `BrowserReplayPublicInput`. GREEN passed the compiler defaults/order/value-boundary group 4/4.
2. State/fencing: RED failed with unresolved coordinator/status/projection/failure types plus missing transition errors. GREEN passed 4/4 for all seven statuses, exact progress/pause/completion, one-active replacement/isolation, terminal immutability, stale calls, and capacity-two eviction. The private value-free secret-readiness seam passed 1/1.
3. Cancellation: RED failed on the absent cancellation lease type and `BrowserReplayStart::lease`. GREEN passed 4/4 across Pending, NeedsUserSecret, Running step gaps, PausedLocatorRepair, replacement, interruption, shared clones, and Completed/Failed non-cancellation.
4. Audit hardening: an ordered-binding regression first failed because no declaration-order accessor existed. After that slice, the 16-test replay target had exactly two RED failures: safe in-bound 64-KiB Text was rejected by the 4,000-character display-redaction cap, and a credential-bearing Secret name reached projection. A shared nontruncating boolean credential detector and declaration-ordered binding storage made 16/16 pass while existing browser automation and recipe suites remained green.
5. Exact scope: a foreign coordinator with the same workspace and local instance ID was incorrectly accepted. Adding one opaque coordinator scope to instance equality made the collision regression GREEN without serializing or debugging the scope.

### Independent-review hardening

The initial checkpoint-7 implementation landed as `b3ab2e163227228f93ca87e55c5f7ded9dc86e7d`. Independent review rejected one P1 boundary: recipe and step IDs used slug syntax alone, so bare credential shapes such as `sk-proj-*` and `ghp_*` could compile and then appear in serialized/Debug replay projections and bounded terminal history.

- Recipe-wire RED: `browser_recipe_identifiers_reject_bare_credentials_on_every_wire_boundary` failed because `BrowserRecipeV1::validate` returned `Ok(())` for the `sk-proj-*` recipe ID. The completed regression covers both required bare shapes in the recipe ID and step ID, top-level validation/serialization/deserialization, direct `BrowserRecipeStep` serialization/deserialization, fixed non-echoing errors, and normal lookalike IDs.
- Replay RED: `replay_compiler_rejects_credential_shaped_recipe_and_every_step_id_before_history` compiled the malicious recipe, started it, and failed because serialized/Debug projection and terminal-history surfaces contained the credential-shaped ID. The completed regression checks recipe ID plus every one of four ordered step IDs for both bare shapes and requires rejection before plan/start with zero retained history.
- GREEN: the existing centralized recipe identifier predicate now combines the unchanged bounded safe-slug syntax with the shared nontruncating credential-content detector. `BrowserRecipeV1` validation and serialization, nested/direct step deserialization, recipe path/list/temp ownership checks, and a new direct-step serialization guard all use that predicate. Ordinary IDs such as `sketch-project_2` and `gh-preview_2` still validate and round-trip with the identical wire shape.

### Verification

- `cargo test --locked --test browser_replay -- --test-threads=1` -> 17 passed, 0 failed.
- `cargo test --locked --lib browser::replay::tests -- --test-threads=1` -> 3 passed, 0 failed for internal secret readiness, checked ID overflow, and poisoned-lock recovery.
- Shared-detector regressions: `browser_automation` 12 passed, 0 failed; `browser_recipes` 15 passed, 0 failed.
- `cargo test --locked browser -- --test-threads=1` -> 120 matching tests passed across library and integration targets, 0 failed.
- `cargo check --locked --all-targets` and native Windows `cargo build --locked` -> exit 0.
- `cargo fmt --all -- --check` and `git diff --check` -> exit 0 on the completed source and documentation.

Review-hardening verification:

- `cargo test --locked --test browser_recipes -- --test-threads=1` -> 16 passed, 0 failed.
- `cargo test --locked --test browser_replay -- --test-threads=1` -> 18 passed, 0 failed.
- `cargo test --locked --test browser_automation -- --test-threads=1` -> 12 passed, 0 failed.
- `cargo test --locked browser -- --test-threads=1` -> 121 matching tests passed across library and integration targets, 0 failed.
- `cargo check --locked --all-targets` and native Windows `cargo build --locked` -> exit 0.
- `cargo fmt --all -- --check` and `git diff --check` -> exit 0.

### Files

- `src/browser/automation.rs`
- `src/browser/mod.rs`
- `src/browser/recipes.rs`
- `src/browser/replay.rs`
- `tests/browser_replay.rs`
- `.superpowers/sdd/browser-task-5c-checkpoints.md`
- `.superpowers/sdd/browser-task-5c-report.md`
- `.superpowers/sdd/progress.md`

## Checkpoint 6: Exact `browser_recording` MCP

### Status

Checkpoint 6 started from the approved clean checkpoint-5 head `e39693c6bcf6d7d78d11d469423df0d072099ec2`. It implements only the exact authenticated recording MCP group and its host/store safety path. The immutable final head, stable patch ID, and review package range are recorded by the checkpoint handoff after the commit exists.

Checkpoints 7 through 12 are not implemented. This checkpoint adds no workflow replay/compiler/status, replay cancellation lease, runtime secret prompt, locator failure/repair state, or `browser_workflow` MCP group.

### Contract decisions

- `browser_recording` is one exact v1 group with only `status | start | stop | review | discard | save`. Its deny-unknown-fields schema contains only required `intent`, `risk`, and `operation`; intent is nonblank and bounded to 1024 bytes, risk is the existing exact seven-value enum, and no workspace, route, tab, instance, token, password, secret, path, or file-content argument exists.
- The bearer-authenticated registration remains the sole source of the `BrowserWorkspaceKey` and canonical owning local project root. The root travels in a private request envelope, never in `BrowserCommand`, the MCP schema, response, resource handle, or journal. Missing, noncanonical, UNC/remote, stale-registration, cross-workspace, and unavailable-host paths fail closed.
- The checkpoint-4 `BrowserWorkflowCoordinator` remains the only Recording/Review authority. Status is inactive by default, start is explicit, stop transitions the exact active instance to Review without saving, and successful save/discard retires only the expected instance. Approval resume carries the original instance ID and immutable request route/root; a replacement review is never mutated by a stale resume.
- Status/start/stop/review return compact value-free inline state. Stop/review expose the full validated v1 recipe only through an owner-scoped bounded `WorkflowReview` resource. Inline Secret/File data is name/kind only; resource generation revalidates the recipe and cannot retain captured credential text or file material.
- New-file save has Normal path risk. Runtime-observed overwrite and every discard add Destructive path risk through the existing conservative effective-risk ordering. Save/discard use the existing per-workspace queue and approval event; every operation retains safe actor/intent/timing/result/resource-ID journal metadata without recipe literals or storage paths.
- Save holds the coordinator review lock across validation, hardened repository IO, and exact discard. The store supports an atomic no-clobber path for Normal new-file writes and atomic replace after overwrite approval. A destination appearing after the risk probe fails closed; storage failure retains Review, while only successful save or explicit discard retires it.
- The unsupported adapter returns typed platform-unavailable for all six operations. A failed tool call is a typed MCP result and does not terminate the authenticated session.

### RED to GREEN evidence

1. Exact schema/dispatch: RED failed because `browser_recording` did not exist. GREEN proves the exact operation/risk enums, three required fields, bounded intent, forbidden route/secret/path vocabulary, typed malformed errors, all-six authenticated dispatch, and v1 `devmanager-browser` results.
2. Coordinator/resource handoff: RED failed on absent typed command/result/resource functions. GREEN routes the bearer workspace to the one coordinator and returns owner-isolated validated resources whose Secret/File inputs contain only name/kind and whose bytes omit a captured bearer sentinel.
3. Authenticated root: RED failed on the absent private root seam. GREEN carries the registration's canonical root in a private request envelope for every operation while keeping it absent from public command/schema/result wires.
4. Save/discard policy: RED failed on absent overwrite probe, effective risk, exact mutation, and recording approval-resume APIs. GREEN proves Normal new save, Destructive overwrite/discard, exact-instance resume, replacement stale fencing, successful retirement, failure retention, and no-clobber failure when a destination appears after risk probe.
5. Failure/platform behavior: GREEN keeps observation/lifecycle operations direct, queues save/discard, returns typed non-terminal tool failures, and rejects every operation through the macOS/unsupported adapter.

### Independent-review hardening

The initial checkpoint-6 implementation landed as `8d0d8a29e6a1cf186e8cfa7658d296d18d6dfc09`. Independent review rejected three boundaries, each reproduced by a focused test before the production fix:

1. Resource error disclosure: RED showed a failed review-resource `put` could expose the store path through `BrowserError::Io`/`ToolFailure`. GREEN maps review encoding and persistence plus both Stop/Review trusted-root/store-open failures to `RecordingResourceUnavailable`, whose MCP code and message are fixed and path-free. The real rmcp regression proves project root, resource root, `.devmanager`, and injected underlying error detail stay absent while the exact Review instance remains active and the same authenticated session still reports Review.
2. Root preflight ordering: RED exercised all six operations with a UNC root. Each eventually returned a safe invalid-request result, but the host had already observed Ensure, pane-open, and workspace-state lifecycle work. GREEN runs one shared canonical-directory/local/non-UNC validator before first-use initialization and reuses it in the lower controller and save seams. No host command or recording state effect occurs on rejection. The Windows prefix classifier permits canonical local `VerbatimDisk` paths while rejecting only `UNC` and `VerbatimUNC` roots.
3. Workspace mutation target: RED failed to compile because Save/Discard had no stable workspace-target seam; the Windows host fell back to the currently selected tab, so a selection change could create a second concurrent queue/approval target. GREEN derives `__workspace__` for both operations regardless of selection. The regression queues Save under one selected tab, changes selection before Discard, then proves one target, ordered approval/mutation resume, and exact queue completion.

### Verification

- Recording gateway tests: 3 passed, 0 failed; recording MCP domain tests: 2 passed, 0 failed.
- Final focused host plus recording MCP gate: 87 passed, 0 failed.
- Full 15-target browser integration gate: 242 passed, 0 failed before the final unsupported-adapter regression; the final focused gate covers that added regression.
- `cargo test --locked browser -- --test-threads=1`: 115 matching tests passed, 0 failed.
- ProcessManager gate: 69 passed and the documented pre-existing `stopped_server_can_start_again_on_same_terminal_session` timeout recurred; its exact rerun passed 1/1 in 1.69 seconds. This checkpoint has no `src/services` diff.
- `cargo check --locked --all-targets`, native Windows `cargo build --locked`, `cargo fmt --all -- --check`, and `git diff --check`: passed.
- `cargo check --locked --target aarch64-apple-darwin --lib` was attempted from Windows but stopped in third-party `ring`/`psm`/`aws-lc-sys` build scripts because no Apple-target `cc` is installed. The platform-neutral unsupported seam is compiled and behavior-tested on Windows for all six macOS operations; native macOS compilation remains environment-limited.

Review-hardening verification:

- Recording MCP domain: 3 passed, 0 failed; real gateway: 18 passed, 0 failed; host/platform/queue: 86 passed, 0 failed; exact all-six root preflight unit: 1 passed, 0 failed.
- `cargo test --locked browser -- --test-threads=1`: 117 matching tests passed across all targets, 0 failed.
- `cargo check --locked --all-targets`, native Windows `cargo build --locked`, `cargo fmt --all -- --check`, and `git diff --check`: passed.

### Files

- `src/browser/commands.rs`
- `src/browser/host/mod.rs`
- `src/browser/host/unsupported.rs`
- `src/browser/host/windows.rs`
- `src/browser/mcp.rs`
- `src/browser/mod.rs`
- `src/browser/pane.rs`
- `src/browser/recipes.rs`
- `src/browser/recording.rs`
- `src/browser/recording_mcp.rs`
- `src/browser/resources.rs`
- `tests/browser_gateway.rs`
- `tests/browser_host.rs`
- `tests/browser_recording_mcp.rs`

## Checkpoint 5: Pane Record/review UI

### Status

Checkpoint 5 started from the approved clean checkpoint-4 head `59846cbf0ff28671125f640aac88d7d4280555d5`. It implements only the native split-pane recording controls and in-memory review/save experience. The immutable final head, stable patch ID, and review package range are recorded by the checkpoint handoff after the commit exists.

Checkpoints 6 through 12 are not implemented. This checkpoint adds no `browser_recording` or `browser_workflow` MCP group, replay compiler/executor/status, runtime secret prompt, locator-failure state, repair preview/apply, or replay lifecycle lease.

### Contract decisions

- The checkpoint-4 `BrowserWorkflowCoordinator` remains the one recording/review authority. The App and host add no mirrored draft, status, or instance map. Every UI action is fenced by the currently active `BrowserWorkspaceKey`, Claude/Codex surface, and exact recording instance.
- Record is explicit and off by default. Only local Claude/Codex panes project Record, active red Stop Recording, or Review controls. Server, SSH, and remote-client surfaces project none. Stop fences page delivery, drains accepted source-order events, and transitions to review without saving.
- The pane receives a bounded value-free projection: safe metadata, step ID/index, User/Agent actor, fixed action summary, eligible conversion kind, wait/assertion counts, locator-assertion eligibility, safe adjacent-move eligibility, and input name/kind/unset status. Captured action values, locators, file paths/content, secret values, tokens, cookies, and headers do not cross this projection or its `Debug` output.
- Review, preview, and keyboard-editor state is volatile and never enters `AppState`. Switching AI routes, leaving for Server/SSH, entering remote mode, disabling Browser, reset, project interrupt, or workspace destruction discards the exact in-memory workflow state; collapsing and reopening the same pane does not. Active recording is never restored after restart.
- Save resolves only the exact owning local project's real canonical root, then uses the existing hardened deterministic atomic recipe store for `.devmanager/browser-workflows/<slug>.json`. Remote clients are rejected before storage. The coordinator lock spans validate/save/discard so another mutation cannot race the bytes; save failure retains the review, and discard happens only after successful atomic replacement.
- Review replaces the page canvas and is vertically scrollable. Full-width wrapping control groups keep metadata, viewport, step, assertion, input, editor, and terminal controls reachable at the 320px pane minimum. Ordered actor-labelled steps support delete and only domain-approved reordering, ordinary Text/URL conversion, bounded unset Text/URL/File/Secret input creation, rename/default/remove, Duration/Load/NetworkIdle wait replacement/removal, URL/title/text/element/value assertion addition/removal, validated preview, Save, and Discard. Shared keyboard editing uses Enter to validate/commit and Escape to cancel. Nested browser buttons stop event propagation.
- Volatile preview carriers do not implement `Debug`; the editor and mutation types use manual value-redacting `Debug`. Generated add-input names select the first unused bounded name, so remove/re-add cannot collide and no add control is emitted at capacity.

### Implemented

- Added safe workflow review projection, typed mutation, immutable preview, atomic local save, and exact discard functions in `browser::pane`, plus a private actor map preserved from the recorder's committed source steps.
- Extended the coordinator and both Windows/unsupported host adapters with one current Recording-or-Review instance seam and review operations. Windows destructive lifecycle now fences/drains/removes recording instrumentation and discards either state for exact workspace/project ownership.
- Replaced the placeholder pane recording action with explicit Start/Stop/Preview/Save/Discard/Mutate/Focus/Cancel actions and state-driven controls. Review mode hides the WebView and renders the responsive native editor and preview.
- Added App dispatch for exact start/stop/review operations, canonical local project-root resolution, fixed path-free failure messages, route-owned volatile state, shared keyboard editing, and WebView visibility suppression during review.
- Added integration coverage for projection redaction and ownership, every typed mutation, immutable preview, atomic save/failure retention/discard, explicit action/model vocabulary, host bridging, App routing without persistence, lifecycle cleanup, rendered control reachability, redacted editor/debug boundaries, and Recording-or-Review project enumeration.

### RED to GREEN evidence

1. Projection and ownership: RED failed with `E0432` because no review projection/state API existed. GREEN projects only exact local Claude/Codex state, preserves User/Agent labels, and omits captured literal/file/secret values.
2. Typed edits: RED failed with unresolved mutation types/functions. GREEN exercises metadata, delete/reorder, Text/URL conversion, add/rename/default/remove input, wait set/remove, and assertion add/remove against exact workspace/instance fences.
3. Preview/save/discard: RED failed with three unresolved APIs. GREEN proves preview clones are immutable, remote/cross-route saves fail before writes, atomic local output has deterministic trailing-newline bytes, failed stores retain review, successful stores and explicit discard retire it.
4. Pane/App/host wiring: successive REDs failed on absent explicit action/model fields, canonical-root helper, host methods, App dispatch calls, safe metadata/index/convertibility fields, editor actions, and Recording-or-Review lifecycle enumeration. Each focused test turned GREEN only after its bounded production seam existed.
5. Native reachability audit: RED reported `BrowserRecipeWait::Duration` was unreachable and then exposed GPUI's requirement for stable scroll IDs. GREEN covers metadata, viewport, delete/reorder/convert, all four input kinds plus rename/default/remove, three safe wait presets/removal, all five assertion kinds/removal, preview/save/discard, keyboard commits, and scrollable review/preview surfaces.
6. Self-review input naming: RED failed with `E0432` for the absent first-unused bounded helper. GREEN fills a removed-name hole without colliding and returns no candidate at the 64-input capacity.
7. Self-review diagnostics: RED proved the pane/App preview carriers still derived `Debug`. GREEN removes that diagnostic path while retaining manual redacted `Debug` for editor and mutation types.

### Independent-review hardening

The initial checkpoint-5 implementation landed as `109530a0b774ecea8f35e7f2f98e58617eb98428`. Independent review rejected four UI/domain boundaries, each reproduced by a focused test before the production fix:

1. Invalid draft repair: RED showed field focus depended on validated preview, trapping a blank name, invalid ID, or invalid viewport. GREEN derives editor state and submission mutations from the current safe projection; the behavioral regression makes all three states editable, repairs them, then previews and saves successfully.
2. Meaningful assertions: RED found hard-coded expected strings and a fabricated locator. GREEN keeps user-entered URL/title/text/value expectations only in redacted volatile editor/mutation state, resolves element/value locators from the actual recorded step under the coordinator lock, rejects blank or locatorless assertions atomically, and never projects locator data.
3. Narrow-pane reachability: RED found dense non-wrapping horizontal control rows. GREEN uses one full-width wrapping group for metadata, step, assertion, input, editor, viewport, add-input, and Preview/Save/Discard controls while retaining stable vertical review and preview scroll containers.
4. Safe ordering: RED reproduced moving `SelectTab` before `CreateTab`. GREEN centralizes one recorder predicate that rejects moves involving or crossing `CreateTab`, `SelectTab`, or `CloseTab`, projects only allowed adjacent moves, and still permits adjacent non-tab reordering. The regression proves rejected moves are atomic and an allowed move remains previewable and saveable.

### Verification

- `cargo test --locked --test browser_workflow_review_ui -- --test-threads=1` -> 14 passed, 0 failed.
- Add-input collision unit regression -> 1 passed, 0 failed.
- Focused pane, recipes, recording, and coordinator integration gate -> 70 passed, 0 failed.
- Full 14-target browser integration gate covering annotations, attachment lifecycle, automation, core, fixture, gateway, host, pane, provider, recipes, recording, recording IPC, coordinator, and review UI -> 235 passed, 0 failed.
- `cargo test --locked browser -- --test-threads=1` -> 112 matching tests passed across all targets, 0 failed.
- `cargo test --locked --lib app::tests -- --test-threads=1` -> 67 passed, 0 failed.
- ProcessManager surrounding gate produced 69 passes plus the same `stopped_server_can_start_again_on_same_terminal_session` server-start timeout on both full runs; the exact failed test reran GREEN 1/1 in 1.53 seconds. This checkpoint has no diff under `src/services`; the repeated full-suite limitation is reported rather than expanding checkpoint-5 scope.
- `cargo check --locked --all-targets` -> exit 0.
- Native Windows `cargo build --locked` -> exit 0.
- `cargo fmt --all -- --check` and `git diff --check` -> exit 0.

### Files

- `src/app/mod.rs`
- `src/browser/host/unsupported.rs`
- `src/browser/host/windows.rs`
- `src/browser/mod.rs`
- `src/browser/pane.rs`
- `src/browser/recording.rs`
- `src/browser/recording_coordinator.rs`
- `tests/browser_pane.rs`
- `tests/browser_workflow_review_ui.rs`
- `.superpowers/sdd/browser-task-5c-checkpoints.md`
- `.superpowers/sdd/progress.md`
- `.superpowers/sdd/browser-task-5c-report.md`

## Checkpoint 4: Unified host capture

### Status

Checkpoint 4 started from the approved clean `master` head `5972652c5df5706ece58ba83b59fd3aa57b563a7` and initially landed as `1f43913c872ce0476cbe476a0b8ae79b826223b2`. It adds only unified in-memory capture for user chrome and queued agent/controller actions through the existing page/recording authority. Independent review rejected the initial implementation because user-chrome capture reserved only after a successful browser mutation, so a recorder capacity/conversion/commit failure could silently leave a saveable draft missing that action. The focused strict-TDD hardening below fixes that transaction boundary. The immutable follow-up head, stable patch ID, and package range are recorded by the checkpoint handoff after the commit exists.

Checkpoints 5 through 12 are not implemented. This checkpoint adds no pane Record/review UI, MCP recording surface, recipe persistence call, replay compiler/executor, runtime secret prompt, or locator repair.

### Contract decisions

- `BrowserWorkflowCoordinator` is the single platform-neutral owner used by Windows host page IPC, user chrome, and agent capture. There is no host recorder mirror, agent recorder, or duplicate workspace-instance map.
- Every producer uses the checkpoint-2 reserve/commit/cancel sequence. Accepted page messages drain before a user command or agent request reserves, user chrome reserves before its browser mutation, agent work reserves before queue admission, and asynchronous completion timing cannot reorder source actions.
- User chrome preflights a sanitized non-sensitive intent before mutation and retains no raw command URL. Browser failure cancels the reservation. Exact successful Workspace responses are converted into response-derived typed actions and committed; every sanitizer, capacity, alias, conversion, and commit failure invalidates/discards only that transaction's exact incomplete recording and emits a typed host diagnostic. Create/select/close use deterministic per-instance logical aliases (`tab-1`, `tab-2`, and so on); runtime tab IDs are never emitted. Navigation uses the returned post-success URL and the existing credential-stripping boundary. Page click/type/select/upload/download remain page IPC only, so chrome capture cannot duplicate them.
- Agent `Act` target metadata is runtime-inspected before any command text is copied into recorder-owned state. Password/security/credential targets and password/one-time-code autocomplete values create only an unset Secret input. Sensitive targets retain only these fixed non-text keys: `Enter`, `Tab`, `Escape`, `Backspace`, `Delete`, `ArrowUp`, `ArrowDown`, `ArrowLeft`, `ArrowRight`, `Home`, `End`, `PageUp`, and `PageDown`. Printable values, whitespace, modifiers, chords, and arbitrary key names are cancelled.
- Upload capture accepts only the semantic locator and materializes an unset File input. CDP capture accepts only the typed method marker. Upload paths/files and CDP params, request bodies, response bodies, resource data, and inline results never enter coordinator state.
- Inactive capture is an unconditional no-op. Stop/discard remove pending agent state for that exact instance; late completions are ignored. Direct user input, Stop/close/reset/profile clear, approval denial, callback/process failure, stale cancellation, and queue interruption converge on the same response finalization/cancellation path available at this checkpoint.

### Implemented

- Added a cloneable, mutex-protected coordinator over one `BrowserWorkflowRecorder`, including exact active-instance/project queries, shared page-recorder ingress, bounded logical tab aliases, and pending agent reservations keyed by workspace and operation.
- Replaced the Windows host's separate recorder plus duplicate instance map with the coordinator. Start/Stop, reinjection, overflow invalidation, reset/profile clear, page IPC, user chrome, and agent queue/controller paths now consult the same authority.
- Extended strict recipe v1 with typed create/select/close tab, back/forward/reload, viewport, and method-only CDP marker actions. Each variant participates in strict deserialize/validate/reference/redaction/review traversal; create-tab URLs pass the existing recording URL sanitizer.
- Captured user tab create/select/close, navigate, back, forward, reload, and viewport changes with an opaque non-Debug/non-Serialize transaction token that owns the exact instance, pre-mutation reservation, and sanitized intent through success commit or browser-failure cancellation. User `Act` remains ignored by chrome capture because semantic page actions arrive only through the private IPC.
- Reserved every recordable agent command before enqueue, prepared `Act` actions only after runtime target inspection, upgraded reservation risk to effective runtime risk, and finalized before delivering the response. Only exact Workspace/Action/Wait/Screenshot/Upload/CDP response shapes can commit; every other result cancels.
- Added safe conversions for semantic actions, waits, screenshots, uploads, and CDP method markers. Invalid or unsupported conversions cancel their reservation without blocking later source-order slots.

### RED to GREEN evidence

1. Shared authority and async ordering: the first focused test failed to compile because `BrowserWorkflowCoordinator` did not exist. GREEN interleaved page, chrome, agent success, and agent failure completions but emitted only successes in source order through one instance.
2. Typed user chrome capture: RED had no coordinator API or typed tab/history/viewport variants. GREEN emitted deterministic strict actions only for exact success; failed navigation and user `Act` emitted nothing; credential query material and runtime tab IDs were absent.
3. Agent inspection and completion: RED lacked reserve/inspect/complete, runtime `autocomplete`, and the CDP marker. GREEN buffered later navigation/upload/CDP behind inspected password typing, cancelled failure, and retained only unset Secret/File plus method-only CDP state.
4. Host boundaries: RED found no capture at host ingress. GREEN reserves before enqueue, inspects before approval/execution, and finalizes before response delivery; interruption, denial, callback failure, and destructive lifecycle paths use the same finalizer.
5. Hardening: RED showed inactive validation, printable password key retention, and Upload commitment from `Acknowledged`. GREEN makes inactive capture inert, uses the fixed key allowlist, and cancels mismatched response variants.
6. Cross-producer order: RED found no page drain before user/agent reservations. GREEN synchronously drains already-accepted page events at both ingress paths.

### Verification

- `cargo test --locked --test browser_workflow_coordinator -- --test-threads=1` -> 11 passed, 0 failed.
- Full browser integration targets covering annotations, attachment lifecycle, automation, core, fixture, gateway, host, pane, provider, recipes, recording, recording IPC, and coordinator -> 219 passed, 0 failed.
- `cargo test --locked browser -- --test-threads=1` -> 109 matching tests passed across all targets, 0 failed.
- `cargo check --locked --all-targets` -> exit 0.
- Native Windows `cargo build --locked` -> exit 0.
- `cargo fmt --all -- --check` -> exit 0.
- `git diff --check` -> exit 0.

### Independent review hardening

The review finding was reproduced before production edits: `cargo test --test browser_workflow_coordinator --no-fail-fast` exited 1 with 12 `E0599` compile errors for the absent `begin_user_chrome_capture` and `complete_user_chrome_capture` transaction API. GREEN establishes the transaction at Windows host ingress after draining prior page IPC and before `handle_command_inner` mutates browser state.

- Preflight runs the recording URL/action sanitizer and reserves capacity without retaining a raw command URL. A sanitizer, invalid-action, or reservation/capacity error synchronously stops and discards only the exact active recording before the host proceeds with the browser command, and the host emits a typed `BrowserHostEvent::Diagnostic`.
- Browser failure cancels the reservation and leaves a complete recording. Exact Workspace success supplies the final tab alias, returned URL, and typed action. Alias exhaustion, response URL rejection, response conversion failure, an ignored/stale commit, or a recorder commit error invalidates and discards only the token's exact recording, so no draft missing a successful mutation remains saveable.
- Exact-instance invalidation holds the coordinator lock, matches the token's instance ID, and handles either Recording or Review state. A late completion from a stopped/discarded instance cannot stop, discard, or remove aliases from a newer restarted instance.
- Behavioral coverage includes the complete successful chrome sequence; browser-failure cancellation; direct preflight URL rejection; zero-capacity reservation failure; 65th logical-alias failure; unsafe returned-URL rejection; a genuine post-success stale-reservation commit failure; exact-instance restart fencing; and page-IPC-before-chrome reservation order.

Follow-up verification:

- `cargo test --locked --test browser_workflow_coordinator -- --test-threads=1` -> 13 passed, 0 failed.
- `cargo test --lib post_success_commit_failure_invalidates_the_exact_user_chrome_recording` -> 1 passed, 0 failed.
- Focused host, recording, recording IPC, and coordinator integration targets -> 117 passed, 0 failed.
- Full browser integration targets covering annotations, attachment lifecycle, automation, core, fixture, gateway, host, pane, provider, recipes, recording, recording IPC, and coordinator -> 221 passed, 0 failed.
- `cargo test --locked browser -- --test-threads=1` -> 110 matching tests passed across all targets, 0 failed.
- `cargo check --locked --all-targets` -> exit 0.
- Native Windows `cargo build --locked` -> exit 0.
- `cargo fmt --all -- --check` and `git diff --check` -> exit 0.

### Files

- `src/browser/recording_coordinator.rs`
- `src/browser/mod.rs`
- `src/browser/recording.rs`
- `src/browser/recipes.rs`
- `src/browser/automation.rs`
- `src/browser/host/initialization.rs`
- `src/browser/host/windows.rs`
- `tests/browser_workflow_coordinator.rs`
- `tests/browser_automation.rs`
- `tests/browser_host.rs`
- `tests/browser_recording_ipc.rs`
- `.superpowers/sdd/browser-task-5c-checkpoints.md`
- `.superpowers/sdd/progress.md`
- `.superpowers/sdd/browser-task-5c-report.md`

## Checkpoint 3: Semantic page recording IPC

### Status

Checkpoint 3 started from the approved clean `master` head `f11183989e324440bd0722c9fa3b51157c5b7c0a`. It adds only the active recording page-IPC and Windows host lifecycle seam. The immutable final head, stable patch ID, and package range are recorded by the checkpoint handoff after the commit exists.

Checkpoints 4 through 12 are not implemented. In particular, this checkpoint adds no pane controls, MCP surface, recipe persistence, replay, runtime secret prompt, locator repair, or agent/chrome action capture. Tab create/select/close remains host chrome capture for checkpoint 4; it is deliberately not accepted from page JavaScript.

### Contract decisions

- The always-on page adapter remains recording-free. A fresh script exists only for an exact active recording workspace/tab/revision/origin/instance/nonce authority and is removed on Stop, navigation/reload/history traversal, tab close, workspace reset, and project-profile clear.
- Page messages use one strict v1 envelope. Unknown or duplicate members at any object depth, malformed JSON, unsafe identifiers/origins, oversized strings/bodies, excessive nesting/items, stale sequences, and mismatched workspace/tab/revision/origin/instance/actor/source/nonce fail closed.
- Wry's observed request-URI origin is authoritative; a body origin cannot override it. Raw messages cross a private non-`Debug`, non-serializable bounded `SyncSender` and never enter `BrowserHostEvent`.
- Only trusted semantic page actions are accepted: click, ordinary text/clear, select, safe navigation, upload marker, and download marker. Password, credential-like text, paste, and file selection branch before value/file access and become content-free Secret/File recording actions.
- The recorder's checkpoint-2 reserve/commit/cancel authority remains the only retained-state path. Semantic messages drain before generic user-input revision events so the page revision fence reflects the event's source order.

### Implemented

- Added platform-neutral `BrowserPageRecordingAuthority`, strict envelope/event types, bounded parser, duplicate-member visitor, replay fence, transport-origin verification, recorder conversion, and exact activation/deactivation scripts.
- Added semantic locator capture using bounded accessibility role/name, test ID, and CSS fallbacks. The source script ignores untrusted events and annotation overlays, strips credentials from navigation URLs, and never reads clipboard contents, file lists/paths, cookies, storage, HTML, computed style, or password values.
- Added the Windows host's private 256-message recording queue, exact per-view authorities, explicit start/stop/status seam, post-load reinjection with a fresh nonce, and teardown/discard across all destructive view/workspace/profile lifecycle paths.
- Added a compile-safe unsupported-platform adapter that reports recording unavailable/inactive without exposing a partial implementation.

### Independent review hardening

- Stop now synchronously fences each exact per-view transport authority, drains every already-accepted raw message through the still-active recorder in global source order, and only then retires the view authorities and stops the recorder. Post-fence delivery is ignored before queueing; Start drains prior transport state before publishing a replacement instance.
- The bounded transport no longer ignores `try_send` failure. Overflow/disconnection produces a private typed per-view failure plus a diagnostic; an overflow for the exact live instance discards the incomplete recording, and old-instance traffic is rejected at the ingress gate before it can consume replacement capacity.
- Source-side credential detection now covers bare JWT, OpenAI-style, GitHub-style, AWS-style, and Google-style high-confidence tokens in ordinary text, selected values, and locator metadata. Sensitive selections become content-free Secret markers. Locator capture no longer reads label or element text, and the Rust parser independently rejects crafted sensitive text/select/navigation/locator values before reservation.
- All authority, envelope, script, and observed-request origin checks share the `url` parser's canonical HTTP(S) origin form, including case folding, default-port normalization, IPv6, and IDN handling. Credentials, non-HTTP(S), malformed origins, and origin fields containing paths, queries, or fragments fail closed.

Review RED to GREEN:

- Queue RED: the behavioral transport tests failed with E0425/E0433 because no instance-gated transport or typed submit/failure result existed; the Windows Stop-order regression then failed at the missing synchronous fence. GREEN: 2/2 transport behaviors and the exact Windows fence/drain/retire order passed, including accepted pre-Stop retention, late suppression, overflow signaling, and restart starvation fencing.
- Secret RED: crafted JWT text returned `Ok(Recorded)` instead of `Err(Malformed)`, while the executable Node harness failed with `secret marker missing at 5`. GREEN: both Rust defense-in-depth and raw-wire Node tests passed for JWT, `sk-proj`, GitHub, AWS, aria, label, text, and select sentinels; neither wire JSON nor recipe JSON contains them.
- Origin RED: the canonical-origin regression failed with E0432 because no shared canonical origin API existed. GREEN: canonical equivalence and spoof rejection passed across case/default ports, IPv6, IDN, credentials, malformed/non-HTTP schemes, and authority mismatches.

### RED to GREEN evidence

1. Strict authority/envelope/parser:
   - RED: `cargo test --locked --test browser_recording_ipc -- --test-threads=1` failed with E0432 because the authority, envelope/event, IPC, errors, and bounds did not exist.
   - GREEN: strict parsing, exact authority, duplicate/unknown-field rejection, origin/nonce/revision fencing, replay rejection, and successful recorder commitment passed.
2. Secret-safe semantic conversion:
   - RED: the semantic regression accepted nested `{type: "password", text: ...}` and retained an invalid sensitive shape.
   - GREEN: strict empty marker variants reject extra fields; password, clipboard, credential-like text, and upload produce only unset Secret/File inputs, while ordinary text/select/navigation/download/click retain safe semantics.
3. Active-only script lifecycle:
   - RED: the focused lifecycle test failed with E0599 because activation/deactivation scripts did not exist.
   - GREEN: the Rust lifecycle test and Node runtime harness passed; password/file getters throw if touched, trusted safe markers emit, untrusted input is ignored, and teardown removes every listener.
4. Windows host integration:
   - RED: host coverage first failed because `BrowserPageRecordingRawMessage` and then install/remove/discard helpers and the unsupported adapter were absent.
   - GREEN: the private bounded channel, start/stop/status seam, pre-revision drain, origin observation, reinjection, and lifecycle fencing all passed.
5. Adversarial bounds and stale delivery:
   - RED: count-bound coverage failed before the explicit limits and `TooManyItems` error existed; transport coverage failed before `SyncSender`; reset/profile coverage failed before explicit discard calls.
   - GREEN: excessive values/fallbacks/strings/depth/body size fail before retention, `try_send` bounds raw queueing, and late old-instance or wrong-origin messages cannot reserve into a replacement recording.

### Verification

- `cargo test --locked --lib browser::recording_ipc::transport_tests -- --test-threads=1` -> 2 passed, 0 failed.
- `cargo test --locked --test browser_recording_ipc --test browser_recording --test browser_host -- --test-threads=1` -> 104 passed, 0 failed.
- Full browser integration target command covering annotations, attachment lifecycle, automation, core, fixture, gateway, host, pane, provider, recipes, recording, and recording IPC -> 208 passed, 0 failed.
- `cargo test --locked browser -- --test-threads=1` -> 109 matching tests passed across all targets, 0 failed.
- `cargo test --locked services::process_manager::tests --lib -- --test-threads=1` -> 70 passed, 0 failed.
- `cargo check --locked --all-targets` -> exit 0.
- Native Windows `cargo build --locked` -> exit 0.
- `cargo fmt --all -- --check` -> exit 0.
- `git diff --check` -> exit 0.
- Production-source scan found none of the forbidden clipboard/file-list/cookie/storage/HTML/computed-style reads; the Node harness verifies sensitive getters are not evaluated and sentinel values do not cross the wire.

### Files

- `src/browser/recording_ipc.rs`
- `src/browser/mod.rs`
- `src/browser/host/windows.rs`
- `src/browser/host/unsupported.rs`
- `tests/browser_recording_ipc.rs`
- `.superpowers/sdd/browser-task-5c-checkpoints.md`
- `.superpowers/sdd/progress.md`
- `.superpowers/sdd/browser-task-5c-report.md`
- `Cargo.toml`
- `Cargo.lock`

## Checkpoint 2: Pure recording/review domain

### Status

Checkpoint 2 started on the approved checkpoint-1 head `64c9f394f1e3fd3229d9c9b79bd765d5ed748c91` and landed initially as `36a3bf189e66f6cbe65283611f350d512fdcf7f1`. Independent review did not approve that first implementation and identified four Important findings; the first focused hardening follow-up landed as `b61b0a6dc83ae434ba875aaba0aec117ff029fb8`. Re-review then identified one remaining generated-input lifecycle finding. It is addressed under strict RED-to-GREEN in the second focused follow-up documented below. The immutable follow-up head, patch ID, and package range are recorded by the checkpoint handoff after the commit exists.

Checkpoints 3 through 12 are not implemented. This checkpoint adds no page IPC, host/pane integration, persistence or recipe-store write, MCP tool, replay, secret prompt, locator repair, or lifecycle wiring.

### Contract decisions

- `BrowserWorkflowRecorder` is a platform-neutral, in-memory authority keyed only by `BrowserWorkspaceKey`. It is inactive by default, starts only explicitly, and does not implement serialization or persistence.
- `start` returns an exact `BrowserRecordingInstance`. Reservations carry the instance/workspace fence and a monotonic source-order ticket; asynchronous completion order cannot reorder source actions.
- Capture values cross a non-`Debug`, non-`Serialize` `BrowserRecordingAction` boundary. Password, clipboard, and file constructors accept no value/content. Credential-like text is replaced with an unset Secret marker before pending state; raw, encoded, and repeatedly encoded URL query/fragment credential keys are removed before retention, while invalid percent encodings fail closed.
- Stop cancels unresolved slots, drains successes that completed before Stop in source order, fences later completions as `Ignored`, and returns an immutable review clone. Discard removes only that exact instance.
- Review edits mutate the recorder-owned copy and return fresh immutable previews. `recipe_for_save` clones only after `BrowserRecipeV1::validate`; it does not call the recipe store.

### Implemented

- Added bounded per-workspace recording state with explicit start/status, deterministic instance/reservation/step/input IDs, reserve/commit/cancel ordering, cross-workspace isolation, restart fencing, and late-completion suppression.
- Added successful-action capture for strict recipe actions plus safe navigation, text, password, clipboard, and upload constructors. Failed/cancelled actions never become steps.
- Added deterministic adjacent coalescing for literal typing/clear transitions, repeated select state, exact duplicate navigation, and sensitive typing markers. Coalescing never crosses actor, tab, locator, risk, wait, or assertion boundaries and never creates an orphan Secret input.
- Added unset generated Secret/File inputs. Password, credential-like text, clipboard content, file paths/contents, cookies, tokens, bearer/basic values, and credential URL query members cannot enter retained state.
- Added review metadata, delete/reorder, literal-to-Text/URL conversion, input add/rename/default/remove with reference safety, wait replacement, assertion add/remove, immutable preview, strict save handoff validation, and discard.
- Kept recorder, capture action, metadata, and review non-`Debug` and non-`Serialize`; tests assert these boundaries at compile time.

### Independent review hardening

- URL security inspection now validates raw percent encoding, repeatedly decodes an inspection-only copy through eight bounded passes, removes credential-bearing query pairs or fragments after decoding, recognizes session keys, and preserves the original bytes of legitimate encoded query/fragment values.
- Review state now tracks generated-input provenance privately. Deleting a step removes only generated Secret/File/Text/URL definitions that are no longer referenced, retains generated definitions shared by another step, and never removes explicitly user-added review inputs. Provenance follows rename and explicit removal.
- Generic captured actions, waits, and assertions fail closed on every `BrowserRecipeValue::Input`; no unresolved input reference can enter pending or retained capture state. Literal navigation and generated Secret/File constructors remain the supported capture paths.
- Exported hard bounds cover 64 total review inputs, 16 assertions per captured/review action, and 256 total review assertions. Buffered generated-input/assertion commitments reserve against the same totals; capture and review overflow return typed `CapacityExceeded`, cancel the rejected reservation when needed, and leave retained recipe state atomic.
- Centralized generated-input collection now follows every successful mutation that can remove or replace recipe input references: step deletion, action-value input conversion/rename, wait removal/replacement, and assertion removal. Shared generated definitions survive until their last reference disappears, explicitly added inputs are never collected, and validation/index/capacity failures return before both mutation and collection. Step reorder is intentionally excluded because it does not change the reference graph.

Consolidated review RED to GREEN:

- RED: `cargo test --locked --test browser_recording review_hardening_rejects_encoded_secrets_or_unbounded_invalid_state -- --exact --test-threads=1` exited 1 with exactly `encoded URL credentials, generated input garbage collection, unresolved generic input capture, retained collection capacity and atomicity`.
- Each minimal production slice removed only its corresponding finding from the same failure. After URL inspection, generated-input provenance, and generic-input fail-closed changes, the exact remaining failure was `retained collection capacity and atomicity`.
- GREEN: the same exact command passed 1/1 after the fixed collection bounds and atomic overflow handling landed.

Second re-review RED to GREEN:

- RED: `cargo test --locked --test browser_recording generated_input_gc_follows_successful_reference_mutations_atomically -- --exact --test-threads=1` exited 1 with exactly `wait removal left a generated input orphan, wait replacement left a generated input orphan, assertion removal left a generated input orphan`; shared-reference preservation and invalid wait/index atomicity already passed.
- GREEN: the same exact command passed 1/1 after all successful reference-changing review mutations shared one post-mutation collector.

### RED to GREEN evidence

1. Explicit instance and async source ordering:
   - RED: `cargo test --locked --test browser_recording recorder_is_explicit_orders_async_commits_and_fences_workspace_instances -- --exact --test-threads=1` exited 1 with E0432 because `BrowserWorkflowRecorder`, actor/status/error types, and instance/ticket ordering did not exist.
   - GREEN: the same command passed 1/1 with default-off, two-workspace isolation, completion order 2 then 1 producing source order 1 then 2, discard/restart, and stale-instance rejection.
2. Cancel/failure, capacity, and late completion:
   - RED: the focused test failed with E0432/E0599 because `BrowserRecordingCommit` and `cancel` did not exist.
   - GREEN: `cancellation_capacity_and_late_completion_preserve_the_exact_instance` passed 1/1; overflow is typed, cancellation unblocks a buffered success without recording the failure, and post-Stop completion is `Ignored`.
3. Coalescing and redaction:
   - RED: the focused test failed with E0432/E0599 because the safe capture action and tab/risk-aware reservation surface did not exist.
   - GREEN: `coalescing_and_redaction_produce_only_safe_unset_inputs` passed 1/1 with literal typing coalesced, unset Secret/Secret/File inputs, token-query stripping, valid v1 output, and no forbidden sentinel.
4. Coalescing boundaries and stable IDs:
   - RED: `coalescing_never_crosses_actor_tab_locator_risk_wait_or_assertion_boundaries` ran and failed with 4 steps instead of 2 for type to clear to type plus exact duplicate navigation.
   - GREEN: the same command passed 1/1; every required boundary splits, safe transitions coalesce, and retained step IDs remain deterministic.
5. Immutable review and save handoff:
   - RED: the focused review test exited 1 with E0432/E0599 across the absent metadata and 21 review/handoff methods or variants.
   - GREEN: `review_mutations_are_immutable_validated_and_discardable_without_saving` passed 1/1 across metadata, delete/reorder, Text/URL input conversion and editing, wait/assertion editing, immutable previews, invalid secret-default rejection, reference safety, v1 validation, and discard.
6. Cookie/token/clipboard non-retention:
   - RED: the focused test exited 1 with E0599 because there was no content-free clipboard capture boundary.
   - GREEN: `cookie_token_and_clipboard_values_never_enter_recording_state` passed 1/1; all three become unset Secret definitions and no sentinel appears in recipe JSON.
7. Stop-time ordered drain:
   - RED: `stop_cancels_unresolved_slots_but_keeps_later_successes_completed_in_time` ran with 0 retained steps instead of 1 when a later ticket completed before Stop behind an unresolved earlier ticket.
   - GREEN: the same command passed 1/1 after Stop cancels unresolved slots, drains already-ready successes in source order, and fences the late earlier completion.
8. Sensitive typing coalescing:
   - RED: `sensitive_typing_coalesces_without_allocating_orphan_inputs` ran with 3 steps instead of 2 because adjacent same-context password markers each allocated a Secret input.
   - GREEN: the same command passed 1/1 after pre-materialization coalescing reuses the prior unset Secret step only across an exact safe context; an actor change remains a boundary.

### Verification

- `cargo test --locked --test browser_recording -- --test-threads=1` -> 10 passed, 0 failed.
- `cargo test --locked --test browser_recipes -- --test-threads=1` -> 15 passed, 0 failed.
- `cargo test --locked --lib browser::recipes::tests -- --test-threads=1` -> 5 passed, 0 failed.
- `cargo test --locked browser -- --test-threads=1` -> 107 matching tests passed across all targets, 0 failed.
- Full browser target command covering annotations, attachment lifecycle, automation/resources, core/model/errors, fixture, gateway, host, pane, provider, recipes, and recording -> 196 passed, 0 failed.
- `cargo check --locked --all-targets` -> exit 0.
- Native Windows `cargo build --locked` -> exit 0.
- `cargo fmt --all -- --check` -> exit 0.
- `git diff --check` -> exit 0.
- Production-source scan confirms no filesystem/store call and no serialization derive in `recording.rs`; compile-time assertions cover capture/review secret-bearing state.

### Files

- `src/browser/mod.rs`
- `src/browser/recording.rs`
- `tests/browser_recording.rs`
- `.superpowers/sdd/browser-task-5c-checkpoints.md`
- `.superpowers/sdd/progress.md`
- `.superpowers/sdd/browser-task-5c-report.md`

## Checkpoint 1: Strict recipe wire/store

### Status

Checkpoint 1 is complete on the approved base `e088ccab1ce10afa73ae58c0ecf15077616d9a82`. This report is part of the focused checkpoint commit. The immutable final head, patch ID, and package range are recorded by the checkpoint handoff after the commit exists.

At the checkpoint-1 commit, checkpoints 2 through 12 were not implemented. Checkpoint 1 itself contains no recording, review UI, replay, secret prompt, locator repair, or Task 5C MCP surface.

### Contract decision

The unreleased flat step wire (`action` string plus `locator`, `valueRef`, `waitCondition`, and string assertions) is not accepted as a second v1 format. The repository has one strict v1 JSON contract. Source-level conversion between shared browser viewport/locator models remains available through `From`, but deserialization does not guess, alias, or partially interpret an old or future shape.

### Implemented

- Added strict recipe-specific viewport, locator, value, action, wait, assertion, and element-state types. Every object-shaped wire node denies unknown fields.
- Made top-level deserialization inspect `schemaVersion` before v1 shape parsing. Only exact version 1 is accepted; `load_recipe` returns `UnsupportedRecipeVersion` for a future version even when the future body is not v1.
- Added validation for safe recipe/step slugs, unique step IDs, trimmed unique input names, nonempty steps, viewport bounds, semantic locator fallbacks, required values, wait/timeout bounds, and typed assertions.
- Added input-reference type checking: URL uses require URL inputs, ordinary typed values use Text, upload requires File, password-like targets require Secret, and Secret values cannot enter assertions or waits.
- Reject Secret and File defaults at both serialization and nested-input deserialization boundaries. Credential-like metadata, URL credentials/query keys, sensitive literal assignments, password-target literals, file-upload literals, and secret/file-content aliases cannot enter emitted v1 JSON.
- Added deterministic pretty JSON with a trailing newline and an exact SHA-256 byte fixture.
- Added `list_recipes`, restricted to direct safe-slug `.devmanager/browser-workflows/<slug>.json` files in deterministic ID order. Load/save/list reject non-directory components, symlink classifications, non-regular recipe files, ID/file mismatches, and traversal slugs.
- Replaced direct writes with a same-directory, random `create_new` sibling temp, full write plus `sync_all`, and one atomic replace. Windows uses `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH`; in-process saves are serialized to avoid Windows replace races.
- Added RAII temp cleanup, injected replace-failure coverage, a real Windows locked-destination failure test, concurrent-save coverage, and checks that no operation leaves an orphan temp.

### Independent review hardening

- Replaced `serde_json::Value`'s last-member-wins object parsing with a recursive strict parser. Duplicate members now fail at every object depth, including `schemaVersion`, action tags, and nested input/value members; future versions are still reported before v1 body parsing.
- Made every public object-shaped nested wire type validate on direct deserialization. Context-free invariants now hold even when callers deserialize Action, Value, Wait, Viewport, Locator, Assertion, Step, or Input without going through the top-level recipe.
- Added Windows `FILE_ATTRIBUTE_REPARSE_POINT` classification in addition to symlink classification. The workflow directory and relevant recipe destination are revalidated immediately before list/read, sibling-temp open, and atomic replace boundaries.
- Added injected boundary tests for reparse swaps before read, temporary open, and replacement. A rejected replacement preserves the old complete document and removes the sibling temp without calling the replacer.
- Gave recipe temps an exact store-owned prefix and nonce shape. Save scavenges only direct regular files matching that shape, only after a 24-hour stale threshold, with a 1,024-entry scan bound and 64-delete bound; fresh files, lookalikes, malformed names, and matching directories survive.

### RED to GREEN evidence

1. Strict typed document and deterministic save/load:
   - RED: `cargo test --locked --test browser_recipes browser_recipe_strict_typed_v1_round_trips_with_deterministic_bytes -- --exact --test-threads=1` exited 1 because `BrowserRecipeAssertion`, `BrowserRecipeLocator`, `BrowserRecipeValue`, `BrowserRecipeViewport`, and `BrowserRecipeWait` did not exist; the old action/step fields could not construct the typed fixture.
   - GREEN: the same command passed 1/1. It saves twice, asserts identical bytes, trailing newline, strict tagged nodes, exact byte hash, no temp, and round-trip equality.
2. Strict nested fields and old flat shape:
   - RED: the strict nested/old-flat test failed to compile with the same absent typed contract before production edits.
   - GREEN: `browser_recipe_rejects_unknown_nested_fields_and_the_old_flat_step_shape` passed 1/1 across viewport, input, step, action, locator, value, wait, and assertion unknown fields.
3. Direct repository listing:
   - RED: `browser_recipe_list_reads_only_direct_safe_slug_json_files_in_id_order` exited 1 with unresolved import `list_recipes`.
   - GREEN: the same command passed 1/1 and ignored README, temp, unsafe-slug, and nested entries.
4. Path classification:
   - RED: `browser_recipe_paths_reject_traversal_and_non_directory_components` failed because a hostile `.devmanager` file returned the wrong error class.
   - GREEN: the same command passed 1/1 after direct component classification returned `OutsideWorkspace`. The pure symlink classification test passed without requiring Windows symlink privilege.
5. Metadata redaction:
   - RED: `browser_recipe_serialization_rejects_credential_material_without_echoing_it` failed because direct serialization succeeded and emitted the bearer sentinel.
   - GREEN: the same command passed 1/1 after validation moved before serialization and rejects without echoing the value.
6. Nested Secret/File defaults:
   - RED: `browser_recipe_input_wire_rejects_secret_and_file_defaults_on_deserialize` failed because nested `BrowserRecipeInput` deserialization produced a Secret input with `Some(default)`.
   - GREEN: the same command passed 1/1 after strict validate-on-deserialize.
7. Required steps and file targets:
   - RED: `browser_recipe_validation_requires_steps_and_upload_actions_for_file_targets` failed because an empty recipe validated successfully.
   - GREEN: the same command passed 1/1; empty recipes fail and file input targets require typed Upload rather than Type literals.
8. Concurrent Windows replacement:
   - RED 1: `browser_recipe_concurrent_saves_leave_one_complete_document_and_no_temps` failed with Windows error 183 while threads raced directory creation.
   - RED 2: after race-safe directory creation, the same test exposed concurrent Windows replace `Access is denied` failures.
   - GREEN: the same command passed 1/1 after a poisoned-lock-safe in-process write gate; the winner is one complete parseable document and no temp remains.
9. Duplicate JSON members:
   - RED: `browser_recipe_rejects_duplicate_top_level_and_nested_members` accepted a duplicate `schemaVersion` document because deserialization first collapsed the object into `serde_json::Value`.
   - GREEN: the same command passed 1/1; duplicate top-level, action-tag, and nested value members all fail with a duplicate-member error.
10. Direct nested wire safety:
   - RED: `browser_recipe_public_nested_wire_rejects_context_free_unsafe_values` constructed a direct Upload action containing a literal file value. After the action gate, it exposed direct `BrowserRecipeValue` deserialization accepting an Authorization bearer literal.
   - GREEN: the same command passed 1/1 across direct Action, Value, Wait, Viewport, Locator, and Assertion deserialization; Step and Input use the same strict checked boundary.
11. Reparse and operation-boundary validation:
   - RED: the new unit regressions failed to compile because there was no reparse-point kind, Windows attribute classifier, operation-boundary verifier, or injected checked read/write seam.
   - GREEN: `recipe_path_classification_rejects_windows_reparse_attributes` and `injected_reparse_swap_blocks_read_temp_open_and_replace_boundaries` passed. Injected swaps block all three I/O boundaries; replacement is not called, old bytes survive, and the temp is cleaned.
12. Owned stale-temp cleanup:
   - RED: the new unit regression failed to compile because no bounded owned-temp scavenger or ownership classifier existed.
   - GREEN: `stale_temp_scavenger_is_bounded_and_removes_only_owned_regular_files` passed. Fresh exact temps survive; injected stale cleanup removes exactly the 64-file bound while preserving lookalikes, malformed names, and directories.

Additional atomic failure verification:

- `browser::recipes::tests::recipe_atomic_replace_failure_preserves_old_file_and_cleans_sibling_temp` passed with an injected same-directory replace failure: the original complete bytes survived and only the destination remained.
- `browser_recipe_windows_replace_failure_preserves_old_bytes_and_cleans_temp` passed against the real Windows API while the destination was locked against replacement.

### Verification

- `cargo test --locked --test browser_recipes -- --test-threads=1` -> 15 passed, 0 failed.
- `cargo test --locked --test browser_core -- --test-threads=1` -> 17 passed, 0 failed.
- `cargo test --locked --lib browser::recipes::tests -- --test-threads=1` -> 5 passed, 0 failed.
- `cargo test --locked browser -- --test-threads=1` -> 107 matching tests passed across all targets, 0 failed.
- Full browser target command covering annotations, attachment lifecycle, automation/resources, core/model/errors, fixture, gateway, host, pane, provider, and recipes -> 186 passed, 0 failed.
- `cargo check --locked --all-targets` -> exit 0.
- Native Windows `cargo build --locked` -> exit 0.
- `cargo fmt --all -- --check` -> exit 0.
- `git diff --check` -> exit 0.

### Files

- `Cargo.toml`
- `src/browser/mod.rs`
- `src/browser/recipes.rs`
- `tests/browser_core.rs`
- `tests/browser_recipes.rs`
- `.superpowers/sdd/browser-task-5c-checkpoints.md`
- `.superpowers/sdd/progress.md`
- `.superpowers/sdd/browser-task-5c-report.md`
