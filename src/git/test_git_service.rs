use crate::git::command::{
    issue_git_host_binding, test_issue_git_host_binding, test_issue_git_host_binding_with_fence,
    GitCancellation, GitConfirmation, GitError, GitOperationPermit, GitRepository,
};
use crate::git::model::{
    parse_porcelain_v2_z, parse_porcelain_v2_z_limited, BranchName, CommitId, DiffLineKind,
    DiffMarker, DiffSide, FileState, GitCapability, MutationPlan, ObjectId, RepoFingerprint,
    RepoPath, ReviewComment, StatusKind,
};
use crate::git::review::{
    anchor_is_stale, parse_pr_url, parse_unified_diff, parse_unified_diff_limited,
    PullRequestProvider,
};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;
fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(repo)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("git executable");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}
fn test_tempdir(prefix: &str) -> TempDir {
    let root = Path::new(r"C:\Temp");
    fs::create_dir_all(root).expect("test temp root");
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(root)
        .expect("temporary fixture")
}
fn init_repo() -> TempDir {
    let repo = test_tempdir("devmanager-phase66-git-service-");
    git(repo.path(), &["init", "--initial-branch=main"]);
    git(repo.path(), &["config", "user.name", "Git Foundation Test"]);
    git(
        repo.path(),
        &["config", "user.email", "git-foundation@example.invalid"],
    );
    repo
}
fn commit_initial(repo: &Path, file: &str, body: &str) {
    fs::write(repo.join(file), body).expect("initial file");
    git(repo, &["add", "--", file]);
    git(repo, &["commit", "-m", "initial"]);
}
fn confirm<P: MutationPlan>(repo: &GitRepository, plan: &P) -> GitConfirmation {
    repo.test_confirm(plan).expect("test mutation capability")
}

#[test]
fn repository_confirm_cannot_self_authorize_a_mutation() {
    let repo_dir = init_repo();
    commit_initial(repo_dir.path(), "tracked.txt", "tracked\n");
    fs::write(repo_dir.path().join("tracked.txt"), "changed\n").expect("change tracked file");
    let repo = GitRepository::test_open(repo_dir.path()).expect("open repository");
    let plan = repo
        .plan_stage(&[RepoPath::from("tracked.txt")])
        .expect("stage plan");

    assert!(
        repo.confirm(&plan).is_err(),
        "a repository must not mint its own mutation authority"
    );
}

#[test]
fn service_mutation_without_an_operation_permit_fails_closed() {
    let repo_dir = init_repo();
    commit_initial(repo_dir.path(), "tracked.txt", "tracked\n");
    fs::write(repo_dir.path().join("tracked.txt"), "changed\n").expect("change tracked file");
    let repo = GitRepository::test_open(repo_dir.path()).expect("open repository");

    let error = repo
        .run_service_mutation(
            vec!["add".into(), "--".into(), "tracked.txt".into()],
            None,
            None,
        )
        .expect_err("legacy service mutation must require an operation permit");
    assert!(matches!(error, GitError::AuthorityUnavailable));
}

#[test]
fn test_service_permit_cannot_authorize_a_host_bound_repository() {
    let repo_dir = init_repo();
    commit_initial(repo_dir.path(), "tracked.txt", "tracked\n");
    fs::write(repo_dir.path().join("tracked.txt"), "changed\n").expect("change tracked file");
    let binding =
        test_issue_git_host_binding(repo_dir.path(), Vec::new()).expect("issue host binding");
    let repo =
        GitRepository::from_host_binding(binding, crate::git::command::GitCancellation::new())
            .expect("open host-bound repository");
    let arguments = vec!["add".into(), "--".into(), "tracked.txt".into()];
    let permit = GitOperationPermit::test_service_mutation(&arguments, None, None);

    let error = repo
        .run_service_mutation_with_permit(arguments, None, None, permit)
        .expect_err("a test permit must not authorize a host-bound repository");
    assert!(matches!(error, GitError::AuthorityUnavailable));
}

#[test]
fn service_mutation_permit_binds_the_exact_command_arguments() {
    let repo_dir = init_repo();
    commit_initial(repo_dir.path(), "tracked.txt", "tracked\n");
    fs::write(repo_dir.path().join("tracked.txt"), "changed\n").expect("change tracked file");
    let repo = GitRepository::test_open(repo_dir.path()).expect("open repository");
    let arguments = vec!["add".into(), "--".into(), "tracked.txt".into()];
    let permit = GitOperationPermit::test_service_mutation(&arguments, None, None);

    repo.run_service_mutation_with_permit(arguments, None, None, permit)
        .expect("exact test service permit must authorize the command");
    let status = repo.status().expect("read staged status");
    assert!(status
        .entries
        .iter()
        .any(|entry| entry.path == RepoPath::from("tracked.txt")));
}

#[test]
fn remote_service_mutation_requires_an_exact_endpoint_policy() {
    let repo_dir = init_repo();
    commit_initial(repo_dir.path(), "tracked.txt", "tracked\n");
    let repo = GitRepository::test_open(repo_dir.path()).expect("open repository");
    let arguments = vec![OsString::from("push")];
    let permit = GitOperationPermit::test_service_mutation(&arguments, None, None);

    let error = repo
        .run_service_mutation_with_permit(arguments, None, None, permit)
        .expect_err("remote service mutations must not use configured ambient remotes");
    assert!(matches!(error, GitError::AuthorityUnavailable));
}

#[test]
fn service_local_push_uses_a_retained_endpoint_lease() {
    let repo_dir = init_repo();
    commit_initial(repo_dir.path(), "tracked.txt", "tracked\n");
    git(repo_dir.path(), &["init", "--bare", "remote.git"]);
    git(repo_dir.path(), &["remote", "add", "origin", "remote.git"]);

    let repo = GitRepository::test_open(repo_dir.path()).expect("open repository");
    let policy = repo
        .service_remote_policy("origin")
        .expect("admit in-workspace local remote");
    assert!(
        policy.endpoint_lease().is_some(),
        "local policy must retain lease"
    );
    let debug = format!("{policy:?}");
    assert!(
        !debug.contains("remote.git"),
        "remote endpoint paths must stay out of Debug transport data"
    );
    let arguments = vec![
        OsString::from("push"),
        OsString::from("origin"),
        OsString::from("HEAD:refs/heads/main"),
    ];
    let permit = GitOperationPermit::test_service_mutation(
        &arguments,
        Some(policy.clone()),
        Some("origin".to_string()),
    );

    repo.run_service_mutation_with_permit(
        arguments,
        Some(policy),
        Some("origin".to_string()),
        permit,
    )
    .expect("authorized local push must execute through the bounded runner");
    let remote = repo_dir.path().join("remote.git");
    assert!(
        git(&remote, &["show-ref", "--verify", "refs/heads/main"]).contains("refs/heads/main"),
        "local push must update the temporary bare repository"
    );
}

#[test]
fn service_remote_permit_rejects_a_mismatched_remote_name() {
    let repo_dir = init_repo();
    commit_initial(repo_dir.path(), "tracked.txt", "tracked\n");
    git(repo_dir.path(), &["init", "--bare", "remote.git"]);
    git(repo_dir.path(), &["remote", "add", "origin", "remote.git"]);

    let repo = GitRepository::test_open(repo_dir.path()).expect("open repository");
    let policy = repo
        .service_remote_policy("origin")
        .expect("admit in-workspace local remote");
    let arguments = vec![OsString::from("push"), OsString::from("other")];
    let permit = GitOperationPermit::test_service_mutation(
        &arguments,
        Some(policy.clone()),
        Some("origin".to_string()),
    );

    let error = repo
        .run_service_mutation_with_permit(
            arguments,
            Some(policy),
            Some("origin".to_string()),
            permit,
        )
        .expect_err("service permit must bind the remote selected by Git arguments");
    assert!(matches!(error, GitError::RemoteNotAuthorized));
}

#[test]
fn service_remote_permit_rejects_an_option_value_masquerading_as_a_remote() {
    let repo_dir = init_repo();
    commit_initial(repo_dir.path(), "tracked.txt", "tracked\n");
    git(repo_dir.path(), &["init", "--bare", "remote.git"]);
    git(repo_dir.path(), &["remote", "add", "origin", "remote.git"]);

    let repo = GitRepository::test_open(repo_dir.path()).expect("open repository");
    let policy = repo
        .service_remote_policy("origin")
        .expect("admit in-workspace local remote");
    let arguments = vec![
        OsString::from("push"),
        OsString::from("--receive-pack"),
        OsString::from("origin"),
    ];
    let permit = GitOperationPermit::test_service_mutation(
        &arguments,
        Some(policy.clone()),
        Some("origin".to_string()),
    );

    let error = repo
        .run_service_mutation_with_permit(
            arguments,
            Some(policy),
            Some("origin".to_string()),
            permit,
        )
        .expect_err("option values must not select the authorized Git remote");
    assert!(matches!(error, GitError::RemoteNotAuthorized));
}

#[test]
fn service_reset_adopts_only_its_authorized_index_transition() {
    let repo_dir = init_repo();
    commit_initial(repo_dir.path(), "tracked.txt", "tracked\n");
    fs::write(repo_dir.path().join("tracked.txt"), "changed\n").expect("change tracked file");
    git(repo_dir.path(), &["add", "--", "tracked.txt"]);

    let repo = GitRepository::test_open(repo_dir.path()).expect("open repository");
    let arguments = vec![
        OsString::from("reset"),
        OsString::from("HEAD"),
        OsString::from("--"),
        OsString::from("tracked.txt"),
    ];
    let permit = GitOperationPermit::test_service_mutation(&arguments, None, None);

    repo.run_service_mutation_with_permit(arguments, None, None, permit)
        .expect("reset of an explicit path must authorize its index transition");
    let status = repo.status().expect("read reset status");
    assert!(status
        .entries
        .iter()
        .any(|entry| entry.path == RepoPath::from("tracked.txt")
            && entry.index == FileState::Unchanged));
}

