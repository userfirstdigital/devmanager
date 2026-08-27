//! Core behavioral regressions for provider settings.

use std::collections::BTreeMap;
use std::ffi::OsString;

use crate::domain::TaskId;
use crate::providers::registry::ProviderDiscoveryConfig;
use crate::providers::settings::launch_policy::{resolve_launch_config, ProviderInstanceScope};
use crate::providers::settings::model::{
    builtin_slugs_for_driver, BuiltinProviderDriver, ProviderDriverKind, ProviderEnvVar,
    ProviderInstanceConfig, ProviderInstanceId, ProviderSettingsDocument, ProviderSettingsError,
};
use crate::providers::settings::secret::{
    decode_os_string_map, encode_os_string_map, protect_bytes, reveal_bytes,
};
use crate::providers::settings::{
    default_instance_id_for_kind, parse_cursor_about_json, prepare_codex_shadow_home,
    CursorAboutAuth, ProviderHealthCache, ProviderInstanceBindingStore, ProviderProfileOwner,
    ProviderSettingsAuthority, ProviderSettingsMutation, DEFAULT_HEALTH_INTERVAL_SECS,
};
use crate::providers::ProviderKind;
use tempfile::tempdir;

#[test]
fn configuration_fingerprint_frames_fields_and_includes_launch_args() {
    let mut a = ProviderInstanceConfig::builtin_default(BuiltinProviderDriver::Claude);
    a.binary_path = Some("a|b".into());
    a.home_path = Some("c".into());
    let mut b = a.clone();
    b.binary_path = Some("a".into());
    b.home_path = Some("b|c".into());
    assert_ne!(
        a.launch_identity_fingerprint(),
        b.launch_identity_fingerprint()
    );
    b = a.clone();
    b.launch_args.push("--verbose".into());
    assert_ne!(
        a.launch_identity_fingerprint(),
        b.launch_identity_fingerprint()
    );
    b = a.clone();
    b.display_name = "Cosmetic rename".into();
    assert_eq!(
        a.launch_identity_fingerprint(),
        b.launch_identity_fingerprint()
    );
}

#[test]
fn canonical_default_binding_fingerprint_remains_compatible() {
    use sha2::{Digest, Sha256};
    for driver in [
        BuiltinProviderDriver::Claude,
        BuiltinProviderDriver::Codex,
        BuiltinProviderDriver::Cursor,
    ] {
        let mut instance = ProviderInstanceConfig::builtin_default(driver);
        let expected = format!(
            "{:x}",
            Sha256::digest(
                format!("{}|{}||||", instance.instance_id, instance.driver.as_str()).as_bytes()
            )
        );
        let current = instance.launch_identity_fingerprint();
        assert_ne!(current, expected);
        assert!(instance.matches_launch_identity_fingerprint(&current));
        assert!(instance.matches_launch_identity_fingerprint(&expected));
        let dir = tempdir().unwrap();
        let store = ProviderInstanceBindingStore::open_dir(dir.path()).unwrap();
        let settings = ProviderSettingsDocument::with_builtins();
        let task = TaskId::new();
        store
            .bind_on_first_launch(
                &task,
                instance.instance_id.as_str(),
                instance.driver.as_str(),
                Some(expected.clone()),
                &settings,
            )
            .unwrap();
        store.require_binding_for_resume(&task, &settings).unwrap();
        store
            .bind_on_first_launch(
                &task,
                instance.instance_id.as_str(),
                instance.driver.as_str(),
                Some(current.clone()),
                &settings,
            )
            .unwrap();
        instance.launch_args.push("--verbose".into());
        assert!(!instance.matches_launch_identity_fingerprint(&expected));
        assert!(!instance.matches_launch_identity_fingerprint(&current));
    }
}

#[test]
fn same_executable_different_env_scopes_differ() {
    let mut a = ProviderInstanceConfig::builtin_default(BuiltinProviderDriver::Claude);
    a.home_path = Some("C:/homes/a".into());
    let mut b = a.clone();
    b.instance_id = ProviderInstanceId::new("claude_b").unwrap();
    b.home_path = Some("C:/homes/b".into());
    let scope_a = ProviderInstanceScope::from_instance(&a);
    let scope_b = ProviderInstanceScope::from_instance(&b);
    assert_ne!(scope_a.as_cache_key(), scope_b.as_cache_key());
    let ra = resolve_launch_config(&a, b"scope", None).unwrap();
    let rb = resolve_launch_config(&b, b"scope", None).unwrap();
    assert_eq!(ra.discovery.child_environment, ra.environment);
    assert_ne!(ra.environment, rb.environment);
}

