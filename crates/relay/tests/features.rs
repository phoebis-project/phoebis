//! The feature module as the feature listener serves it, over TLS under a pair of the test's own making: the key is
//! checked as on a keyed route of the proxy listener, no model route answers there, no feature path answers on the
//! proxy listener, and a call on the table leaves under the feature account's token toward the harness's one fake,
//! which plays `api.anthropic.com`.

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use bytes::Bytes;
use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule, SseEvent};
use phoebis_relay::{Limits, Relay};
use phoebis_store::{Cancellation, FeatureOverride, KeyFeatureAccounts, PoolFeatureAccounts};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tower::ServiceExt;

const API: &str = "api.anthropic.com";
/// The access token of the account `a`, which each feature call leaves under.
const SECRET_A: &str = "sk-ant-oat01-PLACEHOLDER-A";

/// The fake of `api.anthropic.com`, answering a model request with 200, so a model route that served on the feature
/// listener would reach it and answer that 200.
async fn upstream() -> FakeUpstream {
    FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: b"{}".to_vec(),
        },
    }])
    .await
}

async fn feature_harness(up: &FakeUpstream) -> Harness {
    harness(
        up,
        &[api_key("acct", "sk-ant-api03-PLACEHOLDER", 0)],
        PoolSpec::default(),
    )
    .await
}

/// A keyed Claude Code model request, the head and the body the model route forwards, with `Host` naming `host`.
fn model_request(host: &str) -> Vec<u8> {
    let session = fresh_session();
    let user_id = serde_json::to_string(&user_id_for(&session)).unwrap();
    let body = format!(
        r#"{{"model":"claude-sonnet-4-5","messages":[],"metadata":{{"user_id":{user_id}}}}}"#
    );
    format!(
        "POST /v1/messages HTTP/1.1\r\nHost: {host}\r\nx-api-key: {KEY}\r\n\
         User-Agent: {CLAUDE_CLI_USER_AGENT}\r\nX-Claude-Code-Session-Id: {session}\r\n\
         x-claude-code-request-class: main\r\nanthropic-version: 2023-06-01\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// The proxy listener as the router serves it, on plain HTTP.
async fn proxy_listener(relay: Arc<Relay>) -> (SocketAddr, tokio::sync::watch::Sender<bool>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stop_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(phoebis_relay::serve(
        listener,
        relay,
        axum::Router::new(),
        stop_rx,
        phoebis_relay::ListenLimits {
            drain_timeout: Duration::from_secs(30),
            header_read_timeout: Duration::from_secs(30),
            max_connections: 64,
        },
        None,
    ));
    (addr, stop)
}

/// The status line of the answer to `request`, which carries `Connection: close`, on a new plain connection.
async fn status_line(addr: SocketAddr, request: &[u8]) -> String {
    let mut socket = TcpStream::connect(addr).await.unwrap();
    socket.write_all(request).await.unwrap();
    let mut answer = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), socket.read_to_end(&mut answer))
        .await
        .expect("the answer ended")
        .unwrap();
    let answer = String::from_utf8_lossy(&answer);
    answer.lines().next().unwrap_or_default().to_owned()
}