#[test]
fn repository_graph_rejects_a_worktrees_directory_created_after_admission() {
    let repo_dir = init_repo();
    commit_initial(repo_dir.path(), "tracked.txt", "tracked\n");
    let repository = GitRepository::test_open(repo_dir.path()).expect("open repository");
    let worktrees = repo_dir.path().join(".git").join("worktrees");
    assert!(
        !worktrees.exists(),
        "fixture must start without worktree metadata"
    );

    fs::create_dir_all(&worktrees).expect("create late worktree metadata root");
    let error = repository
        .status()
        .expect_err("a late worktrees root must not enter a read operation");
    assert!(matches!(error, GitError::InvalidRepositoryRoot { .. }));
}
#[test]
fn parses_branch_head_upstream_ahead_behind_and_all_porcelain_states() {
    let fixture = b"# branch.oid 0123456789012345678901234567890123456789\n# branch.head main\n# branch.upstream origin/main\n# branch.ab +2 -1\n1 .M N... 100644 100644 100644 1111111 2222222 worktree.txt\0\
1 M. N... 100644 100644 100644 4444444 5555555 staged.txt\0\
2 R. N... 100644 100644 100644 7777777 8888888 R100 renamed.txt\0old.txt\0\
2 C. N... 100644 100644 100644 9999999 aaaaaaa C075 copied.txt\0source.txt\0\
1 .M S.M. 160000 160000 160000 bbbbbbb ccccccc submodule\0\
u UU N... 100644 100644 100644 100644 eeeeeee fffffff 0000000 conflict.txt\0\
? untracked.txt\0\
? non-utf8-\xff.txt\0";
    let status = parse_porcelain_v2_z(fixture).expect("porcelain fixture");
    assert_eq!(status.branch.as_ref().map(BranchName::as_str), Some("main"));
    assert_eq!(status.upstream.as_deref(), Some("origin/main"));
    assert_eq!(status.ahead, 2);
    assert_eq!(status.behind, 1);
    assert_eq!(
        status.head,
        Some(ObjectId::from("0123456789012345678901234567890123456789"))
    );
    assert_eq!(status.entries.len(), 8);
    let worktree = status.entry("worktree.txt").expect("worktree entry");
    assert_eq!(worktree.worktree, FileState::Modified);
    assert_eq!(worktree.index, FileState::Unchanged);
    let staged = status.entry("staged.txt").expect("staged entry");
    assert_eq!(staged.index, FileState::Modified);
    assert_eq!(staged.worktree, FileState::Unchanged);
    let renamed = status.entry("renamed.txt").expect("rename entry");
    assert_eq!(renamed.kind, StatusKind::Renamed);
    assert_eq!(renamed.original_path, Some(RepoPath::from("old.txt")));
    assert_eq!(renamed.rename_score, Some(100));
    let copied = status.entry("copied.txt").expect("copy entry");
    assert_eq!(copied.kind, StatusKind::Copied);
    assert_eq!(copied.original_path, Some(RepoPath::from("source.txt")));
    let submodule = status.entry("submodule").expect("submodule entry");
    assert_eq!(submodule.kind, StatusKind::Submodule);
    assert!(submodule
        .submodule
        .as_ref()
        .is_some_and(|state| state.worktree_modified));
    let conflict = status.entry("conflict.txt").expect("conflict entry");
    assert_eq!(conflict.kind, StatusKind::Conflict);
    assert_eq!(conflict.index, FileState::Unmerged);
    assert_eq!(conflict.worktree, FileState::Unmerged);
    let untracked = status.entry("untracked.txt").expect("untracked entry");
    assert_eq!(untracked.kind, StatusKind::Untracked);
    assert_eq!(untracked.worktree, FileState::Untracked);
    let raw_path = status
        .entries
        .iter()
        .find(|entry| entry.path.as_bytes().ends_with(b"\xff.txt"))
        .expect("raw path entry");
    assert_eq!(raw_path.path.as_bytes(), b"non-utf8-\xff.txt");
}
#[test]
fn parses_detached_head_initial_and_empty_status_without_loss() {
    let fixture = b"# branch.oid (initial)\n# branch.head (detached)\0";
    let status = parse_porcelain_v2_z(fixture).expect("initial detached fixture");
    assert!(status.head.is_none());
    assert!(status.branch.is_none());
    assert!(status.is_detached);
    assert_eq!(status.fingerprint.head, None);
    assert_eq!(status.fingerprint.status_digest.len(), 64);
}
#[test]
fn parses_text_diff_blob_ids_sides_hunks_and_stable_anchors() {
    let diff = br#"diff --git a/old.txt b/new.txt
similarity index 85%
rename from old.txt
rename to new.txt
index 1111111111111111111111111111111111111111..2222222222222222222222222222222222222222 100644
--- a/old.txt
+++ b/new.txt
@@ -1,2 +1,3 @@ heading
 keep
-before
+after
+added
\ No newline at end of file
"#;
    let document = parse_unified_diff(diff).expect("text diff");
    assert_eq!(document.files.len(), 1);
    let file = &document.files[0];
    assert_eq!(file.old_path, Some(RepoPath::from("old.txt")));
    assert_eq!(file.new_path, Some(RepoPath::from("new.txt")));
    assert_eq!(
        file.old_blob,
        Some(ObjectId::from("1111111111111111111111111111111111111111"))
    );
    assert_eq!(
        file.new_blob,
        Some(ObjectId::from("2222222222222222222222222222222222222222"))
    );
    assert!(!file.is_binary);
    assert_eq!(file.hunks.len(), 1);
    assert_eq!(file.hunks[0].lines[0].kind, DiffLineKind::Context);
    assert_eq!(file.hunks[0].lines[1].kind, DiffLineKind::Delete);
    assert_eq!(file.hunks[0].lines[2].kind, DiffLineKind::Add);
    assert_eq!(file.hunks[0].lines[2].new_line, Some(2));
    assert!(file.markers.contains(&DiffMarker::NoNewlineAtEndOfFile));
    let anchor = crate::git::model::ReviewAnchor::new(
        RepoPath::from("old.txt"),
        ObjectId::from("1111111111111111111111111111111111111111"),
        DiffSide::Old,
        2,
    );
    assert!(!anchor_is_stale(&document, &anchor));
    let comment = ReviewComment::new(anchor.clone(), "Please keep this line covered.")
        .expect("anchored review comment");
    assert_eq!(comment.anchor, anchor);
}
#[test]
fn parses_binary_and_marks_large_diff_as_truncated() {
    let binary = br#"diff --git a/image.bin b/image.bin
index aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa..bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb 100644
GIT binary patch
literal 4
Lc${Nk!<Z
"#;
    let document = parse_unified_diff(binary).expect("binary diff");
    assert!(document.files[0].is_binary);
    assert!(document.files[0].markers.contains(&DiffMarker::Binary));
    let text = b"diff --git a/large.txt b/large.txt\nindex a..b 100644\n--- a/large.txt\n+++ b/large.txt\n@@ -1 +1 @@\n-old\n+new\n";
    let limited = parse_unified_diff_limited(text, 32).expect("limited diff");
    assert!(limited.truncated);
    assert!(limited.markers.contains(&DiffMarker::Truncated));
}
#[test]
fn changed_base_blob_makes_review_anchor_stale() {
    let original = b"diff --git a/a.txt b/a.txt\nindex 1111111111111111111111111111111111111111..2222222222222222222222222222222222222222 100644\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-old\n+new\n";
    let changed = b"diff --git a/a.txt b/a.txt\nindex 3333333333333333333333333333333333333333..4444444444444444444444444444444444444444 100644\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-old\n+newer\n";
    let anchor = crate::git::model::ReviewAnchor::new(
        RepoPath::from("a.txt"),
        ObjectId::from("1111111111111111111111111111111111111111"),
        DiffSide::New,
        1,
    );
    assert!(anchor_is_stale(
        &parse_unified_diff(changed).expect("changed diff"),
        &anchor
    ));
    assert!(!anchor_is_stale(
        &parse_unified_diff(original).expect("original diff"),
        &anchor
    ));
}
#[test]
fn parses_common_pull_request_urls_as_data_only() {
    let github = parse_pr_url("https://github.com/acme/widget/pull/42").expect("github PR");
    assert_eq!(github.host, "github.com");
    assert_eq!(github.owner, "acme");
    assert_eq!(github.repository, "widget");
    assert_eq!(github.number, 42);
    let gitlab = parse_pr_url("https://gitlab.example/acme/widget/-/merge_requests/7?view=1")
        .expect("gitlab MR");
    assert_eq!(gitlab.owner, "acme");
    assert_eq!(gitlab.repository, "widget");
    assert_eq!(gitlab.number, 7);
    let azure =
        parse_pr_url("https://dev.azure.com/acme/platform/_git/widget/pullrequest/9?view=1")
            .expect("azure PR");
    assert_eq!(azure.owner, "acme/platform");
    assert_eq!(azure.repository, "widget");
    assert_eq!(azure.number, 9);
    assert!(parse_pr_url("https://example.com/acme/widget/issues/42").is_none());
    assert!(parse_pr_url("file:///acme/widget/pull/42").is_none());
}
#[test]
fn rejects_non_repository_roots_before_running_git() {
    let folder = test_tempdir("devmanager-phase66-git-invalid-");
    let error = GitRepository::test_open(folder.path()).expect_err("non-repository must fail");
    assert!(matches!(error, GitError::InvalidRepositoryRoot { .. }));
}
#[test]
fn repository_graph_revalidates_mutable_head_before_read() {
    let repo_dir = init_repo();
    commit_initial(repo_dir.path(), "tracked.txt", "tracked\n");
    let repository = GitRepository::test_open(repo_dir.path()).expect("open repository");
    let head = repo_dir.path().join(".git").join("HEAD");
    fs::write(&head, "ref: refs/heads/attacker\n").expect("replace HEAD");
    let error = repository
        .status()
        .expect_err("a mutable Git input replacement must fail closed");
    assert!(
        matches!(error, GitError::InvalidRepositoryRoot { .. }),
        "unexpected error: {error:?}"
    );
}
#[test]
fn selective_stage_preserves_unselected_changes() {
    let repo_dir = init_repo();
    commit_initial(repo_dir.path(), "a.txt", "a\n");
    fs::write(repo_dir.path().join("a.txt"), "changed\n").expect("change a");
    fs::write(repo_dir.path().join("b.txt"), "untracked\n").expect("create b");
    let repo = GitRepository::test_open(repo_dir.path()).expect("open repository");
    let before = repo.status().expect("status before");
    let plan = repo
        .plan_stage(&[RepoPath::from("a.txt")])
        .expect("stage plan");
    assert_eq!(plan.files, vec![RepoPath::from("a.txt")]);
    assert_eq!(plan.expected, before.fingerprint);
    let confirmation = confirm(&repo, &plan);
    repo.stage(&plan, &confirmation).expect("selective stage");
    let after = repo.status().expect("status after");
    assert_eq!(
        after.entry("a.txt").expect("a status").index,
        FileState::Modified
    );
    assert_eq!(
        after.entry("b.txt").expect("b status").kind,
        StatusKind::Untracked
    );
    assert_eq!(
        after.entry("b.txt").expect("b status").index,
        FileState::Unchanged
    );
}
#[test]
fn unstage_plan_only_removes_the_requested_index_entry() {
    let repo_dir = init_repo();
    commit_initial(repo_dir.path(), "a.txt", "a\n");
    commit_initial(repo_dir.path(), "b.txt", "b\n");
    fs::write(repo_dir.path().join("a.txt"), "changed a\n").expect("change a");
    fs::write(repo_dir.path().join("b.txt"), "changed b\n").expect("change b");
    git(repo_dir.path(), &["add", "--", "a.txt", "b.txt"]);
    let repo = GitRepository::test_open(repo_dir.path()).expect("open repository");
    let plan = repo
        .plan_unstage(&[RepoPath::from("a.txt")])
        .expect("unstage plan");
    let confirmation = confirm(&repo, &plan);
    repo.unstage(&plan, &confirmation)
        .unwrap_or_else(|error| match error {
            GitError::InvalidRepositoryRoot { reason, .. } => {
                panic!("selective unstage: {reason}")
            }
            error => panic!("selective unstage: {error}"),
        });
    let status = repo.status().expect("status after");
    assert_eq!(
        status.entry("a.txt").expect("a status").index,
        FileState::Unchanged
    );
    assert_eq!(
        status.entry("a.txt").expect("a status").worktree,
        FileState::Modified
    );
    assert_eq!(
        status.entry("b.txt").expect("b status").index,
        FileState::Modified
    );
}

#[test]
fn unstage_before_the_first_commit_removes_only_the_index_entry() {
    let repo_dir = init_repo();
    fs::write(repo_dir.path().join("a.txt"), "new file\n").expect("write new file");
    let repo = GitRepository::test_open(repo_dir.path()).expect("open repository");
    let stage = repo
        .plan_stage(&[RepoPath::from("a.txt")])
        .expect("stage plan");
    repo.stage(&stage, &confirm(&repo, &stage)).expect("stage");

    let unstage = repo
        .plan_unstage(&[RepoPath::from("a.txt")])
        .expect("unstage plan");
    assert_eq!(unstage.arguments().first().map(String::as_str), Some("rm"));
    repo.unstage(&unstage, &confirm(&repo, &unstage))
        .unwrap_or_else(|error| match error {
            GitError::InvalidRepositoryRoot { reason, .. } => {
                panic!("unstage before first commit: {reason}")
            }
            error => panic!("unstage before first commit: {error}"),
        });

    let status = repo.status().expect("status after unstage");
    let entry = status.entry("a.txt").expect("untracked worktree file");
    assert_eq!(entry.index, FileState::Unchanged);
    assert_eq!(entry.kind, StatusKind::Untracked);
}

#[test]
fn expected_fingerprint_rejects_external_drift_before_stage() {
    let repo_dir = init_repo();
    commit_initial(repo_dir.path(), "a.txt", "a\n");
    fs::write(repo_dir.path().join("a.txt"), "first\n").expect("first change");
    let repo = GitRepository::test_open(repo_dir.path()).expect("open repository");
    let plan = repo
        .plan_stage(&[RepoPath::from("a.txt")])
        .expect("stage plan");
    fs::write(repo_dir.path().join("a.txt"), "external\n").expect("external change");
    let confirmation = confirm(&repo, &plan);
    let error = repo
        .stage(&plan, &confirmation)
        .expect_err("stale plan must fail");
    assert!(matches!(error, GitError::FingerprintMismatch { .. }));
    assert!(git(repo_dir.path(), &["diff", "--cached", "--quiet"]).is_empty());
}
#[test]
fn expected_fingerprint_rejects_untracked_content_drift_before_stage() {
    let repo_dir = init_repo();
    commit_initial(repo_dir.path(), "a.txt", "a\n");
    fs::write(repo_dir.path().join("new.txt"), "first\n").expect("untracked file");
    let repo = GitRepository::test_open(repo_dir.path()).expect("open repository");
    let plan = repo
        .plan_stage(&[RepoPath::from("new.txt")])
        .expect("stage plan");
    fs::write(repo_dir.path().join("new.txt"), "external\n").expect("external change");
    let confirmation = confirm(&repo, &plan);
    let error = repo
        .stage(&plan, &confirmation)
        .expect_err("stale untracked plan must fail");
    assert!(matches!(error, GitError::FingerprintMismatch { .. }));
}
#[test]
fn commit_plan_exposes_exact_files_and_message_and_advances_head() {
    let repo_dir = init_repo();
    commit_initial(repo_dir.path(), "a.txt", "a\n");
    fs::write(repo_dir.path().join("a.txt"), "committed\n").expect("change");
    let repo = GitRepository::test_open(repo_dir.path()).expect("open repository");
    let stage = repo
        .plan_stage(&[RepoPath::from("a.txt")])
        .expect("stage plan");
    let stage_confirmation = confirm(&repo, &stage);
    repo.stage(&stage, &stage_confirmation).expect("stage");
    let before = repo.status().expect("status");
    let plan = repo.plan_commit("safe commit").expect("commit plan");
    assert_eq!(plan.files, vec![RepoPath::from("a.txt")]);
    assert_eq!(plan.message, "safe commit");
    assert_eq!(plan.expected, before.fingerprint);
    let confirmation = confirm(&repo, &plan);
    repo.commit(&plan, &confirmation).expect("commit");
    assert_eq!(
        git(repo_dir.path(), &["log", "-1", "--format=%s"]).trim(),
        "safe commit"
    );
    assert!(repo.status().expect("clean status").entries.is_empty());
}
#[test]
fn push_plan_requires_upstream_and_rejects_external_local_bare_remote() {
    let repo_dir = init_repo();
    commit_initial(repo_dir.path(), "a.txt", "a\n");
    let bare = test_tempdir("devmanager-phase66-git-bare-");
    git(bare.path(), &["init", "--bare"]);
    let bare_url = bare.path().to_string_lossy().into_owned();
    git(repo_dir.path(), &["remote", "add", "origin", &bare_url]);
    let repo = GitRepository::test_open(repo_dir.path()).expect("open repository");
    let no_upstream = repo.plan_push(None, None).expect_err("upstream is absent");
    assert!(matches!(no_upstream, GitError::NoUpstream { .. }));
    let error = repo
        .plan_push(Some("origin"), Some("main"))
        .expect_err("external local remotes are outside the repository authority");
    assert!(matches!(error, GitError::InvalidRequest { .. }));
}
#[test]
fn push_plan_rejects_embedded_remote_credentials() {
    let repo_dir = init_repo();
    commit_initial(repo_dir.path(), "a.txt", "a\n");
    let repo = GitRepository::test_open(repo_dir.path()).expect("open repository");
    let error = repo
        .plan_push(
            Some("https://alice:embedded-secret@example.invalid/repo"),
            Some("main"),
        )
        .expect_err("remote credentials must not enter a public push plan");
    let message = error.to_string();
    assert!(matches!(error, GitError::InvalidRequest { .. }));
    assert!(!message.contains("embedded-secret"));
}
#[test]
fn push_plan_rejects_external_local_remote_before_spawn() {
    let repo_dir = init_repo();
    commit_initial(repo_dir.path(), "a.txt", "a\n");
    let bare = test_tempdir("devmanager-phase66-git-bare-");
    git(bare.path(), &["init", "--bare"]);
    let bare_url = bare.path().to_string_lossy().into_owned();
    git(repo_dir.path(), &["remote", "add", "origin", &bare_url]);
    let repo = GitRepository::test_open(repo_dir.path()).expect("open repository");
    let error = repo
        .plan_push(Some("origin"), Some("main"))
        .expect_err("external local remotes must be rejected before spawn");
    assert!(matches!(error, GitError::InvalidRequest { .. }));
}
#[test]
fn fingerprint_contains_head_and_stable_status_digest() {
    let repo_dir = init_repo();
    commit_initial(repo_dir.path(), "a.txt", "a\n");
    let repo = GitRepository::test_open(repo_dir.path()).expect("open repository");
    let clean = repo.status().expect("clean status");
    let clean_again = repo.status().expect("clean status again");
    assert_eq!(clean.fingerprint, clean_again.fingerprint);
    assert_eq!(clean.fingerprint.head, clean.head);
    fs::write(repo_dir.path().join("a.txt"), "changed\n").expect("change");
    let dirty = repo.status().expect("dirty status");
    assert_ne!(
        dirty.fingerprint.status_digest,
        clean.fingerprint.status_digest
    );
    assert_ne!(
        dirty.fingerprint,
        RepoFingerprint {
            head: clean.head,
            status_digest: clean.fingerprint.status_digest
        }
    );
}
#[test]
fn no_destructive_whole_repository_commands_are_in_plan_arguments() {
    let repo_dir = init_repo();
    commit_initial(repo_dir.path(), "a.txt", "a\n");
    fs::write(repo_dir.path().join("a.txt"), "changed\n").expect("change");
    let repo = GitRepository::test_open(repo_dir.path()).expect("open repository");
    let stage = repo
        .plan_stage(&[RepoPath::from("a.txt")])
        .expect("stage plan");
    assert!(!stage
        .arguments()
        .iter()
        .any(|arg| arg == "reset" || arg == "clean"));
    let stage_confirmation = confirm(&repo, &stage);
    repo.stage(&stage, &stage_confirmation).expect("stage");
    let unstage = repo
        .plan_unstage(&[RepoPath::from("a.txt")])
        .expect("unstage plan");
    assert!(!unstage
        .arguments()
        .iter()
        .any(|arg| arg == "reset" || arg == "clean"));
}
#[test]
fn path_plans_reject_absolute_and_parent_paths() {
    let repo_dir = init_repo();
    commit_initial(repo_dir.path(), "a.txt", "a\n");
    let repo = GitRepository::test_open(repo_dir.path()).expect("open repository");
    let absolute = RepoPath::from_path(PathBuf::from("C:\\outside.txt"));
    assert!(repo.plan_stage(&[absolute]).is_err());
    assert!(repo
        .plan_stage(&[RepoPath::from("../outside.txt")])
        .is_err());
    assert!(repo.plan_stage(&[RepoPath::from(".")]).is_err());
}
#[test]
fn command_errors_redact_credentials_and_absolute_paths() {
    let error = GitError::CommandFailed {
        operation: "push".to_string(),
        code: Some(128),
        stderr: "fatal: https://alice:redacted-secret@example.invalid/repo C:\\Users\\micro\\private.txt"
            .to_string(),
    };
    let message = error.to_string();
    assert!(!message.contains("redacted-secret"));
    assert!(!message.contains("C:\\Users\\micro\\private.txt"));
    assert!(message.contains("<secret>"));
    assert!(message.contains("<path>"));
}
#[test]
fn review_comments_reject_escaping_anchor_paths() {
    let anchor = crate::git::model::ReviewAnchor::new(
        RepoPath::from("../outside.txt"),
        ObjectId::from("1111111111111111111111111111111111111111"),
        DiffSide::New,
        1,
    );
    assert!(ReviewComment::new(anchor, "Do not review outside this workspace.").is_err());
}
#[test]
fn quoted_unicode_binary_diff_preserves_exact_paths() {
    let diff = "diff --git \"a/naïve file.bin\" \"b/naïve file.bin\"\n"
        .to_string()
        + "index aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa..bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb 100644\n"
        + "GIT binary patch\n";
    let document = parse_unified_diff(diff.as_bytes()).expect("quoted Unicode diff");
    let file = document.files.first().expect("binary file");
    assert_eq!(file.old_path, Some(RepoPath::from("naïve file.bin")));
    assert_eq!(file.new_path, Some(RepoPath::from("naïve file.bin")));
    assert!(file.is_binary);
}
#[test]
fn mutation_plans_reject_symlink_targets_outside_workspace() {
    let repo_dir = init_repo();
    commit_initial(repo_dir.path(), "tracked.txt", "tracked\n");
    let outside = test_tempdir("devmanager-phase66-git-outside-");
    let outside_file = outside.path().join("secret.txt");
    fs::write(&outside_file, "outside\n").expect("outside file");
    let link = repo_dir.path().join("escape.txt");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside_file, &link).expect("symlink support");
    #[cfg(windows)]
    if std::os::windows::fs::symlink_file(&outside_file, &link).is_err() {
        eprintln!("skipping symlink containment test: symlink creation is unavailable");
        return;
    }
    let repo = GitRepository::test_open(repo_dir.path()).expect("open repository");
    let error = repo
        .plan_stage(&[RepoPath::from("escape.txt")])
        .expect_err("outside symlink target must be rejected");
    assert!(matches!(error, GitError::InvalidPath { .. }));
}
#[test]
fn cancelled_repository_rejects_work_before_spawning_git() {
    let repo_dir = init_repo();
    commit_initial(repo_dir.path(), "tracked.txt", "tracked\n");
    let repo = GitRepository::test_open(repo_dir.path()).expect("open repository");
    repo.cancel();
    let error = repo
        .status()
        .expect_err("cancelled status must not spawn Git");
    assert!(matches!(error, GitError::Cancelled { .. }));
}
#[test]
fn branch_and_commit_identifiers_are_validated_types() {
    let branch = BranchName::new("feature/naïve").expect("valid Unicode branch");
    assert_eq!(branch.as_str(), "feature/naïve");
    assert!(BranchName::new("").is_err());
    assert!(BranchName::new("feature\0secret").is_err());
    let commit: CommitId = CommitId::from("1111111111111111111111111111111111111111");
    assert_eq!(commit.as_str().len(), 40);
}
#[test]
fn porcelain_parser_rejects_input_above_the_declared_bound() {
    let fixture = b"# branch.head main\0? Unicode-\xc3\xaf.txt\0";
    assert!(parse_porcelain_v2_z_limited(fixture, fixture.len() - 1)
        .expect_err("parser must reject over-bound input")
        .contains("bound"));
    assert!(parse_porcelain_v2_z(fixture).is_ok());
}
#[test]
fn repository_root_preserves_trailing_unicode_space_in_canonical_path() {
    let parent = test_tempdir("devmanager-phase66-git-parent-");
    let root = parent.path().join("workspace\u{00a0}");
    fs::create_dir(&root).expect("workspace with trailing space");
    git(&root, &["init", "--initial-branch=main"]);
    git(&root, &["config", "user.name", "Git Foundation Test"]);
    git(
        &root,
        &["config", "user.email", "git-foundation@example.invalid"],
    );
    let repository = GitRepository::test_open(&root).expect("root path must remain exact");
    assert_eq!(repository.root(), fs::canonicalize(root).unwrap());
}
#[test]
fn read_command_plans_carry_workspace_identity_and_explicit_bounds() {
    let repo_dir = init_repo();
    commit_initial(repo_dir.path(), "tracked.txt", "tracked\n");
    let repo = GitRepository::test_open(repo_dir.path()).expect("open repository");
    let status = repo.plan_status();
    assert_eq!(status.workspace().cwd(), repo.root());
    assert!(!status.workspace().id().is_empty());
    assert_eq!(status.arguments()[0], "status");
    let diff = repo.plan_diff(false, 128).expect("diff plan");
    assert_eq!(diff.workspace().cwd(), repo.root());
    assert_eq!(diff.max_bytes, 128);
    assert!(diff.arguments().contains(&"--no-ext-diff".to_string()));
    let review = repo.plan_review(true, 256).expect("review plan");
    assert_eq!(review.workspace().id(), status.workspace().id());
    assert!(review.arguments().contains(&"--cached".to_string()));
}
#[test]
fn bounded_review_exposes_a_continuation_offset() {
    let text = b"diff --git a/a.txt b/a.txt\nindex a..b 100644\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-old\n+new\n";
    let document = parse_unified_diff_limited(text, 32).expect("bounded diff");
    assert!(document.truncated);
    assert_eq!(document.bytes_read, 32);
    assert_eq!(
        document
            .continuation
            .as_ref()
            .map(|continuation| continuation.next_offset),
        Some(32)
    );
}
#[test]
fn mutation_execution_requires_repository_issued_plan_confirmation() {
    let repo_dir = init_repo();
    commit_initial(repo_dir.path(), "tracked.txt", "tracked\n");
    fs::write(repo_dir.path().join("tracked.txt"), "changed\n").expect("change tracked file");
    let repo = GitRepository::test_open(repo_dir.path()).expect("open repository");
    let plan = repo
        .plan_stage(&[RepoPath::from("tracked.txt")])
        .expect("stage plan");
    let confirmation = repo
        .test_confirm(&plan)
        .expect("test-issued repository capability");
    assert_eq!(confirmation.capability(), GitCapability::Stage);
    repo.stage(&plan, &confirmation)
        .expect("confirmed stage must execute");
    assert_eq!(plan.capability(), GitCapability::Stage);
}
#[test]
fn pull_request_plan_is_provider_typed_and_dry_by_construction() {
    let repo_dir = init_repo();
    commit_initial(repo_dir.path(), "tracked.txt", "tracked\n");
    let repo = GitRepository::test_open(repo_dir.path()).expect("open repository");
    let plan = repo
        .plan_pull_request(
            PullRequestProvider::GitHub,
            "https://github.com/acme/widget.git",
            Some("feature/demo"),
            "main",
            "Ship the change",
            "Body stays in the typed invocation.",
        )
        .unwrap_or_else(|error| match error {
            GitError::InvalidRepositoryRoot { reason, .. } => {
                panic!("pull request plan: {reason}")
            }
            error => panic!("pull request plan: {error}"),
        });
    assert_eq!(plan.provider, PullRequestProvider::GitHub);
    assert_eq!(plan.executable(), "gh");
    assert_eq!(plan.workspace().cwd(), repo.root());
    assert!(plan.arguments().contains(&"--head".to_string()));
    assert!(plan.arguments().contains(&"feature/demo".to_string()));
    assert!(repo.test_confirm(&plan).is_ok());
}
#[test]
fn branch_names_reject_git_ref_ambiguity_and_invalid_components() {
    for invalid in [
        "@",
        "feature..two",
        "feature@{two}",
        "feature/.hidden",
        "feature/",
        "feature.lock",
        "feature.LOCK",
        "feature~two",
        "feature^two",
        "feature:two",
        "feature?two",
        "feature*two",
        "feature[two",
        "feature\\two",
        "feature two",
    ] {
        assert!(
            BranchName::new(invalid).is_err(),
            "{invalid} must be rejected"
        );
    }
    assert!(BranchName::new("feature/demo").is_ok());
}
#[test]
fn legacy_service_rejects_unsafe_local_hooks_before_commit() {
    let repo_dir = init_repo();
    commit_initial(repo_dir.path(), "tracked.txt", "tracked\n");
    fs::write(repo_dir.path().join("tracked.txt"), "changed\n").expect("change tracked file");
    git(repo_dir.path(), &["add", "--", "tracked.txt"]);
    let hook_dir = test_tempdir("devmanager-phase66-git-hook-");
    let marker = hook_dir.path().join("hook-ran.txt");
    let hook = if cfg!(windows) {
        hook_dir.path().join("pre-commit")
    } else {
        hook_dir.path().join("pre-commit")
    };
    fs::write(
        &hook,
        format!(
            "#!/bin/sh\nprintf 'unsafe hook ran\\n' > '{}'\n",
            marker.display()
        ),
    )
    .expect("write hook");
    git(
        repo_dir.path(),
        &[
            "config",
            "core.hooksPath",
            hook_dir.path().to_str().unwrap(),
        ],
    );
    let result = GitRepository::test_open(repo_dir.path());
    assert!(result.is_err(), "unsafe local hook config must be rejected");
    assert!(!marker.exists(), "an untrusted hook must never execute");
}
#[test]
fn legacy_service_preserves_merge_and_rebase_state_from_the_bound_git_dir() {
    let repo_dir = init_repo();
    commit_initial(repo_dir.path(), "tracked.txt", "tracked\n");
    let git_dir = repo_dir.path().join(".git");
    fs::write(
        git_dir.join("MERGE_HEAD"),
        "0123456789012345678901234567890123456789\n",
    )
    .expect("write merge state");
    let repository = GitRepository::test_open(repo_dir.path()).expect("open repository");
    let (merging, rebasing) = repository.operation_state().expect("read merge state");
    assert!(merging);
    assert!(!rebasing);
    fs::remove_file(git_dir.join("MERGE_HEAD")).expect("remove merge state");
    fs::create_dir(git_dir.join("rebase-merge")).expect("write rebase state");
    let repository = GitRepository::test_open(repo_dir.path()).expect("open repository");
    let (merging, rebasing) = repository.operation_state().expect("read rebase state");
    assert!(!merging);
    assert!(rebasing);
}
#[test]
fn repository_graph_rejects_external_include_and_alternate_object_paths() {
    let repo_dir = init_repo();
    let outside = test_tempdir("devmanager-phase66-git-external-");
    let included = outside.path().join("included.config");
    fs::write(&included, "[user]\n\tname = external\n").expect("write include");
    git(
        repo_dir.path(),
        &["config", "include.path", included.to_str().unwrap()],
    );
    assert!(
        GitRepository::test_open(repo_dir.path()).is_err(),
        "external local includes must be rejected before Git starts"
    );
    let repo_dir = init_repo();
    let objects = repo_dir.path().join(".git").join("objects");
    let alternate = test_tempdir("devmanager-phase66-git-alternate-");
    let alternates_file = objects.join("info").join("alternates");
    fs::create_dir_all(alternates_file.parent().unwrap()).expect("create alternates dir");
    fs::write(
        &alternates_file,
        format!("{}\n", alternate.path().display()),
    )
    .expect("write alternates");
    assert!(
        GitRepository::test_open(repo_dir.path()).is_err(),
        "external alternate object stores must be rejected before Git starts"
    );
}
#[test]
fn repository_graph_rejects_hard_linked_configuration_files() {
    let repo_dir = init_repo();
    let config = repo_dir.path().join(".git").join("config");
    let backup = repo_dir.path().join(".git").join("config.original");
    fs::rename(&config, &backup).expect("move original config");
    if fs::hard_link(&backup, &config).is_err() {
        eprintln!("skipping hard-link graph test: hard links are unavailable");
        return;
    }
    assert!(
        GitRepository::test_open(repo_dir.path()).is_err(),
        "hard-linked configuration files must be rejected before Git starts"
    );
}
#[test]
fn repository_graph_rejects_creation_of_absent_mutable_inputs_before_read() {
    let repo_dir = init_repo();
    let git_dir = repo_dir.path().join(".git");
    let index = git_dir.join("index");
    let packed_refs = git_dir.join("packed-refs");
    assert!(
        !index.exists(),
        "fresh repository unexpectedly has an index"
    );
    assert!(
        !packed_refs.exists(),
        "fresh repository unexpectedly has packed refs"
    );
    let repository = GitRepository::test_open(repo_dir.path()).expect("open fresh repository");
    fs::write(&index, b"attacker-created-index").expect("create unexpected index");
    let error = repository
        .status()
        .expect_err("an index created after admission must be rejected");
    assert!(
        matches!(error, GitError::InvalidRepositoryRoot { .. }),
        "unexpected error for an unexpected index: {error}"
    );
    fs::remove_file(&index).expect("remove unexpected index");
    fs::write(&packed_refs, b"# pack-refs with: peeled fully-peeled\n")
        .expect("create unexpected packed refs");
    let error = repository
        .status()
        .expect_err("packed refs created after admission must be rejected");
    assert!(
        matches!(error, GitError::InvalidRepositoryRoot { .. }),
        "unexpected error for unexpected packed refs: {error}"
    );
}
#[test]
fn repository_graph_rejects_creation_of_absent_operation_state_before_read() {
    let repo_dir = init_repo();
    let repository = GitRepository::test_open(repo_dir.path()).expect("open fresh repository");
    fs::write(
        repo_dir.path().join(".git").join("MERGE_HEAD"),
        "0123456789012345678901234567890123456789\n",
    )
    .expect("create unexpected merge state");
    let error = repository
        .operation_state()
        .expect_err("operation state created after admission must be rejected");
    assert!(
        matches!(error, GitError::InvalidRepositoryRoot { .. }),
        "unexpected error for unexpected operation state: {error}"
    );
}
#[test]
fn repository_graph_rejects_creation_of_absent_worktree_config_before_read() {
    let repo_dir = init_repo();
    let repository = GitRepository::test_open(repo_dir.path()).expect("open fresh repository");
    fs::write(
        repo_dir.path().join(".git").join("config.worktree"),
        "[include]\n\tpath = C:/outside/config\n",
    )
    .expect("create unexpected worktree config");
    let error = repository
        .status()
        .expect_err("worktree config created after admission must be rejected");
    assert!(
        matches!(error, GitError::InvalidRepositoryRoot { .. }),
        "unexpected error for unexpected worktree config: {error}"
    );
}

