use super::*;
use crate::domain::{
    apply, AgentSessionId, DomainEvent, Event, EventId, PageLimits, PrivacyClass, ResourceId,
    TaskId, TaskSnapshot,
};
use crate::protocol::{
    query_semantic_journal_page, semantic_journal_query_available, FrameLimits, MessagePackCodec,
    SemanticJournalPage,
};
use rusqlite::Connection;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const NOW_MS: i64 = 1_725_000_002_000;

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/providers/journal")
        .join(name)
}

fn load_bytes(name: &str) -> Vec<u8> {
    std::fs::read(fixture_path(name)).expect("read journal fixture")
}

fn fixture_at(name: &str, occurred_at_ms: i64) -> Vec<u8> {
    let mut body = String::from_utf8(load_bytes(name)).expect("fixture utf8");
    let marker = "\"occurred_at_ms\":";
    let marker_start = body.find(marker).expect("timestamp marker") + marker.len();
    let start = marker_start
        + body[marker_start..]
            .find(|ch: char| !ch.is_whitespace())
            .expect("timestamp value");
    let end = body[start..]
        .find(|ch: char| !ch.is_ascii_digit() && ch != '-')
        .map(|offset| start + offset)
        .expect("timestamp terminator");
    body.replace_range(start..end, &occurred_at_ms.to_string());
    body.into_bytes()
}

fn ids() -> (TaskId, AgentSessionId, ResourceId) {
    (
        TaskId::parse("018f60b0-9c1a-7001-8000-00000000000b").expect("task"),
        AgentSessionId::parse("018f60b0-9c1a-7001-8000-000000000021").expect("agent"),
        ResourceId::parse("018f60b0-9c1a-7001-8000-000000000057").expect("resource"),
    )
}

fn test_permit(provider: ProviderKind, delivery: &str) -> AdapterDeliveryPermit {
    let (task, agent, resource) = ids();
    AdapterDeliveryPermit::issue_for_test(
        provider,
        task,
        agent,
        resource,
        1,
        1,
        [0x11; 32],
        delivery,
        NOW_MS - 1_000,
        NOW_MS + 60_000,
    )
    .expect("test permit")
}

fn open_on(path: &Path, provider: ProviderKind) -> SemanticJournal {
    SemanticJournal::open(
        path,
        &test_permit(provider, "session_open"),
        JournalLimits::default(),
        NOW_MS,
    )
    .expect("open journal")
}

fn temp_journal(provider: ProviderKind) -> (TempDir, PathBuf, SemanticJournal) {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("kernel.sqlite3");
    let journal = open_on(&path, provider);
    (dir, path, journal)
}

fn ingest_named(
    journal: &mut SemanticJournal,
    provider: ProviderKind,
    delivery: &str,
    fixture: &str,
) -> JournalIngestOutcome {
    journal.ingest(
        test_permit(provider, delivery),
        &load_bytes(fixture),
        NOW_MS,
    )
}

#[test]
fn journal_stock_adapter_ingress_is_explicitly_unavailable() {
    assert!(!stock_adapter_ingress_available());
    assert!(stock_adapter_ingress().is_err());
}

#[test]
fn journal_protocol_query_is_explicitly_capability_unavailable() {
    assert!(!semantic_journal_query_available());
    assert!(
        query_semantic_journal_page(0, PageLimits::new(16, 8 * 1024).expect("limits")).is_err()
    );
}

#[test]
fn journal_normalizes_claude_codex_cursor_content_fixtures_with_trusted_permit() {
    let (_dir, _path, mut journal) = temp_journal(ProviderKind::ClaudeCode);
    let claude = ingest_named(
        &mut journal,
        ProviderKind::ClaudeCode,
        "relay_claude_user_1",
        "claude_user_message.json",
    );
    let tool = ingest_named(
        &mut journal,
        ProviderKind::ClaudeCode,
        "relay_claude_tool_1",
        "claude_tool_call.json",
    );

    let (_dir, _path, mut journal) = temp_journal(ProviderKind::Codex);
    let assistant = ingest_named(
        &mut journal,
        ProviderKind::Codex,
        "relay_codex_asst_1",
        "codex_assistant_text.json",
    );
    let approval = ingest_named(
        &mut journal,
        ProviderKind::Codex,
        "relay_codex_approval_1",
        "codex_approval_request.json",
    );

    let (_dir, _path, mut journal) = temp_journal(ProviderKind::Cursor);
    let session = ingest_named(
        &mut journal,
        ProviderKind::Cursor,
        "relay_cursor_session_1",
        "cursor_session_state.json",
    );
    let usage = ingest_named(
        &mut journal,
        ProviderKind::Cursor,
        "relay_cursor_usage_1",
        "cursor_usage.json",
    );

    assert_eq!(
        claude.accepted().expect("claude").kind(),
        JournalSemanticKind::UserMessage
    );
    assert_eq!(
        tool.accepted().expect("tool").kind(),
        JournalSemanticKind::ToolCall
    );
    let tool_event = tool.accepted().expect("tool event");
    assert!(matches!(
        tool_event.to_snapshot_fact().payload,
        SemanticJournalPayload::ToolCall {
            tool_name,
            call_id
        } if tool_name == "Read" && call_id == "toolu_claude_1"
    ));
    assert_eq!(
        assistant.accepted().expect("asst").kind(),
        JournalSemanticKind::AssistantText
    );
    assert_eq!(
        approval.accepted().expect("approval").kind(),
        JournalSemanticKind::ApprovalRequest
    );
    assert_eq!(
        session.accepted().expect("session").kind(),
        JournalSemanticKind::SessionState
    );
    assert_eq!(
        usage.accepted().expect("usage").kind(),
        JournalSemanticKind::UsageObservation
    );
    assert_eq!(
        claude.accepted().expect("claude").schema_version(),
        JOURNAL_SCHEMA_VERSION
    );
}

#[test]
fn journal_preserves_allowlisted_extension_metadata() {
    let (_dir, _path, mut journal) = temp_journal(ProviderKind::ClaudeCode);
    let event = ingest_named(
        &mut journal,
        ProviderKind::ClaudeCode,
        "relay_claude_user_1",
        "claude_user_message.json",
    )
    .accepted()
    .expect("accepted");
    assert_eq!(
        event.extension("hook_event_name").map(str::to_owned),
        Some("UserPromptSubmit".into())
    );
}

#[test]
fn journal_malformed_after_foreign_permit_stays_on_delivery_binding() {
    let (task, agent, resource) = ids();
    let (_dir, _path, mut journal) = temp_journal(ProviderKind::ClaudeCode);
    ingest_named(
        &mut journal,
        ProviderKind::ClaudeCode,
        "relay_claude_user_1",
        "claude_user_message.json",
    );
    let foreign = AdapterDeliveryPermit::issue_for_test(
        ProviderKind::Codex,
        TaskId::parse("018f60b0-9c1a-7001-8000-000000000099").expect("foreign task"),
        agent,
        resource,
        2,
        1,
        [0x22; 32],
        "relay_foreign_malformed",
        NOW_MS - 1_000,
        NOW_MS + 60_000,
    )
    .expect("foreign permit");
    let outcome = journal.ingest(foreign, &load_bytes("malformed_not_object.json"), NOW_MS);
    assert!(
        matches!(
            outcome,
            JournalIngestOutcome::Rejected(JournalRejectReason::Foreign)
        ),
        "foreign permit must fail closed, got {outcome:?}"
    );
    assert_eq!(journal.retained_len().expect("retained"), 1);
    let kept = journal.event_at(1).expect("load").expect("first event");
    assert_eq!(kept.task_id(), task);
    assert_ne!(kept.delivery_id(), "relay_foreign_malformed");
}

#[test]
fn journal_malformed_with_trusted_permit_is_delivery_scoped_diagnostic() {
    let (_dir, _path, mut journal) = temp_journal(ProviderKind::ClaudeCode);
    ingest_named(
        &mut journal,
        ProviderKind::ClaudeCode,
        "relay_claude_user_1",
        "claude_user_message.json",
    );
    let quarantined = journal
        .ingest(
            test_permit(ProviderKind::ClaudeCode, "relay_malformed_only"),
            &load_bytes("malformed_not_object.json"),
            NOW_MS,
        )
        .quarantined()
        .expect("diagnostic");
    assert_eq!(
        quarantined.kind(),
        JournalSemanticKind::UnknownProviderEvent
    );
    assert_eq!(quarantined.delivery_id(), "relay_malformed_only");
    assert_eq!(quarantined.task_id(), ids().0);
    assert_eq!(quarantined.runtime_generation(), 1);
    assert!(quarantined.unknown_provider_event().is_some());
    assert!(quarantined.projected_text().is_none());
}

