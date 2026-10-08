//! The keepalive mod's reporting protocol: six events in JSON bodies, monotonic sequences, per-agent
//! generations, a lease, and a registration acknowledgment that carries the session's agents so the mod
//! reconciles from the answer. The contract is the Go engine's, plus `owner_id` on `session_start`, the park
//! fields of a `parked` state, the `presence` activity and the key scope on every session.

use std::collections::BTreeMap;

use crate::config::LEASE_MS;
use crate::engine::{
    Agent, AgentState, Engine, Inner, ParkKind, Reason, Session, SessionKey, Wasted, MAIN_AGENT,
};
use crate::recipe::valid_id;

pub const PROTOCOL: u64 = 1;

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub agent_id: Option<String>,
    /// Epoch milliseconds: the latest deadline across the background commands a park waits on.
    pub deadline: Option<i64>,
    pub event: String,
    pub instance_id: String,
    pub kind: Option<String>,
    pub owner_id: Option<String>,
    pub park_kind: Option<String>,
    pub previous_instance_id: Option<String>,
    pub protocol: Option<u64>,
    pub request_generation: Option<u64>,
    pub sequence: u64,
    pub session_id: String,
    pub state: Option<String>,
    /// Epoch milliseconds: the earliest scheduled wakeup a park waits on.
    pub wake_at: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct AckAgent {
    pub agent_id: String,
    pub request_generation: u64,
    pub state: AgentState,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Ack {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agents: Option<Vec<AckAgent>>,
    pub instance_id: String,
    pub lease_expires_ms: i64,
    pub request_generation: u64,
    pub sequence: u64,
    pub session_id: String,
}

#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq, serde::Serialize)]
#[error("{code}")]
pub struct EventError {
    pub code: &'static str,
    /// The holder an `instance_conflict` names when it ended or went silent, so the mod can retry the start as
    /// a takeover of that instance.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    /// The agent's current generation on `stale_request_generation` and `agent_released`, so the mod adopts it
    /// without a registration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_generation: Option<u64>,
    #[serde(skip)]
    pub status: u16,
}

pub(crate) const fn err(code: &'static str, status: u16) -> EventError {
    EventError {
        code,
        instance_id: None,
        request_generation: None,
        status,
    }
}

fn err_for_instance(code: &'static str, instance: &str) -> EventError {
    EventError {
        instance_id: Some(instance.to_string()),
        ..err(code, 409)
    }
}

fn err_for_generation(code: &'static str, generation: u64) -> EventError {
    EventError {
        request_generation: Some(generation),
        ..err(code, 409)
    }
}

/// The members each event takes beyond the five every report carries, each with whether it is required. A
/// member on an event that does not take it is refused rather than ignored, so a report the mod got wrong never
/// reads as one it got right.
fn members(event: &str) -> Option<&'static [(&'static str, bool)]> {
    let members: &'static [(&'static str, bool)] = match event {
        "activity" => &[
            ("agent_id", true),
            ("kind", true),
            ("request_generation", false),
        ],
        "agent_start" => &[("agent_id", true), ("request_generation", false)],
        "heartbeat" | "session_end" => &[],
        "session_start" => &[("owner_id", false), ("previous_instance_id", false)],
        "state" => &[
            ("agent_id", true),
            ("deadline", false),
            ("park_kind", false),
            ("request_generation", false),
            ("state", true),
            ("wake_at", false),
        ],
        _ => return None,
    };
    Some(members)
}

#[derive(serde::Deserialize)]
struct ProtocolOnly {
    protocol: Option<u64>,
}

