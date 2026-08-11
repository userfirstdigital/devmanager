use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::thread;

use devmanager::domain::{CommandId, PromptChainId, PromptChainLinkId, PromptId, PromptVersionId};
use devmanager::prompts::{
    ArchivePrompt, CreatePrompt, CreatePromptChain, CreatePromptVersion, InsertPromptChainLink,
    MovePromptChainLink, PromptChainCommand, PromptChainService, PromptCommand, PromptStore,
    PromptStoreError, RemovePromptChainLink, UpdatePromptChainLinkVersion,
};
use rusqlite::Connection;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct StrictDurableChainCommandWire<'a> {
    schema_version: u32,
    original_command_sha256: [u8; 32],
    original_command_payload: Vec<u8>,
    command: &'a PromptChainCommand,
    resolved_prompt_version_id: Option<PromptVersionId>,
}

#[derive(Serialize)]
struct UnknownStrictDurableChainCommandWire<'a> {
    schema_version: u32,
    original_command_sha256: [u8; 32],
    original_command_payload: Vec<u8>,
    command: &'a PromptChainCommand,
    resolved_prompt_version_id: Option<PromptVersionId>,
    unexpected: u8,
}

#[derive(Serialize)]
struct MissingOriginalStrictDurableChainCommandWire<'a> {
    schema_version: u32,
    command: &'a PromptChainCommand,
    resolved_prompt_version_id: Option<PromptVersionId>,
}

fn db_path(dir: &TempDir) -> PathBuf {
    dir.path().join("prompts.sqlite3")
}

fn open_store(path: &Path) -> PromptStore {
    PromptStore::open(path).expect("open isolated prompt store")
}

fn create_prompt(store: &mut PromptStore, prompt_id: PromptId, version_id: PromptVersionId) {
    store
        .execute(
            CommandId::new(),
            PromptCommand::CreatePrompt(CreatePrompt {
                prompt_id,
                prompt_version_id: version_id,
                title: "Manual chain prompt".into(),
                description: Some("A prompt used by a manual ordered chain".into()),
                tags: Vec::new(),
                variables: Vec::new(),
                body: "Inspect the selected change.".into(),
                created_at_ms: 1,
            }),
        )
        .expect("create prompt");
}

fn create_chain(store: &mut PromptStore, chain_id: PromptChainId) {
    let mut service = PromptChainService::new(store);
    service
        .apply(
            CommandId::new(),
            PromptChainCommand::CreatePromptChain(CreatePromptChain {
                chain_id,
                title: "Manual review chain".into(),
                description: Some("Human-guided ordered prompts".into()),
                created_at_ms: 1,
            }),
        )
        .expect("create prompt chain");
}

fn append_link(
    store: &mut PromptStore,
    chain_id: PromptChainId,
    link_id: PromptChainLinkId,
    prompt_id: PromptId,
    prompt_version_id: Option<PromptVersionId>,
    expected_revision: u64,
) {
    let mut service = PromptChainService::new(store);
    service
        .apply(
            CommandId::new(),
            PromptChainCommand::InsertPromptChainLink(InsertPromptChainLink {
                chain_id,
                link_id,
                prompt_id,
                prompt_version_id,
                before_link_id: None,
                expected_revision,
            }),
        )
        .expect("append prompt chain link");
}

fn prompt_version(
    store: &mut PromptStore,
    prompt_id: PromptId,
    prompt_version_id: PromptVersionId,
    body: &str,
    expected_revision: u64,
) {
    store
        .execute(
            CommandId::new(),
            PromptCommand::CreatePromptVersion(CreatePromptVersion {
                prompt_id,
                prompt_version_id,
                variables: Vec::new(),
                body: body.into(),
                created_at_ms: i64::try_from(expected_revision + 1).expect("small test time"),
                expected_revision,
            }),
        )
        .expect("create prompt version");
}

fn chain_retry_fixture() -> (
    TempDir,
    PromptStore,
    PromptId,
    PromptVersionId,
    PromptVersionId,
    PromptChainId,
    PromptChainLinkId,
) {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let mut store = open_store(&path);
    let prompt_id = PromptId::new();
    let first_version = PromptVersionId::new();
    let second_version = PromptVersionId::new();
    let chain_id = PromptChainId::new();
    let link_id = PromptChainLinkId::new();
    create_prompt(&mut store, prompt_id, first_version);
    create_chain(&mut store, chain_id);
    (
        dir,
        store,
        prompt_id,
        first_version,
        second_version,
        chain_id,
        link_id,
    )
}

fn retry_insert_command(
    chain_id: PromptChainId,
    link_id: PromptChainLinkId,
    prompt_id: PromptId,
    prompt_version_id: Option<PromptVersionId>,
) -> PromptChainCommand {
    PromptChainCommand::InsertPromptChainLink(InsertPromptChainLink {
        chain_id,
        link_id,
        prompt_id,
        prompt_version_id,
        before_link_id: None,
        expected_revision: 1,
    })
}

