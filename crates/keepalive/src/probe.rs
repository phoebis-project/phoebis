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
use phoebis_routing::{seconds_ms, SEVEN_DAY};
use phoebis_store::AccountKind;
use serde_json::Value;

use crate::breakeven::Ceilings;
use crate::engine::{
    agent_mut, agent_ref, Agent, AgentState, Engine, Inner, Kept, ParkKind, ProbeCharge, Reason,
    SavingsUnit, SessionKey, UnansweredProbe, Verdict, Wasted, AGENT_HELD, MAIN_AGENT,
};
use crate::recipe::Recipe;
use crate::savings::{answered_speed, probe_cost, settlement_unit, typed, unpriced_reason};
use crate::usage::{ProbeUsage, MAX_USAGE_BYTES};

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
    /// The account the gate admitted the probe on, the one its pin names.
    pub account: String,
    pub pool_id: String,
    pub canonical_model: String,
    /// The agent's entrypoint at dispatch, which the probe's headers carry, so the probe settles under the regime it
    /// was sent with even when the agent observes another before it lands.
    pub entrypoint: Option<String>,
    pub recipe: Recipe,
    pub expected: u64,
    /// The dispatch counted this probe in the agent's `idle_ticks`, as it does for an idle main agent or an unattended
    /// session alone. A hold gives back only a tick its dispatch counted.
    pub counted_idle_tick: bool,
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
    /// The relay's own code for an answer it built itself, `accounts_at_cap`, `accounts_exhausted` and
    /// `accounts_unavailable` among them; `None` for an upstream answer forwarded as it came.
    pub error_code: Option<String>,
    /// `None` when the read crossed the usage bound or failed: nothing past it is a usage block.
    pub body: Option<Bytes>,
    /// The TTL the relay dispatched the probe's body with, off `RelayOutcome::cache_ttl_ms`; `None` when the
    /// body went as sent.
    pub cache_ttl_ms: Option<i64>,
    /// Whether the answer's head said overage was in use, off `RelayOutcome::overage_in_use`: the fact the ledger
    /// files the probe's request row under, so the credit files beside it.
    pub in_overage: bool,
    /// The served account's kind off `RelayOutcome::account_kind`, as the leg that answered stamped it at its
    /// selection: the fact the request row files its funding under, carried so the credit files beside it even when
    /// the account leaves the registry while the probe is in flight. `None` for an answer the relay built before any
    /// selection.
    pub account_kind: Option<AccountKind>,
    /// The served account's class off `RelayOutcome::usage_based`, stamped beside the kind, so the settlement reads the
    /// funding the request row files under. False for an answer the relay built before any selection.
    pub usage_based: bool,
    /// The `Retry-After` header's seconds, on a pool-at-its-cap 429; `None` otherwise.
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
pub enum ProbeOutcome {
    Canceled,
    Exhausted,
    Failed,
    Held,
    Migrated,
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
        ProbeOutcome::Migrated | ProbeOutcome::Ok if cache_read => "landed",
        ProbeOutcome::Migrated | ProbeOutcome::Ok => "cold",
        ProbeOutcome::Held => "held",
        ProbeOutcome::Canceled
        | ProbeOutcome::Exhausted
        | ProbeOutcome::Failed
        | ProbeOutcome::RateLimited
        | ProbeOutcome::Rejected
        | ProbeOutcome::TimedOut => "failed",
    }
}

/// The cold judgment `apply_probe` makes, shared with the probe counter so the counter's `cold` is the streak's.
pub(crate) fn covers(migrated: bool, usage: &ProbeUsage, expected: u64) -> bool {
    if migrated {
        usage.evidence() >= expected
    } else {
        usage.read >= expected
    }
}

/// The counts one probe used and how it ended, from the job and the exchange alone. Whether the recipe it ran for
/// still exists changes nothing here: the upstream served it, so the pool's spend, the metrics, and the ledger
/// carry it either way.
#[derive(Clone, Debug, PartialEq)]
pub struct ProbeRecord {
    pub account: String,
    pub agent_id: String,
    pub canonical_model: String,
    /// The recipe carried `speed: "fast"`, the speed its migration mark files under. The probe's cost and its waste
    /// file under the speed its answer reported, `answered_speed` of `usage`.
    pub fast: bool,
    /// Whether the probe's answer reported overage in use, the fact its request row files under: the credit row
    /// keys the same funding, and the settlement unit follows the same side.
    pub in_overage: bool,
    pub key: SessionKey,
    pub key_generation: i64,
    /// The served account's kind as the leg that answered stamped it, off the answer rather than the registry, so the
    /// credit files under the kind the request row carries even when the account leaves while the probe is in flight.
    /// `None` for an answer the relay built before any selection, which prices nothing.
    pub kind: Option<AccountKind>,
    /// The served account's class as the same leg stamped it beside the kind, false where no leg answered.
    pub usage_based: bool,
    pub outcome: ProbeOutcome,
    pub pool_id: String,
    pub usage: ProbeUsage,
    /// The instant the relay received the probe's request, the one its request row files under, so the settlement
    /// keys the same hour and prices at the same revision whenever the answer lands. The instant the engine sent the
    /// request, on the same clock, where the relay built no answer.
    pub at: i64,
    /// The probe's prompt-length band, `table.band_of(model, prompt_tokens(input, read, write_5m, write_1h))` on the
    /// revision in force at `at`, read from the answer's counts.
    pub band: Band,
    /// The account tier the settlement read at the exchange, which a probe settling in the window share reads from
    /// the store: its settlement's credit row keys the tier the probe ran under. `None` for a probe that settles in
    /// dollars and for an account whose tier could not be read.
    pub tier: Option<String>,
}

