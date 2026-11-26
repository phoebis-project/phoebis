//! The keepalive mod's reporting protocol: eight events in JSON bodies, monotonic sequences, per-agent
//! generations, a lease, and a registration acknowledgment that carries the session's agents so the mod
//! reconciles from the answer. The contract is the Go engine's, plus the park fields of a `parked` state, the
//! `presence` activity, the key scope on every session and the two facts events.

use std::collections::{BTreeMap, BTreeSet};

use phoebis_relay::is_valid_id as valid_id;
use phoebis_routing::{RegistrySnapshot, Report};
use phoebis_store::FactsKind;

use crate::config::LEASE_MS;
use crate::engine::{
    agent_mut, agent_ref, open_idle_episode, Agent, AgentState, Engine, HeldPark, Inner, Park,
    ParkKind, Reason, Session, SessionKey, Wasted, AGENT_HELD, MAIN_AGENT,
};
use crate::facts::{AgentFacts, RawAgentFacts, RawSessionFacts, SessionFacts};

pub(crate) const PROTOCOL: u64 = 1;

/// An optional member takes one field here and one row of `members()`. The body's own keys say which members it
/// names, so no second list repeats them.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Event {
    pub agent_id: Option<String>,
    pub agent_type: Option<String>,
    pub branch: Option<String>,
    pub cwd: Option<String>,
    /// Epoch milliseconds: the latest deadline across the background commands a park waits on.
    pub deadline: Option<i64>,
    pub description: Option<String>,
    pub event: String,
    pub instance_id: String,
    pub kind: Option<String>,
    pub name: Option<String>,
    pub park_kind: Option<String>,
    pub previous_instance_id: Option<String>,
    pub project: Option<String>,
    pub reported_facts: Option<Vec<String>>,
    pub repository: Option<String>,
    pub repository_root: Option<String>,
    pub request_generation: Option<u64>,
    pub sequence: u64,
    pub session_id: String,
    pub state: Option<String>,
    pub title: Option<String>,
    /// Epoch milliseconds: the earliest scheduled wakeup a park waits on.
    pub wake_at: Option<i64>,
}

/// A parked agent's entry carries its park as well as its state, so the mod can see that a router restored from an
/// older save holds an older park.
#[derive(Debug, serde::Serialize)]
pub(crate) struct AckAgent {
    pub agent_id: String,
    #[serde(flatten)]
    pub park: Option<Park>,
    pub request_generation: u64,
    pub state: AgentState,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct Ack {
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

/// An accepted report's answer, with whether the report filed a set of facts, which asks for a save at once.
pub(crate) struct Applied {
    pub ack: Ack,
    pub filed_facts: bool,
}

#[derive(Debug, thiserror::Error, serde::Serialize)]
#[error("{code}")]
pub(crate) struct EventError {
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

enum EventKind {
    Activity,
    AgentFacts,
    AgentStart,
    Heartbeat,
    SessionEnd,
    SessionFacts,
    SessionStart,
    State,
}

/// The members every report carries. Each other key of a body, a null one too, is a member that the body names.
const COMMON_MEMBERS: [&str; 5] = ["event", "instance_id", "protocol", "sequence", "session_id"];

/// The members each event takes beyond the five every report carries, each with whether it is required. A
/// member on an event that does not take it is refused rather than ignored, so a report the mod got wrong never
/// reads as one it got right.
fn members(event: &str) -> Option<(EventKind, &'static [(&'static str, bool)])> {
    let members: (EventKind, &'static [(&'static str, bool)]) = match event {
        "activity" => (
            EventKind::Activity,
            &[
                ("agent_id", true),
                ("kind", true),
                ("request_generation", false),
            ],
        ),
        "agent_facts" => (
            EventKind::AgentFacts,
            &[
                ("agent_id", true),
                ("agent_type", false),
                ("description", false),
                ("name", false),
                ("request_generation", false),
            ],
        ),
        "agent_start" => (
            EventKind::AgentStart,
            &[("agent_id", true), ("request_generation", false)],
        ),
        "heartbeat" => (EventKind::Heartbeat, &[]),
        "session_end" => (EventKind::SessionEnd, &[]),
        "session_facts" => (
            EventKind::SessionFacts,
            &[
                ("branch", false),
                ("cwd", false),
                ("project", false),
                ("repository", false),
                ("repository_root", false),
                ("title", false),
            ],
        ),
        "session_start" => (
            EventKind::SessionStart,
            &[("previous_instance_id", false), ("reported_facts", true)],
        ),
        "state" => (
            EventKind::State,
            &[
                ("agent_id", true),
                ("deadline", false),
                ("park_kind", false),
                ("request_generation", false),
                ("state", true),
                ("wake_at", false),
            ],
        ),
        _ => return None,
    };
    Some(members)
}

