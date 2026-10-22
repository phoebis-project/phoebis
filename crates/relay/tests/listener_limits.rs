//! The proxy listener ends a connection that sends no head in time and admits at most its cap at once.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule, SseEvent};
use phoebis_bus::{Bus, Delivery, Event};
use phoebis_relay::LiveAcceptor;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The stop sender comes back with the address: `stopped()` reads a dropped sender as a stop, so a test holds
/// it until its end or the listener ends first.
async fn listening(
    limits: phoebis_relay::ListenLimits,
    acceptor: Option<Arc<LiveAcceptor>>,
) -> (
    std::net::SocketAddr,
    FakeUpstream,
    tokio::sync::watch::Sender<bool>,
) {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(
        &up,
        &[oauth("acct", "sk-ant-oat01-PLACEHOLDER", 0)],
        PoolSpec::default(),
    )
    .await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stop_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(phoebis_relay::serve(
        listener,
        h.relay.clone(),
        axum::Router::new(),
        stop_rx,
        limits,
        acceptor,
    ));
    (addr, up, stop)
}

#[tokio::test]
async fn a_connection_that_sends_no_head_is_closed_at_the_header_read_timeout() {
    let limits = phoebis_relay::ListenLimits {
        drain_timeout: Duration::from_secs(30),
        header_read_timeout: Duration::from_millis(300),
        max_connections: 8,
        shutdown_mark: None,
    };
    let (addr, up, _stop) = listening(limits, None).await;
    let mut idle = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut buf = [0u8; 16];
    let read = tokio::time::timeout(Duration::from_secs(3), idle.read(&mut buf)).await;
    assert!(
        matches!(read, Ok(Ok(0)) | Ok(Err(_))),
        "the listener closed the idle connection: {read:?}"
    );
    up.shutdown().await;
}