#[test]
fn journal_future_diagnostic_retains_schema_source_and_provider_metadata() {
    let (_dir, _path, mut journal) = temp_journal(ProviderKind::ClaudeCode);
    let body = br#"{"schema_version":99,"source_type":"future.hook.v9","payload":{"kind":"new_semantic"}}"#;
    let event = journal
        .ingest(
            test_permit(ProviderKind::ClaudeCode, "relay_future_schema"),
            body,
            NOW_MS,
        )
        .quarantined()
        .expect("future diagnostic");
    let unknown = event.unknown_provider_event().expect("unknown metadata");
    assert_eq!(unknown.provider(), ProviderKind::ClaudeCode);
    assert_eq!(unknown.source_type(), "future.hook.v9");
    assert_eq!(unknown.schema_version(), 99);
    let restored = journal
        .event_at(event.sequence())
        .expect("restore")
        .expect("event");
    assert_eq!(
        restored
            .unknown_provider_event()
            .expect("unknown")
            .schema_version(),
        99
    );
    let page = journal
        .projected_page(0, None, PageLimits::new(8, 8 * 1024).expect("limits"))
        .expect("page");
    assert!(matches!(
        page.facts[0].payload,
        SemanticJournalPayload::Unknown {
            schema_version: 99,
            ..
        }
    ));
}

#[test]
fn journal_future_diagnostic_preserves_bounded_provider_id_for_dedupe() {
    let (_dir, _path, mut journal) = temp_journal(ProviderKind::ClaudeCode);
    let body = br#"{"schema_version":99,"source_type":"future.hook.v9","provider_event_id":"future_evt_1","payload":{"kind":"new_semantic"}}"#;
    let first = journal
        .ingest(
            test_permit(ProviderKind::ClaudeCode, "relay_future_provider_id_1"),
            body,
            NOW_MS,
        )
        .quarantined()
        .expect("future diagnostic");
    assert_eq!(first.provider_event_id(), Some("future_evt_1"));
    assert!(matches!(
        journal.ingest(
            test_permit(ProviderKind::ClaudeCode, "relay_future_provider_id_2"),
            body,
            NOW_MS,
        ),
        JournalIngestOutcome::Duplicate { existing_id } if existing_id == first.id()
    ));
}

#[test]
fn journal_forged_identity_in_payload_fails_closed() {
    let (_dir, _path, mut journal) = temp_journal(ProviderKind::ClaudeCode);
    let outcome = journal.ingest(
        test_permit(ProviderKind::ClaudeCode, "relay_forged"),
        &load_bytes("forged_authority.json"),
        NOW_MS,
    );
    assert!(
        matches!(
            outcome,
            JournalIngestOutcome::Rejected(JournalRejectReason::ForgedIdentity)
        ),
        "payload must not stamp authority, got {outcome:?}"
    );
    assert_eq!(journal.retained_len().expect("retained"), 0);
}

#[test]
fn journal_expired_and_foreign_permits_fail_closed() {
    let (_dir, _path, mut journal) = temp_journal(ProviderKind::ClaudeCode);
    let expired = AdapterDeliveryPermit::issue_for_test(
        ProviderKind::ClaudeCode,
        ids().0,
        ids().1,
        ids().2,
        1,
        1,
        [0x11; 32],
        "relay_expired",
        NOW_MS - 10_000,
        NOW_MS - 1,
    )
    .expect("expired permit");
    assert!(matches!(
        journal.ingest(expired, &load_bytes("claude_user_message.json"), NOW_MS),
        JournalIngestOutcome::Rejected(JournalRejectReason::Expired)
    ));

    let foreign = AdapterDeliveryPermit::issue_for_test(
        ProviderKind::Cursor,
        ids().0,
        ids().1,
        ids().2,
        9,
        1,
        [0x33; 32],
        "relay_wrong_gen",
        NOW_MS - 1,
        NOW_MS + 10,
    )
    .expect("foreign");
    assert!(matches!(
        journal.ingest(foreign, &load_bytes("claude_user_message.json"), NOW_MS),
        JournalIngestOutcome::Rejected(JournalRejectReason::Foreign)
    ));

    let other_generation = AdapterDeliveryPermit::issue_for_test(
        ProviderKind::ClaudeCode,
        ids().0,
        ids().1,
        ids().2,
        2,
        1,
        [0x11; 32],
        "relay_other_generation",
        NOW_MS - 1_000,
        NOW_MS + 60_000,
    )
    .expect("generation permit");
    assert!(matches!(
        journal.ingest(
            other_generation,
            &load_bytes("claude_user_message.json"),
            NOW_MS
        ),
        JournalIngestOutcome::Rejected(JournalRejectReason::Foreign)
    ));
}

#[test]
fn journal_quarantines_unknown_without_raw_payload() {
    let (_dir, _path, mut journal) = temp_journal(ProviderKind::ClaudeCode);
    let unknown = ingest_named(
        &mut journal,
        ProviderKind::ClaudeCode,
        "relay_claude_unknown_1",
        "unknown_future_event.json",
    )
    .quarantined()
    .expect("unknown");
    assert_eq!(unknown.kind(), JournalSemanticKind::UnknownProviderEvent);
    assert_eq!(unknown.visibility(), JournalVisibility::Diagnostic);
    assert!(unknown.projected_text().is_none());
    let body = unknown.unknown_provider_event().expect("body");
    assert_eq!(body.provider(), ProviderKind::ClaudeCode);
    assert_eq!(body.source_type(), "claude.future_hook.v9");
    assert_eq!(body.schema_version(), JOURNAL_SCHEMA_VERSION);
    assert_eq!(body.diagnostic_ref().len(), 64);
    assert!(!format!("{unknown:?}").contains("SECRET_SHOULD_NOT_PERSIST"));
}

#[test]
fn journal_same_delivery_same_payload_is_duplicate_after_commit() {
    let (_dir, _path, mut journal) = temp_journal(ProviderKind::ClaudeCode);
    let first = ingest_named(
        &mut journal,
        ProviderKind::ClaudeCode,
        "relay_claude_user_1",
        "claude_user_message.json",
    )
    .accepted()
    .expect("first");
    match ingest_named(
        &mut journal,
        ProviderKind::ClaudeCode,
        "relay_claude_user_1",
        "claude_user_message.json",
    ) {
        JournalIngestOutcome::Duplicate { existing_id } => assert_eq!(existing_id, first.id()),
        other => panic!("expected duplicate, got {other:?}"),
    }
}

#[test]
fn journal_same_native_id_different_payload_is_conflict() {
    let (_dir, _path, mut journal) = temp_journal(ProviderKind::ClaudeCode);
    let first = ingest_named(
        &mut journal,
        ProviderKind::ClaudeCode,
        "relay_shared",
        "claude_user_message.json",
    )
    .accepted()
    .expect("first");
    match ingest_named(
        &mut journal,
        ProviderKind::ClaudeCode,
        "relay_shared",
        "claude_tool_call.json",
    ) {
        JournalIngestOutcome::Conflict { existing_id } => assert_eq!(existing_id, first.id()),
        other => panic!("expected conflict, got {other:?}"),
    }
}

#[test]
fn journal_delivery_and_provider_key_hits_must_agree() {
    let (_dir, _path, mut journal) = temp_journal(ProviderKind::ClaudeCode);
    ingest_named(
        &mut journal,
        ProviderKind::ClaudeCode,
        "relay_key_a",
        "claude_user_message.json",
    );
    ingest_named(
        &mut journal,
        ProviderKind::ClaudeCode,
        "relay_key_b",
        "claude_tool_call.json",
    );
    assert!(matches!(
        journal.ingest(
            test_permit(ProviderKind::ClaudeCode, "relay_key_a"),
            &load_bytes("claude_tool_call.json"),
            NOW_MS,
        ),
        JournalIngestOutcome::NeedsResync
    ));
}

