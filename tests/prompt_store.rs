use std::path::{Path, PathBuf};

use devmanager::domain::{
    CommandId, EventId, PromptChainId, PromptChainLinkId, PromptId, PromptVersionId,
};
use devmanager::prompts::{
    ArchivePrompt, CreatePrompt, CreatePromptChain, CreatePromptVersion, InsertPromptChainLink,
    MovePromptChainLink, PromptChainCommand, PromptCommand, PromptEvent, PromptMutationReceipt,
    PromptStore, PromptStoreError, PromptVersion, RemovePromptChainLink, RenamePrompt,
    RestorePrompt, SavedPrompt, SetPromptTags, UpdatePromptChainLinkVersion,
};
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

fn db_path(dir: &TempDir) -> PathBuf {
    dir.path().join("prompts.sqlite3")
}

fn command_id(tail: u8) -> CommandId {
    CommandId::from_bytes([
        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        tail,
    ])
    .expect("UUIDv7 command id")
}

fn prompt_id(tail: u8) -> PromptId {
    PromptId::from_bytes([
        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        tail,
    ])
    .expect("UUIDv7 prompt id")
}

fn version_id(tail: u8) -> PromptVersionId {
    PromptVersionId::from_bytes([
        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        tail,
    ])
    .expect("UUIDv7 prompt version id")
}

fn chain_id(tail: u8) -> PromptChainId {
    PromptChainId::from_bytes([
        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
        tail,
    ])
    .expect("UUIDv7 prompt chain id")
}

fn link_id(tail: u8) -> PromptChainLinkId {
    PromptChainLinkId::from_bytes([
        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02,
        tail,
    ])
    .expect("UUIDv7 prompt chain link id")
}

fn create_prompt(prompt_id: PromptId, version_id: PromptVersionId) -> PromptCommand {
    PromptCommand::CreatePrompt(CreatePrompt {
        prompt_id,
        prompt_version_id: version_id,
        title: "Review code".into(),
        description: Some("A bounded local prompt".into()),
        tags: vec![" Rust ".into(), "review".into(), "rust".into()],
        variables: Vec::new(),
        body: "Review this code carefully.".into(),
        created_at_ms: 1_725_000_000_000,
    })
}

fn open_store(path: &Path) -> PromptStore {
    PromptStore::open(path).expect("open isolated prompt store")
}

#[test]
fn create_prompt_creates_version_one() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let mut store = open_store(&path);
    let prompt = prompt_id(1);
    let version = version_id(2);

    let receipt = store
        .execute(command_id(3), create_prompt(prompt, version))
        .expect("create prompt");

    assert_eq!(receipt.prompt_id, prompt);
    assert_eq!(receipt.prompt_version_id, version);
    assert_eq!(receipt.revision, 1);
    let saved = store
        .get_prompt(prompt)
        .expect("query prompt")
        .expect("saved prompt");
    assert_eq!(saved.current_version_id, version);
    assert_eq!(saved.tags, vec!["rust", "review"]);
    let current = store
        .get_version(version)
        .expect("query version")
        .expect("version one");
    assert_eq!(current.prompt_id, prompt);
    assert_eq!(current.version, 1);
}

#[test]
fn editing_creates_immutable_next_version() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let mut store = open_store(&path);
    let prompt = prompt_id(10);
    let first = version_id(11);
    store
        .execute(command_id(12), create_prompt(prompt, first))
        .expect("create prompt");

    let second = version_id(13);
    store
        .execute(
            command_id(14),
            PromptCommand::CreatePromptVersion(CreatePromptVersion {
                prompt_id: prompt,
                prompt_version_id: second,
                variables: Vec::new(),
                body: "A revised body.".into(),
                created_at_ms: 2,
                expected_revision: 1,
            }),
        )
        .expect("create immutable next version");

    let versions = store.list_versions(prompt, 0, 10).expect("list versions");
    assert_eq!(
        versions.iter().map(|v| v.version).collect::<Vec<_>>(),
        vec![2, 1]
    );
    assert_eq!(versions[1].body, "Review this code carefully.");
    assert_eq!(versions[0].body, "A revised body.");
    assert_eq!(
        store
            .get_prompt(prompt)
            .unwrap()
            .unwrap()
            .current_version_id,
        second
    );
}

