//! Upstream connection lifecycle and the piece-by-piece relay of a response body.

use std::future::Future;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use bytes::{Bytes, BytesMut};
use futures::future::BoxFuture;
use phoebis_bus::{Interruption, Outcome, Phase, ResponseChunk, ResponseFacts, Usage};
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
use crate::credit::CreditStore;
use crate::dispatch::until_canceled;
use crate::errors::{
    Failure, ACCOUNT_CANCELED_CODE, UPSTREAM_CUT, UPSTREAM_STALLED, UPSTREAM_UNDECODABLE,
};
use crate::hold::INSPECT_BUFFER;
use crate::identity::PendingReports;
use crate::messages::{cut_code, Guards, RelayOutcome, Trace, CANCELED, REFUSED, TRANSPORT_ERROR};
use crate::observe::{answer_from_json, is_tool_use_delta, Answer};
use crate::sse::{SkippingParser, SseEvent};

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
/// deflate's compression ceiling one wire chunk can still decode past it, and the body ends with an error there
/// rather than a silently shortened chunk.
pub(crate) const STREAM_DECODED_CHUNK_MAX: usize = 1 << 20;

/// Reports what a post-commit error event means for routing, through the body's `Waiting`, so a report the full
/// mailbox refuses waits behind the client's answer and ahead of the answer's own end reports.
pub(crate) type OnError = Box<dyn FnOnce(Bytes, &mut Waiting) + Send>;

/// Records the answer's message id in the session memory, awaited on the body task before the next piece goes out.
pub(crate) type OnMessageId = Box<dyn FnOnce(String) -> BoxFuture<'static, ()> + Send>;

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

/// What the bus copy reads of a body, one of three exclusive ways.
pub(crate) enum BodyRead {
    /// Parses an event stream for phases. The parser skips an event past its bound and keeps parsing, so an oversized
    /// event costs a bounded buffer and its own phase while the events after it still reach the completion. A held
    /// stream's is the hold's own, already past the held events.
    Events(SkippingParser),
    /// The decoded copy of a committed body that is not an event stream, kept up to the request bound so its usage
    /// block can be read at the end.
    Json {
        body: Vec<u8>,
        /// `Limits.max_request_bytes`, so the usage of any answer the router would accept as a request is read.
        bound: usize,
    },
    /// Nothing is read: a rejection that is no event stream, or a JSON answer past the bound.
    Unread,
}