fn replace_chain_command_payload(path: &Path, command_id: CommandId, payload: Vec<u8>) {
    let command_sha256: [u8; 32] = Sha256::digest(&payload).into();
    let connection = Connection::open(path).expect("open command payload fixture database");
    connection
        .execute_batch("DROP TRIGGER prompt_chain_command_receipts_immutable_update")
        .expect("disable immutable trigger for command payload fixture");
    connection
        .execute(
            "UPDATE prompt_chain_command_receipts
             SET command_sha256 = ?1, command_payload = ?2
             WHERE command_id = ?3",
            rusqlite::params![
                command_sha256.as_slice(),
                payload,
                command_id.as_bytes().as_slice(),
            ],
        )
        .expect("write command payload fixture");
}

#[test]
fn chain_command_id_rejects_explicit_version_then_omitted_retry() {
    let (_dir, mut store, prompt_id, first_version, _second_version, chain_id, link_id) =
        chain_retry_fixture();
    let command_id = CommandId::new();

    store
        .execute_chain(
            command_id,
            retry_insert_command(chain_id, link_id, prompt_id, Some(first_version)),
        )
        .expect("insert explicit pinned version");

    let error = store
        .execute_chain(
            command_id,
            retry_insert_command(chain_id, link_id, prompt_id, None),
        )
        .expect_err("omitted version is a different original command payload");
    assert!(matches!(error, PromptStoreError::IdempotencyConflict));
}

#[test]
fn chain_command_id_rejects_omitted_version_then_explicit_retry() {
    let (_dir, mut store, prompt_id, first_version, _second_version, chain_id, link_id) =
        chain_retry_fixture();
    let command_id = CommandId::new();

    store
        .execute_chain(
            command_id,
            retry_insert_command(chain_id, link_id, prompt_id, None),
        )
        .expect("insert current version through omitted payload");

    let error = store
        .execute_chain(
            command_id,
            retry_insert_command(chain_id, link_id, prompt_id, Some(first_version)),
        )
        .expect_err("explicit version is a different original command payload");
    assert!(matches!(error, PromptStoreError::IdempotencyConflict));
}

#[test]
fn chain_command_id_rejects_different_explicit_version_retry() {
    let (_dir, mut store, prompt_id, first_version, second_version, chain_id, link_id) =
        chain_retry_fixture();
    let command_id = CommandId::new();

    store
        .execute_chain(
            command_id,
            retry_insert_command(chain_id, link_id, prompt_id, Some(first_version)),
        )
        .expect("insert first pinned version");
    prompt_version(
        &mut store,
        prompt_id,
        second_version,
        "A later immutable body.",
        1,
    );

    let error = store
        .execute_chain(
            command_id,
            retry_insert_command(chain_id, link_id, prompt_id, Some(second_version)),
        )
        .expect_err("a different explicit version is a different payload");
    assert!(matches!(error, PromptStoreError::IdempotencyConflict));
}

#[test]
fn exact_omitted_version_retry_returns_original_pinned_outcome_after_revision() {
    let (_dir, mut store, prompt_id, first_version, second_version, chain_id, link_id) =
        chain_retry_fixture();
    let command_id = CommandId::new();
    let command = retry_insert_command(chain_id, link_id, prompt_id, None);

    let original = store
        .execute_chain(command_id, command.clone())
        .expect("insert current version through omitted payload");
    prompt_version(
        &mut store,
        prompt_id,
        second_version,
        "A later immutable body.",
        1,
    );

    let retry = store
        .execute_chain(command_id, command)
        .expect("exact omitted payload remains idempotent after version advance");
    assert_eq!(retry, original);
    assert_eq!(
        PromptChainService::new(&mut store)
            .links(chain_id)
            .expect("read pinned link after retry")[0]
            .prompt_version_id(),
        first_version
    );
}

#[test]
fn exact_explicit_version_retry_returns_original_pinned_outcome_after_revision() {
    let (_dir, mut store, prompt_id, first_version, second_version, chain_id, link_id) =
        chain_retry_fixture();
    let command_id = CommandId::new();
    let command = retry_insert_command(chain_id, link_id, prompt_id, Some(first_version));

    let original = store
        .execute_chain(command_id, command.clone())
        .expect("insert explicit pinned version");
    prompt_version(
        &mut store,
        prompt_id,
        second_version,
        "A later immutable body.",
        1,
    );

    let retry = store
        .execute_chain(command_id, command)
        .expect("exact explicit payload remains idempotent after version advance");
    assert_eq!(retry, original);
    assert_eq!(
        PromptChainService::new(&mut store)
            .links(chain_id)
            .expect("read pinned link after retry")[0]
            .prompt_version_id(),
        first_version
    );
}

