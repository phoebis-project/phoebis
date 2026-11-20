//! The control plane's client, with one connection per request that is used once and dropped, and the send of an
//! upgrade request, which serves both planes and hands the connection to its caller at a `101`.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use boring::ssl::SslConnector;
use bytes::{Bytes, BytesMut};
use phoebis_profile::Profile;

use crate::decode::{decoder, encoding};
use crate::pool::Conn;
use crate::request::{write_request, ProgressCell, RawRequest, WriteProgress};
use crate::response::{framing, read_head, read_upgrade_head, BodyReader, ResponseHead};
use crate::{connect_tls, TransportError};

/// Maps a host name to an address; production resolves DNS with a bound, tests map both control hosts to a
/// fixed loopback fake.
pub type Resolver = Arc<dyn Fn(&str) -> ResolveFuture + Send + Sync>;
pub type ResolveFuture = Pin<Box<dyn Future<Output = Option<SocketAddr>> + Send>>;

/// Bounds an answer to the buffered `send`, which carries the router's own calls. Their answers are small JSON
/// payloads, so anything past this is a malformed or malicious upstream rather than a legitimate response.
pub const MAX_BODY: usize = 4 * 1024 * 1024;

/// Why a streaming send handed back no answer, in the four cases a caller's log line tells apart.
#[derive(Debug, thiserror::Error)]
pub enum StreamFailure {
    /// The connection failed or ended before the answer's whole head arrived, after the request was written.
    #[error("the connection was cut before the answer's head: {0}")]
    Cut(String),
    /// The answer's head did not arrive inside its bound.
    #[error("no answer head in time")]
    Stalled,
    /// Bytes arrived that yield no answer head: a head that does not parse or passes the reader's size bound, or more
    /// interim heads than the reader allows.
    #[error("unreadable answer head: {0}")]
    Undecodable(String),
    /// No address, no connection or no handshake in time, or a write that failed before the request's last byte,
    /// so nothing upstream can have acted on it.
    #[error("upstream unreachable: {0}")]
    Unreachable(String),
}

/// A control answer as its head and a reader over its body, for a caller that passes the body on as it arrives.
pub struct ControlAnswer {
    pub head: ResponseHead,
    pub body: BodyReader<Conn>,
}

