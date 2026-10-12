//! A request body sent with `content-encoding: gzip`: inflated within the body bound before any read of it,
//! edited as plaintext with the client's tail in place, and sent toward the upstream as `upstream_gzip` says.

mod common;

use std::io::Write;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use bytes::Bytes;
use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule};
use http_body_util::BodyExt;
use phoebis_bus::{Bus, Delivery, Event};
use phoebis_profile::fixtures::{body, inference_head, BodyFixture, FixtureKind};
use phoebis_relay::{Limits, UpstreamGzip};
use phoebis_store::AccountKind;
use phoebis_transport::decode::{decoder, Encoding};
use tower::ServiceExt;

const A: &str = "sk-ant-oat01-AAAA";
const MODEL: &str = "claude-sonnet-4-5";

/// The one OAuth account every harness here selects, carrying `ACCOUNT_UUID` so the edited body names it.
fn account() -> TestAccount {
    TestAccount {
        id: "acct",
        kind: AccountKind::Oauth,
        secret: "sk-ant-oat01-PLACEHOLDER",
        priority: 0,
        uuid: Some(ACCOUNT_UUID),
    }
}

/// What the client appends to the JSON before it gzips a body: a separator and a run of spaces.
fn tail() -> Vec<u8> {
    let mut tail = b"\n".to_vec();
    tail.extend(std::iter::repeat_n(b' ', 37));
    tail
}

fn tailed(json: &[u8]) -> Vec<u8> {
    let mut out = json.to_vec();
    out.extend(tail());
    out
}

fn gzipped(plain: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(plain).expect("a Vec never fails a write");
    encoder.finish().expect("the encoder flushes into the Vec")
}

fn inflated(compressed: &[u8]) -> Vec<u8> {
    let mut d = decoder(Encoding::Gzip);
    let mut out = d.feed_bounded(compressed, 64 << 20).unwrap();
    out.extend(d.finish_bounded((64 << 20) - out.len()).unwrap());
    out
}

/// `plain` gzipped on a fixture head, as the client sends a compressed body: `content-encoding: gzip` and the
/// compressed length.
fn gzip_request(kind: FixtureKind, plain: &[u8]) -> Request<Body> {
    let compressed = gzipped(plain);
    let mut req = request_from_fixture(inference_head(kind), &compressed, KEY);
    set_header(&mut req, "content-encoding", "gzip");
    set_header(&mut req, "content-length", &compressed.len().to_string());
    req
}

/// The reduced-shape fixture body with the tail, gzipped, on the head and session `request_for` builds.
fn reduced_shape_gzip(session: &str) -> Request<Body> {
    let mut req = gzip_request(
        FixtureKind::OauthNonFirstParty,
        &tailed(body(BodyFixture::Oauth)),
    );
    set_header(&mut req, "x-claude-code-session-id", session);
    req
}

/// A Messages body that passes admission, `size` bytes long: the model and the account's `metadata.user_id`,
/// as the fixture carries them, and the rest spaces inside the one message's text.
fn padded_body(size: usize) -> Vec<u8> {
    let start = format!(
        r#"{{"model":"{MODEL}","max_tokens":1,"stream":true,"metadata":{{"user_id":"{{\"device_id\":\"d\",\"account_uuid\":\"{ACCOUNT_UUID}\",\"session_id\":\"s1\"}}"}},"messages":[{{"role":"user","content":""#
    );
    let end = r#""}]}"#;
    let mut out = start.into_bytes();
    out.resize(size - end.len(), b' ');
    out.extend_from_slice(end.as_bytes());
    out
}

fn padded_gzip_request(size: usize, session: &str) -> Request<Body> {
    let mut req = gzip_request(FixtureKind::OauthNonFirstParty, &padded_body(size));
    set_header(&mut req, "x-claude-code-session-id", session);
    req
}

fn messages(respond: Respond) -> Vec<Rule> {
    vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond,
    }]
}

fn json_429_retry_after(seconds: &str) -> Respond {
    Respond::Json {
        status: 429,
        headers: vec![("retry-after".into(), seconds.into())],
        body: br#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#
            .to_vec(),
    }
}

/// Every value of the lines named `name`, so a test sees a duplicate as well as a missing line.
fn lines<'a>(r: &'a fake_upstream::RecordedRequest, name: &str) -> Vec<&'a str> {
    r.headers
        .iter()
        .filter(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
        .collect()
}

/// The JSON before the tail, after checking the tail follows it byte for byte.
fn json_before_tail(plain: &[u8]) -> serde_json::Value {
    let tail = tail();
    assert!(
        plain.ends_with(&tail),
        "the tail survives: {:?}",
        String::from_utf8_lossy(&plain[plain.len().saturating_sub(48)..])
    );
    serde_json::from_slice(&plain[..plain.len() - tail.len()]).expect("the JSON before the tail")
}

