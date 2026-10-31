//! The Go events and lifecycle suites over the reporting protocol: registration, sequences, generations and leases.
mod common;
use common::*;
use phoebis_keepalive::{
    AgentState, CompanionState, Event, EventError, KeepaliveLimits, ParkKind, Reason, RecipeState,
};

#[tokio::test]
async fn the_session_bound_counts_live_sessions_only() {
    let mut rig = rig(RigSpec {
        limits: KeepaliveLimits {
            max_sessions: 128,
            ..RigSpec::default().limits
        },
        ..RigSpec::default()
    })
    .await;
    for i in 0..128 {
        rig.event(session_start(&format!("s-{i}"))).unwrap();
        rig.event(session_end(&format!("s-{i}"))).unwrap();
    }
    let ack = rig.event(session_start("s-128"));
    assert!(ack.is_ok(), "128 ended sessions hold no slot: {ack:?}");
    // The rig's own bootstrap session and "s-128" already hold two of the 128 slots, so the rest of the
    // budget is 126 before the next live-bringing start is refused.
    for i in 0..126 {
        rig.event(session_start(&format!("t-{i}"))).unwrap();
    }
    let refused = rig.event(session_start("t-126")).unwrap_err();
    assert_eq!(refused.code, "session_limit");
    assert!(
        rig.engine
            .sessions()
            .iter()
            .any(|s| s.session_id == "s-0" && s.companion == CompanionState::None),
        "the tombstone is still read"
    );
}

#[tokio::test]
async fn a_restart_of_an_ended_session_under_a_new_instance_takes_a_slot() {
    // The rig's own bootstrap session holds one slot already, so the cap here is two: one for it, one for
    // "a" or "b".
    let mut rig = rig(RigSpec {
        limits: KeepaliveLimits {
            max_sessions: 2,
            ..RigSpec::default().limits
        },
        ..RigSpec::default()
    })
    .await;
    rig.event(session_start_as("a", "i1", None)).unwrap();
    rig.event(session_end("a")).unwrap();
    rig.event(session_start_as("b", "i-b", None)).unwrap();
    // The tombstone's entry exists, so a bound over "adds an entry" would let this through.
    let refused = rig
        .event(session_start_as("a", "i2", Some("i1")))
        .unwrap_err();
    assert_eq!(refused.code, "session_limit");
}

#[tokio::test]
async fn a_same_instance_recovery_of_a_live_session_takes_no_slot() {
    let mut rig = rig(RigSpec {
        limits: KeepaliveLimits {
            max_sessions: 2,
            ..RigSpec::default().limits
        },
        ..RigSpec::default()
    })
    .await;
    rig.event(session_start_as("a", "i1", None)).unwrap();
    rig.advance(rig.lease_ms() + 1_000); // the lease lapsed; the session is live and unattended
    let ack = rig.event(session_start_as("a", "i1", None));
    assert!(
        ack.is_ok(),
        "a live session's own instance re-adopts it: {ack:?}"
    );
}

fn refusal(code: &'static str, status: u16) -> EventError {
    EventError {
        code,
        instance_id: None,
        request_generation: None,
        status,
    }
}

#[tokio::test]
async fn the_engine_refuses_an_unknown_event_word_and_an_unknown_state_word() {
    let r = rig(RigSpec::default()).await;
    assert_eq!(
        r.engine
            .apply_event(
                "desktop",
                r.generation("desktop"),
                Event {
                    event: "bogus".into(),
                    session_id: "session".into(),
                    instance_id: "instance".into(),
                    sequence: 105,
                    ..Default::default()
                }
            )
            .unwrap_err(),
        refusal("unknown_event", 400)
    );
    assert_eq!(
        r.engine
            .apply_event(
                "desktop",
                r.generation("desktop"),
                Event {
                    event: "state".into(),
                    session_id: "session".into(),
                    instance_id: "instance".into(),
                    sequence: 106,
                    state: Some("napping".into()),
                    request_generation: Some(0),
                    ..Default::default()
                }
            )
            .unwrap_err(),
        refusal("invalid_state", 400)
    );
}