/// A report's body as one JSON object, with what the read saw of a key that the body names twice. An object keeps the
/// last value of such a key, so the read records the repeat for the checks that follow.
struct Body {
    object: serde_json::Map<String, serde_json::Value>,
    /// `protocol` stands twice in the body.
    protocol_repeated: bool,
    /// A key other than `protocol` stands twice in the body.
    member_repeated: bool,
}

impl<'de> serde::Deserialize<'de> for Body {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Read;
        impl<'de> serde::de::Visitor<'de> for Read {
            type Value = Body;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a JSON object")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Body, A::Error> {
                let mut body = Body {
                    object: serde_json::Map::new(),
                    protocol_repeated: false,
                    member_repeated: false,
                };
                while let Some((key, value)) = map.next_entry::<String, serde_json::Value>()? {
                    if body.object.contains_key(&key) {
                        if key == "protocol" {
                            body.protocol_repeated = true;
                        } else {
                            body.member_repeated = true;
                        }
                    }
                    body.object.insert(key, value);
                }
                Ok(body)
            }
        }
        deserializer.deserialize_map(Read)
    }
}

/// The members a body names: each key beyond `COMMON_MEMBERS`, with whether its value is null.
struct Named(BTreeMap<String, bool>);

impl Named {
    fn of(object: &serde_json::Map<String, serde_json::Value>) -> Named {
        Named(
            object
                .iter()
                .filter(|(key, _)| !COMMON_MEMBERS.contains(&key.as_str()))
                .map(|(key, value)| (key.clone(), value.is_null()))
                .collect(),
        )
    }
}

/// A report the protocol takes, with its event and the event's own members typed, so the state machine reads no
/// word the conversion from `Event` did not check.
#[derive(Debug)]
pub(crate) struct EventReport {
    pub session_id: String,
    pub instance_id: String,
    pub sequence: u64,
    pub event: Reported,
}

/// One event with the members it takes.
#[derive(Debug, PartialEq)]
pub(crate) enum Reported {
    /// A prompt, or the person's presence at the terminal, which moves the idle clock alone.
    Activity {
        agent_id: String,
        presence: bool,
        request_generation: Option<u64>,
    },
    AgentFacts {
        agent_id: String,
        facts: AgentFacts,
        request_generation: Option<u64>,
    },
    AgentStart {
        agent_id: String,
        request_generation: Option<u64>,
    },
    Heartbeat,
    /// A `parked` state, the one state that carries a park.
    Parked {
        agent_id: String,
        park: Park,
        request_generation: Option<u64>,
    },
    SessionEnd,
    SessionFacts {
        facts: SessionFacts,
    },
    SessionStart {
        previous_instance_id: Option<String>,
        /// The kinds of facts the instance reports. The session's record keeps no copy.
        reported_facts: BTreeSet<FactsKind>,
    },
    /// Any state but `parked`.
    State {
        agent_id: String,
        request_generation: Option<u64>,
        state: AgentState,
    },
}

impl Reported {
    /// The agent an agent event reports. A session-level event names none.
    fn agent_id(&self) -> Option<&str> {
        match self {
            Reported::Activity { agent_id, .. }
            | Reported::AgentFacts { agent_id, .. }
            | Reported::AgentStart { agent_id, .. }
            | Reported::Parked { agent_id, .. }
            | Reported::State { agent_id, .. } => Some(agent_id),
            Reported::Heartbeat
            | Reported::SessionEnd
            | Reported::SessionFacts { .. }
            | Reported::SessionStart { .. } => None,
        }
    }
}

