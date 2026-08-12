#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

use devmanager::ui::native_shell::run_native_shell;
use devmanager::ui::preview::{run_cli, PreviewPathPolicy};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if let Some(exit_code) =
        devmanager::ai::claude_hooks::run_hook_relay_subcommand(&args, std::io::stdin().lock())
    {
        return exit_code;
    }
    if let Some(exit_code) =
        devmanager::ai::codex_hooks::run_codex_hook_relay_subcommand(&args, std::io::stdin().lock())
    {
        return exit_code;
    }

    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    if !args.iter().any(|argument| argument == "--ui-preview") {
        return match run_native_shell(env!("CARGO_MANIFEST_DIR")) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("{error}");
                ExitCode::from(2)
            }
        };
    }

    let policy = PreviewPathPolicy::for_workspace(env!("CARGO_MANIFEST_DIR"));
    match run_cli(args, &policy) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(2)
        }
    }
}
