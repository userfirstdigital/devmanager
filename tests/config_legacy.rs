use devmanager::models::Settings;
use devmanager::workspace::apply_browser_enabled_preference;

#[test]
fn legacy_settings_default_and_browser_toggle_round_trip() {
    let legacy: Settings = serde_json::from_str("{}").expect("legacy settings");
    assert_eq!(legacy.browser_enabled, cfg!(windows));

    let mut toggled = legacy;
    apply_browser_enabled_preference(&mut toggled, !cfg!(windows));
    assert_eq!(toggled.browser_enabled, !cfg!(windows));

    let value = serde_json::to_value(&toggled).expect("serialize settings");
    assert_eq!(
        value.get("browserEnabled"),
        Some(&serde_json::json!(!cfg!(windows)))
    );
    let round_trip: Settings = serde_json::from_value(value).expect("round-trip settings");
    assert_eq!(round_trip.browser_enabled, !cfg!(windows));
}

#[test]
fn settings_surface_exposes_browser_toggle_and_scoped_actions() {
    let source = include_str!("../src/workspace/mod.rs");
    assert!(source.contains("pub browser_enabled: bool"));
    assert!(source.contains("ToggleBrowserEnabled"));
    assert!(source.contains("ClearActiveBrowserProfile"));
    assert!(source.contains("ResetActiveBrowserWorkspace"));
    assert!(source.contains("RevealActiveBrowserDownloads"));
    assert!(source.contains("Enable per-conversation Browser"));
}
