//! Wire-facing model catalog and usage projections for provider settings.
//!
//! These types are host/local-authority only. They never carry home paths,
//! environment maps, tokens, or API keys. Serde defaults keep older clients
//! wire-compatible when new fields appear.

use serde::{Deserialize, Serialize};

/// How the projected catalog was obtained for the current snapshot read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum ProviderMetadataSource {
    #[default]
    Empty,
    LastGood,
    Live,
}

impl ProviderMetadataSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::LastGood => "lastGood",
            Self::Live => "live",
        }
    }
}

/// One picker model row after policy (favorites/hidden/custom) is applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderModelEntryWire {
    pub slug: String,
    pub display_name: String,
    #[serde(default)]
    pub supports_effort: bool,
    #[serde(default)]
    pub supported_efforts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub is_custom: bool,
    #[serde(default)]
    pub is_favorite: bool,
    #[serde(default)]
    pub input_modalities: Vec<String>,
}

/// Per-instance model catalog projection for UI pickers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderModelCatalogWire {
    pub instance_id: String,
    pub driver: String,
    #[serde(default)]
    pub models: Vec<ProviderModelEntryWire>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at_unix_ms: Option<u64>,
    #[serde(default)]
    pub stale: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub source: ProviderMetadataSource,
    /// Non-secret config fingerprint that produced this catalog.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_fingerprint: Option<String>,
    /// Non-secret account proof fingerprint; changes invalidate usage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_fingerprint: Option<String>,
}

impl ProviderModelCatalogWire {
    pub fn empty(instance_id: impl Into<String>, driver: impl Into<String>) -> Self {
        Self {
            instance_id: instance_id.into(),
            driver: driver.into(),
            models: Vec::new(),
            checked_at_unix_ms: None,
            stale: false,
            error: None,
            source: ProviderMetadataSource::Empty,
            config_fingerprint: None,
            account_fingerprint: None,
        }
    }
}

/// Truthful usage window. Absent percents stay `None` (never invented 0/100).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderUsageWindowWire {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_percent: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_percent: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_duration_mins: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_label: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum ProviderUsageStateWire {
    #[default]
    Unknown,
    Fresh,
    Stale,
    Unavailable,
    Unsupported,
    AuthRequired,
    Failed,
    Backoff,
}

impl ProviderUsageStateWire {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Fresh => "fresh",
            Self::Stale => "stale",
            Self::Unavailable => "unavailable",
            Self::Unsupported => "unsupported",
            Self::AuthRequired => "authRequired",
            Self::Failed => "failed",
            Self::Backoff => "backoff",
        }
    }
}

/// Per-instance usage projection for settings UI (not chat-token scrape).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderUsageWire {
    pub instance_id: String,
    pub driver: String,
    #[serde(default)]
    pub state: ProviderUsageStateWire,
    #[serde(default)]
    pub windows: Vec<ProviderUsageWindowWire>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_unix_ms: Option<u64>,
    #[serde(default)]
    pub source: ProviderMetadataSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_fingerprint: Option<String>,
}

impl ProviderUsageWire {
    pub fn empty(instance_id: impl Into<String>, driver: impl Into<String>) -> Self {
        Self {
            instance_id: instance_id.into(),
            driver: driver.into(),
            state: ProviderUsageStateWire::Unknown,
            windows: Vec::new(),
            checked_at_unix_ms: None,
            error: None,
            retry_after_unix_ms: None,
            source: ProviderMetadataSource::Empty,
            config_fingerprint: None,
            account_fingerprint: None,
        }
    }

    pub fn unsupported(instance_id: impl Into<String>, driver: impl Into<String>) -> Self {
        Self {
            state: ProviderUsageStateWire::Unsupported,
            ..Self::empty(instance_id, driver)
        }
    }
}

/// Internal discovered model before visibility policy merge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredModel {
    pub slug: String,
    pub display_name: String,
    #[serde(default)]
    pub supports_effort: bool,
    #[serde(default)]
    pub supported_efforts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub input_modalities: Vec<String>,
}

/// Durable last-good catalog payload (no secrets).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CachedModelCatalog {
    #[serde(default)]
    pub models: Vec<DiscoveredModel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at_unix_ms: Option<u64>,
}

/// Durable last-good usage payload (no secrets).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CachedUsageSnapshot {
    #[serde(default)]
    pub windows: Vec<ProviderUsageWindowWire>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at_unix_ms: Option<u64>,
    #[serde(default)]
    pub state: ProviderUsageStateWire,
}

/// One scoped cache row keyed by instance + config + account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderMetadataCacheEntry {
    pub instance_id: String,
    pub driver: String,
    pub config_fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_fingerprint: Option<String>,
    #[serde(default)]
    pub models: CachedModelCatalog,
    #[serde(default)]
    pub usage: CachedUsageSnapshot,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_backoff_until_unix_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ProviderMetadataCacheDocument {
    #[serde(default = "metadata_cache_version")]
    pub version: u32,
    #[serde(default)]
    pub entries: Vec<ProviderMetadataCacheEntry>,
}

fn metadata_cache_version() -> u32 {
    1
}

pub const METADATA_CACHE_VERSION: u32 = 1;
pub const METADATA_STALE_AFTER_MS: u64 = 60 * 60 * 1000;
pub const MAX_METADATA_MODELS: usize = 256;
pub const MAX_METADATA_EFFORTS: usize = 16;
pub const MAX_USAGE_WINDOWS: usize = 8;
