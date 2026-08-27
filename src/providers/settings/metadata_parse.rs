//! Parsers for live provider metadata/usage wire bodies (fixture-tested).
//!
//! These never invent absent percents, never scrape chat tokens, and never
//! retain credential material.

use serde_json::Value as JsonValue;

use super::metadata_types::{
    CachedUsageSnapshot, DiscoveredModel, ProviderUsageStateWire, ProviderUsageWindowWire,
    MAX_METADATA_EFFORTS, MAX_METADATA_MODELS, MAX_USAGE_WINDOWS,
};
use super::model::normalize_model_slug;

const CLAUDE_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];
const CODEX_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max", "ultra"];

fn clamp_percent(value: Option<f64>) -> Option<u8> {
    let value = value?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    Some(value.min(100.0).round() as u8)
}

fn remaining_from_used(used: Option<u8>) -> Option<u8> {
    used.map(|used| 100u8.saturating_sub(used))
}

fn resets_at_to_ms(value: &JsonValue) -> Option<u64> {
    match value {
        JsonValue::Null => None,
        JsonValue::Number(n) => {
            let raw = n.as_u64().or_else(|| n.as_f64().map(|f| f as u64))?;
            // Heuristic: values that look like seconds (< year 2100 in ms scale).
            if raw < 10_000_000_000 {
                raw.checked_mul(1000)
            } else {
                Some(raw)
            }
        }
        JsonValue::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return None;
            }
            let raw: u64 = trimmed.parse().ok()?;
            if raw < 10_000_000_000 {
                raw.checked_mul(1000)
            } else {
                Some(raw)
            }
        }
        _ => None,
    }
}

fn normalize_effort_label(raw: &str) -> Option<String> {
    let lower = raw.trim().to_ascii_lowercase();
    match lower.as_str() {
        "" | "none" | "default" | "auto" => None,
        "extra_high" | "extrahigh" | "x-high" => Some("xhigh".into()),
        other => Some(other.to_string()),
    }
}

fn push_unique_effort(out: &mut Vec<String>, raw: &str) {
    if out.len() >= MAX_METADATA_EFFORTS {
        return;
    }
    let Some(label) = normalize_effort_label(raw) else {
        return;
    };
    if !out.iter().any(|existing| existing == &label) {
        out.push(label);
    }
}

