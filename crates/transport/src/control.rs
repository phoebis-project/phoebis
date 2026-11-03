//! Control-plane client: one TCP connection per request, used once and dropped.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use boring::ssl::SslConnector;
use bytes::{Bytes, BytesMut};
use phoebis_profile::Profile;

use crate::decode::{decoder, encoding};
use crate::request::{write_request, ProgressCell, RawRequest, WriteProgress};
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
        let progress = ProgressCell::default();
        match tokio::time::timeout(timeout, self.send_once(req, host, addr, &progress)).await {
            Ok(result) => result,
            Err(_) if progress.get() != WriteProgress::BodyWritten => {
                Err(TransportError::Unreachable(format!(
                    "timed out before the request to {host} was sent"
                )))
            }
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
        progress: &ProgressCell,
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
        write_request(&mut conn, &req, Some(progress))
            .await
            .map_err(|failure| match failure.progress {
                WriteProgress::NothingWritten | WriteProgress::HeadWritten => {
                    TransportError::Unreachable(format!(
                        "request to {host} was not sent: {}",
                        failure.error
                    ))
                }
                WriteProgress::BodyWritten => TransportError::Io(failure.error),
            })?;
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
        let mut out = d
            .feed_bounded(&body, MAX_BODY)
            .map_err(|e| e.at_bound(MAX_BODY))?;
        out.extend(
            d.finish_bounded(MAX_BODY - out.len())
                .map_err(|e| e.at_bound(MAX_BODY))?,
        );
        Ok((head, Bytes::from(out)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    /// Accepts every write instantly but never completes a flush, so the cell's timing is provable without
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
        let progress = ProgressCell::default();
        let mut w = StallOnFlush;
        let outcome = tokio::time::timeout(
            Duration::from_millis(50),
            write_request(&mut w, &token_request(), Some(&progress)),
        )
        .await;
        assert!(
            outcome.is_err(),
            "the flush never resolves, so the call has to time out"
        );
        assert_eq!(
            progress.get(),
            WriteProgress::BodyWritten,
            "the body was handed over before the stalled flush, not after it"
        );
    }

    #[tokio::test]
    async fn the_body_is_not_marked_sent_while_the_write_itself_never_returns() {
        let progress = ProgressCell::default();
        let mut w = StallOnWrite;
        let outcome = tokio::time::timeout(
            Duration::from_millis(50),
            write_request(&mut w, &token_request(), Some(&progress)),
        )
        .await;
        assert!(
            outcome.is_err(),
            "the write never resolves, so the call has to time out"
        );
        assert_ne!(
            progress.get(),
            WriteProgress::BodyWritten,
            "the cell stays short of the body until the body has fully entered the writer"
        );
    }
}
