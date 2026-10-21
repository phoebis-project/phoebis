//! One upstream attempt: the connection, the write, the response head, and the two ways it can fail without
//! an answer. Every wait here races the selected account's cancellation token.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

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

/// The wait before the one retry of a request whose connection could not be opened: it gives a listener that
/// refused, or a handshake that failed, a moment to come back. A reused connection that died on the write is retried
/// at once, because the host answered on it a moment before.
pub(crate) const CONNECT_RETRY_WAIT: Duration = Duration::from_secs(1);

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
    Canceled,
    /// The upstream may have received the whole request and acted on it, so it must not be sent again.
    #[error("the response was lost: {0}")]
    Lost(String),
    /// The request was never written whole into a live connection, so nothing upstream can have acted on it.
    /// `connect` says no connection was opened at all (the checkout's error, the handshake's error or the connect
    /// bound's expiry), as against a checked-out connection that died on the write.
    #[error("the request never reached the upstream: {detail}")]
    Unsent { detail: String, connect: bool },
}

/// `None` once the account is gone. A read that outlives a disconnect must not classify, refresh, or mark
/// anything: the account it would act on no longer holds the credential the response was about.
pub(crate) async fn until_canceled<T>(
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
        let checkout = async {
            let (conn, reused) = if fresh {
                (checkout_fresh(&relay.pool, key, host, addr).await?, false)
            } else {
                relay
                    .pool
                    .checkout(key.clone(), host, addr)
                    .await
                    .map_err(unsent)?
            };
            if !reused {
                relay.note_opened();
            }
            Ok::<_, AttemptFailure>(conn)
        };
        // The connect and the handshake have their own bound: a stalled handshake would otherwise wait out the
        // whole head timeout, ten minutes, before the one retry on a fresh connection.
        let conn = tokio::time::timeout(relay.limits.connect_timeout, checkout)
            .await
            .map_err(|_| AttemptFailure::Unsent {
                detail: "timed out before a connection was ready".into(),
                connect: true,
            })??;
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
            Err(_) if !sending.load(Ordering::SeqCst) => Err(AttemptFailure::Unsent {
                detail: "timed out before a connection was ready".into(),
                connect: true,
            }),
            Err(_) => Err(AttemptFailure::Lost(
                "timed out sending the request or waiting for the response head".into(),
            )),
        },
        // The account was disconnected or deleted under this request; the connection goes with the future.
        _ = cancel.cancelled() => Err(AttemptFailure::Canceled),
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
            WriteProgress::NothingWritten | WriteProgress::HeadWritten => AttemptFailure::Unsent {
                detail: failure.error.to_string(),
                connect: false,
            },
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
    AttemptFailure::Unsent {
        detail: error.to_string(),
        connect: true,
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::task::{Context, Poll};

    use bytes::Bytes;
    use phoebis_oauth::Clock;
    use phoebis_transport::request::RawRequest;
    use phoebis_transport::TransportError;
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

    use super::{send_and_read, unsent, AttemptFailure};

    /// A connection that takes every byte of the request and then fails the flush.
    struct FlushFails;

    impl AsyncRead for FlushFails {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for FlushFails {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn a_flush_that_fails_after_the_whole_request_was_taken_is_lost_rather_than_unsent() {
        let request = RawRequest {
            method: "POST",
            target: "/v1/messages".into(),
            headers: vec![("Content-Length".into(), "2".into())],
            body: Bytes::from_static(b"{}"),
        };
        let clock: Clock = Arc::new(|| 0);
        let sending = AtomicBool::new(false);
        let failure = send_and_read(FlushFails, &request, &sending, &clock)
            .await
            .err()
            .expect("a failed flush fails the attempt");
        assert!(matches!(failure, AttemptFailure::Lost(_)), "{failure}");
        assert!(sending.load(Ordering::SeqCst));
    }

    #[test]
    fn a_checkout_that_could_not_open_a_connection_is_unsent_as_a_connect_failure() {
        let failure = unsent(TransportError::Io(io::ErrorKind::ConnectionRefused.into()));
        assert!(
            matches!(failure, AttemptFailure::Unsent { connect: true, .. }),
            "{failure:?}"
        );
    }

    #[tokio::test]
    async fn a_write_that_fails_on_a_checked_out_connection_is_unsent_but_no_connect_failure() {
        let request = RawRequest {
            method: "POST",
            target: "/v1/messages".into(),
            headers: vec![("Content-Length".into(), "2".into())],
            body: Bytes::from_static(b"{}"),
        };
        let clock: Clock = Arc::new(|| 0);
        let sending = AtomicBool::new(false);
        // An open connection whose peer has gone, so the first write fails before any byte is taken.
        let (conn, peer) = tokio::io::duplex(64);
        drop(peer);
        let failure = send_and_read(conn, &request, &sending, &clock)
            .await
            .err()
            .expect("a failed write fails the attempt");
        assert!(
            matches!(failure, AttemptFailure::Unsent { connect: false, .. }),
            "{failure:?}"
        );
    }
}
