//! Upstream connection lifecycle and the piece-by-piece relay of a response body.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use bytes::{Bytes, BytesMut};
use phoebis_bus::{Outcome, Phase, ResponseChunk, ResponseFacts, Usage};
use phoebis_oauth::Clock;
use phoebis_routing::{AffinityReport, PinRef, PinUsage, Report, RouterHandle};
use phoebis_transport::decode::Decoder;
use phoebis_transport::pool::{Conn, Pool, PoolKey};
use phoebis_transport::response::BodyReader;
use phoebis_transport::TransportError;
use futures::future::BoxFuture;
use tokio::io::AsyncRead;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::catalog::CatalogService;
use crate::hold::Inspect;
use crate::messages::{Guards, RelayOutcome, Trace};
use crate::observe::{answer_from_json, is_tool_use_delta, message_id_from_json};
use crate::sse::{SseEvent, SseParser};

/// Chunks held between the upstream reader and the caller's socket. Deep enough that a caller pausing for a
/// moment does not stall the read loop, shallow enough that a caller that stops reading still pushes back on
/// the upstream instead of letting the response pile up in memory.
const CHANNEL_DEPTH: usize = 64;

/// Cap on a response the relay reads before it can decide what to do with it. Only an authentication failure
/// takes that path and those bodies are a few hundred bytes, so a larger one is simply forwarded unread. The
/// same cap bounds the decoded copy a compressed rejection is classified from.
pub(crate) const MAX_BUFFERED_BODY: usize = 64 * 1024;

/// Per-chunk bound on what a decoder may hand back for one upstream chunk: an 8 KiB compressed chunk can
/// expand into hundreds of MiB in a single call without this, whatever the caller does with the result after.
/// It is a budget against the 8 KiB reads the transport fills at a time, not against a whole body: at
/// deflate's compression ceiling one wire chunk can still decode past it, and the decode-for-caller path ends
/// the body with an error there rather than a cut. The native client passes bytes through undecoded and is
/// unaffected.
pub(crate) const STREAM_DECODED_CHUNK_MAX: usize = 1 << 20;

/// What a committed stream is still watched for. The inspection copy is the pre-commit hold's, so the events it
/// already consumed are not seen a second time and the parser's bound is all the memory this costs.
pub(crate) struct PostCommit {
    pub inspect: Inspect,
    pub on_error: Box<dyn FnOnce(Bytes) + Send>,
}

/// What the body task tells the bus as the body goes by: every decoded piece as a chunk, the SSE phases it
/// carries, and the completion once the body has ended one way or another.
pub(crate) struct Publish {
    pub trace: Trace,
    pub clock: Clock,
    pub outcome: RelayOutcome,
    /// Notified once the answer's model is known, so a snapshot the router did not resolve to triggers a
    /// bounded refresh of the account that served it.
    pub catalog: Arc<CatalogService>,
    /// Decodes the raw chunk for the bus when the caller receives the upstream's coding as it is; `None`
    /// when the relay already decodes for the caller and the piece is text.
    pub decoder: Option<Box<dyn Decoder>>,
    /// Set once the decoder above rejected a piece, and never cleared: what follows it is no more readable
    /// than the piece that failed.
    pub decode_failed: bool,
    /// Parses the decoded copy for phases on an event stream; `None` for any other body, and dropped on
    /// overflow so a malformed stream costs a bounded buffer and no phases rather than the chunks.
    pub parser: Option<SseParser>,
    /// The decoded copy of a committed body that is not an event stream, kept up to `MAX_BUFFERED_BODY` so its
    /// usage block can be read at the end; `None` for a stream or once the bound is passed.
    pub json: Option<Vec<u8>>,
    pub usage: Usage,
    pub response: ResponseFacts,
    pub index: u32,
    pub bytes: u64,
    pub first_content_at: Option<i64>,
    pub saw_tool_use: bool,
    /// The error type of an SSE `error` event seen after the head, which turns a clean end into a failure.
    pub stream_error: Option<String>,
    /// The status the client was answered with, for the completion.
    pub status: u16,
    /// The id of the message this response carries, once `message_start` or the JSON answer named it.
    pub message_id: Option<String>,
    /// Set by `phase_of` the moment the id is known and taken by `chunk` or `finish` right after, which await
    /// the hook there.
    pub pending_message_id: Option<String>,
    /// Runs once, with the id, the moment it is known, and is awaited there: a later request on the same
    /// session reads the id before this body has finished, so the hand-off cannot wait for `finish`, and a
    /// flush must not overtake it.
    pub on_message_id: Option<Box<dyn FnOnce(String) -> BoxFuture<'static, ()> + Send>>,
    /// The pin a committed answer renews, or a rejection releases, sent with the usage `finish` assembled.
    pub renewal: Option<Renewal>,
    /// The conversation report a committed answer sends, beside the renewal.
    pub affinity: Option<AffinityFacts>,
}