impl EventReport {
    /// The one conversion a report takes on its way to the state machine: everything the protocol does not take is
    /// refused here, the body's own decoding aside. `named` is what the body names, read off its keys.
    fn of(event: Event, named: &Named) -> Result<EventReport, EventError> {
        let Some((event_kind, members)) = members(&event.event) else {
            return Err(err("unknown_event", 400));
        };
        // A null names its member as a value does, so it is refused off its event, and a required member needs a
        // value.
        if named
            .0
            .keys()
            .any(|name| !members.iter().any(|(member, _)| member == name))
            || members
                .iter()
                .any(|(member, required)| *required && named.0.get(*member) != Some(&false))
        {
            return Err(err("invalid_event", 400));
        }
        // A word that names no kind is refused rather than skipped, so a misspelled kind never reads as one the list
        // leaves out, which would drop its stored facts.
        let reported_facts = event
            .reported_facts
            .iter()
            .flatten()
            .map(|word| FactsKind::parse(word).ok_or(err("invalid_event", 400)))
            .collect::<Result<BTreeSet<FactsKind>, EventError>>()?;
        let presence = match event.kind.as_deref() {
            None | Some("prompt") => false,
            Some("presence") => true,
            Some(_) => return Err(err("invalid_state", 400)),
        };
        let (state, park) = match event.state.as_deref() {
            None => (None, None),
            Some(word) => {
                let state = AgentState::parse(word).ok_or(err("invalid_state", 400))?;
                let park = if state == AgentState::Parked {
                    let kind = event
                        .park_kind
                        .as_deref()
                        .ok_or(err("invalid_event", 400))?;
                    let kind = ParkKind::parse(kind).ok_or(err("invalid_state", 400))?;
                    let park = Park::new(kind, event.deadline, event.wake_at);
                    if (kind.scheduled() && event.wake_at.is_none())
                        || park.deadline() != event.deadline
                        || park.wake_at() != event.wake_at
                    {
                        return Err(err("invalid_event", 400));
                    }
                    Some(park)
                } else if event.park_kind.is_some()
                    || event.wake_at.is_some()
                    || event.deadline.is_some()
                {
                    return Err(err("invalid_event", 400));
                } else {
                    None
                };
                (Some(state), park)
            }
        };
        let Event {
            agent_id,
            agent_type,
            branch,
            cwd,
            description,
            instance_id,
            name,
            previous_instance_id,
            project,
            repository,
            repository_root,
            request_generation,
            sequence,
            session_id,
            title,
            ..
        } = validate(event)?;
        let required = || err("invalid_event", 400);
        let event = match event_kind {
            EventKind::Activity => Reported::Activity {
                agent_id: agent_id.ok_or_else(required)?,
                presence,
                request_generation,
            },
            EventKind::AgentFacts => Reported::AgentFacts {
                agent_id: agent_id.ok_or_else(required)?,
                facts: AgentFacts::of(RawAgentFacts {
                    agent_type,
                    description,
                    name,
                }),
                request_generation,
            },
            EventKind::AgentStart => Reported::AgentStart {
                agent_id: agent_id.ok_or_else(required)?,
                request_generation,
            },
            EventKind::Heartbeat => Reported::Heartbeat,
            EventKind::SessionEnd => Reported::SessionEnd,
            EventKind::SessionFacts => Reported::SessionFacts {
                facts: SessionFacts::of(RawSessionFacts {
                    branch,
                    cwd,
                    project,
                    repository,
                    repository_root,
                    title,
                }),
            },
            EventKind::SessionStart => Reported::SessionStart {
                previous_instance_id,
                reported_facts,
            },
            EventKind::State => {
                let agent_id = agent_id.ok_or_else(required)?;
                let state = state.ok_or_else(required)?;
                match park {
                    Some(park) => Reported::Parked {
                        agent_id,
                        park,
                        request_generation,
                    },
                    None => Reported::State {
                        agent_id,
                        request_generation,
                        state,
                    },
                }
            }
        };
        Ok(EventReport {
            session_id,
            instance_id,
            sequence,
            event,
        })
    }
}

