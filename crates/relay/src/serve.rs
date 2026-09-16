//! The proxy listener's accept loop.
//!
//! The relayed surface is served through hyper directly rather than through `axum::serve` so every accepted
//! stream can be wrapped in a [`HeadRecorder`]: the caller's original header spelling and order exist nowhere
//! else by the time a handler runs.

use std::sync::Arc;
use std::time::Duration;

use axum::http::Request;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use hyper_util::server::graceful::GracefulShutdown;
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::casing::{head_queue, HeadQueue, HeadRecorder};
use crate::messages::{router, Relay};

/// How long the loop waits out a listener error. An accept that fails for a reason the process cannot fix,
/// such as a descriptor limit, fails again immediately, so the pause is what keeps the loop off the CPU.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);

/// Serves the relayed routes until `stop` flips, then waits for the connections already accepted.
pub async fn serve(
    listener: TcpListener,
    relay: Arc<Relay>,
    mut stop: watch::Receiver<bool>,
) -> std::io::Result<()> {
    let app = router(relay);
    let builder = auto::Builder::new(TokioExecutor::new());
    let graceful = GracefulShutdown::new();
    loop {
        let stream = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => stream,
                Err(error) => {
                    tracing::debug!(%error, "the proxy listener could not accept a connection");
                    tokio::time::sleep(ACCEPT_BACKOFF).await;
                    continue;
                }
            },
            _ = stopped(&mut stop) => break,
        };
        // The measured client writes a request as soon as it has one, so batching its head with whatever
        // follows would only add a round trip's worth of latency.
        let _ = stream.set_nodelay(true);
        let queue = head_queue();
        let io = TokioIo::new(HeadRecorder::new(stream, queue.clone()));
        let service = WithOriginalHead {
            inner: TowerToHyperService::new(app.clone()),
            queue,
        };
        // No upgrades: no route on the relayed surface switches protocols.
        let connection = graceful.watch(builder.serve_connection(io, service).into_owned());
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                tracing::debug!(%error, "a relayed connection ended early");
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

/// Hands each request the head the recorder took from the same bytes. hyper parses one request at a time per
/// connection and the measured client does not pipeline, so the queue holds at most one head when a request
/// arrives; an empty queue inserts nothing and the outbound builder falls back to the `HeaderMap`.
struct WithOriginalHead<S> {
    inner: S,
    queue: HeadQueue,
}

impl<S, B> hyper::service::Service<Request<B>> for WithOriginalHead<S>
where
    S: hyper::service::Service<Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn call(&self, mut request: Request<B>) -> Self::Future {
        if let Some(head) = self.queue.lock().expect("head queue poisoned").pop_front() {
            request.extensions_mut().insert(head);
        }
        self.inner.call(request)
    }
}
