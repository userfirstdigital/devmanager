//! Provider-reported models and usage; static choices are offline fallback only.

pub use crate::providers::settings::{
    ProviderModelCatalogWire as UiModelCatalog, ProviderModelEntryWire as UiModelEntry,
    ProviderUsageWindowWire as UiUsageWindow, ProviderUsageWire as UiUsage,
};
use crate::providers::ProviderReasoningEffort;

/// Merge policy-ordered slugs with provider-reported catalog rows.
/// Policy order wins; reported-only slugs (e.g. Fable) append when not hidden.
pub fn merge_picker_slugs(
    policy_ordered: &[String],
    catalog: Option<&UiModelCatalog>,
) -> Vec<String> {
    let mut out = policy_ordered.to_vec();
    let Some(catalog) = catalog else {
        return out;
    };
    if catalog.source.as_str() == "empty" && catalog.models.is_empty() {
        return out;
    }
    out.retain(|slug| {
        catalog
            .models
            .iter()
            .any(|model| !model.hidden && &model.slug == slug)
    });
    for model in &catalog.models {
        if model.hidden {
            continue;
        }
        if !out.iter().any(|slug| slug == &model.slug) {
            out.push(model.slug.clone());
        }
    }
    out
}

pub fn catalog_for_instance<'a>(
    catalogs: &'a [UiModelCatalog],
    instance_id: &str,
) -> Option<&'a UiModelCatalog> {
    catalogs.iter().find(|c| c.instance_id == instance_id)
}

pub fn usage_for_instance<'a>(usages: &'a [UiUsage], instance_id: &str) -> Option<&'a UiUsage> {
    usages.iter().find(|u| u.instance_id == instance_id)
}

pub fn entry_for_slug<'a>(catalog: &'a UiModelCatalog, slug: &str) -> Option<&'a UiModelEntry> {
    catalog.models.iter().find(|m| m.slug == slug)
}

/// Map wire effort tokens to typed efforts; empty list means no thinking control.
pub fn efforts_from_supported(supported: &[String]) -> Vec<ProviderReasoningEffort> {
    if supported.is_empty() {
        return vec![ProviderReasoningEffort::ProviderDefault];
    }
    let mut out = vec![ProviderReasoningEffort::ProviderDefault];
    for token in supported {
        let effort = match token.to_ascii_lowercase().as_str() {
            "low" => Some(ProviderReasoningEffort::Low),
            "medium" | "mid" => Some(ProviderReasoningEffort::Medium),
            "high" => Some(ProviderReasoningEffort::High),
            "xhigh" | "extra_high" | "extrahigh" | "extra-high" => {
                Some(ProviderReasoningEffort::ExtraHigh)
            }
            "max" => Some(ProviderReasoningEffort::Max),
            "ultra" => Some(ProviderReasoningEffort::Ultra),
            _ => None,
        };
        if let Some(effort) = effort {
            if !out.contains(&effort) {
                out.push(effort);
            }
        }
    }
    out
}

pub fn normalize_effort_to_supported(
    current: ProviderReasoningEffort,
    supported: &[ProviderReasoningEffort],
    default_token: Option<&str>,
) -> ProviderReasoningEffort {
    if supported.contains(&current) {
        return current;
    }
    if let Some(token) = default_token {
        let mapped = efforts_from_supported(&[token.to_string()]);
        if let Some(effort) = mapped
            .into_iter()
            .find(|e| *e != ProviderReasoningEffort::ProviderDefault && supported.contains(e))
        {
            return effort;
        }
    }
    supported
        .iter()
        .copied()
        .find(|e| *e != ProviderReasoningEffort::ProviderDefault)
        .unwrap_or(ProviderReasoningEffort::ProviderDefault)
}

/// Honest compact usage label: remaining + window/reset when reported.
pub fn format_usage_summary(usage: &UiUsage) -> String {
    match usage.state.as_str() {
        "unsupported" => "Usage unsupported".into(),
        "authRequired" => "Usage: sign in required".into(),
        "backoff" => {
            if usage.retry_after_unix_ms.is_some() {
                "Usage: backing off".into()
            } else {
                "Usage: backoff".into()
            }
        }
        "failed" => usage
            .error
            .clone()
            .unwrap_or_else(|| "Usage unavailable".into()),
        "unavailable" | "unknown" => "Usage unknown".into(),
        "stale" => {
            let base = format_windows_summary(&usage.windows);
            if base.is_empty() {
                "Usage stale".into()
            } else {
                format!("{base} (stale)")
            }
        }
        _ => {
            let base = format_windows_summary(&usage.windows);
            if base.is_empty() {
                if usage.source.as_str() == "empty" {
                    "Usage unknown".into()
                } else {
                    "Usage reported".into()
                }
            } else {
                base
            }
        }
    }
}

fn format_windows_summary(windows: &[UiUsageWindow]) -> String {
    windows
        .iter()
        .filter_map(|window| {
            let remaining = window.remaining_percent.map(|p| format!("{p}% left"));
            let used = window.used_percent.map(|p| format!("{p}% used"));
            let amount = remaining.or(used)?;
            let scope = window
                .scope_label
                .as_deref()
                .or(Some(window.label.as_str()))
                .unwrap_or("");
            let amount = if scope.is_empty() {
                amount
            } else {
                format!("{scope}: {amount}")
            };
            let reset = window.resets_at_unix_ms.and_then(|reset| {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()?
                    .as_millis() as u64;
                let minutes = reset.saturating_sub(now).div_ceil(60_000);
                Some(if minutes >= 1440 {
                    format!("{}d {}h", minutes / 1440, minutes % 1440 / 60)
                } else if minutes >= 60 {
                    format!("{}h {}m", minutes / 60, minutes % 60)
                } else {
                    format!("{minutes}m")
                })
            });
            Some(match reset {
                Some(reset) => format!("{amount} ↻ {reset}"),
                None => amount,
            })
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::settings::ProviderMetadataSource;

    #[test]
    fn live_catalog_replaces_offline_models_and_keeps_dynamic_efforts() {
        let mut catalog = UiModelCatalog::empty("claude", "claude");
        catalog.source = ProviderMetadataSource::Live;
        catalog.models.push(UiModelEntry {
            slug: "claude-fable-5".into(),
            display_name: "Claude Fable".into(),
            supports_effort: true,
            supported_efforts: vec!["high".into(), "max".into()],
            default_effort: Some("high".into()),
            hidden: false,
            is_custom: false,
            is_favorite: false,
            input_modalities: vec!["text".into(), "image".into()],
        });
        assert_eq!(
            merge_picker_slugs(&["old-model".into()], Some(&catalog)),
            vec!["claude-fable-5"]
        );
        let efforts = efforts_from_supported(&["max".into(), "ultra".into()]);
        assert!(efforts.contains(&ProviderReasoningEffort::Ultra));
        assert_eq!(
            normalize_effort_to_supported(ProviderReasoningEffort::Low, &efforts, Some("ultra")),
            ProviderReasoningEffort::Ultra
        );
    }
}
