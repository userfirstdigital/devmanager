use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use rusqlite::Connection;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

use crate::domain::id::{
    AgentSessionId, CommandId, EventId, PromptChainId, PromptChainLinkId, PromptHistoryId,
    PromptId, PromptVersionId, RequestId, TaskId,
};
use crate::prompts::{
    CreatePrompt, CreatePromptChain, InsertPromptChainLink, PromptChainCommand, PromptCommand,
    PromptStore, RemovePromptChainLink, RenamePrompt, SetPromptTags,
};

use super::history_testing::{self, PromptHistoryAttempt};
use super::{
    PromptHistoryErrorCode, PromptHistoryPolicy, PromptHistoryProvenance, PromptHistoryStore,
    PromptSearchBudget, PromptSearchQuery, PromptSearchSource, PromptSearchStatus,
    ValidatedDeliveredInputProof, DEFAULT_HISTORY_INDEX_CAPACITY, MAX_PROMPT_SEARCH_PAGE,
    MAX_PROMPT_SEARCH_QUERY_BYTES, MAX_PROMPT_SEARCH_TERMS,
};

fn uuid_tail(kind: u8, tail: u8) -> [u8; 16] {
    [
        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, kind, 0x00,
        tail,
    ]
}

fn history_id(tail: u8) -> PromptHistoryId {
    PromptHistoryId::from_bytes(uuid_tail(0x10, tail)).expect("history id")
}
fn request_id(tail: u8) -> RequestId {
    RequestId::from_bytes(uuid_tail(0x11, tail)).expect("request id")
}
fn event_id(tail: u8) -> EventId {
    EventId::from_bytes(uuid_tail(0x12, tail)).expect("event id")
}
fn task_id(tail: u8) -> TaskId {
    TaskId::from_bytes(uuid_tail(0x13, tail)).expect("task id")
}
fn session_id(tail: u8) -> AgentSessionId {
    AgentSessionId::from_bytes(uuid_tail(0x14, tail)).expect("session id")
}
fn prompt_id(tail: u8) -> PromptId {
    PromptId::from_bytes(uuid_tail(0x15, tail)).expect("prompt id")
}
fn version_id(tail: u8) -> PromptVersionId {
    PromptVersionId::from_bytes(uuid_tail(0x16, tail)).expect("version id")
}
fn chain_id(tail: u8) -> PromptChainId {
    PromptChainId::from_bytes(uuid_tail(0x17, tail)).expect("chain id")
}
fn link_id(tail: u8) -> PromptChainLinkId {
    PromptChainLinkId::from_bytes(uuid_tail(0x18, tail)).expect("link id")
}
fn command_id(tail: u8) -> CommandId {
    CommandId::from_bytes(uuid_tail(0x19, tail)).expect("command id")
}

fn accepted(
    tail: u8,
    body: &str,
    at_ms: i64,
    provenance: PromptHistoryProvenance,
) -> PromptHistoryAttempt {
    PromptHistoryAttempt::AcceptedForDelivery {
        history_id: history_id(tail),
        request_id: request_id(tail),
        submitted_event_id: event_id(tail),
        task_id: task_id(1),
        agent_session_id: session_id(1),
        provider_kind: "claude".into(),
        body: body.into(),
        accepted_at_ms: at_ms,
        provenance,
    }
}

fn open_pair(dir: &TempDir) -> (PromptStore, PromptHistoryStore) {
    let path = dir.path().join("prompts.sqlite3");
    let prompts = PromptStore::open(&path).expect("open prompt store");
    let history = PromptHistoryStore::open(&path).expect("open history store");
    (prompts, history)
}

fn recent_bodies(history: &PromptHistoryStore) -> Vec<String> {
    history
        .recent(10, None)
        .expect("recent")
        .entries
        .into_iter()
        .map(|entry| entry.body)
        .collect()
}

#[test]
fn production_provider_input_wiring_is_unavailable() {
    let error = ValidatedDeliveredInputProof::from_provider_input_settlement()
        .expect_err("provider input is not in this base");
    assert_eq!(
        error.code(),
        PromptHistoryErrorCode::ProviderInputUnavailable
    );
    let rendered = format!("{error} {error:?}");
    assert!(!rendered.contains("sqlite"));
    assert!(!rendered.contains("SELECT"));
    assert!(!rendered.contains("prompt"));
    assert!(!rendered.contains('\\') && !rendered.contains('/'));
}

#[test]
fn delivered_user_prompt_enters_history() {
    let dir = TempDir::new().expect("tempdir");
    let (mut prompts, mut history) = open_pair(&dir);
    prompts
        .execute(
            command_id(1),
            PromptCommand::CreatePrompt(CreatePrompt {
                prompt_id: prompt_id(1),
                prompt_version_id: version_id(1),
                title: "Review".into(),
                description: None,
                tags: vec!["review".into()],
                variables: Vec::new(),
                body: "Review this carefully.".into(),
                created_at_ms: 1_000,
            }),
        )
        .expect("create saved prompt");

    let recorded = history_testing::commit_delivered(
        &mut history,
        accepted(
            1,
            "Review this carefully.",
            2_000,
            PromptHistoryProvenance {
                prompt_id: Some(prompt_id(1)),
                prompt_version_id: Some(version_id(1)),
                chain_id: None,
                chain_link_id: None,
            },
        ),
    )
    .expect("record accepted delivery")
    .expect("accepted delivery persisted");

    assert_eq!(recorded.history_id, history_id(1));
    assert_eq!(recorded.request_id, request_id(1));
    let page = history.recent(10, None).expect("recent");
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].body, "Review this carefully.");
    assert_eq!(page.entries[0].prompt_version_id, Some(version_id(1)));
    assert_eq!(page.entries[0].request_id, request_id(1));
}

#[test]
fn delivery_and_history_share_caller_settlement_transaction() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("prompts.sqlite3");
    let mut history = PromptHistoryStore::open(&path).expect("open");

    history_testing::apply_then_rollback(
        &mut history,
        accepted(
            1,
            "will roll back",
            1_000,
            PromptHistoryProvenance::default(),
        ),
    )
    .expect("rollback staged delivery");
    assert!(history.recent(10, None).expect("recent").entries.is_empty());

    history_testing::commit_delivered(
        &mut history,
        accepted(
            2,
            "committed delivery",
            2_000,
            PromptHistoryProvenance::default(),
        ),
    )
    .expect("commit accepted delivery");
    assert_eq!(history.recent(10, None).expect("recent").entries.len(), 1);
}

#[test]
fn host_crash_after_delivery_preserves_history() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("prompts.sqlite3");
    {
        let mut history = PromptHistoryStore::open(&path).expect("open");
        history_testing::commit_delivered(
            &mut history,
            accepted(
                1,
                "survives crash",
                3_000,
                PromptHistoryProvenance::default(),
            ),
        )
        .expect("record");
    }

    let history = PromptHistoryStore::open(&path).expect("reopen after crash");
    let page = history.recent(10, None).expect("recent after reopen");
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].body, "survives crash");
    assert_eq!(page.entries[0].request_id, request_id(1));
}

#[test]
fn failed_cancelled_synthetic_and_provider_internal_do_not() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("prompts.sqlite3");
    let mut history = PromptHistoryStore::open(&path).expect("open");

    for attempt in [
        PromptHistoryAttempt::RejectedDraft {
            request_id: request_id(2),
            body: "draft".into(),
        },
        PromptHistoryAttempt::Failed {
            request_id: request_id(3),
            body: "failed".into(),
        },
        PromptHistoryAttempt::Cancelled {
            request_id: request_id(4),
            body: "cancelled".into(),
        },
        PromptHistoryAttempt::Synthetic {
            request_id: request_id(5),
            body: "synthetic".into(),
        },
        PromptHistoryAttempt::ProviderInternal {
            request_id: request_id(6),
            body: "internal".into(),
        },
        PromptHistoryAttempt::RawTerminal {
            request_id: request_id(7),
            body: "raw pty".into(),
        },
        PromptHistoryAttempt::Secret {
            request_id: request_id(8),
            body: "sk-secret".into(),
        },
    ] {
        assert!(
            history_testing::commit_delivered(&mut history, attempt)
                .expect("refuse non-delivery")
                .is_none(),
            "non-accepted attempt must not persist"
        );
    }
    assert!(history.recent(10, None).expect("recent").entries.is_empty());
}

#[test]
fn same_event_same_payload_is_duplicate_not_a_second_row() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("prompts.sqlite3");
    let mut history = PromptHistoryStore::open(&path).expect("open");

    let first = history_testing::commit_delivered(
        &mut history,
        accepted(1, "same request", 1_000, PromptHistoryProvenance::default()),
    )
    .expect("first")
    .expect("accepted");
    let second = history_testing::commit_delivered(
        &mut history,
        accepted(1, "same request", 1_000, PromptHistoryProvenance::default()),
    )
    .expect("duplicate")
    .expect("idempotent accepted");

    assert_eq!(first.history_id, second.history_id);
    assert_eq!(history.recent(10, None).expect("recent").entries.len(), 1);
    history
        .drain_index(PromptSearchBudget::default())
        .expect("drain");
    let page = history
        .search(
            &PromptSearchQuery {
                text: "same".into(),
                source: PromptSearchSource::History,
                cursor: None,
                page_size: 10,
            },
            PromptSearchBudget::default(),
        )
        .expect("search");
    assert_eq!(page.hits.len(), 1);
}

#[test]
fn same_request_different_body_is_conflict() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("prompts.sqlite3");
    let mut history = PromptHistoryStore::open(&path).expect("open");
    history_testing::commit_delivered(
        &mut history,
        accepted(
            1,
            "original body",
            1_000,
            PromptHistoryProvenance::default(),
        ),
    )
    .expect("first");

    let mut replay = accepted(1, "changed body", 1_000, PromptHistoryProvenance::default());
    if let PromptHistoryAttempt::AcceptedForDelivery { body, .. } = &mut replay {
        *body = "changed body".into();
    }
    let error = history_testing::commit_delivered(&mut history, replay)
        .expect_err("different body must conflict");
    assert_eq!(error.code(), PromptHistoryErrorCode::Conflict);
    assert_eq!(recent_bodies(&history), vec!["original body".to_string()]);
}

