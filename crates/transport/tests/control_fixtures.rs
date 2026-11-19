use bytes::Bytes;
use fake_upstream::{acceptor_for, generate_pair, FakeUpstream, Respond, Rule};
use phoebis_profile::{
    claude_code_2_1_310_linux_x64,
    fixtures::{control_head, control_response, ControlFixture},
};
use phoebis_transport::request::{parse_head, RawRequest};
use phoebis_transport::{
    connector_builder,
    control::{ControlClient, ResolveFuture, Resolver, StreamFailure},
    Plane, TransportError,
};
use std::{future::Future, net::SocketAddr, sync::Arc, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{JoinHandle, JoinSet};
use tokio_boring::SslStream;

#[tokio::test]
async fn control_requests_reach_the_upstream_byte_identical() {
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
    let profile = claude_code_2_1_310_linux_x64();
    let mut b = connector_builder(profile, Plane::Control).unwrap();
    phoebis_transport::trust_extra_ca(&mut b, up.ca_pem()).unwrap();
    let addr = up.addr();
    let resolver: Resolver =
        Arc::new(move |_| Box::pin(async move { Some(addr) }) as ResolveFuture);
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
    let profile = claude_code_2_1_310_linux_x64();

    let unresolved: Resolver = Arc::new(|_| Box::pin(async { None }) as ResolveFuture);
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
    let refused: Resolver =
        Arc::new(move |_| Box::pin(async move { Some(closed) }) as ResolveFuture);
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

/// The upstream reads the whole request before it gates, so by the time the timeout fires the body is
/// already handed over; only the response is missing.
#[tokio::test]
async fn a_timeout_after_the_body_was_handed_over_is_not_unreachable() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/oauth/token".into(),
        respond: Respond::Gated(Box::new(Respond::CloseAfterRequest)),
    }])
    .await;
    let profile = claude_code_2_1_310_linux_x64();
    let mut b = connector_builder(profile, Plane::Control).unwrap();
    phoebis_transport::trust_extra_ca(&mut b, up.ca_pem()).unwrap();
    let addr = up.addr();
    let resolver: Resolver =
        Arc::new(move |_| Box::pin(async move { Some(addr) }) as ResolveFuture);
    let client = ControlClient::new(profile, b.build(), resolver);
    let sending = tokio::spawn(async move {
        client
            .send(
                token_request(),
                "platform.claude.com",
                Duration::from_secs(60),
            )
            .await
            .map(|_| ())
    });
    // The handshake and the write run on the real clock, which the timeout outlasts. The clock stops once the
    // upstream holds the whole request, and with nothing left to arrive on the socket the paused runtime moves it
    // straight to the timeout.
    fake_upstream::wait_for(Duration::from_secs(5), || {
        (!up.requests().is_empty()).then_some(())
    })
    .await;
    tokio::time::pause();
    let error = sending
        .await
        .unwrap()
        .expect_err("the gated upstream never answers");
    assert!(matches!(error, TransportError::Io(_)), "{error:?}");
    up.shutdown().await;
}

#[tokio::test]
async fn a_zstd_bomb_response_is_refused_at_the_control_bound() {
    let bomb = zstd::stream::encode_all(vec![0u8; 8 << 20].as_slice(), 19).unwrap();
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/oauth/token".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![("content-encoding".into(), "zstd".into())],
            body: bomb,
        },
    }])
    .await;
    let profile = claude_code_2_1_310_linux_x64();
    let mut b = connector_builder(profile, Plane::Control).unwrap();
    phoebis_transport::trust_extra_ca(&mut b, up.ca_pem()).unwrap();
    let addr = up.addr();
    let resolver: Resolver =
        Arc::new(move |_| Box::pin(async move { Some(addr) }) as ResolveFuture);
    let client = ControlClient::new(profile, b.build(), resolver);
    let error = client
        .send(
            token_request(),
            "platform.claude.com",
            Duration::from_secs(5),
        )
        .await
        .map(|_| ())
        .expect_err("a decoded zstd bomb is refused at the control bound");
    match error {
        TransportError::TooLarge(bound) => assert_eq!(bound, phoebis_transport::control::MAX_BODY),
        other => panic!("expected TooLarge, got {other:?}"),
    }
    up.shutdown().await;
}

