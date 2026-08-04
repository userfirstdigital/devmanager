# Phase 7: Prompt Library and Guided Chains Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a local-first personal prompt library with immutable versions and readable diffs, searchable recent submitted prompts, and simple ordered prompt chains that place one chosen prompt into the Task composer without automatically executing or advancing anything.

**Architecture:** The Rust host and its SQLite database are authoritative for personal prompts, versions, history, and ordered chains. Saved prompts and retention-governed recent submitted prompts are durable local records; only the FTS5 search index is rebuildable and updated by a bounded background worker. A delivered-input settlement and its history row commit atomically, so a crash cannot acknowledge delivery and lose the recent prompt. GPUI renders the library, version diff, and linear chain editor. Provider-native slash commands remain live provider capabilities and never merge into the saved library. Phase 9 carries the same host projection to paired remote clients; Phase 10 adds separately published organization prompts.

**Tech Stack:** Rust 1.94.0, rusqlite 0.40.1 bundled SQLite/FTS5, `similar = "=3.1.2"` with Apache-2.0 license, GPUI/gpui-component, Phase 1 command/event/store/protocol contracts, Phase 5 design system.

## Global Constraints

- Personal saved prompts, versions, chains, and recent history are host-authoritative local data. Signing into Connect does not upload or create a cleartext cloud copy.
- Paired clients later access this host library through the existing E2E channel. Cross-host personal-library replication, merge/conflict logic, and prompt-specific crypto are outside this program.
- Saved prompts, recent submitted-prompt history, and provider-native slash commands are three separate models and UI sections.
- A saved prompt version is immutable. Editing creates the next monotonically increasing version and atomically advances the prompt's current-version pointer.
- A chain is one ordered list of links to exact immutable prompt versions. Insert, reorder, remove, and explicit update-to-current are allowed.
- Chain use is manual: **Put in composer** copies text into the client-local draft. It does not send, advance, branch, evaluate a condition, mark completion, launch a provider, or mutate the chain.
- Recent history records only prompts whose provider-input operation settled with the adapter outcome `Delivered`. Admission-only, failed, cancelled, uncertain, synthetic, and provider-internal inputs are excluded.
- Default recent-history policy is enabled, 90-day retention, maximum 10,000 entries per host. The user can disable history or clear it without deleting Task conversation facts.
- FTS indexing and retention execute outside input, PTY, and render paths. The queue carries index work only: capacity is 1,024 and batches flush at 50 records or 250 ms. Overflow never loses canonical history; it marks the index dirty and schedules a rebuild rather than blocking prompt submission.
- Prompt content is untrusted text. Bound lengths, render without HTML execution, preserve literal content on composer insertion, and never treat prompt text as an ActionId/provider command until the user explicitly sends it.

---

## File map

- Create: `src/prompts/mod.rs`
- Create: `src/prompts/model.rs`
- Create: `src/prompts/store.rs`
- Create: `src/prompts/diff.rs`
- Create: `src/prompts/search.rs`
- Create: `src/prompts/service.rs`
- Create: `src/prompts/projection.rs`
- Modify: `src/domain/id.rs`
- Modify: `src/domain/{command,event,snapshot}.rs`
- Modify: `src/kernel/{schema,store,command_bus,outbox}.rs`
- Modify: `src/host/mod.rs`
- Modify: `src/protocol/{capabilities,envelope}.rs`
- Modify: `src/client/{action,model}.rs`
- Create: `src/ui/prompts/mod.rs`
- Create: `src/ui/prompts/{library,editor,version_diff,chain_editor,history,picker}.rs`
- Modify: `src/ui/shell.rs`
- Modify: `src/ui/task_cockpit/composer.rs`
- Modify: `Cargo.toml`, `Cargo.lock`, `THIRD_PARTY_NOTICES.md`
- Create: `tests/prompt_model.rs`
- Create: `tests/prompt_store.rs`
- Create: `tests/prompt_diff.rs`
- Create: `tests/prompt_search.rs`
- Create: `tests/prompt_chains.rs`
- Create: `tests/prompt_ui.rs`
- Create: `tests/fixtures/prompts/v1/*`
- Create: `tests/fixtures/conformance/prompts/v1/*`
- Create: `scripts/native-next/Invoke-PromptLibrarySmoke.ps1`
- Create: `docs/prompts.md`

