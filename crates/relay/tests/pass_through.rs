//! What the relay writes upstream for a caller the profile never measured: the caller's own headers, in the
//! caller's order and spelling, with nothing invented for the slots it left out.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use phoebis_relay::Relay;
use phoebis_store::{AccountKind, ApiKeyRow};
use fake_upstream::{FakeUpstream, RecordedRequest, Respond, Rule};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tower::ServiceExt;

mod common;
use common::*;

const BODY: &[u8] = br#"{"model":"claude-sonnet-4-5","messages":[]}"#;

/// A head a test writes itself, in the shape the recorder hands the relay.
fn head(lines: &[String]) -> Vec<u8> {
    let mut raw = String::from("POST /v1/messages?beta=true HTTP/1.1\r\n");
    for line in lines {
        raw.push_str(line);
        raw.push_str("\r\n");
    }
    raw.push_str("\r\n");
    raw.into_bytes()
}

fn lines(lines: &[&str], body: &[u8]) -> Vec<u8> {
    let mut all: Vec<String> = lines.iter().map(|l| (*l).to_string()).collect();
    all.push(format!("Content-Length: {}", body.len()));
    head(&all)
}

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

fn names(request: &RecordedRequest) -> Vec<&str> {
    request.headers.iter().map(|(n, _)| n.as_str()).collect()
}

fn value<'a>(request: &'a RecordedRequest, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

#[tokio::test]
async fn a_custom_caller_header_reaches_the_upstream_verbatim_and_in_its_own_slot() {
    let up = upstream().await;
    let app = relay_with(AccountKind::ApiKey, &up).await;
    let raw = lines(
        &[
            "Accept: application/json",
            "x-api-key: sk-ant-api03-PLACEHOLDER",
            "X-Corp-Trace: 9f2a/BETA",
            "anthropic-version: 2023-06-01",
        ],
        BODY,
    );
    let resp = app
        .oneshot(request_from_fixture(&raw, BODY, KEY))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let got = &up.requests()[0];
    assert_eq!(
        got.headers,
        vec![
            ("Accept".to_string(), "application/json".to_string()),
            (
                "x-api-key".to_string(),
                "sk-ant-api03-PLACEHOLDER".to_string()
            ),
            ("X-Corp-Trace".to_string(), "9f2a/BETA".to_string()),
            ("anthropic-version".to_string(), "2023-06-01".to_string()),
            ("Content-Length".to_string(), BODY.len().to_string()),
            ("Host".to_string(), "api.anthropic.com".to_string()),
        ],
        "the header the capture never had travels verbatim, in the slot the caller gave it"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_caller_that_omits_the_measured_headers_has_nothing_invented_for_it() {
    let up = upstream().await;
    let app = relay_with(AccountKind::ApiKey, &up).await;
    let raw = lines(
        &[
            "Accept: application/json",
            "x-api-key: sk-ant-api03-PLACEHOLDER",
            "anthropic-version: 2023-06-01",
        ],
        BODY,
    );
    let resp = app
        .oneshot(request_from_fixture(&raw, BODY, KEY))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let got = &up.requests()[0];
    for absent in [
        "accept-encoding",
        "anthropic-beta",
        "user-agent",
        "x-app",
        "x-stainless-arch",
        "x-stainless-lang",
        "x-stainless-retry-count",
        "x-stainless-timeout",
    ] {
        assert_eq!(
            value(got, absent),
            None,
            "{absent} was invented for a caller that never sent it"
        );
    }
    up.shutdown().await;
}

#[tokio::test]
async fn an_oauth_account_adds_the_oauth_beta_once_and_an_api_key_account_strips_it() {
    let up = upstream().await;
    let caller = "fine-grained-tool-streaming-2025-05-14,claude-code-20250219";
    let native = phoebis_profile::fixtures::body(phoebis_profile::fixtures::BodyFixture::Oauth);
    let raw = lines(
        &[
            "Authorization: Bearer sk-ant-oat01-PLACEHOLDER",
            &format!("anthropic-beta: {caller}"),
            "anthropic-version: 2023-06-01",
        ],
        native,
    );
    let app = relay_with(AccountKind::Oauth, &up).await;
    let resp = app
        .oneshot(request_from_fixture(&raw, native, KEY))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        value(&up.requests()[0], "anthropic-beta"),
        Some(format!("oauth-2025-04-20,{caller}").as_str()),
        "the beta the credential claims leads the caller's own list, which keeps its order"
    );

    let claimed = "fine-grained-tool-streaming-2025-05-14,oauth-2025-04-20,claude-code-20250219";
    let raw = lines(
        &[
            "x-api-key: sk-ant-api03-PLACEHOLDER",
            &format!("anthropic-beta: {claimed}"),
            "anthropic-version: 2023-06-01",
        ],
        BODY,
    );
    let key_up = upstream().await;
    let app = relay_with(AccountKind::ApiKey, &key_up).await;
    let resp = app
        .oneshot(request_from_fixture(&raw, BODY, KEY))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        value(&key_up.requests()[0], "anthropic-beta"),
        Some("fine-grained-tool-streaming-2025-05-14,claude-code-20250219"),
        "an api key may not claim the oauth beta, and loses only that entry"
    );
    up.shutdown().await;
    key_up.shutdown().await;
}