fn resolve_linked_worktree_git_roots(linked: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let descriptor = fs::read_to_string(linked.join(".git")).expect("linked gitdir descriptor");
    let gitdir_value = descriptor
        .trim()
        .strip_prefix("gitdir:")
        .expect("gitdir descriptor")
        .trim();
    let gitdir_requested = PathBuf::from(gitdir_value);
    let gitdir = fs::canonicalize(if gitdir_requested.is_absolute() {
        gitdir_requested
    } else {
        linked.join(gitdir_requested)
    })
    .expect("external linked gitdir");
    let commondir_value =
        fs::read_to_string(gitdir.join("commondir")).expect("linked commondir descriptor");
    let commondir_requested = PathBuf::from(commondir_value.trim());
    let commondir = fs::canonicalize(if commondir_requested.is_absolute() {
        commondir_requested
    } else {
        gitdir.join(commondir_requested)
    })
    .expect("external linked common directory");
    let objects = commondir.join("objects");
    (gitdir, commondir, objects)
}

#[test]
fn approved_linked_worktree_graph_is_bound_to_external_git_roots() {
    let primary = init_repo();
    commit_initial(primary.path(), "tracked.txt", "tracked\n");
    let linked_parent = test_tempdir("devmanager-phase66-linked-");
    let linked = linked_parent.path().join("linked");
    let linked_arg = linked.to_str().expect("linked worktree path");
    git(primary.path(), &["worktree", "add", "--detach", linked_arg]);

    let (gitdir, commondir, objects) = resolve_linked_worktree_git_roots(&linked);
    let binding =
        test_issue_git_host_binding(&linked, vec![gitdir.clone(), commondir.clone(), objects])
            .expect("approved linked worktree authority");
    let repository =
        GitRepository::from_host_binding(binding, crate::git::command::GitCancellation::new())
            .expect("open approved linked worktree");
    let status = repository.status().expect("read linked worktree status");
    assert!(!status.is_detached || status.head.is_some());
}

