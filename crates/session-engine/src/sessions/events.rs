//! The companion mod's reporting protocol: eight events in JSON bodies, monotonic sequences, per-agent
//! generations, a lease, and a registration acknowledgment that carries the session's agents so the mod
//! reconciles from the answer. The contract is the Go engine's, plus the park fields of a `parked` state, the
//! `presence` activity, the key scope on every session and the two facts events.

use std::collections::{BTreeMap, BTreeSet};

use phoebis_relay::{is_valid_agent_id, is_valid_id as valid_id};
use phoebis_store::FactsKind;

use crate::sessions::config::LEASE_MS;
use crate::sessions::facts::{AgentFacts, RawAgentFacts, RawSessionFacts, SessionFacts};
use crate::sessions::state::{
    agent_mut, agent_ref, Agent, AgentState, Park, ParkKind, Session, SessionKey, SessionsState,
    AGENT_HELD, MAIN_AGENT,
};
use crate::sessions::view::CompanionState;

pub(crate) const PROTOCOL: u64 = 1;

/// An optional member takes one field here and one row of `members()`. The body's own keys say which members it
/// names, so no second list repeats them.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Event {
    pub agent_id: Option<String>,
    pub agent_type: Option<String>,
    pub branch: Option<String>,
    pub cache_warming: Option<bool>,
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