#[test]
fn current_version_must_belong_to_prompt() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let mut store = open_store(&path);
    let first_prompt = prompt_id(20);
    let first_version = version_id(21);
    let second_prompt = prompt_id(22);
    let second_version = version_id(23);
    store
        .execute(command_id(24), create_prompt(first_prompt, first_version))
        .expect("first prompt");
    store
        .execute(command_id(25), create_prompt(second_prompt, second_version))
        .expect("second prompt");
    drop(store);

    let conn = Connection::open(&path).expect("open isolated raw connection");
    conn.execute_batch("PRAGMA foreign_keys = ON;")
        .expect("foreign keys");
    let error = conn
        .execute(
            "UPDATE saved_prompts SET current_version_id = ?1 WHERE prompt_id = ?2",
            rusqlite::params![
                second_version.as_bytes().as_slice(),
                first_prompt.as_bytes().as_slice()
            ],
        )
        .expect_err("cross-prompt current version must fail");
    assert!(error.to_string().to_lowercase().contains("foreign key"));
}

#[test]
fn duplicate_command_is_idempotent() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let mut store = open_store(&path);
    let command = create_prompt(prompt_id(30), version_id(31));
    let first = store
        .execute(command_id(32), command.clone())
        .expect("first create");
    let second = store
        .execute(command_id(32), command)
        .expect("idempotent retry");
    assert_eq!(first, second);
    assert_eq!(store.count_prompts().expect("count prompts"), 1);
    assert_eq!(store.count_prompt_events().expect("count events"), 1);
}

#[test]
fn expected_revision_prevents_lost_update() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let mut store = open_store(&path);
    let prompt = prompt_id(40);
    store
        .execute(command_id(41), create_prompt(prompt, version_id(42)))
        .expect("create prompt");

    let error = store
        .execute(
            command_id(43),
            PromptCommand::RenamePrompt(RenamePrompt {
                prompt_id: prompt,
                title: "Lost update".into(),
                expected_revision: 0,
            }),
        )
        .expect_err("stale revision must reject");
    assert!(matches!(
        error,
        PromptStoreError::RevisionConflict {
            expected: 0,
            actual: 1
        }
    ));
    assert_eq!(
        store.get_prompt(prompt).unwrap().unwrap().title,
        "Review code"
    );
}

#[test]
fn archived_prompt_versions_remain_readable() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let mut store = open_store(&path);
    let prompt = prompt_id(50);
    let version = version_id(51);
    store
        .execute(command_id(52), create_prompt(prompt, version))
        .expect("create prompt");
    store
        .execute(
            command_id(53),
            PromptCommand::ArchivePrompt(ArchivePrompt {
                prompt_id: prompt,
                archived_at_ms: 3,
                expected_revision: 1,
            }),
        )
        .expect("archive prompt");

    assert!(store.get_version(version).unwrap().is_some());
    assert_eq!(store.list_versions(prompt, 0, 10).unwrap().len(), 1);
    assert!(store
        .get_prompt(prompt)
        .unwrap()
        .unwrap()
        .archived_at_ms
        .is_some());
}

#[test]
fn body_hash_round_trips() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let mut store = open_store(&path);
    let body = "Unicode: café — keep bytes.";
    let prompt = prompt_id(60);
    let version = version_id(61);
    let command = match create_prompt(prompt, version) {
        PromptCommand::CreatePrompt(mut command) => {
            command.body = body.into();
            PromptCommand::CreatePrompt(command)
        }
        _ => unreachable!(),
    };
    store
        .execute(command_id(62), command)
        .expect("create prompt");
    let stored = store.get_version(version).unwrap().unwrap();
    let expected: [u8; 32] = Sha256::digest(body.as_bytes()).into();
    assert_eq!(stored.body_sha256, expected);
}

#[test]
fn validation_bounds_reject_without_body_diagnostics() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let mut store = open_store(&path);
    let sentinel = "secret prompt body that must not appear in diagnostics";
    let error = store
        .execute(
            command_id(70),
            PromptCommand::CreatePrompt(CreatePrompt {
                prompt_id: prompt_id(71),
                prompt_version_id: version_id(72),
                title: "title".into(),
                description: None,
                tags: Vec::new(),
                variables: Vec::new(),
                body: format!("{}{}", sentinel, "x".repeat(256 * 1024)),
                created_at_ms: 1,
            }),
        )
        .expect_err("oversized body must reject");
    assert!(!error.to_string().contains(sentinel));
}