#[tokio::test]
async fn the_feature_listener_serves_no_model_route() {
    let up = upstream().await;
    let h = feature_harness(&up).await;
    let listener = feature_listener(&h).await;
    let (status, headers, body) = listener
        .send("api.anthropic.com", &model_request("api.anthropic.com"))
        .await;
    assert_eq!(
        status,
        403,
        "{headers:?} {}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(
        up.requests().len(),
        0,
        "a model request reached the upstream"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_feature_path_on_the_proxy_listener_keeps_the_unknown_routes_404() {
    let up = upstream().await;
    let h = feature_harness(&up).await;
    let (addr, _stop) = proxy_listener(h.relay.clone()).await;
    let line = status_line(
        addr,
        format!(
            "GET /api/frame/x HTTP/1.1\r\nHost: api.anthropic.com\r\nx-api-key: {KEY}\r\n\
             Connection: close\r\n\r\n"
        )
        .as_bytes(),
    )
    .await;
    assert!(line.starts_with("HTTP/1.1 404 "), "{line}");
    assert_eq!(
        up.requests().len(),
        0,
        "a feature path reached the upstream"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_request_without_a_key_names_auth_missing_in_the_code_header() {
    let up = upstream().await;
    let h = feature_harness(&up).await;
    let listener = feature_listener(&h).await;
    let (status, headers, _) = listener
        .send(
            "api.anthropic.com",
            b"GET / HTTP/1.1\r\nHost: api.anthropic.com\r\nConnection: close\r\n\r\n",
        )
        .await;
    assert_eq!(status, 401, "{headers:?}");
    assert!(
        headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("x-phoebis-error-code") && value == "auth_missing"
        }),
        "{headers:?}"
    );
    assert_eq!(
        up.requests().len(),
        0,
        "a request without a key reached the upstream"
    );
    up.shutdown().await;
}

/// The fake of `api.anthropic.com` answering each path prefix of `rules` as the rule says.
async fn fake(rules: Vec<(&str, Respond)>) -> FakeUpstream {
    FakeUpstream::start(
        rules
            .into_iter()
            .map(|(path_prefix, respond)| Rule {
                path_prefix: path_prefix.into(),
                respond,
            })
            .collect(),
    )
    .await
}

/// The pool `main` with the OAuth account `a` as its account for both features.
fn both_features_at_a() -> PoolSpec {
    PoolSpec {
        feature_accounts: PoolFeatureAccounts {
            artifacts: Some("a".into()),
            connectors: Some("a".into()),
        },
        ..PoolSpec::default()
    }
}

async fn features_harness(up: &FakeUpstream, pool: PoolSpec) -> Harness {
    harness(up, &[oauth("a", SECRET_A, 0)], pool).await
}

/// A request under the router key: `request_line` without its version, the head's `Host` line `host`, then `lines`,
/// each ending in CRLF, and `body`.
fn keyed(request_line: &str, host: &str, lines: &str, body: &[u8]) -> Vec<u8> {
    let mut request = format!(
        "{request_line} HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {KEY}\r\n{lines}\
         Connection: close\r\n\r\n"
    )
    .into_bytes();
    request.extend_from_slice(body);
    request
}

/// The request lines the fake recorded, the model route's left out.
fn feature_lines(up: &FakeUpstream) -> Vec<String> {
    up.requests()
        .into_iter()
        .map(|request| request.line)
        .filter(|line| !line.starts_with("POST /v1/messages"))
        .collect()
}

#[tokio::test]
async fn a_sibling_of_an_allowed_prefix_is_refused() {
    let up = fake(vec![("/api/frame", json_200())]).await;
    let h = features_harness(&up, both_features_at_a()).await;
    let listener = feature_listener(&h).await;
    let (status, headers, _) = listener
        .send(API, &keyed("GET /api/framework/x", API, "", b""))
        .await;
    assert_eq!(status, 403, "{headers:?}");
    let (status, headers, _) = listener
        .send(API, &keyed("GET /api/frame/x", API, "", b""))
        .await;
    assert_eq!(status, 200, "{headers:?}");
    assert_eq!(feature_lines(&up), ["GET /api/frame/x HTTP/1.1"]);
    up.shutdown().await;
}

#[tokio::test]
async fn a_dot_segment_under_an_allowed_prefix_is_refused() {
    let up = fake(vec![("/api", json_200())]).await;
    let h = features_harness(&up, both_features_at_a()).await;
    let listener = feature_listener(&h).await;
    let (status, headers, _) = listener
        .send(API, &keyed("GET /api/frame/../oauth/profile", API, "", b""))
        .await;
    assert_eq!(status, 403, "{headers:?}");
    assert_eq!(feature_lines(&up), Vec::<String>::new());
    up.shutdown().await;
}

#[tokio::test]
async fn a_path_an_upstream_could_fold_is_refused() {
    let up = fake(vec![("/api", json_200())]).await;
    let h = features_harness(&up, both_features_at_a()).await;
    let listener = feature_listener(&h).await;
    let mut answers = Vec::new();
    for request_line in [
        "GET /api/frame/%2e%2e/oauth/profile",
        "GET /api/frame/x%2Fy",
        "GET /api/frame/x%5Cy",
        "GET /api/frame/..\\oauth/profile",
        "GET /api/frame/..;/oauth/profile",
        "POST /api/frame/Track",
    ] {
        let (status, _, _) = listener.send(API, &keyed(request_line, API, "", b"")).await;
        answers.push((request_line, status));
    }
    assert_eq!(
        answers,
        [
            ("GET /api/frame/%2e%2e/oauth/profile", 403),
            ("GET /api/frame/x%2Fy", 403),
            ("GET /api/frame/x%5Cy", 403),
            ("GET /api/frame/..\\oauth/profile", 403),
            ("GET /api/frame/..;/oauth/profile", 403),
            ("POST /api/frame/Track", 403),
        ]
    );
    let (status, headers, _) = listener
        .send(API, &keyed("GET /api/frame/x", API, "", b""))
        .await;
    assert_eq!(status, 200, "{headers:?}");
    assert_eq!(feature_lines(&up), ["GET /api/frame/x HTTP/1.1"]);
    up.shutdown().await;
}

#[tokio::test]
async fn a_path_that_ends_in_a_slash_is_refused() {
    let up = fake(vec![("/api/frame/", json_200())]).await;
    let h = features_harness(&up, both_features_at_a()).await;
    let listener = feature_listener(&h).await;
    let (status, headers, _) = listener
        .send(API, &keyed("POST /api/frame/track/", API, "", b""))
        .await;
    assert_eq!(status, 403, "{headers:?}");
    let (status, headers, _) = listener
        .send(API, &keyed("POST /api/frame/x", API, "", b""))
        .await;
    assert_eq!(status, 200, "{headers:?}");
    assert_eq!(feature_lines(&up), ["POST /api/frame/x HTTP/1.1"]);
    up.shutdown().await;
}

#[tokio::test]
async fn a_call_without_a_key_reaches_no_upstream() {
    let up = fake(vec![("/api/frame/", json_200())]).await;
    let h = features_harness(&up, both_features_at_a()).await;
    let listener = feature_listener(&h).await;
    let (status, headers, _) = listener
        .send(
            API,
            b"GET /api/frame/x HTTP/1.1\r\nHost: api.anthropic.com\r\nConnection: close\r\n\r\n",
        )
        .await;
    assert_eq!(status, 401, "{headers:?}");
    assert_eq!(feature_lines(&up), Vec::<String>::new());
    up.shutdown().await;
}

#[tokio::test]
async fn a_keys_none_wins_over_the_pools_account() {
    let none = fake(vec![("/api/frame/", json_200())]).await;
    let h = features_harness(
        &none,
        PoolSpec {
            key_feature_accounts: KeyFeatureAccounts {
                artifacts: FeatureOverride::NoAccount,
                connectors: FeatureOverride::Inherit,
            },
            ..both_features_at_a()
        },
    )
    .await;
    let (status, headers, _) = feature_listener(&h)
        .await
        .send(API, &keyed("GET /api/frame/x", API, "", b""))
        .await;
    assert_eq!(status, 403, "{headers:?}");
    assert_eq!(feature_lines(&none), Vec::<String>::new());
    let inherit = fake(vec![("/api/frame/", json_200())]).await;
    let h = features_harness(&inherit, both_features_at_a()).await;
    let (status, headers, _) = feature_listener(&h)
        .await
        .send(API, &keyed("GET /api/frame/x", API, "", b""))
        .await;
    assert_eq!(status, 200, "{headers:?}");
    assert_eq!(feature_lines(&inherit), ["GET /api/frame/x HTTP/1.1"]);
    none.shutdown().await;
    inherit.shutdown().await;
}

#[tokio::test]
async fn a_paused_or_cooling_account_serves_and_one_past_its_subscription_refuses() {
    let up = fake(vec![
        ("/api/frame/", json_200()),
        ("/v1/messages", json_429_unified(4_600, "five_hour")),
    ])
    .await;
    let h = features_harness(&up, both_features_at_a()).await;
    let listener = feature_listener(&h).await;
    // The model request the fake answers 429 cools `a` and spends its five-hour window.
    h.app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", &fresh_session()))
        .await
        .unwrap();
    wait_for_async(Duration::from_secs(5), || async {
        let cooldowns = h.router.cooldowns().await.unwrap();
        cooldowns
            .iter()
            .any(|row| row.account_id == "a")
            .then_some(())
    })
    .await;
    let (status, headers, _) = listener
        .send(API, &keyed("GET /api/frame/cooled", API, "", b""))
        .await;
    assert_eq!(status, 200, "{headers:?}");
    h.store.set_enabled("a", false, "test").unwrap();
    h.router.reload().await.unwrap();
    let (status, headers, _) = listener
        .send(API, &keyed("GET /api/frame/paused", API, "", b""))
        .await;
    assert_eq!(status, 200, "{headers:?}");
    h.store
        .set_canceled("a", Cancellation::On { ends_at: Some(NOW) })
        .unwrap();
    h.router.reload().await.unwrap();
    let (status, headers, _) = listener
        .send(API, &keyed("GET /api/frame/ended", API, "", b""))
        .await;
    assert_eq!(status, 403, "{headers:?}");
    assert!(
        headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("x-phoebis-error-code")
                && value == "feature_account_unavailable"
        }),
        "{headers:?}"
    );
    assert_eq!(
        feature_lines(&up),
        [
            "GET /api/frame/cooled HTTP/1.1",
            "GET /api/frame/paused HTTP/1.1"
        ]
    );
    up.shutdown().await;
}

