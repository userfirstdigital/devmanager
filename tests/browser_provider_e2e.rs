//! Ordinary fixture validation and explicit authenticated HOLD tests.
//!
//! The default path never launches Claude, Codex, Cursor, WebView2, or the
//! installed DevManager app. A local HTTP round-trip uses the fixture-server
//! binary only when that binary is discoverable.

use devmanager::browser::{
    browser_fixture_root, hold_authenticated_provider_launch, real_provider_launch_is_forbidden,
    validate_browser_fixture_site, BrowserFixtureAction, BrowserFixtureRecoveryCase,
    BrowserProviderArm, BrowserProviderE2EHold, BROWSER_E2E_VERIFICATION_TOKEN,
    BROWSER_FIXTURE_CASES,
};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn manifest_path(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative)
}

fn e2e_fixture_root() -> PathBuf {
    manifest_path("tests/fixtures/browser-e2e")
}

fn discover_fixture_server_binary() -> Option<PathBuf> {
    if let Some(path) = option_env!("CARGO_BIN_EXE_browser_fixture_server") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

fn assert_no_external_or_secret(body: &str, label: &str) {
    assert!(
        !body.contains("https://") && !body.contains("http://"),
        "{label} must not contain external URLs"
    );
    assert!(
        !body.contains("sk-") && !body.contains("secret:"),
        "{label} must not contain secrets"
    );
    assert!(
        !body.to_ascii_lowercase().contains("bearer "),
        "{label} must not contain a bearer token"
    );
}

#[test]
fn fixture_cases_cover_required_actions_and_recovery() {
    let mut actions = Vec::new();
    let mut recoveries = Vec::new();
    for case in BROWSER_FIXTURE_CASES {
        assert_eq!(case.expected_token, BROWSER_E2E_VERIFICATION_TOKEN);
        assert!(!case.id.is_empty());
        assert!(!case.prompt.is_empty());
        actions.extend(case.actions.iter().copied());
        if let Some(recovery) = case.recovery {
            recoveries.push(recovery);
        }
    }
    for required in [
        BrowserFixtureAction::Navigate,
        BrowserFixtureAction::InspectValue,
        BrowserFixtureAction::FillNonSecretForm,
        BrowserFixtureAction::ChooseOption,
        BrowserFixtureAction::Submit,
        BrowserFixtureAction::OpenTab,
        BrowserFixtureAction::DownloadArtifact,
        BrowserFixtureAction::UploadArtifact,
        BrowserFixtureAction::HandlePermission,
        BrowserFixtureAction::ReportVerificationToken,
    ] {
        assert!(
            actions.contains(&required),
            "BROWSER_FIXTURE_CASES missing action {required:?}"
        );
    }
    for required in [
        BrowserFixtureRecoveryCase::NavigationError,
        BrowserFixtureRecoveryCase::RendererCrash,
        BrowserFixtureRecoveryCase::ProviderCrash,
        BrowserFixtureRecoveryCase::HostFullQuit,
        BrowserFixtureRecoveryCase::FailedBrowserLaunch,
    ] {
        assert!(
            recoveries.contains(&required),
            "BROWSER_FIXTURE_CASES missing recovery {required:?}"
        );
    }
}

#[test]
fn e2e_fixture_site_satisfies_public_validation_and_local_policy() {
    let root = e2e_fixture_root();
    let validation = validate_browser_fixture_site(&root).expect("e2e fixture site");
    assert_eq!(validation.cases, BROWSER_FIXTURE_CASES.len());
    assert_eq!(validation.token, BROWSER_E2E_VERIFICATION_TOKEN);
    assert_eq!(validation.network_urls, 0);
    assert_eq!(validation.secrets_in_manifest, 0);

    let index = std::fs::read_to_string(root.join("index.html")).expect("index");
    for marker in [
        "data-testid=\"semantic-target\"",
        "data-testid=\"fixture-form\"",
        "data-testid=\"fixture-select\"",
        "data-testid=\"submit-form\"",
        "data-testid=\"new-tab-link\"",
        "data-testid=\"fixture-download\"",
        "data-testid=\"fixture-upload\"",
        "data-testid=\"permission-target\"",
        "data-testid=\"verification-token\"",
        "href=\"./tab.html\"",
        BROWSER_E2E_VERIFICATION_TOKEN,
    ] {
        assert!(index.contains(marker), "missing marker {marker}");
    }
    assert_no_external_or_secret(&index, "index.html");
    let tab = std::fs::read_to_string(root.join("tab.html")).expect("tab");
    assert!(tab.contains("data-testid=\"second-tab-target\""));
    assert_no_external_or_secret(&tab, "tab.html");
    assert!(root.join("download.txt").is_file());
    assert!(root.join("upload-marker.txt").is_file());
    assert!(root.join("permission.html").is_file());
    assert!(root.join("navigation-error.html").is_file());
    assert!(root.join("renderer-crash.html").is_file());
    assert!(root.join("recovery/host-full-quit.html").is_file());
    assert!(root.join("recovery/provider-crash.html").is_file());
    assert!(root.join("recovery/failed-browser-launch.html").is_file());
}

#[test]
fn contract_public_fixture_root_still_names_browser_site() {
    let declared = browser_fixture_root();
    assert!(
        declared.ends_with(Path::new("tests/fixtures/browser-site")),
        "public browser_fixture_root still names the older Task 5A site: {}",
        declared.display()
    );
    let e2e = e2e_fixture_root();
    assert_ne!(declared, e2e);
    assert!(
        e2e.is_dir(),
        "Phase 8 e2e fixtures live at tests/fixtures/browser-e2e without editing production"
    );
}

#[test]
fn authenticated_provider_launch_stays_hold_for_every_allowlisted_name() {
    assert!(real_provider_launch_is_forbidden());
    for provider in [None, Some("claude"), Some("codex"), Some("cursor")] {
        let record = hold_authenticated_provider_launch(provider);
        assert_eq!(record.arm, BrowserProviderArm::AuthenticatedHold);
        assert!(!record.launched);
        assert_eq!(
            record.hold,
            BrowserProviderE2EHold::AuthenticatedLaunchRequiresExplicitOptIn
        );
        assert_eq!(record.provider.as_deref(), provider);
    }
}

#[test]
fn contract_provider_e2e_script_default_never_launches_stock_providers() {
    let script = std::fs::read_to_string(manifest_path(
        "scripts/native-next/Invoke-BrowserProviderE2E.ps1",
    ))
    .expect("provider e2e script");
    assert!(script.contains("Set-StrictMode"));
    assert!(script.contains("[switch]$Fixture"));
    assert!(script.contains("[switch]$IncludeProjectionFixture"));
    assert!(script.contains("[switch]$IncludeRecovery"));
    assert!(script.contains("[switch]$Authenticated"));
    assert!(script.contains("[string[]]$Provider"));
    assert!(script.contains("DEVMANAGER_ALLOW_AUTHENTICATED_BROWSER_E2E"));
    assert!(script.contains("[string]$ConfigBase"));
    assert!(script.contains("claude"));
    assert!(script.contains("codex"));
    assert!(script.contains("cursor"));
    assert!(
        !script.contains("Start-Process"),
        "default provider script must not use Start-Process"
    );
    assert!(
        !script.contains("claude.exe")
            && !script.contains("codex.exe")
            && !script.contains("cursor.exe"),
        "script must not name installed provider executables"
    );
}

#[test]
fn hold_and_fixture_prompts_are_not_persisted_by_this_test() {
    let mut evidence = serde_json::Map::new();
    evidence.insert("arm".into(), serde_json::Value::String("fixture".into()));
    evidence.insert(
        "tokenRef".into(),
        serde_json::Value::String("BROWSER_E2E_VERIFICATION_TOKEN".into()),
    );
    evidence.insert("launched".into(), serde_json::Value::Bool(false));
    let serialized = serde_json::Value::Object(evidence).to_string();
    assert!(!serialized.contains(BROWSER_FIXTURE_CASES[0].prompt));
    assert!(!serialized.contains("Bearer "));
    assert!(!serialized.contains("sk-"));
}

fn wait_ready_line(child: &mut std::process::Child, timeout: Duration) -> String {
    let stdout = child.stdout.as_mut().expect("stdout");
    let deadline = Instant::now() + timeout;
    let mut collected = Vec::new();
    let mut byte = [0u8; 1];
    while Instant::now() < deadline {
        match stdout.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {
                collected.push(byte[0]);
                if collected.ends_with(b"\n") {
                    let line = String::from_utf8_lossy(&collected).to_string();
                    if line.contains("BROWSER_FIXTURE_SERVER_READY") {
                        return line;
                    }
                    collected.clear();
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("read fixture server stdout: {error}"),
        }
    }
    panic!(
        "fixture server did not emit a ready line: {}",
        String::from_utf8_lossy(&collected)
    );
}

fn http_get(url_path: &str, port: u16) -> (u16, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect fixture server");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
    write!(
        stream,
        "GET {url_path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )
    .expect("write request");
    let mut response = Vec::new();
    let mut chunk = [0u8; 4096];
    let complete_len = loop {
        let read = stream.read(&mut chunk).expect("read response");
        assert!(read > 0, "fixture server closed before a complete response");
        response.extend_from_slice(&chunk[..read]);
        assert!(response.len() <= 1_048_576, "fixture response is unbounded");
        let Some(header_end) = response.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let headers = std::str::from_utf8(&response[..header_end]).expect("response headers");
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().expect("content length"))
            })
            .expect("fixture response content length");
        let complete_len = header_end + 4 + content_length;
        if response.len() >= complete_len {
            break complete_len;
        }
    };
    response.truncate(complete_len);
    let response = String::from_utf8(response).expect("utf-8 fixture response");
    let status = response
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    (status, response)
}

