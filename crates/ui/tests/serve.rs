//! The embedded app: hashed assets cached for a year, the shell never cached, every unknown path outside the API
//! reference page's directory the shell, and the admin and metrics paths never shadowed.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::Router;
use http_body_util::BodyExt;
use tower::ServiceExt;

/// Pinned here rather than read from the crate, so a loosened policy fails instead of following along.
const CSP: &str = "base-uri 'none'; connect-src 'self'; default-src 'self'; form-action 'self'; frame-ancestors 'none'; img-src 'self' data:; object-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'";

fn assert_security_headers(headers: &axum::http::HeaderMap, what: &str) {
    assert_eq!(headers["content-security-policy"], CSP, "{what}");
    assert_eq!(headers["referrer-policy"], "no-referrer", "{what}");
    assert_eq!(headers["x-content-type-options"], "nosniff", "{what}");
}

fn app() -> Router {
    Router::new()
        .route("/admin/session", get(|| async { "admin" }))
        .route("/metrics", get(|| async { "metrics" }))
        .merge(phoebis_ui::router())
}

async fn send(method: &str, path: &str) -> (StatusCode, axum::http::HeaderMap, String) {
    let response = app()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = String::from_utf8_lossy(&response.into_body().collect().await.unwrap().to_bytes())
        .into_owned();
    (status, headers, body)
}

#[tokio::test]
async fn the_shell_is_served_for_every_app_path_and_never_cached() {
    for path in [
        "/",
        "/pools",
        "/accounts/business",
        "/requests/req_1",
        "/nope/deeper",
    ] {
        let (status, headers, body) = send("GET", path).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert!(body.contains("<div id=\"root\">"), "{path}: {body}");
        assert_eq!(headers["cache-control"], "no-store", "{path}");
        assert!(
            headers["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/html"),
            "{path}"
        );
        assert_security_headers(&headers, path);
    }
    let (status, _, _) = send("HEAD", "/pools").await;
    assert_eq!(status, StatusCode::OK);
    let (status, headers, _) = send("POST", "/pools").await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(headers["cache-control"], "no-store");
    assert_eq!(headers["allow"], "GET, HEAD");
    assert_security_headers(&headers, "POST /pools");
}

#[tokio::test]
async fn hashed_assets_are_immutable_and_typed_and_a_missing_asset_is_a_404() {
    let asset = phoebis_ui::asset_paths()
        .into_iter()
        .find(|p| p.starts_with("assets/") && p.ends_with(".js"));
    if let Some(asset) = asset {
        let (status, headers, body) = send("GET", &format!("/{asset}")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers["cache-control"],
            "public, max-age=31536000, immutable"
        );
        assert!(headers["content-type"]
            .to_str()
            .unwrap()
            .contains("javascript"));
        assert!(!body.is_empty());
        // A HEAD tells a caller what it would get without sending it: the same headers, the length, no body.
        let (head_status, head_headers, head_body) = send("HEAD", &format!("/{asset}")).await;
        let (bytes, _) = phoebis_ui::lookup(&asset).unwrap();
        assert_eq!(head_status, StatusCode::OK);
        assert_eq!(head_headers["cache-control"], headers["cache-control"]);
        assert_eq!(head_headers["content-type"], headers["content-type"]);
        assert_eq!(head_headers["content-length"], bytes.len().to_string());
        assert!(head_body.is_empty(), "a HEAD answer carries no body");
    } else {
        assert!(
            phoebis_ui::is_placeholder(),
            "a real bundle carries at least one script under assets/"
        );
    }
    let (status, headers, _) = send("GET", "/assets/missing-0000.js").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(headers["cache-control"], "no-store");
    let (status, headers, body) = send("GET", "/theme.js").await;
    if phoebis_ui::is_placeholder() {
        assert_eq!(status, StatusCode::NOT_FOUND);
    } else {
        assert_eq!(
            (status, headers["cache-control"].to_str().unwrap()),
            (StatusCode::OK, "no-store")
        );
        assert!(body.contains("data-theme"));
    }
    let (status, headers, body) = send("GET", "/favicon.svg").await;
    if phoebis_ui::is_placeholder() {
        assert_eq!(status, StatusCode::NOT_FOUND);
    } else {
        assert_eq!(status, StatusCode::OK);
        assert!(headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with("image/svg+xml"));
        assert_eq!(headers["cache-control"], "no-store");
        assert!(
            body.contains("prefers-color-scheme"),
            "the favicon follows the browser's scheme"
        );
        assert_eq!(
            phoebis_ui::lookup("favicon-32.png")
                .map(|(_, mime)| mime)
                .as_deref(),
            Some("image/png"),
            "the PNG fallback is in the bundle and served as a PNG"
        );
    }
}

