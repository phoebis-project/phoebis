//! Record v1: the exact log of one routing decision, what the engine read in the order it read it and what it
//! wrote. It travels beside the attempt's `SelectionTrace`, which the analytics keep reading; nothing a rollup
//! reads lives here alone. A word from a fixed vocabulary is a `&'static str`, an entity is its id, and a field
//! the decision did not reach serializes as `null`.

use std::collections::BTreeMap;

use phoebis_store::PoolRow;

use crate::select::{ClientClass, CohortHold, HoldFacts, Placement, SelectRequest, MIGRATE_BAND};
use crate::state::Tier;
use crate::RegistrySnapshot;

pub const RECORD_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct DecisionRecord {
    pub version: u32,
    pub request: RequestFacts,
    /// `None` for a request that pins nothing, with `pin_reason` beside it.
    pub pin: Option<PinFacts>,
    /// `pinning_off`; `None` beside a pin block.
    pub pin_reason: Option<&'static str>,
    /// `None` for a refusal whose key or pool the snapshot no longer holds (a revoked key's), written as `null`.
    pub settings: Option<Settings>,
    pub candidates: Vec<CandidateFacts>,
    pub walk: Vec<WalkStep>,
    pub outcome: Outcome,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct RequestFacts {
    pub request_id: String,
    pub attempt: u32,
    pub seq: u32,
    pub at: i64,
    pub key_id: String,
    pub key_generation: i64,
    pub pool_id: String,
    pub agent_id: String,
    pub parent_agent_id: Option<String>,
    pub client_class: &'static str,
    pub request_class: Option<String>,
    pub canonical_model: String,
    pub lane: LaneFacts,
    pub flags: RequestFlags,
    pub chain: ChainFacts,
    pub attempted: Vec<String>,
    pub holds: Vec<HoldFacts>,
    pub registry_revision: u64,
}

/// The request's cache lane split from its `lane_key` spelling.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct LaneFacts {
    pub speed: String,
    pub thinking: String,
    pub effort: String,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct RequestFlags {
    pub fast: bool,
    pub count: bool,
    pub probe: bool,
    pub portable: bool,
    pub marker: bool,
    pub context_compacted: bool,
}

/// The chains' counts, never the hashes: the record keeps a matched boundary's hash in the prefix lookups alone.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct ChainFacts {
    pub static_boundaries: u32,
    pub boundaries: u32,
    pub static_hash: bool,
}

/// Step 1's reading of the pin. `found` is `live`, `expired_idle`, `left_pool` or `none`; the row's fields are
/// `None` under `none`, and the warmth pair under anything but `live`.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct PinFacts {
    pub found: &'static str,
    pub account_id: Option<String>,
    pub epoch: Option<u64>,
    pub incarnation: Option<u64>,
    pub created_at: Option<i64>,
    pub last_used_at: Option<i64>,
    pub last_refresh_at: Option<i64>,
    pub idle_ms: Option<i64>,
    pub refresh_age_ms: Option<i64>,
    pub lane: Option<String>,
    pub shed: Option<PinShed>,
    pub marked: bool,
    pub warmth: Option<&'static str>,
    pub warmth_cause: Option<&'static str>,
    pub released: Vec<&'static str>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct PinShed {
    pub from: String,
    pub at: i64,
}

/// The values that entered the decision, each named by the `PoolRow` field it is read from, so a renamed setting
/// renames here with no second edit.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Settings {
    pub strategy: &'static str,
    pub prefer_subscription: bool,
    pub overage_policy: &'static str,
    pub affinity_ttl_seconds: u64,
    pub affinity_mode: &'static str,
    pub session_placement: &'static str,
    pub min_shared_prefix_tokens: u64,
    pub subagent_placement: &'static str,
    pub select_below_utilization: Option<f64>,
    pub migrate_above_utilization: Option<f64>,
    pub migrate_when_spent: bool,
    pub overage_move_grace_minutes: u32,
    pub wind_down_minutes: u32,
    pub shed_share: f64,
    pub burn_window_seconds: u64,
    pub source_dwell_seconds: u64,
    pub warm_sessions: &'static str,
    pub in_flight_cap: u32,
    pub retry_in_place_seconds: u64,
    pub pace_floor: f64,
    pub migrate_band: f64,
    pub pool_updated_at: i64,
}

