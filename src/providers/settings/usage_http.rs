//! Bounded, allowlisted, no-redirect HTTP for provider usage endpoints.
//!
//! Credentials are read only for the exact configured stock context and never
//! logged or persisted. Custom/API-key instances must not borrow stock auth.
//! Effective account context comes from [`ResolvedProviderLaunchConfig`]
//! (discovery child env + exact home), not instance fields alone.

use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::launch_policy::ResolvedProviderLaunchConfig;
use super::metadata_parse::{
    claude_account_fingerprint_material_from_config, claude_credential_context_material,
    claude_credentials_have_access_token, codex_account_fingerprint_material,
    cursor_account_fingerprint_material, extract_claude_oauth_token, extract_cursor_access_token,
    fingerprint_account_material, parse_claude_usage_json, parse_cursor_period_usage,
};
use super::metadata_types::{CachedUsageSnapshot, ProviderUsageStateWire};
use super::model::ProviderDriverKind;

pub const USAGE_HTTP_TIMEOUT: Duration = Duration::from_secs(8);
pub const MAX_USAGE_RESPONSE_BYTES: u64 = 256 * 1024;
pub const MAX_CREDENTIAL_FILE_BYTES: u64 = 64 * 1024;
/// Stock `.claude.json` is larger than credentials; keep a separate bound.
pub const MAX_CLAUDE_ACCOUNT_CONFIG_BYTES: u64 = 256 * 1024;
pub const MAX_MODELS_CACHE_FILE_BYTES: u64 = 1024 * 1024;
/// Conservative default when Retry-After is absent on HTTP 429.
pub const DEFAULT_USAGE_BACKOFF_SECS: u64 = 15 * 60;
pub const MAX_USAGE_BACKOFF_SECS: u64 = 60 * 60;

const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const CLAUDE_API_ORIGIN: &str = "https://api.anthropic.com";
const CURSOR_USAGE_URL: &str =
    "https://api2.cursor.sh/aiserver.v1.DashboardService/GetCurrentPeriodUsage";

const API_KEY_ENV_NAMES: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "OPENAI_API_KEY",
    "CURSOR_API_KEY",
    "CURSOR_AUTH_TOKEN",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsageHttpError {
    UnsupportedContext(String),
    AuthRequired,
    Unavailable(String),
    Backoff { retry_after_secs: u64 },
    Failed(String),
}

impl std::fmt::Display for UsageHttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedContext(msg) => write!(f, "usage unsupported: {msg}"),
            Self::AuthRequired => write!(f, "usage auth required"),
            Self::Unavailable(msg) => write!(f, "usage unavailable: {msg}"),
            Self::Backoff { retry_after_secs } => {
                write!(f, "usage backoff retry-after={retry_after_secs}s")
            }
            Self::Failed(msg) => write!(f, "usage failed: {msg}"),
        }
    }
}

impl std::error::Error for UsageHttpError {}

#[derive(Debug, Clone)]
pub struct UsageQueryOutcome {
    pub usage: CachedUsageSnapshot,
    pub account_fingerprint: Option<String>,
    pub retry_after_secs: Option<u64>,
}

fn env_name_is(key: &OsStr, expected: &str) -> bool {
    if cfg!(windows) {
        key.to_string_lossy().eq_ignore_ascii_case(expected)
    } else {
        key == OsStr::new(expected)
    }
}

/// True when the effective child environment carries an API-key credential.
pub fn effective_env_has_api_key(env: &std::collections::BTreeMap<OsString, OsString>) -> bool {
    env.iter().any(|(key, value)| {
        API_KEY_ENV_NAMES.iter().any(|name| env_name_is(key, name)) && !value.is_empty()
    })
}

fn claude_home_from_resolved(
    resolved: &ResolvedProviderLaunchConfig,
) -> Result<PathBuf, UsageHttpError> {
    // Explicit instance home / CLAUDE_CONFIG_DIR override — never stock fallback.
    if let Some(home) = resolved
        .home_path
        .as_ref()
        .filter(|p| !p.as_os_str().is_empty())
    {
        return Ok(home.clone());
    }
    if let Some(dir) = resolved
        .discovery
        .child_environment
        .iter()
        .find(|(k, v)| env_name_is(k, "CLAUDE_CONFIG_DIR") && !v.is_empty())
        .map(|(_, v)| PathBuf::from(v))
    {
        return Ok(dir);
    }
    dirs::home_dir()
        .map(|h| h.join(".claude"))
        .ok_or_else(|| UsageHttpError::Unavailable("claude home unavailable".into()))
}

