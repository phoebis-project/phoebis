//! One probe: the recipe replayed through the relay's failover loop under the recipe's key, and the result read
//! from the response the relay returns rather than from the bus, which is best-effort. What the probe cost is
//! settled from the exchange alone; what it does to the recipe is settled only when the recipe is still the
//! one the probe ran for.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request};
use bytes::Bytes;
use phoebis_ledger::{Band, PlanTables, PriceTable, Speed};
use phoebis_relay::{ProbeObstacle, RelayOutcome};
use phoebis_routing::{seconds_ms, Holder, SEVEN_DAY};
use serde_json::Value;

use crate::engine::Engine;
use crate::sessions::state::{
    agent_ref, Agent, AgentState, SessionKey, SessionsState, AGENT_HELD, MAIN_AGENT,
};
use crate::warming::agent::{warming_if_held_mut, warming_mut, warming_ref};
use crate::warming::breakeven::Ceilings;
use crate::warming::causes::{Reason, Wasted};
use crate::warming::gate::Verdict;
use crate::warming::observation::{
    note_ttl_change, Kept, ProbeCharge, Settlement, UnansweredProbe,
};
use crate::warming::recipe::Recipe;
use crate::warming::savings::{
    probe_cost, settlement_unit, typed, unpriced_reason, AttemptFunding, RowClient, SavingsUnit,
};
use crate::warming::state::{Changes, WarmingState};
use crate::warming::usage::{ProbeUsage, MAX_USAGE_BYTES};

/// The engine's own bound on one exchange. A probe with `max_tokens = 0` answers in seconds; the relay's idle
/// timeout bounds a body that stalls mid-way, and this bounds the whole thing so a worker slot never waits on
/// an upstream for longer than the schedule it serves.
pub const PROBE_DEADLINE: Duration = Duration::from_secs(300);

pub(crate) struct ProbeJob {
    pub key: SessionKey,
    pub key_generation: i64,
    pub agent_id: String,
    pub incarnation: u64,
    pub observation_id: String,
    pub generation: u64,
    pub attempt: u8,
    pub started: i64,
    /// The holder that the gate admitted the probe on: the account that last answered the recipe, with its
    /// incarnation.
    pub holder: Holder,
    pub pool_id: String,
    pub canonical_model: String,
    /// The agent's entrypoint at dispatch, which the probe's headers carry, so the probe settles under the regime it
    /// was sent with even when the agent observes another before it lands.
    pub entrypoint: Option<String>,
    /// The observation's client label and user agent at dispatch, taken beside `entrypoint` for the same reason: the
    /// probe's own request row keys the client of the headers it replays, and its settlement keys that client too.
    pub client: RowClient,
    pub recipe: Recipe,
    pub expected: u64,
}

#[derive(Default)]
pub(crate) struct ProbeResult {
    pub status: u16,
    /// The relay's id of the exchange, which the probe's ledger request row carries. `None` for an answer the relay
    /// never built.
    pub request_id: Option<String>,
    /// The instant the relay received the probe's request, off `RelayOutcome::received_at`: the hour and the price
    /// revision the probe's request row files under. `None` for an answer the relay never built.
    pub received_at: Option<i64>,
    pub account: Option<String>,
    /// The relay's own code for an answer it built itself, `accounts_exhausted` and `accounts_unavailable` among them,
    /// and `None` for an upstream answer forwarded as it came.
    pub error_code: Option<String>,
    /// `None` when the read crossed the usage bound or failed: nothing past it is a usage block.
    pub body: Option<Bytes>,
    /// The TTL the relay dispatched the probe's body with, off `RelayOutcome::cache_ttl_ms`; `None` when the
    /// body went as sent.
    pub cache_ttl_ms: Option<i64>,
    /// The served account's kind and class off `RelayOutcome::account_kind` and `RelayOutcome::usage_based`, as the
    /// leg that answered stamped them at its selection, and whether the answer's head said overage was in use, off
    /// `RelayOutcome::overage_in_use`: the facts the ledger files the probe's request row under, carried so the credit
    /// and the settlement file beside it even when the account leaves the registry while the probe is in flight. The
    /// default for an answer the relay built before any selection.
    pub funding: AttemptFunding,
    /// The `Retry-After` header's seconds: the wait of a quota `429` and of a `ledger_behind` hold. `None` for a
    /// missing header or one that does not parse.
    pub retry_after_secs: Option<u64>,
    /// The relay's `RelayOutcome::obstacle`, the standing the agent holds on rather than pausing as an exhausted
    /// pool.
    pub obstacle: Option<ProbeObstacle>,
}

pub(crate) enum Exchange {
    Answered(ProbeResult),
    Canceled,
    TimedOut,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProbeOutcome {
    Canceled,
    Exhausted,
    Failed,
    Held,
    Ok,
    RateLimited,
    Rejected,
    TimedOut,
}

/// The word the probe counter files a probe under: a served probe landed when it read its prefix back and was cold
/// otherwise, and every other outcome is one the upstream did not serve. Every outcome is named, so one added later
/// fails to build here rather than counting as `failed` unread.
pub(crate) fn probe_outcome_label(outcome: ProbeOutcome, cache_read: bool) -> &'static str {
    match outcome {
        ProbeOutcome::Ok if cache_read => "landed",
        ProbeOutcome::Ok => "cold",
        ProbeOutcome::Held => "held",
        ProbeOutcome::Canceled
        | ProbeOutcome::Exhausted
        | ProbeOutcome::Failed
        | ProbeOutcome::RateLimited
        | ProbeOutcome::Rejected
        | ProbeOutcome::TimedOut => "failed",
    }
}

/// The coverage judgment `apply_probe` makes, shared with the probe counter so the counter's `cold` is the streak's:
/// a probe covers by reading back the count `expected` holds, the last request's read and write.
pub(crate) fn covers(usage: &ProbeUsage, expected: u64) -> bool {
    usage.read >= expected
}

/// The counts one probe used and how it ended, from the job and the exchange alone. Whether the recipe it ran for
/// still exists changes nothing here: the upstream served it, so the pool's spend, the metrics, and the ledger
/// carry it either way.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ProbeRecord {
    /// What the probe is charged under, as its own request row files it, carried whole to its settlement.
    pub(crate) charge: ProbeCharge,
    pub(crate) outcome: ProbeOutcome,
}

/// How an upstream `Retry-After` bears on a rung's wait.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RetryWait {
    /// No header: the ladder's step.
    Step,
    /// The quota `429`'s header: it replaces the step only where it is longer.
    AtLeast(i64),
}

/// The unit a probe settles in: the one `settlement_unit` decides under the regime of the probe's `entrypoint`, or
/// dollars where the share it names cannot be priced. Only the unit is kept, since the credit reads its presence and
/// the ledger prices the probe's counts.
#[allow(clippy::too_many_arguments)]
fn settled_unit(
    default_unit: SavingsUnit,
    tables: &PlanTables,
    entrypoint: Option<&str>,
    tier: Option<&str>,
    account: &str,
    canonical_model: &str,
    usage: ProbeUsage,
    at: i64,
) -> SavingsUnit {
    let unit = settlement_unit(default_unit, tables, entrypoint, tier, canonical_model, at);
    if unit == SavingsUnit::WindowShare
        && probe_cost(tables, entrypoint, tier, canonical_model, usage, at).is_none()
    {
        // `settlement_unit` reads `priced`, which asks exactly what `share_at` needs (a weight on the 7-day window and
        // a capacity for the tier on it), so this branch is unreachable. It stays as the defensive read of a `None`.
        tracing::debug!(
            account,
            model = canonical_model,
            unpriced = unpriced_reason(tables, entrypoint, canonical_model, at),
            "a probe the cost model cannot price on the 7-day window share settles in dollars"
        );
        return SavingsUnit::Usd;
    }
    unit
}