#[test]
fn probe_and_launch_share_same_resolved_env() {
    let mut inst = ProviderInstanceConfig::builtin_default(BuiltinProviderDriver::Codex);
    inst.home_path = Some("C:/codex-home".into());
    inst.environment.push(ProviderEnvVar {
        name: "EXTRA".into(),
        value: Some("1".into()),
        sensitive: false,
        protected_value: None,
        value_redacted: false,
    });
    let resolved = resolve_launch_config(&inst, b"scope", None).unwrap();
    assert_eq!(resolved.discovery.child_environment, resolved.environment);
    assert!(resolved
        .environment
        .contains_key(&OsString::from("CODEX_HOME")));
    assert!(resolved.environment.contains_key(&OsString::from("EXTRA")));
}

#[test]
fn discovery_path_uses_the_sealed_instance_environment() {
    let mut instance = ProviderInstanceConfig::builtin_default(BuiltinProviderDriver::Codex);
    instance.environment.push(ProviderEnvVar {
        name: "PATH".into(),
        value: Some("C:/fixture/provider-bin".into()),
        sensitive: false,
        protected_value: None,
        value_redacted: false,
    });
    let resolved = resolve_launch_config(&instance, b"scope", None).unwrap();
    assert_eq!(
        resolved.discovery.path,
        Some(OsString::from("C:/fixture/provider-bin"))
    );
    assert_eq!(
        resolved.discovery.path.as_ref(),
        resolved.environment.get(&OsString::from("PATH"))
    );
}

#[test]
fn reserved_arg_aliases_rejected() {
    let mut inst = ProviderInstanceConfig::builtin_default(BuiltinProviderDriver::Codex);
    for arg in [
        "-c",
        "--config=foo",
        "-s",
        "--sandbox",
        "--dangerously-bypass-approvals-and-sandbox",
        "resume",
        "--cd=/tmp",
        "-C",
    ] {
        inst.launch_args = vec![arg.into()];
        assert!(
            matches!(
                inst.validate(),
                Err(ProviderSettingsError::ReservedLaunchArg(_))
            ),
            "expected reject for {arg}"
        );
    }
    let mut claude = ProviderInstanceConfig::builtin_default(BuiltinProviderDriver::Claude);
    claude.launch_args = vec!["-p".into()];
    assert!(matches!(
        claude.validate(),
        Err(ProviderSettingsError::ReservedLaunchArg(_))
    ));
}

#[cfg(windows)]
#[test]
fn secret_runtime_roundtrip_no_plaintext_in_protected_blob() {
    let mut map = BTreeMap::new();
    map.insert(
        OsString::from("TOKEN"),
        OsString::from("super-secret-value"),
    );
    let bytes = encode_os_string_map(&map).unwrap();
    let protected = protect_bytes(&bytes, b"launch-scope").unwrap();
    assert!(!protected.contains("super-secret-value"));
    let revealed = reveal_bytes(&protected, b"launch-scope").unwrap();
    let back = decode_os_string_map(&revealed).unwrap();
    assert_eq!(map, back);
}

#[test]
fn binding_concurrent_writes_serialize() {
    let dir = tempdir().unwrap();
    let owner = ProviderProfileOwner::open_dir_for_test(dir.path()).unwrap();
    let settings = owner.settings.snapshot();
    let t1 = TaskId::new();
    let t2 = TaskId::new();
    owner
        .bindings
        .bind_on_first_launch(&t1, "claude", "claude", Some("fp1".into()), &settings)
        .unwrap();
    owner
        .bindings
        .bind_on_first_launch(&t2, "codex", "codex", Some("fp2".into()), &settings)
        .unwrap();
    assert!(owner.bindings.get(&t1).is_some());
    assert!(owner.bindings.get(&t2).is_some());
    let reopened = ProviderInstanceBindingStore::open_dir(dir.path()).unwrap();
    assert_eq!(reopened.get(&t1).unwrap().instance_id, "claude");
    assert_eq!(reopened.get(&t2).unwrap().instance_id, "codex");
}