### Task 7.1: Define prompt identities, facts, commands, and schema

**Files:** `src/prompts/{mod,model,store}.rs`, `src/domain/{id,command,event,snapshot}.rs`, `src/kernel/schema.rs`, `src/lib.rs`, `tests/{prompt_model,prompt_store}.rs`

**Interfaces:**

```rust
pub struct PromptId(Uuid);
pub struct PromptVersionId(Uuid);
pub struct PromptChainId(Uuid);
pub struct PromptChainLinkId(Uuid);
pub struct PromptHistoryId(Uuid);

pub struct SavedPrompt {
    pub id: PromptId,
    pub title: String,
    pub description: Option<String>,
    pub tags: Vec<String>,
    pub current_version_id: PromptVersionId,
    pub revision: u64,
    pub archived_at_ms: Option<i64>,
}

pub struct PromptVersion {
    pub id: PromptVersionId,
    pub prompt_id: PromptId,
    pub version: u32,
    pub body: String,
    pub body_sha256: [u8; 32],
    pub created_at_ms: i64,
}
```

- [ ] **Step 1: Write failing tests** `prompt_ids_are_not_interchangeable`, `create_prompt_creates_version_one`, `editing_creates_immutable_next_version`, `current_version_must_belong_to_prompt`, `duplicate_command_is_idempotent`, `expected_revision_prevents_lost_update`, `archived_prompt_versions_remain_readable`, `body_hash_round_trips`, and `phase07_schema_rebuilds_projection`.
- [ ] **Step 2: Run** `cargo test --test prompt_model --test prompt_store -- --nocapture` and retain the missing prompt module/schema failure.
- [ ] **Step 3: Add typed IDs and commands** `CreatePrompt`, `CreatePromptVersion`, `RenamePrompt`, `SetPromptTags`, `ArchivePrompt`, and `RestorePrompt` with expected prompt revision. Limits: title 160 Unicode scalar values, description 2,000, body 256 KiB UTF-8, 32 normalized tags, each tag 48 characters.
- [ ] **Step 4: Add the named migration `phase07-prompts-v1`** to the ordered migration registry:

```sql
CREATE TABLE saved_prompts (
  prompt_id BLOB PRIMARY KEY CHECK(length(prompt_id) = 16),
  title TEXT NOT NULL,
  description TEXT,
  current_version_id BLOB NOT NULL CHECK(length(current_version_id) = 16),
  revision INTEGER NOT NULL,
  created_at_ms INTEGER NOT NULL,
  updated_at_ms INTEGER NOT NULL,
  archived_at_ms INTEGER,
  FOREIGN KEY(prompt_id, current_version_id)
    REFERENCES prompt_versions(prompt_id, prompt_version_id)
    DEFERRABLE INITIALLY DEFERRED
);
CREATE TABLE prompt_versions (
  prompt_version_id BLOB PRIMARY KEY CHECK(length(prompt_version_id) = 16),
  prompt_id BLOB NOT NULL REFERENCES saved_prompts(prompt_id),
  version INTEGER NOT NULL CHECK(version > 0),
  body TEXT NOT NULL,
  body_sha256 BLOB NOT NULL CHECK(length(body_sha256) = 32),
  created_at_ms INTEGER NOT NULL,
  UNIQUE(prompt_id, version),
  UNIQUE(prompt_id, prompt_version_id)
);
CREATE TABLE prompt_tags (
  prompt_id BLOB NOT NULL REFERENCES saved_prompts(prompt_id),
  tag TEXT NOT NULL,
  position INTEGER NOT NULL,
  PRIMARY KEY(prompt_id, tag),
  UNIQUE(prompt_id, position)
);
```

Create the prompt and version in one transaction using the deferred foreign key. Updates insert a new version and advance `current_version_id` atomically. No command updates or deletes a version body.
- [ ] **Step 5: Add prompt projections and paged queries** ordered by archived/title/PromptId, plus version pages newest-first. Include only metadata/body pages requested by the client; do not put every prompt body/version in the global kernel snapshot.
- [ ] **Step 6: Run** `cargo test --test prompt_model --test prompt_store -- --nocapture`, rebuild projections from events, and commit as `feat(prompts): add immutable personal prompt versions`.

