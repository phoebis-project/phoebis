//! The companion's lifecycle protocol: six events, monotonic sequences, per-agent generations, a lease, and the
//! snapshot the companion reconciles against. The contract is the Go engine's, plus `owner_id` on
//! `session_start` and the key scope on every session.

use std::collections::BTreeMap;

use phoebis_bus::SessionLifecycle;

use crate::engine::{
    Agent, AgentState, Engine, Inner, Reason, Session, SessionKey, Wasted, MAIN_AGENT,
};
use crate::recipe::valid_id;

/// Decoded key and value bytes of one event, with two per pair for the separators the wire had.
const MAX_EVENT_BYTES: usize = 16 << 10;

pub const EVENT_FIELDS: &[&str] = &[
    "agent_id",
    "event",
    "instance_id",
    "owner_id",
    "previous_instance_id",
    "request_generation",
    "sequence",
    "session_id",
    "state",
];

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Event {
    pub agent_id: Option<String>,
    pub event: String,
    pub instance_id: String,
    pub owner_id: Option<String>,
    pub previous_instance_id: Option<String>,
    pub request_generation: Option<u64>,
    pub sequence: u64,
    pub session_id: String,
    pub state: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Ack {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    pub instance_id: String,
    /// RFC 3339, because the companion parses it with `datetime.fromisoformat`.
    pub lease_expires: String,
    pub request_generation: u64,
    pub sequence: u64,
    pub session_id: String,
}

#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
#[error("{code}")]
pub struct EventError {
    pub code: &'static str,
    pub status: u16,
}

const fn err(code: &'static str, status: u16) -> EventError {
    EventError { code, status }
}

pub fn rfc3339(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms)
        .map(|t| t.to_string())
        .unwrap_or_default()
}

/// The raw query string, decoded here rather than by an extractor: the size bound is on decoded bytes, a
/// repeated key is a refusal rather than a last-wins, and the codes are the ones the companion branches on.
pub fn decode_query_event(query: &str) -> Result<Event, EventError> {
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    let mut total = 0usize;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        if !EVENT_FIELDS.contains(&key.as_ref()) || seen.contains_key(key.as_ref()) {
            return Err(err("invalid_event", 400));
        }
        total += key.len() + value.len() + 2;
        seen.insert(key.into_owned(), value.into_owned());
    }
    if total > MAX_EVENT_BYTES {
        return Err(err("event_too_large", 413));
    }
    let single = |key: &str| seen.get(key).cloned();
    let sequence = single("sequence")
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or(err("invalid_event", 400))?;
    let request_generation = match single("request_generation") {
        None => None,
        Some(raw) => Some(raw.parse::<u64>().map_err(|_| err("invalid_event", 400))?),
    };
    let event = Event {
        agent_id: single("agent_id"),
        event: single("event").unwrap_or_default(),
        instance_id: single("instance_id").unwrap_or_default(),
        owner_id: single("owner_id"),
        previous_instance_id: single("previous_instance_id"),
        request_generation,
        sequence,
        session_id: single("session_id").unwrap_or_default(),
        state: single("state"),
    };
    if event.owner_id.is_some() && event.event != "session_start" {
        return Err(err("invalid_event", 400));
    }
    validate(event)
}

