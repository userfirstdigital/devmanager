#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

fn main() -> std::process::ExitCode {
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
    devmanager::app::run();
    std::process::ExitCode::SUCCESS
}
