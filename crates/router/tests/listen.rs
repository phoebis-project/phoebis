//! The admin and metrics listeners share the accept loop the proxy listener uses: the same header read
//! timeout and connection cap, over a plain tower service.

use std::sync::Arc;
use std::time::Duration;

use axum::routing::get;
use phoebis_relay::LiveAcceptor;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn app() -> axum::Router {
    axum::Router::new().route("/ping", get(|| async { "pong" }))
}

/// The stop sender comes back with the address: `stopped()` reads a dropped sender as a stop, so a test holds
/// it until its end or the listener ends first.
async fn listening(
    limits: phoebis_router::listen::ListenLimits,
    acceptor: Option<Arc<LiveAcceptor>>,
) -> (std::net::SocketAddr, tokio::sync::watch::Sender<bool>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stop_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(phoebis_router::listen::serve(
        "admin",
        listener,
        app(),
        stop_rx,
        limits,
        acceptor,
    ));
    (addr, stop)
}

#[tokio::test]
async fn a_connection_that_sends_no_head_is_closed_at_the_header_read_timeout() {
    let limits = phoebis_router::listen::ListenLimits {
        drain_timeout: Duration::from_secs(30),
        header_read_timeout: Duration::from_millis(300),
        max_connections: 8,
        shutdown_mark: None,
    };
    let (addr, _stop) = listening(limits, None).await;
    let mut idle = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut buf = [0u8; 16];
    let read = tokio::time::timeout(Duration::from_secs(3), idle.read(&mut buf)).await;
    assert!(
        matches!(read, Ok(Ok(0)) | Ok(Err(_))),
        "the listener closed the idle connection: {read:?}"
    );
}

/// The timeout bounds the whole head, so a byte that starts one buys a stalled peer no more time than silence would.
#[tokio::test]
async fn a_plain_peer_that_sends_one_byte_and_stalls_is_closed_at_the_header_read_timeout() {
    let limits = phoebis_router::listen::ListenLimits {
        drain_timeout: Duration::from_secs(30),
        header_read_timeout: Duration::from_millis(300),
        max_connections: 8,
        shutdown_mark: None,
    };
    let (addr, _stop) = listening(limits, None).await;
    let connected = std::time::Instant::now();
    let mut stalled = tokio::net::TcpStream::connect(addr).await.unwrap();
    stalled.write_all(b"G").await.unwrap();
    let mut answer = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(3), stalled.read_to_end(&mut answer)).await;
    assert!(
        matches!(read, Ok(Ok(_)) | Ok(Err(_))),
        "the listener closed the stalled connection: {read:?}"
    );
    // The byte itself is no reason to close: the head stays open until the timeout runs out.
    assert!(
        connected.elapsed() >= Duration::from_millis(200),
        "{:?}",
        connected.elapsed()
    );
    let answer = String::from_utf8_lossy(&answer);
    assert!(
        answer.is_empty() || answer.starts_with("HTTP/1.1 408"),
        "nothing but a timeout answer: {answer}"
    );
}

/// The handshake runs under the header read timeout, so a peer that opens a socket and sends no ClientHello is
/// held to it like a peer that sends no head; with one slot, the handshake behind it proves the slot came back.
#[tokio::test]
async fn a_tls_listener_drops_a_silent_peer_at_the_bound_and_serves_a_finished_handshake() {
    let pair = fake_upstream::generate_pair(&["localhost", "127.0.0.1"]);
    let acceptor = Arc::new(LiveAcceptor::new(fake_upstream::acceptor_for(&pair)));
    let limits = phoebis_router::listen::ListenLimits {
        drain_timeout: Duration::from_secs(30),
        header_read_timeout: Duration::from_millis(200),
        max_connections: 1,
        shutdown_mark: None,
    };
    let (addr, _stop) = listening(limits, Some(acceptor.clone())).await;
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
    tls.write_all(b"GET /ping HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut answer = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(3), tls.read_to_end(&mut answer)).await;
    let answer = String::from_utf8_lossy(&answer);
    assert!(
        answer.starts_with("HTTP/1.1 200") && answer.ends_with("pong"),
        "{answer}"
    );
}

#[tokio::test]
async fn the_connection_cap_holds_the_next_accept_until_a_slot_frees() {
    let limits = phoebis_router::listen::ListenLimits {
        drain_timeout: Duration::from_secs(30),
        header_read_timeout: Duration::from_millis(500),
        max_connections: 2,
        shutdown_mark: None,
    };
    let (addr, _stop) = listening(limits, None).await;
    let _first = tokio::net::TcpStream::connect(addr).await.unwrap();
    let _second = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut third = tokio::net::TcpStream::connect(addr).await.unwrap();
    third
        .write_all(b"GET /ping HTTP/1.1\r\nhost: x\r\n\r\n")
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
}

/// A peer that opens a socket on a TLS listener and sends nothing has no request to finish, so the stop ends its
/// handshake rather than waiting out the handshake's bound. The served request behind it proves the silent peer
/// was accepted, and its handshake running, before the stop.
#[tokio::test]
async fn a_stop_ends_a_silent_handshake_well_before_its_bound() {
    let pair = fake_upstream::generate_pair(&["localhost"]);
    let acceptor = Arc::new(LiveAcceptor::new(fake_upstream::acceptor_for(&pair)));
    let limits = phoebis_router::listen::ListenLimits {
        drain_timeout: Duration::from_secs(30),
        header_read_timeout: Duration::from_secs(30),
        max_connections: 8,
        shutdown_mark: None,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stop_rx) = tokio::sync::watch::channel(false);
    let server = tokio::spawn(phoebis_router::listen::serve(
        "admin",
        listener,
        app(),
        stop_rx,
        limits,
        Some(acceptor.clone()),
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
    tls.write_all(b"GET /ping HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut answer = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(3), tls.read_to_end(&mut answer)).await;
    assert!(String::from_utf8_lossy(&answer).starts_with("HTTP/1.1 200"));
    stop.send(true).unwrap();
    let returned = tokio::time::timeout(Duration::from_secs(3), server).await;
    assert!(
        matches!(returned, Ok(Ok(Ok(())))),
        "the listener returned with a silent peer mid-handshake: {returned:?}"
    );
}

/// The stop reaches a connection that is idle between requests, so the listener returns once it closes rather
/// than at the drain deadline or the next header read timeout.
#[tokio::test]
async fn a_stop_ends_an_idle_keep_alive_connection_well_before_the_drain_deadline() {
    let limits = phoebis_router::listen::ListenLimits {
        drain_timeout: Duration::from_secs(30),
        header_read_timeout: Duration::from_secs(30),
        max_connections: 8,
        shutdown_mark: None,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stop_rx) = tokio::sync::watch::channel(false);
    let server = tokio::spawn(phoebis_router::listen::serve(
        "admin",
        listener,
        app(),
        stop_rx,
        limits,
        None,
    ));
    let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
    client
        .write_all(b"GET /ping HTTP/1.1\r\nhost: x\r\n\r\n")
        .await
        .unwrap();
    let mut buf = [0u8; 256];
    let read = tokio::time::timeout(Duration::from_secs(3), client.read(&mut buf))
        .await
        .expect("the request is answered")
        .unwrap();
    assert!(std::str::from_utf8(&buf[..read])
        .unwrap()
        .starts_with("HTTP/1.1 200"));
    stop.send(true).unwrap();
    let returned = tokio::time::timeout(Duration::from_secs(5), server).await;
    assert!(
        matches!(returned, Ok(Ok(Ok(())))),
        "the listener returned once its idle connection closed: {returned:?}"
    );
}
