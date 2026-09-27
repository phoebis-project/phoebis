//! The proxy listener ends a connection that sends no head in time and admits at most its cap at once.

mod common;

use std::time::Duration;

use phoebis_bus::{Bus, Event};
use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule, SseEvent};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The stop sender comes back with the address: `stopped()` reads a dropped sender as a stop, so a test holds
/// it until its end or the listener ends first.
async fn listening(
    limits: phoebis_relay::ListenLimits,
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
    ));
    (addr, up, stop)
}

#[tokio::test]
async fn a_connection_that_sends_no_head_is_closed_at_the_header_read_timeout() {
    let limits = phoebis_relay::ListenLimits {
        drain_timeout: Duration::from_secs(30),
        header_read_timeout: Duration::from_millis(300),
        max_connections: 8,
    };
    let (addr, up, _stop) = listening(limits).await;
    let mut idle = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut buf = [0u8; 16];
    let read = tokio::time::timeout(Duration::from_secs(3), idle.read(&mut buf)).await;
    assert!(
        matches!(read, Ok(Ok(0)) | Ok(Err(_))),
        "the listener closed the idle connection: {read:?}"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn the_connection_cap_holds_the_next_accept_until_a_slot_frees() {
    let limits = phoebis_relay::ListenLimits {
        drain_timeout: Duration::from_secs(30),
        header_read_timeout: Duration::from_millis(500),
        max_connections: 2,
    };
    let (addr, up, _stop) = listening(limits).await;
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
    };
    let serving = tokio::spawn(phoebis_relay::serve(
        listener,
        h.relay.clone(),
        axum::Router::new(),
        stop_rx,
        limits,
    ));
    let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
    let body = r#"{"model":"claude-sonnet-4-5","messages":[{"role":"user","content":"hi"}],"max_tokens":8,"metadata":{"user_id":"{\"account_uuid\":\"\",\"session_id\":\"s-1\"}"}}"#;
    client
        .write_all(
            format!(
                "POST /v1/messages HTTP/1.1\r\nhost: x\r\nauthorization: Bearer {}\r\n\
                 user-agent: claude-cli/2.1.269 (external, sdk-cli)\r\n\
                 content-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                KEY,
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
    let mut sub = bus.subscribe("test");
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
    };
    let serving = tokio::spawn(phoebis_relay::serve(
        listener,
        h.relay.clone(),
        axum::Router::new(),
        stop_rx,
        limits,
    ));
    let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
    let body = r#"{"model":"claude-sonnet-4-5","messages":[{"role":"user","content":"hi"}],"max_tokens":8,"metadata":{"user_id":"{\"account_uuid\":\"\",\"session_id\":\"s-1\"}"}}"#;
    client
        .write_all(
            format!(
                "POST /v1/messages HTTP/1.1\r\nhost: x\r\nauthorization: Bearer {}\r\n\
                 user-agent: claude-cli/2.1.269 (external, sdk-cli)\r\n\
                 content-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                KEY,
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
