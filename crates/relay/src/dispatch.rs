//! One upstream attempt: the connection, the write, the response head, and the two ways it can fail without
//! an answer. Every wait here races the selected account's cancellation token.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};

use bytes::BytesMut;
use phoebis_oauth::Clock;
use phoebis_transport::pool::{Conn, Pool, PoolKey};
use phoebis_transport::request::{write_request, RawRequest, WriteProgress};
use phoebis_transport::response::{read_head, ResponseHead};
use phoebis_transport::TransportError;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::sync::CancellationToken;

use crate::messages::Relay;

/// The one authority the relay dials: `api.anthropic.com` on 443, so the `Host` value, the resolver's input,
/// and the pool key's host are the bare name.
pub(crate) const UPSTREAM_AUTHORITY: &str = phoebis_store::INFERENCE_HOST;

pub(crate) struct Sent<C> {
    pub conn: C,
    pub head: ResponseHead,
    pub leftover: BytesMut,
    /// When the request was in the socket, which is where a waterfall separates the write from the wait.
    pub written_at: i64,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum AttemptFailure {
    /// The account the request was bound to went away, so nothing about this attempt is actionable.
    #[error("the bound account was disconnected")]
    Cancelled,
    /// The upstream may have received the whole request and acted on it, so it must not be sent again.
    #[error("the response was lost: {0}")]
    Lost(String),
    /// The request was never written whole into a live connection, so nothing upstream can have acted on it.
    #[error("the request never reached the upstream: {0}")]
    Unsent(String),
}

/// `None` once the account is gone. A read that outlives a disconnect must not classify, refresh, or mark
/// anything: the account it would act on no longer holds the credential the response was about.
pub(crate) async fn until_cancelled<T>(
    work: impl Future<Output = T>,
    cancel: &CancellationToken,
) -> Option<T> {
    tokio::select! {
        result = work => Some(result),
        _ = cancel.cancelled() => None,
    }
}

pub(crate) async fn attempt(
    relay: &Relay,
    request: &RawRequest,
    key: &PoolKey,
    host: &str,
    addr: SocketAddr,
    fresh: bool,
    cancel: &CancellationToken,
) -> Result<Sent<Conn>, AttemptFailure> {
    // Read by the timeout arms, which cannot see how far the attempt itself got. It is raised before the
    // first byte goes out rather than after the last one, because a timeout that fires part way through
    // the write leaves the request in the socket for an upstream that may already be acting on it.
    let sending = AtomicBool::new(false);
    let once = async {
        let conn = if fresh {
            checkout_fresh(&relay.pool, key, host, addr).await?
        } else {
            relay
                .pool
                .checkout(key.clone(), host, addr)
                .await
                .map(|(conn, _)| conn)
                .map_err(unsent)?
        };
        send_and_read(conn, request, &sending, &relay.clock).await
    };
    bounded(relay, cancel, &sending, once).await
}

/// The head timeout and the account's cancellation, which bound an attempt on either plane.
async fn bounded<T>(
    relay: &Relay,
    cancel: &CancellationToken,
    sending: &AtomicBool,
    once: impl Future<Output = Result<T, AttemptFailure>>,
) -> Result<T, AttemptFailure> {
    let work = tokio::time::timeout(relay.limits.head_timeout, once);
    tokio::select! {
        result = work => match result {
            Ok(result) => result,
            Err(_) if !sending.load(Ordering::SeqCst) => Err(AttemptFailure::Unsent(
                "timed out before a connection was ready".into(),
            )),
            Err(_) => Err(AttemptFailure::Lost(
                "timed out sending the request or waiting for the response head".into(),
            )),
        },
        // The account was disconnected or deleted under this request; the connection goes with the future.
        _ = cancel.cancelled() => Err(AttemptFailure::Cancelled),
    }
}

/// Writes the request into a connection that is already open and reads the response head back.
async fn send_and_read<C: AsyncRead + AsyncWrite + Unpin>(
    mut conn: C,
    request: &RawRequest,
    sending: &AtomicBool,
    clock: &Clock,
) -> Result<Sent<C>, AttemptFailure> {
    // From here on the request may be on the wire, which is what the timeout arms need to know; the
    // write's own failures carry their progress and are classified below.
    sending.store(true, Ordering::SeqCst);
    write_request(&mut conn, request)
        .await
        .map_err(|failure| match failure.progress {
            WriteProgress::NothingWritten | WriteProgress::HeadWritten => {
                AttemptFailure::Unsent(failure.error.to_string())
            }
            // The body is already in the socket and only the flush failed, so the upstream may well have
            // read the whole request.
            WriteProgress::BodyWritten => AttemptFailure::Lost(failure.error.to_string()),
        })?;
    let written_at = clock();
    let mut leftover = BytesMut::new();
    match read_head(&mut conn, &mut leftover).await {
        Ok(head) => Ok(Sent {
            conn,
            head,
            leftover,
            written_at,
        }),
        // The whole request is in the socket, so an upstream that read it and then closed without
        // answering is indistinguishable from one that never saw it. Sending it again would bill the
        // caller twice; the pool's liveness probe, not a retry, is what keeps a reaped idle connection
        // from reaching this point.
        Err(error) => Err(AttemptFailure::Lost(error.to_string())),
    }
}

/// Drains the idle connections for this key until the pool opens a new one. The retry that calls this has
/// just been failed by a connection that was already idle, so handing it another one would repeat the failure.
async fn checkout_fresh(
    pool: &Pool,
    key: &PoolKey,
    host: &str,
    addr: SocketAddr,
) -> Result<Conn, AttemptFailure> {
    loop {
        let (conn, reused) = pool
            .checkout(key.clone(), host, addr)
            .await
            .map_err(unsent)?;
        if !reused {
            return Ok(conn);
        }
        drop(conn);
    }
}

fn unsent(error: TransportError) -> AttemptFailure {
    AttemptFailure::Unsent(error.to_string())
}
