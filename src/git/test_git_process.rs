use crate::git::command::{
    test_issue_git_host_binding, GitCancellation, GitError, GitOperationPermit, GitRepository,
};
use crate::git::model::{RepoPath, WorkspaceIdentity};
use std::fs;
use std::path::Path;
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
    let repo = test_tempdir("devmanager-phase66-git-process-");
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

#[test]
fn host_mutation_permit_cannot_authorize_a_replaced_host_binding() {
    let repo_dir = init_repo();
    commit_initial(repo_dir.path(), "tracked.txt", "tracked\n");
    fs::write(repo_dir.path().join("tracked.txt"), "changed\n").expect("change tracked file");

    let binding_a =
        test_issue_git_host_binding(repo_dir.path(), Vec::new()).expect("issue first host binding");
    let repo_a = GitRepository::from_host_binding(binding_a, GitCancellation::new())
        .expect("open first bound repository");
    let plan = repo_a
        .plan_stage(&[RepoPath::from("tracked.txt")])
        .expect("plan stage from the first binding");
    let confirmation = repo_a
        .test_confirm(&plan)
        .expect("confirm with the first host binding");
    let binding_b = test_issue_git_host_binding(repo_dir.path(), Vec::new())
        .expect("issue replacement host binding");
    let repo_b = GitRepository::from_host_binding(binding_b, GitCancellation::new())
        .expect("open replacement bound repository");

    let error = repo_b
        .stage(&plan, &confirmation)
        .expect_err("a replaced host binding must not inherit the prior permit");
    assert!(
        matches!(
            error,
            GitError::AuthorityUnavailable | GitError::ConfirmationMismatch { .. }
        ),
        "unexpected error for a swapped host binding: {error}"
    );
}

#[test]
fn test_service_permit_cannot_run_on_a_host_bound_repository() {
    let repo_dir = init_repo();
    commit_initial(repo_dir.path(), "tracked.txt", "tracked\n");
    fs::write(repo_dir.path().join("tracked.txt"), "changed\n").expect("change tracked file");
    let binding =
        test_issue_git_host_binding(repo_dir.path(), Vec::new()).expect("issue host binding");
    let repo = GitRepository::from_host_binding(binding, GitCancellation::new())
        .expect("open host-bound repository");
    let arguments = vec!["add".into(), "--".into(), "tracked.txt".into()];
    let permit = GitOperationPermit::test_service_mutation(&arguments, None, None);

    let error = repo
        .run_service_mutation_with_permit(arguments, None, None, permit)
        .expect_err("a test permit must not authorize a host-bound repository");
    assert!(matches!(error, GitError::AuthorityUnavailable));
}

#[cfg(windows)]
#[test]
fn workspace_identity_hashes_exact_os_path_units() {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::path::PathBuf;

    let left = PathBuf::from(OsString::from_wide(&[0x0043, 0x003A, 0x005C, 0xD800]));
    let right = PathBuf::from(OsString::from_wide(&[0x0043, 0x003A, 0x005C, 0xD801]));
    assert_eq!(
        left.to_string_lossy(),
        right.to_string_lossy(),
        "fixture must collide under lossy display conversion"
    );
    let left_id = WorkspaceIdentity::from_canonical_root(left);
    let right_id = WorkspaceIdentity::from_canonical_root(right);
    assert_ne!(
        left_id.id(),
        right_id.id(),
        "workspace identity must hash exact OS path units"
    );
}