/// The handshake runs under the header read timeout, so a peer that opens a socket and sends no ClientHello is
/// held to it like a peer that sends no head; with one slot, the handshake behind it proves the slot came back.
#[tokio::test]
async fn a_tls_listener_drops_a_silent_peer_at_the_bound_and_serves_a_finished_handshake() {
    let pair = fake_upstream::generate_pair(&["localhost", "127.0.0.1"]);
    let limits = phoebis_relay::ListenLimits {
        drain_timeout: Duration::from_secs(30),
        header_read_timeout: Duration::from_millis(200),
        max_connections: 1,
        shutdown_mark: None,
    };
    let acceptor = LiveAcceptor::new(fake_upstream::acceptor_for(&pair));
    let (addr, up, _stop) = listening(limits, Some(Arc::new(acceptor))).await;
    let mut silent = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut buf = [0u8; 16];
    let read = tokio::time::timeout(Duration::from_secs(3), silent.read(&mut buf)).await;
    assert!(
        matches!(read, Ok(Ok(0)) | Ok(Err(_))),
        "the listener dropped the silent peer: {read:?}"
    );
    let socket = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut tls = tokio::time::timeout(
        Duration::from_secs(3),
        tokio_boring::connect(
            fake_upstream::connector_trusting(&pair),
            "localhost",
            socket,
        ),
    )
    .await
    .expect("the slot came back and the handshake finished")
    .unwrap();
    assert_eq!(tls.ssl().selected_alpn_protocol(), Some(&b"http/1.1"[..]));
    tls.write_all(b"HEAD /api/hello HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut answer = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(3), tls.read_to_end(&mut answer)).await;
    let answer = String::from_utf8_lossy(&answer);
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    up.shutdown().await;
}

/// A peer that opens a socket on a TLS listener and sends nothing has no request to finish, so the stop ends its
/// handshake as it ends a plain connection that has read no byte, rather than waiting out the handshake's bound.
/// The served request behind it proves the silent peer was accepted, and its handshake running, before the stop.
#[tokio::test]
async fn a_stop_ends_a_silent_handshake_well_before_its_bound() {
    let pair = fake_upstream::generate_pair(&["localhost"]);
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(
        &up,
        &[oauth("acct", "sk-ant-oat01-PLACEHOLDER", 0)],
        PoolSpec::default(),
    )
    .await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stop_rx) = tokio::sync::watch::channel(false);
    let limits = phoebis_relay::ListenLimits {
        drain_timeout: Duration::from_secs(30),
        header_read_timeout: Duration::from_secs(30),
        max_connections: 8,
        shutdown_mark: None,
    };
    let acceptor = LiveAcceptor::new(fake_upstream::acceptor_for(&pair));
    let serving = tokio::spawn(phoebis_relay::serve(
        listener,
        h.relay.clone(),
        axum::Router::new(),
        stop_rx,
        limits,
        Some(Arc::new(acceptor)),
    ));
    let _silent = tokio::net::TcpStream::connect(addr).await.unwrap();
    let socket = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut tls = tokio_boring::connect(
        fake_upstream::connector_trusting(&pair),
        "localhost",
        socket,
    )
    .await
    .unwrap();
    tls.write_all(b"HEAD /api/hello HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut answer = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(3), tls.read_to_end(&mut answer)).await;
    assert!(String::from_utf8_lossy(&answer).starts_with("HTTP/1.1 200"));
    stop.send(true).unwrap();
    let returned = tokio::time::timeout(Duration::from_secs(3), serving).await;
    assert!(
        matches!(returned, Ok(Ok(Ok(())))),
        "the listener returned with a silent peer mid-handshake: {returned:?}"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn the_connection_cap_holds_the_next_accept_until_a_slot_frees() {
    let limits = phoebis_relay::ListenLimits {
        drain_timeout: Duration::from_secs(30),
        header_read_timeout: Duration::from_millis(500),
        max_connections: 2,
        shutdown_mark: None,
    };
    let (addr, up, _stop) = listening(limits, None).await;
    let _first = tokio::net::TcpStream::connect(addr).await.unwrap();
    let _second = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut third = tokio::net::TcpStream::connect(addr).await.unwrap();
    third
        .write_all(b"HEAD /api/hello HTTP/1.1\r\nhost: x\r\n\r\n")
        .await
        .unwrap();
    let mut buf = [0u8; 64];
    let early = tokio::time::timeout(Duration::from_millis(200), third.read(&mut buf)).await;
    assert!(early.is_err(), "the third connection waits behind the cap");
    // The two idle connections time out, their slots free, and the third is served.
    let late = tokio::time::timeout(Duration::from_secs(3), third.read(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert!(std::str::from_utf8(&buf[..late])
        .unwrap()
        .starts_with("HTTP/1.1 200"));
    up.shutdown().await;
}

#[tokio::test]
async fn a_request_still_in_flight_at_the_drain_deadline_is_ended_before_serve_returns() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Gated(Box::new(Respond::Json {
            status: 200,
            headers: vec![],
            body: b"{}".to_vec(),
        })),
    }])
    .await;
    let h = harness(
        &up,
        &[oauth("acct", "sk-ant-oat01-PLACEHOLDER", 0)],
        PoolSpec::default(),
    )
    .await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stop_rx) = tokio::sync::watch::channel(false);
    let limits = phoebis_relay::ListenLimits {
        header_read_timeout: Duration::from_secs(30),
        max_connections: 8,
        drain_timeout: Duration::from_millis(300),
        shutdown_mark: None,
    };
    let serving = tokio::spawn(phoebis_relay::serve(
        listener,
        h.relay.clone(),
        axum::Router::new(),
        stop_rx,
        limits,
        None,
    ));
    let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
    let body = r#"{"model":"claude-sonnet-4-5","messages":[{"role":"user","content":"hi"}],"max_tokens":8,"metadata":{"user_id":"{\"account_uuid\":\"\",\"session_id\":\"s-1\"}"}}"#;
    client
        .write_all(
            format!(
                "POST /v1/messages HTTP/1.1\r\nhost: x\r\nauthorization: Bearer {}\r\n\
                 user-agent: {}\r\n\
                 x-claude-code-session-id: s-1\r\nx-claude-code-request-class: main\r\n\
                 content-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                KEY,
                CLAUDE_CLI_USER_AGENT,
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    fake_upstream::wait_for(Duration::from_secs(5), || {
        (up.requests().len() == 1).then_some(())
    })
    .await;
    let _ = stop.send(true);
    tokio::time::timeout(Duration::from_secs(3), serving)
        .await
        .expect("serve returned at the deadline")
        .unwrap()
        .unwrap();
    up.release();
    let mut buf = [0u8; 16];
    let read = tokio::time::timeout(Duration::from_secs(2), client.read(&mut buf)).await;
    assert!(
        matches!(read, Ok(Ok(0)) | Ok(Err(_))),
        "the connection was cut, not answered: {read:?}"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_body_task_cut_at_the_deadline_publishes_its_completion_before_the_drain_returns() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::SseGated {
            status: 200,
            headers: vec![],
            events: vec![
                SseEvent {
                    event: "message_start".into(),
                    data: r#"{"type":"message_start","message":{"usage":{"input_tokens":5}}}"#.into(),
                },
                SseEvent {
                    event: "content_block_delta".into(),
                    data: r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"hi"}}"#.into(),
                },
                SseEvent {
                    event: "message_stop".into(),
                    data: r#"{"type":"message_stop"}"#.into(),
                },
            ],
        },
    }])
    .await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus(
        &up,
        &[oauth("acct", "sk-ant-oat01-PLACEHOLDER", 0)],
        PoolSpec::default(),
        bus,
    )
    .await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stop_rx) = tokio::sync::watch::channel(false);
    let limits = phoebis_relay::ListenLimits {
        header_read_timeout: Duration::from_secs(30),
        max_connections: 8,
        drain_timeout: Duration::from_millis(300),
        shutdown_mark: None,
    };
    let serving = tokio::spawn(phoebis_relay::serve(
        listener,
        h.relay.clone(),
        axum::Router::new(),
        stop_rx,
        limits,
        None,
    ));
    let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
    let body = r#"{"model":"claude-sonnet-4-5","messages":[{"role":"user","content":"hi"}],"max_tokens":8,"metadata":{"user_id":"{\"account_uuid\":\"\",\"session_id\":\"s-1\"}"}}"#;
    client
        .write_all(
            format!(
                "POST /v1/messages HTTP/1.1\r\nhost: x\r\nauthorization: Bearer {}\r\n\
                 user-agent: {}\r\n\
                 x-claude-code-session-id: s-1\r\nx-claude-code-request-class: main\r\n\
                 content-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                KEY,
                CLAUDE_CLI_USER_AGENT,
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    fake_upstream::wait_for(Duration::from_secs(5), || {
        (up.requests().len() == 1).then_some(())
    })
    .await;
    let _ = stop.send(true);
    tokio::time::timeout(Duration::from_secs(3), serving)
        .await
        .expect("serve returned at the deadline")
        .unwrap()
        .unwrap();
    up.release();
    tokio::time::timeout(Duration::from_secs(3), h.relay.drain_bodies())
        .await
        .expect("drain_bodies returns once the cut body task publishes its completion");
    let mut saw_complete = false;
    while let Ok(Some(event)) =
        tokio::time::timeout(Duration::from_millis(500), sub.events.recv()).await
    {
        if matches!(event, Event::RequestComplete(_)) {
            saw_complete = true;
            break;
        }
    }
    assert!(
        saw_complete,
        "the subscriber holds the request's RequestComplete"
    );
    up.shutdown().await;
}

/// A relay with a bus and the shutdown mark, served on a loopback port under a drain deadline of `drain_timeout`
/// whose listener holds the same mark.
async fn serving_with_mark(
    up: &FakeUpstream,
    drain_timeout: Duration,
) -> (
    std::net::SocketAddr,
    phoebis_bus::Subscriber,
    phoebis_relay::ShutdownMark,
    tokio::sync::watch::Sender<bool>,
    tokio::task::JoinHandle<std::io::Result<()>>,
) {
    let bus = Bus::new();
    let sub = bus.subscribe("test", Delivery::Durable);
    let mark = phoebis_relay::ShutdownMark::default();
    let h = harness_with_shutdown_mark(
        up,
        &[oauth("acct", "sk-ant-oat01-PLACEHOLDER", 0)],
        PoolSpec::default(),
        bus,
        default_limits(),
        mark.clone(),
    )
    .await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stop_rx) = tokio::sync::watch::channel(false);
    let serving = tokio::spawn(phoebis_relay::serve(
        listener,
        h.relay.clone(),
        axum::Router::new(),
        stop_rx,
        phoebis_relay::ListenLimits {
            drain_timeout,
            header_read_timeout: Duration::from_secs(30),
            max_connections: 8,
            shutdown_mark: Some(mark.clone()),
        },
        None,
    ));
    (addr, sub, mark, stop, serving)
}