fn account_uuid(value: &serde_json::Value) -> String {
    let user_id: serde_json::Value =
        serde_json::from_str(value["metadata"]["user_id"].as_str().unwrap()).unwrap();
    user_id["account_uuid"].as_str().unwrap().to_string()
}

fn cch_digits(plain: &[u8]) -> Option<Vec<u8>> {
    phoebis_cch::find_cch(plain).map(|l| plain[l.digits_start..l.digits_start + 5].to_vec())
}

async fn drain(resp: axum::http::Response<Body>) -> Vec<u8> {
    tokio::time::timeout(Duration::from_secs(5), resp.into_body().collect())
        .await
        .expect("the body ends")
        .unwrap()
        .to_bytes()
        .to_vec()
}

async fn observed_and_completed(
    sub: &mut phoebis_bus::Subscriber,
    request_id: &str,
) -> (phoebis_bus::RequestObserved, phoebis_bus::RequestComplete) {
    let mut events = Vec::new();
    wait_for(Duration::from_secs(5), || {
        while let Ok(e) = sub.events.try_recv() {
            events.push(e);
        }
        let observed = events.iter().find_map(|e| match e {
            Event::RequestObserved(o) if o.request_id == request_id => Some(o.clone()),
            _ => None,
        })?;
        let complete = events.iter().find_map(|e| match e {
            Event::RequestComplete(c) if c.request_id == request_id => Some(c.clone()),
            _ => None,
        })?;
        Some((observed, complete))
    })
    .await
}

