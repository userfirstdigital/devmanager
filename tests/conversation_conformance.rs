//! Phase 1 conformance. Two conversation derivations exist while the PWA
//! still derives its own rows in TypeScript; this proves they agree over a
//! shared corpus so `timelineModel.ts` can be deleted with evidence.
//!
//! `crate::ui::conversation::fixtures` (Task 2) cannot be reused here: it is
//! declared `#[cfg(test)] pub(crate) mod fixtures;`, so it is neither
//! compiled into the rlib this integration test links against nor nameable
//! from outside the crate even when it is. The item builders below
//! deliberately mirror its construction logic instead (same fields, same
//! defaults) rather than importing it -- see the task report for detail.

use std::path::PathBuf;

use devmanager::domain::id::{EventId, TaskId};
use devmanager::ui::components::interaction::{AccessibilityMetadata, AccessibleRole};
use devmanager::ui::conversation::rows::{
    apply_activity_collapse, derive_conversation_rows, ConversationRow, ConversationVerbosity,
};
use devmanager::ui::renderers::{
    GenericSemanticCard, GenericStatus, InteractionEligibility, MarkdownBlock, MarkdownDocument,
    MessageRole, MessageView, ProviderKind, RendererSelection, SemanticKind, TimelineItemContent,
    TimelineItemId, TimelineItemModel, ToolView,
};

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
    UserMessage {
        text: String,
    },
    AssistantText {
        text: String,
    },
    ToolCall {
        tool_id: String,
        name: String,
        state: String,
    },
    SessionState {
        state: String,
    },
    TurnState {
        state: String,
    },
    UsageObservation {
        remaining_percent: u8,
    },
    UnknownProviderEvent {
        source_type: String,
    },
}

#[derive(serde::Deserialize, Debug, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ExpectedRow {
    Message { role: String, text: String },
    Activity { entries: usize, state: String },
    ActivityToggle { hidden: usize },
    Question { prompt: String },
    Error { text: String },
    TurnFold { turn_id: String },
    Working,
}

fn assert_conversation_fixture(path: &std::path::Path) {
    let bytes = std::fs::read(path).expect("fixture bytes");
    let fixture: ConversationFixture = serde_json::from_slice(&bytes).expect("fixture parses");
    assert_eq!(
        fixture.schema, "devmanager.conversation.fixture/v1",
        "{} has an unexpected schema",
        fixture.id
    );

    let items: Vec<TimelineItemModel> = fixture.events.iter().map(item_for_event).collect();
    let rows = apply_activity_collapse(
        derive_conversation_rows(&items, ConversationVerbosity::Calm),
        &[],
    );

    let actual: Vec<ExpectedRow> = rows.iter().map(summarize_row).collect();
    assert_eq!(
        actual, fixture.expected_rows,
        "fixture {} derived the wrong rows",
        fixture.id
    );
}

fn summarize_row(row: &ConversationRow) -> ExpectedRow {
    // Total over `ConversationRow`: a spurious row of any of the four kinds
    // that never appear in this corpus (Question, Error, TurnFold, Working)
    // must still be REPORTED by the comparison, not silently dropped. Adding
    // a variant to `ConversationRow` is a compile error here, matching the
    // wildcard-free matches in rows.rs itself.
    match row {
        ConversationRow::Message { role, text, .. } => ExpectedRow::Message {
            role: match role {
                MessageRole::User => "user".to_string(),
                MessageRole::Assistant => "assistant".to_string(),
                MessageRole::Reasoning => "reasoning".to_string(),
                MessageRole::Error => "error".to_string(),
            },
            text: text.clone(),
        },
        ConversationRow::Activity { entries, state, .. } => ExpectedRow::Activity {
            entries: entries.len(),
            state: format!("{state:?}").to_lowercase(),
        },
        ConversationRow::ActivityToggle { hidden, .. } => {
            ExpectedRow::ActivityToggle { hidden: *hidden }
        }
        ConversationRow::Question { prompt, .. } => ExpectedRow::Question {
            prompt: prompt.clone(),
        },
        ConversationRow::Error { text, .. } => ExpectedRow::Error { text: text.clone() },
        ConversationRow::TurnFold { turn_id, .. } => ExpectedRow::TurnFold {
            turn_id: turn_id.clone(),
        },
        ConversationRow::Working { .. } => ExpectedRow::Working,
    }
}