/// True when the resolved launch config selected an explicit Claude config dir
/// (instance home or CLAUDE_CONFIG_DIR), not the implicit stock `~/.claude`.
pub fn claude_config_dir_is_explicit(resolved: &ResolvedProviderLaunchConfig) -> bool {
    resolved
        .home_path
        .as_ref()
        .is_some_and(|p| !p.as_os_str().is_empty())
        || resolved
            .discovery
            .child_environment
            .iter()
            .any(|(k, v)| env_name_is(k, "CLAUDE_CONFIG_DIR") && !v.is_empty())
}

fn codex_home_from_resolved(resolved: &ResolvedProviderLaunchConfig) -> Option<PathBuf> {
    // Explicit override paths win even when unreadable (AuthRequired on read).
    if let Some(shadow) = resolved
        .shadow_home_path
        .as_ref()
        .filter(|p| !p.as_os_str().is_empty())
    {
        return Some(shadow.clone());
    }
    if let Some(home) = resolved
        .home_path
        .as_ref()
        .filter(|p| !p.as_os_str().is_empty())
    {
        return Some(home.clone());
    }
    if let Some(dir) = resolved
        .discovery
        .child_environment
        .iter()
        .find(|(k, v)| env_name_is(k, "CODEX_HOME") && !v.is_empty())
        .map(|(_, v)| PathBuf::from(v))
    {
        return Some(dir);
    }
    // Stock default only when no override is present.
    dirs::home_dir().map(|h| h.join(".codex"))
}

/// Path to Claude account metadata JSON for the exact selected config context.
///
/// - Implicit stock (`~/.claude`): account file is `~/.claude.json`
/// - Explicit `CLAUDE_CONFIG_DIR=D` (even when `D` ends with `.claude`): `D/.claude.json`
///
/// Never falls back to the stock user home when a custom config dir is selected.
pub fn claude_account_config_path(config_dir: &Path, explicit_override: bool) -> PathBuf {
    if explicit_override {
        return config_dir.join(".claude.json");
    }
    config_dir
        .parent()
        .map(|parent| parent.join(".claude.json"))
        .unwrap_or_else(|| config_dir.join(".claude.json"))
}

/// Reject any custom Claude endpoint; only the official Anthropic origin is used.
fn reject_custom_claude_endpoint(
    resolved: &ResolvedProviderLaunchConfig,
) -> Result<(), UsageHttpError> {
    if let Some(endpoint) = resolved
        .api_endpoint
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty())
    {
        if !anthropic_origin_allowed(endpoint) && endpoint != CLAUDE_USAGE_URL {
            return Err(UsageHttpError::UnsupportedContext(
                "custom Claude endpoint cannot use stock OAuth usage".into(),
            ));
        }
    }
    // CURSOR_API_ENDPOINT / foreign vendor URLs in child env also block stock Claude auth.
    for (key, value) in &resolved.discovery.child_environment {
        if env_name_is(key, "CURSOR_API_ENDPOINT") && !value.is_empty() {
            return Err(UsageHttpError::UnsupportedContext(
                "Cursor endpoint present in Claude child env".into(),
            ));
        }
        if env_name_is(key, "ANTHROPIC_BASE_URL") {
            let text = value.to_string_lossy();
            let trimmed = text.trim();
            if !trimmed.is_empty() && !anthropic_origin_allowed(trimmed) {
                return Err(UsageHttpError::UnsupportedContext(
                    "custom Anthropic base URL blocks stock OAuth usage".into(),
                ));
            }
        }
    }
    Ok(())
}