#[test]
fn same_event_different_task_provider_or_provenance_is_conflict() {
    let dir = TempDir::new().expect("tempdir");
    let (mut prompts, mut history) = open_pair(&dir);
    prompts
        .execute(
            command_id(1),
            PromptCommand::CreatePrompt(CreatePrompt {
                prompt_id: prompt_id(1),
                prompt_version_id: version_id(1),
                title: "Pinned".into(),
                description: None,
                tags: Vec::new(),
                variables: Vec::new(),
                body: "version one".into(),
                created_at_ms: 1_000,
            }),
        )
        .expect("saved");
    history_testing::commit_delivered(
        &mut history,
        accepted(
            1,
            "version one",
            2_000,
            PromptHistoryProvenance {
                prompt_id: Some(prompt_id(1)),
                prompt_version_id: Some(version_id(1)),
                chain_id: None,
                chain_link_id: None,
            },
        ),
    )
    .expect("first");

    let mut different_task = accepted(1, "version one", 2_000, PromptHistoryProvenance::default());
    if let PromptHistoryAttempt::AcceptedForDelivery { task_id: id, .. } = &mut different_task {
        *id = task_id(9);
    }
    assert_eq!(
        history_testing::commit_delivered(&mut history, different_task)
            .expect_err("task mismatch")
            .code(),
        PromptHistoryErrorCode::Conflict
    );

    let mut different_provider = accepted(
        1,
        "version one",
        2_000,
        PromptHistoryProvenance {
            prompt_id: Some(prompt_id(1)),
            prompt_version_id: Some(version_id(1)),
            chain_id: None,
            chain_link_id: None,
        },
    );
    if let PromptHistoryAttempt::AcceptedForDelivery { provider_kind, .. } = &mut different_provider
    {
        *provider_kind = "codex".into();
    }
    assert_eq!(
        history_testing::commit_delivered(&mut history, different_provider)
            .expect_err("provider mismatch")
            .code(),
        PromptHistoryErrorCode::Conflict
    );

    let different_provenance =
        accepted(1, "version one", 2_000, PromptHistoryProvenance::default());
    assert_eq!(
        history_testing::commit_delivered(&mut history, different_provenance)
            .expect_err("provenance mismatch")
            .code(),
        PromptHistoryErrorCode::Conflict
    );
    assert_eq!(history.recent(10, None).expect("recent").entries.len(), 1);
}

#[test]
fn saved_and_history_search_are_distinct() {
    let dir = TempDir::new().expect("tempdir");
    let (mut prompts, mut history) = open_pair(&dir);
    prompts
        .execute(
            command_id(1),
            PromptCommand::CreatePrompt(CreatePrompt {
                prompt_id: prompt_id(1),
                prompt_version_id: version_id(1),
                title: "Saved title".into(),
                description: None,
                tags: vec!["savedtag".into()],
                variables: Vec::new(),
                body: "shared token lives in the library".into(),
                created_at_ms: 1_000,
            }),
        )
        .expect("create saved");
    history_testing::commit_delivered(
        &mut history,
        accepted(
            1,
            "shared token was submitted",
            2_000,
            PromptHistoryProvenance::default(),
        ),
    )
    .expect("record history");
    while history
        .rebuild_search(PromptSearchBudget::default())
        .expect("rebuild")
        .remaining
        > 0
    {}

    let saved = history
        .search(
            &PromptSearchQuery {
                text: "shared".into(),
                source: PromptSearchSource::Saved,
                cursor: None,
                page_size: 10,
            },
            PromptSearchBudget::default(),
        )
        .expect("saved search");
    let hist = history
        .search(
            &PromptSearchQuery {
                text: "shared".into(),
                source: PromptSearchSource::History,
                cursor: None,
                page_size: 10,
            },
            PromptSearchBudget::default(),
        )
        .expect("history search");
    assert_eq!(saved.hits.len(), 1);
    assert_eq!(saved.hits[0].source, PromptSearchSource::Saved);
    assert_eq!(hist.hits.len(), 1);
    assert_eq!(hist.hits[0].source, PromptSearchSource::History);
}

#[test]
fn unicode_prefix_phrase_and_tag_search() {
    let dir = TempDir::new().expect("tempdir");
    let (mut prompts, mut history) = open_pair(&dir);
    prompts
        .execute(
            command_id(1),
            PromptCommand::CreatePrompt(CreatePrompt {
                prompt_id: prompt_id(1),
                prompt_version_id: version_id(1),
                title: "Cafe notes".into(),
                description: None,
                tags: vec!["resume".into()],
                variables: Vec::new(),
                body: "unrelated saved body".into(),
                created_at_ms: 1_000,
            }),
        )
        .expect("create tagged prompt");
    history_testing::commit_delivered(
        &mut history,
        accepted(
            1,
            "Café résumé for the Tokyo office",
            2_000,
            PromptHistoryProvenance::default(),
        ),
    )
    .expect("unicode history");
    history_testing::commit_delivered(
        &mut history,
        accepted(
            2,
            "plain cafe later",
            3_000,
            PromptHistoryProvenance::default(),
        ),
    )
    .expect("ascii history");
    while history
        .rebuild_search(PromptSearchBudget::default())
        .expect("rebuild")
        .remaining
        > 0
    {}

    let prefix = history
        .search(
            &PromptSearchQuery {
                text: "caf*".into(),
                source: PromptSearchSource::History,
                cursor: None,
                page_size: 10,
            },
            PromptSearchBudget::default(),
        )
        .expect("prefix");
    assert_eq!(prefix.hits.len(), 2, "unicode61 must fold Café and cafe");

    let phrase = history
        .search(
            &PromptSearchQuery {
                text: "\"Tokyo office\"".into(),
                source: PromptSearchSource::History,
                cursor: None,
                page_size: 10,
            },
            PromptSearchBudget::default(),
        )
        .expect("phrase");
    assert_eq!(phrase.hits.len(), 1);
    assert!(phrase.hits[0].highlights.iter().any(|range| {
        range.end > range.start
            && phrase.hits[0].body.is_char_boundary(range.start)
            && phrase.hits[0].body.is_char_boundary(range.end)
    }));

    let tagged = history
        .search(
            &PromptSearchQuery {
                text: "tag:resume".into(),
                source: PromptSearchSource::Saved,
                cursor: None,
                page_size: 10,
            },
            PromptSearchBudget::default(),
        )
        .expect("tag");
    assert_eq!(tagged.hits.len(), 1);
    assert_eq!(tagged.hits[0].source, PromptSearchSource::Saved);
}

#[test]
fn disabled_history_writes_nothing() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("prompts.sqlite3");
    let mut history = PromptHistoryStore::open(&path).expect("open");
    let preview = history.preview_policy().expect("preview");
    history
        .set_policy(
            PromptHistoryPolicy {
                enabled: false,
                retention_days: 90,
                max_entries: 10_000,
            },
            preview.confirmation(),
        )
        .expect("disable");
    assert!(history_testing::commit_delivered(
        &mut history,
        accepted(
            1,
            "should not persist",
            1_000,
            PromptHistoryProvenance::default(),
        ),
    )
    .expect("disabled write")
    .is_none());
    assert!(history.recent(10, None).expect("recent").entries.is_empty());
}

#[test]
fn retention_removes_history_not_task_fact() {
    let dir = TempDir::new().expect("tempdir");
    let (mut prompts, mut history) = open_pair(&dir);
    prompts
        .execute(
            command_id(1),
            PromptCommand::CreatePrompt(CreatePrompt {
                prompt_id: prompt_id(1),
                prompt_version_id: version_id(1),
                title: "Keep me".into(),
                description: None,
                tags: Vec::new(),
                variables: Vec::new(),
                body: "saved fact stays".into(),
                created_at_ms: 1_000,
            }),
        )
        .expect("saved prompt");
    history_testing::commit_delivered(
        &mut history,
        accepted(
            1,
            "old submission",
            1_000,
            PromptHistoryProvenance::default(),
        ),
    )
    .expect("old");
    history_testing::commit_delivered(
        &mut history,
        accepted(
            2,
            "fresh submission",
            172_000_000,
            PromptHistoryProvenance::default(),
        ),
    )
    .expect("fresh");
    let preview = history.preview_policy().expect("policy preview");
    history
        .set_policy(
            PromptHistoryPolicy {
                enabled: true,
                retention_days: 1,
                max_entries: 100,
            },
            preview.confirmation(),
        )
        .expect("tighten retention");
    let preview = history
        .preview_retention(1_000 + 2 * 86_400_000)
        .expect("preview");
    assert_eq!(preview.expired, 1);
    let removed = history
        .apply_retention(
            1_000 + 2 * 86_400_000,
            preview.confirmation(),
            PromptSearchBudget::default(),
        )
        .expect("retain");
    assert_eq!(removed.removed, 1);
    assert!(removed.done);
    let page = history.recent(10, None).expect("recent");
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].body, "fresh submission");
    assert!(prompts
        .get_prompt(prompt_id(1))
        .expect("saved prompt remains")
        .is_some());
    assert_eq!(prompts.count_prompt_events().expect("events remain"), 1);
}

#[test]
fn pending_index_intent_survives_commit_without_memory_enqueue() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("prompts.sqlite3");
    {
        let mut history = PromptHistoryStore::open(&path).expect("open");
        history_testing::commit_delivered(
            &mut history,
            accepted(
                1,
                "pending after commit",
                1_000,
                PromptHistoryProvenance::default(),
            ),
        )
        .expect("commit");
        assert!(history.is_search_dirty().expect("dirty after commit"));
    }
    let mut history = PromptHistoryStore::open(&path).expect("reopen");
    assert!(history.is_search_dirty().expect("dirty survives reopen"));
    let drained = history
        .drain_index(PromptSearchBudget::default())
        .expect("drain after reopen");
    assert_eq!(drained.processed, 1);
    let page = history
        .search(
            &PromptSearchQuery {
                text: "pending".into(),
                source: PromptSearchSource::History,
                cursor: None,
                page_size: 10,
            },
            PromptSearchBudget::default(),
        )
        .expect("search");
    assert_eq!(page.hits.len(), 1);
}

#[test]
fn fts_rebuild_is_paged_and_stays_dirty_until_parity() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("prompts.sqlite3");
    let mut history = PromptHistoryStore::open(&path).expect("open");
    for tail in 1_u8..=51 {
        history_testing::commit_delivered(
            &mut history,
            accepted(
                tail,
                &format!("rebuild token {tail}"),
                i64::from(tail) * 1_000,
                PromptHistoryProvenance::default(),
            ),
        )
        .expect("seed");
    }
    let first = history
        .rebuild_search(PromptSearchBudget::default())
        .expect("first page");
    assert!(first.processed <= 50);
    assert!(first.remaining > 0);
    assert!(history.is_search_dirty().expect("dirty mid rebuild"));
    let mut steps = 1;
    while history
        .rebuild_search(PromptSearchBudget::default())
        .expect("continue")
        .remaining
        > 0
    {
        steps += 1;
        assert!(steps < 8, "rebuild must finish in bounded pages");
    }
    assert!(!history.is_search_dirty().expect("clean after parity"));
}

