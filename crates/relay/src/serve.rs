//! The accept loop the router's listeners share, and the connections of the proxy and the feature listener on it.
//!
//! The relayed surface is served through hyper directly rather than through `axum::serve` so every accepted
//! stream can be wrapped in a [`HeadRecorder`]: the caller's original header spelling and order exist nowhere
//! else by the time a handler runs.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use axum::extract::ConnectInfo;
use axum::http::Request;
use axum::Router;
use boring::ssl::SslAcceptor;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::{GracefulShutdown, Watcher};
use hyper_util::service::TowerToHyperService;
use phoebis_profile::FeatureTable;
use phoebis_transport::control::ControlClient;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, Semaphore};
use tokio::task::JoinSet;
use tokio_boring::SslStream;
use tokio_util::either::Either;

use crate::casing::{head_queue, HeadQueue, HeadRecorder};
use crate::messages::{router, Relay, ShutdownMark};

/// How long the loop waits out a listener error. An accept that fails for a reason the process cannot fix,
/// such as a descriptor limit, fails again immediately, so the pause is what keeps the loop off the CPU.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);

/// What bounds a connection before a request exists on it: the wait for its head, how many the listener holds
/// at once, and how long a graceful shutdown waits before it cuts what is still running. The bounds are compiled
/// in and the router names the values.
#[derive(Clone, Debug)]
pub struct Limits {
    pub drain_timeout: Duration,
    pub header_read_timeout: Duration,
    pub max_connections: usize,
}

/// A TLS listener's acceptor, replaceable while the router runs. Each accepted connection takes the current one
/// and finishes its handshake on it, so a reload never touches a connection in flight.
pub struct LiveAcceptor(RwLock<Arc<SslAcceptor>>);

impl LiveAcceptor {
    pub fn new(acceptor: SslAcceptor) -> Self {
        LiveAcceptor(RwLock::new(Arc::new(acceptor)))
    }

    /// The acceptor the next connection's handshake runs on.
    pub fn current(&self) -> Arc<SslAcceptor> {
        // The lock guards one `Arc` swap, which leaves no half-written state for a panic to poison.
        Arc::clone(&self.0.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// Puts `acceptor` in front of every connection accepted from here on.
    pub fn replace(&self, acceptor: SslAcceptor) {
        *self.0.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(acceptor);
    }
}

/// An accepted connection after its handshake, one type for both loops so hyper is built once per listener:
/// `Left` a plain stream, `Right` a TLS one.
pub type Accepted = Either<TcpStream, SslStream<TcpStream>>;

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
        return Ok(Either::Left(stream));
    };
    // hyper's header read timeout starts at its first poll, after this returns, so the handshake has a bound of
    // its own: a peer that opens a socket and sends no ClientHello holds a slot for the same time a peer that
    // sends no head does, and no longer.
    match tokio::time::timeout(bound, tokio_boring::accept(acceptor, stream)).await {
        Ok(Ok(tls)) => Ok(Either::Right(tls)),
        Ok(Err(error)) => Err(HandshakeError::Failed(error.to_string())),
        Err(_) => Err(HandshakeError::TimedOut(bound)),
    }
}

/// Serves the relayed routes, and whatever `extra` adds beside them, until `stop` flips, then waits for the
/// connections already accepted. `extra` is merged rather than nested so a route on it keeps its full path;
/// the relay's own fallback still answers everything neither names. With `acceptor` set, every connection
/// completes a TLS handshake before hyper reads it, on the acceptor current when it was accepted.
pub async fn serve(
    listener: TcpListener,
    relay: Arc<Relay>,
    extra: Router,
    stop: watch::Receiver<bool>,
    limits: Limits,
    acceptor: Option<Arc<LiveAcceptor>>,
) -> std::io::Result<()> {
    let shutdown_mark = relay.shutdown_mark();
    serve_recorded(
        "proxy",
        listener,
        router(relay).merge(extra),
        stop,
        limits,
        shutdown_mark,
        acceptor,
    )
    .await
}

/// Serves the feature module's routes until `stop` flips, each connection wrapped as a proxy listener's is, so a
/// head reaches the forward in the caller's spelling and order. The calls of the control plane leave through
/// `control`, and `table` is the feature table of the router's profile.
pub async fn serve_features(
    listener: TcpListener,
    relay: Arc<Relay>,
    control: Arc<ControlClient>,
    table: &'static FeatureTable,
    stop: watch::Receiver<bool>,
    limits: Limits,
    acceptor: Arc<LiveAcceptor>,
) -> std::io::Result<()> {
    let shutdown_mark = relay.shutdown_mark();
    serve_recorded(
        "features",
        listener,
        crate::features::router(relay, control, table),
        stop,
        limits,
        shutdown_mark,
        Some(acceptor),
    )
    .await
}