/// One report as the mod sends it. The body bound is the route's, the protocol is checked here, and the conversion
/// refuses everything else the protocol does not take, before the state machine sees it.
pub(crate) fn decode_json_event(body: &[u8]) -> Result<EventReport, EventError> {
    let Ok(Body {
        mut object,
        protocol_repeated,
        member_repeated,
    }) = serde_json::from_slice(body)
    else {
        return Err(err("invalid_event", 400));
    };
    // Two `protocol` members leave no one protocol to refuse.
    if protocol_repeated {
        return Err(err("invalid_event", 400));
    }
    // A newer protocol's body may carry members this one does not know, so the protocol is read ahead of every member:
    // the mod has to learn that the protocol is what the router refuses rather than its report.
    match object
        .remove("protocol")
        .as_ref()
        .and_then(serde_json::Value::as_u64)
    {
        Some(PROTOCOL) => {}
        Some(_) => return Err(err("unsupported_protocol", 400)),
        None => return Err(err("invalid_event", 400)),
    }
    // The object holds the last value of a repeated member, so the typed read below cannot see the repeat.
    if member_repeated {
        return Err(err("invalid_event", 400));
    }
    let named = Named::of(&object);
    let event: Event = serde_json::from_value(serde_json::Value::Object(object))
        .map_err(|_| err("invalid_event", 400))?;
    EventReport::of(event, &named)
}

fn validate(event: Event) -> Result<Event, EventError> {
    let optional_ok = |value: &Option<String>| value.as_deref().is_none_or(valid_id);
    if !valid_id(&event.session_id)
        || !valid_id(&event.instance_id)
        || event.sequence == 0
        || !optional_ok(&event.agent_id)
        || !optional_ok(&event.previous_instance_id)
    {
        return Err(err("invalid_identity", 400));
    }
    Ok(event)
}

/// Each kind of stored facts that a registration's list does not name, read off the store's words so that a kind the
/// store adds is dropped too.
fn left_out(reported: &BTreeSet<FactsKind>) -> impl Iterator<Item = FactsKind> + '_ {
    FactsKind::WORDS
        .iter()
        .filter_map(|word| FactsKind::parse(word))
        .filter(|kind| !reported.contains(kind))
}

fn agents_of(session: &Session) -> Vec<AckAgent> {
    session
        .agents
        .values()
        .map(|a| AckAgent {
            agent_id: a.id.clone(),
            park: a.park.map(|held| held.park),
            request_generation: a.generation,
            state: a.state,
        })
        .collect()
}

/// `named` is the agent an agent event reports. A session-level event (a heartbeat, a `session_facts`, a
/// `session_end`, a registration) names none and its answer carries main's generation, which the mod adopts for main.
fn acknowledge(
    key: &SessionKey,
    session: &Session,
    named: Option<&str>,
    agents: Option<Vec<AckAgent>>,
) -> Ack {
    let agent = session.agents.get(named.unwrap_or(MAIN_AGENT));
    Ack {
        agent_id: named.and(agent).map(|a| a.id.clone()),
        agents,
        instance_id: session.instance.clone(),
        lease_expires_ms: session.lease,
        request_generation: agent.map(|a| a.generation).unwrap_or(0),
        sequence: session.sequence,
        session_id: key.1.clone(),
    }
}

