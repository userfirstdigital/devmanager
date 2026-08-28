use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;
use std::sync::Arc;

use super::fleet_publication::{self, FLEET_META_NAME};

pub async fn native_index_handler(
    State(state): State<Arc<super::WebState>>,
    headers: HeaderMap,
) -> Response {
    serve_native_embedded("index.html", &state, &headers).await
}

pub async fn native_static_handler(
    State(state): State<Arc<super::WebState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    serve_native_embedded(uri.path().trim_start_matches('/'), &state, &headers).await
}

async fn serve_native_embedded(
    path: &str,
    state: &super::WebState,
    headers: &HeaderMap,
) -> Response {
    let requested = if path.is_empty() { "index.html" } else { path };
    if !is_reserved_server_path(requested)
        && (requested == "index.html"
            || (WebAssets::get(requested).is_none() && is_spa_route(requested)))
    {
        if let Some(index) = WebAssets::get("index.html") {
            // Data, not executable inline script. Trust I/O only for authenticated HTML.
            let authenticated = super::validate_authenticated_request(state, headers).is_ok();
            let (marker, fleet) = if authenticated {
                match fleet_publication::load_authenticated_fleet(state, headers).await {
                    Ok(publication) => (publication.self_marker_json, Some(publication.roster)),
                    Err(_) => {
                        // Fail closed: never pair fleet meta with a stale/unavailable self marker.
                        (r#"{"transport":"connect","unavailable":true}"#.into(), None)
                    }
                }
            } else {
                let marker = state
                    .connect_startup
                    .as_ref()
                    .and_then(|startup| startup.web_publication().marker_json())
                    .or_else(|| {
                        #[cfg(test)]
                        {
                            state
                                .fleet_test_publication
                                .as_ref()
                                .and_then(|publication| publication.marker_json())
                        }
                        #[cfg(not(test))]
                        {
                            None
                        }
                    })
                    .unwrap_or_else(|| r#"{"transport":"connect","unavailable":true}"#.into());
                (marker, None)
            };
            let html = inject_connect_shell_metadata(
                &String::from_utf8_lossy(&index.data),
                &marker,
                fleet.as_ref().map(|roster| roster.json.as_str()),
            );
            let mut response = html.into_response();
            let cache = if authenticated {
                "private, no-store"
            } else {
                "no-cache"
            };
            apply_static_headers(&mut response, "text/html; charset=utf-8", cache);
            if let Some(roster) = fleet {
                response.extensions_mut().insert(roster);
            }
            return response;
        }
    }
    // Static JS/CSS/wasm: never spawn trust-store work.
    serve_embedded(requested)
}

fn inject_connect_shell_metadata(
    html: &str,
    self_marker: &str,
    fleet_json: Option<&str>,
) -> String {
    let mut metas = inject_meta_tag("devmanager-connect", self_marker);
    if let Some(fleet) = fleet_json {
        metas.push_str(&inject_meta_tag(FLEET_META_NAME, fleet));
    }
    html.replacen("</head>", &format!("{metas}</head>"), 1)
}

fn inject_meta_tag(name: &str, content: &str) -> String {
    let attribute = escape_meta_attribute(content);
    format!("<meta name=\"{name}\" content=\"{attribute}\">")
}

fn escape_meta_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[derive(RustEmbed)]
#[folder = "web/bundle/"]
pub struct WebAssets;

pub async fn static_handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    serve_embedded(path)
}

pub async fn index_handler() -> Response {
    serve_embedded("index.html")
}

fn serve_embedded(path: &str) -> Response {
    let requested = if path.is_empty() { "index.html" } else { path };

    if is_reserved_server_path(requested) {
        return not_found_response();
    }

    if let Some(content) = WebAssets::get(requested) {
        let mime = mime_guess::from_path(requested).first_or_octet_stream();
        let mut response = content.data.into_owned().into_response();
        apply_static_headers(&mut response, mime.as_ref(), cache_control_for(requested));
        return response;
    }

    if is_spa_route(requested) {
        if let Some(content) = WebAssets::get("index.html") {
            let mut response = content.data.into_owned().into_response();
            apply_static_headers(&mut response, "text/html; charset=utf-8", "no-cache");
            return response;
        }
    }

    not_found_response()
}

fn is_reserved_server_path(path: &str) -> bool {
    path == "api" || path.starts_with("api/") || path == "pair" || path.starts_with("pair/")
}

fn is_spa_route(path: &str) -> bool {
    !path.starts_with("assets/") && !path.starts_with("icons/") && !path.contains('.')
}

fn cache_control_for(path: &str) -> &'static str {
    if is_hashed_asset(path) {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    }
}

