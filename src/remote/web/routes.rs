//! Shared host-side task routes for resume and notification links.
//! Mirrors web/src/app/router.ts: the task key is one opaque encoded segment.

use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};

use crate::remote::presentation::StableSessionKey;

pub(crate) const TASKS_PATH: &str = "/tasks";
const MAX_KEY_BYTES: usize = 1024;
// JavaScript encodeURIComponent's unescaped characters.
const COMPONENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'!')
    .remove(b'~')
    .remove(b'*')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')');

fn stable_key(value: &str) -> Option<StableSessionKey> {
    if value.len() > MAX_KEY_BYTES || value.chars().any(char::is_control) {
        return None;
    }
    match value.split_once(':') {
        Some(("tab", id)) if !id.is_empty() => Some(StableSessionKey::from_tab(id)),
        Some(("server", id)) if !id.is_empty() => Some(StableSessionKey::from_server(id)),
        _ => None,
    }
}

pub(crate) fn task_path(key: &StableSessionKey) -> String {
    if stable_key(key.as_str()).is_none() {
        return TASKS_PATH.to_string();
    }
    format!(
        "{TASKS_PATH}/{}",
        utf8_percent_encode(key.as_str(), COMPONENT)
    )
}

pub(crate) fn decode_component(value: &str) -> Option<String> {
    if value.is_empty() || value.len() > MAX_KEY_BYTES * 3 {
        return None;
    }
    // percent_decode_str deliberately tolerates malformed escapes; routes must
    // match decodeURIComponent, which rejects them and invalid UTF-8.
    let mut bytes = value.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%'
            && !(bytes.next().is_some_and(|b| b.is_ascii_hexdigit())
                && bytes.next().is_some_and(|b| b.is_ascii_hexdigit()))
        {
            return None;
        }
    }
    let decoded = percent_decode_str(value).decode_utf8().ok()?;
    (decoded.len() <= MAX_KEY_BYTES && !decoded.chars().any(char::is_control))
        .then(|| decoded.into_owned())
}

/// Return a canonical route and its key, never an execution target. Callers
/// still require the requested key and current authorized projection to match.
pub(crate) fn parse_task_path(path: &str) -> Option<(String, StableSessionKey)> {
    let rest = path.strip_prefix("/tasks/")?;
    let mut parts = rest.split('/');
    let decoded = decode_component(parts.next()?)?;
    let key = stable_key(&decoded)?;
    let resource = match parts.next() {
        None | Some("chat") => None,
        Some(resource @ ("terminal" | "browser")) => Some(resource),
        _ => return None,
    };
    if parts.next().is_some() {
        return None;
    }
    let mut canonical = task_path(&key);
    if let Some(resource) = resource {
        canonical.push('/');
        canonical.push_str(resource);
    }
    Some((canonical, key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_match_browser_literals_and_keep_opaque_ids_in_one_segment() {
        for (path, key, canonical) in [
            ("/tasks/tab%3Aabc", "tab:abc", "/tasks/tab%3Aabc"),
            (
                "/tasks/server%3Adev%2Fweb%20%231",
                "server:dev/web #1",
                "/tasks/server%3Adev%2Fweb%20%231",
            ),
            (
                "/tasks/tab%3Aabc/terminal",
                "tab:abc",
                "/tasks/tab%3Aabc/terminal",
            ),
            ("/tasks/tab%3aabc/chat", "tab:abc", "/tasks/tab%3Aabc"),
            ("/tasks/tab%3Ax%2By", "tab:x+y", "/tasks/tab%3Ax%2By"),
        ] {
            let (actual, parsed) = parse_task_path(path).expect("browser route");
            assert_eq!(actual, canonical);
            assert_eq!(parsed.as_str(), key);
        }
    }

    #[test]
    fn malformed_routes_do_not_attach_a_task() {
        for route in [
            "/tasks/%E0%A4%A",
            "/tasks/tab%3A%FF",
            "/tasks/tab%3Ax%00",
            "/tasks/tab%3A",
            "/tasks/pty%3Ax",
            "/tasks/tab%3Ax/files",
            "/tasks/tab%3Ax/terminal/extra",
            "/tasks/tab%3Ax%2",
            "/tasks/tab%3Ax%ZZ",
        ] {
            assert!(parse_task_path(route).is_none(), "{route}");
        }
        assert!(parse_task_path(&format!("/tasks/tab%3A{}", "x".repeat(1024))).is_none());
    }
}