#[test]
fn journal_older_duplicate_retries_before_timestamp_regression() {
    let (_dir, _path, mut journal) = temp_journal(ProviderKind::ClaudeCode);
    let first_outcome = journal.ingest(
        test_permit(ProviderKind::ClaudeCode, "relay_old_duplicate"),
        &fixture_at("claude_user_message.json", 1_725_000_001_800),
        NOW_MS,
    );
    let first = first_outcome
        .accepted()
        .unwrap_or_else(|| panic!("first outcome: {first_outcome:?}"));
    let newer = journal.ingest(
        test_permit(ProviderKind::ClaudeCode, "relay_newer_event"),
        &fixture_at("claude_tool_call.json", 1_725_000_001_900),
        NOW_MS,
    );
    assert!(newer.accepted().is_some(), "newer outcome: {newer:?}");
    assert!(matches!(
        journal.ingest(
            test_permit(ProviderKind::ClaudeCode, "relay_old_duplicate"),
            &fixture_at("claude_user_message.json", 1_725_000_001_800),
            NOW_MS,
        ),
        JournalIngestOutcome::Duplicate { existing_id } if existing_id == first.id()
    ));
}

#[test]
fn journal_two_handles_reject_new_older_timestamp_in_write_transaction() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("kernel.sqlite3");
    let mut first = open_on(&path, ProviderKind::ClaudeCode);
    let mut second = open_on(&path, ProviderKind::ClaudeCode);
    ingest_named(
        &mut first,
        ProviderKind::ClaudeCode,
        "relay_timestamp_200",
        "claude_user_message.json",
    );
    let outcome = second.ingest(
        test_permit(ProviderKind::ClaudeCode, "relay_timestamp_100"),
        &fixture_at("claude_tool_call.json", 1_725_000_000_900),
        NOW_MS,
    );
    assert!(matches!(
        outcome,
        JournalIngestOutcome::Rejected(JournalRejectReason::TimestampRegression)
    ));
    assert_eq!(second.retained_len().expect("retained"), 1);
}

#[test]
fn journal_rejects_ingested_timestamp_regression_when_occurred_timestamp_advances() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("kernel.sqlite3");
    let mut first_handle = open_on(&path, ProviderKind::ClaudeCode);
    let mut second_handle = open_on(&path, ProviderKind::ClaudeCode);
    let first = first_handle.ingest(
        test_permit(ProviderKind::ClaudeCode, "relay_ingested_high_water_1"),
        &fixture_at("claude_user_message.json", NOW_MS),
        NOW_MS + 100,
    );
    assert!(first.accepted().is_some(), "first outcome: {first:?}");
    let second = second_handle.ingest(
        test_permit(ProviderKind::ClaudeCode, "relay_ingested_high_water_2"),
        &fixture_at("claude_tool_call.json", NOW_MS + 100),
        NOW_MS,
    );
    assert!(matches!(
        second,
        JournalIngestOutcome::Rejected(JournalRejectReason::TimestampRegression)
    ));
    assert_eq!(second_handle.retained_len().expect("retained"), 1);
}

#[test]
fn journal_older_ingested_duplicate_retries_before_timestamp_regression() {
    let (_dir, _path, mut journal) = temp_journal(ProviderKind::ClaudeCode);
    let first = journal.ingest(
        test_permit(ProviderKind::ClaudeCode, "relay_ingested_duplicate"),
        &fixture_at("claude_user_message.json", NOW_MS),
        NOW_MS + 100,
    );
    let first_id = first
        .accepted()
        .unwrap_or_else(|| panic!("first outcome: {first:?}"))
        .id();
    assert!(matches!(
        journal.ingest(
            test_permit(ProviderKind::ClaudeCode, "relay_ingested_duplicate"),
            &fixture_at("claude_user_message.json", NOW_MS),
            NOW_MS,
        ),
        JournalIngestOutcome::Duplicate { existing_id } if existing_id == first_id
    ));
}

#[test]
fn journal_crash_before_commit_then_reopen_uses_new_sqlite_connection() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("kernel.sqlite3");
    let journal = open_on(&path, ProviderKind::ClaudeCode);
    let draft = journal
        .propose(
            test_permit(ProviderKind::ClaudeCode, "relay_claude_user_1"),
            &load_bytes("claude_user_message.json"),
            NOW_MS,
        )
        .expect("draft");
    let uncommitted_id = draft.event().id();
    drop(journal);

    let reopened = open_on(&path, ProviderKind::ClaudeCode);
    assert_eq!(reopened.retained_len().expect("retained"), 0);
    drop(reopened);

    let mut journal = open_on(&path, ProviderKind::ClaudeCode);
    let committed = journal
        .ingest(
            test_permit(ProviderKind::ClaudeCode, "relay_claude_user_1"),
            &load_bytes("claude_user_message.json"),
            NOW_MS,
        )
        .accepted()
        .expect("committed");
    assert_ne!(committed.id(), uncommitted_id);
    drop(journal);

    let mut replayed = open_on(&path, ProviderKind::ClaudeCode);
    match replayed.ingest(
        test_permit(ProviderKind::ClaudeCode, "relay_claude_user_1"),
        &load_bytes("claude_user_message.json"),
        NOW_MS,
    ) {
        JournalIngestOutcome::Duplicate { existing_id } => {
            assert_eq!(existing_id, committed.id());
        }
        other => panic!("reopen must replay committed id, got {other:?}"),
    }
    assert_eq!(
        replayed.event_at(1).expect("load").expect("seq").sequence(),
        1
    );
}

#[test]
fn journal_draft_cannot_commit_into_another_journal() {
    let (_a_dir, _a_path, journal_a) = temp_journal(ProviderKind::ClaudeCode);
    let b_dir = TempDir::new().expect("tempdir");
    let b_path = b_dir.path().join("kernel.sqlite3");
    let mut journal_b = SemanticJournal::open(
        &b_path,
        &test_permit(ProviderKind::ClaudeCode, "session_open"),
        JournalLimits::default(),
        NOW_MS,
    )
    .expect("open same-authority other store");
    let draft = journal_a
        .propose(
            test_permit(ProviderKind::ClaudeCode, "relay_claude_user_1"),
            &load_bytes("claude_user_message.json"),
            NOW_MS,
        )
        .expect("draft");
    let outcome = journal_b.commit(draft);
    assert!(
        matches!(
            outcome,
            JournalIngestOutcome::Rejected(JournalRejectReason::Foreign)
        ),
        "same-authority draft must not commit across store instances, got {outcome:?}"
    );
    assert_eq!(journal_b.retained_len().expect("retained"), 0);
}

#[test]
fn journal_auth_seal_covers_delivery_and_semantic_fields() {
    let (_dir, _path, mut journal) = temp_journal(ProviderKind::ClaudeCode);
    let mut draft = journal
        .propose(
            test_permit(ProviderKind::ClaudeCode, "relay_mac_binding"),
            &load_bytes("claude_user_message.json"),
            NOW_MS,
        )
        .expect("draft");
    draft.event.delivery_id = RelayDeliveryId::new("relay_mac_mutated").expect("delivery");
    assert!(matches!(
        journal.commit(draft),
        JournalIngestOutcome::Rejected(JournalRejectReason::Foreign)
    ));
    assert_eq!(journal.retained_len().expect("retained"), 0);
}

#[test]
fn journal_does_not_dedupe_by_content_or_timestamp() {
    let (_dir, _path, mut journal) = temp_journal(ProviderKind::Codex);
    let first = ingest_named(
        &mut journal,
        ProviderKind::Codex,
        "relay_codex_unique_a",
        "codex_assistant_text.json",
    )
    .accepted()
    .expect("first");
    let second = ingest_named(
        &mut journal,
        ProviderKind::Codex,
        "relay_codex_unique_b",
        "codex_assistant_text_alt.json",
    )
    .accepted()
    .expect("second");
    assert_ne!(first.id(), second.id());
    assert_eq!(journal.retained_len().expect("retained"), 2);
}

#[test]
fn journal_never_persists_raw_terminal_bytes_as_semantic_text() {
    let (_dir, _path, mut journal) = temp_journal(ProviderKind::Cursor);
    let outcome = ingest_named(
        &mut journal,
        ProviderKind::Cursor,
        "relay_cursor_term_1",
        "terminal_bytes.json",
    );
    assert!(matches!(outcome, JournalIngestOutcome::IgnoredTerminal));
    let page = journal
        .persist_page(0, None, PageLimits::new(16, 8 * 1024).expect("limits"))
        .expect("page");
    assert!(page.facts.is_empty());
    assert!(!format!("{outcome:?}").contains("RAW_TERMINAL"));
}