#[test]
fn current_only_status_admits_external_sibling_worktree_without_authorizing_its_backlink() {
    let primary = init_repo();
    commit_initial(primary.path(), "tracked.txt", "tracked\n");
    let sibling_parent = test_tempdir("devmanager-phase66-sibling-wt-");
    let sibling = sibling_parent.path().join("sibling");
    git(
        primary.path(),
        &[
            "worktree",
            "add",
            "--detach",
            sibling.to_str().expect("sibling path"),
        ],
    );
    fs::write(primary.path().join("local-change.txt"), "primary change\n")
        .expect("write primary change");

    let repository = GitRepository::test_open(primary.path())
        .expect("current-only open must ignore unrelated sibling backlink targets");
    let status = repository
        .status_summary()
        .expect("current-only status must succeed with an external sibling worktree");
    assert!(
        status
            .entries
            .iter()
            .any(|entry| entry.path == RepoPath::from("local-change.txt")),
        "status must report the current worktree change without authorizing the sibling"
    );
    let summary = repository
        .status()
        .expect("full status must also succeed under current-only admission");
    assert!(summary
        .entries
        .iter()
        .any(|entry| entry.path == RepoPath::from("local-change.txt")));
}

#[test]
fn current_linked_worktree_backlink_still_requires_approved_external_roots() {
    let primary = init_repo();
    commit_initial(primary.path(), "tracked.txt", "tracked\n");
    let linked_parent = test_tempdir("devmanager-phase66-linked-roots-");
    let linked = linked_parent.path().join("linked");
    git(
        primary.path(),
        &[
            "worktree",
            "add",
            "--detach",
            linked.to_str().expect("linked path"),
        ],
    );

    let error = GitRepository::test_open(&linked)
        .expect_err("current linked backlink must still require exact approved external roots");
    assert!(matches!(error, GitError::InvalidRepositoryRoot { .. }));

    let (gitdir, commondir, objects) = resolve_linked_worktree_git_roots(&linked);
    let binding = test_issue_git_host_binding(&linked, vec![gitdir, commondir, objects])
        .expect("approved linked worktree authority");
    let repository =
        GitRepository::from_host_binding(binding, crate::git::command::GitCancellation::new())
            .expect("open approved linked worktree");
    repository
        .status_summary()
        .expect("approved current linked backlink must admit status");
}

