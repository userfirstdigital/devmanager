#[path = "../src/workspace/files.rs"]
mod files;

use files::{
    ContentKind, ExpectedRevision, FilePageRequest, FileServiceError, LinePageRequest, ReadOptions,
    SearchOptions, SecretClassification, WorkspaceFileService, MAX_CHUNK_BYTES,
    MAX_CONCURRENT_OPERATIONS, MAX_DIRECTORY_IDENTITIES, MAX_LINE_COUNT, MAX_MUTATION_LOCKS,
    MAX_PAGE_SIZE, MAX_READ_BYTES, MAX_SEARCH_DEPTH, TEST_OPERATION_EXPIRED_ENTRY,
    TEST_OPERATION_EXPIRED_MID,
};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;
use std::sync::{Arc, Barrier};
use std::thread;
use tempfile::TempDir;

const HELLO_FIXTURE: &[u8] = include_bytes!("fixtures/files/text/hello.txt");
const SECRET_FIXTURE: &[u8] = include_bytes!("fixtures/files/secret/.env.example");

fn service_root() -> (TempDir, WorkspaceFileService) {
    let temp = tempfile::tempdir().expect("create workspace root");
    let service =
        WorkspaceFileService::new_for_test(temp.path()).expect("bind canonical workspace root");
    (temp, service)
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

#[cfg(test)]
fn wait_for_test_pause() {
    for _ in 0..5_000 {
        if files::test_pause_ready() {
            return;
        }
        thread::sleep(std::time::Duration::from_millis(1));
    }
    files::clear_test_pause();
    panic!("file-service test pause did not become ready");
}

#[test]
fn linux_atomic_replace_does_not_use_anchor_bindings_before_declaration() {
    let source = include_str!("../src/workspace/files.rs");
    let before_anchor_declaration = source
        .split_once("let old_tombstone_name =")
        .map(|(before, _)| before)
        .expect("Linux exchange branch declares its anchor");
    assert!(
        !before_anchor_declaration.contains("old_tombstone_name,"),
        "the Linux branch must not reference its tombstone bindings before declaration"
    );
}

#[test]
fn nonmutation_operations_share_the_callers_operation_budget() {
    let (temp, service) = service_root();
    fs::write(temp.path().join("budget.txt"), b"budget").expect("write budget fixture");
    service.set_test_budget_mode(TEST_OPERATION_EXPIRED_ENTRY);

    assert!(matches!(
        service.list(None, 8),
        Err(FileServiceError::DeadlineExceeded)
    ));
    assert!(matches!(
        service.read("budget.txt", ReadOptions::default()),
        Err(FileServiceError::DeadlineExceeded)
    ));
    assert!(matches!(
        service.search("budget", SearchOptions::default()),
        Err(FileServiceError::DeadlineExceeded)
    ));
}

#[test]
fn list_returns_sorted_metadata_only_and_explicit_overflow() {
    let (temp, service) = service_root();
    fs::write(temp.path().join("z.txt"), b"text").expect("write text fixture");
    fs::write(temp.path().join("a.bin"), [0_u8, 1, 2]).expect("write binary fixture");
    fs::create_dir(temp.path().join("middle")).expect("create directory fixture");

    let entries = service
        .list(None, 8)
        .expect("bounded metadata listing should succeed");
    let names = entries
        .iter()
        .map(|entry| entry.path.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, ["a.bin", "middle", "z.txt"]);
    assert_eq!(entries[0].byte_len, Some(3));
    assert!(entries[0].content_kind.is_none());

    fs::write(temp.path().join("overflow"), b"x").expect("write overflow fixture");
    assert!(matches!(
        service.list(None, 3),
        Err(FileServiceError::ListOverflow { limit: 3 })
    ));
}

#[test]
fn private_cleanup_names_are_hidden_from_public_file_operations() {
    let (temp, service) = service_root();
    let parent_identity = service.root_identity_for_test();

    let temporary_path = temp.path().join("staged-source.tmp");
    fs::write(&temporary_path, b"needle in private temporary").expect("write temporary fixture");
    let temporary_file = fs::File::open(&temporary_path).expect("open temporary fixture");
    let temporary_name =
        files::test_temporary_name(parent_identity, None, &temporary_file, [1_u8; 16]);
    let temporary = temp.path().join(&temporary_name);
    fs::rename(&temporary_path, &temporary).expect("install private temporary name");

    let tombstone_path = temp.path().join("staged-tombstone.old");
    fs::write(&tombstone_path, b"needle in private tombstone").expect("write tombstone fixture");
    let tombstone_identity = service
        .read("staged-tombstone.old", ReadOptions::default())
        .expect("read tombstone identity")
        .revision
        .fingerprint
        .identity;
    let tombstone_name =
        files::test_tombstone_name(parent_identity, tombstone_identity, [2_u8; 16]);
    let tombstone = temp.path().join(&tombstone_name);
    fs::rename(&tombstone_path, &tombstone).expect("install private tombstone name");

    let entries = service
        .list(None, files::MAX_LIST_ENTRIES)
        .expect("public listing should remain usable");
    assert!(!entries.iter().any(
        |entry| entry.path.as_str() == temporary_name || entry.path.as_str() == tombstone_name
    ));
    assert!(matches!(
        service.read(&temporary_name, ReadOptions::default()),
        Err(FileServiceError::NotFound { .. })
    ));
    assert!(matches!(
        service.search("needle", SearchOptions::default()),
        Ok(result) if result.matches.is_empty() && result.scanned_files == 0
    ));
}

#[cfg(windows)]
#[test]
fn windows_uppercase_private_cleanup_names_are_hidden_redacted_and_unmutable() {
    let (temp, service) = service_root();
    let parent_identity = service.root_identity_for_test();
    let source_path = temp.path().join("uppercase-private-source.txt");
    fs::write(&source_path, b"private uppercase needle").expect("write private fixture");
    let private_name = {
        let file = fs::File::open(&source_path).expect("open private fixture");
        files::test_temporary_name(parent_identity, None, &file, [3_u8; 16])
    };
    let uppercase_name = private_name.to_ascii_uppercase();
    assert_ne!(private_name, uppercase_name);
    fs::rename(&source_path, temp.path().join(&uppercase_name))
        .expect("install uppercase private fixture");

    let entries = service
        .list(None, files::MAX_LIST_ENTRIES)
        .expect("listing should remain usable");
    assert!(!entries
        .iter()
        .any(|entry| entry.path.as_str() == uppercase_name));

    let read_error = service
        .read(&uppercase_name, ReadOptions::default())
        .expect_err("uppercase private name must not be readable");
    assert!(matches!(
        &read_error,
        FileServiceError::NotFound { path } if path == "<path-redacted>"
    ));
    assert!(!read_error.to_string().contains(&uppercase_name));

    let write_error = service
        .plan_write(
            &uppercase_name,
            b"replacement".to_vec(),
            ExpectedRevision::missing(),
        )
        .expect_err("uppercase private name must not be writable");
    assert!(matches!(
        &write_error,
        FileServiceError::NotFound { path } if path == "<path-redacted>"
    ));

    let delete_error = service
        .plan_delete(&uppercase_name, ExpectedRevision::missing())
        .expect_err("uppercase private name must not be deletable");
    assert!(matches!(
        &delete_error,
        FileServiceError::NotFound { path } if path == "<path-redacted>"
    ));

    assert!(matches!(
        service.search("uppercase", SearchOptions::default()),
        Ok(result) if result.matches.is_empty() && result.scanned_files == 0
    ));
}

#[test]
fn private_cleanup_parsers_case_fold_reserved_markers() {
    let source = include_str!("../src/workspace/files.rs");
    assert!(
        source.contains("fn strip_ascii_case_insensitive_prefix")
            && source.contains("eq_ignore_ascii_case"),
        "reserved cleanup syntax must be parsed with ASCII case folding"
    );
    for parser in [
        "fn parse_tombstone_binding(",
        "fn parse_temporary_binding(",
        "fn parse_authority_entry_binding(",
    ] {
        let start = source.find(parser).expect("private-name parser exists");
        let body_end = source[start + parser.len()..]
            .find("\nfn ")
            .map(|offset| start + parser.len() + offset)
            .unwrap_or(source.len());
        let body = &source[start..body_end];
        assert!(
            body.contains("strip_ascii_case_insensitive_prefix")
                && body.contains("strip_ascii_case_insensitive_suffix"),
            "{parser} must case-fold both reserved prefix and suffix"
        );
    }
}

#[test]
fn physical_private_entries_count_toward_list_and_search_overflow() {
    let (temp, service) = service_root();
    let parent_identity = service.root_identity_for_test();
    for index in 0..=files::MAX_LIST_ENTRIES {
        let source_path = temp.path().join(format!("private-source-{index}.tmp"));
        fs::write(&source_path, b"private physical entry").expect("write private entry");
        let private_name = {
            let file = fs::File::open(&source_path).expect("open private entry");
            let mut nonce = [0_u8; 16];
            nonce[..8].copy_from_slice(&(index as u64).to_le_bytes());
            files::test_temporary_name(parent_identity, None, &file, nonce)
        };
        fs::rename(&source_path, temp.path().join(private_name)).expect("install private entry");
    }
    fs::write(
        temp.path().join("public-after-private.txt"),
        b"public needle",
    )
    .expect("write public entry after private entries");

    assert!(matches!(
        service.list(None, files::MAX_LIST_ENTRIES),
        Err(FileServiceError::ListOverflow {
            limit: files::MAX_LIST_ENTRIES
        })
    ));
    assert!(matches!(
        service.search("needle", SearchOptions::default()),
        Err(FileServiceError::ListOverflow {
            limit: files::MAX_LIST_ENTRIES
        })
    ));
}

#[test]
fn strict_relative_paths_reject_windows_escape_and_alias_forms() {
    let (_temp, service) = service_root();
    for path in [
        "",
        ".",
        "..",
        "child/../file.txt",
        "child/./file.txt",
        "/absolute.txt",
        "\\absolute.txt",
        "C:relative.txt",
        "C:\\absolute.txt",
        "\\\\server\\share\\file.txt",
        "CON.txt",
        "nested\\NUL",
        "CONIN$",
        "CONOUT$",
        "secret.txt:stream",
        "trailing.",
        "trailing ",
        "bad\u{0001}name",
        "café.txt",
        "e\u{301}.txt",
        "\\\\?\\C:\\device.txt",
    ] {
        assert!(
            service.normalize_relative_path(path).is_err(),
            "path must be rejected: {path:?}"
        );
    }
    assert_eq!(
        service
            .normalize_relative_path("nested\\safe.txt")
            .expect("valid path")
            .as_str(),
        "nested/safe.txt"
    );
}

#[test]
fn read_emits_forward_chunks_and_current_sha256_fingerprint() {
    let (temp, service) = service_root();
    let body = (0..97)
        .map(|index| b'a' + (index % 26) as u8)
        .collect::<Vec<_>>();
    fs::write(temp.path().join("sample.txt"), &body).expect("write text fixture");

    let result = service
        .read(
            "sample.txt",
            ReadOptions {
                chunk_bytes: 7,
                total_bytes: 128,
            },
        )
        .expect("bounded read should succeed");
    assert_eq!(result.content_kind, ContentKind::Text);
    assert_eq!(result.total_bytes, body.len() as u64);
    assert_eq!(result.revision.sha256, Some(sha256(&body)));
    assert!(result.revision.fingerprint.byte_len == body.len() as u64);

    let mut reconstructed = Vec::new();
    for (expected_offset, chunk) in result.chunks.iter().enumerate() {
        assert_eq!(chunk.offset, (expected_offset * 7) as u64);
        assert!(chunk.bytes.len() <= 7);
        reconstructed.extend_from_slice(&chunk.bytes);
    }
    assert_eq!(reconstructed, body);
}

#[test]
fn binary_read_is_classified_and_total_cap_is_explicit() {
    let (temp, service) = service_root();
    fs::write(temp.path().join("sample.bin"), [b't', 0, 1, 2]).expect("write binary fixture");
    let result = service
        .read("sample.bin", ReadOptions::default())
        .expect("binary read should succeed");
    assert_eq!(result.content_kind, ContentKind::Binary);

    let body = vec![b'x'; MAX_READ_BYTES + 1];
    fs::write(temp.path().join("large.txt"), body).expect("write large fixture");
    assert!(matches!(
        service.read(
            "large.txt",
            ReadOptions {
                chunk_bytes: MAX_CHUNK_BYTES,
                total_bytes: MAX_READ_BYTES,
            }
        ),
        Err(FileServiceError::ReadLimitExceeded {
            limit: MAX_READ_BYTES
        })
    ));
}

#[test]
fn write_plan_is_atomic_and_execute_revalidates_expected_revision() {
    let (temp, service) = service_root();
    let target = temp.path().join("atomic.txt");
    fs::write(&target, b"before").expect("write original");
    let original = service
        .read("atomic.txt", ReadOptions::default())
        .expect("read original");

    let plan = service
        .plan_write(
            "atomic.txt",
            b"after-complete".to_vec(),
            ExpectedRevision::exact(original.revision.clone()),
        )
        .expect("plan write");
    fs::write(&target, b"external-change").expect("simulate concurrent edit");
    assert!(matches!(
        service.execute_write(plan),
        Err(FileServiceError::Conflict { .. })
    ));
    assert_eq!(
        fs::read(&target).expect("read unchanged target"),
        b"external-change"
    );

    let current = service
        .read("atomic.txt", ReadOptions::default())
        .expect("read current target");
    let plan = service
        .plan_write(
            "atomic.txt",
            b"after-complete".to_vec(),
            ExpectedRevision::exact(current.revision),
        )
        .expect("plan second write");
    if let Err(error) = service.execute_write(plan) {
        panic!("atomic write: {error}");
    }
    assert_eq!(
        fs::read(&target).expect("read replaced target"),
        b"after-complete"
    );
    assert!(fs::read_dir(temp.path())
        .expect("list sibling temp files")
        .all(|entry| entry.expect("read entry").file_name() != ".devmanager-file.tmp"));

    let current = service
        .read("atomic.txt", ReadOptions::default())
        .expect("read fingerprint-only target");
    let plan = service
        .plan_write(
            "atomic.txt",
            b"fingerprint-only".to_vec(),
            ExpectedRevision::fingerprint(current.revision.fingerprint),
        )
        .expect("plan fingerprint-only write");
    service
        .execute_write(plan)
        .expect("execute fingerprint-only write");
}

#[test]
fn missing_write_expectation_never_overwrites_a_new_target() {
    let (temp, service) = service_root();
    let plan = service
        .plan_write("new.txt", b"planned".to_vec(), ExpectedRevision::missing())
        .expect("plan missing target");
    fs::write(temp.path().join("new.txt"), b"created-after-preview")
        .expect("create target after preview");
    assert!(matches!(
        service.execute_write(plan),
        Err(FileServiceError::Conflict { .. })
    ));
    assert_eq!(
        fs::read(temp.path().join("new.txt")).expect("read target"),
        b"created-after-preview"
    );
}

#[test]
fn temporary_path_replacement_is_rejected_before_commit() {
    let (temp, service) = service_root();
    let service = Arc::new(service);
    let plan = service
        .plan_write(
            "bound-temp.txt",
            b"planned".to_vec(),
            ExpectedRevision::missing(),
        )
        .expect("plan missing target");
    files::set_test_pause(files::TEST_PAUSE_BEFORE_RENAME);
    let worker = {
        let service = Arc::clone(&service);
        thread::spawn(move || service.execute_write(plan))
    };
    wait_for_test_pause();
    let temporary = fs::read_dir(temp.path())
        .expect("list private temporary files")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".devmanager-file-"))
        })
        .expect("write should hold a private temporary path");
    let moved = temporary.with_extension("attacker");
    fs::rename(&temporary, &moved).expect("move held temporary away");
    fs::write(&temporary, b"attacker-temp").expect("replace temporary pathname");
    files::clear_test_pause();
    assert!(matches!(
        worker.join().expect("join write worker"),
        Err(FileServiceError::Conflict { .. })
    ));
    assert_eq!(
        fs::read(&temporary).expect("attacker temporary replacement survives"),
        b"attacker-temp"
    );
    let _ = fs::remove_file(moved);
}

