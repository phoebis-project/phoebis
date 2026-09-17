//! The proxy listener's accept loop.
//!
//! The relayed surface is served through hyper directly rather than through `axum::serve` so every accepted
//! stream can be wrapped in a [`HeadRecorder`]: the caller's original header spelling and order exist nowhere
//! else by the time a handler runs.

use std::sync::Arc;
use std::time::Duration;

use axum::http::Request;
use axum::Router;
use hyper_util::rt::TokioIo;
use hyper_util::server::graceful::GracefulShutdown;
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::casing::{head_queue, HeadQueue, HeadRecorder};
use crate::messages::{router, Relay};

/// How long the loop waits out a listener error. An accept that fails for a reason the process cannot fix,
/// such as a descriptor limit, fails again immediately, so the pause is what keeps the loop off the CPU.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);

/// Serves the relayed routes, and whatever `extra` adds beside them, until `stop` flips, then waits for the
/// connections already accepted. `extra` is merged rather than nested so a route on it keeps its full path;
/// the relay's own fallback still answers everything neither names.
pub async fn serve(
    listener: TcpListener,
    relay: Arc<Relay>,
    extra: Router,
    mut stop: watch::Receiver<bool>,
) -> std::io::Result<()> {
    let app = router(relay).merge(extra);
    // HTTP/1 only. An h2c caller's head reaches the relay through hyper's own map, in hyper's own spelling,
    // which is the silently different outbound shape the pass-through exists to prevent; refusing the
    // connection is the honest answer.
    //
    // Left at hyper's default parser configuration on purpose: the recorder ends a head where this builder's
    // `httparse` pass ends it, and a leniency option set here would have to be given to the recorder too.
    let builder = hyper::server::conn::http1::Builder::new();
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
        let connection = graceful.watch(builder.serve_connection(io, service));
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

/// Hands each request the head the recorder took from the same bytes, and only that head: the request line is
/// compared before the head is used, so a queue that ever drifts out of step with hyper costs the caller's
/// casing rather than putting one request's headers on another's wire.
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
        let mut queue = self.queue.lock().expect("head queue poisoned");
        if queue.front().is_some_and(|head| head.matches(&request)) {
            let head = queue
                .pop_front()
                .expect("the front head matched this request");
            request.extensions_mut().insert(head);
        } else {
            // Either the recorder gave this connection up, or it and hyper disagree about where the
            // connection is. Neither is worth the risk of relaying one request under another's headers, so
            // the queue is emptied and the request goes out on the header map.
            queue.clear();
        }
        drop(queue);
        self.inner.call(request)
    }
}