/// A pinned answer's report on its pin, sent once the answer has ended and its usage is known: a renewal, or
/// the release of a pin its own request created and a client error ended.
pub(crate) struct Renewal {
    pub router: RouterHandle,
    pub pin: PinRef,
    /// `client` or `keepalive`.
    pub origin: &'static str,
    pub canonical_model: String,
    pub cache_ttl_ms: i64,
    /// Whether this request created the pin rather than renewing one that already served.
    pub fresh: bool,
    /// False for a token count: it writes no cache, so its answer renews nothing, though a client error on a
    /// pin it created still releases that pin.
    pub renews: bool,
    /// The attempt's (incarnation, lifecycle, credential) generations, which fence a release.
    pub generations: (u64, u64, u64),
}

/// The client errors that end a pin their own request created: the pin never served, so keeping it would hold
/// the session to an account on no evidence. A 429 is the cooldown path's.
const RELEASING_STATUSES: [u16; 4] = [400, 401, 403, 404];

impl Renewal {
    /// Whether an answer of `status` releases the pin instead of renewing it.
    pub(crate) fn releases(&self, status: u16) -> bool {
        self.fresh && RELEASING_STATUSES.contains(&status)
    }

    pub(crate) fn send(self, now: i64, status: u16, usage: Option<&Usage>) {
        if self.releases(status) {
            let (incarnation, lifecycle_generation, credential_generation) = self.generations;
            self.router.report(Report::PinReleased {
                pin: self.pin,
                incarnation,
                lifecycle_generation,
                credential_generation,
            });
            return;
        }
        if !self.renews {
            return;
        }
        self.router.report(Report::PinRenewed {
            pin: self.pin,
            now,
            usage: usage.filter(|u| !u.is_empty()).map(pin_usage),
            origin: self.origin,
            canonical_model: self.canonical_model,
            cache_ttl_ms: self.cache_ttl_ms,
        });
    }
}

/// A completed inference or probe answer's report on its conversation: built at the commit beside the renewal
/// with every fact the request knows, completed and sent as `Report::Affinity` once the answer has ended and its
/// message id and usage are known. A token count builds none: it writes no cache.
pub(crate) struct AffinityFacts {
    pub router: RouterHandle,
    pub account_id: String,
    pub incarnation: u64,
    pub key_id: String,
    pub key_generation: i64,
    pub canonical_model: String,
    /// The identity's `session_id`, `None` for a request with no identity (the first message boundary is the
    /// root then).
    pub root: Option<String>,
    pub static_boundaries: Vec<String>,
    pub static_sizes: Vec<u64>,
    pub message_boundaries: Vec<String>,
    /// The upstream head's `request-id`, the id a client sends back as `cc_prev_req`.
    pub request_id: Option<String>,
    /// `tier_ttls` of the body as this pass sent it upstream.
    pub static_ttl_ms: Option<i64>,
    pub message_ttl_ms: Option<i64>,
}