// The refusals that more than one site answers, each with the status the protocol pairs with its code.
const AGENT_LIMIT: EventError = err("agent_limit", 429);
pub(crate) const INVALID_EVENT: EventError = err("invalid_event", 400);
const INVALID_STATE: EventError = err("invalid_state", 400);
const STALE_SEQUENCE: EventError = err("stale_sequence", 409);
const UNKNOWN_INSTANCE: EventError = err("unknown_instance", 409);

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
            &[
                ("cache_warming", true),
                ("previous_instance_id", false),
                ("reported_facts", true),
            ],
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
    /// The body's last `protocol`, held apart from `object`, whose typed read as an `Event` refuses the member.
    protocol: Option<serde_json::Value>,
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
                    protocol: None,
                    protocol_repeated: false,
                    member_repeated: false,
                };
                while let Some((key, value)) = map.next_entry::<String, serde_json::Value>()? {
                    if key == "protocol" {
                        body.protocol_repeated |= body.protocol.replace(value).is_some();
                    } else {
                        body.member_repeated |= body.object.insert(key, value).is_some();
                    }
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
        /// What the session's own option says of its cache warming, `false` where it holds the warming off.
        cache_warming: bool,
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
    pub(crate) fn agent_id(&self) -> Option<&str> {
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
            return Err(INVALID_EVENT);
        }
        // A word that names no kind is refused rather than skipped, so a misspelled kind never reads as one the list
        // leaves out, which would drop its stored facts.
        let reported_facts = event
            .reported_facts
            .iter()
            .flatten()
            .map(|word| FactsKind::parse(word).ok_or(INVALID_EVENT))
            .collect::<Result<BTreeSet<FactsKind>, EventError>>()?;
        let presence = match event.kind.as_deref() {
            None | Some("prompt") => false,
            Some("presence") => true,
            Some(_) => return Err(INVALID_STATE),
        };
        let (state, park) = match event.state.as_deref() {
            None => (None, None),
            Some(word) => {
                let state = AgentState::parse(word).ok_or(INVALID_STATE)?;
                let park = if state == AgentState::Parked {
                    let kind = event.park_kind.as_deref().ok_or(INVALID_EVENT)?;
                    let kind = ParkKind::parse(kind).ok_or(INVALID_STATE)?;
                    let park = Park::new(kind, event.deadline, event.wake_at);
                    if (kind.scheduled() && event.wake_at.is_none())
                        || park.deadline() != event.deadline
                        || park.wake_at() != event.wake_at
                    {
                        return Err(INVALID_EVENT);
                    }
                    Some(park)
                } else if event.park_kind.is_some()
                    || event.wake_at.is_some()
                    || event.deadline.is_some()
                {
                    return Err(INVALID_EVENT);
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
            cache_warming,
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
        let event = match event_kind {
            EventKind::Activity => Reported::Activity {
                agent_id: agent_id.ok_or(INVALID_EVENT)?,
                presence,
                request_generation,
            },
            EventKind::AgentFacts => Reported::AgentFacts {
                agent_id: agent_id.ok_or(INVALID_EVENT)?,
                facts: AgentFacts::of(RawAgentFacts {
                    agent_type,
                    description,
                    name,
                }),
                request_generation,
            },
            EventKind::AgentStart => Reported::AgentStart {
                agent_id: agent_id.ok_or(INVALID_EVENT)?,
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
                cache_warming: cache_warming.ok_or(INVALID_EVENT)?,
                previous_instance_id,
                reported_facts,
            },
            EventKind::State => {
                let agent_id = agent_id.ok_or(INVALID_EVENT)?;
                let state = state.ok_or(INVALID_EVENT)?;
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
        object,
        protocol,
        protocol_repeated,
        member_repeated,
    }) = serde_json::from_slice(body)
    else {
        return Err(INVALID_EVENT);
    };
    // Two `protocol` members leave no one protocol to refuse.
    if protocol_repeated {
        return Err(INVALID_EVENT);
    }
    // A newer protocol's body may carry members this one does not know, so the protocol is read ahead of every member:
    // the mod has to learn that the protocol is what the router refuses rather than its report.
    match protocol.as_ref().and_then(serde_json::Value::as_u64) {
        Some(PROTOCOL) => {}
        Some(_) => return Err(err("unsupported_protocol", 400)),
        None => return Err(INVALID_EVENT),
    }
    // The object holds the last value of a repeated member, so the typed read below cannot see the repeat.
    if member_repeated {
        return Err(INVALID_EVENT);
    }
    let named = Named::of(&object);
    let event: Event =
        serde_json::from_value(serde_json::Value::Object(object)).map_err(|_| INVALID_EVENT)?;
    EventReport::of(event, &named)
}

fn validate(event: Event) -> Result<Event, EventError> {
    let optional_ok = |value: &Option<String>| value.as_deref().is_none_or(valid_id);
    if !valid_id(&event.session_id)
        || !valid_id(&event.instance_id)
        || event.sequence == 0
        || !event.agent_id.as_deref().is_none_or(is_valid_agent_id)
        || !optional_ok(&event.previous_instance_id)
    {
        return Err(err("invalid_identity", 400));
    }
    Ok(event)
}

/// Each kind of stored facts that a registration's list does not name, read off the store's words.
pub(crate) fn left_out(reported: &BTreeSet<FactsKind>) -> impl Iterator<Item = FactsKind> + '_ {
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

/// What a registration that passed its checks starts: a new session, a session that replaces the one held under its
/// ids, or the same instance's own session again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StartKind {
    NewSession,
    Replacement,
    SameInstance,
}

/// What a registration left of main: the record it kept, one it made, or a record of an ended session that it resumed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MainStart {
    Kept,
    New,
    Revived,
}

/// What a registration's writes left, for the calls of the warming behind it.
pub(crate) struct Started {
    pub main: MainStart,
    /// The same instance turned the session's own cache warming option off.
    pub warming_turned_off: bool,
}

/// Whether an `agent_start` makes a new record or restarts a released one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AgentStartKind {
    New,
    Restart,
}

impl SessionsState {
    /// The checks every report but a registration passes on its session.
    pub(crate) fn check(
        &self,
        key: &SessionKey,
        key_generation: i64,
        report: &EventReport,
    ) -> Result<(), EventError> {
        // A session the previous holder of this key id registered answers as one that was never registered, so
        // a replacement holder learns nothing about it: not its instance, not its sequence, not its state.
        let Some(session) = self.held_by(key, key_generation) else {
            return Err(UNKNOWN_INSTANCE);
        };
        if session.instance != report.instance_id {
            return Err(UNKNOWN_INSTANCE);
        }
        // A lapsed lease reads as an ended instance to the companion, as the Go engine reported it: only a
        // `session_start` can take the session back.
        if session.companion() != CompanionState::Reporting {
            return Err(err("instance_ended", 409));
        }
        if report.sequence <= session.sequence {
            return Err(STALE_SEQUENCE);
        }
        Ok(())
    }

