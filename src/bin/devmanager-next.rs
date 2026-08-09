use devmanager::ui::preview::{run_cli, PreviewPathPolicy};
use std::process::ExitCode;

fn main() -> ExitCode {
    let policy = PreviewPathPolicy::for_workspace(env!("CARGO_MANIFEST_DIR"));
    match run_cli(std::env::args_os().skip(1), &policy) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(2)
        }
    }
}