#[tokio::test]
async fn a_head_in_an_order_the_capture_never_had_travels_in_that_order() {
    let up = upstream().await;
    let app = relay_with(AccountKind::ApiKey, &up).await;
    let raw = head(&[
        format!("Content-Length: {}", BODY.len()),
        "X-Stainless-Lang: js".into(),
        "Accept: application/json".into(),
        "x-api-key: sk-ant-api03-PLACEHOLDER".into(),
        "Host: proxy.example.invalid".into(),
        "anthropic-version: 2023-06-01".into(),
    ]);
    let resp = app
        .oneshot(request_from_fixture(&raw, BODY, KEY))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let got = &up.requests()[0];
    assert_eq!(
        names(got),
        [
            "Content-Length",
            "X-Stainless-Lang",
            "Accept",
            "x-api-key",
            "Host",
            "anthropic-version",
        ],
        "the relay imposes no order of its own, not even on the slots it owns"
    );
    assert_eq!(value(got, "host"), Some("api.anthropic.com"));
    up.shutdown().await;
}

/// The listener the socket tests drive, and the stop channel that drains it.
async fn listening(relay: Arc<Relay>) -> (SocketAddr, tokio::sync::watch::Sender<bool>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stop_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(phoebis_relay::serve(
        listener,
        relay,
        axum::Router::new(),
        stop_rx,
        phoebis_relay::ListenLimits {
            header_read_timeout: Duration::from_secs(30),
            max_connections: 64,
        },
    ));
    (addr, stop)
}

fn raw_request(addr: SocketAddr, trace: &str) -> Vec<u8> {
    let mut out = format!(
        "POST /v1/messages?beta=true HTTP/1.1\r\nHost: {addr}\r\nAccePT: application/json\r\n\
         X-CoRp-TrAcE: {trace}\r\nx-api-key: {KEY}\r\nanthropic-VERSION: 2023-06-01\r\n\
         Content-Length: {}\r\n\r\n",
        BODY.len()
    )
    .into_bytes();
    out.extend_from_slice(BODY);
    out
}

/// Reads one whole response off the socket, so the connection is ready for the next request. A relayed body
/// reaches the caller in hyper's chunked framing, so both endings have to be recognised.
async fn read_response(socket: &mut TcpStream) -> String {
    let mut buffer = Vec::new();
    loop {
        let mut chunk = [0u8; 512];
        let read = socket.read(&mut chunk).await.unwrap();
        assert!(read > 0, "the connection closed mid-response");
        buffer.extend_from_slice(&chunk[..read]);
        let text = String::from_utf8_lossy(&buffer).to_string();
        let Some(head_end) = text.find("\r\n\r\n") else {
            continue;
        };
        let declared = text[..head_end].split("\r\n").find_map(|line| {
            let lower = line.to_ascii_lowercase();
            lower
                .strip_prefix("content-length:")?
                .trim()
                .parse::<usize>()
                .ok()
        });
        let complete = match declared {
            Some(length) => buffer.len() - head_end - 4 >= length,
            None => text.ends_with("0\r\n\r\n"),
        };
        if complete {
            return text;
        }
    }
}