#[test]
fn exact_omitted_version_retry_survives_independent_reopen() {
    let (dir, mut store, prompt_id, first_version, second_version, chain_id, link_id) =
        chain_retry_fixture();
    let path = db_path(&dir);
    let command_id = CommandId::new();
    let command = retry_insert_command(chain_id, link_id, prompt_id, None);

    let original = store
        .execute_chain(command_id, command.clone())
        .expect("insert current version through omitted payload");
    prompt_version(
        &mut store,
        prompt_id,
        second_version,
        "A later immutable body.",
        1,
    );
    drop(store);

    let mut reopened = open_store(&path);
    let retry = reopened
        .execute_chain(command_id, command)
        .expect("exact omitted payload remains idempotent after reopen");
    assert_eq!(retry, original);
    assert_eq!(
        PromptChainService::new(&mut reopened)
            .links(chain_id)
            .expect("read pinned link after reopen retry")[0]
            .prompt_version_id(),
        first_version
    );
}

#[test]
fn chain_receipt_rejects_damaged_original_command_hash_on_reopen() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let mut store = open_store(&path);
    let chain_id = PromptChainId::new();
    let command_id = CommandId::new();
    let command = PromptChainCommand::CreatePromptChain(CreatePromptChain {
        chain_id,
        title: "Durable corruption chain".into(),
        description: None,
        created_at_ms: 1,
    });
    store
        .execute_chain(command_id, command.clone())
        .expect("create chain before corruption fixture");
    drop(store);

    let original_command_payload = command.encode().expect("canonical original payload");
    let corrupt_payload = rmp_serde::to_vec_named(&StrictDurableChainCommandWire {
        schema_version: 3,
        original_command_sha256: [0_u8; 32],
        original_command_payload,
        command: &command,
        resolved_prompt_version_id: None,
    })
    .expect("encode corrupt durable command");
    replace_chain_command_payload(&path, command_id, corrupt_payload);

    let error = PromptStore::open(&path).expect_err("damaged original hash must fail visibly");
    assert!(
        error
            .to_string()
            .contains("original prompt chain command hash"),
        "unexpected corruption error: {error}"
    );
}

#[test]
fn chain_receipt_rejects_unknown_durable_command_fields_on_reopen() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let mut store = open_store(&path);
    let chain_id = PromptChainId::new();
    let command_id = CommandId::new();
    let command = PromptChainCommand::CreatePromptChain(CreatePromptChain {
        chain_id,
        title: "Durable unknown-field chain".into(),
        description: None,
        created_at_ms: 1,
    });
    store
        .execute_chain(command_id, command.clone())
        .expect("create chain before unknown-field fixture");
    drop(store);

    let original_command_payload = command.encode().expect("canonical original payload");
    let original_command_sha256: [u8; 32] = Sha256::digest(&original_command_payload).into();
    let corrupt_payload = rmp_serde::to_vec_named(&UnknownStrictDurableChainCommandWire {
        schema_version: 3,
        original_command_sha256,
        original_command_payload,
        command: &command,
        resolved_prompt_version_id: None,
        unexpected: 7,
    })
    .expect("encode unknown-field durable command");
    replace_chain_command_payload(&path, command_id, corrupt_payload);

    let error = PromptStore::open(&path).expect_err("unknown durable field must fail visibly");
    assert!(
        error
            .to_string()
            .contains("prompt chain command decoding failed"),
        "unexpected corruption error: {error}"
    );
}

#[test]
fn chain_receipt_rejects_missing_original_command_fields_on_reopen() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let mut store = open_store(&path);
    let chain_id = PromptChainId::new();
    let command_id = CommandId::new();
    let command = PromptChainCommand::CreatePromptChain(CreatePromptChain {
        chain_id,
        title: "Durable missing-original chain".into(),
        description: None,
        created_at_ms: 1,
    });
    store
        .execute_chain(command_id, command.clone())
        .expect("create chain before missing-field fixture");
    drop(store);

    let corrupt_payload = rmp_serde::to_vec_named(&MissingOriginalStrictDurableChainCommandWire {
        schema_version: 3,
        command: &command,
        resolved_prompt_version_id: None,
    })
    .expect("encode missing-field durable command");
    replace_chain_command_payload(&path, command_id, corrupt_payload);

    let error = PromptStore::open(&path).expect_err("missing original fields must fail visibly");
    assert!(
        error
            .to_string()
            .contains("prompt chain command decoding failed"),
        "unexpected corruption error: {error}"
    );
}