impl AffinityFacts {
    /// Converts into `AffinityReport` (`cache_touched` and `prefix_tokens` from the usage's three cache fields,
    /// false and 0 without them) and sends `Report::Affinity` through `router.report`; a committed answer only
    /// (`status == 200`), since a rejection continues no conversation.
    pub(crate) fn send(
        self,
        now: i64,
        status: u16,
        message_id: Option<String>,
        usage: Option<&Usage>,
    ) {
        // A rejection continues no conversation, so it leaves no link to resolve a later request by.
        if status != 200 {
            return;
        }
        let cached = |field: fn(&Usage) -> Option<u64>| usage.and_then(field).unwrap_or(0);
        let prefix_tokens = cached(|u| u.cache_read_tokens)
            + cached(|u| u.cache_write_5m_tokens)
            + cached(|u| u.cache_write_1h_tokens);
        let report = AffinityReport {
            account_id: self.account_id,
            incarnation: self.incarnation,
            key_id: self.key_id,
            key_generation: self.key_generation,
            canonical_model: self.canonical_model,
            root: self.root,
            static_boundaries: self.static_boundaries,
            message_boundaries: self.message_boundaries,
            static_sizes: self.static_sizes,
            message_id,
            request_id: self.request_id,
            static_ttl_ms: self.static_ttl_ms,
            message_ttl_ms: self.message_ttl_ms,
            cache_touched: prefix_tokens > 0,
            prefix_tokens,
            now,
        };
        self.router.report(Report::Affinity(report));
    }
}

fn pin_usage(usage: &Usage) -> PinUsage {
    PinUsage {
        input: usage.input_tokens.unwrap_or(0),
        cache_read: usage.cache_read_tokens.unwrap_or(0),
        write_5m: usage.cache_write_5m_tokens.unwrap_or(0),
        write_1h: usage.cache_write_1h_tokens.unwrap_or(0),
        output: usage.output_tokens.unwrap_or(0),
    }
}

impl Publish {
    /// `raw` is the wire chunk and `piece` what the caller is sent; one of the two is the text.
    async fn chunk(&mut self, raw: &[u8], piece: &Bytes) {
        // The size is what the caller received, which stands whether or not the bus could read it.
        self.bytes += piece.len() as u64;
        if self.decode_failed {
            return;
        }
        let text = match self.decoder.as_mut() {
            None => piece.clone(),
            Some(decoder) => match decoder.feed_bounded(raw, STREAM_DECODED_CHUNK_MAX) {
                Ok(out) => Bytes::from(out),
                // A consumer of a chunk was promised text, so nothing is better than bytes that are not.
                Err(_) => {
                    self.decode_failed = true;
                    self.decoder = None;
                    self.parser = None;
                    self.json = None;
                    return;
                }
            },
        };
        if let Some(json) = self.json.as_mut() {
            if json.len() + text.len() > MAX_BUFFERED_BODY {
                // Past the bound nothing is read; the usage stays unknown rather than half parsed.
                self.json = None;
            } else {
                json.extend_from_slice(&text);
            }
        }
        let mut phases = self.phases(&text);
        let first = phases.next();
        self.emit(text, first);
        // A piece that carried several phase events: the rest go out as empty chunks with their phase.
        for phase in phases {
            self.emit(Bytes::new(), Some(phase));
        }
        self.fire_pending_hook().await;
    }

    fn emit(&mut self, body: Bytes, phase: Option<Phase>) {
        self.trace.publish_chunk(ResponseChunk {
            request_id: self.trace.request_id.clone(),
            attempt: self.outcome.attempt,
            index: self.index,
            body,
            phase,
        });
        self.index += 1;
    }

    fn phases(&mut self, text: &[u8]) -> std::vec::IntoIter<Phase> {
        let mut out = Vec::new();
        let Some(parser) = self.parser.as_mut() else {
            return out.into_iter();
        };
        if parser.feed(text).is_err() {
            self.parser = None;
            return out.into_iter();
        }
        // Collected first: `phase_of` borrows the whole struct, so the parser's borrow has to end before it runs.
        let mut events = Vec::new();
        while let Some(event) = parser.next_event() {
            events.push(event);
        }
        for event in &events {
            if let Some(phase) = self.phase_of(event) {
                out.push(phase);
            }
        }
        out.into_iter()
    }

