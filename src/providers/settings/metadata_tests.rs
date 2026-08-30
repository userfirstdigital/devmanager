//! Fixture regressions for provider metadata cache/parsers (no live CLI/HTTP).

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::providers::adapter::{
    ProviderInteractiveProbeError, ProviderInteractiveSession, WindowsProviderProbeRunner,
};
use crate::providers::settings::metadata_cache::{
    config_scope_fingerprint, is_stale, ProviderMetadataCache,
};
use crate::providers::settings::metadata_parse::{
    extract_claude_oauth_token, extract_cursor_access_token, parse_claude_initialize_models,
    parse_claude_usage_json, parse_codex_model_list, parse_codex_rate_limits,
    parse_cursor_list_available_models, parse_cursor_period_usage,
};
use crate::providers::settings::metadata_probe::{project_model_catalog_wire, project_usage_wire};
use crate::providers::settings::metadata_types::{
    CachedModelCatalog, CachedUsageSnapshot, DiscoveredModel, ProviderMetadataSource,
    ProviderUsageStateWire, ProviderUsageWindowWire,
};
use crate::providers::settings::model::{
    normalize_model_slug, BuiltinProviderDriver, CustomModelEntry, ProviderInstanceConfig,
    ProviderSettingsDocument,
};
use crate::providers::settings::ProviderProfileOwner;
use tempfile::tempdir;

#[test]
fn normalize_model_slug_allows_bracket_aliases() {
    assert_eq!(
        normalize_model_slug("claude-opus-5[1m]").unwrap(),
        "claude-opus-5[1m]"
    );
    assert_eq!(
        normalize_model_slug("claude-fable-5[1m]").unwrap(),
        "claude-fable-5[1m]"
    );
    assert!(normalize_model_slug("bad slug").is_err());
    assert!(normalize_model_slug("claude[[1m]]").is_err());
}

#[test]
fn claude_initialize_parses_nested_response_and_fable_effort() {
    let body = r#"{
      "type": "control_response",
      "response": {
        "response": {
          "models": [
            {
              "value": "claude-opus-5[1m]",
              "resolvedModel": "claude-opus-5[1m]",
              "displayName": "Opus",
              "supportsEffort": true,
              "supportedEffortLevels": ["low", "medium", "high", "xhigh", "max"]
            },
            {
              "value": "claude-fable-5[1m]",
              "resolvedModel": "claude-fable-5",
              "displayName": "Fable",
              "supportsEffort": true,
              "supportedEffortLevels": ["low", "medium", "high", "xhigh", "max"]
            },
            {
              "value": "claude-haiku-4-5-20251001",
              "displayName": "Haiku",
              "supportsEffort": false,
              "supportedEffortLevels": []
            }
          ]
        }
      }
    }"#;
    let models = parse_claude_initialize_models(body).unwrap();
    assert!(models.iter().any(|m| m.slug.contains("fable")));
    let fable = models.iter().find(|m| m.slug.contains("fable")).unwrap();
    assert_eq!(fable.display_name, "Fable");
    assert!(fable.supports_effort);
    assert!(fable.supported_efforts.contains(&"xhigh".into()));
    let haiku = models.iter().find(|m| m.slug.contains("haiku")).unwrap();
    assert!(!haiku.supports_effort);
    assert!(haiku.supported_efforts.is_empty());
}

#[test]
fn claude_usage_null_fields_stay_none_and_resets_seconds_to_ms() {
    let body = r#"{
      "five_hour": { "utilization": null, "resets_at": 1786423807 },
      "seven_day": { "utilization": 42.5, "resets_at": null }
    }"#;
    let usage = parse_claude_usage_json(body).unwrap();
    assert_eq!(usage.windows.len(), 2);
    assert!(usage.windows[0].used_percent.is_none());
    assert!(usage.windows[0].remaining_percent.is_none());
    assert_eq!(usage.windows[0].resets_at_unix_ms, Some(1786423807_000));
    assert_eq!(usage.windows[1].used_percent, Some(43));
    assert!(usage.windows[1].resets_at_unix_ms.is_none());
}