#[test]
fn chain_receipt_rejects_trailing_original_command_bytes_on_reopen() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let mut store = open_store(&path);
    let chain_id = PromptChainId::new();
    let command_id = CommandId::new();
    let command = PromptChainCommand::CreatePromptChain(CreatePromptChain {
        chain_id,
        title: "Durable trailing-byte chain".into(),
        description: None,
        created_at_ms: 1,
    });
    store
        .execute_chain(command_id, command.clone())
        .expect("create chain before trailing-byte fixture");
    drop(store);

    let canonical_original_payload = command.encode().expect("canonical original payload");
    let original_command_sha256: [u8; 32] = Sha256::digest(&canonical_original_payload).into();
    let mut trailing_original_payload = canonical_original_payload;
    trailing_original_payload.push(0);
    let corrupt_payload = rmp_serde::to_vec_named(&StrictDurableChainCommandWire {
        schema_version: 3,
        original_command_sha256,
        original_command_payload: trailing_original_payload,
        command: &command,
        resolved_prompt_version_id: None,
    })
    .expect("encode trailing-byte durable command");
    replace_chain_command_payload(&path, command_id, corrupt_payload);

    let error = PromptStore::open(&path).expect_err("trailing original bytes must fail visibly");
    assert!(
        error
            .to_string()
            .contains("original prompt chain command payload is invalid"),
        "unexpected corruption error: {error}"
    );
}

#[test]
fn chain_accepts_any_positive_link_count() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let mut store = open_store(&path);
    let prompt_id = PromptId::new();
    let version_id = PromptVersionId::new();
    let chain_id = PromptChainId::new();
    create_prompt(&mut store, prompt_id, version_id);
    create_chain(&mut store, chain_id);

    // Exercise the public mutation path for a positive link count, then use
    // one bounded fixture transaction to reach the documented 2,000-link
    // ceiling. Building the ceiling through one-link commands would itself
    // serialize a full replacement event per append and turn this bounded
    // storage test into an O(n^2) harness.
    let first_link = PromptChainLinkId::new();
    append_link(&mut store, chain_id, first_link, prompt_id, None, 1);
    let connection = Connection::open(&path).expect("open chain fixture database");
    let transaction = connection
        .unchecked_transaction()
        .expect("start chain fixture transaction");
    let mut last_link = first_link;
    for position in 1..2_000_i64 {
        let link_id = PromptChainLinkId::new();
        last_link = link_id;
        transaction
            .execute(
                "INSERT INTO prompt_chain_links(
                    link_id, chain_id, position, prompt_id, prompt_version_id
                 ) VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    link_id.as_bytes().as_slice(),
                    chain_id.as_bytes().as_slice(),
                    position,
                    prompt_id.as_bytes().as_slice(),
                    version_id.as_bytes().as_slice(),
                ],
            )
            .expect("fill bounded chain fixture");
    }
    transaction.commit().expect("commit chain fixture");
    drop(connection);

    let links = PromptChainService::new(&mut store)
        .links(chain_id)
        .expect("list chain links");
    assert_eq!(links.len(), 2_000);
    assert_eq!(links.first().expect("first link").position(), 0);
    assert_eq!(links.last().expect("last link").position(), 1_999);

    PromptChainService::new(&mut store)
        .apply(
            CommandId::new(),
            PromptChainCommand::MovePromptChainLink(MovePromptChainLink {
                chain_id,
                link_id: last_link,
                before_link_id: Some(first_link),
                expected_revision: 2,
            }),
        )
        .expect("move must use the bounded temporary-position offset");
    let moved = PromptChainService::new(&mut store)
        .links(chain_id)
        .expect("list moved maximum chain");
    assert_eq!(moved.len(), 2_000);
    assert_eq!(moved.first().expect("moved first link").id(), last_link);
    assert_eq!(moved.get(1).expect("moved second link").id(), first_link);
    assert!(
        moved
            .iter()
            .enumerate()
            .all(|(position, link)| link.position() == position as u32),
        "maximum chain remains dense after one SQL interval shift"
    );
}

#[test]
fn chain_position_u32_boundary_fails_closed_without_mutation() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let mut store = open_store(&path);
    let prompt_id = PromptId::new();
    let version_id = PromptVersionId::new();
    let chain_id = PromptChainId::new();
    let link_id = PromptChainLinkId::new();
    create_prompt(&mut store, prompt_id, version_id);
    create_chain(&mut store, chain_id);
    append_link(&mut store, chain_id, link_id, prompt_id, None, 1);
    let connection = Connection::open(&path).expect("open position boundary database");
    connection
        .execute(
            "UPDATE prompt_chain_links SET position = ?1 WHERE link_id = ?2",
            rusqlite::params![i64::from(u32::MAX) + 1, link_id.as_bytes().as_slice()],
        )
        .expect("write out-of-range position fixture");
    drop(connection);

    let error = PromptChainService::new(&mut store)
        .links(chain_id)
        .expect_err("positions beyond u32 must fail closed");
    assert!(matches!(error, PromptStoreError::Corruption(_)));
}