/// What the send of an upgrade request hands back.
pub enum UpgradeAnswer {
    /// Another final head than a `101`, with a reader over its body, as `send_streaming` hands an answer back.
    Answered(ControlAnswer),
    /// A `101`: its head, the connection, and the bytes that arrived behind the head.
    Switched {
        conn: Conn,
        head: ResponseHead,
        leftover: BytesMut,
    },
}

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
        let addr = address(&self.resolver, host)
            .await
            .map_err(TransportError::Unreachable)?;
        // Read by the timeout arm below, which cannot see how far `send_once` got on its own.
        let progress = ProgressCell::default();
        match tokio::time::timeout(timeout, self.send_once(req, host, addr, &progress)).await {
            Ok(result) => result,
            Err(_) if !progress.body_written() => Err(TransportError::Unreachable(format!(
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
                WriteProgress::Unsent => TransportError::Unreachable(unsent(host, &failure.error)),
                WriteProgress::Written => TransportError::Io(failure.error),
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
        let mut out = Vec::new();
        d.feed_into(&mut out, &body, MAX_BODY)?;
        d.finish_into(&mut out, MAX_BODY)?;
        Ok((head, Bytes::from(out)))
    }

    /// One fresh connection per call, the head as the caller built it, and the answer handed back as its head and a
    /// reader over its body. Nothing bounds the body and nothing decodes it, so the caller passes each piece on in
    /// the coding the upstream gave it. `connect_timeout` bounds the lookup, the connection and the handshake, and
    /// `head_timeout` the write and the wait for the answer's head.
    pub async fn send_streaming(
        &self,
        req: RawRequest,
        host: &str,
        connect_timeout: Duration,
        head_timeout: Duration,
    ) -> Result<ControlAnswer, StreamFailure> {
        let (conn, head, buf) = exchange(
            &self.connector,
            &self.resolver,
            req,
            host,
            connect_timeout,
            head_timeout,
            false,
        )
        .await?;
        let f = framing(&head);
        Ok(ControlAnswer {
            head,
            body: BodyReader::new(conn, buf, f),
        })
    }

    /// `send_upgrade` over this client's connector and resolver.
    pub async fn send_upgrade(
        &self,
        req: RawRequest,
        host: &str,
        connect_timeout: Duration,
        head_timeout: Duration,
    ) -> Result<UpgradeAnswer, StreamFailure> {
        send_upgrade(
            &self.connector,
            &self.resolver,
            req,
            host,
            connect_timeout,
            head_timeout,
        )
        .await
    }
}

/// One fresh connection through `connector`, the head as the caller built it, and the answer with a `101` as a final
/// head. `connect_timeout` and `head_timeout` bound what they bound in `ControlClient::send_streaming`.
pub(crate) async fn send_upgrade(
    connector: &SslConnector,
    resolver: &Resolver,
    req: RawRequest,
    host: &str,
    connect_timeout: Duration,
    head_timeout: Duration,
) -> Result<UpgradeAnswer, StreamFailure> {
    let (conn, head, leftover) = exchange(
        connector,
        resolver,
        req,
        host,
        connect_timeout,
        head_timeout,
        true,
    )
    .await?;
    if head.status == 101 {
        return Ok(UpgradeAnswer::Switched {
            conn,
            head,
            leftover,
        });
    }
    let f = framing(&head);
    Ok(UpgradeAnswer::Answered(ControlAnswer {
        head,
        body: BodyReader::new(conn, leftover, f),
    }))
}

/// The connect, the write and the head read that both streaming sends share, with `upgrade` for whether the request
/// asked for an upgrade, so that a `101` is the final head.
async fn exchange(
    connector: &SslConnector,
    resolver: &Resolver,
    req: RawRequest,
    host: &str,
    connect_timeout: Duration,
    head_timeout: Duration,
    upgrade: bool,
) -> Result<(Conn, ResponseHead, BytesMut), StreamFailure> {
    let connecting = async {
        let addr = address(resolver, host)
            .await
            .map_err(StreamFailure::Unreachable)?;
        connect_tls(connector, host, addr)
            .await
            .map_err(|error| StreamFailure::Unreachable(error.to_string()))
    };
    let mut conn = match tokio::time::timeout(connect_timeout, connecting).await {
        Ok(connected) => connected?,
        Err(_) => {
            return Err(StreamFailure::Unreachable(format!(
                "no connection to {host} in time"
            )))
        }
    };
    let mut buf = BytesMut::new();
    let round_trip = async {
        write_request(&mut conn, &req, None)
            .await
            .map_err(|failure| match failure.progress {
                WriteProgress::Unsent => StreamFailure::Unreachable(unsent(host, &failure.error)),
                WriteProgress::Written => StreamFailure::Cut(failure.error.to_string()),
            })?;
        let read = if upgrade {
            read_upgrade_head(&mut conn, &mut buf).await
        } else {
            read_head(&mut conn, &mut buf).await
        };
        read.map_err(|error| match error {
            TransportError::ClosedBeforeHead | TransportError::Io(_) => {
                StreamFailure::Cut(error.to_string())
            }
            _ => StreamFailure::Undecodable(error.to_string()),
        })
    };
    let head = match tokio::time::timeout(head_timeout, round_trip).await {
        Ok(read) => read?,
        Err(_) => return Err(StreamFailure::Stalled),
    };
    Ok((conn, head, buf))
}

async fn address(resolver: &Resolver, host: &str) -> Result<SocketAddr, String> {
    resolver(host)
        .await
        .ok_or_else(|| format!("no address for {host}"))
}

fn unsent(host: &str, error: &std::io::Error) -> String {
    format!("request to {host} was not sent: {error}")
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
        assert!(
            progress.body_written(),
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
        assert!(
            !progress.body_written(),
            "the cell stays short of the body until the body has fully entered the writer"
        );
    }
}