#[test]
fn atomic_rollback_leaves_no_partial_prompt_or_event() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let mut store = open_store(&path);
    let shared_version = version_id(81);
    store
        .execute(command_id(82), create_prompt(prompt_id(80), shared_version))
        .expect("first prompt");

    let error = store
        .execute(command_id(84), create_prompt(prompt_id(83), shared_version))
        .expect_err("duplicate version id must rollback create");
    assert!(matches!(error, PromptStoreError::ConstraintViolation));
    assert!(store.get_prompt(prompt_id(83)).unwrap().is_none());
    assert_eq!(store.count_prompts().unwrap(), 1);
    assert_eq!(store.count_prompt_events().unwrap(), 1);
}

#[test]
fn version_history_is_immutable_at_the_sqlite_boundary() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let mut store = open_store(&path);
    let prompt = prompt_id(85);
    let version = version_id(86);
    store
        .execute(command_id(87), create_prompt(prompt, version))
        .expect("create prompt");
    drop(store);

    let conn = Connection::open(&path).expect("open isolated raw connection");
    conn.execute_batch("PRAGMA foreign_keys = ON;")
        .expect("foreign keys");
    let update = conn.execute(
        "UPDATE prompt_versions SET body = 'tampered' WHERE prompt_version_id = ?1",
        [version.as_bytes().as_slice()],
    );
    assert!(update.is_err(), "version body update must be rejected");
    let delete = conn.execute(
        "DELETE FROM prompt_versions WHERE prompt_version_id = ?1",
        [version.as_bytes().as_slice()],
    );
    assert!(delete.is_err(), "version deletion must be rejected");
}

#[test]
fn corrupt_tag_projection_fails_closed() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let mut store = open_store(&path);
    let prompt = prompt_id(88);
    store
        .execute(command_id(89), create_prompt(prompt, version_id(90)))
        .expect("create prompt");
    drop(store);

    let conn = Connection::open(&path).expect("open isolated raw connection");
    conn.execute_batch("PRAGMA foreign_keys = ON;")
        .expect("foreign keys");
    conn.execute(
        "UPDATE prompt_tags SET tag = ' RUST ' WHERE prompt_id = ?1 AND tag = 'rust'",
        [prompt.as_bytes().as_slice()],
    )
    .expect("corrupt isolated fixture");
    let store = open_store(&path);
    let error = store
        .get_prompt(prompt)
        .expect_err("non-normalized tag must not be returned as valid data");
    assert!(matches!(error, PromptStoreError::Corruption(_)));
}

#[test]
fn corrupt_tag_position_fails_closed() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let mut store = open_store(&path);
    let prompt = prompt_id(87);
    store
        .execute(command_id(88), create_prompt(prompt, version_id(89)))
        .expect("create prompt");
    drop(store);

    let conn = Connection::open(&path).expect("open isolated raw connection");
    conn.execute_batch("PRAGMA foreign_keys = ON;")
        .expect("foreign keys");
    conn.execute(
        "UPDATE prompt_tags SET position = 2 WHERE prompt_id = ?1 AND tag = 'review'",
        [prompt.as_bytes().as_slice()],
    )
    .expect("corrupt isolated fixture");
    let store = open_store(&path);
    let error = store
        .get_prompt(prompt)
        .expect_err("sparse tag positions must not be returned as valid data");
    assert!(matches!(error, PromptStoreError::Corruption(_)));
}

#[test]
fn prompt_event_provenance_is_append_only() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let mut store = open_store(&path);
    store
        .execute(command_id(91), create_prompt(prompt_id(90), version_id(92)))
        .expect("create prompt");
    drop(store);

    let conn = Connection::open(&path).expect("open isolated raw connection");
    conn.execute_batch("PRAGMA foreign_keys = ON;")
        .expect("foreign keys");
    let update = conn.execute(
        "UPDATE prompt_events SET event_type = 'tampered' WHERE sequence = 1",
        [],
    );
    assert!(update.is_err(), "prompt event provenance must be immutable");
    let delete = conn.execute("DELETE FROM prompt_events WHERE sequence = 1", []);
    assert!(
        delete.is_err(),
        "prompt event provenance must be append-only"
    );
}

