# Task 5C Report: Sequential checkpoints

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