#[test]
fn shadow_home_conflict_fails_closed() {
    let dir = tempdir().unwrap();
    let home = dir.path().join("home");
    let shadow = dir.path().join("shadow");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&shadow).unwrap();
    std::fs::create_dir_all(shadow.join("sessions")).unwrap();
    assert!(prepare_codex_shadow_home(&home, &shadow).is_err());
}

#[test]
fn cursor_about_parsing_contract() {
    let facts = parse_cursor_about_json(
        br#"{"cliVersion":"9.9.9","userEmail":null,"subscriptionTier":null}"#,
    );
    assert_eq!(facts.auth, CursorAboutAuth::Unauthenticated);
    assert_eq!(facts.cli_version.as_deref(), Some("9.9.9"));
}

#[test]
fn discovery_config_keeps_path_separate_from_home_env() {
    let mut inst = ProviderInstanceConfig::builtin_default(BuiltinProviderDriver::Codex);
    inst.home_path = Some("C:/codex-home".into());
    let resolved = resolve_launch_config(&inst, b"s", None).unwrap();
    let discovery: ProviderDiscoveryConfig = resolved.discovery.clone();
    assert_eq!(
        discovery.path.as_ref(),
        resolved.environment.get(&OsString::from("PATH"))
    );
    assert_ne!(
        discovery.path.as_ref(),
        Some(&OsString::from("C:/codex-home"))
    );
    assert!(discovery
        .child_environment
        .contains_key(&OsString::from("CODEX_HOME")));
}

#[cfg(windows)]
#[test]
fn sealed_environment_preserves_home_and_normalizes_override_names() {
    let mut overrides = BTreeMap::new();
    overrides.insert(OsString::from("Path"), OsString::from("fixture-search"));
    let effective = crate::providers::adapter::materialize_provider_environment(overrides);
    assert_eq!(
        effective.get(&OsString::from("PATH")),
        Some(&OsString::from("fixture-search"))
    );
    assert!(!effective.contains_key(&OsString::from("Path")));
    for name in [
        "USERPROFILE",
        "HOME",
        "APPDATA",
        "LOCALAPPDATA",
        "HOMEDRIVE",
        "HOMEPATH",
    ] {
        assert_eq!(
            effective.get(&OsString::from(name)),
            std::env::var_os(name).as_ref()
        );
    }
}

#[test]
fn health_interval_zero_manual_reentry() {
    let cache = ProviderHealthCache::new();
    assert!(!cache.should_schedule_refresh(0));
    let generation = cache.try_begin_refresh().unwrap();
    assert!(cache.try_begin_refresh().is_none());
    cache.finish_refresh(generation, None);
    assert!(cache.try_begin_refresh().is_some());
}

#[test]
fn stale_generation_does_not_publish_error() {
    let cache = ProviderHealthCache::new();
    let generation = cache.try_begin_refresh().unwrap();
    cache.finish_refresh(generation, None);
    cache.set_refresh_error(generation.wrapping_add(99), "stale");
    assert!(cache.last_error().is_none());
}

#[test]
fn scope_fingerprint_rejects_cross_instance_reuse() {
    let mut a = ProviderInstanceConfig::builtin_default(BuiltinProviderDriver::Claude);
    a.home_path = Some("C:/a".into());
    let mut b = a.clone();
    b.instance_id = ProviderInstanceId::new("claude_b").unwrap();
    b.home_path = Some("C:/b".into());
    let fa = ProviderInstanceScope::from_instance(&a).as_cache_key();
    let fb = ProviderInstanceScope::from_instance(&b).as_cache_key();
    assert_ne!(fa, fb);
    assert!(!fa.is_empty() && !fb.is_empty());
}

#[test]
fn legacy_default_builtin_is_unambiguous_when_unchanged() {
    let doc = ProviderSettingsDocument::with_builtins();
    let claude = doc.get("claude").unwrap();
    assert!(claude.home_path.as_ref().is_none_or(|p| p.is_empty()));
    assert!(claude.environment.is_empty());
    assert_eq!(
        default_instance_id_for_kind(ProviderKind::ClaudeCode),
        "claude"
    );
}

