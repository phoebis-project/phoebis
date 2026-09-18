//! The embedded app: hashed assets cached for a year, the shell never cached, every unknown path the shell, and
//! the admin and metrics paths never shadowed.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::Router;
use http_body_util::BodyExt;
use tower::ServiceExt;

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
        "/upstreams/business",
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
        assert!(headers["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("script-src 'self'"));
        assert_eq!(headers["x-content-type-options"], "nosniff");
    }
    let (status, _, _) = send("HEAD", "/pools").await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = send("POST", "/pools").await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn hashed_assets_are_immutable_and_typed_and_a_missing_asset_is_a_404() {
    let asset = phoebis_ui::asset_paths()
        .into_iter()
        .find(|p| p.starts_with("assets/") && p.ends_with(".js"));
    if let Some(asset) = asset {
        let (status, headers, _) = send("GET", &format!("/{asset}")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers["cache-control"],
            "public, max-age=31536000, immutable"
        );
        assert!(headers["content-type"]
            .to_str()
            .unwrap()
            .contains("javascript"));
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
}

#[tokio::test]
async fn the_admin_and_metrics_routes_are_never_shadowed() {
    assert_eq!(send("GET", "/admin/session").await.2, "admin");
    assert_eq!(send("GET", "/metrics").await.2, "metrics");
    let (status, _, body) = send("GET", "/admin/definitely-not-a-route").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        body.contains("not found") && !body.contains("<div"),
        "an unknown admin path is JSON, never the shell"
    );
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