#[test]
fn immutable_version_and_chain_provenance_is_exact() {
    let dir = TempDir::new().expect("tempdir");
    let (mut prompts, mut history) = open_pair(&dir);
    prompts
        .execute(
            command_id(1),
            PromptCommand::CreatePrompt(CreatePrompt {
                prompt_id: prompt_id(1),
                prompt_version_id: version_id(1),
                title: "Chain source".into(),
                description: None,
                tags: Vec::new(),
                variables: Vec::new(),
                body: "version one".into(),
                created_at_ms: 1_000,
            }),
        )
        .expect("create prompt");
    prompts
        .execute_chain(
            command_id(2),
            PromptChainCommand::CreatePromptChain(CreatePromptChain {
                chain_id: chain_id(1),
                title: "Guide".into(),
                description: None,
                created_at_ms: 1_100,
            }),
        )
        .expect("create chain");
    prompts
        .execute_chain(
            command_id(3),
            PromptChainCommand::InsertPromptChainLink(InsertPromptChainLink {
                chain_id: chain_id(1),
                link_id: link_id(1),
                prompt_id: prompt_id(1),
                prompt_version_id: None,
                before_link_id: None,
                expected_revision: 1,
            }),
        )
        .expect("pin current version");

    history_testing::commit_delivered(
        &mut history,
        accepted(
            1,
            "version one",
            2_000,
            PromptHistoryProvenance {
                prompt_id: Some(prompt_id(1)),
                prompt_version_id: Some(version_id(1)),
                chain_id: Some(chain_id(1)),
                chain_link_id: Some(link_id(1)),
            },
        ),
    )
    .expect("record pinned provenance");

    let mismatched = history_testing::commit_delivered(
        &mut history,
        accepted(
            2,
            "version one",
            3_000,
            PromptHistoryProvenance {
                prompt_id: Some(prompt_id(1)),
                prompt_version_id: Some(version_id(9)),
                chain_id: Some(chain_id(1)),
                chain_link_id: Some(link_id(1)),
            },
        ),
    );
    assert!(mismatched.is_err(), "foreign version must fail closed");
    let page = history.recent(10, None).expect("recent");
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].prompt_version_id, Some(version_id(1)));
    assert_eq!(page.entries[0].chain_link_id, Some(link_id(1)));
}

#[test]
fn search_honors_byte_term_page_and_cancellation_caps() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("prompts.sqlite3");
    let mut history = PromptHistoryStore::open(&path).expect("open");
    for tail in 1_u8..=5 {
        history_testing::commit_delivered(
            &mut history,
            accepted(
                tail,
                "repeatable token for paging",
                i64::from(tail) * 1_000,
                PromptHistoryProvenance::default(),
            ),
        )
        .expect("seed");
    }
    while history
        .rebuild_search(PromptSearchBudget::default())
        .expect("index")
        .remaining
        > 0
    {}

    let hundred_k = history.search(
        &PromptSearchQuery {
            text: "x".repeat(100_000),
            source: PromptSearchSource::History,
            cursor: None,
            page_size: 10,
        },
        PromptSearchBudget::default(),
    );
    assert_eq!(
        hundred_k.expect_err("100k query").code(),
        PromptHistoryErrorCode::QueryTooLong
    );

    let oversized = history.search(
        &PromptSearchQuery {
            text: "x".repeat(MAX_PROMPT_SEARCH_QUERY_BYTES + 1),
            source: PromptSearchSource::History,
            cursor: None,
            page_size: 10,
        },
        PromptSearchBudget::default(),
    );
    assert_eq!(
        oversized.expect_err("byte cap").code(),
        PromptHistoryErrorCode::QueryTooLong
    );

    let expanding = "İ".repeat(300);
    assert!(expanding.len() > MAX_PROMPT_SEARCH_QUERY_BYTES);
    assert_eq!(
        history
            .search(
                &PromptSearchQuery {
                    text: expanding,
                    source: PromptSearchSource::History,
                    cursor: None,
                    page_size: 10,
                },
                PromptSearchBudget::default(),
            )
            .expect_err("unicode expansion counted before fold")
            .code(),
        PromptHistoryErrorCode::QueryTooLong
    );

    let too_many_terms = history.search(
        &PromptSearchQuery {
            text: (0..=MAX_PROMPT_SEARCH_TERMS)
                .map(|i| format!("term{i}"))
                .collect::<Vec<_>>()
                .join(" "),
            source: PromptSearchSource::History,
            cursor: None,
            page_size: 10,
        },
        PromptSearchBudget::default(),
    );
    assert_eq!(
        too_many_terms.expect_err("term cap").code(),
        PromptHistoryErrorCode::InvalidQuery
    );

    let too_wide = history.search(
        &PromptSearchQuery {
            text: "repeatable".into(),
            source: PromptSearchSource::History,
            cursor: None,
            page_size: MAX_PROMPT_SEARCH_PAGE + 1,
        },
        PromptSearchBudget::default(),
    );
    assert_eq!(
        too_wide.expect_err("page cap").code(),
        PromptHistoryErrorCode::PageTooLarge
    );

    let paged = history
        .search(
            &PromptSearchQuery {
                text: "repeatable".into(),
                source: PromptSearchSource::History,
                cursor: None,
                page_size: 2,
            },
            PromptSearchBudget::default(),
        )
        .expect("page");
    assert_eq!(paged.hits.len(), 2);
    assert!(paged.hits[0].submitted_at_ms >= paged.hits[1].submitted_at_ms);
    let next = history
        .search(
            &PromptSearchQuery {
                text: "repeatable".into(),
                source: PromptSearchSource::History,
                cursor: paged.next.clone(),
                page_size: 2,
            },
            PromptSearchBudget::default(),
        )
        .expect("keyset page");
    assert!(!next.hits.is_empty());
    assert_ne!(next.hits[0].history_id, paged.hits[0].history_id);

    let cancelled = AtomicBool::new(true);
    let stopped = history.search(
        &PromptSearchQuery {
            text: "repeatable".into(),
            source: PromptSearchSource::History,
            cursor: None,
            page_size: 10,
        },
        PromptSearchBudget::default()
            .with_cancellation(&cancelled)
            .with_deadline(Instant::now() + Duration::from_secs(1)),
    );
    assert_eq!(
        stopped.expect_err("cancelled").code(),
        PromptHistoryErrorCode::Cancelled
    );
}

#[test]
fn recent_uses_keyset_and_rejects_unbounded_limit() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("prompts.sqlite3");
    let mut history = PromptHistoryStore::open(&path).expect("open");
    for tail in 1_u8..=5 {
        history_testing::commit_delivered(
            &mut history,
            accepted(
                tail,
                &format!("row {tail}"),
                i64::from(tail) * 1_000,
                PromptHistoryProvenance::default(),
            ),
        )
        .expect("seed");
    }
    assert_eq!(
        history
            .recent(MAX_PROMPT_SEARCH_PAGE + 1, None)
            .expect_err("cap+1")
            .code(),
        PromptHistoryErrorCode::PageTooLarge
    );
    let first = history.recent(2, None).expect("first page");
    assert_eq!(first.entries.len(), 2);
    let second = history.recent(2, first.next).expect("keyset").entries;
    assert_eq!(second.len(), 2);
    assert_ne!(second[0].history_id, first.entries[0].history_id);
}

#[test]
fn retention_is_previewed_paged_and_cancellable() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("prompts.sqlite3");
    let mut history = PromptHistoryStore::open(&path).expect("open");
    for tail in 1_u8..=51 {
        history_testing::commit_delivered(
            &mut history,
            accepted(
                tail,
                "old enough",
                1_000,
                PromptHistoryProvenance::default(),
            ),
        )
        .expect("seed");
    }
    let preview = history.preview_policy().expect("policy");
    history
        .set_policy(
            PromptHistoryPolicy {
                enabled: true,
                retention_days: 1,
                max_entries: 100,
            },
            preview.confirmation(),
        )
        .expect("policy");
    let now = 1_000 + 2 * 86_400_000;
    let preview = history.preview_retention(now).expect("preview");
    assert_eq!(preview.expired, 51);
    assert_eq!(
        history
            .apply_retention(now, preview.confirmation(), PromptSearchBudget::default())
            .expect("first batch")
            .removed,
        50
    );
    assert_eq!(
        history.recent(100, None).expect("remaining").entries.len(),
        1
    );
    let cancelled = AtomicBool::new(true);
    let preview = history.preview_retention(now).expect("second preview");
    assert_eq!(
        history
            .apply_retention(
                now,
                preview.confirmation(),
                PromptSearchBudget::default().with_cancellation(&cancelled),
            )
            .expect_err("cancelled retention")
            .code(),
        PromptHistoryErrorCode::Cancelled
    );
    let preview = history.preview_retention(now).expect("resume preview");
    let finished = history
        .apply_retention(now, preview.confirmation(), PromptSearchBudget::default())
        .expect("resume");
    assert!(finished.done);
    assert!(history
        .recent(10, None)
        .expect("cleared")
        .entries
        .is_empty());
}

#[test]
fn index_error_leaves_dirty_and_rebuild_resumes() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("prompts.sqlite3");
    let mut history = PromptHistoryStore::open(&path).expect("open");
    history_testing::commit_delivered(
        &mut history,
        accepted(1, "index me", 1_000, PromptHistoryProvenance::default()),
    )
    .expect("commit");
    history_testing::force_next_index_error(&mut history);
    assert_eq!(
        history
            .drain_index(PromptSearchBudget::default())
            .expect_err("forced index error")
            .code(),
        PromptHistoryErrorCode::Storage
    );
    assert!(history.is_search_dirty().expect("dirty after error"));
    drop(history);
    let mut history = PromptHistoryStore::open(&path).expect("reopen dirty");
    assert!(history.is_search_dirty().expect("dirty after restart"));
    while history
        .rebuild_search(PromptSearchBudget::default())
        .expect("rebuild")
        .remaining
        > 0
    {}
    assert!(!history.is_search_dirty().expect("clean"));
}

#[test]
fn reopen_applies_history_migration_and_keeps_rows() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("prompts.sqlite3");
    {
        let mut history = PromptHistoryStore::open(&path).expect("open");
        history_testing::commit_delivered(
            &mut history,
            accepted(1, "migrated row", 4_000, PromptHistoryProvenance::default()),
        )
        .expect("record");
        assert!(history.schema_has_history_and_search().expect("schema"));
    }
    let history = PromptHistoryStore::open(&path).expect("reopen");
    assert!(history
        .schema_has_history_and_search()
        .expect("reopen schema"));
    assert_eq!(
        history.recent(1, None).expect("recent").entries[0].body,
        "migrated row"
    );
}

