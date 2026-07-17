# Task 5C Report: Sequential checkpoints

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
