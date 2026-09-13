//! Bounded per-instance provider health probes and Cursor about parsing.

use serde_json::Value as JsonValue;

use crate::providers::adapter::ProviderProbeKind;
use crate::providers::settings::health::{
    apply_probe_outcome, ProviderHealthRow, ProviderHealthStatus,
};
use crate::providers::settings::model::ProviderDriverKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorAboutFacts {
    pub cli_version: Option<String>,
    pub user_email: Option<String>,
    pub subscription_tier: Option<String>,
    pub auth: CursorAboutAuth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorAboutAuth {
    Authenticated,
    Unauthenticated,
    Unknown,
}

fn unknown_cursor_about() -> CursorAboutFacts {
    CursorAboutFacts {
        cli_version: None,
        user_email: None,
        subscription_tier: None,
        auth: CursorAboutAuth::Unknown,
    }
}

/// Parse Cursor `about --format json`, falling back to plain text labels.
/// Trusted auth and health prefer the strict helpers below; this remains for
/// mixed stdout that may be either shape.
pub fn parse_cursor_about_json(bytes: &[u8]) -> CursorAboutFacts {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return unknown_cursor_about();
    };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return unknown_cursor_about();
    }
    if let Ok(value) = serde_json::from_str::<JsonValue>(trimmed) {
        return parse_cursor_about_value(&value);
    }
    parse_cursor_about_plain(trimmed)
}

/// Strict JSON-only Cursor about parser. Non-JSON stdout is Unknown (never plain).
pub fn parse_cursor_about_strict_json(bytes: &[u8]) -> CursorAboutFacts {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return unknown_cursor_about();
    };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return unknown_cursor_about();
    }
    match serde_json::from_str::<JsonValue>(trimmed) {
        Ok(value) => parse_cursor_about_value(&value),
        Err(_) => unknown_cursor_about(),
    }
}

/// Plain `about` fallback parser (used only after unsupported-format diagnostic).
pub fn parse_cursor_about_plain_bytes(bytes: &[u8]) -> CursorAboutFacts {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return unknown_cursor_about();
    };
    parse_cursor_about_plain(text.trim())
}

fn parse_cursor_about_value(value: &JsonValue) -> CursorAboutFacts {
    let cli_version = bounded_field(value.get("cliVersion").and_then(JsonValue::as_str));
    let mut user_email = bounded_field(value.get("userEmail").and_then(JsonValue::as_str));
    let subscription_tier =
        bounded_field(value.get("subscriptionTier").and_then(JsonValue::as_str));
    let flags: Vec<_> = ["authenticated", "loggedIn", "logged_in"]
        .iter()
        .filter_map(|key| value.get(*key))
        .collect();
    let explicitly_false = flags.iter().any(|flag| flag.as_bool() == Some(false));
    let explicitly_true =
        !flags.is_empty() && flags.iter().all(|flag| flag.as_bool() == Some(true));
    let auth = if value.get("userEmail").is_some_and(JsonValue::is_null)
        || user_email.as_deref().is_some_and(is_logged_out)
        || explicitly_false
    {
        // Negative auth fields override any positive email/tier signal.
        user_email = None;
        CursorAboutAuth::Unauthenticated
    } else if explicitly_true || (flags.is_empty() && user_email.as_deref().is_some_and(is_email)) {
        CursorAboutAuth::Authenticated
    } else {
        CursorAboutAuth::Unknown
    };
    CursorAboutFacts {
        cli_version,
        user_email,
        subscription_tier,
        auth,
    }
}

fn parse_cursor_about_plain(text: &str) -> CursorAboutFacts {
    let field = |label: &str| {
        text.lines().find_map(|line| {
            line.trim()
                .strip_prefix(label)
                .and_then(|value| bounded_field(Some(value.trim_start_matches([' ', '\t', ':']))))
        })
    };
    let cli_version = field("CLI Version");
    let mut user_email = field("User Email");
    let subscription_tier = field("Subscription Tier");
    let auth = if is_logged_out(text) {
        user_email = None;
        CursorAboutAuth::Unauthenticated
    } else if user_email.as_deref().is_some_and(is_email) {
        CursorAboutAuth::Authenticated
    } else {
        CursorAboutAuth::Unknown
    };
    CursorAboutFacts {
        cli_version,
        user_email,
        subscription_tier,
        auth,
    }
}

fn bounded_field(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|text| !text.is_empty() && text.len() <= 512)
        .map(str::to_owned)
}

fn is_logged_out(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "not logged in",
        "login required",
        "authentication required",
        "not authenticated",
        "logged out",
        "signed out",
        "not signed in",
        "unauthenticated",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn is_email(value: &str) -> bool {
    !value
        .chars()
        .any(|ch| ch.is_whitespace() || ch.is_control())
        && value.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty() && !domain.is_empty() && !domain.contains('@')
        })
}

pub fn cursor_about_probe_kinds() -> [ProviderProbeKind; 2] {
    [
        ProviderProbeKind::CursorAboutJson,
        ProviderProbeKind::CursorAboutPlain,
    ]
}