#[test]
fn strict_worktree_admission_rejects_sibling_descriptor_outside_approved_graph() {
    let primary = init_repo();
    commit_initial(primary.path(), "tracked.txt", "tracked\n");
    let foreign = test_tempdir("devmanager-phase66-foreign-worktree-");
    let foreign_gitdir = foreign.path().join(".git");
    fs::create_dir_all(&foreign_gitdir).expect("foreign gitdir");

    let metadata = primary
        .path()
        .join(".git")
        .join("worktrees")
        .join("foreign");
    fs::create_dir_all(&metadata).expect("foreign worktree metadata");
    fs::write(
        metadata.join("gitdir"),
        format!("{}\n", foreign_gitdir.display()),
    )
    .expect("foreign worktree gitdir descriptor");
    fs::write(metadata.join("commondir"), "../..\n").expect("foreign worktree commondir");

    let error = GitRepository::test_open_with_strict_worktree_descriptors(primary.path())
        .expect_err("strict admission must still reject outside sibling backlinks");
    assert!(matches!(error, GitError::InvalidRepositoryRoot { .. }));
}

#[test]
fn current_only_still_rejects_sibling_commondir_that_leaves_admitted_store() {
    let primary = init_repo();
    commit_initial(primary.path(), "tracked.txt", "tracked\n");
    let sibling_parent = test_tempdir("devmanager-phase66-sibling-common-");
    let sibling = sibling_parent.path().join("sibling");
    git(
        primary.path(),
        &[
            "worktree",
            "add",
            "--detach",
            sibling.to_str().expect("sibling path"),
        ],
    );
    let worktrees = primary.path().join(".git").join("worktrees");
    let metadata = fs::read_dir(&worktrees)
        .expect("enumerate worktrees")
        .map(|entry| entry.expect("worktree entry").path())
        .find(|path| path.is_dir())
        .expect("sibling metadata");
    let outside = test_tempdir("devmanager-phase66-false-common-");
    fs::write(
        metadata.join("commondir"),
        format!("{}\n", outside.path().display()),
    )
    .expect("rewrite sibling commondir away from admitted store");

    let error = GitRepository::test_open(primary.path())
        .expect_err("current-only must still validate sibling commondir equality");
    assert!(matches!(error, GitError::InvalidRepositoryRoot { .. }));
}