#[test]
fn journal_unknown_and_known_facts_never_drive_task_reducer() {
    let (_dir, _path, mut journal) = temp_journal(ProviderKind::ClaudeCode);
    let question = ingest_named(
        &mut journal,
        ProviderKind::ClaudeCode,
        "relay_claude_question_1",
        "question.json",
    )
    .accepted()
    .expect("question");
    ingest_named(
        &mut journal,
        ProviderKind::ClaudeCode,
        "relay_claude_unknown_1",
        "unknown_future_event.json",
    );
    assert!(question.as_domain_event().is_none());
    assert!(!question.drives_task_question_approval_or_settlement());
    let unknown = journal.event_at(2).expect("load").expect("unknown");
    assert!(unknown.as_domain_event().is_none());
    assert!(!unknown.drives_task_question_approval_or_settlement());
    let domain = DomainEvent {
        id: EventId::new(),
        task_id: Some(ids().0),
        sequence: 1,
        task_revision: Some(1),
        occurred_at_ms: 1,
        payload: Event::TaskReopened,
    };
    assert!(apply(Option::<TaskSnapshot>::None, &domain).is_err());
}

#[test]
fn journal_assigns_stable_ids_from_permit_not_payload() {
    let (task, agent, _) = ids();
    let (_dir, _path, mut journal) = temp_journal(ProviderKind::ClaudeCode);
    let first = ingest_named(
        &mut journal,
        ProviderKind::ClaudeCode,
        "relay_claude_user_1",
        "claude_user_message.json",
    )
    .accepted()
    .expect("first");
    let second = ingest_named(
        &mut journal,
        ProviderKind::ClaudeCode,
        "relay_claude_tool_1",
        "claude_tool_call.json",
    )
    .accepted()
    .expect("second");
    assert_eq!(first.task_id(), task);
    assert_eq!(first.agent_session_id(), agent);
    assert_eq!(first.runtime_generation(), 1);
    assert_eq!(first.sequence(), 1);
    assert_eq!(second.sequence(), 2);
    assert_eq!(first.delivery_id(), "relay_claude_user_1");
    assert_eq!(
        first.provider_event_id().map(str::to_owned),
        Some("claude_evt_user_1".into())
    );
}

#[test]
fn journal_applies_explicit_privacy_policy() {
    let (_dir, _path, mut journal) = temp_journal(ProviderKind::ClaudeCode);
    let user = ingest_named(
        &mut journal,
        ProviderKind::ClaudeCode,
        "relay_claude_user_1",
        "claude_user_message.json",
    )
    .accepted()
    .expect("user");
    let unknown = ingest_named(
        &mut journal,
        ProviderKind::ClaudeCode,
        "relay_claude_unknown_1",
        "unknown_future_event.json",
    )
    .quarantined()
    .expect("unknown");
    assert_eq!(
        user.redaction_class(),
        JournalRedactionClass::PersistableLocalOnly
    );
    assert_eq!(user.privacy_class(), PrivacyClass::LocalOnly);
    assert_eq!(
        unknown.redaction_class(),
        JournalRedactionClass::MetadataOnly
    );
    assert!(unknown.projected_text().is_none());
    let page = journal
        .persist_page(0, None, PageLimits::new(16, 8 * 1024).expect("limits"))
        .expect("page");
    assert_eq!(page.facts.len(), 2);
    assert!(page.facts.iter().all(|fact| fact.redacted));
    assert!(!format!("{page:?}").contains("SECRET_SHOULD_NOT_PERSIST"));
    assert!(!format!("{user:?}").contains("Normalize Claude"));
}

#[test]
fn journal_round_trips_snapshot_page_through_protocol_codec() {
    let (_dir, _path, mut journal) = temp_journal(ProviderKind::ClaudeCode);
    ingest_named(
        &mut journal,
        ProviderKind::ClaudeCode,
        "relay_claude_user_1",
        "claude_user_message.json",
    );
    ingest_named(
        &mut journal,
        ProviderKind::ClaudeCode,
        "relay_claude_tool_1",
        "claude_tool_call.json",
    );
    let page = journal
        .projected_page(0, None, PageLimits::new(16, 8 * 1024).expect("limits"))
        .expect("page");
    assert!(page
        .facts
        .iter()
        .all(|fact| fact.kind != "user_message" || fact.privacy_class == PrivacyClass::LocalOnly));
    let codec = MessagePackCodec::from_limits(FrameLimits::v1_default()).expect("codec");
    let encoded = codec.encode(&page).expect("encode");
    let restored: SemanticJournalPage = codec.decode(&encoded).expect("decode");
    assert_eq!(restored, page);
    let page_bytes = codec.encode(&page).expect("encode page");
    assert_eq!(
        page.encoded_bytes,
        u32::try_from(page_bytes.len()).expect("page bytes fit u32"),
        "page encoded_bytes must be the complete encoded page length"
    );
}

#[test]
fn journal_rejects_16mib_cap_plus_one_nesting_duplicate_keys_and_wide_arrays() {
    let (_dir, _path, mut journal) = temp_journal(ProviderKind::ClaudeCode);
    let oversized = vec![b'{'; 16 * 1024 * 1024 + 1];
    assert!(matches!(
        journal.ingest(
            test_permit(ProviderKind::ClaudeCode, "relay_bounds_1"),
            &oversized,
            NOW_MS
        ),
        JournalIngestOutcome::Rejected(JournalRejectReason::InvalidEnvelope)
    ));

    let cap_plus_one = vec![b'x'; MAX_JOURNAL_DOCUMENT_BYTES + 1];
    assert!(matches!(
        journal.ingest(
            test_permit(ProviderKind::ClaudeCode, "relay_bounds_2"),
            &cap_plus_one,
            NOW_MS
        ),
        JournalIngestOutcome::Rejected(JournalRejectReason::InvalidEnvelope)
    ));

    let nested = format!(
        "{}{}",
        "[".repeat(MAX_JOURNAL_NESTING + 2),
        "]".repeat(MAX_JOURNAL_NESTING + 2)
    );
    assert!(matches!(
        journal.ingest(
            test_permit(ProviderKind::ClaudeCode, "relay_bounds_3"),
            nested.as_bytes(),
            NOW_MS
        ),
        JournalIngestOutcome::Rejected(JournalRejectReason::InvalidEnvelope)
    ));

    let duplicate = br#"{"schema_version":1,"schema_version":1,"source_type":"user_message","occurred_at_ms":1,"payload":{"kind":"user_message","text":"x"}}"#;
    assert!(
        matches!(
            journal.ingest(
                test_permit(ProviderKind::ClaudeCode, "relay_bounds_4"),
                duplicate,
                NOW_MS
            ),
            JournalIngestOutcome::Rejected(JournalRejectReason::InvalidEnvelope)
        ),
        "duplicate keys must not parse as a second schema"
    );

    let wide_array = format!("[{}]", vec!["0"; MAX_JOURNAL_ARRAY_ITEMS + 1].join(","));
    assert!(
        matches!(
            journal.ingest(
                test_permit(ProviderKind::ClaudeCode, "relay_bounds_5"),
                wide_array.as_bytes(),
                NOW_MS
            ),
            JournalIngestOutcome::Rejected(JournalRejectReason::InvalidEnvelope)
        ),
        "wide arrays must be rejected before serde allocation"
    );

    let nested_arrays = format!(
        "{{\"schema_version\":1,\"source_type\":\"user_message\",\"occurred_at_ms\":1,\"payload\":{{\"kind\":\"user_message\",\"text\":\"x\",\"opts\":{}}}}}",
        format!("[{}]", vec!["[1]"; MAX_JOURNAL_ARRAY_ITEMS + 1].join(","))
    );
    assert!(
        matches!(
            journal.ingest(
                test_permit(ProviderKind::ClaudeCode, "relay_bounds_6"),
                nested_arrays.as_bytes(),
                NOW_MS
            ),
            JournalIngestOutcome::Rejected(JournalRejectReason::InvalidEnvelope)
        ),
        "nested wide arrays must fail closed in preflight"
    );

    let too_many_options = format!(
        "{{\"schema_version\":1,\"source_type\":\"question\",\"occurred_at_ms\":1,\"payload\":{{\"kind\":\"question\",\"question_id\":\"q1\",\"prompt\":\"p\",\"options\":[{}]}}}}",
        (0..=MAX_QUESTION_OPTIONS)
            .map(|index| format!("\"o{index}\""))
            .collect::<Vec<_>>()
            .join(",")
    );
    assert!(matches!(
        journal.ingest(
            test_permit(ProviderKind::ClaudeCode, "relay_bounds_7"),
            too_many_options.as_bytes(),
            NOW_MS
        ),
        JournalIngestOutcome::Rejected(JournalRejectReason::InvalidEnvelope)
    ));
    assert_eq!(journal.retained_len().expect("retained"), 0);
}