/// Claude initialize control response: `msg.response.response.models`.
pub fn parse_claude_initialize_models(body: &str) -> Result<Vec<DiscoveredModel>, String> {
    let root: JsonValue =
        serde_json::from_str(body).map_err(|e| format!("claude metadata json: {e}"))?;
    let models = root
        .pointer("/response/response/models")
        .or_else(|| root.pointer("/response/models"))
        .and_then(|v| v.as_array())
        .ok_or_else(|| "claude metadata missing response.response.models".to_string())?;
    let mut out = Vec::new();
    for entry in models {
        if out.len() >= MAX_METADATA_MODELS {
            break;
        }
        let slug_raw = entry
            .get("value")
            .or_else(|| entry.get("resolvedModel"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if slug_raw.is_empty() {
            continue;
        }
        let Ok(slug) = normalize_model_slug(slug_raw) else {
            continue;
        };
        let display_name = entry
            .get("displayName")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(friendly_claude_display)
            .unwrap_or_else(|| friendly_claude_display(&slug));
        let supports_effort = entry
            .get("supportsEffort")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let mut supported_efforts = Vec::new();
        if let Some(levels) = entry
            .get("supportedEffortLevels")
            .and_then(|v| v.as_array())
        {
            for level in levels {
                if let Some(s) = level.as_str() {
                    push_unique_effort(&mut supported_efforts, s);
                }
            }
        }
        if supports_effort && supported_efforts.is_empty() {
            for level in CLAUDE_EFFORTS {
                push_unique_effort(&mut supported_efforts, level);
            }
        }
        if !supports_effort {
            supported_efforts.clear();
        }
        out.push(DiscoveredModel {
            slug,
            display_name,
            supports_effort: supports_effort && !supported_efforts.is_empty(),
            supported_efforts,
            default_effort: None,
            hidden: false,
            input_modalities: Vec::new(),
        });
    }
    Ok(apply_claude_alias_defaults(out))
}

fn friendly_claude_display(slug: &str) -> String {
    let lower = slug.to_ascii_lowercase();
    if lower.contains("fable") {
        return "Fable".into();
    }
    if lower.contains("opus") {
        return "Opus".into();
    }
    if lower.contains("sonnet") {
        return "Sonnet".into();
    }
    if lower.contains("haiku") {
        return "Haiku".into();
    }
    slug.to_string()
}

fn apply_claude_alias_defaults(mut models: Vec<DiscoveredModel>) -> Vec<DiscoveredModel> {
    // Keep Fable only when reported. Ensure Haiku has no effort.
    models.retain(|m| {
        let lower = m.slug.to_ascii_lowercase();
        if lower.contains("fable") {
            return true;
        }
        true
    });
    for model in &mut models {
        let lower = model.slug.to_ascii_lowercase();
        if lower.contains("haiku") {
            model.supports_effort = false;
            model.supported_efforts.clear();
            model.default_effort = None;
        }
    }
    models
}

/// Codex `model/list` JSON-RPC result payload (`result` object or array wrapper).
pub fn parse_codex_model_list(
    result: &JsonValue,
) -> Result<(Vec<DiscoveredModel>, Option<String>), String> {
    let data = result
        .get("data")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "codex model/list missing data".to_string())?;
    let next = result
        .get("nextCursor")
        .or_else(|| result.get("cursor"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let mut out = Vec::new();
    for entry in data {
        if out.len() >= MAX_METADATA_MODELS {
            break;
        }
        let slug_raw = entry
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if slug_raw.is_empty() {
            continue;
        }
        let Ok(slug) = normalize_model_slug(slug_raw) else {
            continue;
        };
        let display_name = entry
            .get("displayName")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(slug_raw)
            .to_string();
        let mut supported_efforts = Vec::new();
        if let Some(levels) = entry
            .get("supportedReasoningEfforts")
            .and_then(|v| v.as_array())
        {
            for level in levels {
                let effort = level
                    .get("reasoningEffort")
                    .and_then(|v| v.as_str())
                    .or_else(|| level.as_str());
                if let Some(effort) = effort {
                    push_unique_effort(&mut supported_efforts, effort);
                }
            }
        }
        if supported_efforts.is_empty() {
            let lower = slug.to_ascii_lowercase();
            if lower.contains("sol") || lower.contains("terra") {
                for level in CODEX_EFFORTS {
                    push_unique_effort(&mut supported_efforts, level);
                }
            }
        }
        let default_effort = entry
            .get("defaultReasoningEffort")
            .and_then(|v| v.as_str())
            .and_then(normalize_effort_label);
        let hidden = entry
            .get("hidden")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let input_modalities = entry
            .get("inputModalities")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        out.push(DiscoveredModel {
            slug,
            display_name,
            supports_effort: !supported_efforts.is_empty(),
            supported_efforts,
            default_effort,
            hidden,
            input_modalities,
        });
    }
    Ok((out, next))
}

/// Codex `models_cache.json` fallback.
pub fn parse_codex_models_cache_file(body: &str) -> Result<Vec<DiscoveredModel>, String> {
    let root: JsonValue =
        serde_json::from_str(body).map_err(|e| format!("codex models_cache json: {e}"))?;
    let models = root
        .get("models")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "codex models_cache missing models".to_string())?;
    let mut synthetic = serde_json::json!({ "data": models, "nextCursor": null });
    // Remap cache field names onto live shape.
    if let Some(arr) = synthetic.get_mut("data").and_then(|v| v.as_array_mut()) {
        for entry in arr.iter_mut() {
            if let Some(obj) = entry.as_object_mut() {
                if !obj.contains_key("model") {
                    if let Some(slug) = obj.get("slug").cloned() {
                        obj.insert("model".into(), slug);
                    }
                }
                if !obj.contains_key("displayName") {
                    if let Some(name) = obj.get("display_name").cloned() {
                        obj.insert("displayName".into(), name);
                    }
                }
                if !obj.contains_key("defaultReasoningEffort") {
                    if let Some(level) = obj.get("default_reasoning_level").cloned() {
                        let effort = level.get("effort").cloned().unwrap_or(level);
                        obj.insert("defaultReasoningEffort".into(), effort);
                    }
                }
                if !obj.contains_key("supportedReasoningEfforts") {
                    if let Some(levels) = obj.get("supported_reasoning_levels").cloned() {
                        obj.insert("supportedReasoningEfforts".into(), levels);
                    }
                }
            }
        }
    }
    parse_codex_model_list(&synthetic).map(|(models, _)| models)
}

/// Cursor `cursor/list_available_models` result.
pub fn parse_cursor_list_available_models(
    result: &JsonValue,
) -> Result<Vec<DiscoveredModel>, String> {
    let models = result
        .get("models")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "cursor list_available_models missing models".to_string())?;
    let mut out = Vec::new();
    for entry in models {
        if out.len() >= MAX_METADATA_MODELS {
            break;
        }
        let slug_raw = entry
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if slug_raw.is_empty() {
            continue;
        }
        let Ok(slug) = normalize_model_slug(slug_raw) else {
            continue;
        };
        let display_name = entry
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(slug_raw)
            .to_string();
        let mut supported_efforts = Vec::new();
        let mut default_effort = None;
        if let Some(options) = entry.get("configOptions").and_then(|v| v.as_array()) {
            for opt in options {
                let category = opt.get("category").and_then(|v| v.as_str()).unwrap_or("");
                if category != "thought_level"
                    && opt.get("id").and_then(|v| v.as_str()) != Some("effort")
                {
                    continue;
                }
                if let Some(current) = opt.get("currentValue").and_then(|v| v.as_str()) {
                    default_effort = normalize_effort_label(current);
                }
                if let Some(choices) = opt.get("options").and_then(|v| v.as_array()) {
                    for choice in choices {
                        if let Some(value) = choice.get("value").and_then(|v| v.as_str()) {
                            push_unique_effort(&mut supported_efforts, value);
                        }
                    }
                }
            }
        }
        // defaultAuto none — leave default_effort None when auto/none.
        if default_effort.as_deref() == Some("auto") {
            default_effort = None;
        }
        out.push(DiscoveredModel {
            slug,
            display_name,
            supports_effort: !supported_efforts.is_empty(),
            supported_efforts,
            default_effort,
            hidden: false,
            input_modalities: Vec::new(),
        });
    }
    Ok(out)
}

/// Claude OAuth usage JSON (legacy + modern meters).
pub fn parse_claude_usage_json(body: &str) -> Result<CachedUsageSnapshot, String> {
    let root: JsonValue =
        serde_json::from_str(body).map_err(|e| format!("claude usage json: {e}"))?;
    let mut windows = Vec::new();

    if let Some(meters) = root.get("meters").and_then(|v| v.as_array()) {
        for meter in meters {
            if windows.len() >= MAX_USAGE_WINDOWS {
                break;
            }
            let kind = meter.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            let (id, label) = match kind {
                "session" => ("five_hour", "5-hour"),
                "weekly_all" => ("week", "Weekly"),
                "weekly_scoped" => ("week_scoped", "Weekly (scoped)"),
                other if !other.is_empty() => (other, other),
                _ => continue,
            };
            let used = clamp_percent(meter.get("percent").and_then(|v| v.as_f64()));
            let scope_label = meter
                .pointer("/scope/model/display_name")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let resets = meter.get("resets_at").map(resets_at_to_ms).unwrap_or(None);
            windows.push(ProviderUsageWindowWire {
                id: id.into(),
                label: label.into(),
                used_percent: used,
                remaining_percent: remaining_from_used(used),
                resets_at_unix_ms: resets,
                window_duration_mins: None,
                scope_label,
            });
        }
    }

    if windows.is_empty() {
        // Legacy five_hour / seven_day utilization.
        if let Some(five) = root.get("five_hour") {
            let used = clamp_percent(
                five.get("utilization")
                    .or_else(|| five.get("used_percentage"))
                    .and_then(|v| v.as_f64()),
            );
            windows.push(ProviderUsageWindowWire {
                id: "five_hour".into(),
                label: "5-hour".into(),
                used_percent: used,
                remaining_percent: remaining_from_used(used),
                resets_at_unix_ms: five.get("resets_at").map(resets_at_to_ms).unwrap_or(None),
                window_duration_mins: Some(5 * 60),
                scope_label: None,
            });
        }
        if let Some(week) = root.get("seven_day") {
            let used = clamp_percent(
                week.get("utilization")
                    .or_else(|| week.get("used_percentage"))
                    .and_then(|v| v.as_f64()),
            );
            windows.push(ProviderUsageWindowWire {
                id: "week".into(),
                label: "Weekly".into(),
                used_percent: used,
                remaining_percent: remaining_from_used(used),
                resets_at_unix_ms: week.get("resets_at").map(resets_at_to_ms).unwrap_or(None),
                window_duration_mins: Some(7 * 24 * 60),
                scope_label: None,
            });
        }
    }

    if windows.is_empty() {
        return Ok(CachedUsageSnapshot {
            windows: Vec::new(),
            checked_at_unix_ms: None,
            state: ProviderUsageStateWire::Unavailable,
        });
    }
    Ok(CachedUsageSnapshot {
        windows,
        checked_at_unix_ms: None,
        state: ProviderUsageStateWire::Fresh,
    })
}

/// Codex `account/rateLimits/read` result.
pub fn parse_codex_rate_limits(result: &JsonValue) -> Result<CachedUsageSnapshot, String> {
    let mut windows = Vec::new();
    if let Some(groups) = result
        .get("rateLimitsByLimitId")
        .and_then(JsonValue::as_object)
    {
        for (scope, snapshot) in groups {
            push_codex_limit_snapshot(&mut windows, Some(scope), snapshot);
        }
    } else if let Some(snapshot) = result.get("rateLimits").filter(|value| !value.is_null()) {
        push_codex_limit_snapshot(&mut windows, None, snapshot);
    } else if result.get("primary").is_some() || result.get("secondary").is_some() {
        push_codex_limit_snapshot(&mut windows, None, result);
    } else {
        return Err("codex rate limits missing".into());
    }

    if windows.is_empty() {
        return Ok(CachedUsageSnapshot {
            windows: Vec::new(),
            checked_at_unix_ms: None,
            state: ProviderUsageStateWire::Unavailable,
        });
    }
    Ok(CachedUsageSnapshot {
        windows,
        checked_at_unix_ms: None,
        state: ProviderUsageStateWire::Fresh,
    })
}

fn push_codex_limit_snapshot(
    windows: &mut Vec<ProviderUsageWindowWire>,
    scope: Option<&str>,
    snapshot: &JsonValue,
) {
    for key in ["primary", "secondary"] {
        if windows.len() >= MAX_USAGE_WINDOWS {
            return;
        }
        let Some(value) = snapshot.get(key).filter(|value| !value.is_null()) else {
            continue;
        };
        let previous = windows.len();
        push_codex_limit_window(windows, key, value);
        if let Some(scope) = scope.filter(|scope| *scope != "codex") {
            if let Some(window) = windows.get_mut(previous) {
                window.id = format!("{scope}:{key}");
                window.label = format!("{scope} · {}", window.label);
            }
        }
    }
}

fn push_codex_limit_window(
    windows: &mut Vec<ProviderUsageWindowWire>,
    id: &str,
    value: &JsonValue,
) {
    if value.is_null() {
        return;
    }
    let used = clamp_percent(
        value
            .get("usedPercent")
            .or_else(|| value.get("used_percent"))
            .and_then(|v| v.as_f64()),
    );
    let duration = value
        .get("windowDurationMins")
        .or_else(|| value.get("window_duration_mins"))
        .and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64)));
    let resets = value
        .get("resetsAt")
        .or_else(|| value.get("resets_at"))
        .map(resets_at_to_ms)
        .unwrap_or(None);
    // Window roles do not imply duration: a primary window can be weekly.
    let label = match duration {
        Some(300) => "5h".into(),
        Some(10080) => "Weekly".into(),
        Some(43200) => "Monthly (30d)".into(),
        Some(mins) if mins > 0 && mins % 1440 == 0 => format!("{}d", mins / 1440),
        Some(mins) if mins > 0 && mins % 60 == 0 => format!("{}h", mins / 60),
        Some(mins) => format!("{mins}m"),
        None => id.to_string(),
    };
    windows.push(ProviderUsageWindowWire {
        id: id.to_string(),
        label,
        used_percent: used,
        remaining_percent: remaining_from_used(used),
        resets_at_unix_ms: resets,
        window_duration_mins: duration,
        scope_label: None,
    });
}

