# Task 5B Report: Browser Annotations

## Status

Task 5B is implemented through checkpoint 4C1, including two independent-review hardening follow-ups, and awaits re-review. Persistence/restored AI workspaces, native element/region capture, the attachment transaction core, ProcessManager session ownership, local native exactly-once PTY attachment, authoritative AppState/host projection with attachment pin reconciliation, and native pending-annotation chips/preview/remove UI are implemented. Remote web input wiring remains intentionally out of scope here.

## Commit range

- Persistence and restored AI workspaces: `ae1229b` (`feat: persist browser annotations and AI workspaces`)
- Native element/region capture: `5a066c1` (`feat: add native browser annotations`)
- Capture lifecycle hardening: `fe90233` (`fix: harden browser annotation lifecycle`)
- Authenticated MCP operations/resources: this checkpoint commit
- Checkpoint 4B1 session lifecycle and local user-origin input: this checkpoint commit
- Checkpoint 4C1 native pending-annotation chips: this checkpoint commit
- Checkpoint 4C1 independent-review hardening: this follow-up commit
- Checkpoint 4C1 terminal-model notice lifecycle hardening: this second follow-up commit
- Branch: `master`, explicitly authorized by the user

## Authenticated MCP architecture

- Added the exact `browser_annotations` grouped tool with `list | get | resolve | unresolve | delete`, the shared exact seven-value risk enum, nonblank intent, optional `annotationId`, no routing fields, and a strict no-unknown-fields schema. A lenient wire wrapper converts malformed arguments into the same structured MCP `isError` envelope instead of a transport-only error.
- Added shared typed `BrowserAnnotationOperation`, compact summary, full details, mutation result, `BrowserCommand`, and `BrowserResponse` interfaces. Host responses carry authoritative post-journal workspace mutations and the resource IDs needed by audit and persistence consumers.
- Kept the Windows host as the sole mutator. List/get/resolve/unresolve/delete route through the bound controller and host operation queue. Delete always raises effective risk to `Destructive`, enters a distinct DevManager approval/resume path before mutation, and denial is journaled without changing annotation state.
- `list` returns short redacted comments/URLs and deliberately omits screenshot handles so an unjournaled compact response cannot advertise an expiring resource. `get` returns the full redacted structured annotation plus owner-bound screenshot and dedicated JSON-details resource handles.
- Added `AnnotationDetails` as a bounded resource kind. Before `get` succeeds, the host verifies screenshot owner, opaque ID, direct-file byte size, `AnnotationScreenshot` kind, and `image/png` MIME, temporarily pins it, creates the redacted details resource, and rolls back the temporary pin on creation failure.
- Resolve/unresolve/delete validate and pin the owned screenshot before state mutation. Direct synchronous callers reconcile after the mutation; agent calls reconcile again after the audit journal append. Annotation resources stay pinned while referenced by unresolved/pending annotations or bounded journal entries and are released after later journal eviction, including eviction caused by non-annotation actions.
- Standard rmcp `read_resource` remains registration-lease and immutable-workspace-owner bound. Cross-workspace annotation IDs return `missing_annotation`; forged cross-owner or same-owner wrong-kind screenshot references return `missing_resource` without exposing the other resource.
- Annotation list/get/mutation responses participate in the existing GPUI/app snapshot synchronization so persisted app state receives the host's post-journal snapshot.

## RED / GREEN evidence

- The real rmcp schema test was RED because tool listing contained the existing thirteen tools but no `browser_annotations`. It is GREEN for the exact tool name, exact operation/risk enums, required fields, `additionalProperties: false`, absence of routing fields, structured unknown-field errors, blank intent, and typed missing `annotationId`.
- The host contract test was RED on missing operation/summary/details/mutation APIs and the dedicated resource kind. It is GREEN for compact redaction, owner isolation, full details creation/read, response-to-journal resource linkage, resolve/unresolve/delete state, and shared screenshot handles.
- Failure tests are GREEN for details-resource creation rollback, forged cross-owner/same-owner resource denial, byte-size/kind/MIME validation, shared-reference pins, direct-caller cleanup, and non-annotation journal eviction releasing the final annotation resource reference.
- The authenticated real rmcp test is GREEN for list, get, standard screenshot/details reads, cross-workspace missing annotation, and cross-token resource denial.
- Windows-path invariants are GREEN for forced destructive delete risk, approval before `handle_command`, distinct annotation approval resume, denial before resume, post-journal response mutation/resource linkage, and pin reconciliation after every agent journal append.

## Verification

