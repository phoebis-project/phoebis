//! The session engine's part of the engine's locked state: each session's record with its agents' records, and the
//! calls that check and write them.

use std::collections::BTreeMap;

use phoebis_routing::RegistrySnapshot;

use crate::sessions::config::LEASE_MS;
use crate::sessions::view::CompanionState;

/// The main conversation's agent, which the idle limits apply to.
pub use phoebis_routing::MAIN_AGENT;

pub type SessionKey = (String, String);

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Busy,
    Idle,
    Parked,
    Released,
    Unknown,
}

impl AgentState {
    pub fn parse(s: &str) -> Option<AgentState> {
        Some(match s {
            "busy" => AgentState::Busy,
            "idle" => AgentState::Idle,
            "parked" => AgentState::Parked,
            "released" => AgentState::Released,
            "unknown" => AgentState::Unknown,
            _ => return None,
        })
    }
}

/// What a parked agent waits on: a background task of its own, a scheduled wakeup, or both.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParkKind {
    Background,
    Both,
    Scheduled,
}

impl ParkKind {
    pub fn parse(s: &str) -> Option<ParkKind> {
        Some(match s {
            "background" => ParkKind::Background,
            "both" => ParkKind::Both,
            "scheduled" => ParkKind::Scheduled,
            _ => return None,
        })
    }

    pub(crate) fn background(self) -> bool {
        matches!(self, ParkKind::Background | ParkKind::Both)
    }

    pub(crate) fn scheduled(self) -> bool {
        matches!(self, ParkKind::Scheduled | ParkKind::Both)
    }

    fn of_halves(background: bool, scheduled: bool) -> Option<ParkKind> {
        match (background, scheduled) {
            (true, true) => Some(ParkKind::Both),
            (true, false) => Some(ParkKind::Background),
            (false, true) => Some(ParkKind::Scheduled),
            (false, false) => None,
        }
    }
}

/// What a parked agent waits on and until when, held as one value so that a park with no kind cannot be represented
/// and no reader has to compensate for a deadline or a wake that a pass left behind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Park {
    #[serde(skip_serializing_if = "Option::is_none")]
    deadline: Option<i64>,
    #[serde(rename = "park_kind")]
    kind: ParkKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    wake_at: Option<i64>,
}

impl Park {
    /// The park of `kind`, keeping the deadline only on a background kind and the wake only on a scheduled one, so no
    /// park holds a part its kind does not take.
    pub(crate) fn new(kind: ParkKind, deadline: Option<i64>, wake_at: Option<i64>) -> Park {
        Park {
            deadline: deadline.filter(|_| kind.background()),
            kind,
            wake_at: wake_at.filter(|_| kind.scheduled()),
        }
    }

    pub(crate) fn kind(self) -> ParkKind {
        self.kind
    }

    pub(crate) fn deadline(self) -> Option<i64> {
        self.deadline
    }

    pub(crate) fn wake_at(self) -> Option<i64> {
        self.wake_at
    }

    /// The park left at `now` once each part whose return came without a request drops out with its half: a deadline
    /// the harness stopped the command at, or a wake that passed. `None` when no half is left, which is when it ended.
    pub(crate) fn passed(self, now: i64) -> Option<Park> {
        let deadline_passed = self.deadline.is_some_and(|deadline| deadline <= now);
        let wake_passed = self.wake_at.is_some_and(|wake| wake <= now);
        ParkKind::of_halves(
            self.kind.background() && !deadline_passed,
            self.kind.scheduled() && !wake_passed,
        )
        .map(|kind| {
            Park::new(
                kind,
                self.deadline.filter(|_| !deadline_passed),
                self.wake_at.filter(|_| !wake_passed),
            )
        })
    }

    /// For the readers whose view or saved row keeps the deadline, the kind and the wake as members of their own.
    pub(crate) fn parts(park: Option<Park>) -> (Option<i64>, Option<ParkKind>, Option<i64>) {
        park.map_or((None, None, None), |park| {
            (park.deadline, Some(park.kind), park.wake_at)
        })
    }
}

