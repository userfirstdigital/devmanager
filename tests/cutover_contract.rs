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