#[test]
fn chain_revision_i64_boundary_rejects_next_revision_without_mutation() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let mut store = open_store(&path);
    let prompt_id = PromptId::new();
    let version_id = PromptVersionId::new();
    let chain_id = PromptChainId::new();
    create_prompt(&mut store, prompt_id, version_id);
    create_chain(&mut store, chain_id);
    let connection = Connection::open(&path).expect("open revision boundary database");
    connection
        .execute(
            "UPDATE prompt_chains SET revision = ?1 WHERE chain_id = ?2",
            rusqlite::params![i64::MAX, chain_id.as_bytes().as_slice()],
        )
        .expect("write maximum revision fixture");
    drop(connection);

    let error = PromptChainService::new(&mut store)
        .apply(
            CommandId::new(),
            PromptChainCommand::RenamePromptChain(devmanager::prompts::RenamePromptChain {
                chain_id,
                title: "Revision boundary".into(),
                expected_revision: i64::MAX as u64,
            }),
        )
        .expect_err("next revision must reject SQLite integer overflow");
    assert!(matches!(error, PromptStoreError::Corruption(_)));
    let revision: i64 = Connection::open(&path)
        .expect("reopen revision boundary database")
        .query_row(
            "SELECT revision FROM prompt_chains WHERE chain_id = ?1",
            [chain_id.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("load unchanged maximum revision");
    assert_eq!(revision, i64::MAX);
}

#[test]
fn append_pins_exact_current_version() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let mut store = open_store(&path);
    let prompt_id = PromptId::new();
    let first_version = PromptVersionId::new();
    let second_version = PromptVersionId::new();
    let chain_id = PromptChainId::new();
    create_prompt(&mut store, prompt_id, first_version);
    create_chain(&mut store, chain_id);

    append_link(
        &mut store,
        chain_id,
        PromptChainLinkId::new(),
        prompt_id,
        None,
        1,
    );
    prompt_version(&mut store, prompt_id, second_version, "The newer body.", 1);
    append_link(
        &mut store,
        chain_id,
        PromptChainLinkId::new(),
        prompt_id,
        None,
        2,
    );

    let links = PromptChainService::new(&mut store)
        .links(chain_id)
        .expect("list chain links");
    assert_eq!(links[0].prompt_version_id(), first_version);
    assert_eq!(links[1].prompt_version_id(), second_version);
}

#[test]
fn insert_between_shifts_positions_atomically() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let mut store = open_store(&path);
    let prompt_id = PromptId::new();
    let version_id = PromptVersionId::new();
    let chain_id = PromptChainId::new();
    let first_link = PromptChainLinkId::new();
    let second_link = PromptChainLinkId::new();
    create_prompt(&mut store, prompt_id, version_id);
    create_chain(&mut store, chain_id);
    append_link(&mut store, chain_id, first_link, prompt_id, None, 1);
    append_link(&mut store, chain_id, second_link, prompt_id, None, 2);

    let inserted_link = PromptChainLinkId::new();
    let before_events = store.count_chain_events().expect("count chain events");
    PromptChainService::new(&mut store)
        .apply(
            CommandId::new(),
            PromptChainCommand::InsertPromptChainLink(InsertPromptChainLink {
                chain_id,
                link_id: inserted_link,
                prompt_id,
                prompt_version_id: Some(version_id),
                before_link_id: Some(second_link),
                expected_revision: 3,
            }),
        )
        .expect("insert before second link");

    let links = PromptChainService::new(&mut store)
        .links(chain_id)
        .expect("list chain links");
    assert_eq!(
        links.iter().map(|link| link.id()).collect::<Vec<_>>(),
        vec![first_link, inserted_link, second_link]
    );
    assert_eq!(
        links.iter().map(|link| link.position()).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    let context = PromptChainService::new(&mut store)
        .link_context(chain_id, inserted_link)
        .expect("read inserted link context")
        .expect("inserted link");
    assert_eq!(context.previous_link_id, Some(first_link));
    assert_eq!(context.next_link_id, Some(second_link));
    assert_eq!(
        store.count_chain_events().expect("count chain events"),
        before_events + 1
    );
    assert_eq!(
        PromptChainService::new(&mut store)
            .chain(chain_id)
            .expect("load chain")
            .expect("chain")
            .revision,
        4
    );
}

