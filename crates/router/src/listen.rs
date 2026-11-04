//! The admin and metrics listeners' accept loop: the relay's own, with the same header read timeout and
//! connection cap the proxy listener uses, over a plain tower service rather than the relay's header-recording one.

use std::sync::Arc;

use axum::Router;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;
use tokio::sync::watch;

pub use phoebis_relay::ListenLimits;
use phoebis_relay::{accept_loop, LiveAcceptor};

/// Serves `app` until `stop` flips, then waits for the connections already accepted. With `acceptor` set, every
/// connection completes a TLS handshake before hyper reads it, on the acceptor current when it was accepted. `name`
/// labels the loop's log lines.
pub async fn serve(
    name: &'static str,
    listener: TcpListener,
    app: Router,
    stop: watch::Receiver<bool>,
    limits: ListenLimits,
    acceptor: Option<Arc<LiveAcceptor>>,
) -> std::io::Result<()> {
    // Under `http1_only` the auto builder skips its HTTP/2 preface sniff, a read with no timeout of its own
    // (hyperium/hyper#3756, open as of hyper-util 0.1.20), so hyper's header read timeout runs from the
    // connection's first poll, before a byte arrives.
    let mut plain = auto::Builder::new(TokioExecutor::new()).http1_only();
    // hyper's own 30 second default is silently off until a timer is set; the timeout is named here so the
    // listener's bound does not depend on a default that a timer-less builder discards.
    plain
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(limits.header_read_timeout);
    // Shared rather than borrowed per connection: the auto builder's `serve_connection` returns a future
    // carrying the builder's own lifetime, which a spawned task can only outlive by owning it.
    let plain = Arc::new(plain);
    accept_loop(
        name,
        listener,
        stop,
        limits,
        acceptor,
        move |stream, watcher| {
            let plain = Arc::clone(&plain);
            let service = TowerToHyperService::new(app.clone());
            // The connection is built inside the future rather than before it, because the future the auto
            // builder's `serve_connection` returns borrows the builder.
            async move {
                watcher
                    .watch(plain.serve_connection(TokioIo::new(stream), service))
                    .await
            }
        },
    )
    .await
}