fn stop_fixture_server(child: &mut Child) {
    if child
        .try_wait()
        .expect("inspect fixture server status")
        .is_none()
    {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = writeln!(stdin, "shutdown");
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if child
                .try_wait()
                .expect("inspect fixture server shutdown")
                .is_some()
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = child.kill();
    }
    let status = child.wait().expect("wait for fixture server");
    assert!(
        status.success(),
        "fixture server exited unsuccessfully: {status}"
    );
}

#[test]
fn local_http_round_trip_uses_fixture_server_or_unit_helper() {
    if let Some(binary) = discover_fixture_server_binary() {
        let mut child = Command::new(&binary)
            .arg("--root")
            .arg(e2e_fixture_root())
            .arg("--port")
            .arg("0")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn fixture server");
        let ready = wait_ready_line(&mut child, Duration::from_secs(10));
        let payload = ready
            .split_once("BROWSER_FIXTURE_SERVER_READY ")
            .map(|(_, rest)| rest.trim())
            .expect("ready payload");
        let json: serde_json::Value = serde_json::from_str(payload).expect("ready json");
        let url = json["url"].as_str().expect("url");
        let port: u16 = url
            .trim_start_matches("http://127.0.0.1:")
            .trim_end_matches('/')
            .parse()
            .expect("port");
        assert_eq!(json["pid"].as_u64(), Some(u64::from(child.id())));
        let (health_status, health) = http_get("/health", port);
        assert_eq!(health_status, 200);
        assert!(health.contains("\"ok\":true"));
        let (index_status, index) = http_get("/index.html", port);
        assert_eq!(index_status, 200);
        assert!(index.contains(BROWSER_E2E_VERIFICATION_TOKEN));
        assert!(!index.contains("Bearer "));
        let (denied_status, _) = http_get("/../Cargo.toml", port);
        assert_eq!(denied_status, 400);
        stop_fixture_server(&mut child);
        assert!(
            child
                .try_wait()
                .expect("inspect stopped fixture server")
                .is_some(),
            "fixture server process must be reaped"
        );
        return;
    }

    let helper = TcpListener::bind("127.0.0.1:0").expect("unit helper bind");
    let port = helper.local_addr().expect("addr").port();
    let root = e2e_fixture_root();
    let thread = std::thread::spawn(move || {
        let (mut stream, _) = helper.accept().expect("accept");
        let mut request = [0u8; 1024];
        let _ = stream.read(&mut request);
        let body = std::fs::read_to_string(root.join("verification.json")).expect("token file");
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.shutdown(Shutdown::Both);
    });
    let (status, body) = http_get("/verification.json", port);
    assert_eq!(status, 200);
    assert!(body.contains(BROWSER_E2E_VERIFICATION_TOKEN));
    assert!(!body.contains("Bearer "));
    thread.join().expect("helper finished");

    let source = include_str!("../src/bin/browser-fixture-server.rs");
    assert!(source.contains("fn validate_root"));
    assert!(source.contains("--isolated-parent"));
    assert!(source.contains("MAX_REQUEST_LINE_BYTES"));
    assert!(source.contains("MAX_HEADER_BYTES"));
    assert!(source.contains("TcpListener::bind"));
}