#[tokio::test]
async fn the_bearer_is_the_accounts() {
    let up = fake(vec![("/api/frame/", json_200())]).await;
    let h = features_harness(&up, both_features_at_a()).await;
    let (status, headers, _) = feature_listener(&h)
        .await
        .send(API, &keyed("GET /api/frame/x", API, "", b""))
        .await;
    assert_eq!(status, 200, "{headers:?}");
    let requests = up.requests();
    let [arrived] = requests.as_slice() else {
        panic!("{} requests arrived", requests.len())
    };
    assert_eq!(arrived.credential(), Some(SECRET_A));
    assert!(
        !arrived.headers.iter().any(|(_, value)| value.contains(KEY)),
        "a line carried the router key: {:?}",
        arrived.headers
    );
    up.shutdown().await;
}

#[tokio::test]
async fn the_beta_line_arrives_as_sent() {
    let up = fake(vec![("/v1/mcp_servers", json_200())]).await;
    let h = features_harness(&up, both_features_at_a()).await;
    let (status, headers, _) = feature_listener(&h)
        .await
        .send(
            API,
            &keyed(
                "GET /v1/mcp_servers",
                API,
                "Anthropic-Beta: mcp-servers-2025-12-04\r\n",
                b"",
            ),
        )
        .await;
    assert_eq!(status, 200, "{headers:?}");
    let requests = up.requests();
    let [arrived] = requests.as_slice() else {
        panic!("{} requests arrived", requests.len())
    };
    let betas: Vec<(&str, &str)> = arrived
        .headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("anthropic-beta"))
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    assert_eq!(betas, [("Anthropic-Beta", "mcp-servers-2025-12-04")]);
    assert!(
        !arrived
            .headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("x-client-request-id")),
        "{:?}",
        arrived.headers
    );
    up.shutdown().await;
}

