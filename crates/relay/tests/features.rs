//! The feature module as the feature listener serves it, over TLS under a pair of the test's own making: the key is
//! checked as on a keyed route of the proxy listener, no model route answers there, and no feature path answers on the
//! proxy listener.

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule};
use phoebis_relay::Relay;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

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