/// A park as the engine holds it: the reported value with the start the parked ceiling counts from, which no report
/// or answer carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HeldPark {
    pub began: i64,
    pub park: Park,
}

impl HeldPark {
    /// How much of the park passed at `now`, with the park it leaves.
    fn passed_at(self, now: i64) -> (ParkPassed, Option<Park>) {
        let left = self.park.passed(now);
        let passed = if left == Some(self.park) {
            ParkPassed::No
        } else if left.is_some() {
            ParkPassed::Part
        } else {
            ParkPassed::Whole
        };
        (passed, left)
    }
}

/// One companion registration, or one session a request revealed before any registration.
pub(crate) struct Session {
    /// The generation of the key that registered the session, so a key id created again reaches none of the
    /// previous holder's sessions: the companion identity is the pair, never the id alone.
    pub key_generation: i64,
    pub instance: String,
    pub sequence: u64,
    /// When the companion's lease runs out. Past it the session is unattended: its recipes stay and are probed
    /// under the idle limits from `last_real_at`.
    pub lease: i64,
    pub attended: bool,
    /// A `session_end` or a replacement: nothing under this instance is live any more.
    pub ended: bool,
    pub last_report_at: i64,
    /// The session's last real request, one refused its slot included, which the expiry and a silent session's idle
    /// ceiling count from. A refused session keeps no recipe, so the ceiling never reads a refused request's stamp.
    pub last_real_at: Option<i64>,
    /// Whether the session took its slots of `max_sessions` and `max_agents`, which the first request the caps admit
    /// sets. From then on its agents' records hold slots of `max_agents` until each leaves, a record its end released
    /// included. A request refused the slot leaves it false and still writes `last_real_at`, so the session's next
    /// request reads both caps again while the expiry counts from the refused request.
    pub slotted: bool,
    /// A session the load restored from a save that held it attended keeps the attended reading of the ceilings until
    /// this instant or until the mod registers it again, whichever is first: the mod re-adopts a restored session
    /// within its lease, and a parked recipe must not be discarded as idle before it can.
    pub restored_until: Option<i64>,
    /// Set at the first request that named another instance than the registration while the holder's lease was live,
    /// so the warning goes out once per session rather than once per request.
    pub instance_mismatch_warned: bool,
    pub agents: BTreeMap<String, Agent>,
    /// What the session's last registration said of its own cache warming option.
    pub cache_warming: bool,
}

impl Session {
    /// Whether the ceilings read the session as attended at `now`: as the mod's lease says, or as the save said for
    /// one lease length after a load.
    pub(crate) fn attended_at(&self, now: i64) -> bool {
        self.attended || self.restored_until.is_some_and(|until| now < until)
    }

    /// Holds a slot of `max_sessions`: it took its slots and has not ended.
    pub(crate) fn holds_slot(&self) -> bool {
        !self.ended && self.slotted
    }

    /// The slots of `max_agents` the session's agents hold, none before the session takes its slots.
    pub(crate) fn agent_slots(&self) -> usize {
        if self.slotted {
            self.agents.len()
        } else {
            0
        }
    }
}

pub(crate) struct Agent {
    pub id: String,
    /// The session's key, repeated here so a gate over one agent needs no session lookup.
    pub key_id: String,
    /// Which record this is. A replacement registration builds a new record under the same ids, and a probe
    /// worker that started on the old one must not touch the new one's bookkeeping.
    pub incarnation: u64,
    /// Written through `set_state`, which ends the park on any move out of `Parked`.
    pub state: AgentState,
    /// The park the last `parked` report described, cleared on every move out of `Parked`, so a reader never finds a
    /// park on an agent that is not parked. Its start is kept across a repeated `parked` of the same kind.
    pub park: Option<HeldPark>,
    /// When a deadline or a wake passed without a request: the idle window restarts here. No report clears it,
    /// since the window starts at the later of it and `last_activity` and an older end never outranks a newer
    /// request.
    pub park_ended_at: Option<i64>,
    pub generation: u64,
    /// When a released record leaves the engine: the release plus the longest TTL of the pool's cache TTL policy,
    /// `None` on a record that is not released.
    pub leaves_at: Option<i64>,
    pub last_activity: i64,
    /// Main's last presence report: when the person was last seen at the terminal.
    pub last_presence_at: Option<i64>,
}

