use axum::http::{header, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

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
        HeaderValue::from_static(
            "default-src 'self'; base-uri 'self'; connect-src 'self' ws: wss:; \
font-src 'self' data:; img-src 'self' data: blob:; manifest-src 'self'; \
object-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
worker-src 'self' blob:; frame-ancestors 'none'; form-action 'self'",
        ),
    );
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
            .find(|path| {
                path.starts_with("assets/") && (path.ends_with(".js") || path.ends_with(".css"))
            })
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
        for path in ["index.html", "manifest.webmanifest", "sw.js"] {
            let response = serve_embedded(path);
            assert_eq!(
                header_value(&response, "cache-control"),
                "no-cache",
                "{path}"
            );
        }
    }

    #[test]
    fn static_responses_apply_security_headers_and_compatible_csp() {
        let response = serve_embedded("index.html");

        assert_eq!(header_value(&response, "x-content-type-options"), "nosniff");
        assert_eq!(header_value(&response, "x-frame-options"), "DENY");
        let csp = header_value(&response, "content-security-policy");
        assert!(csp.contains("connect-src 'self' ws: wss:"));
        assert!(csp.contains("frame-ancestors 'none'"));
        assert!(csp.contains("worker-src 'self'"));
    }
}
