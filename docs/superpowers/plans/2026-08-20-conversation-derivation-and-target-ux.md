# Conversation Derivation and Target UX Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make DevManager's native transcript derive from a closed conversation vocabulary in Rust, then paint it with the Target UX, so the surface can no longer regress into debug rows.

**Architecture:** A new pure module `src/ui/conversation/rows.rs` turns the existing `Vec<TimelineItemModel>` projection into a closed `ConversationRow` list, with activity grouping, stable row identity, turn folding, and lifecycle suppression by discriminant rather than by substring. `src/ui/task_cockpit/timeline.rs` is reduced to virtualization and painting over those rows. Classification stops round-tripping through localized display strings.

**Tech Stack:** Rust 2021, `gpui` 0.2.2, `gpui-component` 0.5.1, existing `ThemeTokens` in `src/ui/tokens.rs`.

## Global Constraints

- Isolated Cargo target directory is mandatory. Resolve and print it before the first Cargo command and fail if it is not beneath the active worktree or an exact `C:\Temp\devmanager-*` root. Never share the daily checkout's target directory.
- Focused tests may run normally. The full library suite runs as `cargo test --lib -- --test-threads=1`.
- Every train must pass `cargo check --locked --lib --bins --tests` in an isolated target before it is merge-ready.
- Do not set an external `DEVMANAGER_PROFILE` for the full suite.
- Cargo colorizes output. When reading a build log, strip ANSI first (`sed 's/\x1b\[[0-9;]*m//g'`) or an anchored `^error` grep silently reports zero errors on a failed build.
- The conversation column measure is **768 px** (`CONVERSATION_CONTENT_MAX_WIDTH`, defined in `src/ui/task_cockpit/timeline.rs`), matching T3 Code's `max-w-3xl` geometry and the accepted visual reference.
- `items_end()` on a flex column collapses its children to zero width in GPUI and they never paint. Right alignment uses `justify_end()` on a row.
- A definite width (`w(px(..))`) is required for the readable measure. `w_full().max_w(..)` does not clamp.
- Preserve every invariant in the "Invariants preserved" section of `docs/superpowers/specs/2026-08-20-conversation-derivation-single-source-design.md`: host authority, `providerSessionId` never synthesized, `ComposerFence` / `action_epoch` / `runtime_generation` fencing, question and approval identity fencing, `PromptVersionRef` provenance, `ComposerHostProjection` boundary, no raw PTY becoming conversation truth, and durable journal storage and `ALLOWED_JOURNAL_KINDS` unchanged.
- `src/ui/conversation_preview.rs` is the throwaway look prototype committed in `5336cc2`. It is a reference for the Target UX treatments during Train 2 and is deleted in Task 10.

---

## File Structure

| File | Responsibility |
| --- | --- |
| `src/ui/renderers/message.rs` | Gains `MessageRole`, a closed enum, replacing role classification by display string. |
| `src/ui/renderers/journal_view.rs` | Populates `MessageRole` from the journal fact kind instead of writing `"You"` / `"Assistant"` / `"Reasoning"` and reading them back. |
| `src/ui/conversation/mod.rs` | New module root. |
| `src/ui/conversation/rows.rs` | New. Pure derivation: `ConversationRow`, `derive_conversation_rows`, `stable_conversation_rows`, activity grouping, turn folding. No `gpui` imports. |
| `src/ui/conversation/render.rs` | New. Target UX painting for each `ConversationRow`. Owns all row treatments. |
| `src/ui/task_cockpit/timeline.rs` | Reduced to virtualization, scroll, follow behaviour, and delegation to `render.rs`. Loses classification entirely. |
| `tests/conversation_conformance.rs` | New. Phase 1 conformance: Rust derivation against the shared fixture corpus that `web/src/tasks/timeline/timelineModel.test.ts` also covers. |
| `tests/fixtures/conversation/*.json` | New. Shared fixture corpus, authoritative for both derivations. |

---

# Train 1 — Transcript derivation

### Task 1: Replace role-string classification with a closed enum

The defect: `src/ui/renderers/journal_view.rs:246-319` writes **localized display strings** into `MessageView.role` (`"You"`, `"Assistant"`, `"Reasoning"`, `format!("Error ({code})")`), and `src/ui/task_cockpit/timeline.rs:33-46` classifies by reading them back with `role.starts_with("error")` and `role.contains("reason")`. Renaming `"Reasoning"` to `"Thinking"` would silently reclassify every reasoning summary as ordinary assistant prose, with no test failure.

**Files:**
- Modify: `src/ui/renderers/message.rs:12-16`
- Modify: `src/ui/renderers/journal_view.rs:246-319`
- Test: `src/ui/renderers/message.rs` (inline `#[cfg(test)]` module)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `pub enum MessageRole { User, Assistant, Reasoning, Error }` and field `MessageView::role_kind: MessageRole`. `MessageView::role: String` is retained as the **display label only** and must never be matched on again.

- [ ] **Step 1: Write the failing test**

Add to `src/ui/renderers/message.rs`:

```rust
#[cfg(test)]
mod role_tests {
    use super::*;

    #[test]
    fn role_kind_is_independent_of_the_display_label() {
        let view = MessageView {
            role: "Thinking".to_string(),
            role_kind: MessageRole::Reasoning,
            streaming: false,
            markdown: MarkdownDocument {
                selectable: true,
                copyable: true,
                html_executed: false,
                prose_wraps: true,
                blocks: Vec::new(),
                pending_links: Vec::new(),
            },
        };
        // Renaming the label must not change classification.
        assert_eq!(view.role_kind, MessageRole::Reasoning);
        assert_ne!(view.role_kind, MessageRole::Assistant);
    }

    #[test]
    fn every_role_kind_is_distinct() {
        let all = [
            MessageRole::User,
            MessageRole::Assistant,
            MessageRole::Reasoning,
            MessageRole::Error,
        ];
        for (index, left) in all.iter().enumerate() {
            for (other, right) in all.iter().enumerate() {
                assert_eq!(index == other, left == right);
            }
        }
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_TARGET_DIR="C:/Temp/devmanager-train1" cargo test --lib role_tests`
Expected: FAIL to compile — `cannot find type MessageRole`, `struct MessageView has no field role_kind`.

- [ ] **Step 3: Add the enum and field**

In `src/ui/renderers/message.rs`, replace the `MessageView` struct:

```rust
/// Closed conversation role. Classification must read this, never the
/// human-facing `role` label, which is a display string and may be reworded
/// or localized at any time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRole {
    User,
    Assistant,
    Reasoning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageView {
    /// Display label only. Never match on this.
    pub role: String,
    pub role_kind: MessageRole,
    pub streaming: bool,
    pub markdown: MarkdownDocument,
}
```

- [ ] **Step 4: Thread the discriminant through `SemanticEventBody::Message`**

**Corrected after a first implementation attempt proved this plan wrong.** The four `journal_view.rs` sites do not construct `MessageView` at all - they construct `SemanticEventBody::Message`, and `src/ui/renderers/message.rs` later projects that into a `MessageView`. So adding a field to `MessageView` alone cannot carry the discriminant: `message.rs` has nothing to read it from and can only guess.

The role is destroyed one layer earlier, at `src/ui/renderers/mod.rs:196-200`:

```rust
    Message {
        role: String,
        text: String,
        streaming: bool,
    },
```

Add the discriminant there:

```rust
    Message {
        /// Display label only. Never match on this.
        role: String,
        role_kind: MessageRole,
        text: String,
        streaming: bool,
    },
```

Populate it at the four `journal_view.rs` sites, each of which already has the typed `SemanticJournalPayload` variant in hand - the discriminant is right there and is currently thrown away:

```rust
SemanticJournalPayload::UserMessage { text } => SemanticEventBody::Message {
    role: "You".to_string(),
    role_kind: MessageRole::User,
    text: text.clone(),
    streaming: false,
},
SemanticJournalPayload::AssistantText { text } => SemanticEventBody::Message {
    role: "Assistant".to_string(),
    role_kind: MessageRole::Assistant,
    text: text.clone(),
    streaming: false,
},
SemanticJournalPayload::ReasoningSummary { text } => SemanticEventBody::Message {
    role: "Reasoning".to_string(),
    role_kind: MessageRole::Reasoning,
    text: text.clone(),
    streaming: false,
},
```

and the error site at roughly line 319 with `role_kind: MessageRole::Error`.

Then have `message.rs`'s `project` destructure `role_kind` alongside `role` and pass it into the `MessageView` it builds. `AgentView` is a different type and needs no change.

- [ ] **Step 4b: Prove the discriminant survives the projection**

The Step 1 tests construct a `MessageView` by hand, so they cannot detect a `message.rs` that hardcodes one role - which is exactly what a first attempt shipped, green. Add a test that goes through the real projection:

```rust
    #[test]
    fn a_reasoning_payload_projects_to_a_reasoning_role_kind() {
        let event = semantic_event(SemanticEventBody::Message {
            role: "Reasoning".to_string(),
            role_kind: MessageRole::Reasoning,
            text: "checking fences".to_string(),
            streaming: false,
        });
        let item = MessageRenderer.project(&event).expect("projection");
        match item.content {
            TimelineItemContent::Message(view) => {
                assert_eq!(view.role_kind, MessageRole::Reasoning);
            }
            other => panic!("expected message content, got {other:?}"),
        }
    }

    #[test]
    fn an_error_payload_does_not_project_as_assistant() {
        let event = semantic_event(SemanticEventBody::Message {
            role: "Error (provider)".to_string(),
            role_kind: MessageRole::Error,
            text: "exact resume failed".to_string(),
            streaming: false,
        });
        let item = MessageRenderer.project(&event).expect("projection");
        match item.content {
            TimelineItemContent::Message(view) => {
                assert_ne!(view.role_kind, MessageRole::Assistant);
                assert_eq!(view.role_kind, MessageRole::Error);
            }
            other => panic!("expected message content, got {other:?}"),
        }
    }
```

Build `semantic_event` from the existing `SemanticEvent` shape in `src/ui/renderers/mod.rs:144-155`. These two tests are the ones that actually gate this task: a projection that hardcodes `MessageRole::Assistant` must fail them.

- [ ] **Step 5: Run tests to verify they pass**

Run: `CARGO_TARGET_DIR="C:/Temp/devmanager-train1" cargo test --lib role_tests`
Expected: PASS, `2 passed`.

- [ ] **Step 6: Confirm the build is clean**

Run: `CARGO_TARGET_DIR="C:/Temp/devmanager-train1" cargo check --locked --lib --bins --tests > /tmp/t1.log 2>&1; echo "EXIT=$?"; sed 's/\x1b\[[0-9;]*m//g' /tmp/t1.log | grep -c '^error'`
Expected: `EXIT=0` and error count `0`.

- [ ] **Step 7: Commit**

```bash
git add src/ui/renderers/message.rs src/ui/renderers/journal_view.rs src/ui/renderers/agent.rs
git commit -m "refactor(ui): classify messages by a closed role enum

journal_view wrote localized display strings into MessageView.role and
timeline.rs read them back with substring matches, so rewording a label
silently reclassified the message. role_kind carries the discriminant;
role stays a display label only."
```

---

### Task 2: Closed ConversationRow and the derivation core

**Files:**
- Create: `src/ui/conversation/mod.rs`
- Create: `src/ui/conversation/rows.rs`
- Modify: `src/ui/mod.rs` (add `pub mod conversation;` in alphabetical position, before `pub mod conversation_preview;`)
- Test: inline `#[cfg(test)]` in `src/ui/conversation/rows.rs`

**Interfaces:**
- Consumes: `MessageRole` from Task 1. `TimelineItemModel`, `TimelineItemContent`, `TimelineItemId` from `src/ui/renderers/mod.rs:271-330`.
- Produces:
  - `pub enum ConversationRow { Message {..}, Activity {..}, ActivityToggle {..}, Question {..}, Error {..}, TurnFold {..}, Working {..} }`
  - `pub enum ConversationVerbosity { Minimal, Calm, Full }`
  - `pub fn derive_conversation_rows(items: &[TimelineItemModel], verbosity: ConversationVerbosity) -> Vec<ConversationRow>`
  - `pub fn conversation_row_key(row: &ConversationRow) -> ConversationRowKey`

**Terminal fallback output is deliberately absent.** T3 Code's row vocabulary has a `fallbackOutput` variant fed by its `output` event, and `web/src/tasks/timeline/timelineModel.ts` bounds it to 12,000 characters. DevManager's `TimelineItemContent` has **no `Output` variant** - wire-level `SemanticEventKind::Output` currently lands in `Generic` - so nothing could construct such a row, and a variant no code produces is dead weight. It is added when `TimelineItemContent` gains `Output`, together with the bounding rule. Until then the Task 6 conformance corpus must contain no `output` events, or the two derivations will legitimately disagree.

On the spec's `Unknown` variant: `TimelineItemContent::Generic(GenericSemanticCard)` **already is** that representation. It carries the `source_type`, it stays addressable by the inspector, and this task simply stops it producing a conversation row. Do not add a second Unknown type beside it.

`ConversationVerbosity` is deliberately **not** named `Density`: `src/ui/tokens.rs:15` already defines `Density { Compact, Comfortable }`, a visual metric scale over spacing, radii, typography, icons, controls and motion. One selects which rows exist, the other how large they are.

- [ ] **Step 1: Write the failing test**