### Task 7.2: Add readable version comparison

**Files:** `Cargo.toml`, `Cargo.lock`, `src/prompts/diff.rs`, `tests/prompt_diff.rs`, `THIRD_PARTY_NOTICES.md`

- [ ] **Step 1: Verify and record** `similar` 3.1.2 source, Apache-2.0 license, Rust 1.85 minimum, and enabled features (`default-features = false`, `features = ["text", "unicode", "inline"]`) in `THIRD_PARTY_NOTICES.md`.
- [ ] **Step 2: Write failing tests** for identical, added/removed/replaced lines, inline Unicode changes, CRLF/LF normalization without body mutation, very long lines, empty versions, deterministic output, and 256 KiB body bound.
- [ ] **Step 3: Run** `cargo test --test prompt_diff -- --nocapture` and retain the missing dependency/module failure.
- [ ] **Step 4: Add** `similar = { version = "=3.1.2", default-features = false, features = ["text", "unicode", "inline"] }` and implement `diff_versions(old, new) -> PromptDiff` with line hunks plus bounded inline spans. Diffing uses a background worker, caps output at 20,000 spans/2 MiB, and returns an explicit truncation marker.
- [ ] **Step 5: Preserve original bodies byte-for-byte** except schema-valid UTF-8; newline normalization is comparison-only. Cache diff results by the two immutable body hashes in a bounded host LRU, not SQLite truth.
- [ ] **Step 6: Run** `cargo test --test prompt_diff -- --nocapture`; commit as `feat(prompts): add version diffs`.

### Task 7.3: Add recent submitted-prompt history and FTS5 search

**Files:** `src/prompts/{model,search,store}.rs`, `src/kernel/{schema,outbox}.rs`, `src/providers/input.rs`, `tests/{prompt_search,prompt_store}.rs`

**Policy:** `PromptHistoryPolicy { enabled: true, retention_days: 90, max_entries: 10_000 }`; valid ranges are 1–365 days and 100–100,000 entries.

- [ ] **Step 1: Write failing tests** `delivered_user_prompt_enters_history`, `delivery_and_history_commit_atomically`, `host_crash_after_delivery_preserves_history`, `failed_cancelled_synthetic_and_provider_internal_do_not`, `duplicate_delivery_indexes_once`, `saved_and_history_search_are_distinct`, `unicode_prefix_phrase_and_tag_search`, `disabled_history_writes_nothing`, `retention_removes_history_not_task_fact`, `queue_overflow_marks_dirty_without_blocking`, and `fts_rebuild_matches_canonical_rows`.
- [ ] **Step 2: Run** `cargo test --test prompt_search -- --nocapture` and save the missing tables/indexer failure.
- [ ] **Step 3: Extend `phase07-prompts-v1`** before it ships:

```sql
CREATE TABLE prompt_history (
  history_rowid INTEGER PRIMARY KEY AUTOINCREMENT,
  prompt_history_id BLOB NOT NULL UNIQUE CHECK(length(prompt_history_id) = 16),
  submitted_event_id BLOB NOT NULL UNIQUE CHECK(length(submitted_event_id) = 16),
  task_id BLOB NOT NULL CHECK(length(task_id) = 16),
  agent_session_id BLOB NOT NULL CHECK(length(agent_session_id) = 16),
  provider_kind TEXT NOT NULL,
  body TEXT NOT NULL,
  body_sha256 BLOB NOT NULL CHECK(length(body_sha256) = 32),
  submitted_at_ms INTEGER NOT NULL
);
CREATE VIRTUAL TABLE prompt_search USING fts5(
  source_kind UNINDEXED,
  source_id UNINDEXED,
  title,
  body,
  tags,
  tokenize = 'unicode61 remove_diacritics 2'
);
```

