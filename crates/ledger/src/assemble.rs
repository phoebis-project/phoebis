//! Turns the event sequence of one request into its ledger rows. Events arrive in publish order per request
//! but interleaved across requests, so each request's partial rows wait in a map keyed by id until its
//! completion, and a request that goes silent is aged out rather than kept forever.

use std::collections::HashMap;

use phoebis_bus::{
    AccountSelected, Event, Outcome, RequestComplete, RequestObserved, ResponseChunk, ResponseHead,
};
use phoebis_oauth::Clock;
use phoebis_relay::UnifiedHeaders;
use phoebis_transport::response::ResponseHead as WireHead;
use serde_json::json;

use crate::quota::samples_from_unified;
use crate::record::{
    AttemptRecord, BodyRecord, EventRecord, ExtraUsageSampleRecord, QuotaSampleRecord,
    RequestRecord,
};

/// Well past the relay's own ten-minute idle wait: an entry nothing has touched for this long belongs to a
/// request whose completion the bus dropped. Measured from the last event or chunk, not from observation, so
/// a stream that keeps sending is never cut off by its own length.
pub const IN_FLIGHT_TTL_MS: i64 = 15 * 60 * 1_000;

// Boxing the request would put an allocation on the path every request takes, to shrink a value that is
// built once per completion and moved once into the writer's bounded channel.
#[allow(clippy::large_enum_variant)]
pub enum Record {
    Event(EventRecord),
    Request {
        request: RequestRecord,
        attempts: Vec<AttemptRecord>,
        bodies: Option<BodyRecord>,
    },
    Samples {
        quota: Vec<QuotaSampleRecord>,
        extra: Option<ExtraUsageSampleRecord>,
    },
}

pub(crate) struct InFlight {
    pub observed: RequestObserved,
    pub attempts: Vec<AttemptRecord>,
    /// The response text assembled from chunks while the request's key captures bodies.
    pub response_body: Option<Vec<u8>>,
    /// The chunk index expected next; a chunk past it means one was dropped at the bus.
    pub next_index: u32,
    /// Set once a chunk was skipped: the capture is incomplete and is not stored.
    pub gapped: bool,
    /// The last moment anything about this request arrived.
    pub seen_at: i64,
}

pub struct Assembler {
    clock: Clock,
    in_flight: HashMap<String, InFlight>,
}

impl Assembler {
    pub fn new(clock: Clock) -> Assembler {
        Assembler {
            clock,
            in_flight: HashMap::new(),
        }
    }

    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }

    pub fn observe(&mut self, event: Event) -> Vec<Record> {
        let now = (self.clock)();
        match event {
            Event::RequestObserved(observed) => {
                self.in_flight.insert(
                    observed.request_id.clone(),
                    InFlight {
                        observed,
                        attempts: Vec::new(),
                        response_body: None,
                        next_index: 0,
                        gapped: false,
                        seen_at: now,
                    },
                );
                Vec::new()
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
                let Some(attempt) = entry
                    .attempts
                    .iter_mut()
                    .find(|a| a.attempt as u8 == head.attempt)
                else {
                    return Vec::new();
                };
                let parsed = fill_head(attempt, &head);
                if attempt.account_kind == "oauth" {
                    if let Some((unified, wire)) = parsed {
                        let (quota, extra) = samples_from_unified(
                            &attempt.account_id,
                            &unified,
                            &wire,
                            head.observed_at,
                        );
                        if !quota.is_empty() || extra.is_some() {
                            return vec![Record::Samples { quota, extra }];
                        }
                    }
                }
                Vec::new()
            }
            Event::ResponseChunk(chunk) => self.chunk(&chunk),
            Event::RequestComplete(complete) => match self.in_flight.remove(&complete.request_id) {
                Some(entry) => finish(entry, &complete, now),
                None => Vec::new(),
            },
        }
    }

    /// Every chunk is activity and carries its index; the first skipped index ends the capture and is reported
    /// once.
    pub fn chunk(&mut self, chunk: &ResponseChunk) -> Vec<Record> {
        let now = (self.clock)();
        let Some(entry) = self.in_flight.get_mut(&chunk.request_id) else {
            return Vec::new();
        };
        entry.seen_at = now;
        let mut records = Vec::new();
        if chunk.index != entry.next_index && !entry.gapped {
            entry.gapped = true;
            entry.response_body = None;
            records.push(gap(&chunk.request_id, "chunk_skipped", now));
        }
        entry.next_index = chunk.index.saturating_add(1);
        if let Some(body) = entry.response_body.as_mut() {
            body.extend_from_slice(&chunk.body);
        }
        records
    }

    pub fn expire(&mut self, now: i64) -> usize {
        let before = self.in_flight.len();
        self.in_flight
            .retain(|_, entry| now - entry.seen_at < IN_FLIGHT_TTL_MS);
        let dropped = before - self.in_flight.len();
        if dropped > 0 {
            tracing::warn!(
                dropped,
                "requests whose completion never reached the ledger were dropped"
            );
        }
        dropped
    }
}