#[cfg(target_os = "linux")]
#[test]
fn post_exchange_same_name_replacement_is_not_clobbered() {
    let (temp, service) = service_root();
    let target = temp.path().join("exchange-race.txt");
    fs::write(&target, b"before").expect("write exchange target");
    let revision = service
        .read("exchange-race.txt", ReadOptions::default())
        .expect("read exchange target")
        .revision;
    let service = Arc::new(service);
    let plan = service
        .plan_write(
            "exchange-race.txt",
            b"planned".to_vec(),
            ExpectedRevision::exact(revision),
        )
        .expect("plan exchange write");
    files::set_test_pause(files::TEST_PAUSE_AFTER_EXCHANGE);
    let worker = {
        let service = Arc::clone(&service);
        thread::spawn(move || service.execute_write(plan))
    };
    wait_for_test_pause();
    let temporary = fs::read_dir(temp.path())
        .expect("list exchanged temporary files")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".devmanager-file-"))
        })
        .expect("exchange should retain the private old-name candidate");
    let temporary_backup = temporary.with_extension("old-attacker");
    fs::rename(&temporary, &temporary_backup).expect("move old exchange candidate");
    fs::write(&temporary, b"attacker-old").expect("install old-name replacement");
    let moved = temp.path().join("exchange-attacker.tmp");
    fs::rename(&target, &moved).expect("move exchanged destination");
    fs::write(&target, b"attacker-destination").expect("install same-name attacker");
    files::clear_test_pause();
    assert!(worker.join().expect("join exchange worker").is_err());
    assert_eq!(
        fs::read(&target).expect("same-name attacker survives"),
        b"attacker-destination"
    );
    assert_eq!(
        fs::read(&temporary).expect("old-name attacker survives"),
        b"attacker-old"
    );
    let _restarted = WorkspaceFileService::new_for_test(temp.path())
        .expect("restart should recover the exact hard-linked tombstone");
    assert!(
        fs::read_dir(temp.path())
            .expect("list after tombstone recovery")
            .filter_map(Result::ok)
            .all(|entry| {
                !entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with(".devmanager-tombstone-"))
            }),
        "restart must remove only the generated tombstone, not the moved old inode"
    );
    assert!(
        temporary_backup.exists(),
        "the old inode's user-visible link survives"
    );
    let _ = fs::remove_file(moved);
    let _ = fs::remove_file(temporary_backup);
}

#[cfg(target_os = "linux")]
#[test]
fn linux_pre_exchange_anchor_is_retained_as_the_single_recovery_slot() {
    let (temp, service) = service_root();
    let target = temp.path().join("exchange-anchor.txt");
    fs::write(&target, b"before").expect("write exchange anchor target");
    let revision = service
        .read("exchange-anchor.txt", ReadOptions::default())
        .expect("read exchange anchor target")
        .revision;
    let service = Arc::new(service);
    let plan = service
        .plan_write(
            "exchange-anchor.txt",
            b"after".to_vec(),
            ExpectedRevision::exact(revision),
        )
        .expect("plan exchange anchor write");
    files::set_test_pause(files::TEST_PAUSE_BEFORE_EXCHANGE);
    let worker = {
        let service = Arc::clone(&service);
        thread::spawn(move || service.execute_write(plan))
    };
    wait_for_test_pause();
    fs::remove_file(&target).expect("remove destination before exchange");
    files::clear_test_pause();
    assert!(worker.join().expect("join exchange anchor worker").is_err());
    let tombstones = fs::read_dir(temp.path())
        .expect("list exchange anchor residue")
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(".devmanager-tombstone-"))
        })
        .count();
    assert_eq!(tombstones, 1, "one exact old-inode anchor must remain");
    assert_eq!(
        service.cleanup_occupancy_for_test(),
        1,
        "the pre-exchange anchor must own the existing ledger slot"
    );
    drop(service);
    let restarted = WorkspaceFileService::new_for_test(temp.path())
        .expect("restart should recover the exact pre-exchange anchor");
    assert!(!fs::read_dir(temp.path())
        .expect("list recovered exchange anchor workspace")
        .filter_map(Result::ok)
        .any(|entry| entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(".devmanager-tombstone-"))));
    drop(restarted);
}

#[cfg(windows)]
#[test]
fn windows_old_tombstone_delete_failure_keeps_committed_destination() {
    let (temp, service) = service_root();
    let target = temp.path().join("windows-delete-race.txt");
    fs::write(&target, b"before").expect("write Windows target");
    let revision = service
        .read("windows-delete-race.txt", ReadOptions::default())
        .expect("read Windows target")
        .revision;
    let service = Arc::new(service);
    let plan = service
        .plan_write(
            "windows-delete-race.txt",
            b"after".to_vec(),
            ExpectedRevision::exact(revision),
        )
        .expect("plan Windows write");
    files::set_test_pause(files::TEST_PAUSE_AFTER_INSTALL);
    let worker = {
        let service = Arc::clone(&service);
        thread::spawn(move || service.execute_write(plan))
    };
    wait_for_test_pause();
    files::set_test_force_old_delete_failure(true);
    files::clear_test_pause();
    assert!(worker.join().expect("join Windows worker").is_err());
    assert_eq!(
        fs::read(&target).expect("new destination survives"),
        b"after"
    );
    assert!(
        fs::read_dir(temp.path())
            .expect("list retained tombstones")
            .filter_map(Result::ok)
            .any(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with(".devmanager-tombstone-"))
            }),
        "old inode must remain durably tracked"
    );
    files::set_test_force_old_delete_failure(false);
}

#[cfg(windows)]
#[test]
fn windows_install_failure_after_old_detach_keeps_cleanup_authority() {
    let (temp, service) = service_root();
    let target = temp.path().join("windows-install-race.txt");
    fs::write(&target, b"before").expect("write Windows install target");
    let revision = service
        .read("windows-install-race.txt", ReadOptions::default())
        .expect("read Windows install target")
        .revision;
    let service = Arc::new(service);
    let plan = service
        .plan_write(
            "windows-install-race.txt",
            b"after".to_vec(),
            ExpectedRevision::exact(revision),
        )
        .expect("plan Windows install race");
    files::set_test_pause(files::TEST_PAUSE_AFTER_OLD_DETACH);
    let worker = {
        let service = Arc::clone(&service);
        thread::spawn(move || service.execute_write(plan))
    };
    wait_for_test_pause();
    fs::write(&target, b"attacker-destination").expect("install same-name destination writer");
    files::clear_test_pause();
    assert!(worker.join().expect("join Windows install worker").is_err());
    assert_eq!(
        fs::read(&target).expect("attacker destination survives"),
        b"attacker-destination"
    );
    assert_eq!(
        service.cleanup_occupancy_for_test(),
        1,
        "the detached old inode must retain one exact cleanup slot"
    );
    assert!(!fs::read_dir(temp.path())
        .expect("list Windows install residue")
        .filter_map(Result::ok)
        .any(|entry| entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(".devmanager-file-"))));
}