    fn phase_of(&mut self, event: &SseEvent) -> Option<Phase> {
        match event.name.as_deref() {
            Some(b"message_start") => {
                let answer = answer_from_json(&event.data, &["message"]);
                self.usage.merge(&answer.usage);
                self.response.merge(&answer.response);
                if let Some(id) = message_id_from_json(&event.data, &["message"]) {
                    self.saw_message_id(id);
                }
                Some(Phase::MessageStart {
                    usage: answer.usage,
                })
            }
            Some(b"content_block_delta") => {
                if self.first_content_at.is_none() {
                    self.first_content_at = Some((self.clock)());
                    return Some(Phase::FirstContentDelta);
                }
                if !self.saw_tool_use && is_tool_use_delta(&event.data) {
                    self.saw_tool_use = true;
                    return Some(Phase::ToolUseDelta);
                }
                None
            }
            Some(b"message_delta") => {
                let answer = answer_from_json(&event.data, &[]);
                self.usage.merge(&answer.usage);
                self.response.merge(&answer.response);
                Some(Phase::MessageDelta {
                    usage: answer.usage,
                })
            }
            Some(b"message_stop") => Some(Phase::MessageStop),
            // The post-commit observer already acts on this event for routing; here it only decides how the
            // request is accounted, so the bytes the client sees are untouched.
            Some(b"error") => {
                if self.stream_error.is_none() {
                    self.stream_error = Some(error_type_of(&event.data));
                }
                None
            }
            _ => None,
        }
    }

    async fn finish(mut self, end: &BodyEnd) {
        let last = self.parser.as_mut().and_then(|parser| parser.finish());
        if let Some(event) = last {
            if let Some(phase) = self.phase_of(&event) {
                self.emit(Bytes::new(), Some(phase));
            }
        }
        if let Some(json) = self.json.take() {
            let answer = answer_from_json(&json, &[]);
            self.usage.merge(&answer.usage);
            self.response.merge(&answer.response);
            if let Some(id) = message_id_from_json(&json, &[]) {
                self.saw_message_id(id);
            }
        }
        // The API answering under another snapshot is the alias moving; one refresh brings the listing up to it.
        // A body that failed or was cancelled may have named a model from a partial or misleading answer.
        if matches!(end, BodyEnd::Complete) {
            if let (Some(resolved), Some(served)) = (
                self.outcome.canonical_model.as_deref(),
                self.response.model.as_deref(),
            ) {
                self.catalog
                    .note_served(&self.outcome.account_id, resolved, served);
            }
        }
        self.fire_pending_hook().await;
        let outcome = match (end, self.stream_error.take()) {
            (BodyEnd::Complete, None) => None,
            (BodyEnd::Complete, Some(error)) => Some((Outcome::Failed, Some(error))),
            (BodyEnd::Cancelled, _) => Some((Outcome::Canceled, Some("account_cancelled".into()))),
            (BodyEnd::ClientGone, _) => Some((Outcome::Canceled, Some("client_gone".into()))),
            (BodyEnd::Failed, error) => Some((
                Outcome::Failed,
                error.or_else(|| Some("upstream_unreachable".into())),
            )),
        };
        let ttfc_ms = self
            .first_content_at
            .map(|at| at - self.outcome.received_at);
        let usage = (!self.usage.is_empty()).then(|| self.usage.clone());
        if let Some(renewal) = self.renewal.take() {
            renewal.send((self.clock)(), self.status, usage.as_ref());
        }
        if let Some(affinity) = self.affinity.take() {
            affinity.send(
                (self.clock)(),
                self.status,
                self.message_id.clone(),
                usage.as_ref(),
            );
        }
        self.trace
            .complete(
                &self.outcome,
                self.status,
                usage,
                self.response.clone(),
                ttfc_ms,
                Some(self.bytes),
                Some(self.index),
                outcome,
                (self.clock)(),
            )
            .await;
    }

    /// The id is handed off once, on first sight: `message_start` names it for a stream, the whole document for a
    /// JSON answer, and neither arrives twice. Recorded here; `chunk` and `finish` take the pending id and await
    /// the hook once `phase_of` returns.
    fn saw_message_id(&mut self, id: String) {
        if self.message_id.is_none() {
            self.pending_message_id = Some(id.clone());
            self.message_id = Some(id);
        }
    }

    /// Awaited from `chunk` and `finish` once `phase_of` has had its turn, so a pending id from either path
    /// fires the hook the same way.
    async fn fire_pending_hook(&mut self) {
        if let Some(id) = self.pending_message_id.take() {
            if let Some(hook) = self.on_message_id.take() {
                hook(id).await;
            }
        }
    }
}