- `cargo fmt --all -- --check` - PASS
- `git diff --check` - PASS
- Focused browser matrix - PASS, 147/147:
  - `browser_annotations` 5
  - `browser_automation` 12
  - `browser_core` 17
  - `browser_gateway` 14
  - `browser_host` 75
  - `browser_pane` 24
- `cargo check --locked --lib` - PASS

## Independent review hardening

- Blocking review found that unrelated agent journaling could unpin screenshot resources still owned by the native draft editor. Resource reconciliation now unions every live lifecycle-owned draft screenshot for the exact workspace; the RED/GREEN regression also proves bounded cleanup preserves it until the draft is consumed.
- Blocking review found that raw synchronous `Annotations::Get` bypassed queued response cleanup and could leave every generated details resource pinned. All direct annotation commands now finalize resource pins before returning, while queued agent calls finalize again after their journal link is appended. Repeated direct Get details and resolved screenshots are verified unpinned.
- Review found that saved annotation URLs are deliberately redacted but staleness compared them with raw tab URLs. Staleness now compares equivalent deterministic redactions, keeping a fresh secret-query annotation current while still detecting actual navigation.
- Hardening RED evidence: the secret-query freshness assertion failed; the direct-command source-path assertion failed; and the live-draft test failed to compile because lifecycle-owned draft resources were not queryable. All three are GREEN after the fixes.
- Post-hardening focused matrix - PASS, 151/151: annotations 5, automation 12, core 17, gateway 14, host 79, pane 24. Formatting, diff checks, and `cargo check --locked --lib` also pass.

## Remaining Task 5B work

- Wire remote web composer/input without changing its established wire contract.
- Complete re-review of checkpoint 4C1 hardening.

## Checkpoint 4A: attachment transaction core

- Added a dedicated `BrowserAttachmentRevision`; saving, acknowledging, or detaching pending annotations advances it without changing page/DOM `BrowserRevision`.
- Added one cloneable `BrowserAttachmentBroker`, keyed solely by `BrowserWorkspaceKey`, with per-session generation bindings, immutable exact-ID reservations, rollback, exact commit, workspace isolation, runtime projections, bounded tombstones, and dirty-projection draining.
- A replacement session clears only the old generation's active reservation. Stale reservations cannot commit against a replacement binding, and stale snapshots cannot resurrect committed/detached IDs.
- The prompt-boundary classifier accepts printable UTF-8 text, ordinary space, CR/LF, and all nonempty Paste input; it rejects empty, invalid UTF-8, escape/navigation/mouse/control bytes, DEL, C0, and C1 input. Paste provenance remains a distinct input kind.
- Generated preambles contain only compact redacted IDs/comments/URLs plus the `browser_annotations` instruction. They are single-line, control-free, bounded to 2 KiB, and retain a terminal separator even when truncated.
- `TerminalSession` now provides compound user text/raw/paste writes. Prefix and input are concatenated before one writer lock, one `write_all`, and one flush; bracketed paste markers wrap only sanitized user text, never the annotation preamble.
- Strict RED evidence was the missing broker/revision/composite interfaces (42 compile errors), followed by a focused truncation regression whose preamble lost its separator at the 2 KiB boundary. Both are GREEN.
- Verification: attachment tests 7/7, composite PTY tests 3/3, `browser_core` 17/17, `browser_annotations` 5/5, all terminal-session tests 12/12, formatting/diff checks, and `cargo check --locked --lib` pass.

Checkpoint 4B must wire the broker into ProcessManager session lifecycle, explicit user-origin local/remote input paths, AppState/host projection and resource-pin reconciliation. Checkpoint 4C then renders native AI-only chips and preview/remove actions.

## Checkpoint 4A review fixes (uncommitted)

- Review RED: `snapshot_observation_unions_concurrent_additions_and_keeps_revision_monotonic` proved that a newer, partial AppState/host snapshot cleared a live pending ID. `observe_workspace` now treats broker runtime state as authoritative: snapshots only union genuinely new, non-tombstoned IDs, never clear absent IDs, and attachment revisions remain monotonic by advancing when equal/stale snapshots contribute an ID.
- Review RED: `a_new_session_for_the_same_workspace_fences_the_old_session` proved that binding a new PTY session to the same workspace left the old generation able to commit. A workspace now has exactly one current binding; rebinding either that session ID or workspace removes and rolls back every replaced binding's active reservation before installing the new generation.
- The existing classifier regression remains GREEN: printable Unicode, ordinary space, CR/LF, and nonempty Paste are accepted; NBSP/line-separator whitespace is rejected for Text/Raw input.