#[cfg(windows)]
#[test]
fn windows_tombstone_recovery_reclaims_and_allows_repeated_startup() {
    let (temp, service) = service_root();
    let mut service = Arc::new(service);

    for iteration in 0..2 {
        let target_name = format!("windows-recovery-{iteration}.txt");
        let target = temp.path().join(&target_name);
        fs::write(&target, b"before").expect("write Windows recovery target");
        let revision = service
            .read(&target_name, ReadOptions::default())
            .expect("read Windows recovery target")
            .revision;
        let plan = service
            .plan_write(
                &target_name,
                b"after".to_vec(),
                ExpectedRevision::exact(revision),
            )
            .expect("plan Windows recovery write");
        files::set_test_pause(files::TEST_PAUSE_AFTER_INSTALL);
        let worker = {
            let service = Arc::clone(&service);
            thread::spawn(move || service.execute_write(plan))
        };
        wait_for_test_pause();
        files::set_test_force_old_delete_failure(true);
        files::clear_test_pause();
        assert!(worker
            .join()
            .expect("join Windows recovery worker")
            .is_err());
        files::set_test_force_old_delete_failure(false);

        assert!(fs::read_dir(temp.path())
            .expect("list retained Windows recovery tombstone")
            .filter_map(Result::ok)
            .any(|entry| entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(".devmanager-tombstone-"))));

        drop(service);
        let restarted = WorkspaceFileService::new_for_test(temp.path())
            .expect("restart should recover the durable tombstone");
        assert!(!fs::read_dir(temp.path())
            .expect("list recovered Windows workspace")
            .filter_map(Result::ok)
            .any(|entry| entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(".devmanager-tombstone-"))));
        service = Arc::new(restarted);
    }
}

#[cfg(windows)]
#[test]
fn windows_target_replacement_after_cas_validation_is_not_clobbered() {
    let (temp, service) = service_root();
    let target = temp.path().join("windows-cas-race.txt");
    fs::write(&target, b"before").expect("write Windows CAS target");
    let revision = service
        .read("windows-cas-race.txt", ReadOptions::default())
        .expect("read Windows CAS target")
        .revision;
    let service = Arc::new(service);
    let plan = service
        .plan_write(
            "windows-cas-race.txt",
            b"planned".to_vec(),
            ExpectedRevision::exact(revision),
        )
        .expect("plan Windows CAS write");
    files::set_test_pause(files::TEST_PAUSE_BEFORE_OLD_DETACH);
    let worker = {
        let service = Arc::clone(&service);
        thread::spawn(move || service.execute_write(plan))
    };
    wait_for_test_pause();
    let moved = temp.path().join("windows-cas-old.tmp");
    fs::rename(&target, &moved).expect("move validated target aside");
    fs::write(&target, b"attacker-destination").expect("install same-name attacker");
    files::clear_test_pause();
    assert!(worker.join().expect("join Windows CAS worker").is_err());
    assert_eq!(
        fs::read(&target).expect("same-name attacker survives"),
        b"attacker-destination"
    );
    let _ = fs::remove_file(moved);
}

#[test]
fn write_plan_never_overwrites_a_same_name_replacement() {
    let (temp, service) = service_root();
    let target = temp.path().join("write-replaced.txt");
    let replacement = temp.path().join("write-replacement.tmp");
    fs::write(&target, b"original").expect("write original replacement fixture");
    let revision = service
        .read("write-replaced.txt", ReadOptions::default())
        .expect("read original replacement fixture")
        .revision;
    let plan = service
        .plan_write(
            "write-replaced.txt",
            b"new-content".to_vec(),
            ExpectedRevision::exact(revision),
        )
        .expect("plan write replacement fixture");

    fs::write(&replacement, b"external-writer").expect("write replacement fixture");
    fs::remove_file(&target).expect("remove original name before replacement");
    fs::rename(&replacement, &target).expect("install same-name replacement");

    assert!(matches!(
        service.execute_write(plan),
        Err(FileServiceError::Conflict { .. })
    ));
    assert_eq!(
        fs::read(&target).expect("replacement must survive stale write"),
        b"external-writer"
    );
}

#[cfg(any(unix, windows))]
fn restore_modified_time(path: &Path, metadata: &fs::Metadata) {
    #[cfg(windows)]
    {
        let times = fs::FileTimes::new()
            .set_accessed(metadata.accessed().expect("original access time"))
            .set_modified(metadata.modified().expect("original modified time"));
        fs::File::options()
            .write(true)
            .open(path)
            .expect("open timestamp fixture")
            .set_times(times)
            .expect("restore original timestamps");
    }
    #[cfg(unix)]
    {
        fs::File::options()
            .write(true)
            .open(path)
            .expect("open timestamp fixture")
            .set_modified(metadata.modified().expect("original modified time"))
            .expect("restore original timestamp");
    }
}

#[cfg(any(unix, windows))]
#[test]
fn fingerprint_only_mutations_still_reject_same_identity_content_changes() {
    let (temp, service) = service_root();
    let target = temp.path().join("hash-commit.txt");
    fs::write(&target, b"before").expect("write original hash fixture");
    let original = service
        .read("hash-commit.txt", ReadOptions::default())
        .expect("read original hash fixture");
    let original_metadata = fs::metadata(&target).expect("stat original hash fixture");

    let write_plan = service
        .plan_write(
            "hash-commit.txt",
            b"writer".to_vec(),
            ExpectedRevision::fingerprint(original.revision.fingerprint.clone()),
        )
        .expect("plan fingerprint-only write");
    fs::write(&target, b"change").expect("mutate same identity before write commit");
    restore_modified_time(&target, &original_metadata);
    assert!(matches!(
        service.execute_write(write_plan),
        Err(FileServiceError::Conflict { .. })
    ));
    assert_eq!(
        fs::read(&target).expect("read changed write target"),
        b"change"
    );

    let current = service
        .read("hash-commit.txt", ReadOptions::default())
        .expect("read changed hash fixture");
    let current_metadata = fs::metadata(&target).expect("stat changed hash fixture");
    let delete_preview = service
        .plan_delete(
            "hash-commit.txt",
            ExpectedRevision::fingerprint(current.revision.fingerprint.clone()),
        )
        .expect("plan fingerprint-only delete");
    fs::write(&target, b"mutate").expect("mutate same identity before delete commit");
    restore_modified_time(&target, &current_metadata);
    assert!(matches!(
        service.execute_delete(delete_preview),
        Err(FileServiceError::Conflict { .. })
    ));
    assert_eq!(
        fs::read(&target).expect("read changed delete target"),
        b"mutate"
    );
}

#[test]
fn delete_preview_is_opaque_and_revalidates_fingerprint() {
    let (temp, service) = service_root();
    let target = temp.path().join("delete.txt");
    fs::write(&target, b"delete me").expect("write delete fixture");
    let current = service
        .read("delete.txt", ReadOptions::default())
        .expect("read delete fixture");
    let preview = service
        .plan_delete(
            "delete.txt",
            ExpectedRevision::exact(current.revision.clone()),
        )
        .expect("plan delete");
    assert_eq!(preview.revision().fingerprint, current.revision.fingerprint);

    fs::write(&target, b"changed after preview").expect("simulate concurrent edit");
    assert!(matches!(
        service.execute_delete(preview),
        Err(FileServiceError::Conflict { .. })
    ));
    assert!(target.exists());

    let current = service
        .read("delete.txt", ReadOptions::default())
        .expect("read current delete fixture");
    let preview = service
        .plan_delete("delete.txt", ExpectedRevision::exact(current.revision))
        .expect("plan final delete");
    if let Err(error) = service.execute_delete(preview) {
        panic!("execute delete: {error}");
    }
    assert!(!target.exists());
}

