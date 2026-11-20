//! The feature module as the feature listener serves it, over TLS under a pair of the test's own making: the key is
//! checked as on a keyed route of the proxy listener, no model route answers there, no feature path answers on the
//! proxy listener, and a call on the table leaves under the feature account's token toward the harness's fake of
//! `api.anthropic.com`, or toward its second fake, which plays `mcp-proxy.anthropic.com` for the connectors' sessions.

mod common;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use common::*;
use fake_upstream::{
    FakeUpstream, RecordedRequest, Respond, Rule, SseEvent, Switched, UpstreamSocket,
};
use phoebis_bus::{Bus, Delivery, Event, FeatureEnd, FeatureOutcome, ResponseChunk, QUEUE_DEPTH};
use phoebis_relay::{Accounting, AccountingSource, Limits, Relay};
use phoebis_store::{
    AccountState, Cancellation, FeatureOverride, KeyFeatureAccounts, NewBundle, Secret, StateCause,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tower::ServiceExt;

const API: &str = "api.anthropic.com";

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
        "POST /api/frame/%74rack",
        "POST /api/frame/track%3Bx",
        "POST /api/frame/%2574rack",
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
            ("POST /api/frame/%74rack", 403),
            ("POST /api/frame/track%3Bx", 403),
            ("POST /api/frame/%2574rack", 403),
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
async fn an_escape_that_decodes_to_no_refused_form_goes_upstream_as_sent() {
    let up = fake(vec![("/api/frame/", json_200())]).await;
    let h = features_harness(&up, both_features_at_a()).await;
    let (status, headers, _) = feature_listener(&h)
        .await
        .send(
            API,
            &keyed("POST /api/frame/retitle/my%20artifact", API, "", b""),
        )
        .await;
    assert_eq!(status, 200, "{headers:?}");
    assert_eq!(
        feature_lines(&up),
        ["POST /api/frame/retitle/my%20artifact HTTP/1.1"]
    );
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
async fn a_spelling_of_the_excepted_call_cut_at_a_decoded_mark_is_refused() {
    let up = fake(vec![("/api/frame/", json_200())]).await;
    let h = features_harness(&up, both_features_at_a()).await;
    let listener = feature_listener(&h).await;
    let mut answers = Vec::new();
    for request_line in ["POST /api/frame/track%3Fx", "POST /api/frame/track%23x"] {
        let (status, headers, _) = listener.send(API, &keyed(request_line, API, "", b"")).await;
        let (status, code, _) = refusal_of(status, &headers);
        answers.push((request_line, status, code.to_owned()));
    }
    assert_eq!(
        answers,
        [
            (
                "POST /api/frame/track%3Fx",
                403,
                "feature_not_allowed".to_owned()
            ),
            (
                "POST /api/frame/track%23x",
                403,
                "feature_not_allowed".to_owned()
            ),
        ]
    );
    assert_eq!(feature_lines(&up), Vec::<String>::new());
    up.shutdown().await;
}

#[tokio::test]
async fn a_decoded_mark_beside_no_exception_goes_upstream_as_sent() {
    let up = fake(vec![("/api/frame/", json_200())]).await;
    let h = features_harness(&up, both_features_at_a()).await;
    let (status, headers, _) = feature_listener(&h)
        .await
        .send(API, &keyed("POST /api/frame/retitle/a%3Fb", API, "", b""))
        .await;
    assert_eq!(status, 200, "{headers:?}");
    assert_eq!(
        feature_lines(&up),
        ["POST /api/frame/retitle/a%3Fb HTTP/1.1"]
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_path_that_decodes_to_a_control_byte_is_refused() {
    let up = fake(vec![("/api/frame/", json_200())]).await;
    let h = features_harness(&up, both_features_at_a()).await;
    let listener = feature_listener(&h).await;
    let mut answers = Vec::new();
    for request_line in [
        "GET /api/frame/x%00",
        "GET /api/frame/x%1Fy",
        "GET /api/frame/x%7F",
        "GET /api/frame/x%2509",
    ] {
        let (status, headers, _) = listener.send(API, &keyed(request_line, API, "", b"")).await;
        let (status, code, _) = refusal_of(status, &headers);
        answers.push((request_line, status, code.to_owned()));
    }
    assert_eq!(
        answers,
        [
            ("GET /api/frame/x%00", 403, "feature_not_allowed".to_owned()),
            (
                "GET /api/frame/x%1Fy",
                403,
                "feature_not_allowed".to_owned()
            ),
            ("GET /api/frame/x%7F", 403, "feature_not_allowed".to_owned()),
            (
                "GET /api/frame/x%2509",
                403,
                "feature_not_allowed".to_owned()
            ),
        ]
    );
    assert_eq!(feature_lines(&up), Vec::<String>::new());
    up.shutdown().await;
}

#[tokio::test]
async fn a_decoded_mark_behind_a_closing_slash_or_a_dot_segment_is_refused() {
    let up = fake(vec![("/api/frame/", json_200())]).await;
    let h = features_harness(&up, both_features_at_a()).await;
    let listener = feature_listener(&h).await;
    let mut answers = Vec::new();
    for request_line in [
        "POST /api/frame/track/%3Fx",
        "POST /api/frame/track/.%3Fx",
        "POST /api/frame/track/x/..%23y",
        "POST /api/frame/retitle/%3Fx",
    ] {
        let (status, headers, _) = listener.send(API, &keyed(request_line, API, "", b"")).await;
        let (status, code, _) = refusal_of(status, &headers);
        answers.push((request_line, status, code.to_owned()));
    }
    assert_eq!(
        answers,
        [
            (
                "POST /api/frame/track/%3Fx",
                403,
                "feature_not_allowed".to_owned()
            ),
            (
                "POST /api/frame/track/.%3Fx",
                403,
                "feature_not_allowed".to_owned()
            ),
            (
                "POST /api/frame/track/x/..%23y",
                403,
                "feature_not_allowed".to_owned()
            ),
            (
                "POST /api/frame/retitle/%3Fx",
                403,
                "feature_not_allowed".to_owned()
            ),
        ]
    );
    assert_eq!(feature_lines(&up), Vec::<String>::new());
    up.shutdown().await;
}

#[tokio::test]
async fn a_spelling_of_the_excepted_call_with_a_space_or_a_byte_outside_ascii_is_refused() {
    let up = fake(vec![("/api/frame/", json_200())]).await;
    let h = features_harness(&up, both_features_at_a()).await;
    let listener = feature_listener(&h).await;
    let mut answers = Vec::new();
    for request_line in [
        "POST /api/frame/track%20",
        "POST /api/frame/tr%20ack",
        "POST /api/frame/track%C2%A0",
    ] {
        let (status, headers, _) = listener.send(API, &keyed(request_line, API, "", b"")).await;
        let (status, code, _) = refusal_of(status, &headers);
        answers.push((request_line, status, code.to_owned()));
    }
    assert_eq!(
        answers,
        [
            (
                "POST /api/frame/track%20",
                403,
                "feature_not_allowed".to_owned()
            ),
            (
                "POST /api/frame/tr%20ack",
                403,
                "feature_not_allowed".to_owned()
            ),
            (
                "POST /api/frame/track%C2%A0",
                403,
                "feature_not_allowed".to_owned()
            ),
        ]
    );
    assert_eq!(feature_lines(&up), Vec::<String>::new());
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
async fn a_keyed_call_the_table_refuses_moves_the_keys_last_use() {
    let up = upstream().await;
    let h = feature_harness(&up).await;
    let last_use = || h.store.api_key("desktop").unwrap().unwrap().last_used_at;
    assert_eq!(last_use(), None);
    let (status, headers, _) = feature_listener(&h)
        .await
        .send(API, &keyed("GET /not/on/the/table", API, "", b""))
        .await;
    assert_eq!(status, 403, "{headers:?}");
    assert!(
        headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("x-phoebis-error-code") && value == "feature_not_allowed"
        }),
        "{headers:?}"
    );
    // The router task writes the stamp on a blocking thread, so the write can land behind the answer.
    wait_for(Duration::from_secs(5), || {
        (last_use() == Some(NOW)).then_some(())
    })
    .await;
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

/// A keyed `POST /api/frame/x` whose head declares 16 bytes of body, with the first 6 of them.
fn held_body_request() -> Vec<u8> {
    keyed(
        "POST /api/frame/x",
        API,
        "Content-Length: 16\r\n",
        b"{\"a\": ",
    )
}

/// The status and the `x-phoebis-error-code` and `retry-after` lines of an answer, an absent line read as empty.
fn refusal_of(status: u16, headers: &[(String, String)]) -> (u16, &str, &str) {
    let line = |name: &str| {
        headers
            .iter()
            .find(|(held, _)| held.eq_ignore_ascii_case(name))
            .map_or("", |(_, value)| value.as_str())
    };
    (status, line("x-phoebis-error-code"), line("retry-after"))
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
    let listener = feature_listener(&h).await;
    let _first = listener.hold(API, &held_body_request()).await;
    let (status, headers, _) = tokio::time::timeout(
        Duration::from_secs(5),
        listener.send(
            API,
            &keyed("POST /api/frame/x", API, "Content-Length: 2\r\n", b"{}"),
        ),
    )
    .await
    .expect("refused at once");
    assert_eq!(refusal_of(status, &headers), (429, "key_body_reads", "1"));
    assert_eq!(feature_lines(&up), Vec::<String>::new());
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
    let listener = feature_listener(&h).await;
    let held = listener.hold(API, &held_body_request()).await;
    let (status, headers, _) = tokio::time::timeout(Duration::from_secs(5), held.answer())
        .await
        .expect("answered within a few seconds");
    assert_eq!(refusal_of(status, &headers), (408, "request_timeout", ""));
    assert_eq!(feature_lines(&up), Vec::<String>::new());
    up.shutdown().await;
}

/// The harness of `features_harness` with `connectors` as the fake of `CONNECTOR_HOST`.
async fn connectors_harness(up: &FakeUpstream, connectors: &FakeUpstream) -> Harness {
    harness_with_connector_host(
        up,
        connectors,
        &[oauth("a", SECRET_A, 0)],
        both_features_at_a(),
        default_limits(),
    )
    .await
}

/// A keyed `POST` of the two bytes `{}` to `path` under the `Host` line `host`.
fn keyed_post(path: &str, host: &str) -> Vec<u8> {
    keyed(
        &format!("POST {path}"),
        host,
        "Content-Length: 2\r\n",
        b"{}",
    )
}

#[tokio::test]
async fn a_rows_host_binds() {
    let up = fake(vec![("/v1/mcp/", json_200())]).await;
    let connectors = connector_host_fake(vec![Rule {
        path_prefix: "/v1/mcp/".into(),
        respond: json_200(),
    }])
    .await;
    let h = connectors_harness(&up, &connectors).await;
    let listener = feature_listener(&h).await;
    let (status, headers, _) = listener.send(API, &keyed_post("/v1/mcp/x", API)).await;
    assert_eq!(status, 403, "{headers:?}");
    assert_eq!(
        (feature_lines(&up), feature_lines(&connectors)),
        (Vec::<String>::new(), Vec::<String>::new())
    );
    let (status, headers, _) = listener
        .send(CONNECTOR_HOST, &keyed_post("/v1/mcp/x", CONNECTOR_HOST))
        .await;
    assert_eq!(status, 200, "{headers:?}");
    assert_eq!(
        (feature_lines(&up), feature_lines(&connectors)),
        (
            Vec::<String>::new(),
            vec!["POST /v1/mcp/x HTTP/1.1".to_owned()]
        )
    );
    up.shutdown().await;
    connectors.shutdown().await;
}

#[tokio::test]
async fn each_plane_takes_its_rows() {
    let up = fake(vec![("/api/frame/", json_200())]).await;
    let connectors = connector_host_fake(vec![Rule {
        path_prefix: "/v1/mcp/".into(),
        respond: json_200(),
    }])
    .await;
    let h = connectors_harness(&up, &connectors).await;
    let listener = feature_listener(&h).await;
    let (status, headers, _) = listener
        .send(API, &keyed("GET /api/frame/x", API, "", b""))
        .await;
    assert_eq!(status, 200, "{headers:?}");
    let (status, headers, _) = listener
        .send(CONNECTOR_HOST, &keyed_post("/v1/mcp/x", CONNECTOR_HOST))
        .await;
    assert_eq!(status, 200, "{headers:?}");
    let offered = |fake: &FakeUpstream| -> Vec<(String, Vec<String>)> {
        fake.requests()
            .into_iter()
            .map(|request| (request.line, request.alpn))
            .collect()
    };
    assert_eq!(
        offered(&up),
        [("GET /api/frame/x HTTP/1.1".to_owned(), Vec::<String>::new())]
    );
    assert_eq!(
        offered(&connectors),
        [(
            "POST /v1/mcp/x HTTP/1.1".to_owned(),
            vec!["http/1.1".to_owned()]
        )]
    );
    up.shutdown().await;
    connectors.shutdown().await;
}

#[tokio::test]
async fn a_call_on_each_plane_publishes_its_end() {
    let up = fake(vec![
        ("/api/artifacts/mcp", json_200()),
        ("/api/frame/", json_200()),
    ])
    .await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus(&up, &[oauth("a", SECRET_A, 0)], both_features_at_a(), bus).await;
    let listener = feature_listener(&h).await;
    let (status, headers, _) = listener
        .send(API, &keyed("GET /api/frame/x", API, "", b""))
        .await;
    assert_eq!(status, 200, "{headers:?}");
    let (status, headers, _) = listener
        .send(API, &keyed_post("/api/artifacts/mcp", API))
        .await;
    assert_eq!(status, 200, "{headers:?}");
    let mut ends = Vec::new();
    while ends.len() < 2 {
        let event = tokio::time::timeout(Duration::from_secs(5), sub.events.recv())
            .await
            .expect("a call's end stayed off the bus")
            .expect("the bus stays open");
        if let Event::FeatureCall(call) = event {
            ends.push((call.path, call.end));
        }
    }
    ends.sort_by(|a, b| a.0.cmp(&b.0));
    let sent = FeatureEnd::Sent {
        account_id: "a".into(),
        error_code: None,
        feature: "artifacts",
        key_id: "desktop".into(),
        outcome: FeatureOutcome::Forwarded,
        status: Some(200),
    };
    assert_eq!(
        ends,
        [
            ("/api/artifacts/mcp".to_owned(), sent.clone()),
            ("/api/frame/x".to_owned(), sent)
        ]
    );
    up.shutdown().await;
}

/// A writer that stopped draining fills the bus queue behind its channel, and a refusal that waited there would hold
/// its connection, the launch script's request without a key among them.
#[tokio::test]
async fn a_refused_feature_call_answers_while_the_ledger_is_behind_and_the_bus_queue_is_full() {
    let up = upstream().await;
    let bus = Bus::new();
    let _ledger = bus.subscribe("ledger", Delivery::Durable);
    for index in 0..QUEUE_DEPTH {
        bus.publish_fact(Event::ResponseChunk(ResponseChunk {
            request_id: "filler".into(),
            attempt: 1,
            index: index as u32,
            body: Bytes::from_static(b"x"),
            phase: None,
        }))
        .await;
    }
    let behind: AccountingSource = Arc::new(|_| Accounting { behind: true });
    let h = harness_with_bus_and_accounting(
        &up,
        &[oauth("a", SECRET_A, 0)],
        bus,
        default_limits(),
        behind,
    )
    .await;
    let listener = feature_listener(&h).await;
    let (status, headers, _) = tokio::time::timeout(
        Duration::from_secs(1),
        listener.send(
            API,
            b"GET / HTTP/1.1\r\nHost: api.anthropic.com\r\nConnection: close\r\n\r\n",
        ),
    )
    .await
    .expect("the refusal waited on the full queue");
    assert_eq!(status, 401);
    assert_eq!(refusal_of(status, &headers).1, "auth_missing");
    up.shutdown().await;
}

/// The ledger's standing as `behind` holds it at each read, so a test moves it between two calls.
fn standing(behind: &Arc<AtomicBool>) -> AccountingSource {
    let behind = Arc::clone(behind);
    Arc::new(move |_| Accounting {
        behind: behind.load(Ordering::SeqCst),
    })
}

#[tokio::test]
async fn a_keyed_call_on_the_table_answers_ledger_behind_before_its_body_is_read() {
    let up = fake(vec![("/api/frame/", json_200())]).await;
    let h = harness_with_pool_and_accounting(
        &up,
        &[oauth("a", SECRET_A, 0)],
        both_features_at_a(),
        default_limits(),
        standing(&Arc::new(AtomicBool::new(true))),
    )
    .await;
    let listener = feature_listener(&h).await;
    let (status, headers, _) = listener
        .send(API, &keyed("GET /api/frame/x", API, "", b""))
        .await;
    assert_eq!(refusal_of(status, &headers), (503, "ledger_behind", "5"));
    // The body never arrives whole, so a refusal that waited for it would come at the read's 60 seconds.
    let held = listener.hold(API, &held_body_request()).await;
    let (status, headers, _) = tokio::time::timeout(Duration::from_secs(5), held.answer())
        .await
        .expect("refused ahead of the body's read");
    assert_eq!(refusal_of(status, &headers), (503, "ledger_behind", "5"));
    assert_eq!(feature_lines(&up), Vec::<String>::new());
    up.shutdown().await;
}

#[tokio::test]
async fn a_domain_check_answers_ledger_behind_before_its_wait_for_a_place() {
    let up = fake(vec![
        (
            "/api/web/domain_info?domain=held.example",
            Respond::Gated(Box::new(json_200())),
        ),
        ("/api/web/domain_info", json_200()),
    ])
    .await;
    let behind = Arc::new(AtomicBool::new(true));
    let h = harness_with_pool_and_accounting(
        &up,
        &[api_key("acct", "sk-ant-api03-PLACEHOLDER", 0)],
        PoolSpec::default(),
        Limits {
            domain_checks_per_source: 1,
            ..default_limits()
        },
        standing(&behind),
    )
    .await;
    let listener = Arc::new(feature_listener(&h).await);
    let check = keyless("GET /api/web/domain_info?domain=example.com", "", b"");
    let (status, headers, _) =
        tokio::time::timeout(Duration::from_secs(5), listener.send(API, &check))
            .await
            .expect("refused at once");
    assert_eq!(refusal_of(status, &headers), (503, "ledger_behind", "5"));
    // A check admitted while the ledger is current holds the source's one place at the fake's gate.
    behind.store(false, Ordering::SeqCst);
    let first = tokio::spawn({
        let listener = Arc::clone(&listener);
        async move {
            let request = keyless("GET /api/web/domain_info?domain=held.example", "", b"");
            listener.send(API, &request).await.0
        }
    });
    wait_for(Duration::from_secs(5), || {
        (up.requests().len() == 1).then_some(())
    })
    .await;
    behind.store(true, Ordering::SeqCst);
    // A refusal behind the wait would hold the check for the place until the check's 30 seconds end.
    let (status, headers, _) =
        tokio::time::timeout(Duration::from_secs(5), listener.send(API, &check))
            .await
            .expect("refused ahead of the wait for a place");
    assert_eq!(refusal_of(status, &headers), (503, "ledger_behind", "5"));
    up.release();
    assert_eq!(first.await.unwrap(), 200);
    assert_eq!(
        feature_lines(&up),
        ["GET /api/web/domain_info?domain=held.example HTTP/1.1"]
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_call_its_account_cannot_serve_keeps_its_refusal_while_the_ledger_is_behind() {
    let up = fake(vec![("/api/frame/", json_200())]).await;
    let h = harness_with_pool_and_accounting(
        &up,
        &[oauth("a", SECRET_A, 0)],
        both_features_at_a(),
        default_limits(),
        standing(&Arc::new(AtomicBool::new(true))),
    )
    .await;
    h.store
        .set_canceled("a", Cancellation::On { ends_at: Some(NOW) })
        .unwrap();
    h.router.reload().await.unwrap();
    let (status, headers, _) = feature_listener(&h)
        .await
        .send(API, &keyed("GET /api/frame/x", API, "", b""))
        .await;
    assert_eq!(
        refusal_of(status, &headers),
        (403, "feature_account_unavailable", "")
    );
    assert_eq!(feature_lines(&up), Vec::<String>::new());
    up.shutdown().await;
}

#[tokio::test]
async fn a_caller_that_leaves_mid_stream_ends_the_upstream_read_and_its_connection_is_not_reused() {
    let up = fake(vec![]).await;
    let connectors = connector_host_fake(vec![
        Rule {
            path_prefix: "/v1/mcp/stream".into(),
            respond: Respond::SseGatedKeptOpen {
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
        },
        Rule {
            path_prefix: "/v1/mcp/x".into(),
            respond: json_200(),
        },
    ])
    .await;
    let h = connectors_harness(&up, &connectors).await;
    let listener = feature_listener(&h).await;
    for _ in 0..2 {
        let (status, headers, _) = listener
            .send(CONNECTOR_HOST, &keyed_post("/v1/mcp/x", CONNECTOR_HOST))
            .await;
        assert_eq!(status, 200, "{headers:?}");
    }
    let (status, headers, mut body) = listener
        .open(
            CONNECTOR_HOST,
            &keyed_post("/v1/mcp/stream", CONNECTOR_HOST),
        )
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
    drop(body);
    connectors.release_one().await;
    let requests = connectors.requests();
    let [whole, next, stream] = requests.as_slice() else {
        panic!("{} requests arrived", requests.len())
    };
    assert_eq!(
        whole.connection, next.connection,
        "a call read to its end left its connection to the next call"
    );
    wait_for(Duration::from_secs(5), || {
        (!connectors.is_open(stream.connection)).then_some(())
    })
    .await;
    let (status, headers, _) = listener
        .send(CONNECTOR_HOST, &keyed_post("/v1/mcp/x", CONNECTOR_HOST))
        .await;
    assert_eq!(status, 200, "{headers:?}");
    let requests = connectors.requests();
    let after = requests.last().expect("the last call arrived");
    assert_eq!(after.line, "POST /v1/mcp/x HTTP/1.1");
    assert_ne!(
        after.connection, stream.connection,
        "the call after the one its caller left arrived on that call's connection"
    );
    up.shutdown().await;
    connectors.shutdown().await;
}

#[tokio::test]
async fn a_body_past_the_free_byte_budget_is_refused() {
    let up = fake(vec![("/api/frame/", json_200())]).await;
    // A body of 600 bytes fits the size bound and not the budget.
    let h = harness_with(
        &up,
        &[oauth("a", SECRET_A, 0)],
        both_features_at_a(),
        Limits {
            body_budget_bytes: 512,
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
                "POST /api/frame/x",
                API,
                "Content-Length: 600\r\n",
                &[b'x'; 600],
            ),
        )
        .await;
    assert_eq!(status, 503, "{headers:?}");
    assert!(
        headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("x-phoebis-error-code") && value == "body_budget_exhausted"
        }),
        "{headers:?}"
    );
    assert_eq!(feature_lines(&up), Vec::<String>::new());
    let (status, headers, _) = listener
        .send(
            API,
            &keyed("POST /api/frame/x", API, "Content-Length: 2\r\n", b"{}"),
        )
        .await;
    assert_eq!(status, 200, "{headers:?}");
    assert_eq!(feature_lines(&up), ["POST /api/frame/x HTTP/1.1"]);
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

#[tokio::test]
async fn a_feature_call_answers_502_for_a_lookup_that_fails_at_a_pool_miss() {
    let up = fake(vec![("/api/artifacts/mcp", json_200())]).await;
    let h = features_harness(&up, both_features_at_a()).await;
    let listener = feature_listener(&h).await;
    // The row leaves over the account's pooled connections, and the pool holds none yet, so the call's checkout
    // looks the host up.
    h.failing_lookups.store(1, Ordering::SeqCst);
    let (status, headers, _) = listener
        .send(API, &keyed("GET /api/artifacts/mcp", API, "", b""))
        .await;
    assert_eq!(
        refusal_of(status, &headers),
        (502, "upstream_unreachable", "")
    );
    up.shutdown().await;
}

/// A body of nineteen bytes, which a resend carries whole.
const TITLE: &[u8] = br#"{"title":"example"}"#;

/// The body of the upstream's 401 that `expired` answers.
const EXPIRED_BODY: &str =
    r#"{"type":"error","error":{"type":"authentication_error","message":"expired"}}"#;

/// A keyed `POST` of `TITLE` to `path` under the `Host` line `host`.
fn keyed_title(path: &str, host: &str) -> Vec<u8> {
    keyed(
        &format!("POST {path}"),
        host,
        "Content-Length: 19\r\n",
        TITLE,
    )
}

/// The requests a fake recorded for feature calls, the token endpoint's and the model route's left out.
fn feature_requests(fake: &FakeUpstream) -> Vec<RecordedRequest> {
    fake.requests()
        .into_iter()
        .filter(|request| {
            !request.line.starts_with("POST /v1/oauth/token")
                && !request.line.starts_with("POST /v1/messages")
        })
        .collect()
}

/// The `Authorization` line and the body of the second of exactly two requests.
fn second_of_two(requests: &[RecordedRequest]) -> (Option<&str>, &[u8]) {
    let [_, second] = requests else {
        panic!("{} feature requests arrived", requests.len())
    };
    let authorization = second
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
        .map(|(_, value)| value.as_str());
    (authorization, second.body.as_slice())
}

#[tokio::test]
async fn a_401_is_refreshed_and_resent_once() {
    let up = fake(vec![
        ("/api/frame/", json_200()),
        ("/v1/oauth/token", token_answer()),
    ])
    .await;
    up.add_credential_rule(SECRET_A, "/api/frame/", expired());
    let h = harness(&up, &[paired("a", SECRET_A)], both_features_at_a()).await;
    let (status, headers, _) = feature_listener(&h)
        .await
        .send(API, &keyed_title("/api/frame/artifacts", API))
        .await;
    assert_eq!(status, 200, "{headers:?}");
    let requests = feature_requests(&up);
    assert_eq!(
        second_of_two(&requests),
        (Some("Bearer sk-ant-oat01-PLACEHOLDER"), TITLE)
    );
    up.shutdown().await;

    // The connectors' session rides the other plane, on a harness of its own, since a refresh rotates the token the
    // account then holds and this call has to start from the first one.
    let up = fake(vec![("/v1/oauth/token", token_answer())]).await;
    let connectors = connector_host_fake(vec![Rule {
        path_prefix: "/v1/mcp/".into(),
        respond: json_200(),
    }])
    .await;
    connectors.add_credential_rule(SECRET_A, "/v1/mcp/", expired());
    let h = harness_with_connector_host(
        &up,
        &connectors,
        &[paired("a", SECRET_A)],
        both_features_at_a(),
        default_limits(),
    )
    .await;
    let (status, headers, _) = feature_listener(&h)
        .await
        .send(
            CONNECTOR_HOST,
            &keyed_title("/v1/mcp/srv_example", CONNECTOR_HOST),
        )
        .await;
    assert_eq!(status, 200, "{headers:?}");
    let requests = connectors.requests();
    assert_eq!(
        second_of_two(&requests),
        (Some("Bearer sk-ant-oat01-PLACEHOLDER"), TITLE)
    );
    up.shutdown().await;
    connectors.shutdown().await;
}

#[tokio::test]
async fn a_second_401_goes_back() {
    let up = fake(vec![
        ("/api/frame/", expired()),
        ("/v1/oauth/token", token_answer()),
    ])
    .await;
    let h = harness(&up, &[paired("a", SECRET_A)], both_features_at_a()).await;
    let (status, headers, body) = feature_listener(&h)
        .await
        .send(API, &keyed_title("/api/frame/artifacts", API))
        .await;
    assert_eq!(status, 401, "{headers:?}");
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains(EXPIRED_BODY), "{body}");
    assert_eq!((feature_requests(&up).len(), token_posts(&up)), (2, 1));
    up.shutdown().await;
}

#[tokio::test]
async fn a_connectors_own_401_goes_back_at_once() {
    let up = fake(vec![("/v1/oauth/token", token_answer())]).await;
    let connectors = connector_host_fake(vec![Rule {
        path_prefix: "/v1/mcp/".into(),
        respond: json_with_headers(401, &[("X-Mcp-Error-Code", "example_reauth")]),
    }])
    .await;
    let h = harness_with_connector_host(
        &up,
        &connectors,
        &[paired("a", SECRET_A)],
        both_features_at_a(),
        default_limits(),
    )
    .await;
    let (status, headers, _) = feature_listener(&h)
        .await
        .send(
            CONNECTOR_HOST,
            &keyed_title("/v1/mcp/srv_example", CONNECTOR_HOST),
        )
        .await;
    let codes: Vec<&str> = headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("x-mcp-error-code"))
        .map(|(_, value)| value.as_str())
        .collect();
    assert_eq!((status, codes), (401, vec!["example_reauth"]));
    assert_eq!((feature_lines(&connectors).len(), token_posts(&up)), (1, 0));
    up.shutdown().await;
    connectors.shutdown().await;
}

#[tokio::test]
async fn a_401_leaves_the_account_as_it_was() {
    let up = fake(vec![
        ("/api/frame/", expired()),
        ("/v1/oauth/token", token_answer()),
        ("/v1/messages", json_200()),
    ])
    .await;
    let h = harness(&up, &[paired("a", SECRET_A)], both_features_at_a()).await;
    let (status, headers, _) = feature_listener(&h)
        .await
        .send(API, &keyed_title("/api/frame/artifacts", API))
        .await;
    assert_eq!(status, 401, "{headers:?}");
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", &fresh_session()))
        .await
        .unwrap();
    let row = h.store.account("a").unwrap().unwrap();
    assert_eq!(
        (row.state, row.state_cause, resp.status().as_u16()),
        (AccountState::Ready, StateCause::Operator, 200)
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_401_whose_refresh_fails_goes_back_as_the_upstream_sent_it() {
    let up = fake(vec![
        ("/api/frame/", expired()),
        ("/v1/oauth/token", invalid_grant()),
    ])
    .await;
    let h = harness(&up, &[paired("a", SECRET_A)], both_features_at_a()).await;
    let (status, headers, body) = feature_listener(&h)
        .await
        .send(API, &keyed_title("/api/frame/artifacts", API))
        .await;
    assert_eq!(status, 401, "{headers:?}");
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains(EXPIRED_BODY), "{body}");
    assert_eq!(feature_requests(&up).len(), 1);
    up.shutdown().await;
}

#[tokio::test]
async fn a_forced_refresh_that_lands_a_mark_reaches_the_routing_snapshot() {
    let up = fake(vec![
        ("/api/frame/", expired()),
        ("/v1/oauth/token", invalid_grant()),
    ])
    .await;
    let h = harness(&up, &[paired("a", SECRET_A)], both_features_at_a()).await;
    let state_of_a = || {
        h.router
            .snapshot()
            .account("a")
            .map(|account| account.state)
    };
    assert_eq!(state_of_a(), Some(AccountState::Ready));
    let (status, headers, _) = feature_listener(&h)
        .await
        .send(API, &keyed_title("/api/frame/artifacts", API))
        .await;
    assert_eq!(status, 401, "{headers:?}");
    wait_for(Duration::from_secs(5), || {
        (state_of_a() == Some(AccountState::ReauthRequired)).then_some(())
    })
    .await;
    up.shutdown().await;
}

/// A disconnect of `id` and a new pairing under `secret`, as the admin's two routes leave the store, the manager and
/// the routing: the stored token row gone, the lifecycle generation moved by one and the token's count back at 1.
async fn signed_in_again(h: &Harness, id: &str, secret: &str) {
    let lifecycle = h.store.bump_lifecycle(id, StateCause::Operator).unwrap();
    h.store
        .commit_pairing(
            id,
            1,
            lifecycle,
            None,
            &NewBundle {
                access_token: Secret::new(secret),
                refresh_token: Some(Secret::new(&format!("{secret}-refresh"))),
                issued_at: NOW,
                expires_at: i64::MAX / 2,
                refresh_expires_at: None,
                scopes: "user:inference".into(),
            },
        )
        .unwrap();
    h.store
        .set_state(id, AccountState::Ready, StateCause::Paired)
        .unwrap();
    h.manager.invalidate(id);
    h.router.reload().await.unwrap();
}

#[tokio::test]
async fn a_later_token_is_refreshed_and_resent_again() {
    let up = fake(vec![
        ("/api/frame/", expired()),
        ("/v1/oauth/token", token_answer()),
    ])
    .await;
    let h = harness(&up, &[paired("a", SECRET_A)], both_features_at_a()).await;
    let listener = feature_listener(&h).await;
    let (status, headers, _) = listener
        .send(API, &keyed_title("/api/frame/artifacts", API))
        .await;
    assert_eq!(status, 401, "{headers:?}");
    // A forced refresh outside the feature path, as the model path runs one, moves the account past the held
    // generation 2.
    let row = h.store.account("a").unwrap().unwrap();
    h.manager.refresh_now(&row, 2).await.unwrap();
    // Generation 3 gets its refresh and its resend again, and the resend's 401 holds generation 4 in place of 2, so
    // the third call goes out once with no refresh.
    let (status, headers, _) = listener
        .send(API, &keyed_title("/api/frame/artifacts", API))
        .await;
    assert_eq!(status, 401, "{headers:?}");
    let (status, headers, _) = listener
        .send(API, &keyed_title("/api/frame/artifacts", API))
        .await;
    assert_eq!(status, 401, "{headers:?}");
    assert_eq!((feature_requests(&up).len(), token_posts(&up)), (5, 3));
    up.shutdown().await;
}

#[tokio::test]
async fn a_401_whose_refresh_gave_no_token_sets_no_hold() {
    let up = fake(vec![
        ("/api/frame/", expired()),
        (
            "/v1/oauth/token",
            Respond::Json {
                status: 503,
                headers: vec![],
                body: b"{}".to_vec(),
            },
        ),
    ])
    .await;
    up.set_rule_after(1, "/v1/oauth/token", token_answer());
    let h = harness(&up, &[paired("a", SECRET_A)], both_features_at_a()).await;
    let listener = feature_listener(&h).await;
    let (status, headers, _) = listener
        .send(API, &keyed_title("/api/frame/artifacts", API))
        .await;
    assert_eq!(status, 401, "{headers:?}");
    // The 503 armed the manager's backoff, which holds each refresh of the account until the clock reaches its end.
    step_to_backoff(&h, &up, "a").await;
    let (status, headers, _) = listener
        .send(API, &keyed_title("/api/frame/artifacts", API))
        .await;
    assert_eq!(status, 401, "{headers:?}");
    assert_eq!((feature_requests(&up).len(), token_posts(&up)), (3, 2));
    up.shutdown().await;
}

#[tokio::test]
async fn an_accepted_resend_sets_no_hold() {
    let up = fake(vec![
        ("/api/frame/", json_200()),
        ("/v1/oauth/token", token_answer()),
    ])
    .await;
    up.add_credential_rule(SECRET_A, "/api/frame/", expired());
    let h = harness(&up, &[paired("a", SECRET_A)], both_features_at_a()).await;
    let listener = feature_listener(&h).await;
    let (status, headers, _) = listener
        .send(API, &keyed_title("/api/frame/artifacts", API))
        .await;
    assert_eq!(status, 200, "{headers:?}");
    // The token the resend went out under, the token fixture's, goes stale behind the accepted call.
    up.add_credential_rule("sk-ant-oat01-PLACEHOLDER", "/api/frame/", expired());
    let (status, headers, _) = listener
        .send(API, &keyed_title("/api/frame/artifacts", API))
        .await;
    assert_eq!(status, 401, "{headers:?}");
    assert_eq!((feature_requests(&up).len(), token_posts(&up)), (4, 2));
    up.shutdown().await;
}

#[tokio::test]
async fn a_new_sign_in_starts_without_the_hold() {
    let up = fake(vec![
        ("/api/frame/", expired()),
        ("/v1/oauth/token", token_answer()),
    ])
    .await;
    let h = harness(&up, &[paired("a", SECRET_A)], both_features_at_a()).await;
    let listener = feature_listener(&h).await;
    let (status, headers, _) = listener
        .send(API, &keyed_title("/api/frame/artifacts", API))
        .await;
    assert_eq!(status, 401, "{headers:?}");
    signed_in_again(&h, "a", "sk-ant-oat01-PLACEHOLDER-AGAIN").await;
    // The new sign-in's token reaches generation 2, the number the first sign-in's hold names.
    let row = h.store.account("a").unwrap().unwrap();
    h.manager.refresh_now(&row, 1).await.unwrap();
    let (status, headers, _) = listener
        .send(API, &keyed_title("/api/frame/artifacts", API))
        .await;
    assert_eq!(status, 401, "{headers:?}");
    assert_eq!((feature_requests(&up).len(), token_posts(&up)), (4, 3));
    up.shutdown().await;
}

/// A deletion of `id` and an account created again under it with `secret`, as the admin's two routes leave the store,
/// the manager and the routing: a new incarnation, the lifecycle and the token's count at 1 again, and the selections
/// the deletion cleared left clear.
async fn created_again(h: &Harness, id: &'static str, secret: &str) {
    h.store.delete_account(id, "test").unwrap();
    h.store
        .insert_account(&test_account_row(&oauth(id, "", 0)))
        .unwrap();
    let row = h.store.account(id).unwrap().unwrap();
    h.store
        .commit_credentials(
            id,
            row.incarnation,
            row.lifecycle_generation,
            None,
            None,
            &NewBundle {
                access_token: Secret::new(secret),
                refresh_token: Some(Secret::new(&format!("{secret}-refresh"))),
                issued_at: NOW,
                expires_at: i64::MAX / 2,
                refresh_expires_at: None,
                scopes: "user:inference".into(),
            },
        )
        .unwrap();
    h.manager.invalidate(id);
    h.router.reload().await.unwrap();
}

/// A ledger that stays current and adds one to `admitted` at each counted reading, which a keyed call takes behind its
/// admission checks and ahead of its body's read, so a test learns there that a call was admitted.
fn counted(admitted: &Arc<AtomicUsize>) -> AccountingSource {
    let admitted = Arc::clone(admitted);
    Arc::new(move |counts| {
        if counts {
            admitted.fetch_add(1, Ordering::SeqCst);
        }
        Accounting { behind: false }
    })
}

#[tokio::test]
async fn a_call_whose_account_is_created_again_while_its_body_arrives_is_refused() {
    let up = fake(vec![("/api/frame/", json_200())]).await;
    let admitted = Arc::new(AtomicUsize::new(0));
    let h = harness_with_pool_and_accounting(
        &up,
        &[oauth("a", SECRET_A, 0)],
        both_features_at_a(),
        default_limits(),
        counted(&admitted),
    )
    .await;
    let listener = feature_listener(&h).await;
    let mut held = listener.hold(API, &held_body_request()).await;
    wait_for(Duration::from_secs(5), || {
        (admitted.load(Ordering::SeqCst) == 1).then_some(())
    })
    .await;
    created_again(&h, "a", "sk-ant-oat01-PLACEHOLDER-B").await;
    // The ten bytes the head declared beyond the six it carried.
    held.write(br#""example"}"#).await;
    let (status, headers, _) = held.answer().await;
    assert_eq!(
        refusal_of(status, &headers),
        (403, "feature_account_unavailable", "")
    );
    assert_eq!(feature_lines(&up), Vec::<String>::new());
    up.shutdown().await;
}

#[tokio::test]
async fn a_401_whose_account_signed_in_again_while_it_waited_goes_back_without_a_refresh() {
    let up = fake(vec![
        ("/api/frame/", Respond::Gated(Box::new(expired()))),
        ("/v1/oauth/token", token_answer()),
    ])
    .await;
    let h = harness(&up, &[paired("a", SECRET_A)], both_features_at_a()).await;
    let listener = feature_listener(&h).await;
    let held = listener
        .hold(API, &keyed_title("/api/frame/artifacts", API))
        .await;
    wait_for(Duration::from_secs(5), || {
        (feature_requests(&up).len() == 1).then_some(())
    })
    .await;
    signed_in_again(&h, "a", "sk-ant-oat01-PLACEHOLDER-AGAIN").await;
    up.release();
    let (status, headers, body) = held.answer().await;
    assert_eq!(status, 401, "{headers:?}");
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains(EXPIRED_BODY), "{body}");
    assert_eq!((feature_requests(&up).len(), token_posts(&up)), (1, 0));
    up.shutdown().await;
}

/// The access token each refresh under `token_answer` gives, whatever the generation.
const REFRESHED: &str = "sk-ant-oat01-PLACEHOLDER";

/// The fake of the hold tests: each call answers the upstream's 401, and a call under `REFRESHED` takes the answers a
/// test scripts for that token first.
async fn refreshed_401s() -> FakeUpstream {
    let up = fake(vec![
        ("/api/frame/", expired()),
        ("/v1/oauth/token", token_answer()),
    ])
    .await;
    up.add_credential_rule(REFRESHED, "/api/frame/", expired());
    up
}

#[tokio::test]
async fn an_older_tokens_401_leaves_the_hold_on_a_newer_token() {
    let up = refreshed_401s().await;
    up.push_credential_rule(
        REFRESHED,
        "/api/frame/",
        Respond::Gated(Box::new(expired())),
    );
    let h = harness(&up, &[paired("a", SECRET_A)], both_features_at_a()).await;
    let listener = feature_listener(&h).await;
    // The resend under generation 2 waits for its 401.
    let first = listener
        .hold(API, &keyed_title("/api/frame/artifacts", API))
        .await;
    wait_for(Duration::from_secs(5), || {
        (feature_requests(&up).len() == 2).then_some(())
    })
    .await;
    // Generation 2 goes out, is refreshed to 3, and the resend's 401 holds generation 3.
    let (status, headers, _) = listener
        .send(API, &keyed_title("/api/frame/artifacts", API))
        .await;
    assert_eq!(status, 401, "{headers:?}");
    up.release();
    let (status, headers, _) = first.answer().await;
    assert_eq!(status, 401, "{headers:?}");
    let (status, headers, _) = listener
        .send(API, &keyed_title("/api/frame/artifacts", API))
        .await;
    assert_eq!(status, 401, "{headers:?}");
    assert_eq!((feature_requests(&up).len(), token_posts(&up)), (5, 2));
    up.shutdown().await;
}

#[tokio::test]
async fn an_earlier_sign_ins_401_leaves_the_hold_on_the_current_sign_in() {
    let up = refreshed_401s().await;
    up.push_credential_rule(REFRESHED, "/api/frame/", expired());
    up.push_credential_rule(
        REFRESHED,
        "/api/frame/",
        Respond::Gated(Box::new(expired())),
    );
    let h = harness(&up, &[paired("a", SECRET_A)], both_features_at_a()).await;
    let listener = feature_listener(&h).await;
    // The first sign-in's token reaches generation 2, and the call's resend under generation 3 waits for its 401.
    let row = h.store.account("a").unwrap().unwrap();
    h.manager.refresh_now(&row, 1).await.unwrap();
    let first = listener
        .hold(API, &keyed_title("/api/frame/artifacts", API))
        .await;
    wait_for(Duration::from_secs(5), || {
        (feature_requests(&up).len() == 2).then_some(())
    })
    .await;
    signed_in_again(&h, "a", "sk-ant-oat01-PLACEHOLDER-AGAIN").await;
    // The new sign-in's generation 1 is refreshed to 2, and the resend's 401 holds generation 2 of the new sign-in.
    let (status, headers, _) = listener
        .send(API, &keyed_title("/api/frame/artifacts", API))
        .await;
    assert_eq!(status, 401, "{headers:?}");
    up.release();
    let (status, headers, _) = first.answer().await;
    assert_eq!(status, 401, "{headers:?}");
    let (status, headers, _) = listener
        .send(API, &keyed_title("/api/frame/artifacts", API))
        .await;
    assert_eq!(status, 401, "{headers:?}");
    assert_eq!((feature_requests(&up).len(), token_posts(&up)), (5, 3));
    up.shutdown().await;
}

/// A request under the feature host: `request_line` without its version, the head's `Host: api.anthropic.com`, then
/// `lines`, each ending in CRLF, and `body`.
fn keyless(request_line: &str, lines: &str, body: &[u8]) -> Vec<u8> {
    let mut request =
        format!("{request_line} HTTP/1.1\r\nHost: {API}\r\n{lines}Connection: close\r\n\r\n")
            .into_bytes();
    request.extend_from_slice(body);
    request
}

#[tokio::test]
async fn get_api_hello_answers_200_without_a_key() {
    let up = upstream().await;
    let h = feature_harness(&up).await;
    let (status, headers, body) = feature_listener(&h)
        .await
        .send(API, &keyless("GET /api/hello", "", b""))
        .await;
    assert_eq!(
        (status, &*String::from_utf8_lossy(&body)),
        (200, ""),
        "{headers:?}"
    );
    assert_eq!(
        up.requests().len(),
        0,
        "the first-start check reached the upstream"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn the_domain_check_carries_no_credential() {
    let up = fake(vec![("/api/web/domain_info", json_200())]).await;
    let h = feature_harness(&up).await;
    let (status, headers, _) = feature_listener(&h)
        .await
        .send(
            API,
            &keyless(
                "GET /api/web/domain_info?domain=example.com",
                &format!(
                    "Authorization: Bearer {KEY}\r\nx-api-key: {KEY}\r\nUser-Agent: example/1.0\r\n"
                ),
                b"",
            ),
        )
        .await;
    assert_eq!(status, 200, "{headers:?}");
    let requests = up.requests();
    let [arrived] = requests.as_slice() else {
        panic!("{} requests arrived", requests.len())
    };
    assert_eq!(
        arrived.line,
        "GET /api/web/domain_info?domain=example.com HTTP/1.1"
    );
    let named = |wanted: &str| {
        arrived
            .headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case(wanted))
            .map(|(_, value)| value.as_str())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        (
            named("authorization"),
            named("x-api-key"),
            named("user-agent")
        ),
        (vec![], vec![], vec!["example/1.0"]),
        "{:?}",
        arrived.headers
    );
    up.shutdown().await;
}

#[tokio::test]
async fn the_domain_checks_shape_binds() {
    let up = fake(vec![("/api/web/domain_info", json_200())]).await;
    let h = feature_harness(&up).await;
    let listener = feature_listener(&h).await;
    let past_the_bound = format!("GET /api/web/domain_info?domain={}", "a".repeat(513));
    let at_the_bound = format!("GET /api/web/domain_info?domain={}", "a".repeat(512));
    for (request_line, lines, body) in [
        (
            "GET /api/web/domain_info?domain=example.com&x=1",
            "",
            &b""[..],
        ),
        ("GET /api/web/domain_info?domain=", "", b""),
        (past_the_bound.as_str(), "", b""),
        (
            "GET /api/web/domain_info?domain=example.com",
            "Content-Length: 2\r\n",
            b"{}",
        ),
        ("GET /api/web/domain_info/x?domain=example.com", "", b""),
    ] {
        let (status, headers, _) = listener
            .send(API, &keyless(request_line, lines, body))
            .await;
        assert_eq!(status, 401, "{request_line}: {headers:?}");
    }
    for request_line in ["GET /api/web/domain_info?domain=x", at_the_bound.as_str()] {
        let (status, headers, _) = listener.send(API, &keyless(request_line, "", b"")).await;
        assert_eq!(status, 200, "{request_line}: {headers:?}");
    }
    assert_eq!(
        feature_lines(&up),
        [
            "GET /api/web/domain_info?domain=x HTTP/1.1".to_owned(),
            format!("{at_the_bound} HTTP/1.1"),
        ]
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_check_past_one_sources_share_waits_and_a_freed_place_serves_the_next() {
    let up = fake(vec![(
        "/api/web/domain_info",
        Respond::Gated(Box::new(json_200())),
    )])
    .await;
    let h = harness_with_limits(
        &up,
        &[api_key("acct", "sk-ant-api03-PLACEHOLDER", 0)],
        PoolSpec::default(),
        Limits {
            domain_checks_per_source: 2,
            ..default_limits()
        },
    )
    .await;
    let listener = Arc::new(feature_listener(&h).await);
    let check = || {
        let listener = Arc::clone(&listener);
        tokio::spawn(async move {
            let request = keyless("GET /api/web/domain_info?domain=example.com", "", b"");
            listener.send(API, &request).await.0
        })
    };
    let arrived = |count: usize| {
        let up = &up;
        move || (up.requests().len() == count).then_some(())
    };
    let first = check();
    wait_for(Duration::from_secs(5), arrived(1)).await;
    let second = check();
    wait_for(Duration::from_secs(5), arrived(2)).await;
    let third = check();
    // The cap holds the third check until a place frees, so the window never fails correct code. Its length only
    // bounds how late a check that passed the cap may arrive and still be seen.
    stays_for(Duration::from_millis(200), || up.requests().len() == 2).await;
    up.release_one().await;
    assert_eq!(first.await.unwrap(), 200);
    wait_for(Duration::from_secs(5), arrived(3)).await;
    up.release();
    assert_eq!((second.await.unwrap(), third.await.unwrap()), (200, 200));
    up.shutdown().await;
}

/// The harness of the domain check's limit: `limit` for the whole call and `per_source` places for one address, each
/// other bound at its default, the harness's head bound of ten seconds among them.
async fn domain_check_harness(up: &FakeUpstream, limit: Duration, per_source: usize) -> Harness {
    harness_with_limits(
        up,
        &[api_key("acct", "sk-ant-api03-PLACEHOLDER", 0)],
        PoolSpec::default(),
        Limits {
            domain_check_timeout: limit,
            domain_checks_per_source: per_source,
            ..default_limits()
        },
    )
    .await
}

#[tokio::test]
async fn a_domain_check_whose_upstream_sends_no_head_gets_the_502_at_its_limit() {
    let up = fake(vec![(
        "/api/web/domain_info",
        Respond::Gated(Box::new(json_200())),
    )])
    .await;
    let h = domain_check_harness(&up, Duration::from_millis(500), 4).await;
    let listener = feature_listener(&h).await;
    // Well inside the harness's head bound of ten seconds, which ends a send that keeps no limit of the check's own.
    let (status, headers, _) = tokio::time::timeout(
        Duration::from_secs(5),
        listener.send(
            API,
            &keyless("GET /api/web/domain_info?domain=example.com", "", b""),
        ),
    )
    .await
    .expect("the check ran past its limit");
    let code = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("x-phoebis-error-code"))
        .map(|(_, value)| value.as_str());
    assert_eq!(
        (status, code),
        (502, Some("upstream_unreachable")),
        "{headers:?}"
    );
    assert_eq!(up.requests().len(), 1, "the check never reached the fake");
    up.shutdown().await;
}

#[tokio::test]
async fn a_domain_check_whose_answer_stalls_behind_its_head_ends_at_its_limit_and_frees_its_place()
{
    let up = fake(vec![(
        "/api/web/domain_info",
        Respond::Stall {
            status: 200,
            headers: vec![],
            events: vec![],
        },
    )])
    .await;
    up.set_rule_after(1, "/api/web/domain_info", json_200());
    let h = domain_check_harness(&up, Duration::from_millis(500), 1).await;
    let listener = feature_listener(&h).await;
    let check = keyless("GET /api/web/domain_info?domain=example.com", "", b"");
    let (status, headers, mut body) = listener.open(API, &check).await;
    assert_eq!(status, 200, "{headers:?}");
    // The head has arrived, so its bound plays no part, and the control plane sets no bound between two pieces of a
    // body: the check's limit alone ends the stalled answer, and one with no deadline runs past these five seconds.
    tokio::time::timeout(Duration::from_secs(5), async {
        while body.next().await.is_some() {}
    })
    .await
    .expect("the stalled answer ran past the check's limit");
    let (status, headers, _) = listener.send(API, &check).await;
    assert_eq!(status, 200, "{headers:?}");
    up.shutdown().await;
}

#[tokio::test]
async fn a_domain_check_whose_lookup_takes_most_of_its_limit_gets_the_502_at_its_limit() {
    let up = fake(vec![(
        "/api/web/domain_info",
        Respond::Gated(Box::new(json_200())),
    )])
    .await;
    let h = domain_check_harness(&up, Duration::from_secs(10), 4).await;
    let listener = Arc::new(feature_listener(&h).await);
    tokio::time::pause();
    h.lookup_delay_ms.store(8_000, Ordering::SeqCst);
    let check = {
        let listener = Arc::clone(&listener);
        tokio::spawn(async move {
            let request = keyless("GET /api/web/domain_info?domain=example.com", "", b"");
            listener.send(API, &request).await
        })
    };
    // A timer of the test's own never fires while the clock stands, so each wait yields until its state shows.
    on_held_clock(async {
        while h.lookups_started.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the check made no lookup");
    // Past the lookup's 8 s, since a timer's deadline rounds up to the clock's next millisecond.
    on_held_clock(async {
        tokio::time::advance(Duration::from_millis(8_001)).await;
        while up.requests().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the check never reached the fake");
    // Just past the check's limit at 10 s. The head bound runs from the connect's end, to 18 s.
    let (status, headers, _) = on_held_clock(async {
        tokio::time::advance(Duration::from_millis(2_001)).await;
        check.await.unwrap()
    })
    .await
    .expect("no answer with the clock at the check's limit");
    let code = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("x-phoebis-error-code"))
        .map(|(_, value)| value.as_str());
    assert_eq!(
        (status, code),
        (502, Some("upstream_unreachable")),
        "{headers:?}"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_domain_check_that_waits_out_its_limit_for_a_place_publishes_a_refused_end() {
    let up = fake(vec![("/api/web/domain_info", json_200())]).await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    // A total of no place, so the check's whole limit runs out in its wait for one. A held first check frees its place
    // at its own deadline, which comes ahead of a second's, so the second would get the place and answer the 502.
    let h = harness_with_bus_limits(
        &up,
        &[api_key("acct", "sk-ant-api03-PLACEHOLDER", 0)],
        PoolSpec::default(),
        bus,
        Limits {
            domain_check_timeout: Duration::from_millis(300),
            domain_checks_total: 0,
            ..default_limits()
        },
    )
    .await;
    let listener = feature_listener(&h).await;
    let (status, headers, _) = listener
        .send(
            API,
            &keyless("GET /api/web/domain_info?domain=example.com", "", b""),
        )
        .await;
    let code = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("x-phoebis-error-code"))
        .map(|(_, value)| value.as_str());
    assert_eq!(
        (status, code),
        (429, Some("domain_checks_at_cap")),
        "{headers:?}"
    );
    let end = loop {
        let event = tokio::time::timeout(Duration::from_secs(5), sub.events.recv())
            .await
            .expect("the refused check's end stayed off the bus")
            .expect("the bus stays open");
        if let Event::FeatureCall(call) = event {
            break (call.path, call.end);
        }
    };
    assert_eq!(
        end,
        (
            "/api/web/domain_info".to_owned(),
            FeatureEnd::Refused {
                code: "domain_checks_at_cap",
                feature: None,
                key_id: None,
            }
        )
    );
    up.shutdown().await;
}

/// The answer to a socket request that gets no `101`, its body read to the length its head declares, since the
/// connection stays open behind it.
async fn refused_upgrade(
    listener: &FeatureListener,
    request: &[u8],
) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let (status, headers, mut body) = listener.open(API, request).await;
    let length: usize = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map_or(0, |(_, value)| value.parse().unwrap());
    let mut read = Vec::new();
    while read.len() < length {
        read.extend(
            body.next()
                .await
                .expect("the body ended ahead of its length"),
        );
    }
    (status, headers, read)
}

/// The value of the head's `x-phoebis-error-code` line.
fn error_code(headers: &[(String, String)]) -> Option<&str> {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("x-phoebis-error-code"))
        .map(|(_, value)| value.as_str())
}

/// An error answer's JSON without its request id, which differs for each request.
fn without_request_id(body: &[u8]) -> serde_json::Value {
    let mut value: serde_json::Value = serde_json::from_slice(body).unwrap();
    value.as_object_mut().unwrap().remove("request_id");
    value
}

/// The end the bus delivers for the first feature call on `path`.
async fn end_on(sub: &mut phoebis_bus::Subscriber, path: &str) -> FeatureEnd {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), sub.events.recv())
            .await
            .expect("the call's end stayed off the bus")
            .expect("the bus stays open");
        if let Event::FeatureCall(call) = event {
            if call.path == path {
                return call.end;
            }
        }
    }
}

#[tokio::test]
async fn a_socket_without_an_admitting_token_gets_the_answer_of_a_call_without_a_key() {
    let up = fake(vec![]).await;
    let h = features_harness(&up, both_features_at_a()).await;
    let listener = feature_listener(&h).await;
    let (socket_status, socket_headers, socket_body) =
        refused_upgrade(&listener, &upgrade_request(&sync_token(1_600))).await;
    let (status, headers, body) = listener
        .send(
            API,
            b"GET /off/the/table HTTP/1.1\r\nHost: api.anthropic.com\r\nConnection: close\r\n\r\n",
        )
        .await;
    assert_eq!(socket_status, status);
    assert_eq!(error_code(&socket_headers), error_code(&headers));
    assert_eq!(without_request_id(&socket_body), without_request_id(&body));
    up.shutdown().await;
}

#[tokio::test]
async fn a_socket_without_an_admitting_token_counts_under_its_own_code() {
    let up = fake(vec![]).await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus(&up, &[oauth("a", SECRET_A, 0)], both_features_at_a(), bus).await;
    let listener = feature_listener(&h).await;
    let (status, headers, _) =
        refused_upgrade(&listener, &upgrade_request(&sync_token(1_600))).await;
    assert_eq!(status, 401, "{headers:?}");
    assert_eq!(
        end_on(&mut sub, "/api/frame/sync").await,
        FeatureEnd::Refused {
            code: "socket_token_unknown",
            feature: None,
            key_id: None,
        }
    );
    up.shutdown().await;
}

/// The fake of `api.anthropic.com` for a watch: each page read under `/api/frame/` answered as `page`, and the upgrade
/// request on `/api/frame/sync` as `upgrade`.
async fn watch_fake(page: Respond, upgrade: Respond) -> FakeUpstream {
    fake(vec![("/api/frame/sync", upgrade), ("/api/frame/", page)]).await
}

/// The upstream's `101` to an upgrade request, with the connection handed to `sockets`.
fn switch(sockets: &Switched) -> Respond {
    Respond::Switch {
        headers: vec![
            ("Connection".into(), "Upgrade".into()),
            ("Upgrade".into(), "websocket".into()),
        ],
        first: Vec::new(),
        sockets: sockets.clone(),
    }
}

/// A keyed page read of `/api/frame/x`, answered `200` and read whole.
async fn read_page(listener: &FeatureListener) {
    let (status, headers, _) = listener
        .send(API, &keyed("GET /api/frame/x", API, "", b""))
        .await;
    assert_eq!(status, 200, "{headers:?}");
}

/// The upgrade requests the fake holds, by their heads.
fn upgrade_heads(up: &FakeUpstream) -> Vec<RecordedRequest> {
    up.requests()
        .into_iter()
        .filter(|request| request.line.starts_with("GET /api/frame/sync "))
        .collect()
}

/// `token` with another signature, which no page read handed out.
fn signed_otherwise(token: &str) -> String {
    format!("{}.b3RoZXIgc2lnbmF0dXJl", token.rsplit_once('.').unwrap().0)
}

fn gzip(payload: &[u8]) -> Vec<u8> {
    use std::io::Write as _;

    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(payload).unwrap();
    encoder.finish().unwrap()
}

/// The JSON body `page_answer` answers for `token`.
fn page_body(token: &str) -> Vec<u8> {
    let Respond::Json { body, .. } = page_answer(token) else {
        unreachable!("a page's answer is JSON")
    };
    body
}

#[tokio::test]
async fn a_token_a_page_read_brought_admits_and_one_no_read_brought_is_refused() {
    let sockets = Switched::default();
    let token = sync_token(1_600);
    let up = watch_fake(page_answer(&token), switch(&sockets)).await;
    let h = features_harness(&up, both_features_at_a()).await;
    let listener = feature_listener(&h).await;
    read_page(&listener).await;
    let (status, headers, _socket) = listener.open(API, &upgrade_request(&token)).await;
    assert_eq!(status, 101, "{headers:?}");
    let (status, headers, _) =
        refused_upgrade(&listener, &upgrade_request(&signed_otherwise(&token))).await;
    assert_eq!(status, 401, "{headers:?}");
    assert_eq!(upgrade_heads(&up).len(), 1, "an unread token went upstream");
    up.shutdown().await;
}

#[tokio::test]
async fn a_token_from_a_gzip_answer_admits() {
    let sockets = Switched::default();
    let token = sync_token(1_600);
    let page = Respond::Json {
        status: 200,
        headers: vec![
            ("Content-Type".into(), "application/json".into()),
            ("Content-Encoding".into(), "gzip".into()),
        ],
        body: gzip(&page_body(&token)),
    };
    let up = watch_fake(page, switch(&sockets)).await;
    let h = features_harness(&up, both_features_at_a()).await;
    let listener = feature_listener(&h).await;
    read_page(&listener).await;
    let (status, headers, _socket) = listener.open(API, &upgrade_request(&token)).await;
    assert_eq!(status, 101, "{headers:?}");
    up.shutdown().await;
}

#[tokio::test]
async fn a_token_admits_inside_its_life_and_is_refused_past_its_end() {
    let sockets = Switched::default();
    let token = sync_token(1_600);
    let up = watch_fake(page_answer(&token), switch(&sockets)).await;
    let h = features_harness(&up, both_features_at_a()).await;
    let listener = feature_listener(&h).await;
    read_page(&listener).await;
    h.clock.store(1_599_999, Ordering::SeqCst);
    let (status, headers, _socket) = listener.open(API, &upgrade_request(&token)).await;
    assert_eq!(status, 101, "{headers:?}");
    h.clock.store(1_600_000, Ordering::SeqCst);
    let (status, headers, _) = refused_upgrade(&listener, &upgrade_request(&token)).await;
    assert_eq!(status, 401, "{headers:?}");
    up.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_read_past_a_keys_bound_drops_its_oldest_token() {
    let sockets = Switched::default();
    let up = watch_fake(json_200(), switch(&sockets)).await;
    // One answer for each read in the order the reads arrive, each with a token of its own: part 5 moves by a second
    // from answer to answer.
    for end in 1_000..2_025 {
        up.push_credential_rule(SECRET_A, "/api/frame/", page_answer(&sync_token(end)));
    }
    let h = features_harness(&up, both_features_at_a()).await;
    let listener = Arc::new(feature_listener(&h).await);
    // The first read stores the oldest token and the last read the newest. The reads between them run side by side,
    // since one by one they take a minute.
    read_page(&listener).await;
    let between: Vec<_> = (0..1_023)
        .map(|_| {
            let listener = Arc::clone(&listener);
            tokio::spawn(async move { read_page(&listener).await })
        })
        .collect();
    for read in between {
        read.await.unwrap();
    }
    read_page(&listener).await;
    let (status, headers, _) =
        refused_upgrade(&listener, &upgrade_request(&sync_token(1_000))).await;
    assert_eq!(status, 401, "{headers:?}");
    let (status, headers, _socket) = listener
        .open(API, &upgrade_request(&sync_token(2_024)))
        .await;
    assert_eq!(status, 101, "{headers:?}");
    up.shutdown().await;
}

/// The values of the lines named `name` in the head of the fake's first request whose line starts with `line`.
fn lines_of(up: &FakeUpstream, line: &str, name: &str) -> Vec<String> {
    let requests = up.requests();
    let request = requests
        .iter()
        .find(|request| request.line.starts_with(line))
        .unwrap_or_else(|| panic!("no request {line:?}"));
    request
        .headers
        .iter()
        .filter(|(held, _)| held.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.clone())
        .collect()
}

#[tokio::test]
async fn the_upgrade_lines_leave_on_a_socket_request_alone() {
    let sockets = Switched::default();
    let token = sync_token(1_600);
    let up = watch_fake(page_answer(&token), switch(&sockets)).await;
    let h = features_harness(&up, both_features_at_a()).await;
    let listener = feature_listener(&h).await;
    let (status, headers, _) = listener
        .send(
            API,
            &keyed("GET /api/frame/x", API, "Upgrade: websocket\r\n", b""),
        )
        .await;
    assert_eq!(status, 200, "{headers:?}");
    let (status, headers, _socket) = listener.open(API, &upgrade_request(&token)).await;
    assert_eq!(status, 101, "{headers:?}");
    assert_eq!(
        lines_of(&up, "GET /api/frame/sync ", "connection"),
        ["Upgrade"]
    );
    assert_eq!(
        lines_of(&up, "GET /api/frame/sync ", "upgrade"),
        ["websocket"]
    );
    assert_eq!(lines_of(&up, "GET /api/frame/x ", "connection"), ["close"]);
    assert_eq!(
        lines_of(&up, "GET /api/frame/x ", "upgrade"),
        Vec::<String>::new()
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_socket_request_leaves_with_no_credential_line() {
    let sockets = Switched::default();
    let token = sync_token(1_600);
    let up = watch_fake(page_answer(&token), switch(&sockets)).await;
    let h = features_harness(&up, both_features_at_a()).await;
    let listener = feature_listener(&h).await;
    read_page(&listener).await;
    let mut request = upgrade_request(&token);
    request.truncate(request.len() - 2);
    request.extend_from_slice(format!("x-api-key: {KEY}\r\n\r\n").as_bytes());
    let (status, headers, _socket) = listener.open(API, &request).await;
    assert_eq!(status, 101, "{headers:?}");
    let heads = upgrade_heads(&up);
    let credentials: Vec<&str> = heads[0]
        .headers
        .iter()
        .filter(|(name, _)| {
            name.eq_ignore_ascii_case("authorization") || name.eq_ignore_ascii_case("x-api-key")
        })
        .map(|(name, _)| name.as_str())
        .collect();
    assert_eq!(credentials, Vec::<&str>::new());
    up.shutdown().await;
}

#[tokio::test]
async fn the_101_reaches_the_client_with_its_connection_and_upgrade_lines() {
    let sockets = Switched::default();
    let token = sync_token(1_600);
    let upgrade = Respond::Switch {
        headers: vec![
            ("Connection".into(), "Upgrade".into()),
            ("Upgrade".into(), "websocket".into()),
            ("Sec-WebSocket-Accept".into(), "x".into()),
        ],
        first: Vec::new(),
        sockets: sockets.clone(),
    };
    let up = watch_fake(page_answer(&token), upgrade).await;
    let h = features_harness(&up, both_features_at_a()).await;
    let listener = feature_listener(&h).await;
    read_page(&listener).await;
    let (status, headers, _socket) = listener.open(API, &upgrade_request(&token)).await;
    assert_eq!(status, 101, "{headers:?}");
    let value = |name: &str| {
        headers
            .iter()
            .find(|(held, _)| held.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    };
    assert_eq!(
        (
            value("connection"),
            value("upgrade"),
            value("sec-websocket-accept")
        ),
        (Some("Upgrade"), Some("websocket"), Some("x")),
        "{headers:?}"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn an_upstream_refusal_of_an_admitted_socket_goes_back_with_its_row() {
    let token = sync_token(1_600);
    let refusal = br#"{"error":"an invented refusal"}"#;
    let upgrade = Respond::Json {
        status: 403,
        headers: vec![("Content-Type".into(), "application/json".into())],
        body: refusal.to_vec(),
    };
    let up = watch_fake(page_answer(&token), upgrade).await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus(&up, &[oauth("a", SECRET_A, 0)], both_features_at_a(), bus).await;
    let listener = feature_listener(&h).await;
    read_page(&listener).await;
    let (status, headers, mut body) = listener.open(API, &upgrade_request(&token)).await;
    assert_eq!(status, 403, "{headers:?}");
    // The answer goes back in chunks, and the last one ends it.
    let mut framed = Vec::new();
    while !framed.ends_with(b"0\r\n\r\n") {
        let Some(piece) = body.next().await else {
            break;
        };
        framed.extend(piece);
    }
    assert!(
        framed
            .windows(refusal.len())
            .any(|window| window == refusal),
        "{:?}",
        String::from_utf8_lossy(&framed)
    );
    assert_eq!(
        end_on(&mut sub, "/api/frame/sync").await,
        FeatureEnd::Sent {
            account_id: "a".into(),
            error_code: None,
            feature: "artifacts",
            key_id: "desktop".into(),
            outcome: FeatureOutcome::Forwarded,
            status: Some(403),
        }
    );
    up.shutdown().await;
}

#[tokio::test]
async fn an_admitted_socket_whose_upstream_sends_no_head_gets_the_502_and_a_failed_end() {
    let token = sync_token(1_600);
    let up = watch_fake(page_answer(&token), Respond::CloseAfterRequest).await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus(&up, &[oauth("a", SECRET_A, 0)], both_features_at_a(), bus).await;
    let listener = feature_listener(&h).await;
    read_page(&listener).await;
    let (status, headers, _) = refused_upgrade(&listener, &upgrade_request(&token)).await;
    assert_eq!(
        (status, error_code(&headers)),
        (502, Some("upstream_unreachable")),
        "{headers:?}"
    );
    assert_eq!(
        end_on(&mut sub, "/api/frame/sync").await,
        FeatureEnd::Sent {
            account_id: "a".into(),
            error_code: Some("upstream_unreachable"),
            feature: "artifacts",
            key_id: "desktop".into(),
            outcome: FeatureOutcome::Failed,
            status: None,
        }
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_token_that_straddles_two_pieces_of_the_answer_admits() {
    let sockets = Switched::default();
    let token = sync_token(1_600);
    let body = page_body(&token);
    let start = body
        .windows(token.len())
        .position(|window| window == token.as_bytes())
        .unwrap();
    let cut = start + 10;
    let page = Respond::Pieces {
        status: 200,
        headers: vec![("Content-Type".into(), "application/json".into())],
        pieces: vec![body[..cut].to_vec(), body[cut..].to_vec()],
    };
    let up = watch_fake(page, switch(&sockets)).await;
    let h = features_harness(&up, both_features_at_a()).await;
    let listener = feature_listener(&h).await;
    read_page(&listener).await;
    let (status, headers, _socket) = listener.open(API, &upgrade_request(&token)).await;
    assert_eq!(status, 101, "{headers:?}");
    up.shutdown().await;
}

#[tokio::test]
async fn a_page_read_in_a_coding_the_router_cannot_read_goes_back_whole() {
    let sockets = Switched::default();
    let page = Respond::Json {
        status: 200,
        headers: vec![("Content-Encoding".into(), "compress".into())],
        body: b"invented 16 byte".to_vec(),
    };
    let up = watch_fake(page, switch(&sockets)).await;
    let h = features_harness(&up, both_features_at_a()).await;
    let listener = feature_listener(&h).await;
    let (status, headers, body) = listener
        .send(API, &keyed("GET /api/frame/x", API, "", b""))
        .await;
    assert_eq!(status, 200, "{headers:?}");
    // In its framing, one chunk of the 16 bytes and the last chunk.
    assert_eq!(body, b"10\r\ninvented 16 byte\r\n0\r\n\r\n");
    let (status, headers, _) =
        refused_upgrade(&listener, &upgrade_request(&sync_token(1_600))).await;
    assert_eq!(status, 401, "{headers:?}");
    up.shutdown().await;
}

/// The secret of the harness's second key, `laptop`, in the pool of `desktop`.
const LAPTOP: &str = "phoebis-testkey-laptop";

#[tokio::test]
async fn a_token_that_two_keys_read_admits_no_socket() {
    let sockets = Switched::default();
    let token = sync_token(1_600);
    let up = watch_fake(page_answer(&token), switch(&sockets)).await;
    let h = features_harness(&up, both_features_at_a()).await;
    let desktop = h.store.api_key("desktop").unwrap().unwrap();
    h.store
        .insert_api_key(
            &phoebis_store::ApiKeyRow {
                id: "laptop".into(),
                label: "laptop".into(),
                secret_digest: h.store.api_key_digest(LAPTOP).to_vec(),
                prefix: phoebis_store::key_prefix(LAPTOP),
                ..desktop
            },
            "test",
        )
        .unwrap();
    h.router.reload().await.unwrap();
    let listener = feature_listener(&h).await;
    let laptop_read = format!(
        "GET /api/frame/x HTTP/1.1\r\nHost: {API}\r\nAuthorization: Bearer {LAPTOP}\r\n\
         Connection: close\r\n\r\n"
    );
    read_page(&listener).await;
    read_page(&listener).await;
    let (status, headers, _socket) = listener.open(API, &upgrade_request(&token)).await;
    assert_eq!(status, 101, "{headers:?}");
    let (status, headers, _) = listener.send(API, laptop_read.as_bytes()).await;
    assert_eq!(status, 200, "{headers:?}");
    read_page(&listener).await;
    let (status, headers, _) = refused_upgrade(&listener, &upgrade_request(&token)).await;
    assert_eq!(status, 401, "{headers:?}");
    assert_eq!(
        upgrade_heads(&up).len(),
        1,
        "a second upgrade went upstream"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_socket_request_that_offers_two_tokens_is_refused() {
    let sockets = Switched::default();
    let token = sync_token(1_600);
    let up = watch_fake(page_answer(&token), switch(&sockets)).await;
    let h = features_harness(&up, both_features_at_a()).await;
    let listener = feature_listener(&h).await;
    read_page(&listener).await;
    let offered = format!("{token},{}", sync_token(1_601));
    let (status, headers, _) = refused_upgrade(&listener, &upgrade_request(&offered)).await;
    assert_eq!(status, 401, "{headers:?}");
    assert_eq!(upgrade_heads(&up).len(), 0, "an upgrade went upstream");
    up.shutdown().await;
}

#[tokio::test]
async fn a_page_read_cut_behind_its_token_admits_nothing() {
    let sockets = Switched::default();
    let token = sync_token(1_600);
    let page = Respond::SseCut {
        status: 200,
        headers: vec![],
        events: vec![SseEvent {
            event: "message".into(),
            data: String::from_utf8(page_body(&token)).unwrap(),
        }],
    };
    let up = watch_fake(page, switch(&sockets)).await;
    let h = features_harness(&up, both_features_at_a()).await;
    let listener = feature_listener(&h).await;
    // A cut can end the caller's connection ahead of the head the listener had written, so nothing of the read is
    // checked but its end.
    listener
        .hold(API, &keyed("GET /api/frame/x", API, "", b""))
        .await
        .rest()
        .await;
    let (status, headers, _) = refused_upgrade(&listener, &upgrade_request(&token)).await;
    assert_eq!(status, 401, "{headers:?}");
    up.shutdown().await;
}

#[tokio::test]
async fn a_token_behind_text_that_expands_far_admits() {
    let sockets = Switched::default();
    let token = sync_token(1_600);
    let text = "a".repeat(16 << 20);
    let body = format!(r#"{{"text":"{text}","syncClient":true,"syncToken":"{token}"}}"#);
    let page = Respond::Json {
        status: 200,
        headers: vec![
            ("Content-Type".into(), "application/json".into()),
            ("Content-Encoding".into(), "gzip".into()),
        ],
        body: gzip(body.as_bytes()),
    };
    let up = watch_fake(page, switch(&sockets)).await;
    let h = features_harness(&up, both_features_at_a()).await;
    let listener = feature_listener(&h).await;
    read_page(&listener).await;
    let (status, headers, _socket) = listener.open(API, &upgrade_request(&token)).await;
    assert_eq!(status, 101, "{headers:?}");
    up.shutdown().await;
}

#[tokio::test]
async fn a_token_of_a_deleted_key_is_refused() {
    let sockets = Switched::default();
    let token = sync_token(1_600);
    let up = watch_fake(page_answer(&token), switch(&sockets)).await;
    let h = features_harness(&up, both_features_at_a()).await;
    let listener = feature_listener(&h).await;
    read_page(&listener).await;
    h.store.delete_api_key("desktop", "test").unwrap();
    h.router.reload().await.unwrap();
    let (status, headers, _) = refused_upgrade(&listener, &upgrade_request(&token)).await;
    assert_eq!(status, 401, "{headers:?}");
    up.shutdown().await;
}

#[tokio::test]
async fn a_token_of_a_key_whose_selection_moved_is_refused() {
    let sockets = Switched::default();
    let token = sync_token(1_600);
    let up = watch_fake(page_answer(&token), switch(&sockets)).await;
    let h = harness(
        &up,
        &[
            oauth("a", SECRET_A, 0),
            oauth("b", "sk-ant-oat01-PLACEHOLDER-B", 1),
        ],
        both_features_at_a(),
    )
    .await;
    let listener = feature_listener(&h).await;
    read_page(&listener).await;
    let desktop = h.store.api_key("desktop").unwrap().unwrap();
    h.store
        .update_api_key(
            &phoebis_store::ApiKeyRow {
                feature_accounts: KeyFeatureAccounts {
                    artifacts: FeatureOverride::Account("b".into()),
                    connectors: FeatureOverride::Inherit,
                },
                ..desktop.clone()
            },
            desktop.updated_at,
            "test",
        )
        .unwrap();
    h.router.reload().await.unwrap();
    let (status, headers, _) = refused_upgrade(&listener, &upgrade_request(&token)).await;
    assert_eq!(status, 401, "{headers:?}");
    up.shutdown().await;
}

#[tokio::test]
async fn a_token_read_before_a_new_pairing_is_refused() {
    let sockets = Switched::default();
    let token = sync_token(1_600);
    let up = watch_fake(page_answer(&token), switch(&sockets)).await;
    let h = features_harness(&up, both_features_at_a()).await;
    let listener = feature_listener(&h).await;
    read_page(&listener).await;
    signed_in_again(&h, "a", "sk-ant-oat01-PLACEHOLDER-AGAIN").await;
    let (status, headers, _) = refused_upgrade(&listener, &upgrade_request(&token)).await;
    assert_eq!(status, 401, "{headers:?}");
    up.shutdown().await;
}

#[tokio::test]
async fn an_admitted_socket_gets_the_503_while_the_ledger_stands_behind() {
    let sockets = Switched::default();
    let token = sync_token(1_600);
    let up = watch_fake(page_answer(&token), switch(&sockets)).await;
    let behind = Arc::new(AtomicBool::new(false));
    let h = harness_with_pool_and_accounting(
        &up,
        &[oauth("a", SECRET_A, 0)],
        both_features_at_a(),
        default_limits(),
        standing(&behind),
    )
    .await;
    let listener = feature_listener(&h).await;
    read_page(&listener).await;
    behind.store(true, Ordering::SeqCst);
    let (status, headers, _) = refused_upgrade(&listener, &upgrade_request(&token)).await;
    assert_eq!(refusal_of(status, &headers), (503, "ledger_behind", "5"));
    assert_eq!(upgrade_heads(&up).len(), 0, "an upgrade went upstream");
    up.shutdown().await;
}

#[tokio::test]
async fn a_key_deleted_while_the_upstream_holds_the_upgrade_opens_no_socket() {
    let sockets = Switched::default();
    let token = sync_token(1_600);
    let up = watch_fake(
        page_answer(&token),
        Respond::Gated(Box::new(switch(&sockets))),
    )
    .await;
    let h = features_harness(&up, both_features_at_a()).await;
    let listener = Arc::new(feature_listener(&h).await);
    read_page(&listener).await;
    let upgrade = tokio::spawn({
        let listener = Arc::clone(&listener);
        let request = upgrade_request(&token);
        async move { refused_upgrade(&listener, &request).await.0 }
    });
    wait_for(Duration::from_secs(5), || {
        (upgrade_heads(&up).len() == 1).then_some(())
    })
    .await;
    h.store.delete_api_key("desktop", "test").unwrap();
    h.router.reload().await.unwrap();
    up.release();
    assert_eq!(upgrade.await.unwrap(), 401);
    let mut socket = sockets.next().await;
    let received = tokio::time::timeout(Duration::from_secs(5), socket.received())
        .await
        .expect("the upstream connection stayed open");
    assert_eq!(received, None);
    up.shutdown().await;
}

/// The next `n` bytes the client reads on its side of a socket.
async fn client_reads(body: &mut OpenBody, n: usize) -> Vec<u8> {
    let mut read = Vec::new();
    while read.len() < n {
        read.extend(
            body.next()
                .await
                .expect("the client's side of the socket ended"),
        );
    }
    read
}

/// The next `n` bytes the upstream's side of a socket receives.
async fn upstream_receives(socket: &mut UpstreamSocket, n: usize) -> Vec<u8> {
    let mut received = Vec::new();
    while received.len() < n {
        let piece = tokio::time::timeout(Duration::from_secs(5), socket.received())
            .await
            .expect("no bytes reached the upstream")
            .expect("the upstream's side of the socket ended");
        received.extend(piece);
    }
    received
}

/// A watch on `listener` whose upstream sends `first` with its `101`: the client's side and the upstream's.
async fn tunnel(
    listener: &FeatureListener,
    token: &str,
    sockets: &Switched,
) -> (OpenBody, UpstreamSocket) {
    let (status, headers, client) = listener.open(API, &upgrade_request(token)).await;
    assert_eq!(status, 101, "{headers:?}");
    (client, sockets.next().await)
}

#[tokio::test]
async fn bytes_cross_the_tunnel_both_ways_behind_what_came_with_the_101() {
    let sockets = Switched::default();
    let token = sync_token(1_600);
    let upgrade = Respond::Switch {
        headers: vec![
            ("Connection".into(), "Upgrade".into()),
            ("Upgrade".into(), "websocket".into()),
        ],
        first: b"hello".to_vec(),
        sockets: sockets.clone(),
    };
    let up = watch_fake(page_answer(&token), upgrade).await;
    let h = features_harness(&up, both_features_at_a()).await;
    let listener = feature_listener(&h).await;
    read_page(&listener).await;
    let (mut client, mut socket) = tunnel(&listener, &token, &sockets).await;
    assert_eq!(client_reads(&mut client, 5).await, b"hello");
    socket.send(b"one").await;
    assert_eq!(client_reads(&mut client, 3).await, b"one");
    client.write(b"two").await;
    assert_eq!(upstream_receives(&mut socket, 3).await, b"two");
    up.shutdown().await;
}

#[tokio::test]
async fn the_clients_leave_ends_the_upstream_leg() {
    let sockets = Switched::default();
    let token = sync_token(1_600);
    let up = watch_fake(page_answer(&token), switch(&sockets)).await;
    let h = features_harness(&up, both_features_at_a()).await;
    let listener = feature_listener(&h).await;
    read_page(&listener).await;
    let (mut client, mut socket) = tunnel(&listener, &token, &sockets).await;
    socket.send(b"one").await;
    assert_eq!(client_reads(&mut client, 3).await, b"one");
    drop(client);
    let received = tokio::time::timeout(Duration::from_secs(5), socket.received())
        .await
        .expect("the upstream leg stayed open behind the client's leave");
    assert_eq!(received, None);
    up.shutdown().await;
}

#[tokio::test]
async fn bytes_sent_with_the_upgrade_request_reach_the_upstream_first() {
    let sockets = Switched::default();
    let token = sync_token(1_600);
    let up = watch_fake(page_answer(&token), switch(&sockets)).await;
    let h = features_harness(&up, both_features_at_a()).await;
    let listener = feature_listener(&h).await;
    read_page(&listener).await;
    let early = [upgrade_request(&token), b"early".to_vec()].concat();
    let (status, headers, mut client) = listener.open(API, &early).await;
    assert_eq!(status, 101, "{headers:?}");
    let mut socket = sockets.next().await;
    client.write(b"late").await;
    // Read up to the later bytes, so a tunnel that dropped the early ones shows what came instead.
    let mut received = Vec::new();
    while !received.ends_with(b"late") {
        received.extend(upstream_receives(&mut socket, 1).await);
    }
    assert_eq!(String::from_utf8_lossy(&received), "earlylate");
    up.shutdown().await;
}

/// The client's bytes behind its head keep the listener from reading the reset, so it meets the reset only at the
/// write of the `101`, behind which hyper hands nothing over.
#[tokio::test]
async fn a_socket_whose_101_cannot_reach_its_client_ends_the_upstream_leg() {
    let sockets = Switched::default();
    let token = sync_token(1_600);
    let up = watch_fake(
        page_answer(&token),
        Respond::Gated(Box::new(switch(&sockets))),
    )
    .await;
    let h = features_harness(&up, both_features_at_a()).await;
    let listener = feature_listener(&h).await;
    read_page(&listener).await;
    let early = [upgrade_request(&token), b"early".to_vec()].concat();
    let client = listener.hold(API, &early).await;
    wait_for(Duration::from_secs(5), || {
        (upgrade_heads(&up).len() == 1).then_some(())
    })
    .await;
    client.reset();
    up.release();
    let mut socket = sockets.next().await;
    let received = tokio::time::timeout(Duration::from_secs(5), socket.received())
        .await
        .expect("the upstream leg stayed open behind the client's reset");
    assert_eq!(received, None);
    up.shutdown().await;
}

/// The fake of a watch whose two page reads, both under the account's one bearer, answer `first` and then `second`.
async fn two_reads_fake(first: &str, second: &str, sockets: &Switched) -> FakeUpstream {
    let up = watch_fake(json_200(), switch(sockets)).await;
    up.push_credential_rule(SECRET_A, "/api/frame/", page_answer(first));
    up.push_credential_rule(SECRET_A, "/api/frame/", page_answer(second));
    up
}

/// The harness's second key, `laptop`, in the pool of `desktop`.
async fn add_laptop(h: &Harness) {
    let desktop = h.store.api_key("desktop").unwrap().unwrap();
    h.store
        .insert_api_key(
            &phoebis_store::ApiKeyRow {
                id: "laptop".into(),
                label: "laptop".into(),
                secret_digest: h.store.api_key_digest(LAPTOP).to_vec(),
                prefix: phoebis_store::key_prefix(LAPTOP),
                ..desktop
            },
            "test",
        )
        .unwrap();
    h.router.reload().await.unwrap();
}

/// A page read of `/api/frame/x` under `laptop`, answered `200` and read whole.
async fn read_page_as_laptop(listener: &FeatureListener) {
    let read = format!(
        "GET /api/frame/x HTTP/1.1\r\nHost: {API}\r\nAuthorization: Bearer {LAPTOP}\r\n\
         Connection: close\r\n\r\n"
    );
    let (status, headers, _) = listener.send(API, read.as_bytes()).await;
    assert_eq!(status, 200, "{headers:?}");
}

#[tokio::test]
async fn a_key_at_its_count_gets_the_429_and_a_second_key_is_admitted() {
    let sockets = Switched::default();
    let (desktop_token, laptop_token) = (sync_token(1_600), sync_token(1_601));
    let up = two_reads_fake(&desktop_token, &laptop_token, &sockets).await;
    let limits = Limits {
        key_sockets: 1,
        sockets_total: 16,
        ..default_limits()
    };
    let h = harness_with_limits(
        &up,
        &[oauth("a", SECRET_A, 0)],
        both_features_at_a(),
        limits,
    )
    .await;
    add_laptop(&h).await;
    let listener = feature_listener(&h).await;
    read_page(&listener).await;
    read_page_as_laptop(&listener).await;
    let (status, headers, _open) = listener.open(API, &upgrade_request(&desktop_token)).await;
    assert_eq!(status, 101, "{headers:?}");
    let (status, headers, _) = refused_upgrade(&listener, &upgrade_request(&desktop_token)).await;
    assert_eq!(refusal_of(status, &headers), (429, "key_sockets", ""));
    assert_eq!(
        upgrade_heads(&up).len(),
        1,
        "a refused upgrade went upstream"
    );
    let (status, headers, _laptops) = listener.open(API, &upgrade_request(&laptop_token)).await;
    assert_eq!(status, 101, "{headers:?}");
    up.shutdown().await;
}

#[tokio::test]
async fn a_socket_past_the_total_gets_the_429_of_the_total() {
    let sockets = Switched::default();
    let (desktop_token, laptop_token) = (sync_token(1_600), sync_token(1_601));
    let up = two_reads_fake(&desktop_token, &laptop_token, &sockets).await;
    let limits = Limits {
        key_sockets: 16,
        sockets_total: 1,
        ..default_limits()
    };
    let h = harness_with_limits(
        &up,
        &[oauth("a", SECRET_A, 0)],
        both_features_at_a(),
        limits,
    )
    .await;
    add_laptop(&h).await;
    let listener = feature_listener(&h).await;
    read_page(&listener).await;
    read_page_as_laptop(&listener).await;
    let (status, headers, _open) = listener.open(API, &upgrade_request(&desktop_token)).await;
    assert_eq!(status, 101, "{headers:?}");
    let (status, headers, _) = refused_upgrade(&listener, &upgrade_request(&laptop_token)).await;
    assert_eq!(refusal_of(status, &headers), (429, "sockets_at_cap", ""));
    up.shutdown().await;
}

/// The harness of one key that may hold one open socket.
async fn one_socket_harness(up: &FakeUpstream) -> Harness {
    let limits = Limits {
        key_sockets: 1,
        ..default_limits()
    };
    harness_with_limits(up, &[oauth("a", SECRET_A, 0)], both_features_at_a(), limits).await
}

#[tokio::test]
async fn a_keys_next_socket_is_admitted_once_one_of_its_sockets_ended() {
    let sockets = Switched::default();
    let token = sync_token(1_600);
    let up = watch_fake(page_answer(&token), switch(&sockets)).await;
    let h = one_socket_harness(&up).await;
    let listener = feature_listener(&h).await;
    read_page(&listener).await;
    let (client, mut socket) = tunnel(&listener, &token, &sockets).await;
    drop(client);
    let received = tokio::time::timeout(Duration::from_secs(5), socket.received())
        .await
        .expect("the upstream leg stayed open behind the client's leave");
    assert_eq!(received, None);
    let (status, headers, _next) = listener.open(API, &upgrade_request(&token)).await;
    assert_eq!(status, 101, "{headers:?}");
    up.shutdown().await;
}

#[tokio::test]
async fn an_upgrade_the_upstream_refuses_frees_its_keys_place() {
    let sockets = Switched::default();
    let token = sync_token(1_600);
    let refused = Respond::Stall {
        status: 403,
        headers: vec![],
        events: vec![],
    };
    let up = watch_fake(page_answer(&token), refused).await;
    up.set_rule_after(1, "/api/frame/sync", switch(&sockets));
    let h = one_socket_harness(&up).await;
    let listener = feature_listener(&h).await;
    read_page(&listener).await;
    // The refusal's body waits for the release, and the client keeps its connection with the head alone.
    let (status, headers, _waiting) = listener.open(API, &upgrade_request(&token)).await;
    assert_eq!(status, 403, "{headers:?}");
    let (status, headers, _next) = listener.open(API, &upgrade_request(&token)).await;
    assert_eq!(status, 101, "{headers:?}");
    up.release();
    up.shutdown().await;
}

#[tokio::test]
async fn a_socket_stays_open_while_bytes_move_and_ends_once_it_is_silent_past_its_idle_time() {
    let sockets = Switched::default();
    let token = sync_token(1_600);
    let up = watch_fake(page_answer(&token), switch(&sockets)).await;
    let limits = Limits {
        socket_idle_timeout: Duration::from_secs(5),
        ..default_limits()
    };
    let h = harness_with_limits(
        &up,
        &[oauth("a", SECRET_A, 0)],
        both_features_at_a(),
        limits,
    )
    .await;
    let listener = feature_listener(&h).await;
    read_page(&listener).await;
    // On the running clock, since the fake hands its side of the socket over behind a timer of its own.
    let (mut client, mut socket) = tunnel(&listener, &token, &sockets).await;
    tokio::time::pause();
    // A byte at an instant of the paused clock, whatever real time the opening took, from which each step counts.
    on_held_clock(async {
        client.write(b"z").await;
        assert_eq!(upstream_receives(&mut socket, 1).await, b"z");
    })
    .await
    .expect("z never reached the upstream");
    on_held_clock(async {
        tokio::time::advance(Duration::from_millis(4_999)).await;
        client.write(b"a").await;
        assert_eq!(upstream_receives(&mut socket, 1).await, b"a");
    })
    .await
    .expect("a never reached the upstream");
    on_held_clock(async {
        tokio::time::advance(Duration::from_millis(4_999)).await;
        socket.send(b"b").await;
        assert_eq!(client_reads(&mut client, 1).await, b"b");
    })
    .await
    .expect("b never reached the client");
    on_held_clock(async {
        tokio::time::advance(Duration::from_millis(4_999)).await;
        client.write(b"c").await;
        assert_eq!(upstream_receives(&mut socket, 1).await, b"c");
    })
    .await
    .expect("c never reached the upstream");
    let ends = on_held_clock(async {
        tokio::time::advance(Duration::from_millis(5_001)).await;
        (client.next().await, socket.received().await)
    })
    .await
    .expect("the socket stayed open past its idle time");
    assert_eq!(ends, (None, None));
    up.shutdown().await;
}

#[tokio::test]
async fn a_socket_whose_account_no_longer_serves_is_cut_at_its_next_check_and_one_beside_it_stays()
{
    let secret_b = "sk-ant-oat01-PLACEHOLDER-B";
    let sockets = Switched::default();
    let (desktop_token, laptop_token) = (sync_token(1_600), sync_token(1_601));
    // A token that two keys read admits no socket, so each key's read, under its own account's bearer, gets its own.
    let up = watch_fake(json_200(), switch(&sockets)).await;
    up.push_credential_rule(SECRET_A, "/api/frame/", page_answer(&desktop_token));
    up.push_credential_rule(secret_b, "/api/frame/", page_answer(&laptop_token));
    let limits = Limits {
        socket_check_interval: Duration::from_secs(30),
        socket_idle_timeout: Duration::from_secs(600),
        ..default_limits()
    };
    let h = harness_with_limits(
        &up,
        &[oauth("a", SECRET_A, 0), oauth("b", secret_b, 1)],
        both_features_at_a(),
        limits,
    )
    .await;
    add_laptop(&h).await;
    let laptop = h.store.api_key("laptop").unwrap().unwrap();
    h.store
        .update_api_key(
            &phoebis_store::ApiKeyRow {
                feature_accounts: KeyFeatureAccounts {
                    artifacts: FeatureOverride::Account("b".into()),
                    connectors: FeatureOverride::Inherit,
                },
                ..laptop.clone()
            },
            laptop.updated_at,
            "test",
        )
        .unwrap();
    h.router.reload().await.unwrap();
    let listener = feature_listener(&h).await;
    read_page(&listener).await;
    read_page_as_laptop(&listener).await;
    let (mut cut, mut cut_socket) = tunnel(&listener, &desktop_token, &sockets).await;
    let (mut kept, mut kept_socket) = tunnel(&listener, &laptop_token, &sockets).await;
    tokio::time::pause();
    on_held_clock(async {
        h.store
            .set_canceled("a", Cancellation::On { ends_at: Some(NOW) })
            .unwrap();
        h.router.reload().await.unwrap();
    })
    .await
    .expect("the reload never ended");
    on_held_clock(tokio::time::advance(Duration::from_millis(30_001)))
        .await
        .expect("the clock never moved on");
    let ends = on_held_clock(async { (cut.next().await, cut_socket.received().await) })
        .await
        .expect("the socket of the ended account stayed open past its check");
    assert_eq!(ends, (None, None));
    on_held_clock(async {
        kept.write(b"x").await;
        assert_eq!(upstream_receives(&mut kept_socket, 1).await, b"x");
    })
    .await
    .expect("x never reached the upstream");
    up.shutdown().await;
}