#[tokio::test]
async fn the_callers_method_arrives() {
    let up = fake(vec![("/api/frame/", json_200())]).await;
    let h = features_harness(&up, both_features_at_a()).await;
    let (status, headers, _) = feature_listener(&h)
        .await
        .send(
            API,
            &keyed("DELETE /api/frame/artifacts/art_example", API, "", b""),
        )
        .await;
    assert_eq!(status, 200, "{headers:?}");
    assert_eq!(
        feature_lines(&up),
        ["DELETE /api/frame/artifacts/art_example HTTP/1.1"]
    );
    up.shutdown().await;
}

#[tokio::test]
async fn an_answer_streams() {
    let up = fake(vec![(
        "/api/frame/",
        Respond::SseGated {
            status: 200,
            headers: vec![],
            events: vec![
                SseEvent {
                    event: "message".into(),
                    data: "first".into(),
                },
                SseEvent {
                    event: "message".into(),
                    data: "second".into(),
                },
            ],
        },
    )])
    .await;
    let h = features_harness(&up, both_features_at_a()).await;
    let listener = feature_listener(&h).await;
    let (status, headers, mut body) = listener
        .open(API, &keyed("GET /api/frame/x", API, "", b""))
        .await;
    assert_eq!(status, 200, "{headers:?}");
    let mut read = Vec::new();
    while !String::from_utf8_lossy(&read).contains("data: first") {
        let piece = body
            .next()
            .await
            .expect("the body ended before its first event");
        read.extend_from_slice(&piece);
    }
    up.release_one().await;
    while let Some(piece) = body.next().await {
        read.extend_from_slice(&piece);
    }
    assert!(
        String::from_utf8_lossy(&read).contains("data: second"),
        "{}",
        String::from_utf8_lossy(&read)
    );
    up.shutdown().await;
}

