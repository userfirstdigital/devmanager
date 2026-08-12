#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

use devmanager::ui::native_shell::run_native_shell;
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
    if args.iter().any(|argument| argument == "--ui-preview") {
        return run_ui_preview(args);
    }

    run_product_shell()
}

fn run_product_shell() -> ExitCode {
    #[cfg(debug_assertions)]
    let result = run_native_shell(env!("CARGO_MANIFEST_DIR"));
    #[cfg(not(debug_assertions))]
    // Release builds resolve production without package-source path reliance.
    let result = run_native_shell(std::path::Path::new("."));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(2)
        }
    }
}

fn run_ui_preview(args: Vec<std::ffi::OsString>) -> ExitCode {
    #[cfg(debug_assertions)]
    {
        use devmanager::ui::preview::{run_cli, PreviewPathPolicy};
        // Preview fixtures resolve against the package workspace in debug only.
        let policy = PreviewPathPolicy::for_workspace(env!("CARGO_MANIFEST_DIR"));
        match run_cli(args, &policy) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("{error}");
                ExitCode::from(2)
            }
        }
    }
    #[cfg(not(debug_assertions))]
    {
        let _ = args;
        eprintln!("--ui-preview is available only in debug builds");
        ExitCode::from(2)
    }
}
