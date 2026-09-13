use std::path::{Path, PathBuf};

use devmanager::domain::{
    CommandId, EventId, PromptChainId, PromptChainLinkId, PromptId, PromptVersionId,
};
use devmanager::prompts::{
    ArchivePrompt, CreatePrompt, CreatePromptChain, CreatePromptVersion, InsertPromptChainLink,
    MovePromptChainLink, PromptChain, PromptChainCommand, PromptChainEvent,
    PromptChainMutationReceipt, PromptCommand, PromptEvent, PromptMutationReceipt, PromptStore,
    PromptStoreError, PromptValidationError, PromptVersion, RemovePromptChainLink, RenamePrompt,
    RestorePrompt, SavedPrompt, SetPromptTags, UpdatePromptChainLinkVersion,
};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

#[derive(Serialize)]
struct PromptChainCommandWire<'a> {
    schema_version: u32,
    original_command_sha256: [u8; 32],
    original_command_payload: Vec<u8>,
    command: &'a PromptChainCommand,
    resolved_prompt_version_id: Option<PromptVersionId>,
}

#[derive(Deserialize)]
struct PromptChainCommandWireOwned {
    schema_version: u32,
    original_command_sha256: [u8; 32],
    original_command_payload: Vec<u8>,
    command: PromptChainCommand,
    resolved_prompt_version_id: Option<PromptVersionId>,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct RawPromptChainEventWire {
    schema_version: u32,
    event: RawPromptChainEvent,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum RawPromptChainEvent {
    PromptChainCreated {
        chain: RawPromptChain,
    },
    PromptChainLinksReplaced {
        chain_id: PromptChainId,
        links: Vec<RawPromptChainLink>,
        revision: u64,
    },
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct RawPromptChainLink {
    id: PromptChainLinkId,
    chain_id: PromptChainId,
    position: u32,
    prompt_id: PromptId,
    prompt_version_id: PromptVersionId,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct RawPromptEventWire {
    schema_version: u32,
    event: RawPromptEvent,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum RawPromptEvent {
    PromptCreated {
        prompt: RawSavedPrompt,
        version: PromptVersion,
    },
    PromptRenamed {
        prompt_id: PromptId,
        title: String,
        revision: u64,
    },
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct RawSavedPrompt {
    id: PromptId,
    title: String,
    description: Option<String>,
    tags: Vec<String>,
    current_version_id: PromptVersionId,
    revision: u64,
    archived_at_ms: Option<i64>,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct RawPromptChain {
    id: PromptChainId,
    title: String,
    description: Option<String>,
    revision: u64,
    archived_at_ms: Option<i64>,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct RawPromptCommandWire {
    schema_version: u32,
    command: RawPromptCommand,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum RawPromptCommand {
    SetPromptTags {
        prompt_id: PromptId,
        tags: Vec<String>,
        expected_revision: u64,
    },
}

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

fn canonical_create_prompt(prompt_id: PromptId, version_id: PromptVersionId) -> PromptCommand {
    PromptCommand::CreatePrompt(CreatePrompt {
        prompt_id,
        prompt_version_id: version_id,
        title: "Review code".into(),
        description: Some("A bounded local prompt".into()),
        tags: vec!["rust".into(), "review".into()],
        variables: Vec::new(),
        body: "Review this code carefully.".into(),
        created_at_ms: 1_725_000_000_000,
    })
}

fn open_store(path: &Path) -> PromptStore {
    PromptStore::open(path).expect("open isolated prompt store")
}

#[test]
fn prompt_store_debug_is_opaque_and_does_not_leak_database_metadata() {
    let temp_dir = TempDir::new().expect("create isolated prompt store directory");
    let database_path = temp_dir
        .path()
        .join("PROMPT_STORE_DEBUG_ATTACKER_SENTINEL.sqlite3");
    let store = open_store(&database_path);

    let rendered = format!("{store:?}");
    let database_path_text = database_path.to_string_lossy().into_owned();
    let database_name = database_path
        .file_name()
        .expect("database filename")
        .to_string_lossy()
        .into_owned();
    let parent_path = database_path
        .parent()
        .expect("database parent")
        .to_string_lossy()
        .into_owned();

    assert_eq!(rendered, "PromptStore");
    for (label, forbidden) in [
        ("database path", database_path_text),
        ("database basename", database_name),
        ("database parent", parent_path),
        (
            "attacker sentinel",
            "PROMPT_STORE_DEBUG_ATTACKER_SENTINEL".into(),
        ),
        ("connection details", "Connection".into()),
    ] {
        assert!(
            !rendered.contains(&forbidden),
            "PromptStore Debug leaked {label}: {rendered}"
        );
    }
}

fn write_id_bytes(buffer: &mut Vec<u8>, id: &[u8; 16]) {
    rmp::encode::write_bin_len(buffer, 16).expect("write UUID length");
    buffer.extend_from_slice(id);
}

fn noncanonical_receipt_payload(receipt: &PromptMutationReceipt) -> Vec<u8> {
    let mut payload = Vec::new();
    rmp::encode::write_map_len(&mut payload, 4).expect("write receipt map");
    rmp::encode::write_str(&mut payload, "revision").expect("write revision key");
    rmp::encode::write_uint(&mut payload, receipt.revision).expect("write revision");
    rmp::encode::write_str(&mut payload, "prompt_version_id").expect("write version key");
    write_id_bytes(&mut payload, receipt.prompt_version_id.as_bytes());
    rmp::encode::write_str(&mut payload, "prompt_id").expect("write prompt key");
    write_id_bytes(&mut payload, receipt.prompt_id.as_bytes());
    rmp::encode::write_str(&mut payload, "command_id").expect("write command key");
    write_id_bytes(&mut payload, receipt.command_id.as_bytes());
    payload
}

fn noncanonical_prompt_renamed_payload(prompt_id: PromptId, title: &str, revision: u64) -> Vec<u8> {
    let mut payload = Vec::new();
    rmp::encode::write_map_len(&mut payload, 2).expect("write event wire map");
    rmp::encode::write_str(&mut payload, "event").expect("write event key");
    rmp::encode::write_map_len(&mut payload, 1).expect("write event variant map");
    rmp::encode::write_str(&mut payload, "prompt_renamed").expect("write variant key");
    rmp::encode::write_map_len(&mut payload, 3).expect("write event fields map");
    rmp::encode::write_str(&mut payload, "revision").expect("write revision key");
    rmp::encode::write_uint(&mut payload, revision).expect("write revision");
    rmp::encode::write_str(&mut payload, "title").expect("write title key");
    rmp::encode::write_str(&mut payload, title).expect("write title");
    rmp::encode::write_str(&mut payload, "prompt_id").expect("write prompt key");
    write_id_bytes(&mut payload, prompt_id.as_bytes());
    rmp::encode::write_str(&mut payload, "schema_version").expect("write schema key");
    rmp::encode::write_uint(&mut payload, 1).expect("write schema version");
    payload
}

fn chain_command_payload(command: &PromptChainCommand) -> Vec<u8> {
    let original_command_payload = command.encode().expect("original command payload");
    let original_command_sha256: [u8; 32] = Sha256::digest(&original_command_payload).into();
    rmp_serde::to_vec_named(&PromptChainCommandWire {
        schema_version: 3,
        original_command_sha256,
        original_command_payload,
        command,
        resolved_prompt_version_id: None,
    })
    .expect("canonical chain command payload")
}

fn stored_chain_command(path: &Path, command_id: CommandId) -> PromptChainCommandWireOwned {
    let conn = Connection::open(path).expect("open command payload database");
    let payload: Vec<u8> = conn
        .query_row(
            "SELECT command_payload FROM prompt_chain_command_receipts WHERE command_id = ?1",
            [command_id.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("load durable chain command payload");
    rmp_serde::from_slice(&payload).expect("decode durable chain command payload")
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
    let message = error.to_string().to_lowercase();
    assert!(message.contains("foreign key") || message.contains("latest"));
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
fn idempotent_receipt_requires_correlated_command_target_version_and_revision() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let command_id = command_id(122);
    let prompt = prompt_id(123);
    let version = version_id(124);
    let command = canonical_create_prompt(prompt, version);
    let mut stored_command = command.clone();
    if let PromptCommand::CreatePrompt(command) = &mut stored_command {
        command.tags = vec!["rust".into(), "review".into()];
    }
    let command_payload = stored_command.encode().expect("canonical command payload");
    let command_sha256: [u8; 32] = Sha256::digest(&command_payload).into();
    let stored_receipt = PromptMutationReceipt {
        command_id,
        prompt_id: prompt_id(125),
        prompt_version_id: version_id(126),
        revision: 99,
    };

    drop(open_store(&path));
    let conn = Connection::open(&path).expect("open isolated raw connection");
    conn.execute_batch("DROP TRIGGER prompt_command_receipts_lineage_insert")
        .expect("disable lineage trigger for corruption fixture");
    conn.execute(
        "INSERT INTO prompt_command_receipts(
            command_id, command_sha256, command_payload, prompt_id,
            prompt_version_id, revision, receipt, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            command_id.as_bytes().as_slice(),
            command_sha256.as_slice(),
            command_payload,
            stored_receipt.prompt_id.as_bytes().as_slice(),
            stored_receipt.prompt_version_id.as_bytes().as_slice(),
            99_i64,
            rmp_serde::to_vec_named(&stored_receipt).expect("receipt payload"),
            1_i64,
        ],
    )
    .expect("insert corrupt receipt");
    drop(conn);

    let error =
        PromptStore::open(&path).expect_err("correlated receipt fields must be validated at open");
    assert!(matches!(error, PromptStoreError::Corruption(_)));
}

#[test]
fn idempotent_receipt_requires_exact_canonical_payload_bytes() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let command_id = command_id(127);
    let prompt = prompt_id(128);
    let version = version_id(129);
    let command = create_prompt(prompt, version);
    let receipt = PromptMutationReceipt {
        command_id,
        prompt_id: prompt,
        prompt_version_id: version,
        revision: 1,
    };
    let mut stored_command = command.clone();
    if let PromptCommand::CreatePrompt(command) = &mut stored_command {
        command.tags = vec!["rust".into(), "review".into()];
    }
    let command_payload = stored_command.encode().expect("canonical command payload");
    let command_sha256: [u8; 32] = Sha256::digest(&command_payload).into();

    drop(open_store(&path));
    let conn = Connection::open(&path).expect("open isolated raw connection");
    conn.execute_batch("DROP TRIGGER prompt_command_receipts_lineage_insert")
        .expect("disable lineage trigger for corruption fixture");
    conn.execute(
        "INSERT INTO prompt_command_receipts(
            command_id, command_sha256, command_payload, prompt_id,
            prompt_version_id, revision, receipt, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            command_id.as_bytes().as_slice(),
            command_sha256.as_slice(),
            command_payload,
            prompt.as_bytes().as_slice(),
            version.as_bytes().as_slice(),
            1_i64,
            noncanonical_receipt_payload(&receipt),
            1_i64,
        ],
    )
    .expect("insert noncanonical receipt");
    drop(conn);

    let error =
        PromptStore::open(&path).expect_err("noncanonical receipt bytes must be rejected at open");
    assert!(matches!(error, PromptStoreError::Corruption(_)));
}

#[test]
fn sqlite_rejects_legacy_receipt_without_command_bytes_atomically() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let command_id = command_id(133);
    let prompt = prompt_id(134);
    let version = version_id(135);
    let receipt = PromptMutationReceipt {
        command_id,
        prompt_id: prompt,
        prompt_version_id: version,
        revision: 1,
    };

    drop(open_store(&path));
    let conn = Connection::open(&path).expect("open isolated raw connection");
    let insert = conn.execute(
        "INSERT INTO prompt_command_receipts(
            command_id, command_sha256, prompt_id, prompt_version_id, revision,
            receipt, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            command_id.as_bytes().as_slice(),
            [0u8; 32].as_slice(),
            prompt.as_bytes().as_slice(),
            version.as_bytes().as_slice(),
            1_i64,
            receipt.encode().expect("canonical receipt payload"),
            1_i64,
        ],
    );
    assert!(
        insert.is_err(),
        "a V9 receipt must not be inserted without command bytes"
    );
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM prompt_command_receipts WHERE command_id = ?1",
            [command_id.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("count prompt receipts");
    assert_eq!(count, 0, "failed legacy receipt insert must be atomic");
}

#[test]
fn idempotent_effectful_receipt_requires_exactly_one_effect_event() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(136);
    let create_version = version_id(137);
    let rename_command_id = command_id(138);
    let rename = PromptCommand::RenamePrompt(RenamePrompt {
        prompt_id: prompt,
        title: "Renamed code".into(),
        expected_revision: 1,
    });

    let mut store = open_store(&path);
    store
        .execute(command_id(139), create_prompt(prompt, create_version))
        .expect("create prompt");
    store
        .execute(rename_command_id, rename.clone())
        .expect("rename prompt");
    drop(store);

    let conn = Connection::open(&path).expect("open isolated raw connection");
    let (occurred_at_ms, payload): (i64, Vec<u8>) = conn
        .query_row(
            "SELECT occurred_at_ms, payload FROM prompt_events WHERE command_id = ?1",
            [rename_command_id.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("load canonical rename effect");
    conn.execute(
        "INSERT INTO prompt_events(
            prompt_event_id, command_id, prompt_id, event_type, occurred_at_ms, payload
         ) VALUES (?1, ?2, ?3, 'prompt.renamed', ?4, ?5)",
        rusqlite::params![
            EventId::new().as_bytes().as_slice(),
            rename_command_id.as_bytes().as_slice(),
            prompt.as_bytes().as_slice(),
            occurred_at_ms,
            payload,
        ],
    )
    .expect("insert duplicate effect row");
    drop(conn);

    let error = PromptStore::open(&path)
        .expect_err("duplicate effect rows must reject idempotent replay at open");
    assert!(matches!(error, PromptStoreError::Corruption(_)));
}

#[test]
fn idempotent_effectful_receipt_requires_one_event_not_zero() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(140);
    let create_version = version_id(141);
    let rename_command_id = command_id(142);
    let rename = PromptCommand::RenamePrompt(RenamePrompt {
        prompt_id: prompt,
        title: "Renamed code".into(),
        expected_revision: 1,
    });

    let mut store = open_store(&path);
    store
        .execute(command_id(143), create_prompt(prompt, create_version))
        .expect("create prompt");
    store
        .execute(rename_command_id, rename.clone())
        .expect("rename prompt");
    drop(store);

    let conn = Connection::open(&path).expect("open isolated raw connection");
    conn.execute_batch("DROP TRIGGER prompt_events_immutable_delete")
        .expect("disable append-only trigger for corruption fixture");
    conn.execute(
        "DELETE FROM prompt_events WHERE command_id = ?1",
        [rename_command_id.as_bytes().as_slice()],
    )
    .expect("remove effect row");
    drop(conn);

    let error = PromptStore::open(&path)
        .expect_err("missing effect row must reject idempotent replay at open");
    assert!(matches!(error, PromptStoreError::Corruption(_)));
}

#[test]
fn idempotent_semantic_noop_requires_unchanged_state_and_zero_events() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(144);
    let create_version = version_id(145);
    let noop_command_id = command_id(146);
    let noop = PromptCommand::RenamePrompt(RenamePrompt {
        prompt_id: prompt,
        title: "Review code".into(),
        expected_revision: 1,
    });

    let mut store = open_store(&path);
    store
        .execute(command_id(147), create_prompt(prompt, create_version))
        .expect("create prompt");
    store
        .execute(noop_command_id, noop.clone())
        .expect("semantic no-op");
    assert_eq!(store.count_prompt_events().expect("count events"), 1);
    drop(store);

    let conn = Connection::open(&path).expect("open isolated raw connection");
    conn.execute_batch("DROP TRIGGER saved_prompts_metadata_update_bounds")
        .expect("disable metadata trigger for corruption fixture");
    conn.execute(
        "UPDATE saved_prompts SET title = 'Forged state' WHERE prompt_id = ?1",
        [prompt.as_bytes().as_slice()],
    )
    .expect("forge changed state after no-op receipt");
    drop(conn);

    let error = PromptStore::open(&path)
        .expect_err("no-op replay must prove unchanged exact state at open");
    assert!(matches!(error, PromptStoreError::Corruption(_)));
}

#[test]
fn idempotent_semantic_noop_rejects_a_forged_event() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(148);
    let create_version = version_id(149);
    let noop_command_id = command_id(150);
    let noop = PromptCommand::RenamePrompt(RenamePrompt {
        prompt_id: prompt,
        title: "Review code".into(),
        expected_revision: 1,
    });

    let mut store = open_store(&path);
    store
        .execute(command_id(151), create_prompt(prompt, create_version))
        .expect("create prompt");
    store
        .execute(noop_command_id, noop.clone())
        .expect("semantic no-op");
    drop(store);

    let forged_event = PromptEvent::PromptRenamed {
        prompt_id: prompt,
        title: "Review code".into(),
        revision: 1,
    };
    let conn = Connection::open(&path).expect("open isolated raw connection");
    conn.execute(
        "INSERT INTO prompt_events(
            prompt_event_id, command_id, prompt_id, event_type, occurred_at_ms, payload
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            EventId::new().as_bytes().as_slice(),
            noop_command_id.as_bytes().as_slice(),
            prompt.as_bytes().as_slice(),
            forged_event.event_type(),
            1_i64,
            forged_event.encode().expect("canonical forged event"),
        ],
    )
    .expect("insert forged no-op event");
    drop(conn);

    let error = PromptStore::open(&path)
        .expect_err("no-op replay must reject a nonzero effect count at open");
    assert!(matches!(error, PromptStoreError::Corruption(_)));
}

#[test]
fn historical_prompt_noop_survives_later_revision_reopen_rebuild_and_retry() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(236);
    let first = version_id(237);
    let second = version_id(238);
    let noop_id = command_id(239);
    let noop = PromptCommand::RenamePrompt(RenamePrompt {
        prompt_id: prompt,
        title: "Review code".into(),
        expected_revision: 1,
    });
    let mut store = open_store(&path);

    store
        .execute(command_id(240), create_prompt(prompt, first))
        .expect("create prompt");
    let expected_receipt = store
        .execute(noop_id, noop.clone())
        .expect("rename to current title is a semantic no-op");
    store
        .execute(
            command_id(241),
            PromptCommand::CreatePromptVersion(CreatePromptVersion {
                prompt_id: prompt,
                prompt_version_id: second,
                variables: Vec::new(),
                body: "new body".into(),
                created_at_ms: 2,
                expected_revision: 1,
            }),
        )
        .expect("create newer prompt version");

    assert_eq!(
        store
            .execute(noop_id, noop.clone())
            .expect("historical prompt no-op retry"),
        expected_receipt
    );

    drop(store);
    let mut reopened = open_store(&path);
    reopened
        .rebuild_projection()
        .expect("rebuild accepts historical prompt no-op receipt");
    assert_eq!(
        reopened
            .execute(noop_id, noop)
            .expect("historical prompt no-op retry after reopen"),
        expected_receipt
    );
    assert_eq!(
        reopened
            .get_prompt(prompt)
            .expect("load prompt")
            .expect("prompt")
            .current_version_id,
        second
    );
}

#[test]
fn idempotent_chain_receipt_requires_correlated_chain_target() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let command_id = command_id(130);
    let chain = chain_id(131);
    let command = PromptChainCommand::CreatePromptChain(CreatePromptChain {
        chain_id: chain,
        title: "Receipt chain".into(),
        description: None,
        created_at_ms: 1,
    });
    let receipt = devmanager::prompts::PromptChainMutationReceipt {
        command_id,
        chain_id: chain_id(132),
        link_id: None,
        revision: 1,
    };
    let command_payload = chain_command_payload(&command);
    let command_sha256: [u8; 32] = Sha256::digest(&command_payload).into();

    drop(open_store(&path));
    let conn = Connection::open(&path).expect("open isolated raw connection");
    conn.execute_batch("DROP TRIGGER prompt_chain_command_receipts_lineage_insert")
        .expect("disable lineage trigger for corruption fixture");
    conn.execute(
        "INSERT INTO prompt_chain_command_receipts(
            command_id, command_sha256, command_payload, chain_id,
            chain_link_id, revision, receipt, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            command_id.as_bytes().as_slice(),
            command_sha256.as_slice(),
            command_payload,
            receipt.chain_id.as_bytes().as_slice(),
            Option::<Vec<u8>>::None,
            1_i64,
            rmp_serde::to_vec_named(&receipt).expect("chain receipt payload"),
            1_i64,
        ],
    )
    .expect("insert corrupt chain receipt");
    drop(conn);