#[tokio::test]
async fn the_answer_filter_holds() {
    let up = fake(vec![(
        "/api/frame/",
        json_200_with_headers(&[
            ("set-cookie", "s=1"),
            ("anthropic-organization-id", "org_example"),
            ("x-frame-publish-remaining", "7"),
        ]),
    )])
    .await;
    let h = features_harness(&up, both_features_at_a()).await;
    let (status, headers, _) = feature_listener(&h)
        .await
        .send(API, &keyed("GET /api/frame/x", API, "", b""))
        .await;
    assert_eq!(status, 200, "{headers:?}");
    let named = |wanted: &str| -> Vec<&str> {
        headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case(wanted))
            .map(|(_, value)| value.as_str())
            .collect()
    };
    assert_eq!(named("set-cookie"), Vec::<&str>::new());
    assert_eq!(named("anthropic-organization-id"), Vec::<&str>::new());
    assert_eq!(named("x-frame-publish-remaining"), ["7"]);
    up.shutdown().await;
}

#[tokio::test]
async fn a_429_changes_nothing_on_the_account() {
    let up = fake(vec![
        (
            "/api/frame/",
            json_with_headers(
                429,
                &[
                    ("retry-after", "60"),
                    ("anthropic-ratelimit-unified-status", "rejected"),
                    ("anthropic-ratelimit-unified-5h-status", "rejected"),
                    ("anthropic-ratelimit-unified-5h-utilization", "1.0"),
                    ("anthropic-ratelimit-unified-5h-reset", "4600"),
                    ("anthropic-ratelimit-unified-reset", "4600"),
                    (
                        "anthropic-ratelimit-unified-representative-claim",
                        "five_hour",
                    ),
                ],
            ),
        ),
        ("/v1/messages", json_200()),
    ])
    .await;
    let h = features_harness(&up, both_features_at_a()).await;
    let windows_of_a = || async {
        h.router
            .samples()
            .await
            .unwrap()
            .into_iter()
            .find(|(account, _)| account == "a")
    };
    let windows_before = windows_of_a().await;
    let (status, headers, _) = feature_listener(&h)
        .await
        .send(API, &keyed("GET /api/frame/x", API, "", b""))
        .await;
    assert_eq!(status, 429, "{headers:?}");
    assert!(
        headers
            .iter()
            .any(|(name, value)| name.eq_ignore_ascii_case("retry-after") && value == "60"),
        "{headers:?}"
    );
    assert_eq!(windows_of_a().await, windows_before);
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", &fresh_session()))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(credentials_seen(&up), [SECRET_A]);
    up.shutdown().await;
}