#[test]
fn journal_returns_backpressure_before_100k_retained_events() {
    assert!(MAX_JOURNAL_EVENTS < 100_000);
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("kernel.sqlite3");
    let limits = JournalLimits {
        max_events: 8,
        max_dedupe_keys: 32,
        max_ingest_steps: 64,
    };
    let mut journal = SemanticJournal::open(
        &path,
        &test_permit(ProviderKind::ClaudeCode, "session_open"),
        limits,
        NOW_MS,
    )
    .expect("open");
    let mut saw_backpressure = false;
    for index in 0..100_000u32 {
        let delivery = format!("relay_{index}");
        let body = format!(
            "{{\"schema_version\":1,\"source_type\":\"usage_observation\",\"provider_event_id\":\"evt_{index}\",\"occurred_at_ms\":{},\"payload\":{{\"kind\":\"usage_observation\",\"remaining_percent\":80}},\"extensions\":{{}}}}",
            1_725_000_001_500_i64 + i64::from(index)
        );
        match journal.ingest(
            test_permit(ProviderKind::ClaudeCode, &delivery),
            body.as_bytes(),
            NOW_MS,
        ) {
            JournalIngestOutcome::Accepted(_) => {}
            JournalIngestOutcome::Backpressure(JournalBackpressure::EventCapacity)
            | JournalIngestOutcome::NeedsResync => {
                saw_backpressure = true;
                break;
            }
            other => panic!("unexpected {other:?} at {index}"),
        }
    }
    assert!(saw_backpressure);
    assert!(journal.retained_len().expect("retained") <= 8);
    assert!(journal.retained_len().expect("retained") < 100_000);
}

#[test]
fn journal_sequence_overflow_fails_closed() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("kernel.sqlite3");
    let mut journal = open_on(&path, ProviderKind::ClaudeCode);
    journal.debug_set_next_sequence(u64::MAX);
    assert!(matches!(
        journal.ingest(
            test_permit(ProviderKind::ClaudeCode, "relay_overflow"),
            &load_bytes("cursor_usage.json"),
            NOW_MS
        ),
        JournalIngestOutcome::Rejected(JournalRejectReason::SequenceOverflow)
    ));
}

#[test]
fn journal_timestamp_regression_fails_closed() {
    let (_dir, _path, mut journal) = temp_journal(ProviderKind::ClaudeCode);
    ingest_named(
        &mut journal,
        ProviderKind::ClaudeCode,
        "relay_later",
        "unknown_future_event.json",
    );
    assert!(matches!(
        journal.ingest(
            test_permit(ProviderKind::ClaudeCode, "relay_regression"),
            &load_bytes("claude_user_message.json"),
            NOW_MS
        ),
        JournalIngestOutcome::Rejected(JournalRejectReason::TimestampRegression)
    ));
}

#[test]
fn journal_pages_replay_by_durable_sequence_and_canonical_bytes() {
    let (_dir, _path, mut journal) = temp_journal(ProviderKind::ClaudeCode);
    ingest_named(
        &mut journal,
        ProviderKind::ClaudeCode,
        "relay_claude_user_1",
        "claude_user_message.json",
    );
    ingest_named(
        &mut journal,
        ProviderKind::ClaudeCode,
        "relay_claude_tool_1",
        "claude_tool_call.json",
    );
    ingest_named(
        &mut journal,
        ProviderKind::ClaudeCode,
        "relay_claude_question_1",
        "question.json",
    );
    let first = journal
        .projected_page(0, None, PageLimits::new(1, 8 * 1024).expect("limits"))
        .expect("first page");
    assert_eq!(first.facts.len(), 1);
    assert_eq!(first.next_sequence, Some(2));
    assert_eq!(first.high_water, 3);
    let codec = MessagePackCodec::from_limits(FrameLimits::v1_default()).expect("codec");
    let encoded = codec.encode(&first).expect("encode first page");
    assert_eq!(
        first.encoded_bytes,
        u32::try_from(encoded.len()).expect("fits")
    );
    assert!(
        journal
            .projected_page(
                0,
                None,
                PageLimits::new(1, first.encoded_bytes).expect("exact page cap"),
            )
            .is_ok(),
        "an exact encoded page cap must accept the complete page"
    );
    if first.encoded_bytes > 1 {
        debug_reset_journal_page_materialization_counters();
        let under = journal
            .projected_page(
                0,
                None,
                PageLimits::new(1, first.encoded_bytes - 1).expect("under page cap"),
            )
            .expect("under cap must produce a bounded page");
        let under_bytes = codec.encode(&under).expect("encode under-cap page");
        assert!(under.facts.len() < first.facts.len());
        assert!(under_bytes.len() <= usize::try_from(first.encoded_bytes - 1).unwrap());
        assert_eq!(under.encoded_bytes as usize, under_bytes.len());
        assert_eq!(under.through_sequence, 0);
        assert_eq!(under.next_sequence, Some(1));
        assert_eq!(
            debug_journal_page_materialization_counters(),
            (0, 0),
            "an oversized first candidate must be rejected before event/fact materialization"
        );
    }
    let second = journal
        .projected_page(
            first.through_sequence,
            Some(first.high_water),
            PageLimits::new(8, 8 * 1024).expect("limits"),
        )
        .expect("second page");
    assert_eq!(second.facts.len(), 2);
    assert_eq!(second.high_water, first.high_water);
    assert!(second.next_sequence.is_none());
    assert!(
        journal
            .projected_page(
                first.through_sequence,
                Some(first.high_water + 1),
                PageLimits::new(8, 8 * 1024).expect("limits"),
            )
            .is_err(),
        "continuation must reject a changed high-water"
    );
}

#[test]
fn journal_page_high_water_is_immutable_after_new_ingestion() {
    let (_dir, _path, mut journal) = temp_journal(ProviderKind::ClaudeCode);
    ingest_named(
        &mut journal,
        ProviderKind::ClaudeCode,
        "relay_claude_user_1",
        "claude_user_message.json",
    );
    ingest_named(
        &mut journal,
        ProviderKind::ClaudeCode,
        "relay_claude_tool_1",
        "claude_tool_call.json",
    );
    let page = journal
        .projected_page(0, None, PageLimits::new(1, 8 * 1024).expect("limits"))
        .expect("page");
    assert_eq!(page.high_water, 2);

    ingest_named(
        &mut journal,
        ProviderKind::ClaudeCode,
        "relay_claude_question_1",
        "question.json",
    );
    assert_eq!(page.high_water, 2, "returned page must retain its capture");
    assert!(matches!(
        journal.projected_page(
            page.through_sequence,
            Some(page.high_water),
            PageLimits::new(8, 8 * 1024).expect("limits"),
        ),
        Err(JournalIngestOutcome::NeedsResync)
    ));
}

#[test]
fn journal_open_rejects_expired_permit() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("kernel.sqlite3");
    let expired = AdapterDeliveryPermit::issue_for_test(
        ProviderKind::ClaudeCode,
        ids().0,
        ids().1,
        ids().2,
        1,
        1,
        [0x11; 32],
        "session_open",
        NOW_MS - 10_000,
        NOW_MS - 1,
    )
    .expect("expired permit");
    match SemanticJournal::open(&path, &expired, JournalLimits::default(), NOW_MS) {
        Err(err) => assert_eq!(err, JournalError::Expired),
        Ok(_) => panic!("expired open must fail closed"),
    }
}

#[test]
fn journal_same_file_reopen_commits_pre_reopen_draft() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("kernel.sqlite3");
    let journal = open_on(&path, ProviderKind::ClaudeCode);
    let draft = journal
        .propose(
            test_permit(ProviderKind::ClaudeCode, "relay_reopen_draft"),
            &load_bytes("claude_user_message.json"),
            NOW_MS,
        )
        .expect("draft");
    drop(journal);
    let mut reopened = open_on(&path, ProviderKind::ClaudeCode);
    let outcome = reopened.commit(draft);
    assert!(
        matches!(outcome, JournalIngestOutcome::Accepted(_)),
        "persisted store identity must survive reopen, got {outcome:?}"
    );
    assert_eq!(reopened.retained_len().expect("retained"), 1);
}