#[tokio::test]
async fn the_admin_and_metrics_routes_are_never_shadowed() {
    assert_eq!(send("GET", "/admin/session").await.2, "admin");
    assert_eq!(send("GET", "/metrics").await.2, "metrics");
    let (status, headers, body) = send("GET", "/admin/definitely-not-a-route").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(headers["cache-control"], "no-store");
    assert_security_headers(&headers, "/admin/definitely-not-a-route");
    assert!(
        body.contains("not found") && !body.contains("<div"),
        "an unknown admin path is JSON, never the shell"
    );
}

/// The API reference page lives at its directory and its index, never under the console's shell rule, and
/// anything else under the directory is a JSON 404, so a typo there never shows the console.
#[tokio::test]
async fn the_api_reference_page_is_served_at_its_directory_and_nothing_else_under_it() {
    for path in [
        "/api-reference",
        "/api-reference/",
        "/api-reference/index.html",
    ] {
        let (status, headers, body) = send("GET", path).await;
        assert_eq!(headers["cache-control"], "no-store", "{path}");
        assert_security_headers(&headers, path);
        if phoebis_ui::is_placeholder() {
            assert_eq!(
                status,
                StatusCode::NOT_FOUND,
                "{path}: a placeholder build carries no reference page"
            );
            assert!(body.contains("not found"), "{path}");
        } else {
            assert_eq!(status, StatusCode::OK, "{path}");
            assert!(
                headers["content-type"]
                    .to_str()
                    .unwrap()
                    .starts_with("text/html"),
                "{path}"
            );
            assert!(
                body.contains(r#"<div id="app">"#),
                "{path}: the reference shell"
            );
            assert!(
                body.contains("<title>Phoebis Router API reference</title>"),
                "{path}"
            );
            assert!(
                !body.contains("theme.js"),
                "{path}: the page's theme is its own"
            );
            assert!(
                body.contains(r#"src="/assets/"#),
                "{path}: the entry is loaded from the hashed assets"
            );
        }
    }
    for path in [
        "/api-reference/x",
        "/api-reference/x.js",
        "/api-reference/openapi.json",
    ] {
        let (status, headers, body) = send("GET", path).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert_eq!(headers["cache-control"], "no-store", "{path}");
        assert!(
            body.contains("not found") && !body.contains("<div"),
            "{path}: JSON, never a shell"
        );
    }
    let (status, headers, body) = send("HEAD", "/api-reference/").await;
    if !phoebis_ui::is_placeholder() {
        assert_eq!(status, StatusCode::OK);
        assert!(headers.contains_key("content-length"));
    }
    assert!(body.is_empty(), "a HEAD carries no body");
    let (status, headers, _) = send("POST", "/api-reference/").await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(headers["allow"], "GET, HEAD");
}

/// `PHOEBIS_UI_EXPECT` pins which shell the build embedded, so a run that removes the bundle or sets
/// `PHOEBIS_SKIP_UI_BUILD` can assert the outcome rather than accept either one. Without it only the consistency
/// check runs.
#[test]
fn the_placeholder_gives_way_to_a_build_and_a_bundle_survives_the_skip() {
    let is_placeholder = phoebis_ui::is_placeholder();
    match std::env::var("PHOEBIS_UI_EXPECT").as_deref() {
        Ok("placeholder") => assert!(
            is_placeholder,
            "a skipped build with no bundle embeds the placeholder page"
        ),
        Ok("bundle") => assert!(!is_placeholder, "a build replaced the placeholder"),
        _ => {}
    }
    assert_eq!(
        is_placeholder,
        phoebis_ui::asset_paths()
            .iter()
            .all(|p| !p.starts_with("assets/")),
        "a placeholder has no hashed assets and a bundle has some"
    );
}

#[test]
fn a_path_never_escapes_the_bundle() {
    assert!(phoebis_ui::lookup("../Cargo.toml").is_none());
    assert!(phoebis_ui::lookup("assets/../../Cargo.toml").is_none());
}
