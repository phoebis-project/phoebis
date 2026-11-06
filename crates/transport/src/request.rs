//! HTTP/1.1 request serialization that emits headers exactly in the order and casing it is given.

use std::sync::atomic::{AtomicBool, Ordering};

use bytes::Bytes;
use tokio::io::{AsyncWrite, AsyncWriteExt};

use crate::TransportError;

pub struct RawRequest {
    pub method: &'static str,
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

/// Whether the upstream can have acted on the request: not before it took the whole request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriteProgress {
    Unsent,
    Written,
}

/// Whether a `write_request` call has handed its whole body to the writer, published for a caller whose timeout drops
/// the call before it answers. It is set once the last byte entered the writer, before the flush: from then on the
/// upstream may act on the request whether or not the flush returns, so a control refresh that times out there must
/// not be sent again with the same refresh token.
#[derive(Debug, Default)]
pub struct ProgressCell(AtomicBool);

impl ProgressCell {
    pub fn body_written(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    fn mark_body_written(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
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
    progress: Option<&ProgressCell>,
) -> Result<(), WriteFailure> {
    // One buffer for the head and the body, offered to the writer whole, at the cost of one copy of a body the
    // request limit bounds: no write is issued for the head alone.
    let mut wire = serialize_head(req);
    wire.extend_from_slice(&req.body);
    if let Err(error) = w.write_all(&wire).await {
        return Err(WriteFailure {
            progress: WriteProgress::Unsent,
            error,
        });
    }
    // Every byte entered the writer before the flush is attempted, and the upstream may have read the whole
    // request, so a flush failure is never sent again.
    if let Some(cell) = progress {
        cell.mark_body_written();
    }
    if let Err(error) = w.flush().await {
        return Err(WriteFailure {
            progress: WriteProgress::Written,
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
