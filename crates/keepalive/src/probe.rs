//! One probe: the recipe replayed through the relay's failover loop under the recipe's key, and the result read
//! from the response the relay returns rather than from the bus, which is best-effort. What the probe cost is
//! settled from the exchange alone; what it does to the recipe is settled only when the recipe is still the
//! one the probe ran for.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request};
use bytes::{Bytes, BytesMut};
use http_body_util::BodyExt;
use phoebis_ledger::{PlanTables, PriceTable};
use phoebis_relay::{RelayOutcome, ServedUnder};
use phoebis_routing::{PinUsage, BUDGET_WINDOW};
use phoebis_store::{AccountKind, BudgetRow, BudgetScope};
use serde_json::Value;

use crate::engine::{Agent, Engine, Funding, Inner, Reason, SavingsUnit, SessionKey, Wasted};
use crate::recipe::Recipe;
use crate::savings::{probe_cost, probe_dollars, settlement_unit, unpriced_reason};
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
    pub account: Option<String>,
    pub pool_id: String,
    pub canonical_model: String,
    pub recipe: Recipe,
    pub expected: u64,
    /// The typed usage the gate decided the probe's unit on, from the TTL the relay dispatches it with: the
    /// settlement decides on the same set, so the gate, the caps and the savings read one unit.
    pub types: PinUsage,
}

pub(crate) struct ProbeResult {
    pub status: u16,
    pub account: Option<String>,
    /// The relay's own code for an answer it built itself, `accounts_at_cap`, `accounts_exhausted` and
    /// `accounts_unavailable` among them; `None` for an upstream answer forwarded as it came.
    pub error_code: Option<String>,
    /// `None` when the read crossed the usage bound or failed: nothing past it is a usage block.
    pub body: Option<Bytes>,
    /// The TTL the relay dispatched the probe's body with, off `RelayOutcome::cache_ttl_ms`; `None` when the
    /// body went as sent.
    pub cache_ttl_ms: Option<i64>,
    /// The account's overage state at the selection the probe dispatched under, off `ServedUnder`.
    pub in_overage: bool,
    /// The `Retry-After` header's seconds, on a pool-at-its-cap 429; `None` otherwise.
    pub retry_after_secs: Option<u64>,
    /// The relay's `RelayOutcome::obstacle`: `budget` for a probe the selector refused on the pinned member's
    /// spend, `operator`, `subscription_ended` or `attempted` for one it answered on the pinned member's standing,
    /// `reauth_required` for one whose leg marked the account for re-authentication.
    pub obstacle: Option<&'static str>,
}

pub(crate) enum Exchange {
    Answered(ProbeResult),
    Cancelled,
    TimedOut,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeOutcome {
    Cancelled,
    Exhausted,
    Failed,
    Migrated,
    Ok,
    RateLimited,
    Rejected,
    TimedOut,
}

impl ProbeOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            ProbeOutcome::Cancelled => "cancelled",
            ProbeOutcome::Exhausted => "exhausted",
            ProbeOutcome::Failed => "failed",
            ProbeOutcome::Migrated => "migrated",
            ProbeOutcome::Ok => "ok",
            ProbeOutcome::RateLimited => "rate_limited",
            ProbeOutcome::Rejected => "rejected",
            ProbeOutcome::TimedOut => "timed_out",
        }
    }
}

/// What one probe cost and how it ended, from the job and the exchange alone. Whether the recipe it ran for
/// still exists changes nothing here: the upstream served it, so the pool's spend, the metrics, and the ledger
/// carry it either way.
#[derive(Clone, Debug, PartialEq)]
pub struct ProbeRecord {
    pub account: String,
    pub agent_id: String,
    pub canonical_model: String,
    pub cost_usd: f64,
    /// The recipe carried `speed: "fast"`, so its ledger rows file under speed `fast`.
    pub fast: bool,
    pub key: SessionKey,
    pub key_generation: i64,
    /// The serving account's kind, looked up by `account` after the response; `None` when the account left the
    /// registry between dispatch and landing.
    pub kind: Option<AccountKind>,
    pub outcome: ProbeOutcome,
    pub pool_id: String,
    pub usage: ProbeUsage,
}

/// What the fenced half did with the result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Applied {
    Landed { covered: bool },
    Orphaned,
}

