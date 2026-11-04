//! The admin and metrics listeners' accept loop: the relay's own, with the same header read timeout and
//! connection cap the proxy listener uses, over a plain tower service rather than the relay's header-recording one.

use std::sync::Arc;

use axum::Router;
use hyper_util::rt::TokioIo;
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;
use tokio::sync::watch;

pub use phoebis_relay::ListenLimits;
use phoebis_relay::{accept_loop, http1_builder, LiveAcceptor};

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
    let builder = http1_builder(limits.header_read_timeout);
    accept_loop(
        name,
        listener,
        stop,
        limits,
        None,
        acceptor,
        move |stream, watcher| {
            watcher.watch(
                builder
                    .serve_connection(TokioIo::new(stream), TowerToHyperService::new(app.clone())),
            )
        },
    )
    .await
}