/// Exact allowlist for Anthropic API origin. Prefix matches like
/// `https://api.anthropic.com.evil` must fail.
pub fn anthropic_origin_allowed(raw: &str) -> bool {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed == CLAUDE_API_ORIGIN {
        return true;
    }
    let Ok(url) = url::Url::parse(trimmed) else {
        return false;
    };
    if url.scheme() != "https" {
        return false;
    }
    if url.host_str() != Some("api.anthropic.com") {
        return false;
    }
    if url.port().is_some() {
        return false;
    }
    let path = url.path();
    path.is_empty() || path == "/"
}

fn reject_custom_cursor_context(
    resolved: &ResolvedProviderLaunchConfig,
) -> Result<(), UsageHttpError> {
    if resolved
        .api_endpoint
        .as_deref()
        .is_some_and(|e| !e.trim().is_empty())
    {
        return Err(UsageHttpError::UnsupportedContext(
            "custom Cursor endpoint cannot borrow stock auth".into(),
        ));
    }
    if resolved.home_path.is_some() {
        return Err(UsageHttpError::UnsupportedContext(
            "custom Cursor home has no attested usage mapping".into(),
        ));
    }
    for (key, value) in &resolved.discovery.child_environment {
        if env_name_is(key, "CURSOR_API_ENDPOINT") && !value.is_empty() {
            return Err(UsageHttpError::UnsupportedContext(
                "custom CURSOR_API_ENDPOINT blocks stock auth".into(),
            ));
        }
    }
    Ok(())
}

/// Claude usage for the exact selected home/env. Never borrows stock when custom/API.
pub fn query_claude_usage(
    resolved: &ResolvedProviderLaunchConfig,
    now_ms: u64,
) -> Result<UsageQueryOutcome, UsageHttpError> {
    if resolved.provider_kind != crate::providers::ProviderKind::ClaudeCode {
        return Err(UsageHttpError::UnsupportedContext("not claude".into()));
    }
    if effective_env_has_api_key(&resolved.discovery.child_environment)
        || effective_env_has_api_key(&resolved.environment)
    {
        return Err(UsageHttpError::UnsupportedContext(
            "API key context has no OAuth usage surface".into(),
        ));
    }
    reject_custom_claude_endpoint(resolved)?;
    let home = claude_home_from_resolved(resolved)?;
    let credentials_path = home.join(".credentials.json");
    let credentials = read_bounded_string(&credentials_path, MAX_CREDENTIAL_FILE_BYTES)
        .map_err(|_| UsageHttpError::AuthRequired)?;
    let token = extract_claude_oauth_token(&credentials).ok_or(UsageHttpError::AuthRequired)?;
    let config = read_bounded_string(
        &claude_account_config_path(&home, claude_config_dir_is_explicit(resolved)),
        MAX_CLAUDE_ACCOUNT_CONFIG_BYTES,
    )
    .map_err(|_| UsageHttpError::AuthRequired)?;
    let account_fingerprint = Some(
        claude_account_fingerprint_material_from_config(&config)
            .map(|material| fingerprint_account_material(&material))
            .ok_or(UsageHttpError::AuthRequired)?,
    );
    let response = http_get_allowlisted(
        CLAUDE_USAGE_URL,
        &[
            ("Authorization", &format!("Bearer {token}")),
            ("anthropic-beta", "oauth-2025-04-20"),
            ("Accept", "application/json"),
        ],
    )?;
    drop(token);
    let mut usage =
        parse_claude_usage_json(&response.body).map_err(|e| UsageHttpError::Failed(e))?;
    usage.checked_at_unix_ms = Some(now_ms);
    Ok(UsageQueryOutcome {
        usage,
        account_fingerprint,
        retry_after_secs: None,
    })
}