/// One pool member's standing at decision time, eligible or not, in the candidate list's order.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct CandidateFacts {
    pub account_id: String,
    pub incarnation: u64,
    pub kind: &'static str,
    pub plan_tier: Option<String>,
    pub enabled: bool,
    pub state: &'static str,
    pub priority: u32,
    /// `choose`'s `key_of` computed for every member: group, priority, overage bit.
    pub tier: Tier,
    pub in_overage: bool,
    pub overage: Option<OverageFacts>,
    pub in_flight: u32,
    pub in_flight_cap: u32,
    /// By window name; the three plan windows for a subscription member, the worst bucket and the two fast buckets
    /// for an api_key one, each as `predicted` reads it at decision time.
    pub windows: BTreeMap<String, WindowFacts>,
    pub allowance_known: bool,
    /// Filled for every member under Pace, chosen or not; `None` under another strategy.
    pub pace: Option<PaceFacts>,
    pub budget: Vec<BudgetFacts>,
    pub cooldowns: Vec<CooldownFacts>,
    pub credits: Option<CreditsFacts>,
    pub term: Option<TermFacts>,
    pub prefix: PrefixLookup,
    pub obstacle: Option<ObstacleFacts>,
    /// The `narrow` label that dropped the member, `None` for one every partition kept.
    pub partition: Option<&'static str>,
    pub rank: Option<u32>,
    pub score: Option<f64>,
    /// `chosen`, `renewed`, `held_on`, `source`, `destination`, `eligible`, `ranked`, `out`, `not_needed`.
    pub verdict: &'static str,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct OverageFacts {
    pub since: i64,
    pub reset: Option<i64>,
    pub source: &'static str,
    pub observed_at: i64,
}

/// `ahead` is the lead over pace `select.rs`'s `ahead` reads for the two shared plan windows; `None` elsewhere.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct WindowFacts {
    pub utilization: f64,
    pub reset: Option<i64>,
    pub source: &'static str,
    pub observed_at: i64,
    pub age_ms: i64,
    pub ahead: Option<f64>,
}