impl Agent {
    pub(crate) fn new(id: &str, key_id: &str, incarnation: u64, now: i64) -> Agent {
        Agent {
            id: id.into(),
            key_id: key_id.into(),
            incarnation,
            state: AgentState::Busy,
            park: None,
            park_ended_at: None,
            generation: 0,
            leaves_at: None,
            last_activity: now,
            last_presence_at: None,
        }
    }

    /// Where main's idle window starts now: the later of its last activity and a park's end without a request.
    pub(crate) fn idle_window_start(&self) -> i64 {
        self.park_ended_at
            .map_or(self.last_activity, |ended| ended.max(self.last_activity))
    }

    /// Every state write. A move to any state but `Parked` ends the park, so a stale park never outlives it.
    pub(crate) fn set_state(&mut self, state: AgentState) {
        self.state = state;
        if state != AgentState::Parked {
            self.park = None;
        }
    }

    pub(crate) fn resume(&mut self, now: i64) {
        self.set_state(AgentState::Busy);
        self.leaves_at = None;
        self.last_activity = now;
    }

    /// Whether the record is released and its leave passed at `now`, so it leaves the engine.
    pub(crate) fn leave_passed(&self, now: i64) -> bool {
        self.state == AgentState::Released && self.leaves_at.is_some_and(|at| now >= at)
    }
}

/// What a walk whose caller just read the agent under the same lock expects of it.
pub(crate) const AGENT_HELD: &str =
    "an agent the caller read under the lock is still in its session";

/// The agent `agent_id` of the session `key`, borrowing the session map alone, so a caller may write another member of
/// the session engine's state while it holds the agent.
pub(crate) fn agent_mut<'a>(
    sessions: &'a mut BTreeMap<SessionKey, Session>,
    key: &SessionKey,
    agent_id: &str,
) -> Option<&'a mut Agent> {
    sessions.get_mut(key)?.agents.get_mut(agent_id)
}

/// `agent_mut`'s shared twin.
pub(crate) fn agent_ref<'a>(
    sessions: &'a BTreeMap<SessionKey, Session>,
    key: &SessionKey,
    agent_id: &str,
) -> Option<&'a Agent> {
    sessions.get(key)?.agents.get(agent_id)
}

/// What an arrival's request does to its session's slots: the session held them already, took them now, or was refused
/// them over the session cap or over the agent cap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Slot {
    Held,
    Taken,
    OverSessions,
    OverAgents,
}

/// What an arrival's request does to the agent's record: an `unknown` record stays as it is, and any other turns busy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Arrived {
    Busy,
    Unknown,
}

/// Whether a park's deadline or wake passed at a tick: none of it, a part with the rest left, or the whole park.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ParkPassed {
    No,
    Part,
    Whole,
}

/// The session engine's state under the engine's lock: each session's record with its agents' records, and the bounds
/// that a registration, an arrival and the sweep read.
pub(crate) struct SessionsState {
    pub entries: BTreeMap<SessionKey, Session>,
    pub max_agents: usize,
    pub max_sessions: usize,
    pub next_incarnation: u64,
    pub session_expiry_ms: i64,
}

/// Whether the registry still holds the key id under the generation that created it. The sessions of a key and the
/// facts they filed leave by this one test, so they leave together.
pub(crate) fn key_stands(snapshot: &RegistrySnapshot, key_id: &str, key_generation: i64) -> bool {
    snapshot.key(key_id).map(|k| k.created_at) == Some(key_generation)
}

