use axum::http::{header, StatusCode, Uri};
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

    if let Some(content) = WebAssets::get(requested) {
        let mime = mime_guess::from_path(requested).first_or_octet_stream();
        return (
            [(header::CONTENT_TYPE, mime.as_ref().to_string())],
            content.data.into_owned(),
        )
            .into_response();
    }

    // SPA fallback: any non-asset path (like /foo) falls back to index.html so
    // client-side routing works. Real asset files under /assets/ still 404 if
    // missing.
    if !requested.starts_with("assets/") {
        if let Some(content) = WebAssets::get("index.html") {
            return (
                [(header::CONTENT_TYPE, "text/html; charset=utf-8".to_string())],
                content.data.into_owned(),
            )
                .into_response();
        }
    }

    (StatusCode::NOT_FOUND, "not found").into_response()
}
