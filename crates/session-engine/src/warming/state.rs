//! The cache warming's part of the engine's locked state.

use std::collections::{BTreeMap, HashMap};

use crate::engine::config::KeepaliveTiming;
use crate::engine::{ChainMark, IdleReturns, Reason, SessionKey, Settlement, Skipped};
use crate::warming::agent::AgentWarming;

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
    pub settlements_after_save: Vec<Settlement>,
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