#[test]
fn current_only_admission_rejects_descriptor_consuming_git_operations() {
    let primary = init_repo();
    commit_initial(primary.path(), "tracked.txt", "tracked\n");
    let sibling_parent = test_tempdir("devmanager-phase66-policy-sibling-");
    let sibling = sibling_parent.path().join("sibling");
    git(
        primary.path(),
        &[
            "worktree",
            "add",
            "--detach",
            sibling.to_str().expect("sibling path"),
        ],
    );
    let repository = GitRepository::test_open(primary.path()).expect("current-only open");

    for arguments in [
        vec![
            OsString::from("switch"),
            OsString::from("-c"),
            OsString::from("feature-branch"),
        ],
        vec![OsString::from("branch"), OsString::from("policy-branch")],
        vec![
            OsString::from("worktree"),
            OsString::from("list"),
            OsString::from("--porcelain"),
        ],
    ] {
        let permit = GitOperationPermit::test_service_mutation(&arguments, None, None);
        let error = repository
            .run_service_mutation_with_permit(arguments.clone(), None, None, permit)
            .expect_err("descriptor-consuming shapes must not reuse current-only admission");
        match error {
            GitError::InvalidRequest { message } => {
                assert!(
                    message.contains("strict worktree descriptor admission"),
                    "unexpected invalid-request message for {arguments:?}: {message}"
                );
            }
            other => panic!("expected InvalidRequest for {arguments:?}, got {other}"),
        }
    }
}

#[test]
fn repository_graph_rejects_an_unrecognized_direct_git_entry_on_read() {
    let repo_dir = init_repo();
    commit_initial(repo_dir.path(), "tracked.txt", "tracked\n");
    let repository = GitRepository::test_open(repo_dir.path()).expect("open repository");
    fs::write(
        repo_dir.path().join(".git").join("unexpected-state"),
        b"foreign state",
    )
    .expect("create unexpected Git state");

    let error = repository
        .status()
        .expect_err("an unrecognized direct Git entry must fail closed on reads");
    assert!(matches!(error, GitError::InvalidRepositoryRoot { .. }));
}

#[test]
fn repository_graph_rejects_unreferenced_external_graph_roots() {
    let repo_dir = init_repo();
    commit_initial(repo_dir.path(), "tracked.txt", "tracked\n");
    let unrelated = test_tempdir("devmanager-phase66-unrelated-graph-");

    let error = GitRepository::test_open_with_approved_external_roots(
        repo_dir.path(),
        vec![unrelated.path().to_path_buf()],
    )
    .expect_err("an approved root must be referenced by the bound Git graph");
    assert!(matches!(error, GitError::InvalidRepositoryRoot { .. }));
}

#[test]
fn repository_graph_rejects_new_content_in_a_bound_alternate_store() {
    let repo_dir = init_repo();
    commit_initial(repo_dir.path(), "tracked.txt", "tracked\n");
    let git_dir = repo_dir.path().join(".git");
    let alternate = git_dir.join("alternate-objects");
    fs::create_dir_all(&alternate).expect("create alternate object store");
    fs::write(
        git_dir.join("objects").join("info").join("alternates"),
        b"../alternate-objects\n",
    )
    .expect("bind alternate object store");
    let repository = GitRepository::test_open(repo_dir.path()).expect("open alternate graph");

    fs::write(alternate.join("unexpected.pack"), b"attacker content")
        .expect("create unexpected alternate content");
    let error = repository
        .status()
        .expect_err("new alternate content must fail the static graph check");
    assert!(matches!(error, GitError::InvalidRepositoryRoot { .. }));
}