#[test]
fn reorder_is_dense_and_stable() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let mut store = open_store(&path);
    let prompt_id = PromptId::new();
    let version_id = PromptVersionId::new();
    let chain_id = PromptChainId::new();
    create_prompt(&mut store, prompt_id, version_id);
    create_chain(&mut store, chain_id);
    let links = (0..4).map(|_| PromptChainLinkId::new()).collect::<Vec<_>>();
    for (index, link_id) in links.iter().copied().enumerate() {
        append_link(
            &mut store,
            chain_id,
            link_id,
            prompt_id,
            None,
            u64::try_from(index + 1).expect("small test revision"),
        );
    }

    PromptChainService::new(&mut store)
        .apply(
            CommandId::new(),
            PromptChainCommand::MovePromptChainLink(MovePromptChainLink {
                chain_id,
                link_id: links[2],
                before_link_id: Some(links[1]),
                expected_revision: 5,
            }),
        )
        .expect("move link");

    let reordered = PromptChainService::new(&mut store)
        .links(chain_id)
        .expect("list reordered links");
    assert_eq!(
        reordered.iter().map(|link| link.id()).collect::<Vec<_>>(),
        vec![links[0], links[2], links[1], links[3]]
    );
    assert_eq!(
        reordered
            .iter()
            .map(|link| link.position())
            .collect::<Vec<_>>(),
        vec![0, 1, 2, 3]
    );
}

#[test]
fn remove_compacts_positions() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let mut store = open_store(&path);
    let prompt_id = PromptId::new();
    let version_id = PromptVersionId::new();
    let chain_id = PromptChainId::new();
    create_prompt(&mut store, prompt_id, version_id);
    create_chain(&mut store, chain_id);
    let links = (0..3).map(|_| PromptChainLinkId::new()).collect::<Vec<_>>();
    for (index, link_id) in links.iter().copied().enumerate() {
        append_link(
            &mut store,
            chain_id,
            link_id,
            prompt_id,
            None,
            u64::try_from(index + 1).expect("small test revision"),
        );
    }

    PromptChainService::new(&mut store)
        .apply(
            CommandId::new(),
            PromptChainCommand::RemovePromptChainLink(RemovePromptChainLink {
                chain_id,
                link_id: links[1],
                expected_revision: 4,
            }),
        )
        .expect("remove middle link");

    let remaining = PromptChainService::new(&mut store)
        .links(chain_id)
        .expect("list remaining links");
    assert_eq!(
        remaining.iter().map(|link| link.id()).collect::<Vec<_>>(),
        vec![links[0], links[2]]
    );
    assert_eq!(
        remaining
            .iter()
            .map(|link| link.position())
            .collect::<Vec<_>>(),
        vec![0, 1]
    );
}

#[test]
fn archived_prompt_link_remains_readable() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let mut store = open_store(&path);
    let prompt_id = PromptId::new();
    let version_id = PromptVersionId::new();
    let chain_id = PromptChainId::new();
    let link_id = PromptChainLinkId::new();
    create_prompt(&mut store, prompt_id, version_id);
    create_chain(&mut store, chain_id);
    append_link(&mut store, chain_id, link_id, prompt_id, None, 1);

    store
        .execute(
            CommandId::new(),
            PromptCommand::ArchivePrompt(ArchivePrompt {
                prompt_id,
                archived_at_ms: 2,
                expected_revision: 1,
            }),
        )
        .expect("archive prompt");

    let version = PromptChainService::new(&mut store)
        .version(version_id)
        .expect("read pinned version")
        .expect("version remains readable");
    assert_eq!(version.body, "Inspect the selected change.");
    assert_eq!(
        PromptChainService::new(&mut store)
            .links(chain_id)
            .expect("read chain links")[0]
            .id(),
        link_id
    );
}

#[test]
fn new_prompt_version_does_not_mutate_link() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let mut store = open_store(&path);
    let prompt_id = PromptId::new();
    let first_version = PromptVersionId::new();
    let second_version = PromptVersionId::new();
    let chain_id = PromptChainId::new();
    let link_id = PromptChainLinkId::new();
    create_prompt(&mut store, prompt_id, first_version);
    create_chain(&mut store, chain_id);
    append_link(&mut store, chain_id, link_id, prompt_id, None, 1);
    prompt_version(
        &mut store,
        prompt_id,
        second_version,
        "A later immutable body.",
        1,
    );

    let context = PromptChainService::new(&mut store)
        .link_context(chain_id, link_id)
        .expect("read link context")
        .expect("link");
    assert_eq!(context.link.prompt_version_id(), first_version);
    assert!(context.update_available);
}