    let error =
        PromptStore::open(&path).expect_err("chain receipt target must be validated at open");
    assert!(matches!(error, PromptStoreError::Corruption(_)));
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
        PromptStoreError::Validation(PromptValidationError::ExpectedRevisionZero)
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
fn archived_prompt_metadata_commands_and_replay_are_rejected() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(115);
    let version = version_id(116);
    let mut store = open_store(&path);
    store
        .execute(command_id(117), create_prompt(prompt, version))
        .expect("create prompt");
    store
        .execute(
            command_id(118),
            PromptCommand::ArchivePrompt(ArchivePrompt {
                prompt_id: prompt,
                archived_at_ms: 3,
                expected_revision: 1,
            }),
        )
        .expect("archive prompt");

    let rename_error = store
        .execute(
            command_id(119),
            PromptCommand::RenamePrompt(RenamePrompt {
                prompt_id: prompt,
                title: "Archived rename".into(),
                expected_revision: 2,
            }),
        )
        .expect_err("archived prompt rename must reject");
    assert!(matches!(rename_error, PromptStoreError::InvalidTransition));
    let tags_error = store
        .execute(
            command_id(120),
            PromptCommand::SetPromptTags(SetPromptTags {
                prompt_id: prompt,
                tags: vec!["archived".into()],
                expected_revision: 2,
            }),
        )
        .expect_err("archived prompt tag edit must reject");
    assert!(matches!(tags_error, PromptStoreError::InvalidTransition));
    assert_eq!(store.count_prompt_events().unwrap(), 2);
    drop(store);

    let replay_command = command_id(121);
    let replay_event = PromptEvent::PromptTagsSet {
        prompt_id: prompt,
        tags: vec!["replayed".into()],
        revision: 3,
    };
    let conn = Connection::open(&path).expect("open isolated raw connection");
    conn.execute_batch("PRAGMA foreign_keys = ON;")
        .expect("foreign keys");
    let replay_receipt = PromptMutationReceipt {
        command_id: replay_command,
        prompt_id: prompt,
        prompt_version_id: version,
        revision: 3,
    };
    let replay_command_payload = canonical_create_prompt(prompt, version)
        .encode()
        .expect("canonical replay command payload");
    let replay_command_sha256: [u8; 32] = Sha256::digest(&replay_command_payload).into();
    conn.execute(
        "INSERT INTO prompt_command_receipts(
            command_id, command_sha256, command_payload, prompt_id, prompt_version_id, revision,
            receipt, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            replay_command.as_bytes().as_slice(),
            replay_command_sha256.as_slice(),
            replay_command_payload,
            prompt.as_bytes().as_slice(),
            version.as_bytes().as_slice(),
            3_i64,
            rmp_serde::to_vec_named(&replay_receipt).expect("receipt payload"),
            4_i64,
        ],
    )
    .expect("insert replay receipt");
    conn.execute(
        "INSERT INTO prompt_events(
            prompt_event_id, command_id, prompt_id, event_type, occurred_at_ms, payload
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            EventId::new().as_bytes().as_slice(),
            replay_command.as_bytes().as_slice(),
            prompt.as_bytes().as_slice(),
            replay_event.event_type(),
            4_i64,
            replay_event.encode().expect("event payload"),
        ],
    )
    .expect("insert replay event");
    drop(conn);

    let replay_error = PromptStore::open(&path)
        .expect_err("open must reject archived metadata mutation before replay");
    assert!(matches!(replay_error, PromptStoreError::Corruption(_)));
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
        "UPDATE prompt_tags SET tag = 'É' WHERE prompt_id = ?1 AND tag = 'rust'",
        [prompt.as_bytes().as_slice()],
    )
    .expect_err("SQLite must reject a tag outside the shared ASCII grammar");
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
    let error = PromptStore::open(&path).expect_err("sparse tag positions must fail at open");
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
    let event_type = "prompt.created";
    let event_payload = rmp_serde::to_vec_named(&RawPromptEventWire {
        schema_version: 1,
        event: RawPromptEvent::PromptCreated {
            prompt: RawSavedPrompt {
                id: prompt,
                title: "  noncanonical title  ".into(),
                description: None,
                tags: Vec::new(),
                current_version_id: version_id,
                revision: 1,
                archived_at_ms: None,
            },
            version,
        },
    })
    .expect("noncanonical event payload");

    drop(open_store(&path));
    let conn = Connection::open(&path).expect("open isolated raw connection");
    conn.execute_batch("DROP TRIGGER prompt_command_receipts_lineage_insert")
        .expect("disable lineage trigger for corruption fixture");
    conn.execute_batch("PRAGMA foreign_keys = ON;")
        .expect("foreign keys");
    let receipt = PromptMutationReceipt {
        command_id: command,
        prompt_id: prompt,
        prompt_version_id: version_id,
        revision: 1,
    };
    let command_payload = canonical_create_prompt(prompt, version_id)
        .encode()
        .expect("canonical command payload");
    let command_sha256: [u8; 32] = Sha256::digest(&command_payload).into();
    conn.execute(
        "INSERT INTO prompt_command_receipts(
            command_id, command_sha256, command_payload, prompt_id, prompt_version_id, revision,
            receipt, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            command.as_bytes().as_slice(),
            command_sha256.as_slice(),
            command_payload,
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
            event_type,
            1_i64,
            event_payload,
        ],
    )
    .expect("insert event");
    drop(conn);

    let error = PromptStore::open(&path).expect_err("noncanonical event must fail closed at open");
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
    let event_type = "prompt.renamed";
    let event_payload = rmp_serde::to_vec_named(&RawPromptEventWire {
        schema_version: 1,
        event: RawPromptEvent::PromptRenamed {
            prompt_id: prompt,
            title: "  noncanonical title  ".into(),
            revision: 2,
        },
    })
    .expect("noncanonical mutation event payload");
    let conn = Connection::open(&path).expect("open isolated raw connection");
    conn.execute_batch("PRAGMA foreign_keys = ON;")
        .expect("foreign keys");
    let receipt = PromptMutationReceipt {
        command_id: command,
        prompt_id: prompt,
        prompt_version_id: version_id,
        revision: 2,
    };
    let command_payload = canonical_create_prompt(prompt, version_id)
        .encode()
        .expect("canonical command payload");
    let command_sha256: [u8; 32] = Sha256::digest(&command_payload).into();
    conn.execute(
        "INSERT INTO prompt_command_receipts(
            command_id, command_sha256, command_payload, prompt_id, prompt_version_id, revision,
            receipt, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            command.as_bytes().as_slice(),
            command_sha256.as_slice(),
            command_payload,
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
            event_type,
            2_i64,
            event_payload,
        ],
    )
    .expect("insert event");
    drop(conn);

    let error =
        PromptStore::open(&path).expect_err("noncanonical mutation event must fail closed at open");
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
fn identical_body_edit_creates_a_new_immutable_version() {
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
        .expect("identical body still creates a version");

    assert_eq!(receipt.prompt_version_id, version_id(114));
    assert_eq!(receipt.revision, 2);
    assert_eq!(store.list_versions(prompt, 0, 10).unwrap().len(), 2);
    assert_eq!(store.count_prompt_events().unwrap(), 2);
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
        links.iter().map(|link| link.position()).collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert_eq!(links[0].prompt_version_id(), first_version);
    assert_eq!(links[1].prompt_version_id(), version_id(148));

    let first_context = store
        .get_chain_link_context(chain, links[0].id())
        .expect("first context")
        .expect("first link");
    assert_eq!(first_context.previous_link_id, None);
    assert_eq!(first_context.next_link_id, Some(links[1].id()));
    assert!(first_context.update_available);
    let last_context = store
        .get_chain_link_context(chain, links[1].id())
        .expect("last context")
        .expect("last link");
    assert_eq!(last_context.previous_link_id, Some(links[0].id()));
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
            .map(|link| link.id())
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
            .map(|link| link.id())
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
        links.iter().map(|link| link.position()).collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert_eq!(
        links.iter().map(|link| link.id()).collect::<Vec<_>>(),
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
        PromptStoreError::Validation(PromptValidationError::ExpectedRevisionZero)
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
        store.list_chain_links(chain).unwrap()[0].prompt_version_id(),
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
        store.list_chain_links(chain).unwrap()[0].prompt_version_id(),
        second
    );
    store
        .rebuild_projection()
        .expect("rebuild all prompt projections");
    assert_eq!(
        store.list_chain_links(chain).unwrap()[0].prompt_version_id(),
        second
    );
}

#[test]
fn rebuild_chain_insert_uses_the_pinned_version_after_a_later_version() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(191);
    let first = version_id(192);
    let second = version_id(193);
    let chain = chain_id(194);
    let link = link_id(195);
    let mut store = open_store(&path);
    store
        .execute(command_id(196), create_prompt(prompt, first))
        .expect("create prompt");
    store
        .execute_chain(command_id(197), create_chain(chain))
        .expect("create chain");
    store
        .execute_chain(
            command_id(198),
            insert_link(chain, link, prompt, None, None, 1),
        )
        .expect("insert link at the current version");
    let stored_insert = stored_chain_command(&path, command_id(198));
    assert_eq!(stored_insert.schema_version, 3);
    let original_insert_payload = insert_link(chain, link, prompt, None, None, 1)
        .encode()
        .expect("canonical original insert payload");
    let original_insert_hash: [u8; 32] = Sha256::digest(&original_insert_payload).into();
    assert_eq!(
        stored_insert.original_command_payload,
        original_insert_payload
    );
    assert_eq!(stored_insert.original_command_sha256, original_insert_hash);
    assert_eq!(stored_insert.resolved_prompt_version_id, Some(first));
    match stored_insert.command {
        PromptChainCommand::InsertPromptChainLink(command) => {
            assert_eq!(command.prompt_version_id, Some(first));
        }
        other => panic!("unexpected durable insert command: {other:?}"),
    }
    store
        .execute(
            command_id(199),
            PromptCommand::CreatePromptVersion(CreatePromptVersion {
                prompt_id: prompt,
                prompt_version_id: second,
                variables: Vec::new(),
                body: "later body".into(),
                created_at_ms: 2,
                expected_revision: 1,
            }),
        )
        .expect("create later version");

    let retry = store
        .execute_chain(
            command_id(198),
            insert_link(chain, link, prompt, None, None, 1),
        )
        .expect("retry must use the originally resolved insert version");
    assert_eq!(retry.revision, 2);

    store
        .rebuild_projection()
        .expect("rebuild must use the durable link effect");
    assert_eq!(
        store.list_chain_links(chain).unwrap()[0].prompt_version_id(),
        first
    );
}