/// Cursor usage for stock CLI auth only.
pub fn query_cursor_usage(
    resolved: &ResolvedProviderLaunchConfig,
    now_ms: u64,
) -> Result<UsageQueryOutcome, UsageHttpError> {
    if resolved.provider_kind != crate::providers::ProviderKind::Cursor {
        return Err(UsageHttpError::UnsupportedContext("not cursor".into()));
    }
    if effective_env_has_api_key(&resolved.discovery.child_environment)
        || effective_env_has_api_key(&resolved.environment)
    {
        return Err(UsageHttpError::UnsupportedContext(
            "API key Cursor context cannot borrow stock auth".into(),
        ));
    }
    reject_custom_cursor_context(resolved)?;
    let auth_path = stock_cursor_auth_path().ok_or(UsageHttpError::AuthRequired)?;
    let auth = read_bounded_string(&auth_path, MAX_CREDENTIAL_FILE_BYTES)
        .map_err(|_| UsageHttpError::AuthRequired)?;
    let token = extract_cursor_access_token(&auth).ok_or(UsageHttpError::AuthRequired)?;
    let account_fingerprint = cursor_account_fingerprint_material(&auth)
        .map(|material| fingerprint_account_material(&material));
    let response = http_post_allowlisted(
        CURSOR_USAGE_URL,
        "{}",
        &[
            ("Authorization", &format!("Bearer {token}")),
            ("Content-Type", "application/json"),
            ("Connect-Protocol-Version", "1"),
        ],
    )?;
    drop(token);
    let mut usage =
        parse_cursor_period_usage(&response.body).map_err(|e| UsageHttpError::Failed(e))?;
    usage.checked_at_unix_ms = Some(now_ms);
    if usage.state == ProviderUsageStateWire::Unknown {
        usage.state = ProviderUsageStateWire::Fresh;
    }
    Ok(UsageQueryOutcome {
        usage,
        account_fingerprint,
        retry_after_secs: None,
    })
}

/// Bounded credential/account fingerprint without logging tokens.
pub fn resolve_claude_account_fingerprint(
    resolved: &ResolvedProviderLaunchConfig,
) -> Result<Option<String>, UsageHttpError> {
    if effective_env_has_api_key(&resolved.discovery.child_environment)
        || effective_env_has_api_key(&resolved.environment)
    {
        return Err(UsageHttpError::UnsupportedContext("api key context".into()));
    }
    reject_custom_claude_endpoint(resolved)?;
    let home = claude_home_from_resolved(resolved)?;
    let credentials =
        match read_bounded_string(&home.join(".credentials.json"), MAX_CREDENTIAL_FILE_BYTES) {
            Ok(body) => body,
            Err(_) => return Err(UsageHttpError::AuthRequired),
        };
    if !claude_credentials_have_access_token(&credentials) {
        return Err(UsageHttpError::AuthRequired);
    }
    let config_path = claude_account_config_path(&home, claude_config_dir_is_explicit(resolved));
    let config = match read_bounded_string(&config_path, MAX_CLAUDE_ACCOUNT_CONFIG_BYTES) {
        Ok(body) => body,
        Err(_) => return Err(UsageHttpError::AuthRequired),
    };
    Ok(Some(
        claude_account_fingerprint_material_from_config(&config)
            .map(|material| fingerprint_account_material(&material))
            .ok_or(UsageHttpError::AuthRequired)?,
    ))
}

/// In-memory Claude credential context for probe stability checks (never persisted).
pub fn resolve_claude_credential_context(
    resolved: &ResolvedProviderLaunchConfig,
) -> Result<String, UsageHttpError> {
    let home = claude_home_from_resolved(resolved)?;
    let credentials =
        match read_bounded_string(&home.join(".credentials.json"), MAX_CREDENTIAL_FILE_BYTES) {
            Ok(body) => body,
            Err(_) => return Err(UsageHttpError::AuthRequired),
        };
    claude_credential_context_material(&credentials)
        .map(|material| fingerprint_account_material(&material))
        .ok_or(UsageHttpError::AuthRequired)
}

pub fn resolve_cursor_account_fingerprint(
    resolved: &ResolvedProviderLaunchConfig,
) -> Result<Option<String>, UsageHttpError> {
    if effective_env_has_api_key(&resolved.discovery.child_environment)
        || effective_env_has_api_key(&resolved.environment)
    {
        return Err(UsageHttpError::UnsupportedContext("api key context".into()));
    }
    reject_custom_cursor_context(resolved)?;
    let auth_path = stock_cursor_auth_path().ok_or(UsageHttpError::AuthRequired)?;
    let auth = read_bounded_string(&auth_path, MAX_CREDENTIAL_FILE_BYTES)
        .map_err(|_| UsageHttpError::AuthRequired)?;
    Ok(Some(
        cursor_account_fingerprint_material(&auth)
            .map(|material| fingerprint_account_material(&material))
            .ok_or(UsageHttpError::AuthRequired)?,
    ))
}

