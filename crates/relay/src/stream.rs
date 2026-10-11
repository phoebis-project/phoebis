//! Upstream connection lifecycle and the piece-by-piece relay of a response body.

use std::future::Future;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use bytes::{Bytes, BytesMut};
use futures::future::BoxFuture;
use phoebis_bus::{Outcome, Phase, ResponseChunk, ResponseFacts, Usage};
use phoebis_oauth::Clock;
use phoebis_routing::{AffinityReport, PinRef, PinUsage, Report, RouterHandle};
use phoebis_transport::decode::Decoder;
use phoebis_transport::pool::{Conn, Pool, PoolKey};
use phoebis_transport::response::BodyReader;
use phoebis_transport::TransportError;
use tokio::io::AsyncRead;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::catalog::CatalogService;
use crate::identity::PendingReports;
use crate::messages::{Guards, RelayOutcome, Trace};
use crate::observe::{answer_from_json, is_tool_use_delta, message_id_from_json};
use crate::sse::{SseEvent, SseParser};

/// Chunks held between the upstream reader and the caller's socket. Deep enough that a caller pausing for a
/// moment does not stall the read loop, shallow enough that a caller that stops reading still pushes back on
/// the upstream instead of letting the response pile up in memory.
const CHANNEL_DEPTH: usize = 64;

/// Cap on a response the relay reads before it can decide what to do with it, and on the decoded copy a
/// compressed rejection is classified from: a 400, 401, 403, 404 or 429 is classified from its first 64 KiB, and a
/// larger body is forwarded after the prefix when the verdict forwards it.
pub(crate) const MAX_BUFFERED_BODY: usize = 64 * 1024;

/// The four ways a body ends short, the request row's `error_code`: under `incomplete` on a 200, and a cut, a stall
/// or a body that does not decode under `failed` on any other status.
pub const STREAM_CUT: &str = "stream_cut";
pub const STREAM_INCOMPLETE: &str = "stream_incomplete";
pub const STREAM_STALLED: &str = "stream_stalled";
pub const STREAM_UNDECODABLE: &str = "stream_undecodable";

/// Per-chunk bound on what a decoder may hand back for one upstream chunk: an 8 KiB compressed chunk can
/// expand into hundreds of MiB in a single call without this, whatever the caller does with the result after.
/// It is a budget against the 8 KiB reads the transport fills at a time, not against a whole body: at
/// deflate's compression ceiling one wire chunk can still decode past it, and the decode-for-caller path ends
/// the body with an error there rather than a cut. The native client passes bytes through undecoded and is
/// unaffected.
pub(crate) const STREAM_DECODED_CHUNK_MAX: usize = 1 << 20;

/// Reports what a post-commit error event means for routing, through the body's `Waiting`, so a report the full
/// mailbox refuses waits behind the client's answer and ahead of the answer's own end reports.
pub(crate) type OnError = Box<dyn FnOnce(Bytes, &mut Waiting) + Send>;

/// The held bytes of a stream as the pre-commit hold read them: their text and the events the hold took from them,
/// the committing one last. The bus copy publishes these rather than decoding and parsing the held bytes again.
pub(crate) struct HeldText {
    pub text: Bytes,
    pub events: Vec<SseEvent>,
}

