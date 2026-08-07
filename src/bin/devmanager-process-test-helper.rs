use std::fs;
use std::path::Path;
use std::process::{Child, Command};
use std::time::Duration;

const NATURAL_EXIT_BOUND: Duration = Duration::from_secs(20);

fn write_marker(path: &Path, value: impl AsRef<[u8]>) {
    fs::write(path, value).expect("write process-test marker");
}

fn wait_naturally() {
    std::thread::sleep(NATURAL_EXIT_BOUND);
}

fn spawn_marker_child(marker: &Path) -> Child {
    Command::new(std::env::current_exe().expect("test-helper executable"))
        .arg("mark-wait")
        .arg(marker)
        .spawn()
        .expect("spawn marker child")
}

fn mark_and_wait(marker: &Path) {
    write_marker(marker, b"started");
    wait_naturally();
}

fn spawn_child_and_wait(root_marker: &Path, child_marker: &Path, child_pid: &Path) {
    write_marker(root_marker, b"started");
    let mut child = spawn_marker_child(child_marker);
    write_marker(child_pid, child.id().to_string());
    let _ = child.wait();
}

#[cfg(windows)]
fn attempt_breakaway(result: &Path, escaped_marker: &Path) {
    use std::os::windows::process::CommandExt;

    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    let spawn = Command::new(std::env::current_exe().expect("test-helper executable"))
        .arg("mark-wait")
        .arg(escaped_marker)
        .creation_flags(CREATE_BREAKAWAY_FROM_JOB)
        .spawn();
    match spawn {
        Ok(mut escaped) => {
            write_marker(result, format!("escaped:{}", escaped.id()));
            let _ = escaped.kill();
            let _ = escaped.wait();
        }
        Err(error) => write_marker(result, format!("blocked:{:?}", error.kind())),
    }
    wait_naturally();
}

#[cfg(not(windows))]
fn attempt_breakaway(result: &Path, _escaped_marker: &Path) {
    write_marker(result, b"unsupported");
}

fn required_path(
    args: &mut impl Iterator<Item = std::ffi::OsString>,
    name: &str,
) -> std::path::PathBuf {
    args.next()
        .map(Into::into)
        .unwrap_or_else(|| panic!("missing {name}"))
}

fn main() {
    let mut args = std::env::args_os().skip(1);
    let mode = args.next().expect("process-test helper mode");
    match mode.to_string_lossy().as_ref() {
        "mark-wait" => mark_and_wait(&required_path(&mut args, "marker path")),
        "spawn-child" => spawn_child_and_wait(
            &required_path(&mut args, "root marker path"),
            &required_path(&mut args, "child marker path"),
            &required_path(&mut args, "child PID path"),
        ),
        "attempt-breakaway" => attempt_breakaway(
            &required_path(&mut args, "breakaway result path"),
            &required_path(&mut args, "escaped marker path"),
        ),
        other => panic!("unknown process-test helper mode: {other}"),
    }
}