#[test]
fn rebuild_chain_update_uses_the_version_pinned_by_the_effect() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(196);
    let first = version_id(197);
    let second = version_id(198);
    let third = version_id(199);
    let chain = chain_id(200);
    let link = link_id(201);
    let mut store = open_store(&path);
    store
        .execute(command_id(202), create_prompt(prompt, first))
        .expect("create prompt");
    store
        .execute_chain(command_id(203), create_chain(chain))
        .expect("create chain");
    store
        .execute_chain(
            command_id(204),
            insert_link(chain, link, prompt, Some(first), None, 1),
        )
        .expect("insert first version");
    store
        .execute(
            command_id(205),
            PromptCommand::CreatePromptVersion(CreatePromptVersion {
                prompt_id: prompt,
                prompt_version_id: second,
                variables: Vec::new(),
                body: "second body".into(),
                created_at_ms: 2,
                expected_revision: 1,
            }),
        )
        .expect("create second version");
    store
        .execute_chain(
            command_id(206),
            PromptChainCommand::UpdatePromptChainLinkVersion(UpdatePromptChainLinkVersion {
                chain_id: chain,
                link_id: link,
                expected_revision: 2,
            }),
        )
        .expect("pin second version");
    let stored_update = stored_chain_command(&path, command_id(206));
    assert_eq!(stored_update.schema_version, 3);
    assert_eq!(stored_update.resolved_prompt_version_id, Some(second));
    store
        .execute(
            command_id(207),
            PromptCommand::CreatePromptVersion(CreatePromptVersion {
                prompt_id: prompt,
                prompt_version_id: third,
                variables: Vec::new(),
                body: "third body".into(),
                created_at_ms: 3,
                expected_revision: 2,
            }),
        )
        .expect("create third version");

    store
        .execute_chain(
            command_id(206),
            PromptChainCommand::UpdatePromptChainLinkVersion(UpdatePromptChainLinkVersion {
                chain_id: chain,
                link_id: link,
                expected_revision: 2,
            }),
        )
        .expect("retry must use the originally resolved update version");

    store
        .rebuild_projection()
        .expect("rebuild must use the durable update effect");
    assert_eq!(
        store.list_chain_links(chain).unwrap()[0].prompt_version_id(),
        second
    );
}

#[test]
fn idempotent_chain_noop_replay_proves_immediate_successor_and_end_state() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(202);
    let first = version_id(203);
    let second = version_id(204);
    let chain = chain_id(205);
    let first_link = link_id(206);
    let second_link = link_id(207);
    let mut store = open_store(&path);
    store
        .execute(command_id(208), create_prompt(prompt, first))
        .expect("create prompt");
    store
        .execute_chain(command_id(209), create_chain(chain))
        .expect("create chain");
    store
        .execute_chain(
            command_id(210),
            insert_link(chain, first_link, prompt, Some(first), None, 1),
        )
        .expect("insert first link");
    store
        .execute_chain(
            command_id(211),
            insert_link(chain, second_link, prompt, Some(first), None, 2),
        )
        .expect("insert second link");
    store
        .execute(
            command_id(212),
            PromptCommand::CreatePromptVersion(CreatePromptVersion {
                prompt_id: prompt,
                prompt_version_id: second,
                variables: Vec::new(),
                body: "second body".into(),
                created_at_ms: 2,
                expected_revision: 1,
            }),
        )
        .expect("create second version");

    let immediate_successor = PromptChainCommand::MovePromptChainLink(MovePromptChainLink {
        chain_id: chain,
        link_id: first_link,
        before_link_id: Some(second_link),
        expected_revision: 3,
    });
    store
        .execute_chain(command_id(213), immediate_successor.clone())
        .expect("immediate-successor move is a semantic no-op");

    let conn = Connection::open(&path).expect("open isolated raw connection");
    conn.execute(
        "UPDATE prompt_chain_links SET prompt_version_id = ?1 WHERE link_id = ?2",
        rusqlite::params![
            second.as_bytes().as_slice(),
            first_link.as_bytes().as_slice()
        ],
    )
    .expect("forge changed immediate-successor no-op state");
    drop(conn);

    let error = store
        .execute_chain(command_id(213), immediate_successor)
        .expect_err("immediate-successor no-op must prove unchanged state");
    assert!(matches!(error, PromptStoreError::Corruption(_)));

    let conn = Connection::open(&path).expect("restore isolated raw connection");
    conn.execute(
        "UPDATE prompt_chain_links SET prompt_version_id = ?1 WHERE link_id = ?2",
        rusqlite::params![
            first.as_bytes().as_slice(),
            first_link.as_bytes().as_slice()
        ],
    )
    .expect("restore immediate-successor no-op state");
    drop(conn);

    let end_move = PromptChainCommand::MovePromptChainLink(MovePromptChainLink {
        chain_id: chain,
        link_id: second_link,
        before_link_id: None,
        expected_revision: 3,
    });
    store
        .execute_chain(command_id(214), end_move.clone())
        .expect("end move is a semantic no-op");

    let conn = Connection::open(&path).expect("reopen isolated raw connection");
    conn.execute(
        "UPDATE prompt_chain_links SET prompt_version_id = ?1 WHERE link_id = ?2",
        rusqlite::params![
            second.as_bytes().as_slice(),
            second_link.as_bytes().as_slice()
        ],
    )
    .expect("forge changed end no-op state");
    drop(conn);

    let error = store
        .execute_chain(command_id(214), end_move)
        .expect_err("end no-op must prove unchanged state");
    assert!(matches!(error, PromptStoreError::Corruption(_)));
}

#[test]
fn historical_chain_noop_survives_later_revision_reopen_rebuild_and_retry() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(215);
    let version = version_id(216);
    let chain = chain_id(217);
    let first_link = link_id(218);
    let second_link = link_id(219);
    let noop_id = command_id(220);
    let mut store = open_store(&path);

    store
        .execute(command_id(221), create_prompt(prompt, version))
        .expect("create prompt");
    store
        .execute_chain(command_id(222), create_chain(chain))
        .expect("create chain");
    store
        .execute_chain(
            command_id(223),
            insert_link(chain, first_link, prompt, Some(version), None, 1),
        )
        .expect("insert first link");
    store
        .execute_chain(
            command_id(224),
            insert_link(chain, second_link, prompt, Some(version), None, 2),
        )
        .expect("insert second link");

    let noop = PromptChainCommand::MovePromptChainLink(MovePromptChainLink {
        chain_id: chain,
        link_id: first_link,
        before_link_id: Some(second_link),
        expected_revision: 3,
    });
    let expected_receipt = store
        .execute_chain(noop_id, noop.clone())
        .expect("immediate-successor move is a semantic no-op");

    store
        .execute_chain(
            command_id(225),
            PromptChainCommand::MovePromptChainLink(MovePromptChainLink {
                chain_id: chain,
                link_id: second_link,
                before_link_id: Some(first_link),
                expected_revision: 3,
            }),
        )
        .expect("later move creates a new chain revision");

    assert_eq!(
        store
            .execute_chain(noop_id, noop.clone())
            .expect("historical no-op retry"),
        expected_receipt
    );

    drop(store);
    let mut reopened = open_store(&path);
    let mut second_connection = open_store(&path);
    reopened
        .rebuild_projection()
        .expect("rebuild accepts historical no-op receipt");
    assert_eq!(
        second_connection
            .execute_chain(noop_id, noop.clone())
            .expect("historical no-op retry from a second connection"),
        expected_receipt
    );
    assert_eq!(
        reopened
            .execute_chain(noop_id, noop)
            .expect("historical no-op retry after reopen"),
        expected_receipt
    );
    assert_eq!(
        reopened
            .list_chain_links(chain)
            .expect("list final chain")
            .iter()
            .map(|link| link.id())
            .collect::<Vec<_>>(),
        vec![second_link, first_link]
    );
}

#[test]
fn historical_chain_update_noop_survives_new_prompt_version_reopen_rebuild_and_retry() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(226);
    let first = version_id(227);
    let second = version_id(228);
    let chain = chain_id(229);
    let link = link_id(230);
    let noop_id = command_id(231);
    let mut store = open_store(&path);

    store
        .execute(command_id(232), create_prompt(prompt, first))
        .expect("create prompt");
    store
        .execute_chain(command_id(233), create_chain(chain))
        .expect("create chain");
    store
        .execute_chain(
            command_id(234),
            insert_link(chain, link, prompt, Some(first), None, 1),
        )
        .expect("insert pinned link");

    let noop = PromptChainCommand::UpdatePromptChainLinkVersion(UpdatePromptChainLinkVersion {
        chain_id: chain,
        link_id: link,
        expected_revision: 2,
    });
    let expected_receipt = store
        .execute_chain(noop_id, noop.clone())
        .expect("update to current version is a semantic no-op");

    store
        .execute(
            command_id(235),
            PromptCommand::CreatePromptVersion(CreatePromptVersion {
                prompt_id: prompt,
                prompt_version_id: second,
                variables: Vec::new(),
                body: "new body".into(),
                created_at_ms: 2,
                expected_revision: 1,
            }),
        )
        .expect("create newer prompt version");

    assert_eq!(
        store
            .execute_chain(noop_id, noop.clone())
            .expect("historical update no-op retry"),
        expected_receipt
    );

    drop(store);
    let mut reopened = open_store(&path);
    reopened
        .rebuild_projection()
        .expect("rebuild accepts historical update no-op receipt");
    assert_eq!(
        reopened
            .execute_chain(noop_id, noop)
            .expect("historical update no-op retry after reopen"),
        expected_receipt
    );
    assert_eq!(
        reopened
            .list_chain_links(chain)
            .expect("list pinned chain")
            .first()
            .expect("pinned link")
            .prompt_version_id(),
        first
    );
}

#[test]
fn rebuild_repairs_missing_chain_projection_through_ordered_events() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let chain = chain_id(201);
    let mut store = open_store(&path);
    store
        .execute_chain(command_id(202), create_chain(chain))
        .expect("create chain");
    store
        .execute_chain(
            command_id(203),
            PromptChainCommand::RenamePromptChain(devmanager::prompts::RenamePromptChain {
                chain_id: chain,
                title: "Replayed workflow".into(),
                expected_revision: 1,
            }),
        )
        .expect("rename chain");
    let conn = Connection::open(&path).expect("open isolated raw connection");
    conn.execute(
        "DELETE FROM prompt_chains WHERE chain_id = ?1",
        [chain.as_bytes().as_slice()],
    )
    .expect("remove only the chain projection");
    drop(conn);

    store
        .rebuild_projection()
        .expect("valid ordered chain events rebuild the missing projection");
    let rebuilt = store
        .get_chain(chain)
        .expect("query rebuilt chain")
        .expect("rebuilt chain");
    assert_eq!(rebuilt.title, "Replayed workflow");
    assert_eq!(rebuilt.revision, 2);
}

#[test]
fn chain_link_create_rejects_stale_prompt_current_pointer() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(204);
    let first = version_id(205);
    let mut store = open_store(&path);
    store
        .execute(command_id(206), create_prompt(prompt, first))
        .expect("create prompt");
    drop(store);

    let body = "later version";
    let body_sha256: [u8; 32] = Sha256::digest(body.as_bytes()).into();
    let conn = Connection::open(&path).expect("open isolated raw connection");
    conn.execute_batch("DROP TRIGGER IF EXISTS prompt_versions_advance_current_after_insert")
        .expect("disable current-version trigger for corruption fixture");
    conn.execute(
        "INSERT INTO prompt_versions(
            prompt_version_id, prompt_id, version, body, body_sha256, created_at_ms
         ) VALUES (?1, ?2, 2, ?3, ?4, 2)",
        rusqlite::params![
            version_id(207).as_bytes().as_slice(),
            prompt.as_bytes().as_slice(),
            body,
            body_sha256.as_slice(),
        ],
    )
    .expect("insert later version without advancing pointer");
    drop(conn);

    let error = PromptStore::open(&path)
        .expect_err("a stale prompt pointer must fail before chain operations");
    assert!(matches!(error, PromptStoreError::Corruption(_)));
}

#[test]
fn chain_link_update_rejects_stale_prompt_current_pointer() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(212);
    let first = version_id(213);
    let mut store = open_store(&path);
    store
        .execute(command_id(214), create_prompt(prompt, first))
        .expect("create prompt");
    let chain = chain_id(215);
    store
        .execute_chain(command_id(216), create_chain(chain))
        .expect("create chain");
    store
        .execute_chain(
            command_id(217),
            insert_link(chain, link_id(218), prompt, Some(first), None, 1),
        )
        .expect("create pinned link");
    drop(store);

    let body = "later version";
    let body_sha256: [u8; 32] = Sha256::digest(body.as_bytes()).into();
    let conn = Connection::open(&path).expect("open isolated raw connection");
    conn.execute_batch("DROP TRIGGER IF EXISTS prompt_versions_advance_current_after_insert")
        .expect("disable current-version trigger for corruption fixture");
    conn.execute(
        "INSERT INTO prompt_versions(
            prompt_version_id, prompt_id, version, body, body_sha256, created_at_ms
         ) VALUES (?1, ?2, 2, ?3, ?4, 2)",
        rusqlite::params![
            version_id(219).as_bytes().as_slice(),
            prompt.as_bytes().as_slice(),
            body,
            body_sha256.as_slice(),
        ],
    )
    .expect("insert later version without advancing pointer");
    drop(conn);

    let error = PromptStore::open(&path)
        .expect_err("a stale prompt pointer must fail before chain updates");
    assert!(matches!(error, PromptStoreError::Corruption(_)));
}

#[test]
fn replay_rejects_untrimmed_chain_title_and_description() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let chain = chain_id(221);
    let command = command_id(222);
    let event_type = "prompt_chain.created";
    let event_payload = rmp_serde::to_vec_named(&RawPromptChainEventWire {
        schema_version: 1,
        event: RawPromptChainEvent::PromptChainCreated {
            chain: RawPromptChain {
                id: chain,
                title: " Chain ".into(),
                description: Some(" description ".into()),
                revision: 1,
                archived_at_ms: None,
            },
        },
    })
    .expect("untrimmed chain event payload");
    let receipt = PromptChainMutationReceipt {
        command_id: command,
        chain_id: chain,
        link_id: None,
        revision: 1,
    };
    let command_payload =
        chain_command_payload(&PromptChainCommand::CreatePromptChain(CreatePromptChain {
            chain_id: chain,
            title: "Chain".into(),
            description: Some("description".into()),
            created_at_ms: 1,
        }));
    let command_sha256: [u8; 32] = Sha256::digest(&command_payload).into();

    drop(open_store(&path));
    let conn = Connection::open(&path).expect("open isolated raw connection");
    conn.execute(
        "INSERT INTO prompt_chains(
            chain_id, title, description, revision, created_at_ms, updated_at_ms, archived_at_ms
         ) VALUES (?1, ?2, ?3, 1, 1, 1, NULL)",
        rusqlite::params![chain.as_bytes().as_slice(), "Chain", "description"],
    )
    .expect("seed projection for row validation");
    conn.execute(
        "INSERT INTO prompt_chain_command_receipts(
            command_id, command_sha256, command_payload, chain_id, chain_link_id, revision,
            receipt, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            command.as_bytes().as_slice(),
            command_sha256.as_slice(),
            command_payload,
            chain.as_bytes().as_slice(),
            Option::<Vec<u8>>::None,
            1_i64,
            receipt.encode().expect("chain receipt payload"),
            1_i64,
        ],
    )
    .expect("seed chain receipt");
    conn.execute(
        "INSERT INTO prompt_chain_events(
            prompt_chain_event_id, command_id, chain_id, event_type,
            occurred_at_ms, payload
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            EventId::new().as_bytes().as_slice(),
            command.as_bytes().as_slice(),
            chain.as_bytes().as_slice(),
            event_type,
            1_i64,
            event_payload,
        ],
    )
    .expect("seed chain event");
    drop(conn);

    let error = PromptStore::open(&path)
        .expect_err("open must reject untrimmed chain metadata before replay");
    assert!(
        matches!(error, PromptStoreError::Corruption(_)),
        "unexpected replay error: {error:?}"
    );
}

