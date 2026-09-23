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

use crate::decode::{decoder, encoding};
use crate::request::{write_request, RawRequest, WriteProgress};
use crate::response::{framing, read_head, BodyReader, ResponseHead};
use crate::{connect_tls, TransportError};

/// Maps a host name to an address; production resolves DNS with a bound, tests map both control hosts to a
/// fixed loopback fake.
pub type Resolver = Arc<dyn Fn(&str) -> ResolveFuture + Send + Sync>;
pub type ResolveFuture = Pin<Box<dyn Future<Output = Option<SocketAddr>> + Send>>;

/// Bounds a control-plane response body: these are small JSON payloads, so anything past this is a malformed
/// or malicious upstream rather than a legitimate response.
const MAX_BODY: usize = 4 * 1024 * 1024;

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
        write_request(&mut conn, &req).await.map_err(|f| {
            match f.progress {
                WriteProgress::NothingWritten | WriteProgress::HeadWritten => {
                    TransportError::Unreachable(format!(
                        "request to {host} was not sent: {}",
                        f.error
                    ))
                }
                // The body is already in the socket and only the flush failed, so the upstream may well
                // have read the whole request and acted on it.
                WriteProgress::BodyWritten => TransportError::Io(f.error),
            }
        })?;
        sent.store(true, Ordering::SeqCst);
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
        let mut out = d.feed(&body)?;
        out.extend(d.finish()?);
        Ok((head, Bytes::from(out)))
    }
}