    /// The checks an `activity`, an `agent_facts` or a `state` report passes on the agent it names: a known agent, the
    /// generation the report carries, and a record not released. Answers the agent's state.
    pub(crate) fn check_agent(
        &self,
        key: &SessionKey,
        agent_id: &str,
        request_generation: Option<u64>,
    ) -> Result<AgentState, EventError> {
        let Some(agent) = agent_ref(&self.entries, key, agent_id) else {
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
        Ok(agent.state)
    }

    /// The refusals of a registration, and what it starts where it passes them.
    pub(crate) fn start_check(
        &self,
        key: &SessionKey,
        key_generation: i64,
        report: &EventReport,
        previous_instance_id: Option<&str>,
    ) -> Result<StartKind, EventError> {
        let existing = self.entries.get(key);
        // An ended session keeps its entry until the expiry and holds no slot, and a registered one holds none
        // until the first real request the caps admit, so a session that has sent nothing never fills the count. The
        // bound is met by every start that brings a session to life, an instance replacement included. A
        // same-instance recovery of a live session consumes nothing.
        if existing.is_none_or(|s| s.ended) && !self.session_fits() {
            return Err(err("session_limit", 429));
        }
        // What a previous holder of this key id left under the same session id is not this caller's to
        // re-adopt or to conflict with. The start below replaces it whole, as it replaces an instance.
        if let Some(session) = self.held_by(key, key_generation) {
            if session.instance == report.instance_id {
                // A same-instance start after a lapse re-adopts the session: the mod may only have been silent,
                // and the recipes it left are still the ones its next request needs. One after the session's end
                // revives it: the mod's instance is its process's, so a /clear and then a /resume back to this
                // id registers it again under the same instance, and a refusal would only draw the same start.
                if report.sequence <= session.sequence {
                    return Err(STALE_SEQUENCE);
                }
                let revived = session.ended;
                // A main the revival registers anew takes a slot as any new record does, and a refusal leaves the
                // session as its end left it.
                let registers_main = revived && !session.agents.contains_key(MAIN_AGENT);
                if registers_main && !self.agents_fit(1, 0) {
                    return Err(AGENT_LIMIT);
                }
                return Ok(StartKind::SameInstance);
            }
            if previous_instance_id != Some(session.instance.as_str()) {
                // An ended or silent holder is named so the mod can take over from it, and a live one is not.
                return Err(if session.companion() != CompanionState::Reporting {
                    err_for_instance("instance_conflict", &session.instance)
                } else {
                    err("instance_conflict", 409)
                });
            }
        } else if previous_instance_id.is_some() {
            return Err(err("unknown_previous_instance", 409));
        }
        // The replaced session's own slots, so the subtraction leaves the other sessions' count.
        let held = existing.map_or(0, Session::agent_slots);
        if !self.agents_fit(1, held) {
            return Err(AGENT_LIMIT);
        }
        Ok(if existing.is_some() {
            StartKind::Replacement
        } else {
            StartKind::NewSession
        })
    }

    /// The writes of a same-instance registration that passed its checks: the instance's session renewed.
    pub(crate) fn readopt(
        &mut self,
        key: &SessionKey,
        report: &EventReport,
        cache_warming: bool,
        now: i64,
    ) -> Started {
        self.tail(key, report.sequence, true, now);
        let session = self.entries.get_mut(key).expect("the session just read");
        let revived = session.ended;
        let has_main = session.agents.contains_key(MAIN_AGENT);
        // The registration's own standing outranks the one the load restored.
        session.restored_until = None;
        session.ended = false;
        let turns_warming_off = session.cache_warming && !cache_warming;
        session.cache_warming = cache_warming;
        let mut main = MainStart::Kept;
        if revived {
            // The end released every agent, and a real request arms nothing on a released one, so main
            // starts again as a registration starts it. The subagents stay released until the mod starts
            // them again, and every generation stays as the end left it.
            if has_main {
                self.restart_agent(key, MAIN_AGENT, now);
                main = MainStart::Revived;
            } else {
                // Main's record left one cache TTL after the end, so a resume past it registers main anew as
                // a fresh start does.
                self.add_agent(key, MAIN_AGENT, now);
                main = MainStart::New;
            }
        }
        Started {
            main,
            warming_turned_off: turns_warming_off,
        }
    }

    /// The writes of a registration that passed its checks and starts a new record, with its main, in the place of
    /// whatever stood under the ids.
    pub(crate) fn register(
        &mut self,
        key: &SessionKey,
        key_generation: i64,
        report: &EventReport,
        cache_warming: bool,
        now: i64,
    ) {
        // A previous holder's traffic says nothing about this key's session, so only a start of the same
        // generation inherits whether the session took its slots and when it last carried a real request.
        let (last_real_at, slotted) = self
            .held_by(key, key_generation)
            .map_or((None, false), |s| (s.last_real_at, s.slotted));
        self.entries.insert(
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
                instance_mismatch_warned: false,
                agents: BTreeMap::new(),
                cache_warming,
            },
        );
        self.add_agent(key, MAIN_AGENT, now);
    }