#[test]
fn journal_unicode_escaped_forged_root_key_fails_closed() {
    let (_dir, _path, mut journal) = temp_journal(ProviderKind::ClaudeCode);
    let body = r#"{"schema_version":1,"\u0074ask_id":"x","source_type":"user_message","occurred_at_ms":1725000001500,"payload":{"kind":"user_message","text":"hi"}}"#;
    let outcome = journal.ingest(
        test_permit(ProviderKind::ClaudeCode, "relay_unicode_forge"),
        body.as_bytes(),
        NOW_MS,
    );
    assert!(
        matches!(
            outcome,
            JournalIngestOutcome::Rejected(JournalRejectReason::ForgedIdentity)
        ),
        "unicode-escaped root key must be forged identity, got {outcome:?}"
    );
    assert_eq!(journal.retained_len().expect("retained"), 0);
}

#[test]
fn journal_corrupt_identity_blob_fails_closed() {
    let (_dir, _path, mut journal) = temp_journal(ProviderKind::ClaudeCode);
    ingest_named(
        &mut journal,
        ProviderKind::ClaudeCode,
        "relay_claude_user_1",
        "claude_user_message.json",
    );
    journal.debug_zero_event_id(1);
    assert!(
        matches!(
            journal.projected_page(0, None, PageLimits::new(8, 8 * 1024).expect("limits")),
            Err(JournalIngestOutcome::NeedsResync)
        ),
        "corrupt identity must fail closed on page"
    );
    assert!(
        matches!(journal.event_at(1), Err(JournalIngestOutcome::NeedsResync)),
        "corrupt identity must fail closed on event_at"
    );
    assert!(matches!(
        crate::kernel::semantic_journal::exact16(vec![0u8; 8]),
        Err(crate::kernel::StoreError::Corruption)
    ));
    assert!(matches!(
        crate::kernel::semantic_journal::exact32(vec![0u8; 16]),
        Err(crate::kernel::StoreError::Corruption)
    ));
}

#[test]
fn journal_post_open_corrupt_row_surfaces_retained_error_before_next_write() {
    let (_dir, path, mut journal) = temp_journal(ProviderKind::ClaudeCode);
    ingest_named(
        &mut journal,
        ProviderKind::ClaudeCode,
        "relay_corrupt_after_open",
        "claude_user_message.json",
    );
    let conn = Connection::open(&path).expect("reopen raw");
    conn.execute(
        "UPDATE semantic_journal_facts SET event_id = zeroblob(16) WHERE sequence = 1",
        [],
    )
    .expect("corrupt row");
    drop(conn);

    assert!(matches!(
        journal.retained_len(),
        Err(JournalIngestOutcome::NeedsResync)
    ));
    assert!(matches!(
        journal.ingest(
            test_permit(ProviderKind::ClaudeCode, "relay_after_corrupt_row"),
            &load_bytes("claude_tool_call.json"),
            NOW_MS,
        ),
        JournalIngestOutcome::NeedsResync
    ));
}

#[test]
fn journal_corruption_in_later_row_is_sticky_across_all_reads_and_writes() {
    let (_dir, path, mut journal) = temp_journal(ProviderKind::ClaudeCode);
    ingest_named(
        &mut journal,
        ProviderKind::ClaudeCode,
        "relay_sticky_first",
        "claude_user_message.json",
    );
    ingest_named(
        &mut journal,
        ProviderKind::ClaudeCode,
        "relay_sticky_second",
        "claude_tool_call.json",
    );
    let conn = Connection::open(&path).expect("reopen raw");
    conn.execute(
        "UPDATE semantic_journal_facts SET kind = 'not_a_semantic_kind' WHERE sequence = 2",
        [],
    )
    .expect("corrupt later row");
    drop(conn);

    assert!(matches!(
        journal.retained_len(),
        Err(JournalIngestOutcome::NeedsResync)
    ));
    assert!(matches!(
        journal.event_at(1),
        Err(JournalIngestOutcome::NeedsResync)
    ));
    assert!(matches!(
        journal.projected_page(0, None, PageLimits::new(8, 8 * 1024).expect("limits")),
        Err(JournalIngestOutcome::NeedsResync)
    ));
    assert!(matches!(
        journal.ingest(
            test_permit(ProviderKind::ClaudeCode, "relay_sticky_after_corruption"),
            &load_bytes("cursor_usage.json"),
            NOW_MS,
        ),
        JournalIngestOutcome::NeedsResync
    ));
}

#[test]
fn journal_open_rejects_unbounded_unknown_metadata() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("kernel.sqlite3");
    let mut journal = open_on(&path, ProviderKind::ClaudeCode);
    ingest_named(
        &mut journal,
        ProviderKind::ClaudeCode,
        "relay_claude_unknown_1",
        "unknown_future_event.json",
    );
    let authority_digest = journal.authority_digest;
    drop(journal);

    let conn = Connection::open(&path).expect("reopen raw");
    let body = PersistedJournalBody {
        text: None,
        extensions: BTreeMap::new(),
        unknown_source_type: Some("x".repeat(MAX_SOURCE_TYPE_BYTES + 1)),
        unknown_schema_version: Some(JOURNAL_SCHEMA_VERSION),
        unknown_diagnostic_ref: Some("0".repeat(64)),
        provider_event_id: Some("claude_evt_unknown_1".into()),
        payload: SemanticJournalPayload::Unknown {
            provider: "claude_code".into(),
            source_type: "x".repeat(MAX_SOURCE_TYPE_BYTES + 1),
            schema_version: JOURNAL_SCHEMA_VERSION,
            diagnostic_ref: "0".repeat(64),
        },
    };
    let payload = rmp_serde::to_vec_named(&body).expect("encode corrupt body");
    conn.execute(
        "UPDATE semantic_journal_facts SET payload = ?1
         WHERE authority_digest = ?2 AND sequence = 1",
        rusqlite::params![payload, authority_digest.as_slice()],
    )
    .expect("replace persisted body");

    let reopened = SemanticJournal::open(
        &path,
        &test_permit(ProviderKind::ClaudeCode, "session_open"),
        JournalLimits::default(),
        NOW_MS,
    );
    assert!(matches!(reopened, Err(JournalError::Store)));
}

#[test]
fn journal_oversized_first_page_fails_before_returning_materialized_facts() {
    let (_dir, _path, mut journal) = temp_journal(ProviderKind::ClaudeCode);
    ingest_named(
        &mut journal,
        ProviderKind::ClaudeCode,
        "relay_oversized_first",
        "claude_user_message.json",
    );
    debug_reset_journal_page_materialization_counters();
    assert!(matches!(
        journal.projected_page(0, None, PageLimits::new(1, 1).expect("minimum cap")),
        Err(JournalIngestOutcome::Backpressure(
            JournalBackpressure::PageBudget
        ))
    ));
    assert_eq!(
        debug_journal_page_materialization_counters(),
        (0, 0),
        "an oversized first candidate must not restore or allocate a fact"
    );
    assert_eq!(
        debug_journal_page_preflight_counters(),
        (0, 0),
        "an oversized first candidate must not finalize a row or decode its payload"
    );
}

#[test]
fn journal_page_cap_blocks_continuation_restore_before_owned_fact_copy() {
    let (_dir, _path, mut journal) = temp_journal(ProviderKind::ClaudeCode);
    ingest_named(
        &mut journal,
        ProviderKind::ClaudeCode,
        "relay_page_counter_first",
        "claude_user_message.json",
    );
    ingest_named(
        &mut journal,
        ProviderKind::ClaudeCode,
        "relay_page_counter_second",
        "claude_tool_call.json",
    );
    let first = journal
        .projected_page(0, None, PageLimits::new(1, 8 * 1024).expect("limits"))
        .expect("first page");
    debug_reset_journal_page_materialization_counters();
    let bounded = journal
        .projected_page(
            0,
            None,
            PageLimits::new(2, first.encoded_bytes).expect("exact first-page cap"),
        )
        .expect("bounded continuation");
    assert_eq!(bounded.facts.len(), 1);
    assert_eq!(bounded.next_sequence, Some(2));
    assert!(bounded.encoded_bytes <= first.encoded_bytes);
    assert_eq!(
        debug_journal_page_materialization_counters(),
        (1, 1),
        "the continuation row must be rejected before restore and owned-copy"
    );
    assert_eq!(
        debug_journal_page_preflight_counters(),
        (1, 1),
        "only the admitted continuation row may finalize or decode a payload"
    );
}