#[test]
fn corrupt_and_oversized_rows_fail_closed_without_leaking() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("prompts.sqlite3");
    let history = PromptHistoryStore::open(&path).expect("open");
    drop(history);
    let conn = Connection::open(&path).expect("raw");
    let oversized = "x".repeat(262_145);
    let rejected = conn.execute(
        "INSERT INTO prompt_history(
            prompt_history_id, request_id, submitted_event_id, task_id,
            agent_session_id, provider_kind, body, body_sha256, submitted_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, 'claude', ?6, ?7, -1)",
        rusqlite::params![
            history_id(1).as_bytes().as_slice(),
            request_id(1).as_bytes().as_slice(),
            event_id(1).as_bytes().as_slice(),
            task_id(1).as_bytes().as_slice(),
            session_id(1).as_bytes().as_slice(),
            oversized,
            &[0u8; 32][..],
        ],
    );
    assert!(
        rejected.is_err(),
        "sqlite must reject oversized/negative time"
    );
    let history = PromptHistoryStore::open(&path).expect("reopen");
    assert!(history.recent(10, None).expect("empty").entries.is_empty());
}

#[test]
fn phase07_v1_cannot_absorb_history_after_v8_v9_lineage() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("prior-v9.sqlite3");
    {
        let _history = PromptHistoryStore::open(&path).expect("open current");
    }
    let conn = Connection::open(&path).expect("raw");
    let v7: String = conn
        .query_row(
            "SELECT name FROM schema_migrations WHERE version = 7",
            [],
            |row| row.get(0),
        )
        .expect("v7");
    assert_eq!(v7, "phase07-prompts-v1");
    let v9_commitment: i64 = conn
        .query_row(
            "SELECT migration_version FROM prompt_lineage_migration_commitment
             WHERE singleton_key = 1",
            [],
            |row| row.get(0),
        )
        .expect("v9 pins lineage version 9");
    assert_eq!(v9_commitment, 9);
    let latest: (i64, String) = conn
        .query_row(
            "SELECT version, name FROM schema_migrations ORDER BY version DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("latest");
    assert_eq!(latest, (10, "phase07-prompt-history-v1".into()));
}

#[test]
fn same_event_different_history_id_or_timestamp_is_conflict() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("prompts.sqlite3");
    let mut history = PromptHistoryStore::open(&path).expect("open");
    history_testing::commit_delivered(
        &mut history,
        accepted(1, "bound body", 1_000, PromptHistoryProvenance::default()),
    )
    .expect("first");

    let mut different_id = accepted(1, "bound body", 1_000, PromptHistoryProvenance::default());
    if let PromptHistoryAttempt::AcceptedForDelivery { history_id: id, .. } = &mut different_id {
        *id = history_id(9);
    }
    assert_eq!(
        history_testing::commit_delivered(&mut history, different_id)
            .expect_err("history id mismatch")
            .code(),
        PromptHistoryErrorCode::Conflict
    );

    let mut different_time = accepted(1, "bound body", 1_000, PromptHistoryProvenance::default());
    if let PromptHistoryAttempt::AcceptedForDelivery { accepted_at_ms, .. } = &mut different_time {
        *accepted_at_ms = 9_000;
    }
    assert_eq!(
        history_testing::commit_delivered(&mut history, different_time)
            .expect_err("timestamp mismatch")
            .code(),
        PromptHistoryErrorCode::Conflict
    );
    assert_eq!(history.recent(10, None).expect("recent").entries.len(), 1);
}

#[test]
fn same_event_different_request_or_session_is_conflict() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("prompts.sqlite3");
    let mut history = PromptHistoryStore::open(&path).expect("open");
    history_testing::commit_delivered(
        &mut history,
        accepted(1, "bound body", 1_000, PromptHistoryProvenance::default()),
    )
    .expect("first");

    let mut different_request =
        accepted(1, "bound body", 1_000, PromptHistoryProvenance::default());
    if let PromptHistoryAttempt::AcceptedForDelivery { request_id: id, .. } = &mut different_request
    {
        *id = request_id(9);
    }
    assert_eq!(
        history_testing::commit_delivered(&mut history, different_request)
            .expect_err("request id mismatch")
            .code(),
        PromptHistoryErrorCode::Conflict
    );

    let mut different_event = accepted(1, "bound body", 1_000, PromptHistoryProvenance::default());
    if let PromptHistoryAttempt::AcceptedForDelivery {
        submitted_event_id: id,
        ..
    } = &mut different_event
    {
        *id = event_id(9);
    }
    assert_eq!(
        history_testing::commit_delivered(&mut history, different_event)
            .expect_err("event id mismatch")
            .code(),
        PromptHistoryErrorCode::Conflict
    );

    let mut different_session =
        accepted(1, "bound body", 1_000, PromptHistoryProvenance::default());
    if let PromptHistoryAttempt::AcceptedForDelivery {
        agent_session_id: id,
        ..
    } = &mut different_session
    {
        *id = session_id(9);
    }
    assert_eq!(
        history_testing::commit_delivered(&mut history, different_session)
            .expect_err("session mismatch")
            .code(),
        PromptHistoryErrorCode::Conflict
    );
    assert_eq!(history.recent(10, None).expect("recent").entries.len(), 1);
}

#[test]
fn pending_capacity_overflow_marks_dirty_without_1025th_row() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("prompts.sqlite3");
    let mut history = PromptHistoryStore::open(&path).expect("open");
    history_testing::seed_pending_rows(&mut history, DEFAULT_HISTORY_INDEX_CAPACITY)
        .expect("fill pending");
    history_testing::commit_delivered(
        &mut history,
        accepted(
            200,
            "overflowed delivery",
            200_000,
            PromptHistoryProvenance::default(),
        ),
    )
    .expect("history still persists");
    assert!(history.is_search_dirty().expect("dirty"));
    assert!(history.is_search_overflow().expect("overflow"));
    assert_eq!(
        history.pending_count().expect("pending"),
        u64::from(DEFAULT_HISTORY_INDEX_CAPACITY)
    );
    assert_eq!(
        history.recent(1, None).expect("recent").entries[0].body,
        "overflowed delivery"
    );
}

#[test]
fn saved_prompt_write_enqueues_in_same_transaction() {
    let dir = TempDir::new().expect("tempdir");
    let (mut prompts, history) = open_pair(&dir);
    prompts
        .execute(
            command_id(1),
            PromptCommand::CreatePrompt(CreatePrompt {
                prompt_id: prompt_id(1),
                prompt_version_id: version_id(1),
                title: "Library".into(),
                description: None,
                tags: Vec::new(),
                variables: Vec::new(),
                body: "enqueued saved body".into(),
                created_at_ms: 1_000,
            }),
        )
        .expect("create");
    assert!(history.is_search_dirty().expect("dirty after saved write"));
    assert!(history.pending_count().expect("pending") >= 1);
}

#[test]
fn saved_search_keyset_continues_after_first_page() {
    let dir = TempDir::new().expect("tempdir");
    let (mut prompts, mut history) = open_pair(&dir);
    for tail in 1_u8..=3 {
        prompts
            .execute(
                command_id(tail),
                PromptCommand::CreatePrompt(CreatePrompt {
                    prompt_id: prompt_id(tail),
                    prompt_version_id: version_id(tail),
                    title: format!("Saved {tail}"),
                    description: None,
                    tags: Vec::new(),
                    variables: Vec::new(),
                    body: format!("shared saved token {tail}"),
                    created_at_ms: i64::from(tail) * 1_000,
                }),
            )
            .expect("saved");
    }
    while history
        .rebuild_search(PromptSearchBudget::default())
        .expect("index")
        .remaining
        > 0
    {}
    let first = history
        .search(
            &PromptSearchQuery {
                text: "shared".into(),
                source: PromptSearchSource::Saved,
                cursor: None,
                page_size: 1,
            },
            PromptSearchBudget::default(),
        )
        .expect("page 1");
    assert_eq!(first.hits.len(), 1);
    assert_eq!(first.status, PromptSearchStatus::Complete);
    let second = history
        .search(
            &PromptSearchQuery {
                text: "shared".into(),
                source: PromptSearchSource::Saved,
                cursor: first.next.clone(),
                page_size: 1,
            },
            PromptSearchBudget::default(),
        )
        .expect("page 2");
    assert_eq!(second.hits.len(), 1);
    assert_ne!(second.hits[0].body, first.hits[0].body);
}

#[test]
fn search_budget_exhaustion_returns_partial() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("prompts.sqlite3");
    let mut history = PromptHistoryStore::open(&path).expect("open");
    for tail in 1_u8..=5 {
        history_testing::commit_delivered(
            &mut history,
            accepted(
                tail,
                "budget token",
                i64::from(tail) * 1_000,
                PromptHistoryProvenance::default(),
            ),
        )
        .expect("seed");
    }
    while history
        .rebuild_search(PromptSearchBudget::default())
        .expect("index")
        .remaining
        > 0
    {}
    let page = history
        .search(
            &PromptSearchQuery {
                text: "budget".into(),
                source: PromptSearchSource::History,
                cursor: None,
                page_size: 10,
            },
            PromptSearchBudget::default().with_work_limit(1),
        )
        .expect("partial");
    assert_eq!(page.status, PromptSearchStatus::Partial);
    assert!(page.next.is_some());
}

#[test]
fn rebuild_preserves_arrivals_and_requires_high_water() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("prompts.sqlite3");
    let mut history = PromptHistoryStore::open(&path).expect("open");
    for tail in 1_u8..=51 {
        history_testing::commit_delivered(
            &mut history,
            accepted(
                tail,
                &format!("hw {tail}"),
                i64::from(tail) * 1_000,
                PromptHistoryProvenance::default(),
            ),
        )
        .expect("seed");
    }
    let first = history
        .rebuild_search(PromptSearchBudget::default())
        .expect("start rebuild");
    assert!(first.remaining > 0);
    history_testing::commit_delivered(
        &mut history,
        accepted(
            200,
            "arrived during rebuild",
            200_000,
            PromptHistoryProvenance::default(),
        ),
    )
    .expect("arrival");
    let mut steps = 1;
    while history
        .rebuild_search(PromptSearchBudget::default())
        .expect("continue")
        .remaining
        > 0
    {
        steps += 1;
        assert!(steps < 12, "rebuild must finish");
    }
    assert!(!history
        .is_search_dirty()
        .expect("clean after high-water plus tail"));
    let found = history
        .search(
            &PromptSearchQuery {
                text: "arrived".into(),
                source: PromptSearchSource::History,
                cursor: None,
                page_size: 10,
            },
            PromptSearchBudget::default(),
        )
        .expect("search arrival");
    assert_eq!(found.hits.len(), 1);
}