/// A gap inside one request, as opposed to the writer's gap over dropped records: the row that follows is
/// incomplete in a way the reader can see.
fn gap(request_id: &str, reason: &str, at: i64) -> Record {
    Record::Event(EventRecord {
        at,
        kind: "ledger_gap".into(),
        detail: Some(json!({ "request_id": request_id, "reason": reason }).to_string()),
        ..Default::default()
    })
}

fn attempt_row(selected: &AccountSelected) -> AttemptRecord {
    AttemptRecord {
        request_id: selected.request_id.clone(),
        attempt: selected.attempt as u32,
        account_id: selected.account_id.clone(),
        account_kind: selected.account_kind.clone(),
        pin_epoch: selected.pin_epoch,
        started_at: selected.started_at,
        outcome: "transport_error".into(),
        ..AttemptRecord::default()
    }
}

/// Fills the attempt row's rate-limit fields and hands back the parse it just ran, so the quota sampler
/// downstream reads the same `UnifiedHeaders` and `WireHead` instead of cloning the header vector and parsing
/// it again.
fn fill_head(
    attempt: &mut AttemptRecord,
    head: &ResponseHead,
) -> Option<(UnifiedHeaders, WireHead)> {
    attempt.upstream_status = Some(head.status);
    attempt.ttfb_ms = Some(head.observed_at - attempt.started_at);
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
    let wire = WireHead {
        status: head.status,
        headers: head.headers.clone(),
    };
    let unified = UnifiedHeaders::parse(&wire)?;
    attempt.ratelimit_5h_utilization = unified.five_hour_utilization;
    attempt.ratelimit_5h_reset = unified.five_hour_reset;
    attempt.ratelimit_7d_utilization = unified.seven_day_utilization;
    attempt.ratelimit_7d_reset = unified.seven_day_reset;
    attempt.ratelimit_7d_oi_reset = unified.seven_day_oi_reset;
    attempt.ratelimit_7d_oi_utilization = wire
        .get("anthropic-ratelimit-unified-7d_oi-utilization")
        .and_then(|v| v.trim().parse().ok());
    attempt.overage_in_use = unified.overage_in_use;
    attempt.overage_utilization = wire
        .get("anthropic-ratelimit-unified-overage-utilization")
        .and_then(|v| v.trim().parse().ok());
    Some((unified, wire))
}

/// The attempt outcome a request's terminal cause implies for the attempt that served it, once the head's own
/// classification is known to have been overtaken.
fn terminal_outcome(complete: &RequestComplete) -> Option<&'static str> {
    match complete.outcome {
        Outcome::Canceled => Some("canceled"),
        Outcome::Failed if complete.error_code.as_deref() == Some("rate_limit_error") => {
            Some("rate_limited")
        }
        Outcome::Failed => Some("upstream_error"),
        _ => None,
    }
}

