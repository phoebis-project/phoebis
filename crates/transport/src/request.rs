//! HTTP/1.1 request serialization that emits headers exactly in the order and casing it is given.

use std::io;

use bytes::Bytes;
use tokio::io::{AsyncWrite, AsyncWriteExt};

use crate::TransportError;

pub struct RawRequest {
    pub method: &'static str,
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

/// How far `write_request` got before it failed. `NothingWritten` covers a partial head as well: an incomplete
/// head is never dispatched upstream, so the request did not happen.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriteProgress {
    NothingWritten,
    HeadWritten,
    BodyWritten,
}

#[derive(Debug)]
pub struct WriteFailure {
    pub progress: WriteProgress,
    pub error: std::io::Error,
}

pub fn serialize_head(req: &RawRequest) -> Vec<u8> {
    let mut out = Vec::with_capacity(512);
    out.extend_from_slice(req.method.as_bytes());
    out.push(b' ');
    out.extend_from_slice(req.target.as_bytes());
    out.extend_from_slice(b" HTTP/1.1\r\n");
    for (name, value) in &req.headers {
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");
    out
}

pub async fn write_request<W: AsyncWrite + Unpin>(
    w: &mut W,
    req: &RawRequest,
) -> Result<(), WriteFailure> {
    // One buffer for the head and the body, offered to the writer whole, at the cost of one copy of a body the
    // request limit bounds: no write is issued for the head alone. The loop counts what the writer took, which
    // `write_all` does not report, so a failure still tells a head the upstream cannot have acted on (fewer
    // bytes than the head) from one it may have.
    let mut wire = serialize_head(req);
    let head_len = wire.len();
    wire.extend_from_slice(&req.body);
    let mut written = 0;
    while written < wire.len() {
        let progress = if written < head_len {
            WriteProgress::NothingWritten
        } else {
            WriteProgress::HeadWritten
        };
        match w.write(&wire[written..]).await {
            Ok(0) => {
                return Err(WriteFailure {
                    progress,
                    error: io::ErrorKind::WriteZero.into(),
                })
            }
            Ok(n) => written += n,
            Err(error) => return Err(WriteFailure { progress, error }),
        }
    }
    // Every byte entered the writer before the flush is attempted, and the upstream may have read the whole
    // request, so a flush failure is never sent again.
    if let Err(error) = w.flush().await {
        return Err(WriteFailure {
            progress: WriteProgress::BodyWritten,
            error,
        });
    }
    Ok(())
}

pub fn parse_head(raw: &[u8]) -> Result<(String, Vec<(String, String)>), TransportError> {
    let end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| TransportError::Response("no head terminator".into()))?;
    let head =
        std::str::from_utf8(&raw[..end]).map_err(|e| TransportError::Response(e.to_string()))?;
    let mut lines = head.split("\r\n");
    let line = lines.next().unwrap_or_default().to_string();
    let headers = lines
        .map(|l| {
            let (n, v) = l.split_once(':').unwrap_or((l, ""));
            (n.to_string(), v.trim_start().to_string())
        })
        .collect();
    Ok((line, headers))
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use bytes::Bytes;
    use tokio::io::AsyncWrite;

    use super::{serialize_head, write_request, RawRequest, WriteProgress};

    /// Takes every byte offered up to `limit`, then refuses each write; refuses its flush when `flush_fails`.
    struct Writer {
        taken: Vec<u8>,
        limit: usize,
        flush_fails: bool,
    }

    impl AsyncWrite for Writer {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let room = self.limit - self.taken.len();
            if room == 0 {
                return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
            }
            let n = room.min(buf.len());
            self.taken.extend_from_slice(&buf[..n]);
            Poll::Ready(Ok(n))
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(if self.flush_fails {
                Err(io::ErrorKind::BrokenPipe.into())
            } else {
                Ok(())
            })
        }

        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn request() -> RawRequest {
        let body = Bytes::from_static(br#"{"model":"m","messages":[]}"#);
        RawRequest {
            method: "POST",
            target: "/v1/messages".into(),
            headers: vec![("Content-Length".into(), body.len().to_string())],
            body,
        }
    }

    #[tokio::test]
    async fn a_flush_that_fails_after_every_byte_was_taken_reports_the_body_written() {
        let req = request();
        let mut w = Writer {
            taken: Vec::new(),
            limit: usize::MAX,
            flush_fails: true,
        };
        let failure = write_request(&mut w, &req).await.unwrap_err();
        assert_eq!(failure.progress, WriteProgress::BodyWritten);
        let mut wire = serialize_head(&req);
        wire.extend_from_slice(&req.body);
        assert_eq!(w.taken, wire, "the head and then the body, every byte");
    }

    #[tokio::test]
    async fn a_write_cut_inside_the_head_reports_nothing_written_and_one_cut_inside_the_body_the_head(
    ) {
        let req = request();
        let head = serialize_head(&req).len();
        for (limit, progress) in [
            (head - 1, WriteProgress::NothingWritten),
            (head + 3, WriteProgress::HeadWritten),
        ] {
            let mut w = Writer {
                taken: Vec::new(),
                limit,
                flush_fails: false,
            };
            let failure = write_request(&mut w, &req).await.unwrap_err();
            assert_eq!(failure.progress, progress, "cut after {limit} bytes");
        }
    }
}