/// The reports an answer could not hand the router at once, in the order it made them. Each report takes a mailbox
/// slot where it is made while the router keeps up, so an affinity number is filed and a cooldown is in force before
/// the client reads the answer's end; from the first report a full mailbox refuses, that one and every later one wait
/// here, and the body task awaits them after the client's answer, holding the request's guards until they are queued.
#[derive(Default)]
pub(crate) struct Waiting(Vec<BoxFuture<'static, ()>>);

impl Waiting {
    /// Hands `report` to the router now when its mailbox has a slot and nothing of this answer waits ahead of it,
    /// answering what `RouterHandle::report` does; otherwise answers the report back for `wait`.
    pub(crate) fn try_now(
        &self,
        router: &RouterHandle,
        report: Report,
    ) -> Result<Option<u64>, Box<Report>> {
        if self.0.is_empty() {
            router.try_report(report)
        } else {
            Err(Box::new(report))
        }
    }

    /// `try_now`, and behind the waiting reports when it cannot go now.
    pub(crate) fn send(&mut self, router: &RouterHandle, report: Report) {
        if let Err(report) = self.try_now(router, report) {
            let router = router.clone();
            self.wait(async move {
                router.report(*report).await;
            });
        }
    }

    pub(crate) fn wait(&mut self, report: impl Future<Output = ()> + Send + 'static) {
        self.0.push(Box::pin(report));
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Awaits each waiting report in order.
    pub(crate) async fn land(self) {
        for report in self.0 {
            report.await;
        }
    }
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
    /// when the relay already decodes for the caller and the piece is text. A held stream's is the hold's own,
    /// already past the held bytes.
    pub decoder: Option<Box<dyn Decoder>>,
    /// Set once the decoder above rejected a piece, and never cleared: what follows it is no more readable
    /// than the piece that failed.
    pub decode_failed: bool,
    /// Parses the decoded copy for phases on an event stream; `None` for any other body, and dropped on
    /// overflow so a malformed stream costs a bounded buffer and no phases rather than the chunks. A held stream's
    /// is the hold's own, already past the held events.
    pub parser: Option<SseParser>,
    /// The held bytes' text and events when the hold handed its decoder and parser over, taken by the prefix.
    pub held: Option<HeldText>,
    /// Fired once, on the first `error` event after the commit; `None` for a stream nothing watches.
    pub on_error: Option<OnError>,
    /// The decoded copy of a committed body that is not an event stream, kept up to the request bound so its
    /// usage block can be read at the end; `None` for a stream or once the bound is passed.
    pub json: Option<Vec<u8>>,
    /// `Limits.max_request_bytes`, so the usage of any answer the router would accept as a request is read.
    pub json_bound: usize,
    pub usage: Usage,
    pub response: ResponseFacts,
    pub index: u32,
    pub bytes: u64,
    pub first_content_at: Option<i64>,
    pub saw_tool_use: bool,
    /// The error type of an SSE `error` event seen after the head, which turns a clean end into a failure.
    pub stream_error: Option<String>,
    /// Whether the parser saw `message_stop`, without which a clean end of a stream is one the upstream cut short.
    pub saw_stop: bool,
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

    /// `fast` is whether the answer reported `usage.speed` `fast`.
    pub(crate) fn send(
        self,
        now: i64,
        status: u16,
        usage: Option<&Usage>,
        fast: bool,
        waiting: &mut Waiting,
    ) {
        if self.releases(status) {
            let (incarnation, lifecycle_generation, credential_generation) = self.generations;
            waiting.send(
                &self.router,
                Report::PinReleased {
                    pin: self.pin,
                    incarnation,
                    lifecycle_generation,
                    credential_generation,
                },
            );
            return;
        }
        if !self.renews {
            return;
        }
        waiting.send(
            &self.router,
            Report::PinRenewed {
                pin: self.pin,
                now,
                usage: usage.filter(|u| !u.is_empty()).map(pin_usage),
                origin: self.origin,
                cache_ttl_ms: self.cache_ttl_ms,
                fast,
            },
        );
    }
}

/// A completed inference or probe answer's report on its conversation: built at the commit beside the renewal
/// with every fact the request knows, completed and sent as `Report::Affinity` once the answer has ended and its
/// message id and usage are known. Built for a pinned and an unpinned request alike, so it also carries the
/// answer's usage to the router's window prediction. A token count builds none: it writes no cache.
pub(crate) struct AffinityFacts {
    pub router: RouterHandle,
    /// Where the report's sequence is filed for the conversation's next turn to wait on.
    pub pending: Arc<PendingReports>,
    /// The request's own session id, which its next turn waits on.
    pub session_id: Option<String>,
    pub account_id: String,
    pub incarnation: u64,
    pub key_id: String,
    pub key_generation: i64,
    pub canonical_model: String,
    pub lane: String,
    /// The identity's session id, `None` for a request with no identity.
    pub root: Option<String>,
    pub static_boundaries: Arc<[String]>,
    pub static_sizes: Vec<u64>,
    pub message_boundaries: Arc<[String]>,
    /// `tier_ttls` of the body as this pass sent it upstream.
    pub static_ttl_ms: Option<i64>,
    pub message_ttl_ms: Option<i64>,
    /// `uniform_ttl` of the same body.
    pub uniform_ttl_ms: Option<i64>,
    /// The request, the decision that selected the account and the wire attempt that answered, so the rows the
    /// report writes reach the selecting decision's row.
    pub request_id: String,
    pub seq: u32,
    pub attempt: u32,
    /// Whether the answer's id is remembered: the leg carried or received the member and is not a probe, whose
    /// answer belongs to the turn it replayed.
    pub remembers: bool,
}

impl AffinityFacts {
    /// Sends `Report::Usage` for an answer whose usage is not empty, then converts into `AffinityReport`
    /// (`cache_touched` and `prefix_tokens` from the usage's three cache fields, false and 0 without them) and
    /// sends `Report::Affinity` through `waiting`, filing the sequence it took in `pending` once it has one; a
    /// committed answer only (`status == 200`), since a rejection continues no conversation and consumes no window.
    pub(crate) fn send(
        self,
        now: i64,
        status: u16,
        usage: Option<&Usage>,
        fast: bool,
        message_id: Option<String>,
        waiting: &mut Waiting,
    ) {
        // A rejection continues no conversation, so it leaves no link to resolve a later request by.
        if status != 200 {
            return;
        }
        if let Some(counted) = usage.filter(|u| !u.is_empty()) {
            waiting.send(
                &self.router,
                Report::Usage {
                    account_id: self.account_id.clone(),
                    at: now,
                    canonical_model: self.canonical_model.clone(),
                    incarnation: self.incarnation,
                    usage: pin_usage(counted),
                    fast,
                },
            );
        }
        let cached = |field: fn(&Usage) -> Option<u64>| usage.and_then(field).unwrap_or(0);
        let prefix_tokens = cached(|u| u.cache_read_tokens)
            + cached(|u| u.cache_write_5m_tokens)
            + cached(|u| u.cache_write_1h_tokens);
        // The identifier the next turn's wait reads this conversation by.
        let ids: Vec<String> = self
            .session_id
            .iter()
            .map(|id| format!("session:{id}"))
            .collect();
        let (router, pending, key_id, key_generation) = (
            self.router,
            self.pending,
            self.key_id.clone(),
            self.key_generation,
        );
        let report = AffinityReport {
            account_id: self.account_id,
            incarnation: self.incarnation,
            key_id: self.key_id,
            key_generation: self.key_generation,
            canonical_model: self.canonical_model,
            lane: self.lane,
            root: self.root,
            static_boundaries: self.static_boundaries,
            message_boundaries: self.message_boundaries,
            static_sizes: self.static_sizes,
            static_ttl_ms: self.static_ttl_ms,
            message_ttl_ms: self.message_ttl_ms,
            cache_touched: prefix_tokens > 0,
            prefix_tokens,
            now,
            cache_write_5m_tokens: usage.and_then(|u| u.cache_write_5m_tokens),
            cache_write_1h_tokens: usage.and_then(|u| u.cache_write_1h_tokens),
            uniform_ttl_ms: self.uniform_ttl_ms,
            request_id: self.request_id,
            seq: self.seq,
            attempt: self.attempt,
            message_id: message_id.filter(|_| self.remembers),
        };
        let file = move |router: &RouterHandle, seq: Option<u64>| {
            if let Some(seq) = seq {
                pending.record(&key_id, key_generation, &ids, seq, router.applied_report());
            }
        };
        match waiting.try_now(&router, Report::Affinity(report)) {
            Ok(seq) => file(&router, seq),
            Err(report) => waiting.wait(async move {
                let seq = router.report(*report).await;
                file(&router, seq);
            }),
        }
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
    /// The bytes the body starts with: a held stream's as the hold read them, any other prefix as a chunk.
    async fn prefix(&mut self, raw: &[u8], piece: &Bytes, waiting: &mut Waiting) {
        let Some(held) = self.held.take() else {
            return self.chunk(raw, piece, waiting).await;
        };
        self.bytes += piece.len() as u64;
        // The held events were the verdict's to read, the committing one among them; only what follows them is
        // watched for a late error.
        let on_error = self.on_error.take();
        let mut phases: Vec<Phase> = held
            .events
            .iter()
            .filter_map(|event| self.phase_of(event, waiting))
            .collect();
        self.on_error = on_error;
        phases.extend(self.parsed(waiting));
        self.emit_phases(held.text, phases);
        self.fire_pending_hook().await;
    }

    /// `raw` is the wire chunk and `piece` what the caller is sent; one of the two is the text.
    async fn chunk(&mut self, raw: &[u8], piece: &Bytes, waiting: &mut Waiting) {
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
            if json.len() + text.len() > self.json_bound {
                // Past the bound nothing is read; the usage stays unknown rather than half parsed.
                self.json = None;
            } else {
                json.extend_from_slice(&text);
            }
        }
        let phases = self.phases(&text, waiting);
        self.emit_phases(text, phases);
        self.fire_pending_hook().await;
    }

    fn emit_phases(&mut self, text: Bytes, phases: Vec<Phase>) {
        let mut phases = phases.into_iter();
        let first = phases.next();
        self.emit(text, first);
        // A piece that carried several phase events: the rest go out as empty chunks with their phase.
        for phase in phases {
            self.emit(Bytes::new(), Some(phase));
        }
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

    fn phases(&mut self, text: &[u8], waiting: &mut Waiting) -> Vec<Phase> {
        let Some(parser) = self.parser.as_mut() else {
            return Vec::new();
        };
        if parser.feed(text).is_err() {
            self.parser = None;
            return Vec::new();
        }
        self.parsed(waiting)
    }

    /// The phases of the complete events the parser holds.
    fn parsed(&mut self, waiting: &mut Waiting) -> Vec<Phase> {
        let Some(parser) = self.parser.as_mut() else {
            return Vec::new();
        };
        // Collected first: `phase_of` borrows the whole struct, so the parser's borrow has to end before it runs.
        let mut events = Vec::new();
        while let Some(event) = parser.next_event() {
            events.push(event);
        }
        events
            .iter()
            .filter_map(|event| self.phase_of(event, waiting))
            .collect()
    }

    fn phase_of(&mut self, event: &SseEvent, waiting: &mut Waiting) -> Option<Phase> {
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
            Some(b"message_stop") => {
                self.saw_stop = true;
                Some(Phase::MessageStop)
            }
            // Read before the piece that carries it goes out, so a late rate limit is acted on no later than the
            // client sees it; the bytes the client sees are untouched.
            Some(b"error") => {
                if self.stream_error.is_none() {
                    self.stream_error = Some(error_type_of(&event.data));
                }
                if let Some(on_error) = self.on_error.take() {
                    on_error(Bytes::copy_from_slice(&event.data), waiting);
                }
                None
            }
            _ => None,
        }
    }

    /// The answer's end reports go through `waiting`, ahead of the completion.
    async fn finish(mut self, end: &BodyEnd, waiting: &mut Waiting) {
        // A stream cut off after its last event never sent the empty line that would close it, so the event, an
        // error among them, can still be sitting in the parser.
        let last = self.parser.as_mut().and_then(|parser| parser.finish());
        if let Some(event) = last {
            if let Some(phase) = self.phase_of(&event, waiting) {
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
        // `incomplete` is a 200 whose message ended short: a cut, a stall, or a clean end before which a live
        // parser saw no stop and no error, which a success count would hide. A dropped parser proves nothing and
        // keeps the head's word. Any other status answered no message to cut short, so it stays `failed`, and a cut
        // or a stall still names its cause.
        let committed = self.status == 200;
        let outcome = match (end, self.stream_error.take()) {
            (BodyEnd::Complete, None) if committed && self.parser.is_some() && !self.saw_stop => {
                Some((Outcome::Incomplete, Some(STREAM_INCOMPLETE.into())))
            }
            (BodyEnd::Complete, None) => None,
            (BodyEnd::Complete, Some(error)) => Some((Outcome::Failed, Some(error))),
            (BodyEnd::Cancelled, _) => Some((Outcome::Canceled, Some("account_cancelled".into()))),
            (BodyEnd::ClientGone, _) => Some((Outcome::Canceled, Some("client_gone".into()))),
            (BodyEnd::Failed(_), Some(error)) => Some((Outcome::Failed, Some(error))),
            (BodyEnd::Failed(cut), None) if committed => {
                Some((Outcome::Incomplete, Some(cut.code().into())))
            }
            (BodyEnd::Failed(cut), None) => Some((Outcome::Failed, Some(cut.code().into()))),
        };
        let ttfc_ms = self
            .first_content_at
            .map(|at| at - self.outcome.received_at);
        let usage = (!self.usage.is_empty()).then(|| self.usage.clone());
        let fast = self.response.speed.as_deref() == Some("fast");
        if let Some(renewal) = self.renewal.take() {
            renewal.send((self.clock)(), self.status, usage.as_ref(), fast, waiting);
        }
        if let Some(affinity) = self.affinity.take() {
            affinity.send(
                (self.clock)(),
                self.status,
                usage.as_ref(),
                fast,
                self.message_id.clone(),
                waiting,
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
                self.message_id.clone(),
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
            prefix,
            mut publish,
            mut reader,
        } = relayed;
        let mut waiting = Waiting::default();
        let end = 'body: {
            if !prefix.is_empty() {
                match decode_piece(decoder.as_mut(), prefix.clone()) {
                    Ok(piece) => {
                        if let Some(publish) = publish.as_mut() {
                            publish.prefix(&prefix, &piece, &mut waiting).await;
                        }
                        match send(&tx, &cancel, Ok(piece)).await {
                            Delivery::Delivered => {}
                            Delivery::Cancelled => break 'body BodyEnd::Cancelled,
                            Delivery::Gone => break 'body BodyEnd::ClientGone,
                        }
                    }
                    Err(error) => {
                        let cut = Cut::of_decode(&error);
                        let _ = send(&tx, &cancel, Err(error)).await;
                        break 'body BodyEnd::Failed(cut);
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
                        break 'body BodyEnd::Failed(Cut::Stalled);
                    }
                    Ok(Err(error)) => {
                        let _ = send(&tx, &cancel, Err(error)).await;
                        break 'body BodyEnd::Failed(Cut::Transport);
                    }
                    Ok(Ok(next)) => next,
                };
                let Some(chunk) = next else {
                    break;
                };
                let piece = match decode_piece(decoder.as_mut(), chunk.clone()) {
                    Ok(piece) => piece,
                    Err(error) => {
                        let cut = Cut::of_decode(&error);
                        let _ = send(&tx, &cancel, Err(error)).await;
                        break 'body BodyEnd::Failed(cut);
                    }
                };
                // Published before the piece goes out, so a late rate-limit error is acted on no later than the
                // client sees it. That holds while the router's mailbox has room; a full one makes the cooldown
                // wait until after the body's end, behind the client's answer.
                publish_piece(&mut publish, &chunk, &piece, &mut waiting).await;
                if piece.is_empty() {
                    continue;
                }
                match send(&tx, &cancel, Ok(piece)).await {
                    Delivery::Delivered => {}
                    Delivery::Cancelled => break 'body BodyEnd::Cancelled,
                    Delivery::Gone => break 'body BodyEnd::ClientGone,
                }
            }
            if let Some(mut decoder) = decoder {
                match decoder.finish_bounded(STREAM_DECODED_CHUNK_MAX) {
                    Ok(tail) if !tail.is_empty() => {
                        let tail = Bytes::from(tail);
                        // The tail is the rest of the decoded body, so the chunks a consumer joins are the
                        // whole of what the caller received.
                        publish_piece(&mut publish, &[], &tail, &mut waiting).await;
                        match send(&tx, &cancel, Ok(tail)).await {
                            Delivery::Delivered => {}
                            Delivery::Cancelled => break 'body BodyEnd::Cancelled,
                            Delivery::Gone => break 'body BodyEnd::ClientGone,
                        }
                    }
                    Err(error) => {
                        let cut = Cut::of_decode(&error);
                        let _ = send(&tx, &cancel, Err(error)).await;
                        break 'body BodyEnd::Failed(cut);
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
            BodyEnd::ClientGone | BodyEnd::Failed(_) => {}
            // Checking in before the sender drops is what makes the connection available again by the time the
            // caller sees the end of the body, which is the only ordering a sequential caller can observe.
            BodyEnd::Complete => {
                if keep_alive {
                    reader.into_inner().return_to(checkin);
                }
            }
        }
        if let Some(publish) = publish {
            publish.finish(&end, &mut waiting).await;
        }
        if waiting.is_empty() {
            // Ahead of the sender's drop, so a caller that has read the body's end finds both counts released.
            drop(guards);
            drop(tx);
        } else {
            // The client reads its EOF before any report is awaited: a full mailbox holds the key permit and the
            // account ticket, never the answer. The counts are released once the reports are queued, so under a
            // wedged loop a client's next turn can arrive before this one's affinity number is filed, and its
            // climb runs without the wait.
            drop(tx);
            waiting.land().await;
            drop(guards);
        }
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
    /// The body ended in an error the caller has already been sent; `Cut` is the cause the completion names.
    Failed(Cut),
}

#[derive(Clone, Copy)]
enum Cut {
    Stalled,
    Transport,
    Undecodable,
}

impl Cut {
    fn code(self) -> &'static str {
        match self {
            Cut::Stalled => STREAM_STALLED,
            Cut::Transport => STREAM_CUT,
            Cut::Undecodable => STREAM_UNDECODABLE,
        }
    }

    /// A decoder's failure: undecodable when the decoder refused the payload, and a cut for the per-chunk bound, so
    /// the streamed body files the two errors as `unreadable` files them on a buffered one.
    fn of_decode(error: &TransportError) -> Cut {
        if is_undecodable(error) {
            Cut::Undecodable
        } else {
            Cut::Transport
        }
    }
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
async fn publish_piece(
    publish: &mut Option<Publish>,
    raw: &[u8],
    piece: &Bytes,
    waiting: &mut Waiting,
) {
    if let Some(publish) = publish.as_mut() {
        publish.chunk(raw, piece, waiting).await;
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

/// The idle bound's error, its own payload type telling it apart from any other timeout.
#[derive(Debug)]
struct Stalled;

impl std::fmt::Display for Stalled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("upstream stopped sending")
    }
}

impl std::error::Error for Stalled {}

fn stalled() -> TransportError {
    TransportError::Io(io::Error::new(io::ErrorKind::TimedOut, Stalled))
}

/// Whether `error` is the idle bound's: the upstream went silent past it.
pub(crate) fn is_stalled(error: &TransportError) -> bool {
    matches!(error, TransportError::Io(error) if error.get_ref().is_some_and(|inner| inner.is::<Stalled>()))
}

/// Whether `error` is a decoder's refusal: a coding the relay cannot read, or a payload that does not decode.
pub(crate) fn is_undecodable(error: &TransportError) -> bool {
    matches!(error, TransportError::Decode(_))
}