#[test]
fn chain_receipts_and_events_are_append_only() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let command = PromptChainCommand::CreatePromptChain(CreatePromptChain {
        chain_id: chain_id(133),
        title: "Append-only chain".into(),
        description: None,
        created_at_ms: 1,
    });
    let command_id = command_id(134);
    let mut store = open_store(&path);
    store
        .execute_chain(command_id, command)
        .expect("create chain");
    drop(store);

    let conn = Connection::open(&path).expect("open isolated raw connection");
    let update_receipt = conn.execute(
        "UPDATE prompt_chain_command_receipts SET revision = 2 WHERE command_id = ?1",
        [command_id.as_bytes().as_slice()],
    );
    assert!(
        update_receipt.is_err(),
        "chain receipt updates must be rejected"
    );
    let delete_receipt = conn.execute(
        "DELETE FROM prompt_chain_command_receipts WHERE command_id = ?1",
        [command_id.as_bytes().as_slice()],
    );
    assert!(
        delete_receipt.is_err(),
        "chain receipt deletes must be rejected"
    );
    let update_event = conn.execute(
        "UPDATE prompt_chain_events SET event_type = 'tampered' WHERE sequence = 1",
        [],
    );
    assert!(
        update_event.is_err(),
        "chain event updates must be rejected"
    );
    let delete_event = conn.execute("DELETE FROM prompt_chain_events WHERE sequence = 1", []);
    assert!(
        delete_event.is_err(),
        "chain event deletes must be rejected"
    );
}

#[test]
fn database_rejects_rollback_to_an_older_prompt_version() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(135);
    let first = version_id(136);
    let second = version_id(137);
    let mut store = open_store(&path);
    store
        .execute(command_id(138), create_prompt(prompt, first))
        .expect("create prompt");
    store
        .execute(
            command_id(139),
            PromptCommand::CreatePromptVersion(CreatePromptVersion {
                prompt_id: prompt,
                prompt_version_id: second,
                variables: Vec::new(),
                body: "new body".into(),
                created_at_ms: 2,
                expected_revision: 1,
            }),
        )
        .expect("create second version");
    drop(store);

    let conn = Connection::open(&path).expect("open isolated raw connection");
    conn.execute_batch("PRAGMA foreign_keys = ON;")
        .expect("foreign keys");
    let error = conn
        .execute(
            "UPDATE saved_prompts SET current_version_id = ?1 WHERE prompt_id = ?2",
            rusqlite::params![first.as_bytes().as_slice(), prompt.as_bytes().as_slice()],
        )
        .expect_err("database must reject current-version rollback");
    assert!(error.to_string().to_lowercase().contains("latest"));
}

#[test]
fn direct_sql_version_insert_atomically_advances_current_pointer() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(138);
    let first = version_id(139);
    let mut store = open_store(&path);
    store
        .execute(command_id(140), create_prompt(prompt, first))
        .expect("create prompt");
    drop(store);

    let body = "direct later version";
    let body_sha256: [u8; 32] = Sha256::digest(body.as_bytes()).into();
    let conn = Connection::open(&path).expect("open isolated raw connection");
    let transaction = conn
        .unchecked_transaction()
        .expect("start direct version transaction");
    transaction
        .execute(
            "INSERT INTO prompt_versions(
            prompt_version_id, prompt_id, version, body, body_sha256, created_at_ms
         ) VALUES (?1, ?2, 2, ?3, ?4, 2)",
            rusqlite::params![
                version_id(141).as_bytes().as_slice(),
                prompt.as_bytes().as_slice(),
                body,
                body_sha256.as_slice(),
            ],
        )
        .expect("insert later version");
    transaction
        .commit()
        .expect("commit direct version transaction");
    let current_version_id: Vec<u8> = conn
        .query_row(
            "SELECT current_version_id FROM saved_prompts WHERE prompt_id = ?1",
            [prompt.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("load current pointer after direct version insert");
    assert_eq!(current_version_id, version_id(141).as_bytes().to_vec());
    drop(conn);

    let conn = Connection::open(&path).expect("reopen database after direct version insert");
    let revision: i64 = conn
        .query_row(
            "SELECT revision FROM saved_prompts WHERE prompt_id = ?1",
            [prompt.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("load prompt revision after direct version insert");
    assert_eq!(revision, 1);
}

#[test]
fn replay_rejects_chain_links_not_derived_from_exact_command() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(160);
    let first_version = version_id(161);
    let second_version = version_id(162);
    let chain = chain_id(163);
    let link = link_id(164);
    let insert_command_id = command_id(165);
    let mut store = open_store(&path);

    store
        .execute(command_id(166), create_prompt(prompt, first_version))
        .expect("create prompt");
    store
        .execute(
            command_id(167),
            PromptCommand::CreatePromptVersion(CreatePromptVersion {
                prompt_id: prompt,
                prompt_version_id: second_version,
                variables: Vec::new(),
                body: "second body".into(),
                created_at_ms: 2,
                expected_revision: 1,
            }),
        )
        .expect("create second prompt version");
    store
        .execute_chain(command_id(168), create_chain(chain))
        .expect("create chain");
    store
        .execute_chain(
            insert_command_id,
            insert_link(chain, link, prompt, Some(first_version), None, 1),
        )
        .expect("insert chain link pinned to first version");
    drop(store);

    let forged_event_payload = rmp_serde::to_vec_named(&RawPromptChainEventWire {
        schema_version: 1,
        event: RawPromptChainEvent::PromptChainLinksReplaced {
            chain_id: chain,
            links: vec![RawPromptChainLink {
                id: link,
                chain_id: chain,
                position: 0,
                prompt_id: prompt,
                prompt_version_id: second_version,
            }],
            revision: 2,
        },
    })
    .expect("forge event payload");
    let conn = Connection::open(&path).expect("open isolated raw connection");
    conn.execute_batch("DROP TRIGGER prompt_chain_events_immutable_update")
        .expect("disable append-only trigger for corruption fixture");
    conn.execute(
        "UPDATE prompt_chain_events SET payload = ?1
         WHERE command_id = ?2 AND event_type = 'prompt_chain.links_replaced'",
        rusqlite::params![
            forged_event_payload,
            insert_command_id.as_bytes().as_slice(),
        ],
    )
    .expect("forge chain event content");
    drop(conn);

    let error = PromptStore::open(&path)
        .expect_err("chain lineage must prove exact command link content at open");
    assert!(matches!(error, PromptStoreError::Corruption(_)));
}

#[test]
fn schema_rejects_prompt_metadata_bounds_and_collection_overflow() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(169);
    let version = version_id(170);
    let mut store = open_store(&path);
    store
        .execute(command_id(171), create_prompt(prompt, version))
        .expect("create prompt");
    drop(store);

    let conn = Connection::open(&path).expect("open isolated raw connection");
    let title_error = conn
        .execute(
            "UPDATE saved_prompts SET title = ?1 WHERE prompt_id = ?2",
            rusqlite::params![" untrimmed ", prompt.as_bytes().as_slice()],
        )
        .expect_err("SQLite must reject untrimmed prompt titles");
    assert!(title_error.to_string().contains("title"));
    let description_error = conn
        .execute(
            "UPDATE saved_prompts SET description = ?1 WHERE prompt_id = ?2",
            rusqlite::params!["d".repeat(2_001), prompt.as_bytes().as_slice()],
        )
        .expect_err("SQLite must reject overlong prompt descriptions");
    assert!(description_error.to_string().contains("description"));

    for position in 0..30_i64 {
        conn.execute(
            "INSERT INTO prompt_tags(prompt_id, tag, position) VALUES (?1, ?2, ?3)",
            rusqlite::params![
                prompt.as_bytes().as_slice(),
                format!("tag-{position}"),
                position + 2,
            ],
        )
        .expect("fill prompt tags to the SQL maximum");
    }
    let tag_overflow = conn
        .execute(
            "INSERT INTO prompt_tags(prompt_id, tag, position) VALUES (?1, 'tag-overflow', 32)",
            [prompt.as_bytes().as_slice()],
        )
        .expect_err("SQLite must reject more than 32 prompt tags");
    assert!(tag_overflow.to_string().contains("tag"));

    let overflow_version = version_id(172);
    let body = "variable fixture";
    let body_sha256: [u8; 32] = Sha256::digest(body.as_bytes()).into();
    conn.execute(
        "INSERT INTO prompt_versions(
            prompt_version_id, prompt_id, version, body, body_sha256, created_at_ms,
            variables_sealed
         ) VALUES (?1, ?2, 2, ?3, ?4, 2, 0)",
        rusqlite::params![
            overflow_version.as_bytes().as_slice(),
            prompt.as_bytes().as_slice(),
            body,
            body_sha256.as_slice(),
        ],
    )
    .expect("insert unsealed variable fixture");
    for position in 0..32_i64 {
        conn.execute(
            "INSERT INTO prompt_version_variables(prompt_version_id, variable, position)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![
                overflow_version.as_bytes().as_slice(),
                format!("variable-{position}"),
                position,
            ],
        )
        .expect("fill variables to the SQL maximum");
    }
    let variable_overflow = conn
        .execute(
            "INSERT INTO prompt_version_variables(prompt_version_id, variable, position)
             VALUES (?1, 'variable-overflow', 32)",
            [overflow_version.as_bytes().as_slice()],
        )
        .expect_err("SQLite must reject more than 32 version variables");
    assert!(variable_overflow.to_string().contains("variable"));
}

#[test]
fn prompt_chain_links_have_a_bounded_sqlite_maximum() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(173);
    let version = version_id(174);
    let chain = chain_id(175);
    let mut store = open_store(&path);
    store
        .execute(command_id(176), create_prompt(prompt, version))
        .expect("create prompt");
    store
        .execute_chain(command_id(177), create_chain(chain))
        .expect("create chain");
    drop(store);

    let conn = Connection::open(&path).expect("open isolated raw connection");
    for position in 0..2_000_i64 {
        let link = PromptChainLinkId::new();
        conn.execute(
            "INSERT INTO prompt_chain_links(
                link_id, chain_id, position, prompt_id, prompt_version_id
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                link.as_bytes().as_slice(),
                chain.as_bytes().as_slice(),
                position,
                prompt.as_bytes().as_slice(),
                version.as_bytes().as_slice(),
            ],
        )
        .expect("fill chain to its bounded maximum");
    }
    let overflow = PromptChainLinkId::new();
    let error = conn
        .execute(
            "INSERT INTO prompt_chain_links(
                link_id, chain_id, position, prompt_id, prompt_version_id
             ) VALUES (?1, ?2, 2000, ?3, ?4)",
            rusqlite::params![
                overflow.as_bytes().as_slice(),
                chain.as_bytes().as_slice(),
                prompt.as_bytes().as_slice(),
                version.as_bytes().as_slice(),
            ],
        )
        .expect_err("chain link count must be bounded before projection allocation");
    assert!(error.to_string().contains("chain"));
}

#[test]
fn prompt_rebuild_rejects_an_oversized_journal_before_allocating_rows() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(176);
    let version = version_id(177);
    let mut store = open_store(&path);
    store
        .execute(command_id(178), create_prompt(prompt, version))
        .expect("create prompt");
    drop(store);

    let conn = Connection::open(&path).expect("open isolated raw connection");
    conn.execute_batch("DROP TRIGGER prompt_command_receipts_lineage_insert")
        .expect("disable lineage trigger for corruption fixture");
    let command_payload = canonical_create_prompt(prompt, version)
        .encode()
        .expect("canonical command payload");
    let command_sha256: [u8; 32] = Sha256::digest(&command_payload).into();
    let transaction = conn
        .unchecked_transaction()
        .expect("start oversized journal fixture transaction");
    for index in 0..10_001_u64 {
        let mut raw_command_id = [0_u8; 16];
        raw_command_id[..8].copy_from_slice(&index.to_be_bytes());
        transaction
            .execute(
                "INSERT INTO prompt_command_receipts(
                    command_id, command_sha256, command_payload, prompt_id, prompt_version_id,
                    revision, receipt, created_at_ms
                 ) VALUES (?1, ?2, ?3, zeroblob(16), zeroblob(16), 1, X'00', 1)",
                rusqlite::params![
                    raw_command_id.as_slice(),
                    command_sha256.as_slice(),
                    &command_payload,
                ],
            )
            .expect("seed bounded journal fixture row");
    }
    transaction
        .commit()
        .expect("commit oversized journal fixture");
    drop(conn);

    let error = PromptStore::open(&path)
        .expect_err("oversized journal must fail at open before replay allocation");
    match error {
        PromptStoreError::Corruption(message) => assert!(message.contains("10000")),
        other => panic!("unexpected oversized journal error: {other:?}"),
    }
    let conn = Connection::open(&path).expect("reopen database after bounded-journal rollback");
    let current_version_id: Vec<u8> = conn
        .query_row(
            "SELECT current_version_id FROM saved_prompts WHERE prompt_id = ?1",
            [prompt.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("prior projection must remain present");
    assert_eq!(current_version_id, version.as_bytes().to_vec());
}

#[test]
fn version_variables_cannot_be_inserted_after_version_creation() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(140);
    let version = version_id(141);
    let mut store = open_store(&path);
    store
        .execute(
            command_id(142),
            PromptCommand::CreatePrompt(CreatePrompt {
                prompt_id: prompt,
                prompt_version_id: version,
                title: "Variables".into(),
                description: None,
                tags: Vec::new(),
                variables: vec!["reviewer".into()],
                body: "Review {{reviewer}}".into(),
                created_at_ms: 1,
            }),
        )
        .expect("create version with variables");
    drop(store);

    let conn = Connection::open(&path).expect("open isolated raw connection");
    let error = conn
        .execute(
            "INSERT INTO prompt_version_variables(prompt_version_id, variable, position)
             VALUES (?1, 'later', 1)",
            [version.as_bytes().as_slice()],
        )
        .expect_err("version variables must be sealed after creation");
    assert!(error.to_string().to_lowercase().contains("sealed"));
}