#[test]
fn prompt_event_payload_uses_the_prompt_wire_codec() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let mut store = open_store(&path);
    store
        .execute(
            command_id(98),
            create_prompt(prompt_id(99), version_id(100)),
        )
        .expect("create prompt");
    drop(store);

    let conn = Connection::open(&path).expect("open isolated raw connection");
    let payload: Vec<u8> = conn
        .query_row(
            "SELECT payload FROM prompt_events WHERE sequence = 1",
            [],
            |row| row.get(0),
        )
        .expect("event payload");
    let event = PromptEvent::decode(&payload).expect("decode prompt event payload");
    assert_eq!(event.event_type(), "prompt.created");
}

#[test]
fn projection_rejects_noncanonical_prompt_created_event() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(101);
    let version_id = version_id(102);
    let command = command_id(103);
    let version =
        PromptVersion::new(version_id, prompt, 1, "body".into(), 1).expect("valid version");
    let event = PromptEvent::PromptCreated {
        prompt: SavedPrompt {
            id: prompt,
            title: "  noncanonical title  ".into(),
            description: None,
            tags: Vec::new(),
            current_version_id: version_id,
            revision: 1,
            archived_at_ms: None,
        },
        version,
    };

    drop(open_store(&path));
    let conn = Connection::open(&path).expect("open isolated raw connection");
    conn.execute_batch("PRAGMA foreign_keys = ON;")
        .expect("foreign keys");
    let receipt = PromptMutationReceipt {
        command_id: command,
        prompt_id: prompt,
        prompt_version_id: version_id,
        revision: 1,
    };
    conn.execute(
        "INSERT INTO prompt_command_receipts(
            command_id, command_sha256, prompt_id, prompt_version_id, revision,
            receipt, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            command.as_bytes().as_slice(),
            [0u8; 32].as_slice(),
            prompt.as_bytes().as_slice(),
            version_id.as_bytes().as_slice(),
            1_i64,
            rmp_serde::to_vec_named(&receipt).expect("receipt payload"),
            1_i64,
        ],
    )
    .expect("insert receipt");
    conn.execute(
        "INSERT INTO prompt_events(
            prompt_event_id, command_id, prompt_id, event_type, occurred_at_ms, payload
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            EventId::new().as_bytes().as_slice(),
            command.as_bytes().as_slice(),
            prompt.as_bytes().as_slice(),
            event.event_type(),
            1_i64,
            event.encode().expect("event payload"),
        ],
    )
    .expect("insert event");
    drop(conn);

    let mut store = open_store(&path);
    let error = store
        .rebuild_projection()
        .expect_err("noncanonical event must fail closed");
    assert!(matches!(error, PromptStoreError::Corruption(_)));
}

#[test]
fn projection_rejects_noncanonical_prompt_mutation_event() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(107);
    let version_id = version_id(108);
    let mut store = open_store(&path);
    store
        .execute(command_id(109), create_prompt(prompt, version_id))
        .expect("create prompt");
    drop(store);

    let command = command_id(110);
    let event = PromptEvent::PromptRenamed {
        prompt_id: prompt,
        title: "  noncanonical title  ".into(),
        revision: 2,
    };
    let conn = Connection::open(&path).expect("open isolated raw connection");
    conn.execute_batch("PRAGMA foreign_keys = ON;")
        .expect("foreign keys");
    let receipt = PromptMutationReceipt {
        command_id: command,
        prompt_id: prompt,
        prompt_version_id: version_id,
        revision: 2,
    };
    conn.execute(
        "INSERT INTO prompt_command_receipts(
            command_id, command_sha256, prompt_id, prompt_version_id, revision,
            receipt, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            command.as_bytes().as_slice(),
            [0u8; 32].as_slice(),
            prompt.as_bytes().as_slice(),
            version_id.as_bytes().as_slice(),
            2_i64,
            rmp_serde::to_vec_named(&receipt).expect("receipt payload"),
            2_i64,
        ],
    )
    .expect("insert receipt");
    conn.execute(
        "INSERT INTO prompt_events(
            prompt_event_id, command_id, prompt_id, event_type, occurred_at_ms, payload
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            EventId::new().as_bytes().as_slice(),
            command.as_bytes().as_slice(),
            prompt.as_bytes().as_slice(),
            event.event_type(),
            2_i64,
            event.encode().expect("event payload"),
        ],
    )
    .expect("insert event");
    drop(conn);

    let mut store = open_store(&path);
    let error = store
        .rebuild_projection()
        .expect_err("noncanonical mutation event must fail closed");
    assert!(matches!(error, PromptStoreError::Corruption(_)));
}

