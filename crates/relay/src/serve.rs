//! The proxy listener's accept loop.
//!
//! The relayed surface is served through hyper directly rather than through `axum::serve` so every accepted
//! stream can be wrapped in a [`HeadRecorder`]: the caller's original header spelling and order exist nowhere
//! else by the time a handler runs.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::http::Request;
use axum::Router;
use boring::ssl::SslAcceptor;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use hyper_util::service::TowerToHyperService;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, Semaphore};
use tokio::task::JoinSet;
use tokio_boring::SslStream;

use crate::casing::{head_queue, HeadQueue, HeadRecorder};
use crate::messages::{router, Relay};

/// How long the loop waits out a listener error. An accept that fails for a reason the process cannot fix,
/// such as a descriptor limit, fails again immediately, so the pause is what keeps the loop off the CPU.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);

/// What bounds a connection before a request exists on it: the wait for its head, how many the listener holds
/// at once, and how long a graceful shutdown waits before it cuts what is still running. All compiled in; the
/// router names the values.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub drain_timeout: Duration,
    pub header_read_timeout: Duration,
    pub max_connections: usize,
}

/// An accepted connection after its handshake, one type for both loops so hyper is built once per listener.
pub enum Accepted {
    Plain(TcpStream),
    Tls(SslStream<TcpStream>),
}

impl AsyncRead for Accepted {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Accepted::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
            Accepted::Tls(stream) => Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Accepted {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Accepted::Plain(stream) => Pin::new(stream).poll_write(cx, buf),
            Accepted::Tls(stream) => Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Accepted::Plain(stream) => Pin::new(stream).poll_write_vectored(cx, bufs),
            Accepted::Tls(stream) => Pin::new(stream).poll_write_vectored(cx, bufs),
        }
    }

    fn is_write_vectored(&self) -> bool {
        match self {
            Accepted::Plain(stream) => stream.is_write_vectored(),
            Accepted::Tls(stream) => stream.is_write_vectored(),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Accepted::Plain(stream) => Pin::new(stream).poll_flush(cx),
            Accepted::Tls(stream) => Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Accepted::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
            Accepted::Tls(stream) => Pin::new(stream).poll_shutdown(cx),
        }
    }
}

/// Why a connection ended before hyper saw it.
#[derive(Debug, thiserror::Error)]
pub enum HandshakeError {
    #[error("the handshake did not finish within {0:?}")]
    TimedOut(Duration),
    #[error("{0}")]
    Failed(String),
}

/// Runs the TLS handshake when `acceptor` is set, under `bound`; a plain listener answers at once. The error names
/// the reason and never a byte of the stream.
pub async fn handshake(
    stream: TcpStream,
    acceptor: Option<&SslAcceptor>,
    bound: Duration,
) -> Result<Accepted, HandshakeError> {
    let Some(acceptor) = acceptor else {
        return Ok(Accepted::Plain(stream));
    };
    // hyper's header read timeout starts at its first poll, after this returns, so the handshake has a bound of
    // its own: a peer that opens a socket and sends no ClientHello holds a slot for the same time a peer that
    // sends no head does, and no longer.
    match tokio::time::timeout(bound, tokio_boring::accept(acceptor, stream)).await {
        Ok(Ok(tls)) => Ok(Accepted::Tls(tls)),
        Ok(Err(error)) => Err(HandshakeError::Failed(error.to_string())),
        Err(_) => Err(HandshakeError::TimedOut(bound)),
    }
}

/// Serves the relayed routes, and whatever `extra` adds beside them, until `stop` flips, then waits for the
/// connections already accepted. `extra` is merged rather than nested so a route on it keeps its full path;
/// the relay's own fallback still answers everything neither names. With `acceptor` set, every connection
/// completes a TLS handshake before hyper reads it.
pub async fn serve(
    listener: TcpListener,
    relay: Arc<Relay>,
    extra: Router,
    mut stop: watch::Receiver<bool>,
    limits: Limits,
    acceptor: Option<Arc<SslAcceptor>>,
) -> std::io::Result<()> {
    let app = router(relay).merge(extra);
    // HTTP/1 only. An h2c caller's head reaches the relay through hyper's own map, in hyper's own spelling,
    // which is the silently different outbound shape the pass-through exists to prevent; refusing the
    // connection is the honest answer.
    //
    // Left at hyper's default parser configuration on purpose: the recorder ends a head where this builder's
    // `httparse` pass ends it, and a leniency option set here would have to be given to the recorder too.
    let mut builder = hyper::server::conn::http1::Builder::new();
    // hyper's own 30 second default is silently off until a timer is set; the timeout is named here so the
    // listener's bound does not depend on a default that a timer-less builder discards.
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(limits.header_read_timeout);
    let slots = Arc::new(Semaphore::new(limits.max_connections));
    let graceful = GracefulShutdown::new();
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
        let watcher = graceful.watcher();
        let builder = builder.clone();
        let acceptor = acceptor.clone();
        let app = app.clone();
        // The handshake runs here rather than in the accept loop, so a slow peer holds its own slot and not the
        // loop; the permit is dropped with the task, so a failed handshake releases it.
        tasks.spawn(async move {
            let _slot = permit;
            let stream = match handshake(stream, acceptor.as_deref(), limits.header_read_timeout)
                .await
            {
                Ok(stream) => stream,
                Err(error) => {
                    // A plain-HTTP request on a TLS port lands here too; the bytes are never logged.
                    tracing::debug!(%peer, %error, "a relayed connection ended at the handshake");
                    return;
                }
            };
            let queue = head_queue();
            let io = TokioIo::new(HeadRecorder::new(stream, queue.clone()));
            let service = WithOriginalHead {
                inner: TowerToHyperService::new(app),
                queue,
            };
            // No upgrades: no route on the relayed surface switches protocols.
            if let Err(error) = watcher.watch(builder.serve_connection(io, service)).await {
                tracing::debug!(%error, "a relayed connection ended early");
            }
        });
    }
    // Dropped before the wait so the port is free the moment the drain starts rather than when it ends.
    drop(listener);
    // The graceful wait lets in-flight requests finish; the deadline is the operator's promise that a
    // shutdown ends within it, so what is still running then is cut rather than left to commit after the
    // final flush.
    if tokio::time::timeout(limits.drain_timeout, graceful.shutdown())
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