/// One report as the mod sends it. The body bound is the route's, and everything else the protocol does not
/// take is refused here, before the state machine sees it.
pub fn decode_json_event(body: &[u8]) -> Result<Event, EventError> {
    let event: Event = match serde_json::from_slice(body) {
        Ok(event) => event,
        // A newer protocol's body may carry members this one does not know, and the mod has to learn that the
        // protocol is what the router refuses rather than its report.
        Err(_) => {
            return Err(match serde_json::from_slice::<ProtocolOnly>(body) {
                Ok(ProtocolOnly {
                    protocol: Some(protocol),
                }) if protocol != PROTOCOL => err("unsupported_protocol", 400),
                _ => err("invalid_event", 400),
            })
        }
    };
    match event.protocol {
        None => return Err(err("invalid_event", 400)),
        Some(PROTOCOL) => {}
        Some(_) => return Err(err("unsupported_protocol", 400)),
    }
    let Some(members) = members(&event.event) else {
        return Err(err("unknown_event", 400));
    };
    let present: Vec<&str> = [
        ("agent_id", event.agent_id.is_some()),
        ("deadline", event.deadline.is_some()),
        ("kind", event.kind.is_some()),
        ("owner_id", event.owner_id.is_some()),
        ("park_kind", event.park_kind.is_some()),
        ("previous_instance_id", event.previous_instance_id.is_some()),
        ("request_generation", event.request_generation.is_some()),
        ("state", event.state.is_some()),
        ("wake_at", event.wake_at.is_some()),
    ]
    .into_iter()
    .filter_map(|(name, there)| there.then_some(name))
    .collect();
    if present
        .iter()
        .any(|name| !members.iter().any(|(member, _)| member == name))
        || members
            .iter()
            .any(|(member, required)| *required && !present.contains(member))
    {
        return Err(err("invalid_event", 400));
    }
    if event
        .kind
        .as_deref()
        .is_some_and(|kind| kind != "prompt" && kind != "presence")
    {
        return Err(err("invalid_state", 400));
    }
    if let Some(state) = event.state.as_deref() {
        if AgentState::parse(state).is_none() {
            return Err(err("invalid_state", 400));
        }
        let park = event.park_kind.is_some() || event.wake_at.is_some() || event.deadline.is_some();
        if state != "parked" && park {
            return Err(err("invalid_event", 400));
        }
        if state == "parked" {
            let kind = event
                .park_kind
                .as_deref()
                .ok_or(err("invalid_event", 400))?;
            let kind = ParkKind::parse(kind).ok_or(err("invalid_state", 400))?;
            if kind != ParkKind::Background && event.wake_at.is_none() {
                return Err(err("invalid_event", 400));
            }
        }
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

fn agents_of(session: &Session) -> Vec<AckAgent> {
    session
        .agents
        .values()
        .map(|a| AckAgent {
            agent_id: a.id.clone(),
            request_generation: a.generation,
            state: a.state,
        })
        .collect()
}

fn acknowledge(session: &Session, agent: Option<&Agent>, agents: Option<Vec<AckAgent>>) -> Ack {
    Ack {
        agent_id: agent.map(|a| a.id.clone()),
        agents,
        instance_id: session.instance.clone(),
        lease_expires_ms: session.lease,
        request_generation: agent.map(|a| a.generation).unwrap_or(0),
        sequence: session.sequence,
        session_id: session.session_id.clone(),
    }
}

impl Engine {
    /// `key_id` and `key_generation` are the authenticated key's, never fields of the event: a session is
    /// scoped to the holder that reports it, and no member of the body can name another.
    pub fn apply_event(
        &self,
        key_id: &str,
        key_generation: i64,
        event: Event,
    ) -> Result<Ack, EventError> {
        let now = self.now();
        let expiry = self.parts.session_expiry.as_millis() as i64;
        let lease = LEASE_MS;
        let limits = self.parts.limits.clone();
        let acked = {
            let mut inner = self.lock();
            if inner.closed {
                inner.late_updates += 1;
                return Err(err("quiesced", 503));
            }
            inner.expire(now, expiry);
            inner.apply(
                key_id,
                key_generation,
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
}

impl Inner {
    #[allow(clippy::too_many_arguments)]
    fn apply(
        &mut self,
        key_id: &str,
        key_generation: i64,
        ev: Event,
        now: i64,
        lease_ms: i64,
        max_sessions: usize,
        max_agents: usize,
    ) -> Result<Ack, EventError> {
        let key: SessionKey = (key_id.to_string(), ev.session_id.clone());
        if ev.event == "session_start" {
            return self.start(
                key,
                key_generation,
                ev,
                now,
                lease_ms,
                max_sessions,
                max_agents,
            );
        }
        let Some(session) = self.sessions.get(&key) else {
            return Err(err("unknown_instance", 409));
        };
        // A session the previous holder of this key id registered answers as one that was never registered, so
        // a replacement holder learns nothing about it: not its instance, not its sequence, not its state.
        if session.key_generation != key_generation {
            return Err(err("unknown_instance", 409));
        }
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
                    let agent = self
                        .sessions
                        .get_mut(&key)
                        .unwrap()
                        .agents
                        .get_mut(agent)
                        .unwrap();
                    agent.set_state(AgentState::Released);
                    // A session's end is no return, so main's window ends with it.
                    agent.close_idle_episode();
                }
                let session = self.sessions.get_mut(&key).unwrap();
                session.ended = true;
                session.sequence = ev.sequence;
                session.last_report_at = now;
                self.touch_session(&key);
                let session = &self.sessions[&key];
                return Ok(acknowledge(session, session.agents.get(&agent_id), None));
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
                        agent.set_state(AgentState::Busy);
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
                            Agent::new(&agent_id, &key.0, incarnation, now, self.timing),
                        );
                        self.agents += 1;
                        self.set_reason(&key, &agent_id, Reason::AwaitingRequest);
                    }
                }
            }
            "activity" | "state" => {
                let Some(agent) = self.sessions[&key].agents.get(&agent_id) else {
                    return Err(err("unknown_agent", 409));
                };
                if ev.request_generation != Some(agent.generation) {
                    return Err(err_for_generation(
                        "stale_request_generation",
                        agent.generation,
                    ));
                }
                if agent.state == AgentState::Released {
                    return Err(err_for_generation("agent_released", agent.generation));
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
                    // A person at the terminal says nothing about the agent's work, so presence moves the idle
                    // clock alone. Only the decoder requires the kind, so one a direct caller left out is a prompt.
                    let presence = ev.kind.as_deref() == Some("presence");
                    if presence {
                        // Presence restarts the window, so the one counted so far ends with no return, closed
                        // before the start moves so the new start clears the floor.
                        agent.close_idle_episode();
                        agent.last_presence_at = Some(now);
                    }
                    agent.last_activity = now;
                    if !presence {
                        // The request this prompt announces is judged at the window's age now, however many ticks
                        // pass before it arrives. The window's start stays where it is.
                        if let Some(episode) = agent.idle_episode.as_mut() {
                            episode.prompt_at = Some(now);
                        }
                        agent.idle_ticks = 0;
                        agent.set_state(AgentState::Busy);
                    }
                } else {
                    let state = ev
                        .state
                        .as_deref()
                        .and_then(AgentState::parse)
                        .ok_or(err("invalid_state", 400))?;
                    let park_kind = match ev.park_kind.as_deref() {
                        None => None,
                        Some(kind) => Some(ParkKind::parse(kind).ok_or(err("invalid_state", 400))?),
                    };
                    let parked = state == AgentState::Parked;
                    let agent = self
                        .sessions
                        .get_mut(&key)
                        .unwrap()
                        .agents
                        .get_mut(&agent_id)
                        .unwrap();
                    if parked {
                        // A repeated `parked` of the same kind moves the wake or the deadline and keeps the park's
                        // start, which the parked ceiling counts from.
                        if was != AgentState::Parked || agent.park_kind != park_kind {
                            agent.park_began = Some(now);
                        }
                        agent.set_state(state);
                        agent.park_kind = park_kind;
                        agent.wake_at = ev.wake_at;
                        agent.deadline = ev.deadline;
                    } else {
                        // The mod's heartbeat closes a command past its deadline or sees the wake pass, and reports
                        // idle before or after the router's tick does: a park that ended without a request, which
                        // restarts the idle window. A request behind a busy report moves the window itself.
                        if was == AgentState::Parked && state == AgentState::Idle {
                            agent.close_idle_episode();
                            agent.park_ended_at = Some(now);
                        } else if state == AgentState::Released {
                            // A released main is the end of its session's window, as `session_end` is.
                            agent.close_idle_episode();
                        } else if state == AgentState::Idle {
                            // A prompt that sent no request, such as a local command: the window goes on.
                            if let Some(episode) = agent.idle_episode.as_mut() {
                                episode.prompt_at = None;
                            }
                        }
                        agent.set_state(state);
                    }
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
        self.touch_session(&key);
        // A heartbeat moves the session alone, so it rewrites no agent's sealed prompt.
        if !renews_lease {
            self.touch_agent(&key, &agent_id);
        }
        let session = &self.sessions[&key];
        Ok(acknowledge(session, session.agents.get(&agent_id), None))
    }

    #[allow(clippy::too_many_arguments)]
    fn start(
        &mut self,
        key: SessionKey,
        key_generation: i64,
        ev: Event,
        now: i64,
        lease_ms: i64,
        max_sessions: usize,
        max_agents: usize,
    ) -> Result<Ack, EventError> {
        // What a previous holder of this key id left under the same session id is not this caller's to
        // re-adopt or to conflict with; the start below replaces it whole, as it replaces an instance.
        let mine = self
            .sessions
            .get(&key)
            .is_some_and(|s| s.key_generation == key_generation);
        // An ended session keeps its entry until the expiry and holds no slot. The bound is about what is live,
        // and it is met by every start that brings a session to life, an instance replacement included. A
        // same-instance recovery of a live session consumes nothing.
        let becomes_live = self.sessions.get(&key).is_none_or(|s| s.ended);
        if becomes_live {
            let live = self.sessions.values().filter(|s| !s.ended).count();
            if live >= max_sessions {
                return Err(err("session_limit", 429));
            }
        }
        if mine {
            let session = self.sessions.get_mut(&key).expect("the session just read");
            if session.instance == ev.instance_id {
                // A same-instance start after a lapse re-adopts the session: the mod may only have been silent,
                // and the recipes it left are still the ones its next request needs. One after the session's end
                // revives it: the mod's instance is its process's, so a /clear and then a /resume back to this
                // id registers it again under the same instance, and a refusal would only draw the same start.
                if ev.sequence <= session.sequence {
                    return Err(err("stale_sequence", 409));
                }
                let revived = session.ended;
                session.sequence = ev.sequence;
                session.lease = now + lease_ms;
                session.attended = true;
                session.ended = false;
                session.last_report_at = now;
                if ev.owner_id.is_some() {
                    session.owner_id = ev.owner_id.clone();
                }
                if revived {
                    // The end released every agent, and a real request arms nothing on a released one, so main
                    // starts again as a registration starts it. The subagents stay released until the mod starts
                    // them again, and every generation stays as the end left it.
                    if let Some(main) = session.agents.get_mut(MAIN_AGENT) {
                        main.set_state(AgentState::Busy);
                        main.last_activity = now;
                        main.idle_ticks = 0;
                    }
                    self.set_reason(&key, MAIN_AGENT, Reason::AwaitingRequest);
                }
                self.touch_session(&key);
                let session = &self.sessions[&key];
                return Ok(acknowledge(
                    session,
                    session.agents.get(MAIN_AGENT),
                    Some(agents_of(session)),
                ));
            }
            if ev.previous_instance_id.as_deref() != Some(session.instance.as_str()) {
                // An ended or silent holder is named so the mod can take over from it, and a live one is not.
                return Err(if session.ended || !session.attended {
                    err_for_instance("instance_conflict", &session.instance)
                } else {
                    err("instance_conflict", 409)
                });
            }
        } else if ev.previous_instance_id.is_some() {
            return Err(err("unknown_previous_instance", 409));
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
            // Filed under the generation the replaced rows were saved under: a start under a new generation leaves
            // one removal for the old row and one write for the new, so a load never meets both.
            self.forget_session(&key);
        }
        // A previous holder's traffic says nothing about this key's session, so only a start of the same
        // generation inherits when the session last carried a real request.
        let last_real_at = if mine {
            self.sessions.get(&key).and_then(|s| s.last_real_at)
        } else {
            None
        };
        let mut agents = BTreeMap::new();
        let incarnation = self.incarnation();
        agents.insert(
            MAIN_AGENT.to_string(),
            Agent::new(MAIN_AGENT, &key.0, incarnation, now, self.timing),
        );
        self.sessions.insert(
            key.clone(),
            Session {
                key_id: key.0.clone(),
                key_generation,
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
        self.touch_session(&key);
        self.set_reason(&key, MAIN_AGENT, Reason::AwaitingRequest);
        let session = &self.sessions[&key];
        Ok(acknowledge(
            session,
            session.agents.get(MAIN_AGENT),
            Some(agents_of(session)),
        ))
    }
}
