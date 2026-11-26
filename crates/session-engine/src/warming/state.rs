//! The cache warming's part of the engine's locked state, what one of its calls leaves to the engine above, and the
//! calls that make and drop an agent's record.

use std::collections::{BTreeMap, HashMap};

use phoebis_relay::{CacheTtl, CacheTtlPolicy};
use phoebis_routing::RegistrySnapshot;

use crate::sessions::state::{SessionKey, SessionsState, AGENT_HELD};
use crate::warming::agent::{warming_mut, warming_ref, AgentWarming};
use crate::warming::causes::Reason;
use crate::warming::config::{KeepaliveTiming, FIVE_MINUTES_MS};
use crate::warming::idle::IdleReturns;
use crate::warming::observation::{ChainMark, PriorCoverage, Settlement, Skipped};

/// The cache warming's state under the engine's lock.
pub(crate) struct WarmingState {
    /// The warming's record of each agent that the session engine's state holds, by the session's key and the agent's
    /// id.
    pub agents: BTreeMap<SessionKey, BTreeMap<String, AgentWarming>>,
    /// The engine's fixed bound on `retained`, held here so the warming reads it where it applies it.
    pub max_retained_bytes: usize,
    /// Request id to the agent following it.
    pub pending: HashMap<String, (SessionKey, String)>,
    /// A compaction's request id to the agent it is open on, read before `pending` by every request event, so the
    /// observation's paths never see a compaction.
    pub compactions: HashMap<String, (SessionKey, String)>,
    /// The marks by request id, inserted before the arrival's refusals and taken at the completion before the
    /// `pending` lookup, a mark older than an hour dropped at the next insert. Never saved.
    pub contradiction_marks: HashMap<String, ChainMark>,
    /// The instant of the last contradiction line per client version, so one prints per version per hour.
    pub chain_warned: HashMap<String, i64>,
    pub reasons: BTreeMap<Reason, u64>,
    /// Every body in memory, a recipe an arrival set aside beside the current ones, since `max_retained_bytes` bounds
    /// memory.
    pub retained: usize,
    pub running: usize,
    pub settlements: Vec<Settlement>,
    /// The settlements the load and the lapse sweep queued, held until a save has dropped their probes from the
    /// agents: a failed save then sends nothing to the ledger, and a restart before a landed save settles each probe
    /// once rather than twice.
    pub(in crate::warming) settlements_after_save: Vec<Settlement>,
    /// The probes a gate withheld this locked section, with the keys their skip rows file under, read while the
    /// recipe still held its speed.
    pub skipped: Vec<Skipped>,
    /// Bus gaps seen by the observer. Each one discarded every observation that was still pending.
    pub bus_gaps: u64,
    /// The last gap's instant, so a reader can tell a recent gap from one long past.
    pub last_bus_gap_at: Option<i64>,
    /// The engine's timing, copied onto the warming's record of every agent it creates.
    pub timing: KeepaliveTiming,
    /// The idle-age counters by pool id and UTC day. A day older than the timing's `idle_return_retention_days` leaves
    /// at the day roll. Nothing prunes by pool: a pool that leaves keeps its days until they age out, since a range the
    /// overview reads may still cover them.
    pub idle_returns: BTreeMap<String, BTreeMap<i64, IdleReturns>>,
    /// The UTC day the idle-age counters last counted under, `i64::MIN` until the first count, which rolls.
    pub idle_returns_day: i64,
    /// The cutoff of the days the last roll dropped, for the next save to purge from the registry, cleared once a save
    /// that carried it lands.
    pub purge_counters_before: Option<i64>,
}