async fn socket_harness(up: &FakeUpstream) -> Harness {
    harness(
        up,
        &[api_key("acct", "sk-ant-api03-PLACEHOLDER", 0)],
        PoolSpec::default(),
    )
    .await
}

#[tokio::test]
async fn a_head_read_off_a_socket_reaches_the_upstream_with_the_callers_own_spelling() {
    let up = upstream().await;
    let h = socket_harness(&up).await;
    let (addr, stop) = listening(h.relay.clone()).await;
    let mut socket = TcpStream::connect(addr).await.unwrap();
    socket.write_all(&raw_request(addr, "first")).await.unwrap();
    assert!(read_response(&mut socket)
        .await
        .starts_with("HTTP/1.1 200 "));
    assert_eq!(
        names(&up.requests()[0]),
        [
            "Host",
            "AccePT",
            "X-CoRp-TrAcE",
            "x-api-key",
            "anthropic-VERSION",
            "Content-Length",
        ],
        "hyper's lowercase map is not what the upstream sees"
    );
    assert_eq!(value(&up.requests()[0], "host"), Some("api.anthropic.com"));
    let _ = stop.send(true);
    up.shutdown().await;
}

/// A caller may write the same line twice. Hyper keeps one `Content-Length` in its map, but the recorded head
/// holds both lines, and a relay-owned slot filled once per line would frame the upstream request twice and
/// claim the OAuth beta twice.
#[tokio::test]
async fn a_duplicated_caller_line_fills_a_relay_owned_slot_once() {
    let up = upstream().await;
    let h = harness(
        &up,
        &[oauth("acct", "sk-ant-oat01-PLACEHOLDER", 0)],
        PoolSpec::default(),
    )
    .await;
    let (addr, stop) = listening(h.relay.clone()).await;
    let native = phoebis_profile::fixtures::body(phoebis_profile::fixtures::BodyFixture::Oauth);
    let mut raw = format!(
        "POST /v1/messages?beta=true HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {KEY}\r\n\
         anthropic-beta: claude-code-20250219\r\nanthropic-beta: claude-code-20250219\r\n\
         anthropic-version: 2023-06-01\r\nContent-Length: {length}\r\nContent-Length: {length}\r\n\r\n",
        length = native.len()
    )
    .into_bytes();
    raw.extend_from_slice(native);
    let mut socket = TcpStream::connect(addr).await.unwrap();
    socket.write_all(&raw).await.unwrap();
    assert!(read_response(&mut socket)
        .await
        .starts_with("HTTP/1.1 200 "));
    let got = &up.requests()[0];
    let sent = |name: &str| {
        got.headers
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case(name))
            .count()
    };
    assert_eq!(sent("content-length"), 1, "the framing is written once");
    assert_eq!(sent("anthropic-beta"), 1, "the beta list is written once");
    assert_eq!(
        value(got, "anthropic-beta"),
        Some("oauth-2025-04-20,claude-code-20250219"),
        "the credential's beta leads the list the caller sent, once"
    );
    let _ = stop.send(true);
    up.shutdown().await;
}

/// A request head written with one line ending or the other, so a test can send what hyper accepts and the
/// recorder has to agree with.
fn session_request(addr: SocketAddr, eol: &str, session: &str, key: &str) -> Vec<u8> {
    let mut out = [
        "POST /v1/messages?beta=true HTTP/1.1",
        &format!("Host: {addr}"),
        &format!("X-Session: {session}"),
        &format!("x-api-key: {key}"),
        "anthropic-version: 2023-06-01",
        &format!("Content-Length: {}", BODY.len()),
        "",
        "",
    ]
    .join(eol)
    .into_bytes();
    out.extend_from_slice(BODY);
    out
}