#[test]
fn journal_page_preflight_uses_cursor_after_runtime_skips_at_integer_boundary() {
    let (_dir, path, mut journal) = temp_journal(ProviderKind::ClaudeCode);
    for index in 1..=129 {
        let body = format!(
            r#"{{"schema_version":1,"source_type":"user_message","provider_event_id":"claude_evt_boundary_{index}","occurred_at_ms":{},"payload":{{"kind":"user_message","text":"boundary"}},"extensions":{{}}}}"#,
            NOW_MS - 1_000 + i64::from(index)
        );
        assert!(matches!(
            journal.ingest(
                test_permit(ProviderKind::ClaudeCode, &format!("relay_boundary_{index}")),
                body.as_bytes(),
                NOW_MS,
            ),
            JournalIngestOutcome::Accepted(_)
        ));
    }
    let authority_digest = journal.authority_digest;
    let conn = Connection::open(&path).expect("reopen raw");
    conn.execute(
        "UPDATE semantic_journal_facts
         SET visibility = 'runtime_only'
         WHERE authority_digest = ?1 AND sequence IN (127, 128)",
        rusqlite::params![authority_digest.as_slice()],
    )
    .expect("mark cursor-boundary rows runtime-only");
    drop(conn);

    let wide = journal
        .projected_page(125, None, PageLimits::new(1, 8 * 1024).expect("limits"))
        .expect("wide page");
    assert_eq!(wide.facts.len(), 1);
    assert_eq!(wide.facts[0].sequence, 126);
    assert_eq!(wide.next_sequence, Some(129));
    assert!(wide.encoded_bytes > 1);

    debug_reset_journal_page_materialization_counters();
    let cap = wide.encoded_bytes - 1;
    let bounded = journal
        .projected_page(125, None, PageLimits::new(1, cap).expect("under cap"))
        .expect("under-cap page must stop before candidate materialization");
    assert!(bounded.facts.is_empty());
    assert_eq!(bounded.next_sequence, Some(126));
    assert!(bounded.encoded_bytes <= cap);
    assert_eq!(
        debug_journal_page_materialization_counters(),
        (0, 0),
        "cursor-width overflow must be rejected before event/fact materialization"
    );
    assert_eq!(
        debug_journal_page_preflight_counters(),
        (0, 0),
        "cursor-width overflow must be rejected before owned row/payload decode"
    );
}

#[test]
fn journal_persisted_bidi_delivery_id_fails_closed_during_integrity_scan() {
    let (_dir, path, mut journal) = temp_journal(ProviderKind::ClaudeCode);
    ingest_named(
        &mut journal,
        ProviderKind::ClaudeCode,
        "relay_bidi_corrupt",
        "claude_user_message.json",
    );
    let authority_digest = journal.authority_digest;
    let conn = Connection::open(&path).expect("reopen raw");
    conn.execute(
        "UPDATE semantic_journal_facts SET delivery_id = ?1
         WHERE authority_digest = ?2 AND sequence = 1",
        rusqlite::params!["relay_\u{202e}corrupt", authority_digest.as_slice()],
    )
    .expect("corrupt persisted delivery id");
    drop(conn);

    assert!(matches!(
        journal.retained_len(),
        Err(JournalIngestOutcome::NeedsResync)
    ));
    assert!(matches!(
        journal.projected_page(0, None, PageLimits::new(1, 8 * 1024).expect("limits")),
        Err(JournalIngestOutcome::NeedsResync)
    ));
}

#[test]
fn journal_page_global_integrity_rejects_post_cap_corrupt_body() {
    let (_dir, path, mut journal) = temp_journal(ProviderKind::ClaudeCode);
    ingest_named(
        &mut journal,
        ProviderKind::ClaudeCode,
        "relay_global_integrity_first",
        "claude_user_message.json",
    );
    ingest_named(
        &mut journal,
        ProviderKind::ClaudeCode,
        "relay_global_integrity_second",
        "claude_tool_call.json",
    );
    let authority_digest = journal.authority_digest;
    let conn = Connection::open(&path).expect("reopen raw");
    conn.execute(
        "UPDATE semantic_journal_facts SET payload = ?1
         WHERE authority_digest = ?2 AND sequence = 2",
        rusqlite::params![
            vec![0x81, 0xa7, b'u', b'n', b'k', b'n', b'o', b'w', b'n', 0xc0],
            authority_digest.as_slice()
        ],
    )
    .expect("corrupt post-cap body");
    drop(conn);

    assert!(matches!(
        journal.projected_page(0, None, PageLimits::new(1, 8 * 1024).expect("limits")),
        Err(JournalIngestOutcome::NeedsResync)
    ));
}

#[test]
fn journal_page_global_integrity_rejects_runtime_only_corrupt_body_before_skip() {
    let (_dir, path, mut journal) = temp_journal(ProviderKind::ClaudeCode);
    ingest_named(
        &mut journal,
        ProviderKind::ClaudeCode,
        "relay_global_integrity_runtime",
        "claude_user_message.json",
    );
    let authority_digest = journal.authority_digest;
    let conn = Connection::open(&path).expect("reopen raw");
    conn.execute(
        "UPDATE semantic_journal_facts
         SET visibility = 'runtime_only', payload = ?1
         WHERE authority_digest = ?2 AND sequence = 1",
        rusqlite::params![
            vec![0x81, 0xa7, b'u', b'n', b'k', b'n', b'o', b'w', b'n', 0xc0],
            authority_digest.as_slice()
        ],
    )
    .expect("corrupt runtime-only body");
    drop(conn);

    assert!(matches!(
        journal.projected_page(0, None, PageLimits::new(1, 8 * 1024).expect("limits")),
        Err(JournalIngestOutcome::NeedsResync)
    ));
}

#[test]
fn journal_page_global_integrity_rejects_runtime_only_known_field_for_kind() {
    let (_dir, path, mut journal) = temp_journal(ProviderKind::ClaudeCode);
    ingest_named(
        &mut journal,
        ProviderKind::ClaudeCode,
        "relay_global_integrity_runtime_kind",
        "claude_user_message.json",
    );
    let body = PersistedJournalBody {
        text: None,
        extensions: BTreeMap::new(),
        unknown_source_type: None,
        unknown_schema_version: None,
        unknown_diagnostic_ref: None,
        provider_event_id: Some("claude_evt_user_1".into()),
        payload: SemanticJournalPayload::UserMessage { text: "ok".into() },
    };
    let mut payload = rmp_serde::to_vec_named(&body).expect("encode body");
    let payload_map = payload
        .iter()
        .rposition(|marker| *marker == 0x82)
        .expect("payload map");
    payload[payload_map] = 0x83;
    payload.extend_from_slice(&[0xa6, b's', b't', b'a', b't', b'u', b's', 0xa1, b'x']);
    let authority_digest = journal.authority_digest;
    let conn = Connection::open(&path).expect("reopen raw");
    conn.execute(
        "UPDATE semantic_journal_facts
         SET visibility = 'runtime_only', payload = ?1
         WHERE authority_digest = ?2 AND sequence = 1",
        rusqlite::params![payload, authority_digest.as_slice()],
    )
    .expect("corrupt runtime-only kind fields");
    drop(conn);

    assert!(matches!(
        journal.projected_page(0, None, PageLimits::new(1, 8 * 1024).expect("limits")),
        Err(JournalIngestOutcome::NeedsResync)
    ));
}

#[test]
fn journal_raw_preflight_rejects_unknown_envelope_and_payload_keys() {
    let body = PersistedJournalBody {
        text: None,
        extensions: BTreeMap::new(),
        unknown_source_type: None,
        unknown_schema_version: None,
        unknown_diagnostic_ref: None,
        provider_event_id: Some("claude_evt_raw_keys".into()),
        payload: SemanticJournalPayload::UserMessage { text: "ok".into() },
    };
    let mut envelope = rmp_serde::to_vec_named(&body).expect("encode body");
    assert_eq!(envelope.first(), Some(&0x87), "body map must be a fixmap");
    envelope[0] = 0x88;
    envelope.extend_from_slice(&[0xa5, b'b', b'o', b'g', b'u', b's', 0xc0]);
    assert!(matches!(
        RawMessagePack::new(&envelope).persisted_payload_shape(),
        Err(JournalError::InvalidEnvelope)
    ));

    let mut payload = rmp_serde::to_vec_named(&body).expect("encode body");
    let payload_map = payload
        .iter()
        .rposition(|marker| *marker == 0x82)
        .expect("payload map");
    payload[payload_map] = 0x83;
    payload.extend_from_slice(&[0xa5, b'b', b'o', b'g', b'u', b's', 0xc0]);
    assert!(matches!(
        RawMessagePack::new(&payload).persisted_payload_shape(),
        Err(JournalError::InvalidEnvelope)
    ));
}