/// What the fenced half did with the result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Applied {
    Landed,
    Orphaned,
}

/// How an upstream `Retry-After` bears on a rung's wait.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RetryWait {
    /// No header: the ladder's step.
    Step,
    /// The `accounts_at_cap` header: a slot frees up in seconds, so the header replaces the step whatever its length.
    Replace(i64),
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

/// The `Retry-After` seconds a pool-at-its-cap 429 or a `ledger_behind` 503 carries, `None` for a missing header or
/// one that does not parse.
fn retry_after_secs(headers: &axum::http::HeaderMap) -> Option<u64> {
    headers
        .get(header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
}

fn classify_status(status: u16, error_code: Option<&str>) -> ProbeOutcome {
    let exhausted = matches!(
        error_code,
        Some("accounts_exhausted") | Some("accounts_unavailable")
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
    let marker = crate::probe_marker(&job.key.0, job.key_generation);
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
                        in_overage: o.overage_in_use == Some(true),
                        account_kind: o.account_kind,
                        usage_based: o.usage_based,
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
    let mut record = Inner::probe_record(&job, &exchange, table, at);
    // The serving account's kind decides the unit this probe's cost settles in. It rides the relay's answer, so a
    // registry write mid-exchange never changes which account this probe is charged against, and an answer the relay
    // never built names no account: its probe settles in dollars and the credit path never matches it.
    // A fast answer draws usage credits whatever the account's funding, so it settles in dollars. The answer's speed
    // decides rather than the recipe's: a fast recipe answered at standard speed spent the plan, as its request row
    // files it.
    let default_unit = match record.kind {
        Some(kind) if answered_speed(record.usage) != Speed::Fast => {
            SavingsUnit::of(kind, record.usage_based, record.in_overage)
        }
        _ => SavingsUnit::Usd,
    };
    // The tier the router prices the account by, as the store's term resolves it now: a share is of the window the
    // account has at this settlement, and the record keeps that tier as the key its rows file under, as the request row
    // keeps its own, none for a free organization. The unit reads it where the snapshot prices the account too, so a
    // plan the cost model does not price settles in dollars as an untiered account does. Read on a blocking worker
    // ahead of the engine's lock, since a save may hold the store's connection, and `apply_probe` fences a change that
    // lands meanwhile. A worker that panicked reads no tier, as a failed read does.
    let tier = if default_unit == SavingsUnit::WindowShare {
        let store = engine.parts.store.clone();
        let account = record.account.clone();
        match tokio::task::spawn_blocking(move || store.subscription_term(&account)).await {
            Ok(read) => engine.tier_from(
                &record.account,
                read,
                "the settlement falls back to dollars",
            ),
            Err(_) => None,
        }
    } else {
        None
    };
    record.tier = tier.clone();
    let snapshot = engine.parts.router.snapshot();
    let priced_tier = snapshot.priced_tier(&record.account).and(tier.as_deref());
    let unit = settled_unit(
        default_unit,
        &tables,
        job.entrypoint.as_deref(),
        priced_tier,
        &record.account,
        &record.canonical_model,
        record.usage,
        at,
    );
    let covered_unit = record.kind.map(|_| unit);
    engine
        .lock()
        .apply_probe(&job, exchange, &record, covered_unit, now);
    engine.record_probe(&record, now);
    // Read off the answer rather than off what the fenced half did with it, so a probe whose recipe moved on while it
    // was in flight, or paused on its answer, still counts as the upstream served it.
    let cache_read = covers(
        record.outcome == ProbeOutcome::Migrated,
        &record.usage,
        job.expected,
    );
    engine.parts.metrics.observe_probe(
        &record.pool_id,
        probe_outcome_label(record.outcome, cache_read),
    );
    engine.flush();
    // The worker slot comes back only after both sends: `wait_idle` returns on a zero count, and a caller that
    // asserts on the ledger right after it would otherwise race the records this probe owes.
    engine.lock().running -= 1;
    engine.idle.notify_waiters();
}

impl Inner {
    /// The immutable half: the outcome the exchange had, the counts it used and their prompt-length band on `table`,
    /// the price revision in force at `at`, the relay's receipt of the probe.
    /// A migrated probe is one the relay served on an account other than the job's; it is priced like any other
    /// and reported as migrated.
    pub(crate) fn probe_record(
        job: &ProbeJob,
        exchange: &Exchange,
        table: &PriceTable,
        at: i64,
    ) -> ProbeRecord {
        let mut record = ProbeRecord {
            account: job.account.clone(),
            agent_id: job.agent_id.clone(),
            canonical_model: job.canonical_model.clone(),
            fast: job.recipe.fast,
            in_overage: false,
            key: job.key.clone(),
            key_generation: job.key_generation,
            kind: None,
            usage_based: false,
            outcome: ProbeOutcome::Failed,
            pool_id: job.pool_id.clone(),
            usage: ProbeUsage::default(),
            at,
            band: Band::Standard,
            tier: None,
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
        record.in_overage = result.in_overage;
        record.kind = result.account_kind;
        record.usage_based = result.usage_based;
        if let Some(account) = &result.account {
            record.account = account.clone();
        }
        record.outcome = classify_status(result.status, result.error_code.as_deref());
        if record.outcome != ProbeOutcome::Ok {
            return record;
        }
        if let Some(usage) = result.body.as_deref().and_then(ProbeUsage::parse) {
            record.usage = usage;
            let counts = typed(usage);
            record.band = table.band_of(&job.canonical_model, counts.prompt_tokens());
        }
        if result
            .account
            .as_deref()
            .is_some_and(|served| served != job.account)
        {
            record.outcome = ProbeOutcome::Migrated;
        }
        record
    }

    /// The fenced half, the Go `runProbe` after the call returned. The fence is the agent's incarnation and
    /// generation and the observation's id and attempt; a result that fails it touches no recipe and settles
    /// as one wasted probe under the stop that orphaned it, since the upstream served it either way. A held result
    /// asked no account and settles under none, and a success the idle limit orphaned waits as a pending probe.
    pub(crate) fn apply_probe(
        &mut self,
        job: &ProbeJob,
        exchange: Exchange,
        record: &ProbeRecord,
        covered_unit: Option<SavingsUnit>,
        now: i64,
    ) -> Applied {
        let key = &job.key;
        let agent_id = job.agent_id.as_str();
        let charge = ProbeCharge {
            account: Some(record.account.clone()),
            at: record.at,
            band: record.band,
            in_overage: record.in_overage,
            kind: record.kind,
            tier: record.tier.clone(),
            usage: record.usage,
            usage_based: record.usage_based,
        };
        // The worker slot comes back to the record the probe started on whatever the fence says. A result
        // orphaned on a record that is still alive, by a superseding request or by a discard, would otherwise
        // leave `in_flight` set, and `step` skips an agent that has a probe in flight for good.
        if let Some(agent) = agent_mut(&mut self.sessions, key, agent_id)
            .filter(|agent| agent.incarnation == job.incarnation)
        {
            agent.in_flight = false;
        }
        let fenced = agent_ref(&self.sessions, key, agent_id).is_some_and(|agent| {
            agent.incarnation == job.incarnation
                && agent.generation == job.generation
                && agent
                    .current
                    .as_ref()
                    .is_some_and(|o| o.id == job.observation_id && o.attempt == job.attempt)
        }) && self.live(key, agent_id);
        if !fenced {
            // A record still on the incarnation the probe started on, carrying no stop label, was superseded by
            // a real request rather than stopped: the request the probe was paying for arrived, so the probe is
            // not wasted. A record that is gone leaves no label to read, so what removed it is what labels the
            // probe: a restart clears every recipe at once and wastes none of them, a session that went is a
            // session that ended, and anything else replaced the record.
            let label = if self.closed {
                None
            } else {
                match self.sessions.get(key) {
                    None => Some(Wasted::SessionEnded),
                    Some(session) => match session
                        .agents
                        .get(agent_id)
                        .filter(|agent| agent.incarnation == job.incarnation)
                    {
                        Some(agent) => agent.last_stop,
                        None => Some(Wasted::Replaced),
                    },
                }
            };
            let Some(label) = label else {
                return Applied::Orphaned;
            };
            if record.outcome == ProbeOutcome::Held {
                return Applied::Orphaned;
            }
            // The idle limit keeps its probes pending until their entry lapses or a turn reads them, so a success
            // still in flight at the ceiling joins them by the entry it wrote, computed as the landing computes it.
            if label == Wasted::IdleLimit
                && matches!(record.outcome, ProbeOutcome::Ok | ProbeOutcome::Migrated)
            {
                if let Exchange::Answered(result) = &exchange {
                    if let Some(agent) = agent_mut(&mut self.sessions, key, agent_id)
                        .filter(|agent| agent.incarnation == job.incarnation)
                    {
                        let requested_ms = result.cache_ttl_ms.unwrap_or(job.recipe.ttl_ms);
                        let ttl = record.usage.ttl_ms(agent.ttl_ms, requested_ms);
                        agent.kept.push(Kept {
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
                        self.touch_agent(key, agent_id);
                        return Applied::Orphaned;
                    }
                }
            }
            self.settlements.push(crate::engine::Settlement {
                key: key.clone(),
                key_generation: job.key_generation,
                pool_id: job.pool_id.clone(),
                canonical_model: job.canonical_model.clone(),
                label,
                charge,
            });
            return Applied::Orphaned;
        }
        let agent = agent_mut(&mut self.sessions, key, agent_id).expect(AGENT_HELD);
        let cold_limit = agent.timing.cold_probe_limit;
        let result = match exchange {
            Exchange::Canceled | Exchange::TimedOut => {
                self.probe_error(
                    key,
                    agent_id,
                    record.outcome,
                    None,
                    job.counted_idle_tick,
                    now,
                );
                return Applied::Landed;
            }
            Exchange::Answered(result) => result,
        };
        if record.outcome != ProbeOutcome::Ok && record.outcome != ProbeOutcome::Migrated {
            self.probe_error(
                key,
                agent_id,
                record.outcome,
                Some(&result),
                job.counted_idle_tick,
                now,
            );
            return Applied::Landed;
        }
        // The upstream served this one and the pool was charged for it, so it goes on the recipe's own books
        // here, before any branch below pauses the recipe over what the answer said: the settlement a stop leaves
        // would otherwise disagree with the ledger. Every probe since the last real request is unanswered until one
        // arrives, a migrated one at its price like any other.
        let migrated = record.outcome == ProbeOutcome::Migrated;
        let agent = agent_mut(&mut self.sessions, key, agent_id).expect(AGENT_HELD);
        // The end of the entry this probe wrote, computed once as the landing reads it, so a probe a check below
        // pauses still settles by its own entry.
        let previous_ms = agent.ttl_ms;
        let requested_ms = result.cache_ttl_ms.unwrap_or(job.recipe.ttl_ms);
        let ttl = record.usage.ttl_ms(previous_ms, requested_ms);
        agent.unanswered.push(UnansweredProbe {
            id: result.request_id.clone().unwrap_or_default(),
            charge,
            unit: covered_unit,
            window: SEVEN_DAY,
            fast: job.recipe.fast,
            model: agent.canonical_model.clone(),
            expires: job.started + ttl,
            covered: false,
        });
        let Some(account) = result.account.clone() else {
            self.pause(key, agent_id, Reason::MissingAccount);
            return Applied::Landed;
        };
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
            self.pause(key, agent_id, Reason::InvalidProbeUsage);
            return Applied::Landed;
        };
        let agent = agent_mut(&mut self.sessions, key, agent_id).expect(AGENT_HELD);
        agent.usage.add(usage);
        if usage.output != 0 || !content.is_empty() {
            self.pause(key, agent_id, Reason::UnexpectedOutput);
            return Applied::Landed;
        }
        let agent = agent_mut(&mut self.sessions, key, agent_id).expect(AGENT_HELD);
        if migrated {
            agent.account = Some(account.clone());
            agent.migrations += 1;
            agent.cold = 0;
        }
        // Full coverage is the whole prefix read back, which is what the next real request would read; a
        // write beside it is the prefix rebuilt rather than kept. A migrated probe wrote the prefix on its
        // new account instead, and that write is the coverage the next request reads there; it does not count
        // toward the cold streak.
        let covered = covers(migrated, &usage, job.expected);
        if !migrated {
            if covered {
                agent.cold = 0;
            } else {
                agent.cold += 1;
            }
        }
        let cold = agent.cold;
        if cold >= cold_limit {
            self.pause(key, agent_id, Reason::RepeatedColdProbe);
            return Applied::Landed;
        }
        if usage.evidence() == 0 {
            self.retry(
                key,
                agent_id,
                Reason::NoProbeCacheEvidence,
                now,
                RetryWait::Step,
            );
            return Applied::Landed;
        }
        // A probe that read part of the prefix and rebuilt the rest never warmed the whole entry; one that
        // read none of it rebuilt the entry, which the cold streak counts instead.
        if usage.evidence() < job.expected || (usage.read > 0 && !covered) {
            self.pause(key, agent_id, Reason::PartialCacheCoverage);
            return Applied::Landed;
        }
        let agent = agent_mut(&mut self.sessions, key, agent_id).expect(AGENT_HELD);
        agent.failures = 0;
        agent.retry_at = None;
        agent.ttl_ms = ttl;
        crate::engine::note_ttl_change(
            key,
            agent_id,
            previous_ms,
            ttl,
            requested_ms,
            &record.usage,
        );
        agent.cache_started = job.started;
        agent.expires = Some(job.started + ttl);
        agent.jitter_frac = rand::random::<f64>();
        // A full read and an accepted complete rewrite alike leave the whole prefix in an entry the next request
        // can read, so either covers from here.
        if let Some(probe) = agent.unanswered.last_mut() {
            probe.covered = true;
        }
        self.set_reason(key, agent_id, Reason::Scheduled);
        // A probe that consumed its whole refresh interval must not start a tight loop.
        if self.sessions[key].agents[agent_id]
            .due_at()
            .is_some_and(|due| due <= now)
        {
            self.retry(key, agent_id, Reason::SlowProbe, now, RetryWait::Step);
        }
        Applied::Landed
    }

    pub(crate) fn pause(&mut self, key: &SessionKey, agent_id: &str, reason: Reason) {
        self.set_reason(key, agent_id, reason);
        if let Some(agent) = agent_mut(&mut self.sessions, key, agent_id) {
            agent.pause();
        }
    }

    /// The Go `probeErrorLocked`, read from the answer the relay gave. A 429 after failover, or the exhaustion 429
    /// the relay answers when every account is cooled, retries on the ladder under `probe_quota_exhausted`, its
    /// `Retry-After` replacing the step where it is longer. A pool at its in-flight cap (`accounts_at_cap`) retries
    /// after the header's wait whatever its length, since a slot there may free up before the next real request
    /// would. A probe the selector refused on the pinned member's spend budget holds under `budget`
    /// without a pause, since only a raised budget or the period's end clears it, never a wait. A probe the router
    /// refused with `ledger_behind` holds under it for the answer's wait, counting no failure and no idle tick. A
    /// probe the selector answered on its pinned member's standing holds under that standing: `Paused` for the
    /// operator's pause, `IneligibleCredential` for an ended subscription (both read `paused` in the console), a
    /// retry for a member this probe already tried, or for one whose budget the degraded ledger leaves unknown,
    /// which the ledger's recovery clears without a real request. A pinned member a leg of the probe marked for
    /// re-authentication pauses under the authentication reason. The untimed exhaustion 503, when every account of
    /// the pool was unusable at selection, retries under its own reason. A 5xx forwarded from upstream retries. A 403
    /// under `client_contract` or `client_drift` refuses the probe's own request shape and pauses as an unsupported
    /// probe, any other 401 or 403 pauses under the authentication reason, any other 4xx is a shape the upstream
    /// refuses, and a canceled, timed-out, or lost exchange (status 0) retries as a transport error.
    pub(crate) fn probe_error(
        &mut self,
        key: &SessionKey,
        agent_id: &str,
        outcome: ProbeOutcome,
        result: Option<&ProbeResult>,
        counted_idle_tick: bool,
        now: i64,
    ) {
        // An exchange the relay never answered reads as status 0, with no code, obstacle or wait.
        let status = result.map_or(0, |result| result.status);
        let error_code = result.and_then(|result| result.error_code.as_deref());
        let obstacle = result.and_then(|result| result.obstacle);
        let exhausted = outcome == ProbeOutcome::Exhausted;
        let retry_after_ms = result
            .and_then(|result| result.retry_after_secs)
            .map(seconds_ms);
        // Read before the obstacles, which never meet it: a 429 classifies as rate limited and never as exhausted,
        // and the budget's timed answer carries `accounts_exhausted`.
        if status == 429 && error_code == Some("accounts_at_cap") {
            // The pool is momentarily full and a slot frees up in seconds, so `RetryableProbeError` ("retrying") is
            // the honest label.
            self.retry(
                key,
                agent_id,
                Reason::RetryableProbeError,
                now,
                retry_after_ms.map_or(RetryWait::Step, RetryWait::Replace),
            );
            return;
        }
        // Every obstacle is named, so one added later fails to build here until it is given its hold.
        match (exhausted, obstacle) {
            (true, Some(ProbeObstacle::Operator)) => {
                self.pause(key, agent_id, Reason::Paused);
                return;
            }
            (true, Some(ProbeObstacle::SubscriptionEnded)) => {
                self.pause(key, agent_id, Reason::IneligibleCredential);
                return;
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
                    key,
                    agent_id,
                    Reason::RetryableProbeError,
                    now,
                    RetryWait::Step,
                )
            }
            (true, Some(ProbeObstacle::ReauthRequired)) => {
                self.pause(key, agent_id, Reason::ProbeAuthenticationError);
                return;
            }
            (_, Some(ProbeObstacle::Budget)) if status == 429 => {
                // The pinned member reached its spend budget between the gate's read and the selection. A pause
                // would outlive the budget (nothing but a real request undoes one), and the reason alone leaves
                // the schedule armed, so the gate re-reads the spend on the next poll and releases the probe
                // itself when the period ends or the budget is raised.
                self.set_reason(key, agent_id, Reason::Budget);
                return;
            }
            (
                false,
                Some(
                    ProbeObstacle::Attempted
                    | ProbeObstacle::BudgetUnknown
                    | ProbeObstacle::Operator
                    | ProbeObstacle::PoolMoved
                    | ProbeObstacle::ReauthRequired
                    | ProbeObstacle::SubscriptionEnded,
                ),
            )
            | (_, Some(ProbeObstacle::Budget))
            | (_, None) => {}
        }
        if status == 503 && error_code == Some(phoebis_relay::LEDGER_BEHIND) {
            // The router's own standing, not the account's: no failure is counted, so the cold-probe limit never
            // pauses a recipe for a ledger that is catching up, and the entry's own lapse bounds the re-sends.
            let agent = agent_mut(&mut self.sessions, key, agent_id).expect(AGENT_HELD);
            let wait_ms = retry_after_ms.unwrap_or(agent.timing.retry_ms);
            agent.retry_at = Some(now.saturating_add(wait_ms));
            // A hold costs no tick: `idle_ticks` counts served or failed probes, so the tick this probe's dispatch
            // counted comes back, and a dispatch that counted none gives back nothing. The entry's lapse and the
            // ceilings bound the hold.
            if counted_idle_tick {
                agent.idle_ticks = agent.idle_ticks.saturating_sub(1);
            }
            self.set_reason(key, agent_id, Reason::LedgerBehind);
            return;
        }
        let (reason, wait) = match status {
            429 => (
                Reason::ProbeQuotaExhausted,
                retry_after_ms.map_or(RetryWait::Step, RetryWait::AtLeast),
            ),
            _ if exhausted => (Reason::ProbePoolExhausted, RetryWait::Step),
            500..=599 => (Reason::RetryableProbeError, RetryWait::Step),
            // Under these codes the relay's 403 refuses the probe's own request shape, not the credential.
            _ if matches!(error_code, Some("client_contract") | Some("client_drift")) => {
                self.pause(key, agent_id, Reason::UnsupportedProbe);
                return;
            }
            401 | 403 => {
                self.pause(key, agent_id, Reason::ProbeAuthenticationError);
                return;
            }
            400..=499 => {
                self.pause(key, agent_id, Reason::UnsupportedProbe);
                return;
            }
            _ => (Reason::ProbeTransportError, RetryWait::Step),
        };
        self.retry(key, agent_id, reason, now, wait);
    }

    /// The ladder: `retry_ms` doubled per consecutive error up to `retry_max_ms`, `wait` resolving the header against
    /// the step, the wait capped at `retry_max_ms`. The entry's TTL bounds the ladder: a rung due at or past the
    /// entry's expiry is not scheduled, and the failure that would plan it pauses under `ProbeRetryLimit`, since
    /// `Engine::step` lapses the entry at that instant before it reads a due probe.
    pub(crate) fn retry(
        &mut self,
        key: &SessionKey,
        agent_id: &str,
        reason: Reason,
        now: i64,
        wait: RetryWait,
    ) {
        let agent = agent_mut(&mut self.sessions, key, agent_id).expect(AGENT_HELD);
        agent.failures += 1;
        let exponent = (agent.failures - 1).min(4);
        let step = (agent.timing.retry_ms << exponent).min(agent.timing.retry_max_ms);
        let delay = match wait {
            RetryWait::Step => step,
            RetryWait::Replace(ms) => ms,
            RetryWait::AtLeast(ms) => ms.max(step),
        }
        .min(agent.timing.retry_max_ms);
        if agent.expires.is_some_and(|expires| now + delay >= expires) {
            self.pause(key, agent_id, Reason::ProbeRetryLimit);
            return;
        }
        agent.retry_at = Some(now + delay);
        self.set_reason(key, agent_id, reason);
    }
}

/// The ceilings over one agent at `now`, active while any of them says probe. The idle window is main's in every
/// state, from the later of `last_activity` (a real request, a prompt or a presence report) and `park_ended_at`. A
/// park on a background command or a subagent is probed for `parked_ms` from `park_began`, and one on a scheduled
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
            let kind = agent.park_kind;
            let background = matches!(kind, Some(ParkKind::Background | ParkKind::Both));
            if background
                && agent
                    .park_began
                    .is_some_and(|began| now - began < ceilings.parked_ms)
            {
                return Verdict::Probe;
            }
            let wake = agent
                .wake_at
                .filter(|_| matches!(kind, Some(ParkKind::Scheduled | ParkKind::Both)));
            match wake {
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

    use crate::config::{KeepaliveLimits, KeepaliveTiming};
    use crate::engine::{Agent, Session};
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
        agents.insert(
            "main".to_string(),
            Agent::new("main", "k", 1, 0, KeepaliveTiming::default()),
        );
        let mut inner = Inner::empty(KeepaliveTiming::default(), KeepaliveLimits::default(), 0);
        inner.sessions.insert(
            key.clone(),
            Session {
                key_generation: 0,
                instance: "i".into(),
                sequence: 0,
                lease: i64::MAX,
                attended: true,
                ended: false,
                owner_id: None,
                last_report_at: 0,
                last_real_at: Some(0),
                restored_until: None,
                owner_mismatches: 0,
                agents,
            },
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
            account: "a".into(),
            pool_id: "main".into(),
            canonical_model: "m".into(),
            entrypoint: None,
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
            counted_idle_tick: false,
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
                    let agent = agent_mut(&mut inner.sessions, &key, "main").expect(AGENT_HELD);
                    agent.generation = 1;
                    agent.last_stop = Some(Wasted::Released);
                }
                _ => {
                    inner.sessions.clear();
                }
            }
            let exchange = held_answer();
            let record = Inner::probe_record(
                &job,
                &exchange,
                PriceRevisions::shipped().in_force(2_000),
                2_000,
            );
            assert_eq!(record.outcome, ProbeOutcome::Held, "{orphaned_by}");
            let applied = inner.apply_probe(&job, exchange, &record, None, 2_000);
            assert_eq!(applied, Applied::Orphaned, "{orphaned_by}");
            assert!(
                inner.settlements.is_empty(),
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
        ] {
            let (mut inner, key) = one_agent_inner();
            inner.probe_error(
                &key,
                "main",
                ProbeOutcome::Exhausted,
                Some(&answered(
                    503,
                    Some("accounts_unavailable"),
                    Some(obstacle),
                    None,
                )),
                false,
                1_000,
            );
            let agent = &inner.sessions[&key].agents["main"];
            assert_eq!(
                (agent.reason, agent.paused),
                (reason, paused),
                "the {obstacle:?} answer holds the agent under its own standing, not the exhausted pool"
            );
        }
    }

    #[test]
    fn a_probe_refused_for_a_moved_key_retries_under_the_new_pool_rather_than_pausing_as_exhausted()
    {
        let (mut inner, key) = one_agent_inner();
        inner.probe_error(
            &key,
            "main",
            ProbeOutcome::Exhausted,
            Some(&answered(
                503,
                Some("accounts_unavailable"),
                Some(ProbeObstacle::PoolMoved),
                Some(1),
            )),
            false,
            1_000,
        );
        let agent = &inner.sessions[&key].agents["main"];
        assert_eq!(
            (agent.reason, agent.paused),
            (Reason::RetryableProbeError, false),
            "the key's next request admits under its new pool, so the probe retries"
        );
    }

    #[test]
    fn an_accounts_at_cap_429_retries_after_the_header_rather_than_pausing() {
        let (mut inner, key) = one_agent_inner();
        inner.probe_error(
            &key,
            "main",
            ProbeOutcome::RateLimited,
            Some(&answered(429, Some("accounts_at_cap"), None, Some(3))),
            false,
            1_000,
        );
        let agent = &inner.sessions[&key].agents["main"];
        assert!(!agent.paused, "retries in place rather than pausing");
        assert_eq!(
            agent.retry_at,
            Some(4_000),
            "the header's seconds replace the backoff"
        );
        assert_eq!(agent.reason, Reason::RetryableProbeError);
    }

    #[test]
    fn a_quota_429_retries_on_the_first_rung_under_probe_quota_exhausted() {
        let (mut inner, key) = one_agent_inner();
        inner.probe_error(
            &key,
            "main",
            ProbeOutcome::RateLimited,
            Some(&answered(429, None, None, None)),
            false,
            1_000,
        );
        let agent = &inner.sessions[&key].agents["main"];
        assert_eq!(
            (agent.paused, agent.reason, agent.retry_at),
            (
                false,
                Reason::ProbeQuotaExhausted,
                Some(1_000 + agent.timing.retry_ms)
            ),
            "the quota 429 retries rather than pausing"
        );
        assert_eq!(
            crate::view::recipe_state(agent.reason, agent.paused),
            crate::view::RecipeState::Held
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
            inner.probe_error(
                &key,
                "main",
                ProbeOutcome::Rejected,
                Some(&answered(403, Some(code), None, None)),
                false,
                1_000,
            );
            let agent = &inner.sessions[&key].agents["main"];
            assert_eq!(
                (agent.reason, agent.paused, agent.retry_at),
                (reason, true, None),
                "{code}"
            );
        }
    }

    #[test]
    fn accounts_at_cap_without_a_header_falls_back_to_the_usual_backoff() {
        let (mut inner, key) = one_agent_inner();
        inner.probe_error(
            &key,
            "main",
            ProbeOutcome::RateLimited,
            Some(&answered(429, Some("accounts_at_cap"), None, None)),
            false,
            1_000,
        );
        let agent = &inner.sessions[&key].agents["main"];
        assert!(!agent.paused);
        assert_eq!(agent.retry_at, Some(1_000 + agent.timing.retry_ms));
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
        inner.probe_error(
            &key,
            "main",
            ProbeOutcome::Held,
            Some(&answered(503, Some("ledger_behind"), None, Some(5))),
            false,
            1_000,
        );
        let agent = &inner.sessions[&key].agents["main"];
        assert_eq!(
            (agent.paused, agent.failures, agent.retry_at, agent.reason),
            (false, 0, Some(6_000), Reason::LedgerBehind),
            "the router's own standing: held after the header's wait, no failure counted"
        );
    }

    #[test]
    fn a_ledger_behind_answer_without_a_header_waits_the_timings_retry() {
        let (mut inner, key) = one_agent_inner();
        inner.probe_error(
            &key,
            "main",
            ProbeOutcome::Held,
            Some(&answered(503, Some("ledger_behind"), None, None)),
            false,
            1_000,
        );
        let agent = &inner.sessions[&key].agents["main"];
        assert_eq!(
            (agent.paused, agent.failures, agent.retry_at, agent.reason),
            (
                false,
                0,
                Some(1_000 + agent.timing.retry_ms),
                Reason::LedgerBehind
            ),
            "the timing's base wait stands in for a missing header, still with no failure counted"
        );
    }

    #[test]
    fn ledger_behind_answers_in_a_row_never_reach_the_retry_limit() {
        let (mut inner, key) = one_agent_inner();
        // An hour of entry ahead, so the ladder's deadline would pause a hold that counted a failure.
        agent_mut(&mut inner.sessions, &key, "main")
            .expect(AGENT_HELD)
            .expires = Some(1_000 + crate::ONE_HOUR_MS);
        let answers = 6;
        for answer in 0..answers {
            let now = 1_000 + i64::from(answer) * 6_000;
            inner.probe_error(
                &key,
                "main",
                ProbeOutcome::Held,
                Some(&answered(503, Some("ledger_behind"), None, Some(5))),
                false,
                now,
            );
            let agent = &inner.sessions[&key].agents["main"];
            assert_eq!(
                (agent.paused, agent.failures, agent.retry_at, agent.reason),
                (false, 0, Some(now + 5_000), Reason::LedgerBehind),
                "answer {answer} of {answers}: a ledger catching up never pauses the recipe"
            );
        }
    }

    #[test]
    fn a_503_without_the_code_still_retries_under_backoff() {
        let (mut inner, key) = one_agent_inner();
        inner.probe_error(
            &key,
            "main",
            ProbeOutcome::Failed,
            Some(&answered(503, None, None, None)),
            false,
            1_000,
        );
        let agent = &inner.sessions[&key].agents["main"];
        assert_eq!(
            (agent.paused, agent.failures, agent.reason),
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
        for outcome in [ProbeOutcome::Migrated, ProbeOutcome::Ok] {
            assert_eq!(
                (
                    probe_outcome_label(outcome, true),
                    probe_outcome_label(outcome, false)
                ),
                ("landed", "cold"),
                "{outcome:?}"
            );
        }
        assert_eq!(
            (
                probe_outcome_label(ProbeOutcome::Failed, true),
                probe_outcome_label(ProbeOutcome::Failed, false)
            ),
            ("failed", "failed")
        );
    }

    #[test]
    fn a_probe_covers_its_prefix_by_reading_it_back_or_by_writing_it_after_a_migration() {
        let usage = |read, write| ProbeUsage {
            read,
            write,
            ..ProbeUsage::default()
        };
        assert_eq!(
            [
                covers(false, &usage(2_000, 0), 2_000),
                covers(false, &usage(1_999, 1), 2_000),
                covers(false, &usage(0, 2_000), 2_000),
                covers(true, &usage(0, 2_000), 2_000),
                covers(true, &usage(0, 1_999), 2_000),
            ],
            [true, false, false, true, false],
            "a whole read, a partial read, a rebuild, a migrated write, a migrated short write"
        );
    }
}