/// hyper ends a head at a bare-LF blank line, so a recorder that waits for `\r\n\r\n` is still holding the
/// first request when the second one arrives and hands the second request the first one's headers.
#[tokio::test]
async fn an_lf_terminated_head_ends_where_hyper_ends_it() {
    const SECOND: &str = "phoebis-secondkey";
    let up = upstream().await;
    let h = socket_harness(&up).await;
    h.store
        .insert_api_key(
            &ApiKeyRow {
                id: "second".into(),
                label: "second".into(),
                notes: None,
                pool_id: "main".into(),
                enabled: true,
                revoked_at: None,
                expires_at: None,
                allowed_models: None,
                keepalive_opt_out: false,
                body_capture_hours: None,
                secret_digest: h.store.api_key_digest(SECOND).to_vec(),
                prefix: phoebis_store::key_prefix(SECOND),
                created_at: NOW,
                rotated_at: None,
                last_used_at: None,
                retired_secret_last_used_at: None,
            },
            "test",
        )
        .unwrap();
    h.router.reload().await.unwrap();
    let (addr, stop) = listening(h.relay.clone()).await;
    let mut socket = TcpStream::connect(addr).await.unwrap();
    socket
        .write_all(&session_request(addr, "\n", "first", KEY))
        .await
        .unwrap();
    assert!(read_response(&mut socket)
        .await
        .starts_with("HTTP/1.1 200 "));
    socket
        .write_all(&session_request(addr, "\r\n", "second", SECOND))
        .await
        .unwrap();
    assert!(read_response(&mut socket)
        .await
        .starts_with("HTTP/1.1 200 "));
    let seen = up.requests();
    assert_eq!(seen.len(), 2, "both requests reached the upstream");
    assert_eq!(
        value(&seen[1], "x-session"),
        Some("second"),
        "the second request travels under its own head, not the head hyper already finished with"
    );
    let _ = stop.send(true);
    up.shutdown().await;
}

/// A caller may split a list-valued header across several lines. Both lines name betas the caller asked for,
/// so both have to reach the upstream, on the one line the relay is allowed to write.
#[tokio::test]
async fn two_beta_lines_reach_the_upstream_as_one_list() {
    let up = upstream().await;
    let h = harness(
        &up,
        &[oauth("acct", "sk-ant-oat01-PLACEHOLDER", 0)],
        PoolSpec::default(),
    )
    .await;
    let (addr, stop) = listening(h.relay.clone()).await;
    let native = phoebis_profile::fixtures::body(phoebis_profile::fixtures::BodyFixture::Oauth);
    let mut raw = format!(
        "POST /v1/messages?beta=true HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {KEY}\r\n\
         anthropic-beta: claude-code-20250219\r\nanthropic-beta: interleaved-thinking-2025-05-14\r\n\
         anthropic-version: 2023-06-01\r\nContent-Length: {length}\r\n\r\n",
        length = native.len()
    )
    .into_bytes();
    raw.extend_from_slice(native);
    let mut socket = TcpStream::connect(addr).await.unwrap();
    socket.write_all(&raw).await.unwrap();
    assert!(read_response(&mut socket)
        .await
        .starts_with("HTTP/1.1 200 "));
    let got = &up.requests()[0];
    assert_eq!(
        got.headers
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case("anthropic-beta"))
            .count(),
        1,
        "the beta list is written once"
    );
    assert_eq!(
        value(got, "anthropic-beta"),
        Some("oauth-2025-04-20,claude-code-20250219,interleaved-thinking-2025-05-14"),
        "every line's tokens are entries in one list, behind the credential's own claim"
    );
    let _ = stop.send(true);
    up.shutdown().await;
}

#[tokio::test]
async fn the_second_request_on_a_keep_alive_connection_keeps_its_own_spelling_too() {
    let up = upstream().await;
    let h = socket_harness(&up).await;
    let (addr, stop) = listening(h.relay.clone()).await;
    let mut socket = TcpStream::connect(addr).await.unwrap();
    for trace in ["first", "second"] {
        socket.write_all(&raw_request(addr, trace)).await.unwrap();
        assert!(read_response(&mut socket)
            .await
            .starts_with("HTTP/1.1 200 "));
    }
    let seen = up.requests();
    assert_eq!(seen.len(), 2, "both requests reached the upstream");
    for (request, trace) in seen.iter().zip(["first", "second"]) {
        assert_eq!(
            names(request),
            [
                "Host",
                "AccePT",
                "X-CoRp-TrAcE",
                "x-api-key",
                "anthropic-VERSION",
                "Content-Length",
            ],
            "the recorder skipped the first body and read the second head"
        );
        assert_eq!(value(request, "x-corp-trace"), Some(trace));
    }
    let _ = stop.send(true);
    up.shutdown().await;
}