/// What the body task tells the bus as the body goes by: every decoded piece as a chunk, the SSE phases it
/// carries, and the completion once the body has ended one way or another.
pub(crate) struct Publish {
    pub trace: Trace,
    pub clock: Clock,
    pub outcome: RelayOutcome,
    /// The serving account's incarnation, which a refusal's fallback credit token is recorded under, so a holder
    /// created again under the id takes no credit.
    pub incarnation: u64,
    /// Notified once the answer's model is known, so a snapshot the router did not resolve to triggers a
    /// bounded refresh of the account that served it.
    pub catalog: Arc<CatalogService>,
    /// Where a refusal's fallback credit token is recorded for the serving account, which the selection of the body
    /// carrying the token reads.
    pub credits: Arc<CreditStore>,
    /// What the copy reads of the decoded body.
    pub read: BodyRead,
    /// The held bytes' text and events when the hold handed its parser over, taken by the prefix.
    pub held: Option<HeldText>,
    /// Fired once, on the first `error` event after the commit; `None` for a stream nothing watches.
    pub on_error: Option<OnError>,
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
    /// Runs once, with the id, the moment it is known, and is awaited there: a later request on the same
    /// session reads the id before this body has finished, so the hand-off cannot wait for `finish`, and a
    /// flush must not overtake it.
    pub on_message_id: Option<OnMessageId>,
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
    /// Whether a keepalive probe sent the request, rather than the client.
    pub keepalive: bool,
    pub cache_ttl_ms: i64,
    /// Whether this request created the pin rather than renewing one that already served.
    pub fresh: bool,
    /// The request is the client's own classifier call, so the renewal enters the unmetered copy.
    pub classifier: bool,
    /// The request's class is a turn's, `main` or `subagent`, so its stream's end is the turn's end the router notes.
    pub turn: bool,
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
                keepalive: self.keepalive,
                cache_ttl_ms: self.cache_ttl_ms,
                fast,
                classifier: self.classifier,
                turn: self.turn,
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
    pub account_id: String,
    pub incarnation: u64,
    pub key_id: String,
    pub key_generation: i64,
    pub canonical_model: String,
    /// The request's entrypoint as the identity read it, what the usage report's charge prices under: `None` for a
    /// client that is not Claude Code.
    pub entrypoint: Option<String>,
    /// The request is the client's own classifier call, so its usage report charges no plan window.
    pub classifier: bool,
    pub lane: String,
    /// The identity's session id, which the contract guarantees.
    pub root: String,
    pub static_boundaries: Arc<[String]>,
    pub static_sizes: Vec<u64>,
    pub message_boundaries: Arc<[String]>,
    /// `MarkerTtls::tiers` of the body as this pass sent it upstream.
    pub static_ttl_ms: Option<i64>,
    pub message_ttl_ms: Option<i64>,
    /// `MarkerTtls::uniform` of the same body.
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
    /// Answers whether the usage went out, which is what releases the request's booking.
    pub(crate) fn send(
        self,
        now: i64,
        status: u16,
        usage: Option<&Usage>,
        fast: bool,
        message_id: Option<String>,
        waiting: &mut Waiting,
    ) -> bool {
        // A rejection continues no conversation, so it leaves no link to resolve a later request by.
        if status != 200 {
            return false;
        }
        let mut usage_sent = false;
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
                    classifier: self.classifier,
                    request_id: self.request_id.clone(),
                    attempt: self.attempt,
                    entrypoint: self.entrypoint.clone(),
                },
            );
            usage_sent = true;
        }
        let cached = |field: fn(&Usage) -> Option<u64>| usage.and_then(field).unwrap_or(0);
        let prefix_tokens = cached(|u| u.cache_read_tokens)
            + cached(|u| u.cache_write_5m_tokens)
            + cached(|u| u.cache_write_1h_tokens);
        // The identifier the next turn's wait reads this conversation by.
        let session_id = self.root.clone();
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
                pending.record(
                    &key_id,
                    key_generation,
                    &session_id,
                    seq,
                    router.applied_report(),
                );
            }
        };
        match waiting.try_now(&router, Report::Affinity(report)) {
            Ok(seq) => file(&router, seq),
            Err(report) => waiting.wait(async move {
                let seq = router.report(*report).await;
                file(&router, seq);
            }),
        }
        usage_sent
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
    async fn prefix(&mut self, piece: &Bytes, waiting: &mut Waiting) {
        let Some(held) = self.held.take() else {
            return self.chunk(piece, waiting).await;
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

    /// `piece` is what the caller is sent, the decoded text.
    async fn chunk(&mut self, piece: &Bytes, waiting: &mut Waiting) {
        self.bytes += piece.len() as u64;
        let text = piece.clone();
        if let BodyRead::Json { body, bound } = &mut self.read {
            if body.len() + text.len() > *bound {
                // Past the bound nothing is read; the usage stays unknown rather than half parsed.
                self.read = BodyRead::Unread;
            } else {
                body.extend_from_slice(&text);
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
        let BodyRead::Events(parser) = &mut self.read else {
            return Vec::new();
        };
        parser.feed(text);
        self.parsed(waiting)
    }

    /// The phases of the complete events the parser holds.
    fn parsed(&mut self, waiting: &mut Waiting) -> Vec<Phase> {
        let mut phases = Vec::new();
        loop {
            let event = match &mut self.read {
                BodyRead::Events(parser) => parser.next_event(),
                BodyRead::Json { .. } | BodyRead::Unread => None,
            };
            let Some(event) = event else {
                break;
            };
            phases.extend(self.phase_of(&event, waiting));
        }
        phases
    }

    fn phase_of(&mut self, event: &SseEvent, waiting: &mut Waiting) -> Option<Phase> {
        match event.name.as_deref() {
            Some(b"message_start") => {
                let answer =
                    answer_from_json(&event.data, &["message"], self.outcome.diagnostics_named);
                self.usage.merge(&answer.usage);
                self.response.merge(&answer.response);
                if let Some(id) = answer.message_id {
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
                let answer = answer_from_json(&event.data, &[], self.outcome.diagnostics_named);
                self.usage.merge(&answer.usage);
                self.response.merge(&answer.response);
                self.record_credit(&answer);
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
                self.stream_error.get_or_insert_with(|| {
                    crate::classify::error_type(&event.data).unwrap_or_else(|| "error".into())
                });
                if let Some(on_error) = self.on_error.take() {
                    on_error(event.data.clone(), waiting);
                }
                None
            }
            _ => None,
        }
    }

    /// Records a refusal's fallback credit token under the serving account. A stream's delta is recorded here, before
    /// the piece that carries it reaches the client, so a fallback the client sends while the upstream body is still
    /// open finds the issuer.
    fn record_credit(&self, answer: &Answer) {
        if answer.response.stop_reason.as_deref() != Some("refusal") {
            return;
        }
        if let Some(digest) = answer.credit_digest {
            self.credits.record(
                digest,
                &self.outcome.account_id,
                self.incarnation,
                (self.clock)(),
            );
        }
    }

    /// The answer's end reports go through `waiting`, ahead of the completion. Answers whether the usage went out.
    async fn finish(mut self, end: &BodyEnd, waiting: &mut Waiting) -> bool {
        let (last, (skipped, largest), answer) = match &mut self.read {
            BodyRead::Events(parser) => {
                // A stream cut off after its last event never sent the empty line that would close it, so the
                // event, an error among them, can still be sitting in the parser.
                let last = parser.finish();
                // Read after `finish`, which counts an event the stream ended inside while it was being discarded.
                (last, parser.skipped(), None)
            }
            // Taken, so a copy up to `bound` is freed before the hook and the completion are awaited.
            BodyRead::Json { body, .. } => (
                None,
                (0, 0),
                Some(answer_from_json(
                    &std::mem::take(body),
                    &[],
                    self.outcome.diagnostics_named,
                )),
            ),
            BodyRead::Unread => (None, (0, 0), None),
        };
        if let Some(event) = last {
            if let Some(phase) = self.phase_of(&event, waiting) {
                self.emit(Bytes::new(), Some(phase));
            }
        }
        if skipped > 0 {
            tracing::warn!(
                request_id = %self.trace.request_id,
                skipped,
                largest_bytes = largest,
                bound = INSPECT_BUFFER,
                "stream events past the parser's bound were skipped"
            );
        }
        if let Some(answer) = answer {
            self.usage.merge(&answer.usage);
            self.response.merge(&answer.response);
            // A JSON answer is whole only at the body's end.
            self.record_credit(&answer);
            if let Some(id) = answer.message_id {
                self.saw_message_id(id);
            }
        }
        // The API answering under another snapshot is the alias moving; one refresh brings the listing up to it.
        // A body that failed or was canceled may have named a model from a partial or misleading answer.
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
        // parser saw no stop and no error, which a success count would hide. A body read for no events proves nothing
        // and keeps the head's word. Any other status answered no message to cut short, so it stays `failed`, and a
        // cut or a stall still names its cause.
        let committed = self.status == 200;
        let stream_error = self.stream_error.take();
        // The error's own words for a body the upstream or the account's cancel ended short. A stream that carried an
        // `error` event ended in the upstream's own words, which its code already names.
        let words = match (end, &stream_error) {
            (BodyEnd::Canceled, _) => Some(canceled().to_string()),
            (BodyEnd::Failed { text, .. }, None) => Some(text.clone()),
            _ => None,
        };
        let outcome = match (end, stream_error) {
            (BodyEnd::Complete, None)
                if committed && matches!(self.read, BodyRead::Events(_)) && !self.saw_stop =>
            {
                Some((Outcome::Incomplete, Some(STREAM_INCOMPLETE.into())))
            }
            (BodyEnd::Complete, None) => None,
            (BodyEnd::Complete, Some(error)) => Some((Outcome::Failed, Some(error))),
            (BodyEnd::Canceled, _) => Some((Outcome::Canceled, Some(ACCOUNT_CANCELED_CODE.into()))),
            (BodyEnd::ClientGone, _) => Some((
                Outcome::Canceled,
                Some(cut_code(self.trace.shutdown_mark.as_ref()).into()),
            )),
            (BodyEnd::Failed { .. }, Some(error)) => Some((Outcome::Failed, Some(error))),
            (BodyEnd::Failed { cut, .. }, None) if committed => {
                Some((Outcome::Incomplete, Some(cut.code().into())))
            }
            (BodyEnd::Failed { cut, .. }, None) => Some((Outcome::Failed, Some(cut.code().into()))),
        };
        let interruption = words
            .zip(outcome.as_ref().and_then(|(_, code)| code.clone()))
            .map(|(text, code)| Interruption {
                code,
                text,
                bytes_relayed: self.bytes,
            });
        if let Some(interruption) = &interruption {
            tracing::warn!(
                account = %self.outcome.account_id,
                request_id = %self.trace.request_id,
                upstream_request_id = self
                    .outcome
                    .upstream_request_id
                    .as_deref()
                    .map(tracing::field::display),
                bytes_relayed = interruption.bytes_relayed,
                code = %interruption.code,
                error = %interruption.text,
                "the upstream body ended short after the head"
            );
        }
        let ttfc_ms = self
            .first_content_at
            .map(|at| at - self.outcome.received_at);
        let usage = (!self.usage.is_empty()).then(|| self.usage.clone());
        let fast = self.response.speed.as_deref() == Some("fast");
        if let Some(renewal) = self.renewal.take() {
            renewal.send((self.clock)(), self.status, usage.as_ref(), fast, waiting);
        }
        let usage_sent = match self.affinity.take() {
            Some(affinity) => affinity.send(
                (self.clock)(),
                self.status,
                usage.as_ref(),
                fast,
                self.message_id.clone(),
                waiting,
            ),
            None => false,
        };
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
                skipped,
                interruption,
                (self.clock)(),
            )
            .await;
        usage_sent
    }

    /// The id is handed off once, on first sight: `message_start` names it for a stream, the whole document for a
    /// JSON answer, and neither arrives twice. `chunk` and `finish` await the hook once `phase_of` returns.
    fn saw_message_id(&mut self, id: String) {
        self.message_id.get_or_insert(id);
    }

    /// Awaited from `chunk` and `finish` once `phase_of` has had its turn, so an id from either path fires the
    /// hook the same way. The hook is taken, so it fires once.
    async fn fire_pending_hook(&mut self) {
        let Some(id) = self.message_id.as_ref() else {
            return;
        };
        if let Some(hook) = self.on_message_id.take() {
            hook(id.clone()).await;
        }
    }
}

/// The measured pool a relayed connection came from, so the body task can hand it back.
#[derive(Clone)]
pub(crate) struct Checkin {
    pub pool: Arc<Pool>,
    pub key: PoolKey,
}

impl Checkin {
    pub(crate) fn put(self, conn: Conn) {
        self.pool.checkin(self.key, conn);
    }
}

/// Everything the body task needs once the head has been forwarded.
pub(crate) struct Relayed {
    /// Canceled when the account this body belongs to is disconnected or deleted.
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
    pub reader: BodyReader<Conn>,
}

/// Relays the upstream body as it arrives: one task reads, the response body drains the channel, and the
/// connection goes back to the pool only after a clean end. Spawned under `tracker` rather than bare, so a
/// shutdown can wait for a task a cut connection left running.
pub(crate) fn relay_body(relayed: Relayed, tracker: &TaskTracker) -> Body {
    let (tx, rx) = mpsc::channel::<Result<Bytes, TransportError>>(CHANNEL_DEPTH);
    tracker.spawn(async move {
        let Relayed {
            cancel,
            checkin,
            mut decoder,
            mut guards,
            idle_timeout,
            keep_alive,
            prefix,
            mut publish,
            mut reader,
        } = relayed;
        let mut waiting = Waiting::default();
        let end = 'body: {
            if !prefix.is_empty() {
                let piece = match decode_piece(decoder.as_mut(), prefix) {
                    Ok(piece) => piece,
                    Err(error) => break 'body failed(&tx, &cancel, error).await,
                };
                if let Some(publish) = publish.as_mut() {
                    publish.prefix(&piece, &mut waiting).await;
                }
                if let Some(end) = send(&tx, &cancel, Ok(piece)).await {
                    break 'body end;
                }
            }
            loop {
                // A caller that hangs up mid-stream has to release the upstream connection right away; waiting
                // for the next chunk would hold it until the idle timeout on a stream that has gone quiet.
                let read = tokio::select! {
                    _ = tx.closed() => break 'body BodyEnd::ClientGone,
                    // The credential this stream was opened with is gone, so the body ends in an error and
                    // axum closes the caller's connection rather than letting a dead account keep streaming.
                    _ = cancel.cancelled() => break 'body BodyEnd::Canceled,
                    read = tokio::time::timeout(idle_timeout, reader.next_chunk()) => read,
                };
                let next = match read {
                    Err(_) => {
                        let _ = send(&tx, &cancel, Err(TransportError::Stalled)).await;
                        // In fractional seconds, since a bound under a second would read 0 in whole ones.
                        break 'body BodyEnd::Failed {
                            cut: Cut::Stalled,
                            text: format!("no chunk for {} s", idle_timeout.as_secs_f64()),
                        };
                    }
                    Ok(Err(error)) => break 'body failed(&tx, &cancel, error).await,
                    Ok(Ok(next)) => next,
                };
                let Some(chunk) = next else {
                    break;
                };
                let piece = match decode_piece(decoder.as_mut(), chunk) {
                    Ok(piece) => piece,
                    Err(error) => {
                        break 'body failed(&tx, &cancel, error).await;
                    }
                };
                // Published before the piece goes out, so a late rate-limit error is acted on no later than the
                // client sees it. That holds while the router's mailbox has room; a full one makes the cooldown
                // wait until after the body's end, behind the client's answer.
                publish_piece(&mut publish, &piece, &mut waiting).await;
                if piece.is_empty() {
                    continue;
                }
                if let Some(end) = send(&tx, &cancel, Ok(piece)).await {
                    break 'body end;
                }
            }
            if let Some(mut decoder) = decoder {
                match decoder.finish_bounded(STREAM_DECODED_CHUNK_MAX) {
                    Ok(tail) if !tail.is_empty() => {
                        let tail = Bytes::from(tail);
                        // The tail is the rest of the decoded body, so the chunks a consumer joins are the
                        // whole of what the caller received.
                        publish_piece(&mut publish, &tail, &mut waiting).await;
                        if let Some(end) = send(&tx, &cancel, Ok(tail)).await {
                            break 'body end;
                        }
                    }
                    Err(error) => {
                        break 'body failed(&tx, &cancel, error).await;
                    }
                    Ok(_) => {}
                }
            }
            BodyEnd::Complete
        };
        match end {
            BodyEnd::Canceled => {
                // The upstream socket goes first. A caller that stopped reading leaves the channel full, so
                // waiting for capacity to report the end would keep a dead account's connection open for as
                // long as that caller cares to hold it.
                drop(reader);
                let _ = tx.try_send(Err(canceled()));
            }
            BodyEnd::ClientGone | BodyEnd::Failed { .. } => {}
            // Checking in before the sender drops is what makes the connection available again by the time the
            // caller sees the end of the body, which is the only ordering a sequential caller can observe.
            BodyEnd::Complete => {
                if keep_alive {
                    checkin.put(reader.into_inner());
                }
            }
        }
        let usage_sent = match publish {
            Some(publish) => publish.finish(&end, &mut waiting).await,
            None => false,
        };
        // A committed usage releases the request's booking itself. Any other end names the cause of the end report
        // the guards send when they drop below: a body that ran to its end without a usage is an answer the
        // upstream refused, a cut or a stall is the transport's, and a caller or an account that left is a cancel.
        guards.ended(if usage_sent {
            None
        } else {
            Some(match end {
                BodyEnd::Complete => REFUSED,
                BodyEnd::Failed { .. } => TRANSPORT_ERROR,
                BodyEnd::Canceled | BodyEnd::ClientGone => CANCELED,
            })
        });
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
    Canceled,
    /// The caller is no longer listening.
    ClientGone,
    Complete,
    /// The body ended in an error the caller has already been sent. `cut` is the cause the completion names and
    /// `text` the error's own words, which the warning and the event carry.
    Failed {
        cut: Cut,
        text: String,
    },
}

#[derive(Clone, Copy)]
pub(crate) enum Cut {
    Stalled,
    Transport,
    Undecodable,
}

impl Cut {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Cut::Stalled => STREAM_STALLED,
            Cut::Transport => STREAM_CUT,
            Cut::Undecodable => STREAM_UNDECODABLE,
        }
    }

    /// The relay's own 502 for a response it could not read after the head.
    pub(crate) fn failure(self) -> Failure {
        match self {
            Cut::Stalled => UPSTREAM_STALLED,
            Cut::Transport => UPSTREAM_CUT,
            Cut::Undecodable => UPSTREAM_UNDECODABLE,
        }
    }

    /// The idle bound's stall, a decoder's refusal of the payload, and a cut for anything else, the per-chunk bound
    /// among them, so a streamed body and a buffered one file each error alike.
    pub(crate) fn of(error: &TransportError) -> Cut {
        match error {
            TransportError::Stalled => Cut::Stalled,
            TransportError::Decode(_) => Cut::Undecodable,
            _ => Cut::Transport,
        }
    }
}

/// One piece downstream, raced against cancellation, answering how the body ends when the piece did not go out.
/// Without the race a caller that stops reading with the channel full would pin this task inside `send` and, with
/// it, the upstream connection: cancellation could not be observed until the caller resumed or hung up. A receiver
/// that is gone resolves the send on its own.
async fn send(
    tx: &mpsc::Sender<Result<Bytes, TransportError>>,
    cancel: &CancellationToken,
    item: Result<Bytes, TransportError>,
) -> Option<BodyEnd> {
    match until_canceled(tx.send(item), cancel).await {
        None => Some(BodyEnd::Canceled),
        Some(Ok(())) => None,
        Some(Err(_)) => Some(BodyEnd::ClientGone),
    }
}

/// A failure goes to the caller as the body's last item, and the end names its cause and its words.
async fn failed(
    tx: &mpsc::Sender<Result<Bytes, TransportError>>,
    cancel: &CancellationToken,
    error: TransportError,
) -> BodyEnd {
    let cut = Cut::of(&error);
    let text = error.to_string();
    let _ = send(tx, cancel, Err(error)).await;
    BodyEnd::Failed { cut, text }
}

/// Hands a forwarded piece, the decoded text the caller is sent, to the bus.
async fn publish_piece(publish: &mut Option<Publish>, piece: &Bytes, waiting: &mut Waiting) {
    if let Some(publish) = publish.as_mut() {
        publish.chunk(piece, waiting).await;
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
            .map_err(|_| TransportError::Stalled)??;
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

fn canceled() -> TransportError {
    TransportError::Io(io::Error::new(
        io::ErrorKind::ConnectionAborted,
        "the bound account was disconnected",
    ))
}