- [ ] **Step 4: Insert history only from a durable delivered provider-input settlement.** When history is enabled, insert the canonical `prompt_history` row in the same settlement transaction, keyed by the unique submitted event; a duplicate settlement cannot add another row. After commit, enqueue only its FTS work. The bounded background worker drains at 50 records or 250 ms and applies index updates outside the input path. On queue overflow/index error, persist `prompt_search_dirty=1` and schedule a low-priority rebuild from canonical saved-prompt/history rows.
- [ ] **Step 5: Implement search** with parsed terms/quoted phrases/tag filter/source filter, maximum query length 512, 100 results/page, escaped FTS syntax, deterministic rank/recency/ID ordering, and highlighted ranges derived from trusted offsets rather than HTML snippets.
- [ ] **Step 6: Implement clear/disable/retention commands** with preview counts and explicit confirmation. Clearing history removes history/FTS rows only; it does not rewrite task journal events or saved prompts.
- [ ] **Step 7: Run** search/store tests with a fake clock and saturated test queue; commit as `feat(prompts): add local prompt history search`.

### Task 7.4: Implement simple ordered prompt chains

**Files:** `src/prompts/{model,store,service}.rs`, `src/domain/{command,event}.rs`, `tests/prompt_chains.rs`

**Interfaces:**

```rust
pub struct PromptChainLink {
    pub id: PromptChainLinkId,
    pub chain_id: PromptChainId,
    pub position: u32,
    pub prompt_id: PromptId,
    pub prompt_version_id: PromptVersionId,
}
```

- [ ] **Step 1: Write failing tests** `chain_accepts_any_positive_link_count`, `append_pins_exact_current_version`, `insert_between_shifts_positions_atomically`, `reorder_is_dense_and_stable`, `remove_compacts_positions`, `archived_prompt_link_remains_readable`, `new_prompt_version_does_not_mutate_link`, `explicit_update_link_uses_current_version`, `revision_conflict_changes_nothing`, and `chain_has_no_execute_or_advance_command`.
- [ ] **Step 2: Run** `cargo test --test prompt_chains -- --nocapture` and retain the missing chain failure.
- [ ] **Step 3: Extend the named prompt migration** with `prompt_chains(chain_id, title, description, revision, created_at_ms, updated_at_ms, archived_at_ms)` and `prompt_chain_links(link_id, chain_id, position, prompt_id, prompt_version_id)` plus foreign keys and unique `(chain_id, position)`. Add a composite reference that proves each `prompt_version_id` belongs to the stored `prompt_id`; a mismatched pair cannot enter the chain.
- [ ] **Step 4: Add commands** `CreatePromptChain`, `RenamePromptChain`, `InsertPromptChainLink { before_link_id: Option<_> }`, `MovePromptChainLink { before_link_id: Option<_> }`, `RemovePromptChainLink`, `UpdatePromptChainLinkVersion`, `ArchivePromptChain`, and `RestorePromptChain`. Insert/move renumbers the affected ordered set inside one immediate transaction and emits one revisioned event batch.
- [ ] **Step 5: Expose queries** that return the selected link with explicit previous/next IDs and show `Update available` when the prompt's current version differs. Do not define runtime cursor, completion state, conditional edge, branch, loop, scheduler, or automatic transition.
- [ ] **Step 6: Run** chain tests including 2,000 links and concurrent insert conflicts; commit as `feat(prompts): add manual guided prompt chains`.

### Task 7.5: Build the native Prompt Library experience

**Files:** `src/ui/prompts/{mod,library,editor,version_diff,chain_editor,history,picker}.rs`, `src/ui/shell.rs`, `src/client/model.rs`, `tests/prompt_ui.rs`, `tests/fixtures/prompts/v1/*`

