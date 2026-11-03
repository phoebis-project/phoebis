//! Turns the event sequence of one request into its ledger rows. Events arrive in publish order per request
//! but interleaved across requests, so each request's partial rows wait in a map keyed by id until its
//! completion, and a request that goes silent is aged out rather than kept forever.

use std::collections::HashMap;

use bytes::Bytes;
use phoebis_bus::{
    user_agent_of, AccountSelected, AutoMode, Event, Outcome, Phase, RequestComplete,
    RequestObserved, ResponseChunk, ResponseHead, BODY_SHAPE_ERROR_CODE,
};
use phoebis_oauth::Clock;
use phoebis_relay::UnifiedHeaders;
use phoebis_routing::{LoadCounts, MODEL_REJECTED_REASON};
use phoebis_store::AccountKind;
use serde_json::json;

use crate::bodies::MAX_CAPTURED_BODY;
use crate::query::{LEDGER_GAP, MODEL_PERMITTED, MODEL_REJECTED, STREAM_INTERRUPTED};
use crate::quota::samples_from_unified;
use crate::record::{
    AttemptRecord, BodyRecord, CacheMapWrite, DecisionRow, EventRecord, ExtraUsageSampleRecord,
    GuardrailMatchRecord, KeepaliveDelta, QuotaSampleRecord, RequestRecord,
};

/// A completion is published on every path, the handler's drop guard included, so an entry silent this long belongs
/// to a stream the relay itself ended at its idle timeout. The router asserts this exceeds that timeout.
pub const IN_FLIGHT_TTL_MS: i64 = 15 * 60 * 1_000;

/// Four times the proxy's own connection cap, so the cap is reached only by a flood the listener never admits
/// and an eviction stays the last resort rather than a policy. An entry without bodies is small, so this
/// bounds memory at a few tens of megabytes.
pub const MAX_IN_FLIGHT: usize = 16_384;

/// Charged at observation for a capturing key's request body and per response chunk kept; past it the largest
/// capture's bodies are dropped so every request still lands its row.
pub const MAX_CAPTURE_BYTES: usize = 64 << 20;

// Boxing the request would put an allocation on the path every request takes, to shrink a value that is
// built once per completion and moved once into the writer's bounded channel.
#[allow(clippy::large_enum_variant)]
pub enum Record {
    Event(EventRecord),
    Keepalive(KeepaliveDelta),
    Request {
        request: RequestRecord,
        attempts: Vec<AttemptRecord>,
        bodies: Option<BodyRecord>,
    },
    Samples {
        quota: Vec<QuotaSampleRecord>,
        extra: Option<ExtraUsageSampleRecord>,
        /// True from the poll loop, false from the head path: a poll's samples settle which shared windows the account
        /// has, so a window it lacks loses its gauge series.
        poll: bool,
    },
    Decision(Box<DecisionRow>),
    CacheMap(CacheMapWrite),
    /// The start row: the writer fills `down_since` from the newest row it holds when it lands this.
    RouterStarted {
        version: String,
        counts: LoadCounts,
        at: i64,
    },
}

/// What `expire` aged out: the request ids whose completion never came, each of which the caller files as a gap.
pub struct Expired {
    pub ids: Vec<String>,
}

pub(crate) struct InFlight {
    pub observed: RequestObserved,
    pub attempts: Vec<AttemptRecord>,
    /// The response text assembled from chunks while the request's key captures bodies.
    pub response_body: Option<Vec<u8>>,
    /// Every chunk's length whether or not it was kept, so a cut capture can say how much it dropped.
    pub response_len_seen: u64,
    /// The chunk index expected next; a chunk past it means one was dropped at the bus, and the completion's
    /// own count catches the case where the dropped chunk was the last one and nothing later reveals it.
    pub next_index: u32,
    /// Set once an interior chunk was skipped: the capture is truncated from that point on.
    pub gapped: bool,
    /// The last moment anything about this request arrived.
    pub seen_at: i64,
    /// The request body's length, kept when the body itself is cleared because the key does not capture or
    /// the capture budget dropped it.
    pub request_len: u64,
    /// Insertion order, used only to break a tie between entries an eviction could otherwise pick between at
    /// random: a real clock rarely ties, but a flood arriving in the same millisecond must still evict the
    /// same entry every time.
    pub seq: u64,
    /// Every account whose head rejected this request's model, in the order the heads arrived: what a later
    /// success's `model_permitted` event names as who was skipped first.
    pub rejected_by: Vec<String>,
    /// The ledger task's clock at the answer's `message_stop` chunk. The first such chunk wins.
    pub stream_end_at: Option<i64>,
}

pub struct Assembler {
    clock: Clock,
    in_flight: HashMap<String, InFlight>,
    next_seq: u64,
    /// The request and response bytes currently held for capturing entries; charged at observation and per
    /// chunk kept, checked against `MAX_CAPTURE_BYTES` after each charge.
    captured_bytes: usize,
}

impl Assembler {
    pub fn new(clock: Clock) -> Assembler {
        Assembler {
            clock,
            in_flight: HashMap::new(),
            next_seq: 0,
            captured_bytes: 0,
        }
    }

    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }

    #[cfg(test)]
    pub(crate) fn retained_request_bytes(&self) -> u64 {
        self.in_flight
            .values()
            .map(|e| e.observed.body.len() as u64)
            .sum()
    }

    #[cfg(test)]
    pub(crate) fn retained_capture_bytes(&self) -> usize {
        self.captured_bytes
    }

    /// Frees the largest capturing entry's bodies until the charge fits the budget, the oldest among equals:
    /// the row still lands for every request, only the bytes this eviction names go.
    fn enforce_capture_budget(&mut self, now: i64) -> Option<Record> {
        if self.captured_bytes <= MAX_CAPTURE_BYTES {
            return None;
        }
        let (victim, freed) = self
            .in_flight
            .iter()
            .filter(|(_, e)| e.observed.body_capture_hours > 0)
            .map(|(id, e)| (id.clone(), capture_size(e), e.seq))
            .max_by_key(|(_, size, seq)| (*size, std::cmp::Reverse(*seq)))
            .map(|(id, size, _)| (id, size))?;
        if let Some(entry) = self.in_flight.get_mut(&victim) {
            entry.observed.body = Bytes::new();
            entry.response_body = None;
        }
        self.captured_bytes = self.captured_bytes.saturating_sub(freed);
        Some(gap(&victim, "capture_budget", now))
    }

    /// Removes an entry and releases its share of the capture budget with it. Every way an entry leaves (its
    /// completion, its expiry, the entry cap) goes through here, since the budget has no other hook into a
    /// removal and a charge left behind would count against every capture after it until a restart.
    fn remove(&mut self, id: &str) -> Option<InFlight> {
        let entry = self.in_flight.remove(id)?;
        if entry.observed.body_capture_hours > 0 {
            self.captured_bytes = self.captured_bytes.saturating_sub(capture_size(&entry));
        }
        Some(entry)
    }

    pub fn observe(&mut self, event: Event) -> Vec<Record> {
        let now = (self.clock)();
        match event {
            Event::RequestObserved(mut observed) => {
                let request_len = observed.body.len() as u64;
                let capturing = observed.body_capture_hours > 0;
                // Nothing will read it: a captured body is the one reason to hold a request's bytes past its
                // observation.
                if !capturing {
                    observed.body = Bytes::new();
                }
                let seq = self.next_seq;
                self.next_seq += 1;
                let mut records = Vec::new();
                if self.in_flight.len() >= MAX_IN_FLIGHT {
                    if let Some(victim) = self
                        .in_flight
                        .iter()
                        .min_by_key(|(_, e)| (e.seen_at, e.seq))
                        .map(|(id, _)| id.clone())
                    {
                        self.remove(&victim);
                        records.push(gap(&victim, "in_flight_cap", now));
                    }
                }
                if capturing {
                    self.captured_bytes += request_len as usize;
                }
                let response_body = capturing.then(Vec::new);
                self.in_flight.insert(
                    observed.request_id.clone(),
                    InFlight {
                        observed,
                        attempts: Vec::new(),
                        response_body,
                        response_len_seen: 0,
                        next_index: 0,
                        gapped: false,
                        seen_at: now,
                        request_len,
                        seq,
                        rejected_by: Vec::new(),
                        stream_end_at: None,
                    },
                );
                if capturing {
                    records.extend(self.enforce_capture_budget(now));
                }
                records
            }
            Event::AccountSelected(selected) => {
                let Some(entry) = self.in_flight.get_mut(&selected.request_id) else {
                    return Vec::new();
                };
                entry.seen_at = now;
                if let Some(previous) = entry.attempts.last_mut() {
                    previous.finished_at = Some(selected.started_at);
                }
                entry.attempts.push(attempt_row(&selected));
                Vec::new()
            }
            Event::ResponseHead(head) => {
                let Some(entry) = self.in_flight.get_mut(&head.request_id) else {
                    return Vec::new();
                };
                entry.seen_at = now;
                let key_id = entry.observed.key_id.clone();
                let session_id = entry.observed.session_id.clone();
                let canonical_model = entry.observed.canonical_model.clone();
                let wire_model = entry.observed.wire_model.clone();
                let source_request_id = entry.observed.request_id.clone();
                let Some(attempt) = entry
                    .attempts
                    .iter_mut()
                    .find(|a| a.attempt as u8 == head.attempt)
                else {
                    return Vec::new();
                };
                let parsed = fill_head(attempt, &head);
                let mut records = Vec::new();
                if head
                    .cooldown
                    .as_ref()
                    .is_some_and(|c| c.reason == MODEL_REJECTED_REASON)
                {
                    entry.rejected_by.push(attempt.account_id.clone());
                    records.push(Record::Event(EventRecord {
                        at: head.observed_at,
                        kind: MODEL_REJECTED.into(),
                        account_id: Some(attempt.account_id.clone()),
                        key_id,
                        session_id,
                        model: Some(canonical_model),
                        detail: Some(
                            json!({
                                "attempt": head.attempt,
                                "request_id": attempt.upstream_request_id,
                                "source_request_id": source_request_id,
                                "wire_model": wire_model,
                            })
                            .to_string(),
                        ),
                        request_id: Some(source_request_id),
                        ..Default::default()
                    }));
                }
                if attempt.account_kind == AccountKind::Oauth {
                    if let Some(unified) = parsed {
                        let (quota, extra) = samples_from_unified(
                            &attempt.account_id,
                            &unified,
                            head.observed_at,
                            attempt.usage_based,
                        );
                        if !quota.is_empty() || extra.is_some() {
                            records.push(Record::Samples {
                                quota,
                                extra,
                                poll: false,
                            });
                        }
                    }
                }
                records
            }
            Event::ResponseChunk(chunk) => self.chunk(&chunk),
            Event::RequestComplete(complete) => match self.remove(&complete.request_id) {
                Some(entry) => finish(entry, &complete, now),
                None => Vec::new(),
            },
        }
    }

    /// Every chunk is activity and carries its index; the first skipped index truncates the capture from
    /// there on and is reported once. What was captured before the gap is kept, not cleared: a truncated
    /// capture is stored truncated rather than dropped.
    pub fn chunk(&mut self, chunk: &ResponseChunk) -> Vec<Record> {
        let now = (self.clock)();
        let Some(entry) = self.in_flight.get_mut(&chunk.request_id) else {
            return Vec::new();
        };
        entry.seen_at = now;
        // The chunk carries no instant of its own, so the ledger's clock stands in and `finish` holds it to the
        // finish.
        if chunk.phase == Some(Phase::MessageStop) {
            entry.stream_end_at.get_or_insert(now);
        }
        let mut records = Vec::new();
        if chunk.index != entry.next_index && !entry.gapped {
            entry.gapped = true;
            records.push(gap(&chunk.request_id, "chunk_skipped", now));
        }
        entry.next_index = chunk.index.saturating_add(1);
        entry.response_len_seen += chunk.body.len() as u64;
        let mut kept = 0;
        if !entry.gapped {
            if let Some(body) = entry.response_body.as_mut() {
                let room = MAX_CAPTURED_BODY.saturating_sub(body.len());
                kept = chunk.body.len().min(room);
                body.extend_from_slice(&chunk.body[..kept]);
            }
        }
        if kept > 0 {
            self.captured_bytes += kept;
            records.extend(self.enforce_capture_budget(now));
        }
        records
    }

    pub fn expire(&mut self, now: i64) -> Expired {
        let stale: Vec<String> = self
            .in_flight
            .iter()
            .filter(|(_, entry)| now - entry.seen_at >= IN_FLIGHT_TTL_MS)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &stale {
            self.remove(id);
        }
        if !stale.is_empty() {
            tracing::warn!(
                dropped = stale.len(),
                "requests whose completion never reached the ledger were dropped"
            );
        }
        Expired { ids: stale }
    }
}