fn validate(event: Event) -> Result<Event, EventError> {
    let optional_ok = |value: &Option<String>| value.as_deref().is_none_or(valid_id);
    if !valid_id(&event.session_id)
        || !valid_id(&event.instance_id)
        || event.sequence == 0
        || !optional_ok(&event.agent_id)
        || !optional_ok(&event.owner_id)
        || !optional_ok(&event.previous_instance_id)
    {
        return Err(err("invalid_identity", 400));
    }
    Ok(event)
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct SnapshotAgent {
    pub agent_id: String,
    pub request_generation: u64,
    pub state: AgentState,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct SnapshotSession {
    pub agents: Vec<SnapshotAgent>,
    pub ended: bool,
    pub instance_id: String,
    pub sequence: u64,
    pub session_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Snapshot {
    pub sessions: Vec<SnapshotSession>,
}

fn acknowledge(session: &Session, agent: Option<&Agent>) -> Ack {
    Ack {
        agent_id: agent.map(|a| a.id.clone()),
        instance_id: session.instance.clone(),
        lease_expires: rfc3339(session.lease),
        request_generation: agent.map(|a| a.generation).unwrap_or(0),
        sequence: session.sequence,
        session_id: session.session_id.clone(),
    }
}

impl Engine {
    /// `key_id` is the authenticated key, never a field of the event: a session is scoped to the key that
    /// reports it, and no query field can name another.
    pub fn apply_event(&self, key_id: &str, event: Event) -> Result<Ack, EventError> {
        let now = self.now();
        let expiry = self.parts.session_expiry.as_millis() as i64;
        let lease = self.parts.limits.lease_ms();
        let limits = self.parts.limits.clone();
        let acked = {
            let mut inner = self.lock();
            if inner.closed {
                return Err(err("quiesced", 503));
            }
            inner.expire(now, expiry);
            inner.apply(
                key_id,
                event,
                now,
                lease,
                limits.max_sessions,
                limits.max_agents,
            )
        };
        self.flush();
        acked
    }

    pub fn snapshot(&self, key_id: &str) -> Snapshot {
        let now = self.now();
        let expiry = self.parts.session_expiry.as_millis() as i64;
        let snapshot = {
            let mut inner = self.lock();
            inner.expire(now, expiry);
            Snapshot {
                sessions: inner
                    .sessions
                    .values()
                    .filter(|s| s.key_id == key_id)
                    .map(|s| SnapshotSession {
                        agents: s
                            .agents
                            .values()
                            .map(|a| SnapshotAgent {
                                agent_id: a.id.clone(),
                                request_generation: a.generation,
                                state: a.state,
                            })
                            .collect(),
                        ended: s.ended || !s.attended,
                        instance_id: s.instance.clone(),
                        sequence: s.sequence,
                        session_id: s.session_id.clone(),
                    })
                    .collect(),
            }
        };
        self.flush();
        snapshot
    }
}

impl Inner {
    fn apply(
        &mut self,
        key_id: &str,
        ev: Event,
        now: i64,
        lease_ms: i64,
        max_sessions: usize,
        max_agents: usize,
    ) -> Result<Ack, EventError> {
        let key: SessionKey = (key_id.to_string(), ev.session_id.clone());
        if ev.event == "session_start" {
            return self.start(key, ev, now, lease_ms, max_sessions, max_agents);
        }
        let Some(session) = self.sessions.get(&key) else {
            return Err(err("unknown_instance", 409));
        };
        if session.instance != ev.instance_id {
            return Err(err("unknown_instance", 409));
        }
        // A lapsed lease reads as an ended instance to the companion, as the Go engine reported it: only a
        // `session_start` can take the session back.
        if session.ended || !session.attended {
            return Err(err("instance_ended", 409));
        }
        if ev.sequence <= session.sequence {
            return Err(err("stale_sequence", 409));
        }
        let agent_id = ev
            .agent_id
            .clone()
            .unwrap_or_else(|| MAIN_AGENT.to_string());
        // The lease renews on a heartbeat only, as the Go engine had it; agent_start, activity, and state
        // report the agent, not the companion's own liveness, so they leave it alone.
        let renews_lease = ev.event == "heartbeat";
        match ev.event.as_str() {
            "heartbeat" => {}
            "session_end" => {
                let agents: Vec<String> = self.sessions[&key].agents.keys().cloned().collect();
                for agent in &agents {
                    self.discard(
                        &key,
                        agent,
                        Reason::SessionEnded,
                        Some(Wasted::SessionEnded),
                    );
                    self.sessions
                        .get_mut(&key)
                        .unwrap()
                        .agents
                        .get_mut(agent)
                        .unwrap()
                        .state = AgentState::Released;
                }
                let session = self.sessions.get_mut(&key).unwrap();
                session.ended = true;
                session.sequence = ev.sequence;
                session.last_report_at = now;
                let ack = acknowledge(session, session.agents.get(&agent_id));
                self.emit(SessionLifecycle::SessionEnded {
                    key_id: key.0.clone(),
                    session_id: key.1.clone(),
                });
                return Ok(ack);
            }
            "agent_start" => {
                let existing = self.sessions[&key]
                    .agents
                    .get(&agent_id)
                    .map(|a| (a.state, a.generation));
                match existing {
                    Some((state, generation)) => {
                        if state != AgentState::Released
                            || ev.request_generation != Some(generation)
                        {
                            return Err(err("agent_exists", 409));
                        }
                        self.discard(&key, &agent_id, Reason::AwaitingRequest, None);
                        let agent = self
                            .sessions
                            .get_mut(&key)
                            .unwrap()
                            .agents
                            .get_mut(&agent_id)
                            .unwrap();
                        agent.state = AgentState::Busy;
                        agent.last_activity = now;
                        agent.idle_ticks = 0;
                    }
                    None => {
                        if self.agents >= max_agents {
                            return Err(err("agent_limit", 429));
                        }
                        let incarnation = self.incarnation();
                        self.sessions.get_mut(&key).unwrap().agents.insert(
                            agent_id.clone(),
                            Agent::new(&agent_id, &key.0, incarnation, now),
                        );
                        self.agents += 1;
                        self.set_reason(&key, &agent_id, Reason::AwaitingRequest);
                    }
                }
                if agent_id != MAIN_AGENT {
                    self.emit(SessionLifecycle::AgentStarted {
                        key_id: key.0.clone(),
                        session_id: key.1.clone(),
                        agent_id: agent_id.clone(),
                    });
                }
            }
            "activity" | "state" => {
                let Some(agent) = self.sessions[&key].agents.get(&agent_id) else {
                    return Err(err("unknown_agent", 409));
                };
                if ev.request_generation != Some(agent.generation) {
                    return Err(err("stale_request_generation", 409));
                }
                if agent.state == AgentState::Released {
                    return Err(err("agent_released", 409));
                }
                let was = agent.state;
                if ev.event == "activity" {
                    let agent = self
                        .sessions
                        .get_mut(&key)
                        .unwrap()
                        .agents
                        .get_mut(&agent_id)
                        .unwrap();
                    agent.last_activity = now;
                    agent.idle_ticks = 0;
                    agent.state = AgentState::Busy;
                    if agent_id == MAIN_AGENT && was == AgentState::Parked {
                        self.emit(SessionLifecycle::MainResumed {
                            key_id: key.0.clone(),
                            session_id: key.1.clone(),
                        });
                    }
                } else {
                    let state = ev
                        .state
                        .as_deref()
                        .and_then(AgentState::parse)
                        .ok_or(err("invalid_state", 400))?;
                    self.sessions
                        .get_mut(&key)
                        .unwrap()
                        .agents
                        .get_mut(&agent_id)
                        .unwrap()
                        .state = state;
                    match state {
                        AgentState::Released => self.discard(
                            &key,
                            &agent_id,
                            Reason::AgentReleased,
                            Some(Wasted::Released),
                        ),
                        AgentState::Unknown => {
                            self.discard(&key, &agent_id, Reason::AgentUnknown, None)
                        }
                        _ => {}
                    }
                    // The main agent's park and its return are the session's, and a subagent's release is the
                    // end of that agent; a report of the state it already held is no transition.
                    let transition = match (agent_id == MAIN_AGENT, was, state) {
                        (true, AgentState::Parked, AgentState::Busy | AgentState::Idle) => {
                            Some(SessionLifecycle::MainResumed {
                                key_id: key.0.clone(),
                                session_id: key.1.clone(),
                            })
                        }
                        (true, previous, AgentState::Parked) if previous != AgentState::Parked => {
                            Some(SessionLifecycle::MainParked {
                                key_id: key.0.clone(),
                                session_id: key.1.clone(),
                            })
                        }
                        (false, _, AgentState::Released) => Some(SessionLifecycle::AgentReleased {
                            key_id: key.0.clone(),
                            session_id: key.1.clone(),
                            agent_id: agent_id.clone(),
                        }),
                        _ => None,
                    };
                    if let Some(transition) = transition {
                        self.emit(transition);
                    }
                }
            }
            _ => return Err(err("unknown_event", 400)),
        }
        let session = self.sessions.get_mut(&key).unwrap();
        session.sequence = ev.sequence;
        if renews_lease {
            session.lease = now + lease_ms;
        }
        session.attended = true;
        session.last_report_at = now;
        Ok(acknowledge(session, session.agents.get(&agent_id)))
    }

    fn start(
        &mut self,
        key: SessionKey,
        ev: Event,
        now: i64,
        lease_ms: i64,
        max_sessions: usize,
        max_agents: usize,
    ) -> Result<Ack, EventError> {
        if let Some(session) = self.sessions.get_mut(&key) {
            if session.instance == ev.instance_id {
                // A same-instance start after a lapse re-adopts the session: the companion may only have been
                // silent, and the recipes it left are still the ones its next request needs.
                if session.ended && session.attended {
                    return Err(err("instance_ended", 409));
                }
                if ev.sequence <= session.sequence {
                    return Err(err("stale_sequence", 409));
                }
                session.sequence = ev.sequence;
                session.lease = now + lease_ms;
                session.attended = true;
                session.ended = false;
                session.last_report_at = now;
                if ev.owner_id.is_some() {
                    session.owner_id = ev.owner_id.clone();
                }
                return Ok(acknowledge(session, session.agents.get(MAIN_AGENT)));
            }
            if ev.previous_instance_id.as_deref() != Some(session.instance.as_str()) {
                return Err(err("instance_conflict", 409));
            }
        } else {
            if ev.previous_instance_id.is_some() {
                return Err(err("unknown_previous_instance", 409));
            }
            if self.sessions.len() >= max_sessions {
                return Err(err("session_limit", 429));
            }
        }
        let held = self.sessions.get(&key).map(|s| s.agents.len()).unwrap_or(0);
        if self.agents - held >= max_agents {
            return Err(err("agent_limit", 429));
        }
        if let Some(previous) = self.sessions.get(&key) {
            let agents: Vec<String> = previous.agents.keys().cloned().collect();
            for agent in &agents {
                self.discard(
                    &key,
                    agent,
                    Reason::InstanceReplaced,
                    Some(Wasted::Replaced),
                );
            }
            self.agents -= agents.len();
        }
        let last_real_at = self.sessions.get(&key).and_then(|s| s.last_real_at);
        let mut agents = BTreeMap::new();
        let incarnation = self.incarnation();
        agents.insert(
            MAIN_AGENT.to_string(),
            Agent::new(MAIN_AGENT, &key.0, incarnation, now),
        );
        self.sessions.insert(
            key.clone(),
            Session {
                key_id: key.0.clone(),
                session_id: key.1.clone(),
                instance: ev.instance_id.clone(),
                sequence: ev.sequence,
                lease: now + lease_ms,
                attended: true,
                ended: false,
                owner_id: ev.owner_id.clone(),
                last_report_at: now,
                last_real_at,
                agents,
            },
        );
        self.agents += 1;
        self.set_reason(&key, MAIN_AGENT, Reason::AwaitingRequest);
        self.emit(SessionLifecycle::SessionStarted {
            key_id: key.0.clone(),
            session_id: key.1.clone(),
        });
        let session = &self.sessions[&key];
        Ok(acknowledge(session, session.agents.get(MAIN_AGENT)))
    }
}