#[tokio::test]
async fn a_gzip_body_is_inflated_edited_and_relayed_with_its_tail_unchanged() {
    let up = FakeUpstream::start(messages(json_200())).await;
    let h = harness_with(&up, &[account()], PoolSpec::default(), default_limits()).await;
    let resp = h
        .app
        .clone()
        .oneshot(reduced_shape_gzip("s-1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let sent = &up.requests()[0];
    assert_eq!(
        lines(sent, "content-encoding"),
        ["gzip"],
        "auto mirrors the client"
    );
    assert_eq!(
        lines(sent, "content-length"),
        [sent.body.len().to_string()],
        "the length of the encoded bytes"
    );
    let plain = inflated(&sent.body);
    let edited = json_before_tail(&plain);
    assert_eq!(
        account_uuid(&edited),
        ACCOUNT_UUID,
        "the account's identity spliced in"
    );
    assert_eq!(edited["model"], MODEL);
    assert_eq!(
        cch_digits(&plain).as_deref(),
        Some(&b"00000"[..]),
        "the restored placeholder, unsigned"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_client_stamped_body_with_a_tail_passes_the_drift_check_and_is_relayed() {
    // The client stamps the bytes it sends, the tail among them.
    let stamped = phoebis_cch::sign(&tailed(body(BodyFixture::FirstParty9b244))).unwrap();
    let up = FakeUpstream::start(messages(json_200())).await;
    let h = harness_with(&up, &[account()], PoolSpec::default(), default_limits()).await;
    let resp = h
        .app
        .clone()
        .oneshot(gzip_request(FixtureKind::OauthFirstParty, &stamped))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(outcome(&resp).client_drift, None);
    let plain = inflated(&up.requests()[0].body);
    let edited = json_before_tail(&plain);
    assert_eq!(account_uuid(&edited), ACCOUNT_UUID);
    assert_eq!(edited["model"], MODEL);
    assert_eq!(
        phoebis_cch::sign(&plain).unwrap(),
        plain,
        "the restamp signs the edited plaintext as a whole, tail included"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn with_the_switch_on_the_signed_placeholder_covers_the_tail() {
    let up = FakeUpstream::start(messages(json_200())).await;
    let h = harness_with_signed_placeholder(&up, &[account()], PoolSpec::default()).await;
    let resp = h
        .app
        .clone()
        .oneshot(reduced_shape_gzip("s-1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let plain = inflated(&up.requests()[0].body);
    let edited = json_before_tail(&plain);
    assert_eq!(account_uuid(&edited), ACCOUNT_UUID);
    assert_eq!(phoebis_cch::sign(&plain).unwrap(), plain);
    assert_ne!(cch_digits(&plain).as_deref(), Some(&b"00000"[..]));
    up.shutdown().await;
}

/// The client's unpatched gzip path leaves its placeholder on the full first-party line inside the compressed
/// stream; inflated, it is the same placeholder an identity body carries and travels unsigned.
#[tokio::test]
async fn a_gzip_full_shape_body_carrying_the_clients_placeholder_is_relayed_with_it_unsigned() {
    let placeholder = String::from_utf8(body(BodyFixture::FirstParty9b244).to_vec())
        .unwrap()
        .replacen("cch=9b244", "cch=00000", 1)
        .into_bytes();
    let up = FakeUpstream::start(messages(json_200())).await;
    let h = harness_with(&up, &[account()], PoolSpec::default(), default_limits()).await;
    let resp = h
        .app
        .clone()
        .oneshot(gzip_request(
            FixtureKind::OauthFirstParty,
            &tailed(&placeholder),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(outcome(&resp).client_drift, None);
    let sent = &up.requests()[0];
    assert_eq!(lines(sent, "content-encoding"), ["gzip"]);
    let plain = inflated(&sent.body);
    let edited = json_before_tail(&plain);
    assert_eq!(account_uuid(&edited), ACCOUNT_UUID);
    assert_eq!(
        cch_digits(&plain).as_deref(),
        Some(&b"00000"[..]),
        "the client's placeholder, unsigned"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_retry_in_place_re_sends_the_same_gzip_bytes() {
    let up = FakeUpstream::start(messages(json_200())).await;
    up.push_credential_rule(A, "/v1/messages", json_429_retry_after("1"));
    up.push_credential_rule(A, "/v1/messages", json_200());
    let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    let resp = h
        .app
        .clone()
        .oneshot(reduced_shape_gzip("s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(outcome(&resp).attempt, 2, "one retry in place");
    let sent = up.requests();
    assert_eq!(sent.len(), 2);
    assert_eq!(lines(&sent[0], "content-encoding"), ["gzip"]);
    assert_eq!(lines(&sent[1], "content-encoding"), ["gzip"]);
    assert_eq!(
        sent[0].body, sent[1].body,
        "the same gzip bytes on both legs"
    );
    json_before_tail(&inflated(&sent[1].body));
    drain(resp).await;
    up.shutdown().await;
}

#[tokio::test]
async fn concurrent_gzip_bodies_against_an_exhausted_budget_are_refused_before_any_inflation() {
    let up = FakeUpstream::start(messages(Respond::Stall {
        status: 200,
        headers: vec![],
        events: vec![],
    }))
    .await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus_limits(
        &up,
        &[account()],
        PoolSpec::default(),
        bus,
        Limits {
            max_request_bytes: 64 << 10,
            body_budget_bytes: 64 << 10,
            ..default_limits()
        },
    )
    .await;
    // Held at the upstream's stalled head, so its reservation stays at the 60 KiB it inflated to.
    let app = h.app.clone();
    let held = tokio::spawn(async move {
        app.oneshot(padded_gzip_request(60 << 10, "s1"))
            .await
            .unwrap()
    });
    wait_for(Duration::from_secs(5), || {
        (up.requests().len() == 1).then_some(())
    })
    .await;
    // Each inflates to under 4 KiB, which the budget's last 4 KiB would hold: only the worst case reserved
    // before the decoder runs refuses it.
    for session in ["s2", "s3"] {
        let refused = tokio::time::timeout(
            Duration::from_secs(5),
            h.app.clone().oneshot(padded_gzip_request(2 << 10, session)),
        )
        .await
        .expect("refused at once")
        .unwrap();
        assert_eq!(
            (
                refused.status().as_u16(),
                refused.headers()["x-phoebis-error-code"].to_str().unwrap()
            ),
            (503, "body_budget_exhausted"),
            "{session}"
        );
        let id = outcome(&refused).request_id;
        let (observed, complete) = observed_and_completed(&mut sub, &id).await;
        assert!(observed.body.is_empty(), "{session}: observed with no body");
        assert_eq!(
            (complete.status, complete.error_code.as_deref()),
            (503, Some("body_budget_exhausted"))
        );
    }
    assert_eq!(up.requests().len(), 1, "refused before the decoder ran");
    held.abort();
    up.release();
    up.shutdown().await;
}

#[tokio::test]
async fn a_body_that_inflates_past_the_bound_is_refused_413_with_no_upstream_attempt() {
    let up = FakeUpstream::start(messages(json_200())).await;
    let h = harness_with(
        &up,
        &[account()],
        PoolSpec::default(),
        Limits {
            max_request_bytes: 64 << 10,
            ..default_limits()
        },
    )
    .await;
    // A few hundred bytes compressed, 100 KiB inflated.
    let req = padded_gzip_request(100 << 10, "s1");
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(
        (
            resp.status().as_u16(),
            resp.headers()["x-phoebis-error-code"].to_str().unwrap()
        ),
        (413, "request_too_large")
    );
    assert!(up.requests().is_empty());
    up.shutdown().await;
}

#[tokio::test]
async fn an_unsupported_request_encoding_is_refused_415_before_its_body_is_read() {
    let up = FakeUpstream::start(messages(json_200())).await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus_limits(
        &up,
        &[account()],
        PoolSpec::default(),
        bus,
        default_limits(),
    )
    .await;
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(1);
    let request = Request::post("/v1/messages")
        .header("authorization", format!("Bearer {KEY}"))
        .header("content-type", "application/json")
        .header("content-encoding", "br")
        .body(Body::from_stream(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        ))
        .unwrap();
    let refused = tokio::time::timeout(Duration::from_secs(2), h.app.clone().oneshot(request))
        .await
        .expect("answered at once, before the body that never comes")
        .unwrap();
    assert_eq!(
        (
            refused.status().as_u16(),
            refused.headers()["x-phoebis-error-code"].to_str().unwrap()
        ),
        (415, "unsupported_content_encoding")
    );
    assert!(refused.headers().get("retry-after").is_none());
    let refused_id = outcome(&refused).request_id;
    let json: serde_json::Value = serde_json::from_slice(&drain(refused).await).unwrap();
    assert_eq!(
        (
            json["error"]["type"].as_str(),
            json["error"]["message"].as_str()
        ),
        (
            Some("invalid_request_error"),
            Some("the request body's content-encoding is not supported. Send gzip or identity")
        )
    );
    let (observed, complete) = observed_and_completed(&mut sub, &refused_id).await;
    assert!(observed.body.is_empty(), "no byte of the body was read");
    assert_eq!(
        (complete.status, complete.error_code.as_deref()),
        (415, Some("unsupported_content_encoding"))
    );
    assert!(up.requests().is_empty());
    drop(tx);
    up.shutdown().await;
}

#[tokio::test]
async fn a_body_that_is_not_gzip_is_refused_400_invalid_body() {
    let up = FakeUpstream::start(messages(json_200())).await;
    let h = harness_with(&up, &[account()], PoolSpec::default(), default_limits()).await;
    let mut req = request_for(KEY, MODEL, "s1");
    set_header(&mut req, "content-encoding", "gzip");
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(
        (
            resp.status().as_u16(),
            resp.headers()["x-phoebis-error-code"].to_str().unwrap()
        ),
        (400, "invalid_body")
    );
    let json: serde_json::Value = serde_json::from_slice(&drain(resp).await).unwrap();
    assert_eq!(
        (
            json["error"]["type"].as_str(),
            json["error"]["message"].as_str()
        ),
        (
            Some("invalid_request_error"),
            Some("the request body is not valid gzip")
        )
    );
    assert!(up.requests().is_empty());
    up.shutdown().await;
}

#[tokio::test]
async fn under_auto_an_identity_client_body_leaves_identity_with_no_content_encoding() {
    let up = FakeUpstream::start(messages(json_200())).await;
    let h = harness_with(&up, &[account()], PoolSpec::default(), default_limits()).await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let sent = &up.requests()[0];
    assert!(lines(sent, "content-encoding").is_empty());
    assert_eq!(lines(sent, "content-length"), [sent.body.len().to_string()]);
    let edited: serde_json::Value = serde_json::from_slice(&sent.body).unwrap();
    assert_eq!(account_uuid(&edited), ACCOUNT_UUID);
    up.shutdown().await;
}

#[tokio::test]
async fn under_on_an_identity_client_body_leaves_gzip_and_under_off_a_gzip_client_body_leaves_identity(
) {
    let up = FakeUpstream::start(messages(json_200())).await;
    let on = harness_with(
        &up,
        &[account()],
        PoolSpec::default(),
        Limits {
            upstream_gzip: UpstreamGzip::On,
            ..default_limits()
        },
    )
    .await;
    let resp = on
        .app
        .clone()
        .oneshot(request_for(KEY, MODEL, "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let sent = &up.requests()[0];
    assert_eq!(lines(sent, "content-encoding"), ["gzip"], "on: always gzip");
    assert_eq!(lines(sent, "content-length"), [sent.body.len().to_string()]);
    let edited: serde_json::Value = serde_json::from_slice(&inflated(&sent.body)).unwrap();
    assert_eq!(account_uuid(&edited), ACCOUNT_UUID);
    assert_eq!(edited["model"], MODEL);
    assert_eq!(
        cch_digits(&inflated(&sent.body)).as_deref(),
        Some(&b"00000"[..])
    );

    let off = harness_with(
        &up,
        &[account()],
        PoolSpec::default(),
        Limits {
            upstream_gzip: UpstreamGzip::Off,
            ..default_limits()
        },
    )
    .await;
    let resp = off
        .app
        .clone()
        .oneshot(reduced_shape_gzip("s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let sent = &up.requests()[1];
    assert!(
        lines(sent, "content-encoding").is_empty(),
        "off: no content-encoding line, whatever the client sent: {:?}",
        sent.headers
    );
    assert_eq!(lines(sent, "content-length"), [sent.body.len().to_string()]);
    let edited = json_before_tail(&sent.body);
    assert_eq!(account_uuid(&edited), ACCOUNT_UUID);
    assert_eq!(cch_digits(&sent.body).as_deref(), Some(&b"00000"[..]));
    up.shutdown().await;
}
