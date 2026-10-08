//! The admin and metrics listeners' accept loop: the same header read timeout and connection cap the proxy
//! listener uses, over a plain tower service rather than the relay's header-recording one.

use std::future::Future;
use std::pin::{pin, Pin};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use hyper::server::conn::http1;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;
use tokio::sync::{watch, Semaphore};
use tokio::task::JoinSet;

pub use phoebis_relay::ListenLimits;
use phoebis_relay::{handshake, LiveAcceptor};

/// How long the loop waits out a listener error. An accept that fails for a reason the process cannot fix,
/// such as a descriptor limit, fails again immediately, so the pause is what keeps the loop off the CPU.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);

/// Serves `app` until `stop` flips, then waits for the connections already accepted. `upgrades` carries the
/// admin listener's live WebSocket through hyper's upgrade path; the metrics listener, which never upgrades a
/// connection, runs without it. With `acceptor` set, every connection completes a TLS handshake before hyper
/// reads it, on the acceptor current when it was accepted.
pub async fn serve(
    listener: TcpListener,
    app: Router,
    mut stop: watch::Receiver<bool>,
    limits: ListenLimits,
    upgrades: bool,
    acceptor: Option<Arc<LiveAcceptor>>,
) -> std::io::Result<()> {
    // The admin listener's live WebSocket goes through hyper's own HTTP/1 builder: the auto builder's upgrade
    // path runs its HTTP/2 preface sniff whatever `http1_only` says, and that read has no timeout of its own
    // (hyperium/hyper#3756, open as of hyper-util 0.1.20), so a client that sent one byte of the preface would
    // keep its slot for good. The metrics listener never upgrades, and under `http1_only` the auto builder's
    // plain path skips the sniff. Either way hyper's header read timeout runs from the connection's first
    // poll, before a byte arrives.
    let mut upgrading = http1::Builder::new();
    // hyper's own 30 second default is silently off until a timer is set; the timeout is named here so the
    // listener's bound does not depend on a default that a timer-less builder discards.
    upgrading
        .timer(TokioTimer::new())
        .header_read_timeout(limits.header_read_timeout);
    let mut plain = auto::Builder::new(TokioExecutor::new()).http1_only();
    plain
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(limits.header_read_timeout);
    // Shared rather than borrowed per connection: the auto builder's `serve_connection` returns a future
    // carrying the builder's own lifetime, which a spawned task can only outlive by owning it.
    let plain = Arc::new(plain);
    let slots = Arc::new(Semaphore::new(limits.max_connections));
    let mut tasks = JoinSet::new();
    loop {
        while tasks.try_join_next().is_some() {}
        // A permit is taken before the accept so a full listener leaves connections in the kernel's backlog
        // rather than holding a task and a buffer for each; the permit rides with the connection task.
        let permit = tokio::select! {
            permit = slots.clone().acquire_owned() => permit.expect("the listener's semaphore is never closed"),
            _ = stopped(&mut stop) => break,
        };
        let (stream, peer) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(accepted) => accepted,
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
        let upgrading = upgrading.clone();
        let plain = Arc::clone(&plain);
        let mut drain = stop.clone();
        let acceptor = acceptor.as_ref().map(|live| live.current());
        // The permit is released once the connection future itself ends: for an upgraded socket that is when
        // hyper hands it to its `on_upgrade` task, not when the socket later closes, so the live crate's own
        // cap is what bounds those sockets from here on. The auto builder's `serve_connection` is called
        // inside the spawned task, not before it, because the future it returns borrows the builder.
        tasks.spawn(async move {
            let _slot = permit;
            let handshaken = tokio::select! {
                // Biased toward the handshake: a plain listener's resolves on its first poll, so the stop never
                // drops a plain connection here.
                biased;
                handshaken = handshake(stream, acceptor.as_deref(), limits.header_read_timeout) => handshaken,
                // A peer still in its handshake has sent no request, so the stop ends it as hyper's graceful
                // shutdown ends a connection that has read no byte, rather than waiting out the bound.
                _ = stopped(&mut drain) => return,
            };
            let stream = match handshaken {
                Ok(stream) => stream,
                Err(error) => {
                    // A plain-HTTP request on a TLS port lands here too; the bytes are never logged.
                    tracing::debug!(%peer, %error, "a connection ended at the handshake");
                    return;
                }
            };
            let io = TokioIo::new(stream);
            let ended: Result<(), Box<dyn std::error::Error + Send + Sync>> = if upgrades {
                let conn = upgrading.serve_connection(io, service).with_upgrades();
                until_drained(conn, http1::UpgradeableConnection::graceful_shutdown, drain)
                    .await
                    .map_err(Into::into)
            } else {
                let conn = plain.serve_connection(io, service);
                until_drained(conn, auto::Connection::graceful_shutdown, drain).await
            };
            if let Err(error) = ended {
                tracing::debug!(%error, "a connection ended early");
            }
        });
    }
    // Dropped before the wait so the port is free the moment the drain starts rather than when it ends.
    drop(listener);
    // Each connection watches the same stop and is finishing what it holds; the deadline is the operator's
    // promise that a shutdown ends within it, so what is still running then is cut rather than left to commit
    // after the final flush.
    let drained = async { while tasks.join_next().await.is_some() {} };
    if tokio::time::timeout(limits.drain_timeout, drained)
        .await
        .is_err()
    {
        tracing::warn!(
            seconds = limits.drain_timeout.as_secs(),
            "connections still open at the drain deadline were cut"
        );
        tasks.abort_all();
    }
    while tasks.join_next().await.is_some() {}
    Ok(())
}

/// Runs a connection to its end, asking it to finish gracefully once `stop` flips. hyper-util's graceful
/// watcher would do this, but 0.1.20 implements its `GracefulConnection` for hyper's plain HTTP/1 connection
/// and the auto builder's, not for the upgradeable HTTP/1 one, so both listeners take this one watch instead.
async fn until_drained<C, E>(
    conn: C,
    graceful_shutdown: impl FnOnce(Pin<&mut C>),
    mut stop: watch::Receiver<bool>,
) -> Result<(), E>
where
    C: Future<Output = Result<(), E>>,
{
    let mut conn = pin!(conn);
    tokio::select! {
        ended = conn.as_mut() => return ended,
        _ = stopped(&mut stop) => {}
    }
    graceful_shutdown(conn.as_mut());
    conn.await
}

/// Resolves once the stop flag is set. The borrowed guard `wait_for` yields is not `Send`, so it is dropped
/// here rather than becoming part of the accept loop's future.
async fn stopped(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|stop| *stop).await;
}