/// Cursor DashboardService GetCurrentPeriodUsage body.
pub fn parse_cursor_period_usage(body: &str) -> Result<CachedUsageSnapshot, String> {
    let root: JsonValue =
        serde_json::from_str(body).map_err(|e| format!("cursor usage json: {e}"))?;
    let plan = root
        .get("planUsage")
        .ok_or_else(|| "cursor usage missing planUsage".to_string())?;

    let auto_used = clamp_percent(plan.get("autoPercentUsed").and_then(|v| v.as_f64()));
    let api_used = clamp_percent(plan.get("apiPercentUsed").and_then(|v| v.as_f64()));
    // Do not derive a blended total bar from totalSpend/limit (bonus blend).
    let cycle_end = root
        .get("billingCycleEnd")
        .map(resets_at_to_ms)
        .unwrap_or(None);

    let mut windows = Vec::new();
    windows.push(ProviderUsageWindowWire {
        id: "auto".into(),
        label: "Auto / included".into(),
        used_percent: auto_used,
        remaining_percent: remaining_from_used(auto_used),
        resets_at_unix_ms: cycle_end,
        window_duration_mins: None,
        scope_label: None,
    });
    windows.push(ProviderUsageWindowWire {
        id: "api".into(),
        label: "API / other".into(),
        used_percent: api_used,
        remaining_percent: remaining_from_used(api_used),
        resets_at_unix_ms: cycle_end,
        window_duration_mins: None,
        scope_label: None,
    });

    let display_message = root
        .get("displayMessage")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let state = if root.get("enabled").and_then(|v| v.as_bool()) == Some(false) {
        ProviderUsageStateWire::Unavailable
    } else {
        ProviderUsageStateWire::Fresh
    };

    let mut snapshot = CachedUsageSnapshot {
        windows,
        checked_at_unix_ms: None,
        state,
    };
    if let Some(message) = display_message {
        // Surface limit message via a synthetic window scope, not as invented %.
        if let Some(first) = snapshot.windows.first_mut() {
            first.scope_label = Some(message);
        }
    }
    Ok(snapshot)
}