### Exact verification commands

- `cargo test --locked --lib snapshot_observation_unions_concurrent_additions_and_keeps_revision_monotonic -- --nocapture` - RED before the fix (`["ann-2"]` instead of `["ann-1", "ann-2"]`); GREEN after.
- `cargo test --locked --lib a_new_session_for_the_same_workspace_fences_the_old_session -- --nocapture` - RED before the fix (old reservation committed); GREEN after.
- `cargo test --locked --lib prompt_boundary_classifier_accepts_only_user_prompt_content -- --nocapture` - GREEN.
- `cargo test --locked --lib browser::attachments::tests -- --nocapture` - PASS, 9/9.
- `cargo test --locked --lib composite_ -- --nocapture` - PASS, 3/3.
- `cargo test --locked --test browser_core -- --test-threads=1` - PASS, 17/17.
- `cargo test --locked --test browser_annotations -- --test-threads=1` - PASS, 5/5.
- `cargo fmt --all -- --check` - PASS after `cargo fmt --all` normalized new code.
- `git diff --check` - PASS.
- `cargo check --locked --lib` - PASS.

## Checkpoint 4A conservative URL preamble hardening

- Browser-annotation preambles now emit only a valid URL's scheme, host, and explicit port. They omit all userinfo, path content, query names and values (including bare capability queries), and fragments. Invalid/non-authority URLs collapse to `[redacted]`; complete context remains behind `browser_annotations` resources.
- RED coverage updated the preamble contract with a port-bearing URL containing credentials, a magic-link path token, a bare `?oauth-code-value` capability query, secret-bearing query names, signed values, and a fragment. The initial failure leaked `magic`; the green result preserves only `https://example.test:8443`.
- The compact redaction regression now places its comment secret inside the retained prefix. This preserves its intended coverage independently of the deliberately removed URL query redaction marker.

### Conservative URL verification

- `cargo test --locked --lib reserved_preamble_retains_only_safe_url_origin -- --nocapture` - RED before the origin-only summary; GREEN after.
- `cargo test --locked --lib browser::attachments::tests -- --nocapture` - PASS, 11/11.
- `cargo test --locked --lib composite_ -- --nocapture` - PASS, 3/3.
- `cargo test --locked --test browser_core -- --test-threads=1` - PASS, 17/17.
- `cargo test --locked --test browser_annotations -- --test-threads=1` - PASS, 5/5.
- `cargo fmt --all -- --check` - PASS.
- `git diff --check` - PASS.
- `cargo check --locked --lib` - PASS.

## Checkpoint 4A immutable-review boundary hardening

- URL preambles now retain only a compact origin/path plus query keys. Userinfo is removed, every query value is replaced with `[redacted]`, and fragments are omitted entirely; this covers ordinary OAuth values, arbitrary signed-query values, and fragment tokens without relying on key-name heuristics.
- The broker continues to bound tombstones at 512 entries, but no longer needs a second unbounded retired-ID set. Its retained saved-annotation map is the durable identity record: an ID already known before a snapshot arrived cannot be re-queued by that stale snapshot after a tombstone is evicted, while previously unseen IDs still merge normally.
- Prompt classification now rejects every representable non-printing Unicode General Category C value: Rust handles Control (`Cc`) directly; the existing cross-platform `regex` dependency identifies Format (`Cf`), Private Use (`Co`), and Unassigned (`Cn`). UTF-16 surrogate (`Cs`) code points cannot exist in a Rust `char`; combining marks, emoji, and ordinary Unicode text remain valid.
- No dependency or lockfile change was needed: this uses the existing direct `regex` dependency and standard-library `OnceLock`, so the classifier is platform-neutral.

### Immutable-review RED / GREEN evidence

- `cargo test --locked --lib reserved_preamble_never_leaks_url_userinfo_query_or_fragment_values -- --nocapture` - RED before URL-aware summaries (origin/path was not preserved and credentials/query/fragment values remained eligible for output); GREEN after.
- `cargo test --locked --lib stale_snapshots_cannot_resurrect_after_bounded_tombstone_eviction -- --nocapture` - RED before durable identity suppression; GREEN after 512 subsequent detach operations.
- `cargo test --locked --lib prompt_boundary_classifier_accepts_only_user_prompt_content -- --nocapture` - RED for U+200B/U+202E initially, then RED for representable Private Use U+E000 after the first Format-only pass; GREEN for Control/Format/Private Use/Unassigned rejection and combining-mark/emoji acceptance.
- `cargo test --locked --lib browser::attachments::tests -- --nocapture` - PASS, 11/11.
- `cargo test --locked --lib composite_ -- --nocapture` - PASS, 3/3.
- `cargo test --locked --test browser_core -- --test-threads=1` - PASS, 17/17.
- `cargo test --locked --test browser_annotations -- --test-threads=1` - PASS, 5/5.
- `cargo fmt --all -- --check` - PASS.
- `git diff --check` - PASS.
- `cargo check --locked --lib` - PASS.