/// `error.type` of an SSE `error` event's data, or `error` when the data does not say.
fn error_type_of(data: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(data)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("type"))
                .and_then(|t| t.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "error".into())
}

/// The measured pool a relayed connection came from, so the body task can hand it back.
#[derive(Clone)]
pub(crate) struct Checkin {
    pub pool: Arc<Pool>,
    pub key: PoolKey,
}

pub(crate) trait ReturnToPool: Sized {
    fn return_to(self, checkin: Checkin);
}

impl ReturnToPool for Conn {
    fn return_to(self, checkin: Checkin) {
        checkin.pool.checkin(checkin.key, self);
    }
}

/// Everything the body task needs once the head has been forwarded.
pub(crate) struct Relayed<C> {
    /// Cancelled when the account this body belongs to is disconnected or deleted.
    pub cancel: CancellationToken,
    pub checkin: Checkin,
    pub decoder: Option<Box<dyn Decoder>>,
    /// The response's in-flight counts, released when this body ends.
    pub guards: Guards,
    pub idle_timeout: Duration,
    pub keep_alive: bool,
    pub observer: Option<PostCommit>,
    /// Body bytes already read, which go out ahead of anything the reader still produces.
    pub prefix: Bytes,
    pub publish: Option<Publish>,
    pub reader: BodyReader<C>,
}

/// Relays the upstream body as it arrives: one task reads, the response body drains the channel, and the
/// connection goes back to the pool only after a clean end. Spawned under `tracker` rather than bare, so a
/// shutdown can wait for a task a cut connection left running.
pub(crate) fn relay_body<C: AsyncRead + ReturnToPool + Unpin + Send + 'static>(
    relayed: Relayed<C>,
    tracker: &TaskTracker,
) -> Body {
    let (tx, rx) = mpsc::channel::<Result<Bytes, TransportError>>(CHANNEL_DEPTH);
    tracker.spawn(async move {
        let Relayed {
            cancel,
            checkin,
            mut decoder,
            guards,
            idle_timeout,
            keep_alive,
            mut observer,
            prefix,
            mut publish,
            mut reader,
        } = relayed;
        // The hold already fed the held bytes to the inspection copy, so the prefix is scanned for what it left
        // behind rather than fed again.
        observe(&mut observer);
        let end = 'body: {
            if !prefix.is_empty() {
                match decode_piece(decoder.as_mut(), prefix.clone()) {
                    Ok(piece) => {
                        publish_piece(&mut publish, &prefix, &piece).await;
                        match send(&tx, &cancel, Ok(piece)).await {
                            Delivery::Delivered => {}
                            Delivery::Cancelled => break 'body BodyEnd::Cancelled,
                            Delivery::Gone => break 'body BodyEnd::ClientGone,
                        }
                    }
                    Err(error) => {
                        let _ = send(&tx, &cancel, Err(error)).await;
                        break 'body BodyEnd::Failed;
                    }
                }
            }
            loop {
                // A caller that hangs up mid-stream has to release the upstream connection right away; waiting
                // for the next chunk would hold it until the idle timeout on a stream that has gone quiet.
                let read = tokio::select! {
                    _ = tx.closed() => break 'body BodyEnd::ClientGone,
                    // The credential this stream was opened with is gone, so the body ends in an error and
                    // axum closes the caller's connection rather than letting a dead account keep streaming.
                    _ = cancel.cancelled() => break 'body BodyEnd::Cancelled,
                    read = tokio::time::timeout(idle_timeout, reader.next_chunk()) => read,
                };
                let next = match read {
                    Err(_) => {
                        let _ = send(&tx, &cancel, Err(stalled())).await;
                        break 'body BodyEnd::Failed;
                    }
                    Ok(Err(error)) => {
                        let _ = send(&tx, &cancel, Err(error)).await;
                        break 'body BodyEnd::Failed;
                    }
                    Ok(Ok(next)) => next,
                };
                let Some(chunk) = next else {
                    break;
                };
                // The raw chunk, because the inspection copy runs the upstream's own content coding; it is read
                // before the chunk goes out so a late rate-limit error is acted on no later than the client
                // sees it.
                feed(&mut observer, &chunk);
                observe(&mut observer);
                let piece = match decode_piece(decoder.as_mut(), chunk.clone()) {
                    Ok(piece) => piece,
                    Err(error) => {
                        let _ = send(&tx, &cancel, Err(error)).await;
                        break 'body BodyEnd::Failed;
                    }
                };
                publish_piece(&mut publish, &chunk, &piece).await;
                if piece.is_empty() {
                    continue;
                }
                match send(&tx, &cancel, Ok(piece)).await {
                    Delivery::Delivered => {}
                    Delivery::Cancelled => break 'body BodyEnd::Cancelled,
                    Delivery::Gone => break 'body BodyEnd::ClientGone,
                }
            }
            observe_end(&mut observer);
            if let Some(mut decoder) = decoder {
                match decoder.finish_bounded(STREAM_DECODED_CHUNK_MAX) {
                    Ok(tail) if !tail.is_empty() => {
                        let tail = Bytes::from(tail);
                        // The tail is the rest of the decoded body, so the chunks a consumer joins are the
                        // whole of what the caller received.
                        publish_piece(&mut publish, &[], &tail).await;
                        match send(&tx, &cancel, Ok(tail)).await {
                            Delivery::Delivered => {}
                            Delivery::Cancelled => break 'body BodyEnd::Cancelled,
                            Delivery::Gone => break 'body BodyEnd::ClientGone,
                        }
                    }
                    Err(error) => {
                        let _ = send(&tx, &cancel, Err(error)).await;
                        break 'body BodyEnd::Failed;
                    }
                    Ok(_) => {}
                }
            }
            BodyEnd::Complete
        };
        match end {
            BodyEnd::Cancelled => {
                // The upstream socket goes first. A caller that stopped reading leaves the channel full, so
                // waiting for capacity to report the end would keep a dead account's connection open for as
                // long as that caller cares to hold it.
                drop(reader);
                let _ = tx.try_send(Err(cancelled()));
            }
            BodyEnd::ClientGone | BodyEnd::Failed => {}
            // Checking in before the sender drops is what makes the connection available again by the time the
            // caller sees the end of the body, which is the only ordering a sequential caller can observe.
            BodyEnd::Complete => {
                if keep_alive {
                    reader.into_inner().return_to(checkin);
                }
            }
        }
        if let Some(publish) = publish {
            publish.finish(&end).await;
        }
        // Ahead of the sender's drop, so a caller that has read the body's end finds both counts released.
        drop(guards);
    });
    Body::from_stream(futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    }))
}

