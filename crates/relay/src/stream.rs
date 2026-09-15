//! Upstream connection lifecycle and the piece-by-piece relay of a response body.

use std::collections::HashSet;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
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

// A poisoned tracker mutex means a prior panic happened while the parked set was being mutated, so its
// contents can no longer be trusted to describe which sockets are idle.
const LOCK_POISONED: &str = "connection tracker mutex poisoned by a prior panic";

/// Bound on the parked set. Forgetting an entry only costs a retry the relay could have taken, never a request
/// sent twice, so the set is cleared wholesale rather than reconciled with the pool's own eviction.
const MAX_PARKED: usize = 1024;

/// The pool, plus the relay's own record of which sockets it parked there.
pub(crate) struct Connections {
    parked: Mutex<HashSet<SocketAddr>>,
    pool: Arc<Pool>,
}

impl Connections {
    pub(crate) fn new(pool: Arc<Pool>) -> Connections {
        Connections {
            parked: Mutex::new(HashSet::new()),
            pool,
        }
    }

    /// Hands out a connection along with whether it came back from the pool. A connection served from the pool
    /// has been idle and the upstream may have closed it without the relay noticing, which is the one case
    /// where a request that got no response byte can be sent again without risking a duplicate.
    pub(crate) async fn checkout(
        &self,
        key: &PoolKey,
        host: &str,
        addr: SocketAddr,
    ) -> Result<(Conn, bool), TransportError> {
        let conn = self.pool.checkout(key.clone(), host, addr).await?;
        let reused = self.take(&conn);
        Ok((conn, reused))
    }

    /// Drains the idle connections for this key until a genuinely new one comes back. The retry that calls
    /// this has just been failed by an idle connection, so handing it another one would repeat the failure.
    pub(crate) async fn checkout_fresh(
        &self,
        key: &PoolKey,
        host: &str,
        addr: SocketAddr,
    ) -> Result<Conn, TransportError> {
        loop {
            let (conn, reused) = self.checkout(key, host, addr).await?;
            if !reused {
                return Ok(conn);
            }
            drop(conn);
        }
    }

    pub(crate) fn park(&self, key: PoolKey, conn: Conn) {
        // A socket that cannot report its address only costs the retry the record would have allowed, so the
        // connection still goes back to the pool.
        if let Ok(addr) = conn.get_ref().local_addr() {
            let mut parked = self.parked.lock().expect(LOCK_POISONED);
            if parked.len() >= MAX_PARKED {
                parked.clear();
            }
            parked.insert(addr);
        }
        self.pool.checkin(key, conn);
    }

    fn take(&self, conn: &Conn) -> bool {
        conn.get_ref()
            .local_addr()
            .is_ok_and(|addr| self.parked.lock().expect(LOCK_POISONED).remove(&addr))
    }
}

/// Everything the body task needs once the head has been forwarded.
pub(crate) struct Relayed {
    pub conn: Conn,
    pub connections: Arc<Connections>,
    pub decoder: Option<Box<dyn Decoder>>,
    pub framing: Framing,
    pub idle_timeout: Duration,
    pub keep_alive: bool,
    pub key: PoolKey,
    pub leftover: BytesMut,
}

/// Relays the upstream body as it arrives: one task reads, the response body drains the channel, and the
/// connection goes back to the pool only after a clean end.
pub(crate) fn relay_body(relayed: Relayed) -> Body {
    let (tx, rx) = mpsc::channel::<Result<Bytes, TransportError>>(CHANNEL_DEPTH);
    tokio::spawn(async move {
        let Relayed {
            mut conn,
            connections,
            mut decoder,
            framing,
            idle_timeout,
            keep_alive,
            key,
            leftover,
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
        // Parking before the sender drops is what makes the connection available again by the time the caller
        // sees the end of the body, which is the only ordering a sequential caller can observe.
        if keep_alive {
            connections.park(key, conn);
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
