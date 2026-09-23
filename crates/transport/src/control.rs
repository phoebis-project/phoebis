//! Control-plane client: one TCP connection per request, used once and dropped.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use boring::ssl::SslConnector;
use bytes::{Bytes, BytesMut};
use phoebis_profile::Profile;
use tokio::io::AsyncWriteExt;

use crate::decode::{decoder, encoding};
use crate::request::{serialize_head, RawRequest};
use crate::response::{framing, read_head, BodyReader, ResponseHead};
use crate::{connect_tls, TransportError};

/// Maps a host name to an address; production resolves DNS with a bound, tests map both control hosts to a
/// fixed loopback fake.
pub type Resolver = Arc<dyn Fn(&str) -> ResolveFuture + Send + Sync>;
pub type ResolveFuture = Pin<Box<dyn Future<Output = Option<SocketAddr>> + Send>>;

/// Bounds a control-plane response body: these are small JSON payloads, so anything past this is a malformed
/// or malicious upstream rather than a legitimate response.
pub const MAX_BODY: usize = 4 * 1024 * 1024;

pub struct ControlClient {
    // Kept for the constructor's stable shape across its call sites; `send` forwards the caller-built `RawRequest`
    // verbatim and needs no profile fields of its own.
    _profile: &'static Profile,
    connector: SslConnector,
    resolver: Resolver,
}

impl ControlClient {
    pub fn new(profile: &'static Profile, connector: SslConnector, resolver: Resolver) -> Self {
        Self {
            _profile: profile,
            connector,
            resolver,
        }
    }

    pub async fn send(
        &self,
        req: RawRequest,
        host: &str,
        timeout: Duration,
    ) -> Result<(ResponseHead, Bytes), TransportError> {
        let Some(addr) = (self.resolver)(host).await else {
            return Err(TransportError::Unreachable(format!(
                "no address for {host}"
            )));
        };
        // Read by the timeout arm below, which cannot see how far `send_once` got on its own.
        let sent = Arc::new(AtomicBool::new(false));
        match tokio::time::timeout(timeout, self.send_once(req, host, addr, &sent)).await {
            Ok(result) => result,
            Err(_) if !sent.load(Ordering::SeqCst) => Err(TransportError::Unreachable(format!(
                "timed out before the request to {host} was sent"
            ))),
            Err(_) => Err(TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "control request timed out",
            ))),
        }
    }

    async fn send_once(
        &self,
        req: RawRequest,
        host: &str,
        addr: SocketAddr,
        sent: &AtomicBool,
    ) -> Result<(ResponseHead, Bytes), TransportError> {
        // A fresh connection per call, dropped at the end of this function: the control plane makes too few
        // requests to justify pooling, and a dead pooled connection would need the same retry machinery as the
        // inference plane for no real benefit.
        let mut conn =
            connect_tls(&self.connector, host, addr)
                .await
                .map_err(|error| match error {
                    // Not a byte of the request has been written yet, so a refused connection or a rejected
                    // handshake cannot have reached the upstream. A `Boring` error is a local configuration
                    // fault instead, and keeps its own variant.
                    TransportError::Handshake(detail) => TransportError::Unreachable(detail),
                    TransportError::Io(error) => TransportError::Unreachable(error.to_string()),
                    other => other,
                })?;
        write_tracked(&mut conn, &req, host, sent).await?;
        let mut buf = BytesMut::new();
        let head = read_head(&mut conn, &mut buf).await?;
        let f = framing(&head);
        let mut body_reader = BodyReader::new(conn, buf, f);
        let mut body = BytesMut::new();
        while let Some(chunk) = body_reader.next_chunk().await? {
            if body.len() + chunk.len() > MAX_BODY {
                return Err(TransportError::TooLarge(MAX_BODY));
            }
            body.extend_from_slice(&chunk);
        }
        let mut d = decoder(encoding(&head)?);
        let mut out = d.feed_bounded(&body, MAX_BODY).map_err(at_body_bound)?;
        out.extend(
            d.finish_bounded(MAX_BODY - out.len())
                .map_err(at_body_bound)?,
        );
        Ok((head, Bytes::from(out)))
    }
}

/// The sink reports the room left in the call it refused; the caller's own bound is the number worth telling.
fn at_body_bound(error: TransportError) -> TransportError {
    match error {
        TransportError::TooLarge(_) => TransportError::TooLarge(MAX_BODY),
        other => other,
    }
}

/// Written here rather than through the shared `write_request`, so `sent` can flip the instant the body
/// clears `write_all`: a cancellation racing the call as a whole cannot tell that instant from one a moment
/// earlier, and the same refresh token would otherwise be spent twice. This repeats `write_request`'s own
/// head, body, and flush sequence deliberately, `serialize_head` the one part still shared; the two have to
/// be kept in step by hand if either one changes.
async fn write_tracked<W: tokio::io::AsyncWrite + Unpin>(
    w: &mut W,
    req: &RawRequest,
    host: &str,
    sent: &AtomicBool,
) -> Result<(), TransportError> {
    if let Err(error) = w.write_all(&serialize_head(req)).await {
        return Err(TransportError::Unreachable(format!(
            "request to {host} was not sent: {error}"
        )));
    }
    if let Err(error) = w.write_all(&req.body).await {
        return Err(TransportError::Unreachable(format!(
            "request to {host} was not sent: {error}"
        )));
    }
    // Handed to the socket in full: the peer may read and act on it whether or not the flush returns, so a
    // timeout from here on is an uncertain outcome rather than an unsent request.
    sent.store(true, Ordering::SeqCst);
    w.flush().await.map_err(TransportError::Io)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    /// Accepts every write instantly but never completes a flush, so the flag's timing is provable without
    /// racing a real socket's timing.
    struct StallOnFlush;

    impl tokio::io::AsyncWrite for StallOnFlush {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Pending
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// Never completes a write at all, so a timeout during the head or the body write itself can be told
    /// apart from one during the flush.
    struct StallOnWrite;

    impl tokio::io::AsyncWrite for StallOnWrite {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Pending
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn token_request() -> RawRequest {
        RawRequest {
            method: "POST",
            target: "/v1/oauth/token".into(),
            headers: vec![],
            body: Bytes::from_static(b"{}"),
        }
    }

    #[tokio::test]
    async fn the_body_is_marked_sent_before_a_flush_that_never_returns() {
        let sent = AtomicBool::new(false);
        let mut w = StallOnFlush;
        let outcome = tokio::time::timeout(
            Duration::from_millis(50),
            write_tracked(&mut w, &token_request(), "host", &sent),
        )
        .await;
        assert!(
            outcome.is_err(),
            "the flush never resolves, so the call has to time out"
        );
        assert!(
            sent.load(Ordering::SeqCst),
            "the body was handed over before the stalled flush, not after it"
        );
    }

    #[tokio::test]
    async fn the_body_is_not_marked_sent_while_the_write_itself_never_returns() {
        let sent = AtomicBool::new(false);
        let mut w = StallOnWrite;
        let outcome = tokio::time::timeout(
            Duration::from_millis(50),
            write_tracked(&mut w, &token_request(), "host", &sent),
        )
        .await;
        assert!(
            outcome.is_err(),
            "the write never resolves, so the call has to time out"
        );
        assert!(
            !sent.load(Ordering::SeqCst),
            "the flag stays false until the body has fully cleared write_all"
        );
    }
}
