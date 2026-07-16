use std::path::PathBuf;

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/browser-site")
}

fn fixture(name: &str) -> String {
    std::fs::read_to_string(fixture_root().join(name))
        .unwrap_or_else(|error| panic!("read browser fixture {name}: {error}"))
}

#[test]
fn loopback_browser_fixture_covers_task_5a_protocol_scenarios() {
    let index = fixture("index.html");
    for marker in [
        "data-testid=\"semantic-target\"",
        "aria-label=\"Semantic action target\"",
        "data-testid=\"fixture-form\"",
        "type=\"password\"",
        "data-testid=\"delayed-target\"",
        "setTimeout",
        "window.open",
        "target=\"_blank\"",
        "console.error",
        "throw new Error",
        "fetch(\"./api-success.json\")",
        "fetch(\"./api-missing.json\")",
        "XMLHttpRequest",
        "type=\"file\"",
        "data-testid=\"fixture-upload\"",
        "download=\"fixture-download.txt\"",
        "data-testid=\"never-appears-trigger\"",
    ] {
        assert!(index.contains(marker), "missing fixture marker {marker}");
    }

    let redirect = fixture("redirect.html");
    assert!(redirect.contains("./destination.html"));
    assert!(fixture("destination.html").contains("data-testid=\"redirect-destination\""));

    let success: serde_json::Value =
        serde_json::from_str(&fixture("api-success.json")).expect("valid success JSON");
    assert_eq!(success["ok"], true);
    assert_eq!(success["source"], "devmanager-loopback-fixture");
    assert_eq!(
        fixture("download.txt"),
        "DevManager browser fixture download.\n"
    );

    let all_static_content = [
        index,
        redirect,
        fixture("destination.html"),
        fixture("api-success.json"),
    ]
    .join("\n");
    assert!(!all_static_content.contains("https://"));
    assert!(!all_static_content.contains("http://"));
}