/// The unit and the cost a probe settles in: the unit `settlement_unit` decides from the probe's typed usage
/// `types`, the same decision the gate took, and the probe's own usage priced in it.
#[allow(clippy::too_many_arguments)]
fn settled_cost(
    default_unit: SavingsUnit,
    table: &PriceTable,
    tables: &PlanTables,
    tier: Option<&str>,
    account: &str,
    canonical_model: &str,
    usage: ProbeUsage,
    types: PinUsage,
    at: i64,
) -> (SavingsUnit, f64) {
    let unit = settlement_unit(default_unit, tables, tier, canonical_model, types);
    if unit == SavingsUnit::WindowShare {
        if let Some(share) = probe_cost(unit, table, tables, tier, canonical_model, usage, at) {
            return (unit, share);
        }
        // The answer carried a type the typed usage did not foresee, such as an hour write after the pool's TTL
        // rule changed between the gate and the dispatch; a null cell never books a share, so the probe settles
        // in dollars and its savings follow.
        tracing::debug!(
            account,
            model = canonical_model,
            unpriced = unpriced_reason(tables, canonical_model),
            "a probe the cost table cannot price on the window share settles in dollars"
        );
    }
    (
        SavingsUnit::Usd,
        probe_dollars(table, canonical_model, usage),
    )
}

/// A pool-at-its-cap 429's `Retry-After` seconds, `None` for anything else or a header that does not parse.
fn retry_after_secs(headers: &axum::http::HeaderMap) -> Option<u64> {
    headers
        .get(header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
}

/// The body read frame by frame with a running total, so a response that keeps sending never grows past the
/// usage bound in memory: the first frame past it ends the read, and the response is dropped with it.
async fn read_bounded(body: Body) -> Option<Bytes> {
    let mut body = body;
    let mut out = BytesMut::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.ok()?;
        if let Ok(data) = frame.into_data() {
            if out.len() + data.len() > MAX_USAGE_BYTES {
                return None;
            }
            out.extend_from_slice(&data);
        }
    }
    Some(out.freeze())
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
        _ => ProbeOutcome::Failed,
    }
}