#[test]
fn combining_and_expanding_unicode_highlights() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("prompts.sqlite3");
    let mut history = PromptHistoryStore::open(&path).expect("open");
    history_testing::commit_delivered(
        &mut history,
        accepted(
            1,
            "cafe\u{0301} resume\u{0301}",
            1_000,
            PromptHistoryProvenance::default(),
        ),
    )
    .expect("combining");
    while history
        .rebuild_search(PromptSearchBudget::default())
        .expect("index")
        .remaining
        > 0
    {}
    let page = history
        .search(
            &PromptSearchQuery {
                text: "café".into(),
                source: PromptSearchSource::History,
                cursor: None,
                page_size: 10,
            },
            PromptSearchBudget::default(),
        )
        .expect("folded");
    assert_eq!(page.hits.len(), 1);
    assert!(page.hits[0].highlights.iter().any(|range| {
        page.hits[0].body.is_char_boundary(range.start)
            && page.hits[0].body.is_char_boundary(range.end)
            && range.end > range.start
            && page.hits[0].body[range.start..range.end].contains('\u{0301}')
    }));
}

#[test]
fn debug_redacts_bodies_from_entries_and_proofs() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("prompts.sqlite3");
    let mut history = PromptHistoryStore::open(&path).expect("open");
    let secret = "UNIQUE_SECRET_BODY_TEXT";
    history_testing::commit_delivered(
        &mut history,
        accepted(1, secret, 1_000, PromptHistoryProvenance::default()),
    )
    .expect("commit");
    let entry = &history.recent(1, None).expect("recent").entries[0];
    let rendered = format!("{entry:?} {:?}", history_testing::debug_proof(secret));
    assert!(!rendered.contains(secret));
    assert!(!rendered.contains("UNIQUE_SECRET"));
}

#[test]
fn prompt_search_query_debug_redacts_text() {
    let query = PromptSearchQuery {
        text: "UNIQUE_SECRET_QUERY_TEXT".into(),
        source: PromptSearchSource::History,
        cursor: None,
        page_size: 10,
    };
    let rendered = format!("{query:?}");
    assert!(!rendered.contains("UNIQUE_SECRET_QUERY_TEXT"));
    assert!(!rendered.contains("UNIQUE_SECRET"));
}

#[test]
fn search_byte_budget_rejects_before_body_and_continues_without_loss() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("prompts.sqlite3");
    let mut history = PromptHistoryStore::open(&path).expect("open");
    history_testing::commit_delivered(
        &mut history,
        accepted(
            1,
            &"oversized-search-body".repeat(8),
            1_000,
            PromptHistoryProvenance::default(),
        ),
    )
    .expect("seed");
    while history
        .rebuild_search(PromptSearchBudget::default())
        .expect("index")
        .remaining
        > 0
    {}
    let tight = history
        .search(
            &PromptSearchQuery {
                text: "oversized".into(),
                source: PromptSearchSource::History,
                cursor: None,
                page_size: 10,
            },
            PromptSearchBudget::default().with_max_bytes(10),
        )
        .expect("partial before first hit");
    assert_eq!(tight.status, PromptSearchStatus::Partial);
    assert!(tight.hits.is_empty());
    assert!(
        tight.next.is_some(),
        "first-row budget miss must still resume"
    );

    let resumed = history
        .search(
            &PromptSearchQuery {
                text: "oversized".into(),
                source: PromptSearchSource::History,
                cursor: tight.next,
                page_size: 10,
            },
            PromptSearchBudget::default(),
        )
        .expect("resume");
    assert_eq!(resumed.hits.len(), 1);
    assert!(resumed.hits[0].body.contains("oversized-search-body"));
}

#[test]
fn search_partial_pages_union_equals_complete_set() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("prompts.sqlite3");
    let mut history = PromptHistoryStore::open(&path).expect("open");
    for tail in 1_u8..=5 {
        history_testing::commit_delivered(
            &mut history,
            accepted(
                tail,
                "budget token",
                i64::from(tail) * 1_000,
                PromptHistoryProvenance::default(),
            ),
        )
        .expect("seed");
    }
    while history
        .rebuild_search(PromptSearchBudget::default())
        .expect("index")
        .remaining
        > 0
    {}
    let complete = history
        .search(
            &PromptSearchQuery {
                text: "budget".into(),
                source: PromptSearchSource::History,
                cursor: None,
                page_size: 10,
            },
            PromptSearchBudget::default(),
        )
        .expect("complete");
    let expected: Vec<_> = complete.hits.iter().map(|hit| hit.history_id).collect();

    let mut seen = Vec::new();
    let mut cursor = None;
    for _ in 0..8 {
        let page = history
            .search(
                &PromptSearchQuery {
                    text: "budget".into(),
                    source: PromptSearchSource::History,
                    cursor,
                    page_size: 10,
                },
                PromptSearchBudget::default().with_work_limit(1),
            )
            .expect("partial page");
        for hit in &page.hits {
            seen.push(hit.history_id);
        }
        cursor = page.next.clone();
        if page.status == PromptSearchStatus::Complete && cursor.is_none() {
            break;
        }
        assert!(
            page.next.is_some() || !page.hits.is_empty(),
            "partial must remain resumable"
        );
    }
    assert_eq!(seen, expected);
}

#[test]
fn rebuild_clean_consumes_high_water_after_arrival() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("prompts.sqlite3");
    let mut history = PromptHistoryStore::open(&path).expect("open");
    for tail in 1_u8..=51 {
        history_testing::commit_delivered(
            &mut history,
            accepted(
                tail,
                &format!("hw {tail}"),
                i64::from(tail) * 1_000,
                PromptHistoryProvenance::default(),
            ),
        )
        .expect("seed");
    }
    let first = history
        .rebuild_search(PromptSearchBudget::default())
        .expect("start rebuild");
    assert!(first.remaining > 0);
    let (_current_before, high_water) = history_testing::index_seqs(&history).expect("hw");
    assert!(high_water > 0);
    history_testing::commit_delivered(
        &mut history,
        accepted(
            200,
            "arrived during rebuild",
            200_000,
            PromptHistoryProvenance::default(),
        ),
    )
    .expect("arrival");
    let (current_after_arrival, high_water_mid) =
        history_testing::index_seqs(&history).expect("seqs");
    assert_eq!(high_water_mid, high_water);
    assert!(current_after_arrival > high_water_mid);
    let mut steps = 1;
    while history
        .rebuild_search(PromptSearchBudget::default())
        .expect("continue")
        .remaining
        > 0
    {
        steps += 1;
        assert!(steps < 12, "rebuild must finish");
    }
    assert!(!history.is_search_dirty().expect("clean"));
    let (current_done, high_water_done) = history_testing::index_seqs(&history).expect("consumed");
    assert_eq!(
        current_done, high_water_done,
        "clean must consume high-water through the queued tail"
    );
}

#[test]
fn saved_enqueue_survives_drop_and_reopen_then_rebuild() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("prompts.sqlite3");
    {
        let mut prompts = PromptStore::open(&path).expect("prompts");
        prompts
            .execute(
                command_id(1),
                PromptCommand::CreatePrompt(CreatePrompt {
                    prompt_id: prompt_id(1),
                    prompt_version_id: version_id(1),
                    title: "Persisted".into(),
                    description: None,
                    tags: Vec::new(),
                    variables: Vec::new(),
                    body: "durable saved enqueue".into(),
                    created_at_ms: 1_000,
                }),
            )
            .expect("create");
    }
    let mut history = PromptHistoryStore::open(&path).expect("reopen");
    assert!(history.is_search_dirty().expect("dirty after reopen"));
    assert!(history.pending_count().expect("pending") >= 1);
    while history
        .rebuild_search(PromptSearchBudget::default())
        .expect("rebuild")
        .remaining
        > 0
    {}
    let page = history
        .search(
            &PromptSearchQuery {
                text: "durable".into(),
                source: PromptSearchSource::Saved,
                cursor: None,
                page_size: 10,
            },
            PromptSearchBudget::default(),
        )
        .expect("search");
    assert_eq!(page.hits.len(), 1);
}

#[test]
fn rebuild_reopen_mid_history_phase_continues_cursor() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("prompts.sqlite3");
    {
        let mut history = PromptHistoryStore::open(&path).expect("open");
        for tail in 1_u8..=51 {
            history_testing::commit_delivered(
                &mut history,
                accepted(
                    tail,
                    &format!("reopen token {tail}"),
                    i64::from(tail) * 1_000,
                    PromptHistoryProvenance::default(),
                ),
            )
            .expect("seed");
        }
        let first = history
            .rebuild_search(PromptSearchBudget::default())
            .expect("first page");
        assert!(first.remaining > 0);
        assert!(history.is_search_dirty().expect("dirty"));
    }
    let mut history = PromptHistoryStore::open(&path).expect("reopen");
    assert!(history.is_search_dirty().expect("dirty survives"));
    let mut steps = 0;
    while history
        .rebuild_search(PromptSearchBudget::default())
        .expect("continue")
        .remaining
        > 0
    {
        steps += 1;
        assert!(steps < 12, "reopen rebuild must finish");
    }
    assert!(!history.is_search_dirty().expect("clean"));
    let page = history
        .search(
            &PromptSearchQuery {
                text: "reopen".into(),
                source: PromptSearchSource::History,
                cursor: None,
                page_size: 100,
            },
            PromptSearchBudget::default(),
        )
        .expect("all");
    assert_eq!(page.hits.len(), 51);
}

#[test]
fn overflowed_delivery_unsearchable_until_rebuild_not_drain() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("prompts.sqlite3");
    let mut history = PromptHistoryStore::open(&path).expect("open");
    history_testing::seed_pending_rows(&mut history, DEFAULT_HISTORY_INDEX_CAPACITY)
        .expect("fill pending");
    history_testing::commit_delivered(
        &mut history,
        accepted(
            200,
            "overflowed delivery",
            200_000,
            PromptHistoryProvenance::default(),
        ),
    )
    .expect("history still persists");
    let _drained = history
        .drain_index(PromptSearchBudget::default())
        .expect("drain cannot clear overflow");
    assert!(history.is_search_overflow().expect("overflow stays"));
    assert!(history.is_search_dirty().expect("dirty stays"));
    let missed = history
        .search(
            &PromptSearchQuery {
                text: "overflowed".into(),
                source: PromptSearchSource::History,
                cursor: None,
                page_size: 10,
            },
            PromptSearchBudget::default(),
        )
        .expect("search during overflow");
    assert!(missed.hits.is_empty());
    while history
        .rebuild_search(PromptSearchBudget::default())
        .expect("rebuild")
        .remaining
        > 0
    {}
    assert!(!history.is_search_overflow().expect("overflow cleared"));
    assert!(!history.is_search_dirty().expect("clean after rebuild"));
    let found = history
        .search(
            &PromptSearchQuery {
                text: "overflowed".into(),
                source: PromptSearchSource::History,
                cursor: None,
                page_size: 10,
            },
            PromptSearchBudget::default(),
        )
        .expect("search after rebuild");
    assert_eq!(found.hits.len(), 1);
}

