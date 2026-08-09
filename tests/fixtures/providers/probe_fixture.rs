use std::env;
use std::fs;
use std::io::{self, Write};
use std::process::Command;
use std::thread;
use std::time::Duration;

const SLEEP: Duration = Duration::from_secs(30);

fn executable_stem() -> String {
    env::current_exe()
        .ok()
        .and_then(|path| {
            path.file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
        })
        .unwrap_or_default()
        .to_ascii_lowercase()
}

fn write_repeated(mut output: impl Write, byte: u8, count: usize) -> io::Result<()> {
    let bytes = vec![byte; count];
    output.write_all(&bytes)?;
    output.flush()
}

fn tree_child() -> ! {
    thread::sleep(SLEEP);
    std::process::exit(0)
}

fn tree_root() -> ! {
    let executable = env::current_exe().expect("fixture executable path");
    let child_pid_path = executable.with_extension("child.pid");
    let child = Command::new(&executable)
        .arg("--tree-child")
        .spawn()
        .expect("spawn probe fixture child");
    fs::write(child_pid_path, child.id().to_string()).expect("write probe fixture child pid");
    thread::sleep(SLEEP);
    std::process::exit(0)
}

fn main() {
    let mut args = env::args().skip(1);
    if args.next().as_deref() == Some("--tree-child") {
        tree_child();
    }

    let stem = executable_stem();
    if stem.contains("probe-tree") {
        tree_root();
    }
    if stem.contains("probe-timeout") {
        thread::sleep(SLEEP);
        return;
    }
    if stem.contains("probe-flood") {
        write_repeated(io::stdout(), b'o', 16 * 1024).expect("write flood stdout");
        write_repeated(io::stderr(), b'e', 16 * 1024).expect("write flood stderr");
        return;
    }

    let arguments = env::args().skip(1).collect::<Vec<_>>();
    if stem.contains("probe-env") {
        for key in [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "OPENAI_API_KEY",
            "CODEX_API_KEY",
            "CURSOR_API_KEY",
        ] {
            println!(
                "{key}={}",
                env::var(key).unwrap_or_else(|_| "<unset>".to_string())
            );
        }
        return;
    }

    if arguments == ["--version"] {
        println!("fixture-probe-1");
    } else if arguments == ["--help"] {
        println!("fixture probe help");
    } else if arguments == ["auth", "status"] {
        println!("auth required");
    }
}