/// The `Retry-After` seconds a quota 429 or a `ledger_behind` 503 carries, `None` for a missing header or one that does
/// not parse.
fn retry_after_secs(headers: &axum::http::HeaderMap) -> Option<u64> {
    headers
        .get(header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
}

fn classify_status(status: u16, error_code: Option<&str>) -> ProbeOutcome {
    let exhausted = matches!(
        error_code,
        Some(phoebis_relay::ACCOUNTS_EXHAUSTED) | Some(phoebis_relay::ACCOUNTS_UNAVAILABLE)
    );
    match status {
        200..=299 => ProbeOutcome::Ok,
        429 => ProbeOutcome::RateLimited,
        _ if exhausted => ProbeOutcome::Exhausted,
        400..=499 => ProbeOutcome::Rejected,
        503 if error_code == Some(phoebis_relay::LEDGER_BEHIND) => ProbeOutcome::Held,
        _ => ProbeOutcome::Failed,
    }
}

pub(crate) async fn run_job(engine: Arc<Engine>, mut job: ProbeJob) {
    let mut request = Request::builder().method("POST").uri("/v1/messages");
    for (name, value) in &job.recipe.headers {
        request = request.header(name.as_str(), value.as_str());
    }
    // The job is read for its recipe's speed and TTL alone past here, so the body moves into the request.
    let request = request
        .header(header::CONTENT_LENGTH, job.recipe.body.len())
        .body(Body::from(std::mem::take(&mut job.recipe.body)));
    let marker = phoebis_relay::Probe {
        key_id: job.key.0.clone(),
        key_generation: job.key_generation,
        holder: job.holder.clone(),
    };
    // Read before the send, so an exchange the relay never answered keys the instant the relay would have received it.
    let sent = engine.now();
    let exchange = match request {
        Err(_) => Exchange::Answered(ProbeResult::default()),
        Ok(request) => {
            let relay = engine.parts.relay.clone();
            // Through the relay, never around it: a probe has to take the same admission, selection, edit
            // (the billing-hash restamp among them), and failover path a real request takes, or it would warm
            // an entry no real request can read back. The answer it returns is the only result the engine
            // trusts; the bus is best-effort, and the TTL the entry actually got is learned from this body's
            // usage rather than assumed from the TTL the recipe asked for.
            let answered = async {
                let response = relay.probe(marker, request).await;
                let status = response.status().as_u16();
                let outcome = response.extensions().get::<RelayOutcome>().cloned();
                let retry_after_secs = retry_after_secs(response.headers());
                // A response that keeps sending stops at the usage bound: the first frame past it ends the read, and
                // the response is dropped with it.
                let body = axum::body::to_bytes(response.into_body(), MAX_USAGE_BYTES)
                    .await
                    .ok();
                match outcome {
                    Some(o) => ProbeResult {
                        status,
                        request_id: Some(o.request_id),
                        received_at: Some(o.received_at),
                        account: Some(o.account_id).filter(|a| !a.is_empty()),
                        error_code: o.error_code,
                        body,
                        cache_ttl_ms: o.cache_ttl_ms,
                        funding: AttemptFunding {
                            kind: o.account_kind,
                            usage_based: o.usage_based,
                            in_overage: o.overage_in_use == Some(true),
                        },
                        retry_after_secs,
                        obstacle: o.obstacle,
                    },
                    None => ProbeResult {
                        status,
                        body,
                        retry_after_secs,
                        ..ProbeResult::default()
                    },
                }
            };
            // The whole exchange, the head and the body, sits in one arm: a cancellation or the deadline ends
            // it wherever it is, and a body that keeps coming never outlives either.
            tokio::select! {
                result = answered => Exchange::Answered(result),
                _ = engine.cancel.cancelled() => Exchange::Canceled,
                _ = tokio::time::sleep(PROBE_DEADLINE) => Exchange::TimedOut,
            }
        }
    };
    let now = engine.now();
    // The relay files the probe's request row at its receipt, on the revision in force then, so the record keys that
    // instant and the settlement prices beside the row even when the answer lands past an hour's end or a revision's
    // start. The completion stays the clock the schedule reads.
    let at = match &exchange {
        Exchange::Answered(result) => result.received_at.unwrap_or(sent),
        Exchange::Canceled | Exchange::TimedOut => sent,
    };
    let revisions = engine.parts.pricing.revisions();
    let table = revisions.in_force(at);
    let tables = engine.parts.tables.clone();
    let mut record = probe_record(&job, &exchange, table, at);
    // The serving account's kind decides the unit this probe's cost settles in. It rides the relay's answer, so a
    // registry write mid-exchange never changes which account this probe is charged against, and an answer the relay
    // never built names no account: its probe settles in dollars and the credit path never matches it.
    // A fast answer draws usage credits whatever the account's funding, so it settles in dollars. The answer's speed
    // decides rather than the recipe's: a fast recipe answered at standard speed spent the plan, as its request row
    // files it.
    let funding = record.charge.funding;
    let default_unit = match funding.kind {
        Some(kind) if record.charge.usage.speed != Speed::Fast => {
            SavingsUnit::of(kind, funding.usage_based, funding.in_overage)
        }
        _ => SavingsUnit::Usd,
    };
    let snapshot = engine.parts.router.snapshot();
    let account = record.charge.account.as_deref().unwrap_or_default();
    // The tier the router prices the account by, as its snapshot holds it, the read the kept-warm credit makes, so
    // the probe's row, its cost and its credit carry one tier. None for a free organization, a usage-based seat and
    // a plan the cost model does not price, which settle in dollars.
    record.charge.tier = (default_unit == SavingsUnit::WindowShare)
        .then(|| snapshot.priced_tier(account).map(String::from))
        .flatten();
    let unit = settled_unit(
        default_unit,
        &tables,
        job.entrypoint.as_deref(),
        record.charge.tier.as_deref(),
        account,
        &job.canonical_model,
        record.charge.usage,
        at,
    );
    let covered_unit = funding.kind.map(|_| unit);
    engine.apply_probe(&job, exchange, &record, covered_unit, now);
    // Read off the answer rather than off what the fenced half did with it, so a probe whose recipe moved on while it
    // was in flight, or paused on its answer, still counts as the upstream served it.
    let cache_read = covers(&record.charge.usage, job.expected);
    engine.parts.metrics.observe_probe(
        &job.pool_id,
        probe_outcome_label(record.outcome, cache_read),
    );
    engine.flush();
    // The worker slot comes back only after both sends: `Engine::stats` reads a zero count only then, and a caller
    // that asserts on the ledger right after it would otherwise race the records this probe owes.
    engine.lock().warming.running -= 1;
}

/// The immutable half: the outcome the exchange had, the counts it used and their prompt-length band on `table`,
/// the price revision in force at `at`, the relay's receipt of the probe. The record files under the account that
/// served the probe, which the answer names.
pub(crate) fn probe_record(
    job: &ProbeJob,
    exchange: &Exchange,
    table: &PriceTable,
    at: i64,
) -> ProbeRecord {
    let mut record = ProbeRecord {
        charge: ProbeCharge {
            account: Some(job.holder.account_id.clone()),
            at,
            band: Band::Standard,
            client: job.client.clone(),
            funding: AttemptFunding::default(),
            tier: None,
            usage: ProbeUsage::default(),
        },
        outcome: ProbeOutcome::Failed,
    };
    let result = match exchange {
        Exchange::Canceled => {
            record.outcome = ProbeOutcome::Canceled;
            return record;
        }
        Exchange::TimedOut => {
            record.outcome = ProbeOutcome::TimedOut;
            return record;
        }
        Exchange::Answered(result) => result,
    };
    record.charge.funding = result.funding;
    if let Some(account) = &result.account {
        record.charge.account = Some(account.clone());
    }
    record.outcome = classify_status(result.status, result.error_code.as_deref());
    if record.outcome != ProbeOutcome::Ok {
        return record;
    }
    if let Some(usage) = result.body.as_deref().and_then(ProbeUsage::parse) {
        record.charge.usage = usage;
        let counts = typed(usage);
        record.charge.band = table.band_of(&job.canonical_model, counts.prompt_tokens());
    }
    record
}

/// The TTL a served probe asked for, the one its body was dispatched with over the recipe's own, and the TTL its entry
/// took as `usage` proves it over `previous_ms`, one reading for a probe the idle limit keeps and for one that lands.
fn entry_ttl(
    job: &ProbeJob,
    result: &ProbeResult,
    usage: &ProbeUsage,
    previous_ms: i64,
) -> (i64, i64) {
    let requested_ms = result.cache_ttl_ms.unwrap_or(job.recipe.ttl_ms);
    (requested_ms, usage.ttl_ms(previous_ms, requested_ms))
}

impl WarmingState {
    /// The job of a due probe on the holder the gate admitted, read off the agent's two records, with the worker it
    /// reserves.
    pub(crate) fn dispatch_probe(
        &mut self,
        sessions: &SessionsState,
        key: &SessionKey,
        agent_id: &str,
        holder: Holder,
        now: i64,
    ) -> (Changes, ProbeJob) {
        let agent = agent_ref(&sessions.entries, key, agent_id).expect(AGENT_HELD);
        let warming = warming_mut(&mut self.agents, key, agent_id).expect(AGENT_HELD);
        let observation = warming.current.as_ref().unwrap();
        let job = ProbeJob {
            key: key.clone(),
            key_generation: warming.key_generation,
            agent_id: agent_id.to_string(),
            incarnation: agent.incarnation,
            observation_id: observation.id.clone(),
            generation: agent.generation,
            attempt: observation.attempt,
            started: now,
            holder,
            pool_id: warming.pool_id.clone(),
            canonical_model: warming.canonical_model.clone(),
            entrypoint: warming.entrypoint.clone(),
            client: observation.row_client(),
            recipe: observation.recipe.clone(),
            expected: observation.expected,
        };
        warming.in_flight = true;
        warming.hold_counted = false;
        warming.probes += 1;
        warming.count_probe(now);
        self.running += 1;
        let changes = self.set_reason(sessions, key, agent_id, Reason::Probing);
        (changes, job)
    }

    pub(crate) fn release_probe_slot(&mut self, sessions: &SessionsState, job: &ProbeJob) {
        let key = &job.key;
        let agent_id = job.agent_id.as_str();
        // The worker slot comes back to the record the probe started on whatever the fence says. A result
        // orphaned on a record that is still alive, by a superseding request or by a discard, would otherwise
        // leave `in_flight` set, and `step` skips an agent that has a probe in flight for good.
        if let Some(warming) = agent_ref(&sessions.entries, key, agent_id)
            .filter(|agent| agent.incarnation == job.incarnation)
            .map(|_| warming_mut(&mut self.agents, key, agent_id).expect(AGENT_HELD))
        {
            warming.in_flight = false;
        }
    }

    /// The fenced half, the Go `runProbe` after the call returned. The fence is the agent's incarnation and
    /// generation and the observation's id and attempt; a result that fails it touches no recipe and settles
    /// as one wasted probe under the stop that orphaned it, since the upstream served it either way. A held result
    /// asked no account and settles under none, and a success the idle limit orphaned waits as a pending probe.
    pub(crate) fn apply_probe(
        &mut self,
        sessions: &SessionsState,
        job: &ProbeJob,
        exchange: Exchange,
        record: &ProbeRecord,
        covered_unit: Option<SavingsUnit>,
        now: i64,
    ) -> Changes {
        let mut changes = Changes::default();
        let key = &job.key;
        let agent_id = job.agent_id.as_str();
        let charge = record.charge.clone();
        self.release_probe_slot(sessions, job);
        let on_incarnation = agent_ref(&sessions.entries, key, agent_id)
            .is_some_and(|agent| agent.incarnation == job.incarnation);
        let fenced = agent_ref(&sessions.entries, key, agent_id).is_some_and(|agent| {
            agent.incarnation == job.incarnation
                && agent.generation == job.generation
                && warming_ref(&self.agents, key, agent_id)
                    .expect(AGENT_HELD)
                    .current
                    .as_ref()
                    .is_some_and(|o| o.id == job.observation_id && o.attempt == job.attempt)
        }) && sessions.live(key, agent_id);
        if !fenced {
            // A record still on the incarnation the probe started on, carrying no stop label, was superseded by
            // a real request rather than stopped: the request the probe was paying for arrived, so the probe is
            // not wasted. A record that is gone leaves no label to read, so what removed it is what labels the
            // probe: a session that went is a session that ended, and anything else replaced the record.
            let label = if on_incarnation {
                warming_ref(&self.agents, key, agent_id)
                    .expect(AGENT_HELD)
                    .last_stop
            } else if sessions.entries.contains_key(key) {
                Some(Wasted::Replaced)
            } else {
                Some(Wasted::SessionEnded)
            };
            let Some(label) = label else {
                return changes;
            };
            if record.outcome == ProbeOutcome::Held {
                return changes;
            }
            // The idle limit keeps its probes pending until their entry lapses or a turn reads them, so a success
            // still in flight at the ceiling joins them by the entry it wrote, computed as the landing computes it.
            if label == Wasted::IdleLimit && record.outcome == ProbeOutcome::Ok {
                if let Exchange::Answered(result) = &exchange {
                    let warming = warming_mut(&mut self.agents, key, agent_id).expect(AGENT_HELD);
                    let (_, ttl) = entry_ttl(job, result, &record.charge.usage, warming.ttl_ms);
                    warming.kept.push(Kept {
                        key_generation: job.key_generation,
                        pool_id: job.pool_id.clone(),
                        canonical_model: job.canonical_model.clone(),
                        charge,
                        fast: job.recipe.fast,
                        entry_expires: job.started + ttl,
                        // The landing's coverage checks never ran on this path, so the probe earns no credit,
                        // and a turn on its lineage that reads the cache leaves it unwasted.
                        covered: false,
                        unit: covered_unit,
                        window: SEVEN_DAY,
                        pending: Some(Wasted::IdleLimit),
                    });
                    changes.agent_row(key, agent_id);
                    return changes;
                }
            }
            self.settlements.push(Settlement {
                key: key.clone(),
                key_generation: job.key_generation,
                pool_id: job.pool_id.clone(),
                canonical_model: job.canonical_model.clone(),
                label,
                charge,
            });
            return changes;
        }
        let warming = warming_mut(&mut self.agents, key, agent_id).expect(AGENT_HELD);
        let cold_limit = warming.timing.cold_probe_limit;
        let answered = match exchange {
            Exchange::Answered(result) => Some(result),
            Exchange::Canceled | Exchange::TimedOut => None,
        };
        let result = match answered {
            Some(result) if record.outcome == ProbeOutcome::Ok => result,
            answered => {
                changes.append(self.probe_error(
                    sessions,
                    key,
                    agent_id,
                    record.outcome,
                    answered.as_ref(),
                    now,
                ));
                return changes;
            }
        };
        // The upstream served this one and the pool was charged for it, so it goes on the recipe's own books
        // here, before any branch below pauses the recipe over what the answer said: the settlement a stop leaves
        // would otherwise disagree with the ledger. Every probe since the last real request is unanswered until one
        // arrives.
        // The end of the entry this probe wrote, computed once as the landing reads it, so a probe a check below
        // pauses still settles by its own entry.
        let previous_ms = warming.ttl_ms;
        let (requested_ms, ttl) = entry_ttl(job, &result, &record.charge.usage, previous_ms);
        warming.unanswered.push(UnansweredProbe {
            id: result.request_id.clone().unwrap_or_default(),
            charge,
            unit: covered_unit,
            window: SEVEN_DAY,
            fast: job.recipe.fast,
            model: warming.canonical_model.clone(),
            expires: job.started + ttl,
            covered: false,
        });
        if result.account.is_none() {
            changes.append(self.pause(sessions, key, agent_id, Reason::MissingAccount));
            return changes;
        }
        let body = result.body.unwrap_or_default();
        let root: Option<Value> = serde_json::from_slice(&body).ok();
        let output_known = root
            .as_ref()
            .and_then(|r| r.pointer("/usage/output_tokens"))
            .is_some();
        let content = root
            .as_ref()
            .and_then(|r| r.get("content"))
            .and_then(Value::as_array)
            .cloned();
        let (Some(usage), true, Some(content)) = (ProbeUsage::parse(&body), output_known, content)
        else {
            changes.append(self.pause(sessions, key, agent_id, Reason::InvalidProbeUsage));
            return changes;
        };
        warming.usage.add(usage);
        if usage.output != 0 || !content.is_empty() {
            changes.append(self.pause(sessions, key, agent_id, Reason::UnexpectedOutput));
            return changes;
        }
        // Full coverage is the whole prefix read back, which is what the next real request would read. A write
        // beside it is the prefix rebuilt, which the cold streak counts.
        let covered = covers(&usage, job.expected);
        if covered {
            warming.cold = 0;
        } else {
            warming.cold += 1;
        }
        let cold = warming.cold;
        if cold >= cold_limit {
            changes.append(self.pause(sessions, key, agent_id, Reason::RepeatedColdProbe));
            return changes;
        }
        if usage.evidence() == 0 {
            changes.append(self.retry(
                sessions,
                key,
                agent_id,
                Reason::NoProbeCacheEvidence,
                now,
                RetryWait::Step,
            ));
            return changes;
        }
        // A probe that read none of the prefix and wrote all of it rebuilt the entry, which the cold streak counts
        // instead. Any other probe that does not cover never warmed the whole entry.
        let rebuilt = usage.read == 0 && usage.evidence() >= job.expected;
        if !covered && !rebuilt {
            changes.append(self.pause(sessions, key, agent_id, Reason::PartialCacheCoverage));
            return changes;
        }
        warming.failures = 0;
        warming.retry_at = None;
        warming.ttl_ms = ttl;
        note_ttl_change(
            key,
            agent_id,
            previous_ms,
            ttl,
            requested_ms,
            &record.charge.usage,
        );
        warming.cache_started = job.started;
        warming.expires = Some(job.started + ttl);
        warming.jitter_frac = rand::random::<f64>();
        // A full read and an accepted complete rewrite alike leave the whole prefix in an entry the next request
        // can read, so either covers from here.
        if let Some(probe) = warming.unanswered.last_mut() {
            probe.covered = true;
        }
        changes.append(self.set_reason(sessions, key, agent_id, Reason::Scheduled));
        // A probe that consumed its whole refresh interval must not start a tight loop.
        if warming_ref(&self.agents, key, agent_id)
            .expect(AGENT_HELD)
            .due_at()
            .is_some_and(|due| due <= now)
        {
            changes.append(self.retry(
                sessions,
                key,
                agent_id,
                Reason::SlowProbe,
                now,
                RetryWait::Step,
            ));
        }
        changes
    }

    pub(crate) fn pause(
        &mut self,
        sessions: &SessionsState,
        key: &SessionKey,
        agent_id: &str,
        reason: Reason,
    ) -> Changes {
        let changes = self.set_reason(sessions, key, agent_id, reason);
        if let Some(warming) = warming_if_held_mut(&mut self.agents, sessions, key, agent_id) {
            warming.pause();
        }
        changes
    }

    /// The Go `probeErrorLocked`, read from the answer the relay gave. A 429 after failover, or the exhaustion 429
    /// the relay answers when every account is cooled, retries on the ladder under `probe_quota_exhausted`, its
    /// `Retry-After` replacing the step where it is longer. A probe the selector refused on the pinned member's spend
    /// budget holds under `budget` without a pause, since only a raised budget or the period's end clears it, never a
    /// wait. A probe the router refused with `ledger_behind` holds under it for the answer's wait, counting no
    /// failure. A probe the selector answered on its pinned member's standing holds under that standing:
    /// `Paused` for the operator's pause and for a deleted member or one in a state other than ready,
    /// `IneligibleCredential` for a subscription that ended between the gate's read and the selection (both read
    /// `paused` in the console), a retry for a member this probe already tried or for one whose budget the degraded
    /// ledger leaves unknown, which the ledger's recovery clears without a real request, and `NoPin` for a session that
    /// held no pin to renew, paused until its next real request. A probe whose pin stands on another holder than its
    /// recipe's holds under `pin_moved` without a pause, so the gate reads the pin again at each tick and releases the
    /// recipe once a request arms it on the pin's account. A pinned member's cooldown, credits block, overage or
    /// spend cap answers a 429, which retries under `probe_quota_exhausted` as above. A pinned member a leg of the
    /// probe marked for re-authentication pauses under the authentication reason. The untimed exhaustion 503, when
    /// every account of the pool was unusable at selection, retries under its own reason. A 5xx forwarded from upstream
    /// retries. A 403 under `client_contract` or `client_drift` refuses the probe's own request shape and pauses as an
    /// unsupported probe, any other 401 or 403 pauses under the authentication reason, any other 4xx is a shape the
    /// upstream refuses, and a canceled, timed-out, or lost exchange (status 0) retries as a transport error.
    pub(crate) fn probe_error(
        &mut self,
        sessions: &SessionsState,
        key: &SessionKey,
        agent_id: &str,
        outcome: ProbeOutcome,
        result: Option<&ProbeResult>,
        now: i64,
    ) -> Changes {
        // An exchange the relay never answered reads as status 0, with no code, obstacle or wait.
        let status = result.map_or(0, |result| result.status);
        let error_code = result.and_then(|result| result.error_code.as_deref());
        let obstacle = result.and_then(|result| result.obstacle);
        let exhausted = outcome == ProbeOutcome::Exhausted;
        let retry_after_ms = result
            .and_then(|result| result.retry_after_secs)
            .map(seconds_ms);
        // Every obstacle is named, so one added later fails to build here until it is given its hold.
        match (exhausted, obstacle) {
            (true, Some(ProbeObstacle::Operator)) => {
                return self.pause(sessions, key, agent_id, Reason::Paused);
            }
            (true, Some(ProbeObstacle::SubscriptionEnded)) => {
                return self.pause(sessions, key, agent_id, Reason::IneligibleCredential);
            }
            // A moved key's next probe admits under its new pool.
            (
                true,
                Some(
                    ProbeObstacle::Attempted
                    | ProbeObstacle::BudgetUnknown
                    | ProbeObstacle::PoolMoved,
                ),
            ) => {
                return self.retry(
                    sessions,
                    key,
                    agent_id,
                    Reason::RetryableProbeError,
                    now,
                    RetryWait::Step,
                )
            }
            (true, Some(ProbeObstacle::ReauthRequired)) => {
                return self.pause(sessions, key, agent_id, Reason::ProbeAuthenticationError);
            }
            // A retry would send a probe that still has no pin to renew, so the recipe waits for the session's next
            // real request.
            (true, Some(ProbeObstacle::NoPin)) => {
                return self.pause(sessions, key, agent_id, Reason::NoPin);
            }
            // The pin moved between the gate's read and the selection: the hold the gate itself places and reads live
            // at each tick, so the reason alone leaves the schedule armed rather than pausing the recipe.
            (true, Some(ProbeObstacle::PinMoved)) => {
                return self.set_reason(sessions, key, agent_id, Reason::PinMoved);
            }
            (_, Some(ProbeObstacle::Budget)) if outcome == ProbeOutcome::RateLimited => {
                // The pinned member reached its spend budget between the gate's read and the selection. A pause
                // would outlive the budget (nothing but a real request undoes one), and the reason alone leaves
                // the schedule armed, so the gate re-reads the spend on the next poll and releases the probe
                // itself when the period ends or the budget is raised.
                return self.set_reason(sessions, key, agent_id, Reason::Budget);
            }
            // The four waits reach here as a 429, which retries on the ladder below with the answer's wait.
            (
                false,
                Some(
                    ProbeObstacle::Attempted
                    | ProbeObstacle::BudgetUnknown
                    | ProbeObstacle::NoPin
                    | ProbeObstacle::Operator
                    | ProbeObstacle::PinMoved
                    | ProbeObstacle::PoolMoved
                    | ProbeObstacle::ReauthRequired
                    | ProbeObstacle::SubscriptionEnded,
                ),
            )
            | (
                _,
                Some(
                    ProbeObstacle::Budget
                    | ProbeObstacle::Cooldown
                    | ProbeObstacle::Credits
                    | ProbeObstacle::Overage
                    | ProbeObstacle::Spent,
                ),
            )
            | (_, None) => {}
        }
        if outcome == ProbeOutcome::Held {
            // The router's own standing, not the account's: no failure is counted, so the cold-probe limit never
            // pauses a recipe for a ledger that is catching up, and the entry's own lapse bounds the re-sends.
            let warming = warming_mut(&mut self.agents, key, agent_id).expect(AGENT_HELD);
            let wait_ms = retry_after_ms.unwrap_or(warming.timing.retry_ms);
            warming.retry_at = Some(now.saturating_add(wait_ms));
            return self.set_reason(sessions, key, agent_id, Reason::LedgerBehind);
        }
        let (reason, wait) = match status {
            _ if outcome == ProbeOutcome::RateLimited => (
                Reason::ProbeQuotaExhausted,
                retry_after_ms.map_or(RetryWait::Step, RetryWait::AtLeast),
            ),
            _ if exhausted => (Reason::ProbePoolExhausted, RetryWait::Step),
            500..=599 => (Reason::RetryableProbeError, RetryWait::Step),
            // Under these codes the relay's 403 refuses the probe's own request shape, not the credential.
            _ if matches!(error_code, Some("client_contract") | Some("client_drift")) => {
                return self.pause(sessions, key, agent_id, Reason::UnsupportedProbe);
            }
            401 | 403 => {
                return self.pause(sessions, key, agent_id, Reason::ProbeAuthenticationError);
            }
            400..=499 => {
                return self.pause(sessions, key, agent_id, Reason::UnsupportedProbe);
            }
            _ => (Reason::ProbeTransportError, RetryWait::Step),
        };
        self.retry(sessions, key, agent_id, reason, now, wait)
    }

    /// The ladder: `retry_ms` doubled per consecutive error up to `retry_max_ms`, `wait` resolving the header against
    /// the step, the wait capped at `retry_max_ms`. The entry's TTL bounds the ladder: a rung due at or past the
    /// entry's expiry is not scheduled, and the failure that would plan it pauses under `ProbeRetryLimit`, since
    /// `Engine::step` lapses the entry at that instant before it reads a due probe.
    pub(crate) fn retry(
        &mut self,
        sessions: &SessionsState,
        key: &SessionKey,
        agent_id: &str,
        reason: Reason,
        now: i64,
        wait: RetryWait,
    ) -> Changes {
        let warming = warming_mut(&mut self.agents, key, agent_id).expect(AGENT_HELD);
        warming.failures += 1;
        let exponent = (warming.failures - 1).min(4);
        let step = (warming.timing.retry_ms << exponent).min(warming.timing.retry_max_ms);
        let delay = match wait {
            RetryWait::Step => step,
            RetryWait::AtLeast(ms) => ms.max(step),
        }
        .min(warming.timing.retry_max_ms);
        if warming
            .expires
            .is_some_and(|expires| now + delay >= expires)
        {
            return self.pause(sessions, key, agent_id, Reason::ProbeRetryLimit);
        }
        warming.retry_at = Some(now + delay);
        self.set_reason(sessions, key, agent_id, reason)
    }
}

/// The ceilings over one agent at `now`, active while any of them says probe. The idle window is main's in every
/// state, from the later of `last_activity` (a real request, a prompt or a presence report) and `park_ended_at`. A
/// park on a background command or a subagent is probed for `parked_ms` from its start, and one on a scheduled
/// wakeup while the wake is at most `horizon_ms` away, held under `horizon` beyond it. A busy subagent has no
/// ceiling of its own. A silent session reads the window alone, from the session's last real request, for every
/// agent: nothing reports its parks any more.
pub(crate) fn ceiling(
    agent: &Agent,
    attended: bool,
    session_last_real_at: Option<i64>,
    ceilings: &Ceilings,
    now: i64,
) -> Verdict {
    if !attended {
        let since = session_last_real_at.unwrap_or(agent.last_activity);
        return if now - since < ceilings.idle_ms {
            Verdict::Probe
        } else {
            Verdict::End(Reason::IdleLimit)
        };
    }
    let main = agent.id == MAIN_AGENT;
    if main {
        let since = agent.idle_window_start();
        if now - since < ceilings.idle_ms {
            return Verdict::Probe;
        }
    }
    match agent.state {
        AgentState::Parked => {
            // A park that passed leaves a worker parked with no park until the mod restates it, and a restored
            // row with no kind lands here too: no rule says probe for a park that ended.
            let Some(held) = agent.park else {
                return Verdict::End(Reason::ParkedLimit);
            };
            let park = held.park;
            if park.kind().background() && now - held.began < ceilings.parked_ms {
                return Verdict::Probe;
            }
            match park.wake_at() {
                Some(wake) if wake - now <= ceilings.horizon_ms => Verdict::Probe,
                // The wake is a certain return the hold keeps the prompt for, so it outranks a command's part
                // whose ceiling ran out.
                Some(_) => Verdict::Hold(Reason::Horizon),
                None => Verdict::End(Reason::ParkedLimit),
            }
        }
        AgentState::Busy if !main => Verdict::Probe,
        // A subagent is busy, parked or released: in any other state no rule says probe for it.
        _ if !main => Verdict::End(Reason::ParkedLimit),
        _ => Verdict::End(Reason::IdleLimit),
    }
}

pub(crate) fn wasted_label(reason: Reason) -> Option<Wasted> {
    match reason {
        Reason::Horizon => Some(Wasted::Horizon),
        Reason::IdleLimit => Some(Wasted::IdleLimit),
        Reason::ParkedLimit => Some(Wasted::ParkedLimit),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::engine::Inner;
    use crate::sessions::config::SessionCaps;
    use crate::sessions::state::{agent_mut, Agent, Session};
    use crate::warming::agent::AgentWarming;
    use crate::warming::config::{KeepaliveLimits, KeepaliveTiming};
    use phoebis_ledger::PriceRevisions;

    use super::*;

    /// The relay's answer to a probe, with the fields `probe_error` reads.
    fn answered(
        status: u16,
        error_code: Option<&str>,
        obstacle: Option<ProbeObstacle>,
        retry_after_secs: Option<u64>,
    ) -> ProbeResult {
        ProbeResult {
            status,
            error_code: error_code.map(String::from),
            obstacle,
            retry_after_secs,
            ..ProbeResult::default()
        }
    }

    /// One session holding one never-probed agent, the minimum `probe_error`/`retry` need, built onto the same
    /// `Inner::empty` a fresh engine starts from.
    fn one_agent_inner() -> (Inner, SessionKey) {
        let key: SessionKey = ("k".into(), "s".into());
        let mut agents = BTreeMap::new();
        agents.insert("main".to_string(), Agent::new("main", "k", 1, 0));
        let mut inner = Inner::empty(
            KeepaliveTiming::default(),
            KeepaliveLimits::default(),
            SessionCaps::default(),
            0,
        );
        inner.sessions.entries.insert(
            key.clone(),
            Session {
                key_generation: 0,
                instance: "i".into(),
                sequence: 0,
                lease: i64::MAX,
                attended: true,
                ended: false,
                last_report_at: 0,
                last_real_at: Some(0),
                slotted: true,
                restored_until: None,
                instance_mismatch_warned: false,
                agents,
                cache_warming: true,
            },
        );
        inner.warming.agents.insert(
            key.clone(),
            BTreeMap::from([(
                "main".to_string(),
                AgentWarming::new(0, KeepaliveTiming::default()),
            )]),
        );
        (inner, key)
    }

    /// A job for `one_agent_inner`'s agent at generation 0, the recipe's first.
    fn job_on(key: &SessionKey) -> ProbeJob {
        ProbeJob {
            key: key.clone(),
            key_generation: 0,
            agent_id: "main".into(),
            incarnation: 1,
            observation_id: "o".into(),
            generation: 0,
            attempt: 1,
            started: 1_000,
            holder: Holder {
                account_id: "a".into(),
                incarnation: 1,
            },
            pool_id: "main".into(),
            canonical_model: "m".into(),
            entrypoint: None,
            client: RowClient::default(),
            recipe: Recipe {
                body: Bytes::new(),
                headers: Vec::new(),
                model: "m".into(),
                ttl_ms: 0,
                longest_ttl_ms: 0,
                size: 0,
                static_hash: None,
                tail_hash: None,
                fast: false,
            },
            expected: 0,
        }
    }

    /// The router's own `503 ledger_behind`, which asked no account.
    fn held_answer() -> Exchange {
        Exchange::Answered(ProbeResult {
            status: 503,
            request_id: Some("p".into()),
            received_at: Some(2_000),
            error_code: Some(phoebis_relay::LEDGER_BEHIND.into()),
            retry_after_secs: Some(5),
            ..ProbeResult::default()
        })
    }

    /// A held probe asked no account, so it never counts as wasted: a `Held` result orphaned by a stop or by the
    /// session's end writes no settlement under the label that would otherwise settle it.
    #[test]
    fn a_held_result_of_an_orphaned_probe_writes_no_settlement() {
        for orphaned_by in ["release", "session_end"] {
            let (mut inner, key) = one_agent_inner();
            let job = job_on(&key);
            match orphaned_by {
                "release" => {
                    let agent =
                        agent_mut(&mut inner.sessions.entries, &key, "main").expect(AGENT_HELD);
                    let warming =
                        warming_mut(&mut inner.warming.agents, &key, "main").expect(AGENT_HELD);
                    agent.generation = 1;
                    warming.last_stop = Some(Wasted::Released);
                }
                _ => {
                    inner.sessions.entries.clear();
                    inner.warming.agents.clear();
                }
            }
            let exchange = held_answer();
            let record = probe_record(
                &job,
                &exchange,
                PriceRevisions::shipped().in_force(2_000),
                2_000,
            );
            assert_eq!(record.outcome, ProbeOutcome::Held, "{orphaned_by}");
            let changes =
                inner
                    .warming
                    .apply_probe(&inner.sessions, &job, exchange, &record, None, 2_000);
            inner.apply_changes(changes);
            assert!(
                inner.warming.settlements.is_empty(),
                "{orphaned_by}: the held probe settles under no label"
            );
        }
    }

    #[test]
    fn a_pinned_answer_holds_the_agent_under_its_obstacle() {
        for (obstacle, reason, paused) in [
            (ProbeObstacle::Operator, Reason::Paused, true),
            (
                ProbeObstacle::SubscriptionEnded,
                Reason::IneligibleCredential,
                true,
            ),
            (ProbeObstacle::Attempted, Reason::RetryableProbeError, false),
            (ProbeObstacle::NoPin, Reason::NoPin, true),
            (ProbeObstacle::PinMoved, Reason::PinMoved, false),
        ] {
            let (mut inner, key) = one_agent_inner();
            let changes = inner.warming.probe_error(
                &inner.sessions,
                &key,
                "main",
                ProbeOutcome::Exhausted,
                Some(&answered(
                    503,
                    Some("accounts_unavailable"),
                    Some(obstacle),
                    None,
                )),
                1_000,
            );
            inner.apply_changes(changes);
            let warming = &inner.warming.agents[&key]["main"];
            assert_eq!(
                (warming.reason, warming.paused),
                (reason, paused),
                "the {obstacle:?} answer holds the agent under its own standing, not the exhausted pool"
            );
        }
    }

    /// A wait on the pinned member clears on its own, so the probe goes back on the ladder no sooner than the
    /// answer's header rather than pausing until a real request.
    #[test]
    fn a_pinned_cooldowns_429_retries_after_its_header_under_probe_quota_exhausted() {
        let (mut inner, key) = one_agent_inner();
        let changes = inner.warming.probe_error(
            &inner.sessions,
            &key,
            "main",
            ProbeOutcome::RateLimited,
            Some(&answered(
                429,
                Some("accounts_exhausted"),
                Some(ProbeObstacle::Cooldown),
                Some(120),
            )),
            1_000,
        );
        inner.apply_changes(changes);
        let warming = &inner.warming.agents[&key]["main"];
        assert_eq!(
            (warming.paused, warming.reason, warming.retry_at),
            (false, Reason::ProbeQuotaExhausted, Some(121_000)),
            "the header's 120 seconds outlast the ladder's first rung"
        );
    }

    #[test]
    fn a_probe_refused_for_a_moved_key_retries_under_the_new_pool_rather_than_pausing_as_exhausted()
    {
        let (mut inner, key) = one_agent_inner();
        let changes = inner.warming.probe_error(
            &inner.sessions,
            &key,
            "main",
            ProbeOutcome::Exhausted,
            Some(&answered(
                503,
                Some("accounts_unavailable"),
                Some(ProbeObstacle::PoolMoved),
                Some(1),
            )),
            1_000,
        );
        inner.apply_changes(changes);
        let warming = &inner.warming.agents[&key]["main"];
        assert_eq!(
            (warming.reason, warming.paused),
            (Reason::RetryableProbeError, false),
            "the key's next request admits under its new pool, so the probe retries"
        );
    }

    #[test]
    fn a_quota_429_retries_on_the_first_rung_under_probe_quota_exhausted() {
        let (mut inner, key) = one_agent_inner();
        let changes = inner.warming.probe_error(
            &inner.sessions,
            &key,
            "main",
            ProbeOutcome::RateLimited,
            Some(&answered(429, None, None, None)),
            1_000,
        );
        inner.apply_changes(changes);
        let warming = &inner.warming.agents[&key]["main"];
        assert_eq!(
            (warming.paused, warming.reason, warming.retry_at),
            (
                false,
                Reason::ProbeQuotaExhausted,
                Some(1_000 + warming.timing.retry_ms)
            ),
            "the quota 429 retries rather than pausing"
        );
        assert_eq!(
            crate::warming::view::recipe_state(warming.reason, warming.paused),
            crate::warming::view::RecipeState::Held
        );
    }

    /// The relay answers its client refusals with a 403 that refuses the probe's own request shape, so the code and
    /// not the status picks the reason, and a 403 under any other code keeps the authentication reason.
    #[test]
    fn a_403_client_refusal_pauses_as_an_unsupported_probe_and_another_403_as_an_authentication_error(
    ) {
        for (code, reason) in [
            ("auth_unknown", Reason::ProbeAuthenticationError),
            ("client_contract", Reason::UnsupportedProbe),
            ("client_drift", Reason::UnsupportedProbe),
        ] {
            let (mut inner, key) = one_agent_inner();
            let changes = inner.warming.probe_error(
                &inner.sessions,
                &key,
                "main",
                ProbeOutcome::Rejected,
                Some(&answered(403, Some(code), None, None)),
                1_000,
            );
            inner.apply_changes(changes);
            let warming = &inner.warming.agents[&key]["main"];
            assert_eq!(
                (warming.reason, warming.paused, warming.retry_at),
                (reason, true, None),
                "{code}"
            );
        }
    }

    #[test]
    fn a_503_ledger_behind_classifies_as_held_and_no_other_outcome() {
        assert_eq!(
            classify_status(503, Some("ledger_behind")),
            ProbeOutcome::Held,
            "the router's own refusal is its own outcome"
        );
        assert_eq!(
            classify_status(503, None),
            ProbeOutcome::Failed,
            "an upstream 503 stays a failure"
        );
    }

    #[test]
    fn a_503_ledger_behind_holds_after_the_header_and_counts_no_failure() {
        let (mut inner, key) = one_agent_inner();
        let changes = inner.warming.probe_error(
            &inner.sessions,
            &key,
            "main",
            ProbeOutcome::Held,
            Some(&answered(503, Some("ledger_behind"), None, Some(5))),
            1_000,
        );
        inner.apply_changes(changes);
        let warming = &inner.warming.agents[&key]["main"];
        assert_eq!(
            (
                warming.paused,
                warming.failures,
                warming.retry_at,
                warming.reason
            ),
            (false, 0, Some(6_000), Reason::LedgerBehind),
            "the router's own standing: held after the header's wait, no failure counted"
        );
    }

    #[test]
    fn a_ledger_behind_answer_without_a_header_waits_the_timings_retry() {
        let (mut inner, key) = one_agent_inner();
        let changes = inner.warming.probe_error(
            &inner.sessions,
            &key,
            "main",
            ProbeOutcome::Held,
            Some(&answered(503, Some("ledger_behind"), None, None)),
            1_000,
        );
        inner.apply_changes(changes);
        let warming = &inner.warming.agents[&key]["main"];
        assert_eq!(
            (
                warming.paused,
                warming.failures,
                warming.retry_at,
                warming.reason
            ),
            (
                false,
                0,
                Some(1_000 + warming.timing.retry_ms),
                Reason::LedgerBehind
            ),
            "the timing's base wait stands in for a missing header, still with no failure counted"
        );
    }

    #[test]
    fn ledger_behind_answers_in_a_row_never_reach_the_retry_limit() {
        let (mut inner, key) = one_agent_inner();
        // An hour of entry ahead, so the ladder's deadline would pause a hold that counted a failure.
        warming_mut(&mut inner.warming.agents, &key, "main")
            .expect(AGENT_HELD)
            .expires = Some(1_000 + crate::ONE_HOUR_MS);
        let answers = 6;
        for answer in 0..answers {
            let now = 1_000 + i64::from(answer) * 6_000;
            let changes = inner.warming.probe_error(
                &inner.sessions,
                &key,
                "main",
                ProbeOutcome::Held,
                Some(&answered(503, Some("ledger_behind"), None, Some(5))),
                now,
            );
            inner.apply_changes(changes);
            let warming = &inner.warming.agents[&key]["main"];
            assert_eq!(
                (
                    warming.paused,
                    warming.failures,
                    warming.retry_at,
                    warming.reason
                ),
                (false, 0, Some(now + 5_000), Reason::LedgerBehind),
                "answer {answer} of {answers}: a ledger catching up never pauses the recipe"
            );
        }
    }

    #[test]
    fn a_503_without_the_code_still_retries_under_backoff() {
        let (mut inner, key) = one_agent_inner();
        let changes = inner.warming.probe_error(
            &inner.sessions,
            &key,
            "main",
            ProbeOutcome::Failed,
            Some(&answered(503, None, None, None)),
            1_000,
        );
        inner.apply_changes(changes);
        let warming = &inner.warming.agents[&key]["main"];
        assert_eq!(
            (warming.paused, warming.failures, warming.reason),
            (false, 1, Reason::RetryableProbeError),
            "an upstream 503 is a failure the backoff and the retry limit count"
        );
    }

    #[test]
    fn retry_after_secs_reads_the_header_run_job_passes_on() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(header::RETRY_AFTER, "2".parse().unwrap());
        assert_eq!(retry_after_secs(&headers), Some(2));
        assert_eq!(
            retry_after_secs(&axum::http::HeaderMap::new()),
            None,
            "no header at all"
        );
        let mut not_a_number = axum::http::HeaderMap::new();
        not_a_number.insert(header::RETRY_AFTER, "soon".parse().unwrap());
        assert_eq!(
            retry_after_secs(&not_a_number),
            None,
            "a header that does not parse"
        );
    }

    const MAX_20X: Option<&str> = Some("default_claude_max_20x");
    /// 1970-01-01 00:00 UTC, a Wednesday afternoon in the band's zone and outside its hours.
    const OFF_BAND: i64 = 0;
    /// Monday 2026-09-21 at 14:00 UTC, 07:00 in the band's zone and inside its hours.
    const IN_BAND: i64 = 1_789_999_200_000;

    fn million_input() -> ProbeUsage {
        ProbeUsage {
            input: 1_000_000,
            ..ProbeUsage::default()
        }
    }

    #[test]
    fn a_plan_funded_probe_on_an_unlisted_model_settles_in_dollars() {
        let tables = PlanTables::shipped();
        let usage = ProbeUsage {
            write: 2_000,
            five_minutes: 2_000,
            ..ProbeUsage::default()
        };
        let unit = settled_unit(
            SavingsUnit::WindowShare,
            &tables,
            None,
            MAX_20X,
            "o",
            "unlisted-model",
            usage,
            OFF_BAND,
        );
        assert_eq!(
            unit,
            SavingsUnit::Usd,
            "the cost model has no weight, so the unit falls back too"
        );
    }

    // The test model's regime `desk` sizes the `default_claude_max_20x` 7-day window apart from the base row, so a
    // probe sent under it costs the base share scaled by the base week over the regime's, each as its band weighs it.
    #[test]
    fn a_probes_cost_and_its_settlement_follow_the_agents_entrypoint() {
        let tables = phoebis_ledger::test_tables::subscription_costs();
        let at = phoebis_ledger::test_tables::JANUARY_2026;
        let cost = |entrypoint| {
            probe_cost(
                &tables,
                entrypoint,
                MAX_20X,
                "claude-opus-5-5",
                million_input(),
                at,
            )
            .expect("a weighted model on a sized tier")
        };
        let week = |entrypoint| {
            let row = tables
                .revisions
                .in_force(at)
                .capacity_of(entrypoint, MAX_20X.unwrap())
                .and_then(|row| row.seven_day)
                .expect("the tier sizes its week");
            row / tables.band_factor(SEVEN_DAY, entrypoint, at)
        };
        let ratio = week(None) / week(Some("desk"));
        let (interactive, base) = (cost(Some("desk")), cost(None));
        assert!(
            (interactive / base - ratio).abs() < 1e-12,
            "the probe's share under the regime is {ratio} of the base one: {interactive} against {base}"
        );
        for entrypoint in [Some("desk"), None] {
            assert_eq!(
                settled_unit(
                    SavingsUnit::WindowShare,
                    &tables,
                    entrypoint,
                    MAX_20X,
                    "o",
                    "claude-opus-5-5",
                    million_input(),
                    at,
                ),
                SavingsUnit::WindowShare,
                "{entrypoint:?}"
            );
        }
    }

    // One input token's units over the `default_claude_max_20x` 7-day window, with the band's factor on the week
    // inside the price.
    #[test]
    fn a_plan_funded_probe_on_a_listed_model_settles_in_seven_day_share() {
        // The first cost entry alone, so the September instant below reads its figures.
        let tables = PlanTables {
            revisions: phoebis_ledger::test_tables::first_entry_costs(),
            path: None,
            source: "test".to_string(),
        };
        let costs = tables.revisions.in_force(OFF_BAND);
        let units = costs
            .weight("claude-opus-5-5", SEVEN_DAY)
            .expect("Opus 5.5 weighs the week")
            * costs.type_multiplier.input;
        let week = costs
            .capacity_of(None, MAX_20X.unwrap())
            .and_then(|row| row.seven_day)
            .expect("the tier sizes its week");
        let one_input = ProbeUsage {
            input: 1,
            ..ProbeUsage::default()
        };
        let unit = settled_unit(
            SavingsUnit::WindowShare,
            &tables,
            None,
            MAX_20X,
            "o",
            "claude-opus-5-5",
            one_input,
            OFF_BAND,
        );
        assert_eq!(unit, SavingsUnit::WindowShare);
        let share =
            |at: i64| probe_cost(&tables, None, MAX_20X, "claude-opus-5-5", one_input, at).unwrap();
        let cost = share(OFF_BAND);
        assert!(
            (cost - units / week).abs() < 1e-12,
            "one Opus 5.5 input token is its {units} units over the week's {week}: {cost}"
        );
        let banded = share(IN_BAND);
        let factor = tables.band_factor(SEVEN_DAY, None, IN_BAND);
        assert!(
            (banded - units / week * factor).abs() < 1e-12,
            "the band's factor on the week is inside the price at the settlement instant: {banded}"
        );
    }

    #[test]
    fn a_plan_funded_probe_on_an_account_without_a_tier_settles_in_dollars() {
        let tables = PlanTables::shipped();
        let unit = settled_unit(
            SavingsUnit::WindowShare,
            &tables,
            None,
            None,
            "o",
            "claude-opus-5",
            million_input(),
            OFF_BAND,
        );
        assert_eq!(
            unit,
            SavingsUnit::Usd,
            "no tier names the window the share would be of, so the probe settles in dollars"
        );
    }

    #[test]
    fn a_plan_funded_probe_on_a_model_of_no_weighted_tier_settles_in_dollars() {
        // The test table places Mystery 2 in a tier with no weighted model, so nothing weighs it.
        let tables = phoebis_ledger::test_tables::subscription_costs();
        let unit = settled_unit(
            SavingsUnit::WindowShare,
            &tables,
            None,
            MAX_20X,
            "o",
            "claude-mystery-2",
            million_input(),
            OFF_BAND,
        );
        assert_eq!(
            unit,
            SavingsUnit::Usd,
            "Mystery 2 has no weight to read, so the share is unpriced and the dollar unit governs"
        );
    }

    #[test]
    fn a_probe_reads_landed_cold_or_failed_by_its_outcome_and_its_cache_read() {
        assert_eq!(
            (
                probe_outcome_label(ProbeOutcome::Ok, true),
                probe_outcome_label(ProbeOutcome::Ok, false)
            ),
            ("landed", "cold")
        );
        assert_eq!(
            (
                probe_outcome_label(ProbeOutcome::Failed, true),
                probe_outcome_label(ProbeOutcome::Failed, false)
            ),
            ("failed", "failed")
        );
    }
}