// -- item construction --------------------------------------------------
//
// Mirrors `src/ui/conversation/fixtures.rs` (Task 2), which this test
// cannot import -- see the module doc comment above. No `output` fixture
// event exists here by design (constraint 1: the Rust `ConversationRow`
// vocabulary has no fallback-output variant to construct one from) and no
// event here carries a `turn_id` (constraint 2: no per-turn identifier is
// persisted in the journal today, per Task 6b's finding, so a fixture that
// expected a `TurnFold` would pin a state production never reaches).

fn item_for_event(event: &FixtureEvent) -> TimelineItemModel {
    match event {
        FixtureEvent::UserMessage { text } => message_item(MessageRole::User, text),
        FixtureEvent::AssistantText { text } => message_item(MessageRole::Assistant, text),
        FixtureEvent::ToolCall {
            tool_id,
            name,
            state,
        } => tool_item(tool_id, name, state),
        FixtureEvent::SessionState { .. } => generic_item("session_state"),
        FixtureEvent::TurnState { .. } => generic_item("turn_state"),
        FixtureEvent::UsageObservation { .. } => generic_item("usage_observation"),
        FixtureEvent::UnknownProviderEvent { source_type } => generic_item(source_type),
    }
}

fn role_label(role_kind: MessageRole) -> &'static str {
    match role_kind {
        MessageRole::User => "You",
        MessageRole::Assistant => "Assistant",
        MessageRole::Reasoning => "Reasoning",
        MessageRole::Error => "Error",
    }
}

fn message_item(role_kind: MessageRole, text: &str) -> TimelineItemModel {
    let role = role_label(role_kind);
    TimelineItemModel {
        id: TimelineItemId::Event(EventId::new()),
        task_id: TaskId::new(),
        renderer_selection: RendererSelection::Specialized(SemanticKind::Message),
        interaction: InteractionEligibility::None,
        content: TimelineItemContent::Message(MessageView {
            role: role.to_string(),
            role_kind,
            occurred_at_ms: None,
            streaming: false,
            markdown: MarkdownDocument {
                selectable: true,
                copyable: true,
                html_executed: false,
                prose_wraps: true,
                blocks: vec![MarkdownBlock::Paragraph {
                    text: text.to_string(),
                }],
                pending_links: Vec::new(),
            },
        }),
        activated_on_enter: false,
        accessibility: AccessibilityMetadata::new(AccessibleRole::Region, role)
            .expect("accessible name"),
        turn_id: None,
        related_event_id: None,
    }
}

fn tool_item(tool_id: &str, name: &str, state: &str) -> TimelineItemModel {
    TimelineItemModel {
        id: TimelineItemId::Event(EventId::new()),
        task_id: TaskId::new(),
        renderer_selection: RendererSelection::Specialized(SemanticKind::Tool),
        interaction: InteractionEligibility::None,
        content: TimelineItemContent::Tool(ToolView {
            tool_id: tool_id.to_string(),
            name: name.to_string(),
            state: state.to_string(),
            summary: String::new(),
            provider_specific: false,
        }),
        activated_on_enter: false,
        accessibility: AccessibilityMetadata::new(AccessibleRole::Status, name)
            .expect("accessible name"),
        turn_id: None,
        related_event_id: None,
    }
}

fn generic_item(source_type: &str) -> TimelineItemModel {
    let event_id = EventId::new();
    TimelineItemModel {
        id: TimelineItemId::Event(event_id),
        task_id: TaskId::new(),
        renderer_selection: RendererSelection::GenericFallback,
        interaction: InteractionEligibility::None,
        content: TimelineItemContent::Generic(GenericSemanticCard {
            event_id,
            provider: ProviderKind::parse("claude").expect("provider"),
            source_type: source_type.to_string(),
            schema_version: 1,
            status: GenericStatus::Unknown,
            title: source_type.to_string(),
            redacted_fields: Vec::new(),
            raw_terminal_available: false,
        }),
        activated_on_enter: false,
        accessibility: AccessibilityMetadata::new(AccessibleRole::Status, source_type)
            .expect("accessible name"),
        turn_id: None,
        related_event_id: None,
    }
}