#[test]
fn delete_preview_never_removes_a_same_name_replacement() {
    let (temp, service) = service_root();
    let target = temp.path().join("delete-replaced.txt");
    let replacement = temp.path().join("delete-replacement.tmp");
    fs::write(&target, b"original").expect("write original delete fixture");
    let revision = service
        .read("delete-replaced.txt", ReadOptions::default())
        .expect("read original delete fixture")
        .revision;
    let preview = service
        .plan_delete("delete-replaced.txt", ExpectedRevision::exact(revision))
        .expect("plan delete replacement fixture");

    fs::write(&replacement, b"replacement").expect("write replacement fixture");
    fs::remove_file(&target).expect("remove original name before replacement");
    fs::rename(&replacement, &target).expect("install same-name replacement");

    assert!(matches!(
        service.execute_delete(preview),
        Err(FileServiceError::Conflict { .. })
    ));
    assert_eq!(
        fs::read(&target).expect("replacement must survive stale delete"),
        b"replacement"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn linux_delete_really_removes_the_target_without_a_placeholder_collision() {
    let (temp, service) = service_root();
    let target = temp.path().join("linux-delete.txt");
    fs::write(&target, b"delete me on linux").expect("write linux delete fixture");
    let revision = service
        .read("linux-delete.txt", ReadOptions::default())
        .expect("read linux delete fixture")
        .revision;
    let preview = service
        .plan_delete("linux-delete.txt", ExpectedRevision::exact(revision))
        .expect("plan linux delete fixture");

    service
        .execute_delete(preview)
        .expect("linux delete must commit");
    assert!(!target.exists(), "logical target must be gone after delete");
    assert!(
        fs::read_dir(temp.path())
            .expect("list linux delete workspace")
            .all(|entry| !entry
                .expect("read linux delete entry")
                .file_name()
                .to_string_lossy()
                .starts_with(".devmanager-tombstone-")),
        "a successful delete must not leave its placeholder behind"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn linux_delete_rejects_recreated_target_before_detach() {
    let (temp, service) = service_root();
    let target = temp.path().join("linux-delete-final-cas.txt");
    let replacement = temp.path().join("linux-delete-final-cas.tmp");
    fs::write(&target, b"original delete target").expect("write final CAS fixture");
    let revision = service
        .read("linux-delete-final-cas.txt", ReadOptions::default())
        .expect("read final CAS fixture")
        .revision;
    let service = Arc::new(service);
    let preview = service
        .plan_delete(
            "linux-delete-final-cas.txt",
            ExpectedRevision::exact(revision),
        )
        .expect("plan final CAS fixture");
    files::set_test_pause(files::TEST_PAUSE_BEFORE_OLD_DETACH);
    let worker = {
        let service = Arc::clone(&service);
        thread::spawn(move || service.execute_delete(preview))
    };
    wait_for_test_pause();
    fs::write(&replacement, b"replacement must survive").expect("write replacement");
    fs::remove_file(&target).expect("remove original target name");
    fs::rename(&replacement, &target).expect("recreate target name");
    files::clear_test_pause();
    assert!(matches!(
        worker.join().expect("join final CAS worker"),
        Err(FileServiceError::Conflict { .. })
    ));
    assert_eq!(
        fs::read(&target).expect("recreated target must survive"),
        b"replacement must survive"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn linux_delete_tombstone_replacement_fails_closed_without_clobbering_writer() {
    let (temp, service) = service_root();
    let target = temp.path().join("linux-delete-race.txt");
    fs::write(&target, b"delete me safely").expect("write linux delete race fixture");
    let revision = service
        .read("linux-delete-race.txt", ReadOptions::default())
        .expect("read linux delete race fixture")
        .revision;
    let service = Arc::new(service);
    let preview = service
        .plan_delete("linux-delete-race.txt", ExpectedRevision::exact(revision))
        .expect("plan linux delete race fixture");
    files::set_test_pause(files::TEST_PAUSE_BEFORE_UNLINK);
    let worker = {
        let service = Arc::clone(&service);
        thread::spawn(move || service.execute_delete(preview))
    };
    wait_for_test_pause();
    let tombstone = fs::read_dir(temp.path())
        .expect("list Linux tombstones")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".devmanager-tombstone-"))
        })
        .expect("delete should hold a private tombstone before unlink");
    let moved = tombstone.with_extension("attacker");
    fs::rename(&tombstone, &moved).expect("move validated tombstone away");
    fs::write(&tombstone, b"attacker-tombstone").expect("install tombstone replacement");
    files::clear_test_pause();
    assert!(worker.join().expect("join Linux delete worker").is_err());
    assert!(!target.exists(), "logical target remains deleted");
    assert_eq!(
        fs::read(&tombstone).expect("attacker tombstone replacement survives"),
        b"attacker-tombstone"
    );
    let _ = fs::remove_file(moved);
}

#[cfg(target_os = "linux")]
#[test]
fn linux_delete_source_is_rebound_before_handle_relative_detach() {
    let source = include_str!("../src/workspace/files.rs");
    let pause = source
        .find("test_pause(TEST_PAUSE_BEFORE_OLD_DETACH);")
        .expect("delete detach race pause exists");
    let rebind = source[pause..]
        .find("let final_target = open_child_nofollow(parent, name)")
        .expect("delete rebind follows final CAS pause");
    let detach = source[pause..]
        .find("unix_at::renameat2(")
        .expect("delete detach primitive exists");
    assert!(
        rebind < detach,
        "the source identity must be checked after the race window and before detach"
    );
    assert!(source.contains("final_revision != *expected_revision"));
}

#[test]
fn replacement_recovery_never_adopts_the_substituted_inode() {
    let source = include_str!("../src/workspace/files.rs");
    assert!(source.contains("CleanupFailed"));
    assert!(source.contains("AtomicReplaceError::Uncertain"));
    assert!(
        !source.contains("AtomicReplaceError::io(error, true, true)"),
        "a post-commit failure must carry a tombstone or explicit uncertainty"
    );
    assert!(
        !source.contains("tombstone_observed.0.fingerprint.identity,\n                true"),
        "a post-detach mismatch must never persist the replacement identity"
    );
    assert!(
        !source.contains("source_name.to_string(),\n            expected_identity"),
        "an uncertain normal pathname must not become a durable tombstone record"
    );
}

#[test]
fn unbound_private_temporary_replacement_is_foreign_after_restart() {
    let temp = tempfile::tempdir().expect("create workspace root");
    let residue = temp
        .path()
        .join(".devmanager-file-0123456789abcdef0123456789abcdef.tmp");
    fs::write(&residue, b"foreign replacement").expect("write unbound private name");

    let _service = WorkspaceFileService::new_for_test(temp.path())
        .expect("startup must remain usable with foreign private residue");
    assert_eq!(
        fs::read(&residue).expect("foreign same-name replacement survives"),
        b"foreign replacement"
    );
}

#[test]
fn relabelled_private_temporary_identity_is_foreign_after_restart() {
    let temp = tempfile::tempdir().expect("create workspace root");
    let service = WorkspaceFileService::new_for_test(temp.path())
        .expect("bind workspace before staging relabelled private residue");
    let parent = service.root_identity_for_test();
    let residue_name = format!(
        ".devmanager-file-{:016x}-{:016x}-{:016x}-{:016x}-{:016x}-{:016x}-{}.tmp",
        parent.volume_or_device,
        parent.file_or_inode,
        1_u64,
        2_u64,
        0xdead_beef_dead_beef_u64,
        0xcafe_babe_cafe_babe_u64,
        "5a".repeat(16),
    );
    let residue = temp.path().join(&residue_name);
    fs::write(&residue, b"relabelled foreign replacement").expect("write relabelled residue");
    drop(service);

    let _service = WorkspaceFileService::new_for_test(temp.path())
        .expect("startup must remain usable with relabelled private residue");
    assert_eq!(
        fs::read(&residue).expect("relabelled same-name replacement survives"),
        b"relabelled foreign replacement"
    );
}

#[test]
fn full_cleanup_ledger_recovery_settles_one_exact_record_at_capacity() {
    let temp = tempfile::tempdir().expect("create workspace root");
    let service = WorkspaceFileService::new_for_test(temp.path())
        .expect("bind workspace before staging exact recovery residue");
    let parent_identity = service.root_identity_for_test();
    for index in 0..files::MAX_TOMBSTONES {
        let seed_name = format!("seed-{index}.old");
        let seed = temp.path().join(&seed_name);
        fs::write(&seed, format!("residue-{index}")).expect("write recovery residue");
        let revision = service
            .read(&seed_name, ReadOptions::default())
            .expect("read recovery residue identity")
            .revision;
        let identity = revision.fingerprint.identity;
        let tombstone = temp.path().join(files::test_tombstone_name(
            parent_identity,
            identity,
            [index as u8; 16],
        ));
        fs::rename(seed, tombstone).expect("name exact recovery residue");
    }
    drop(service);

    let _service = WorkspaceFileService::new_for_test(temp.path())
        .expect("full cleanup ledger must still permit startup");
    let remaining = fs::read_dir(temp.path())
        .expect("list recovery residue")
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".devmanager-tombstone-")
        })
        .count();
    assert!(
        remaining < files::MAX_TOMBSTONES,
        "recovery must transfer the existing slot instead of requiring a 65th slot"
    );
}

#[test]
fn post_effect_rename_expiry_keeps_effect_and_exact_residue_owned() {
    let source = include_str!("../src/workspace/files.rs");
    assert!(
        source.contains("temporary_moved: true") || source.contains("destination_committed: true"),
        "post-rename deadline errors must carry the committed effect"
    );
    assert!(
        source.contains("record_cleanup_move")
            || source.contains("update_cleanup_record_after_move")
            || source.contains("persist_cleanup_record"),
        "a successful rename must publish its current path and identity before expiry returns"
    );
    assert!(
        source.contains("RestoreCleanupOutcome::RestoredUncertain")
            && source.contains("quarantine_private_temporary"),
        "post-effect restore/quarantine failures must retain an exact private name"
    );
}

#[test]
fn windows_deadline_cleanup_flags_bind_to_the_renamed_handle() {
    let source = include_str!("../src/workspace/files.rs");
    let detach_start = source
        .find("match windows_rename_relative(&target")
        .expect("Windows old-detach rename exists");
    let install_offset = source[detach_start..]
        .find("match windows_rename_relative(temporary")
        .expect("Windows temporary install rename exists");
    let detach_body = &source[detach_start..detach_start + install_offset];
    let compact_detach = detach_body.split_whitespace().collect::<String>();
    assert!(
        compact_detach.contains("tombstone(tombstone_name,identity,false,false"),
        "old-detach deadline paths must retain the still-unmoved temporary"
    );

    let delete_start = source
        .find("let delete_result = delete_opened_file(&target);")
        .expect("Windows old-target delete exists");
    let delete_body = &source[delete_start..];
    assert!(
        delete_body.contains("tombstone_name")
            && delete_body.contains("identity")
            && delete_body.contains("true,\n                            true"),
        "post-install deadline expiry must retain the committed old-inode tombstone"
    );
}

#[test]
fn linux_exchange_exact_unlink_failure_is_retained_for_retry() {
    let source = include_str!("../src/workspace/files.rs");
    let preferred_start = source
        .find("if let Some(preferred_name) = preferred_name")
        .expect("Linux preferred anchor branch exists");
    let preferred_end = source[preferred_start..]
        .find("if !source_is_expected")
        .map(|offset| preferred_start + offset)
        .expect("Linux preferred anchor branch boundary exists");
    let preferred_body = &source[preferred_start..preferred_end];
    assert!(
        !preferred_body.contains("let _ = unlink_exact_private_link_if_identity"),
        "exact unlink failure must not be discarded"
    );
    assert!(
        preferred_body.contains("if let Err(error) = unlink_exact_private_link_if_identity")
            || preferred_body.contains("match unlink_exact_private_link_if_identity"),
        "exact unlink failure must remain visible to recovery handling"
    );
    assert!(
        source.contains("expected_target_identity != Some(expected_identity)"),
        "recovery must retry an exchanged temporary alias bound to the old identity"
    );
}

#[test]
fn directory_identity_cache_lock_uses_the_callers_deadline() {
    let source = include_str!("../src/workspace/files.rs");
    let start = source
        .find("fn observe_directory_identity(")
        .expect("directory identity cache helper exists");
    let end = source[start..]
        .find("fn current_expected_state_with_deadline(")
        .map(|offset| start + offset)
        .expect("directory identity cache helper boundary exists");
    let body = &source[start..end];
    assert!(
        body.contains("deadline: &OperationDeadline"),
        "cache observation must receive the caller deadline"
    );
    assert!(
        body.contains("lock_until(deadline)"),
        "cache observation must not block on an ordinary Mutex lock"
    );
}

#[test]
fn linux_uncertain_delete_records_generated_tombstone_not_public_original() {
    let source = include_str!("../src/workspace/files.rs");
    let start = source
        .find("pub fn execute_delete(")
        .expect("delete execution exists");
    let end = source[start..]
        .find("fn target_lock(")
        .map(|offset| start + offset)
        .expect("delete execution boundary exists");
    let body = &source[start..end];
    assert!(
        body.contains("generated_tombstone") || body.contains("delete_result.tombstone"),
        "Linux uncertainty must retain the exact generated tombstone identity"
    );
    assert!(
        !body.contains("resolved.name.as_str(),\n                        delete_identity"),
        "the public original pathname must never become an uncertain cleanup record"
    );
}

#[test]
fn cleanup_authority_restart_scan_and_initialization_use_callers_budget() {
    let source = include_str!("../src/workspace/files.rs");
    assert!(
        source.contains("discover_cleanup_authority") || source.contains("scan_cleanup_authority"),
        "restart must scan the process-private cleanup authority"
    );
    assert!(
        source.contains("cleanup_authority(deadline")
            || source.contains("cleanup_authority_with_deadline"),
        "authority initialization must accept the caller deadline"
    );
    assert!(
        !source.contains("CLEANUP_AUTHORITY.get_or_try_init(||"),
        "first-use OnceLock initialization must not perform unbounded I/O"
    );
    assert!(
        source.contains("format_cleanup_authority_entry_name")
            && source.contains("parse_authority_entry_binding"),
        "authority residue must retain the original parent/target/nonce binding"
    );
    assert!(
        source.contains("is_private_cleanup_authority") && source.contains("permissions().mode()"),
        "restart must ignore authority directories without private permissions"
    );
}

#[test]
fn wrong_path_cleanup_guard_is_visible_uncertainty_not_recoverable_record() {
    let source = include_str!("../src/workspace/files.rs");
    assert!(
        source.contains("reject_foreign_cleanup_name")
            || source.contains("is_private_cleanup_name(name)"),
        "cleanup guards must reject ordinary or attacker-controlled names before recording"
    );
    assert!(
        source.contains("foreign cleanup name") || source.contains("CleanupFailed"),
        "a wrong-path guard must report visible uncertainty"
    );
}

