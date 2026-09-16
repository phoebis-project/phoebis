//! One client: a snapshot, then every delta as it happens, over a bounded queue that disconnects a client
//! who stops reading rather than letting its backlog grow with the traffic.

use axum::extract::ws::{Message, WebSocket};
use futures::{SinkExt, StreamExt};
use tokio::sync::{broadcast, mpsc};

use crate::protocol::{Delta, LiveEvent, LiveSnapshot};

pub const CLIENT_QUEUE: usize = 1_024;

/// Serves a socket: the snapshot first, then the broadcast, filtered of previews when the client may not see
/// them. Returns when the client closes, the socket fails, or the client falls behind.
pub(crate) async fn serve(
    socket: WebSocket,
    snapshot: LiveSnapshot,
    mut deltas: broadcast::Receiver<Delta>,
    preview: bool,
) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::channel::<String>(CLIENT_QUEUE);
    let first = serde_json::to_string(&LiveEvent::Snapshot {
        snapshot: snapshot.clone(),
    })
    .expect("the snapshot serializes");
    if tx.try_send(first).is_err() {
        return;
    }
    let after = snapshot.cursor;
    // The writer owns the sink; the reader below only watches for the client going away.
    let writer = tokio::spawn(async move {
        while let Some(text) = rx.recv().await {
            if sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });
    let mut reader = tokio::spawn(async move {
        while let Some(Ok(message)) = stream.next().await {
            if matches!(message, Message::Close(_)) {
                break;
            }
        }
    });
    loop {
        tokio::select! {
            _ = &mut reader => break,
            received = deltas.recv() => match received {
                Ok(delta) => {
                    if delta.cursor() <= after || (delta.is_preview() && !preview) {
                        continue;
                    }
                    let text = serde_json::to_string(&LiveEvent::Delta { delta }).expect("a delta serializes");
                    // A full queue means the client is more than CLIENT_QUEUE messages behind: it is cut off
                    // and reconnects to a fresh snapshot, which is cheaper than the backlog it would need.
                    if tx.try_send(text).is_err() {
                        tracing::debug!("a live client fell behind and was disconnected");
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => break,
                Err(broadcast::error::RecvError::Closed) => break,
            },
        }
    }
    drop(tx);
    writer.abort();
    reader.abort();
}