pub(crate) async fn run_job(engine: Arc<Engine>, job: ProbeJob) {
    let mut request = Request::builder().method("POST").uri("/v1/messages");
    for (name, value) in &job.recipe.headers {
        request = request.header(name.as_str(), value.as_str());
    }
    let request = request
        .header(header::CONTENT_LENGTH, job.recipe.body.len())
        .body(Body::from(job.recipe.body.clone()));
    let marker = crate::probe_marker(&job.key.0, job.key_generation);
    let exchange = match request {
        Err(_) => Exchange::Answered(ProbeResult {
            status: 0,
            account: None,
            error_code: None,
            body: None,
            cache_ttl_ms: None,
            in_overage: false,
            retry_after_secs: None,
            obstacle: None,
        }),
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
                let served_under = response.extensions().get::<ServedUnder>().copied();
                let retry_after_secs = retry_after_secs(response.headers());
                let body = read_bounded(response.into_body()).await;
                ProbeResult {
                    status,
                    account: outcome
                        .as_ref()
                        .map(|o| o.account_id.clone())
                        .filter(|a| !a.is_empty()),
                    error_code: outcome.as_ref().and_then(|o| o.error_code.clone()),
                    body,
                    cache_ttl_ms: outcome.as_ref().and_then(|o| o.cache_ttl_ms),
                    in_overage: served_under.is_some_and(|s| s.in_overage),
                    retry_after_secs,
                    obstacle: outcome.as_ref().and_then(|o| o.obstacle),
                }
            };
            // The whole exchange, the head and the body, sits in one arm: a cancellation or the deadline ends
            // it wherever it is, and a body that keeps coming never outlives either.
            tokio::select! {
                result = answered => Exchange::Answered(result),
                _ = engine.cancel.cancelled() => Exchange::Cancelled,
                _ = tokio::time::sleep(PROBE_DEADLINE) => Exchange::TimedOut,
            }
        }
    };
    let now = engine.now();
    let table = engine.parts.pricing.current();
    let tables = engine.parts.tables.clone();
    // The serving account's kind decides the unit this probe's cost and any later credit settle in; read from
    // the snapshot taken here, after the response, so a registry write mid-exchange never changes which account
    // this probe is charged against.
    let snapshot = engine.parts.router.snapshot();
    let (record, applied, rolled, charge) = {
        let mut inner = engine.lock();
        let mut record = inner.probe_record(&job, &exchange, &table);
        record.kind = snapshot.account(&record.account).map(|a| a.kind);
        let rolled = inner.roll_day(now);
        // A dangling account (gone from the registry between dispatch and landing) cannot serve the next
        // request either, so its probe settles in dollars like today and the credit path never matches it.
        let in_overage = matches!(&exchange, Exchange::Answered(result) if result.in_overage);
        let funding = record.kind.map(|kind| Funding::of(kind, in_overage));
        // A fast replay draws usage credits whatever the account's funding, so it settles in dollars: the pool's
        // dollar figure below carries it and the account's share figure never does.
        let default_unit = if job.recipe.fast {
            SavingsUnit::Usd
        } else {
            funding.map(Funding::unit).unwrap_or(SavingsUnit::Usd)
        };
        // The tier the store names now: a share is of the window the account has at this settlement.
        let tier = (default_unit == SavingsUnit::WindowShare)
            .then(|| {
                engine
                    .parts
                    .store
                    .subscription(&record.account)
                    .ok()
                    .flatten()
            })
            .flatten()
            .and_then(|s| s.tier);
        let (unit, cost) = settled_cost(
            default_unit,
            &table,
            &tables,
            tier.as_deref(),
            &record.account,
            &record.canonical_model,
            record.usage,
            job.types,
            now,
        );
        let mut charge = None;
        // The dollar cap counts what actually settles in dollars: an api_key account, a subscription account in
        // overage, or a plan-funded probe the cost table cannot price on the account's tier, which `unit` (the
        // settlement's decision) already reads as `Usd`. That last case is the only cap bounding such a probe at
        // all, since the plan-side cap below counts `WindowShare` alone.
        if unit == SavingsUnit::Usd {
            *inner
                .spent_today
                .entry(record.pool_id.clone())
                .or_insert(0.0) += record.cost_usd;
            charge = Some(BudgetRow {
                scope: BudgetScope::Pool,
                id: record.pool_id.clone(),
                usd: record.cost_usd,
                five_hour_share: 0.0,
            });
        }
        if unit == SavingsUnit::WindowShare {
            *inner
                .share_spent_today
                .entry(record.account.clone())
                .or_insert(0.0) += cost;
            charge = Some(BudgetRow {
                scope: BudgetScope::Account,
                id: record.account.clone(),
                usd: 0.0,
                five_hour_share: cost,
            });
        }
        // Filed under the day the in-memory sum is for: a probe whose `now` fell before a midnight another
        // writer already rolled charged the new day's sum, and its row follows it.
        let charge = charge.map(|c| (inner.spent_day, c));
        let covered_unit = funding.map(|_| unit);
        let applied = inner.apply_probe(&job, exchange, &record, covered_unit, unit, cost, now);
        (record, applied, rolled, charge)
    };
    if rolled {
        engine.purge_budgets_before(now);
    }
    if let Some((day, charge)) = charge {
        engine.persist_charge(day, &charge);
    }
    engine.record_probe(&record, applied, now);
    engine
        .parts
        .metrics
        .observe_probe(&record.account, record.outcome.as_str());
    engine.flush();
    // The worker slot comes back only after both sends: `wait_idle` returns on a zero count, and a caller that
    // asserts on the ledger right after it would otherwise race the records this probe owes.
    engine.lock().running -= 1;
    engine.idle.notify_waiters();
}

impl Inner {
    /// The immutable half: the outcome the exchange had and what it cost at the canonical model's list prices.
    /// A migrated probe is one the relay served on an account other than the job's; it is priced like any other
    /// and reported as migrated.
    pub(crate) fn probe_record(
        &self,
        job: &ProbeJob,
        exchange: &Exchange,
        table: &PriceTable,
    ) -> ProbeRecord {
        let mut record = ProbeRecord {
            account: job.account.clone().unwrap_or_else(|| "-".to_string()),
            agent_id: job.agent_id.clone(),
            canonical_model: job.canonical_model.clone(),
            cost_usd: 0.0,
            fast: job.recipe.fast,
            key: job.key.clone(),
            key_generation: job.key_generation,
            kind: None,
            outcome: ProbeOutcome::Failed,
            pool_id: job.pool_id.clone(),
            usage: ProbeUsage::default(),
        };
        let result = match exchange {
            Exchange::Cancelled => {
                record.outcome = ProbeOutcome::Cancelled;
                return record;
            }
            Exchange::TimedOut => {
                record.outcome = ProbeOutcome::TimedOut;
                return record;
            }
            Exchange::Answered(result) => result,
        };
        if let Some(account) = &result.account {
            record.account = account.clone();
        }
        record.outcome = classify_status(result.status, result.error_code.as_deref());
        if record.outcome != ProbeOutcome::Ok {
            return record;
        }
        if let Some(usage) = result.body.as_deref().and_then(ProbeUsage::parse) {
            record.usage = usage;
            // The ledger's probe-cost column is dollars whatever funded the account, so this always prices
            // through the price table, at the cells the answer's own `usage.speed` selects; the funding-unit
            // cost the agent's own net deducts is a second figure computed once the serving account's kind is
            // known, back in `run_job`. Dollars read neither a tier nor the settlement instant.
            record.cost_usd = probe_dollars(table, &job.canonical_model, usage);
        }
        if result
            .account
            .as_deref()
            .is_some_and(|served| Some(served) != job.account.as_deref())
        {
            record.outcome = ProbeOutcome::Migrated;
        }
        record
    }

