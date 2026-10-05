//! The admin and metrics listeners share the accept loop the proxy listener uses: the same header read
//! timeout and connection cap, over a plain tower service.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::WebSocketUpgrade;
use axum::routing::get;
use boring::ssl::SslAcceptor;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn app() -> axum::Router {
    axum::Router::new()
        .route("/ping", get(|| async { "pong" }))
        .route(
            "/ws",
            get(|ws: WebSocketUpgrade| async move { ws.on_upgrade(|_socket| async {}) }),
        )
}

/// The stop sender comes back with the address: `stopped()` reads a dropped sender as a stop, so a test holds
/// it until its end or the listener ends first.
async fn listening(
    limits: phoebis_router::listen::ListenLimits,
    upgrades: bool,
    acceptor: Option<Arc<SslAcceptor>>,
) -> (std::net::SocketAddr, tokio::sync::watch::Sender<bool>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stop_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(phoebis_router::listen::serve(
        listener,
        app(),
        stop_rx,
        limits,
        upgrades,
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
    };
    for upgrades in [false, true] {
        let (addr, _stop) = listening(limits, upgrades, None).await;
        let mut idle = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut buf = [0u8; 16];
        let read = tokio::time::timeout(Duration::from_secs(3), idle.read(&mut buf)).await;
        assert!(
            matches!(read, Ok(Ok(0)) | Ok(Err(_))),
            "the listener closed the idle connection (upgrades {upgrades}): {read:?}"
        );
    }
}

/// The handshake runs under the header read timeout, so a peer that opens a socket and sends no ClientHello is
/// held to it like a peer that sends no head; with one slot, the handshake behind it proves the slot came back.
#[tokio::test]
async fn a_tls_listener_drops_a_silent_peer_at_the_bound_and_serves_a_finished_handshake() {
    let pair = fake_upstream::generate_pair(&["localhost", "127.0.0.1"]);
    let acceptor = Arc::new(fake_upstream::acceptor_for(&pair));
    let limits = phoebis_router::listen::ListenLimits {
        drain_timeout: Duration::from_secs(30),
        header_read_timeout: Duration::from_millis(200),
        max_connections: 1,
    };
    for upgrades in [false, true] {
        let (addr, _stop) = listening(limits, upgrades, Some(acceptor.clone())).await;
        let mut silent = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut buf = [0u8; 16];
        let read = tokio::time::timeout(Duration::from_secs(3), silent.read(&mut buf)).await;
        assert!(
            matches!(read, Ok(Ok(0)) | Ok(Err(_))),
            "the listener dropped the silent peer (upgrades {upgrades}): {read:?}"
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
        assert_eq!(
            tls.ssl().selected_alpn_protocol(),
            Some(&b"http/1.1"[..]),
            "upgrades {upgrades}"
        );
        tls.write_all(b"GET /ping HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut answer = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(3), tls.read_to_end(&mut answer)).await;
        let answer = String::from_utf8_lossy(&answer);
        assert!(
            answer.starts_with("HTTP/1.1 200") && answer.ends_with("pong"),
            "upgrades {upgrades}: {answer}"
        );
    }
}

#[tokio::test]
async fn the_connection_cap_holds_the_next_accept_until_a_slot_frees() {
    let limits = phoebis_router::listen::ListenLimits {
        drain_timeout: Duration::from_secs(30),
        header_read_timeout: Duration::from_millis(500),
        max_connections: 2,
    };
    let (addr, _stop) = listening(limits, false, None).await;
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

/// The admin listener's path, the one that carries the live WebSocket: a client that opens with the first byte
/// of the HTTP/2 preface and stops is held to the header read timeout like one that sends nothing, so it cannot
/// keep a slot for the life of the process.
#[tokio::test]
async fn a_connection_that_stops_after_one_byte_is_closed_at_the_header_read_timeout_on_the_upgrade_path(
) {
    let limits = phoebis_router::listen::ListenLimits {
        drain_timeout: Duration::from_secs(30),
        header_read_timeout: Duration::from_millis(300),
        max_connections: 1,
    };
    let (addr, _stop) = listening(limits, true, None).await;
    let mut stalled = tokio::net::TcpStream::connect(addr).await.unwrap();
    stalled.write_all(b"P").await.unwrap();
    let mut waiting = tokio::net::TcpStream::connect(addr).await.unwrap();
    waiting
        .write_all(b"GET /ping HTTP/1.1\r\nhost: x\r\n\r\n")
        .await
        .unwrap();
    let mut buf = [0u8; 64];
    let read = tokio::time::timeout(Duration::from_secs(3), stalled.read(&mut buf)).await;
    assert!(
        matches!(read, Ok(Ok(0)) | Ok(Err(_))),
        "the listener closed the stalled connection: {read:?}"
    );
    let served = tokio::time::timeout(Duration::from_secs(3), waiting.read(&mut buf))
        .await
        .expect("the request waiting behind the stalled connection was admitted")
        .unwrap();
    assert!(std::str::from_utf8(&buf[..served])
        .unwrap()
        .starts_with("HTTP/1.1 200"));
}

#[tokio::test]
async fn the_upgrade_path_hands_a_websocket_handshake_to_its_route() {
    let limits = phoebis_router::listen::ListenLimits {
        drain_timeout: Duration::from_secs(30),
        header_read_timeout: Duration::from_millis(500),
        max_connections: 8,
    };
    let (addr, _stop) = listening(limits, true, None).await;
    let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
    client
        .write_all(
            b"GET /ws HTTP/1.1\r\nhost: x\r\nconnection: upgrade\r\nupgrade: websocket\r\n\
              sec-websocket-version: 13\r\nsec-websocket-key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
        )
        .await
        .unwrap();
    let mut buf = [0u8; 256];
    let read = tokio::time::timeout(Duration::from_secs(3), client.read(&mut buf))
        .await
        .expect("the handshake is answered")
        .unwrap();
    assert!(
        std::str::from_utf8(&buf[..read])
            .unwrap()
            .starts_with("HTTP/1.1 101"),
        "the route took the upgrade: {:?}",
        String::from_utf8_lossy(&buf[..read])
    );
}

/// A peer that opens a socket on a TLS listener and sends nothing has no request to finish, so the stop ends its
/// handshake on either path rather than waiting out the handshake's bound. The served request behind it proves the
/// silent peer was accepted, and its handshake running, before the stop.
#[tokio::test]
async fn a_stop_ends_a_silent_handshake_well_before_its_bound() {
    let pair = fake_upstream::generate_pair(&["localhost"]);
    let acceptor = Arc::new(fake_upstream::acceptor_for(&pair));
    let limits = phoebis_router::listen::ListenLimits {
        drain_timeout: Duration::from_secs(30),
        header_read_timeout: Duration::from_secs(30),
        max_connections: 8,
    };
    for upgrades in [false, true] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stop_rx) = tokio::sync::watch::channel(false);
        let server = tokio::spawn(phoebis_router::listen::serve(
            listener,
            app(),
            stop_rx,
            limits,
            upgrades,
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
        assert!(
            String::from_utf8_lossy(&answer).starts_with("HTTP/1.1 200"),
            "upgrades {upgrades}"
        );
        stop.send(true).unwrap();
        let returned = tokio::time::timeout(Duration::from_secs(3), server).await;
        assert!(
            matches!(returned, Ok(Ok(Ok(())))),
            "the listener returned with a silent peer mid-handshake (upgrades {upgrades}): {returned:?}"
        );
    }
}

/// The stop reaches a connection that is idle between requests on either path, so the listener returns once it
/// closes rather than at the drain deadline or the next header read timeout.
#[tokio::test]
async fn a_stop_ends_an_idle_keep_alive_connection_well_before_the_drain_deadline() {
    let limits = phoebis_router::listen::ListenLimits {
        drain_timeout: Duration::from_secs(30),
        header_read_timeout: Duration::from_secs(30),
        max_connections: 8,
    };
    for upgrades in [false, true] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stop_rx) = tokio::sync::watch::channel(false);
        let server = tokio::spawn(phoebis_router::listen::serve(
            listener,
            app(),
            stop_rx,
            limits,
            upgrades,
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
            "the listener returned once its idle connection closed (upgrades {upgrades}): {returned:?}"
        );
    }
}
