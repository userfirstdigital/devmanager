#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

use devmanager::ui::native_shell::run_native_shell;
use std::process::ExitCode;

fn main() -> ExitCode {
    // Sole product desktop entry: one native GPUI client plus durable host.
    // Hook relays and debug-only --ui-preview run before the product shell.
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    // Provider relay arguments are textual; preserve native OS paths for the
    // debug workspace entry instead of panicking on a non-UTF-8 Linux path.
    if let Some(text_args) = args
        .iter()
        .map(|arg| arg.to_str().map(str::to_owned))
        .collect::<Option<Vec<_>>>()
    {
        #[cfg(target_os = "linux")]
        if let Some(code) = devmanager::process::linux_cgroup::run_guardian_subcommand(&text_args) {
            return code;
        }
        if let Some(code) = devmanager::ai::claude_hooks::run_hook_relay_subcommand(
            &text_args,
            std::io::stdin().lock(),
        ) {
            return code;
        }
        if let Some(code) = devmanager::ai::codex_hooks::run_codex_hook_relay_subcommand(
            &text_args,
            std::io::stdin().lock(),
        ) {
            return code;
        }
    }

    if args
        .iter()
        .any(|argument| argument == "--ui-preview" || argument == "--ui-preview-live")
    {
        return run_ui_preview(args);
    }
    run_product_shell(args)
}

fn run_product_shell(args: Vec<std::ffi::OsString>) -> ExitCode {
    #[cfg(debug_assertions)]
    let result = debug_workspace_from_args(&args)
        .and_then(|workspace| run_native_shell(workspace).map_err(|error| error.to_string()));
    #[cfg(not(debug_assertions))]
    // Release builds resolve production without package-source path reliance.
    let result = if args.is_empty() {
        run_native_shell(std::path::Path::new(".")).map_err(|error| error.to_string())
    } else {
        Err("Workspace overrides are available only in debug builds.".to_string())
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(2)
        }
    }
}

#[cfg(debug_assertions)]
fn debug_workspace_from_args(args: &[std::ffi::OsString]) -> Result<std::path::PathBuf, String> {
    match args {
        [] => Ok(std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))),
        [flag, path] if flag == "--dev-workspace" && std::path::Path::new(path).is_absolute() => {
            Ok(std::path::PathBuf::from(path))
        }
        _ => Err("Usage: devmanager [--dev-workspace <absolute directory>]".to_string()),
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn candidate_workspace_is_explicit_and_does_not_require_the_build_machine_path() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("candidate with spaces");
        std::fs::create_dir(&path).unwrap();
        let args = [
            OsString::from("--dev-workspace"),
            path.clone().into_os_string(),
        ];
        assert_eq!(debug_workspace_from_args(&args).unwrap(), path);
        assert_eq!(
            debug_workspace_from_args(&[]).unwrap(),
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        );
        for args in [
            vec![OsString::from("--dev-workspace")],
            vec![
                OsString::from("--dev-workspace"),
                OsString::from("relative"),
            ],
            vec![
                OsString::from("--profile"),
                root.path().as_os_str().to_owned(),
            ],
            vec![
                OsString::from("--dev-workspace"),
                root.path().as_os_str().to_owned(),
                OsString::from("extra"),
            ],
        ] {
            assert!(debug_workspace_from_args(&args).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn candidate_workspace_preserves_native_path_bytes() {
        use std::os::unix::ffi::OsStringExt;
        let path = OsString::from_vec(b"/tmp/candidate-\xff".to_vec());
        assert_eq!(
            debug_workspace_from_args(&[OsString::from("--dev-workspace"), path.clone()])
                .unwrap()
                .into_os_string(),
            path
        );
    }
}

fn run_ui_preview(args: Vec<std::ffi::OsString>) -> ExitCode {
    #[cfg(debug_assertions)]
    {
        use devmanager::ui::preview::{run_cli, run_live_cli, PreviewPathPolicy};
        // Preview fixtures resolve against the package workspace in debug only.
        let policy = PreviewPathPolicy::for_workspace(env!("CARGO_MANIFEST_DIR"));
        let result = if args.first().is_some_and(|arg| arg == "--ui-preview-live") {
            run_live_cli(args, &policy)
        } else {
            run_cli(args, &policy)
        };
        match result {
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