#[test]
fn explicit_update_link_uses_current_version() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let mut store = open_store(&path);
    let prompt_id = PromptId::new();
    let first_version = PromptVersionId::new();
    let second_version = PromptVersionId::new();
    let chain_id = PromptChainId::new();
    let link_id = PromptChainLinkId::new();
    create_prompt(&mut store, prompt_id, first_version);
    create_chain(&mut store, chain_id);
    append_link(&mut store, chain_id, link_id, prompt_id, None, 1);
    prompt_version(
        &mut store,
        prompt_id,
        second_version,
        "The current body.",
        1,
    );

    PromptChainService::new(&mut store)
        .apply(
            CommandId::new(),
            PromptChainCommand::UpdatePromptChainLinkVersion(UpdatePromptChainLinkVersion {
                chain_id,
                link_id,
                expected_revision: 2,
            }),
        )
        .expect("explicitly update pinned version");

    assert_eq!(
        PromptChainService::new(&mut store)
            .links(chain_id)
            .expect("list links")[0]
            .prompt_version_id(),
        second_version
    );
}

#[test]
fn revision_conflict_changes_nothing() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let mut store = open_store(&path);
    let prompt_id = PromptId::new();
    let version_id = PromptVersionId::new();
    let chain_id = PromptChainId::new();
    create_prompt(&mut store, prompt_id, version_id);
    create_chain(&mut store, chain_id);
    append_link(
        &mut store,
        chain_id,
        PromptChainLinkId::new(),
        prompt_id,
        None,
        1,
    );
    let before_links = PromptChainService::new(&mut store)
        .links(chain_id)
        .expect("list links");
    let before_chain = PromptChainService::new(&mut store)
        .chain(chain_id)
        .expect("load chain")
        .expect("chain");
    let before_events = store.count_chain_events().expect("count events");

    let error = PromptChainService::new(&mut store)
        .apply(
            CommandId::new(),
            PromptChainCommand::InsertPromptChainLink(InsertPromptChainLink {
                chain_id,
                link_id: PromptChainLinkId::new(),
                prompt_id,
                prompt_version_id: None,
                before_link_id: None,
                expected_revision: 1,
            }),
        )
        .expect_err("stale revision must be rejected");
    assert!(matches!(
        error,
        PromptStoreError::RevisionConflict {
            expected: 1,
            actual: 2
        }
    ));

    assert_eq!(
        PromptChainService::new(&mut store)
            .links(chain_id)
            .expect("list links after conflict"),
        before_links
    );
    assert_eq!(
        PromptChainService::new(&mut store)
            .chain(chain_id)
            .expect("load chain after conflict")
            .expect("chain after conflict"),
        before_chain
    );
    assert_eq!(
        store.count_chain_events().expect("count events"),
        before_events
    );
}

#[test]
fn overlapping_independent_connections_insert_conflict_leaves_one_ordered_result() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt_id = PromptId::new();
    let version_id = PromptVersionId::new();
    let chain_id = PromptChainId::new();
    let mut seed_store = open_store(&path);
    create_prompt(&mut seed_store, prompt_id, version_id);
    create_chain(&mut seed_store, chain_id);
    drop(seed_store);

    let first_link = PromptChainLinkId::new();
    let second_link = PromptChainLinkId::new();
    let barrier = Arc::new(Barrier::new(2));
    let first_path = path.clone();
    let first_barrier = Arc::clone(&barrier);
    let first_thread = thread::spawn(move || {
        let mut store = open_store(&first_path);
        first_barrier.wait();
        PromptChainService::new(&mut store).apply(
            CommandId::new(),
            PromptChainCommand::InsertPromptChainLink(InsertPromptChainLink {
                chain_id,
                link_id: first_link,
                prompt_id,
                prompt_version_id: None,
                before_link_id: None,
                expected_revision: 1,
            }),
        )
    });
    let second_path = path.clone();
    let second_barrier = Arc::clone(&barrier);
    let second_thread = thread::spawn(move || {
        let mut store = open_store(&second_path);
        second_barrier.wait();
        PromptChainService::new(&mut store).apply(
            CommandId::new(),
            PromptChainCommand::InsertPromptChainLink(InsertPromptChainLink {
                chain_id,
                link_id: second_link,
                prompt_id,
                prompt_version_id: None,
                before_link_id: None,
                expected_revision: 1,
            }),
        )
    });
    let first_result = first_thread.join().expect("first insert thread");
    let second_result = second_thread.join().expect("second insert thread");
    let results = [first_result, second_result];
    assert_eq!(
        results.iter().filter(|result| result.is_ok()).count(),
        1,
        "one overlapping insert must commit"
    );
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(PromptStoreError::RevisionConflict { .. })))
            .count(),
        1,
        "the other overlapping insert must report a revision conflict"
    );
    let mut verify_store = open_store(&path);
    let winning_link = PromptChainService::new(&mut verify_store)
        .links(chain_id)
        .expect("list winning links");
    assert_eq!(
        winning_link.len(),
        1,
        "overlapping inserts must leave one link"
    );
    assert!(winning_link[0].id() == first_link || winning_link[0].id() == second_link);
}