impl SessionsState {
    pub(crate) fn new(
        max_agents: usize,
        max_sessions: usize,
        session_expiry_ms: i64,
    ) -> SessionsState {
        SessionsState {
            entries: BTreeMap::new(),
            max_agents,
            max_sessions,
            next_incarnation: 0,
            session_expiry_ms,
        }
    }

    /// Each released record whose leave passed at `now`.
    pub(crate) fn released_due(&self, now: i64) -> Vec<(SessionKey, String)> {
        self.entries
            .iter()
            .flat_map(|(key, session)| {
                session
                    .agents
                    .iter()
                    .filter(move |(_, agent)| agent.leave_passed(now))
                    .map(move |(agent_id, _)| (key.clone(), agent_id.clone()))
            })
            .collect()
    }

    pub(crate) fn drop_agent(&mut self, key: &SessionKey, agent_id: &str) {
        if let Some(session) = self.entries.get_mut(key) {
            session.agents.remove(agent_id);
        }
    }

    /// Turns each attended session whose lease ran out at `now` silent, and answers those sessions.
    pub(crate) fn lapse_leases(&mut self, now: i64) -> Vec<SessionKey> {
        self.entries
            .iter_mut()
            .filter(|(_, s)| s.companion() == CompanionState::Reporting && now >= s.lease)
            .map(|(key, s)| {
                s.attended = false;
                key.clone()
            })
            .collect()
    }

