//! The in-flight table and the ring of recent requests, kept from bus events alone. Nothing here reads the
//! ledger: the feed shows what the relay is doing now, and a restart shows an empty table.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use phoebis_bus::{
    user_agent_of, AccountSelected, AttemptOutcome, Event, Phase, RequestComplete, RequestObserved,
    ResponseChunk, ResponseHead, Usage,
};
use phoebis_ledger::Pricing;
use phoebis_oauth::Clock;
use phoebis_relay::{SseEvent, SseParser, UnifiedHeaders, INSPECT_BUFFER};
use phoebis_transport::response::ResponseHead as WireHead;
use serde::{Deserialize, Serialize};

use crate::protocol::{Delta, DensityBucket, LiveSnapshot, PastSnapshot};
use crate::tick::Tile;

pub const RING_LIMIT: usize = 2_048;
pub const RING_WINDOW_MS: i64 = 30 * 60 * 1_000;
pub const PREVIEW_BYTES: usize = 4 * 1_024;
pub const PROGRESS_INTERVAL_MS: i64 = 500;
pub const RATE_WINDOW_MS: i64 = 5_000;
/// The most in-flight entries kept; past it the oldest silent entry is dropped first. A bus that drops
/// completions under load must not turn into a table that only ever grows.
pub const IN_FLIGHT_LIMIT: usize = 4_096;
// The ledger's inactivity bound, so both consumers forget a lost request together; imported rather than
// restated so the two cannot drift.
pub use phoebis_ledger::IN_FLIGHT_TTL_MS;

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct LivePhases {
    pub received_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub written_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_content_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_end_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LiveWindow {
    pub id: String,
    pub resets_at: Option<i64>,
    pub utilization: Option<f64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LiveCooldown {
    pub reason: String,
    pub until: i64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LiveAttempt {
    pub account_id: String,
    pub cooldown_set: Option<LiveCooldown>,
    pub index: u8,
    pub outcome: String,
    pub started_at: i64,
    pub upstream_status: Option<u16>,
    pub windows: Vec<LiveWindow>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct LiveUsage {
    pub cache_read: u64,
    pub cache_write_1h: u64,
    pub cache_write_5m: u64,
    pub input: u64,
    pub output: u64,
}

impl From<&Usage> for LiveUsage {
    fn from(u: &Usage) -> LiveUsage {
        LiveUsage {
            cache_read: u.cache_read_tokens.unwrap_or(0),
            cache_write_1h: u.cache_write_1h_tokens.unwrap_or(0),
            cache_write_5m: u.cache_write_5m_tokens.unwrap_or(0),
            input: u.input_tokens.unwrap_or(0),
            output: u.output_tokens.unwrap_or(0),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LiveRequest {
    pub account_id: Option<String>,
    pub agent_id: String,
    pub attempts: Vec<LiveAttempt>,
    pub bytes_relayed: u64,
    pub canonical_model: String,
    pub cost_usd: f64,
    pub error_code: Option<String>,
    pub id: String,
    pub key_id: Option<String>,
    pub kind: String,
    pub model: String,
    pub output_tps: Option<f64>,
    pub phases: LivePhases,
    pub pool_id: Option<String>,
    pub preview: Option<String>,
    pub session_id: Option<String>,
    pub state: String,
    pub status: Option<u16>,
    pub transport: String,
    /// The caller's `User-Agent` as it was sent, `None` when it sent none.
    pub user_agent: Option<String>,
    pub usage: LiveUsage,
}

/// What the table keeps per in-flight request beyond the entry itself.
struct Tracked {
    entry: LiveRequest,
    usage: Usage,
    /// Whether the key captures bodies, which is what admits a preview at all.
    captures: bool,
    /// (at, output tokens) samples over the last five seconds, for the rate.
    output_samples: VecDeque<(i64, u64)>,
    last_progress_at: i64,
    /// The preview's parser, one per request: a transport read ends where the socket says, not where an
    /// event does, so the parser has to outlive the chunk that started an event.
    preview: SseParser,
    /// A progress delta the throttle held back, sent by the next tick.
    progress_due: bool,
    /// The last moment any event or chunk of this request arrived; the expiry reads it.
    seen_at: i64,
}

pub struct LiveState {
    clock: Clock,
    cursor: u64,
    in_flight: HashMap<String, Tracked>,
    pricing: Arc<Pricing>,
    ring: VecDeque<LiveRequest>,
    tile: Tile,
}

impl LiveState {
    pub fn new(pricing: Arc<Pricing>, clock: Clock) -> LiveState {
        LiveState {
            clock,
            cursor: 0,
            in_flight: HashMap::new(),
            pricing,
            ring: VecDeque::new(),
            tile: Tile::new(),
        }
    }

    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    fn next_cursor(&mut self) -> u64 {
        self.cursor += 1;
        self.cursor
    }

    pub fn apply(&mut self, event: Event) -> Vec<Delta> {
        match event {
            Event::RequestObserved(observed) => self.started(observed),
            Event::AccountSelected(selected) => self.selected(selected),
            Event::ResponseHead(head) => self.head(head),
            Event::ResponseChunk(chunk) => self.apply_chunk(&chunk),
            Event::RequestComplete(complete) => self.finished(complete),
        }
    }

    fn started(&mut self, o: RequestObserved) -> Vec<Delta> {
        let entry = LiveRequest {
            account_id: None,
            agent_id: o.agent_id.clone().unwrap_or_else(|| "main".into()),
            attempts: Vec::new(),
            bytes_relayed: 0,
            canonical_model: o.canonical_model.clone(),
            cost_usd: 0.0,
            error_code: None,
            id: o.request_id.clone(),
            key_id: o.key_id.clone(),
            kind: o.kind.as_str().into(),
            model: o.model.clone(),
            output_tps: None,
            phases: LivePhases {
                received_at: o.received_at,
                ..Default::default()
            },
            pool_id: o.pool_id.clone(),
            preview: None,
            session_id: o.session_id.clone(),
            state: "in_flight".into(),
            status: None,
            transport: if o.stream { "sse" } else { "json" }.into(),
            user_agent: user_agent_of(&o.headers),
            usage: LiveUsage::default(),
        };
        let captures = o.body_capture_hours > 0;
        let seen_at = (self.clock)();
        self.in_flight.insert(
            o.request_id.clone(),
            Tracked {
                entry: entry.clone(),
                usage: Usage::default(),
                captures,
                output_samples: VecDeque::new(),
                last_progress_at: 0,
                preview: SseParser::new(INSPECT_BUFFER),
                progress_due: false,
                seen_at,
            },
        );
        let cursor = self.next_cursor();
        vec![Delta::Started {
            cursor,
            request: entry,
        }]
    }

    fn selected(&mut self, s: AccountSelected) -> Vec<Delta> {
        let now = (self.clock)();
        let Some(t) = self.in_flight.get_mut(&s.request_id) else {
            return Vec::new();
        };
        t.seen_at = now;
        let failed = t
            .entry
            .attempts
            .last()
            .cloned()
            .filter(|a| a.outcome != "in_flight");
        let attempt = LiveAttempt {
            account_id: s.account_id.clone(),
            cooldown_set: None,
            index: s.attempt,
            outcome: "in_flight".into(),
            started_at: s.started_at,
            upstream_status: None,
            windows: Vec::new(),
        };
        t.entry.attempts.push(attempt.clone());
        t.entry.account_id = Some(s.account_id.clone());
        t.entry.phases.selected_at = Some(s.started_at);
        // A failover starts a fresh leg: the earlier head and write no longer describe what is in flight.
        t.entry.phases.written_at = None;
        t.entry.phases.head_at = None;
        let cursor = self.next_cursor();
        vec![Delta::Selected {
            at: s.started_at,
            cursor,
            id: s.request_id,
            account_id: s.account_id,
            attempt,
            failed,
        }]
    }

    fn head(&mut self, h: ResponseHead) -> Vec<Delta> {
        let now = (self.clock)();
        let Some(t) = self.in_flight.get_mut(&h.request_id) else {
            return Vec::new();
        };
        t.seen_at = now;
        let Some(attempt) = t.entry.attempts.iter_mut().find(|a| a.index == h.attempt) else {
            return Vec::new();
        };
        attempt.upstream_status = Some(h.status);
        attempt.outcome = h.outcome.as_str().into();
        attempt.cooldown_set = h.cooldown.as_ref().map(|c| LiveCooldown {
            reason: c.reason.clone(),
            until: c.until,
        });
        let wire = WireHead {
            status: h.status,
            headers: h.headers.clone(),
        };
        if let Some(u) = UnifiedHeaders::parse(&wire) {
            let oi = wire
                .get("anthropic-ratelimit-unified-7d_oi-utilization")
                .and_then(|v| v.trim().parse().ok());
            for (id, utilization, resets_at) in [
                ("5h", u.five_hour_utilization, u.five_hour_reset),
                ("7d", u.seven_day_utilization, u.seven_day_reset),
                ("7d_oi", oi, u.seven_day_oi_reset),
            ] {
                if utilization.is_some() || resets_at.is_some() {
                    attempt.windows.push(LiveWindow {
                        id: id.into(),
                        resets_at,
                        utilization,
                    });
                }
            }
        }
        let attempt = attempt.clone();
        t.entry.phases.written_at = Some(h.written_at);
        t.entry.phases.head_at = Some(h.observed_at);
        if h.outcome == AttemptOutcome::Succeeded {
            t.entry.status = Some(h.status);
        }
        let cursor = self.next_cursor();
        vec![Delta::Head {
            at: h.observed_at,
            cursor,
            id: h.request_id,
            attempt,
            status: h.status,
        }]
    }

    pub fn apply_chunk(&mut self, chunk: &ResponseChunk) -> Vec<Delta> {
        // Destructured so the in-flight entry, the tile, and the cursor are borrowed side by side.
        let LiveState {
            clock,
            cursor,
            in_flight,
            pricing,
            tile,
            ..
        } = self;
        let now = clock();
        let Some(t) = in_flight.get_mut(&chunk.request_id) else {
            return Vec::new();
        };
        t.seen_at = now;
        t.entry.bytes_relayed += chunk.body.len() as u64;
        let mut deltas = Vec::new();
        let mut usage_moved = false;
        match &chunk.phase {
            Some(Phase::MessageStart { usage }) | Some(Phase::MessageDelta { usage }) => {
                let previous = t.usage.output_tokens.unwrap_or(0);
                t.usage.merge(usage);
                usage_moved = true;
                if let Some(out) = usage.output_tokens {
                    t.output_samples.push_back((now, out));
                    while t
                        .output_samples
                        .front()
                        .is_some_and(|(at, _)| now - at > RATE_WINDOW_MS)
                    {
                        t.output_samples.pop_front();
                    }
                    // The tile sums increments, so this request's running count becomes what the chunk added.
                    tile.output_tokens(now, out.saturating_sub(previous));
                }
            }
            Some(Phase::FirstContentDelta) => t.entry.phases.first_content_at = Some(now),
            Some(Phase::MessageStop) => t.entry.phases.stream_end_at = Some(now),
            Some(Phase::ToolUseDelta) | None => {}
        }
        if t.captures {
            if let Some(text) = text_of(&mut t.preview, &chunk.body) {
                let preview = t.entry.preview.get_or_insert_with(String::new);
                preview.push_str(&text);
                if preview.len() > PREVIEW_BYTES {
                    let cut = preview.len() - PREVIEW_BYTES;
                    let cut = (cut..preview.len())
                        .find(|i| preview.is_char_boundary(*i))
                        .unwrap_or(cut);
                    preview.drain(..cut);
                }
                *cursor += 1;
                deltas.push(Delta::Preview {
                    cursor: *cursor,
                    id: chunk.request_id.clone(),
                    chunk: text,
                });
            }
        }
        if usage_moved {
            t.entry.usage = LiveUsage::from(&t.usage);
            t.entry.output_tps = rate(&t.output_samples);
            let u = t.entry.usage;
            t.entry.cost_usd = pricing
                .current()
                .estimate(
                    &t.entry.canonical_model,
                    u.input,
                    u.cache_read,
                    u.cache_write_5m,
                    u.cache_write_1h,
                    u.output,
                )
                .map(|(usd, _)| usd)
                .unwrap_or(0.0);
            // At most twice a second per request: a delta that comes sooner waits for the next tick.
            if now - t.last_progress_at >= PROGRESS_INTERVAL_MS {
                t.last_progress_at = now;
                t.progress_due = false;
                *cursor += 1;
                deltas.push(Delta::Progress {
                    at: now,
                    cursor: *cursor,
                    id: chunk.request_id.clone(),
                    output_tps: t.entry.output_tps,
                    usage: t.entry.usage,
                });
            } else {
                t.progress_due = true;
            }
        }
        deltas
    }

    fn finished(&mut self, c: RequestComplete) -> Vec<Delta> {
        let Some(mut t) = self.in_flight.remove(&c.request_id) else {
            return Vec::new();
        };
        if let Some(usage) = &c.usage {
            t.usage.merge(usage);
        }
        if let Some(last) = t.entry.attempts.last_mut() {
            let cancelled = c.error_code.as_deref() == Some("account_cancelled");
            if last.outcome == "in_flight" {
                last.outcome = if cancelled {
                    "canceled"
                } else {
                    "transport_error"
                }
                .into();
            } else if last.outcome == "succeeded" && c.outcome.as_str() != "succeeded" {
                // The head committed before the stream failed, so the attempt reads succeeded while the
                // request it served does not: the request's terminal cause is this attempt's too.
                last.outcome = if cancelled {
                    "canceled"
                } else {
                    c.outcome.as_str()
                }
                .into();
            }
        }
        let table = self.pricing.current();
        let e = &mut t.entry;
        e.usage = LiveUsage::from(&t.usage);
        e.cost_usd = table
            .estimate(
                &e.canonical_model,
                e.usage.input,
                e.usage.cache_read,
                e.usage.cache_write_5m,
                e.usage.cache_write_1h,
                e.usage.output,
            )
            .map(|(usd, _)| usd)
            .unwrap_or(0.0);
        e.state = "finished".into();
        e.status = (c.status != 0).then_some(c.status);
        e.error_code = c.error_code.clone();
        e.account_id = c.account_id.clone().or(e.account_id.clone());
        e.phases.finished_at = Some(c.finished_at);
        if e.phases.first_content_at.is_none() {
            e.phases.first_content_at = c.ttfc_ms.map(|ms| e.phases.received_at + ms);
        }
        e.output_tps = rate(&t.output_samples);
        // The preview exists for the in-flight view only.
        e.preview = None;
        self.tile.completed(
            c.finished_at,
            c.outcome.as_str() == "succeeded",
            c.ttfb_ms,
            e.cost_usd,
        );
        // The stream has ended, so whatever the parser still holds is a last event, complete or not.
        let mut deltas = Vec::new();
        if t.captures {
            if let Some(chunk) = delta_text(t.preview.finish().into_iter().collect()) {
                let cursor = self.next_cursor();
                deltas.push(Delta::Preview {
                    cursor,
                    id: c.request_id.clone(),
                    chunk,
                });
            }
        }
        let entry = t.entry.clone();
        self.remember(t.entry);
        self.trim_ring(c.finished_at);
        let cursor = self.next_cursor();
        deltas.push(Delta::Finished {
            at: c.finished_at,
            cursor,
            id: entry.id,
            attempts: entry.attempts,
            cost_usd: entry.cost_usd,
            error_code: entry.error_code,
            phases: entry.phases,
            status: entry.status,
            usage: entry.usage,
        });
        deltas
    }

    /// Inserts at the entry's place by `received_at`, newest first: requests finish in any order, and the
    /// ring's contract is admission order, which is also what the back-only expiry assumes.
    fn remember(&mut self, entry: LiveRequest) {
        let at = self
            .ring
            .iter()
            .position(|r| r.phases.received_at <= entry.phases.received_at)
            .unwrap_or(self.ring.len());
        self.ring.insert(at, entry);
    }

    fn trim_ring(&mut self, now: i64) {
        while self.ring.len() > RING_LIMIT
            || self
                .ring
                .back()
                .is_some_and(|r| now - r.phases.received_at > RING_WINDOW_MS)
        {
            self.ring.pop_back();
        }
    }

    /// Entries silent for the TTL, and the oldest silent ones past the cap: each leaves as a `finished` delta
    /// with `error_code = "lost"`, which is the one word the table has for a request the bus lost track of.
    fn expire(&mut self, now: i64) -> Vec<Delta> {
        let mut gone: Vec<String> = self
            .in_flight
            .iter()
            .filter(|(_, t)| now - t.seen_at >= IN_FLIGHT_TTL_MS)
            .map(|(id, _)| id.clone())
            .collect();
        if self.in_flight.len() - gone.len() > IN_FLIGHT_LIMIT {
            let mut by_silence: Vec<(i64, String)> = self
                .in_flight
                .iter()
                .filter(|(id, _)| !gone.contains(id))
                .map(|(id, t)| (t.seen_at, id.clone()))
                .collect();
            by_silence.sort();
            let excess = self.in_flight.len() - gone.len() - IN_FLIGHT_LIMIT;
            gone.extend(by_silence.into_iter().take(excess).map(|(_, id)| id));
        }
        let mut deltas = Vec::new();
        for id in gone {
            let Some(t) = self.in_flight.remove(&id) else {
                continue;
            };
            let mut e = t.entry;
            e.state = "finished".into();
            e.error_code = Some("lost".into());
            e.preview = None;
            let cursor = self.next_cursor();
            deltas.push(Delta::Finished {
                at: now,
                cursor,
                id,
                attempts: e.attempts,
                cost_usd: e.cost_usd,
                error_code: Some("lost".into()),
                phases: e.phases,
                status: None,
                usage: e.usage,
            });
        }
        deltas
    }

    /// The tick: the tile, plus every progress delta the throttle held since the last one.
    pub fn tick(&mut self) -> Vec<Delta> {
        let now = (self.clock)();
        let mut deltas = Vec::new();
        let due: Vec<String> = self
            .in_flight
            .iter()
            .filter(|(_, t)| t.progress_due && now - t.last_progress_at >= PROGRESS_INTERVAL_MS)
            .map(|(id, _)| id.clone())
            .collect();
        for id in due {
            let (output_tps, usage) = {
                let t = self.in_flight.get_mut(&id).expect("just listed");
                t.progress_due = false;
                t.last_progress_at = now;
                (t.entry.output_tps, t.entry.usage)
            };
            let cursor = self.next_cursor();
            deltas.push(Delta::Progress {
                at: now,
                cursor,
                id,
                output_tps,
                usage,
            });
        }
        deltas.extend(self.expire(now));
        self.trim_ring(now);
        let tick = self.tile.tick(
            now,
            self.in_flight
                .values()
                .filter(|t| t.entry.transport == "sse" && t.entry.phases.head_at.is_some())
                .count(),
        );
        let cursor = self.next_cursor();
        deltas.push(Delta::Tick { cursor, tick });
        deltas
    }

    /// The table and the ring as the socket's role may see them: without `preview`, no output text anywhere.
    pub fn snapshot(&mut self, preview: bool) -> LiveSnapshot {
        let now = (self.clock)();
        self.trim_ring(now);
        let mut in_flight: Vec<LiveRequest> =
            self.in_flight.values().map(|t| t.entry.clone()).collect();
        in_flight.sort_by_key(|r| std::cmp::Reverse(r.phases.received_at));
        if !preview {
            for r in &mut in_flight {
                r.preview = None;
            }
        }
        let streams = in_flight
            .iter()
            .filter(|r| r.transport == "sse" && r.phases.head_at.is_some())
            .count();
        LiveSnapshot {
            cursor: self.cursor,
            in_flight,
            recent: self.ring.iter().cloned().collect(),
            tick: self.tile.current(now, streams),
        }
    }

    pub fn past(&mut self, from: i64, to: i64) -> PastSnapshot {
        self.trim_ring((self.clock)());
        let overlaps = |r: &LiveRequest| {
            let end = r.phases.finished_at.unwrap_or(i64::MAX);
            r.phases.received_at <= to && end >= from
        };
        let mut entries: Vec<LiveRequest> = self
            .ring
            .iter()
            .chain(self.in_flight.values().map(|t| &t.entry))
            .filter(|r| overlaps(r))
            .cloned()
            .map(|mut r| {
                r.preview = None;
                r
            })
            .collect();
        entries.sort_by_key(|r| std::cmp::Reverse(r.phases.received_at));
        let mut density: std::collections::BTreeMap<i64, u64> = std::collections::BTreeMap::new();
        for r in self
            .ring
            .iter()
            .chain(self.in_flight.values().map(|t| &t.entry))
        {
            *density
                .entry(r.phases.received_at.div_euclid(60_000) * 60_000)
                .or_default() += 1;
        }
        PastSnapshot {
            entries,
            density: density
                .into_iter()
                .map(|(at, requests)| DensityBucket { at, requests })
                .collect(),
        }
    }

    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }
}

/// Output tokens per second over the samples kept: the newest running count against the oldest, over the
/// time between them.
fn rate(samples: &VecDeque<(i64, u64)>) -> Option<f64> {
    let (first, last) = (samples.front()?, samples.back()?);
    let span_ms = last.0 - first.0;
    if span_ms <= 0 || last.1 < first.1 {
        return None;
    }
    Some((last.1 - first.1) as f64 * 1_000.0 / span_ms as f64)
}

/// The text of the content deltas `body` completes: `delta.text` of every `content_block_delta` event the
/// request's parser can finish once these bytes are in it. What is left over waits for the next chunk.
fn text_of(parser: &mut SseParser, body: &[u8]) -> Option<String> {
    if parser.feed(body).is_err() {
        // `feed` keeps what it rejected, so a response that closes no event would go on accumulating here
        // for the whole life of the request. The run is given up on and the preview resumes at the next
        // event boundary.
        *parser = SseParser::new(INSPECT_BUFFER);
        return None;
    }
    let mut events = Vec::new();
    while let Some(event) = parser.next_event() {
        events.push(event);
    }
    delta_text(events)
}

fn delta_text(events: Vec<SseEvent>) -> Option<String> {
    let mut out = String::new();
    for event in events {
        if event.name.as_deref() != Some(b"content_block_delta".as_slice()) {
            continue;
        }
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&event.data) else {
            continue;
        };
        if let Some(text) = value
            .get("delta")
            .and_then(|d| d.get("text"))
            .and_then(|t| t.as_str())
        {
            out.push_str(text);
        }
    }
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use phoebis_bus::{Kind, Origin, Outcome, RequestShape, ResponseFacts, USER_AGENT_MAX_BYTES};
    use phoebis_ledger::PriceTable;

    fn observed(id: &str) -> Event {
        observed_capturing(id, 0)
    }

    fn observed_capturing(id: &str, body_capture_hours: u32) -> Event {
        Event::RequestObserved(RequestObserved {
            request_id: id.into(),
            origin: Origin::Client,
            kind: Kind::Inference,
            key_id: None,
            key_generation: 0,
            pool_id: None,
            session_id: None,
            agent_id: None,
            parent_agent_id: None,
            owner_id: None,
            model: "claude-sonnet-5".into(),
            canonical_model: "claude-sonnet-5".into(),
            wire_model: "claude-sonnet-5".into(),
            stream: true,
            headers: Vec::new(),
            body: Bytes::new(),
            body_capture_hours,
            shape: RequestShape::default(),
            presented_retired: false,
            rotation_at: None,
            received_at: 1_000,
        })
    }

    fn output(id: &str, index: u32, running: u64) -> Event {
        Event::ResponseChunk(ResponseChunk {
            request_id: id.into(),
            attempt: 1,
            index,
            body: Bytes::new(),
            phase: Some(Phase::MessageDelta {
                usage: Usage {
                    output_tokens: Some(running),
                    ..Usage::default()
                },
            }),
        })
    }

    fn selected(id: &str) -> Event {
        Event::AccountSelected(AccountSelected {
            request_id: id.into(),
            attempt: 1,
            account_id: "a".into(),
            account_kind: "oauth".into(),
            pin_epoch: None,
            started_at: 1_010,
            cache_ttl_ms: None,
        })
    }

    fn head(id: &str) -> Event {
        Event::ResponseHead(ResponseHead {
            request_id: id.into(),
            attempt: 1,
            status: 200,
            headers: Vec::new(),
            first_event: None,
            outcome: AttemptOutcome::Succeeded,
            cooldown: None,
            written_at: 1_050,
            observed_at: 1_060,
        })
    }

    fn complete(id: &str, outcome: Outcome, error_code: Option<&str>) -> Event {
        Event::RequestComplete(RequestComplete {
            request_id: id.into(),
            attempt: 1,
            account_id: Some("a".into()),
            outcome,
            status: 200,
            usage: None,
            response: ResponseFacts::default(),
            ttfb_ms: Some(60),
            ttfc_ms: Some(70),
            error_code: error_code.map(String::from),
            client_drift: None,
            client_version: None,
            response_bytes: Some(10),
            response_chunks: None,
            finished_at: 1_500,
        })
    }

    fn state() -> LiveState {
        LiveState::new(
            Arc::new(Pricing::new(PriceTable::shipped())),
            Arc::new(|| 1_000),
        )
    }

    /// The head commits before the stream can fail, so the attempt reads succeeded while the request it
    /// served did not. What the console shows for the attempt has to agree with the request beside it.
    #[test]
    fn a_terminal_failure_after_a_committed_head_reaches_the_attempt() {
        let mut state = state();
        for id in ["a", "b"] {
            state.apply(observed(id));
            state.apply(selected(id));
            state.apply(head(id));
        }
        state.apply(complete("a", Outcome::Failed, Some("overloaded_error")));
        state.apply(complete("b", Outcome::Canceled, Some("account_cancelled")));
        let past = state.past(0, i64::MAX);
        let outcome = |id: &str| {
            past.entries
                .iter()
                .find(|r| r.id == id)
                .map(|r| r.attempts[0].outcome.as_str())
        };
        assert_eq!(
            (outcome("a"), outcome("b")),
            (Some("failed"), Some("canceled"))
        );
    }

    /// A client that applies deltas and one that reconnects to the snapshot must show the same request, so
    /// the outcome a completion settles on an attempt travels with the finished delta as well.
    #[test]
    fn the_finished_delta_carries_the_attempts_the_request_ended_with() {
        let mut state = state();
        // "a" loses its connection before a head, "b" after one.
        state.apply(observed("a"));
        state.apply(selected("a"));
        state.apply(observed("b"));
        state.apply(selected("b"));
        state.apply(head("b"));
        let settled = |deltas: Vec<Delta>| {
            deltas.into_iter().find_map(|d| match d {
                Delta::Finished { attempts, .. } => Some(attempts[0].outcome.clone()),
                _ => None,
            })
        };
        assert_eq!(
            settled(state.apply(complete("a", Outcome::Failed, Some("overloaded_error"))))
                .as_deref(),
            Some("transport_error")
        );
        assert_eq!(
            settled(state.apply(complete("b", Outcome::Failed, Some("overloaded_error"))))
                .as_deref(),
            Some("failed")
        );
    }

    fn chunk(id: &str, index: u32, body: &[u8]) -> Event {
        Event::ResponseChunk(ResponseChunk {
            request_id: id.into(),
            attempt: 1,
            index,
            body: Bytes::copy_from_slice(body),
            phase: None,
        })
    }

    /// A transport read is not an event boundary. Split across two of them, a delta parsed per read loses
    /// its name in one half and the start of its JSON in the other, so its text reaches nobody.
    #[test]
    fn a_content_delta_split_across_two_chunks_reaches_the_preview() {
        let mut state = state();
        state.apply(observed_capturing("a", 1));
        let sse = b"event: content_block_delta\ndata: {\"delta\":{\"text\":\"hello world\"}}\n\n";
        let (first, second) = sse.split_at(30);
        state.apply(chunk("a", 0, first));
        state.apply(chunk("a", 1, second));
        assert_eq!(
            state.snapshot(true).in_flight[0].preview.as_deref(),
            Some("hello world")
        );
    }

    /// A run of bytes that closes no event is not a preview, and `feed` keeps what it rejects: held on to,
    /// one such response would sit in the live state for as long as the request is in flight.
    #[test]
    fn a_run_that_closes_no_event_is_dropped_rather_than_held() {
        let mut state = state();
        state.apply(observed_capturing("a", 1));
        state.apply(chunk("a", 0, &vec![b'x'; INSPECT_BUFFER + 1]));
        assert_eq!(
            state.in_flight["a"].preview.buffered(),
            0,
            "the run past the bound is given up on, not carried into the next chunk"
        );
        state.apply(chunk(
            "a",
            1,
            b"event: content_block_delta\ndata: {\"delta\":{\"text\":\"hello\"}}\n\n",
        ));
        assert_eq!(
            state.snapshot(true).in_flight[0].preview.as_deref(),
            Some("hello"),
            "and the preview resumes at the next event boundary"
        );
    }

    #[test]
    fn the_tile_sums_output_across_concurrent_streams() {
        let mut state = LiveState::new(
            Arc::new(Pricing::new(PriceTable::shipped())),
            Arc::new(|| 1_000),
        );
        state.apply(observed("a"));
        state.apply(observed("b"));
        for (index, running) in [(0, 1), (1, 50), (2, 100)] {
            state.apply(output("a", index, running));
            state.apply(output("b", index, running));
        }
        let Some(Delta::Tick { tick, .. }) = state.tick().pop() else {
            panic!("the tick is the last delta")
        };
        assert_eq!(
            tick.metrics.output_tps.value,
            Some(200.0 / 5.0),
            "two streams of 100 tokens inside the five-second window"
        );
    }

    #[test]
    fn a_user_agent_past_the_bound_is_cut_to_it() {
        let mut state = state();
        let Event::RequestObserved(mut request) = observed("a") else {
            unreachable!()
        };
        request.headers = vec![("user-agent".into(), "a".repeat(300))];
        state.apply(Event::RequestObserved(request));
        let entry = state
            .snapshot(false)
            .in_flight
            .into_iter()
            .find(|r| r.id == "a")
            .expect("the request is in flight");
        let user_agent = entry.user_agent.expect("a user agent was observed");
        assert_eq!(user_agent.len(), USER_AGENT_MAX_BYTES);
    }
}