#[test]
fn cleanup_move_updates_guard_identity_before_observing_post_effect_deadline() {
    let source = include_str!("../src/workspace/files.rs");
    assert!(
        source.contains("current_name")
            && source.contains("current_identity")
            && source.contains("persist_cleanup_record"),
        "the sole guard must publish the moved path and exact identity before deadline checks"
    );
}

#[test]
fn windows_cleanup_reopen_is_delete_capable_and_handle_bound() {
    let source = include_str!("../src/workspace/files.rs");
    assert!(source.contains("open_child_nofollow_for_cleanup"));
    assert!(
        source.contains("open_child_nofollow_with_access(parent, name, 0x0001_0000, 0x0000_0005)")
    );
    assert!(
        source.contains("delete_opened_file(&file)"),
        "recovery must delete the exact handle it validated"
    );
    assert!(
        source
            .matches(
                "validate_temporary_path(parent, temporary, temporary_name, new_sha256, deadline)"
            )
            .count()
            >= 3,
        "Windows must revalidate the held temporary immediately before replacement"
    );
    assert!(
        source.contains("0x0000_0005, // share read/delete; deny post-validation writers"),
        "temporary creation must deny post-validation write opens"
    );
}

#[test]
fn macos_directory_handles_use_target_fd_namespace_and_reads_remain_available() {
    let source = include_str!("../src/workspace/files.rs");
    assert!(!source.contains("format!(\"/proc/self/fd/"));
    assert!(source.contains("let fd_namespace = \"/dev/fd\""));
    assert!(source.contains("target_os = \"macos\""));
    assert!(source.contains("Err(FileServiceError::Unsupported { operation })"));
}

#[test]
fn task6_bridge_accepts_retained_root_handles_without_reopening_by_path() {
    assert!(
        files::task6_bridge_retained_handle_swap_proof_for_test(),
        "the Task 6 bridge must bind the retained handle even after its visible path is replaced"
    );
}

#[test]
fn authority_bridge_has_no_raw_id_or_path_only_issuer() {
    let source = include_str!("../src/workspace/files.rs");
    assert!(
        !source.contains("from_host_lease("),
        "authority must not be minted from raw byte arrays"
    );
    assert!(
        !source.contains("pub(crate) fn from_host_workspace("),
        "production authority must not be minted from a path"
    );
    assert!(
        source.contains("Task6WorkspaceLease"),
        "file service must expose only the typed Task 6.2 lease bridge"
    );
    assert!(
        !source.contains("fs::canonicalize(root)"),
        "root binding must not canonicalize and reopen a path"
    );
    assert!(
        !source.contains(".ancestors()"),
        "root binding must not validate ancestors by path before opening"
    );
}

#[test]
fn approved_root_rejects_a_reparse_in_an_initial_ancestor_chain() {
    let target = tempfile::tempdir().expect("create approved root target");
    let parent = tempfile::tempdir().expect("create root alias parent");
    let real_root = target.path().join("workspace");
    fs::create_dir(&real_root).expect("create approved nested root");
    let alias_parent = parent.path().join("workspace-parent");
    if !create_directory_link(target.path(), &alias_parent) {
        return;
    }

    assert!(matches!(
        WorkspaceFileService::new_for_test(alias_parent.join("workspace")),
        Err(FileServiceError::RootUnavailable)
    ));
}

#[test]
fn temporary_cleanup_and_startup_tombstone_recovery_are_explicit() {
    let source = include_str!("../src/workspace/files.rs");
    assert!(
        source.contains("impl Drop for TemporaryFile"),
        "temporary files must be cleaned if setup fails before cleanup is armed"
    );
    assert!(
        source.contains("discover_tombstones"),
        "durable tombstones must be discovered at service startup"
    );
    assert!(source.contains("quarantine_private_temporary"));
    assert!(
        source.contains("MAX_TOMBSTONES"),
        "startup recovery must remain bounded"
    );
}

#[test]
fn tombstone_recovery_is_bounded_and_identity_guarded() {
    let source = include_str!("../src/workspace/files.rs");
    assert!(source.contains("MAX_TOMBSTONES"));
    assert!(source.contains("recover_tombstones"));
    assert!(source.contains("unlink_private_name_if_identity"));
    assert!(source.contains("retain_tombstone"));
    assert!(source.contains("uncertain_cleanups"));
}

#[test]
fn mutation_operations_have_one_absolute_budget_and_expiry_checks() {
    let source = include_str!("../src/workspace/files.rs");
    for operation in [
        "pub fn plan_write(",
        "pub fn execute_write(",
        "pub fn plan_delete(",
        "pub fn execute_delete(",
    ] {
        let start = source
            .find(operation)
            .unwrap_or_else(|| panic!("missing operation body: {operation}"));
        let body = &source[start..];
        assert!(
            body.contains("let deadline = self.operation_deadline();")
                || body.contains("let deadline = operation_deadline();"),
            "{operation} must own one absolute operation budget before I/O"
        );
        assert!(
            body.contains("deadline.check()?;"),
            "{operation} must check its budget deterministically"
        );
    }
    let resolver = source
        .find("fn resolve_target_with_deadline(")
        .expect("budgeted resolve_target exists");
    let resolver_body = &source[resolver..];
    assert!(
        !resolver_body[..resolver_body
            .find("fn observe_directory_identity(")
            .unwrap_or(0)]
            .contains("OperationDeadline::new()"),
        "nested mutation resolution must not reset the operation budget"
    );
    assert!(
        source.contains("discover_tombstones(&deadline)")
            && source.contains("recover_tombstones(&deadline)"),
        "startup and mutation recovery must consume the caller's budget"
    );
}

#[test]
fn expired_operation_fails_before_entry_io() {
    let (temp, service) = service_root();
    service.set_test_budget_mode(TEST_OPERATION_EXPIRED_ENTRY);

    assert!(matches!(
        service.plan_write("entry.txt", b"new".to_vec(), ExpectedRevision::missing()),
        Err(FileServiceError::DeadlineExceeded)
    ));
    assert!(matches!(
        service.plan_delete("entry.txt", ExpectedRevision::missing()),
        Err(FileServiceError::DeadlineExceeded)
    ));
    let names = fs::read_dir(temp.path())
        .expect("workspace remains readable")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .collect::<Vec<_>>();
    assert!(
        names.is_empty(),
        "expired entry must not create I/O residue"
    );
}

