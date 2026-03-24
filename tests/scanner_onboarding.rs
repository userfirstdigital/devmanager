use devmanager::services::scanner_service;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_root(name: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("devmanager-{name}-{unique:x}"))
}

fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parent dir");
    }
    std::fs::write(path, contents).expect("write file");
}

#[test]
fn root_scan_discovers_househunter_style_folders_and_ports() {
    let root = temp_root("scanner-househunter");
    let api = root.join("api");
    let web = root.join("web");
    let archive = root.join("zz-archive");

    write_file(
        &api.join("package.json"),
        r#"{
  "name": "template-api",
  "private": true,
  "scripts": {
    "dev": "tsx watch src/server.ts",
    "build": "tsc -p tsconfig.json"
  }
}"#,
    );
    write_file(&api.join(".env"), "PORT=4555\nSMTP_PORT=1025\n");

    write_file(
        &web.join("package.json"),
        r#"{
  "name": "template-web",
  "private": true,
  "scripts": {
    "dev": "vite",
    "build": "vite build"
  }
}"#,
    );
    write_file(
        &web.join(".env"),
        "VITE_API_BASE_URL=http://localhost:4555/api\nVITE_DEV_PORT=5555\n",
    );

    write_file(&archive.join("package.json"), "{ invalid json");

    let entries = scanner_service::scan_root(root.to_str().expect("temp root path"))
        .expect("scan root succeeds");

    assert_eq!(
        entries.len(),
        2,
        "only active app folders should be discovered"
    );
    assert!(entries.iter().any(|entry| entry.name == "api"));
    assert!(entries.iter().any(|entry| entry.name == "web"));
    assert!(
        entries.iter().all(|entry| entry.name != "zz-archive"),
        "archive folders should be ignored during onboarding"
    );

    let api_entry = entries
        .iter()
        .find(|entry| entry.name == "api")
        .expect("api entry");
    assert!(api_entry.scripts.iter().any(|script| script.name == "dev"));
    assert_eq!(
        scanner_service::auto_selected_port_variable(&api_entry.ports).as_deref(),
        Some("PORT")
    );

    let web_entry = entries
        .iter()
        .find(|entry| entry.name == "web")
        .expect("web entry");
    assert!(web_entry.scripts.iter().any(|script| script.name == "dev"));
    assert_eq!(
        scanner_service::auto_selected_port_variable(&web_entry.ports).as_deref(),
        Some("VITE_DEV_PORT")
    );

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn root_scan_keeps_valid_entries_when_a_sibling_manifest_is_invalid() {
    let root = temp_root("scanner-tolerant-root");
    let api = root.join("api");
    let broken = root.join("broken");

    write_file(
        &api.join("package.json"),
        r#"{
  "name": "template-api",
  "private": true,
  "scripts": {
    "dev": "tsx watch src/server.ts"
  }
}"#,
    );
    write_file(&broken.join("package.json"), "{ invalid json");

    let entries = scanner_service::scan_root(root.to_str().expect("temp root path"))
        .expect("scan root succeeds");

    assert!(
        entries.iter().any(|entry| entry.name == "api"),
        "valid folders should still be discovered"
    );

    std::fs::remove_dir_all(&root).ok();
}