#[test]
fn version_variables_round_trip_and_remain_with_immutable_version() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let mut store = open_store(&path);
    let prompt = prompt_id(93);
    let first = PromptCommand::CreatePrompt(CreatePrompt {
        prompt_id: prompt,
        prompt_version_id: version_id(94),
        title: "Template".into(),
        description: None,
        tags: Vec::new(),
        variables: vec![" reviewer ".into(), "reviewer".into()],
        body: "Review {{reviewer}}".into(),
        created_at_ms: 1,
    });
    store
        .execute(command_id(95), first)
        .expect("create template");
    let initial = store.get_version(version_id(94)).unwrap().unwrap();
    assert_eq!(initial.variables, vec!["reviewer"]);

    store
        .execute(
            command_id(96),
            PromptCommand::CreatePromptVersion(CreatePromptVersion {
                prompt_id: prompt,
                prompt_version_id: version_id(97),
                variables: vec![" task ".into()],
                body: "Review {{task}}".into(),
                created_at_ms: 2,
                expected_revision: 1,
            }),
        )
        .expect("create second version");
    assert_eq!(
        store
            .get_version(version_id(94))
            .unwrap()
            .unwrap()
            .variables,
        vec!["reviewer"]
    );
    assert_eq!(
        store
            .get_version(version_id(97))
            .unwrap()
            .unwrap()
            .variables,
        vec!["task"]
    );
    store
        .rebuild_projection()
        .expect("rebuild prompt projection");
    assert_eq!(
        store
            .get_prompt(prompt)
            .unwrap()
            .unwrap()
            .current_version_id,
        version_id(97)
    );
}

#[test]
fn version_variables_are_immutable_at_the_sqlite_boundary() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let mut store = open_store(&path);
    let prompt = prompt_id(104);
    store
        .execute(
            command_id(105),
            PromptCommand::CreatePrompt(CreatePrompt {
                prompt_id: prompt,
                prompt_version_id: version_id(106),
                title: "Template".into(),
                description: None,
                tags: Vec::new(),
                variables: vec!["reviewer".into()],
                body: "Review {{reviewer}}".into(),
                created_at_ms: 1,
            }),
        )
        .expect("create template");
    drop(store);

    let conn = Connection::open(&path).expect("open isolated raw connection");
    conn.execute_batch("PRAGMA foreign_keys = ON;")
        .expect("foreign keys");
    let update = conn.execute(
        "UPDATE prompt_version_variables SET variable = 'task' WHERE prompt_version_id = ?1",
        [version_id(106).as_bytes().as_slice()],
    );
    assert!(update.is_err(), "version variables must not be updated");
    let delete = conn.execute(
        "DELETE FROM prompt_version_variables WHERE prompt_version_id = ?1",
        [version_id(106).as_bytes().as_slice()],
    );
    assert!(delete.is_err(), "version variables must not be deleted");
}

#[test]
fn schema_and_projection_rebuild_preserve_prompt_metadata() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let mut store = open_store(&path);
    let prompt = prompt_id(90);
    store
        .execute(command_id(91), create_prompt(prompt, version_id(92)))
        .expect("create prompt");
    store
        .execute(
            command_id(93),
            PromptCommand::SetPromptTags(SetPromptTags {
                prompt_id: prompt,
                tags: vec![" durable ".into(), "rust".into()],
                expected_revision: 1,
            }),
        )
        .expect("set tags");
    store
        .execute(
            command_id(94),
            PromptCommand::RenamePrompt(RenamePrompt {
                prompt_id: prompt,
                title: "Rebuilt prompt".into(),
                expected_revision: 2,
            }),
        )
        .expect("rename");
    let before = store.get_prompt(prompt).unwrap().unwrap();
    let before_versions = store.list_versions(prompt, 0, 10).unwrap();

    let report = store
        .rebuild_projection()
        .expect("rebuild prompt projection");
    assert_eq!(report.events_replayed, 3);
    assert_eq!(store.get_prompt(prompt).unwrap().unwrap(), before);
    assert_eq!(store.list_versions(prompt, 0, 10).unwrap(), before_versions);

    let conn = Connection::open(&path).expect("raw schema connection");
    let migration: String = conn
        .query_row(
            "SELECT name FROM schema_migrations WHERE name = 'phase07-prompts-v1'",
            [],
            |row| row.get(0),
        )
        .expect("phase 07 migration");
    assert_eq!(migration, "phase07-prompts-v1");
}