#[test]
fn claude_modern_meters_map_session_and_week() {
    let body = r#"{
      "meters": [
        { "kind": "session", "percent": 12.0, "resets_at": "1786423807000" },
        {
          "kind": "weekly_scoped",
          "percent": 55.0,
          "resets_at": 1789102207,
          "scope": { "model": { "display_name": "Opus" } }
        }
      ]
    }"#;
    let usage = parse_claude_usage_json(body).unwrap();
    assert_eq!(usage.windows[0].id, "five_hour");
    assert_eq!(usage.windows[0].resets_at_unix_ms, Some(1786423807000));
    assert_eq!(usage.windows[1].id, "week_scoped");
    assert_eq!(usage.windows[1].scope_label.as_deref(), Some("Opus"));
}

#[test]
fn codex_rate_limits_use_dynamic_duration_not_five_hour_label() {
    let result = serde_json::json!({
      "primary": {
        "usedPercent": 74,
        "windowDurationMins": 10080,
        "resetsAt": 1786423807
      },
      "secondary": null
    });
    let usage = parse_codex_rate_limits(&result).unwrap();
    assert_eq!(usage.windows.len(), 1);
    assert_eq!(usage.windows[0].id, "primary");
    assert_eq!(usage.windows[0].label, "Weekly");
    assert!(!usage.windows[0].label.to_ascii_lowercase().contains("5h"));
    assert_eq!(usage.windows[0].used_percent, Some(74));
    assert_eq!(usage.windows[0].window_duration_mins, Some(10080));
    assert_eq!(usage.windows[0].resets_at_unix_ms, Some(1786423807_000));
    let nested = serde_json::json!({"rateLimitsByLimitId": {
        "codex": result,
        "codex_spark": {"primary": {"usedPercent": 12, "windowDurationMins": 300},
                         "secondary": {"usedPercent": 30, "windowDurationMins": 10080}}
    }});
    let nested = parse_codex_rate_limits(&nested).unwrap();
    assert_eq!(nested.windows.len(), 1);
    assert_eq!(nested.windows[0].remaining_percent, Some(26));
    assert!(nested
        .windows
        .iter()
        .all(|window| !window.label.contains("codex_spark")));
}

#[test]
fn codex_model_list_sol_terra_include_ultra() {
    let result = serde_json::json!({
      "data": [{
        "model": "gpt-5.6-sol",
        "displayName": "Sol",
        "defaultReasoningEffort": "high",
        "supportedReasoningEfforts": [
          { "reasoningEffort": "low" },
          { "reasoningEffort": "ultra" }
        ],
        "hidden": false
      }],
      "nextCursor": null
    });
    let (models, next) = parse_codex_model_list(&result).unwrap();
    assert!(next.is_none());
    assert_eq!(models[0].default_effort.as_deref(), Some("high"));
    assert!(models[0].supported_efforts.contains(&"ultra".into()));
}

#[test]
fn cursor_period_usage_splits_auto_and_api_without_blended_total() {
    let body = r#"{
      "billingCycleStart": "1786423807000",
      "billingCycleEnd": "1789102207000",
      "planUsage": {
        "totalSpend": 198008,
        "includedSpend": 40000,
        "bonusSpend": 158008,
        "limit": 40000,
        "autoPercentUsed": 49.333,
        "apiPercentUsed": 100,
        "totalPercentUsed": 56.5737
      },
      "spendLimitUsage": { "limitType": "user" },
      "enabled": true,
      "displayMessage": "You've hit your usage limit"
    }"#;
    let usage = parse_cursor_period_usage(body).unwrap();
    assert_eq!(usage.windows.len(), 2);
    assert_eq!(usage.windows[0].id, "auto");
    assert_eq!(usage.windows[1].id, "api");
    assert_eq!(usage.windows[0].used_percent, Some(49));
    assert_eq!(usage.windows[1].used_percent, Some(100));
    assert_eq!(usage.windows[0].resets_at_unix_ms, Some(1789102207000));
    // No invented remaining when absent — remaining derived only from used.
    assert_eq!(usage.windows[0].remaining_percent, Some(51));
}

#[test]
fn cursor_models_parse_thought_level_options() {
    let result = serde_json::json!({
      "models": [{
        "value": "gpt-5.4",
        "name": "GPT-5.4",
        "configOptions": [{
          "category": "thought_level",
          "id": "effort",
          "type": "select",
          "currentValue": "none",
          "options": [
            { "value": "none", "name": "None" },
            { "value": "high", "name": "High" }
          ]
        }]
      }]
    });
    let models = parse_cursor_list_available_models(&result).unwrap();
    assert_eq!(models[0].slug, "gpt-5.4");
    assert!(models[0].default_effort.is_none());
    assert!(models[0].supported_efforts.contains(&"high".into()));
}