#[test]
fn expired_operation_midway_returns_deadline_and_keeps_target_recoverable() {
    let (temp, service) = service_root();
    let plan = service
        .plan_write(
            "midway.txt",
            b"new contents".to_vec(),
            ExpectedRevision::missing(),
        )
        .expect("prepare write before injecting budget");
    service.set_test_budget_mode(TEST_OPERATION_EXPIRED_MID);

    assert!(matches!(
        service.execute_write(plan),
        Err(FileServiceError::DeadlineExceeded)
    ));
    let entries = fs::read_dir(temp.path())
        .expect("workspace remains readable")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert!(
        entries.iter().all(|name| name.starts_with(".devmanager-")),
        "mid-operation timeout may retain only service-private recovery state: {entries:?}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn cleanup_flood_uses_one_shared_capacity_authority() {
    let source = include_str!("../src/workspace/files.rs");
    assert!(
        source.contains("CleanupLedger"),
        "explicit operations and RAII cleanup must share one ledger"
    );
    assert!(
        source.contains("reserve_tombstone_slot") && source.contains("uncertain_cleanups"),
        "the shared ledger must account for reserved and uncertain cleanup"
    );
    assert!(
        source.contains("quarantine_private_temporary") && source.contains("try_reserve"),
        "RAII quarantine must reserve before creating a tombstone"
    );
    assert!(
        source.contains("MAX_TOMBSTONES"),
        "flooding cleanup failures must remain bounded"
    );
}

#[test]
fn cleanup_capacity_stays_bounded_after_repeated_reservation_failures() {
    let (_temp, service) = service_root();
    let (accepted, occupied) = service.reserve_cleanup_capacity_for_test(files::MAX_TOMBSTONES + 8);
    assert_eq!(accepted, files::MAX_TOMBSTONES);
    assert_eq!(occupied, files::MAX_TOMBSTONES);
    let (additional, still_occupied) = service.reserve_cleanup_capacity_for_test(8);
    assert_eq!(additional, 0);
    assert_eq!(still_occupied, files::MAX_TOMBSTONES);
}

#[test]
fn mutation_and_cleanup_locks_are_deadline_aware_without_short_polling() {
    let source = include_str!("../src/workspace/files.rs");
    assert!(
        source.contains("Condvar") && source.contains("lock_until(deadline)"),
        "target and cleanup locks must wait with one absolute deadline"
    );
    assert!(
        !source.contains("let _guard = lock.lock()"),
        "mutation execution must not block indefinitely on a target lock"
    );
    assert!(
        source.contains("DeadlineMutex<Vec<TombstoneRecord>>"),
        "cleanup-ledger insertion must use the same deadline-aware lock"
    );
}

#[test]
fn blocked_target_lock_returns_typed_deadline_without_waiting_forever() {
    assert!(
        files::deadline_lock_times_out_for_test(),
        "a held target/ledger lock must return DeadlineExceeded at the shared deadline"
    );
}

#[cfg(unix)]
#[test]
fn restart_discovers_and_recovers_private_temporary_residue() {
    let temp = tempfile::tempdir().expect("create workspace root");
    let target = temp.path().join("target.txt");
    fs::write(&target, b"target").expect("write target identity");
    let service = WorkspaceFileService::new_for_test(temp.path())
        .expect("bind workspace for target identity");
    let parent_identity = service.root_identity_for_test();
    let target_identity = service
        .read("target.txt", ReadOptions::default())
        .expect("read target identity")
        .revision
        .fingerprint
        .identity;
    drop(service);

    let staged = temp.path().join("staged.tmp");
    fs::write(&staged, b"recoverable private residue").expect("write private residue");
    let temporary = fs::File::open(&staged).expect("open temporary identity");
    let residue = temp.path().join(files::test_temporary_name(
        parent_identity,
        Some(target_identity),
        &temporary,
        [0xabu8; 16],
    ));
    drop(temporary);
    fs::rename(staged, &residue).expect("stage identity-bound private residue");

    let _service = WorkspaceFileService::new_for_test(temp.path()).expect("restart workspace");
    assert!(
        !residue.exists(),
        "startup recovery must reclaim an identity-verified private temporary"
    );
}

#[test]
fn uncertain_cleanup_is_durable_identity_bound_state_not_anonymous_counter() {
    let source = include_str!("../src/workspace/files.rs");
    assert!(
        source.contains("uncertain: true")
            && source.contains("record_uncertain_cleanup(")
            && source.contains("parent_identity")
            && source.contains("identity"),
        "uncertainty must retain the exact guarded parent/name/identity"
    );
    assert!(
        !source.contains("fn mark_uncertain_cleanup("),
        "an anonymous uncertainty counter cannot consume a cleanup slot"
    );
    assert!(
        source.contains("is_private_temporary_name") && source.contains("parse_tombstone_identity"),
        "startup must recognize both private temporary and tombstone names"
    );
}

#[test]
fn temporary_cleanup_ownership_transfers_once_before_early_errors() {
    let source = include_str!("../src/workspace/files.rs");
    assert!(
        source.contains("TempCleanup::from_temporary")
            || source.contains("temporary.transfer_cleanup_owner"),
        "arming TempCleanup must disarm TemporaryFile exactly once"
    );
    assert!(
        source.contains("cleanup.disarm()"),
        "the final owner must be explicitly disarmed after commit"
    );
}

#[test]
fn temporary_cleanup_owner_is_single_under_barrier() {
    let (_temp, service) = service_root();
    let plan = service
        .plan_write(
            "barrier-owner.txt",
            b"barrier contents".to_vec(),
            ExpectedRevision::missing(),
        )
        .expect("prepare barrier owner write");
    service.set_test_budget_mode(TEST_OPERATION_EXPIRED_MID);
    files::reset_test_cleanup_drop_count();
    let service = Arc::new(service);
    let barrier = Arc::new(Barrier::new(2));
    let worker_service = Arc::clone(&service);
    let worker_barrier = Arc::clone(&barrier);
    let worker = thread::spawn(move || {
        worker_barrier.wait();
        worker_service.execute_write(plan)
    });
    barrier.wait();
    let result = worker.join().expect("barrier owner worker");
    assert!(matches!(result, Err(FileServiceError::DeadlineExceeded)));
    assert_eq!(
        files::test_cleanup_drop_count(),
        1,
        "exactly one armed cleanup owner may run after an early error"
    );
}

#[test]
fn rollback_and_cleanup_restore_have_pre_post_deadline_checks() {
    let source = include_str!("../src/workspace/files.rs");
    assert!(
        source.contains("fn restore_cleanup_entry(")
            && source.contains(") -> io::Result<bool>")
            && source.contains("check_deadline_io(deadline)?"),
        "cleanup restoration must surface deadline expiry around reopen/rename/fsync"
    );
    let rollback_start = source
        .find("let rollback = unsafe")
        .expect("Linux atomic-replace rollback rename exists");
    let rollback_body = &source[rollback_start..];
    let rollback_end = rollback_body
        .find("if rollback == 0")
        .expect("Linux atomic-replace rollback result is checked");
    assert!(
        rollback_body[..rollback_end].contains("check_deadline_io(deadline)?")
            || rollback_body[..rollback_end].contains("deadline.check().is_err()"),
        "Linux atomic-replace rollback rename must have an immediate post-check"
    );
    assert!(
        source.contains("reservation.release()") && source.contains("tombstones.push("),
        "discovery must release a reservation if deadline-aware insertion cannot commit"
    );
}

#[cfg(unix)]
#[test]
fn listing_a_fifo_is_nonblocking_and_classifies_it_as_other() {
    use std::ffi::CString;
    use std::os::raw::c_char;

    unsafe extern "C" {
        fn mkfifo(path: *const c_char, mode: u32) -> i32;
    }

    let (temp, service) = service_root();
    let fifo = temp.path().join("events.fifo");
    let fifo_c = CString::new(fifo.to_string_lossy().as_bytes()).expect("fifo path");
    assert_eq!(unsafe { mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
    let entries = service
        .list(None, files::MAX_LIST_ENTRIES)
        .expect("FIFO metadata must not block listing");
    let fifo_entry = entries
        .iter()
        .find(|entry| entry.path.as_str() == "events.fifo")
        .expect("FIFO entry should be listed");
    assert_eq!(fifo_entry.kind, files::EntryKind::Other);
}

#[test]
fn secret_like_paths_are_classified_without_leaking_values_in_errors() {
    let (temp, service) = service_root();
    let target = temp.path().join(".env.local");
    fs::write(&target, b"API_TOKEN=super-secret-value").expect("write secret-like fixture");
    let entries = service.list(None, 8).expect("list secret-like fixture");
    assert_eq!(entries[0].secret, SecretClassification::SecretLike);

    let current = service
        .read(".env.local", ReadOptions::default())
        .expect("read secret-like fixture");
    let plan = service
        .plan_write(
            ".env.local",
            b"replacement-secret-value".to_vec(),
            ExpectedRevision::exact(current.revision),
        )
        .expect("plan secret-like write");
    fs::write(&target, b"API_TOKEN=changed-secret-value").expect("change secret-like fixture");
    let error = service
        .execute_write(plan)
        .expect_err("stale plan must conflict");
    let display = error.to_string();
    assert!(!display.contains("replacement-secret-value"));
    assert!(!display.contains("changed-secret-value"));
}

#[test]
fn reparse_directory_escape_is_rejected_when_the_platform_allows_a_link() {
    let (temp, service) = service_root();
    let outside = tempfile::tempdir().expect("create outside directory");
    fs::write(outside.path().join("outside.txt"), b"outside").expect("write outside fixture");
    let link = temp.path().join("linked");
    if !create_directory_link(outside.path(), &link) {
        return;
    }

    let error = service
        .read("linked/outside.txt", ReadOptions::default())
        .expect_err("reparse escape must be rejected");
    assert!(matches!(
        error,
        FileServiceError::ReparseRejected { .. } | FileServiceError::OutsideWorkspace { .. }
    ));
}

#[test]
fn approved_root_rejects_a_reparse_alias_before_canonicalization() {
    let target = tempfile::tempdir().expect("create approved root target");
    let parent = tempfile::tempdir().expect("create root alias parent");
    let link = parent.path().join("workspace-alias");
    if !create_directory_link(target.path(), &link) {
        return;
    }

    assert!(matches!(
        WorkspaceFileService::new_for_test(&link),
        Err(FileServiceError::RootUnavailable)
    ));
}

#[test]
fn replacing_the_bound_root_with_a_reparse_target_fails_closed() {
    let (temp, service) = service_root();
    let outside = tempfile::tempdir().expect("create outside root");
    fs::write(outside.path().join("outside.txt"), b"outside").expect("write outside root fixture");
    let original = temp.path().to_path_buf();
    let moved_guard = tempfile::Builder::new()
        .prefix("devmanager-file-service-root-real-")
        .tempdir_in(original.parent().expect("root parent"))
        .expect("reserve moved root path");
    let moved = moved_guard.path().to_path_buf();
    fs::remove_dir(&moved).expect("remove reserved moved root path");
    fs::rename(&original, &moved).expect("move bound root aside");
    if !create_directory_link(outside.path(), &original) {
        fs::rename(&moved, &original).expect("restore bound root");
        return;
    }

    assert!(service.list(None, 8).is_err());
    assert!(service
        .plan_write(
            "created.txt",
            b"must-not-escape".to_vec(),
            ExpectedRevision::missing(),
        )
        .is_err());
    let _ = fs::remove_dir(&original);
    fs::rename(moved, original).expect("restore bound root after rejection");
}

#[test]
fn planned_write_rejects_a_reparse_swap_before_execution() {
    let (temp, service) = service_root();
    let outside = tempfile::tempdir().expect("create outside directory");
    let parent = temp.path().join("parent");
    fs::create_dir(&parent).expect("create parent");
    let target = parent.join("file.txt");
    fs::write(&target, b"inside").expect("write target");
    let current = service
        .read("parent/file.txt", ReadOptions::default())
        .expect("read target");
    let plan = service
        .plan_write(
            "parent/file.txt",
            b"replacement".to_vec(),
            ExpectedRevision::exact(current.revision),
        )
        .expect("plan target write");

    let moved = temp.path().join("parent-real");
    fs::rename(&parent, &moved).expect("move original parent");
    if !create_directory_link(outside.path(), &parent) {
        fs::rename(&moved, &parent).expect("restore original parent");
        return;
    }
    let error = service
        .execute_write(plan)
        .expect_err("reparse swap must not be followed");
    assert!(matches!(
        error,
        FileServiceError::ReparseRejected { .. } | FileServiceError::OutsideWorkspace { .. }
    ));
    let _ = fs::remove_dir(&parent);
    fs::rename(moved, parent).expect("restore original parent");
    assert_eq!(fs::read(target).expect("read restored target"), b"inside");
}

#[test]
fn bounded_pages_lines_search_and_operation_admission_are_explicit() {
    let (temp, service) = service_root();
    fs::write(
        temp.path().join("b.txt"),
        b"zero\nneedle one\nneedle two\nlast",
    )
    .expect("write searchable text fixture");
    fs::write(temp.path().join("a.txt"), b"alpha").expect("write sorted text fixture");
    fs::create_dir(temp.path().join("nested")).expect("create nested directory fixture");
    fs::write(temp.path().join("nested/c.txt"), b"needle nested")
        .expect("write nested searchable fixture");

    let first_page = service
        .list_page(
            None,
            FilePageRequest {
                offset: 0,
                limit: 2,
            },
        )
        .expect("first deterministic page");
    assert_eq!(
        first_page
            .entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<Vec<_>>(),
        ["a.txt", "b.txt"]
    );
    assert_eq!(first_page.total_entries, 3);
    assert_eq!(first_page.next_offset, Some(2));

    let second_page = service
        .list_page(
            None,
            FilePageRequest {
                offset: 2,
                limit: 2,
            },
        )
        .expect("second deterministic page");
    assert_eq!(
        second_page
            .entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<Vec<_>>(),
        ["nested"]
    );
    assert_eq!(second_page.next_offset, None);

    let line_page = service
        .read_lines(
            "b.txt",
            LinePageRequest {
                start_line: 1,
                limit: 2,
                expected_revision: None,
            },
        )
        .expect("bounded text line page");
    assert_eq!(line_page.total_lines, 4);
    assert_eq!(line_page.lines[0].number, 2);
    assert_eq!(line_page.lines[0].text, "needle one");
    assert_eq!(line_page.lines[1].number, 3);
    assert_eq!(line_page.next_start_line, Some(3));

    let search = service
        .search(
            "needle",
            SearchOptions {
                max_matches: 3,
                ..SearchOptions::default()
            },
        )
        .expect("bounded recursive search");
    assert_eq!(search.matches.len(), 3);
    assert_eq!(search.matches[0].path.as_str(), "b.txt");
    assert_eq!(search.matches[0].line, 2);
    assert_eq!(search.matches[2].path.as_str(), "nested/c.txt");

    let nested_search = service
        .search_directory(Some("nested"), "needle", SearchOptions::default())
        .expect("bounded directory search");
    assert_eq!(nested_search.matches.len(), 1);
    assert_eq!(nested_search.matches[0].path.as_str(), "nested/c.txt");

    let revision = service
        .current_revision("a.txt")
        .expect("current file revision");
    assert_eq!(revision.fingerprint.byte_len, 5);

    assert!(matches!(
        service.read_lines(
            "b.txt",
            LinePageRequest {
                start_line: 0,
                limit: MAX_PAGE_SIZE + 1,
                expected_revision: None,
            }
        ),
        Err(FileServiceError::PageLimitExceeded {
            limit: MAX_PAGE_SIZE
        })
    ));
    assert!(matches!(
        service.search(
            "needle",
            SearchOptions {
                max_matches: 2,
                ..SearchOptions::default()
            }
        ),
        Err(FileServiceError::SearchLimitExceeded { limit: 2 })
    ));

    let line_body = (0..=MAX_LINE_COUNT).map(|_| "x\n").collect::<String>();
    fs::write(temp.path().join("too-many-lines.txt"), line_body).expect("write line-limit fixture");
    assert!(matches!(
        service.read_lines(
            "too-many-lines.txt",
            LinePageRequest {
                start_line: 0,
                limit: 1,
                expected_revision: None,
            }
        ),
        Err(FileServiceError::LineLimitExceeded {
            limit: MAX_LINE_COUNT
        })
    ));

    let permits = (0..MAX_CONCURRENT_OPERATIONS)
        .map(|_| {
            service
                .try_acquire_operation()
                .expect("admit up to the operation bound")
        })
        .collect::<Vec<_>>();
    assert!(matches!(
        service.list(None, 1),
        Err(FileServiceError::ConcurrencyLimitExceeded {
            limit: MAX_CONCURRENT_OPERATIONS
        })
    ));
    drop(permits);
    service
        .list(None, 8)
        .expect("operation admission should recover after permits drop");
}

fn create_directory_link(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).is_ok()
    }

    #[cfg(windows)]
    {
        if std::os::windows::fs::symlink_dir(target, link).is_ok() {
            return true;
        }
        let target = target.to_string_lossy();
        let link = link.to_string_lossy();
        std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link.as_ref())
            .arg(target.as_ref())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = (target, link);
        false
    }
}