fn finish(entry: InFlight, complete: &RequestComplete, now: i64) -> Vec<Record> {
    let InFlight {
        observed,
        mut attempts,
        response_body,
        gapped,
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
    let trail_complete = complete.attempt == 0 || serving.is_some();
    if !trail_complete {
        records.push(gap(&observed.request_id, "attempt_missing", now));
    }
    if let Some(i) = serving {
        let last = &mut attempts[i];
        last.finished_at = Some(complete.finished_at);
        last.ttfc_ms = complete.ttfc_ms;
        if let Some(usage) = &complete.usage {
            last.input_tokens = usage.input_tokens;
            last.output_tokens = usage.output_tokens;
            last.cache_read_tokens = usage.cache_read_tokens;
            last.cache_write_5m_tokens = usage.cache_write_5m_tokens;
            last.cache_write_1h_tokens = usage.cache_write_1h_tokens;
            last.service_tier = usage.service_tier.clone();
            last.cache_miss_reason = usage.cache_miss_reason.clone();
        }
        if last.upstream_status.is_none()
            && complete.error_code.as_deref() == Some("account_cancelled")
        {
            last.outcome = "canceled".into();
        }
        // A head classified as succeeded whose body then failed: the request's terminal cause wins for this
        // attempt alone; a failover classification on an earlier attempt is left as it was.
        if last.upstream_status.is_some() && last.outcome == "succeeded" {
            if let Some(outcome) = terminal_outcome(complete) {
                last.outcome = outcome.into();
            }
        }
    }
    for attempt in &mut attempts {
        attempt.client_drift = complete.client_drift.clone();
    }
    let request = RequestRecord {
        id: observed.request_id.clone(),
        received_at: observed.received_at,
        finished_at: Some(complete.finished_at),
        key_id: observed.key_id.clone(),
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
        attempts: if trail_complete {
            attempts.len() as u32
        } else {
            complete.attempt as u32
        },
        request_bytes: Some(observed.body.len() as u64),
        response_bytes: complete.response_bytes,
        ttfb_ms: complete.ttfb_ms,
        ttfc_ms: complete.ttfc_ms,
        duration_ms: Some(complete.finished_at - observed.received_at),
        error_code: complete.error_code.clone(),
        kept_warm: false,
        bodies_captured: response_body.is_some() && !gapped,
    };
    // A capture only becomes a row once the chunks that fed it are complete; nothing arms one yet.
    let bodies = None;
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

    fn observed(id: &str, key: Option<&str>) -> Event {
        observed_capturing(id, key, 0)
    }
    fn observed_capturing(id: &str, key: Option<&str>, capture_hours: u32) -> Event {
        Event::RequestObserved(RequestObserved {
            request_id: id.into(),
            origin: Origin::Client,
            kind: Kind::Inference,
            key_id: key.map(String::from),
            pool_id: key.map(|_| "main".into()),
            session_id: Some("s1".into()),
            agent_id: Some("main".into()),
            parent_agent_id: None,
            owner_id: None,
            model: "claude-sonnet-5[1m]".into(),
            canonical_model: "claude-sonnet-5".into(),
            stream: true,
            headers: vec![],
            body: Bytes::from_static(b"0123456789"),
            body_capture_hours: capture_hours,
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
    fn selected(id: &str, n: u8, account: &str, at: i64) -> Event {
        Event::AccountSelected(AccountSelected {
            request_id: id.into(),
            attempt: n,
            account_id: account.into(),
            account_kind: "oauth".into(),
            pin_epoch: Some(4),
            started_at: at,
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
            ttfb_ms: Some(150),
            ttfc_ms: Some(200),
            error_code: code.map(String::from),
            client_drift: Some("version".into()),
            client_version: Some("2.1.300".into()),
            response_bytes: Some(20),
            finished_at: 1_500,
        })
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
            "usage lands on the serving attempt only; drift on every attempt"
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
            (2, "b", Some(1_500), Some(150), Some(200), "succeeded")
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
        assert_eq!(
            (last.estimated_usd, last.pricing_version.as_deref()),
            (None, None)
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
                Some("account_cancelled"),
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
                Some("account_cancelled"),
            ))
            .pop()
        else {
            panic!()
        };
        assert_eq!(
            attempts[0].outcome.as_str(),
            "canceled",
            "cancelled after the head"
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
        assert_eq!(gap.kind, "ledger_gap");
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
            "the count is the completion's; the trail is what arrived"
        );
        assert_eq!(
            (attempts[0].input_tokens, attempts[0].outcome.as_str()),
            (None, "rate_limited"),
            "b's usage lands on no row rather than on a's failed attempt"
        );
        // A chunk index skipped inside one request: the capture is dropped and a gap says so, once.
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
            !request.bodies_captured && bodies.is_none(),
            "a partial capture is not stored"
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
        assert_eq!(a.expire(1_000 + IN_FLIGHT_TTL_MS - 1), 0);
        // Twenty minutes of chunks: each one is activity, so the entry outlives the TTL measured from observation.
        for minute in [5, 10, 15, 20] {
            now.store(1_000 + minute * 60_000, std::sync::atomic::Ordering::SeqCst);
            a.observe(chunk("r5", minute as u32));
        }
        assert_eq!(
            a.expire(1_000 + 20 * 60_000 + IN_FLIGHT_TTL_MS - 1),
            0,
            "still active"
        );
        assert_eq!(
            a.expire(1_000 + 20 * 60_000 + IN_FLIGHT_TTL_MS),
            1,
            "silent for the whole TTL"
        );
        assert_eq!(a.in_flight(), 0);
    }
}