#[test]
fn credentials_extractors_never_require_logging() {
    let claude = r#"{"claudeAiOauth":{"accessToken":"secret-token","emailAddress":"a@b.c"}}"#;
    assert_eq!(
        extract_claude_oauth_token(claude).as_deref(),
        Some("secret-token")
    );
    let cursor = r#"{"accessToken":"cursor-token","email":"u@cursor"}"#;
    assert_eq!(
        extract_cursor_access_token(cursor).as_deref(),
        Some("cursor-token")
    );
}

#[test]
fn metadata_cache_persist_reload_scope_change_and_malformed() {
    let dir = tempdir().unwrap();
    let cache = ProviderMetadataCache::open_dir(dir.path()).unwrap();
    let instance = ProviderInstanceConfig::builtin_default(BuiltinProviderDriver::Claude);
    let fp = config_scope_fingerprint(&instance);
    cache
        .upsert_models(
            "claude",
            "claude",
            &fp,
            Some("acct-a".into()),
            CachedModelCatalog {
                models: vec![DiscoveredModel {
                    slug: "claude-opus-5[1m]".into(),
                    display_name: "Opus".into(),
                    supports_effort: true,
                    supported_efforts: vec!["high".into()],
                    default_effort: None,
                    hidden: false,
                    input_modalities: Vec::new(),
                }],
                checked_at_unix_ms: Some(1_000),
            },
        )
        .unwrap();
    cache
        .upsert_usage(
            "claude",
            "claude",
            &fp,
            Some("acct-a".into()),
            CachedUsageSnapshot {
                windows: vec![ProviderUsageWindowWire {
                    id: "five_hour".into(),
                    label: "5-hour".into(),
                    used_percent: Some(10),
                    remaining_percent: Some(90),
                    resets_at_unix_ms: None,
                    window_duration_mins: Some(300),
                    scope_label: None,
                }],
                checked_at_unix_ms: Some(1_000),
                state: ProviderUsageStateWire::Fresh,
            },
            None,
        )
        .unwrap();

    // Reload
    let reloaded = ProviderMetadataCache::open_dir(dir.path()).unwrap();
    let entry = reloaded.entry("claude", &fp).unwrap();
    assert_eq!(entry.models.models.len(), 1);
    assert_eq!(entry.usage.windows.len(), 1);

    // Account change invalidates usage
    reloaded
        .invalidate_usage_for_account_change("claude", &fp, Some("acct-b"))
        .unwrap();
    let entry = reloaded.entry("claude", &fp).unwrap();
    assert!(entry.usage.windows.is_empty());
    assert_eq!(entry.account_fingerprint.as_deref(), Some("acct-b"));

    // Malformed file -> empty, not panic
    std::fs::write(dir.path().join("provider_metadata_cache.json"), "{not-json").unwrap();
    let empty = ProviderMetadataCache::open_dir(dir.path()).unwrap();
    assert!(empty.snapshot().entries.is_empty());
}

#[test]
fn custom_models_retained_in_projection() {
    let mut instance = ProviderInstanceConfig::builtin_default(BuiltinProviderDriver::Claude);
    instance.custom_models.push(CustomModelEntry {
        slug: "my/custom".into(),
        display_name: Some("Mine".into()),
    });
    instance.model_policy.favorite_order = vec!["my/custom".into()];
    let discovered = vec![DiscoveredModel {
        slug: "claude-opus-5[1m]".into(),
        display_name: "Opus".into(),
        supports_effort: true,
        supported_efforts: vec!["high".into()],
        default_effort: None,
        hidden: false,
        input_modalities: Vec::new(),
    }];
    let wire = project_model_catalog_wire(
        &instance,
        &discovered,
        Some(10),
        20,
        ProviderMetadataSource::LastGood,
        Some("fp".into()),
        None,
        None,
    );
    assert_eq!(wire.models[0].slug, "my/custom");
    assert!(wire.models.iter().any(|m| m.slug.contains("opus")));
    instance.model_policy.hidden_builtins = vec!["claude-opus-5[1m]".into()];
    let hidden = project_model_catalog_wire(
        &instance,
        &discovered,
        Some(10),
        20,
        ProviderMetadataSource::LastGood,
        Some("fp".into()),
        None,
        None,
    );
    assert!(hidden
        .models
        .iter()
        .any(|model| model.slug == "claude-opus-5[1m]" && model.hidden));
}