#[test]
fn unsealed_prompt_version_is_reported_as_corruption() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(143);
    let initial_version = version_id(144);
    let unsealed_version = version_id(145);
    let mut store = open_store(&path);
    store
        .execute(command_id(146), create_prompt(prompt, initial_version))
        .expect("create prompt");
    drop(store);

    let body = "unsealed version";
    let body_sha256: [u8; 32] = Sha256::digest(body.as_bytes()).into();
    let conn = Connection::open(&path).expect("open isolated raw connection");
    conn.execute(
        "INSERT INTO prompt_versions(
            prompt_version_id, prompt_id, version, body, body_sha256, created_at_ms,
            variables_sealed
         ) VALUES (?1, ?2, 2, ?3, ?4, 2, 0)",
        rusqlite::params![
            unsealed_version.as_bytes().as_slice(),
            prompt.as_bytes().as_slice(),
            body,
            body_sha256.as_slice(),
        ],
    )
    .expect("insert unsealed corruption fixture");
    drop(conn);

    let error = PromptStore::open(&path).expect_err("unsealed version must fail closed at open");
    assert!(matches!(error, PromptStoreError::Corruption(_)));
}

#[test]
fn schema_rejects_multibyte_prompt_body_above_byte_limit() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(147);
    let initial_version = version_id(148);
    let mut store = open_store(&path);
    store
        .execute(command_id(149), create_prompt(prompt, initial_version))
        .expect("create prompt");
    drop(store);

    let body = "é".repeat((256 * 1024 / 2) + 1);
    assert_eq!(body.len(), (256 * 1024) + 2);
    let body_sha256: [u8; 32] = Sha256::digest(body.as_bytes()).into();
    let conn = Connection::open(&path).expect("open isolated raw connection");
    let error = conn
        .execute(
            "INSERT INTO prompt_versions(
                prompt_version_id, prompt_id, version, body, body_sha256, created_at_ms
             ) VALUES (?1, ?2, 2, ?3, ?4, 2)",
            rusqlite::params![
                version_id(150).as_bytes().as_slice(),
                prompt.as_bytes().as_slice(),
                body,
                body_sha256.as_slice(),
            ],
        )
        .expect_err("SQLite must enforce the UTF-8 byte limit");
    assert!(error.to_string().contains("body"));
}

#[test]
fn schema_rejects_untrimmed_tag_and_variable_rows() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(151);
    let version = version_id(152);
    let mut store = open_store(&path);
    store
        .execute(command_id(153), create_prompt(prompt, version))
        .expect("create prompt");
    drop(store);

    let conn = Connection::open(&path).expect("open isolated raw connection");
    let tag_error = conn
        .execute(
            "INSERT INTO prompt_tags(prompt_id, tag, position) VALUES (?1, ' extra ', 2)",
            [prompt.as_bytes().as_slice()],
        )
        .expect_err("SQLite-truthful tag trimming must be enforced");
    assert!(tag_error.to_string().contains("tag"));
    let variable_error = conn
        .execute(
            "INSERT INTO prompt_version_variables(prompt_version_id, variable, position)
             VALUES (?1, ' variable ', 0)",
            [version.as_bytes().as_slice()],
        )
        .expect_err("SQLite-truthful variable trimming must be enforced");
    assert!(variable_error.to_string().contains("variable"));
}

#[test]
fn sqlite_rejects_unicode_whitespace_wrapped_prompt_metadata_and_variables() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    devmanager::kernel::KernelStore::open(&path).expect("apply prompt schema");
    let conn = Connection::open(&path).expect("open isolated schema");
    let prompt = prompt_id(243);
    let version = version_id(244);
    let body = "unicode whitespace fixture";
    let body_hash: [u8; 32] = Sha256::digest(body.as_bytes()).into();

    let transaction = conn
        .unchecked_transaction()
        .expect("start metadata boundary fixture");
    transaction
        .execute(
            "INSERT INTO prompt_versions(
                prompt_version_id, prompt_id, version, body,
                body_sha256, created_at_ms, variables_sealed
             ) VALUES (?1, ?2, 1, ?3, ?4, 1, 0)",
            rusqlite::params![
                version.as_bytes().as_slice(),
                prompt.as_bytes().as_slice(),
                body,
                body_hash.as_slice(),
            ],
        )
        .expect("seed version for metadata boundary fixture");
    let metadata = transaction.execute(
        "INSERT INTO saved_prompts(
            prompt_id, title, description, current_version_id, revision,
            created_at_ms, updated_at_ms, archived_at_ms
         ) VALUES (?1, ?2, NULL, ?3, 1, 1, 1, NULL)",
        rusqlite::params![
            prompt.as_bytes().as_slice(),
            "\u{2003}Unicode title\u{00a0}",
            version.as_bytes().as_slice(),
        ],
    );
    assert!(
        metadata.is_err(),
        "SQLite must reject Unicode-whitespace-wrapped prompt metadata"
    );
    transaction
        .rollback()
        .expect("rollback metadata boundary fixture");

    let transaction = conn
        .unchecked_transaction()
        .expect("start variable boundary fixture");
    transaction
        .execute(
            "INSERT INTO prompt_versions(
                prompt_version_id, prompt_id, version, body,
                body_sha256, created_at_ms, variables_sealed
             ) VALUES (?1, ?2, 1, ?3, ?4, 1, 0)",
            rusqlite::params![
                version.as_bytes().as_slice(),
                prompt.as_bytes().as_slice(),
                body,
                body_hash.as_slice(),
            ],
        )
        .expect("seed version for variable boundary fixture");
    transaction
        .execute(
            "INSERT INTO saved_prompts(
                prompt_id, title, description, current_version_id, revision,
                created_at_ms, updated_at_ms, archived_at_ms
             ) VALUES (?1, 'Unicode variable', NULL, ?2, 1, 1, 1, NULL)",
            rusqlite::params![prompt.as_bytes().as_slice(), version.as_bytes().as_slice()],
        )
        .expect("seed prompt for variable boundary fixture");
    let variable = transaction.execute(
        "INSERT INTO prompt_version_variables(prompt_version_id, variable, position)
         VALUES (?1, ?2, 0)",
        rusqlite::params![version.as_bytes().as_slice(), "\u{2003}reviewer\u{00a0}",],
    );
    assert!(
        variable.is_err(),
        "SQLite must reject Unicode-whitespace-wrapped variables"
    );
    transaction
        .rollback()
        .expect("rollback variable boundary fixture");
}

#[test]
fn projection_rebuild_rejects_prompt_event_row_target_mismatch() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(154);
    let version = version_id(155);
    let mut store = open_store(&path);
    store
        .execute(command_id(156), create_prompt(prompt, version))
        .expect("create prompt");
    drop(store);

    let command = command_id(157);
    let event = PromptEvent::PromptRenamed {
        prompt_id: prompt,
        title: "Replayed title".into(),
        revision: 2,
    };
    let receipt = PromptMutationReceipt {
        command_id: command,
        prompt_id: prompt_id(158),
        prompt_version_id: version,
        revision: 2,
    };
    let command_payload = canonical_create_prompt(prompt, version)
        .encode()
        .expect("canonical command payload");
    let command_sha256: [u8; 32] = Sha256::digest(&command_payload).into();
    let conn = Connection::open(&path).expect("open isolated raw connection");
    conn.execute_batch("DROP TRIGGER prompt_command_receipts_lineage_insert")
        .expect("disable lineage trigger for corruption fixture");
    conn.execute(
        "INSERT INTO prompt_command_receipts(
            command_id, command_sha256, command_payload, prompt_id, prompt_version_id, revision,
            receipt, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            command.as_bytes().as_slice(),
            command_sha256.as_slice(),
            command_payload,
            receipt.prompt_id.as_bytes().as_slice(),
            receipt.prompt_version_id.as_bytes().as_slice(),
            2_i64,
            rmp_serde::to_vec_named(&receipt).expect("receipt payload"),
            2_i64,
        ],
    )
    .expect("insert prompt receipt");
    conn.execute(
        "INSERT INTO prompt_events(
            prompt_event_id, command_id, prompt_id, event_type, occurred_at_ms, payload
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            EventId::new().as_bytes().as_slice(),
            command.as_bytes().as_slice(),
            receipt.prompt_id.as_bytes().as_slice(),
            event.event_type(),
            2_i64,
            event.encode().expect("event payload"),
        ],
    )
    .expect("insert prompt event");
    drop(conn);

    let error = PromptStore::open(&path)
        .expect_err("event row target must match event and receipt lineage at open");
    assert!(matches!(error, PromptStoreError::Corruption(_)));
}

#[test]
fn projection_rebuild_rejects_invalid_event_id() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(159);
    let version = version_id(160);
    let mut store = open_store(&path);
    store
        .execute(command_id(161), create_prompt(prompt, version))
        .expect("create prompt");
    drop(store);

    let command = command_id(162);
    let event = PromptEvent::PromptRenamed {
        prompt_id: prompt,
        title: "Invalid event ID".into(),
        revision: 2,
    };
    let receipt = PromptMutationReceipt {
        command_id: command,
        prompt_id: prompt,
        prompt_version_id: version,
        revision: 2,
    };
    let command_payload = canonical_create_prompt(prompt, version)
        .encode()
        .expect("canonical command payload");
    let command_sha256: [u8; 32] = Sha256::digest(&command_payload).into();
    let conn = Connection::open(&path).expect("open isolated raw connection");
    conn.execute(
        "INSERT INTO prompt_command_receipts(
            command_id, command_sha256, command_payload, prompt_id, prompt_version_id, revision,
            receipt, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            command.as_bytes().as_slice(),
            command_sha256.as_slice(),
            command_payload,
            prompt.as_bytes().as_slice(),
            version.as_bytes().as_slice(),
            2_i64,
            rmp_serde::to_vec_named(&receipt).expect("receipt payload"),
            2_i64,
        ],
    )
    .expect("insert prompt receipt");
    conn.execute(
        "INSERT INTO prompt_events(
            prompt_event_id, command_id, prompt_id, event_type, occurred_at_ms, payload
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            [0u8; 16].as_slice(),
            command.as_bytes().as_slice(),
            prompt.as_bytes().as_slice(),
            event.event_type(),
            2_i64,
            event.encode().expect("event payload"),
        ],
    )
    .expect("insert invalid event id");
    drop(conn);

    let error = PromptStore::open(&path).expect_err("invalid event ID must fail closed at open");
    assert!(matches!(error, PromptStoreError::Corruption(_)));
}

#[test]
fn projection_rebuild_rejects_command_and_event_time_lineage_mismatch() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(163);
    let version = version_id(164);
    let mut store = open_store(&path);
    store
        .execute(command_id(165), create_prompt(prompt, version))
        .expect("create prompt");
    drop(store);

    let command = command_id(166);
    let receipt_command = command_id(167);
    let event = PromptEvent::PromptRenamed {
        prompt_id: prompt,
        title: "Lineage mismatch".into(),
        revision: 2,
    };
    let receipt = PromptMutationReceipt {
        command_id: receipt_command,
        prompt_id: prompt,
        prompt_version_id: version,
        revision: 2,
    };
    let command_payload = canonical_create_prompt(prompt, version)
        .encode()
        .expect("canonical command payload");
    let command_sha256: [u8; 32] = Sha256::digest(&command_payload).into();
    let conn = Connection::open(&path).expect("open isolated raw connection");
    conn.execute(
        "INSERT INTO prompt_command_receipts(
            command_id, command_sha256, command_payload, prompt_id, prompt_version_id, revision,
            receipt, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            command.as_bytes().as_slice(),
            command_sha256.as_slice(),
            command_payload,
            prompt.as_bytes().as_slice(),
            version.as_bytes().as_slice(),
            2_i64,
            rmp_serde::to_vec_named(&receipt).expect("receipt payload"),
            2_i64,
        ],
    )
    .expect("insert mismatched command receipt");
    conn.execute(
        "INSERT INTO prompt_events(
            prompt_event_id, command_id, prompt_id, event_type, occurred_at_ms, payload
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            EventId::new().as_bytes().as_slice(),
            command.as_bytes().as_slice(),
            prompt.as_bytes().as_slice(),
            event.event_type(),
            99_i64,
            event.encode().expect("event payload"),
        ],
    )
    .expect("insert lineage mismatch event");
    drop(conn);

    let error =
        PromptStore::open(&path).expect_err("command and time lineage must be validated at open");
    assert!(matches!(error, PromptStoreError::Corruption(_)));
}

#[test]
fn projection_rebuild_rejects_noncanonical_event_payload_bytes() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(168);
    let version = version_id(169);
    let mut store = open_store(&path);
    store
        .execute(command_id(170), create_prompt(prompt, version))
        .expect("create prompt");
    drop(store);

    let command = command_id(171);
    let event = PromptEvent::PromptRenamed {
        prompt_id: prompt,
        title: "Canonical bytes".into(),
        revision: 2,
    };
    let receipt = PromptMutationReceipt {
        command_id: command,
        prompt_id: prompt,
        prompt_version_id: version,
        revision: 2,
    };
    let command_payload = canonical_create_prompt(prompt, version)
        .encode()
        .expect("canonical command payload");
    let command_sha256: [u8; 32] = Sha256::digest(&command_payload).into();
    let conn = Connection::open(&path).expect("open isolated raw connection");
    conn.execute(
        "INSERT INTO prompt_command_receipts(
            command_id, command_sha256, command_payload, prompt_id, prompt_version_id, revision,
            receipt, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            command.as_bytes().as_slice(),
            command_sha256.as_slice(),
            command_payload,
            prompt.as_bytes().as_slice(),
            version.as_bytes().as_slice(),
            2_i64,
            rmp_serde::to_vec_named(&receipt).expect("receipt payload"),
            2_i64,
        ],
    )
    .expect("insert prompt receipt");
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
            noncanonical_prompt_renamed_payload(prompt, "Canonical bytes", 2),
        ],
    )
    .expect("insert noncanonical event");
    drop(conn);

    let error =
        PromptStore::open(&path).expect_err("event payload bytes must be canonical at open");
    assert!(matches!(error, PromptStoreError::Corruption(_)));
}

#[test]
fn chain_projection_rebuild_rejects_row_target_mismatch() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let chain = chain_id(172);
    let mut store = open_store(&path);
    store
        .execute_chain(
            command_id(173),
            PromptChainCommand::CreatePromptChain(CreatePromptChain {
                chain_id: chain,
                title: "Chain lineage".into(),
                description: None,
                created_at_ms: 1,
            }),
        )
        .expect("create chain");
    drop(store);

    let command = command_id(174);
    let event = devmanager::prompts::PromptChainEvent::PromptChainRenamed {
        chain_id: chain,
        title: "Replayed chain".into(),
        revision: 2,
    };
    let receipt = devmanager::prompts::PromptChainMutationReceipt {
        command_id: command,
        chain_id: chain_id(175),
        link_id: None,
        revision: 2,
    };
    let command_payload =
        chain_command_payload(&PromptChainCommand::CreatePromptChain(CreatePromptChain {
            chain_id: chain,
            title: "Chain lineage".into(),
            description: None,
            created_at_ms: 1,
        }));
    let command_sha256: [u8; 32] = Sha256::digest(&command_payload).into();
    let conn = Connection::open(&path).expect("open isolated raw connection");
    conn.execute_batch("DROP TRIGGER prompt_chain_command_receipts_lineage_insert")
        .expect("disable lineage trigger for corruption fixture");
    conn.execute(
        "INSERT INTO prompt_chain_command_receipts(
            command_id, command_sha256, command_payload, chain_id, chain_link_id, revision,
            receipt, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            command.as_bytes().as_slice(),
            command_sha256.as_slice(),
            command_payload,
            receipt.chain_id.as_bytes().as_slice(),
            Option::<Vec<u8>>::None,
            2_i64,
            rmp_serde::to_vec_named(&receipt).expect("chain receipt payload"),
            2_i64,
        ],
    )
    .expect("insert chain receipt");
    conn.execute(
        "INSERT INTO prompt_chain_events(
            prompt_chain_event_id, command_id, chain_id, event_type,
            occurred_at_ms, payload
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            EventId::new().as_bytes().as_slice(),
            command.as_bytes().as_slice(),
            receipt.chain_id.as_bytes().as_slice(),
            event.event_type(),
            2_i64,
            event.encode().expect("chain event payload"),
        ],
    )
    .expect("insert chain event");
    drop(conn);

    let error = PromptStore::open(&path)
        .expect_err("chain event target must match event and receipt lineage at open");
    assert!(matches!(error, PromptStoreError::Corruption(_)));
}