    /// The sessions past the session expiry at `now`, counted from the later of their last real request and their last
    /// report.
    pub(crate) fn expired(&self, now: i64) -> Vec<SessionKey> {
        self.entries
            .iter()
            .filter(|(_, s)| {
                now - s.last_real_at.unwrap_or(0).max(s.last_report_at) >= self.session_expiry_ms
            })
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// The sessions whose key the registry no longer holds under the generation that registered them.
    pub(crate) fn sessions_of_dead_keys(&self, snapshot: &RegistrySnapshot) -> Vec<SessionKey> {
        self.entries
            .iter()
            .filter(|(key, s)| !key_stands(snapshot, &key.0, s.key_generation))
            .map(|(k, _)| k.clone())
            .collect()
    }

    pub(crate) fn remove_session(&mut self, key: &SessionKey) {
        self.entries.remove(key);
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }

    /// Moves the agent to `state`. A move from parked to idle stamps the park's end, where the idle window restarts.
    pub(crate) fn set_state(
        &mut self,
        key: &SessionKey,
        agent_id: &str,
        state: AgentState,
        now: i64,
    ) {
        let agent = agent_mut(&mut self.entries, key, agent_id).expect(AGENT_HELD);
        let was = agent.state;
        if was == AgentState::Parked && state == AgentState::Idle {
            agent.park_ended_at = Some(now);
        }
        agent.set_state(state);
    }

    pub(crate) fn end_session(&mut self, key: &SessionKey, sequence: u64, now: i64) {
        let session = self.entries.get_mut(key).unwrap();
        session.ended = true;
        session.sequence = sequence;
        session.last_report_at = now;
    }

    /// A presence stamps the person's last sight at the terminal and the idle clock alone.
    pub(crate) fn presence_seen(&mut self, key: &SessionKey, agent_id: &str, now: i64) {
        let agent = agent_mut(&mut self.entries, key, agent_id).expect(AGENT_HELD);
        agent.last_presence_at = Some(now);
        agent.last_activity = now;
    }

    /// A prompt stamps the idle clock and moves the agent to busy.
    pub(crate) fn prompted(&mut self, key: &SessionKey, agent_id: &str, now: i64) {
        let agent = agent_mut(&mut self.entries, key, agent_id).expect(AGENT_HELD);
        agent.last_activity = now;
        agent.set_state(AgentState::Busy);
    }

    pub(crate) fn park(&mut self, key: &SessionKey, agent_id: &str, park: Park, now: i64) {
        let agent = agent_mut(&mut self.entries, key, agent_id).expect(AGENT_HELD);
        // A repeated `parked` of the same kind moves the wake or the deadline and keeps the park's start, which the
        // parked ceiling counts from.
        let began = agent
            .park
            .filter(|held| agent.state == AgentState::Parked && held.park.kind() == park.kind())
            .map_or(now, |held| held.began);
        agent.set_state(AgentState::Parked);
        agent.park = Some(HeldPark { began, park });
    }

    /// The checks an arrival passes on its session and its record: the session stands under the request's key
    /// generation and has not ended, and the record stands and is not released. Writes nothing.
    pub(crate) fn arrival_check(
        &self,
        key: &SessionKey,
        key_generation: i64,
        agent_id: &str,
    ) -> bool {
        // The purge ends a session whose key id now belongs to a later holder, but only at the next tick: until
        // then this is the previous holder's session, and a request of another generation arms nothing on it.
        let Some(session) = self.held_by(key, key_generation) else {
            return false;
        };
        if session.ended {
            return false;
        }
        let Some(agent) = session.agents.get(agent_id) else {
            return false;
        };
        if agent.state == AgentState::Released {
            return false;
        }
        true
    }

    /// Whether the instance an arrival names is the session's, while the session reads attended. The session's first
    /// mismatch warns.
    pub(crate) fn instance_check(
        &mut self,
        key: &SessionKey,
        instance_id: Option<&str>,
        now: i64,
    ) -> bool {
        // The header is a hardening on top of the registration rule: while the holder's lease is live, a request that
        // names an instance has to name the session's. Foreign traffic proves nothing about this agent, so its status,
        // its recipe and its schedule are left as they were, and the session's first mismatch is logged. A restored
        // session's window reads as live, since the mod re-adopts the session within it. Past the lease the holder
        // reports nothing, and a process that reloaded under a new instance would otherwise arm nothing until the idle
        // limit ended the session.
        let session = self.entries.get_mut(key).unwrap();
        if instance_id.is_some_and(|sent| sent != session.instance) && session.attended_at(now) {
            if !session.instance_mismatch_warned {
                tracing::warn!(
                    session = %key.1,
                    "a request named another instance than the session's registration"
                );
                session.instance_mismatch_warned = true;
            }
            return false;
        }
        true
    }

    /// Stamps the session's last real request, and decides its slots at the first request the caps admit.
    pub(crate) fn arrival_slot(&mut self, key: &SessionKey, now: i64) -> Slot {
        // A request refused the slot below moves the session's request time too, which is saved, since the expiry
        // counts from it across a restart.
        let session = self.entries.get_mut(key).unwrap();
        session.last_real_at = Some(now);
        if session.slotted {
            return Slot::Held;
        }
        let joining = session.agents.len();
        // A session takes its slots at the first real request the caps admit, so both caps are read here at each of
        // its requests until then, the session cap first as a registration reads them, and the session's agents join
        // the agent count only where they all fit. A request over either cap is refused the slot as a registration
        // over it is: it keeps nothing and leaves `slotted` false, so the session's next request reads the caps again.
        if !self.session_fits() {
            return Slot::OverSessions;
        }
        if !self.agents_fit(joining, 0) {
            return Slot::OverAgents;
        }
        self.entries.get_mut(key).unwrap().slotted = true;
        Slot::Taken
    }

    /// Leaves an `unknown` record as it is, and moves any other to busy at `now`.
    pub(crate) fn arrival_busy(&mut self, key: &SessionKey, agent_id: &str, now: i64) -> Arrived {
        let agent = agent_mut(&mut self.entries, key, agent_id).expect(AGENT_HELD);
        if agent.state == AgentState::Unknown {
            return Arrived::Unknown;
        }
        agent.set_state(AgentState::Busy);
        agent.last_activity = now;
        Arrived::Busy
    }

    /// Whether a deadline or a wake of the agent's park passed at `now`.
    pub(crate) fn park_passed(&self, key: &SessionKey, agent_id: &str, now: i64) -> ParkPassed {
        let Some(agent) = agent_ref(&self.entries, key, agent_id) else {
            return ParkPassed::No;
        };
        agent
            .park
            .map_or(ParkPassed::No, |held| held.passed_at(now).0)
    }

    /// A deadline or a wake that passed ends that part of the park, and a park with nothing left ended without a
    /// request: the idle window restarts here and main reads idle. The mod reports the same end with `idle` when its
    /// heartbeat sees it first, and whichever comes first records it. Main's end is the engine's own move out of
    /// parked, which no report repeats.
    pub(crate) fn end_passed_park(&mut self, key: &SessionKey, agent_id: &str, now: i64) {
        let Some(agent) = agent_mut(&mut self.entries, key, agent_id) else {
            return;
        };
        let Some(held) = agent.park else {
            return;
        };
        let (passed, left) = held.passed_at(now);
        if passed == ParkPassed::No {
            return;
        }
        agent.park = left.map(|park| HeldPark {
            began: held.began,
            park,
        });
        if passed == ParkPassed::Part {
            return;
        }
        agent.park_ended_at = Some(now);
        if agent.id == MAIN_AGENT {
            agent.set_state(AgentState::Idle);
        }
    }

    /// Moves the agent's request generation for an observation that ended, so no late result about it lands.
    pub(crate) fn end_observation(&mut self, key: &SessionKey, agent_id: &str) {
        if let Some(agent) = agent_mut(&mut self.entries, key, agent_id) {
            agent.generation += 1;
        }
    }

    pub(crate) fn set_leave(&mut self, key: &SessionKey, agent_id: &str, leaves_at: i64) {
        agent_mut(&mut self.entries, key, agent_id)
            .expect(AGENT_HELD)
            .leaves_at = Some(leaves_at);
    }

    /// What every accepted report but a session's end and a registration of a new session writes on its session: the
    /// sequence, the lease where the report renews it, the attended reading and the report's instant.
    pub(crate) fn tail(&mut self, key: &SessionKey, sequence: u64, renews_lease: bool, now: i64) {
        let session = self.entries.get_mut(key).unwrap();
        session.sequence = sequence;
        if renews_lease {
            session.lease = now + LEASE_MS;
        }
        session.attended = true;
        session.last_report_at = now;
    }

    /// Whether a result about `agent` may still land: the session is not ended, the agent is registered, and
    /// its state admits traffic. An unattended session is live. The idle limits decide what happens to it.
    pub(crate) fn live(&self, key: &SessionKey, agent_id: &str) -> bool {
        self.entries.get(key).is_some_and(|s| {
            !s.ended
                && s.agents
                    .get(agent_id)
                    .is_some_and(|a| !matches!(a.state, AgentState::Released | AgentState::Unknown))
        })
    }

    /// The session under `key` that the key generation registered, `None` where none stands or a previous holder of the
    /// key id registered it.
    pub(crate) fn held_by(&self, key: &SessionKey, key_generation: i64) -> Option<&Session> {
        self.entries
            .get(key)
            .filter(|s| s.key_generation == key_generation)
    }

    /// The sessions `max_sessions` bounds, those that hold a slot.
    pub(crate) fn live_sessions(&self) -> usize {
        self.entries.values().filter(|s| s.holds_slot()).count()
    }

    /// The agents `max_agents` bounds: every record of a session that took its slots. A registered session's agents
    /// hold none until a request the caps admit.
    pub(crate) fn agent_count(&self) -> usize {
        self.entries.values().map(Session::agent_slots).sum()
    }

    /// Whether one more session that holds a slot fits under `max_sessions`.
    pub(crate) fn session_fits(&self) -> bool {
        self.live_sessions() < self.max_sessions
    }

    /// Whether the agent count fits under `max_agents` once `joining` records join it and `leaving` leave it.
    pub(crate) fn agents_fit(&self, joining: usize, leaving: usize) -> bool {
        self.agent_count() - leaving + joining <= self.max_agents
    }

    pub(crate) fn incarnation(&mut self) -> u64 {
        self.next_incarnation += 1;
        self.next_incarnation
    }
}
