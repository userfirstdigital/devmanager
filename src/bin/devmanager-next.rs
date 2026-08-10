use devmanager::ui::preview::{run_cli, PreviewPathPolicy};
use devmanager::ui::task_cockpit::run_native_next;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    match args.next() {
        Some(command) if command == "--ui-preview" => {
            let mut preview_args = vec![command];
            preview_args.extend(args);
            let policy = PreviewPathPolicy::for_workspace(env!("CARGO_MANIFEST_DIR"));
            match run_cli(preview_args, &policy) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("{error}");
                    ExitCode::from(2)
                }
            }
        }
        None => {
            run_native_next();
            ExitCode::SUCCESS
        }
        Some(command) => {
            eprintln!("unknown devmanager-next command: {command:?}");
            ExitCode::from(2)
        }
    }
}