#[test]
fn journal_raw_preflight_rejects_known_field_for_wrong_payload_kind() {
    let body = PersistedJournalBody {
        text: None,
        extensions: BTreeMap::new(),
        unknown_source_type: None,
        unknown_schema_version: None,
        unknown_diagnostic_ref: None,
        provider_event_id: Some("claude_evt_wrong_kind_field".into()),
        payload: SemanticJournalPayload::UserMessage { text: "ok".into() },
    };
    let mut payload = rmp_serde::to_vec_named(&body).expect("encode body");
    let payload_map = payload
        .iter()
        .rposition(|marker| *marker == 0x82)
        .expect("payload map");
    payload[payload_map] = 0x83;
    payload.extend_from_slice(&[0xa6, b's', b't', b'a', b't', b'u', b's', 0xa1, b'x']);
    assert!(matches!(
        RawMessagePack::new(&payload).persisted_payload_shape(),
        Err(JournalError::InvalidEnvelope)
    ));
}

#[test]
fn journal_raw_preflight_bounds_total_messagepack_values() {
    let mut bomb = Vec::new();
    bomb.extend_from_slice(&[0xdc, 0, 16]);
    for _ in 0..16 {
        bomb.extend_from_slice(&[0xde, 0, 16]);
        for _ in 0..16 {
            bomb.extend_from_slice(&[0xa1, b'k', 0xa1, b'v']);
        }
    }
    let mut reader = RawMessagePack::new(&bomb);
    assert!(matches!(reader.skip_value(0), Err(JournalError::Oversized)));
}

#[test]
fn journal_open_rejects_persisted_bidi_identity_in_high_water_scan() {
    let (_dir, path, mut journal) = temp_journal(ProviderKind::ClaudeCode);
    ingest_named(
        &mut journal,
        ProviderKind::ClaudeCode,
        "relay_open_bidi_corrupt",
        "claude_user_message.json",
    );
    let authority_digest = journal.authority_digest;
    let conn = Connection::open(&path).expect("reopen raw");
    conn.execute(
        "UPDATE semantic_journal_facts SET delivery_id = ?1
         WHERE authority_digest = ?2 AND sequence = 1",
        rusqlite::params!["relay_\u{202e}open-corrupt", authority_digest.as_slice()],
    )
    .expect("corrupt persisted delivery id");
    drop(conn);
    drop(journal);

    assert!(matches!(
        SemanticJournal::open(
            &path,
            &test_permit(ProviderKind::ClaudeCode, "session_reopen"),
            JournalLimits::default(),
            NOW_MS,
        ),
        Err(JournalError::Store)
    ));
}

#[test]
fn journal_sequence_hole_fails_closed() {
    let (_dir, _path, mut journal) = temp_journal(ProviderKind::ClaudeCode);
    ingest_named(
        &mut journal,
        ProviderKind::ClaudeCode,
        "relay_claude_user_1",
        "claude_user_message.json",
    );
    ingest_named(
        &mut journal,
        ProviderKind::ClaudeCode,
        "relay_claude_tool_1",
        "claude_tool_call.json",
    );
    journal.debug_delete_sequence(1);
    assert!(
        matches!(
            journal.projected_page(0, None, PageLimits::new(8, 8 * 1024).expect("limits")),
            Err(JournalIngestOutcome::NeedsResync)
        ),
        "sequence hole must fail closed"
    );
}

#[test]
fn journal_ingest_after_sequence_hole_fails_closed() {
    let (_dir, _path, mut journal) = temp_journal(ProviderKind::ClaudeCode);
    ingest_named(
        &mut journal,
        ProviderKind::ClaudeCode,
        "relay_claude_user_1",
        "claude_user_message.json",
    );
    ingest_named(
        &mut journal,
        ProviderKind::ClaudeCode,
        "relay_claude_tool_1",
        "claude_tool_call.json",
    );
    journal.debug_delete_sequence(1);

    assert!(matches!(
        journal.ingest(
            test_permit(ProviderKind::ClaudeCode, "relay_after_hole"),
            &load_bytes("cursor_usage.json"),
            NOW_MS,
        ),
        JournalIngestOutcome::NeedsResync
    ));
}

#[test]
fn journal_open_rejects_limits_above_hard_maxima() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("kernel.sqlite3");
    let oversized = JournalLimits {
        max_events: MAX_JOURNAL_EVENTS + 1,
        max_dedupe_keys: MAX_JOURNAL_DEDUPE_KEYS,
        max_ingest_steps: DEFAULT_MAX_INGEST_STEPS,
    };
    match SemanticJournal::open(
        &path,
        &test_permit(ProviderKind::ClaudeCode, "session_open"),
        oversized,
        NOW_MS,
    ) {
        Err(err) => assert_eq!(err, JournalError::Oversized),
        Ok(_) => panic!("oversized journal limits must fail closed"),
    }
}

#[test]
fn journal_never_persists_never_persist_text() {
    let event = JournalEvent {
        id: EventId::new(),
        schema_version: JOURNAL_SCHEMA_VERSION,
        provider: ProviderKind::ClaudeCode,
        provider_event_id: None,
        delivery_id: RelayDeliveryId::new("relay_never").expect("delivery"),
        task_id: ids().0,
        agent_session_id: ids().1,
        resource_id: ids().2,
        runtime_generation: 1,
        action_epoch: 1,
        sequence: 1,
        kind: JournalSemanticKind::UserMessage,
        occurred_at_ms: NOW_MS,
        ingested_at_ms: NOW_MS,
        visibility: JournalVisibility::Semantic,
        redaction_class: JournalRedactionClass::NeverPersist,
        privacy_class: PrivacyClass::LocalOnly,
        text: Some("NEVER_PERSIST_SECRET".into()),
        extensions: BTreeMap::new(),
        unknown: None,
        payload: SemanticJournalPayload::UserMessage {
            text: "NEVER_PERSIST_SECRET".into(),
        },
        payload_hash: [0x44; 32],
    };
    assert!(persist_row(&event).expect("persist").is_none());
    assert!(event.projected_text().is_none());
}

#[test]
fn journal_never_persist_is_ignored_without_receipt_or_projection() {
    let (_dir, _path, mut journal) = temp_journal(ProviderKind::ClaudeCode);
    let event = JournalEvent {
        id: EventId::new(),
        schema_version: JOURNAL_SCHEMA_VERSION,
        provider: ProviderKind::ClaudeCode,
        provider_event_id: None,
        delivery_id: RelayDeliveryId::new("relay_never_commit").expect("delivery"),
        task_id: ids().0,
        agent_session_id: ids().1,
        resource_id: ids().2,
        runtime_generation: 1,
        action_epoch: 1,
        sequence: 1,
        kind: JournalSemanticKind::UserMessage,
        occurred_at_ms: NOW_MS,
        ingested_at_ms: NOW_MS,
        visibility: JournalVisibility::Semantic,
        redaction_class: JournalRedactionClass::NeverPersist,
        privacy_class: PrivacyClass::LocalOnly,
        text: Some("secret".into()),
        extensions: BTreeMap::new(),
        unknown: None,
        payload: SemanticJournalPayload::UserMessage {
            text: "secret".into(),
        },
        payload_hash: [0x55; 32],
    };
    let draft = JournalDraft::sealed(
        event,
        &journal.instance_secret,
        &journal.authority,
        journal.authority_digest,
        [0x33; 16],
        NOW_MS - 1_000,
        NOW_MS + 60_000,
        journal.store_id,
    );
    assert!(matches!(
        journal.commit(draft),
        JournalIngestOutcome::IgnoredNeverPersist
    ));
    assert_eq!(journal.retained_len().expect("retained"), 0);
    let page = journal
        .projected_page(0, None, PageLimits::new(8, 8 * 1024).expect("limits"))
        .expect("empty projection");
    assert!(page.facts.is_empty());
}

#[test]
fn journal_deadline_overflow_returns_needs_resync() {
    let (_dir, _path, mut journal) = temp_journal(ProviderKind::ClaudeCode);
    let outcome = journal.ingest_until(
        test_permit(ProviderKind::ClaudeCode, "relay_deadline"),
        &load_bytes("claude_user_message.json"),
        NOW_MS,
        Instant::now() - Duration::from_secs(1),
    );
    assert!(matches!(outcome, JournalIngestOutcome::NeedsResync));
}