    /// The fenced half, the Go `runProbe` after the call returned. The fence is the agent's incarnation and
    /// generation and the observation's id and attempt; a result that fails it touches no recipe and settles
    /// as one wasted probe under the stop that orphaned it, since the upstream served it either way.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn apply_probe(
        &mut self,
        job: &ProbeJob,
        exchange: Exchange,
        record: &ProbeRecord,
        covered_unit: Option<SavingsUnit>,
        unit: SavingsUnit,
        cost: f64,
        now: i64,
    ) -> Applied {
        let key = &job.key;
        let agent_id = job.agent_id.as_str();
        // The worker slot comes back to the record the probe started on whatever the fence says. A result
        // orphaned on a record that is still alive, by a superseding request or by a discard, would otherwise
        // leave `in_flight` set, and `step` skips an agent that has a probe in flight for good.
        if let Some(agent) = self
            .sessions
            .get_mut(key)
            .and_then(|s| s.agents.get_mut(agent_id))
            .filter(|agent| agent.incarnation == job.incarnation)
        {
            agent.in_flight = false;
        }
        let fenced = self
            .sessions
            .get(key)
            .and_then(|s| s.agents.get(agent_id))
            .is_some_and(|agent| {
                agent.incarnation == job.incarnation
                    && agent.generation == job.generation
                    && agent
                        .current
                        .as_ref()
                        .is_some_and(|o| o.id == job.observation_id && o.attempt == job.attempt)
            })
            && self.live(key, agent_id);
        if !fenced {
            // The job already charged the day and the ledger; the agent's net takes the same debit so an orphaned
            // probe is not free. A probe the upstream did not serve is debited nowhere, as on the fenced path.
            let served = matches!(record.outcome, ProbeOutcome::Ok | ProbeOutcome::Migrated);
            if let Some(agent) = self
                .sessions
                .get_mut(key)
                .and_then(|s| s.agents.get_mut(agent_id))
                .filter(|agent| served && agent.incarnation == job.incarnation)
            {
                debit(agent, unit, cost);
            }
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
            self.settlements.push(crate::engine::Settlement {
                key: key.clone(),
                key_generation: job.key_generation,
                pool_id: job.pool_id.clone(),
                canonical_model: job.canonical_model.clone(),
                account: Some(record.account.clone()),
                label,
                costs: vec![record.cost_usd],
                fast: job.recipe.fast,
            });
            return Applied::Orphaned;
        }
        let agent = self
            .sessions
            .get_mut(key)
            .unwrap()
            .agents
            .get_mut(agent_id)
            .unwrap();
        let cold_limit = agent.policy.cold_probe_limit;
        let result = match exchange {
            Exchange::Cancelled | Exchange::TimedOut => {
                self.probe_error(key, agent_id, 0, None, None, None, false, now);
                return Applied::Landed { covered: false };
            }
            Exchange::Answered(result) => result,
        };
        if record.outcome != ProbeOutcome::Ok && record.outcome != ProbeOutcome::Migrated {
            let exhausted = record.outcome == ProbeOutcome::Exhausted;
            self.probe_error(
                key,
                agent_id,
                result.status,
                result.error_code.as_deref(),
                result.obstacle,
                result.retry_after_secs,
                exhausted,
                now,
            );
            return Applied::Landed { covered: false };
        }
        // The upstream served this one and the pool was charged for it, so it goes on the recipe's own books
        // here, before any branch below pauses the recipe over what the answer said: the agent's net and the
        // settlement a stop leaves would otherwise disagree with the ledger. Every probe since the last real
        // request is unanswered until one arrives, a migrated one at its price like any other.
        let migrated = record.outcome == ProbeOutcome::Migrated;
        let agent = self
            .sessions
            .get_mut(key)
            .unwrap()
            .agents
            .get_mut(agent_id)
            .unwrap();
        agent.unanswered.push(record.cost_usd);
        debit(agent, unit, cost);
        let Some(account) = result.account.clone() else {
            self.set_reason(key, agent_id, Reason::MissingAccount);
            self.pause(key, agent_id);
            return Applied::Landed { covered: false };
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
            self.set_reason(key, agent_id, Reason::InvalidProbeUsage);
            self.pause(key, agent_id);
            return Applied::Landed { covered: false };
        };
        let agent = self
            .sessions
            .get_mut(key)
            .unwrap()
            .agents
            .get_mut(agent_id)
            .unwrap();
        agent.usage.add(usage);
        if usage.output != 0 || !content.is_empty() {
            self.set_reason(key, agent_id, Reason::UnexpectedOutput);
            self.pause(key, agent_id);
            return Applied::Landed { covered: false };
        }
        let agent = self
            .sessions
            .get_mut(key)
            .unwrap()
            .agents
            .get_mut(agent_id)
            .unwrap();
        if migrated {
            agent.account = Some(account.clone());
            agent.migrations += 1;
            agent.cold = 0;
        }
        // Full coverage is the whole prefix read back, which is what the next real request would read; a
        // write beside it is the prefix rebuilt rather than kept. A migrated probe wrote the prefix on its
        // new account instead, and that write is the coverage the next request reads there; it does not count
        // toward the cold streak.
        let covered = if migrated {
            usage.evidence() >= job.expected
        } else {
            usage.read >= job.expected
        };
        if !migrated {
            if covered {
                agent.cold = 0;
            } else {
                agent.cold += 1;
            }
        }
        let cold = agent.cold;
        if cold >= cold_limit {
            self.set_reason(key, agent_id, Reason::RepeatedColdProbe);
            self.pause(key, agent_id);
            return Applied::Landed { covered: false };
        }
        if usage.evidence() == 0 {
            self.retry(key, agent_id, Reason::NoProbeCacheEvidence, now, None);
            return Applied::Landed { covered: false };
        }
        // A probe that read part of the prefix and rebuilt the rest never warmed the whole entry; one that
        // read none of it rebuilt the entry, which the cold streak counts instead.
        if usage.evidence() < job.expected || (usage.read > 0 && !covered) {
            self.set_reason(key, agent_id, Reason::PartialCacheCoverage);
            self.pause(key, agent_id);
            return Applied::Landed { covered: false };
        }
        let agent = self
            .sessions
            .get_mut(key)
            .unwrap()
            .agents
            .get_mut(agent_id)
            .unwrap();
        agent.failures = 0;
        agent.retry_at = None;
        agent.ttl_ms = usage.ttl_ms(
            agent.ttl_ms,
            result.cache_ttl_ms.unwrap_or(job.recipe.ttl_ms),
        );
        agent.cache_started = job.started;
        agent.expires = Some(job.started + agent.ttl_ms);
        agent.jitter_frac = rand::random::<f64>();
        if covered {
            agent.covered_since_real = true;
            agent.covered_account = Some(record.account.clone());
            agent.covered_unit = covered_unit;
            agent.covered_fast = job.recipe.fast;
        }
        self.set_reason(key, agent_id, Reason::Scheduled);
        // A probe that consumed its whole refresh interval must not start a tight loop.
        if self.sessions[key].agents[agent_id]
            .due_at()
            .is_some_and(|due| due <= now)
        {
            self.retry(key, agent_id, Reason::SlowProbe, now, None);
        }
        Applied::Landed { covered }
    }

    pub(crate) fn pause(&mut self, key: &SessionKey, agent_id: &str) {
        if let Some(agent) = self
            .sessions
            .get_mut(key)
            .and_then(|s| s.agents.get_mut(agent_id))
        {
            agent.pause();
        }
    }

    /// The Go `probeErrorLocked`, read from the answer the relay gave: a 429 after failover, or the exhaustion
    /// 429 the relay answers when every account is cooled, is the quota pause; a pool at its in-flight cap
    /// (`accounts_at_cap`) retries after the header's wait instead, since a slot there may free up before the
    /// next real request would; a probe the selector refused on the pinned member's spend budget holds under
    /// `budget` without a pause, since only a raised budget or the period's end clears it, never a wait; a probe
    /// the selector answered on its pinned member's standing holds under that standing: `Paused` for the
    /// operator's pause, `IneligibleCredential` for an ended subscription (both read `paused` in the console), a
    /// retry for a member this probe already tried, or for one whose budget the degraded ledger leaves unknown,
    /// which the ledger's recovery clears without a real request; a pinned member a leg of the probe marked for
    /// re-authentication pauses under the authentication reason; the untimed exhaustion 503, when every account of
    /// the pool was unusable at selection, pauses under its own reason; a 5xx forwarded from upstream retries; any
    /// other 4xx is a shape the upstream refuses; a cancelled, timed-out, or lost exchange (status 0) is a
    /// transport error.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn probe_error(
        &mut self,
        key: &SessionKey,
        agent_id: &str,
        status: u16,
        error_code: Option<&str>,
        obstacle: Option<&str>,
        retry_after_secs: Option<u64>,
        exhausted: bool,
        now: i64,
    ) {
        match (exhausted, obstacle) {
            (true, Some("operator")) => {
                self.set_reason(key, agent_id, Reason::Paused);
                self.pause(key, agent_id);
                return;
            }
            (true, Some("subscription_ended")) => {
                self.set_reason(key, agent_id, Reason::IneligibleCredential);
                self.pause(key, agent_id);
                return;
            }
            // A moved key's next probe admits under its new pool.
            (true, Some("attempted" | "budget_unknown" | "pool_moved")) => {
                return self.retry(key, agent_id, Reason::RetryableProbeError, now, None)
            }
            (true, Some("reauth_required")) => {
                self.set_reason(key, agent_id, Reason::ProbeAuthenticationError);
                self.pause(key, agent_id);
                return;
            }
            _ => {}
        }
        if status == 429 && error_code == Some("accounts_at_cap") {
            // Retrying, not pausing: the pool is momentarily full, not out of quota, and a slot frees up in
            // seconds, so `RetryableProbeError` ("retrying") is the honest label rather than the quota one.
            let delay_ms = retry_after_secs.map(|secs| {
                i64::try_from(secs)
                    .unwrap_or(i64::MAX)
                    .saturating_mul(1_000)
            });
            self.retry(key, agent_id, Reason::RetryableProbeError, now, delay_ms);
            return;
        }
        if status == 429 && obstacle == Some(BUDGET_WINDOW) {
            // The pinned member reached its spend budget between the gate's read and the selection. A pause
            // would outlive the budget (nothing but a real request undoes one); the reason alone leaves the
            // schedule armed, so the gate re-reads the spend on the next poll and releases the probe itself
            // when the period ends or the budget is raised.
            self.set_reason(key, agent_id, Reason::Budget);
            return;
        }
        let reason = match status {
            429 => Reason::ProbeQuotaExhausted,
            _ if exhausted => Reason::ProbePoolExhausted,
            500..=599 => return self.retry(key, agent_id, Reason::RetryableProbeError, now, None),
            401 | 403 => Reason::ProbeAuthenticationError,
            400..=499 => Reason::UnsupportedProbe,
            _ => Reason::ProbeTransportError,
        };
        self.set_reason(key, agent_id, reason);
        self.pause(key, agent_id);
    }

    /// Exponential backoff from the pool's `retry_seconds`, capped at its `retry_max_seconds`, unless
    /// `override_delay_ms` names this attempt's own wait (the `accounts_at_cap` header); the cold-probe limit
    /// bounds consecutive retries either way. A retry that falls past the entry's expiry needs no check here: the
    /// due time is the later of the two, so the first tick at or after the expiry holds the recipe as lapsed
    /// before the retry could send.
    pub(crate) fn retry(
        &mut self,
        key: &SessionKey,
        agent_id: &str,
        reason: Reason,
        now: i64,
        override_delay_ms: Option<i64>,
    ) {
        let agent = self
            .sessions
            .get_mut(key)
            .unwrap()
            .agents
            .get_mut(agent_id)
            .unwrap();
        agent.failures += 1;
        if agent.failures >= agent.policy.cold_probe_limit {
            self.set_reason(key, agent_id, Reason::ProbeRetryLimit);
            self.pause(key, agent_id);
            return;
        }
        let delay = match override_delay_ms {
            Some(delay) => delay.min(agent.policy.retry_max_ms()),
            None => {
                let exponent = (agent.failures - 1).min(4);
                (agent.policy.retry_ms() << exponent).min(agent.policy.retry_max_ms())
            }
        };
        agent.retry_at = Some(now + delay);
        self.set_reason(key, agent_id, reason);
    }

    pub(crate) fn idle_limited(&self, key: &SessionKey, agent_id: &str, now: i64) -> bool {
        let Some(session) = self.sessions.get(key) else {
            return false;
        };
        let Some(agent) = session.agents.get(agent_id) else {
            return false;
        };
        let (counted, since) = if !session.attended {
            (true, session.last_real_at.unwrap_or(agent.last_activity))
        } else {
            (
                agent.id == crate::engine::MAIN_AGENT
                    && agent.state == crate::engine::AgentState::Idle,
                agent.last_activity,
            )
        };
        counted
            && ((agent.policy.max_idle_ticks > 0
                && agent.idle_ticks >= agent.policy.max_idle_ticks)
                || agent
                    .policy
                    .max_idle_ms()
                    .is_some_and(|limit| now >= since + limit))
    }
}