#[test]
fn shipped_api_consumes_bounded_fixtures_without_root_path_disclosure() {
    let (temp, service) = service_root();
    fs::create_dir(temp.path().join("text")).expect("create text fixture directory");
    fs::create_dir(temp.path().join("secret")).expect("create secret fixture directory");
    fs::write(temp.path().join("text/hello.txt"), HELLO_FIXTURE).expect("copy text fixture");
    fs::write(temp.path().join("secret/.env.example"), SECRET_FIXTURE)
        .expect("copy secret fixture");

    let hello = service
        .read("text/hello.txt", ReadOptions::default())
        .expect("read shipped text fixture");
    assert_eq!(hello.content_kind, ContentKind::Text);
    assert_eq!(hello.revision.sha256, Some(sha256(HELLO_FIXTURE)));
    let entries = service
        .list(Some("secret"), 8)
        .expect("list secret fixture");
    assert_eq!(entries[0].secret, SecretClassification::SecretLike);
}

#[test]
fn bound_root_rejects_same_path_ordinary_directory_replacement() {
    let (temp, service) = service_root();
    fs::write(temp.path().join("inside.txt"), b"inside").expect("write bound file");
    let original = temp.path().to_path_buf();
    let moved_guard = tempfile::Builder::new()
        .prefix("devmanager-file-service-root-real-")
        .tempdir_in(original.parent().expect("root parent"))
        .expect("reserve moved root path");
    let moved = moved_guard.path().to_path_buf();
    fs::remove_dir(&moved).expect("remove reserved moved root path");
    fs::rename(&original, &moved).expect("move bound root aside");
    fs::create_dir(&original).expect("replace root at same path");
    fs::write(original.join("outside.txt"), b"outside").expect("write replacement root file");

    assert!(service.list(None, 8).is_err());
    assert!(service.read("inside.txt", ReadOptions::default()).is_err());
    assert!(service
        .plan_write("new.txt", b"write".to_vec(), ExpectedRevision::missing())
        .is_err());
    assert!(service
        .plan_delete("inside.txt", ExpectedRevision::missing())
        .is_err());

    fs::remove_file(original.join("outside.txt")).expect("remove replacement root file");
    fs::remove_dir(&original).expect("remove replacement root");
    fs::rename(moved, original).expect("restore bound root");
}

#[test]
fn parent_directory_replacement_is_rejected_for_read_write_and_delete() {
    let (temp, service) = service_root();
    let parent = temp.path().join("parent");
    fs::create_dir(&parent).expect("create parent");
    fs::write(parent.join("file.txt"), b"inside").expect("write target");
    let current = service
        .read("parent/file.txt", ReadOptions::default())
        .expect("read target");
    let write_plan = service
        .plan_write(
            "parent/file.txt",
            b"replacement".to_vec(),
            ExpectedRevision::exact(current.revision.clone()),
        )
        .expect("plan write");
    let delete_preview = service
        .plan_delete("parent/file.txt", ExpectedRevision::exact(current.revision))
        .expect("plan delete");

    let moved = temp.path().join("parent-real");
    fs::rename(&parent, &moved).expect("move original parent");
    fs::create_dir(&parent).expect("replace parent at same path");
    fs::write(parent.join("file.txt"), b"outside").expect("write replacement target");

    assert!(service
        .read("parent/file.txt", ReadOptions::default())
        .is_err());
    assert!(service.list(Some("parent"), 8).is_err());
    assert!(service.execute_write(write_plan).is_err());
    assert!(service.execute_delete(delete_preview).is_err());

    fs::remove_file(parent.join("file.txt")).expect("remove replacement parent file");
    fs::remove_dir(&parent).expect("remove replacement parent");
    fs::rename(moved, parent).expect("restore original parent");
}

#[cfg(unix)]
#[test]
fn write_rejects_a_parent_detached_after_final_resolution() {
    let (temp, service) = service_root();
    let outside = tempfile::tempdir().expect("create outside directory");
    let parent = temp.path().join("detached-parent");
    fs::create_dir(&parent).expect("create detached parent");
    let target = parent.join("file.txt");
    fs::write(&target, b"inside").expect("write detached target");
    let current = service
        .read("detached-parent/file.txt", ReadOptions::default())
        .expect("read detached target");
    let plan = service
        .plan_write(
            "detached-parent/file.txt",
            b"must-not-follow".to_vec(),
            ExpectedRevision::exact(current.revision),
        )
        .expect("plan detached write");
    let service = Arc::new(service);
    files::set_test_pause(files::TEST_PAUSE_BEFORE_RENAME);
    let worker = {
        let service = Arc::clone(&service);
        thread::spawn(move || service.execute_write(plan))
    };
    wait_for_test_pause();
    let moved = outside.path().join("detached-parent");
    fs::rename(&parent, &moved).expect("detach parent outside root");
    files::clear_test_pause();
    assert!(worker.join().expect("join detached write").is_err());
    assert_eq!(
        fs::read(moved.join("file.txt")).expect("outside target survives"),
        b"inside"
    );
}

#[cfg(unix)]
#[test]
fn delete_rejects_a_parent_detached_after_final_resolution() {
    let (temp, service) = service_root();
    let outside = tempfile::tempdir().expect("create outside directory");
    let parent = temp.path().join("detached-delete-parent");
    fs::create_dir(&parent).expect("create detached delete parent");
    let target = parent.join("file.txt");
    fs::write(&target, b"must-survive").expect("write detached delete target");
    let current = service
        .read("detached-delete-parent/file.txt", ReadOptions::default())
        .expect("read detached delete target");
    let preview = service
        .plan_delete(
            "detached-delete-parent/file.txt",
            ExpectedRevision::exact(current.revision),
        )
        .expect("plan detached delete");
    let service = Arc::new(service);
    files::set_test_pause(files::TEST_PAUSE_BEFORE_DELETE_EFFECT);
    let worker = {
        let service = Arc::clone(&service);
        thread::spawn(move || service.execute_delete(preview))
    };
    wait_for_test_pause();
    let moved = outside.path().join("detached-delete-parent");
    fs::rename(&parent, &moved).expect("detach delete parent outside root");
    files::clear_test_pause();
    assert!(worker.join().expect("join detached delete").is_err());
    assert_eq!(
        fs::read(moved.join("file.txt")).expect("outside delete target survives"),
        b"must-survive"
    );
}

#[test]
fn outside_hardlinks_are_not_accepted_as_workspace_files() {
    let (temp, service) = service_root();
    let outside = tempfile::tempdir().expect("create outside directory");
    fs::write(outside.path().join("outside.txt"), b"outside").expect("write outside file");
    fs::hard_link(
        outside.path().join("outside.txt"),
        temp.path().join("linked.txt"),
    )
    .expect("create outside hardlink");

    assert!(service.read("linked.txt", ReadOptions::default()).is_err());
    assert!(service
        .plan_write(
            "linked.txt",
            b"replacement".to_vec(),
            ExpectedRevision::missing(),
        )
        .is_err());
}

#[cfg(windows)]
#[test]
fn windows_file_identity_is_real_and_rejects_same_size_same_time_replacement() {
    let (temp, service) = service_root();
    let target = temp.path().join("identity.txt");
    fs::write(&target, b"aaaa").expect("write original");
    let original = service
        .read("identity.txt", ReadOptions::default())
        .expect("read original");
    assert_ne!(original.revision.fingerprint.identity.volume_or_device, 0);
    assert_ne!(original.revision.fingerprint.identity.file_or_inode, 0);

    let replacement = temp.path().join("replacement.txt");
    fs::write(&replacement, b"bbbb").expect("write same-size replacement");
    let metadata = fs::metadata(&target).expect("stat original");
    let times = std::fs::FileTimes::new()
        .set_accessed(metadata.accessed().expect("original access time"))
        .set_modified(metadata.modified().expect("original modified time"));
    std::fs::File::options()
        .write(true)
        .open(&replacement)
        .expect("open replacement")
        .set_times(times)
        .expect("preserve original times");
    fs::remove_file(&target).expect("remove original");
    fs::rename(&replacement, &target).expect("install replacement");

    let plan = service
        .plan_write(
            "identity.txt",
            b"after".to_vec(),
            ExpectedRevision::fingerprint(original.revision.fingerprint),
        )
        .expect_err("same-size/time replacement must conflict during planning");
    assert!(matches!(plan, FileServiceError::Conflict { .. }));
}

#[test]
fn concurrent_same_revision_writes_have_one_winner_and_one_conflict() {
    let (temp, service) = service_root();
    fs::write(temp.path().join("concurrent.txt"), b"before").expect("write original");
    let current = service
        .read("concurrent.txt", ReadOptions::default())
        .expect("read original");
    let first = service
        .plan_write(
            "concurrent.txt",
            b"first".to_vec(),
            ExpectedRevision::exact(current.revision.clone()),
        )
        .expect("plan first write");
    let second = service
        .plan_write(
            "concurrent.txt",
            b"second".to_vec(),
            ExpectedRevision::exact(current.revision),
        )
        .expect("plan second write");
    let service = Arc::new(service);
    let barrier = Arc::new(Barrier::new(3));
    let first_thread = {
        let service = Arc::clone(&service);
        let barrier = Arc::clone(&barrier);
        thread::spawn(move || {
            barrier.wait();
            service.execute_write(first)
        })
    };
    let second_thread = {
        let service = Arc::clone(&service);
        let barrier = Arc::clone(&barrier);
        thread::spawn(move || {
            barrier.wait();
            service.execute_write(second)
        })
    };
    barrier.wait();
    let first = first_thread.join().expect("first writer thread");
    let second = second_thread.join().expect("second writer thread");
    let successes = usize::from(first.is_ok()) + usize::from(second.is_ok());
    let conflicts = usize::from(matches!(first, Err(FileServiceError::Conflict { .. })))
        + usize::from(matches!(second, Err(FileServiceError::Conflict { .. })));
    assert_eq!(successes, 1, "exactly one writer must succeed");
    assert_eq!(conflicts, 1, "exactly one writer must conflict");
}

#[test]
fn mutation_plans_and_results_have_redacted_debug_output() {
    let (temp, service) = service_root();
    let secret_content = b"SECRET_SENTINEL_DO_NOT_PRINT";
    let plan = service
        .plan_write(
            ".env.local",
            secret_content.to_vec(),
            ExpectedRevision::missing(),
        )
        .expect("plan secret write");
    let plan_debug = format!("{plan:?}");
    let service_debug = format!("{service:?}");
    let root_text = temp.path().to_string_lossy().into_owned();
    assert!(!plan_debug.contains("SECRET_SENTINEL_DO_NOT_PRINT"));
    assert!(!plan_debug.contains(".env.local"));
    assert!(!plan_debug.contains(root_text.as_str()));
    assert!(!plan_debug.contains("<secret-like-path>"));
    assert!(!service_debug.contains(root_text.as_str()));

    fs::write(temp.path().join(".env.local"), b"existing").expect("write secret target");
    let revision = service
        .read(".env.local", ReadOptions::default())
        .expect("read secret target");
    let preview = service
        .plan_delete(".env.local", ExpectedRevision::exact(revision.revision))
        .expect("plan secret delete");
    let preview_debug = format!("{preview:?}");
    assert!(!preview_debug.contains(".env.local"));
    assert!(!preview_debug.contains(root_text.as_str()));
    assert!(!preview_debug.contains("<secret-like-path>"));
}

