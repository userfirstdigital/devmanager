#[cfg(target_os = "windows")]
fn main() {
    build_web_ui_if_needed();
    use winresource::{VersionInfo, WindowsResource};

    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_string());
    let packed_version = pack_windows_version(&version);

    let mut res = WindowsResource::new();
    res.set_icon("packaging/icons/devmanager.ico")
        .set_language(0x0409)
        .set("ProductName", "DevManager")
        .set(
            "FileDescription",
            "Native GPUI workspace and terminal manager for projects, servers, AI sessions, and SSH.",
        )
        .set("CompanyName", "UserFirst")
        .set("LegalCopyright", "Copyright (c) UserFirst")
        .set("OriginalFilename", "devmanager.exe")
        .set("InternalName", "devmanager")
        .set("Comments", "DevManager desktop application")
        .set("ProductVersion", &version)
        .set("FileVersion", &version)
        .set_version_info(VersionInfo::PRODUCTVERSION, packed_version)
        .set_version_info(VersionInfo::FILEVERSION, packed_version);
    res.compile().expect("failed to compile windows resources");
}

#[cfg(not(target_os = "windows"))]
fn main() {
    build_web_ui_if_needed();
}

/// Auto-build the embedded browser web UI when a fresh clone has nothing
/// but the committed stub in `web/bundle/`. CI and manual builds can still
/// run `npm install && npm run build` in `web/` explicitly — this is the
/// safety net so `cargo build` on a clean checkout produces a binary whose
/// embedded SPA is actually the real React app, not the placeholder page.
fn build_web_ui_if_needed() {
    let web_dir = std::path::Path::new("web");
    if !web_dir.exists() {
        return;
    }

    // Tell cargo this build script's output depends on `web/package.json`
    // and the bundle itself. We deliberately do NOT watch `web/src/`: rust
    // edits under src/ rebuild the binary constantly and we don't want to
    // fire npm on every iteration. Web devs should run `npm run build`
    // themselves (or use `npm run dev` with a Vite dev server) when
    // iterating on the SPA.
    println!("cargo:rerun-if-changed=web/package.json");
    println!("cargo:rerun-if-changed=web/bundle/index.html");

    let index_path = web_dir.join("bundle").join("index.html");
    let is_stub = match std::fs::read_to_string(&index_path) {
        Ok(contents) => contents.contains("placeholder shipped"),
        Err(_) => true,
    };
    if !is_stub {
        return;
    }

    // Stub bundle detected. Try to run `npm install` + `npm run build`.
    if !command_exists("npm") {
        println!(
            "cargo:warning=web/bundle/ contains only the placeholder index.html and `npm` \
is not on PATH. The packaged binary will serve the placeholder page. To \
embed the real web UI, install Node.js and run `cd web && npm install && \
npm run build` before `cargo build --release`."
        );
        return;
    }

    let node_modules = web_dir.join("node_modules");
    if !node_modules.exists() {
        println!("cargo:warning=Installing web UI dependencies (npm install)...");
        let status = std::process::Command::new(npm_cmd())
            .arg("install")
            .arg("--no-audit")
            .arg("--no-fund")
            .current_dir(web_dir)
            .status();
        match status {
            Ok(exit) if exit.success() => {}
            _ => {
                println!("cargo:warning=npm install failed; embedded web UI will be the stub.");
                return;
            }
        }
    }

    println!("cargo:warning=Building web UI bundle (npm run build)...");
    let status = std::process::Command::new(npm_cmd())
        .arg("run")
        .arg("build")
        .current_dir(web_dir)
        .status();
    match status {
        Ok(exit) if exit.success() => {}
        _ => println!("cargo:warning=npm run build failed; embedded web UI may be stale."),
    }
}

fn command_exists(name: &str) -> bool {
    std::process::Command::new(npm_cmd_name(name))
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(target_os = "windows")]
fn npm_cmd() -> &'static str {
    "npm.cmd"
}

#[cfg(not(target_os = "windows"))]
fn npm_cmd() -> &'static str {
    "npm"
}

#[cfg(target_os = "windows")]
fn npm_cmd_name(name: &str) -> String {
    if name == "npm" {
        "npm.cmd".to_string()
    } else {
        name.to_string()
    }
}

#[cfg(not(target_os = "windows"))]
fn npm_cmd_name(name: &str) -> String {
    name.to_string()
}

#[cfg(target_os = "windows")]
fn pack_windows_version(version: &str) -> u64 {
    let mut parts = [0u16; 4];
    let normalized = version
        .split_once('-')
        .map(|(base, _)| base)
        .unwrap_or(version)
        .split_once('+')
        .map(|(base, _)| base)
        .unwrap_or(version);

    for (index, part) in normalized.split('.').take(4).enumerate() {
        parts[index] = part.parse::<u16>().unwrap_or(0);
    }

    ((parts[0] as u64) << 48)
        | ((parts[1] as u64) << 32)
        | ((parts[2] as u64) << 16)
        | (parts[3] as u64)
}