/// A paid probe on the agent's net. The grace budget is granted on the way into a negative net, so the first paid
/// probe leaves the configured allowance; each further paid probe while negative spends one.
fn debit(agent: &mut Agent, unit: SavingsUnit, cost: f64) {
    let was_negative = agent.net.in_unit(unit) < 0.0;
    agent.net.add(unit, -cost);
    if agent.net.in_unit(unit) < 0.0 && !was_negative {
        agent.grace_left = agent.policy.net_negative_grace_probes;
    } else if agent.net.in_unit(unit) < 0.0 && agent.grace_left > 0 {
        agent.grace_left -= 1;
    }
}

pub(crate) fn wasted_label(reason: Reason) -> Option<Wasted> {
    match reason {
        Reason::IdleLimit => Some(Wasted::IdleLimit),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::config::FIVE_MINUTES_MS;
    use crate::engine::{Agent, Session};
    use crate::savings::probe_types;

    use super::*;

    /// One session holding one never-probed agent, the minimum `probe_error`/`retry` need, built onto the same
    /// `Inner::empty` a fresh engine starts from.
    fn one_agent_inner() -> (Inner, SessionKey) {
        let key: SessionKey = ("k".into(), "s".into());
        let mut agents = BTreeMap::new();
        agents.insert("main".to_string(), Agent::new("main", "k", 1, 0));
        let mut inner = Inner::empty(None);
        inner.agents = 1;
        inner.sessions.insert(
            key.clone(),
            Session {
                key_id: "k".into(),
                key_generation: 0,
                session_id: "s".into(),
                instance: "i".into(),
                sequence: 0,
                lease: i64::MAX,
                attended: true,
                ended: false,
                owner_id: None,
                last_report_at: 0,
                last_real_at: Some(0),
                agents,
            },
        );
        (inner, key)
    }

    #[test]
    fn a_pinned_answer_holds_the_agent_under_its_obstacle() {
        for (obstacle, reason, paused) in [
            ("operator", Reason::Paused, true),
            ("subscription_ended", Reason::IneligibleCredential, true),
            ("attempted", Reason::RetryableProbeError, false),
        ] {
            let (mut inner, key) = one_agent_inner();
            inner.probe_error(
                &key,
                "main",
                503,
                Some("accounts_unavailable"),
                Some(obstacle),
                None,
                true,
                1_000,
            );
            let agent = &inner.sessions[&key].agents["main"];
            assert_eq!(
                (agent.reason, agent.paused),
                (reason, paused),
                "the {obstacle} answer holds the agent under its own standing, not the exhausted pool"
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
            503,
            Some("accounts_unavailable"),
            Some("pool_moved"),
            Some(1),
            true,
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
            429,
            Some("accounts_at_cap"),
            None,
            Some(3),
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
    fn every_other_429_still_pauses_as_probe_quota_exhausted() {
        let (mut inner, key) = one_agent_inner();
        inner.probe_error(&key, "main", 429, None, None, None, false, 1_000);
        let agent = &inner.sessions[&key].agents["main"];
        assert!(agent.paused);
        assert_eq!(agent.reason, Reason::ProbeQuotaExhausted);
    }

    #[test]
    fn accounts_at_cap_without_a_header_falls_back_to_the_usual_backoff() {
        let (mut inner, key) = one_agent_inner();
        inner.probe_error(
            &key,
            "main",
            429,
            Some("accounts_at_cap"),
            None,
            None,
            false,
            1_000,
        );
        let agent = &inner.sessions[&key].agents["main"];
        assert!(!agent.paused);
        assert_eq!(agent.retry_at, Some(1_000 + agent.policy.retry_ms()));
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

    /// A table pricing a model the shipped cost table has no row for, so `settled_cost` must fall all the way
    /// back to dollars rather than mixing a dollar figure into a share-priced settlement.
    fn table_pricing_an_unlisted_model() -> PriceTable {
        PriceTable {
            version: "test".into(),
            effective: "test".into(),
            source: "test".into(),
            models: BTreeMap::from([(
                "unlisted-model".to_string(),
                phoebis_ledger::ModelRow {
                    standard: phoebis_ledger::ModelPrice {
                        input: 2.0,
                        cache_write_5m: 2.5,
                        cache_write_1h: 4.0,
                        cache_read: 0.2,
                        output: 10.0,
                    },
                    fast: None,
                },
            )]),
        }
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
        let table = table_pricing_an_unlisted_model();
        let tables = PlanTables::shipped();
        let usage = ProbeUsage {
            write: 2_000,
            five_minutes: 2_000,
            ..ProbeUsage::default()
        };
        let (unit, cost) = settled_cost(
            SavingsUnit::WindowShare,
            &table,
            &tables,
            MAX_20X,
            "o",
            "unlisted-model",
            usage,
            probe_types(FIVE_MINUTES_MS, FIVE_MINUTES_MS),
            OFF_BAND,
        );
        assert_eq!(
            unit,
            SavingsUnit::Usd,
            "the cost table has no row, so the unit falls back too"
        );
        let expected = probe_cost(
            SavingsUnit::Usd,
            &table,
            &tables,
            MAX_20X,
            "unlisted-model",
            usage,
            OFF_BAND,
        )
        .unwrap();
        assert!(expected > 0.0, "{expected}");
        assert_eq!(cost, expected);
    }

    // The test table: Opus 5.5 weighs 2 and an input token multiplies by 1, the Max 20x 5-hour window holds 100
    // units, and the band holds 0.5 of it.
    #[test]
    fn a_plan_funded_probe_on_a_listed_model_settles_in_five_hour_share() {
        let table = phoebis_ledger::test_tables::api_prices();
        let tables = phoebis_ledger::test_tables::subscription_costs();
        let one_input = ProbeUsage {
            input: 1,
            ..ProbeUsage::default()
        };
        let settle = |at: i64| {
            settled_cost(
                SavingsUnit::WindowShare,
                &table,
                &tables,
                MAX_20X,
                "o",
                "claude-opus-5-5",
                one_input,
                probe_types(FIVE_MINUTES_MS, FIVE_MINUTES_MS),
                at,
            )
        };
        let (unit, cost) = settle(OFF_BAND);
        assert_eq!(unit, SavingsUnit::WindowShare);
        assert!(
            (cost - 2.0 / 100.0).abs() < 1e-12,
            "one Opus 5.5 input token is its 2 units over the window's 100: {cost}"
        );
        let (_, banded) = settle(IN_BAND);
        assert!(
            (banded - 2.0 / 100.0 / 0.5).abs() < 1e-12,
            "the band is inside the price at the settlement instant: {banded}"
        );
    }

    #[test]
    fn a_plan_funded_probe_on_an_account_without_a_tier_settles_in_dollars() {
        let table = PriceTable::shipped();
        let tables = PlanTables::shipped();
        let (unit, cost) = settled_cost(
            SavingsUnit::WindowShare,
            &table,
            &tables,
            None,
            "o",
            "claude-opus-5",
            million_input(),
            probe_types(FIVE_MINUTES_MS, FIVE_MINUTES_MS),
            OFF_BAND,
        );
        let dollars = probe_cost(
            SavingsUnit::Usd,
            &table,
            &tables,
            None,
            "claude-opus-5",
            million_input(),
            OFF_BAND,
        )
        .unwrap();
        assert_eq!(
            (unit, cost),
            (SavingsUnit::Usd, dollars),
            "no tier names the window the share would be of, so the probe settles in dollars"
        );
        assert!(dollars > 0.0, "{dollars}");
    }

    #[test]
    fn a_plan_funded_probe_on_a_model_without_a_weight_settles_in_dollars() {
        // The test table leaves Opus 5 without a weight; the test price table lists it.
        let table = phoebis_ledger::test_tables::api_prices();
        let tables = phoebis_ledger::test_tables::subscription_costs();
        let (unit, cost) = settled_cost(
            SavingsUnit::WindowShare,
            &table,
            &tables,
            MAX_20X,
            "o",
            "claude-opus-5",
            million_input(),
            probe_types(FIVE_MINUTES_MS, FIVE_MINUTES_MS),
            OFF_BAND,
        );
        let dollars = probe_cost(
            SavingsUnit::Usd,
            &table,
            &tables,
            None,
            "claude-opus-5",
            million_input(),
            OFF_BAND,
        )
        .unwrap();
        assert_eq!(
            (unit, cost),
            (SavingsUnit::Usd, dollars),
            "Opus 5 has no weight, so the share is unpriced and the dollar unit governs"
        );
        assert!(dollars > 0.0);
    }
}