- [ ] **Step 1: Write failing projection/action tests** for Saved/Recent/Chains sections, search/filter, create/edit/archive/restore, version list/diff, empty/loading/error/stale revision, chain overview with previous/next, insert-between affordance, reorder/remove, update-available badge, keyboard navigation, screen-reader labels, and 5,000 prompts/2,000 links.
- [ ] **Step 2: Run** `cargo test --test prompt_ui -- --nocapture` and retain the missing UI modules.
- [ ] **Step 3: Add the Prompt Library to the navigation rail** and build a virtualized list/detail layout using Phase 5 tokens/components. Keep Saved Prompts, Recent History, and Chains visually and semantically distinct; provider commands do not appear in this screen.
- [ ] **Step 4: Build the editor/version view** with title/description/tags/body, save-as-new-version behavior, unsaved-change confirmation, selectable native text diff, version metadata/hash, restore-by-creating-new-version, and bounded previews.
- [ ] **Step 5: Build the linear chain editor** as one readable vertical sequence with numbered links and visible connectors. Each gap has **Insert prompt here**; each link has previous/next context, pinned version, update action, reorder, remove, and **Put in composer**. Do not add a graph canvas, conditions, run button, progress ceremony, or auto-advance.
- [ ] **Step 6: Build Recent History** with local policy/clear controls, Task/provider/time source, search, **Save as prompt**, and **Put in composer**. Do not imply history deletion removes the original Task conversation.
- [ ] **Step 7: Capture light/dark/compact/comfortable/100–200%/narrow/wide/empty/error/large-data fixtures** and commit as `feat(ui): add native prompt library and chains`.

### Task 7.6: Integrate prompt selection with the composer and provider commands

**Files:** `src/ui/prompts/picker.rs`, `src/ui/task_cockpit/composer.rs`, `src/client/{action,model}.rs`, `src/providers/input.rs`, `tests/{prompt_ui,provider_input}.rs`

- [ ] **Step 1: Write failing tests** for opening the prompt picker, Saved/Recent/Chain search, selecting exact version, literal composer insertion, replacing versus inserting at cursor, edit-before-send, draft preservation, slash-command suggestions remaining provider-native, no send on selection, no chain advance on send/settlement, and stale prompt version remaining explicit.
- [ ] **Step 2: Run** `cargo test --test prompt_ui composer_ -- --nocapture` and retain the missing picker/action failure.
- [ ] **Step 3: Add pure client action `PutPromptVersionInComposer`** carrying TaskId/AgentSessionId/PromptVersionId and insertion mode. Resolve version text through a host query, then update only the client-local draft after the exact response; it emits no provider/kernel mutation beyond the read query.
- [ ] **Step 4: Keep provider-native command discovery** under its own `ProviderCommandSuggestion` model sourced from the active adapter/runtime. A slash prefix may search provider commands, while a separate Prompt Library button/shortcut opens saved/history/chain content; saving a provider command requires explicit **Save as prompt**.
- [ ] **Step 5: Preserve provenance in client-local draft metadata** (`PromptVersionId`/chain link) only for display. Editing makes the draft ordinary text; sending uses the normal provider command and never mutates/advances the chain.
- [ ] **Step 6: Run** prompt UI/provider input tests and commit as `feat(prompts): put exact prompt versions in composer`.

### Task 7.7: Define the transport-neutral prompt projection

**Files:** `src/prompts/projection.rs`, `src/protocol/{capabilities,envelope}.rs`, `tests/{prompt_store,prompt_ui}.rs`, `tests/fixtures/prompts/v1/*`

- [ ] **Step 1: Write failing golden tests** for prompt metadata page, exact version page, diff request/result, search page, chain page, history page, mutation receipt/settlement, permission denial, stale cursor, and oversized-body chunking.
- [ ] **Step 2: Run** `cargo test --test prompt_store projection_ -- --nocapture` and retain the red protocol fixture result.
- [ ] **Step 3: Add capability `personal_prompt_library`** and typed host queries/actions. Page metadata at 100 items/512 KiB, transfer bodies/diffs through the Phase 1 256 KiB chunks/16 MiB ceiling, and keep raw prompt bodies out of the global Task snapshot.
- [ ] **Step 4: Define visibility filtering:** local owner/paired-owner device may read/mutate personal prompts; Task Watcher/Collaborator grants do not imply personal-library access. Phase 9 carries these frames only inside the existing owner-device E2E session; no relay/Connect persistence DTO is introduced here.
- [ ] **Step 5: Add Rust golden fixtures** consumed later by TypeScript generated/fixture-verified codecs. Unknown prompt extension fields render generic metadata and never add an executable action.
- [ ] **Step 6: Run** protocol/projection tests and commit as `feat(prompts): expose bounded host prompt projection`.

### Task 7.8: Prove the local prompt lifecycle and performance

