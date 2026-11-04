//! The web UI, embedded at build time and served from the admin listener as everything the API does not claim.

use axum::body::Body;
use axum::http::header::{
    ALLOW, CACHE_CONTROL, CONTENT_LENGTH, CONTENT_SECURITY_POLICY, CONTENT_TYPE, REFERRER_POLICY,
    X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderValue, Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "../../web/dist"]
struct Assets;

/// Recharts sets inline styles on the SVG it draws, and the API reference page injects styles of its own, so style-src
/// keeps 'unsafe-inline'. No script is inline, because the theme bootstrap lives in `public/theme.js`.
/// `connect-src 'self'` covers the console's own fetches. The app has no base tag, no form post, and no plugin, so
/// those are closed.
const CSP: &str = "base-uri 'none'; connect-src 'self'; default-src 'self'; form-action 'self'; frame-ancestors 'none'; img-src 'self' data:; object-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'";

/// A hashed asset never changes under its name, so a year costs nothing.
const IMMUTABLE: &str = "public, max-age=31536000, immutable";

/// Everything else either names the hashes or talks about them, and a cache holding any of it would answer for
/// a UI this binary no longer carries.
const NO_STORE: &str = "no-store";

/// The API reference page's directory: the page at it and at its index, a JSON 404 for anything else under it.
pub const REFERENCE_PATH: &str = "/api-reference";

/// The comment the placeholder page carries and a built bundle never does, in one file the build script includes too.
#[path = "../placeholder_mark.rs"]
mod placeholder_mark;

use placeholder_mark::PLACEHOLDER_MARK;

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
        return finish(json_not_found(), NO_STORE);
    }
    let method = request.method();
    if method != Method::GET && method != Method::HEAD {
        return finish(method_not_allowed(), NO_STORE);
    }
    let head = method == Method::HEAD;
    // The reference page answers at its directory and its index, and nothing else under the directory lands on
    // the console's shell: a typo there is a JSON 404, as a stale asset path is. The bare directory answers the
    // page rather than redirecting, since the page's URLs are absolute and nothing resolves against its location.
    match path.strip_prefix(REFERENCE_PATH) {
        Some("" | "/" | "/index.html") => {
            return page(&format!("{REFERENCE_PATH}/index.html"), head)
        }
        Some(rest) if rest.starts_with('/') => return finish(json_not_found(), NO_STORE),
        _ => {}
    }
    if let Some((bytes, mime)) = lookup(&path) {
        let cache = if path.starts_with("/assets/") {
            IMMUTABLE
        } else {
            NO_STORE
        };
        return finish(file(bytes, &mime, head), cache);
    }
    // The shell stands in for the app's own routes, which carry no extension. A path that names a file the
    // bundle does not carry is a 404 instead, so a stale script or style tag fails loudly rather than parsing
    // the shell as JavaScript; everything under `/assets/` names a hashed file whether it looks like one or not.
    // A route parameter that can hold a dot, a model name or a host, would land here rather than on the shell,
    // so adding one means revisiting this rule.
    let last_segment = path.rsplit('/').next().unwrap_or_default();
    if path.starts_with("/assets/") || last_segment.contains('.') {
        return finish(json_not_found(), NO_STORE);
    }
    page("index.html", head)
}

fn page(path: &str, head: bool) -> Response {
    match lookup(path) {
        Some((bytes, mime)) => finish(file(bytes, &mime, head), NO_STORE),
        None => finish(json_not_found(), NO_STORE),
    }
}

/// A `HEAD` answer carries the headers a `GET` would and drops the body.
fn file(bytes: Vec<u8>, mime: &str, head: bool) -> Response {
    let length = bytes.len() as u64;
    let mut response = if head { Vec::new() } else { bytes }.into_response();
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_str(mime).unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    if head {
        // The dropped body leaves nothing to count, and a caller sizing the file with HEAD reads this.
        response
            .headers_mut()
            .insert(CONTENT_LENGTH, HeaderValue::from(length));
    }
    response
}

fn json_not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        [(CONTENT_TYPE, "application/json")],
        r#"{"error":"not found"}"#,
    )
        .into_response()
}

/// RFC 9110 makes `Allow` a MUST on a 405.
fn method_not_allowed() -> Response {
    let mut response = StatusCode::METHOD_NOT_ALLOWED.into_response();
    response
        .headers_mut()
        .insert(ALLOW, HeaderValue::from_static("GET, HEAD"));
    response
}

/// The cache policy and the security headers of every answer in one place, so no branch can leave one off.
fn finish(mut response: Response, cache: &'static str) -> Response {
    let headers = response.headers_mut();
    headers.insert(CACHE_CONTROL, HeaderValue::from_static(cache));
    headers.insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    response
}

#[cfg(test)]
#[path = "../build_support.rs"]
mod build_support;