/// Serves `app` on `listener` until `stop` flips, each request with the head the connection's recorder took and the
/// connection's peer.
async fn serve_recorded(
    name: &'static str,
    listener: TcpListener,
    app: Router,
    stop: watch::Receiver<bool>,
    limits: Limits,
    shutdown_mark: Option<ShutdownMark>,
    acceptor: Option<Arc<LiveAcceptor>>,
) -> std::io::Result<()> {
    // HTTP/1 only. An h2c caller's head reaches the relay through hyper's own map, in hyper's own spelling,
    // which is the silently different outbound shape the pass-through exists to prevent, so refusing the
    // connection is the honest answer.
    //
    // Left at hyper's default parser configuration on purpose: the recorder ends a head where this builder's
    // `httparse` pass ends it, and a leniency option set here would have to be given to the recorder too.
    let builder = http1_builder(limits.header_read_timeout);
    accept_loop(
        name,
        listener,
        stop,
        limits,
        shutdown_mark,
        acceptor,
        move |stream, watcher| {
            let peer = peer_of(&stream);
            let queue = head_queue();
            let io = TokioIo::new(HeadRecorder::new(stream, queue.clone()));
            let service = WithOriginalHead {
                inner: TowerToHyperService::new(app.clone()),
                peer,
                queue,
            };
            // No upgrades: no route on the relayed surface switches protocols.
            watcher.watch(builder.serve_connection(io, service))
        },
    )
    .await
}

/// The peer of an accepted connection. A socket the system can no longer name a peer for, as one already reset, has
/// none, and its requests carry no `ConnectInfo`.
fn peer_of(stream: &Accepted) -> Option<ConnectInfo<SocketAddr>> {
    match stream {
        Either::Left(plain) => plain.peer_addr(),
        Either::Right(tls) => tls.get_ref().peer_addr(),
    }
    .ok()
    .map(ConnectInfo)
}

/// The HTTP/1 connection builder each listener serves on, with `header_read_timeout` as the bound on the wait for a
/// request head.
pub fn http1_builder(header_read_timeout: Duration) -> hyper::server::conn::http1::Builder {
    let mut builder = hyper::server::conn::http1::Builder::new();
    // hyper's own 30 second default is silently off until a timer is set. The timeout is named here so the
    // listener's bound does not depend on a default that a timer-less builder discards.
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(header_read_timeout);
    builder
}

/// Accepts on `listener` until `stop` flips, then waits for the connections already accepted, up to the drain
/// deadline. With `acceptor` set, every connection completes a TLS handshake under the header read timeout, on the
/// acceptor current when it was accepted. `connect` then builds the connection's hyper connection inside its task
/// and runs it under the watcher it is handed, so the stop asks every connection to finish what it holds. `name`
/// labels the loop's log lines, since each listener runs one. `shutdown_mark` is the relay's own, set before the drain
/// deadline's cut. The admin and the metrics listeners serve no relayed stream and pass `None`.
pub async fn accept_loop<C, F, E>(
    name: &'static str,
    listener: TcpListener,
    mut stop: watch::Receiver<bool>,
    limits: Limits,
    shutdown_mark: Option<ShutdownMark>,
    acceptor: Option<Arc<LiveAcceptor>>,
    connect: C,
) -> std::io::Result<()>
where
    C: Fn(Accepted, Watcher) -> F + Send + Sync + 'static,
    F: Future<Output = Result<(), E>> + Send + 'static,
    E: std::fmt::Display,
{
    let connect = Arc::new(connect);
    let slots = Arc::new(Semaphore::new(limits.max_connections));
    let graceful = GracefulShutdown::new();
    let mut tasks = JoinSet::new();
    loop {
        while tasks.try_join_next().is_some() {}
        // A permit is taken before the accept so a full listener leaves connections in the kernel's backlog
        // rather than holding a task and a buffer for each. The permit rides with the connection task.
        let permit = tokio::select! {
            permit = slots.clone().acquire_owned() => permit.expect("the listener's semaphore is never closed"),
            _ = stopped(&mut stop) => break,
        };
        let (stream, peer) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(accepted) => accepted,
                Err(error) => {
                    tracing::debug!(listener = name, %error, "the listener could not accept a connection");
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
        let acceptor = acceptor.as_ref().map(|live| live.current());
        let connect = Arc::clone(&connect);
        let mut drain = stop.clone();
        // The handshake runs here rather than in the accept loop, so a slow peer holds its own slot and not the
        // loop. The permit is dropped with the task, so a failed handshake releases it.
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
                    // A plain-HTTP request on a TLS port lands here too. The bytes are never logged.
                    tracing::debug!(listener = name, %peer, %error, "a connection ended at the handshake");
                    return;
                }
            };
            if let Err(error) = connect(stream, watcher).await {
                tracing::debug!(listener = name, %error, "a connection ended early");
            }
        });
    }
    // Dropped before the wait so the port is free the moment the drain starts rather than when it ends.
    drop(listener);
    // The graceful wait lets in-flight requests finish. The deadline is the operator's promise that a shutdown
    // ends within it, so what is still running then is cut rather than left to commit after the final flush.
    if tokio::time::timeout(limits.drain_timeout, graceful.shutdown())
        .await
        .is_err()
    {
        tracing::warn!(
            listener = name,
            seconds = limits.drain_timeout.as_secs(),
            "connections still open at the drain deadline were cut"
        );
        // The router's own abort can come after this deadline, so this cut sets the mark too, ahead of the abort, and
        // every end it causes books the router's shutdown rather than a caller's leaving.
        if !tasks.is_empty() {
            if let Some(mark) = &shutdown_mark {
                mark.store(true, Ordering::SeqCst);
            }
        }
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
    /// The connection's peer, which the request's own head never names.
    peer: Option<ConnectInfo<SocketAddr>>,
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
        if let Some(peer) = self.peer {
            request.extensions_mut().insert(peer);
        }
        self.inner.call(request)
    }
}
