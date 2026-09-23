//! One probe: the recipe replayed through the relay's failover loop under the recipe's key, and the result read
//! from the response the relay returns rather than from the bus, which is best-effort. What the probe cost is
//! settled from the exchange alone; what it does to the recipe is settled only when the recipe is still the
//! one the probe ran for.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request};
use bytes::{Bytes, BytesMut};
use phoebis_ledger::PriceTable;
use phoebis_relay::RelayOutcome;
use http_body_util::BodyExt;
use serde_json::Value;

use crate::engine::{Engine, Inner, Reason, SessionKey, Wasted};
use crate::recipe::Recipe;
use crate::savings::probe_cost;
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
}

pub(crate) struct ProbeResult {
    pub status: u16,
    pub account: Option<String>,
    /// The relay's own code for an answer it built itself, `accounts_exhausted` and `accounts_unavailable`
    /// among them; `None` for an upstream answer forwarded as it came.
    pub error_code: Option<String>,
    /// `None` when the read crossed the usage bound or failed: nothing past it is a usage block.
    pub body: Option<Bytes>,
    /// The TTL the relay dispatched the probe's body with, off `RelayOutcome::cache_ttl_ms`; `None` when the
    /// body went as sent.
    pub cache_ttl_ms: Option<i64>,
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
    pub key: SessionKey,
    pub key_generation: i64,
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
    let marker = crate::probe_marker(&engine, &job.key.0, job.key_generation);
    let exchange = match request {
        Err(_) => Exchange::Answered(ProbeResult {
            status: 0,
            account: None,
            error_code: None,
            body: None,
            cache_ttl_ms: None,
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
    let (record, applied) = {
        let mut inner = engine.lock();
        let record = inner.probe_record(&job, &exchange, &table);
        inner.roll_day(now);
        if record.outcome != ProbeOutcome::Migrated {
            *inner
                .spent_today
                .entry(record.pool_id.clone())
                .or_insert(0.0) += record.cost_usd;
        }
        let applied = inner.apply_probe(&job, exchange, &record, now);
        (record, applied)
    };
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
    /// A migrated probe is one the relay served on an account other than the job's; it costs nothing here (the
    /// write it paid is the one the next request would have paid there) and is reported as migrated.
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
            key: job.key.clone(),
            key_generation: job.key_generation,
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
            record.cost_usd = probe_cost(table, &job.canonical_model, usage).unwrap_or(0.0);
        }
        if result
            .account
            .as_deref()
            .is_some_and(|served| Some(served) != job.account.as_deref())
        {
            record.outcome = ProbeOutcome::Migrated;
            record.cost_usd = 0.0;
        }
        record
    }

    /// The fenced half, the Go `runProbe` after the call returned. The fence is the agent's incarnation and
    /// generation and the observation's id and attempt; a result that fails it touches no recipe and settles
    /// as one wasted probe under the stop that orphaned it, since the upstream served it either way.
    pub(crate) fn apply_probe(
        &mut self,
        job: &ProbeJob,
        exchange: Exchange,
        record: &ProbeRecord,
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
                self.probe_error(key, agent_id, 0, false, now);
                return Applied::Landed { covered: false };
            }
            Exchange::Answered(result) => result,
        };
        if record.outcome != ProbeOutcome::Ok && record.outcome != ProbeOutcome::Migrated {
            let exhausted = record.outcome == ProbeOutcome::Exhausted;
            self.probe_error(key, agent_id, result.status, exhausted, now);
            return Applied::Landed { covered: false };
        }
        // The upstream served this one and the pool was charged for it, so it goes on the recipe's own books
        // here, before any branch below pauses the recipe over what the answer said: the agent's net and the
        // settlement a stop leaves would otherwise disagree with the ledger. Every probe since the last real
        // request is unanswered until one arrives; a migrated probe carries no cost (the write it paid is the
        // one the next request would have paid there) but is still a probe nobody came back for.
        let migrated = record.outcome == ProbeOutcome::Migrated;
        let agent = self
            .sessions
            .get_mut(key)
            .unwrap()
            .agents
            .get_mut(agent_id)
            .unwrap();
        agent
            .unanswered
            .push(if migrated { 0.0 } else { record.cost_usd });
        if !migrated {
            let was_negative = agent.net_usd < 0.0;
            agent.net_usd -= record.cost_usd;
            // The grace budget is granted on the way into a negative net, so the first paid probe leaves the
            // configured allowance; each further paid probe while negative spends one.
            if agent.net_usd < 0.0 && !was_negative {
                agent.grace_left = agent.policy.net_negative_grace_probes;
            } else if agent.net_usd < 0.0 && agent.grace_left > 0 {
                agent.grace_left -= 1;
            }
        }
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
        // new account instead, and that write is the coverage the next request reads there; it counts for
        // neither the cold streak nor the net.
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
            self.retry(key, agent_id, Reason::NoProbeCacheEvidence, now);
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
        }
        self.set_reason(key, agent_id, Reason::Scheduled);
        // A probe that consumed its whole refresh interval must not start a tight loop.
        if self.sessions[key].agents[agent_id]
            .due_at()
            .is_some_and(|due| due <= now)
        {
            self.retry(key, agent_id, Reason::SlowProbe, now);
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
    /// 429 the relay answers when every account is cooled, is the quota pause; the untimed exhaustion 503, when
    /// every account of the pool was unusable at selection, pauses under its own reason; a 5xx forwarded from
    /// upstream retries; any other 4xx is a shape the upstream refuses; a cancelled, timed-out, or lost exchange
    /// (status 0) is a transport error.
    pub(crate) fn probe_error(
        &mut self,
        key: &SessionKey,
        agent_id: &str,
        status: u16,
        exhausted: bool,
        now: i64,
    ) {
        let reason = match status {
            429 => Reason::ProbeQuotaExhausted,
            _ if exhausted => Reason::ProbePoolExhausted,
            500..=599 => return self.retry(key, agent_id, Reason::RetryableProbeError, now),
            401 | 403 => Reason::ProbeAuthenticationError,
            400..=499 => Reason::UnsupportedProbe,
            _ => Reason::ProbeTransportError,
        };
        self.set_reason(key, agent_id, reason);
        self.pause(key, agent_id);
    }

    /// Exponential backoff from the pool's `retry_seconds`, capped at its `retry_max_seconds`; the cold-probe
    /// limit bounds consecutive failures as it bounds cold probes.
    pub(crate) fn retry(&mut self, key: &SessionKey, agent_id: &str, reason: Reason, now: i64) {
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
        let exponent = (agent.failures - 1).min(4);
        let delay = (agent.policy.retry_ms() << exponent).min(agent.policy.retry_max_ms());
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

pub(crate) fn wasted_label(reason: Reason) -> Option<Wasted> {
    match reason {
        Reason::IdleLimit => Some(Wasted::IdleLimit),
        _ => None,
    }
}
