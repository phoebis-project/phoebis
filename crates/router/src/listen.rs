//! The admin and metrics listeners' accept loop: the same header read timeout and connection cap the proxy
//! listener uses, over a plain tower service rather than the relay's header-recording one.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto::Builder;
use hyper_util::server::graceful::GracefulShutdown;
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;
use tokio::sync::{watch, Semaphore};

pub use phoebis_relay::ListenLimits;

/// How long the loop waits out a listener error. An accept that fails for a reason the process cannot fix,
/// such as a descriptor limit, fails again immediately, so the pause is what keeps the loop off the CPU.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);

/// Serves `app` until `stop` flips, then waits for the connections already accepted. `upgrades` carries the
/// admin listener's live WebSocket through hyper's upgrade path; the metrics listener, which never upgrades a
/// connection, runs without it.
pub async fn serve(
    listener: TcpListener,
    app: Router,
    mut stop: watch::Receiver<bool>,
    limits: ListenLimits,
    upgrades: bool,
) -> std::io::Result<()> {
    // No route on either listener switches protocols on its own, and `http1_only` closes the auto builder's
    // HTTP/2 preface sniff, which otherwise runs ahead of the timeout set below (see the guard in the accept
    // loop); it has no effect on the upgrade path, which the loop's own guard covers instead.
    let mut builder = Builder::new(TokioExecutor::new()).http1_only();
    // hyper's own 30 second default is silently off until a timer is set; the timeout is named here so the
    // listener's bound does not depend on a default that a timer-less builder discards.
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(limits.header_read_timeout);
    // Shared rather than borrowed per connection: the auto builder's `serve_connection*` methods return a
    // future carrying the builder's own lifetime, which a spawned task can only outlive by owning it.
    let builder = Arc::new(builder);
    let slots = Arc::new(Semaphore::new(limits.max_connections));
    let graceful = GracefulShutdown::new();
    loop {
        // A permit is taken before the accept so a full listener leaves connections in the kernel's backlog
        // rather than holding a task and a buffer for each; the permit rides with the connection task.
        let permit = tokio::select! {
            permit = slots.clone().acquire_owned() => permit.expect("the listener's semaphore is never closed"),
            _ = stopped(&mut stop) => break,
        };
        let stream = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => stream,
                Err(error) => {
                    tracing::debug!(%error, "the listener could not accept a connection");
                    tokio::time::sleep(ACCEPT_BACKOFF).await;
                    continue;
                }
            },
            _ = stopped(&mut stop) => break,
        };
        let _ = stream.set_nodelay(true);
        let service = TowerToHyperService::new(app.clone());
        let builder = Arc::clone(&builder);
        let watcher = graceful.watcher();
        let header_read_timeout = limits.header_read_timeout;
        // The permit is released once the connection future itself ends: for an upgraded socket that is when
        // hyper hands it to its `on_upgrade` task, not when the socket later closes, so the live crate's own
        // cap is what bounds those sockets from here on. `serve_connection*` is called inside the spawned
        // task, not before it, because the future it returns borrows the builder that produced it.
        tokio::spawn(async move {
            let _slot = permit;
            // The auto builder's protocol sniff reads for the HTTP/2 preface ahead of the HTTP/1 dispatcher,
            // and that read carries no timeout of its own (hyperium/hyper#3756, open as of hyper-util
            // 0.1.20): a connection that never sends a byte would otherwise wait here forever, past the
            // `header_read_timeout` configured above. Requiring readiness under the same deadline first
            // closes that gap without touching a connection that is already sending its head.
            if tokio::time::timeout(header_read_timeout, stream.readable())
                .await
                .is_err()
            {
                tracing::debug!("a connection sent no bytes within the header read timeout");
                return;
            }
            let io = TokioIo::new(stream);
            let result = if upgrades {
                watcher
                    .watch(builder.serve_connection_with_upgrades(io, service))
                    .await
            } else {
                watcher.watch(builder.serve_connection(io, service)).await
            };
            if let Err(error) = result {
                tracing::debug!(%error, "a connection ended early");
            }
        });
    }
    // Dropped before the wait so the port is free the moment the drain starts rather than when it ends.
    drop(listener);
    graceful.shutdown().await;
    Ok(())
}

/// Resolves once the stop flag is set. The borrowed guard `wait_for` yields is not `Send`, so it is dropped
/// here rather than becoming part of the accept loop's future.
async fn stopped(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|stop| *stop).await;
}