#[test]
fn restore_prompt_reverses_archive_with_revision_protection() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let mut store = open_store(&path);
    let prompt = prompt_id(100);
    store
        .execute(command_id(101), create_prompt(prompt, version_id(102)))
        .expect("create prompt");
    store
        .execute(
            command_id(103),
            PromptCommand::ArchivePrompt(ArchivePrompt {
                prompt_id: prompt,
                archived_at_ms: 3,
                expected_revision: 1,
            }),
        )
        .expect("archive");
    store
        .execute(
            command_id(104),
            PromptCommand::RestorePrompt(RestorePrompt {
                prompt_id: prompt,
                expected_revision: 2,
            }),
        )
        .expect("restore");
    assert!(store
        .get_prompt(prompt)
        .unwrap()
        .unwrap()
        .archived_at_ms
        .is_none());
}

#[test]
fn identical_body_edit_is_a_semantic_noop() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let mut store = open_store(&path);
    let prompt = prompt_id(110);
    let version = version_id(111);
    store
        .execute(command_id(112), create_prompt(prompt, version))
        .expect("create prompt");

    let receipt = store
        .execute(
            command_id(113),
            PromptCommand::CreatePromptVersion(CreatePromptVersion {
                prompt_id: prompt,
                prompt_version_id: version_id(114),
                variables: Vec::new(),
                body: "Review this code carefully.".into(),
                created_at_ms: 2,
                expected_revision: 1,
            }),
        )
        .expect("identical body should be acknowledged");

    assert_eq!(receipt.prompt_version_id, version);
    assert_eq!(receipt.revision, 1);
    assert_eq!(store.list_versions(prompt, 0, 10).unwrap().len(), 1);
    assert_eq!(store.count_prompt_events().unwrap(), 1);
}

#[test]
fn normalized_metadata_edit_is_a_semantic_noop() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let mut store = open_store(&path);
    let prompt = prompt_id(120);
    store
        .execute(command_id(121), create_prompt(prompt, version_id(122)))
        .expect("create prompt");

    let rename = store
        .execute(
            command_id(123),
            PromptCommand::RenamePrompt(RenamePrompt {
                prompt_id: prompt,
                title: "  Review code  ".into(),
                expected_revision: 1,
            }),
        )
        .expect("same normalized title should be acknowledged");
    assert_eq!(rename.revision, 1);

    let tags = store
        .execute(
            command_id(124),
            PromptCommand::SetPromptTags(SetPromptTags {
                prompt_id: prompt,
                tags: vec![" RUST ".into(), "review".into(), "RUST".into()],
                expected_revision: 1,
            }),
        )
        .expect("same normalized tags should be acknowledged");
    assert_eq!(tags.revision, 1);
    assert_eq!(store.get_prompt(prompt).unwrap().unwrap().revision, 1);
    assert_eq!(store.count_prompt_events().unwrap(), 1);
}

#[test]
fn repeated_archive_and_restore_are_semantic_noops() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let mut store = open_store(&path);
    let prompt = prompt_id(130);
    store
        .execute(command_id(131), create_prompt(prompt, version_id(132)))
        .expect("create prompt");
    store
        .execute(
            command_id(133),
            PromptCommand::ArchivePrompt(ArchivePrompt {
                prompt_id: prompt,
                archived_at_ms: 3,
                expected_revision: 1,
            }),
        )
        .expect("archive");
    let repeated_archive = store
        .execute(
            command_id(134),
            PromptCommand::ArchivePrompt(ArchivePrompt {
                prompt_id: prompt,
                archived_at_ms: 4,
                expected_revision: 2,
            }),
        )
        .expect("repeated archive should be acknowledged");
    assert_eq!(repeated_archive.revision, 2);

    store
        .execute(
            command_id(135),
            PromptCommand::RestorePrompt(RestorePrompt {
                prompt_id: prompt,
                expected_revision: 2,
            }),
        )
        .expect("restore");
    let repeated_restore = store
        .execute(
            command_id(136),
            PromptCommand::RestorePrompt(RestorePrompt {
                prompt_id: prompt,
                expected_revision: 3,
            }),
        )
        .expect("repeated restore should be acknowledged");
    assert_eq!(repeated_restore.revision, 3);
    assert_eq!(store.count_prompt_events().unwrap(), 3);
}