## Checkpoint 4B1: ProcessManager lifecycle and local user-origin input

- `ProcessManagerInner` now owns one cloneable `BrowserAttachmentBroker`, independent of the browser gateway/provider-session map, and `ProcessManager` exposes its clone for the native shell.
- Every Claude/Codex launch constructs its `BrowserWorkspaceKey`, observes the saved workspace, overlays the broker projection, and binds the PTY generation before gateway lookup or Claude/Codex provider preparation. Missing gateways and provider/adapter failures leave the attachment binding usable.
- Spawn/restart operations carry the captured binding through the process queue. Queue failure, PTY spawn failure, terminal exit, explicit close/forget, startup-command failure, and Codex fallback failure clean up only with `unbind_if_matches` against that captured generation.
- Replacement sessions bind before old PTY cleanup. Stale exit callbacks cannot remove the replacement. Codex same-ID fallback renews the generation before terminating/reaping the old PTY, and the fallback PTY captures the renewed binding.
- Provider revocation/degradation remains separate and never unbinds the attachment broker.
- Added explicit local user-origin text/raw/paste methods. One coordinator classifies first, reserves exact pending IDs only at a 4A prompt boundary, writes one compound `TerminalSession` payload, commits only after write+flush success, and rolls back write/flush failures for retry. Non-triggering inputs write with an empty prefix and never consult the broker.
- Only native keyboard text, clipboard text Paste, and raw clipboard bytes use the new methods. Remote web input, startup commands, SSH/password/host confirmation, protocol replies, focus, mouse/alternate-scroll, and generic methods remain unchanged and non-consuming.

### Checkpoint 4B1 RED / GREEN evidence

- Initial RED compilation failed with 27 errors for the missing broker getter, binding return, captured queue binding, fallback renewal, and shared user-origin coordinator. The focused lifecycle and transaction tests are GREEN.
- PTY spawn cleanup was separately RED because a failed spawn retained its binding; GREEN after generation-conditional cleanup in the spawn error path.
- Actual Codex fallback was RED because it retained the old generation, and fallback-spawn failure was RED because it retained the binding. Both are GREEN after renewal before terminate/reap and conditional cleanup of the renewed binding.
- The restore/ensure source invariant was RED because it forgot the old session before preparing the replacement binding. It is GREEN after deferring old-session cleanup until the replacement is bound.
- Write/flush failure rolls back for retry; later Enter/raw input cannot re-consume a committed reservation; control input leaves pending annotations untouched; and two simultaneous session/workspace transactions remain isolated.

### Checkpoint 4B1 verification

- `cargo test --locked --lib browser::attachments::tests -- --test-threads=1 --nocapture` - PASS, 11/11.
- `cargo test --locked --lib terminal::session::tests -- --test-threads=1 --nocapture` - PASS, 12/12.
- `cargo test --locked --lib services::process_manager::tests -- --test-threads=1 --nocapture` - PASS, 68/68.
- `cargo test --locked --test browser_attachment_lifecycle -- --test-threads=1 --nocapture` - PASS, 2/2 source invariants.
- Affected browser suites - PASS: `browser_core` 17/17, `browser_annotations` 5/5, `browser_gateway` 14/14, `browser_provider` 5/5, and `browser_pane` 24/24.
- `cargo fmt --all -- --check` - PASS.
- `git diff --check` - PASS (Git emitted only the existing LF-to-CRLF working-copy notices).
- `cargo check --locked --lib` - PASS with no Rust warnings.

## Checkpoint 4B1 review race fixes