/// `end`, `deadline` and `hours` are `None` for an api_key member, which Pace scores by its remaining share alone;
/// a subscription member with neither a reset nor an end reads a week of hours.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct PaceFacts {
    pub remaining: f64,
    pub reset: Option<i64>,
    pub end: Option<i64>,
    pub deadline: Option<i64>,
    pub hours: Option<f64>,
    pub score: f64,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct BudgetFacts {
    pub period: &'static str,
    pub budget_usd: f64,
    pub spent_usd: f64,
    pub period_start: i64,
    pub period_end: i64,
    pub observed_at: i64,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct CooldownFacts {
    pub scope: String,
    pub reason: String,
    pub window: String,
    pub until: i64,
    pub blocking: bool,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct CreditsFacts {
    pub enabled: bool,
    pub disabled_reason: Option<String>,
    pub utilization: Option<f64>,
    pub observed_at: i64,
    pub blocked: Option<String>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct TermFacts {
    pub since: Option<i64>,
    pub trial_ends_at: Option<i64>,
    pub cancelled: bool,
    pub ends_at: Option<i64>,
    pub winding_down: bool,
    pub ended: bool,
}

/// Per rung, the member's deepest visited entry (`content`, `static_`) and, apart from it, the entry the rung
/// accepted on this member (`content_match`, `static_match`): the same entry when the deepest was accepted, a
/// shallower one when a deeper entry was refused, `None` when the rung accepted none of the member's or did not
/// run. The card's chips read the match; the deepest names the gate that refused it.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct PrefixLookup {
    pub content: Option<PrefixEntry>,
    pub content_match: Option<PrefixEntry>,
    #[serde(rename = "static")]
    pub static_: Option<PrefixEntry>,
    pub static_match: Option<PrefixEntry>,
    pub with_main: bool,
}

/// One entry the rung walk visited on a member: the boundary's hash as the affinity map keys it (64 lowercase hex
/// characters), the entry's fields, and the gate that refused it (`lane`, `expired`, `incarnation`, `not_admitted`,
/// `size_gate`, `at_cap`, `above_ceiling`, `winding_down`, `floor`, `band`) or `None` for an accepted one.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct PrefixEntry {
    pub hash: String,
    pub depth: u32,
    pub prefix_tokens: u64,
    pub written_at: i64,
    pub ttl_ms: i64,
    pub live: bool,
    pub refused_by: Option<&'static str>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct ObstacleFacts {
    pub name: &'static str,
    pub reason: Option<String>,
    pub window: Option<String>,
    pub until: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct WalkStep {
    pub step: &'static str,
    pub verdict: &'static str,
    pub facts: Vec<Fact>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Fact {
    pub key: &'static str,
    pub value: FactValue,
    pub account_id: Option<String>,
    pub unit: Option<&'static str>,
}

impl Fact {
    fn of(key: &'static str, value: FactValue) -> Fact {
        Fact {
            key,
            value,
            account_id: None,
            unit: None,
        }
    }

    pub(crate) fn bool(key: &'static str, value: bool) -> Fact {
        Fact::of(key, FactValue::Bool(value))
    }

    pub(crate) fn int(key: &'static str, value: i64) -> Fact {
        Fact::of(key, FactValue::Int(value))
    }

    /// A count or a size the engine holds unsigned, saturating at `i64::MAX`.
    pub(crate) fn count(key: &'static str, value: u64) -> Fact {
        Fact::int(key, i64::try_from(value).unwrap_or(i64::MAX))
    }

    pub(crate) fn number(key: &'static str, value: f64) -> Fact {
        Fact::of(key, FactValue::Number(value))
    }

    pub(crate) fn text(key: &'static str, value: impl Into<String>) -> Fact {
        Fact::of(key, FactValue::Text(value.into()))
    }

    pub(crate) fn list(key: &'static str, value: Vec<String>) -> Fact {
        Fact::of(key, FactValue::List(value))
    }

    pub(crate) fn unit(self, unit: &'static str) -> Fact {
        Fact {
            unit: Some(unit),
            ..self
        }
    }

    pub(crate) fn about(self, account_id: &str) -> Fact {
        Fact {
            account_id: Some(account_id.to_string()),
            ..self
        }
    }
}

/// A fact's value as its bare JSON form, so a reader takes the number, the word or the list without a tag.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(untagged)]
pub enum FactValue {
    Bool(bool),
    Int(i64),
    Number(f64),
    Text(String),
    List(Vec<String>),
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Outcome {
    /// `placed`, `renewed`, `moved`, `held`, `exhausted` or `refused`; a probe's decision keeps its kind and the
    /// request's `probe` flag marks it.
    pub kind: &'static str,
    pub account_id: Option<String>,
    pub from_account_id: Option<String>,
    pub upstream_model: Option<String>,
    pub tier: Option<Tier>,
    pub strategy: &'static str,
    pub rule: Option<&'static str>,
    pub rung: Option<Placement>,
    pub in_overage: bool,
    pub pin_written: Option<PinWritten>,
    /// The routing task's own emits, in the order they follow the decision on the broadcast: none for a decision
    /// that moved no pin, one for a fresh pin, a move or an expiry, two for an expired pin the selection replaced
    /// (`pin_expired` then `pin_created`). Filled after the selection and before the record leaves the task.
    pub pin_events: Vec<PinEventFacts>,
    pub hold: Option<HoldOutcome>,
    pub exhaustion: Option<ExhaustionFacts>,
    pub refusal: Option<RefusalFacts>,
    pub cohort_hold: Option<CohortHold>,
    /// The pins the shedding round this decision ran marked; `None` when no round ran.
    pub shed_marks: Option<Vec<PinKeyFacts>>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct PinWritten {
    pub action: &'static str,
    pub epoch: u64,
    pub lane: String,
    pub refresh_stamped: bool,
    pub hashes_stamped: bool,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct PinEventFacts {
    pub kind: &'static str,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct HoldOutcome {
    pub account_id: String,
    pub cohort: Option<String>,
    pub bound_ms: i64,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct ExhaustionFacts {
    pub kind: &'static str,
    pub deadline: Option<i64>,
    pub window: Option<String>,
    pub reason: Option<String>,
    pub obstacle: Option<&'static str>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct RefusalFacts {
    pub kind: &'static str,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct PinKeyFacts {
    pub key_id: String,
    pub session_id: String,
    pub agent_id: String,
    pub model: String,
}

/// The list route's decision row before the ledger joins the attempt and the request: what the router has in hand
/// when the decision is made, so the route never decodes a frame.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct DecisionSummary {
    pub kind: &'static str,
    pub probe: bool,
    pub by: By,
    pub fact: Vec<SummaryFact>,
    pub rule: Option<&'static str>,
    pub strategy: &'static str,
    pub tier: Option<Tier>,
    pub rung: Option<Placement>,
    pub score: Option<Score>,
    pub runner_up: Option<Score>,
    pub skipped: u32,
    pub warmth: Option<WarmthFacts>,
    pub hold: Option<HoldOutcome>,
    pub exhaustion: Option<ExhaustionFacts>,
    pub refusal: Option<RefusalFacts>,
    pub fast: bool,
}

/// `kind` is `rule`, `rung`, `strategy`, `hold`, `exhaustion`, `refusal` or `none`; `value` the rule's, the rung's,
/// the strategy's, the hold's place, the exhaustion's kind or the refusal's kind.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct By {
    pub kind: &'static str,
    pub value: String,
    pub tier: Option<Tier>,
}

/// `key` is one of the list route's fact keys; `unit` one of `ms`, `tokens`, `percent`, `percent_per_hour`, `usd`,
/// `time`, `count`, `text`.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct SummaryFact {
    pub key: &'static str,
    pub value: FactValue,
    pub unit: Option<&'static str>,
    pub entity: Option<FactEntity>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct FactEntity {
    pub kind: &'static str,
    pub id: String,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Score {
    pub account_id: String,
    pub score: f64,
    pub unit: &'static str,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct WarmthFacts {
    pub verdict: &'static str,
    pub cause: &'static str,
}

/// The identity a refused request has before any selection: what the relay holds at its own refusal site and what
/// the task reads off a `SelectRequest` at its three. `attempt` and `seq` are 1 for the relay's refusal, the
/// request's one decision; `at` and the client's version as the relay read them.
#[derive(Clone, Debug, PartialEq)]
pub struct RefusedRequest {
    pub request_id: String,
    pub attempt: u32,
    pub seq: u32,
    pub at: i64,
    pub key_id: String,
    pub key_generation: i64,
    pub agent_id: String,
    pub session_id: Option<String>,
    pub canonical_model: String,
    pub client: ClientClass,
    pub client_version: Option<String>,
}

impl From<&SelectRequest> for RefusedRequest {
    fn from(request: &SelectRequest) -> RefusedRequest {
        RefusedRequest {
            request_id: request.request_id.clone(),
            attempt: request.attempt,
            seq: request.seq,
            at: request.now,
            key_id: request.key_id.clone(),
            key_generation: request.key_generation,
            agent_id: request.agent_id.clone(),
            session_id: request.session_id.clone(),
            canonical_model: request.canonical_model.clone(),
            client: request.client,
            client_version: None,
        }
    }
}

impl DecisionRecord {
    /// The record of a request refused before the engine ran: the request block from the identity, no pin block
    /// (`pin_reason` `not_looked_up`), the settings when the key and its pool are still in the snapshot and `None`
    /// otherwise, no candidate, a two-line walk (`admission` `refused` with the reason, `selection` `not_run`) and
    /// a `refused` outcome carrying `refusal`; with its summary (`by` `refusal`, `fact` the reason's key: `client`
    /// for a drift refusal, whose reason names the trigger and the client version, `key` for a key or a resolve
    /// refusal, whose reason is the refusal's `Display`).
    pub fn refused(
        request: &RefusedRequest,
        snapshot: &RegistrySnapshot,
        refusal: &RefusalFacts,
    ) -> (DecisionRecord, DecisionSummary) {
        let key = snapshot.key(&request.key_id);
        let pool = key.and_then(|key| snapshot.pool(&key.pool_id));
        let strategy = pool.map_or("", |pool| pool.strategy.as_str());
        let record = DecisionRecord {
            version: RECORD_VERSION,
            request: RequestFacts {
                request_id: request.request_id.clone(),
                attempt: request.attempt,
                seq: request.seq,
                at: request.at,
                key_id: request.key_id.clone(),
                key_generation: request.key_generation,
                pool_id: key.map(|key| key.pool_id.clone()).unwrap_or_default(),
                agent_id: request.agent_id.clone(),
                parent_agent_id: None,
                client_class: request.client.as_str(),
                request_class: None,
                canonical_model: request.canonical_model.clone(),
                lane: LaneFacts {
                    speed: String::new(),
                    thinking: String::new(),
                    effort: String::new(),
                },
                flags: RequestFlags {
                    fast: false,
                    count: false,
                    probe: false,
                    portable: false,
                    marker: false,
                    context_compacted: false,
                },
                chain: ChainFacts {
                    static_boundaries: 0,
                    boundaries: 0,
                    static_hash: false,
                },
                attempted: Vec::new(),
                holds: Vec::new(),
                registry_revision: snapshot.revision,
            },
            pin: None,
            pin_reason: Some("not_looked_up"),
            settings: key.zip(pool).map(|(_, pool)| settings_of(pool)),
            candidates: Vec::new(),
            walk: vec![
                WalkStep {
                    step: "admission",
                    verdict: "refused",
                    facts: vec![Fact::text("reason", refusal.reason.as_str())],
                },
                WalkStep {
                    step: "selection",
                    verdict: "not_run",
                    facts: Vec::new(),
                },
            ],
            outcome: Outcome {
                kind: "refused",
                account_id: None,
                from_account_id: None,
                upstream_model: None,
                tier: None,
                strategy,
                rule: None,
                rung: None,
                in_overage: false,
                pin_written: None,
                pin_events: Vec::new(),
                hold: None,
                exhaustion: None,
                refusal: Some(refusal.clone()),
                cohort_hold: None,
                shed_marks: None,
            },
        };
        let fact = if refusal.kind == "client" {
            SummaryFact {
                key: "client",
                value: FactValue::Text(
                    request
                        .client_version
                        .clone()
                        .unwrap_or_else(|| "unknown".into()),
                ),
                unit: Some("text"),
                entity: None,
            }
        } else {
            SummaryFact {
                key: "key",
                value: FactValue::Text(request.key_id.clone()),
                unit: None,
                entity: Some(FactEntity {
                    kind: "key",
                    id: request.key_id.clone(),
                }),
            }
        };
        let summary = DecisionSummary {
            kind: "refused",
            probe: false,
            by: By {
                kind: "refusal",
                value: refusal.kind.to_string(),
                tier: None,
            },
            fact: vec![fact],
            rule: None,
            strategy,
            tier: None,
            rung: None,
            score: None,
            runner_up: None,
            skipped: 0,
            warmth: None,
            hold: None,
            exhaustion: None,
            refusal: Some(refusal.clone()),
            fast: false,
        };
        (record, summary)
    }
}

/// The settings block as the pool reads when the decision ran.
pub(crate) fn settings_of(pool: &PoolRow) -> Settings {
    Settings {
        strategy: pool.strategy.as_str(),
        prefer_subscription: pool.prefer_subscription,
        overage_policy: pool.overage_policy.as_str(),
        affinity_ttl_seconds: pool.affinity_ttl_seconds,
        affinity_mode: pool.affinity.mode.as_str(),
        session_placement: pool.affinity.session_placement.as_str(),
        min_shared_prefix_tokens: pool.affinity.min_shared_prefix_tokens,
        subagent_placement: pool.affinity.subagent_placement.as_str(),
        select_below_utilization: pool.select_below_utilization,
        migrate_above_utilization: pool.migrate_above_utilization,
        migrate_when_spent: pool.shedding.migrate_when_spent,
        overage_move_grace_minutes: pool.failover.overage_move_grace_minutes,
        wind_down_minutes: pool.failover.wind_down_minutes,
        shed_share: pool.shedding.shed_share,
        burn_window_seconds: pool.shedding.burn_window_seconds,
        source_dwell_seconds: pool.shedding.source_dwell_seconds,
        warm_sessions: pool.shedding.warm_sessions.as_str(),
        in_flight_cap: pool.failover.in_flight_cap,
        retry_in_place_seconds: pool.failover.retry_in_place_seconds,
        pace_floor: pool.failover.pace_floor,
        migrate_band: MIGRATE_BAND,
        pool_updated_at: pool.updated_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(hash: &str, depth: u32, refused_by: Option<&'static str>) -> PrefixEntry {
        PrefixEntry {
            hash: hash.into(),
            depth,
            prefix_tokens: 1_200,
            written_at: 1_000,
            ttl_ms: 300_000,
            live: true,
            refused_by,
        }
    }

    fn renewal() -> DecisionRecord {
        let static_entry = entry(&"a".repeat(64), 1, None);
        DecisionRecord {
            version: RECORD_VERSION,
            request: RequestFacts {
                request_id: "r-1".into(),
                attempt: 1,
                seq: 1,
                at: 2_000,
                key_id: "k".into(),
                key_generation: 1,
                pool_id: "p".into(),
                agent_id: "main".into(),
                parent_agent_id: None,
                client_class: "claude_code",
                request_class: Some("main".into()),
                canonical_model: "claude-sonnet-5".into(),
                lane: LaneFacts {
                    speed: "standard".into(),
                    thinking: "none".into(),
                    effort: "-".into(),
                },
                flags: RequestFlags {
                    fast: false,
                    count: false,
                    probe: false,
                    portable: true,
                    marker: true,
                    context_compacted: false,
                },
                chain: ChainFacts {
                    static_boundaries: 1,
                    boundaries: 2,
                    static_hash: true,
                },
                attempted: Vec::new(),
                holds: Vec::new(),
                registry_revision: 7,
            },
            pin: Some(PinFacts {
                found: "live",
                account_id: Some("a".into()),
                epoch: Some(3),
                incarnation: Some(1),
                created_at: Some(500),
                last_used_at: Some(1_500),
                last_refresh_at: Some(1_500),
                idle_ms: Some(500),
                refresh_age_ms: Some(500),
                lane: Some("claude-sonnet-5|standard|none|-".into()),
                shed: None,
                marked: false,
                warmth: Some("refresh"),
                warmth_cause: Some("class_refreshes"),
                released: Vec::new(),
            }),
            pin_reason: None,
            settings: Some(Settings {
                strategy: "priority",
                prefer_subscription: true,
                overage_policy: "allow",
                affinity_ttl_seconds: 3_600,
                affinity_mode: "when_cached",
                session_placement: "warm_prefix",
                min_shared_prefix_tokens: 1_024,
                subagent_placement: "with_main",
                select_below_utilization: None,
                migrate_above_utilization: Some(0.9),
                migrate_when_spent: true,
                overage_move_grace_minutes: 30,
                wind_down_minutes: 60,
                shed_share: 0.25,
                burn_window_seconds: 600,
                source_dwell_seconds: 900,
                warm_sessions: "stop_after_cold",
                in_flight_cap: 8,
                retry_in_place_seconds: 10,
                pace_floor: 0.0,
                migrate_band: 0.05,
                pool_updated_at: 100,
            }),
            candidates: vec![CandidateFacts {
                account_id: "a".into(),
                incarnation: 1,
                kind: "oauth",
                plan_tier: Some("max_5x".into()),
                enabled: true,
                state: "ready",
                priority: 0,
                tier: (0, 0, 0),
                in_overage: false,
                overage: None,
                in_flight: 1,
                in_flight_cap: 8,
                windows: BTreeMap::from([(
                    "five_hour".to_string(),
                    WindowFacts {
                        utilization: 0.4,
                        reset: Some(18_000_000),
                        source: "head",
                        observed_at: 1_900,
                        age_ms: 100,
                        ahead: Some(-0.1),
                    },
                )]),
                allowance_known: true,
                pace: None,
                budget: Vec::new(),
                cooldowns: Vec::new(),
                credits: None,
                term: None,
                prefix: PrefixLookup {
                    content: None,
                    content_match: None,
                    static_: Some(static_entry.clone()),
                    static_match: Some(static_entry),
                    with_main: false,
                },
                obstacle: None,
                partition: None,
                rank: None,
                score: None,
                verdict: "renewed",
            }],
            walk: vec![WalkStep {
                step: "pin",
                verdict: "match",
                facts: vec![Fact {
                    key: "found",
                    value: FactValue::Text("live".into()),
                    account_id: Some("a".into()),
                    unit: None,
                }],
            }],
            outcome: Outcome {
                kind: "renewed",
                account_id: Some("a".into()),
                from_account_id: Some("a".into()),
                upstream_model: Some("claude-sonnet-5".into()),
                tier: None,
                strategy: "priority",
                rule: None,
                rung: None,
                in_overage: false,
                pin_written: Some(PinWritten {
                    action: "renewed",
                    epoch: 3,
                    lane: "claude-sonnet-5|standard|none|-".into(),
                    refresh_stamped: true,
                    hashes_stamped: true,
                }),
                pin_events: Vec::new(),
                hold: None,
                exhaustion: None,
                refusal: None,
                cohort_hold: None,
                shed_marks: None,
            },
        }
    }

    fn keys(value: &serde_json::Value) -> Vec<&str> {
        value
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect()
    }

    #[test]
    fn a_record_serializes_its_blocks_in_the_walk_s_order() {
        let value = serde_json::to_value(renewal()).expect("the record serializes");
        assert_eq!(
            keys(&value),
            [
                "version",
                "request",
                "pin",
                "pin_reason",
                "settings",
                "candidates",
                "walk",
                "outcome"
            ],
            "the blocks follow the walk: what came in, the pin, the settings, the members, the steps, the answer"
        );
        assert_eq!(value["version"], 1);
        let prefix = &value["candidates"][0]["prefix"];
        assert_eq!(
            keys(prefix),
            [
                "content",
                "content_match",
                "static",
                "static_match",
                "with_main"
            ]
        );
        assert_eq!(prefix["static"]["hash"], "a".repeat(64));
        assert_eq!(prefix["static"]["refused_by"], serde_json::Value::Null);
        assert_eq!(value["walk"][0]["facts"][0]["value"], "live");
        assert_eq!(value["pin_reason"], serde_json::Value::Null);
    }
}