#[test]
fn identical_submitted_at_ms_keyset_is_strictly_monotonic() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("prompts.sqlite3");
    let mut history = PromptHistoryStore::open(&path).expect("open");
    history_testing::commit_delivered(
        &mut history,
        accepted(
            1,
            "tie token one",
            5_000,
            PromptHistoryProvenance::default(),
        ),
    )
    .expect("first");
    history_testing::commit_delivered(
        &mut history,
        accepted(
            2,
            "tie token two",
            5_000,
            PromptHistoryProvenance::default(),
        ),
    )
    .expect("second");
    let first = history.recent(1, None).expect("recent page 1");
    assert_eq!(first.entries.len(), 1);
    let second = history.recent(1, first.next).expect("recent page 2");
    assert_eq!(second.entries.len(), 1);
    assert_ne!(second.entries[0].history_id, first.entries[0].history_id);
    assert!(second.next.is_none());

    while history
        .rebuild_search(PromptSearchBudget::default())
        .expect("index")
        .remaining
        > 0
    {}
    let search_first = history
        .search(
            &PromptSearchQuery {
                text: "tie".into(),
                source: PromptSearchSource::History,
                cursor: None,
                page_size: 1,
            },
            PromptSearchBudget::default(),
        )
        .expect("search page 1");
    let search_second = history
        .search(
            &PromptSearchQuery {
                text: "tie".into(),
                source: PromptSearchSource::History,
                cursor: search_first.next.clone(),
                page_size: 1,
            },
            PromptSearchBudget::default(),
        )
        .expect("search page 2");
    assert_eq!(search_first.hits.len(), 1);
    assert_eq!(search_second.hits.len(), 1);
    assert_ne!(
        search_first.hits[0].history_id,
        search_second.hits[0].history_id
    );
    let ids = [
        search_first.hits[0].history_id,
        search_second.hits[0].history_id,
    ];
    assert!(ids.contains(&Some(history_id(1))));
    assert!(ids.contains(&Some(history_id(2))));
}

#[test]
fn saved_keyset_three_pages_cover_exactly_the_set() {
    let dir = TempDir::new().expect("tempdir");
    let (mut prompts, mut history) = open_pair(&dir);
    for tail in 1_u8..=3 {
        prompts
            .execute(
                command_id(tail),
                PromptCommand::CreatePrompt(CreatePrompt {
                    prompt_id: prompt_id(tail),
                    prompt_version_id: version_id(tail),
                    title: format!("Saved {tail}"),
                    description: None,
                    tags: Vec::new(),
                    variables: Vec::new(),
                    body: format!("shared saved token {tail}"),
                    created_at_ms: i64::from(tail) * 1_000,
                }),
            )
            .expect("saved");
    }
    while history
        .rebuild_search(PromptSearchBudget::default())
        .expect("index")
        .remaining
        > 0
    {}
    let mut bodies = Vec::new();
    let mut cursor = None;
    for _ in 0..4 {
        let page = history
            .search(
                &PromptSearchQuery {
                    text: "shared".into(),
                    source: PromptSearchSource::Saved,
                    cursor,
                    page_size: 1,
                },
                PromptSearchBudget::default(),
            )
            .expect("page");
        assert_eq!(page.hits.len(), 1);
        bodies.push(page.hits[0].body.clone());
        cursor = page.next.clone();
        if cursor.is_none() {
            break;
        }
    }
    bodies.sort();
    assert_eq!(
        bodies,
        vec![
            "shared saved token 1".to_string(),
            "shared saved token 2".to_string(),
            "shared saved token 3".to_string(),
        ]
    );
}

#[test]
fn expanding_capital_i_in_body_matches_folded_query() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("prompts.sqlite3");
    let mut history = PromptHistoryStore::open(&path).expect("open");
    history_testing::commit_delivered(
        &mut history,
        accepted(
            1,
            "İstanbul office",
            1_000,
            PromptHistoryProvenance::default(),
        ),
    )
    .expect("seed");
    while history
        .rebuild_search(PromptSearchBudget::default())
        .expect("index")
        .remaining
        > 0
    {}
    let page = history
        .search(
            &PromptSearchQuery {
                text: "istanbul".into(),
                source: PromptSearchSource::History,
                cursor: None,
                page_size: 10,
            },
            PromptSearchBudget::default(),
        )
        .expect("folded");
    assert_eq!(page.hits.len(), 1);
    assert!(page.hits[0].highlights.iter().any(|range| {
        page.hits[0].body.is_char_boundary(range.start)
            && page.hits[0].body.is_char_boundary(range.end)
            && page.hits[0].body[range.start..range.end].contains('İ')
    }));
}

#[test]
fn rebuild_and_drain_check_cancellation_inside_each_batch() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("prompts.sqlite3");
    let mut history = PromptHistoryStore::open(&path).expect("open");
    for tail in 1_u8..=3 {
        history_testing::commit_delivered(
            &mut history,
            accepted(
                tail,
                &format!("cancel token {tail}"),
                i64::from(tail) * 1_000,
                PromptHistoryProvenance::default(),
            ),
        )
        .expect("seed");
    }
    let cancelled = AtomicBool::new(true);
    assert_eq!(
        history
            .rebuild_search(PromptSearchBudget::default().with_cancellation(&cancelled))
            .expect_err("rebuild cancel")
            .code(),
        PromptHistoryErrorCode::Cancelled
    );
    assert!(history
        .is_search_dirty()
        .expect("dirty after rebuild cancel"));
    assert_eq!(
        history
            .drain_index(PromptSearchBudget::default().with_cancellation(&cancelled))
            .expect_err("drain cancel")
            .code(),
        PromptHistoryErrorCode::Cancelled
    );
    let preview = history.preview_clear().expect("preview");
    assert_eq!(
        history
            .apply_clear(
                preview.confirmation(),
                PromptSearchBudget::default().with_cancellation(&cancelled),
            )
            .expect_err("clear cancel")
            .code(),
        PromptHistoryErrorCode::Cancelled
    );
}

#[test]
fn rename_and_tag_edits_enqueue_index_work() {
    let dir = TempDir::new().expect("tempdir");
    let (mut prompts, mut history) = open_pair(&dir);
    prompts
        .execute(
            command_id(1),
            PromptCommand::CreatePrompt(CreatePrompt {
                prompt_id: prompt_id(1),
                prompt_version_id: version_id(1),
                title: "Original title".into(),
                description: None,
                tags: vec!["oldtag".into()],
                variables: Vec::new(),
                body: "library body".into(),
                created_at_ms: 1_000,
            }),
        )
        .expect("create");
    while history
        .rebuild_search(PromptSearchBudget::default())
        .expect("rebuild")
        .remaining
        > 0
    {}
    assert!(!history.is_search_dirty().expect("clean"));
    prompts
        .execute(
            command_id(2),
            PromptCommand::RenamePrompt(RenamePrompt {
                prompt_id: prompt_id(1),
                title: "Renamed searchable title".into(),
                expected_revision: 1,
            }),
        )
        .expect("rename");
    assert!(history.is_search_dirty().expect("dirty after rename"));
    assert!(history.pending_count().expect("pending after rename") >= 1);
    while history
        .drain_index(PromptSearchBudget::default())
        .expect("drain rename")
        .remaining
        > 0
    {}
    let renamed = history
        .search(
            &PromptSearchQuery {
                text: "Renamed".into(),
                source: PromptSearchSource::Saved,
                cursor: None,
                page_size: 10,
            },
            PromptSearchBudget::default(),
        )
        .expect("search title");
    assert_eq!(renamed.hits.len(), 1);

    prompts
        .execute(
            command_id(3),
            PromptCommand::SetPromptTags(SetPromptTags {
                prompt_id: prompt_id(1),
                tags: vec!["fresh-tag".into()],
                expected_revision: 2,
            }),
        )
        .expect("tags");
    assert!(history.is_search_dirty().expect("dirty after tags"));
    assert!(history.pending_count().expect("pending after tags") >= 1);
    drop(history);
    let mut history =
        PromptHistoryStore::open(&dir.path().join("prompts.sqlite3")).expect("reopen");
    assert!(history.is_search_dirty().expect("dirty survives reopen"));
    while history
        .drain_index(PromptSearchBudget::default())
        .expect("drain tags")
        .remaining
        > 0
    {}
    let tagged = history
        .search(
            &PromptSearchQuery {
                text: "tag:fresh-tag".into(),
                source: PromptSearchSource::Saved,
                cursor: None,
                page_size: 10,
            },
            PromptSearchBudget::default(),
        )
        .expect("search tag");
    assert_eq!(tagged.hits.len(), 1);
}

#[test]
fn rename_while_pending_bumps_enqueue_generation() {
    let dir = TempDir::new().expect("tempdir");
    let (mut prompts, history) = open_pair(&dir);
    prompts
        .execute(
            command_id(1),
            PromptCommand::CreatePrompt(CreatePrompt {
                prompt_id: prompt_id(1),
                prompt_version_id: version_id(1),
                title: "Before".into(),
                description: None,
                tags: Vec::new(),
                variables: Vec::new(),
                body: "same body".into(),
                created_at_ms: 1_000,
            }),
        )
        .expect("create");
    let first_seq = history_testing::pending_enqueue_seq(&history, prompt_id(1))
        .expect("seq after create")
        .expect("queued");
    prompts
        .execute(
            command_id(2),
            PromptCommand::RenamePrompt(RenamePrompt {
                prompt_id: prompt_id(1),
                title: "After".into(),
                expected_revision: 1,
            }),
        )
        .expect("rename while pending");
    let second_seq = history_testing::pending_enqueue_seq(&history, prompt_id(1))
        .expect("seq after rename")
        .expect("still queued");
    assert!(
        second_seq > first_seq,
        "rename must bump pending generation so drain cannot drop the update"
    );
}

#[test]
fn search_budget_uses_utf8_bytes_not_sqlite_chars() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("prompts.sqlite3");
    let mut history = PromptHistoryStore::open(&path).expect("open");
    let body = format!("token {}", "é".repeat(20));
    assert_eq!(body.chars().count(), 26);
    assert_eq!(body.len(), 46);
    history_testing::commit_delivered(
        &mut history,
        accepted(1, &body, 1_000, PromptHistoryProvenance::default()),
    )
    .expect("seed");
    while history
        .rebuild_search(PromptSearchBudget::default())
        .expect("index")
        .remaining
        > 0
    {}
    let page = history
        .search(
            &PromptSearchQuery {
                text: "token".into(),
                source: PromptSearchSource::History,
                cursor: None,
                page_size: 10,
            },
            PromptSearchBudget::default().with_max_bytes(30),
        )
        .expect("byte budget");
    assert_eq!(page.status, PromptSearchStatus::Partial);
    assert!(page.hits.is_empty());
    assert!(page.next.is_some());
}