- Added `BrowserAttachmentBroker::renew_if_matches`, which compares the complete expected session/workspace/generation and installs the renewed generation under one broker lock. An old Codex fallback can no longer read a binding, lose the workspace to a replacement, and then evict that replacement while renewing.
- Codex fallback captures its optional expected binding before the fallback worker starts. A present binding must renew atomically or the stale fallback aborts before terminal teardown; an absent binding preserves the existing fail-open original-command fallback behavior.
- `schedule_close_ai` now captures the current attachment binding before queue submission and calls only generation-conditional unbind when submission fails. Successful submissions retain the binding for normal close/forget lifecycle cleanup.
- Native input source invariants now inspect the exact remote handler, exact local keyboard/clipboard handler, and exact generic ProcessManager method bodies. They prove remote text/raw/paste and web image paste stay generic/non-consuming, while only local keyboard text, clipboard Paste, and raw clipboard bytes use the user-origin APIs.

### Review RED / GREEN evidence

- `compare_and_renew_cannot_steal_an_interleaved_replacement_binding` was RED with two missing-method compile errors. It is GREEN for the deterministic captured-old/bind-replacement/attempt-renew interleaving, and also proves ordinary current same-session renewal advances the generation.
- `close_queue_failure_unbinds_only_the_captured_attachment_generation` was RED because the failed close submission left its binding current. It is GREEN after captured conditional cleanup.
- The first full ProcessManager gate exposed a direct Codex fallback fixture with no attachment binding: fallback timed out after the initial stale-race implementation made binding renewal mandatory. It is GREEN after optional expected-binding handling preserved fail-open recovery while still aborting stale present bindings.
- The tightened source invariant initially failed only because its test region ended at a marker preceding the handler. After anchoring it to the exact resize-handler boundary, both lifecycle/source invariants are GREEN.

### Review verification

- `cargo test --locked --lib browser::attachments::tests -- --test-threads=1 --nocapture` - PASS, 12/12.
- `cargo test --locked --lib terminal::session::tests -- --test-threads=1 --nocapture` - PASS, 12/12.
- `cargo test --locked --test browser_attachment_lifecycle -- --test-threads=1 --nocapture` - PASS, 2/2.
- `cargo test --locked --lib services::process_manager::tests -- --test-threads=1 --nocapture` - PASS, 69/69.
- Affected browser suites - PASS: `browser_core` 17/17, `browser_annotations` 5/5, `browser_gateway` 14/14, `browser_provider` 5/5, and `browser_pane` 24/24.
- `cargo fmt --all -- --check` - PASS.
- `git diff --check` - PASS (Git emitted only LF-to-CRLF working-copy notices).
- `cargo check --locked --lib` - PASS with no Rust warnings.

## Checkpoint 4B2: Authoritative projection, persistence, and pins

- Broker dirty observation is now acknowledge-after-success. `dirty_projections` is nondestructive; `acknowledge_dirty_projection` clears a workspace only when the captured projection generation is still current, so host/AppState/persistence failure remains retryable and a concurrent newer projection remains dirty.
- Dirty projections carry the exact unacknowledged delivery/detach tombstone delta. The host's narrow attachment acknowledgement removes only those exact IDs, unions current broker pending IDs without deleting concurrent host additions, advances only `BrowserAttachmentRevision`, and preserves annotations, page `BrowserRevision`, tabs, and selection.
- The Windows host reconciles annotation resource pins after the narrow mutation. A delivered resolved screenshot is released, while unresolved saved annotation context and a concurrently added pending annotation remain available and pinned.
- Locally hosted full-snapshot ingress is observe-then-overlay at restored AppState, synchronous browser responses, and host `SyncSnapshot` events; provider registration already uses the same broker order. Delivered/detached IDs therefore cannot reappear in AppState during the 33ms event-pump interval.
- The browser event pump reconciles dirty projections before its empty-event early return through one behavior-tested transaction, persists each changed AppState immediately, and acknowledges the broker only after host mutation, AppState replacement, and persistence succeed. Broker calls occur outside the host/bridge critical section.
- Remote-client snapshot merge remains remote-host authoritative and contains no local broker overlay path while connected, regardless of whether the local broker is empty or dirty. On disconnect, the captured local backup is observe-then-overlaid through the local broker after remote mode ends and before it replaces AppState.
- Reset Workspace and Clear Project Profile discard only their scoped broker workspace state and preserve live PTY bindings. Local AI tab close fully retires the exact workspace and binding without affecting another conversation.

### Checkpoint 4B2 RED / GREEN evidence