#[tokio::test]
async fn a_streamed_answer_past_the_buffered_bound_arrives_whole() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/api/frame/".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: vec![b'a'; 5_242_880],
        },
    }])
    .await;
    let profile = claude_code_2_1_310_linux_x64();
    let mut b = connector_builder(profile, Plane::Control).unwrap();
    phoebis_transport::trust_extra_ca(&mut b, up.ca_pem()).unwrap();
    let addr = up.addr();
    let resolver: Resolver =
        Arc::new(move |_| Box::pin(async move { Some(addr) }) as ResolveFuture);
    let client = ControlClient::new(profile, b.build(), resolver);
    let req = RawRequest {
        method: "GET",
        target: "/api/frame/artifacts".into(),
        headers: vec![
            ("Host".into(), "api.anthropic.com".into()),
            ("Connection".into(), "close".into()),
        ],
        body: Bytes::new(),
    };
    let mut answer = client
        .send_streaming(
            req,
            "api.anthropic.com",
            Duration::from_secs(5),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert_eq!(answer.head.status, 200);
    let mut total = 0;
    while let Some(piece) = answer.body.next_chunk().await.unwrap() {
        total += piece.len();
    }
    assert_eq!(total, 5_242_880);
    up.shutdown().await;
}

/// A streamed frame request, for the tests whose answer never gets past its head.
fn frame_request() -> RawRequest {
    RawRequest {
        method: "GET",
        target: "/api/frame/artifacts".into(),
        headers: vec![
            ("Host".into(), "api.anthropic.com".into()),
            ("Connection".into(), "close".into()),
        ],
        body: Bytes::new(),
    }
}

#[tokio::test]
async fn a_peer_that_closes_before_the_answer_head_is_named_a_cut() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/api/frame/".into(),
        respond: Respond::CloseAfterRequest,
    }])
    .await;
    let profile = claude_code_2_1_310_linux_x64();
    let mut b = connector_builder(profile, Plane::Control).unwrap();
    phoebis_transport::trust_extra_ca(&mut b, up.ca_pem()).unwrap();
    let addr = up.addr();
    let resolver: Resolver =
        Arc::new(move |_| Box::pin(async move { Some(addr) }) as ResolveFuture);
    let client = ControlClient::new(profile, b.build(), resolver);
    let error = client
        .send_streaming(
            frame_request(),
            "api.anthropic.com",
            Duration::from_secs(5),
            Duration::from_secs(5),
        )
        .await
        .map(|_| ())
        .expect_err("the upstream closes without an answer");
    assert!(matches!(error, StreamFailure::Cut(_)), "{error:?}");
    up.shutdown().await;
}

#[tokio::test]
async fn bytes_that_are_no_answer_head_are_named_unreadable() {
    // A status of four digits makes a status line that no HTTP head has.
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/api/frame/".into(),
        respond: Respond::Json {
            status: 1000,
            headers: vec![],
            body: vec![],
        },
    }])
    .await;
    let profile = claude_code_2_1_310_linux_x64();
    let mut b = connector_builder(profile, Plane::Control).unwrap();
    phoebis_transport::trust_extra_ca(&mut b, up.ca_pem()).unwrap();
    let addr = up.addr();
    let resolver: Resolver =
        Arc::new(move |_| Box::pin(async move { Some(addr) }) as ResolveFuture);
    let client = ControlClient::new(profile, b.build(), resolver);
    let error = client
        .send_streaming(
            frame_request(),
            "api.anthropic.com",
            Duration::from_secs(5),
            Duration::from_secs(5),
        )
        .await
        .map(|_| ())
        .expect_err("the upstream answers no head");
    assert!(matches!(error, StreamFailure::Undecodable(_)), "{error:?}");
    up.shutdown().await;
}

