# Persist the Provider-Native Turn Identifier

## Outcome

The semantic journal persists the turn identifier the provider already gives us, so the native conversation transcript can group a turn and offer a turn fold. Today that identifier is read from Claude and Codex hooks, used once as an internal deduplication key, and discarded before anything durable sees it.

## Why this is its own spec

`docs/superpowers/plans/2026-08-20-conversation-derivation-and-target-ux.md` carries a Global Constraint that durable journal storage and `ALLOWED_JOURNAL_KINDS` are unchanged. This work changes the journal schema, so it cannot be a task inside that plan. It is a prerequisite for one Target UX affordance and nothing else; the conversation derivation branch ships without turn folds appearing until this lands.

## Evidence

Established by direct investigation, with citations, before any code was written:

- **No per-turn identifier is persisted anywhere.** Not in the `semantic_journal_facts` schema, not in `SemanticJournalFactRow`, not in `SemanticJournalFact`, not in any `SemanticJournalPayload` variant, and not in the `deny_unknown_fields` wire deserializer for `NativeJournalPayload` at `src/providers/journal.rs:821-865`.
- **A real provider-native turn id does exist upstream.** It is read from Claude hooks at `src/ai/claude_hooks.rs:585-589` and from Codex hooks at `src/ai/codex_hooks.rs:208`, where it serves as a message-batch deduplication key and is then dropped.
- **The projection has nothing in scope to thread.** `src/ui/renderers/journal_view.rs:365` therefore sets `turn_id: None` unconditionally on every `TimelineItemModel`.
- **The consumer is already built and tested.** `derive_conversation_rows` in `src/ui/conversation/rows.rs` emits a `ConversationRow::TurnFold` when an item's `turn_id` differs from the previous non-`None` one, suppresses a fold on the first turn, and is covered by tests including a sabotage check proving the first-turn guard gates. It is correct and inert.

## Design

### 1. Journal schema

Add a nullable `turn_id` column to `semantic_journal_facts`, with a migration. Nullable is deliberate and load-bearing: existing rows have no turn id and must remain distinguishable from rows whose provider genuinely did not supply one. A `NOT NULL DEFAULT ''` column would make "never recorded" and "provider gave none" the same fact forever.

### 2. Wire and payload

Extend `SemanticJournalFactRow`, `SemanticJournalFact`, and the `NativeJournalPayload` deserializer to carry `turn_id: Option<String>`. The deserializer uses `deny_unknown_fields`, so this is an additive field that older producers simply omit — verify a payload without `turn_id` still deserializes, because a strict schema that rejects the previous shape turns a routine deploy into an outage.

### 3. Capture

Thread the provider-native id from `claude_hooks.rs:585-589` and `codex_hooks.rs:208` into the journal write path, alongside its existing deduplication use rather than replacing it.

**Never synthesize a turn identifier.** Do not derive one from sequence numbers, timestamps, event ordering, message ids, or "the previous assistant message settled, so a new turn began." The project's invariants forbid inferring conversation identity from cwd, timestamps, or transcript ordering, and a fabricated id in a durable column is expensive to unpick once other code trusts it. A provider that supplies no turn id stores `NULL`, and the transcript simply shows no fold for it.

### 4. Projection

`journal_view.rs:365` passes the stored value through instead of `None`.

## Acceptance criteria

1. A journal fact written from a Claude or Codex hook that carries a turn id round-trips it: written, read back, and present on the projected `TimelineItemModel`.
2. A fact from a provider that supplies no turn id stores `NULL` and projects `None`, and is distinguishable from a fact predating the column.
3. A `NativeJournalPayload` payload with no `turn_id` field still deserializes despite `deny_unknown_fields`.
4. `derive_conversation_rows` emits a `TurnFold` at a real turn boundary sourced from the journal, not only from hand-built fixtures — the gap that made the existing fold tests pass while the feature was inert.
5. No turn identifier is ever synthesized. A test asserts that a hook payload lacking a turn id produces `NULL`, not a generated value.
6. The migration applies to an existing populated database without loss, and the full library suite passes.

## Out of scope

Rendering. The fold's visual treatment belongs to the Target UX section of the conversation derivation spec and does not change here.