#[tokio::test]
async fn a_body_one_byte_past_the_bound_is_refused_before_the_upstream() {
    let up = fake(vec![("/api/frame/", json_200())]).await;
    let h = harness_with(
        &up,
        &[oauth("a", SECRET_A, 0)],
        both_features_at_a(),
        Limits {
            max_request_bytes: 1_024,
            ..default_limits()
        },
    )
    .await;
    let listener = feature_listener(&h).await;
    let (status, headers, _) = listener
        .send(
            API,
            &keyed(
                "POST /api/frame/artifacts",
                API,
                "Content-Length: 1025\r\n",
                &[b'x'; 1_025],
            ),
        )
        .await;
    assert_eq!(status, 413, "{headers:?}");
    // The same 1,025 bytes in one chunk of 0x401, with no length the head could be refused by.
    let chunked = [&b"401\r\n"[..], &[b'x'; 1_025], b"\r\n0\r\n\r\n"].concat();
    let (status, headers, _) = listener
        .send(
            API,
            &keyed(
                "POST /api/frame/artifacts",
                API,
                "Transfer-Encoding: chunked\r\n",
                &chunked,
            ),
        )
        .await;
    assert_eq!(status, 413, "{headers:?}");
    let (status, headers, _) = listener
        .send(
            API,
            &keyed(
                "POST /api/frame/artifacts",
                API,
                "Content-Length: 1024\r\n",
                &[b'x'; 1_024],
            ),
        )
        .await;
    assert_eq!(status, 200, "{headers:?}");
    let requests = up.requests();
    let [arrived] = requests.as_slice() else {
        panic!("{} requests arrived", requests.len())
    };
    assert_eq!(
        (arrived.line.as_str(), arrived.body.len()),
        ("POST /api/frame/artifacts HTTP/1.1", 1_024)
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_call_without_a_body_takes_no_place_in_the_body_bounds() {
    let up = fake(vec![
        ("/api/frame/", json_200()),
        ("/v1/mcp_servers", json_200()),
    ])
    .await;
    // No body may arrive on the key, and the budget holds less than the bound of one body.
    let h = harness_with(
        &up,
        &[oauth("a", SECRET_A, 0)],
        both_features_at_a(),
        Limits {
            body_budget_bytes: 512,
            key_body_reads: 0,
            max_request_bytes: 1_024,
            ..default_limits()
        },
    )
    .await;
    let listener = feature_listener(&h).await;
    let (status, headers, _) = listener
        .send(
            API,
            &keyed("POST /api/frame/x", API, "Content-Length: 2\r\n", b"{}"),
        )
        .await;
    assert_eq!(status, 429, "{headers:?}");
    let (status, headers, _) = listener
        .send(API, &keyed("GET /v1/mcp_servers", API, "", b""))
        .await;
    assert_eq!(status, 200, "{headers:?}");
    assert_eq!(feature_lines(&up), ["GET /v1/mcp_servers HTTP/1.1"]);
    up.shutdown().await;
}

type BodySender = tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>;

/// A keyed `POST /api/frame/x` for the feature module's routes in process, its head declaring `declared` bytes, and the
/// sender its body arrives through. The channel holds one chunk, so a second send returns once the read took the first.
fn held_body_request(declared: usize) -> (Request<Body>, BodySender) {
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let request = Request::post("/api/frame/x")
        .header("host", API)
        .header("authorization", format!("Bearer {KEY}"))
        .header("content-length", declared)
        .body(Body::from_stream(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        ))
        .unwrap();
    (request, tx)
}

/// The status and the `x-phoebis-error-code` and `retry-after` lines of an answer, an absent line read as empty.
fn refusal_of(answer: &axum::http::Response<Body>) -> (u16, &str, &str) {
    let line = |name: &str| {
        answer
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
    };
    (
        answer.status().as_u16(),
        line("x-phoebis-error-code"),
        line("retry-after"),
    )
}

#[tokio::test]
async fn a_body_past_the_keys_read_slots_is_refused() {
    let up = fake(vec![("/api/frame/", json_200())]).await;
    let h = harness_with(
        &up,
        &[oauth("a", SECRET_A, 0)],
        both_features_at_a(),
        Limits {
            key_body_reads: 1,
            ..default_limits()
        },
    )
    .await;
    let app = phoebis_relay::features::router(h.relay.clone());
    let (held, tx) = held_body_request(16);
    let first = tokio::spawn({
        let app = app.clone();
        async move { app.oneshot(held).await.unwrap() }
    });
    for chunk in [&b"{\"a\":"[..], b" "] {
        tokio::time::timeout(
            Duration::from_secs(5),
            tx.send(Ok(Bytes::from_static(chunk))),
        )
        .await
        .expect("the read takes the chunk")
        .expect("the read is open");
    }
    let second = Request::post("/api/frame/x")
        .header("host", API)
        .header("authorization", format!("Bearer {KEY}"))
        .header("content-length", 2)
        .body(Body::from("{}"))
        .unwrap();
    let refused = tokio::time::timeout(Duration::from_secs(5), app.oneshot(second))
        .await
        .expect("refused at once")
        .unwrap();
    assert_eq!(refusal_of(&refused), (429, "key_body_reads", "1"));
    assert_eq!(feature_lines(&up), Vec::<String>::new());
    first.abort();
    up.shutdown().await;
}

#[tokio::test]
async fn a_body_that_stalls_is_ended_at_the_reads_deadline() {
    let up = fake(vec![("/api/frame/", json_200())]).await;
    let h = harness_with(
        &up,
        &[oauth("a", SECRET_A, 0)],
        both_features_at_a(),
        Limits {
            body_read_timeout: Duration::from_millis(200),
            ..default_limits()
        },
    )
    .await;
    let (request, _tx) = held_body_request(16);
    let answer = tokio::time::timeout(
        Duration::from_secs(5),
        phoebis_relay::features::router(h.relay.clone()).oneshot(request),
    )
    .await
    .expect("answered within a few seconds")
    .unwrap();
    assert_eq!(refusal_of(&answer), (408, "request_timeout", ""));
    assert_eq!(feature_lines(&up), Vec::<String>::new());
    up.shutdown().await;
}

#[tokio::test]
async fn a_host_line_in_capitals_with_a_trailing_dot_and_a_port_names_its_host() {
    let up = fake(vec![("/api/frame/", json_200())]).await;
    let h = features_harness(&up, both_features_at_a()).await;
    let (status, headers, _) = feature_listener(&h)
        .await
        .send(
            API,
            &keyed("GET /api/frame/x", "API.Anthropic.com.:8443", "", b""),
        )
        .await;
    assert_eq!(status, 200, "{headers:?}");
    assert_eq!(feature_lines(&up), ["GET /api/frame/x HTTP/1.1"]);
    up.shutdown().await;
}