fn create_chain(chain: PromptChainId) -> PromptChainCommand {
    PromptChainCommand::CreatePromptChain(CreatePromptChain {
        chain_id: chain,
        title: "Review workflow".into(),
        description: Some("A local ordered chain".into()),
        created_at_ms: 10,
    })
}

fn insert_link(
    chain: PromptChainId,
    link: PromptChainLinkId,
    prompt: PromptId,
    version: Option<PromptVersionId>,
    before_link_id: Option<PromptChainLinkId>,
    expected_revision: u64,
) -> PromptChainCommand {
    PromptChainCommand::InsertPromptChainLink(InsertPromptChainLink {
        chain_id: chain,
        link_id: link,
        prompt_id: prompt,
        prompt_version_id: version,
        before_link_id,
        expected_revision,
    })
}

#[test]
fn chain_pins_current_versions_and_exposes_previous_next_context() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let mut store = open_store(&path);
    let prompt = prompt_id(140);
    let first_version = version_id(141);
    store
        .execute(command_id(142), create_prompt(prompt, first_version))
        .expect("create prompt");
    let chain = chain_id(143);
    store
        .execute_chain(command_id(144), create_chain(chain))
        .expect("create chain");
    store
        .execute_chain(
            command_id(145),
            insert_link(chain, link_id(146), prompt, None, None, 1),
        )
        .expect("append first link");

    store
        .execute(
            command_id(147),
            PromptCommand::CreatePromptVersion(CreatePromptVersion {
                prompt_id: prompt,
                prompt_version_id: version_id(148),
                variables: Vec::new(),
                body: "The revised review body.".into(),
                created_at_ms: 11,
                expected_revision: 1,
            }),
        )
        .expect("create second version");
    store
        .execute_chain(
            command_id(149),
            insert_link(chain, link_id(150), prompt, None, None, 2),
        )
        .expect("append second link");

    let links = store.list_chain_links(chain).expect("list chain links");
    assert_eq!(
        links.iter().map(|link| link.position).collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert_eq!(links[0].prompt_version_id, first_version);
    assert_eq!(links[1].prompt_version_id, version_id(148));

    let first_context = store
        .get_chain_link_context(chain, links[0].id)
        .expect("first context")
        .expect("first link");
    assert_eq!(first_context.previous_link_id, None);
    assert_eq!(first_context.next_link_id, Some(links[1].id));
    assert!(first_context.update_available);
    let last_context = store
        .get_chain_link_context(chain, links[1].id)
        .expect("last context")
        .expect("last link");
    assert_eq!(last_context.previous_link_id, Some(links[0].id));
    assert_eq!(last_context.next_link_id, None);
    assert!(!last_context.update_available);
}

#[test]
fn chain_insert_move_remove_keeps_dense_order() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let mut store = open_store(&path);
    let prompt = prompt_id(151);
    store
        .execute(command_id(152), create_prompt(prompt, version_id(153)))
        .expect("create prompt");
    let chain = chain_id(154);
    store
        .execute_chain(command_id(155), create_chain(chain))
        .expect("create chain");
    store
        .execute_chain(
            command_id(156),
            insert_link(chain, link_id(157), prompt, None, None, 1),
        )
        .expect("first link");
    store
        .execute_chain(
            command_id(158),
            insert_link(chain, link_id(159), prompt, None, None, 2),
        )
        .expect("second link");
    store
        .execute_chain(
            command_id(160),
            insert_link(chain, link_id(161), prompt, None, Some(link_id(159)), 3),
        )
        .expect("insert between links");
    assert_eq!(
        store
            .list_chain_links(chain)
            .unwrap()
            .iter()
            .map(|link| link.id)
            .collect::<Vec<_>>(),
        vec![link_id(157), link_id(161), link_id(159)]
    );

    store
        .execute_chain(
            command_id(162),
            PromptChainCommand::MovePromptChainLink(MovePromptChainLink {
                chain_id: chain,
                link_id: link_id(159),
                before_link_id: Some(link_id(157)),
                expected_revision: 4,
            }),
        )
        .expect("move link");
    assert_eq!(
        store
            .list_chain_links(chain)
            .unwrap()
            .iter()
            .map(|link| link.id)
            .collect::<Vec<_>>(),
        vec![link_id(159), link_id(157), link_id(161)]
    );

    store
        .execute_chain(
            command_id(163),
            PromptChainCommand::RemovePromptChainLink(RemovePromptChainLink {
                chain_id: chain,
                link_id: link_id(157),
                expected_revision: 5,
            }),
        )
        .expect("remove link");
    let links = store.list_chain_links(chain).unwrap();
    assert_eq!(
        links.iter().map(|link| link.position).collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert_eq!(
        links.iter().map(|link| link.id).collect::<Vec<_>>(),
        vec![link_id(159), link_id(161)]
    );
}