#[test]
fn saved_prompt_reads_reject_untrimmed_metadata() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(231);
    let mut store = open_store(&path);
    store
        .execute(command_id(232), create_prompt(prompt, version_id(233)))
        .expect("create prompt");
    drop(store);

    let conn = Connection::open(&path).expect("open isolated raw connection");
    conn.execute_batch("DROP TRIGGER saved_prompts_metadata_update_bounds")
        .expect("disable metadata trigger for corruption fixture");
    conn.execute(
        "UPDATE saved_prompts SET title = ?1, description = ?2 WHERE prompt_id = ?3",
        rusqlite::params![
            " Padded title ",
            " padded description ",
            prompt.as_bytes().as_slice()
        ],
    )
    .expect("corrupt prompt metadata");
    drop(conn);

    let error =
        PromptStore::open(&path).expect_err("saved prompt metadata corruption must fail at open");
    assert!(matches!(error, PromptStoreError::Corruption(_)));
}

#[test]
fn chain_link_reads_move_and_remove_reject_stale_prompt_pointer() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(234);
    let first = version_id(235);
    let chain = chain_id(236);
    let link = link_id(237);
    let mut store = open_store(&path);
    store
        .execute(command_id(238), create_prompt(prompt, first))
        .expect("create prompt");
    store
        .execute_chain(command_id(239), create_chain(chain))
        .expect("create chain");
    store
        .execute_chain(
            command_id(240),
            insert_link(chain, link, prompt, Some(first), None, 1),
        )
        .expect("create link");
    drop(store);

    let body = "later version";
    let body_sha256: [u8; 32] = Sha256::digest(body.as_bytes()).into();
    let conn = Connection::open(&path).expect("open isolated raw connection");
    conn.execute_batch("DROP TRIGGER IF EXISTS prompt_versions_advance_current_after_insert")
        .expect("disable current-version trigger for corruption fixture");
    conn.execute(
        "INSERT INTO prompt_versions(
            prompt_version_id, prompt_id, version, body, body_sha256, created_at_ms
         ) VALUES (?1, ?2, 2, ?3, ?4, 2)",
        rusqlite::params![
            version_id(241).as_bytes().as_slice(),
            prompt.as_bytes().as_slice(),
            body,
            body_sha256.as_slice(),
        ],
    )
    .expect("insert later version without advancing pointer");
    drop(conn);

    let error = PromptStore::open(&path)
        .expect_err("a stale prompt pointer must fail before chain reads or mutations");
    assert!(matches!(error, PromptStoreError::Corruption(_)));
}

#[test]
fn replay_rejects_archived_chain_create_event() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let chain = chain_id(244);
    let command = command_id(245);
    let event_type = "prompt_chain.created";
    let event_payload = rmp_serde::to_vec_named(&RawPromptChainEventWire {
        schema_version: 1,
        event: RawPromptChainEvent::PromptChainCreated {
            chain: RawPromptChain {
                id: chain,
                title: "Archived chain".into(),
                description: None,
                revision: 1,
                archived_at_ms: Some(9),
            },
        },
    })
    .expect("archived chain event payload");
    let receipt = PromptChainMutationReceipt {
        command_id: command,
        chain_id: chain,
        link_id: None,
        revision: 1,
    };
    let command_payload =
        chain_command_payload(&PromptChainCommand::CreatePromptChain(CreatePromptChain {
            chain_id: chain,
            title: "Archived chain".into(),
            description: None,
            created_at_ms: 1,
        }));
    let command_sha256: [u8; 32] = Sha256::digest(&command_payload).into();

    drop(open_store(&path));
    let conn = Connection::open(&path).expect("open isolated raw connection");
    conn.execute_batch("DROP TRIGGER prompt_chain_command_receipts_lineage_insert")
        .expect("disable lineage trigger for corruption fixture");
    conn.execute(
        "INSERT INTO prompt_chain_command_receipts(
            command_id, command_sha256, command_payload, chain_id, chain_link_id, revision,
            receipt, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            command.as_bytes().as_slice(),
            command_sha256.as_slice(),
            command_payload,
            chain.as_bytes().as_slice(),
            Option::<Vec<u8>>::None,
            1_i64,
            receipt.encode().expect("chain receipt payload"),
            1_i64,
        ],
    )
    .expect("seed chain receipt");
    conn.execute(
        "INSERT INTO prompt_chain_events(
            prompt_chain_event_id, command_id, chain_id, event_type,
            occurred_at_ms, payload
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            EventId::new().as_bytes().as_slice(),
            command.as_bytes().as_slice(),
            chain.as_bytes().as_slice(),
            event_type,
            1_i64,
            event_payload,
        ],
    )
    .expect("seed archived chain event");
    drop(conn);

    let error = PromptStore::open(&path)
        .expect_err("archived chain creation must fail at open before replay");
    assert!(matches!(error, PromptStoreError::Corruption(_)));
}

#[test]
fn sqlite_rejects_stale_current_pointer_on_saved_prompt_insert() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(230);
    let first = version_id(231);
    let second = version_id(232);
    devmanager::kernel::KernelStore::open(&path).expect("apply prompt schema");
    let conn = Connection::open(&path).expect("open isolated schema");
    let first_body = "first";
    let second_body = "second";
    let first_hash: [u8; 32] = Sha256::digest(first_body.as_bytes()).into();
    let second_hash: [u8; 32] = Sha256::digest(second_body.as_bytes()).into();
    let transaction = conn
        .unchecked_transaction()
        .expect("start deferred stale-pointer fixture");
    for (version_id, version, body, body_hash) in [
        (first, 1_i64, first_body, first_hash),
        (second, 2_i64, second_body, second_hash),
    ] {
        transaction
            .execute(
                "INSERT INTO prompt_versions(
                    prompt_version_id, prompt_id, version, body,
                    body_sha256, created_at_ms, variables_sealed
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1)",
                rusqlite::params![
                    version_id.as_bytes().as_slice(),
                    prompt.as_bytes().as_slice(),
                    version,
                    body,
                    body_hash.as_slice(),
                    version,
                ],
            )
            .expect("seed deferred version history");
    }
    let insert = transaction.execute(
        "INSERT INTO saved_prompts(
            prompt_id, title, description, current_version_id, revision,
            created_at_ms, updated_at_ms, archived_at_ms
         ) VALUES (?1, 'stale', NULL, ?2, 1, 1, 1, NULL)",
        rusqlite::params![prompt.as_bytes().as_slice(), first.as_bytes().as_slice()],
    );
    assert!(
        insert.is_err(),
        "SQLite must reject a stale current pointer on insert"
    );
    transaction
        .rollback()
        .expect("rollback stale-pointer fixture");
}

#[test]
fn sqlite_rejects_sparse_prompt_version_history_atomically() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(233);
    let first = version_id(234);
    let mut store = open_store(&path);
    store
        .execute(command_id(235), create_prompt(prompt, first))
        .expect("create prompt");
    drop(store);

    let conn = Connection::open(&path).expect("open isolated schema");
    let body = "sparse";
    let body_hash: [u8; 32] = Sha256::digest(body.as_bytes()).into();
    let sparse = version_id(236);
    let insert = conn.execute(
        "INSERT INTO prompt_versions(
            prompt_version_id, prompt_id, version, body,
            body_sha256, created_at_ms, variables_sealed
         ) VALUES (?1, ?2, 3, ?3, ?4, 3, 1)",
        rusqlite::params![
            sparse.as_bytes().as_slice(),
            prompt.as_bytes().as_slice(),
            body,
            body_hash.as_slice(),
        ],
    );
    assert!(
        insert.is_err(),
        "SQLite must reject a sparse version number"
    );
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM prompt_versions WHERE prompt_id = ?1",
            [prompt.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("count prompt versions");
    assert_eq!(
        count, 1,
        "failed sparse insert must leave history unchanged"
    );
}

#[test]
fn sqlite_rejects_missing_prompt_receipt_payload_atomically() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(237);
    let version = version_id(238);
    let command = command_id(239);
    let mut store = open_store(&path);
    store
        .execute(command_id(240), create_prompt(prompt, version))
        .expect("create prompt");
    drop(store);

    let receipt = PromptMutationReceipt {
        command_id: command,
        prompt_id: prompt,
        prompt_version_id: version,
        revision: 1,
    };
    let conn = Connection::open(&path).expect("open isolated schema");
    let insert = conn.execute(
        "INSERT INTO prompt_command_receipts(
            command_id, command_sha256, command_payload, prompt_id,
            prompt_version_id, revision, receipt, created_at_ms
         ) VALUES (?1, zeroblob(32), NULL, ?2, ?3, 1, ?4, 1)",
        rusqlite::params![
            command.as_bytes().as_slice(),
            prompt.as_bytes().as_slice(),
            version.as_bytes().as_slice(),
            receipt.encode().expect("encode receipt"),
        ],
    );
    assert!(
        insert.is_err(),
        "SQLite must reject a prompt receipt without exact command payload"
    );
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM prompt_command_receipts WHERE command_id = ?1",
            [command.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("count prompt receipts");
    assert_eq!(count, 0, "failed prompt receipt insert must be atomic");
}

#[test]
fn sqlite_rejects_missing_chain_receipt_payload_atomically() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let chain = chain_id(241);
    let command = command_id(242);
    let mut store = open_store(&path);
    store
        .execute_chain(command_id(243), create_chain(chain))
        .expect("create chain");
    drop(store);

    let receipt = PromptChainMutationReceipt {
        command_id: command,
        chain_id: chain,
        link_id: None,
        revision: 1,
    };
    let conn = Connection::open(&path).expect("open isolated schema");
    let insert = conn.execute(
        "INSERT INTO prompt_chain_command_receipts(
            command_id, command_sha256, command_payload, chain_id,
            chain_link_id, revision, receipt, created_at_ms
         ) VALUES (?1, zeroblob(32), NULL, ?2, NULL, 1, ?3, 1)",
        rusqlite::params![
            command.as_bytes().as_slice(),
            chain.as_bytes().as_slice(),
            receipt.encode().expect("encode chain receipt"),
        ],
    );
    assert!(
        insert.is_err(),
        "SQLite must reject a chain receipt without exact command payload"
    );
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM prompt_chain_command_receipts WHERE command_id = ?1",
            [command.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("count chain receipts");
    assert_eq!(count, 0, "failed chain receipt insert must be atomic");
}

#[test]
fn prompt_store_open_rejects_missing_lineage_migration_state() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    drop(open_store(&path));

    let conn = Connection::open(&path).expect("open isolated schema");
    conn.execute_batch("DROP TRIGGER prompt_lineage_migration_state_immutable_delete")
        .expect("disable state immutability for missing-state fixture");
    conn.execute(
        "DELETE FROM prompt_lineage_migration_state WHERE singleton_key = 1",
        [],
    )
    .expect("clear lineage migration state");
    drop(conn);

    let error = PromptStore::open(&path).expect_err(
        "a missing lineage migration state row must not make the prompt store look healthy",
    );
    assert!(matches!(error, PromptStoreError::Corruption(_)));
}

#[test]
fn sqlite_rejects_unblocking_unrepaired_lineage_quarantine() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    drop(open_store(&path));

    let conn = Connection::open(&path).expect("open isolated schema");
    conn.execute_batch("DROP TRIGGER prompt_lineage_quarantine_append_only_insert")
        .expect("disable migration-owned insert trigger for quarantine fixture");
    conn.execute(
        "INSERT INTO prompt_lineage_quarantine(
            source_kind, command_id, event_id, reason, command_sha256, quarantined_at_ms
         ) VALUES ('prompt_receipt', ?1, NULL, 'requires exact repair', zeroblob(32), 1)",
        [command_id(244).as_bytes().as_slice()],
    )
    .expect("seed unrepaired lineage quarantine");
    let unblock = conn.execute(
        "UPDATE prompt_lineage_migration_state SET blocked = 0 WHERE singleton_key = 1",
        [],
    );
    assert!(
        unblock.is_err(),
        "SQLite must require exact quarantine repair before unblocking"
    );
    drop(conn);

    let error = PromptStore::open(&path)
        .expect_err("unrepaired lineage quarantine must keep the prompt store blocked");
    assert!(matches!(error, PromptStoreError::Corruption(_)));
}

#[test]
fn sqlite_rejects_zero_hash_legacy_repair_without_external_authority() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(245);
    let version = version_id(246);
    let receipt_command = command_id(247);
    let mut store = open_store(&path);
    store
        .execute(receipt_command, create_prompt(prompt, version))
        .expect("create canonical receipt");
    drop(store);

    let conn = Connection::open(&path).expect("open isolated schema");
    conn.execute_batch(
        "DROP TRIGGER prompt_command_receipts_immutable_update;
         DROP TRIGGER prompt_lineage_quarantine_append_only_insert;
         DROP TRIGGER prompt_lineage_quarantine_ledger_immutable_insert;",
    )
    .expect("disable immutable guards for zero-hash legacy fixture");
    conn.execute(
        "UPDATE prompt_command_receipts
         SET command_sha256 = zeroblob(32)
         WHERE command_id = ?1",
        [receipt_command.as_bytes().as_slice()],
    )
    .expect("seed unknown legacy digest");
    conn.execute(
        "INSERT INTO prompt_lineage_quarantine(
            source_kind, command_id, event_id, reason, command_sha256, quarantined_at_ms
         ) VALUES ('prompt_receipt', ?1, NULL, 'unknown legacy bytes', zeroblob(32), 1)",
        [receipt_command.as_bytes().as_slice()],
    )
    .expect("seed zero-hash quarantine");
    let quarantine_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO prompt_lineage_quarantine_ledger(
            quarantine_id, source_kind, command_id, event_id, reason,
            command_sha256, quarantined_at_ms
         ) VALUES (?1, 'prompt_receipt', ?2, NULL, 'unknown legacy bytes', zeroblob(32), 1)",
        rusqlite::params![quarantine_id, receipt_command.as_bytes().as_slice()],
    )
    .expect("seed zero-hash immutable ledger");
    conn.execute(
        "UPDATE prompt_lineage_migration_state SET blocked = 1 WHERE singleton_key = 1",
        [],
    )
    .expect("enter derived blocked state");
    conn.execute(
        "DELETE FROM prompt_lineage_quarantine WHERE quarantine_id = ?1",
        [quarantine_id],
    )
    .expect("delete only through the audited repair transition");
    let unblock = conn.execute(
        "UPDATE prompt_lineage_migration_state SET blocked = 0 WHERE singleton_key = 1",
        [],
    );
    assert!(
        unblock.is_err(),
        "zero-hash supplied bytes require explicit external/manual authority"
    );
}

