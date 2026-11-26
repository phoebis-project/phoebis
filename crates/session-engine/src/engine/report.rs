//! A report under the one lock: the session engine's checks and writes and the warming's calls, each arm in the order
//! of its statements, with the dirty marks and the filed sets of the save between them.

use std::collections::BTreeSet;
use std::sync::Arc;

use phoebis_routing::{RegistrySnapshot, Report};
use phoebis_store::FactsKind;

use crate::engine::persist::persist_key_of;
use crate::engine::{Engine, Inner};
use crate::sessions::events::{
    err, left_out, Ack, AgentStartKind, EventError, EventReport, MainStart, Reported, StartKind,
};
use crate::sessions::state::{agent_ref, AgentState, SessionKey, AGENT_HELD, MAIN_AGENT};
use crate::warming::causes::{Reason, Wasted};

/// An accepted report's answer, with whether the report asks for a save at once: it filed a set of facts or the leave
/// of a kind, or it turned the session's cache warming off.
pub(crate) struct Applied {
    pub ack: Ack,
    pub asks_save: bool,
}

impl Engine {
    /// `key_id` and `key_generation` are the authenticated key's, never fields of the event: a session is
    /// scoped to the holder that reports it, and no member of the body can name another.
    pub(crate) fn apply_event(
        self: &Arc<Self>,
        key_id: &str,
        key_generation: i64,
        report: EventReport,
    ) -> Result<Ack, EventError> {
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
            inner.sweep(now);
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
        let applied = acked?;
        if applied.asks_save {
            // Not awaited: the answer goes out at once, and the one save worker takes the request from here.
            self.save_soon();
        }
        Ok(applied.ack)
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
            self.sessions.check(&key, key_generation, &report)?;
        }
        let agent_id = report.event.agent_id().unwrap_or(MAIN_AGENT).to_string();
        // Both `session_start` paths in `start`, the same-instance re-adoption and the new registration, set the
        // lease, and among the later reports a heartbeat alone renews it, as the Go engine had it. `activity`,
        // `agent_start` and `state` report the agent, not the companion's own liveness, so they leave it alone.
        let renews_lease = report.event == Reported::Heartbeat;
        let mut filed_facts = false;
        match &report.event {
            Reported::SessionStart {
                cache_warming,
                previous_instance_id,
                reported_facts,
            } => {
                return self.start(
                    key,
                    key_generation,
                    &report,
                    *cache_warming,
                    previous_instance_id.as_deref(),
                    reported_facts,
                    now,
                );
            }
            Reported::Heartbeat => {}
            Reported::SessionFacts { facts } => {
                let row = persist_key_of(&key, key_generation);
                self.file_facts(&row, FactsKind::Project, "", facts.project_value(), now);
                self.file_facts(&row, FactsKind::Title, "", facts.title_value(), now);
                filed_facts = true;
            }
            Reported::SessionEnd => {
                let agents: Vec<String> =
                    self.sessions.entries[&key].agents.keys().cloned().collect();
                for agent_id in &agents {
                    self.warm(|w, s| {
                        w.discard(
                            s,
                            &key,
                            agent_id,
                            Reason::SessionEnded,
                            Some(Wasted::SessionEnded),
                        )
                    });
                    self.sessions
                        .set_state(&key, agent_id, AgentState::Released, now);
                    // A session's end is no return, so main's window ends with it.
                    self.warming
                        .close_idle_window(&self.sessions, &key, agent_id);
                    // The leave counts from the first release, so a record the mod released before the end keeps it.
                    if agent_ref(&self.sessions.entries, &key, agent_id)
                        .expect(AGENT_HELD)
                        .leaves_at
                        .is_none()
                    {
                        self.release_record(&key, agent_id, now, snapshot);
                    }
                }
                self.sessions.end_session(&key, report.sequence, now);
                self.touch_session(&key);
                return Ok(Applied {
                    ack: self.sessions.acknowledge(&key, None),
                    asks_save: false,
                });
            }
            Reported::AgentStart {
                request_generation, ..
            } => match self
                .sessions
                .agent_start_check(&key, &agent_id, *request_generation)?
            {
                AgentStartKind::Restart => {
                    self.warm(|w, s| w.restart(s, &key, &agent_id));
                    self.sessions.restart_agent(&key, &agent_id, now);
                    // The resume moves the idle window's start on purpose, so the floor the release left goes.
                    self.warming.clear_idle_floor(&key, &agent_id);
                }
                AgentStartKind::New => {
                    self.sessions.add_agent(&key, &agent_id, now);
                    self.warm(|w, s| w.admit(s, &key, &agent_id, now));
                }
            },
            Reported::Activity {
                presence,
                request_generation,
                ..
            } => {
                self.sessions
                    .check_agent(&key, &agent_id, *request_generation)?;
                // A person at the terminal says nothing about the agent's work, so presence moves the idle clock
                // alone.
                if *presence {
                    // An open window stays open, since a person at the terminal has not come back to the conversation
                    // until a request says so. Presence moves the next window's start on purpose, so the floor goes: a
                    // clock that stepped back would otherwise leave that start behind the last close.
                    self.warming.clear_idle_floor(&key, &agent_id);
                    self.sessions.presence_seen(&key, &agent_id, now);
                } else {
                    self.sessions.prompted(&key, &agent_id, now);
                    // The request this prompt announces is judged at the window's age now, however many ticks
                    // pass before it arrives. The window's start stays where it is.
                    self.warming.prompt(&key, &agent_id, Some(now));
                }
            }
            Reported::AgentFacts {
                facts,
                request_generation,
                ..
            } => {
                // Display state alone: no state, no park and no idle clock moves, so a facts report never reads as
                // activity.
                self.sessions
                    .check_agent(&key, &agent_id, *request_generation)?;
                let row = persist_key_of(&key, key_generation);
                self.file_facts(&row, FactsKind::Agents, &agent_id, facts.value(), now);
                filed_facts = true;
            }
            Reported::Parked {
                park,
                request_generation,
                ..
            } => {
                self.sessions
                    .check_agent(&key, &agent_id, *request_generation)?;
                self.sessions.park(&key, &agent_id, *park, now);
            }
            Reported::State {
                request_generation,
                state,
                ..
            } => {
                let was = self
                    .sessions
                    .check_agent(&key, &agent_id, *request_generation)?;
                let state = *state;
                // The mod's heartbeat closes a command past its deadline or sees the wake pass, and reports idle
                // before or after the router's tick does: a park that ended without a request, which restarts the
                // idle window. A request behind a busy report moves the window itself.
                if was == AgentState::Parked && state == AgentState::Idle {
                    self.warming
                        .close_idle_window(&self.sessions, &key, &agent_id);
                    self.warming.clear_idle_floor(&key, &agent_id);
                } else if state == AgentState::Released {
                    // A released main is the end of its session's window, as `session_end` is.
                    self.warming
                        .close_idle_window(&self.sessions, &key, &agent_id);
                } else if state == AgentState::Idle {
                    // A prompt that sent no request, such as a local command: the window goes on.
                    self.warming.prompt(&key, &agent_id, None);
                }
                self.sessions.set_state(&key, &agent_id, state, now);
                match state {
                    AgentState::Released => {
                        self.warm(|w, s| {
                            w.discard(
                                s,
                                &key,
                                &agent_id,
                                Reason::AgentReleased,
                                Some(Wasted::Released),
                            )
                        });
                        self.release_record(&key, &agent_id, now, snapshot);
                    }
                    AgentState::Unknown => {
                        self.warm(|w, s| w.discard(s, &key, &agent_id, Reason::AgentUnknown, None));
                    }
                    _ => {}
                }
            }
        }
        self.sessions.tail(&key, report.sequence, renews_lease, now);
        self.touch_session(&key);
        // A session-level report moves the session alone, and a facts report moves no member of an agent's row, so
        // neither moves the saved instant that ranks the agents at a load.
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
            self.warming
                .open_idle_window(&self.sessions, &key, snapshot);
        }
        Ok(Applied {
            ack: self.sessions.acknowledge(&key, report.event.agent_id()),
            asks_save: filed_facts,
        })
    }

    /// Sets the record's leave one pool cache TTL out from `now`, from the snapshot's pool row, on a record its caller
    /// set released.
    fn release_record(
        &mut self,
        key: &SessionKey,
        agent_id: &str,
        now: i64,
        snapshot: &RegistrySnapshot,
    ) {
        let leaves_at = self.warming.leave_after(key, agent_id, snapshot, now);
        self.sessions.set_leave(key, agent_id, leaves_at);
        self.touch_agent(key, agent_id);
    }

    #[allow(clippy::too_many_arguments)]
    fn start(
        &mut self,
        key: SessionKey,
        key_generation: i64,
        report: &EventReport,
        cache_warming: bool,
        previous_instance_id: Option<&str>,
        reported_facts: &BTreeSet<FactsKind>,
        now: i64,
    ) -> Result<Applied, EventError> {
        let kind = self
            .sessions
            .start_check(&key, key_generation, report, previous_instance_id)?;
        if kind == StartKind::SameInstance {
            let started = self.sessions.readopt(&key, report, cache_warming, now);
            match started.main {
                MainStart::Revived => {
                    // The resume moves the idle window's start on purpose, so the floor the release left goes.
                    self.warming.clear_idle_floor(&key, MAIN_AGENT);
                    self.warm(|w, s| w.await_request(s, &key, MAIN_AGENT));
                }
                MainStart::New => {
                    self.warm(|w, s| w.admit(s, &key, MAIN_AGENT, now));
                }
                MainStart::Kept => {}
            }
            // No tick reads the session's option, so a prompt held from before the turn would go on being warmed.
            if started.warming_turned_off {
                self.warm(|w, s| {
                    w.discard_warming_of(s, &key, Reason::SessionOptedOut, Wasted::Released)
                });
            }
            // An install that turned a kind off would otherwise keep the sets that an earlier run stored.
            let row = persist_key_of(&key, key_generation);
            let mut filed_facts = false;
            for kind in left_out(reported_facts) {
                self.file_kind_leave(&row, kind);
                filed_facts = true;
            }
            self.touch_session(&key);
            return Ok(Applied {
                ack: self.sessions.acknowledge_start(&key),
                // A crash ahead of the periodic save would load the dropped prompts back with the warming on.
                asks_save: filed_facts || started.warming_turned_off,
            });
        }
        if kind == StartKind::Replacement {
            self.warm(|w, s| {
                w.discard_session(s, &key, Reason::InstanceReplaced, Some(Wasted::Replaced))
            });
            // Filed under the generation the replaced rows were saved under: a start under a new generation leaves
            // one removal for the old row and one write for the new, so a load never meets both.
            self.forget_session(&key);
            self.warming.drop_records(&key);
        }
        self.sessions
            .register(&key, key_generation, report, cache_warming, now);
        // The session's own sets start over with the record whatever the list says, and its agents' stored sets stay
        // for the earlier agents that no report states again, unless the list leaves the kind out.
        let row = persist_key_of(&key, key_generation);
        self.file_kind_leave(&row, FactsKind::Project);
        self.file_kind_leave(&row, FactsKind::Title);
        if !reported_facts.contains(&FactsKind::Agents) {
            self.file_kind_leave(&row, FactsKind::Agents);
        }
        self.touch_session(&key);
        self.warm(|w, s| w.admit(s, &key, MAIN_AGENT, now));
        Ok(Applied {
            ack: self.sessions.acknowledge_start(&key),
            asks_save: true,
        })
    }
}