impl Engine {
    /// `key_id` and `key_generation` are the authenticated key's, never fields of the event: a session is
    /// scoped to the holder that reports it, and no member of the body can name another.
    pub(crate) fn apply_event(
        &self,
        key_id: &str,
        key_generation: i64,
        report: EventReport,
    ) -> Result<Applied, EventError> {
        let now = self.now();
        // The pool an idle window a report opens counts under, read as the tick reads it.
        let snapshot = self.parts.router.snapshot();
        let session_id = report.session_id.clone();
        let released = match &report.event {
            Reported::State {
                agent_id,
                state: AgentState::Released,
                ..
            } => Some(agent_id.clone()),
            _ => None,
        };
        let acked = {
            let mut inner = self.lock();
            if inner.closed {
                inner.late_updates += 1;
                return Err(err("quiesced", 503));
            }
            inner.expire(now);
            inner.apply(key_id, key_generation, report, now, &snapshot)
        };
        // Told after the lock, which the router's mailbox must never hold up. A full mailbox drops the end, and the
        // pin reads live until its next request or its idle expiry.
        if let Some(agent_id) = released.filter(|_| acked.is_ok()) {
            let ended = Report::AgentEnded {
                key_id: key_id.to_string(),
                session_id: session_id.clone(),
                agent_id: agent_id.clone(),
            };
            if self.parts.router.try_report(ended).is_err() {
                tracing::warn!(
                    key_id,
                    session_id = %session_id,
                    agent_id = %agent_id,
                    "the router's mailbox is full, so the agent's release does not reach its pin"
                );
            }
        }
        self.flush();
        acked
    }
}