#[test]
fn reserved_mixed_case_and_attached_short_flags_rejected() {
    let mut inst = ProviderInstanceConfig::builtin_default(BuiltinProviderDriver::Claude);
    for arg in ["-C", "--allowedTools", "--AllowedTools", "-pfoo", "-C/tmp"] {
        inst.launch_args = vec![arg.into()];
        assert!(
            matches!(
                inst.validate(),
                Err(ProviderSettingsError::ReservedLaunchArg(_))
            ),
            "expected reject for {arg}"
        );
    }
    let mut codex = ProviderInstanceConfig::builtin_default(BuiltinProviderDriver::Codex);
    codex.launch_args = vec!["-cfoo".into()];
    assert!(matches!(
        codex.validate(),
        Err(ProviderSettingsError::ReservedLaunchArg(_))
    ));
}

#[test]
fn reserved_env_prefixes_rejected() {
    let mut inst = ProviderInstanceConfig::builtin_default(BuiltinProviderDriver::Claude);
    for name in [
        "DEVMANAGER_HOOK_NONCE",
        "DEVMANAGER_HOOK_XYZ",
        "DEVMANAGER_RELAY_TOKEN",
        "DEVMANAGER_PROVIDER_SESSION_ID",
        "DEVMANAGER_SESSION_FOO",
        "CLAUDE_CODE_ENTRYPOINT",
    ] {
        inst.environment = vec![ProviderEnvVar {
            name: name.into(),
            value: Some("x".into()),
            sensitive: false,
            protected_value: None,
            value_redacted: false,
        }];
        assert!(
            matches!(
                inst.validate(),
                Err(ProviderSettingsError::ReservedEnvKey(_))
            ),
            "expected reject for {name}"
        );
    }
}

#[test]
fn settings_catalog_order_includes_hidden_and_moves() {
    let mut doc = ProviderSettingsDocument::with_builtins();
    let builtins = builtin_slugs_for_driver(ProviderDriverKind::Claude);
    let first = builtins[0].clone();
    doc.set_builtin_hidden("claude", &first, true).unwrap();
    let picker = doc.ordered_picker_models("claude", &builtins).unwrap();
    assert!(!picker.iter().any(|s| s == &first));
    let catalog = doc.ordered_settings_catalog("claude", &builtins).unwrap();
    assert!(catalog.iter().any(|s| s == &first));
    doc.move_catalog_model("claude", &first, false, &builtins)
        .unwrap();
    assert!(!doc
        .get("claude")
        .unwrap()
        .model_policy
        .catalog_order
        .is_empty());
}

#[test]
fn shadow_only_codex_defaults_shared_home() {
    let mut inst = ProviderInstanceConfig::builtin_default(BuiltinProviderDriver::Codex);
    inst.home_path = None;
    inst.shadow_home_path = Some("C:/shadow-only".into());
    let resolved = resolve_launch_config(&inst, b"scope", None).unwrap();
    assert!(resolved.home_path.is_some());
    let codex_home = resolved
        .environment
        .get(&OsString::from("CODEX_HOME"))
        .expect("CODEX_HOME sealed");
    assert_eq!(codex_home, &OsString::from("C:/shadow-only"));
    assert_eq!(
        resolved
            .discovery
            .child_environment
            .get(&OsString::from("CODEX_HOME")),
        Some(&OsString::from("C:/shadow-only"))
    );
}

#[test]
fn effective_env_includes_allowlist_and_matches_discovery() {
    let mut inst = ProviderInstanceConfig::builtin_default(BuiltinProviderDriver::Claude);
    inst.home_path = Some("C:/claude-home".into());
    let resolved = resolve_launch_config(&inst, b"scope", None).unwrap();
    assert_eq!(resolved.discovery.child_environment, resolved.environment);
    assert!(resolved
        .environment
        .contains_key(&OsString::from("CLAUDE_CONFIG_DIR")));
    // Materialized map includes fixed transport defaults used by probe+launch.
    assert_eq!(
        resolved.environment.get(&OsString::from("TERM")),
        Some(&OsString::from("xterm-256color"))
    );
}