pub fn apply_cursor_about_to_row(row: &mut ProviderHealthRow, facts: &CursorAboutFacts) {
    let ok = matches!(facts.auth, CursorAboutAuth::Authenticated);
    let error = match facts.auth {
        CursorAboutAuth::Unauthenticated => Some("Not signed in".into()),
        CursorAboutAuth::Unknown => None,
        CursorAboutAuth::Authenticated => None,
    };
    apply_probe_outcome(
        row,
        facts.cli_version.clone(),
        facts.user_email.clone(),
        facts.subscription_tier.clone(),
        ok,
        error,
    );
    if matches!(facts.auth, CursorAboutAuth::Unknown) {
        row.status = ProviderHealthStatus::Unknown;
    } else if matches!(facts.auth, CursorAboutAuth::Unauthenticated) {
        row.status = ProviderHealthStatus::Degraded;
    }
}

pub fn stub_health_row(instance_id: &str, driver: ProviderDriverKind) -> ProviderHealthRow {
    ProviderHealthRow::stub(instance_id, driver)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_about_json_parses_account_fields() {
        let json =
            br#"{"cliVersion":"1.2.3","userEmail":"a@example.com","subscriptionTier":"pro"}"#;
        let facts = parse_cursor_about_json(json);
        assert_eq!(facts.cli_version.as_deref(), Some("1.2.3"));
        assert_eq!(facts.user_email.as_deref(), Some("a@example.com"));
        assert_eq!(facts.subscription_tier.as_deref(), Some("pro"));
        assert_eq!(facts.auth, CursorAboutAuth::Authenticated);
    }

    #[test]
    fn cursor_about_null_email_is_unauthenticated() {
        let json = br#"{"cliVersion":"1.2.3","userEmail":null,"subscriptionTier":null}"#;
        let facts = parse_cursor_about_json(json);
        assert_eq!(facts.auth, CursorAboutAuth::Unauthenticated);
        assert!(facts.user_email.is_none());
    }

    #[test]
    fn conflicting_cursor_flags_and_negative_email_never_authenticate() {
        for input in [
            br#"{"authenticated":true,"loggedIn":false,"userEmail":"a@example.com"}"#.as_slice(),
            br#"{"userEmail":"not authenticated"}"#.as_slice(),
            br#"{"logged_in":false,"authenticated":true}"#.as_slice(),
        ] {
            assert_eq!(
                parse_cursor_about_strict_json(input).auth,
                CursorAboutAuth::Unauthenticated
            );
        }
        assert_eq!(
            parse_cursor_about_strict_json(br#"{"userEmail":"unknown"}"#).auth,
            CursorAboutAuth::Unknown
        );
        assert_eq!(
            parse_cursor_about_strict_json(
                br#"{"authenticated":"true","userEmail":"a@example.com"}"#
            )
            .auth,
            CursorAboutAuth::Unknown
        );
    }

    #[test]
    fn cursor_json_negative_auth_overrides_positive_email() {
        let json = br#"{"cliVersion":"1.2.3","userEmail":"a@example.com","authenticated":false}"#;
        let facts = parse_cursor_about_strict_json(json);
        assert_eq!(facts.auth, CursorAboutAuth::Unauthenticated);
        assert!(facts.user_email.is_none());
        let logged_in = br#"{"cliVersion":"1.2.3","userEmail":"a@example.com","loggedIn":false}"#;
        assert_eq!(
            parse_cursor_about_strict_json(logged_in).auth,
            CursorAboutAuth::Unauthenticated
        );
    }

    #[test]
    fn cursor_strict_json_does_not_parse_plain_labels() {
        let facts =
            parse_cursor_about_strict_json(b"CLI Version  1.2.3\nUser Email  user@example.com");
        assert_eq!(facts.auth, CursorAboutAuth::Unknown);
        let plain =
            parse_cursor_about_plain_bytes(b"CLI Version  1.2.3\nUser Email  user@example.com");
        assert_eq!(plain.auth, CursorAboutAuth::Authenticated);
    }

    #[test]
    fn cursor_about_login_required_plain() {
        let facts = parse_cursor_about_json(b"Error: login required");
        assert_eq!(facts.auth, CursorAboutAuth::Unauthenticated);
    }

    #[test]
    fn cursor_missing_empty_and_negative_email_are_not_authenticated() {
        for email in ["Not logged in", "login required", "Authentication required"] {
            let bytes =
                serde_json::to_vec(&serde_json::json!({"cliVersion":"1.2.3", "userEmail":email}))
                    .unwrap();
            let facts = parse_cursor_about_json(&bytes);
            assert_eq!(facts.auth, CursorAboutAuth::Unauthenticated);
            assert!(facts.user_email.is_none());
        }
        for bytes in [
            br#"{"cliVersion":"1.2.3"}"#.as_slice(),
            br#"{"cliVersion":"1.2.3","userEmail":" "}"#.as_slice(),
            b"Contact support@example.com for help".as_slice(),
        ] {
            assert_eq!(
                parse_cursor_about_json(bytes).auth,
                CursorAboutAuth::Unknown
            );
        }
        let facts = parse_cursor_about_json(b"CLI Version  1.2.3\nUser Email  user@example.com");
        assert_eq!(facts.auth, CursorAboutAuth::Authenticated);
        assert_eq!(facts.cli_version.as_deref(), Some("1.2.3"));
    }
}