#[test]
fn accepted_retry_survives_later_chain_link_removal() {
    let dir = TempDir::new().expect("tempdir");
    let (mut prompts, mut history) = open_pair(&dir);
    prompts
        .execute(
            command_id(1),
            PromptCommand::CreatePrompt(CreatePrompt {
                prompt_id: prompt_id(1),
                prompt_version_id: version_id(1),
                title: "Pinned".into(),
                description: None,
                tags: Vec::new(),
                variables: Vec::new(),
                body: "version one".into(),
                created_at_ms: 1_000,
            }),
        )
        .expect("create");
    prompts
        .execute_chain(
            command_id(2),
            PromptChainCommand::CreatePromptChain(CreatePromptChain {
                chain_id: chain_id(1),
                title: "Guide".into(),
                description: None,
                created_at_ms: 1_100,
            }),
        )
        .expect("chain");
    prompts
        .execute_chain(
            command_id(3),
            PromptChainCommand::InsertPromptChainLink(InsertPromptChainLink {
                chain_id: chain_id(1),
                link_id: link_id(1),
                prompt_id: prompt_id(1),
                prompt_version_id: None,
                before_link_id: None,
                expected_revision: 1,
            }),
        )
        .expect("link");
    let provenance = PromptHistoryProvenance {
        prompt_id: Some(prompt_id(1)),
        prompt_version_id: Some(version_id(1)),
        chain_id: Some(chain_id(1)),
        chain_link_id: Some(link_id(1)),
    };
    history_testing::commit_delivered(
        &mut history,
        accepted(1, "version one", 2_000, provenance.clone()),
    )
    .expect("first");
    prompts
        .execute_chain(
            command_id(4),
            PromptChainCommand::RemovePromptChainLink(RemovePromptChainLink {
                chain_id: chain_id(1),
                link_id: link_id(1),
                expected_revision: 2,
            }),
        )
        .expect("remove link");
    let replayed = history_testing::commit_delivered(
        &mut history,
        accepted(1, "version one", 2_000, provenance),
    )
    .expect("exact retry after link removal")
    .expect("duplicate");
    assert_eq!(replayed.history_id, history_id(1));
    assert_eq!(history.recent(10, None).expect("recent").entries.len(), 1);
}

#[test]
fn history_id_only_collision_is_conflict() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("prompts.sqlite3");
    let mut history = PromptHistoryStore::open(&path).expect("open");
    history_testing::commit_delivered(
        &mut history,
        accepted(
            1,
            "first payload",
            1_000,
            PromptHistoryProvenance::default(),
        ),
    )
    .expect("first");
    let mut colliding = accepted(
        2,
        "other payload",
        2_000,
        PromptHistoryProvenance::default(),
    );
    if let PromptHistoryAttempt::AcceptedForDelivery { history_id: id, .. } = &mut colliding {
        *id = history_id(1);
    }
    assert_eq!(
        history_testing::commit_delivered(&mut history, colliding)
            .expect_err("history id collision")
            .code(),
        PromptHistoryErrorCode::Conflict
    );
    assert_eq!(history.recent(10, None).expect("recent").entries.len(), 1);
}

#[test]
fn recent_and_search_fail_closed_on_corrupt_body_hash() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("prompts.sqlite3");
    let mut history = PromptHistoryStore::open(&path).expect("open");
    history_testing::commit_delivered(
        &mut history,
        accepted(
            1,
            "canonical body",
            1_000,
            PromptHistoryProvenance::default(),
        ),
    )
    .expect("commit");
    while history
        .rebuild_search(PromptSearchBudget::default())
        .expect("index")
        .remaining
        > 0
    {}
    drop(history);
    let conn = Connection::open(&path).expect("raw");
    conn.execute(
        "UPDATE prompt_history SET body_sha256 = ?1",
        [vec![0_u8; 32]],
    )
    .expect("corrupt hash");
    drop(conn);
    let history = PromptHistoryStore::open(&path).expect("reopen");
    assert_eq!(
        history
            .recent(10, None)
            .expect_err("recent corruption")
            .code(),
        PromptHistoryErrorCode::Storage
    );
    assert_eq!(
        history
            .search(
                &PromptSearchQuery {
                    text: "canonical".into(),
                    source: PromptSearchSource::History,
                    cursor: None,
                    page_size: 10,
                },
                PromptSearchBudget::default(),
            )
            .expect_err("search corruption")
            .code(),
        PromptHistoryErrorCode::Storage
    );
}

#[test]
fn unicode61_long_s_and_d_dot_highlights() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("prompts.sqlite3");
    let mut history = PromptHistoryStore::open(&path).expect("open");
    history_testing::commit_delivered(
        &mut history,
        accepted(
            1,
            "the ẛun and the ḍog",
            1_000,
            PromptHistoryProvenance::default(),
        ),
    )
    .expect("seed");
    while history
        .rebuild_search(PromptSearchBudget::default())
        .expect("index")
        .remaining
        > 0
    {}
    let sun = history
        .search(
            &PromptSearchQuery {
                text: "sun".into(),
                source: PromptSearchSource::History,
                cursor: None,
                page_size: 10,
            },
            PromptSearchBudget::default(),
        )
        .expect("sun");
    assert_eq!(sun.hits.len(), 1);
    assert!(sun.hits[0].highlights.iter().any(|range| {
        sun.hits[0].body.is_char_boundary(range.start)
            && sun.hits[0].body.is_char_boundary(range.end)
            && sun.hits[0].body[range.start..range.end].contains('ẛ')
    }));
    let dog = history
        .search(
            &PromptSearchQuery {
                text: "dog".into(),
                source: PromptSearchSource::History,
                cursor: None,
                page_size: 10,
            },
            PromptSearchBudget::default(),
        )
        .expect("dog");
    assert_eq!(dog.hits.len(), 1);
    assert!(dog.hits[0].highlights.iter().any(|range| {
        dog.hits[0].body.is_char_boundary(range.start)
            && dog.hits[0].body.is_char_boundary(range.end)
            && dog.hits[0].body[range.start..range.end].contains('ḍ')
    }));
}

#[test]
fn search_cursor_rejects_cross_source_query_or_stale_version() {
    let dir = TempDir::new().expect("tempdir");
    let (mut prompts, mut history) = open_pair(&dir);
    prompts
        .execute(
            command_id(1),
            PromptCommand::CreatePrompt(CreatePrompt {
                prompt_id: prompt_id(1),
                prompt_version_id: version_id(1),
                title: "Saved".into(),
                description: None,
                tags: Vec::new(),
                variables: Vec::new(),
                body: "shared token in library".into(),
                created_at_ms: 1_000,
            }),
        )
        .expect("saved");
    for tail in 1_u8..=3 {
        history_testing::commit_delivered(
            &mut history,
            accepted(
                tail,
                "shared token in history",
                i64::from(tail) * 1_000,
                PromptHistoryProvenance::default(),
            ),
        )
        .expect("history");
    }
    while history
        .rebuild_search(PromptSearchBudget::default())
        .expect("index")
        .remaining
        > 0
    {}
    let first = history
        .search(
            &PromptSearchQuery {
                text: "shared".into(),
                source: PromptSearchSource::History,
                cursor: None,
                page_size: 1,
            },
            PromptSearchBudget::default(),
        )
        .expect("page 1");
    assert!(first.next.is_some());
    assert_eq!(
        history
            .search(
                &PromptSearchQuery {
                    text: "shared".into(),
                    source: PromptSearchSource::Saved,
                    cursor: first.next.clone(),
                    page_size: 1,
                },
                PromptSearchBudget::default(),
            )
            .expect_err("cross source")
            .code(),
        PromptHistoryErrorCode::InvalidQuery
    );
    assert_eq!(
        history
            .search(
                &PromptSearchQuery {
                    text: "token".into(),
                    source: PromptSearchSource::History,
                    cursor: first.next.clone(),
                    page_size: 1,
                },
                PromptSearchBudget::default(),
            )
            .expect_err("cross query")
            .code(),
        PromptHistoryErrorCode::InvalidQuery
    );
    history_testing::commit_delivered(
        &mut history,
        accepted(
            9,
            "shared token newer",
            9_000,
            PromptHistoryProvenance::default(),
        ),
    )
    .expect("arrival");
    assert_eq!(
        history
            .search(
                &PromptSearchQuery {
                    text: "shared".into(),
                    source: PromptSearchSource::History,
                    cursor: first.next,
                    page_size: 1,
                },
                PromptSearchBudget::default(),
            )
            .expect_err("stale version")
            .code(),
        PromptHistoryErrorCode::InvalidQuery
    );
}

#[test]
fn recent_continuation_pins_bind_and_rejects_later_rows() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("prompts.sqlite3");
    let mut history = PromptHistoryStore::open(&path).expect("open");
    for tail in 1_u8..=3 {
        history_testing::commit_delivered(
            &mut history,
            accepted(
                tail,
                &format!("recent row {tail}"),
                i64::from(tail) * 1_000,
                PromptHistoryProvenance::default(),
            ),
        )
        .expect("seed");
    }
    let first = history.recent(1, None).expect("page 1");
    let cursor = first.next.expect("continuation");
    history_testing::commit_delivered(
        &mut history,
        accepted(
            9,
            "later row must invalidate the page",
            9_000,
            PromptHistoryProvenance::default(),
        ),
    )
    .expect("later delivery");
    assert_eq!(
        history
            .recent(1, Some(cursor))
            .expect_err("later rows")
            .code(),
        PromptHistoryErrorCode::InvalidQuery
    );
    let (epoch, high_water) = history_testing::index_seqs(&history).expect("seqs");
    let schema = history_testing::schema_version(&history).expect("schema");
    assert_eq!(
        history
            .recent(
                1,
                Some(cursor.with_bind(
                    PromptSearchSource::Saved,
                    cursor_query_hash(&cursor),
                    epoch,
                    high_water,
                    schema,
                )),
            )
            .expect_err("cross source")
            .code(),
        PromptHistoryErrorCode::InvalidQuery
    );
    assert_eq!(
        history
            .recent(
                1,
                Some(cursor.with_bind(
                    PromptSearchSource::History,
                    [0xAB; 32],
                    epoch,
                    high_water,
                    schema,
                )),
            )
            .expect_err("cross query")
            .code(),
        PromptHistoryErrorCode::InvalidQuery
    );
    assert_eq!(
        history
            .recent(
                1,
                Some(cursor.with_bind(
                    PromptSearchSource::History,
                    cursor_query_hash(&cursor),
                    epoch,
                    high_water.saturating_add(1),
                    schema,
                )),
            )
            .expect_err("stale high water")
            .code(),
        PromptHistoryErrorCode::InvalidQuery
    );
    assert_eq!(
        history
            .recent(
                1,
                Some(cursor.with_bind(
                    PromptSearchSource::History,
                    cursor_query_hash(&cursor),
                    epoch,
                    high_water,
                    schema.saturating_add(1),
                )),
            )
            .expect_err("schema mismatch")
            .code(),
        PromptHistoryErrorCode::InvalidQuery
    );
}

