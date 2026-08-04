use devmanager::config::paths::{resolve_app_paths, AppProfile, BuildKind};

#[test]
fn native_next_profile_cannot_alias_production() {
    let base = std::path::Path::new(r"C:\Users\tester\AppData\Roaming");
    let production = resolve_app_paths(base, AppProfile::Production, BuildKind::Release).unwrap();
    let next = resolve_app_paths(
        base,
        AppProfile::named("native-next-dev").unwrap(),
        BuildKind::Debug,
    )
    .unwrap();

    assert_eq!(production.root, base.join("com.userfirst.devmanager"));
    assert_eq!(
        next.root,
        base.join("com.userfirst.devmanager-native-next-dev")
    );
    assert!(!next.root.starts_with(&production.root));
    assert_eq!(next.database, next.root.join("kernel.sqlite3"));
    assert_eq!(next.browser_root, next.root.join("browser"));
}

#[test]
fn named_profile_rejects_empty_or_path_shaped_values() {
    for invalid in ["", "..", r"a\b", "a/b", "native next"] {
        assert!(AppProfile::named(invalid).is_err(), "accepted {invalid:?}");
    }
}