#[test]
fn docs_matrix_and_adr_exist_and_do_not_claim_visible_webview2() {
    let matrix =
        std::fs::read_to_string(manifest_path("docs/browser-e2e-matrix.md")).expect("matrix");
    assert!(matrix.contains("portable fixture proof"));
    assert!(matrix.contains("Windows/WebView2"));
    assert!(matrix.contains("NOT proven"));
    assert!(matrix.contains("Invoke-BrowserSurfaceProof.ps1"));
    assert!(matrix.contains("Invoke-BrowserProviderE2E.ps1"));
    let adr = std::fs::read_to_string(manifest_path(
        "docs/adr/0001-host-owned-webview2-surface.md",
    ))
    .expect("adr");
    assert!(adr.contains("Status"));
    assert!(adr.contains("opt-in"));
    assert!(
        !adr.to_ascii_lowercase()
            .contains("visible webview2 proof passed"),
        "ADR must not fabricate a passing visible WebView2 run"
    );
    let webview_test = manifest_path("tests/browser_webview2_e2e.rs");
    assert!(
        webview_test.is_file(),
        "the documented Windows WebView2 capability test must be tracked"
    );
    let webview_source = std::fs::read_to_string(webview_test).expect("read WebView2 test");
    assert!(webview_source.contains("DEVMANAGER_BROWSER_WEBVIEW2_E2E"));
}