#[tokio::test]
async fn registration_sequences_and_generations_follow_the_protocol() {
    let mut r = rig(RigSpec::default()).await;
    let stale = r.engine.apply_event(
        "desktop",
        r.generation("desktop"),
        Event {
            event: "heartbeat".into(),
            session_id: "session".into(),
            instance_id: "instance".into(),
            sequence: 1,
            ..Default::default()
        },
    );
    assert_eq!(stale.unwrap_err(), refusal("stale_sequence", 409));
    let other = r.engine.apply_event(
        "desktop",
        r.generation("desktop"),
        Event {
            event: "heartbeat".into(),
            session_id: "session".into(),
            instance_id: "other".into(),
            sequence: 2,
            ..Default::default()
        },
    );
    assert_eq!(other.unwrap_err(), refusal("unknown_instance", 409));
    let conflict = r.engine.apply_event(
        "desktop",
        r.generation("desktop"),
        Event {
            event: "session_start".into(),
            session_id: "session".into(),
            instance_id: "other".into(),
            sequence: 1,
            ..Default::default()
        },
    );
    assert_eq!(
        conflict.unwrap_err(),
        refusal("instance_conflict", 409),
        "a replacement must name the instance it replaces"
    );
    let orphan = r.engine.apply_event(
        "desktop",
        r.generation("desktop"),
        Event {
            event: "session_start".into(),
            session_id: "fresh".into(),
            instance_id: "i".into(),
            sequence: 1,
            previous_instance_id: Some("gone".into()),
            ..Default::default()
        },
    );
    assert_eq!(
        orphan.unwrap_err(),
        refusal("unknown_previous_instance", 409)
    );
    // A same-instance re-registration with a higher sequence renews the lease and keeps agent state.
    r.event(Event {
        event: "agent_start".into(),
        agent_id: Some("worker".into()),
        ..Default::default()
    })
    .unwrap();
    r.state("worker", "parked");
    r.advance(1_000);
    let renewed = r
        .event(Event {
            event: "session_start".into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(renewed.lease_expires_ms, r.now() + r.lease_ms());
    assert_eq!(
        (r.agent("worker").state, r.agent("worker").park_kind),
        (AgentState::Parked, Some(ParkKind::Background))
    );
    // The lease renews on a heartbeat or a same-instance session_start only; an event about an agent
    // leaves the companion's own lease alone.
    r.advance(1_000);
    let untouched = r
        .event(Event {
            event: "activity".into(),
            agent_id: Some("worker".into()),
            kind: Some("prompt".into()),
            request_generation: Some(r.agent("worker").request_generation),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(untouched.lease_expires_ms, renewed.lease_expires_ms);
    let reheartbeat = r
        .event(Event {
            event: "heartbeat".into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(reheartbeat.lease_expires_ms, r.now() + r.lease_ms());
    // Agents: a restart needs the released generation; activity and state need the current one.
    assert_eq!(
        r.event(Event {
            event: "agent_start".into(),
            agent_id: Some("worker".into()),
            ..Default::default()
        })
        .unwrap_err(),
        refusal("agent_exists", 409)
    );
    assert_eq!(
        r.event(Event {
            event: "state".into(),
            agent_id: Some("ghost".into()),
            state: Some("idle".into()),
            request_generation: Some(0),
            ..Default::default()
        })
        .unwrap_err(),
        refusal("unknown_agent", 409)
    );
    assert_eq!(
        r.event(Event {
            event: "activity".into(),
            agent_id: Some("worker".into()),
            kind: Some("prompt".into()),
            request_generation: Some(7),
            ..Default::default()
        })
        .unwrap_err(),
        EventError {
            request_generation: Some(0),
            ..refusal("stale_request_generation", 409)
        },
        "the refusal names the current generation"
    );
    r.state("worker", "released");
    assert_eq!(
        r.event(Event {
            event: "activity".into(),
            agent_id: Some("worker".into()),
            kind: Some("prompt".into()),
            request_generation: Some(r.agent("worker").request_generation),
            ..Default::default()
        })
        .unwrap_err(),
        EventError {
            request_generation: Some(r.agent("worker").request_generation),
            ..refusal("agent_released", 409)
        }
    );
    let generation = r.agent("worker").request_generation;
    assert_eq!(
        r.event(Event {
            event: "agent_start".into(),
            agent_id: Some("worker".into()),
            ..Default::default()
        })
        .unwrap_err(),
        refusal("agent_exists", 409),
        "a restart without the generation"
    );
    let restarted = r
        .event(Event {
            event: "agent_start".into(),
            agent_id: Some("worker".into()),
            request_generation: Some(generation),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        (restarted.agent_id.as_deref(), r.agent("worker").state),
        (Some("worker"), AgentState::Busy)
    );
    // The failed events did not advance the sequence: the next accepted one is one past the last accepted.
    assert_eq!(restarted.sequence, r.sequence);
    // session_end releases every agent and ends the instance; anything after it is refused.
    r.event(Event {
        event: "session_end".into(),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        (
            r.engine.sessions()[0].companion,
            r.engine.sessions()[0].agents
        ),
        (CompanionState::None, 2)
    );
    assert!(["main", "worker"]
        .iter()
        .all(|id| r.agent(id).state == AgentState::Released));
    assert_eq!(
        r.event(Event {
            event: "heartbeat".into(),
            ..Default::default()
        })
        .unwrap_err(),
        refusal("instance_ended", 409)
    );
    // A replacement that names the ended instance takes over with a fresh main agent.
    let replaced = r
        .engine
        .apply_event(
            "desktop",
            r.generation("desktop"),
            Event {
                event: "session_start".into(),
                session_id: "session".into(),
                instance_id: "next".into(),
                sequence: 1,
                previous_instance_id: Some("instance".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        (
            replaced.instance_id.as_str(),
            replaced.request_generation,
            r.engine.sessions()[0].agents
        ),
        ("next", 0, 1)
    );
}

#[tokio::test]
async fn sessions_are_scoped_to_the_key_and_bounded_process_wide() {
    let mut r = rig(RigSpec {
        limits: KeepaliveLimits {
            max_sessions: 2,
            max_agents: 3,
            ..KeepaliveLimits::default()
        },
        ..RigSpec::default()
    })
    .await;
    // The same session id under another key is another session: neither key sees the other's.
    let laptop = r
        .engine
        .apply_event(
            "laptop",
            r.generation("laptop"),
            Event {
                event: "session_start".into(),
                session_id: "session".into(),
                instance_id: "elsewhere".into(),
                sequence: 1,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(laptop.instance_id, "elsewhere");
    assert_eq!(
        r.engine
            .sessions()
            .iter()
            .map(|s| (s.key_id.as_str(), s.session_id.as_str()))
            .collect::<Vec<_>>(),
        [("desktop", "session"), ("laptop", "session")]
    );
    assert_eq!(
        r.engine
            .apply_event(
                "desktop",
                r.generation("desktop"),
                Event {
                    event: "session_start".into(),
                    session_id: "third".into(),
                    instance_id: "i3".into(),
                    sequence: 1,
                    ..Default::default()
                }
            )
            .unwrap_err(),
        refusal("session_limit", 429)
    );
    r.event(Event {
        event: "agent_start".into(),
        agent_id: Some("w1".into()),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        r.event(Event {
            event: "agent_start".into(),
            agent_id: Some("w2".into()),
            ..Default::default()
        })
        .unwrap_err(),
        refusal("agent_limit", 429),
        "three agents across two sessions"
    );
    assert_eq!(r.engine.stats().sessions, 2);
    assert_eq!(r.engine.stats().agents, 3);
}

#[tokio::test]
async fn a_request_of_a_later_generation_arms_nothing_on_the_previous_holders_session() {
    let r = rig(RigSpec::default()).await;
    let before = r.status("main").reason;
    let mut observed = r.observed("r1", "main", BODY);
    if let phoebis_bus::Event::RequestObserved(request) = &mut observed {
        request.key_generation += 1;
    }
    r.engine.observe(observed);
    assert!(
        r.engine.recipes_for("desktop", "session").is_empty(),
        "a key id created again finds the previous holder's session, and arms nothing on it"
    );
    assert_eq!(
        (
            r.engine.stats().retained_bytes,
            r.agent("main").state,
            r.status("main").reason
        ),
        (0, AgentState::Busy, before),
        "and leaves the agent it names exactly as it was"
    );
}

#[tokio::test]
async fn a_lapsed_lease_marks_the_session_unattended_and_a_same_instance_start_re_adopts_it() {
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    let armed = r.engine.recipes();
    let reason = r.status("main").reason;
    assert_eq!(
        (armed.len(), armed[0].state, armed[0].cause),
        (1, RecipeState::Retained, Reason::Scheduled)
    );
    r.advance(90_000);
    r.engine.expire();
    assert_eq!(
        r.engine.sessions()[0].companion,
        CompanionState::Silent,
        "the lapse leaves the session unattended, and the mod re-registers"
    );
    // A silent companion says nothing about the recipe: the lapse is counted, and the views keep reading the
    // reason the recipe's own last probe left.
    assert_eq!(r.status("main").reason, reason);
    assert_eq!(r.engine.recipes(), armed);
    assert_eq!(
        r.event(Event {
            event: "heartbeat".into(),
            ..Default::default()
        })
        .unwrap_err(),
        refusal("instance_ended", 409)
    );
    let again = r
        .event(Event {
            event: "session_start".into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(again.instance_id, "instance");
    assert_eq!(
        r.engine.sessions()[0].companion,
        CompanionState::Reporting,
        "the same instance re-adopted the session"
    );
    let counts = r.engine.reasons();
    assert_eq!(
        counts
            .get(&phoebis_keepalive::Reason::LeaseExpired)
            .copied(),
        Some(1)
    );
}