fn cursor_query_hash(_cursor: &super::HistoryCursor) -> [u8; 32] {
    super::history_recent_query_sha256()
}

#[test]
fn search_continuation_rejects_high_water_and_schema_mismatch() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("prompts.sqlite3");
    let mut history = PromptHistoryStore::open(&path).expect("open");
    for tail in 1_u8..=3 {
        history_testing::commit_delivered(
            &mut history,
            accepted(
                tail,
                "shared token for bind",
                i64::from(tail) * 1_000,
                PromptHistoryProvenance::default(),
            ),
        )
        .expect("seed");
    }
    while history
        .rebuild_search(PromptSearchBudget::default())
        .expect("index")
        .remaining
        > 0
    {}
    let first = history
        .search(
            &PromptSearchQuery {
                text: "shared".into(),
                source: PromptSearchSource::History,
                cursor: None,
                page_size: 1,
            },
            PromptSearchBudget::default(),
        )
        .expect("page 1");
    let cursor = first.next.expect("continuation");
    let (epoch, high_water) = history_testing::index_seqs(&history).expect("seqs");
    let schema = history_testing::schema_version(&history).expect("schema");
    let query_sha256: [u8; 32] = Sha256::digest(b"shared").into();
    assert_eq!(
        history
            .search(
                &PromptSearchQuery {
                    text: "shared".into(),
                    source: PromptSearchSource::History,
                    cursor: Some(cursor.with_bind(
                        PromptSearchSource::History,
                        query_sha256,
                        epoch,
                        high_water,
                        schema.saturating_add(1),
                    )),
                    page_size: 1,
                },
                PromptSearchBudget::default(),
            )
            .expect_err("schema")
            .code(),
        PromptHistoryErrorCode::InvalidQuery
    );
    history_testing::set_high_water_seq(&history, high_water.saturating_add(7)).expect("hw");
    assert_eq!(
        history
            .search(
                &PromptSearchQuery {
                    text: "shared".into(),
                    source: PromptSearchSource::History,
                    cursor: Some(cursor),
                    page_size: 1,
                },
                PromptSearchBudget::default(),
            )
            .expect_err("high water")
            .code(),
        PromptHistoryErrorCode::InvalidQuery
    );
}

#[test]
fn history_open_is_unavailable_when_lineage_quarantined() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("prompts.sqlite3");
    PromptHistoryStore::open(&path).expect("seed schema");
    let conn = Connection::open(&path).expect("raw");
    conn.execute(
        "DROP TRIGGER prompt_lineage_quarantine_append_only_insert",
        [],
    )
    .expect("drop migration-owned insert guard for fixture");
    conn.execute(
        "INSERT INTO prompt_lineage_quarantine(
            source_kind, command_id, event_id, reason, command_sha256, quarantined_at_ms
         ) VALUES ('prompt', ?1, NULL, 'fixture', ?2, 1)",
        rusqlite::params![command_id(1).as_bytes().as_slice(), vec![0_u8; 32]],
    )
    .expect("seed quarantine");
    drop(conn);
    assert_eq!(
        PromptHistoryStore::open(&path)
            .expect_err("quarantine gate")
            .code(),
        PromptHistoryErrorCode::LineageQuarantine
    );
    let rendered = format!("{}", PromptHistoryStore::open(&path).expect_err("opaque"));
    assert!(!rendered.contains("sqlite"));
    assert!(!rendered.contains("SELECT"));
    assert!(!rendered.contains("prompt"));
    assert!(!rendered.contains('\\') && !rendered.contains('/'));
}

#[test]
fn search_without_host_scheduler_is_unscheduled_while_dirty() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("prompts.sqlite3");
    let mut history = PromptHistoryStore::open(&path).expect("open");
    history_testing::commit_delivered(
        &mut history,
        accepted(
            1,
            "dirty until a host drains",
            1_000,
            PromptHistoryProvenance::default(),
        ),
    )
    .expect("commit");
    assert!(history.is_search_dirty().expect("dirty"));
    assert_eq!(
        history
            .search(
                &PromptSearchQuery {
                    text: "dirty".into(),
                    source: PromptSearchSource::History,
                    cursor: None,
                    page_size: 10,
                },
                PromptSearchBudget::default(),
            )
            .expect_err("no scheduler")
            .code(),
        PromptHistoryErrorCode::IndexUnscheduled
    );
    {
        let reopened = PromptHistoryStore::open(&path).expect("reopen");
        assert_eq!(
            reopened
                .search(
                    &PromptSearchQuery {
                        text: "dirty".into(),
                        source: PromptSearchSource::History,
                        cursor: None,
                        page_size: 10,
                    },
                    PromptSearchBudget::default(),
                )
                .expect_err("claim does not survive reopen")
                .code(),
            PromptHistoryErrorCode::IndexUnscheduled
        );
    }
    history.claim_index_scheduler();
    let claimed = history
        .search(
            &PromptSearchQuery {
                text: "dirty".into(),
                source: PromptSearchSource::History,
                cursor: None,
                page_size: 10,
            },
            PromptSearchBudget::default(),
        )
        .expect("claimed host may observe a dirty index");
    assert!(claimed.hits.is_empty());
}

#[test]
fn drain_and_rebuild_fail_closed_on_corrupt_history_hash() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("prompts.sqlite3");
    let mut history = PromptHistoryStore::open(&path).expect("open");
    history_testing::commit_delivered(
        &mut history,
        accepted(
            1,
            "index-hash-secret",
            1_000,
            PromptHistoryProvenance::default(),
        ),
    )
    .expect("commit");
    drop(history);
    let conn = Connection::open(&path).expect("raw");
    conn.execute(
        "UPDATE prompt_history SET body_sha256 = ?1",
        [vec![0_u8; 32]],
    )
    .expect("corrupt hash");
    drop(conn);
    let mut history = PromptHistoryStore::open(&path).expect("reopen");
    assert_eq!(
        history
            .drain_index(PromptSearchBudget::default())
            .expect_err("drain hash")
            .code(),
        PromptHistoryErrorCode::Storage
    );
    assert_eq!(history.pending_count().expect("pending remains"), 1);
    assert!(history.is_search_dirty().expect("dirty after drain fail"));
    let drained = history
        .search(
            &PromptSearchQuery {
                text: "index-hash-secret".into(),
                source: PromptSearchSource::History,
                cursor: None,
                page_size: 10,
            },
            PromptSearchBudget::default(),
        )
        .expect("claimed dirty search");
    assert!(drained.hits.is_empty(), "corrupt body must not enter FTS");
    assert_eq!(
        history
            .rebuild_search(PromptSearchBudget::default())
            .expect_err("rebuild hash")
            .code(),
        PromptHistoryErrorCode::Storage
    );
    let rebuilt = history
        .search(
            &PromptSearchQuery {
                text: "index-hash-secret".into(),
                source: PromptSearchSource::History,
                cursor: None,
                page_size: 10,
            },
            PromptSearchBudget::default(),
        )
        .expect("still unsearchable");
    assert!(rebuilt.hits.is_empty());
}

#[test]
fn drain_fails_closed_on_broken_history_lineage() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("prompts.sqlite3");
    let mut history = PromptHistoryStore::open(&path).expect("open");
    history_testing::commit_delivered(
        &mut history,
        accepted(
            1,
            "lineage-secret",
            1_000,
            PromptHistoryProvenance::default(),
        ),
    )
    .expect("commit");
    drop(history);
    let conn = Connection::open(&path).expect("raw");
    conn.execute(
        "UPDATE prompt_history
         SET prompt_id = ?1, prompt_version_id = ?2",
        rusqlite::params![
            prompt_id(9).as_bytes().as_slice(),
            version_id(9).as_bytes().as_slice()
        ],
    )
    .expect("break lineage");
    drop(conn);
    let mut history = PromptHistoryStore::open(&path).expect("reopen");
    assert_eq!(
        history
            .drain_index(PromptSearchBudget::default())
            .expect_err("lineage")
            .code(),
        PromptHistoryErrorCode::ProvenanceMismatch
    );
    assert_eq!(history.pending_count().expect("pending remains"), 1);
    let page = history
        .search(
            &PromptSearchQuery {
                text: "lineage-secret".into(),
                source: PromptSearchSource::History,
                cursor: None,
                page_size: 10,
            },
            PromptSearchBudget::default(),
        )
        .expect("claimed dirty search");
    assert!(page.hits.is_empty());
}

#[test]
fn drain_and_rebuild_fail_closed_on_corrupt_saved_hash() {
    let dir = TempDir::new().expect("tempdir");
    let (mut prompts, mut history) = open_pair(&dir);
    prompts
        .execute(
            command_id(1),
            PromptCommand::CreatePrompt(CreatePrompt {
                prompt_id: prompt_id(1),
                prompt_version_id: version_id(1),
                title: "Saved".into(),
                description: None,
                tags: Vec::new(),
                variables: Vec::new(),
                body: "saved-hash-secret".into(),
                created_at_ms: 1_000,
            }),
        )
        .expect("saved");
    let path = dir.path().join("prompts.sqlite3");
    drop(prompts);
    let conn = Connection::open(&path).expect("raw");
    conn.execute("DROP TRIGGER prompt_versions_immutable_update", [])
        .expect("drop immutability for fixture");
    conn.execute(
        "UPDATE prompt_versions SET body_sha256 = ?1",
        [vec![0_u8; 32]],
    )
    .expect("corrupt saved hash");
    drop(conn);
    assert_eq!(
        history
            .drain_index(PromptSearchBudget::default())
            .expect_err("saved drain hash")
            .code(),
        PromptHistoryErrorCode::Storage
    );
    assert!(history.pending_count().expect("pending") >= 1);
    assert_eq!(
        history
            .rebuild_search(PromptSearchBudget::default())
            .expect_err("saved rebuild hash")
            .code(),
        PromptHistoryErrorCode::Storage
    );
    history.claim_index_scheduler();
    let page = history
        .search(
            &PromptSearchQuery {
                text: "saved-hash-secret".into(),
                source: PromptSearchSource::Saved,
                cursor: None,
                page_size: 10,
            },
            PromptSearchBudget::default(),
        )
        .expect("unsearchable");
    assert!(page.hits.is_empty());
}