impl Inner {
    fn apply(
        &mut self,
        key_id: &str,
        key_generation: i64,
        report: EventReport,
        now: i64,
        snapshot: &RegistrySnapshot,
    ) -> Result<Applied, EventError> {
        let key: SessionKey = (key_id.to_string(), report.session_id.clone());
        // A start registers the session every other report is checked against, so it takes none of the checks.
        if !matches!(report.event, Reported::SessionStart { .. }) {
            let Some(session) = self.sessions.get(&key) else {
                return Err(err("unknown_instance", 409));
            };
            // A session the previous holder of this key id registered answers as one that was never registered, so
            // a replacement holder learns nothing about it: not its instance, not its sequence, not its state.
            if session.key_generation != key_generation {
                return Err(err("unknown_instance", 409));
            }
            if session.instance != report.instance_id {
                return Err(err("unknown_instance", 409));
            }
            // A lapsed lease reads as an ended instance to the companion, as the Go engine reported it: only a
            // `session_start` can take the session back.
            if session.ended || !session.attended {
                return Err(err("instance_ended", 409));
            }
            if report.sequence <= session.sequence {
                return Err(err("stale_sequence", 409));
            }
        }
        let agent_id = report.event.agent_id().unwrap_or(MAIN_AGENT).to_string();
        // Both `session_start` paths in `start`, the same-instance re-adoption and the new registration, set the
        // lease, and among the later reports a heartbeat alone renews it, as the Go engine had it. `activity`,
        // `agent_start` and `state` report the agent, not the companion's own liveness, so they leave it alone.
        let renews_lease = report.event == Reported::Heartbeat;
        let mut filed_facts = false;
        match &report.event {
            Reported::SessionStart {
                previous_instance_id,
                reported_facts,
            } => {
                return self.start(
                    key,
                    key_generation,
                    &report,
                    previous_instance_id.as_deref(),
                    reported_facts,
                    now,
                );
            }
            Reported::Heartbeat => {}
            Reported::SessionFacts { facts } => {
                let row = (key.0.clone(), key_generation, key.1.clone());
                self.file_facts(&row, FactsKind::Project, "", facts.project_value(), now);
                self.file_facts(&row, FactsKind::Title, "", facts.title_value(), now);
                filed_facts = true;
            }
            Reported::SessionEnd => {
                let agents: Vec<String> = self.sessions[&key].agents.keys().cloned().collect();
                for agent_id in &agents {
                    self.discard(
                        &key,
                        agent_id,
                        Reason::SessionEnded,
                        Some(Wasted::SessionEnded),
                    );
                    let agent = agent_mut(&mut self.sessions, &key, agent_id).expect(AGENT_HELD);
                    agent.set_state(AgentState::Released);
                    // A session's end is no return, so main's window ends with it.
                    agent.close_idle_episode();
                    // The leave counts from the first release, so a record the mod released before the end keeps it.
                    if agent.leaves_at.is_none() {
                        self.release_record(&key, agent_id, now, snapshot);
                    }
                }
                let session = self.sessions.get_mut(&key).unwrap();
                session.ended = true;
                session.sequence = report.sequence;
                session.last_report_at = now;
                self.touch_session(&key);
                return Ok(Applied {
                    ack: acknowledge(&key, &self.sessions[&key], None, None),
                    filed_facts: false,
                });
            }
            Reported::AgentStart {
                request_generation, ..
            } => {
                let existing =
                    agent_ref(&self.sessions, &key, &agent_id).map(|a| (a.state, a.generation));
                match existing {
                    Some((state, generation)) => {
                        if state != AgentState::Released || *request_generation != Some(generation)
                        {
                            return Err(err("agent_exists", 409));
                        }
                        let released = self.sessions[&key].agents[&agent_id].last_stop;
                        self.discard(&key, &agent_id, Reason::AwaitingRequest, None);
                        let agent =
                            agent_mut(&mut self.sessions, &key, &agent_id).expect(AGENT_HELD);
                        // The release's label stays until the late result lands, so a probe the release orphaned
                        // settles under it rather than under nothing.
                        agent.last_stop = released;
                        agent.resume(now);
                    }
                    None => {
                        if self.agent_count() >= self.limits.max_agents {
                            return Err(err("agent_limit", 429));
                        }
                        let incarnation = self.incarnation();
                        self.sessions.get_mut(&key).unwrap().agents.insert(
                            agent_id.clone(),
                            Agent::new(&agent_id, &key.0, incarnation, now, self.timing),
                        );
                        self.set_reason(&key, &agent_id, Reason::AwaitingRequest);
                    }
                }
            }
            Reported::Activity {
                presence,
                request_generation,
                ..
            } => {
                let agent = self.reported_agent(&key, &agent_id, *request_generation)?;
                // A person at the terminal says nothing about the agent's work, so presence moves the idle clock
                // alone.
                if *presence {
                    // An open window stays open, since a person at the terminal has not come back to the conversation
                    // until a request says so. Presence moves the next window's start on purpose, so the floor goes: a
                    // clock that stepped back would otherwise leave that start behind the last close.
                    agent.idle_window_floor = None;
                    agent.last_presence_at = Some(now);
                }
                agent.last_activity = now;
                if !presence {
                    // The request this prompt announces is judged at the window's age now, however many ticks
                    // pass before it arrives. The window's start stays where it is.
                    if let Some(episode) = agent.idle_episode.as_mut() {
                        episode.prompt_at = Some(now);
                    }
                    agent.set_state(AgentState::Busy);
                }
            }
            Reported::AgentFacts {
                facts,
                request_generation,
                ..
            } => {
                // Display state alone: no state, no park and no idle clock moves, so a facts report never reads as
                // activity.
                self.reported_agent(&key, &agent_id, *request_generation)?;
                let row = (key.0.clone(), key_generation, key.1.clone());
                self.file_facts(&row, FactsKind::Agents, &agent_id, facts.value(), now);
                filed_facts = true;
            }
            Reported::Parked {
                park,
                request_generation,
                ..
            } => {
                let agent = self.reported_agent(&key, &agent_id, *request_generation)?;
                // A repeated `parked` of the same kind moves the wake or the deadline and keeps the park's start,
                // which the parked ceiling counts from.
                let began = agent
                    .park
                    .filter(|held| {
                        agent.state == AgentState::Parked && held.park.kind() == park.kind()
                    })
                    .map_or(now, |held| held.began);
                agent.set_state(AgentState::Parked);
                agent.park = Some(HeldPark { began, park: *park });
            }
            Reported::State {
                request_generation,
                state,
                ..
            } => {
                let agent = self.reported_agent(&key, &agent_id, *request_generation)?;
                let was = agent.state;
                let state = *state;
                // The mod's heartbeat closes a command past its deadline or sees the wake pass, and reports idle
                // before or after the router's tick does: a park that ended without a request, which restarts the
                // idle window. A request behind a busy report moves the window itself.
                if was == AgentState::Parked && state == AgentState::Idle {
                    agent.close_idle_episode();
                    agent.idle_window_floor = None;
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
                match state {
                    AgentState::Released => {
                        self.discard(
                            &key,
                            &agent_id,
                            Reason::AgentReleased,
                            Some(Wasted::Released),
                        );
                        self.release_record(&key, &agent_id, now, snapshot);
                    }
                    AgentState::Unknown => {
                        self.discard(&key, &agent_id, Reason::AgentUnknown, None)
                    }
                    _ => {}
                }
            }
        }
        let session = self.sessions.get_mut(&key).unwrap();
        session.sequence = report.sequence;
        if renews_lease {
            session.lease = now + LEASE_MS;
        }
        session.attended = true;
        session.last_report_at = now;
        self.touch_session(&key);
        // A session-level report moves the session alone, and a facts report moves no member of an agent's row, so
        // neither writes an agent's sealed prompt again.
        if report.event.agent_id().is_some() && !matches!(report.event, Reported::AgentFacts { .. })
        {
            self.touch_agent(&key, &agent_id);
        }
        // The window opens with the report that moves main to idle or parked, so a prompt inside the same second
        // still ends a window the tick would have opened later.
        if matches!(
            report.event,
            Reported::Parked { .. } | Reported::State { .. }
        ) && agent_id == MAIN_AGENT
        {
            if let Some(main) = agent_mut(&mut self.sessions, &key, MAIN_AGENT) {
                open_idle_episode(main, snapshot);
            }
        }
        Ok(Applied {
            ack: acknowledge(&key, &self.sessions[&key], report.event.agent_id(), None),
            filed_facts,
        })
    }

    /// The checks an `activity`, an `agent_facts` or a `state` report passes on the agent it names: a known agent, the
    /// generation the report carries, and a record not released. Answers the agent for the report to write.
    fn reported_agent(
        &mut self,
        key: &SessionKey,
        agent_id: &str,
        request_generation: Option<u64>,
    ) -> Result<&mut Agent, EventError> {
        let Some(agent) = agent_mut(&mut self.sessions, key, agent_id) else {
            return Err(err("unknown_agent", 409));
        };
        if request_generation != Some(agent.generation) {
            return Err(err_for_generation(
                "stale_request_generation",
                agent.generation,
            ));
        }
        if agent.state == AgentState::Released {
            return Err(err_for_generation("agent_released", agent.generation));
        }
        Ok(agent)
    }

    fn start(
        &mut self,
        key: SessionKey,
        key_generation: i64,
        report: &EventReport,
        previous_instance_id: Option<&str>,
        reported_facts: &BTreeSet<FactsKind>,
        now: i64,
    ) -> Result<Applied, EventError> {
        // What a previous holder of this key id left under the same session id is not this caller's to
        // re-adopt or to conflict with; the start below replaces it whole, as it replaces an instance.
        let mine = self
            .sessions
            .get(&key)
            .is_some_and(|s| s.key_generation == key_generation);
        // An ended session keeps its entry until the expiry and holds no slot, and a registered one holds none
        // until the first real request the caps admit, so a session that has sent nothing never fills the count. The
        // bound is met by every start that brings a session to life, an instance replacement included. A
        // same-instance recovery of a live session consumes nothing.
        let becomes_live = self.sessions.get(&key).is_none_or(|s| s.ended);
        if becomes_live {
            let live = self.live_sessions();
            if live >= self.limits.max_sessions {
                return Err(err("session_limit", 429));
            }
        }
        if mine {
            let full = self.agent_count() >= self.limits.max_agents;
            let session = self.sessions.get_mut(&key).expect("the session just read");
            if session.instance == report.instance_id {
                // A same-instance start after a lapse re-adopts the session: the mod may only have been silent,
                // and the recipes it left are still the ones its next request needs. One after the session's end
                // revives it: the mod's instance is its process's, so a /clear and then a /resume back to this
                // id registers it again under the same instance, and a refusal would only draw the same start.
                if report.sequence <= session.sequence {
                    return Err(err("stale_sequence", 409));
                }
                let revived = session.ended;
                // A main the revival registers anew takes a slot as any new record does, and a refusal leaves the
                // session as its end left it.
                let registers_main = revived && !session.agents.contains_key(MAIN_AGENT);
                if registers_main && full {
                    return Err(err("agent_limit", 429));
                }
                session.sequence = report.sequence;
                session.lease = now + LEASE_MS;
                session.attended = true;
                // The registration's own standing outranks the one the load restored.
                session.restored_until = None;
                session.ended = false;
                session.last_report_at = now;
                if revived {
                    // The end released every agent, and a real request arms nothing on a released one, so main
                    // starts again as a registration starts it. The subagents stay released until the mod starts
                    // them again, and every generation stays as the end left it.
                    if let Some(main) = session.agents.get_mut(MAIN_AGENT) {
                        main.resume(now);
                    } else {
                        // Main's record left one cache TTL after the end, so a resume past it registers main anew as
                        // a fresh start does.
                        let incarnation = self.incarnation();
                        self.sessions
                            .get_mut(&key)
                            .expect("the session just read")
                            .agents
                            .insert(
                                MAIN_AGENT.to_string(),
                                Agent::new(MAIN_AGENT, &key.0, incarnation, now, self.timing),
                            );
                    }
                    self.set_reason(&key, MAIN_AGENT, Reason::AwaitingRequest);
                }
                // An install that turned a kind off would otherwise keep the sets that an earlier run stored.
                let row = (key.0.clone(), key_generation, key.1.clone());
                let mut filed_facts = false;
                for kind in left_out(reported_facts) {
                    self.file_kind_leave(&row, kind);
                    filed_facts = true;
                }
                self.touch_session(&key);
                let session = &self.sessions[&key];
                return Ok(Applied {
                    ack: acknowledge(&key, session, None, Some(agents_of(session))),
                    filed_facts,
                });
            }
            if previous_instance_id != Some(session.instance.as_str()) {
                // An ended or silent holder is named so the mod can take over from it, and a live one is not.
                return Err(if session.ended || !session.attended {
                    err_for_instance("instance_conflict", &session.instance)
                } else {
                    err("instance_conflict", 409)
                });
            }
        } else if previous_instance_id.is_some() {
            return Err(err("unknown_previous_instance", 409));
        }
        // The replaced session's own slots, so the subtraction leaves the other sessions' count.
        let held = self.sessions.get(&key).map_or(0, Session::agent_slots);
        if self.agent_count() - held >= self.limits.max_agents {
            return Err(err("agent_limit", 429));
        }
        if self.sessions.contains_key(&key) {
            self.discard_session(&key, Reason::InstanceReplaced, Some(Wasted::Replaced));
            // Filed under the generation the replaced rows were saved under: a start under a new generation leaves
            // one removal for the old row and one write for the new, so a load never meets both.
            self.forget_session(&key);
        }
        // A previous holder's traffic says nothing about this key's session, so only a start of the same
        // generation inherits whether the session took its slots and when it last carried a real request.
        let (last_real_at, slotted) = self
            .sessions
            .get(&key)
            .filter(|_| mine)
            .map_or((None, false), |s| (s.last_real_at, s.slotted));
        let mut agents = BTreeMap::new();
        let incarnation = self.incarnation();
        agents.insert(
            MAIN_AGENT.to_string(),
            Agent::new(MAIN_AGENT, &key.0, incarnation, now, self.timing),
        );
        self.sessions.insert(
            key.clone(),
            Session {
                key_generation,
                instance: report.instance_id.clone(),
                sequence: report.sequence,
                lease: now + LEASE_MS,
                attended: true,
                ended: false,
                last_report_at: now,
                last_real_at,
                slotted,
                restored_until: None,
                instance_mismatches: 0,
                agents,
            },
        );
        // The session's own sets start over with the record whatever the list says, and its agents' stored sets stay
        // for the earlier agents that no report states again, unless the list leaves the kind out.
        let row = (key.0.clone(), key_generation, key.1.clone());
        self.file_kind_leave(&row, FactsKind::Project);
        self.file_kind_leave(&row, FactsKind::Title);
        if !reported_facts.contains(&FactsKind::Agents) {
            self.file_kind_leave(&row, FactsKind::Agents);
        }
        self.touch_session(&key);
        self.set_reason(&key, MAIN_AGENT, Reason::AwaitingRequest);
        let session = &self.sessions[&key];
        Ok(Applied {
            ack: acknowledge(&key, session, None, Some(agents_of(session))),
            filed_facts: true,
        })
    }
}
