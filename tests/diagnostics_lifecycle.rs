fn source_section<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
    let start = source
        .find(start)
        .unwrap_or_else(|| panic!("missing source boundary: {start}"));
    let end = source[start..]
        .find(end)
        .map(|offset| start + offset)
        .unwrap_or_else(|| panic!("missing source boundary after {start}: {end}"));
    &source[start..end]
}

#[test]
fn startup_does_not_scan_and_page_open_and_rescan_remain_triggers() {
    let app = include_str!("../src/app/mod.rs").replace("\r\n", "\n");

    let constructor = source_section(
        &app,
        "fn new(cx: &mut Context<Self>) -> Self {",
        "\n    fn diagnostics_summary_text(",
    );
    assert!(
        !constructor.contains("spawn_startup_diagnostics_scan")
            && !constructor.contains("spawn_diagnostics_scan")
            && !constructor.contains("diagnostics_background_scan"),
        "app initialization must not start diagnostics scan/process work"
    );
    assert!(
        !app.contains("fn spawn_startup_diagnostics_scan"),
        "startup diagnostics scan entrypoint must be removed"
    );
    assert!(
        !app.contains("diagnostics_startup_notice_needed"),
        "startup diagnostics banner/notice logic must be removed"
    );

    let open = source_section(
        &app,
        "fn open_diagnostics_action(",
        "fn diagnostics_back_action(",
    );
    assert!(
        open.contains("self.spawn_diagnostics_scan(cx)"),
        "opening Diagnostics must remain a scan trigger"
    );

    let rescan = source_section(
        &app,
        "fn diagnostics_rescan_action(",
        "fn diagnostics_preview_recommended_action(",
    );
    assert!(
        rescan.contains("self.spawn_diagnostics_scan(cx)"),
        "manual Rescan must remain a scan trigger"
    );

    assert!(app.contains("EditorAction::OpenDiagnostics => self.open_diagnostics_action(cx)"));
    assert!(app.contains("EditorAction::DiagnosticsRescan => self.diagnostics_rescan_action(cx)"));
    assert!(app.contains("\"Not scanned yet\""));
}