/// In-memory Cursor credential context (access-token digest only; never persisted).
pub fn resolve_cursor_credential_context(
    resolved: &ResolvedProviderLaunchConfig,
) -> Result<String, UsageHttpError> {
    reject_custom_cursor_context(resolved)?;
    let auth_path = stock_cursor_auth_path().ok_or(UsageHttpError::AuthRequired)?;
    let auth = read_bounded_string(&auth_path, MAX_CREDENTIAL_FILE_BYTES)
        .map_err(|_| UsageHttpError::AuthRequired)?;
    let token = extract_cursor_access_token(&auth).ok_or(UsageHttpError::AuthRequired)?;
    Ok(fingerprint_account_material(&format!(
        "cursor-credctx:{token}"
    )))
}

/// Safe Codex account identity from the exact selected CODEX_HOME `auth.json`.
/// Canonical scope is `codex-id:{tokens.account_id}` only.
pub fn resolve_codex_account_fingerprint(
    resolved: &ResolvedProviderLaunchConfig,
) -> Result<Option<String>, UsageHttpError> {
    if effective_env_has_api_key(&resolved.discovery.child_environment)
        || effective_env_has_api_key(&resolved.environment)
    {
        return Err(UsageHttpError::UnsupportedContext("api key context".into()));
    }
    let home = codex_home_from_resolved(resolved).ok_or(UsageHttpError::AuthRequired)?;
    let auth = match read_bounded_string(&home.join("auth.json"), MAX_CREDENTIAL_FILE_BYTES) {
        Ok(body) => body,
        Err(_) => return Err(UsageHttpError::AuthRequired),
    };
    match codex_account_fingerprint_material(&auth) {
        Some(material) => Ok(Some(fingerprint_account_material(&material))),
        None => Err(UsageHttpError::AuthRequired),
    }
}

fn stock_cursor_auth_path() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let appdata = std::env::var_os("APPDATA")?;
        Some(PathBuf::from(appdata).join("Cursor").join("auth.json"))
    }
    #[cfg(not(windows))]
    {
        dirs::home_dir().map(|h| h.join(".cursor").join("auth.json"))
    }
}

struct HttpBody {
    body: String,
}

fn http_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(USAGE_HTTP_TIMEOUT))
        .max_redirects(0)
        .proxy(None)
        .http_status_as_error(false)
        .build()
        .into()
}

fn http_get_allowlisted(url: &str, headers: &[(&str, &str)]) -> Result<HttpBody, UsageHttpError> {
    if url != CLAUDE_USAGE_URL {
        return Err(UsageHttpError::Failed("endpoint not allowlisted".into()));
    }
    let agent = http_agent();
    let mut request = agent.get(url);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    match request.call() {
        Ok(response) => read_response(response),
        Err(error) => Err(UsageHttpError::Failed(error.to_string())),
    }
}

fn http_post_allowlisted(
    url: &str,
    body: &str,
    headers: &[(&str, &str)],
) -> Result<HttpBody, UsageHttpError> {
    if url != CURSOR_USAGE_URL {
        return Err(UsageHttpError::Failed("endpoint not allowlisted".into()));
    }
    let agent = http_agent();
    let mut request = agent.post(url);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    match request.send(body) {
        Ok(response) => read_response(response),
        Err(error) => Err(UsageHttpError::Failed(error.to_string())),
    }
}

fn read_response(response: ureq::http::Response<ureq::Body>) -> Result<HttpBody, UsageHttpError> {
    let status = response.status().as_u16();
    let retry_after_secs = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .map(parse_retry_after_secs);
    if status == 429 {
        return Err(UsageHttpError::Backoff {
            retry_after_secs: retry_after_secs
                .flatten()
                .unwrap_or(DEFAULT_USAGE_BACKOFF_SECS)
                .clamp(1, MAX_USAGE_BACKOFF_SECS),
        });
    }
    if status == 401 || status == 403 {
        return Err(UsageHttpError::AuthRequired);
    }
    if !(200..300).contains(&status) {
        return Err(UsageHttpError::Failed(format!("http {status}")));
    }
    let mut body = response.into_body();
    let bytes = body
        .with_config()
        .limit(MAX_USAGE_RESPONSE_BYTES)
        .read_to_vec()
        .map_err(|e| UsageHttpError::Failed(format!("read body: {e}")))?;
    let body = String::from_utf8(bytes).map_err(|_| UsageHttpError::Failed("body utf8".into()))?;
    Ok(HttpBody { body })
}