    /// The refusals of an `agent_start`, and whether it makes a new record or restarts a released one.
    pub(crate) fn agent_start_check(
        &self,
        key: &SessionKey,
        agent_id: &str,
        request_generation: Option<u64>,
    ) -> Result<AgentStartKind, EventError> {
        let existing = agent_ref(&self.entries, key, agent_id).map(|a| (a.state, a.generation));
        match existing {
            Some((state, generation)) => {
                if state != AgentState::Released || request_generation != Some(generation) {
                    return Err(err("agent_exists", 409));
                }
                Ok(AgentStartKind::Restart)
            }
            None => {
                if !self.agents_fit(1, 0) {
                    return Err(AGENT_LIMIT);
                }
                Ok(AgentStartKind::New)
            }
        }
    }

    pub(crate) fn restart_agent(&mut self, key: &SessionKey, agent_id: &str, now: i64) {
        agent_mut(&mut self.entries, key, agent_id)
            .expect(AGENT_HELD)
            .resume(now);
    }

    pub(crate) fn add_agent(&mut self, key: &SessionKey, agent_id: &str, now: i64) {
        let incarnation = self.incarnation();
        self.entries.get_mut(key).unwrap().agents.insert(
            agent_id.to_string(),
            Agent::new(agent_id, &key.0, incarnation, now),
        );
    }

    /// `named` is the agent an agent event reports. A session-level event (a heartbeat, a `session_facts`, a
    /// `session_end`, a registration) names none and its answer carries main's generation, which the mod adopts for
    /// main.
    pub(crate) fn acknowledge(&self, key: &SessionKey, named: Option<&str>) -> Ack {
        let session = &self.entries[key];
        let agent = session.agents.get(named.unwrap_or(MAIN_AGENT));
        Ack {
            agent_id: named.and(agent).map(|a| a.id.clone()),
            agents: None,
            instance_id: session.instance.clone(),
            lease_expires_ms: session.lease,
            request_generation: agent.map(|a| a.generation).unwrap_or(0),
            sequence: session.sequence,
            session_id: key.1.clone(),
        }
    }

    /// A registration's answer, which carries the session's agents beside a session-level event's answer.
    pub(crate) fn acknowledge_start(&self, key: &SessionKey) -> Ack {
        Ack {
            agents: Some(agents_of(&self.entries[key])),
            ..self.acknowledge(key, None)
        }
    }
}