impl WarmingState {
    pub(crate) fn new(timing: KeepaliveTiming, max_retained_bytes: usize) -> WarmingState {
        WarmingState {
            agents: BTreeMap::new(),
            max_retained_bytes,
            pending: HashMap::new(),
            compactions: HashMap::new(),
            contradiction_marks: HashMap::new(),
            chain_warned: HashMap::new(),
            reasons: BTreeMap::new(),
            retained: 0,
            running: 0,
            settlements: Vec::new(),
            settlements_after_save: Vec::new(),
            skipped: Vec::new(),
            bus_gaps: 0,
            last_bus_gap_at: None,
            timing,
            idle_returns: BTreeMap::new(),
            idle_returns_day: i64::MIN,
            purge_counters_before: None,
        }
    }
}

/// What one call of the warming leaves to the engine above, in the order of the call's statements.
#[must_use]
#[derive(Default)]
pub(crate) struct Changes(pub Vec<Change>);

pub(crate) enum Change {
    /// An observation of the agent ended, so the session engine moves the agent's request generation.
    Ended { key: SessionKey, agent_id: String },
    /// The agent's row changed and goes into the next save.
    Agent { key: SessionKey, agent_id: String },
    /// The pool's idle counters changed and go into the next save.
    Pool(String),
}

impl Changes {
    pub(crate) fn ended(&mut self, key: &SessionKey, agent_id: &str) {
        self.0.push(Change::Ended {
            key: key.clone(),
            agent_id: agent_id.to_string(),
        });
    }

    pub(crate) fn agent_row(&mut self, key: &SessionKey, agent_id: &str) {
        self.0.push(Change::Agent {
            key: key.clone(),
            agent_id: agent_id.to_string(),
        });
    }

    pub(crate) fn pool_row(&mut self, pool_id: &str) {
        self.0.push(Change::Pool(pool_id.to_string()));
    }

    /// The list of a call this one made, at the place of that call.
    pub(crate) fn append(&mut self, changes: Changes) {
        self.0.extend(changes.0);
    }
}

/// What the arrival's guard leaves to the arming: what the agent's previous recipe had proved at the admission.
pub(crate) struct Arriving {
    pub prior: Option<PriorCoverage>,
}

impl WarmingState {
    /// Makes the warming's record of an agent the session engine just made, which waits for its first request.
    pub(crate) fn admit(
        &mut self,
        sessions: &SessionsState,
        key: &SessionKey,
        agent_id: &str,
        now: i64,
    ) -> Changes {
        let warming = AgentWarming::new(now, self.timing);
        self.agents
            .entry(key.clone())
            .or_default()
            .insert(agent_id.to_string(), warming);
        self.set_reason(sessions, key, agent_id, Reason::AwaitingRequest)
    }

    /// Sets main's cause back to waiting for a request, behind a revival.
    pub(crate) fn await_request(
        &mut self,
        sessions: &SessionsState,
        key: &SessionKey,
        agent_id: &str,
    ) -> Changes {
        self.set_reason(sessions, key, agent_id, Reason::AwaitingRequest)
    }

    /// Discards the recipe of a released record that an `agent_start` takes back, under `awaiting_request` and with
    /// no label, and keeps the label of the stop that released it.
    pub(crate) fn restart(
        &mut self,
        sessions: &SessionsState,
        key: &SessionKey,
        agent_id: &str,
    ) -> Changes {
        let released = warming_ref(&self.agents, key, agent_id)
            .expect(AGENT_HELD)
            .last_stop;
        let changes = self.discard(sessions, key, agent_id, Reason::AwaitingRequest, None);
        // The release's label stays until the late result lands, so a probe the release orphaned
        // settles under it rather than under nothing.
        warming_mut(&mut self.agents, key, agent_id)
            .expect(AGENT_HELD)
            .last_stop = released;
        changes
    }

    pub(crate) fn drop_record(&mut self, key: &SessionKey, agent_id: &str) {
        if let Some(warming_agents) = self.agents.get_mut(key) {
            warming_agents.remove(agent_id);
        }
    }

    pub(crate) fn drop_records(&mut self, key: &SessionKey) {
        self.agents.remove(key);
    }