#[cfg(unix)]
#[test]
fn replacement_preserves_original_unix_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let (temp, service) = service_root();
    let target = temp.path().join("permissions.txt");
    fs::write(&target, b"before").expect("write original");
    fs::set_permissions(&target, fs::Permissions::from_mode(0o640))
        .expect("set original permissions");
    let current = service
        .read("permissions.txt", ReadOptions::default())
        .expect("read original");
    let plan = service
        .plan_write(
            "permissions.txt",
            b"after".to_vec(),
            ExpectedRevision::exact(current.revision),
        )
        .expect("plan write");
    service.execute_write(plan).expect("execute write");
    assert_eq!(
        fs::metadata(target)
            .expect("stat replacement")
            .permissions()
            .mode()
            & 0o777,
        0o640
    );
}

#[test]
fn revisions_include_permission_state_in_cas_and_settlement() {
    let (temp, service) = service_root();
    let target = temp.path().join("permission-cas.txt");
    fs::write(&target, b"before").expect("write original");
    let before = service
        .read("permission-cas.txt", ReadOptions::default())
        .expect("read original");
    let plan = service
        .plan_write(
            "permission-cas.txt",
            b"after".to_vec(),
            ExpectedRevision::exact(before.revision.clone()),
        )
        .expect("plan write");
    service.execute_write(plan).expect("execute write");
    let after = service
        .read("permission-cas.txt", ReadOptions::default())
        .expect("read replacement");
    assert_eq!(
        before.revision.fingerprint.permission_bits, after.revision.fingerprint.permission_bits,
        "replacement must preserve the permission fingerprint"
    );

    let current_permissions = fs::metadata(&target)
        .expect("stat replacement")
        .permissions();
    let mut changed_permissions = current_permissions.clone();
    changed_permissions.set_readonly(!current_permissions.readonly());
    fs::set_permissions(&target, changed_permissions).expect("change permission state");
    assert!(matches!(
        service.plan_write(
            "permission-cas.txt",
            b"must-conflict".to_vec(),
            ExpectedRevision::exact(after.revision),
        ),
        Err(FileServiceError::Conflict { .. })
    ));
}

#[test]
fn line_reads_bound_lines_before_materializing_oversized_content_and_handle_text_edges() {
    let (temp, service) = service_root();
    fs::write(temp.path().join("lines.txt"), "zero\r\nélan\r\nlast\n")
        .expect("write CRLF unicode fixture");
    let page = service
        .read_lines(
            "lines.txt",
            LinePageRequest {
                start_line: 0,
                limit: 8,
                expected_revision: None,
            },
        )
        .expect("read CRLF unicode lines");
    assert_eq!(page.total_lines, 3);
    assert_eq!(page.lines[1].text, "élan");

    fs::write(temp.path().join("binary.txt"), b"safe\0binary").expect("write NUL fixture");
    let nul_result = service.read_lines(
        "binary.txt",
        LinePageRequest {
            start_line: 0,
            limit: 1,
            expected_revision: None,
        },
    );
    assert!(
        matches!(&nul_result, Err(FileServiceError::BinaryContent { .. })),
        "NUL result: {nul_result:?}"
    );

    fs::write(temp.path().join("oversized-line.txt"), vec![b'x'; 300_000])
        .expect("write oversized line fixture");
    assert!(service
        .read_lines(
            "oversized-line.txt",
            LinePageRequest {
                start_line: 0,
                limit: 1,
                expected_revision: None,
            },
        )
        .is_err());
}

#[test]
fn line_reads_stop_at_the_initial_bound_when_the_file_grows() {
    let (temp, service) = service_root();
    let target = temp.path().join("growing-lines.txt");
    fs::write(&target, b"x\n").expect("write initial line fixture");
    let service = Arc::new(service);
    files::reset_test_line_read_bytes();
    files::set_test_pause(files::TEST_PAUSE_BEFORE_LINE_READ);
    let worker = {
        let service = Arc::clone(&service);
        thread::spawn(move || {
            service.read_lines(
                "growing-lines.txt",
                LinePageRequest {
                    start_line: 0,
                    limit: 1,
                    expected_revision: None,
                },
            )
        })
    };
    wait_for_test_pause();
    let mut grown = Vec::with_capacity(MAX_READ_BYTES + MAX_CHUNK_BYTES);
    for _ in 0..=MAX_LINE_COUNT {
        grown.extend_from_slice(b"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\n");
    }
    fs::write(&target, grown).expect("grow line fixture after initial fingerprint");
    files::clear_test_pause();
    let result = worker.join().expect("join growing line reader");
    assert!(matches!(
        result,
        Err(FileServiceError::ChangedDuringRead { .. })
    ));
    assert!(
        files::test_line_read_bytes() <= MAX_READ_BYTES,
        "line reader consumed more than the hard read bound"
    );
}

#[test]
fn paginated_reads_reject_directory_mutation_between_pages() {
    let (temp, service) = service_root();
    fs::write(temp.path().join("a.txt"), b"a").expect("write first entry");
    fs::write(temp.path().join("b.txt"), b"b").expect("write second entry");
    let first = service
        .list_page(
            None,
            FilePageRequest {
                offset: 0,
                limit: 1,
            },
        )
        .expect("read first page");
    let cursor = first.next_cursor.clone().expect("pagination cursor");
    fs::write(temp.path().join("aa.txt"), b"mutation").expect("mutate directory");
    assert!(matches!(
        service.list_page_with_cursor(
            None,
            cursor,
            FilePageRequest {
                offset: 1,
                limit: 1,
            },
        ),
        Err(FileServiceError::DirectoryChanged { .. })
    ));
}

#[test]
fn recursive_search_rejects_directory_depth_overflow() {
    let (temp, service) = service_root();
    let mut directory = temp.path().to_path_buf();
    for index in 0..=MAX_SEARCH_DEPTH {
        directory.push(format!("level-{index}"));
        fs::create_dir(&directory).expect("create bounded-depth fixture");
    }
    fs::write(directory.join("needle.txt"), b"needle").expect("write deep fixture");

    assert!(matches!(
        service.search("needle", SearchOptions::default()),
        Err(FileServiceError::SearchLimitExceeded {
            limit: MAX_SEARCH_DEPTH
        })
    ));
}

#[test]
fn attacker_controlled_paths_are_bounded_before_error_storage_or_display() {
    let (_temp, service) = service_root();
    let oversized = "A".repeat(128 * 1024);
    let error = service
        .normalize_relative_path(&oversized)
        .expect_err("an oversized raw path must be rejected");
    let rendered = error.to_string();
    assert!(rendered.len() < 256, "diagnostic grew with attacker input");
    assert!(
        !rendered.contains(&oversized),
        "diagnostic echoed attacker input"
    );
    if let FileServiceError::InvalidPath { path, .. } = error {
        assert!(
            path.len() < 256,
            "stored error path grew with attacker input"
        );
        assert!(
            !path.contains('A'),
            "stored error path echoed attacker input"
        );
    } else {
        panic!("unexpected error variant: {error:?}");
    }
}

#[test]
fn diagnostics_do_not_disclose_even_benign_workspace_paths() {
    let (_temp, service) = service_root();
    let error = service
        .read("attacker-controlled-name.txt", ReadOptions::default())
        .expect_err("missing path must fail");
    let rendered = error.to_string();
    assert!(
        !rendered.contains("attacker-controlled-name.txt"),
        "diagnostic disclosed attacker-controlled path: {rendered}"
    );
}

#[test]
fn directory_cursor_cannot_replay_across_services_bound_to_one_root() {
    let (temp, first) = service_root();
    fs::write(temp.path().join("a.txt"), b"a").expect("write first entry");
    fs::write(temp.path().join("b.txt"), b"b").expect("write second entry");
    let second = WorkspaceFileService::new_for_test(temp.path())
        .expect("bind a second service to the same root");
    let first_page = first
        .list_page(
            None,
            FilePageRequest {
                offset: 0,
                limit: 1,
            },
        )
        .expect("first page");
    let cursor = first_page.next_cursor.expect("cursor");
    assert!(matches!(
        second.list_page_with_cursor(
            None,
            cursor,
            FilePageRequest {
                offset: 1,
                limit: 1,
            },
        ),
        Err(FileServiceError::DirectoryChanged { .. })
    ));
}

#[test]
fn mutation_plan_rejects_every_mismatched_host_authority_dimension() {
    let (temp, service) = service_root();
    fs::write(temp.path().join("scoped.txt"), b"before").expect("write target");
    let current = service
        .read("scoped.txt", ReadOptions::default())
        .expect("read target");
    for variant in 1..=5 {
        let other =
            WorkspaceFileService::new_for_test_with_authority_dimension(temp.path(), variant)
                .expect("bind alternate authority");
        let plan = service
            .plan_write(
                "scoped.txt",
                b"after".to_vec(),
                ExpectedRevision::exact(current.revision.clone()),
            )
            .expect("plan write");
        assert!(matches!(
            other.execute_write(plan),
            Err(FileServiceError::ForeignPlan)
        ));
    }
}

#[test]
fn read_rejects_a_chunk_count_that_is_too_large_even_when_bytes_are_allowed() {
    let (temp, service) = service_root();
    let body = vec![b'x'; 8 * 1024];
    fs::write(temp.path().join("many-chunks.txt"), &body).expect("write chunk fixture");
    assert!(matches!(
        service.read(
            "many-chunks.txt",
            ReadOptions {
                chunk_bytes: 1,
                total_bytes: body.len(),
            }
        ),
        Err(FileServiceError::ChunkLimitExceeded { .. })
    ));
}

#[test]
fn internal_identity_and_mutation_caches_stay_bounded_under_churn() {
    let (_temp, service) = service_root();
    let (mutation_locks, directory_identities) = service
        .churn_caches_for_test(MAX_MUTATION_LOCKS + 64)
        .expect("cache churn should stay within the bounded contract");
    assert!(mutation_locks <= MAX_MUTATION_LOCKS);
    assert!(directory_identities <= MAX_DIRECTORY_IDENTITIES);
}

#[test]
fn line_page_continuation_rejects_mutation_against_the_exact_revision() {
    let (temp, service) = service_root();
    fs::write(temp.path().join("paged.txt"), b"one\ntwo\nthree\n").expect("write line fixture");
    let first = service
        .read_lines(
            "paged.txt",
            LinePageRequest {
                start_line: 0,
                limit: 1,
                expected_revision: None,
            },
        )
        .expect("first line page");
    fs::write(temp.path().join("paged.txt"), b"changed\ntwo\nthree\n")
        .expect("mutate line fixture");
    assert!(matches!(
        service.read_lines(
            "paged.txt",
            LinePageRequest {
                start_line: 1,
                limit: 1,
                expected_revision: Some(first.revision),
            },
        ),
        Err(FileServiceError::Conflict { .. }) | Err(FileServiceError::ChangedDuringRead { .. })
    ));
}