**Files:** `scripts/native-next/Invoke-PromptLibrarySmoke.ps1`, all Phase 7 tests, `tests/fixtures/conformance/prompts/v1/*`, `docs/prompts.md`, `docs/replacement-deletion-ledger.md`

- [ ] **Step 1: Create a deterministic smoke fixture** with Unicode, Markdown/code, long text, three versions, tags, 500 recent prompts, two chains, an archived prompt, and one provider slash-command fixture.
- [ ] **Step 2: Through real host/GPUI actions**, create/edit/diff/search a saved prompt, build a five-link chain, insert between links two/three, update one link's pinned version, place a link in the composer, edit it, send through a fake provider, and verify no chain state advanced.
- [ ] **Step 3: Saturate the history-index queue** while measuring composer input/provider-send acknowledgement. Require no input-path wait on FTS, a dirty marker/rebuild after overflow, deterministic results, and adherence to `docs/performance-budgets.md`.
- [ ] **Step 4: Restart the isolated host** and verify immutable versions/chains/history persist, FTS rebuild matches canonical rows, personal projection works, provider commands remain separate, and no `session.json` access occurred.
- [ ] **Step 5: Run accessibility/visual checks** for keyboard-only chain editing, diff navigation, screen-reader names/status, contrast, scaling, narrow/wide layouts, and large virtualized lists.
- [ ] **Step 6: Run shared conformance baseline/variant cases** for immutable versioning/diff, delivery-to-history crash recovery, FTS overflow/rebuild, bounded projection/chunks, chain insert-between, exact-version composer placement, and the no-auto-send/no-auto-advance invariant. Record only fixture hashes, declared latency/result/size metrics, and residue—not prompt bodies.
- [ ] **Step 7: Update documentation/deletion ledger** with local authority, retention/clear semantics, manual-chain rule, Connect/org boundaries, and any replaced ad hoc prompt-history path.
- [ ] **Step 8: Run** the smoke and all focused tests; commit as `test(prompts): prove local versioned prompt workflows`.

## Phase 7 verification gate

- [ ] Capture production baseline and start only the isolated host/GPUI preview under `native-next-dev`.
- [ ] Run `cargo test --test prompt_model --test prompt_store --test prompt_diff --test prompt_search --test prompt_chains --test prompt_ui -- --nocapture`.
- [ ] Run adjacent `cargo test --test provider_input --test protocol_contract -- --nocapture`.
- [ ] Run `pwsh scripts/native-next/Invoke-PromptLibrarySmoke.ps1 -LargeData -QueuePressure -Restart`.
- [ ] Run `cargo fmt --all -- --check` and `cargo clippy --all-targets -- -D warnings`.
- [ ] Inspect migration/replay/FTS rebuild, source-license notice, body/chunk limits, and every chain mutation for atomic revision behavior.
- [ ] Visually inspect library/editor/diff/history/chain/picker fixtures in all required themes/scales and complete a keyboard-only walkthrough.
- [ ] Prove prompt selection/send never auto-advances/executes a chain and provider-native commands remain a separate capability list.
- [ ] Rebuild the conformance query index and compare prompt-library baseline/variant arms, including interrupted-run resume.
- [ ] Confirm no FTS/retention/query work occurs on input/PTY/render paths and no Cargo/rustc/test/client/host/provider helper remains.
- [ ] Compare production hashes/PID/start time and review the complete Phase 7 diff/deletion ledger.

## Phase 7 exit criteria

- Personal saved prompts have stable IDs, immutable versions, readable bounded diffs, tags, archive/restore, and host-authoritative persistence.
- Recent delivered user prompts are locally searchable under explicit retention, with deferred bounded FTS maintenance and rebuildable index.
- Chains are simple ordered exact-version links with visible previous/next and insert-between; no automatic execution, branching, completion, or advancement exists.
- **Put in composer** produces editable local draft text and never sends or changes chain state.
- Saved/history/chain content remains separate from live provider-native commands.
- The bounded transport-neutral projection is ready for paired-owner E2E access in Phase 9 without cloud persistence or prompt-specific crypto.
- Production DevManager, configuration, sessions, provider authentication, and processes remain untouched.