- Broker RED failed to compile on missing nondestructive dirty observation, generation-checked acknowledgement, exact tombstone delta, and state-only reset APIs. GREEN proves retryable observation, exact detach/delivery deltas, concurrent-newer retention, stale-snapshot suppression, and reset-versus-retire binding semantics.
- Host RED failed on the missing narrow attachment acknowledgement. Behavioral GREEN proves page revision, tabs, selection, saved annotations, and concurrent pending additions survive while exact delivered IDs are removed. Real resource-store behavior proves a resolved delivered screenshot becomes unpinned while a pending screenshot stays pinned; a supplemental Windows source invariant ties that helper to the production host path.
- Native-shell ingress RED failed because synchronous response, host-event, and restored-state paths did not project through the broker, and because the empty pump returned before dirty reconciliation. Behavioral GREEN exercises the production transaction with real AppState/broker state for success, host failure, persistence failure, and a concurrent newer generation. Supplemental source invariants prove the empty-pump call site, lock/persist seams, observe-before-overlay-before-replacement ingress, and absence of local broker calls inside connected remote-client snapshot merge.
- Local-close RED left the closed workspace binding alive. GREEN fully retires that exact workspace while preserving another conversation; reset/clear retain bindings through the state-only broker reset.

### Checkpoint 4B2 verification

- `cargo test --locked --lib browser::attachments::tests -- --test-threads=1 --nocapture` - PASS, 15/15.
- Focused restored AppState, local-close, and source-invariant regressions - PASS, 5/5.
- `cargo test --locked --lib state::app_state::tests -- --test-threads=1` - PASS, 3/3.
- `cargo test --locked --lib services::process_manager::tests -- --test-threads=1` - PASS, 70/70.
- Affected browser suites - PASS, 156/156: annotations 5, automation 12, core 17, gateway 14, host 81, pane 27.

## Checkpoint 4B2 independent-review recovery hardening

- Disconnecting from a remote host no longer restores `local_state_backup` verbatim. After remote mode is removed, the backup is observed and overlaid through the local attachment broker before AppState assignment, then persisted if projection changed. Connected remote snapshots remain exclusively remote-host authoritative.
- Extracted the exact pump reconciliation transaction behind an injected host-ack/AppState-persist sink. The native sink keeps host/bridge work and broker calls separated; tests use the same transaction with real AppState and broker state.
- Host acknowledgement and pin reconciliation now share one production helper used by the Windows host. It unions live draft resource IDs with snapshot-owned pins and reconciles the real resource store after the narrow state mutation.

### Review recovery RED / GREEN evidence

- Disconnect source RED found no local broker reconciliation between `remote_mode.take()` and `self.state = local_state`. GREEN adds that exact order, while a behavioral regression proves a delivered ID in the stale backup is suppressed and the dirty projection remains available for later host acknowledgement.
- Transaction RED failed to compile on the missing sink, result, and coordinator interfaces. GREEN proves an empty-pump projection applies and persists before acknowledgement; host or persistence failure leaves the projection retryable; and a concurrent newer generation is persisted and remains dirty instead of being cleared by the captured older acknowledgement.
- Resource-pin RED failed to compile because no production host/resource helper existed. GREEN uses a real `BrowserResourceStore`: resolved delivered screenshot A becomes unpinned and pending screenshot B remains pinned. The Windows source invariant is retained only to prove the behavior-tested helper is the live wrapper path.

### Review recovery verification

- Pump transaction behavior - PASS, 3/3; persistence observes the captured projection still dirty before acknowledgement.
- Remote-disconnect stale-backup behavior - PASS, 1/1; supplemental disconnect-order source invariant is included in the pane suite.
- `cargo test --locked --lib browser::attachments::tests -- --test-threads=1 --nocapture` - PASS, 15/15.
- `cargo test --locked --lib state::app_state::tests -- --test-threads=1` - PASS, 3/3.
- `cargo test --locked --lib services::process_manager::tests -- --test-threads=1` - PASS, 70/70.
- `cargo test --locked --test browser_attachment_lifecycle -- --test-threads=1` - PASS, 3/3.
- Affected browser suites - PASS, 158/158: annotations 5, automation 12, core 17, gateway 14, host 82, pane 28.

## Checkpoint 4C1: native pending-annotation chips