#[test]
fn sqlite_rejects_orphan_prompt_receipt_atomically() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let _store = open_store(&path);
    let conn = Connection::open(&path).expect("open isolated schema");
    let command = command_id(237);
    let insert = conn.execute(
        "INSERT INTO prompt_command_receipts(
            command_id, command_sha256, command_payload, prompt_id,
            prompt_version_id, revision, receipt, created_at_ms
         ) VALUES (?1, zeroblob(32), X'01', zeroblob(16), zeroblob(16),
                   1, X'01', 1)",
        [command.as_bytes().as_slice()],
    );
    assert!(
        insert.is_err(),
        "SQLite must reject a receipt without prompt lineage"
    );
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM prompt_command_receipts WHERE command_id = ?1",
            [command.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("count prompt receipts");
    assert_eq!(count, 0, "failed orphan receipt insert must be atomic");
}

#[test]
fn sqlite_rejects_orphan_chain_receipt_atomically() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let _store = open_store(&path);
    let conn = Connection::open(&path).expect("open isolated schema");
    let command = command_id(238);
    let insert = conn.execute(
        "INSERT INTO prompt_chain_command_receipts(
            command_id, command_sha256, command_payload, chain_id,
            chain_link_id, revision, receipt, created_at_ms
         ) VALUES (?1, zeroblob(32), X'01', zeroblob(16), NULL, 1, X'01', 1)",
        [command.as_bytes().as_slice()],
    );
    assert!(
        insert.is_err(),
        "SQLite must reject a receipt without chain lineage"
    );
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM prompt_chain_command_receipts WHERE command_id = ?1",
            [command.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("count chain receipts");
    assert_eq!(
        count, 0,
        "failed orphan chain receipt insert must be atomic"
    );
}

#[test]
fn lineage_quarantine_repair_marker_can_be_cleared_idempotently() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(246);
    let version = version_id(247);
    let receipt_command = command_id(248);
    let mut store = open_store(&path);
    store
        .execute(receipt_command, create_prompt(prompt, version))
        .expect("create canonical receipt for repair proof");
    drop(store);

    let conn = Connection::open(&path).expect("open isolated schema");
    let command_sha256: Vec<u8> = conn
        .query_row(
            "SELECT command_sha256 FROM prompt_command_receipts WHERE command_id = ?1",
            [receipt_command.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("read canonical receipt digest");
    conn.execute_batch(
        "DROP TRIGGER prompt_lineage_quarantine_append_only_insert;
         DROP TRIGGER prompt_lineage_quarantine_ledger_immutable_insert;",
    )
    .expect("disable migration-owned insert triggers for repair fixture");
    conn.execute(
        "INSERT INTO prompt_lineage_quarantine(
            source_kind, command_id, event_id, reason, command_sha256, quarantined_at_ms
         ) VALUES ('prompt_receipt', ?1, NULL, 'exact repair pending', ?2, 1)",
        rusqlite::params![receipt_command.as_bytes().as_slice(), &command_sha256],
    )
    .expect("seed migration repair marker");
    let quarantine_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO prompt_lineage_quarantine_ledger(
            quarantine_id, source_kind, command_id, event_id, reason,
            command_sha256, quarantined_at_ms
         ) VALUES (?1, 'prompt_receipt', ?2, NULL, 'exact repair pending', ?3, 1)",
        rusqlite::params![
            quarantine_id,
            receipt_command.as_bytes().as_slice(),
            &command_sha256,
        ],
    )
    .expect("seed immutable repair ledger");
    let ledger_update = conn
        .execute(
            "UPDATE prompt_lineage_quarantine_ledger SET reason = 'forged'",
            [],
        )
        .expect_err("quarantine ledger updates must be immutable");
    assert!(ledger_update.to_string().to_lowercase().contains("ledger"));
    let ledger_delete = conn
        .execute("DELETE FROM prompt_lineage_quarantine_ledger", [])
        .expect_err("quarantine ledger deletion must be append-only");
    assert!(ledger_delete.to_string().to_lowercase().contains("ledger"));
    conn.execute(
        "UPDATE prompt_lineage_migration_state SET blocked = 1 WHERE singleton_key = 1",
        [],
    )
    .expect("enter derived blocked state");
    conn.execute("DELETE FROM prompt_lineage_quarantine", [])
        .expect("clear repaired quarantine marker");
    conn.execute(
        "UPDATE prompt_lineage_migration_state SET blocked = 0 WHERE singleton_key = 1",
        [],
    )
    .expect("leave derived blocked state");
    drop(conn);

    PromptStore::open(&path).expect("exact repair marker cleanup must reopen healthy");
    let conn = Connection::open(&path).expect("reopen repaired schema");
    conn.execute(
        "UPDATE prompt_lineage_migration_state SET blocked = 0 WHERE singleton_key = 1",
        [],
    )
    .expect("repair unblock must be idempotent");
}

#[test]
fn lineage_creation_commitment_rejects_a_forged_canonical_ledger() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(252);
    let version = version_id(253);
    let receipt_command = command_id(254);
    let mut store = open_store(&path);
    store
        .execute(receipt_command, create_prompt(prompt, version))
        .expect("create canonical receipt for forged ledger fixture");
    drop(store);

    let conn = Connection::open(&path).expect("open isolated schema");
    let command_sha256: Vec<u8> = conn
        .query_row(
            "SELECT command_sha256 FROM prompt_command_receipts WHERE command_id = ?1",
            [receipt_command.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("read canonical receipt digest");
    conn.execute_batch("DROP TRIGGER prompt_lineage_quarantine_ledger_immutable_insert")
        .expect("disable ledger insert trigger for forged-ledger fixture");
    conn.execute(
        "INSERT INTO prompt_lineage_quarantine_ledger(
            quarantine_id, source_kind, command_id, event_id, reason,
            command_sha256, quarantined_at_ms
         ) VALUES (1, 'prompt_receipt', ?1, NULL, 'forged ledger', ?2, 1)",
        rusqlite::params![receipt_command.as_bytes().as_slice(), &command_sha256],
    )
    .expect("seed forged ledger with a currently canonical receipt proof");
    drop(conn);

    let error = PromptStore::open(&path)
        .expect_err("a ledger row without immutable migration provenance must fail at open");
    assert!(matches!(error, PromptStoreError::Corruption(_)));
}

#[test]
fn lineage_creation_commitment_rejects_state_delete_and_recreate_bypass() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    drop(open_store(&path));

    let conn = Connection::open(&path).expect("open isolated schema");
    let state_columns: Vec<String> = {
        let mut statement = conn
            .prepare("PRAGMA table_info(prompt_lineage_migration_state)")
            .expect("inspect lineage state schema");
        statement
            .query_map([], |row| row.get(1))
            .expect("query lineage state columns")
            .map(|row| row.expect("lineage state column"))
            .collect()
    };
    assert!(
        state_columns
            .iter()
            .any(|column| column == "creation_token"),
        "lineage state must be bound to its immutable creation commitment"
    );
    let creation_token: Vec<u8> = conn
        .query_row(
            "SELECT creation_token FROM prompt_lineage_migration_state
             WHERE singleton_key = 1",
            [],
            |row| row.get(0),
        )
        .expect("read lineage state creation token");
    conn.execute_batch(
        "DROP TRIGGER prompt_lineage_migration_state_immutable_delete;
         DROP TRIGGER prompt_lineage_migration_state_append_only_insert;",
    )
    .expect("disable state delete and recreation guards for bypass fixture");
    conn.execute(
        "DELETE FROM prompt_lineage_migration_state WHERE singleton_key = 1",
        [],
    )
    .expect("delete state through disabled trigger");
    conn.execute(
        "INSERT INTO prompt_lineage_migration_state(singleton_key, creation_token, blocked)
         VALUES (1, zeroblob(32), 0)",
        [],
    )
    .expect("recreate state with a forged creation token");
    assert_ne!(creation_token, vec![0; 32]);
    drop(conn);

    let error = PromptStore::open(&path)
        .expect_err("state deletion and forged recreation must fail at open");
    assert!(matches!(error, PromptStoreError::Corruption(_)));
}

#[test]
fn open_rejects_historical_metadata_receipt_forged_to_a_later_version() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(255);
    let version_one = version_id(1);
    let version_two = version_id(2);
    let mut store = open_store(&path);
    store
        .execute(command_id(3), create_prompt(prompt, version_one))
        .expect("create version one");
    store
        .execute(
            command_id(4),
            PromptCommand::RenamePrompt(RenamePrompt {
                prompt_id: prompt,
                title: "Renamed at version one".into(),
                expected_revision: 1,
            }),
        )
        .expect("rename at version one");
    store
        .execute(
            command_id(5),
            PromptCommand::CreatePromptVersion(CreatePromptVersion {
                prompt_id: prompt,
                prompt_version_id: version_two,
                variables: Vec::new(),
                body: "version two".into(),
                created_at_ms: 1_725_000_000_002,
                expected_revision: 2,
            }),
        )
        .expect("create version two");
    drop(store);

    let conn = Connection::open(&path).expect("open isolated schema");
    conn.execute_batch("DROP TRIGGER prompt_command_receipts_immutable_update")
        .expect("disable receipt immutability for historical-version fixture");
    conn.execute(
        "UPDATE prompt_command_receipts SET prompt_version_id = ?1
         WHERE command_id = ?2",
        rusqlite::params![
            version_two.as_bytes().as_slice(),
            command_id(4).as_bytes().as_slice()
        ],
    )
    .expect("forge metadata receipt to the later version");
    drop(conn);

    let error = PromptStore::open(&path).expect_err(
        "historical metadata receipt version must match the replay-time version cursor",
    );
    assert!(matches!(error, PromptStoreError::Corruption(_)));
}

#[test]
fn sqlite_rejects_oversized_prompt_receipt_and_event_payloads_before_store_reads() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(6);
    let version = version_id(7);
    let command = command_id(8);
    let mut store = open_store(&path);
    store
        .execute(command, create_prompt(prompt, version))
        .expect("create prompt");
    drop(store);

    let oversized = vec![0_u8; 512 * 1024 + 1];
    let conn = Connection::open(&path).expect("open isolated schema");
    let receipt_insert = conn.execute(
        "UPDATE prompt_command_receipts SET receipt = ?1 WHERE command_id = ?2",
        rusqlite::params![&oversized, command.as_bytes().as_slice()],
    );
    assert!(
        receipt_insert.is_err(),
        "SQLite must reject oversized prompt receipt payloads"
    );

    let event_insert = conn.execute(
        "UPDATE prompt_events SET payload = ?1 WHERE command_id = ?2",
        rusqlite::params![&oversized, command.as_bytes().as_slice()],
    );
    assert!(
        event_insert.is_err(),
        "SQLite must reject oversized prompt event payloads"
    );
}

#[test]
fn projection_rebuild_rejects_oversized_command_payload_before_decode() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(239);
    let version = version_id(240);
    let mut store = open_store(&path);
    store
        .execute(command_id(241), create_prompt(prompt, version))
        .expect("create prompt");
    drop(store);

    let command = command_id(242);
    let payload = vec![0_u8; 512 * 1024 + 1];
    let receipt = PromptMutationReceipt {
        command_id: command,
        prompt_id: prompt,
        prompt_version_id: version,
        revision: 1,
    };
    let conn = Connection::open(&path).expect("open isolated schema");
    conn.execute_batch(
        "DROP TRIGGER prompt_command_receipts_command_payload_size_insert;
         DROP TRIGGER prompt_command_receipts_command_payload_size_update;",
    )
    .expect("disable payload size guards for pre-existing-row fixture");
    conn.execute(
        "INSERT INTO prompt_command_receipts(
            command_id, command_sha256, command_payload, prompt_id,
            prompt_version_id, revision, receipt, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, 2)",
        rusqlite::params![
            command.as_bytes().as_slice(),
            Sha256::digest(&payload).as_slice(),
            payload,
            prompt.as_bytes().as_slice(),
            version.as_bytes().as_slice(),
            receipt.encode().expect("encode receipt"),
        ],
    )
    .expect("seed oversized command receipt");
    drop(conn);

    let error = PromptStore::open(&path)
        .expect_err("oversized command payload must fail closed before replay");
    assert!(
        error.to_string().contains("exceeds maximum"),
        "expected bounded-wire error, got {error:?}"
    );
}

#[test]
fn replay_rejects_duplicate_chain_link_ids() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let chain = chain_id(246);
    let create_command = command_id(247);
    let link_command = command_id(248);
    let link = link_id(249);
    let create_event = PromptChainEvent::PromptChainCreated {
        chain: PromptChain {
            id: chain,
            title: "Chain".into(),
            description: None,
            revision: 1,
            archived_at_ms: None,
        },
    };
    let link_event_payload = rmp_serde::to_vec_named(&RawPromptChainEventWire {
        schema_version: 1,
        event: RawPromptChainEvent::PromptChainLinksReplaced {
            chain_id: chain,
            links: vec![
                RawPromptChainLink {
                    id: link,
                    chain_id: chain,
                    position: 0,
                    prompt_id: prompt_id(250),
                    prompt_version_id: version_id(251),
                },
                RawPromptChainLink {
                    id: link,
                    chain_id: chain,
                    position: 1,
                    prompt_id: prompt_id(252),
                    prompt_version_id: version_id(253),
                },
            ],
            revision: 2,
        },
    })
    .expect("duplicate-link event payload");
    let create_receipt = PromptChainMutationReceipt {
        command_id: create_command,
        chain_id: chain,
        link_id: None,
        revision: 1,
    };
    let link_receipt = PromptChainMutationReceipt {
        command_id: link_command,
        chain_id: chain,
        link_id: Some(link),
        revision: 2,
    };

    drop(open_store(&path));
    let conn = Connection::open(&path).expect("open isolated raw connection");
    conn.execute_batch("DROP TRIGGER prompt_chain_command_receipts_lineage_insert")
        .expect("disable lineage trigger for corruption fixture");
    let command_payload =
        chain_command_payload(&PromptChainCommand::CreatePromptChain(CreatePromptChain {
            chain_id: chain,
            title: "Chain".into(),
            description: None,
            created_at_ms: 1,
        }));
    let command_sha256: [u8; 32] = Sha256::digest(&command_payload).into();
    for (command, receipt) in [
        (create_command, create_receipt),
        (link_command, link_receipt),
    ] {
        conn.execute(
            "INSERT INTO prompt_chain_command_receipts(
                command_id, command_sha256, command_payload, chain_id, chain_link_id, revision,
                receipt, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                command.as_bytes().as_slice(),
                command_sha256.as_slice(),
                &command_payload,
                chain.as_bytes().as_slice(),
                receipt.link_id.map(|id| id.as_bytes().as_slice().to_vec()),
                i64::try_from(receipt.revision).expect("receipt revision fits SQLite"),
                receipt.encode().expect("chain receipt payload"),
                1_i64,
            ],
        )
        .expect("seed chain receipt");
    }
    for (command, event_type, event_payload) in [
        (
            create_command,
            "prompt_chain.created",
            create_event.encode().expect("chain-created event payload"),
        ),
        (
            link_command,
            "prompt_chain.links_replaced",
            link_event_payload,
        ),
    ] {
        conn.execute(
            "INSERT INTO prompt_chain_events(
                prompt_chain_event_id, command_id, chain_id, event_type,
                occurred_at_ms, payload
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                EventId::new().as_bytes().as_slice(),
                command.as_bytes().as_slice(),
                chain.as_bytes().as_slice(),
                event_type,
                1_i64,
                event_payload,
            ],
        )
        .expect("seed chain event");
    }
    drop(conn);

    let error = PromptStore::open(&path)
        .expect_err("duplicate chain link IDs must fail at open before replay");
    assert!(matches!(error, PromptStoreError::Corruption(_)));
}

