//! Phase 11 cutover source contracts.
//!
//! These tests inspect source and path presence only. They do not launch the
//! product, host, or installed profile.

use std::fs;
use std::path::{Path, PathBuf};

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read_source(relative: &str) -> String {
    let path = crate_root().join(relative);
    fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!("failed to read {}: {error}", path.display());
    })
}

#[test]
fn entry_product_main_dispatches_native_shell_after_hook_relays() {
    let main = read_source("src/main.rs");
    assert!(
        main.contains("run_hook_relay_subcommand"),
        "Claude hook relay dispatch must remain on the product entry"
    );
    assert!(
        main.contains("run_codex_hook_relay_subcommand"),
        "Codex hook relay dispatch must remain on the product entry"
    );
    assert!(
        main.contains("run_native_shell"),
        "ordinary launch must enter the native shell"
    );
    assert!(
        !main.contains("devmanager::app::run()") && !main.contains("app::run()"),
        "legacy app::run() must not remain the product entry"
    );
}

#[test]
fn entry_devmanager_next_binary_identity_is_absent() {
    assert!(
        !crate_root().join("src/bin/devmanager-next.rs").exists(),
        "development-only src/bin/devmanager-next.rs must be deleted"
    );
    let cargo = read_source("Cargo.toml");
    assert!(
        !cargo.contains("name = \"devmanager-next\""),
        "Cargo.toml must not declare a devmanager-next binary"
    );
}

#[test]
fn entry_ui_root_exposes_native_shell_module() {
    let ui_mod = read_source("src/ui/mod.rs");
    assert!(
        ui_mod.contains("pub mod native_shell;"),
        "src/ui/mod.rs must expose native_shell for the product entry"
    );
    assert!(
        ui_mod.contains("pub mod terminal_adapter;"),
        "src/ui/mod.rs must expose terminal_adapter for native_shell"
    );
    assert!(
        Path::new(&crate_root().join("src/ui/native_shell.rs")).is_file(),
        "native shell source must exist"
    );
}

#[test]
fn entry_has_no_runtime_ui_selector() {
    let main = read_source("src/main.rs");
    for forbidden in [
        "new_ui",
        "native_next",
        "use_old",
        "legacy_ui",
        "--legacy",
        "use_legacy",
    ] {
        assert!(
            !main.contains(forbidden),
            "product entry must not contain runtime UI switch {forbidden:?}"
        );
    }
}

#[test]
fn entry_host_binary_remains_alongside_product_main() {
    assert!(
        crate_root().join("src/bin/devmanager-host.rs").is_file(),
        "durable host binary must remain for attach-first startup"
    );
    assert!(
        crate_root().join("src/main.rs").is_file(),
        "sole product client entry must be src/main.rs"
    );
}

#[test]
fn entry_production_host_is_not_deferred_and_omits_parent_pid() {
    let host = read_source("src/bin/devmanager-host.rs");
    assert!(
        !host.contains("release host startup is deferred until Phase 11"),
        "production host startup must not remain deferred"
    );
    assert!(
        host.contains("parse_production_args") && host.contains("prepare_production_paths"),
        "production host must resolve Production profile paths"
    );
    assert!(
        host.contains("PRODUCTION_HOST_PROFILE"),
        "production host must use a stable production pipe/lock profile"
    );
    let shell = read_source("src/ui/native_shell.rs");
    assert!(
        shell.contains("try_attach_existing_host"),
        "client must attach-first before launching the sibling host"
    );
    assert!(
        shell.contains("DetachOnClientClose"),
        "production client close must detach without killing the durable host"
    );
    assert!(
        shell.contains("\"devmanager/\"")
            || shell.contains("CLIENT_BUILD_PREFIX: &str = \"devmanager\""),
        "client identity must be stable devmanager/<version>"
    );
    assert!(
        !shell.contains("\"devmanager-next/"),
        "product runtime must not advertise devmanager-next client builds"
    );
    let production_args_slice = shell
        .split("NativeHostLaunchMode::Production =>")
        .nth(1)
        .unwrap_or_default();
    assert!(
        production_args_slice.contains("--foreground"),
        "production launch must still pass --foreground"
    );
    assert!(
        !production_args_slice
            .lines()
            .take(8)
            .any(|line| line.contains("--parent-pid")),
        "production launch must not pass --parent-pid"
    );
}

