mod authority;
mod binding;
mod health;
mod health_job;
mod health_probe;
mod launch_policy;
mod metadata_cache;
mod metadata_parse;
mod metadata_probe;
mod metadata_types;
mod model;
mod profile;
pub(crate) mod secret;
mod store;
pub(crate) mod usage_http;

#[cfg(test)]
#[path = "core_tests.rs"]
mod core_tests;

#[cfg(test)]
#[path = "metadata_tests.rs"]
mod metadata_tests;

pub use authority::{
    health_probe_plan, publish_instance_health, ComposerProviderChoice, ProviderHealthRowWire,
    ProviderSettingsAuthority, ProviderSettingsAuthorityError, ProviderSettingsHostRequest,
    ProviderSettingsMutation, ProviderSettingsQuery, ProviderSettingsReply,
    ProviderSettingsSnapshot,
};
pub use binding::{
    default_instance_id_for_kind, ProviderInstanceBinding, ProviderInstanceBindingError,
    ProviderInstanceBindingStore,
};
pub use health::{
    apply_probe_outcome, unix_now_ms, ProviderHealthCache, ProviderHealthRefreshGuard,
    ProviderHealthRow, ProviderHealthStatus,
};
pub use health_job::ProviderHealthJobOwner;
pub use health_probe::{
    apply_cursor_about_to_row, cursor_about_probe_kinds, parse_cursor_about_json,
    parse_cursor_about_plain_bytes, parse_cursor_about_strict_json, CursorAboutAuth,
    CursorAboutFacts,
};
pub use launch_policy::{
    apply_instance_to_discovery, merge_instance_environment, prepare_codex_shadow_home,
    resolve_launch_config, resolve_launch_config_with_known_models, scope_fingerprint_bytes,
    LaunchPolicyError, ProviderInstanceScope, ResolvedProviderLaunchConfig,
};
pub use metadata_cache::{
    config_scope_fingerprint, effective_scope_fingerprint, ProviderMetadataCache,
};
pub use metadata_probe::{
    project_all_from_cache, project_model_catalog_wire, project_usage_wire,
    prune_metadata_cache_for_settings, refresh_instance_metadata, METADATA_PROBE_TIMEOUT,
};
pub use metadata_types::{
    DiscoveredModel, ProviderMetadataSource, ProviderModelCatalogWire, ProviderModelEntryWire,
    ProviderUsageStateWire, ProviderUsageWindowWire, ProviderUsageWire,
};
pub use model::{
    builtin_slugs_for_driver, normalize_model_slug, validate_env_name, validate_instance_id,
    validate_model_slug, BuiltinProviderDriver, CustomModelEntry, ModelVisibilityPolicy,
    ProviderDriverKind, ProviderEnvVar, ProviderInstanceConfig, ProviderInstanceId,
    ProviderSettingsDocument, ProviderSettingsError, StubProviderDriver,
    CLAUDE_DEFAULT_INSTANCE_ID, CODEX_DEFAULT_INSTANCE_ID, CURSOR_DEFAULT_INSTANCE_ID,
    DEFAULT_HEALTH_INTERVAL_SECS,
};
pub use profile::ProviderProfileOwner;
pub use secret::{protect_secret_value, reveal_secret_value, SecretCustodyError};
pub use store::{ProviderSettingsStore, ProviderSettingsStoreError};