fn is_hashed_asset(path: &str) -> bool {
    if !path.starts_with("assets/") {
        return false;
    }
    let filename = path.rsplit('/').next().unwrap_or(path);
    let stem = filename.rsplit_once('.').map_or(filename, |(stem, _)| stem);
    let bytes = stem.as_bytes();
    bytes.len() > 9
        && bytes[bytes.len() - 9] == b'-'
        && bytes[bytes.len() - 8..]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_' || *byte == b'-')
}

fn apply_static_headers(response: &mut Response, content_type: &str, cache_control: &'static str) {
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(content_type).expect("valid embedded MIME type"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache_control),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        content_security_policy(None),
    );
}

pub(crate) fn content_security_policy(websocket_authority: Option<&str>) -> HeaderValue {
    fleet_publication::content_security_policy_with_fleet(websocket_authority, None)
        .unwrap_or_else(|_| HeaderValue::from_static("default-src 'self'; frame-ancestors 'none'"))
}

fn not_found_response() -> Response {
    let mut response = (StatusCode::NOT_FOUND, "not found").into_response();
    apply_static_headers(&mut response, "text/plain; charset=utf-8", "no-cache");
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[test]
    fn connect_metadata_is_inert_escaped_data_before_app_bootstrap() {
        let html = inject_connect_shell_metadata(
            "<head></head><body><script src=\"app.js\"></script></body>",
            r#"{"transport":"connect","value":"</head><script>bad</script>&"}"#,
            Some(r#"{"version":1,"hosts":[{"origin":"https://b.example"}]}"#),
        );
        assert!(html.contains("name=\"devmanager-connect\""));
        assert!(html.contains("name=\"devmanager-connect-fleet\""));
        assert!(html.contains("&lt;/head&gt;&lt;script&gt;bad&lt;/script&gt;&amp;"));
        assert_eq!(html.matches("<script").count(), 1);
    }

    #[test]
    fn unauthenticated_shell_never_injects_fleet_meta() {
        let html =
            inject_connect_shell_metadata("<head></head>", r#"{"transport":"connect"}"#, None);
        assert!(html.contains("devmanager-connect\""));
        assert!(!html.contains("devmanager-connect-fleet"));
    }

    fn header_value<'a>(response: &'a Response, name: &str) -> &'a str {
        response
            .headers()
            .get(name)
            .unwrap_or_else(|| panic!("missing {name} header"))
            .to_str()
            .expect("header is valid text")
    }

    #[tokio::test]
    async fn spa_deep_links_fall_back_to_the_embedded_index() {
        let response = static_handler(Uri::from_static("/session/tab/test")).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            header_value(&response, "content-type"),
            "text/html; charset=utf-8"
        );
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body");
        assert!(String::from_utf8_lossy(&body).contains("id=\"root\""));
    }

    #[tokio::test]
    async fn unknown_api_routes_never_fall_through_to_the_spa() {
        let response = static_handler(Uri::from_static("/api/not-a-real-route")).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body");
        assert_eq!(body.as_ref(), b"not found");
    }

    #[tokio::test]
    async fn unknown_pair_routes_never_fall_through_to_the_spa() {
        let response = static_handler(Uri::from_static("/pair/unknown")).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body");
        assert_eq!(body.as_ref(), b"not found");
    }

    #[test]
    fn hashed_assets_are_immutable() {
        let asset = WebAssets::iter()
            .find(|path| is_hashed_asset(path) && (path.ends_with(".js") || path.ends_with(".css")))
            .expect("a hashed Vite asset");
        let response = serve_embedded(asset.as_ref());

        assert_eq!(
            header_value(&response, "cache-control"),
            "public, max-age=31536000, immutable"
        );
    }

    #[test]
    fn vite_url_safe_hashes_may_contain_dashes() {
        assert!(is_hashed_asset("assets/index-cZ6-HVns.js"));
    }

    #[test]
    fn mutable_shell_resources_require_revalidation() {
        for path in [
            "index.html",
            "manifest.webmanifest",
            "sw.js",
            "assets/wasm/connect_crypto.js",
            "assets/wasm/connect_crypto_bg.wasm",
        ] {
            let response = serve_embedded(path);
            assert_eq!(
                header_value(&response, "cache-control"),
                "no-cache",
                "{path} must revalidate"
            );
        }
    }
}
