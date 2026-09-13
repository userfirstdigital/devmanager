use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use tempfile::{tempdir, TempDir};

fn git(cwd: &Path, arguments: &[&str]) -> Output {
    git_os(cwd, arguments.iter().map(OsString::from).collect())
}

fn git_os(cwd: &Path, arguments: Vec<OsString>) -> Output {
    Command::new("git")
        .current_dir(cwd)
        .args(arguments)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("start git")
}

fn successful_git(cwd: &Path, arguments: &[&str]) -> String {
    let output = git(cwd, arguments);
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        arguments,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn successful_git_os(cwd: &Path, arguments: Vec<OsString>) -> String {
    let output = git_os(cwd, arguments);
    assert!(
        output.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn init_repo() -> TempDir {
    let repo = tempdir().expect("create temporary repository");
    successful_git(repo.path(), &["init", "--quiet"]);
    successful_git(
        repo.path(),
        &["config", "user.email", "tests@example.invalid"],
    );
    successful_git(repo.path(), &["config", "user.name", "Git test"]);
    repo
}

fn commit_file(repo: &Path, name: &str, contents: &str) {
    fs::write(repo.join(name), contents).expect("write repository fixture");
    let mut add = vec![OsString::from("add"), OsString::from("--")];
    add.push(OsString::from(name));
    successful_git_os(repo, add);
    successful_git(repo, &["commit", "--quiet", "-m", "initial"]);
}

#[test]
fn temporary_repository_executes_status_and_stage_with_argument_arrays() {
    let repo = init_repo();
    commit_file(repo.path(), "tracked file.txt", "initial\n");

    fs::write(
        repo.path().join("literal; & $(not-a-command).txt"),
        "changed\n",
    )
    .expect("write adversarial filename");
    let output = git(repo.path(), &["status", "--porcelain=v2", "-z"]);
    assert!(output.status.success());
    assert!(output
        .stdout
        .windows(b"literal; & $(not-a-command).txt".len())
        .any(|window| window == b"literal; & $(not-a-command).txt"));

    let mut add = vec![OsString::from("add"), OsString::from("--")];
    add.push(OsString::from("literal; & $(not-a-command).txt"));
    successful_git_os(repo.path(), add);
    let staged = successful_git(repo.path(), &["diff", "--cached", "--name-only"]);
    assert!(staged.contains("literal; & $(not-a-command).txt"));
}

#[test]
fn linked_worktree_and_local_remote_use_the_exact_paths_passed_to_git() {
    let repo = init_repo();
    commit_file(repo.path(), "tracked.txt", "initial\n");

    let linked_parent = tempdir().expect("create linked worktree parent");
    let linked = linked_parent.path().join("linked worktree");
    let mut worktree = vec![
        OsString::from("worktree"),
        OsString::from("add"),
        OsString::from("--detach"),
    ];
    worktree.push(linked.as_os_str().to_os_string());
    successful_git_os(repo.path(), worktree);

    fs::write(linked.join("linked file.txt"), "linked\n").expect("write linked change");
    let mut add = vec![OsString::from("add"), OsString::from("--")];
    add.push(OsString::from("linked file.txt"));
    successful_git_os(&linked, add);
    successful_git(&linked, &["commit", "--quiet", "-m", "linked"]);

    let remote = repo.path().join("local remote.git");
    let mut init_remote = vec![
        OsString::from("init"),
        OsString::from("--bare"),
        OsString::from("--quiet"),
    ];
    init_remote.push(remote.as_os_str().to_os_string());
    successful_git_os(repo.path(), init_remote);

    let mut add_remote = vec![
        OsString::from("remote"),
        OsString::from("add"),
        OsString::from("origin"),
    ];
    add_remote.push(remote.as_os_str().to_os_string());
    successful_git_os(&linked, add_remote);
    successful_git(
        &linked,
        &["push", "--quiet", "origin", "HEAD:refs/heads/main"],
    );

    let linked_head = successful_git(&linked, &["rev-parse", "HEAD"]);
    let remote_head = successful_git(&remote, &["rev-parse", "refs/heads/main"]);
    assert_eq!(linked_head.trim(), remote_head.trim());
}

#[test]
fn noninteractive_remote_failure_returns_without_waiting_for_credentials() {
    let repo = init_repo();
    let started = std::time::Instant::now();
    let output = git(
        repo.path(),
        &["ls-remote", "https://127.0.0.1:1/unreachable.git"],
    );
    assert!(!output.status.success());
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("Username for"));
}