#[test]
fn linked_worktree_allows_normal_stage_and_commit_transitions() {
    let primary = init_repo();
    commit_initial(primary.path(), "tracked.txt", "tracked\n");
    let linked_parent = test_tempdir("devmanager-phase66-linked-mutate-");
    let linked = linked_parent.path().join("linked");
    git(
        primary.path(),
        &[
            "worktree",
            "add",
            "--detach",
            linked.to_str().expect("linked path"),
        ],
    );

    let descriptor = fs::read_to_string(linked.join(".git")).expect("linked gitdir descriptor");
    let gitdir_value = descriptor
        .trim()
        .strip_prefix("gitdir:")
        .expect("gitdir descriptor")
        .trim();
    let gitdir_requested = PathBuf::from(gitdir_value);
    let gitdir = fs::canonicalize(if gitdir_requested.is_absolute() {
        gitdir_requested
    } else {
        linked.join(gitdir_requested)
    })
    .expect("external linked gitdir");
    let commondir_value =
        fs::read_to_string(gitdir.join("commondir")).expect("linked commondir descriptor");
    let commondir_requested = PathBuf::from(commondir_value.trim());
    let commondir = fs::canonicalize(if commondir_requested.is_absolute() {
        commondir_requested
    } else {
        gitdir.join(commondir_requested)
    })
    .expect("external linked common directory");
    let binding = test_issue_git_host_binding(
        &linked,
        vec![gitdir.clone(), commondir.clone(), commondir.join("objects")],
    )
    .expect("approved linked worktree authority");
    let repository =
        GitRepository::from_host_binding(binding, crate::git::command::GitCancellation::new())
            .expect("open linked worktree");

    fs::write(linked.join("linked.txt"), "linked\n").expect("write linked change");
    let stage = repository
        .plan_stage(&[RepoPath::from("linked.txt")])
        .unwrap_or_else(|error| match error {
            GitError::InvalidRepositoryRoot { reason, .. } => {
                panic!("plan linked stage: {reason}")
            }
            error => panic!("plan linked stage: {error}"),
        });
    let confirmation = confirm(&repository, &stage);
    repository
        .stage(&stage, &confirmation)
        .unwrap_or_else(|error| match error {
            GitError::InvalidRepositoryRoot { reason, .. } => {
                panic!("stage linked file: {reason}")
            }
            error => panic!("stage linked file: {error}"),
        });

    let commit = repository
        .plan_commit("linked commit")
        .expect("plan linked commit");
    let confirmation = confirm(&repository, &commit);
    repository
        .commit(&commit, &confirmation)
        .unwrap_or_else(|error| panic!("commit linked file: {error}"));
    assert!(repository
        .status()
        .expect("linked status")
        .entry("linked.txt")
        .is_none());
}

#[test]
fn repository_graph_rejects_a_real_head_swap_until_the_original_is_restored() {
    let repo_dir = init_repo();
    commit_initial(repo_dir.path(), "tracked.txt", "tracked\n");
    let repository = GitRepository::test_open(repo_dir.path()).expect("open repository");
    let head = repo_dir.path().join(".git").join("HEAD");
    let moved = repo_dir.path().join(".git").join("HEAD.swap");
    fs::rename(&head, &moved).expect("swap HEAD out of the graph");
    assert!(
        repository.status().is_err(),
        "read-only operation must reject an absent pinned HEAD"
    );
    fs::rename(&moved, &head).expect("restore the original HEAD identity");
    repository
        .status()
        .expect("restored graph must be usable again");
}

fn issued_production_host_repository(
    root: &Path,
    action_epoch: u64,
    runtime_generation: u64,
) -> GitRepository {
    issued_production_host_repository_with_ids(
        root,
        crate::domain::TaskId::new(),
        crate::domain::ClientId::new(),
        uuid::Uuid::now_v7(),
        action_epoch,
        runtime_generation,
    )
}

fn issued_production_host_repository_with_ids(
    root: &Path,
    task_id: crate::domain::TaskId,
    client_id: crate::domain::ClientId,
    connection_id: uuid::Uuid,
    action_epoch: u64,
    runtime_generation: u64,
) -> GitRepository {
    use crate::workspace::{
        WorkspaceProjectRoots, WorkspaceRequest, WorkspaceResource, WorkspaceResourceCoordinator,
        WorkspaceService,
    };

    let project_id = crate::domain::ProjectId::new();
    let roots = WorkspaceProjectRoots::try_from_pairs([(project_id, root.to_path_buf())])
        .expect("project roots");
    let mut service = WorkspaceService::with_task_coordinator(
        project_id,
        task_id,
        &roots,
        WorkspaceResourceCoordinator::new(),
    )
    .expect("workspace service");
    let request_id = crate::domain::RequestId::new();
    let command_id = crate::domain::CommandId::from_bytes(*request_id.as_bytes()).expect("command");
    service
        .bind_authorized_with_generation(
            WorkspaceRequest::main(),
            task_id,
            client_id,
            connection_id,
            request_id,
            command_id,
            action_epoch,
            runtime_generation,
        )
        .expect("bind workspace");
    let workspace = service
        .current()
        .expect("bound workspace")
        .durable_ref()
        .clone();
    let authorization = service
        .authorize_current_with_generation(
            &workspace,
            task_id,
            client_id,
            connection_id,
            request_id,
            command_id,
            action_epoch,
            runtime_generation,
        )
        .expect("authorize workspace");
    let lease = service
        .acquire_task_resource(
            task_id,
            WorkspaceResource::Git,
            client_id,
            connection_id,
            request_id,
            command_id,
            action_epoch,
            runtime_generation,
        )
        .expect("git lease");
    let binding = issue_git_host_binding(
        &authorization,
        lease,
        task_id,
        project_id,
        client_id,
        connection_id,
        request_id,
        command_id,
        &workspace,
        action_epoch,
        runtime_generation,
    )
    .expect("issue production host binding");
    GitRepository::from_host_binding(binding, GitCancellation::new()).expect("open host repository")
}

#[test]
fn production_host_issuer_confirms_and_executes_stage_unstage_commit_when_fence_and_plan_match() {
    let repo_dir = init_repo();
    commit_initial(repo_dir.path(), "tracked.txt", "tracked\n");
    fs::write(repo_dir.path().join("tracked.txt"), "changed\n").expect("change tracked file");
    let repo = issued_production_host_repository(repo_dir.path(), 1, 1);

    assert!(
        repo.confirm(
            &repo
                .plan_stage(&[RepoPath::from("tracked.txt")])
                .expect("stage plan")
        )
        .is_err(),
        "legacy confirm must stay fail-closed on a host-bound repository"
    );

    let stage = repo
        .plan_stage(&[RepoPath::from("tracked.txt")])
        .expect("stage plan");
    let confirmation = repo
        .host_confirm(&stage)
        .expect("production host issuer must confirm the exact stage plan");
    repo.stage(&stage, &confirmation)
        .expect("confirmed host stage must execute");
    assert!(repo
        .status()
        .expect("status after stage")
        .entry("tracked.txt")
        .expect("tracked")
        .is_staged());

    let repo = issued_production_host_repository(repo_dir.path(), 2, 1);
    let unstage = repo
        .plan_unstage(&[RepoPath::from("tracked.txt")])
        .expect("unstage plan");
    let confirmation = repo
        .host_confirm(&unstage)
        .expect("production host issuer must confirm the exact unstage plan");
    repo.unstage(&unstage, &confirmation)
        .unwrap_or_else(|error| match error {
            GitError::InvalidRepositoryRoot { reason, .. } => {
                panic!("confirmed host unstage must execute: {reason}")
            }
            error => panic!("confirmed host unstage must execute: {error}"),
        });
    assert!(!repo
        .status()
        .expect("status after unstage")
        .entry("tracked.txt")
        .expect("tracked")
        .is_staged());

    let repo = issued_production_host_repository(repo_dir.path(), 3, 1);
    let stage = repo
        .plan_stage(&[RepoPath::from("tracked.txt")])
        .expect("stage before commit");
    repo.stage(&stage, &repo.host_confirm(&stage).expect("confirm restage"))
        .unwrap_or_else(|error| match error {
            GitError::InvalidRepositoryRoot { reason, .. } => panic!("restage: {reason}"),
            error => panic!("restage: {error}"),
        });
    let repo = issued_production_host_repository(repo_dir.path(), 4, 1);
    let commit = repo.plan_commit("host issuer commit").expect("commit plan");
    let confirmation = repo
        .host_confirm(&commit)
        .expect("production host issuer must confirm the exact commit plan");
    repo.commit(&commit, &confirmation)
        .unwrap_or_else(|error| match error {
            GitError::InvalidRepositoryRoot { reason, .. } => {
                panic!("confirmed host commit must execute: {reason}")
            }
            error => panic!("confirmed host commit must execute: {error}"),
        });
    assert!(repo
        .status()
        .expect("status after commit")
        .entry("tracked.txt")
        .is_none());
}

#[test]
fn host_issuer_rejects_wrong_action_or_runtime_generation() {
    let repo_dir = init_repo();
    commit_initial(repo_dir.path(), "tracked.txt", "tracked\n");
    fs::write(repo_dir.path().join("tracked.txt"), "changed\n").expect("change tracked file");
    let current = issued_production_host_repository(repo_dir.path(), 1, 1);
    let mismatched = issued_production_host_repository(repo_dir.path(), 2, 1);
    let plan = current
        .plan_stage(&[RepoPath::from("tracked.txt")])
        .expect("stage plan");
    let confirmation = current
        .host_confirm(&plan)
        .expect("confirm against the issued action epoch");

    let error = mismatched
        .stage(&plan, &confirmation)
        .expect_err("a different action epoch must not inherit the confirmation");
    assert!(
        matches!(
            error,
            GitError::AuthorityUnavailable | GitError::ConfirmationMismatch { .. }
        ),
        "unexpected error for a mismatched action epoch: {error}"
    );

    let mut confirmation = current
        .host_confirm(&plan)
        .expect("fresh confirmation for fence tamper");
    confirmation.tamper_host_fence_for_test(9, 1);
    let error = current
        .stage(&plan, &confirmation)
        .expect_err("an altered action/runtime fence must fail closed");
    assert!(matches!(error, GitError::AuthorityUnavailable));
}