/// How the relayed body stopped, which decides what happens to the upstream connection.
enum BodyEnd {
    Cancelled,
    /// The caller is no longer listening.
    ClientGone,
    Complete,
    /// The body ended in an error the caller has already been sent.
    Failed,
}

enum Delivery {
    Cancelled,
    Delivered,
    Gone,
}

/// One piece downstream, raced against cancellation. Without the race a caller that stops reading with the
/// channel full would pin this task inside `send` and, with it, the upstream connection: cancellation could
/// not be observed until the caller resumed or hung up. A receiver that is gone resolves the send on its own.
async fn send(
    tx: &mpsc::Sender<Result<Bytes, TransportError>>,
    cancel: &CancellationToken,
    item: Result<Bytes, TransportError>,
) -> Delivery {
    tokio::select! {
        _ = cancel.cancelled() => Delivery::Cancelled,
        sent = tx.send(item) => match sent {
            Ok(()) => Delivery::Delivered,
            Err(_) => Delivery::Gone,
        },
    }
}

/// Hands a forwarded piece to the bus: the wire chunk and what the caller is sent, one of which is the text.
async fn publish_piece(publish: &mut Option<Publish>, raw: &[u8], piece: &Bytes) {
    if let Some(publish) = publish.as_mut() {
        publish.chunk(raw, piece).await;
    }
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
pub(crate) async fn drain_bounded<C: AsyncRead + Unpin>(
    reader: &mut BodyReader<C>,
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
        Some(decoder) => decoder
            .feed_bounded(&raw, STREAM_DECODED_CHUNK_MAX)
            .map(Bytes::from),
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
