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
use tempfile::TempDir;

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