/// Parse Retry-After as delta-seconds or HTTP-date; `None` means missing/unparseable.
pub fn parse_retry_after_secs(raw: &str) -> Option<u64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(delta) = trimmed.parse::<u64>() {
        return Some(delta.clamp(1, MAX_USAGE_BACKOFF_SECS));
    }
    // HTTP-date (RFC 9110 / RFC 5322).
    let parsed =
        time::OffsetDateTime::parse(trimmed, &time::format_description::well_known::Rfc2822)
            .ok()
            .or_else(|| {
                // Some servers omit the comma: try a few common layouts via PrimitiveDateTime is hard;
                // accept only well-formed Rfc2822 here.
                None
            })?;
    let now = time::OffsetDateTime::now_utc();
    let secs = (parsed - now).whole_seconds();
    if secs <= 0 {
        Some(1)
    } else {
        Some((secs as u64).clamp(1, MAX_USAGE_BACKOFF_SECS))
    }
}

pub fn read_bounded_string(path: &Path, max_bytes: u64) -> Result<String, std::io::Error> {
    let meta = std::fs::metadata(path)?;
    if !meta.is_file() || meta.len() > max_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "file too large or not a file",
        ));
    }
    let mut file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "file too large",
        ));
    }
    String::from_utf8(bytes)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "file not utf8"))
}

/// Read Codex models_cache.json from the exact selected CODEX_HOME only.
pub fn read_codex_models_cache_file(home: &Path) -> Option<String> {
    read_bounded_string(&home.join("models_cache.json"), MAX_MODELS_CACHE_FILE_BYTES).ok()
}

pub fn codex_home_for_usage(resolved: &ResolvedProviderLaunchConfig) -> Option<PathBuf> {
    codex_home_from_resolved(resolved)
}

pub fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod retry_after_tests {
    use super::*;

    #[test]
    fn retry_after_delta_seconds() {
        assert_eq!(parse_retry_after_secs("120"), Some(120));
        assert_eq!(parse_retry_after_secs("0"), Some(1));
    }

    #[test]
    fn retry_after_http_date_future() {
        let future = time::OffsetDateTime::now_utc() + time::Duration::seconds(90);
        let formatted = future
            .format(&time::format_description::well_known::Rfc2822)
            .unwrap();
        let parsed = parse_retry_after_secs(&formatted).unwrap();
        assert!((80..=100).contains(&parsed));
    }

    #[test]
    fn claude_rejects_cursor_usage_url_as_endpoint() {
        use crate::providers::settings::model::{BuiltinProviderDriver, ProviderInstanceConfig};
        use crate::providers::settings::resolve_launch_config;
        let mut instance = ProviderInstanceConfig::builtin_default(BuiltinProviderDriver::Claude);
        instance.api_endpoint = Some(CURSOR_USAGE_URL.into());
        let resolved = resolve_launch_config(&instance, b"test", None).unwrap();
        assert!(matches!(
            query_claude_usage(&resolved, 1),
            Err(UsageHttpError::UnsupportedContext(_))
        ));
    }

    #[test]
    fn anthropic_origin_rejects_domain_prefix_evil() {
        assert!(anthropic_origin_allowed("https://api.anthropic.com"));
        assert!(anthropic_origin_allowed("https://api.anthropic.com/"));
        assert!(!anthropic_origin_allowed("https://api.anthropic.com.evil"));
        assert!(!anthropic_origin_allowed(
            "https://api.anthropic.com.evil/v1"
        ));
        assert!(!anthropic_origin_allowed("http://api.anthropic.com"));
        assert!(!anthropic_origin_allowed(
            "https://evil.example/api.anthropic.com"
        ));
    }
}
