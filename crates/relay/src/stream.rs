//! Upstream connection lifecycle and the piece-by-piece relay of a response body.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use bytes::{Bytes, BytesMut};
use phoebis_transport::decode::Decoder;
use phoebis_transport::pool::{Conn, Pool, PoolKey};
use phoebis_transport::response::BodyReader;
use phoebis_transport::TransportError;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::hold::Inspect;

/// Chunks held between the upstream reader and the caller's socket. Deep enough that a caller pausing for a
/// moment does not stall the read loop, shallow enough that a caller that stops reading still pushes back on
/// the upstream instead of letting the response pile up in memory.
const CHANNEL_DEPTH: usize = 64;

/// Cap on a response the relay reads before it can decide what to do with it. Only an authentication failure
/// takes that path and those bodies are a few hundred bytes, so a larger one is simply forwarded unread. The
/// same cap bounds the decoded copy a compressed rejection is classified from.
pub(crate) const MAX_BUFFERED_BODY: usize = 64 * 1024;

/// What a committed stream is still watched for. The inspection copy is the pre-commit hold's, so the events it
/// already consumed are not seen a second time and the parser's bound is all the memory this costs.
pub(crate) struct PostCommit {
    pub inspect: Inspect,
    pub on_error: Box<dyn FnOnce(Bytes) + Send>,
}

/// Everything the body task needs once the head has been forwarded.
pub(crate) struct Relayed {
    /// Cancelled when the account this body belongs to is disconnected or deleted.
    pub cancel: CancellationToken,
    pub decoder: Option<Box<dyn Decoder>>,
    pub idle_timeout: Duration,
    pub keep_alive: bool,
    pub key: PoolKey,
    pub observer: Option<PostCommit>,
    pub pool: Arc<Pool>,
    /// Body bytes already read, which go out ahead of anything the reader still produces.
    pub prefix: Bytes,
    pub reader: BodyReader<Conn>,
}

/// Relays the upstream body as it arrives: one task reads, the response body drains the channel, and the
/// connection goes back to the pool only after a clean end.
pub(crate) fn relay_body(relayed: Relayed) -> Body {
    let (tx, rx) = mpsc::channel::<Result<Bytes, TransportError>>(CHANNEL_DEPTH);
    tokio::spawn(async move {
        let Relayed {
            cancel,
            mut decoder,
            idle_timeout,
            keep_alive,
            key,
            mut observer,
            pool,
            prefix,
            mut reader,
        } = relayed;
        // The hold already fed the held bytes to the inspection copy, so the prefix is scanned for what it left
        // behind rather than fed again.
        observe(&mut observer);
        if !prefix.is_empty() {
            match decode_piece(decoder.as_mut(), prefix) {
                Ok(piece) => {
                    if tx.send(Ok(piece)).await.is_err() {
                        return;
                    }
                }
                Err(error) => {
                    let _ = tx.send(Err(error)).await;
                    return;
                }
            }
        }
        let complete = {
            loop {
                // A caller that hangs up mid-stream has to release the upstream connection right away; waiting
                // for the next chunk would hold it until the idle timeout on a stream that has gone quiet.
                let read = tokio::select! {
                    _ = tx.closed() => break false,
                    // The credential this stream was opened with is gone, so the body ends in an error and
                    // axum closes the caller's connection rather than letting a dead account keep streaming.
                    _ = cancel.cancelled() => {
                        let _ = tx.send(Err(cancelled())).await;
                        break false;
                    }
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
                // The raw chunk, because the inspection copy runs the upstream's own content coding; it is read
                // before the chunk goes out so a late rate-limit error is acted on no later than the client
                // sees it.
                feed(&mut observer, &chunk);
                observe(&mut observer);
                let piece = match decode_piece(decoder.as_mut(), chunk) {
                    Ok(piece) => piece,
                    Err(error) => {
                        let _ = tx.send(Err(error)).await;
                        break false;
                    }
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
        observe_end(&mut observer);
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
            pool.checkin(key, reader.into_inner());
        }
    });
    Body::from_stream(futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    }))
}

/// Hands a forwarded chunk to the inspection copy. A copy that no longer decodes or no longer fits its bound
/// cannot be acted on, so the observer goes rather than guessing at what it missed.
fn feed(observer: &mut Option<PostCommit>, raw: &[u8]) {
    if let Some(post) = observer.as_mut() {
        if post.inspect.feed_raw(raw).is_err() {
            *observer = None;
        }
    }
}

/// Fires the observer on the first `error` event the inspection copy can parse.
fn observe(observer: &mut Option<PostCommit>) {
    let Some(post) = observer.as_mut() else {
        return;
    };
    let mut found = None;
    while let Some(event) = post.inspect.parser.next_event() {
        if event.name.as_deref() == Some(b"error".as_slice()) {
            found = Some(event.data);
            break;
        }
    }
    fire(observer, found);
}

/// A stream cut off after its last event never sent the empty line that would close it, so the error can still
/// be sitting in the parser when the body ends.
fn observe_end(observer: &mut Option<PostCommit>) {
    let Some(post) = observer.as_mut() else {
        return;
    };
    let found = post
        .inspect
        .parser
        .finish()
        .filter(|event| event.name.as_deref() == Some(b"error".as_slice()))
        .map(|event| event.data);
    fire(observer, found);
}

/// One firing per stream: the observer is taken with it, so a second `error` event changes nothing.
fn fire(observer: &mut Option<PostCommit>, data: Option<Vec<u8>>) {
    let Some(data) = data else {
        return;
    };
    if let Some(post) = observer.take() {
        (post.on_error)(Bytes::from(data));
    }
}

/// What came of reading a body the relay has to look at before it can decide what to do with it.
pub(crate) enum Drained {
    /// The whole body, read within the bound.
    Complete(Bytes),
    /// The bound was passed. These bytes still belong to the caller and go out ahead of the rest of the body,
    /// which the reader is still holding.
    TooLarge(Bytes),
}

/// Reads a response body into memory until it ends or outgrows the bound. The reader stays with the caller
/// either way: after a clean end it hands back the connection, and after an overrun it is the only thing that
/// can produce the rest of the body, whatever framing the upstream chose.
pub(crate) async fn drain_bounded(
    reader: &mut BodyReader<Conn>,
    idle_timeout: Duration,
) -> Result<Drained, TransportError> {
    let mut out = BytesMut::new();
    loop {
        let next = tokio::time::timeout(idle_timeout, reader.next_chunk())
            .await
            .map_err(|_| stalled())??;
        let Some(chunk) = next else {
            return Ok(Drained::Complete(out.freeze()));
        };
        out.extend_from_slice(&chunk);
        if out.len() > MAX_BUFFERED_BODY {
            return Ok(Drained::TooLarge(out.freeze()));
        }
    }
}

fn decode_piece(
    decoder: Option<&mut Box<dyn Decoder>>,
    raw: Bytes,
) -> Result<Bytes, TransportError> {
    match decoder {
        None => Ok(raw),
        Some(decoder) => decoder.feed(&raw).map(Bytes::from),
    }
}

fn cancelled() -> TransportError {
    TransportError::Io(io::Error::new(
        io::ErrorKind::ConnectionAborted,
        "the bound account was disconnected",
    ))
}

fn stalled() -> TransportError {
    TransportError::Io(io::Error::new(
        io::ErrorKind::TimedOut,
        "upstream stopped sending",
    ))
}