#[test]
fn chain_rejects_mismatched_version_without_mutating() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let mut store = open_store(&path);
    let first_prompt = prompt_id(164);
    let second_prompt = prompt_id(165);
    store
        .execute(
            command_id(166),
            create_prompt(first_prompt, version_id(167)),
        )
        .expect("first prompt");
    store
        .execute(
            command_id(168),
            create_prompt(second_prompt, version_id(169)),
        )
        .expect("second prompt");
    let chain = chain_id(170);
    store
        .execute_chain(command_id(171), create_chain(chain))
        .expect("chain");
    let error = store
        .execute_chain(
            command_id(172),
            insert_link(
                chain,
                link_id(173),
                first_prompt,
                Some(version_id(169)),
                None,
                1,
            ),
        )
        .expect_err("version from another prompt must fail");
    assert!(matches!(error, PromptStoreError::ConstraintViolation));
    assert!(store.list_chain_links(chain).unwrap().is_empty());
}

#[test]
fn chain_revision_conflict_changes_nothing() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let mut store = open_store(&path);
    let prompt = prompt_id(174);
    store
        .execute(command_id(175), create_prompt(prompt, version_id(176)))
        .expect("prompt");
    let chain = chain_id(177);
    store
        .execute_chain(command_id(178), create_chain(chain))
        .expect("chain");
    let error = store
        .execute_chain(
            command_id(179),
            insert_link(chain, link_id(180), prompt, None, None, 0),
        )
        .expect_err("stale chain revision");
    assert!(matches!(
        error,
        PromptStoreError::RevisionConflict {
            expected: 0,
            actual: 1
        }
    ));
    assert!(store.list_chain_links(chain).unwrap().is_empty());
    assert_eq!(store.count_chain_events().unwrap(), 1);
}

#[test]
fn chain_update_to_current_is_explicit_and_replayable() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let mut store = open_store(&path);
    let prompt = prompt_id(181);
    let first = version_id(182);
    let second = version_id(183);
    store
        .execute(command_id(184), create_prompt(prompt, first))
        .expect("prompt");
    let chain = chain_id(185);
    store
        .execute_chain(command_id(186), create_chain(chain))
        .expect("chain");
    store
        .execute_chain(
            command_id(187),
            insert_link(chain, link_id(188), prompt, Some(first), None, 1),
        )
        .expect("pinned link");
    store
        .execute(
            command_id(189),
            PromptCommand::CreatePromptVersion(CreatePromptVersion {
                prompt_id: prompt,
                prompt_version_id: second,
                variables: Vec::new(),
                body: "new body".into(),
                created_at_ms: 12,
                expected_revision: 1,
            }),
        )
        .expect("new prompt version");
    assert_eq!(
        store.list_chain_links(chain).unwrap()[0].prompt_version_id,
        first
    );

    store
        .execute_chain(
            command_id(190),
            PromptChainCommand::UpdatePromptChainLinkVersion(UpdatePromptChainLinkVersion {
                chain_id: chain,
                link_id: link_id(188),
                expected_revision: 2,
            }),
        )
        .expect("explicit update");
    assert_eq!(
        store.list_chain_links(chain).unwrap()[0].prompt_version_id,
        second
    );
    store
        .rebuild_projection()
        .expect("rebuild all prompt projections");
    assert_eq!(
        store.list_chain_links(chain).unwrap()[0].prompt_version_id,
        second
    );
}
