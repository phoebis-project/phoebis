//! What the relay writes upstream for a caller the profile never measured: the caller's own headers, in the
//! caller's order and spelling, with nothing invented for the slots it left out.

use std::net::SocketAddr;
use std::sync::Arc;

use phoebis_relay::Relay;
use phoebis_store::AccountKind;
use fake_upstream::{FakeUpstream, RecordedRequest, Respond, Rule};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tower::ServiceExt;

mod common;
use common::*;

const BODY: &[u8] = br#"{"model":"claude-sonnet-4-5","messages":[]}"#;

/// A head a test writes itself, in the shape the recorder hands the relay.
fn head(lines: &[String]) -> Vec<u8> {
    let mut raw = String::from("POST /v1/messages HTTP/1.1\r\n");
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

/// The listener the two socket tests drive, and the stop channel that drains it.
async fn listening(relay: Arc<Relay>) -> (SocketAddr, tokio::sync::watch::Sender<bool>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stop_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(phoebis_relay::serve(listener, relay, stop_rx));
    (addr, stop)
}

fn raw_request(addr: SocketAddr, trace: &str) -> Vec<u8> {
    let mut out = format!(
        "POST /v1/messages HTTP/1.1\r\nHost: {addr}\r\nAccePT: application/json\r\n\
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