    /// The warming's record of an agent the session engine holds. Both records of an agent are made and dropped in the
    /// same hold, so a held agent without the warming's record panics.
    pub(crate) fn held(&self, key: &SessionKey, agent_id: &str) -> &AgentWarming {
        warming_ref(&self.agents, key, agent_id).expect(AGENT_HELD)
    }

    pub(crate) fn in_flight(&self, key: &SessionKey, agent_id: &str) -> bool {
        warming_ref(&self.agents, key, agent_id)
            .expect(AGENT_HELD)
            .in_flight
    }

    /// When a released record leaves: `now` plus the longest TTL of the cache TTL policy of the agent's pool in the
    /// router's snapshot.
    pub(crate) fn leave_after(
        &self,
        key: &SessionKey,
        agent_id: &str,
        snapshot: &RegistrySnapshot,
        now: i64,
    ) -> i64 {
        let warming = warming_ref(&self.agents, key, agent_id).expect(AGENT_HELD);
        // A pool the snapshot no longer holds, or none for a record that never sent a request, reads the default
        // policy, whose plan set keeps the hour.
        let policy = snapshot
            .pool(&warming.pool_id)
            .map(|pool| CacheTtlPolicy::from_json(&pool.cache_ttl_json))
            .unwrap_or_default();
        now + longest_cache_ttl_ms(&policy)
    }

    /// Drops every record, every request followed and every byte retained, behind the shutdown's last save.
    pub(crate) fn clear(&mut self) {
        self.agents.clear();
        self.pending.clear();
        self.compactions.clear();
        self.retained = 0;
    }
}

/// The longest TTL the policy names in milliseconds, over both rule sets' `main`, `subagent` and model overrides.
pub(crate) fn longest_cache_ttl_ms(policy: &CacheTtlPolicy) -> i64 {
    // Whatever `enabled` says: the rules are what a body's markers are completed against, so a resumed agent's next
    // request can land under the longest of them.
    [&policy.plan, &policy.metered]
        .into_iter()
        .flat_map(|rules| {
            [rules.main, rules.subagent]
                .into_iter()
                .chain(rules.models.iter().flat_map(|m| [m.main, m.subagent]))
        })
        .map(CacheTtl::ms)
        .fold(FIVE_MINUTES_MS, i64::max)
}

#[cfg(test)]
mod tests {
    use crate::warming::config::ONE_HOUR_MS;

    use super::*;

    #[test]
    fn the_longest_cache_ttl_reads_both_sets_and_their_model_overrides_while_the_policy_is_off() {
        let longest =
            |raw: serde_json::Value| longest_cache_ttl_ms(&CacheTtlPolicy::parse(&raw).unwrap());
        let five = serde_json::json!({"main": "5m", "subagent": "5m"});
        let with_model = |main: &str, subagent: &str| {
            serde_json::json!({
                "main": "5m",
                "models": [{"main": main, "model": "claude-opus-4-1", "subagent": subagent}],
                "subagent": "5m",
            })
        };
        assert_eq!(longest(serde_json::json!({})), ONE_HOUR_MS);
        assert_eq!(
            longest(serde_json::json!({"metered": five, "plan": five})),
            FIVE_MINUTES_MS
        );
        for (name, policy) in [
            (
                "a metered subagent",
                serde_json::json!({"metered": {"main": "5m", "subagent": "1h"}, "plan": five}),
            ),
            (
                "a plan main",
                serde_json::json!({"metered": five, "plan": {"main": "1h", "subagent": "5m"}}),
            ),
            (
                "a metered model's main",
                serde_json::json!({"metered": with_model("1h", "5m"), "plan": five}),
            ),
            (
                "a plan model's subagent",
                serde_json::json!({"metered": five, "plan": with_model("5m", "1h")}),
            ),
        ] {
            assert_eq!(longest(policy), ONE_HOUR_MS, "{name}");
        }
    }
}
