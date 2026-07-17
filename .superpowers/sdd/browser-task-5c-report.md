# Task 5C Report: Checkpoint 1 strict recipe wire/store

## Status

Checkpoint 1 is complete on the approved base `e088ccab1ce10afa73ae58c0ecf15077616d9a82`. This report is part of the focused checkpoint commit. The immutable final head, patch ID, and package range are recorded by the checkpoint handoff after the commit exists.

Checkpoints 2 through 12 are not implemented. There is no recording, review UI, replay, secret prompt, locator repair, or Task 5C MCP surface in this checkpoint.

## Contract decision

The unreleased flat step wire (`action` string plus `locator`, `valueRef`, `waitCondition`, and string assertions) is not accepted as a second v1 format. The repository has one strict v1 JSON contract. Source-level conversion between shared browser viewport/locator models remains available through `From`, but deserialization does not guess, alias, or partially interpret an old or future shape.

## Implemented

- Added strict recipe-specific viewport, locator, value, action, wait, assertion, and element-state types. Every object-shaped wire node denies unknown fields.
- Made top-level deserialization inspect `schemaVersion` before v1 shape parsing. Only exact version 1 is accepted; `load_recipe` returns `UnsupportedRecipeVersion` for a future version even when the future body is not v1.
- Added validation for safe recipe/step slugs, unique step IDs, trimmed unique input names, nonempty steps, viewport bounds, semantic locator fallbacks, required values, wait/timeout bounds, and typed assertions.
- Added input-reference type checking: URL uses require URL inputs, ordinary typed values use Text, upload requires File, password-like targets require Secret, and Secret values cannot enter assertions or waits.
- Reject Secret and File defaults at both serialization and nested-input deserialization boundaries. Credential-like metadata, URL credentials/query keys, sensitive literal assignments, password-target literals, file-upload literals, and secret/file-content aliases cannot enter emitted v1 JSON.
- Added deterministic pretty JSON with a trailing newline and an exact SHA-256 byte fixture.
- Added `list_recipes`, restricted to direct safe-slug `.devmanager/browser-workflows/<slug>.json` files in deterministic ID order. Load/save/list reject non-directory components, symlink classifications, non-regular recipe files, ID/file mismatches, and traversal slugs.
- Replaced direct writes with a same-directory, random `create_new` sibling temp, full write plus `sync_all`, and one atomic replace. Windows uses `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH`; in-process saves are serialized to avoid Windows replace races.
- Added RAII temp cleanup, injected replace-failure coverage, a real Windows locked-destination failure test, concurrent-save coverage, and checks that no operation leaves an orphan temp.

## Independent review hardening

- Replaced `serde_json::Value`'s last-member-wins object parsing with a recursive strict parser. Duplicate members now fail at every object depth, including `schemaVersion`, action tags, and nested input/value members; future versions are still reported before v1 body parsing.
- Made every public object-shaped nested wire type validate on direct deserialization. Context-free invariants now hold even when callers deserialize Action, Value, Wait, Viewport, Locator, Assertion, Step, or Input without going through the top-level recipe.
- Added Windows `FILE_ATTRIBUTE_REPARSE_POINT` classification in addition to symlink classification. The workflow directory and relevant recipe destination are revalidated immediately before list/read, sibling-temp open, and atomic replace boundaries.
- Added injected boundary tests for reparse swaps before read, temporary open, and replacement. A rejected replacement preserves the old complete document and removes the sibling temp without calling the replacer.
- Gave recipe temps an exact store-owned prefix and nonce shape. Save scavenges only direct regular files matching that shape, only after a 24-hour stale threshold, with a 1,024-entry scan bound and 64-delete bound; fresh files, lookalikes, malformed names, and matching directories survive.

## RED to GREEN evidence

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

## Verification

- `cargo test --locked --test browser_recipes -- --test-threads=1` -> 15 passed, 0 failed.
- `cargo test --locked --test browser_core -- --test-threads=1` -> 17 passed, 0 failed.
- `cargo test --locked --lib browser::recipes::tests -- --test-threads=1` -> 5 passed, 0 failed.
- `cargo test --locked browser -- --test-threads=1` -> 107 matching tests passed across all targets, 0 failed.
- Full browser target command covering annotations, attachment lifecycle, automation/resources, core/model/errors, fixture, gateway, host, pane, provider, and recipes -> 186 passed, 0 failed.
- `cargo check --locked --all-targets` -> exit 0.
- Native Windows `cargo build --locked` -> exit 0.
- `cargo fmt --all -- --check` -> exit 0.
- `git diff --check` -> exit 0.

## Files

- `Cargo.toml`
- `src/browser/mod.rs`
- `src/browser/recipes.rs`
- `tests/browser_core.rs`
- `tests/browser_recipes.rs`
- `.superpowers/sdd/browser-task-5c-checkpoints.md`
- `.superpowers/sdd/progress.md`
- `.superpowers/sdd/browser-task-5c-report.md`