/// Extract Claude OAuth access token from `.credentials.json` without logging it.
pub fn extract_claude_oauth_token(credentials_json: &str) -> Option<String> {
    let root: JsonValue = serde_json::from_str(credentials_json).ok()?;
    root.pointer("/claudeAiOauth/accessToken")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Extract Cursor CLI access token from `auth.json` without logging it.
pub fn extract_cursor_access_token(auth_json: &str) -> Option<String> {
    let root: JsonValue = serde_json::from_str(auth_json).ok()?;
    root.get("accessToken")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// True when Claude `.credentials.json` carries a non-empty OAuth access token.
/// Does not inspect or return the token value.
pub fn claude_credentials_have_access_token(credentials_json: &str) -> bool {
    let Ok(root) = serde_json::from_str::<JsonValue>(credentials_json) else {
        return false;
    };
    root.pointer("/claudeAiOauth/accessToken")
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.trim().is_empty())
}

/// In-memory credential context for probe stability checks.
/// Includes SHA-256 digests of access/refresh tokens (never logged or persisted
/// as cache identity) plus scopes/subscription — timestamps are excluded.
pub fn claude_credential_context_material(credentials_json: &str) -> Option<String> {
    let root: JsonValue = serde_json::from_str(credentials_json).ok()?;
    let oauth = root.get("claudeAiOauth")?;
    let access = oauth
        .get("accessToken")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let refresh = oauth
        .get("refreshToken")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("");
    let access_fp = fingerprint_account_material(access);
    let refresh_fp = if refresh.is_empty() {
        String::new()
    } else {
        fingerprint_account_material(refresh)
    };
    let subscription = oauth
        .get("subscriptionType")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let scopes = match oauth.get("scopes") {
        Some(JsonValue::Array(items)) => {
            let mut parts: Vec<&str> = items.iter().filter_map(|v| v.as_str()).collect();
            parts.sort_unstable();
            parts.join(",")
        }
        Some(JsonValue::String(s)) => s.trim().to_string(),
        _ => String::new(),
    };
    Some(format!(
        "claude-credctx:{access_fp}|{refresh_fp}|{subscription}|{scopes}"
    ))
}

/// Account scope from stock/custom `.claude.json` `oauthAccount`.
/// Canonical: `claude:{accountUuid}:{organizationUuid}` (email never required).
pub fn claude_account_fingerprint_material_from_config(config_json: &str) -> Option<String> {
    let root: JsonValue = serde_json::from_str(config_json).ok()?;
    let oauth = root.get("oauthAccount")?;
    let account = oauth
        .get("accountUuid")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let org = oauth
        .get("organizationUuid")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    if account.len() > 128 || org.len() > 128 {
        return None;
    }
    Some(format!("claude:{account}:{org}"))
}

/// Decode Cursor access-token JWT payload `sub` for cache-scope only.
/// Not authentication/authorization. Never logs token or claims.
pub fn cursor_jwt_sub_for_cache_scope(access_token: &str) -> Option<String> {
    use base64::Engine;
    const MAX_PART: usize = 8 * 1024;
    const MAX_SUB: usize = 256;
    let token = access_token.trim();
    if token.is_empty() || token.len() > 16 * 1024 {
        return None;
    }
    let mut parts = token.split('.');
    let header = parts.next()?;
    let payload = parts.next()?;
    let signature = parts.next()?;
    if parts.next().is_some() || header.is_empty() || payload.is_empty() || signature.is_empty() {
        return None;
    }
    if payload.len() > MAX_PART {
        return None;
    }
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
        .ok()?;
    if decoded.is_empty() || decoded.len() > MAX_PART {
        return None;
    }
    let claims: JsonValue = serde_json::from_slice(&decoded).ok()?;
    let sub = claims.get("sub")?.as_str()?.trim();
    if sub.is_empty() || sub.len() > MAX_SUB {
        return None;
    }
    if !sub
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '@'))
    {
        return None;
    }
    Some(sub.to_string())
}

