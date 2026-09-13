use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use devmanager::domain::id::TaskId;
use devmanager::process::identity::{
    ManagedProcessId, ManagedProcessIdError, ManagedProcessIdentity, ManagedProcessIdentityError,
    ProcessOwner,
};

fn current_executable() -> PathBuf {
    std::env::current_exe().expect("current test executable")
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).expect("canonical test path")
}

#[test]
fn pid_reuse_does_not_match_identity() {
    let first = ManagedProcessId::new(4_242, 1_000).expect("first identity");
    let reused = ManagedProcessId::new(4_242, 2_000).expect("reused pid identity");
    assert_ne!(
        first, reused,
        "same PID with different creation time must not match"
    );

    let executable = current_executable();
    let first_root = ManagedProcessIdentity::new(first, executable.clone()).expect("first root");
    let reused_root = ManagedProcessIdentity::new(reused, executable).expect("reused root");
    assert!(
        !first_root.matches_root(&reused_root),
        "PID reuse must not match for destructive/root identity"
    );
}

#[test]
fn managed_process_id_exposes_read_only_components() {
    let id = ManagedProcessId::new(8_181, 44_000).expect("managed process id");
    assert_eq!(id.pid(), 8_181);
    assert_eq!(id.creation_time_100ns(), 44_000);
}

#[test]
fn managed_identity_rejects_zero_creation_time() {
    let err = ManagedProcessId::new(1, 0).expect_err("creation_time_100ns 0 must fail");
    assert_eq!(err, ManagedProcessIdError::ZeroCreationTime);
}

#[test]
fn managed_identity_rejects_zero_pid() {
    let err = ManagedProcessId::new(0, 1).expect_err("pid 0 must fail");
    assert_eq!(err, ManagedProcessIdError::ZeroPid);
}

#[test]
fn canonical_executable_participates_in_root_match() {
    let id = ManagedProcessId::new(7, 99).expect("id");
    let executable = current_executable();
    let other_file = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let left = ManagedProcessIdentity::new(id, executable.clone()).expect("left root");
    let mismatch = ManagedProcessIdentity::new(id, other_file).expect("mismatched root");
    assert!(
        !left.matches_root(&mismatch),
        "executable path mismatch must not match root identity"
    );

    let same = ManagedProcessIdentity::new(id, executable).expect("same root");
    assert!(
        left.matches_root(&same),
        "identical pid, creation time, and executable must match"
    );
}

#[test]
fn managed_process_identity_exposes_canonical_root_components() {
    let id = ManagedProcessId::new(17, 101).expect("id");
    let executable = current_executable();
    let identity =
        ManagedProcessIdentity::new(id, executable.clone()).expect("managed process identity");

    assert_eq!(identity.id(), id);
    assert_eq!(identity.canonical_executable(), canonical(&executable));
}

#[test]
fn syntactic_executable_aliases_match_same_root() {
    let id = ManagedProcessId::new(23, 404).expect("id");
    let executable = current_executable();
    let parent = executable.parent().expect("test executable parent");
    let file_name = executable.file_name().expect("test executable file name");
    let alias = parent.join(".").join(file_name);

    let direct = ManagedProcessIdentity::new(id, executable).expect("direct identity");
    let aliased = ManagedProcessIdentity::new(id, alias).expect("aliased identity");

    assert_eq!(
        direct.canonical_executable(),
        aliased.canonical_executable()
    );
    assert!(direct.matches_root(&aliased));
}

#[test]
fn missing_executable_path_is_rejected_with_context() {
    let id = ManagedProcessId::new(29, 505).expect("id");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time after epoch")
        .as_nanos();
    let missing = current_executable().with_file_name(format!(
        "devmanager-missing-{}-{nonce}.exe",
        std::process::id()
    ));
    assert!(!missing.exists(), "missing-path fixture must remain absent");

    let error: ManagedProcessIdentityError =
        ManagedProcessIdentity::new(id, missing.clone()).expect_err("missing path must fail");
    assert_eq!(error.path(), missing.as_path());
    assert_eq!(error.kind(), ErrorKind::NotFound);
    assert!(error
        .to_string()
        .contains("failed to canonicalize managed process executable"));
    assert!(!error.to_string().contains(&missing.display().to_string()));
}

#[test]
fn resource_has_exactly_one_owner() {
    let task_id = TaskId::new();
    let task_owner = ProcessOwner::Task(task_id);
    let host_owner = ProcessOwner::Host;

    match task_owner {
        ProcessOwner::Task(id) => assert_eq!(id, task_id),
        ProcessOwner::Host => panic!("Task ownership must not also be Host"),
    }

    match host_owner {
        ProcessOwner::Host => {}
        ProcessOwner::Task(_) => panic!("Host ownership must not also be Task"),
    }

    assert_ne!(
        task_owner, host_owner,
        "ownership is exactly one enum value, never simultaneous Task and Host"
    );
}
