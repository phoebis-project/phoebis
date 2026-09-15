use bytes::Bytes;
use phoebis_profile::{
    claude_code_2_1_269_linux_x64,
    fixtures::{control_head, control_response, ControlFixture},
};
use phoebis_transport::request::{parse_head, RawRequest};
use phoebis_transport::{
    connector_builder,
    control::{ControlClient, Resolver},
    Plane, TransportError,
};
use fake_upstream::{FakeUpstream, Respond, Rule};
use std::{sync::Arc, time::Duration};

#[tokio::test]
async fn control_requests_reach_the_upstream_byte_identical_and_close() {
    let up = FakeUpstream::start(vec![
        Rule {
            path_prefix: "/v1/oauth/token".into(),
            respond: Respond::Json {
                status: 200,
                headers: vec![],
                body: control_response(ControlFixture::Token).to_vec(),
            },
        },
        Rule {
            path_prefix: "/api/oauth/profile".into(),
            respond: Respond::Json {
                status: 200,
                headers: vec![],
                body: control_response(ControlFixture::Profile).to_vec(),
            },
        },
    ])
    .await;
    let profile = claude_code_2_1_269_linux_x64();
    let mut b = connector_builder(profile, Plane::Control).unwrap();
    phoebis_transport::trust_extra_ca(&mut b, up.ca_pem()).unwrap();
    let addr = up.addr();
    let resolver: Resolver = Arc::new(move |_| Some(addr));
    let client = ControlClient::new(profile, b.build(), resolver);

    let raw = control_head(ControlFixture::Token);
    let head_len = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    let (line, headers) = parse_head(raw).unwrap();
    let req = RawRequest {
        method: "POST",
        target: "/v1/oauth/token".into(),
        headers,
        body: Bytes::copy_from_slice(&raw[head_len..]),
    };
    let _ = line;
    let (head, body) = client
        .send(req, "platform.claude.com", Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(head.status, 200);
    assert!(body.starts_with(b"{\"token_type\""));

    let got = &up.requests()[0];
    let mut wire = got.line.clone().into_bytes();
    wire.extend_from_slice(b"\r\n");
    for (n, v) in &got.headers {
        wire.extend_from_slice(format!("{n}: {v}\r\n").as_bytes());
    }
    wire.extend_from_slice(b"\r\n");
    wire.extend_from_slice(&got.body);
    assert_eq!(
        wire, raw,
        "the token request is byte-identical to the fixture"
    );
    assert_eq!(up.connections(), 1);
    up.shutdown().await;
}

/// The fixture token request, rebuilt for the tests that never get to send it.
fn token_request() -> RawRequest {
    let raw = control_head(ControlFixture::Token);
    let head_len = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    let (_, headers) = parse_head(raw).unwrap();
    RawRequest {
        method: "POST",
        target: "/v1/oauth/token".into(),
        headers,
        body: Bytes::copy_from_slice(&raw[head_len..]),
    }
}

#[tokio::test]
async fn a_request_that_never_leaves_reports_an_unreachable_upstream() {
    let profile = claude_code_2_1_269_linux_x64();

    let unresolved: Resolver = Arc::new(|_| None);
    let client = ControlClient::new(
        profile,
        connector_builder(profile, Plane::Control).unwrap().build(),
        unresolved,
    );
    let error = client
        .send(
            token_request(),
            "platform.claude.com",
            Duration::from_secs(5),
        )
        .await
        .map(|_| ())
        .expect_err("a host with no address cannot be reached");
    assert!(matches!(error, TransportError::Unreachable(_)), "{error:?}");

    // Bound only to learn a free port, then dropped, so the connect is refused rather than accepted.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let closed = listener.local_addr().unwrap();
    drop(listener);
    let refused: Resolver = Arc::new(move |_| Some(closed));
    let client = ControlClient::new(
        profile,
        connector_builder(profile, Plane::Control).unwrap().build(),
        refused,
    );
    let error = client
        .send(
            token_request(),
            "platform.claude.com",
            Duration::from_secs(5),
        )
        .await
        .map(|_| ())
        .expect_err("a closed port refuses the connection");
    assert!(matches!(error, TransportError::Unreachable(_)), "{error:?}");
}