/// A TLS upstream for `api.anthropic.com` that hands its connection to `end` once the request has come up to its blank
/// line, for the ends the fake cannot script, since each of its answers ends a connection in order. It serves the first
/// connection whose handshake completes and drops each other, since a daemon of the development host connects to each
/// new loopback listener. Answers the address, the CA and the task, whose join shows a failure of the upstream's own.
async fn upstream_that_ends<F, Fut>(end: F) -> (SocketAddr, String, JoinHandle<()>)
where
    F: FnOnce(SslStream<TcpStream>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send,
{
    let pair = generate_pair(&["api.anthropic.com"]);
    let acceptor = acceptor_for(&pair);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let mut handshakes = JoinSet::new();
        let mut tls = loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (sock, _) = accepted.unwrap();
                    let acceptor = acceptor.clone();
                    handshakes.spawn(async move { tokio_boring::accept(&acceptor, sock).await });
                }
                Some(joined) = handshakes.join_next() => {
                    if let Ok(Ok(tls)) = joined {
                        break tls;
                    }
                }
            }
        };
        let mut request = Vec::new();
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            let mut piece = [0u8; 1024];
            let n = tls.read(&mut piece).await.unwrap();
            assert_ne!(n, 0, "the client ended its request before the blank line");
            request.extend_from_slice(&piece[..n]);
        }
        end(tls).await;
    });
    (addr, pair.ca_pem, task)
}

#[tokio::test]
async fn a_peer_that_resets_before_the_answer_head_is_named_a_cut() {
    let (addr, ca_pem, upstream) = upstream_that_ends(|tls| async move {
        // A zero linger turns the close into a reset, so the client's read fails rather than ends.
        tls.get_ref().set_zero_linger().unwrap();
        drop(tls);
    })
    .await;
    let profile = claude_code_2_1_310_linux_x64();
    let mut b = connector_builder(profile, Plane::Control).unwrap();
    phoebis_transport::trust_extra_ca(&mut b, &ca_pem).unwrap();
    let resolver: Resolver =
        Arc::new(move |_| Box::pin(async move { Some(addr) }) as ResolveFuture);
    let client = ControlClient::new(profile, b.build(), resolver);
    let error = client
        .send_streaming(
            frame_request(),
            "api.anthropic.com",
            Duration::from_secs(5),
            Duration::from_secs(5),
        )
        .await
        .map(|_| ())
        .expect_err("the upstream resets without an answer");
    assert!(matches!(error, StreamFailure::Cut(_)), "{error:?}");
    upstream.await.unwrap();
}

#[tokio::test]
async fn a_peer_that_closes_inside_the_answer_head_is_named_a_cut() {
    let (addr, ca_pem, upstream) = upstream_that_ends(|mut tls| async move {
        tls.write_all(b"HTTP/1.1 200 OK\r\nContent-Le")
            .await
            .unwrap();
        tls.shutdown().await.unwrap();
    })
    .await;
    let profile = claude_code_2_1_310_linux_x64();
    let mut b = connector_builder(profile, Plane::Control).unwrap();
    phoebis_transport::trust_extra_ca(&mut b, &ca_pem).unwrap();
    let resolver: Resolver =
        Arc::new(move |_| Box::pin(async move { Some(addr) }) as ResolveFuture);
    let client = ControlClient::new(profile, b.build(), resolver);
    let error = client
        .send_streaming(
            frame_request(),
            "api.anthropic.com",
            Duration::from_secs(5),
            Duration::from_secs(5),
        )
        .await
        .map(|_| ())
        .expect_err("the upstream ends its session inside the head");
    assert!(matches!(error, StreamFailure::Cut(_)), "{error:?}");
    upstream.await.unwrap();
}