/// What an entry currently charges to the capture budget: its retained request body plus whatever of its
/// response has been captured so far. The one place the budget eviction and a removal read this from, so bytes
/// released by either always leave the same accounting behind.
fn capture_size(entry: &InFlight) -> usize {
    entry.observed.body.len() + entry.response_body.as_ref().map_or(0, Vec::len)
}

/// A gap inside one request, as opposed to the writer's gap over dropped records: the row that follows is
/// incomplete in a way the reader can see.
pub(crate) fn gap(request_id: &str, reason: &str, at: i64) -> Record {
    Record::Event(EventRecord {
        at,
        kind: LEDGER_GAP.into(),
        detail: Some(json!({ "request_id": request_id, "reason": reason }).to_string()),
        request_id: Some(request_id.into()),
        ..Default::default()
    })
}

fn attempt_row(selected: &AccountSelected) -> AttemptRecord {
    AttemptRecord {
        request_id: selected.request_id.clone(),
        attempt: selected.attempt as u32,
        account_id: selected.account_id.clone(),
        account_kind: AccountKind::parse(&selected.account_kind).unwrap_or_default(),
        usage_based: selected.usage_based,
        pin_epoch: selected.pin_epoch,
        started_at: selected.started_at,
        outcome: "transport_error".into(),
        selection: selected.selection.clone(),
        ..AttemptRecord::default()
    }
}

/// Fills the attempt row's rate-limit fields and hands back the parse it just ran, so the quota sampler
/// downstream reads the same `UnifiedHeaders` instead of parsing the headers again.
fn fill_head(attempt: &mut AttemptRecord, head: &ResponseHead) -> Option<UnifiedHeaders> {
    attempt.upstream_status = Some(head.status);
    attempt.ttfb_ms = Some(head.observed_at - attempt.started_at);
    attempt.written_at = Some(head.written_at);
    attempt.outcome = head.outcome.as_str().to_string();
    attempt.upstream_request_id = head
        .headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("request-id"))
        .map(|(_, v)| v.clone());
    if let Some(cooldown) = &head.cooldown {
        attempt.cooldown_scope = Some(cooldown.scope.clone());
        attempt.cooldown_until = Some(cooldown.until);
    }
    let unified = UnifiedHeaders::parse_list(&head.headers)?;
    attempt.ratelimit_5h_utilization = unified.five_hour_utilization;
    attempt.ratelimit_5h_reset = unified.five_hour_reset;
    attempt.ratelimit_7d_utilization = unified.seven_day_utilization;
    attempt.ratelimit_7d_reset = unified.seven_day_reset;
    attempt.ratelimit_7d_oi_reset = unified.seven_day_oi_reset;
    attempt.ratelimit_7d_oi_utilization = unified.seven_day_oi_utilization;
    attempt.overage_in_use = unified.overage_in_use;
    attempt.overage_utilization = unified.overage_utilization;
    Some(unified)
}

/// The attempt outcome a request's terminal cause implies for the attempt that served it, once the head's
/// own classification is known to have been overtaken; an `incomplete` body is the attempt's failure as a failed
/// body is.
fn terminal_outcome(complete: &RequestComplete) -> Option<&'static str> {
    match complete.outcome {
        Outcome::Canceled => Some("canceled"),
        Outcome::Failed if complete.error_code.as_deref() == Some("rate_limit_error") => {
            Some("rate_limited")
        }
        Outcome::Failed => Some("upstream_error"),
        Outcome::Incomplete => Some("upstream_error"),
        _ => None,
    }
}

