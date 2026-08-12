use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::tempdir;

fn git(cwd: &Path, arguments: &[&str]) -> std::process::Output {
    Command::new("git")
        .current_dir(cwd)
        .args(arguments)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("start git")
}

fn git_os(cwd: &Path, arguments: Vec<OsString>) -> std::process::Output {
    Command::new("git")
        .current_dir(cwd)
        .args(arguments)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("start git")
}

fn successful(cwd: &Path, arguments: &[&str]) {
    let output = git(cwd, arguments);
    assert!(
        output.status.success(),
        "git {:?}: {}",
        arguments,
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn process_arguments_are_literal_and_status_is_noninteractive() {
    let repo = tempdir().expect("create process fixture");
    successful(repo.path(), &["init", "--quiet"]);
    successful(
        repo.path(),
        &["config", "user.email", "tests@example.invalid"],
    );
    successful(repo.path(), &["config", "user.name", "Git process test"]);

    let name = "literal & $(not-a-shell-command).txt";
    fs::write(repo.path().join(name), "payload\n").expect("write process fixture");
    let mut add = vec![OsString::from("add"), OsString::from("--")];
    add.push(OsString::from(name));
    let output = git_os(repo.path(), add);
    assert!(output.status.success());
    successful(repo.path(), &["commit", "--quiet", "-m", "literal path"]);

    let output = git(repo.path(), &["status", "--porcelain=v2", "-z"]);
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
}

#[test]
fn blocking_git_process_closes_stdin_and_joins_with_a_bound() {
    let repo = tempdir().expect("create process cancellation fixture");
    successful(repo.path(), &["init", "--quiet"]);
    let mut child = Command::new("git")
        .current_dir(repo.path())
        .args(["cat-file", "--batch"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start blocking git process");
    drop(child.stdin.take());
    let started = Instant::now();
    loop {
        if child.try_wait().expect("poll git process").is_some() {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(2));
        thread::sleep(Duration::from_millis(10));
    }
    drop(child.stdout.take());
    drop(child.stderr.take());
}
