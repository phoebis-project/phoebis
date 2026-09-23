//! The proxy listener ends a connection that sends no head in time and admits at most its cap at once.

mod common;

use std::time::Duration;

use common::*;
use fake_upstream::FakeUpstream;
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
