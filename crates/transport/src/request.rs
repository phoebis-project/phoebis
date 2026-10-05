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
