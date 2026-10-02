//! One client: a snapshot, then every delta as it happens, over a bounded queue that disconnects a client
//! who stops reading rather than letting its backlog grow with the traffic.

use std::time::Duration;

use axum::extract::ws::{close_code, CloseFrame, Message, WebSocket};
use futures::{SinkExt, StreamExt};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::protocol::{Delta, LiveEvent, LiveSnapshot};
use crate::SessionCheck;

pub const CLIENT_QUEUE: usize = 1_024;

/// How often the writer checks the session between sends, so a socket left open with nothing to deliver still
/// notices a logout or an expiry within a bounded time.
pub const SESSION_CHECK: Duration = Duration::from_secs(5);

/// The close a session that ended before its socket ever subscribed to the feed is owed: nothing else leaves.
pub(crate) async fn close_ended(socket: WebSocket) {
    let (mut sink, _) = socket.split();
    let close = CloseFrame {
        code: close_code::POLICY,
        reason: "session ended".into(),
    };
    let _ = sink.send(Message::Close(Some(close))).await;
    let _ = sink.close().await;
}

/// Serves a socket: the snapshot first, then the broadcast. Returns when the client closes, the socket fails,
/// the client falls behind, the session ends, or the feed shuts down.
pub(crate) async fn serve(
    socket: WebSocket,
    snapshot: LiveSnapshot,
    mut deltas: broadcast::Receiver<Delta>,
    alive: SessionCheck,
    cancel: CancellationToken,
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
    // The writer owns the sink, so the session is checked here, at the send: a logout can land while a frame
    // waits in the queue, and a frame that leaves after it is one the ended session was never owed.
    let mut writer = tokio::spawn(async move {
        let mut check = tokio::time::interval(SESSION_CHECK);
        check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let session_ended = loop {
            let text = tokio::select! {
                text = rx.recv() => match text { Some(text) => text, None => break false },
                _ = check.tick() => if alive() { continue } else { break true },
            };
            if !alive() {
                break true;
            }
            if sink.send(Message::Text(text.into())).await.is_err() {
                break false;
            }
        };
        if session_ended {
            // The backlog was the session's: dropped, not delivered ahead of the close.
            rx.close();
            while rx.try_recv().is_ok() {}
            let close = CloseFrame {
                code: close_code::POLICY,
                reason: "session ended".into(),
            };
            let _ = sink.send(Message::Close(Some(close))).await;
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
            _ = &mut writer => break,
            _ = cancel.cancelled() => break,
            received = deltas.recv() => match received {
                Ok(delta) => {
                    if delta.cursor() <= after {
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