- Added terminal-native `PendingAnnotationChipModel`/action DTOs in authoritative pending order. Local conversations source the broker projection and never stale AppState pending IDs; remote-client conversations source the received host snapshot and never overlay or detach through the local broker.
- The terminal strip renders only for active Claude/Codex tabs, after terminal notices and before search/content. It is independent of browser `pane_open`; Server/SSH remain empty. Terminal viewport sizing now reserves the visible strip height.
- Display content contains only a bounded/redacted stable ID, 96-character comment, safe URL origin, and stale marker. It reuses the attachment preamble redaction/origin helpers and never carries locator/style/screenshot/query-secret content into rendered text.
- Local remove validates the active workspace and current broker pending set, calls broker detach, and immediately reuses the 4B2 host/AppState/persist/acknowledge transaction. Host or persistence failure stays dirty for retry. Saved annotation and screenshot references remain; no annotation Delete or new public `BrowserCommand` was added. Remote-client remove refuses local broker mutation with a concise host-directed diagnostic.
- Preview validates the active workspace and authoritative pending source, then uses existing `Ensure`, `SetPaneOpen`, `SelectTab`, `Navigate`, and `CreateTab` commands through `dispatch_browser_command` and its GPUI bridge/control barrier. Existing tabs select and navigate only when needed; missing tabs are created at the saved URL. Preview never reserves, commits, detaches, or acknowledges an attachment.
- The nested remove control stops propagation before invoking remove, so it cannot also trigger chip preview; preview clicks also stop before reaching terminal selection handling.

### Checkpoint 4C1 RED / GREEN evidence

- Chip DTO/render RED failed with `E0432` because `pending_annotation_chip_models` did not exist. GREEN proves authoritative ordering, Claude/Codex-only visibility, Server/SSH emptiness, bounded redaction, safe URL origins, stale markers, collapsed-pane independence, and viewport reservation.
- Remove RED failed with three `E0425` errors because `remove_pending_annotation_projection_transaction` did not exist. GREEN proves exact detach plus host/persist/acknowledge, saved screenshot context retention, cross-workspace/stale rejection before mutation, and dirty retry after persistence failure.
- Preview RED failed with `E0432` because `browser_annotation_preview_plan` did not exist. GREEN proves conditional existing-tab select/navigation, missing-tab creation at the saved URL, ownership/pending rejection, and no consumption.
- Callback/source RED failed because the terminal actions lacked preview/remove handlers and the native shell lacked a shared authoritative source. GREEN proves nested remove propagation, local broker versus remote snapshot sourcing, existing barrier dispatch, and absence of Delete/consume calls in preview/remove paths.
- Viewport RED failed because `PENDING_ANNOTATION_STRIP_HEIGHT_PX` and its layout reservation were absent. GREEN reserves terminal rows whenever authoritative pending chips are visible.

### Checkpoint 4C1 verification

- `cargo test --locked --test terminal_pending_annotations -- --test-threads=1` - PASS, 5/5.
- `cargo test --locked --lib pending_annotation -- --test-threads=1` - PASS, 4/4 (three checkpoint action tests plus the existing rollback regression).
- `cargo test --locked --test browser_pane -- --test-threads=1` - PASS, 31/31.
- `cargo test --locked --test browser_host -- --test-threads=1` - PASS, 82/82.
- `cargo test --locked --lib browser::attachments::tests -- --test-threads=1` - PASS, 15/15.
- `cargo test --locked --test browser_annotations -- --test-threads=1` - PASS, 5/5.
- `cargo test --locked --lib state::app_state::tests -- --test-threads=1` - PASS, 3/3.
- `cargo test --locked --test browser_attachment_lifecycle -- --test-threads=1` - PASS, 3/3.
- `cargo fmt --all -- --check` - PASS.
- `git diff --check` - PASS (only existing LF-to-CRLF working-copy notices).
- `cargo check --locked --lib` - PASS with no Rust warnings.
- Windows `cargo build --locked` - PASS with no Rust warnings.

## Checkpoint 4C1 independent-review hardening

- Preview no longer raw-compares a persisted redacted annotation URL with the live original URL. One `browser_annotation_urls_equivalent` helper now owns the deterministic-redaction comparison used by both `annotation_anchor_is_stale` and preview planning. A current live secret-query tab is selected without navigation, while genuinely different URLs still navigate and a missing tab is still created at the saved URL.
- Every local remove/preview failure after an active workspace is known now routes through one `show_pending_annotation_action_failure` helper. Fixed messages reach the terminal notice surface even when the browser pane is collapsed and remain mirrored in browser diagnostics; raw host/dispatch errors are not echoed into either user-visible surface.

### Review RED / GREEN evidence

- `cargo test --locked --test browser_pane annotation_preview_treats_a_persisted_redacted_url_as_the_current_live_url -- --test-threads=1 --nocapture` was RED because the plan contained `BrowserCommand::Navigate`; it is GREEN after sharing redaction-aware URL equivalence.
- `cargo test --locked --test terminal_pending_annotations pending_annotation_action_failures_reach_the_visible_terminal_notice_without_raw_details -- --test-threads=1 --nocapture` was RED because no terminal-visible failure helper existed; it is GREEN and source-checks all three local remove failures and all three preview failures.
- `cargo test --locked --lib pending_annotation_action_failure_notices_are_fixed_concise_and_non_sensitive -- --test-threads=1 --nocapture` was compile-RED on the missing fixed failure vocabulary; it is GREEN for five exact bounded messages that cannot carry raw error details.

