//! Upstream connection lifecycle and the piece-by-piece relay of a response body.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use bytes::{Bytes, BytesMut};
use phoebis_transport::decode::Decoder;
use phoebis_transport::pool::{Conn, Pool, PoolKey};
use phoebis_transport::response::{BodyReader, Framing};
use phoebis_transport::TransportError;
use tokio::sync::mpsc;

/// Chunks held between the upstream reader and the caller's socket. Deep enough that a caller pausing for a
/// moment does not stall the read loop, shallow enough that a caller that stops reading still pushes back on
/// the upstream instead of letting the response pile up in memory.
const CHANNEL_DEPTH: usize = 64;

/// Cap on a response the relay has to read before it can decide what to do with it. Only an authentication
/// failure takes that path and those bodies are a few hundred bytes.
pub(crate) const MAX_BUFFERED_BODY: usize = 64 * 1024;

/// Everything the body task needs once the head has been forwarded.
pub(crate) struct Relayed {
    pub conn: Conn,
    pub decoder: Option<Box<dyn Decoder>>,
    pub framing: Framing,
    pub idle_timeout: Duration,
    pub keep_alive: bool,
    pub key: PoolKey,
    pub leftover: BytesMut,
    pub pool: Arc<Pool>,
}

/// Relays the upstream body as it arrives: one task reads, the response body drains the channel, and the
/// connection goes back to the pool only after a clean end.
pub(crate) fn relay_body(relayed: Relayed) -> Body {
    let (tx, rx) = mpsc::channel::<Result<Bytes, TransportError>>(CHANNEL_DEPTH);
    tokio::spawn(async move {
        let Relayed {
            mut conn,
            mut decoder,
            framing,
            idle_timeout,
            keep_alive,
            key,
            leftover,
            pool,
        } = relayed;
        let complete = {
            let mut reader = BodyReader::new(&mut conn, leftover, framing);
            loop {
                // A caller that hangs up mid-stream has to release the upstream connection right away; waiting
                // for the next chunk would hold it until the idle timeout on a stream that has gone quiet.
                let read = tokio::select! {
                    _ = tx.closed() => break false,
                    read = tokio::time::timeout(idle_timeout, reader.next_chunk()) => read,
                };
                let next = match read {
                    Err(_) => {
                        let _ = tx.send(Err(stalled())).await;
                        break false;
                    }
                    Ok(Err(error)) => {
                        let _ = tx.send(Err(error)).await;
                        break false;
                    }
                    Ok(Ok(next)) => next,
                };
                let Some(chunk) = next else {
                    break true;
                };
                let piece = match decoder.as_mut() {
                    None => chunk,
                    Some(decoder) => match decoder.feed(&chunk) {
                        Ok(decoded) => Bytes::from(decoded),
                        Err(error) => {
                            let _ = tx.send(Err(error)).await;
                            break false;
                        }
                    },
                };
                if piece.is_empty() {
                    continue;
                }
                // The caller dropped the response, so there is nobody left to relay to and the upstream
                // connection goes with it.
                if tx.send(Ok(piece)).await.is_err() {
                    return;
                }
            }
        };
        if !complete {
            return;
        }
        if let Some(mut decoder) = decoder {
            match decoder.finish() {
                Ok(tail) if !tail.is_empty() => {
                    if tx.send(Ok(Bytes::from(tail))).await.is_err() {
                        return;
                    }
                }
                Err(error) => {
                    let _ = tx.send(Err(error)).await;
                    return;
                }
                Ok(_) => {}
            }
        }
        // Checking in before the sender drops is what makes the connection available again by the time the
        // caller sees the end of the body, which is the only ordering a sequential caller can observe.
        if keep_alive {
            pool.checkin(key, conn);
        }
    });
    Body::from_stream(futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    }))
}

/// Reads a whole response body into memory, for the one case that has to look at it before forwarding it.
pub(crate) async fn read_bounded(
    conn: &mut Conn,
    leftover: BytesMut,
    framing: Framing,
    idle_timeout: Duration,
) -> Result<Bytes, TransportError> {
    let mut reader = BodyReader::new(conn, leftover, framing);
    let mut out = BytesMut::new();
    loop {
        let next = tokio::time::timeout(idle_timeout, reader.next_chunk())
            .await
            .map_err(|_| stalled())??;
        let Some(chunk) = next else {
            return Ok(out.freeze());
        };
        if out.len() + chunk.len() > MAX_BUFFERED_BODY {
            return Err(TransportError::TooLarge(MAX_BUFFERED_BODY));
        }
        out.extend_from_slice(&chunk);
    }
}

fn stalled() -> TransportError {
    TransportError::Io(io::Error::new(
        io::ErrorKind::TimedOut,
        "upstream stopped sending",
    ))
}