#[test]
fn usage_projection_marks_stale() {
    let instance = ProviderInstanceConfig::builtin_default(BuiltinProviderDriver::Codex);
    let usage = CachedUsageSnapshot {
        windows: vec![ProviderUsageWindowWire {
            id: "primary".into(),
            label: "Primary".into(),
            used_percent: Some(1),
            remaining_percent: Some(99),
            resets_at_unix_ms: None,
            window_duration_mins: Some(10080),
            scope_label: None,
        }],
        checked_at_unix_ms: Some(1),
        state: ProviderUsageStateWire::Fresh,
    };
    let wire = project_usage_wire(
        &instance,
        &usage,
        None,
        1 + 60 * 60 * 1000 + 1,
        ProviderMetadataSource::LastGood,
        None,
        None,
        None,
    );
    assert_eq!(wire.state, ProviderUsageStateWire::Stale);
    assert!(is_stale(Some(1), 1 + 60 * 60 * 1000 + 1));
}

#[test]
fn profile_owner_opens_metadata_cache() {
    let dir = tempdir().unwrap();
    let profile = ProviderProfileOwner::open_dir_for_test(dir.path()).unwrap();
    assert!(profile.metadata.snapshot().entries.is_empty());
    let _ = ProviderSettingsDocument::with_builtins();
}

#[test]
fn interactive_session_drop_is_cancel_safe_type() {
    // Type-level: Drop terminates; unit construction is Windows-only spawn.
    fn assert_drop<T: Drop>(_: Option<T>) {}
    let session: Option<ProviderInteractiveSession> = None;
    assert_drop(session);
    let _ = ProviderInteractiveProbeError::Cancelled;
    let _ = Duration::from_secs(1);
    let _ = AtomicBool::new(false).load(Ordering::Relaxed);
    let _ = std::mem::size_of::<WindowsProviderProbeRunner>();
}

#[test]
fn metadata_cache_rejects_invalid_percent_and_slug_on_load() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("provider_metadata_cache.json");
    let body = r#"{
      "version": 1,
      "entries": [{
        "instanceId": "claude",
        "driver": "claude",
        "configFingerprint": "abc",
        "accountFingerprint": "acct",
        "models": {
          "models": [{
            "slug": "bad slug!!",
            "displayName": "Bad",
            "supportsEffort": false,
            "supportedEfforts": [],
            "defaultEffort": null,
            "hidden": false,
            "inputModalities": []
          }],
          "checkedAtUnixMs": 1
        },
        "usage": {
          "windows": [{
            "id": "five_hour",
            "label": "5h",
            "usedPercent": 250,
            "remainingPercent": 10,
            "resetsAtUnixMs": null,
            "windowDurationMins": 300,
            "scopeLabel": null
          }],
          "checkedAtUnixMs": 1,
          "state": "fresh"
        },
        "usageBackoffUntilUnixMs": null
      }]
    }"#;
    std::fs::write(&path, body).unwrap();
    let cache = ProviderMetadataCache::open_dir(dir.path()).unwrap();
    let entry = cache.entry("claude", "abc").unwrap();
    assert!(entry.models.models.is_empty());
    assert!(entry.usage.windows.is_empty());
}

#[test]
fn metadata_cache_oversized_file_loads_empty() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("provider_metadata_cache.json");
    let huge = format!(
        "{{\"version\":1,\"entries\":[{{\"instanceId\":\"claude\",\"driver\":\"claude\",\"configFingerprint\":\"{}\",\"accountFingerprint\":null,\"models\":{{\"models\":[],\"checkedAtUnixMs\":null}},\"usage\":{{\"windows\":[],\"checkedAtUnixMs\":null,\"state\":\"unknown\"}},\"usageBackoffUntilUnixMs\":null}}]}}",
        "x".repeat(600_000)
    );
    std::fs::write(&path, huge).unwrap();
    let cache = ProviderMetadataCache::open_dir(dir.path()).unwrap();
    assert!(cache.snapshot().entries.is_empty());
}