### Review hardening verification

- `cargo test --locked --test terminal_pending_annotations -- --test-threads=1` - PASS, 6/6.
- `cargo test --locked --lib pending_annotation -- --test-threads=1` - PASS, 5/5.
- `cargo test --locked --test browser_pane -- --test-threads=1` - PASS, 32/32.
- `cargo test --locked --test browser_host -- --test-threads=1` - PASS, 82/82.
- `cargo test --locked --lib browser::attachments::tests -- --test-threads=1` - PASS, 15/15.
- `cargo test --locked --test browser_annotations -- --test-threads=1` - PASS, 5/5.
- `cargo test --locked --lib state::app_state::tests -- --test-threads=1` - PASS, 3/3.
- `cargo test --locked --test browser_attachment_lifecycle -- --test-threads=1` - PASS, 3/3.
- `cargo fmt --all -- --check` - PASS.
- `git diff --check` - PASS (only existing LF-to-CRLF working-copy notices).
- `cargo check --locked --lib` - PASS with no Rust warnings.
- Windows `cargo build --locked` - PASS with no Rust warnings.

## Checkpoint 4C1 terminal-model notice lifecycle hardening

- Re-review found that the first failure helper still wrote `terminal_notice`, which ordinary local AI passive/active model refresh cleared and remote model refresh recomputed before `TerminalPaneModel` assembly. The browser diagnostic survived, but the collapsed terminal chip surface did not actually render the feedback.
- Chip-action feedback now has dedicated fixed-vocabulary state keyed to the exact browser workspace and the local/remote mode that produced it. Both production terminal-model returns project it after their ordinary transient-notice lifecycle, so it survives repeated active-AI refresh without changing existing startup/transient fallback priority when no action feedback exists.
- A same-workspace successful preview/remove clears the feedback. Mode changes and an eight-second expiry clear it; an identity-checked timer requests the expiry render without allowing an older timer to clear newer feedback. Other workspaces never display or consume it. The same projection determines viewport notice reservation.
- Remote remove now uses the same safe lifecycle and fixed host-directed message. No raw error detail can enter the action notice or browser diagnostic.

### Terminal-model notice RED / GREEN evidence

- `cargo test --locked --lib chip_action_ -- --test-threads=1 --nocapture` was compile-RED with 13 expected `E0425`/`E0433` errors for the absent dedicated notice, expiry, model projection, and success-clear seams. A supplemental fixed-message RED added the missing remote-remove vocabulary.
- The same command is GREEN, 2/2. One test builds a real active Claude `TerminalPaneModel` from a `pane_open=false` snapshot, refreshes it twice after transient notice clearing, verifies the exact safe message, and constructs the terminal render tree. The second behavior-tests remote projection, workspace isolation, mode clearing, explicit success clearing, expiry, and restoration of the existing transient notice fallback.
- The terminal chip suite additionally proves both local and remote production `sync_terminal_session` returns use the behavior-tested projection and that viewport sizing reserves its visible banner.

### Terminal-model notice verification

- Behavioral model/lifecycle regressions - PASS, 2/2.
- `cargo test --locked --test terminal_pending_annotations -- --test-threads=1` - PASS, 6/6.
- `cargo test --locked --lib pending_annotation -- --test-threads=1` - PASS, 5/5.
- `cargo test --locked --test browser_pane -- --test-threads=1` - PASS, 32/32.
- `cargo test --locked --test browser_host -- --test-threads=1` - PASS, 82/82.
- `cargo test --locked --lib browser::attachments::tests -- --test-threads=1` - PASS, 15/15.
- `cargo test --locked --test browser_annotations -- --test-threads=1` - PASS, 5/5.
- `cargo test --locked --lib state::app_state::tests -- --test-threads=1` - PASS, 3/3.
- `cargo test --locked --test browser_attachment_lifecycle -- --test-threads=1` - PASS, 3/3.
- `cargo fmt --all -- --check` - PASS.
- `git diff --check` - PASS (only existing LF-to-CRLF working-copy notices).
- `cargo check --locked --lib` - PASS with no Rust warnings.
- Windows `cargo build --locked` - PASS with no Rust warnings.