#[test]
fn overlapping_independent_connections_move_conflict_leaves_one_ordered_result() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let prompt_id = PromptId::new();
    let version_id = PromptVersionId::new();
    let chain_id = PromptChainId::new();
    let mut seed_store = open_store(&path);
    create_prompt(&mut seed_store, prompt_id, version_id);
    create_chain(&mut seed_store, chain_id);
    let first_link = PromptChainLinkId::new();
    let second_link = PromptChainLinkId::new();
    let third_link = PromptChainLinkId::new();
    append_link(&mut seed_store, chain_id, first_link, prompt_id, None, 1);
    append_link(&mut seed_store, chain_id, second_link, prompt_id, None, 2);
    append_link(&mut seed_store, chain_id, third_link, prompt_id, None, 3);
    drop(seed_store);

    let barrier = Arc::new(Barrier::new(2));
    let first_path = path.clone();
    let first_barrier = Arc::clone(&barrier);
    let first_thread = thread::spawn(move || {
        let mut store = open_store(&first_path);
        first_barrier.wait();
        PromptChainService::new(&mut store).apply(
            CommandId::new(),
            PromptChainCommand::MovePromptChainLink(MovePromptChainLink {
                chain_id,
                link_id: first_link,
                before_link_id: Some(third_link),
                expected_revision: 4,
            }),
        )
    });
    let second_path = path.clone();
    let second_barrier = Arc::clone(&barrier);
    let second_thread = thread::spawn(move || {
        let mut store = open_store(&second_path);
        second_barrier.wait();
        PromptChainService::new(&mut store).apply(
            CommandId::new(),
            PromptChainCommand::MovePromptChainLink(MovePromptChainLink {
                chain_id,
                link_id: third_link,
                before_link_id: Some(first_link),
                expected_revision: 4,
            }),
        )
    });
    let first_result = first_thread.join().expect("first move thread");
    let second_result = second_thread.join().expect("second move thread");
    let results = [first_result, second_result];
    assert_eq!(
        results.iter().filter(|result| result.is_ok()).count(),
        1,
        "one overlapping move must commit"
    );
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(PromptStoreError::RevisionConflict { .. })))
            .count(),
        1,
        "the other overlapping move must report a revision conflict"
    );
    let mut verify_store = open_store(&path);
    let order = PromptChainService::new(&mut verify_store)
        .links(chain_id)
        .expect("list moved links")
        .iter()
        .map(|link| link.id())
        .collect::<Vec<_>>();
    assert!(
        order == vec![second_link, first_link, third_link]
            || order == vec![third_link, first_link, second_link],
        "winning move must determine the complete dense order: {order:?}"
    );
}

#[test]
fn chain_reopen_preserves_dense_truth() {
    let dir = TempDir::new().expect("unique temp dir");
    let path = db_path(&dir);
    let mut store = open_store(&path);
    let prompt_id = PromptId::new();
    let version_id = PromptVersionId::new();
    let chain_id = PromptChainId::new();
    create_prompt(&mut store, prompt_id, version_id);
    create_chain(&mut store, chain_id);
    for index in 0..4 {
        append_link(
            &mut store,
            chain_id,
            PromptChainLinkId::new(),
            prompt_id,
            None,
            u64::try_from(index + 1).expect("small test revision"),
        );
    }
    let expected = PromptChainService::new(&mut store)
        .links(chain_id)
        .expect("list links before reopen");
    drop(store);

    let mut reopened = open_store(&path);
    reopened
        .rebuild_projection()
        .expect("rebuild projection after reopen");
    let actual = PromptChainService::new(&mut reopened)
        .links(chain_id)
        .expect("list links after rebuild");
    assert_eq!(actual, expected);
}

#[test]
fn chain_has_no_execute_or_advance_command() {
    fn manual_only(command: &PromptChainCommand) -> bool {
        match command {
            PromptChainCommand::CreatePromptChain(_)
            | PromptChainCommand::RenamePromptChain(_)
            | PromptChainCommand::InsertPromptChainLink(_)
            | PromptChainCommand::MovePromptChainLink(_)
            | PromptChainCommand::RemovePromptChainLink(_)
            | PromptChainCommand::UpdatePromptChainLinkVersion(_)
            | PromptChainCommand::ArchivePromptChain(_)
            | PromptChainCommand::RestorePromptChain(_) => true,
        }
    }

    assert!(manual_only(&PromptChainCommand::CreatePromptChain(
        CreatePromptChain {
            chain_id: PromptChainId::new(),
            title: "Manual".into(),
            description: None,
            created_at_ms: 1,
        }
    )));
    let type_name = std::any::type_name::<PromptChainCommand>();
    for forbidden in [
        "Execute",
        "Advance",
        "Cursor",
        "Scheduler",
        "Branch",
        "Loop",
    ] {
        assert!(
            !type_name.contains(forbidden),
            "forbidden chain concept: {forbidden}"
        );
    }
}