#[test]
fn launch_request_scope_env_key_includes_commitment() {
    use crate::providers::adapter::{provider_scope_env_key, LaunchProviderRequest};
    use crate::providers::capabilities::{commit_child_environment, ProviderExecutable};
    let executable = ProviderExecutable::from_path(std::env::current_exe().unwrap()).unwrap();
    let handle = executable.open_for_launch().unwrap();
    let mut env = std::collections::BTreeMap::new();
    env.insert(
        std::ffi::OsString::from("CLAUDE_CONFIG_DIR"),
        std::ffi::OsString::from("C:/homes/a"),
    );
    let commitment = commit_child_environment(&env);
    let request = LaunchProviderRequest::new(handle, None, None)
        .with_scope_fingerprint(Some("scope-a".into()))
        .with_env_commitment(commitment.clone());
    assert_eq!(
        request.scope_env_key(),
        provider_scope_env_key(Some("scope-a"), &commitment)
    );
    assert_ne!(
        request.scope_env_key(),
        provider_scope_env_key(Some("scope-a"), "")
    );
}

#[test]
fn authority_stale_revision_refuses_mutation() {
    use crate::providers::settings::{
        ProviderSettingsAuthority, ProviderSettingsMutation, ProviderSettingsQuery,
    };
    let dir = tempdir().unwrap();
    let profile = ProviderProfileOwner::open_dir_for_test(dir.path()).unwrap();
    let authority = ProviderSettingsAuthority::from_profile(profile);
    let snap = match authority.query(ProviderSettingsQuery::Snapshot).unwrap() {
        crate::providers::settings::ProviderSettingsReply::Snapshot(s) => s,
        other => panic!("unexpected {other:?}"),
    };
    let mut doc = snap.document.clone();
    doc.health_interval_secs = 0;
    let err = authority.mutate(ProviderSettingsMutation::ReplaceDocument {
        expected_revision: snap.revision.wrapping_add(99),
        document: doc,
    });
    assert!(err.is_err());
}

#[test]
fn two_custom_profile_roots_stay_independent_under_same_ambient_env() {
    let left = tempdir().unwrap();
    let right = tempdir().unwrap();
    let left_owner = ProviderProfileOwner::open_dir_for_test(left.path()).unwrap();
    let right_owner = ProviderProfileOwner::open_dir_for_test(right.path()).unwrap();
    let left_auth = ProviderSettingsAuthority::from_profile(left_owner);
    let right_auth = ProviderSettingsAuthority::from_profile(right_owner);
    let left_rev = left_auth.snapshot().revision;
    left_auth
        .mutate(ProviderSettingsMutation::SetHealthInterval {
            expected_revision: left_rev,
            interval_secs: 0,
        })
        .unwrap();
    assert_eq!(left_auth.snapshot().health_interval_secs, 0);
    assert_eq!(
        right_auth.snapshot().health_interval_secs,
        DEFAULT_HEALTH_INTERVAL_SECS
    );
    assert_ne!(left_auth.profile().root(), right_auth.profile().root());
}

#[test]
fn authority_snapshot_mutation_roundtrip_persists_before_publish() {
    use crate::providers::settings::{
        ProviderSettingsAuthority, ProviderSettingsMutation, ProviderSettingsQuery,
        ProviderSettingsReply,
    };
    let dir = tempdir().unwrap();
    let profile = ProviderProfileOwner::open_dir_for_test(dir.path()).unwrap();
    let authority = ProviderSettingsAuthority::from_profile(profile);
    let snap = match authority.query(ProviderSettingsQuery::Snapshot).unwrap() {
        ProviderSettingsReply::Snapshot(s) => s,
        other => panic!("unexpected {other:?}"),
    };
    let reply = authority
        .mutate(ProviderSettingsMutation::SetHealthInterval {
            expected_revision: snap.revision,
            interval_secs: 60,
        })
        .unwrap();
    match reply {
        ProviderSettingsReply::MutationApplied { snapshot } => {
            assert_eq!(snapshot.health_interval_secs, 60);
            assert_eq!(snapshot.revision, snap.revision + 1);
        }
        other => panic!("unexpected {other:?}"),
    }
    let reopened = ProviderProfileOwner::open_dir_for_test(dir.path()).unwrap();
    assert_eq!(reopened.settings.snapshot().health_interval_secs, 60);
}

#[test]
fn binding_failed_persist_leaves_no_phantom() {
    let dir = tempdir().unwrap();
    let owner = ProviderProfileOwner::open_dir_for_test(dir.path()).unwrap();
    // First launch deferred binding must not be readable until commit.
    assert!(owner.bindings.get(&TaskId::new()).is_none());
}