/// Writes one Messages request on the harness key to `client`.
async fn write_messages_request(client: &mut tokio::net::TcpStream) {
    let body = r#"{"model":"claude-sonnet-4-5","messages":[{"role":"user","content":"hi"}],"max_tokens":8,"metadata":{"user_id":"{\"account_uuid\":\"\",\"session_id\":\"s-1\"}"}}"#;
    client
        .write_all(
            format!(
                "POST /v1/messages HTTP/1.1\r\nhost: x\r\nauthorization: Bearer {}\r\n\
                 user-agent: {}\r\n\
                 x-claude-code-session-id: s-1\r\nx-claude-code-request-class: main\r\n\
                 content-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                KEY,
                CLAUDE_CLI_USER_AGENT,
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
}

/// The first completion `sub` carries.
async fn first_completion(sub: &mut phoebis_bus::Subscriber) -> phoebis_bus::RequestComplete {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match sub.events.recv().await {
                Some(Event::RequestComplete(complete)) => return complete,
                Some(_) => {}
                None => panic!("the bus closed before a completion"),
            }
        }
    })
    .await
    .expect("the request completed")
}

#[tokio::test]
async fn a_request_the_listener_cuts_at_its_drain_deadline_completes_as_router_shutdown() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Gated(Box::new(Respond::Json {
            status: 200,
            headers: vec![],
            body: b"{}".to_vec(),
        })),
    }])
    .await;
    let (addr, mut sub, mark, stop, serving) =
        serving_with_mark(&up, Duration::from_millis(300)).await;
    let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
    write_messages_request(&mut client).await;
    fake_upstream::wait_for(Duration::from_secs(5), || {
        (up.requests().len() == 1).then_some(())
    })
    .await;
    let _ = stop.send(true);
    tokio::time::timeout(Duration::from_secs(3), serving)
        .await
        .expect("serve returned at the deadline")
        .unwrap()
        .unwrap();
    assert!(
        mark.load(std::sync::atomic::Ordering::SeqCst),
        "the deadline's cut set the mark"
    );
    let complete = first_completion(&mut sub).await;
    assert_eq!(
        (complete.outcome, complete.error_code.as_deref()),
        (phoebis_bus::Outcome::Canceled, Some("router_shutdown")),
        "the listener's own cut is the router's shutdown, not the caller's leaving"
    );
    up.release();
    up.shutdown().await;
}

#[tokio::test]
async fn a_drain_that_ends_before_its_deadline_leaves_the_shutdown_mark_unset() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: b"{}".to_vec(),
        },
    }])
    .await;
    let (addr, mut sub, mark, stop, serving) =
        serving_with_mark(&up, Duration::from_secs(30)).await;
    let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
    write_messages_request(&mut client).await;
    let complete = first_completion(&mut sub).await;
    assert_eq!(
        complete.error_code, None,
        "the request was served to its end"
    );
    let _ = stop.send(true);
    tokio::time::timeout(Duration::from_secs(3), serving)
        .await
        .expect("serve returned once its idle connection closed")
        .unwrap()
        .unwrap();
    assert!(
        !mark.load(std::sync::atomic::Ordering::SeqCst),
        "a drain with nothing left open at its deadline sets nothing"
    );
    up.shutdown().await;
}