#[test]
fn host_issuer_rejects_wrong_task_workspace_or_connection() {
    let first_dir = init_repo();
    commit_initial(first_dir.path(), "tracked.txt", "tracked\n");
    fs::write(first_dir.path().join("tracked.txt"), "changed\n").expect("change first");
    let second_dir = init_repo();
    commit_initial(second_dir.path(), "tracked.txt", "tracked\n");
    fs::write(second_dir.path().join("tracked.txt"), "other\n").expect("change second");

    let task_a = crate::domain::TaskId::new();
    let task_b = crate::domain::TaskId::new();
    let client = crate::domain::ClientId::new();
    let connection_a = uuid::Uuid::now_v7();
    let connection_b = uuid::Uuid::now_v7();
    let first = issued_production_host_repository_with_ids(
        first_dir.path(),
        task_a,
        client,
        connection_a,
        1,
        1,
    );
    let foreign_task = issued_production_host_repository_with_ids(
        first_dir.path(),
        task_b,
        client,
        connection_a,
        1,
        1,
    );
    let foreign_connection = issued_production_host_repository_with_ids(
        first_dir.path(),
        task_a,
        client,
        connection_b,
        1,
        1,
    );
    let other_workspace = issued_production_host_repository(second_dir.path(), 1, 1);

    let plan = first
        .plan_stage(&[RepoPath::from("tracked.txt")])
        .expect("stage plan");
    let confirmation = first.host_confirm(&plan).expect("confirm first task");

    assert!(
        matches!(
            foreign_task.stage(&plan, &confirmation),
            Err(GitError::AuthorityUnavailable | GitError::ConfirmationMismatch { .. })
        ),
        "a different task must not reuse the host confirmation"
    );
    assert!(
        matches!(
            foreign_connection.stage(&plan, &confirmation),
            Err(GitError::AuthorityUnavailable | GitError::ConfirmationMismatch { .. })
        ),
        "a different connection must not reuse the host confirmation"
    );
    assert!(
        matches!(
            other_workspace.host_confirm(&plan),
            Err(GitError::WorkspaceMismatch { .. } | GitError::AuthorityUnavailable)
        ),
        "a plan bound to another workspace must not confirm"
    );
}

#[test]
fn host_confirmation_cannot_be_reused_after_execution_attempt() {
    let repo_dir = init_repo();
    commit_initial(repo_dir.path(), "tracked.txt", "tracked\n");
    fs::write(repo_dir.path().join("tracked.txt"), "changed\n").expect("change tracked file");
    let repo = issued_production_host_repository(repo_dir.path(), 1, 1);
    let plan = repo
        .plan_stage(&[RepoPath::from("tracked.txt")])
        .expect("stage plan");
    let confirmation = repo
        .host_confirm(&plan)
        .expect("host confirmation must remain unused until execution");

    fs::write(repo_dir.path().join("tracked.txt"), "drift\n").expect("preview drift");
    let preview = repo
        .stage(&plan, &confirmation)
        .expect_err("fingerprint drift must fail before spawn");
    assert!(
        matches!(preview, GitError::FingerprintMismatch { .. }),
        "failed preview must stay typed as fingerprint mismatch: {preview}"
    );

    fs::write(repo_dir.path().join("tracked.txt"), "changed\n").expect("restore planned content");
    repo.stage(&plan, &confirmation)
        .expect("the same confirmation must still execute after a failed preview");

    let replay = repo
        .stage(&plan, &confirmation)
        .expect_err("a host confirmation must be one-shot after an execution attempt");
    assert!(
        matches!(replay, GitError::AuthorityUnavailable),
        "replay must fail closed as AuthorityUnavailable: {replay}"
    );
}

#[test]
fn host_issuer_rejects_altered_plan_digest_or_confirmation_nonce() {
    let repo_dir = init_repo();
    commit_initial(repo_dir.path(), "tracked.txt", "tracked\n");
    fs::write(repo_dir.path().join("tracked.txt"), "changed\n").expect("change tracked file");
    let repo = issued_production_host_repository(repo_dir.path(), 1, 1);
    let plan = repo
        .plan_stage(&[RepoPath::from("tracked.txt")])
        .expect("stage plan");

    let mut confirmation = repo.host_confirm(&plan).expect("host confirm");
    confirmation.tamper_plan_digest_for_test();
    assert!(
        matches!(
            repo.stage(&plan, &confirmation),
            Err(GitError::ConfirmationMismatch { .. } | GitError::AuthorityUnavailable)
        ),
        "an altered plan digest must not execute"
    );

    let mut confirmation = repo.host_confirm(&plan).expect("fresh host confirm");
    confirmation.tamper_confirmation_nonce_for_test();
    assert!(
        matches!(
            repo.stage(&plan, &confirmation),
            Err(GitError::ConfirmationMismatch { .. } | GitError::AuthorityUnavailable)
        ),
        "an altered confirmation nonce must not execute"
    );
}

#[test]
fn host_issuer_rejects_expired_or_revoked_authority() {
    let repo_dir = init_repo();
    commit_initial(repo_dir.path(), "tracked.txt", "tracked\n");
    fs::write(repo_dir.path().join("tracked.txt"), "changed\n").expect("change tracked file");
    let expired_binding = test_issue_git_host_binding_with_fence(
        repo_dir.path(),
        Vec::new(),
        "test-task-6-6a",
        "test-controller",
        "test-connection",
        "test-request",
        "test-command",
        1,
        1,
        std::time::Duration::ZERO,
    )
    .expect("issue immediately expired host binding");
    assert!(
        GitRepository::from_host_binding(expired_binding, GitCancellation::new()).is_err(),
        "an expired host binding must not open"
    );

    let repo = issued_production_host_repository(repo_dir.path(), 1, 1);
    let plan = repo
        .plan_stage(&[RepoPath::from("tracked.txt")])
        .expect("stage plan");
    let confirmation = repo.host_confirm(&plan).expect("confirm while live");
    repo.expire_host_authority_for_test();
    assert!(
        matches!(
            repo.host_confirm(&plan),
            Err(GitError::AuthorityUnavailable)
        ),
        "expired host authority must not confirm"
    );
    assert!(
        matches!(
            repo.stage(&plan, &confirmation),
            Err(GitError::AuthorityUnavailable)
        ),
        "expired host authority must not execute a prior confirmation"
    );

    let live = issued_production_host_repository(repo_dir.path(), 1, 1);
    let plan = live
        .plan_stage(&[RepoPath::from("tracked.txt")])
        .expect("stage plan");
    let confirmation = live.host_confirm(&plan).expect("confirm before revoke");
    live.revoke_host_authority_for_test();
    assert!(
        matches!(
            live.host_confirm(&plan),
            Err(GitError::AuthorityUnavailable)
        ),
        "revoked host authority must not confirm"
    );
    assert!(
        matches!(
            live.stage(&plan, &confirmation),
            Err(GitError::AuthorityUnavailable)
        ),
        "revoked host authority must not execute a prior confirmation"
    );
}

#[test]
fn host_issuer_rejects_traversal_raw_path_and_unsupported_capability() {
    let repo_dir = init_repo();
    commit_initial(repo_dir.path(), "tracked.txt", "tracked\n");
    fs::write(repo_dir.path().join("tracked.txt"), "changed\n").expect("change tracked file");
    let repo = issued_production_host_repository(repo_dir.path(), 1, 1);

    assert!(
        matches!(
            repo.plan_stage(&[RepoPath::from("../secret")]),
            Err(GitError::InvalidPath { .. })
        ),
        "relative traversal must stay rejected before host confirmation"
    );
    assert!(
        matches!(
            repo.plan_stage(&[RepoPath::from("C:\\Windows\\system32\\drivers\\etc\\hosts")]),
            Err(GitError::InvalidPath { .. })
        ),
        "a raw absolute path must not become a mutation plan"
    );

    // Keep the fixture-authority check on a separate repository. A second Git
    // client can legitimately refresh mutable index metadata; doing that on
    // the host-bound repository would (correctly) stale its retained graph
    // identity before the capability check below.
    let test_repo_dir = init_repo();
    commit_initial(test_repo_dir.path(), "tracked.txt", "tracked\n");
    fs::write(test_repo_dir.path().join("tracked.txt"), "changed\n")
        .expect("change test-fixture tracked file");
    let test_repo = GitRepository::test_open(test_repo_dir.path()).expect("open test repository");
    let plan = test_repo
        .plan_stage(&[RepoPath::from("tracked.txt")])
        .expect("test stage plan");
    assert!(
        matches!(
            test_repo.host_confirm(&plan),
            Err(GitError::AuthorityUnavailable)
        ),
        "a test fixture must not mint production host confirmation"
    );
    assert!(
        test_repo.confirm(&plan).is_err(),
        "legacy confirm must not self-authorize from a repository path"
    );

    let pull_request = repo
        .plan_pull_request(
            PullRequestProvider::GitHub,
            "https://github.com/acme/widget.git",
            Some("feature/demo"),
            "main",
            "Ship the change",
            "Body stays in the typed invocation.",
        )
        .expect("pull request plan");
    assert!(
        matches!(
            repo.host_confirm(&pull_request),
            Err(GitError::CapabilityDenied { .. })
        ),
        "host issuer must not confirm unsupported Git capabilities"
    );
}