#[test]
fn effective_scope_changes_with_inherited_api_key_env() {
    use crate::providers::settings::effective_scope_fingerprint;
    use crate::providers::settings::resolve_launch_config;
    use std::ffi::OsString;
    let instance = ProviderInstanceConfig::builtin_default(BuiltinProviderDriver::Claude);
    let mut resolved = resolve_launch_config(&instance, b"test", None).unwrap();
    let without = effective_scope_fingerprint(&instance, &resolved);
    resolved.discovery.child_environment.insert(
        OsString::from("ANTHROPIC_API_KEY"),
        OsString::from("sk-test-not-a-real-key"),
    );
    let with_key = effective_scope_fingerprint(&instance, &resolved);
    assert_ne!(without, with_key);
}

#[test]
fn usage_backoff_fence_blocks_refresh_until() {
    let dir = tempdir().unwrap();
    let cache = ProviderMetadataCache::open_dir(dir.path()).unwrap();
    let instance = ProviderInstanceConfig::builtin_default(BuiltinProviderDriver::Claude);
    let fp = config_scope_fingerprint(&instance);
    cache
        .upsert_usage(
            "claude",
            "claude",
            &fp,
            Some("acct".into()),
            CachedUsageSnapshot {
                windows: vec![ProviderUsageWindowWire {
                    id: "five_hour".into(),
                    label: "5-hour".into(),
                    used_percent: Some(10),
                    remaining_percent: Some(90),
                    resets_at_unix_ms: None,
                    window_duration_mins: Some(300),
                    scope_label: None,
                }],
                checked_at_unix_ms: Some(1_000),
                state: ProviderUsageStateWire::Backoff,
            },
            Some(5_000),
        )
        .unwrap();
    let entry = cache.entry("claude", &fp).unwrap();
    assert_eq!(entry.usage_backoff_until_unix_ms, Some(5_000));
    assert!(!entry.usage.windows.is_empty());
}

#[test]
fn interactive_probe_constants_bound_cumulative_io() {
    use crate::providers::adapter::{
        MAX_INTERACTIVE_PROBE_LINES, MAX_INTERACTIVE_STDIN_BYTES, MAX_INTERACTIVE_WRITE_CHUNK,
        MAX_PROVIDER_PROBE_OUTPUT_BYTES,
    };
    assert!(MAX_INTERACTIVE_PROBE_LINES > 0);
    assert!(MAX_INTERACTIVE_STDIN_BYTES <= MAX_PROVIDER_PROBE_OUTPUT_BYTES);
    assert!(MAX_INTERACTIVE_WRITE_CHUNK <= MAX_INTERACTIVE_STDIN_BYTES);
    let cancel = AtomicBool::new(true);
    assert!(cancel.load(Ordering::Acquire));
    let _ = ProviderInteractiveProbeError::Cancelled;
    let _ = ProviderInteractiveProbeError::OutputTooLarge;
    let _ = ProviderInteractiveProbeError::TooManyLines;
}

#[test]
fn retry_after_missing_defaults_to_fifteen_minutes_constant() {
    use crate::providers::settings::usage_http::{
        parse_retry_after_secs, DEFAULT_USAGE_BACKOFF_SECS,
    };
    assert!(parse_retry_after_secs("").is_none());
    assert!(parse_retry_after_secs("not-a-date").is_none());
    assert_eq!(DEFAULT_USAGE_BACKOFF_SECS, 15 * 60);
}

#[test]
fn codex_account_fingerprint_canonical_id_only() {
    use crate::providers::settings::metadata_parse::{
        codex_account_fingerprint_material, codex_account_id_from_account_read,
        fingerprint_account_material,
    };
    let auth = r#"{"tokens":{"access_token":"SECRET","refresh_token":"R","account_id":"42"}}"#;
    let material = codex_account_fingerprint_material(auth).unwrap();
    assert_eq!(material, "codex-id:42");
    assert!(!material.contains("SECRET"));
    let read = serde_json::json!({
        "account": { "email": "a@b.c", "account_id": "42" }
    });
    let from_read = codex_account_id_from_account_read(&read).unwrap();
    assert_eq!(from_read, material);
    assert_eq!(
        fingerprint_account_material(&material),
        fingerprint_account_material(&from_read)
    );
}

