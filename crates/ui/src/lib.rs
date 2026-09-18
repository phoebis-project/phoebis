//! The web UI, embedded at build time and served from the admin listener as everything the API does not claim.

use axum::body::Body;
use axum::http::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, REFERRER_POLICY, X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderValue, Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "../../web/dist"]
struct Assets;

/// Recharts sets inline styles on the SVG it draws, so style-src keeps 'unsafe-inline'; no script is inline,
/// because the theme bootstrap lives in `public/theme.js`.
const CSP: &str = "default-src 'self'; connect-src 'self' ws: wss:; img-src 'self' data:; style-src 'self' 'unsafe-inline'; script-src 'self'; frame-ancestors 'none'";

/// The comment the placeholder page carries and a built bundle never does; the build script reads it too.
const PLACEHOLDER_MARK: &str = "<!-- phoebis-ui placeholder -->";

pub fn router() -> Router {
    Router::new().fallback(fallback)
}

/// Whether the embedded shell is the placeholder page rather than a built bundle.
pub fn is_placeholder() -> bool {
    Assets::get("index.html")
        .is_some_and(|f| std::str::from_utf8(&f.data).is_ok_and(|s| s.contains(PLACEHOLDER_MARK)))
}

pub fn asset_paths() -> Vec<String> {
    Assets::iter().map(|p| p.to_string()).collect()
}

/// The embedded file at a request path, or `None`. The derive refuses a path that leaves the folder, so `..`
/// never reads past the bundle.
pub fn lookup(path: &str) -> Option<(Vec<u8>, String)> {
    let path = path.trim_start_matches('/');
    if path.is_empty() || path.split('/').any(|seg| seg == "..") {
        return None;
    }
    let file = Assets::get(path)?;
    let mime = mime_guess::from_path(path)
        .first_or_octet_stream()
        .to_string();
    Some((file.data.into_owned(), mime))
}

async fn fallback(request: Request<Body>) -> Response {
    let path = request.uri().path().to_string();
    if path.starts_with("/admin/") || path == "/admin" || path == "/metrics" {
        return finish(json_not_found());
    }
    if request.method() != Method::GET && request.method() != Method::HEAD {
        return finish(StatusCode::METHOD_NOT_ALLOWED.into_response());
    }
    if let Some((bytes, mime)) = lookup(&path) {
        let immutable = path.starts_with("/assets/");
        return finish(file(bytes, &mime, immutable));
    }
    // The shell stands in for the app's own routes, which carry no extension. A path that names a file the
    // bundle does not carry is a 404 instead, so a stale script or style tag fails loudly rather than parsing
    // the shell as JavaScript; everything under `/assets/` names a hashed file whether it looks like one or not.
    let last_segment = path.rsplit('/').next().unwrap_or_default();
    if path.starts_with("/assets/") || last_segment.contains('.') {
        return finish(json_not_found());
    }
    match lookup("index.html") {
        Some((bytes, mime)) => finish(file(bytes, &mime, false)),
        None => finish(json_not_found()),
    }
}

fn file(bytes: Vec<u8>, mime: &str, immutable: bool) -> Response {
    let mut response = bytes.into_response();
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_str(mime).unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    // A hashed asset never changes under its name; the shell names the hashes, so it must not be cached.
    response.headers_mut().insert(
        CACHE_CONTROL,
        HeaderValue::from_static(if immutable {
            "public, max-age=31536000, immutable"
        } else {
            "no-store"
        }),
    );
    response
}

fn json_not_found() -> Response {
    let mut response = (
        StatusCode::NOT_FOUND,
        [(CONTENT_TYPE, "application/json")],
        r#"{"error":"not found"}"#,
    )
        .into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn finish(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    response
}