fn finish(entry: InFlight, complete: &RequestComplete, now: i64) -> Vec<Record> {
    let InFlight {
        observed,
        mut attempts,
        response_body,
        response_len_seen,
        next_index,
        gapped,
        request_len,
        rejected_by,
        stream_end_at,
        ..
    } = entry;
    let mut records = Vec::new();
    // The serving attempt is the one the completion names, by number and account; a trail the bus lost a
    // selection from must not put the serving account's usage on another account's row.
    let serving = attempts.iter().position(|a| {
        a.attempt as u8 == complete.attempt
            && complete
                .account_id
                .as_deref()
                .is_none_or(|id| id == a.account_id)
    });
    // Every attempt the completion counts has to be on record, not just the one that served: a selection
    // the bus lost earlier in the trail leaves the row short an attempt and a failover with nothing to say
    // that the trail is partial.
    let trail_complete = complete.attempt == 0
        || (serving.is_some()
            && (1..=complete.attempt).all(|n| attempts.iter().any(|a| a.attempt as u8 == n)));
    if !trail_complete {
        records.push(gap(&observed.request_id, "attempt_missing", now));
    }
    if let Some(i) = serving {
        let last = &mut attempts[i];
        last.finished_at = Some(complete.finished_at);
        // The completion measures its first content from admission, every attempt field from the attempt's
        // own start: the instant is re-based here, and one that predates this attempt belonged to another.
        last.ttfc_ms = complete
            .ttfc_ms
            .map(|ttfc| observed.received_at + ttfc - last.started_at)
            .filter(|ttfc| *ttfc >= 0);
        if let Some(usage) = &complete.usage {
            last.input_tokens = usage.input_tokens;
            last.output_tokens = usage.output_tokens;
            last.cache_read_tokens = usage.cache_read_tokens;
            last.cache_write_5m_tokens = usage.cache_write_5m_tokens;
            last.cache_write_1h_tokens = usage.cache_write_1h_tokens;
            last.service_tier = usage.service_tier.clone();
            last.cache_miss_reason = usage.cache_miss_reason.clone();
            last.thinking_tokens = usage.thinking_tokens;
            last.web_search_requests = usage.web_search_requests;
            last.web_fetch_requests = usage.web_fetch_requests;
            // The trace crossed the bus at selection, before the response reported what it cached; folded in
            // here so the request drawer reads one object rather than joining the attempt's own token column.
            // `as_object_mut` rather than indexing: the relay always serializes an object, but a stored trace
            // that somehow did not must not panic the ledger task over one field it cannot add.
            if let (Some(read), Some(trace)) = (usage.cache_read_tokens, &last.selection) {
                if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(trace) {
                    if let Some(obj) = value.as_object_mut() {
                        obj.insert("cache_read".into(), json!(read));
                        last.selection = Some(value.to_string());
                    }
                }
            }
        }
        let response = &complete.response;
        last.stop_reason = response.stop_reason.clone();
        last.stop_sequence = response.stop_sequence.clone();
        // The answer's model is worth a column only when it is not the one the router resolved: a fallback
        // served the request, or the upstream spelled the id its own way.
        last.served_model = response
            .model
            .clone()
            .filter(|served| *served != observed.canonical_model && *served != observed.model);
        last.speed = response.speed.clone();
        last.inference_geo = response.inference_geo.clone();
        last.cleared_input_tokens = response.cleared_input_tokens;
        last.refusal_category = response.refusal_category.clone();
        last.refusal_recommended_model = response.refusal_recommended_model.clone();
        // No transport failed a headless attempt that the account's removal, the client's leaving or the router's
        // shutdown cut short, so it files as canceled over its row's `transport_error` default.
        if last.upstream_status.is_none()
            && matches!(
                complete.error_code.as_deref(),
                Some("account_canceled" | "client_gone" | "router_shutdown")
            )
        {
            last.outcome = "canceled".into();
        }
        // Only the committed answer has a message; an earlier attempt of the request never carries one.
        if last.upstream_status.is_some() {
            last.upstream_message_id = complete.message_id.clone();
        }
        // A head classified as succeeded whose body then failed: the request's terminal cause wins for this
        // attempt alone; a failover classification on an earlier attempt is left as it was.
        if last.upstream_status.is_some() && last.outcome == "succeeded" {
            if let Some(outcome) = terminal_outcome(complete) {
                last.outcome = outcome.into();
            }
        }
        if last.outcome == "succeeded" && !rejected_by.is_empty() {
            records.push(Record::Event(EventRecord {
                at: complete.finished_at,
                kind: MODEL_PERMITTED.into(),
                account_id: Some(last.account_id.clone()),
                key_id: observed.key_id.clone(),
                session_id: observed.session_id.clone(),
                model: Some(observed.canonical_model.clone()),
                detail: Some(
                    json!({
                        "attempt": complete.attempt,
                        "rejected_by": rejected_by,
                        "request_id": last.upstream_request_id,
                        "source_request_id": observed.request_id,
                        "wire_model": observed.wire_model,
                    })
                    .to_string(),
                ),
                request_id: Some(observed.request_id.clone()),
                ..Default::default()
            }));
        }
    }
    // Apart from the serving block: a completion whose trail lost its selection still lands its cut, under the
    // completion's account and with no upstream request id.
    if let Some(interruption) = &complete.interruption {
        let serving = serving.map(|i| &attempts[i]);
        records.push(Record::Event(EventRecord {
            at: complete.finished_at,
            kind: STREAM_INTERRUPTED.into(),
            account_id: serving
                .map(|a| a.account_id.clone())
                .or_else(|| complete.account_id.clone()),
            key_id: observed.key_id.clone(),
            session_id: observed.session_id.clone(),
            model: Some(observed.canonical_model.clone()),
            detail: Some(
                json!({
                    "code": interruption.code,
                    "text": interruption.text,
                    "bytes_relayed": interruption.bytes_relayed,
                    "upstream_request_id": serving.and_then(|a| a.upstream_request_id.as_deref()),
                    "attempt": complete.attempt,
                })
                .to_string(),
            ),
            request_id: Some(observed.request_id.clone()),
            ..Default::default()
        }));
    }
    for attempt in &mut attempts {
        attempt.client_drift = complete.client_drift.clone();
    }
    // Completeness is the terminal chunk count alone: a chunk can carry a phase and no bytes, so a dropped one
    // need not shorten any length the count could be compared with.
    let short = complete
        .response_chunks
        .is_some_and(|published| next_index < published);
    if short && !gapped {
        records.push(gap(&observed.request_id, "chunk_dropped", now));
    }
    // A gapped or short capture is stored truncated rather than dropped: the bytes seen before the gap are
    // still worth reading, and the row says so via `response_truncated`.
    let bodies = response_body.map(|assembled| BodyRecord {
        request_id: observed.request_id.clone(),
        request_body: observed.body.to_vec(),
        response_body: (!assembled.is_empty()).then_some(assembled),
        // The request is never cut: it is the bus's whole `Bytes`, read once at observation.
        request_len_seen: request_len,
        response_len_seen,
        expires_at: complete.finished_at + observed.body_capture_hours as i64 * 3_600_000,
    });
    // An incomplete trail keeps the completion's attempt number, so a refusal after an attempt whose selection the
    // bus lost keeps its count and stays a rejected request.
    let attempt_count = if trail_complete {
        attempts.len() as u32
    } else {
        complete.attempt as u32
    };
    let request = RequestRecord {
        id: observed.request_id.clone(),
        received_at: observed.received_at,
        finished_at: Some(complete.finished_at),
        key_id: observed.key_id.clone(),
        key_generation: observed.key_generation,
        pool_id: observed.pool_id.clone(),
        session_id: observed.session_id.clone(),
        agent_id: observed.agent_id.clone(),
        parent_agent_id: observed.parent_agent_id.clone(),
        model: observed.model.clone(),
        canonical_model: observed.canonical_model.clone(),
        stream: observed.stream,
        kind: observed.kind.as_str().into(),
        origin: observed.origin.as_str().into(),
        status: (complete.status != 0).then_some(complete.status),
        outcome: complete.outcome.as_str().into(),
        attempts: attempt_count,
        request_bytes: Some(request_len),
        response_bytes: complete.response_bytes,
        ttfb_ms: complete.ttfb_ms,
        ttfc_ms: complete.ttfc_ms,
        duration_ms: Some(complete.finished_at - observed.received_at),
        // A ledger task behind the bus reads the message_stop chunk after the relay finished the request, and a
        // stream end drawn after the finish would be a wrong figure.
        stream_end_at: stream_end_at.map(|at| at.min(complete.finished_at)),
        error_code: complete.error_code.clone(),
        // Set here rather than only in the attempt loop above: a drift or a contract refusal answers before any
        // account is selected, so it has no attempt row to carry the kind or the item, and the request row is the one
        // place left.
        client_drift: complete.client_drift.clone(),
        client_contract: complete.client_contract.clone(),
        kept_warm: false,
        bodies_captured: bodies.is_some(),
        request_truncated: false,
        // A gap or a short trail only means something for a key that captures: a non-capturing key
        // never had bytes to lose, so it must not read truncated when the bus merely dropped a chunk.
        response_truncated: bodies.as_ref().is_some_and(|b| {
            gapped
                || short
                || b.response_body
                    .as_ref()
                    .is_some_and(|body| (body.len() as u64) < b.response_len_seen)
        }),
        user_agent: user_agent_of(&observed.headers),
        effort: observed.shape.effort.clone(),
        thinking_type: observed.shape.thinking_type.clone(),
        thinking_budget: observed.shape.thinking_budget,
        max_tokens: observed.shape.max_tokens,
        tool_count: observed.shape.tool_count,
        system_blocks: observed.shape.system_blocks,
        message_count: observed.shape.message_count,
        context_edits: observed.shape.context_edits.join(","),
        temperature: observed.shape.temperature,
        requested_speed: observed.shape.speed.clone(),
        requested_service_tier: observed.shape.service_tier.clone(),
        presented_retired: observed.presented_retired,
        rotation_at: observed.rotation_at,
        client: Some(observed.client.class.clone()),
        agent_type: observed.client.agent_type.clone(),
        compaction: observed.client.compaction.clone(),
        context_compacted: observed.client.context_compacted.clone(),
        prev_tool_ms: observed.client.prev_tool_ms,
        turn_origin: observed.client.turn_origin.clone(),
        request_class: observed.client.request_class.clone(),
        classifier: observed.client.classifier.map(|c| c.as_str().to_string()),
        forked_from: observed.fork.forked_from.clone(),
        fork_depth: observed.fork.fork_depth,
        // A body refused for its shape before any dispatch never reached an upstream, whatever admission read of
        // its member; the standing says so beside the outcome's code. A failover leg refused for its shape comes
        // after a leg that did reach one, so that row keeps what admission read.
        auto_mode: if complete.outcome == Outcome::Rejected
            && complete.error_code.as_deref() == Some(BODY_SHAPE_ERROR_CODE)
        {
            Some(AutoMode::BodyShape.as_str().to_string())
        } else {
            observed.auto_mode.map(|m| m.as_str().to_string())
        },
        held: complete.held.clone(),
        diagnostics_source: complete.diagnostics.map(|s| s.as_str().to_string()),
        guardrail_id: observed.guardrail_id.clone(),
        guardrails: observed
            .guardrails
            .iter()
            .map(|m| GuardrailMatchRecord {
                guardrail_id: m.id.clone(),
                position: m.position,
                blocked: m.blocked,
            })
            .collect(),
        // The ledger's reading of the row's own facts, as the drift and the contract members above are: the relay's
        // outcome and the bus's completion carry no flag.
        refused: crate::refusals::is_refusal(
            observed.kind.as_str(),
            complete.outcome.as_str(),
            complete.error_code.as_deref(),
            attempt_count,
        ),
    };
    records.push(Record::Request {
        request,
        attempts,
        bodies,
    });
    records
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use phoebis_bus::*;
    use std::sync::Arc;

    fn clock() -> Clock {
        Arc::new(|| 5_000)
    }

    fn clock_at(ms: i64) -> Clock {
        Arc::new(move || ms)
    }

    fn observed(id: &str, key: Option<&str>) -> Event {
        observed_capturing(id, key, 0)
    }

    /// A non-capturing observation carrying a request body of the given length, for a test that checks the
    /// body's length is counted without being retained.
    fn observed_with_body(id: &str, capture_hours: u32, body: Vec<u8>) -> Event {
        let Event::RequestObserved(mut observed) = observed_capturing(id, Some("k"), capture_hours)
        else {
            unreachable!()
        };
        observed.body = Bytes::from(body);
        Event::RequestObserved(observed)
    }

    /// A capturing observation carrying the given request body, for a test that drives the capture budget.
    fn observed_capturing_with_body(id: &str, body: Vec<u8>) -> Event {
        observed_with_body(id, 1, body)
    }
    fn observed_capturing(id: &str, key: Option<&str>, capture_hours: u32) -> Event {
        Event::RequestObserved(RequestObserved {
            request_id: id.into(),
            origin: Origin::Client,
            kind: Kind::Inference,
            key_id: key.map(String::from),
            key_generation: 0,
            pool_id: key.map(|_| "main".into()),
            session_id: Some("s1".into()),
            agent_id: Some("main".into()),
            parent_agent_id: None,
            owner_id: None,
            model: "claude-sonnet-5[1m]".into(),
            canonical_model: "claude-sonnet-5".into(),
            wire_model: "claude-sonnet-5".into(),
            stream: true,
            headers: vec![],
            body: Bytes::from_static(b"0123456789"),
            body_capture_hours: capture_hours,
            shape: RequestShape::default(),
            static_hash: None,
            boundaries: Vec::new(),
            client: ClientFacts::default(),
            auto_mode: None,
            fork: Fork::default(),
            presented_retired: false,
            rotation_at: None,
            guardrail_id: None,
            guardrails: Vec::new(),
            received_at: 1_000,
        })
    }
    fn chunk(id: &str, index: u32) -> Event {
        Event::ResponseChunk(ResponseChunk {
            request_id: id.into(),
            attempt: 1,
            index,
            body: Bytes::from_static(b"x"),
            phase: None,
        })
    }
    /// A chunk of the given body, for a test where the bytes captured matter rather than just the index.
    fn chunk_bytes(id: &str, index: u32, body: &[u8]) -> ResponseChunk {
        ResponseChunk {
            request_id: id.into(),
            attempt: 1,
            index,
            body: Bytes::from(body.to_vec()),
            phase: None,
        }
    }
    fn chunk_sized(id: &str, index: u32, len: usize) -> Event {
        Event::ResponseChunk(ResponseChunk {
            request_id: id.into(),
            attempt: 1,
            index,
            body: Bytes::from(vec![b'a'; len]),
            phase: None,
        })
    }
    fn selected(id: &str, n: u8, account: &str, at: i64) -> Event {
        Event::AccountSelected(AccountSelected {
            request_id: id.into(),
            attempt: n,
            account_id: account.into(),
            account_kind: "oauth".into(),
            usage_based: false,
            pin_epoch: Some(4),
            started_at: at,
            cache_ttl_ms: None,
            selection: None,
        })
    }
    fn head(id: &str, n: u8, status: u16, outcome: AttemptOutcome, at: i64) -> Event {
        Event::ResponseHead(ResponseHead {
            request_id: id.into(),
            attempt: n,
            status,
            headers: vec![
                ("request-id".into(), format!("req_up_{n}")),
                (
                    "anthropic-ratelimit-unified-5h-utilization".into(),
                    "0.42".into(),
                ),
                ("anthropic-ratelimit-unified-5h-reset".into(), "2000".into()),
                (
                    "anthropic-ratelimit-unified-overage-in-use".into(),
                    "true".into(),
                ),
                (
                    "anthropic-ratelimit-unified-overage-utilization".into(),
                    "0.1".into(),
                ),
            ],
            first_event: None,
            outcome,
            cooldown: (status == 429).then(|| CooldownSet {
                scope: "account".into(),
                until: 9_000,
                reason: "rate_limited".into(),
                window: "five_hour".into(),
            }),
            written_at: at - 10,
            observed_at: at,
        })
    }
    /// `head`'s sibling for a rejection: a 404 whose cooldown carries the `model_rejected` reason, so the
    /// rejection and permit tests exercise the assembler's real cooldown-reading path rather than the 429
    /// shape `head` builds.
    fn rejection_head(id: &str, n: u8, at: i64) -> Event {
        Event::ResponseHead(ResponseHead {
            request_id: id.into(),
            attempt: n,
            status: 404,
            headers: vec![("request-id".into(), format!("req_up_{n}"))],
            first_event: None,
            outcome: AttemptOutcome::UpstreamError,
            cooldown: Some(CooldownSet {
                scope: "model:claude-sonnet-5".into(),
                until: at + 3_601_000,
                reason: "model_rejected".into(),
                window: "model_rejection".into(),
            }),
            written_at: at - 10,
            observed_at: at,
        })
    }

    fn complete(
        id: &str,
        n: u8,
        account: Option<&str>,
        outcome: Outcome,
        status: u16,
        code: Option<&str>,
    ) -> Event {
        Event::RequestComplete(RequestComplete {
            request_id: id.into(),
            attempt: n,
            account_id: account.map(String::from),
            outcome,
            status,
            usage: Some(Usage {
                input_tokens: Some(5),
                output_tokens: Some(7),
                cache_read_tokens: Some(2),
                cache_miss_reason: Some("none".into()),
                ..Default::default()
            }),
            response: ResponseFacts::default(),
            ttfb_ms: Some(150),
            ttfc_ms: Some(200),
            error_code: code.map(String::from),
            client_drift: Some("version".into()),
            client_contract: None,
            client_version: Some("2.1.300".into()),
            response_bytes: Some(20),
            response_chunks: None,
            held: None,
            diagnostics: None,
            message_id: None,
            skipped_events: 0,
            interruption: None,
            finished_at: 1_500,
        })
    }

    /// `complete`'s answer from account `a`, carrying who put the diagnostics member on its body.
    fn complete_with_source(id: &str, diagnostics: Option<DiagnosticsSource>) -> Event {
        let Event::RequestComplete(mut done) =
            complete(id, 1, Some("a"), Outcome::Succeeded, 200, None)
        else {
            unreachable!()
        };
        done.diagnostics = diagnostics;
        Event::RequestComplete(done)
    }

    /// A completion carrying only the two fields a capture-completeness test cares about; named apart from
    /// `complete` so the two do not collide.
    fn completion(
        id: &str,
        response_chunks: Option<u32>,
        response_bytes: Option<u64>,
    ) -> RequestComplete {
        RequestComplete {
            request_id: id.into(),
            attempt: 0,
            account_id: None,
            outcome: Outcome::Succeeded,
            status: 200,
            usage: None,
            response: ResponseFacts::default(),
            ttfb_ms: None,
            ttfc_ms: None,
            error_code: None,
            client_drift: None,
            client_contract: None,
            client_version: None,
            response_bytes,
            response_chunks,
            held: None,
            diagnostics: None,
            message_id: None,
            skipped_events: 0,
            interruption: None,
            finished_at: 1_500,
        }
    }

    /// The one `Record::Request` among `records`, split into its row and its bodies.
    fn request_and_bodies(records: &[Record]) -> (&RequestRecord, Option<&BodyRecord>) {
        records
            .iter()
            .find_map(|r| match r {
                Record::Request {
                    request, bodies, ..
                } => Some((request, bodies.as_ref())),
                _ => None,
            })
            .expect("a request record")
    }

    fn request_of(records: &[Record]) -> &RequestRecord {
        request_and_bodies(records).0
    }

    /// The `(request_id, reason)` a `ledger_gap` event names, for a test that does not care about the rest of
    /// the detail.
    fn gap_detail(record: &Record) -> Option<(String, String)> {
        let Record::Event(event) = record else {
            return None;
        };
        if event.kind != "ledger_gap" {
            return None;
        }
        let detail: serde_json::Value = serde_json::from_str(event.detail.as_deref()?).ok()?;
        Some((
            detail["request_id"].as_str()?.to_string(),
            detail["reason"].as_str()?.to_string(),
        ))
    }

    #[test]
    fn a_non_finite_overage_utilization_header_lands_no_reading_on_the_attempt() {
        let mut a = Assembler::new(clock());
        a.observe(observed("r1", Some("k")));
        a.observe(selected("r1", 1, "a", 1_010));
        a.observe(Event::ResponseHead(ResponseHead {
            request_id: "r1".into(),
            attempt: 1,
            status: 200,
            headers: vec![(
                "anthropic-ratelimit-unified-overage-utilization".into(),
                "nan".into(),
            )],
            first_event: None,
            outcome: AttemptOutcome::Succeeded,
            cooldown: None,
            written_at: 1_050,
            observed_at: 1_060,
        }));
        let Some(Record::Request { attempts, .. }) = a
            .observe(complete("r1", 1, Some("a"), Outcome::Succeeded, 200, None))
            .pop()
        else {
            panic!("the completion lands the request")
        };
        assert_eq!(attempts[0].overage_utilization, None);
    }

    #[test]
    fn a_failover_yields_one_request_row_and_one_attempt_row_per_attempt() {
        let mut a = Assembler::new(clock());
        assert!(a.observe(observed("r1", Some("k"))).is_empty());
        assert!(a.observe(selected("r1", 1, "a", 1_010)).is_empty());
        // The fixture head carries unified quota headers, so an oauth attempt's head now also yields a sample.
        assert!(matches!(
            a.observe(head("r1", 1, 429, AttemptOutcome::RateLimited, 1_060))
                .as_slice(),
            [Record::Samples { .. }]
        ));
        assert!(a.observe(selected("r1", 2, "b", 1_100)).is_empty());
        assert!(matches!(
            a.observe(head("r1", 2, 200, AttemptOutcome::Succeeded, 1_250))
                .as_slice(),
            [Record::Samples { .. }]
        ));
        assert_eq!(a.in_flight(), 1);
        let Some(Record::Request {
            request,
            attempts,
            bodies,
        }) = a
            .observe(complete("r1", 2, Some("b"), Outcome::Succeeded, 200, None))
            .pop()
        else {
            panic!()
        };
        assert_eq!(a.in_flight(), 0);
        assert!(bodies.is_none());
        assert_eq!(
            (
                request.id.as_str(),
                request.key_id.as_deref(),
                request.pool_id.as_deref(),
                request.model.as_str(),
                request.canonical_model.as_str()
            ),
            (
                "r1",
                Some("k"),
                Some("main"),
                "claude-sonnet-5[1m]",
                "claude-sonnet-5"
            )
        );
        assert_eq!(
            (
                request.status,
                request.outcome.as_str(),
                request.attempts,
                request.request_bytes,
                request.response_bytes
            ),
            (Some(200), "succeeded", 2, Some(10), Some(20))
        );
        assert_eq!(
            (
                request.received_at,
                request.finished_at,
                request.duration_ms,
                request.ttfb_ms,
                request.ttfc_ms
            ),
            (1_000, Some(1_500), Some(500), Some(150), Some(200))
        );
        assert_eq!(
            (
                request.kind.as_str(),
                request.origin.as_str(),
                request.stream,
                request.kept_warm,
                request.bodies_captured
            ),
            ("inference", "client", true, false, false)
        );
        assert_eq!(attempts.len(), 2);
        let first = &attempts[0];
        assert_eq!(
            (
                first.attempt,
                first.account_id.as_str(),
                first.pin_epoch,
                first.started_at,
                first.finished_at,
                first.ttfb_ms
            ),
            (1, "a", Some(4), 1_010, Some(1_100), Some(50))
        );
        assert_eq!(
            (
                first.upstream_status,
                first.upstream_request_id.as_deref(),
                first.outcome.as_str(),
                first.cooldown_scope.as_deref(),
                first.cooldown_until
            ),
            (
                Some(429),
                Some("req_up_1"),
                "rate_limited",
                Some("account"),
                Some(9_000)
            )
        );
        assert_eq!(
            (
                first.ratelimit_5h_utilization,
                first.ratelimit_5h_reset,
                first.overage_in_use,
                first.overage_utilization
            ),
            (Some(0.42), Some(2_000_000), Some(true), Some(0.1))
        );
        assert_eq!(
            (first.input_tokens, first.client_drift.as_deref()),
            (None, Some("version")),
            "usage lands on the serving attempt only, and drift on every attempt"
        );
        let last = &attempts[1];
        assert_eq!(
            (
                last.attempt,
                last.account_id.as_str(),
                last.finished_at,
                last.ttfb_ms,
                last.ttfc_ms,
                last.outcome.as_str()
            ),
            (2, "b", Some(1_500), Some(150), Some(100), "succeeded")
        );
        assert_eq!(
            (
                last.input_tokens,
                last.output_tokens,
                last.cache_read_tokens,
                last.cache_miss_reason.as_deref()
            ),
            (Some(5), Some(7), Some(2), Some("none"))
        );
    }

    /// Every other field on an attempt row is measured from that attempt's start, while the completion's
    /// ttfc is measured from admission: copied across, a retried request reports a first content from before
    /// the attempt that served it began.
    #[test]
    fn the_serving_attempts_ttfc_is_measured_from_its_own_start() {
        let mut a = Assembler::new(clock());
        a.observe(observed("r12", Some("k")));
        a.observe(selected("r12", 1, "a", 1_010));
        a.observe(head("r12", 1, 429, AttemptOutcome::RateLimited, 1_060));
        // Admission is at 1_000, so this attempt starts 500 ms into the request.
        a.observe(selected("r12", 2, "b", 1_500));
        a.observe(head("r12", 2, 200, AttemptOutcome::Succeeded, 1_600));
        let Event::RequestComplete(mut done) =
            complete("r12", 2, Some("b"), Outcome::Succeeded, 200, None)
        else {
            panic!()
        };
        done.ttfc_ms = Some(800);
        let Some(Record::Request {
            request, attempts, ..
        }) = a.observe(Event::RequestComplete(done)).pop()
        else {
            panic!()
        };
        assert_eq!(
            (request.ttfc_ms, attempts[1].ttfc_ms),
            (Some(800), Some(300)),
            "the request row keeps the request-relative value, and the attempt's is re-based on its own start"
        );
    }

    #[test]
    fn the_completions_message_id_lands_on_the_serving_attempt_and_stays_null_on_a_failover_attempt(
    ) {
        let mut a = Assembler::new(clock());
        a.observe(observed("r13", Some("k")));
        a.observe(selected("r13", 1, "a", 1_010));
        a.observe(head("r13", 1, 429, AttemptOutcome::RateLimited, 1_060));
        a.observe(selected("r13", 2, "b", 1_100));
        a.observe(head("r13", 2, 200, AttemptOutcome::Succeeded, 1_250));
        let Event::RequestComplete(mut done) =
            complete("r13", 2, Some("b"), Outcome::Succeeded, 200, None)
        else {
            panic!()
        };
        done.message_id = Some("msg_01abc".into());
        let Some(Record::Request { attempts, .. }) = a.observe(Event::RequestComplete(done)).pop()
        else {
            panic!()
        };
        assert_eq!(
            attempts
                .iter()
                .map(|a| (
                    a.upstream_request_id.as_deref(),
                    a.upstream_message_id.as_deref()
                ))
                .collect::<Vec<_>>(),
            [
                (Some("req_up_1"), None),
                (Some("req_up_2"), Some("msg_01abc"))
            ],
            "the 429 carries its own request id and no message, and the 200 carries both"
        );
    }

    #[test]
    fn refusals_have_a_row_and_no_attempt_and_a_headless_attempt_is_a_transport_error() {
        let mut a = Assembler::new(clock());
        a.observe(observed("r2", None));
        let Some(Record::Request {
            request, attempts, ..
        }) = a
            .observe(complete(
                "r2",
                0,
                None,
                Outcome::Rejected,
                401,
                Some("auth_unknown"),
            ))
            .pop()
        else {
            panic!()
        };
        assert_eq!(
            (
                request.key_id,
                request.pool_id,
                request.attempts,
                request.outcome.as_str(),
                request.error_code.as_deref(),
                request.status
            ),
            (None, None, 0, "rejected", Some("auth_unknown"), Some(401))
        );
        assert!(attempts.is_empty());
        a.observe(observed("r3", Some("k")));
        a.observe(selected("r3", 1, "a", 1_010));
        let Some(Record::Request {
            request, attempts, ..
        }) = a
            .observe(complete(
                "r3",
                1,
                Some("a"),
                Outcome::Failed,
                502,
                Some("upstream_unreachable"),
            ))
            .pop()
        else {
            panic!()
        };
        assert_eq!(
            (
                request.outcome.as_str(),
                attempts[0].outcome.as_str(),
                attempts[0].upstream_status,
                attempts[0].ttfb_ms
            ),
            ("failed", "transport_error", None, None)
        );
        a.observe(observed("r4", Some("k")));
        a.observe(selected("r4", 1, "a", 1_010));
        let Some(Record::Request { attempts, .. }) = a
            .observe(complete(
                "r4",
                1,
                Some("a"),
                Outcome::Canceled,
                503,
                Some("account_canceled"),
            ))
            .pop()
        else {
            panic!()
        };
        assert_eq!(attempts[0].outcome.as_str(), "canceled");
        // A completion for an id never observed is not a record: the observation was dropped at the bus.
        assert!(a
            .observe(complete("r9", 1, Some("a"), Outcome::Succeeded, 200, None))
            .is_empty());
    }

    #[test]
    fn a_failure_after_a_committed_head_finalises_the_serving_attempt_and_keeps_earlier_ones() {
        let mut a = Assembler::new(clock());
        a.observe(observed("r6", Some("k")));
        a.observe(selected("r6", 1, "a", 1_010));
        a.observe(head("r6", 1, 429, AttemptOutcome::RateLimited, 1_060));
        a.observe(selected("r6", 2, "b", 1_100));
        a.observe(head("r6", 2, 200, AttemptOutcome::Succeeded, 1_250));
        // The stream carried an error event after the head: the request failed, the head said succeeded.
        let Some(Record::Request {
            request, attempts, ..
        }) = a
            .observe(complete(
                "r6",
                2,
                Some("b"),
                Outcome::Failed,
                200,
                Some("overloaded_error"),
            ))
            .pop()
        else {
            panic!()
        };
        assert_eq!(
            (
                request.outcome.as_str(),
                request.status,
                request.error_code.as_deref()
            ),
            ("failed", Some(200), Some("overloaded_error"))
        );
        assert_eq!(
            (
                attempts[0].outcome.as_str(),
                attempts[1].outcome.as_str(),
                attempts[1].upstream_status
            ),
            ("rate_limited", "upstream_error", Some(200))
        );
        a.observe(observed("r7", Some("k")));
        a.observe(selected("r7", 1, "a", 1_010));
        a.observe(head("r7", 1, 200, AttemptOutcome::Succeeded, 1_060));
        let Some(Record::Request { attempts, .. }) = a
            .observe(complete(
                "r7",
                1,
                Some("a"),
                Outcome::Failed,
                200,
                Some("rate_limit_error"),
            ))
            .pop()
        else {
            panic!()
        };
        assert_eq!(attempts[0].outcome.as_str(), "rate_limited");
        a.observe(observed("r8", Some("k")));
        a.observe(selected("r8", 1, "a", 1_010));
        a.observe(head("r8", 1, 200, AttemptOutcome::Succeeded, 1_060));
        let Some(Record::Request { attempts, .. }) = a
            .observe(complete(
                "r8",
                1,
                Some("a"),
                Outcome::Canceled,
                503,
                Some("account_canceled"),
            ))
            .pop()
        else {
            panic!()
        };
        assert_eq!(
            attempts[0].outcome.as_str(),
            "canceled",
            "canceled after the head"
        );
    }

    #[test]
    fn an_incomplete_completion_turns_the_serving_attempt_to_upstream_error_and_leaves_an_earlier_attempt(
    ) {
        let mut a = Assembler::new(clock());
        a.observe(observed("r9", Some("k")));
        a.observe(selected("r9", 1, "a", 1_010));
        a.observe(head("r9", 1, 429, AttemptOutcome::RateLimited, 1_060));
        a.observe(selected("r9", 2, "b", 1_100));
        a.observe(head("r9", 2, 200, AttemptOutcome::Succeeded, 1_250));
        let Some(Record::Request {
            request, attempts, ..
        }) = a
            .observe(complete(
                "r9",
                2,
                Some("b"),
                Outcome::Incomplete,
                200,
                Some("stream_cut"),
            ))
            .pop()
        else {
            panic!()
        };
        assert_eq!(
            (
                request.outcome.as_str(),
                request.status,
                request.error_code.as_deref()
            ),
            ("incomplete", Some(200), Some("stream_cut"))
        );
        assert_eq!(
            (
                attempts[0].outcome.as_str(),
                attempts[1].outcome.as_str(),
                attempts[1].upstream_status
            ),
            ("rate_limited", "upstream_error", Some(200)),
            "the body the head promised never ended, so the serving attempt failed, and the failover keeps its own"
        );
    }

    #[test]
    fn a_body_that_did_not_decode_files_stream_undecodable_on_the_request_and_fails_the_serving_attempt(
    ) {
        let mut a = Assembler::new(clock());
        a.observe(observed("r10", Some("k")));
        a.observe(selected("r10", 1, "a", 1_010));
        a.observe(head("r10", 1, 200, AttemptOutcome::Succeeded, 1_060));
        let Some(Record::Request {
            request, attempts, ..
        }) = a
            .observe(complete(
                "r10",
                1,
                Some("a"),
                Outcome::Incomplete,
                200,
                Some("stream_undecodable"),
            ))
            .pop()
        else {
            panic!()
        };
        assert_eq!(
            (
                request.outcome.as_str(),
                request.error_code.as_deref(),
                attempts[0].outcome.as_str()
            ),
            ("incomplete", Some("stream_undecodable"), "upstream_error")
        );
        a.observe(observed("r11", Some("k")));
        a.observe(selected("r11", 1, "a", 1_010));
        a.observe(head("r11", 1, 400, AttemptOutcome::UpstreamError, 1_060));
        let Some(Record::Request { request, .. }) = a
            .observe(complete(
                "r11",
                1,
                Some("a"),
                Outcome::Failed,
                502,
                Some("stream_undecodable"),
            ))
            .pop()
        else {
            panic!()
        };
        assert_eq!(
            (
                request.outcome.as_str(),
                request.status,
                request.error_code.as_deref()
            ),
            ("failed", Some(502), Some("stream_undecodable")),
            "the relay's own 502 for a rejection it could not decode"
        );
    }

    /// A selection lost from the middle of a trail leaves the serving attempt present, so the trail looked
    /// complete while the attempt count and the failovers behind it were short by one.
    #[test]
    fn a_dropped_earlier_selection_is_a_gap_and_keeps_the_completions_attempt_count() {
        let mut a = Assembler::new(clock());
        a.observe(observed("r11", Some("k")));
        // Attempt 1's selection never arrived; attempt 2's did, and it is the one that served.
        a.observe(selected("r11", 2, "b", 1_100));
        a.observe(head("r11", 2, 200, AttemptOutcome::Succeeded, 1_250));
        let records = a.observe(complete("r11", 2, Some("b"), Outcome::Succeeded, 200, None));
        assert_eq!(records.len(), 2, "the request and a gap");
        let Record::Event(gap) = &records[0] else {
            panic!()
        };
        let detail: serde_json::Value =
            serde_json::from_str(gap.detail.as_deref().unwrap()).unwrap();
        assert_eq!(
            (detail["request_id"].as_str(), detail["reason"].as_str()),
            (Some("r11"), Some("attempt_missing"))
        );
        let Record::Request {
            request, attempts, ..
        } = &records[1]
        else {
            panic!()
        };
        assert_eq!(
            (request.attempts, attempts.len()),
            (2, 1),
            "the count is the one the completion names, not the length of the trail that arrived"
        );
    }

    /// The reselect after an attempt can refuse a request with a code the refusal list names. Where the bus lost
    /// that attempt's selection the trail holds no row, so the flag has to read the count the row stores.
    #[test]
    fn a_refusal_after_a_lost_selection_keeps_its_attempt_count_and_is_not_refused() {
        let mut a = Assembler::new(clock());
        a.observe(observed("r12", Some("k")));
        let records = a.observe(complete(
            "r12",
            1,
            Some("a"),
            Outcome::Rejected,
            403,
            Some("client_not_allowed"),
        ));
        assert_eq!(records.len(), 2, "the request and a gap");
        let Record::Event(gap) = &records[0] else {
            panic!()
        };
        let detail: serde_json::Value =
            serde_json::from_str(gap.detail.as_deref().unwrap()).unwrap();
        assert_eq!(
            (detail["request_id"].as_str(), detail["reason"].as_str()),
            (Some("r12"), Some("attempt_missing"))
        );
        let Record::Request {
            request, attempts, ..
        } = &records[1]
        else {
            panic!()
        };
        assert_eq!(
            (attempts.len(), request.attempts, request.refused),
            (0, 1, false),
            "an empty trail, the completion's count, and a rejected request rather than a refusal"
        );
    }

    #[test]
    fn a_dropped_selection_and_a_skipped_chunk_are_recorded_as_gaps_rather_than_misattributed() {
        let mut a = Assembler::new(clock());
        a.observe(observed("r9", Some("k")));
        a.observe(selected("r9", 1, "a", 1_010));
        a.observe(head("r9", 1, 429, AttemptOutcome::RateLimited, 1_060));
        // The bus dropped attempt 2's selection; its completion still arrives, naming account b.
        let records = a.observe(complete("r9", 2, Some("b"), Outcome::Succeeded, 200, None));
        assert_eq!(records.len(), 2, "the request and a gap");
        let Record::Event(gap) = &records[0] else {
            panic!()
        };
        assert_eq!(
            (gap.kind.as_str(), gap.request_id.as_deref()),
            ("ledger_gap", Some("r9"))
        );
        let detail: serde_json::Value =
            serde_json::from_str(gap.detail.as_deref().unwrap()).unwrap();
        assert_eq!(
            (detail["request_id"].as_str(), detail["reason"].as_str()),
            (Some("r9"), Some("attempt_missing"))
        );
        let Record::Request {
            request, attempts, ..
        } = &records[1]
        else {
            panic!()
        };
        assert_eq!(
            (request.attempts, attempts.len(), request.outcome.as_str()),
            (2, 1, "succeeded"),
            "the count is the completion's, and the trail is what arrived"
        );
        assert_eq!(
            (attempts[0].input_tokens, attempts[0].outcome.as_str()),
            (None, "rate_limited"),
            "b's usage lands on no row rather than on a's failed attempt"
        );
        // A chunk index skipped inside one request: the capture is truncated and a gap says so, once.
        a.observe(observed_capturing("r10", Some("k"), 24));
        a.observe(selected("r10", 1, "a", 1_010));
        a.observe(head("r10", 1, 200, AttemptOutcome::Succeeded, 1_060));
        assert!(a.observe(chunk("r10", 0)).is_empty());
        let gaps = a.observe(chunk("r10", 2));
        assert!(
            matches!(gaps.as_slice(), [Record::Event(e)] if e.kind == "ledger_gap" && e.detail.as_deref().unwrap().contains("chunk_skipped"))
        );
        assert!(a.observe(chunk("r10", 3)).is_empty(), "one gap per request");
        let Some(Record::Request {
            request, bodies, ..
        }) = a
            .observe(complete("r10", 1, Some("a"), Outcome::Succeeded, 200, None))
            .pop()
        else {
            panic!()
        };
        assert!(
            request.bodies_captured && request.response_truncated,
            "a truncated capture is stored, not dropped"
        );
        assert_eq!(
            bodies.unwrap().response_body.as_deref(),
            Some(&b"x"[..]),
            "only the bytes seen before the gap are kept"
        );
    }

    #[test]
    fn an_entry_expires_after_silence_and_a_stream_that_keeps_sending_is_kept() {
        let now = Arc::new(std::sync::atomic::AtomicI64::new(1_000));
        let clock: Clock = {
            let c = now.clone();
            Arc::new(move || c.load(std::sync::atomic::Ordering::SeqCst))
        };
        let mut a = Assembler::new(clock);
        a.observe(observed("r5", Some("k")));
        assert!(a.expire(1_000 + IN_FLIGHT_TTL_MS - 1).ids.is_empty());
        // Twenty minutes of chunks: each one is activity, so the entry outlives the TTL measured from observation.
        for minute in [5, 10, 15, 20] {
            now.store(1_000 + minute * 60_000, std::sync::atomic::Ordering::SeqCst);
            a.observe(chunk("r5", minute as u32));
        }
        assert!(
            a.expire(1_000 + 20 * 60_000 + IN_FLIGHT_TTL_MS - 1)
                .ids
                .is_empty(),
            "still active"
        );
        assert_eq!(
            a.expire(1_000 + 20 * 60_000 + IN_FLIGHT_TTL_MS).ids,
            ["r5"],
            "silent for the whole TTL, and named so its gap can say which request it was"
        );
        assert_eq!(a.in_flight(), 0);
    }

    #[test]
    fn a_capture_past_the_cap_keeps_the_length_it_saw_and_stores_the_cap() {
        let mut a = Assembler::new(clock());
        a.observe(observed_capturing("r13", Some("k"), 1));
        a.observe(selected("r13", 1, "a", 1_010));
        let half = MAX_CAPTURED_BODY / 2;
        assert!(a.observe(chunk_sized("r13", 0, half)).is_empty());
        assert!(a.observe(chunk_sized("r13", 1, half)).is_empty());
        assert!(a.observe(chunk_sized("r13", 2, 10)).is_empty());
        let Some(Record::Request { bodies, .. }) = a
            .observe(complete("r13", 1, Some("a"), Outcome::Succeeded, 200, None))
            .pop()
        else {
            panic!()
        };
        let bodies = bodies.expect("the capture stayed on for the whole stream");
        assert_eq!(
            (
                bodies.response_body.as_ref().map(Vec::len),
                bodies.response_len_seen
            ),
            (Some(MAX_CAPTURED_BODY), (MAX_CAPTURED_BODY + 10) as u64),
            "the stored body is cut at the cap, and the seen length is the full count"
        );
    }

    #[test]
    fn a_user_agent_past_the_bound_is_cut_to_it() {
        let mut a = Assembler::new(clock());
        let Event::RequestObserved(mut request) = observed("r14", None) else {
            unreachable!()
        };
        request.headers = vec![("user-agent".into(), "a".repeat(300))];
        assert!(a.observe(Event::RequestObserved(request)).is_empty());
        let Some(Record::Request { request, .. }) = a
            .observe(complete("r14", 0, None, Outcome::Succeeded, 200, None))
            .pop()
        else {
            panic!()
        };
        let user_agent = request.user_agent.expect("a user agent was observed");
        assert_eq!(user_agent.len(), USER_AGENT_MAX_BYTES);
        assert!(user_agent.chars().all(|c| c == 'a'));
    }

    #[test]
    fn a_forked_sessions_identity_lands_its_base_and_depth_on_the_request_row() {
        let mut a = Assembler::new(clock());
        let Event::RequestObserved(mut request) = observed("r16", None) else {
            unreachable!()
        };
        request.fork = Fork {
            forked_from: Some("s1".into()),
            fork_depth: Some(3),
        };
        assert!(a.observe(Event::RequestObserved(request)).is_empty());
        let Some(Record::Request { request, .. }) = a
            .observe(complete("r16", 0, None, Outcome::Succeeded, 200, None))
            .pop()
        else {
            panic!()
        };
        assert_eq!(
            (request.forked_from.as_deref(), request.fork_depth),
            (Some("s1"), Some(3))
        );
    }

    #[test]
    fn a_capture_whose_last_chunk_never_arrived_is_stored_truncated_with_a_gap() {
        let mut a = Assembler::new(clock_at(1_000));
        a.observe(observed_capturing("r1", Some("k"), 1));
        a.chunk(&chunk_bytes("r1", 0, b"A"));
        // The second chunk was dropped at the bus; the completion says two were published.
        let records = a.observe(Event::RequestComplete(completion("r1", Some(2), Some(2))));
        let (request, bodies) = request_and_bodies(&records);
        assert!(request.response_truncated);
        assert_eq!(bodies.unwrap().response_body.as_deref(), Some(&b"A"[..]));
        assert_eq!(
            records.iter().find_map(gap_detail),
            Some(("r1".to_string(), "chunk_dropped".to_string()))
        );
    }

    #[test]
    fn the_entry_cap_evicts_the_oldest_with_a_gap() {
        let mut a = Assembler::new(clock_at(1_000));
        let mut gaps = Vec::new();
        for i in 0..=MAX_IN_FLIGHT {
            gaps.extend(a.observe(observed(&format!("r{i}"), Some("k"))));
        }
        assert_eq!(a.in_flight(), MAX_IN_FLIGHT);
        assert_eq!(
            gaps.iter().filter_map(gap_detail).collect::<Vec<_>>(),
            vec![("r0".to_string(), "in_flight_cap".to_string())],
            "the first observed is the only one evicted"
        );
    }

    #[test]
    fn the_entry_cap_eviction_releases_the_victims_capture_budget_too() {
        let mut a = Assembler::new(clock_at(1_000));
        // The entry cap's worth of captures fills the budget to the byte, so a victim's charge left behind pushes
        // the next capture past it.
        let size = MAX_CAPTURE_BYTES / MAX_IN_FLIGHT;
        for i in 0..MAX_IN_FLIGHT {
            a.observe(observed_capturing_with_body(
                &format!("r{i}"),
                vec![b'x'; size],
            ));
        }
        let records = a.observe(observed_capturing_with_body("overflow", vec![b'x'; size]));
        assert_eq!(
            records.iter().filter_map(gap_detail).collect::<Vec<_>>(),
            vec![("r0".to_string(), "in_flight_cap".to_string())],
            "the evicted entry's bytes came back out, so the new entry's charge stays inside the budget"
        );
        let more = a.observe(observed_capturing_with_body("more", vec![b'x'; size]));
        assert!(
            !more
                .iter()
                .any(|r| gap_detail(r).is_some_and(|(_, reason)| reason == "capture_budget")),
            "the next turnover gives its victim's bytes back too"
        );
    }

    #[test]
    fn a_completed_capture_gives_its_charge_back_to_the_budget() {
        let mut a = Assembler::new(clock_at(1_000));
        a.observe(observed_capturing_with_body("r1", vec![b'x'; 10]));
        a.chunk(&chunk_bytes("r1", 0, b"answer"));
        // One byte past the budget with r1's 10 request bytes and 6 response bytes counted, so the budget drops
        // this larger capture and r1 keeps its bodies.
        let records = a.observe(observed_capturing_with_body(
            "filler",
            vec![b'x'; MAX_CAPTURE_BYTES - 15],
        ));
        assert_eq!(
            records.iter().find_map(gap_detail),
            Some(("filler".to_string(), "capture_budget".to_string())),
            "r1 is charged while in flight"
        );
        let records = a.observe(Event::RequestComplete(completion("r1", Some(1), None)));
        assert!(request_of(&records).bodies_captured);
        let records = a.observe(observed_capturing_with_body(
            "r2",
            vec![b'x'; MAX_CAPTURE_BYTES],
        ));
        assert!(
            records.iter().find_map(gap_detail).is_none(),
            "r1 left the assembler with its bodies, so nothing of it stays charged and the whole budget fits"
        );
    }

    #[test]
    fn an_expired_capture_gives_its_charge_back_to_the_budget() {
        let mut a = Assembler::new(clock_at(1_000));
        a.observe(observed_capturing_with_body("r1", vec![b'x'; 10]));
        a.chunk(&chunk_bytes("r1", 0, b"answer"));
        assert_eq!(a.expire(1_000 + IN_FLIGHT_TTL_MS).ids.len(), 1);
        let records = a.observe(observed_capturing_with_body(
            "r2",
            vec![b'x'; MAX_CAPTURE_BYTES],
        ));
        assert!(
            records.iter().find_map(gap_detail).is_none(),
            "nothing of r1 stays charged, so the whole budget fits"
        );
    }

    #[test]
    fn the_capture_budget_is_charged_at_observation_and_drops_the_largest_capture() {
        let mut a = Assembler::new(clock_at(1_000));
        // Three capturing requests of 24 MiB each: the third observation passes the 64 MiB budget, so the
        // first (the largest, oldest among equals) gives its bodies back.
        let mut last = Vec::new();
        for id in ["r1", "r2", "r3"] {
            last = a.observe(observed_capturing_with_body(id, vec![b'x'; 24 << 20]));
        }
        assert_eq!(
            last.iter().find_map(gap_detail),
            Some(("r1".to_string(), "capture_budget".to_string()))
        );
        assert_eq!(a.retained_capture_bytes(), 48 << 20);
        let r1 =
            request_of(&a.observe(Event::RequestComplete(completion("r1", None, None)))).clone();
        let r2 =
            request_of(&a.observe(Event::RequestComplete(completion("r2", None, None)))).clone();
        let r3 =
            request_of(&a.observe(Event::RequestComplete(completion("r3", None, None)))).clone();
        assert!(
            !r1.bodies_captured,
            "the evicted capture still lands its row, without a body"
        );
        assert!(r2.bodies_captured);
        assert!(r3.bodies_captured);
    }

    #[test]
    fn a_response_larger_on_the_wire_than_decoded_is_stored_complete() {
        let mut a = Assembler::new(clock_at(1_000));
        a.observe(observed_capturing("r1", Some("k"), 1));
        a.chunk(&chunk_bytes("r1", 0, b"0123456789"));
        // One chunk published and delivered, under a byte count four times its length.
        let records = a.observe(Event::RequestComplete(completion("r1", Some(1), Some(40))));
        let (request, bodies) = request_and_bodies(&records);
        assert!(!request.response_truncated);
        assert_eq!(
            bodies.unwrap().response_body.as_deref(),
            Some(&b"0123456789"[..])
        );
        assert!(!records.iter().any(|r| gap_detail(r).is_some()));
    }

    #[test]
    fn a_key_that_captures_nothing_keeps_no_request_body_and_still_counts_its_bytes() {
        let mut a = Assembler::new(clock_at(1_000));
        a.observe(observed_with_body("r1", 0, vec![b'x'; 10_000]));
        assert_eq!(a.retained_request_bytes(), 0);
        // A chunk dropped at the bus still marks the entry gapped even though nothing is captured.
        a.observe(chunk("r1", 1));
        let records = a.observe(Event::RequestComplete(completion("r1", None, None)));
        let request = request_of(&records);
        assert_eq!(request.request_bytes, Some(10_000));
        assert!(
            !request.response_truncated,
            "nothing was captured, so nothing reads truncated"
        );
    }

    #[test]
    fn the_request_row_records_the_completions_diagnostics_source() {
        for (source, word) in [
            (Some(DiagnosticsSource::Router), Some("router")),
            (Some(DiagnosticsSource::Client), Some("client")),
            (None, None),
        ] {
            let mut a = Assembler::new(clock());
            a.observe(observed("r1", Some("k")));
            a.observe(selected("r1", 1, "a", 1_010));
            a.observe(head("r1", 1, 200, AttemptOutcome::Succeeded, 1_060));
            let records = a.observe(complete_with_source("r1", source));
            assert_eq!(
                request_of(&records).diagnostics_source.as_deref(),
                word,
                "{source:?}"
            );
        }
    }

    #[test]
    fn a_rejection_head_records_the_rejection_and_the_serving_attempt_the_permit() {
        let mut a = Assembler::new(clock());
        a.observe(observed("r1", Some("k")));
        a.observe(selected("r1", 1, "a", 1_010));
        let rejected = a.observe(rejection_head("r1", 1, 1_060));
        assert_eq!(
            rejected.len(),
            1,
            "the head arm records exactly the rejection"
        );
        let Record::Event(event) = &rejected[0] else {
            panic!("expected an event record");
        };
        assert_eq!(
            (
                event.kind.as_str(),
                event.account_id.as_deref(),
                event.key_id.as_deref(),
                event.session_id.as_deref(),
                event.model.as_deref(),
                event.at
            ),
            (
                "model_rejected",
                Some("a"),
                Some("k"),
                Some("s1"),
                Some("claude-sonnet-5"),
                1_060
            )
        );
        assert_eq!(
            event.request_id.as_deref(),
            Some("r1"),
            "the router's request id has its own column"
        );
        let detail: serde_json::Value =
            serde_json::from_str(event.detail.as_deref().unwrap()).unwrap();
        assert_eq!(
            (
                detail["attempt"].as_u64(),
                detail["request_id"].as_str(),
                detail["source_request_id"].as_str(),
                detail["wire_model"].as_str()
            ),
            (
                Some(1),
                Some("req_up_1"),
                Some("r1"),
                Some("claude-sonnet-5")
            )
        );
        a.observe(selected("r1", 2, "b", 1_100));
        a.observe(head("r1", 2, 200, AttemptOutcome::Succeeded, 1_250));
        let done = a.observe(complete("r1", 2, Some("b"), Outcome::Succeeded, 200, None));
        let permit = done
            .iter()
            .find_map(|r| match r {
                Record::Event(e) if e.kind == "model_permitted" => Some(e),
                _ => None,
            })
            .expect("a model_permitted event");
        assert_eq!(
            (
                permit.account_id.as_deref(),
                permit.key_id.as_deref(),
                permit.session_id.as_deref(),
                permit.model.as_deref(),
                permit.at
            ),
            (
                Some("b"),
                Some("k"),
                Some("s1"),
                Some("claude-sonnet-5"),
                1_500
            )
        );
        assert_eq!(permit.request_id.as_deref(), Some("r1"));
        let detail: serde_json::Value =
            serde_json::from_str(permit.detail.as_deref().unwrap()).unwrap();
        assert_eq!(detail["attempt"].as_u64(), Some(2));
        assert_eq!(detail["rejected_by"], serde_json::json!(["a"]));
        assert_eq!(
            (
                detail["request_id"].as_str(),
                detail["source_request_id"].as_str(),
                detail["wire_model"].as_str()
            ),
            (Some("req_up_2"), Some("r1"), Some("claude-sonnet-5"))
        );
    }

    #[test]
    fn a_rejection_followed_by_no_success_records_no_permit() {
        let mut a = Assembler::new(clock());
        a.observe(observed("r2", Some("k")));
        a.observe(selected("r2", 1, "a", 1_010));
        a.observe(rejection_head("r2", 1, 1_060));
        a.observe(selected("r2", 2, "b", 1_100));
        a.observe(head("r2", 2, 429, AttemptOutcome::RateLimited, 1_150));
        let done = a.observe(complete(
            "r2",
            2,
            Some("b"),
            Outcome::Failed,
            429,
            Some("rate_limit_error"),
        ));
        assert!(
            !done
                .iter()
                .any(|r| matches!(r, Record::Event(e) if e.kind == "model_permitted")),
            "no success followed the rejection, so no permit is recorded"
        );
    }
}