#[test]
fn entry_missing_pipe_normalizes_to_unavailable_then_spawns_host() {
    let connection = read_source("src/client/connection.rs");
    assert!(
        connection.contains("map_named_pipe_open_error"),
        "named-pipe open must normalize absence through a dedicated mapper"
    );
    assert!(
        connection.contains("ERROR_FILE_NOT_FOUND")
            || connection.contains("raw_os_error() == Some(2)"),
        "missing pipe must map ERROR_FILE_NOT_FOUND/NotFound to Unavailable"
    );
    assert!(
        connection.contains("ERROR_PIPE_BUSY")
            || connection.contains("raw_os_error() == Some(231)"),
        "pipe busy must stay Busy for bounded attach retry"
    );
    let shell = read_source("src/ui/native_shell.rs");
    assert!(
        shell.contains("Err(IpcError::Unavailable) => break"),
        "Unavailable (missing pipe) must fall through to host spawn"
    );
    assert!(
        shell.contains("Err(IpcError::Timeout)") && shell.contains("return Err(IpcError::Timeout)"),
        "present-but-slow attach must preserve Timeout and must not become Unavailable"
    );
    assert!(
        shell.contains("sanitize_spawned_host_environment"),
        "spawned host environment must be sanitized"
    );
}

#[test]
fn entry_hook_relay_dispatch_remains_before_shell_and_preview() {
    let main = read_source("src/main.rs");
    let main_fn = main.split("fn main()").nth(1).expect("main function body");
    let claude = main_fn
        .find("run_hook_relay_subcommand")
        .expect("claude hook relay");
    let codex = main_fn
        .find("run_codex_hook_relay_subcommand")
        .expect("codex hook relay");
    let preview = main_fn.find("--ui-preview").expect("preview gate");
    let product = main_fn
        .find("run_product_shell")
        .expect("product shell dispatch");
    assert!(
        claude < codex && codex < preview.min(product),
        "hook relays must remain ahead of product shell and preview dispatch"
    );
}

#[test]
fn entry_host_ctl_json_dispatch_remains_before_host_lock() {
    let host = read_source("src/bin/devmanager-host.rs");
    let ctl = host.find("\"ctl\"").expect("ctl dispatch");
    let lock = host.find("acquire_lock").expect("host lock acquisition");
    assert!(
        ctl < lock,
        "devmanager-host ctl JSON automation must dispatch before HostLock"
    );
    assert!(
        host.contains("dispatch_ctl_from_args"),
        "ctl must remain the typed JSON automation entry"
    );
}

#[test]
fn entry_preview_identity_uses_devmanager_binary_not_next() {
    let preview = read_source("src/ui/preview.rs");
    assert!(
        preview.contains("usage: devmanager --ui-preview"),
        "preview CLI usage must advertise the sole devmanager binary"
    );
    assert!(
        !preview.contains("devmanager-next --ui-preview")
            && !preview.contains("gpui::actions!(devmanager_next"),
        "preview must not retain devmanager-next identity"
    );
    let main = read_source("src/main.rs");
    assert!(
        main.contains("debug builds") || main.contains("debug_assertions"),
        "release product must not rely on CARGO_MANIFEST_DIR for preview"
    );
    let script = read_source("scripts/native-next/Capture-UiPreviews.ps1");
    assert!(
        script.contains("$artifactName = 'devmanager'")
            && script.contains("$artifactBinaryName = 'devmanager.exe'")
            && script.contains("'--bin', 'devmanager'"),
        "Capture-UiPreviews must build and use the sole devmanager binary"
    );
    assert!(
        !script.contains("$artifactName = 'devmanager-next'")
            && !script.contains("'--bin', 'devmanager-next'"),
        "Capture-UiPreviews must not target the deleted next binary"
    );
}