/// Cursor account scope from auth.json accessToken JWT `sub` only.
pub fn cursor_account_fingerprint_material(auth_json: &str) -> Option<String> {
    let token = extract_cursor_access_token(auth_json)?;
    let sub = cursor_jwt_sub_for_cache_scope(&token)?;
    Some(format!("cursor-sub:{sub}"))
}

/// Canonical Codex account scope: `codex-id:{tokens.account_id}` only.
/// Never switches to email-based schemes (account/read nest differs).
pub fn codex_account_fingerprint_material(auth_json: &str) -> Option<String> {
    let root: JsonValue = serde_json::from_str(auth_json).ok()?;
    let id = root
        .pointer("/tokens/account_id")
        .or_else(|| root.pointer("/tokens/accountId"))
        .and_then(|v| match v {
            JsonValue::String(s) => {
                let t = s.trim();
                if t.is_empty() {
                    None
                } else {
                    Some(t.to_string())
                }
            }
            JsonValue::Number(n) => Some(n.to_string()),
            _ => None,
        })?;
    if id.len() > 128 {
        return None;
    }
    Some(format!("codex-id:{id}"))
}

/// Extract the same canonical Codex account id from `account/read` result.
pub fn codex_account_id_from_account_read(result: &JsonValue) -> Option<String> {
    let id = result
        .pointer("/account/account_id")
        .or_else(|| result.pointer("/account/accountId"))
        .or_else(|| result.pointer("/account/id"))
        .or_else(|| result.pointer("/accountId"))
        .or_else(|| result.get("account_id"))
        .and_then(|v| match v {
            JsonValue::String(s) => {
                let t = s.trim();
                if t.is_empty() {
                    None
                } else {
                    Some(t.to_string())
                }
            }
            JsonValue::Number(n) => Some(n.to_string()),
            _ => None,
        })?;
    if id.len() > 128 {
        return None;
    }
    Some(format!("codex-id:{id}"))
}

pub fn fingerprint_account_material(material: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(material.as_bytes()))
}