Create `src/ui/conversation/rows.rs` containing only this test module for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_items_produce_no_conversation_row() {
        let items = vec![generic_item("session_state"), generic_item("wholly_new_kind")];
        let rows = derive_conversation_rows(&items, ConversationVerbosity::Calm);
        assert!(
            rows.is_empty(),
            "unrecognised content must be structurally unrenderable, got {rows:?}"
        );
    }

    #[test]
    fn an_unknown_kind_nobody_denylisted_is_still_suppressed() {
        // The regression this whole change exists to prevent.
        let items = vec![generic_item("provider_v9_telemetry_frame")];
        let rows = derive_conversation_rows(&items, ConversationVerbosity::Calm);
        assert!(rows.is_empty());
    }

    #[test]
    fn user_and_assistant_messages_become_message_rows() {
        let items = vec![
            message_item(MessageRole::User, "find the flake"),
            message_item(MessageRole::Assistant, "two share a cause"),
        ];
        let rows = derive_conversation_rows(&items, ConversationVerbosity::Calm);
        assert_eq!(rows.len(), 2);
        assert!(matches!(
            &rows[0],
            ConversationRow::Message { role: MessageRole::User, .. }
        ));
        assert!(matches!(
            &rows[1],
            ConversationRow::Message { role: MessageRole::Assistant, .. }
        ));
    }

    #[test]
    fn consecutive_tools_collapse_into_one_activity_row() {
        let items = vec![
            tool_item("t1", "Read", "succeeded"),
            tool_item("t2", "Read", "succeeded"),
            tool_item("t3", "Bash", "succeeded"),
            message_item(MessageRole::Assistant, "done"),
        ];
        let rows = derive_conversation_rows(&items, ConversationVerbosity::Calm);
        assert_eq!(rows.len(), 2);
        match &rows[0] {
            ConversationRow::Activity { entries, state, .. } => {
                assert_eq!(entries.len(), 3);
                assert_eq!(*state, ActivityState::Success);
            }
            other => panic!("expected an activity row, got {other:?}"),
        }
    }

    #[test]
    fn a_repeated_tool_id_replaces_rather_than_appends() {
        let items = vec![
            tool_item("t1", "Bash", "running"),
            tool_item("t1", "Bash", "succeeded"),
        ];
        let rows = derive_conversation_rows(&items, ConversationVerbosity::Calm);
        match &rows[0] {
            ConversationRow::Activity { entries, state, .. } => {
                assert_eq!(entries.len(), 1, "same tool id must update in place");
                assert_eq!(*state, ActivityState::Success);
            }
            other => panic!("expected an activity row, got {other:?}"),
        }
    }

    #[test]
    fn a_failed_tool_makes_the_group_a_failure() {
        let items = vec![tool_item("t1", "Read", "succeeded"), tool_item("t2", "Bash", "failed")];
        let rows = derive_conversation_rows(&items, ConversationVerbosity::Calm);
        match &rows[0] {
            ConversationRow::Activity { state, .. } => assert_eq!(*state, ActivityState::Failure),
            other => panic!("expected an activity row, got {other:?}"),
        }
    }

    #[test]
    fn minimal_verbosity_drops_settled_successful_activity() {
        let items = vec![tool_item("t1", "Read", "succeeded")];
        assert!(derive_conversation_rows(&items, ConversationVerbosity::Minimal).is_empty());
        assert_eq!(derive_conversation_rows(&items, ConversationVerbosity::Calm).len(), 1);
    }

    #[test]
    fn minimal_verbosity_keeps_a_running_group() {
        let items = vec![tool_item("t1", "Bash", "running")];
        assert_eq!(derive_conversation_rows(&items, ConversationVerbosity::Minimal).len(), 1);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_TARGET_DIR="C:/Temp/devmanager-train1" cargo test --lib conversation::rows`
Expected: FAIL to compile — nothing in the module exists yet.

- [ ] **Step 3: Write the module**

Prepend to `src/ui/conversation/rows.rs`:

```rust
//! Pure conversation derivation over the sealed journal projection.
//!
//! This module has no `gpui` imports and is unit-testable without a window.
//! The row vocabulary is CLOSED: there is no generic, diagnostic, or
//! escape-hatch variant, so an event kind nobody has mapped is structurally
//! unrenderable rather than suppressed by a denylist.

use crate::ui::renderers::{MessageRole, TimelineItemContent, TimelineItemId, TimelineItemModel};

/// How much settled activity is derived at all. Distinct from
/// `crate::ui::tokens::Density`, which is a visual metric scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversationVerbosity {
    Minimal,
    Calm,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityState {
    Active,
    Success,
    Failure,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityEntry {
    pub identity: String,
    pub label: String,
    pub detail: String,
    pub state: ActivityState,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ConversationRowKey {
    Message(String),
    Activity(String),
    ActivityToggle(String),
    Question(String),
    Error(String),
    TurnFold(String),
    Working,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConversationRow {
    Message {
        id: TimelineItemId,
        role: MessageRole,
        text: String,
        streaming: bool,
    },
    Activity {
        anchor: TimelineItemId,
        entries: Vec<ActivityEntry>,
        state: ActivityState,
        summary: String,
    },
    ActivityToggle {
        group: String,
        hidden: usize,
        expanded: bool,
        only_tools: bool,
    },
    Question {
        id: TimelineItemId,
        prompt: String,
        choices: Vec<String>,
        settled_choice: Option<usize>,
    },
    Error {
        id: TimelineItemId,
        text: String,
    },
    TurnFold {
        turn_id: String,
        label: String,
        expanded: bool,
    },
    Working {
        elapsed_ms: Option<u64>,
        step: Option<String>,
    },
}

pub fn conversation_row_key(row: &ConversationRow) -> ConversationRowKey {
    match row {
        ConversationRow::Message { id, .. } => ConversationRowKey::Message(format!("{id:?}")),
        ConversationRow::Activity { anchor, .. } => {
            ConversationRowKey::Activity(format!("{anchor:?}"))
        }
        ConversationRow::ActivityToggle { group, .. } => {
            ConversationRowKey::ActivityToggle(group.clone())
        }
        ConversationRow::Question { id, .. } => ConversationRowKey::Question(format!("{id:?}")),
        ConversationRow::Error { id, .. } => ConversationRowKey::Error(format!("{id:?}")),
        ConversationRow::TurnFold { turn_id, .. } => {
            ConversationRowKey::TurnFold(turn_id.clone())
        }
        ConversationRow::Working { .. } => ConversationRowKey::Working,
    }
}

fn activity_state_of(entries: &[ActivityEntry]) -> ActivityState {
    if entries.iter().any(|e| e.state == ActivityState::Failure) {
        return ActivityState::Failure;
    }
    if entries.iter().any(|e| e.state == ActivityState::Active) {
        return ActivityState::Active;
    }
    ActivityState::Success
}

fn activity_summary(entries: &[ActivityEntry]) -> String {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for entry in entries {
        match counts.iter_mut().find(|(label, _)| label == &entry.label) {
            Some((_, count)) => *count += 1,
            None => counts.push((entry.label.clone(), 1)),
        }
    }
    let parts: Vec<String> = counts
        .into_iter()
        .map(|(label, count)| {
            if count > 1 {
                format!("{label} x{count}")
            } else {
                label
            }
        })
        .collect();
    let plural = if entries.len() == 1 { "" } else { "s" };
    format!("{} action{plural} - {}", entries.len(), parts.join(" - "))
}

/// Maps one projected item to an activity entry, or `None` when the item is
/// not activity. This match is TOTAL over `TimelineItemContent` -- every
/// variant is named explicitly and there is no `_` arm, so adding a variant is
/// a compile error here as well as in the main loop. Do not "simplify" either
/// one to a wildcard: making an unmapped kind unrenderable is the whole point
/// of this module, and a wildcard silently defaults it to visible.
fn activity_entry_of(item: &TimelineItemModel) -> Option<ActivityEntry> {
    match &item.content {
        TimelineItemContent::Tool(view) => Some(ActivityEntry {
            identity: format!("tool:{}", view.tool_id),
            label: view.name.clone(),
            detail: view.summary.clone(),
            state: match view.state.as_str() {
                "failed" => ActivityState::Failure,
                "pending" | "running" => ActivityState::Active,
                _ => ActivityState::Success,
            },
        }),
        TimelineItemContent::Plan(view) => Some(ActivityEntry {
            identity: format!("plan:{}", view.title),
            label: "Plan".to_string(),
            detail: view.title.clone(),
            state: match view.status.as_str() {
                "failed" => ActivityState::Failure,
                "running" | "pending" => ActivityState::Active,
                _ => ActivityState::Success,
            },
        }),
        TimelineItemContent::Message(_)
        | TimelineItemContent::Question(_)
        | TimelineItemContent::Approval(_)
        | TimelineItemContent::Operation(_)
        | TimelineItemContent::Artifact(_)
        | TimelineItemContent::Agent(_)
        | TimelineItemContent::Generic(_) => None,
    }
}

pub fn derive_conversation_rows(
    items: &[TimelineItemModel],
    verbosity: ConversationVerbosity,
) -> Vec<ConversationRow> {
    let mut rows: Vec<ConversationRow> = Vec::new();
    let mut pending: Vec<ActivityEntry> = Vec::new();
    let mut pending_anchor: Option<TimelineItemId> = None;

    let mut flush = |rows: &mut Vec<ConversationRow>,
                     pending: &mut Vec<ActivityEntry>,
                     anchor: &mut Option<TimelineItemId>| {
        if pending.is_empty() {
            return;
        }
        let state = activity_state_of(pending);
        let drop_settled =
            verbosity == ConversationVerbosity::Minimal && state == ActivityState::Success;
        if !drop_settled {
            if let Some(anchor_id) = *anchor {
                rows.push(ConversationRow::Activity {
                    anchor: anchor_id,
                    entries: pending.clone(),
                    state,
                    summary: activity_summary(pending),
                });
            }
        }
        pending.clear();
        *anchor = None;
    };

    for item in items {
        if let Some(entry) = activity_entry_of(item) {
            if pending_anchor.is_none() {
                pending_anchor = Some(item.id);
            }
            match pending.iter_mut().find(|e| e.identity == entry.identity) {
                Some(existing) => *existing = entry,
                None => pending.push(entry),
            }
            continue;
        }

        match &item.content {
            TimelineItemContent::Message(view) => {
                flush(&mut rows, &mut pending, &mut pending_anchor);
                let text = view.markdown.plain_text();
                match view.role_kind {
                    MessageRole::Error => rows.push(ConversationRow::Error { id: item.id, text }),
                    role => rows.push(ConversationRow::Message {
                        id: item.id,
                        role,
                        text,
                        streaming: view.streaming,
                    }),
                }
            }
            TimelineItemContent::Question(view) => {
                flush(&mut rows, &mut pending, &mut pending_anchor);
                rows.push(ConversationRow::Question {
                    id: item.id,
                    prompt: view.prompt.clone(),
                    choices: view.choices.clone(),
                    settled_choice: view.settled_choice,
                });
            }
            // Approvals, operations, artifacts, agents and generic extension
            // rows contribute NO conversation row. They remain addressable by
            // the inspector. This arm is what makes an unmapped kind invisible
            // by construction rather than by denylist.
            TimelineItemContent::Approval(_)
            | TimelineItemContent::Operation(_)
            | TimelineItemContent::Artifact(_)
            | TimelineItemContent::Agent(_)
            | TimelineItemContent::Generic(_) => {}
            TimelineItemContent::Tool(_) | TimelineItemContent::Plan(_) => unreachable!(
                "tool and plan items are handled by activity_entry_of above"
            ),
        }
    }

    flush(&mut rows, &mut pending, &mut pending_anchor);
    rows
}
```

`MarkdownDocument::plain_text()` already exists at `src/ui/renderers/message.rs:29`. Use it; do not add a second text accessor.

- [ ] **Step 4: Reuse the existing test helpers**

`src/ui/task_cockpit/timeline.rs` already defines `message_item`, `tool_item` and `generic_item` in its `#[cfg(test)]` module at lines 898-947. **Do not write new ones.** Move those three functions into a shared `#[cfg(test)] pub(crate) mod fixtures` under `src/ui/conversation/`, and have both `timeline.rs` and `rows.rs` import them.

Three details the existing helpers already get right, which a fresh implementation gets wrong:

- `ToolView` has a fifth field, `provider_specific: bool`. Omitting it will not compile.
- `MarkdownDocument` has no constructor. Build the struct literal with `selectable`, `copyable`, `html_executed`, `prose_wraps`, `blocks: vec![MarkdownBlock::Paragraph { text }]` and `pending_links: Vec::new()`.
- Ids are made with `EventId::new()` and `TaskId::new()`, not a `from_raw` constructor. `AccessibilityMetadata::new(AccessibleRole::Region, role)` returns a `Result` and the helpers `.expect("accessible name")` on it.

Extend `message_item` to take a `MessageRole` alongside its display label, since Task 1 added `role_kind`.

- [ ] **Step 5: Run tests to verify they pass**

Run: `CARGO_TARGET_DIR="C:/Temp/devmanager-train1" cargo test --lib conversation::rows`
Expected: PASS, `8 passed`.

- [ ] **Step 6: Commit**

```bash
git add src/ui/conversation/ src/ui/mod.rs src/ui/renderers/message.rs
git commit -m "feat(ui): derive conversation rows from a closed vocabulary

ConversationRow has no generic or diagnostic variant, so an event kind
nobody mapped produces no row at all instead of a debug card suppressed
by a substring denylist."
```

---

### Task 3: Stable row identity across streaming deltas

Without this the transcript re-mounts rows while assistant tokens stream.

**Files:**
- Modify: `src/ui/conversation/rows.rs`
- Test: inline

**Interfaces:**
- Consumes: `ConversationRow`, `conversation_row_key` from Task 2.
- Produces: `pub fn stable_conversation_rows(previous: &[ConversationRow], next: Vec<ConversationRow>) -> Vec<ConversationRow>`

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn row_keys_survive_a_streaming_text_change() {
        // A streaming delta is the SAME event carrying more text. `message_item`
        // mints a fresh EventId per call, so calling it twice models two
        // different messages and no key-based reconciliation could ever match
        // them -- copy the id across to model the real thing.
        let item = message_item(MessageRole::Assistant, "Two of");
        let mut grown = message_item(MessageRole::Assistant, "Two of the three");
        grown.id = item.id;

        let first = derive_conversation_rows(&[item], ConversationVerbosity::Calm);
        let second = derive_conversation_rows(&[grown], ConversationVerbosity::Calm);
        let stable = stable_conversation_rows(&first, second);
        assert_eq!(
            conversation_row_key(&first[0]),
            conversation_row_key(&stable[0]),
            "a growing message must keep its key"
        );
        match &stable[0] {
            ConversationRow::Message { text, .. } => assert_eq!(text, "Two of the three"),
            other => panic!("expected a message row, got {other:?}"),
        }
    }

    #[test]
    fn an_unchanged_row_is_returned_untouched() {
        let rows = derive_conversation_rows(
            &[message_item(MessageRole::User, "hello")],
            ConversationVerbosity::Calm,
        );
        let stable = stable_conversation_rows(&rows, rows.clone());
        assert_eq!(rows, stable);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_TARGET_DIR="C:/Temp/devmanager-train1" cargo test --lib conversation::rows::tests::row_keys_survive`
Expected: FAIL — `cannot find function stable_conversation_rows`.

- [ ] **Step 3: Implement**

```rust
/// Preserves row identity across derivations so a streaming message updates
/// in place rather than re-mounting. Keys come from `conversation_row_key`,
/// which is derived from durable ids, never from content.
pub fn stable_conversation_rows(
    previous: &[ConversationRow],
    next: Vec<ConversationRow>,
) -> Vec<ConversationRow> {
    if previous.is_empty() {
        return next;
    }
    let mut prior: Vec<(ConversationRowKey, &ConversationRow)> = previous
        .iter()
        .map(|row| (conversation_row_key(row), row))
        .collect();
    next.into_iter()
        .map(|row| {
            let key = conversation_row_key(&row);
            match prior.iter().position(|(prior_key, _)| prior_key == &key) {
                Some(index) => {
                    let (_, existing) = prior.remove(index);
                    if existing == &row {
                        existing.clone()
                    } else {
                        row
                    }
                }
                None => row,
            }
        })
        .collect()
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `CARGO_TARGET_DIR="C:/Temp/devmanager-train1" cargo test --lib conversation::rows`
Expected: PASS, `10 passed`.

- [ ] **Step 5: Commit**

```bash
git add src/ui/conversation/rows.rs
git commit -m "feat(ui): keep conversation row identity stable while streaming"
```

---

### Task 4: The activity toggle

**Files:**
- Modify: `src/ui/conversation/rows.rs`
- Test: inline

**Interfaces:**
- Consumes: everything from Tasks 2 and 3.
- Produces: `pub const MAX_VISIBLE_ACTIVITY_ENTRIES: usize = 1;` and `pub fn apply_activity_collapse(rows: Vec<ConversationRow>, expanded: &[String]) -> Vec<ConversationRow>`

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn a_group_over_the_visible_cap_gains_a_toggle() {
        let rows = derive_conversation_rows(
            &[
                tool_item("t1", "Read", "succeeded"),
                tool_item("t2", "Read", "succeeded"),
                tool_item("t3", "Bash", "succeeded"),
            ],
            ConversationVerbosity::Calm,
        );
        let collapsed = apply_activity_collapse(rows, &[]);
        assert_eq!(collapsed.len(), 2);
        match &collapsed[0] {
            ConversationRow::Activity { entries, .. } => {
                assert_eq!(entries.len(), MAX_VISIBLE_ACTIVITY_ENTRIES);
            }
            other => panic!("expected an activity row, got {other:?}"),
        }
        match &collapsed[1] {
            ConversationRow::ActivityToggle { hidden, expanded, only_tools, .. } => {
                assert_eq!(*hidden, 2);
                assert!(!*expanded);
                assert!(*only_tools);
            }
            other => panic!("expected a toggle row, got {other:?}"),
        }
    }

    #[test]
    fn an_expanded_group_shows_every_entry() {
        let rows = derive_conversation_rows(
            &[
                tool_item("t1", "Read", "succeeded"),
                tool_item("t2", "Read", "succeeded"),
                tool_item("t3", "Bash", "succeeded"),
            ],
            ConversationVerbosity::Calm,
        );
        let group = match &rows[0] {
            ConversationRow::Activity { anchor, .. } => format!("{anchor:?}"),
            other => panic!("expected an activity row, got {other:?}"),
        };
        let expanded = apply_activity_collapse(rows, &[group]);
        match &expanded[0] {
            ConversationRow::Activity { entries, .. } => assert_eq!(entries.len(), 3),
            other => panic!("expected an activity row, got {other:?}"),
        }
        match &expanded[1] {
            ConversationRow::ActivityToggle { expanded, .. } => assert!(*expanded),
            other => panic!("expected a toggle row, got {other:?}"),
        }
    }

    #[test]
    fn a_group_at_or_under_the_cap_gains_no_toggle() {
        let rows = derive_conversation_rows(
            &[tool_item("t1", "Read", "succeeded")],
            ConversationVerbosity::Calm,
        );
        assert_eq!(apply_activity_collapse(rows, &[]).len(), 1);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_TARGET_DIR="C:/Temp/devmanager-train1" cargo test --lib conversation::rows::tests::a_group_over`
Expected: FAIL — `cannot find function apply_activity_collapse`.

- [ ] **Step 3: Implement**

```rust
/// Target UX: exactly one activity entry stays visible; the rest sit behind a
/// toggle. This single number is most of why the transcript reads calm.
pub const MAX_VISIBLE_ACTIVITY_ENTRIES: usize = 1;

pub fn apply_activity_collapse(
    rows: Vec<ConversationRow>,
    expanded: &[String],
) -> Vec<ConversationRow> {
    let mut out: Vec<ConversationRow> = Vec::with_capacity(rows.len());
    for row in rows {
        let ConversationRow::Activity {
            anchor,
            entries,
            state,
            summary,
        } = row
        else {
            out.push(row);
            continue;
        };
        let group = format!("{anchor:?}");
        let is_expanded = expanded.iter().any(|candidate| candidate == &group);
        if entries.len() <= MAX_VISIBLE_ACTIVITY_ENTRIES {
            out.push(ConversationRow::Activity { anchor, entries, state, summary });
            continue;
        }
        let hidden = entries.len() - MAX_VISIBLE_ACTIVITY_ENTRIES;
        let only_tools = entries.iter().all(|entry| entry.identity.starts_with("tool:"));
        let shown = if is_expanded {
            entries.clone()
        } else {
            entries
                .iter()
                .rev()
                .take(MAX_VISIBLE_ACTIVITY_ENTRIES)
                .rev()
                .cloned()
                .collect()
        };
        out.push(ConversationRow::Activity {
            anchor,
            entries: shown,
            state,
            summary,
        });
        out.push(ConversationRow::ActivityToggle {
            group,
            hidden,
            expanded: is_expanded,
            only_tools,
        });
    }
    out
}

/// Count- and kind-aware toggle copy, matching the Target UX exactly.
pub fn activity_toggle_label(hidden: usize, expanded: bool, only_tools: bool) -> String {
    let noun = match (only_tools, hidden) {
        (true, 1) => "previous tool call",
        (true, _) => "previous tool calls",
        (false, 1) => "previous log entry",
        (false, _) => "previous log entries",
    };
    if expanded {
        let shown = if only_tools { "tool calls" } else { "log entries" };
        format!("Show fewer {shown}")
    } else {
        format!("+{hidden} {noun}")
    }
}
```

- [ ] **Step 4: Add a test for the copy**

```rust
    #[test]
    fn toggle_copy_is_count_and_kind_aware() {
        assert_eq!(activity_toggle_label(1, false, true), "+1 previous tool call");
        assert_eq!(activity_toggle_label(3, false, true), "+3 previous tool calls");
        assert_eq!(activity_toggle_label(1, false, false), "+1 previous log entry");
        assert_eq!(activity_toggle_label(2, true, true), "Show fewer tool calls");
        assert_eq!(activity_toggle_label(2, true, false), "Show fewer log entries");
    }
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `CARGO_TARGET_DIR="C:/Temp/devmanager-train1" cargo test --lib conversation::rows`
Expected: PASS, `14 passed`.

- [ ] **Step 6: Commit**

```bash
git add src/ui/conversation/rows.rs
git commit -m "feat(ui): collapse activity groups behind a count-aware toggle"
```

---

### Task 4b: Turn folds, the working row, and elapsed-time attribution

Task 2 declared `ConversationRow::TurnFold` and `ConversationRow::Working` but nothing constructs either, and the spec's elapsed-time attribution is not yet implemented. A variant no code produces is dead weight; this task fills all three.

**Files:**
- Modify: `src/ui/conversation/rows.rs`
- Test: inline

**Interfaces:**
- Consumes: `ConversationRow`, `derive_conversation_rows` from Task 2.
- Produces: `pub fn duration_boundaries(rows: &[ConversationRow]) -> Vec<Option<usize>>`, plus turn-fold and working emission inside `derive_conversation_rows`.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn a_turn_boundary_emits_a_fold_row() {
        let mut first = message_item(MessageRole::Assistant, "older answer");
        first.turn_id = Some("turn-1".to_string());
        let mut second = message_item(MessageRole::User, "next question");
        second.turn_id = Some("turn-2".to_string());
        let rows = derive_conversation_rows(&[first, second], ConversationVerbosity::Calm);
        let folded = rows.iter().any(|row| {
            matches!(row, ConversationRow::TurnFold { turn_id, .. } if turn_id == "turn-2")
        });
        assert!(folded, "a change of turn must emit a fold, got {rows:?}");
    }

    #[test]
    fn items_without_a_turn_id_emit_no_fold() {
        let rows = derive_conversation_rows(
            &[
                message_item(MessageRole::User, "a"),
                message_item(MessageRole::Assistant, "b"),
            ],
            ConversationVerbosity::Calm,
        );
        assert!(!rows
            .iter()
            .any(|row| matches!(row, ConversationRow::TurnFold { .. })));
    }

    #[test]
    fn a_running_tool_emits_a_working_row_at_the_tail() {
        let rows = derive_conversation_rows(
            &[tool_item("t1", "Bash", "running", "")],
            ConversationVerbosity::Calm,
        );
        assert!(
            matches!(rows.last(), Some(ConversationRow::Working { .. })),
            "an active group must be followed by a working row, got {rows:?}"
        );
    }

    #[test]
    fn a_settled_transcript_emits_no_working_row() {
        let rows = derive_conversation_rows(
            &[tool_item("t1", "Bash", "succeeded", "")],
            ConversationVerbosity::Calm,
        );
        assert!(!rows
            .iter()
            .any(|row| matches!(row, ConversationRow::Working { .. })));
    }

    #[test]
    fn a_user_message_opens_a_duration_boundary_and_a_settled_assistant_closes_it() {
        let rows = derive_conversation_rows(
            &[
                message_item(MessageRole::User, "q"),
                message_item(MessageRole::Assistant, "a"),
                message_item(MessageRole::User, "q2"),
            ],
            ConversationVerbosity::Calm,
        );
        let boundaries = duration_boundaries(&rows);
        assert_eq!(boundaries.len(), rows.len());
        assert_eq!(boundaries[0], Some(0), "a user turn opens its own boundary");
        assert_eq!(boundaries[1], Some(0), "the answer is measured from the question");
        assert_eq!(boundaries[2], Some(2), "the next question opens a new boundary");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_TARGET_DIR="C:/Temp/devmanager-train1" cargo test --lib conversation::rows::tests::a_turn_boundary`
Expected: FAIL - no fold is emitted, and `duration_boundaries` does not exist.

- [ ] **Step 3: Emit folds and the working row**

Inside `derive_conversation_rows`, track the previous item's `turn_id`. When an item carries a `turn_id` differing from the previous non-`None` one, flush pending activity and push:

```rust
                rows.push(ConversationRow::TurnFold {
                    turn_id: turn.clone(),
                    label: "Earlier turn".to_string(),
                    expanded: false,
                });
```

After the final flush, append a working row when the last activity group is still active:

```rust
    let tail_is_active = matches!(
        rows.last(),
        Some(ConversationRow::Activity {
            state: ActivityState::Active,
            ..
        })
    );
    if tail_is_active {
        rows.push(ConversationRow::Working {
            elapsed_ms: None,
            step: None,
        });
    }
```

- [ ] **Step 4: Implement duration attribution**

```rust
/// For each row, the index of the row that opened its elapsed-time window. A
/// user turn opens a boundary; a settled assistant turn closes it. This is the
/// Rust form of T3 Code's `computeMessageDurationStart`.
pub fn duration_boundaries(rows: &[ConversationRow]) -> Vec<Option<usize>> {
    let mut out = Vec::with_capacity(rows.len());
    let mut open: Option<usize> = None;
    for (index, row) in rows.iter().enumerate() {
        match row {
            ConversationRow::Message {
                role: MessageRole::User,
                ..
            } => {
                open = Some(index);
                out.push(open);
            }
            ConversationRow::Message {
                role: MessageRole::Assistant,
                streaming,
                ..
            } => {
                out.push(open.or(Some(index)));
                if !*streaming {
                    open = None;
                }
            }
            _ => out.push(open),
        }
    }
    out
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `CARGO_TARGET_DIR="C:/Temp/devmanager-train1" cargo test --lib conversation::rows`
Expected: PASS, `19 passed`.

- [ ] **Step 6: Commit**

```bash
git add src/ui/conversation/rows.rs
git commit -m "feat(ui): emit turn folds, a working row, and duration boundaries"
```

---

### Task 5: Delete the substring denylist and paint from rows

This is the task that removes the defect. Nothing before it changes user-visible behaviour.

**Files:**
- Modify: `src/ui/task_cockpit/timeline.rs` — delete `conversation_item_visibility` (lines 26-67), `is_hidden_conversation_chrome` (69-78), `is_user_role` (565-569), and `ConversationItemVisibility`.
- Create: `src/ui/conversation/render.rs`
- Modify: `src/ui/conversation/mod.rs`
- Test: inline in `timeline.rs`

**Interfaces:**
- Consumes: `derive_conversation_rows`, `apply_activity_collapse`, `stable_conversation_rows` from Tasks 2-4.
- Produces: `pub fn conversation_row_element(row: &ConversationRow, tokens: ThemeTokens) -> AnyElement` in `render.rs`; `Timeline::rows(&self) -> &[ConversationRow]`.

- [ ] **Step 1: Write the failing test**

Replace the existing `conversation_hides_lifecycle_usage_and_unknown_diagnostic_chrome` test in `timeline.rs` with:

```rust
    #[test]
    fn an_unmapped_kind_produces_no_conversation_row_without_a_denylist() {
        let items = vec![
            generic_item("session_state", GenericStatus::Neutral),
            generic_item("a_kind_invented_after_this_test_was_written", GenericStatus::Neutral),
        ];
        let rows = derive_conversation_rows(&items, ConversationVerbosity::Calm);
        assert!(rows.is_empty());
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_TARGET_DIR="C:/Temp/devmanager-train1" cargo test --lib timeline::tests::an_unmapped_kind`
Expected: FAIL - the unmapped kind still renders, because `conversation_item_visibility` classifies it as `Activity`.

The test is deliberately **behavioural**, not a source-text search. An assertion like `assert!(!include_str!("timeline.rs").contains("is_hidden_conversation_chrome"))` would pass silently the moment the file is renamed or the substring reworded, and would then guard nothing. Assert what the code does, not what its source says.

- [ ] **Step 3: Store rows on the Timeline**

In `src/ui/task_cockpit/timeline.rs`, add to the `Timeline` struct after `items`:

```rust
    rows: Vec<ConversationRow>,
    expanded_activity: Vec<String>,
```

In `Timeline::project`, after `let items = journal.project_items(registry, capabilities)?;`:

```rust
        let rows = apply_activity_collapse(
            derive_conversation_rows(&items, ConversationVerbosity::Calm),
            &[],
        );
```

and add `rows,` and `expanded_activity: Vec::new(),` to the struct literal.

Add an accessor:

```rust
    pub fn rows(&self) -> &[ConversationRow] {
        &self.rows
    }
```

- [ ] **Step 4: Delete the classification code**

Remove from `src/ui/task_cockpit/timeline.rs`:
- `enum ConversationItemVisibility` and `fn conversation_item_visibility` (lines 26-67)
- `fn is_hidden_conversation_chrome` (lines 69-78)
- `fn is_user_role` (lines 565-569)

Change `fn conversation_item_element` to dispatch on rows instead of items, delegating to `render.rs`. Create `src/ui/conversation/render.rs` with:

```rust
//! Target UX painting for derived conversation rows.

use gpui::{div, px, AnyElement, IntoElement, ParentElement, Styled};

use crate::ui::conversation::rows::{
    activity_toggle_label, ActivityState, ConversationRow,
};
use crate::ui::renderers::MessageRole;
use crate::ui::tokens::ThemeTokens;

pub fn conversation_row_element(row: &ConversationRow, tokens: ThemeTokens) -> AnyElement {
    match row {
        ConversationRow::Message { role: MessageRole::User, text, .. } => {
            user_message_element(text.clone(), tokens)
        }
        ConversationRow::Message { role: MessageRole::Reasoning, text, .. } => {
            reasoning_element(text.clone(), tokens)
        }
        ConversationRow::Message { text, .. } => assistant_message_element(text.clone(), tokens),
        ConversationRow::Error { text, .. } => error_element(text.clone(), tokens),
        ConversationRow::Activity { entries, state, .. } => {
            activity_element(entries, *state, tokens)
        }
        ConversationRow::ActivityToggle { hidden, expanded, only_tools, .. } => {
            toggle_element(activity_toggle_label(*hidden, *expanded, *only_tools), tokens)
        }
        ConversationRow::Question { prompt, choices, settled_choice, .. } => {
            question_element(prompt, choices, *settled_choice, tokens)
        }
        ConversationRow::TurnFold { label, expanded, .. } => {
            turn_fold_element(label, *expanded, tokens)
        }
        ConversationRow::Working { elapsed_ms, step } => {
            working_element(*elapsed_ms, step.as_deref(), tokens)
        }
    }
}
```

Port each element body from `src/ui/conversation_preview.rs`, which already implements the Target UX treatments and is committed at `5336cc2`. Keep the module's two GPUI notes: right alignment uses `justify_end()` on a row, and the readable measure needs a definite `w(px(..))`.

- [ ] **Step 5: Run tests to verify they pass**

Run: `CARGO_TARGET_DIR="C:/Temp/devmanager-train1" cargo test --lib timeline`
Expected: PASS.

Then prove the deletion by state rather than by grep: `cargo check` must fail if anything still calls the removed functions. Confirm `is_hidden_conversation_chrome`, `conversation_item_visibility` and `is_user_role` are gone with `grep -n "is_hidden_conversation_chrome\|conversation_item_visibility\|fn is_user_role" src/ui/task_cockpit/timeline.rs`, expecting no output. A compile that succeeds with no callers left is the real evidence.

- [ ] **Step 6: Confirm the whole crate still builds**

Run: `CARGO_TARGET_DIR="C:/Temp/devmanager-train1" cargo check --locked --lib --bins --tests > /tmp/t5.log 2>&1; echo "EXIT=$?"; sed 's/\x1b\[[0-9;]*m//g' /tmp/t5.log | grep -c '^error'`
Expected: `EXIT=0`, error count `0`.

- [ ] **Step 7: Commit**

```bash
git add src/ui/task_cockpit/timeline.rs src/ui/conversation/
git commit -m "fix(ui): delete the conversation substring denylist

Classification now reads the closed row vocabulary. Unmapped kinds produce
no row at all, so a new provider event cannot reappear as a debug card."
```

---

### Task 6: Phase 1 conformance against the PWA derivation

Two derivations exist until the wire change lands. This proves they agree so `timelineModel.ts` can be deleted later with evidence.

**Files:**
- Create: `tests/conversation_conformance.rs`
- Create: `tests/fixtures/conversation/basic-turn.json`
- Create: `tests/fixtures/conversation/activity-grouping.json`
- Create: `tests/fixtures/conversation/lifecycle-noise.json`

**Interfaces:**
- Consumes: `derive_conversation_rows` from Task 2.
- Produces: the fixture corpus, which is authoritative for both derivations.

- [ ] **Step 1: Write the fixture**

`tests/fixtures/conversation/lifecycle-noise.json`:

```json
{
  "schema": "devmanager.conversation.fixture/v1",
  "id": "lifecycle-noise",
  "events": [
    { "kind": "user_message", "text": "find the flake" },
    { "kind": "session_state", "state": "ready" },
    { "kind": "turn_state", "state": "running" },
    { "kind": "usage_observation", "remaining_percent": 82 },
    { "kind": "unknown_provider_event", "source_type": "provider_v9_frame" },
    { "kind": "assistant_text", "text": "two share a cause" }
  ],
  "expected_rows": [
    { "kind": "message", "role": "user", "text": "find the flake" },
    { "kind": "message", "role": "assistant", "text": "two share a cause" }
  ]
}
```

Write `basic-turn.json` and `activity-grouping.json` in the same shape, covering a plain user/assistant exchange and three consecutive tools collapsing into one activity row with a toggle.

- [ ] **Step 2: Write the failing test**

`tests/conversation_conformance.rs`:

```rust
//! Phase 1 conformance. Two conversation derivations exist while the PWA
//! still derives its own rows in TypeScript; this proves they agree over a
//! shared corpus so `timelineModel.ts` can be deleted with evidence.

use std::path::PathBuf;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/conversation")
}

#[test]
fn every_fixture_derives_its_expected_rows() {
    let mut executed = 0usize;
    for entry in std::fs::read_dir(fixture_dir()).expect("fixture directory") {
        let path = entry.expect("fixture entry").path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        assert_conversation_fixture(&path);
        executed += 1;
    }
    // A harness that skipped and a harness that passed are the same exit code.
    assert!(
        executed >= 3,
        "expected at least 3 conversation fixtures, executed {executed}"
    );
}
```

Implement `assert_conversation_fixture` to parse the fixture, build `TimelineItemModel` values from `events`, run `derive_conversation_rows` plus `apply_activity_collapse`, and compare against `expected_rows`.

- [ ] **Step 3: Run test to verify it fails**

Run: `CARGO_TARGET_DIR="C:/Temp/devmanager-train1" cargo test --test conversation_conformance`
Expected: FAIL — `assert_conversation_fixture` not defined.

- [ ] **Step 4: Implement the comparison**

```rust
#[derive(serde::Deserialize)]
struct ConversationFixture {
    schema: String,
    id: String,
    events: Vec<FixtureEvent>,
    expected_rows: Vec<ExpectedRow>,
}

#[derive(serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum FixtureEvent {
    UserMessage { text: String },
    AssistantText { text: String },
    ToolCall { tool_id: String, name: String, state: String },
    SessionState { state: String },
    TurnState { state: String },
    UsageObservation { remaining_percent: u8 },
    UnknownProviderEvent { source_type: String },
}

#[derive(serde::Deserialize, Debug, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ExpectedRow {
    Message { role: String, text: String },
    Activity { entries: usize, state: String },
    ActivityToggle { hidden: usize },
}

fn assert_conversation_fixture(path: &std::path::Path) {
    let bytes = std::fs::read(path).expect("fixture bytes");
    let fixture: ConversationFixture =
        serde_json::from_slice(&bytes).expect("fixture parses");
    assert_eq!(
        fixture.schema, "devmanager.conversation.fixture/v1",
        "{} has an unexpected schema", fixture.id
    );

    let items: Vec<TimelineItemModel> = fixture.events.iter().map(item_for_event).collect();
    let rows = apply_activity_collapse(
        derive_conversation_rows(&items, ConversationVerbosity::Calm),
        &[],
    );

    let actual: Vec<ExpectedRow> = rows.iter().filter_map(summarize_row).collect();
    assert_eq!(
        actual, fixture.expected_rows,
        "fixture {} derived the wrong rows", fixture.id
    );
}

fn summarize_row(row: &ConversationRow) -> Option<ExpectedRow> {
    match row {
        ConversationRow::Message { role, text, .. } => Some(ExpectedRow::Message {
            role: match role {
                MessageRole::User => "user".to_string(),
                MessageRole::Assistant => "assistant".to_string(),
                MessageRole::Reasoning => "reasoning".to_string(),
                MessageRole::Error => "error".to_string(),
            },
            text: text.clone(),
        }),
        ConversationRow::Activity { entries, state, .. } => Some(ExpectedRow::Activity {
            entries: entries.len(),
            state: format!("{state:?}").to_lowercase(),
        }),
        ConversationRow::ActivityToggle { hidden, .. } => {
            Some(ExpectedRow::ActivityToggle { hidden: *hidden })
        }
        _ => None,
    }
}
```

Write `item_for_event` by reusing the shared `fixtures` module from Task 2 Step 4. A `session_state`, `turn_state`, `usage_observation` or `unknown_provider_event` fixture event must produce a `TimelineItemContent::Generic` item, which is precisely what must yield no row.

- [ ] **Step 5: Run to verify it passes**

Run: `CARGO_TARGET_DIR="C:/Temp/devmanager-train1" cargo test --test conversation_conformance`
Expected: PASS, and the output must show a non-zero executed count.

- [ ] **Step 6: Sabotage-test the harness**

Temporarily add `{ "kind": "session_state", "state": "ready" }` to `lifecycle-noise.json`'s `expected_rows`. Re-run. The test MUST fail. Revert the edit and confirm it passes again. A conformance test that cannot go red is not a conformance test.

- [ ] **Step 7: Commit**

```bash
git add tests/conversation_conformance.rs tests/fixtures/conversation/
git commit -m "test(ui): pin Rust and PWA conversation derivations to one corpus"
```

---

# Train 2 — Target UX

### Task 6b: RESOLVED - no turn id is persisted; schema work moved to its own spec

**Investigated and closed without code changes.** No per-turn identifier exists anywhere in the durable journal: not in the `semantic_journal_facts` schema, `SemanticJournalFactRow`, `SemanticJournalFact`, any `SemanticJournalPayload` variant, or the `deny_unknown_fields` wire deserializer at `src/providers/journal.rs:821-865`. A real provider-native turn id IS read from Claude and Codex hooks (`src/ai/claude_hooks.rs:585-589`, `src/ai/codex_hooks.rs:208`) but is used as a deduplication key and discarded before the journal.

`ConversationRow::TurnFold` therefore stays in place and stays inert on this branch. Persisting the identifier is a durable schema change and breaks this plan's Global Constraint that journal storage is unchanged, so it lives in its own spec: `docs/superpowers/specs/2026-08-20-persist-provider-turn-id-design.md`. Turn folds begin appearing when that lands; nothing on this branch depends on them.

**Do not synthesize a turn id** from sequence numbers, timestamps, ordering, or message ids to make folds appear sooner. The project's invariants forbid inferring conversation identity that way.

### Task 6b (superseded): Populate `turn_id` so turn folds are not inert

**Discovered during Task 4b review, not in the original plan.** `src/ui/renderers/journal_view.rs:365` sets `turn_id: None` unconditionally on every projected item. Task 4b's `TurnFold` emission is therefore correct and completely inert: no production item ever carries a turn id, so no fold is ever produced, and the Target UX's turn-fold affordance cannot appear.

This is a plan gap, not a defect in Task 4b, which implemented its brief faithfully.

**Files:**
- Modify: `src/ui/renderers/journal_view.rs:365`
- Test: inline

**Two things this task must establish, in order:**

1. **Whether a turn identifier exists to populate from at all.** Read `SemanticJournalFactRow` and the journal schema. If no per-turn identifier is persisted, STOP and report that — the honest outcome is then to remove `TurnFold` from `ConversationRow` rather than ship a variant nothing can construct, and that is a decision for the plan owner, not something to work around by synthesising an id. **Never synthesize a turn identifier**; the project's invariants forbid inferring conversation identity from ordering or timestamps.
2. If an identifier does exist, thread it into `TimelineItemModel.turn_id` and add a test proving a projected item carries it, plus an end-to-end test proving `derive_conversation_rows` emits a fold at a real turn boundary rather than only for hand-built fixtures.

Note that the journal is read `ORDER BY sequence ASC` (four sites in `src/kernel/semantic_journal.rs`) and `project_items` preserves that order, so turn ids cannot revisit a value within one list. `ConversationRowKey::TurnFold` keying on the raw string is therefore safe.

---

### Task 7: Row treatments and geometry

**Files:**
- Modify: `src/ui/conversation/render.rs`
- Test: inline

**Interfaces:**
- Consumes: `conversation_row_element` from Task 5.
- Produces: the Target UX constants, exported for tests.

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_conversation_row_renderer_draws_a_border() {
        // KNOWN LIMITATION: this is a source-text assertion, and source-text
        // assertions decay silently. GPUI offers no way to inspect a painted
        // element's computed style from a unit test, so there is no behavioural
        // equivalent available today. Two mitigations, both required:
        //   1. `border_b` for the turn fold is counted separately below, so the
        //      test cannot pass merely because the anchor stopped matching.
        //   2. The sabotage check in Step 4 proves the anchor still matches.
        // Re-anchor this on the real element tree if a render harness lands.
        let source = include_str!("render.rs");
        let renderers = source
            .split("#[cfg(test)]")
            .next()
            .expect("renderer source precedes its tests");
        assert_eq!(
            renderers.matches(".border(px(").count(),
            0,
            "conversation rows are separated by whitespace and surface \
             lightness, never by borders"
        );
        assert_eq!(
            renderers.matches(".border_b(px(").count(),
            1,
            "exactly one hairline exists, the turn fold's -- if this is 0 the \
             anchor above has stopped matching and is guarding nothing"
        );
    }

    #[test]
    fn the_user_bubble_caps_at_eighty_percent_of_the_measure() {
        assert!((USER_BUBBLE_FRACTION - 0.80).abs() < f32::EPSILON);
    }

    #[test]
    fn the_readable_measure_matches_the_t3_reference() {
        assert_eq!(CONVERSATION_CONTENT_MAX_WIDTH, 768.0);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_TARGET_DIR="C:/Temp/devmanager-train2" cargo test --lib conversation::render`
Expected: FAIL — `border_calls` is non-zero because the ported `activity_card_element` still draws a border.

- [ ] **Step 3: Remove every border from the row renderers**

Port the treatments exactly as the prototype implements them: user bubble at 16 px radius and 12 px padding on `surfaces.raised`, capped at 80 % of the measure and right-aligned with `justify_end()`; assistant rows at `px(4) py(2)` with no surface, no border and no role marker; work rows at 6 px radius with only the heading recolouring to `status.warning` or `status.destructive`; the turn fold as the single `border_b` hairline.

- [ ] **Step 4: Sabotage-test the border guard**

The border test is source-anchored, so prove the anchor matches before trusting it. Temporarily add `.border(px(1.0))` to any row renderer and re-run: the test MUST fail. Remove it and confirm the test passes again. An absence assertion nobody has seen go red is not a guard.

Run: `CARGO_TARGET_DIR="C:/Temp/devmanager-train2" cargo test --lib conversation::render`
Expected: FAIL while sabotaged, PASS once reverted.

- [ ] **Step 5: Run tests to verify they pass**

Run: `CARGO_TARGET_DIR="C:/Temp/devmanager-train2" cargo test --lib conversation::render`
Expected: PASS, `3 passed`.

- [ ] **Step 6: Commit**

```bash
git add src/ui/conversation/render.rs
git commit -m "feat(ui): apply target row treatments to the conversation canvas"
```

---

### Task 8: The 40 px follow re-arm band

**Files:**
- Modify: `src/ui/task_cockpit/timeline.rs:329-348`
- Modify: `src/ui/task_cockpit/shell.rs` — preserve the reader's follow/anchor state across fresh projections
- Test: inline

**Interfaces:**
- Consumes: the existing `Timeline` scroll state.
- Produces: `pub const FOLLOW_REARM_THRESHOLD_PX: u32 = 40;`

T3 Code's own comment records why this exists: a half-viewport "near end" test re-armed live-follow while the user was reading history and yanked them back down on the next stream chunk. DevManager's current `at_bottom` uses an exact-bottom test and will reproduce that.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn follow_rearms_within_a_pixel_band_not_only_at_the_exact_bottom() {
        let mut timeline = timeline_with_content_height(2_000, 400);
        timeline.scroll_to_offset(2_000 - 400 - 20); // 20 px from the bottom
        assert!(
            timeline.at_bottom(),
            "a 20 px gap is inside the {FOLLOW_REARM_THRESHOLD_PX} px re-arm band"
        );
    }

    #[test]
    fn follow_does_not_rearm_while_reading_history() {
        let mut timeline = timeline_with_content_height(2_000, 400);
        timeline.scroll_to_offset(400); // well up the transcript
        assert!(!timeline.at_bottom());
        assert!(!timeline.follow_latest());
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_TARGET_DIR="C:/Temp/devmanager-train2" cargo test --lib timeline::tests::follow_rearms`
Expected: FAIL — the exact-bottom test returns false at a 20 px gap.

- [ ] **Step 3: Implement**

```rust
/// Follow re-arm band above the true content bottom. Strict on purpose: a
/// half-viewport "near end" test re-arms live-follow while the user is reading
/// history and yanks them back down on the next streamed chunk.
pub const FOLLOW_REARM_THRESHOLD_PX: u32 = 40;
```

Change `at_bottom` to compare the remaining distance against `FOLLOW_REARM_THRESHOLD_PX` rather than testing for an exact bottom offset.

- [ ] **Step 4: Run tests to verify they pass**

Run: `CARGO_TARGET_DIR="C:/Temp/devmanager-train2" cargo test --lib timeline`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/ui/task_cockpit/timeline.rs
git commit -m "fix(ui): re-arm live follow within a 40px band above the bottom"
```

---

### Task 9: Hover-revealed row chrome

**Files:**
- Modify: `src/ui/conversation/render.rs`
- Test: inline

**Interfaces:**
- Consumes: `conversation_row_element` from Task 5.
- Produces: no new public surface; the meta row becomes hover-gated.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn message_meta_is_invisible_at_rest_and_visible_when_revealed() {
        assert_eq!(message_meta_opacity(false), 0.0);
        assert_eq!(message_meta_opacity(true), 1.0);
    }
```

The same helper must feed the resting style, named-group hover style, and focus style. Do not replace this with a grep for a symbol name; that would pass whether or not the runtime styles consume it.

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_TARGET_DIR="C:/Temp/devmanager-train2" cargo test --lib conversation::render::tests::message_meta_is_invisible`
Expected: FAIL - `message_meta_opacity` does not exist.

- [ ] **Step 3: Implement**

**Correction after checking the pinned GPUI and Zed source.** GPUI 0.2.2 does have named group hover (`.group(..)` / `.group_hover(..)`), and Zed's `VisibleOnHover` trait uses it for exactly this dense-row action pattern. The earlier entity-state direction was based on an incomplete local search and is superseded. Keep the meta row at opacity zero, reveal it from the exact named ancestor group, and make the meta row a tab stop whose focus style also raises opacity. This avoids adding mutable hover state to the pure `Timeline` projection.

Use one shared opacity function for the resting and revealed styles so its contract is directly testable:

```rust
fn message_meta_opacity(revealed: bool) -> f32 {
    if revealed { 1.0 } else { 0.0 }
}
```

The painter uses `message_meta_opacity(false)` at rest and `message_meta_opacity(true)` in both `group_hover` and `focus`, so keyboard focus remains a first-class reveal path.

- [ ] **Step 4: Run tests to verify they pass**

Run: `CARGO_TARGET_DIR="C:/Temp/devmanager-train2" cargo test --lib conversation::render`
Expected: PASS.

- [ ] **Step 5: Verify in the live app**

Run the watcher: `powershell -ExecutionPolicy Bypass -File .\dev-watch.ps1`
Confirm by eye that timestamps and controls are absent at rest and fade in on hover, and that keyboard focus reveals them too.

- [ ] **Step 6: Commit**

```bash
git add src/ui/conversation/render.rs
git commit -m "feat(ui): reveal message chrome on hover and focus only"
```

---

### Task 10: Delete the prototype and run the full gates

**Files:**
- Delete: `src/ui/conversation_preview.rs`
- Delete: `tests/fixtures/ui/conversation.json`
- Modify: `src/ui/mod.rs`, `src/ui/preview.rs`, `src/main.rs`
- Modify: `src/ui/native_shell.rs`, `src/ui/tokens.rs` — promote the prototype's joined composer/context-strip shell into the real interactive composer

- [ ] **Step 1: Remove the prototype**

Delete the module and its fixture. Revert the three `src/ui/preview.rs` edits that added the `"conversation"` root kind, and remove the `--ui-proto` flag and `run_ui_proto` from `src/main.rs`. The prototype has served its purpose once `render.rs` carries the treatments.

- [ ] **Step 2: Confirm nothing still references it**

Run: `grep -rn "conversation_preview\|ui-proto" src/ tests/ || echo "clean"`
Expected: `clean`.

- [ ] **Step 3: Run the full library suite**

Run: `CARGO_TARGET_DIR="C:/Temp/devmanager-train2" cargo test --lib -- --test-threads=1 > /tmp/full.log 2>&1; echo "EXIT=$?"; sed 's/\x1b\[[0-9;]*m//g' /tmp/full.log | grep "test result"`
Expected: `EXIT=0` and zero failures. Read the `EXIT=` line from the file; a backgrounded run's task notification reports the wrapper's status, not the suite's.

- [ ] **Step 4: Run the integration gate**

Run: `CARGO_TARGET_DIR="C:/Temp/devmanager-train2" cargo check --locked --lib --bins --tests > /tmp/gate.log 2>&1; echo "EXIT=$?"; sed 's/\x1b\[[0-9;]*m//g' /tmp/gate.log | grep -c '^error'`
Expected: `EXIT=0`, error count `0`.

- [ ] **Step 5: Live visual pass**

Run the watcher and compare the conversation surface side by side with a running T3 Code window. Check the acceptance criteria in the spec that a test cannot cover: rows carry no borders, the assistant turn has no marker, chrome appears only on hover, the column measures 768 px, and the composer and its context strip read as one surface. Record the composer approximation verdict per acceptance criterion 21.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "chore(ui): remove the throwaway conversation look prototype"
```

#### Execution closeout (2026-08-23)

- Tasks 1-10 are implemented in the real native conversation and composer surfaces; the throwaway prototype and fixture are removed. The final reference-parity pass also replaced the legacy split cockpit chrome with the T3-style full-height task rail, compact workspace top bar, and full-bleed conversation canvas.
- Verification passed in the isolated `C:\Temp\devmanager-conversation-derivation` target: the serial Rust library suite reported 2,755 passed, 0 failed, and 2 ignored; the standalone conversation conformance harness reported 1 passed and 0 failed; the UI token harness reported 20 passed and 0 failed; both reference-shell projection tests passed independently; and `cargo check --locked --lib --bins --tests` exited 0.
- Code-level geometry and treatment review confirms a 256 px full-height sidebar, 78 px borderless task rows, a 44 px workspace top bar, a bare assistant turn, a 768 px centered conversation/composer column, named-group hover/focus chrome, and a joined composer/context strip. Existing question, approval, attachment, provider, checkout, branch, terminal, dock, and task actions remain wired to their production handlers.
- Native pixels are verified through the real isolated host and canonical `NativeShell`: `conversation-target-ux-reference-1912-20260823-r2.png` is a 1912x1200 full-shell capture showing the plum canvas, navy selected task, flat task hierarchy, compact workspace chrome, connected Codex composer, and joined checkout/branch strip after bounded asynchronous settle. The isolated selected task has no durable transcript rows, so the proof exercises the production shell/composer composition without fabricating conversation data. Hover/focus behavior is verified by the GPUI named-group/focus implementation and focused tests. No production profile or installed process was used as a fallback.

#### Independent panel-control follow-up (2026-08-23)

- The workspace header now keeps a small explicit `Conversation | Terminal` center-canvas switch independent from the slim right-panel toggle. The panel toggle ports T3 Code's quiet `PanelRightIcon` treatment and expands the existing production Changes, Files, Browser, Services, Artifacts, and Review dock without replacing its state, shortcuts, width, or handlers.
- Full-shell acceptance is recorded in `conversation-panels-collapsed-1912-20260823.png` and `conversation-browser-expanded-1912-20260823.png`, both captured at 1912x1200 through the canonical isolated preview. The complete serial Rust library suite passed with 2,755 passed, 0 failed, and 2 ignored; `cargo check --locked --lib --bins --tests` exited 0.

---

## Deferred to later trains

These are in the spec but out of scope for trains 1 and 2, and each needs its own plan:

- **Train 3 — Catalog ownership.** Replace the `builtinCatalog.ts` scrape at `src/ui/task_cockpit/composer.rs:174` and `:237` with a Rust const table generated into TypeScript by `build.rs`. Independent of everything here; can land at any time and closes a live silent-failure path.
- **Train 4 — Composer derivation.** `src/ui/composer/segments.rs` and `trigger.rs`: the closed `PromptSegment` vocabulary, the dual expanded/collapsed cursor model, and `@` / `$` / `/` trigger detection. Required before the composer placeholder's promise of `@tag files/folders` and `$use skills` is honest.
- **Train 5 — PWA cutover and file reduction.** Host emits derived rows on the wire, the PWA renders them, `web/src/tasks/timeline/timelineModel.ts` and the Task 6 conformance test are deleted, and `timeline.rs` and `composer.rs` are reduced to painting.