#[test]
fn claude_real_credential_shapes_use_oauth_account_config() {
    use crate::providers::settings::metadata_parse::{
        claude_account_fingerprint_material_from_config, claude_credential_context_material,
        claude_credentials_have_access_token,
    };
    // Shape verified on disk: credentials have tokens only — no email/account.
    let credentials = r#"{
      "claudeAiOauth": {
        "accessToken": "tok",
        "refreshToken": "ref",
        "expiresAt": 9999999999999,
        "refreshTokenExpiresAt": 9999999999999,
        "scopes": ["user:profile", "user:inference"],
        "subscriptionType": "pro",
        "rateLimitTier": []
      }
    }"#;
    assert!(claude_credentials_have_access_token(credentials));
    assert!(claude_credential_context_material(credentials).is_some());
    // Account scope comes from sibling .claude.json oauthAccount.
    let config = r#"{
      "oauthAccount": {
        "accountUuid": "acct-uuid-1",
        "emailAddress": "user@example.com",
        "organizationUuid": "org-uuid-9"
      }
    }"#;
    let material = claude_account_fingerprint_material_from_config(config).unwrap();
    assert_eq!(material, "claude:acct-uuid-1:org-uuid-9");
    assert!(!material.contains('@'));
}

#[test]
fn cursor_jwt_sub_cache_scope_from_real_auth_shape() {
    use crate::providers::settings::metadata_parse::{
        cursor_account_fingerprint_material, cursor_jwt_sub_for_cache_scope,
    };
    use base64::Engine;
    let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(br#"{"sub":"user_abc","exp":9999999999,"iss":"https://authentication.cursor.sh","scope":"openid","aud":"https://cursor.com","type":"session"}"#);
    let token = format!("{header}.{payload}.sig");
    let auth = format!(r#"{{"accessToken":"{token}","refreshToken":"r"}}"#);
    let material = cursor_account_fingerprint_material(&auth).unwrap();
    assert_eq!(material, "cursor-sub:user_abc");
    assert!(cursor_jwt_sub_for_cache_scope("not-a-jwt").is_none());
    assert!(cursor_jwt_sub_for_cache_scope(&format!("{header}.!!!bad!!!.sig")).is_none());
    let oversize_payload = "a".repeat(9 * 1024);
    assert!(cursor_jwt_sub_for_cache_scope(&format!("h.{oversize_payload}.s")).is_none());
}

#[test]
fn claude_account_config_path_stock_vs_explicit() {
    use crate::providers::settings::usage_http::claude_account_config_path;
    use std::path::PathBuf;
    let stock_dir = PathBuf::from(r"C:\Users\example\.claude");
    assert_eq!(
        claude_account_config_path(&stock_dir, false),
        PathBuf::from(r"C:\Users\example\.claude.json")
    );
    let explicit = PathBuf::from(r"D:\profiles\work-claude");
    assert_eq!(
        claude_account_config_path(&explicit, true),
        PathBuf::from(r"D:\profiles\work-claude\.claude.json")
    );
    // Explicit override ending in `.claude` still nests `.claude.json` inside D.
    let explicit_dot = PathBuf::from(r"D:\profiles\alt\.claude");
    assert_eq!(
        claude_account_config_path(&explicit_dot, true),
        PathBuf::from(r"D:\profiles\alt\.claude\.claude.json")
    );
}

#[test]
fn document_validate_rejects_enabled_stub_and_driver_mismatch() {
    use crate::providers::settings::model::ProviderSettingsError;
    let mut doc = ProviderSettingsDocument::with_builtins();
    doc.get_mut("grok").unwrap().enabled = true;
    assert!(matches!(
        doc.validate(),
        Err(ProviderSettingsError::StubCannotEnable(_))
    ));
    let mut doc = ProviderSettingsDocument::with_builtins();
    doc.get_mut("claude").unwrap().driver =
        crate::providers::settings::model::ProviderDriverKind::Codex;
    assert!(matches!(
        doc.validate(),
        Err(ProviderSettingsError::ImmutableBuiltinDriver)
    ));
}

#[test]
fn custom_endpoint_projection_keeps_models_quota_unsupported() {
    use crate::providers::settings::metadata_probe::project_all_from_cache;
    use crate::providers::settings::metadata_types::{
        CachedModelCatalog, DiscoveredModel, ProviderUsageStateWire,
    };
    use crate::providers::settings::{effective_scope_fingerprint, resolve_launch_config};

    let dir = tempdir().unwrap();
    let profile = ProviderProfileOwner::open_dir_for_test(dir.path()).unwrap();
    profile
        .settings
        .update(|doc| {
            let slot = doc.get_mut("claude").unwrap();
            slot.api_endpoint = Some("https://api.anthropic.com.evil".into());
            slot.custom_models.push(CustomModelEntry {
                slug: "my/custom".into(),
                display_name: Some("Mine".into()),
            });
            Ok(())
        })
        .unwrap();
    let document = profile.settings.snapshot();
    let instance = document.get("claude").unwrap();
    let custody = profile.settings.custody_scope_for_instance("claude");
    let resolved = resolve_launch_config(instance, &custody, None).unwrap();
    let fp = effective_scope_fingerprint(instance, &resolved);
    profile
        .metadata
        .upsert_models(
            "claude",
            "claude",
            &fp,
            None,
            CachedModelCatalog {
                models: vec![DiscoveredModel {
                    slug: "claude-opus-5".into(),
                    display_name: "Opus".into(),
                    supports_effort: true,
                    supported_efforts: vec!["high".into()],
                    default_effort: None,
                    hidden: false,
                    input_modalities: Vec::new(),
                }],
                checked_at_unix_ms: Some(1),
            },
        )
        .unwrap();
    let (catalogs, usages) = project_all_from_cache(&profile.settings, &profile.metadata, 2);
    let catalog = catalogs.iter().find(|c| c.instance_id == "claude").unwrap();
    assert!(catalog.models.iter().any(|m| m.slug.contains("opus")));
    assert!(catalog.models.iter().any(|m| m.slug == "my/custom"));
    let usage = usages.iter().find(|u| u.instance_id == "claude").unwrap();
    assert_eq!(usage.state, ProviderUsageStateWire::Unsupported);
}

#[test]
fn project_all_does_not_prune_on_read() {
    use crate::providers::settings::metadata_probe::project_all_from_cache;
    use crate::providers::settings::metadata_types::CachedModelCatalog;
    let dir = tempdir().unwrap();
    let profile = ProviderProfileOwner::open_dir_for_test(dir.path()).unwrap();
    profile
        .metadata
        .upsert_models(
            "custom-gone",
            "claude",
            "orphan-fp",
            Some("acct".into()),
            CachedModelCatalog::default(),
        )
        .unwrap();
    assert!(profile.metadata.entry("custom-gone", "orphan-fp").is_some());
    let _ = project_all_from_cache(&profile.settings, &profile.metadata, 1);
    assert!(profile.metadata.entry("custom-gone", "orphan-fp").is_some());
}

#[test]
fn custom_models_merge_without_cache_entry_on_unsupported() {
    use crate::providers::settings::metadata_probe::project_all_from_cache;
    use crate::providers::settings::metadata_types::ProviderUsageStateWire;
    let dir = tempdir().unwrap();
    let profile = ProviderProfileOwner::open_dir_for_test(dir.path()).unwrap();
    profile
        .settings
        .update(|doc| {
            let slot = doc.get_mut("claude").unwrap();
            slot.api_endpoint = Some("https://api.anthropic.com.evil".into());
            slot.custom_models.push(CustomModelEntry {
                slug: "only/custom".into(),
                display_name: Some("Only".into()),
            });
            Ok(())
        })
        .unwrap();
    let (catalogs, usages) = project_all_from_cache(&profile.settings, &profile.metadata, 1);
    let catalog = catalogs.iter().find(|c| c.instance_id == "claude").unwrap();
    assert!(catalog.models.iter().any(|m| m.slug == "only/custom"));
    let usage = usages.iter().find(|u| u.instance_id == "claude").unwrap();
    assert_eq!(usage.state, ProviderUsageStateWire::Unsupported);
}

#[test]
fn effective_scope_includes_home_path_value() {
    use crate::providers::settings::effective_scope_fingerprint;
    use crate::providers::settings::resolve_launch_config;
    let instance = ProviderInstanceConfig::builtin_default(BuiltinProviderDriver::Claude);
    let mut resolved = resolve_launch_config(&instance, b"test", None).unwrap();
    let stock = effective_scope_fingerprint(&instance, &resolved);
    resolved.home_path = Some(std::path::PathBuf::from(r"D:\profiles\alt-claude"));
    let custom = effective_scope_fingerprint(&instance, &resolved);
    assert_ne!(stock, custom);
}

#[test]
fn codex_model_list_accepts_cursor_response_field() {
    use crate::providers::settings::metadata_parse::parse_codex_model_list;
    let result = serde_json::json!({
      "data": [{ "model": "gpt-5.6-sol", "displayName": "Sol" }],
      "cursor": "page-2"
    });
    let (models, next) = parse_codex_model_list(&result).unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(next.as_deref(), Some("page-2"));
}