#[test]
fn replay_rejects_prompt_created_with_stale_current_pointer() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(254);
    let version = version_id(255);
    let mut store = open_store(&path);
    store
        .execute(command_id(1), create_prompt(prompt, version))
        .expect("create prompt");
    let saved_version = store
        .get_version(version)
        .expect("query version")
        .expect("version");
    drop(store);

    let command = command_id(2);
    let event_type = "prompt.created";
    let event_payload = rmp_serde::to_vec_named(&RawPromptEventWire {
        schema_version: 1,
        event: RawPromptEvent::PromptCreated {
            prompt: RawSavedPrompt {
                id: prompt,
                title: "Review code".into(),
                description: Some("A bounded local prompt".into()),
                tags: vec!["rust".into(), "review".into()],
                current_version_id: version_id(3),
                revision: 1,
                archived_at_ms: None,
            },
            version: saved_version,
        },
    })
    .expect("stale prompt event payload");
    let receipt = PromptMutationReceipt {
        command_id: command,
        prompt_id: prompt,
        prompt_version_id: version,
        revision: 1,
    };
    let command_payload = canonical_create_prompt(prompt, version)
        .encode()
        .expect("canonical command payload");
    let command_sha256: [u8; 32] = Sha256::digest(&command_payload).into();
    let conn = Connection::open(&path).expect("open isolated raw connection");
    conn.execute(
        "INSERT INTO prompt_command_receipts(
            command_id, command_sha256, command_payload, prompt_id, prompt_version_id, revision,
            receipt, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            command.as_bytes().as_slice(),
            command_sha256.as_slice(),
            command_payload,
            prompt.as_bytes().as_slice(),
            version.as_bytes().as_slice(),
            1_i64,
            receipt.encode().expect("prompt receipt payload"),
            1_i64,
        ],
    )
    .expect("seed prompt receipt");
    conn.execute(
        "INSERT INTO prompt_events(
            prompt_event_id, command_id, prompt_id, event_type,
            occurred_at_ms, payload
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            EventId::new().as_bytes().as_slice(),
            command.as_bytes().as_slice(),
            prompt.as_bytes().as_slice(),
            event_type,
            1_i64,
            event_payload,
        ],
    )
    .expect("seed stale prompt event");
    drop(conn);

    let error = PromptStore::open(&path)
        .expect_err("stale current pointer must fail at open before replay");
    assert!(matches!(error, PromptStoreError::Corruption(_)));
}

#[test]
fn open_rejects_semantic_event_corruption_before_replay() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(1);
    let version = version_id(2);
    let rename_command_id = command_id(3);
    let mut store = open_store(&path);
    store
        .execute(command_id(4), create_prompt(prompt, version))
        .expect("create prompt");
    store
        .execute(
            rename_command_id,
            PromptCommand::RenamePrompt(RenamePrompt {
                prompt_id: prompt,
                title: "Renamed code".into(),
                expected_revision: 1,
            }),
        )
        .expect("rename prompt");
    drop(store);

    let forged_event = PromptEvent::PromptRenamed {
        prompt_id: prompt,
        title: "Forged event".into(),
        revision: 2,
    };
    let conn = Connection::open(&path).expect("open isolated raw connection");
    conn.execute_batch("DROP TRIGGER prompt_events_immutable_update")
        .expect("disable event immutability for corruption fixture");
    conn.execute(
        "UPDATE prompt_events SET payload = ?1
         WHERE command_id = ?2 AND event_type = 'prompt.renamed'",
        rusqlite::params![
            forged_event.encode().expect("forged event payload"),
            rename_command_id.as_bytes().as_slice(),
        ],
    )
    .expect("forge event payload");
    drop(conn);

    let error = PromptStore::open(&path).expect_err(
        "open must validate command, receipt, event, and projection semantics before healthy",
    );
    assert!(matches!(error, PromptStoreError::Corruption(_)));
}

#[test]
fn open_rejects_stale_current_projection_before_replay() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(5);
    let first_version = version_id(6);
    let second_version = version_id(7);
    let mut store = open_store(&path);
    store
        .execute(command_id(8), create_prompt(prompt, first_version))
        .expect("create prompt");
    store
        .execute(
            command_id(9),
            PromptCommand::CreatePromptVersion(CreatePromptVersion {
                prompt_id: prompt,
                prompt_version_id: second_version,
                variables: Vec::new(),
                body: "second body".into(),
                created_at_ms: 2,
                expected_revision: 1,
            }),
        )
        .expect("create second version");
    drop(store);

    let conn = Connection::open(&path).expect("open isolated raw connection");
    conn.execute_batch("DROP TRIGGER saved_prompts_current_version_is_latest")
        .expect("disable current pointer trigger for corruption fixture");
    conn.execute(
        "UPDATE saved_prompts SET current_version_id = ?1 WHERE prompt_id = ?2",
        rusqlite::params![
            first_version.as_bytes().as_slice(),
            prompt.as_bytes().as_slice(),
        ],
    )
    .expect("forge stale current pointer");
    drop(conn);

    let error = PromptStore::open(&path)
        .expect_err("open must reject a stale current projection before replay");
    assert!(matches!(error, PromptStoreError::Corruption(_)));
}

#[test]
fn open_rejects_noncontiguous_event_sequence_before_replay() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(5);
    let mut store = open_store(&path);
    store
        .execute(command_id(6), create_prompt(prompt, version_id(7)))
        .expect("create prompt");
    store
        .execute(
            command_id(8),
            PromptCommand::RenamePrompt(RenamePrompt {
                prompt_id: prompt,
                title: "Renamed code".into(),
                expected_revision: 1,
            }),
        )
        .expect("rename prompt");
    drop(store);

    let conn = Connection::open(&path).expect("open isolated raw connection");
    conn.execute_batch("DROP TRIGGER prompt_events_immutable_update")
        .expect("disable event immutability for sequence fixture");
    conn.execute(
        "UPDATE prompt_events SET sequence = 3 WHERE sequence = 2",
        [],
    )
    .expect("forge a sequence gap");
    drop(conn);

    let error =
        PromptStore::open(&path).expect_err("a noncontiguous event sequence must fail at open");
    assert!(matches!(error, PromptStoreError::Corruption(_)));
}

#[test]
fn open_rejects_non_positive_event_sequence_before_replay() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(8);
    let version = version_id(9);
    let mut store = open_store(&path);
    store
        .execute(command_id(10), create_prompt(prompt, version))
        .expect("create prompt");
    let stored_version = store
        .get_version(version)
        .expect("query version")
        .expect("created version");
    drop(store);

    let duplicate_command = command_id(11);
    let receipt = PromptMutationReceipt {
        command_id: duplicate_command,
        prompt_id: prompt,
        prompt_version_id: version,
        revision: 1,
    };
    let event = PromptEvent::PromptCreated {
        prompt: SavedPrompt {
            id: prompt,
            title: "Review code".into(),
            description: Some("A bounded local prompt".into()),
            tags: vec!["rust".into(), "review".into()],
            current_version_id: version,
            revision: 1,
            archived_at_ms: None,
        },
        version: stored_version,
    };
    let conn = Connection::open(&path).expect("open isolated raw connection");
    let command_payload: Vec<u8> = conn
        .query_row(
            "SELECT command_payload FROM prompt_command_receipts WHERE command_id = ?1",
            [command_id(10).as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("read canonical command payload");
    let command_sha256: [u8; 32] = Sha256::digest(&command_payload).into();
    conn.execute(
        "INSERT INTO prompt_command_receipts(
            command_id, command_sha256, command_payload, prompt_id, prompt_version_id,
            revision, receipt, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, 1725000000000)",
        rusqlite::params![
            duplicate_command.as_bytes().as_slice(),
            command_sha256.as_slice(),
            command_payload,
            prompt.as_bytes().as_slice(),
            version.as_bytes().as_slice(),
            receipt.encode().expect("receipt payload"),
        ],
    )
    .expect("seed duplicate receipt fixture");
    conn.execute(
        "INSERT INTO prompt_events(
            prompt_event_id, command_id, prompt_id, event_type, occurred_at_ms, payload
         ) VALUES (?1, ?2, ?3, ?4, 1725000000000, ?5)",
        rusqlite::params![
            EventId::new().as_bytes().as_slice(),
            duplicate_command.as_bytes().as_slice(),
            prompt.as_bytes().as_slice(),
            event.event_type(),
            event.encode().expect("event payload"),
        ],
    )
    .expect("seed duplicate event fixture");
    conn.execute_batch("DROP TRIGGER prompt_events_immutable_update")
        .expect("disable event immutability for sequence fixture");
    conn.execute(
        "UPDATE prompt_events SET sequence = 0 WHERE command_id = ?1",
        [duplicate_command.as_bytes().as_slice()],
    )
    .expect("forge non-positive event sequence");
    drop(conn);

    let error = PromptStore::open(&path)
        .expect_err("non-positive event sequence must fail at open before replay");
    assert!(matches!(error, PromptStoreError::Corruption(_)));
}

#[test]
fn open_rejects_non_ascii_tag_in_durable_receipt_and_sql_projection() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt = prompt_id(10);
    let version = version_id(11);
    let mut store = open_store(&path);
    store
        .execute(
            command_id(12),
            PromptCommand::CreatePrompt(CreatePrompt {
                prompt_id: prompt,
                prompt_version_id: version,
                title: "ASCII tags".into(),
                description: None,
                tags: Vec::new(),
                variables: Vec::new(),
                body: "body".into(),
                created_at_ms: 1,
            }),
        )
        .expect("create prompt");
    drop(store);

    let conn = Connection::open(&path).expect("open isolated raw connection");
    let sql_error = conn
        .execute(
            "INSERT INTO prompt_tags(prompt_id, tag, position) VALUES (?1, ?2, 0)",
            rusqlite::params![prompt.as_bytes().as_slice(), "café"],
        )
        .expect_err("SQLite must reject non-ASCII tags under the shared policy");
    assert!(sql_error.to_string().to_lowercase().contains("tag"));

    let command_id = command_id(13);
    let command_payload = rmp_serde::to_vec_named(&RawPromptCommandWire {
        schema_version: 1,
        command: RawPromptCommand::SetPromptTags {
            prompt_id: prompt,
            tags: vec!["café".into()],
            expected_revision: 1,
        },
    })
    .expect("encode invalid durable command fixture");
    let command_sha256: [u8; 32] = Sha256::digest(&command_payload).into();
    let receipt = PromptMutationReceipt {
        command_id,
        prompt_id: prompt,
        prompt_version_id: version,
        revision: 2,
    };
    conn.execute(
        "INSERT INTO prompt_command_receipts(
            command_id, command_sha256, command_payload, prompt_id, prompt_version_id,
            revision, receipt, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            command_id.as_bytes().as_slice(),
            command_sha256.as_slice(),
            command_payload,
            prompt.as_bytes().as_slice(),
            version.as_bytes().as_slice(),
            2_i64,
            receipt.encode().expect("receipt payload"),
            2_i64,
        ],
    )
    .expect("seed invalid tag receipt");
    drop(conn);

    let error = PromptStore::open(&path)
        .expect_err("open must reject a receipt whose canonical command violates tag policy");
    assert!(matches!(error, PromptStoreError::Corruption(_)));
}

#[test]
fn lineage_state_and_quarantine_rows_are_not_directly_mutable() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let mut store = open_store(&path);
    store
        .execute(command_id(14), create_prompt(prompt_id(15), version_id(16)))
        .expect("create prompt");
    drop(store);

    let conn = Connection::open(&path).expect("open isolated raw connection");
    let direct_insert = conn.execute(
        "INSERT INTO prompt_lineage_quarantine(
            source_kind, command_id, event_id, reason, command_sha256, quarantined_at_ms
         ) VALUES ('direct', ?1, NULL, 'forged', zeroblob(32), 1)",
        [command_id(17).as_bytes().as_slice()],
    );
    assert!(
        direct_insert.is_err(),
        "quarantine rows must be migration-owned"
    );
    conn.execute_batch("DROP TRIGGER prompt_lineage_quarantine_append_only_insert")
        .expect("disable migration-owned insert trigger for quarantine fixture");
    conn.execute(
        "INSERT INTO prompt_lineage_quarantine(
            source_kind, command_id, event_id, reason, command_sha256, quarantined_at_ms
         ) VALUES ('test', ?1, NULL, 'fixture', zeroblob(32), 1)",
        [command_id(17).as_bytes().as_slice()],
    )
    .expect("seed quarantine fixture");
    let quarantine_update = conn
        .execute("UPDATE prompt_lineage_quarantine SET reason = 'forged'", [])
        .expect_err("quarantine rows must be immutable");
    assert!(quarantine_update
        .to_string()
        .to_lowercase()
        .contains("quarantine"));
    let state_delete = conn
        .execute("DELETE FROM prompt_lineage_migration_state", [])
        .expect_err("lineage state must not be replaceable");
    assert!(state_delete.to_string().to_lowercase().contains("lineage"));
    let state_insert = conn
        .execute(
            "INSERT INTO prompt_lineage_migration_state(singleton_key, blocked)
             VALUES (1, 0)",
            [],
        )
        .expect_err("lineage state must not be replaceable");
    assert!(state_insert.to_string().to_lowercase().contains("lineage"));
    let blocked_with_quarantine = conn
        .execute(
            "UPDATE prompt_lineage_migration_state SET blocked = 0
             WHERE singleton_key = 1",
            [],
        )
        .expect_err("state cannot be unblocked while quarantine remains");
    assert!(blocked_with_quarantine
        .to_string()
        .to_lowercase()
        .contains("repair"));
    conn.execute_batch("DROP TRIGGER prompt_command_receipts_immutable_update")
        .expect("disable receipt immutability for invalid lineage fixture");
    conn.execute(
        "UPDATE prompt_command_receipts SET command_payload = X'01'
         WHERE command_id = ?1",
        [command_id(14).as_bytes().as_slice()],
    )
    .expect("forge a noncanonical durable receipt");
    let quarantine_delete = conn
        .execute("DELETE FROM prompt_lineage_quarantine", [])
        .expect_err("quarantine deletion must require immutable repair provenance");
    assert!(quarantine_delete
        .to_string()
        .to_lowercase()
        .contains("provenance"));
    conn.execute_batch("DROP TRIGGER prompt_lineage_quarantine_immutable_delete")
        .expect("disable quarantine delete protection for tamper fixture");
    conn.execute("DELETE FROM prompt_lineage_quarantine", [])
        .expect("remove quarantine after bypassing delete protection");
    conn.execute(
        "UPDATE prompt_lineage_migration_state SET blocked = 0
         WHERE singleton_key = 1",
        [],
    )
    .expect("SQLite structural repair marker update");
    drop(conn);

    let error = PromptStore::open(&path)
        .expect_err("quarantine deletion and forged unblock must not forge health");
    assert!(matches!(error, PromptStoreError::Corruption(_)));
}
