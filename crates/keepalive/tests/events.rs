//! The lifecycle protocol: registration, sequences, generations, leases, and the snapshot the companion
//! reconciles against; the Go events and lifecycle suites.
mod common;
use std::str::FromStr;
use std::time::Duration;

use common::*;
use phoebis_bus::{Phase, ResponseChunk, Usage};
use phoebis_keepalive::{
    decode_query_event, AgentState, Event, EventError, KeepaliveLimits, RecipeState,
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
    let snapshot = rig.snapshot();
    assert!(
        snapshot
            .sessions
            .iter()
            .any(|s| s.session_id == "s-0" && s.ended),
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

fn usage(read: u64, write_5m: u64) -> Usage {
    Usage {
        cache_read_tokens: Some(read),
        cache_write_5m_tokens: Some(write_5m),
        output_tokens: Some(10),
        ..Default::default()
    }
}

#[tokio::test]
async fn query_events_match_the_contract() {
    let mut r = rig(RigSpec::default()).await;
    let ack = r
        .engine
        .apply_event(
            "desktop",
            r.generation("desktop"),
            decode_query_event(
                "event=heartbeat&session_id=session&instance_id=instance&sequence=99",
            )
            .unwrap(),
        )
        .unwrap();
    assert_eq!(
        (
            ack.sequence,
            ack.session_id.as_str(),
            ack.instance_id.as_str(),
            ack.agent_id.as_deref(),
            ack.request_generation
        ),
        (99, "session", "instance", Some("main"), 0)
    );
    assert!(
        ack.lease_expires.ends_with('Z')
            && jiff::Timestamp::from_str(&ack.lease_expires)
                .unwrap()
                .as_millisecond()
                == r.now() + r.lease_ms(),
        "{}",
        ack.lease_expires
    );
    r.sequence = 99;
    for (name, query) in [
        (
            "unknown parameter",
            "event=heartbeat&session_id=session&instance_id=instance&sequence=100&surprise=1",
        ),
        (
            "missing sequence",
            "event=heartbeat&session_id=session&instance_id=instance",
        ),
        (
            "negative sequence",
            "event=heartbeat&session_id=session&instance_id=instance&sequence=-1",
        ),
        (
            "repeated parameter",
            "event=heartbeat&session_id=session&instance_id=instance&sequence=102&sequence=103",
        ),
        (
            "owner outside session_start",
            "event=heartbeat&session_id=session&instance_id=instance&sequence=104&owner_id=o",
        ),
    ] {
        assert_eq!(
            decode_query_event(query).unwrap_err(),
            EventError {
                code: "invalid_event",
                status: 400
            },
            "{name}"
        );
    }
    assert_eq!(
        decode_query_event(
            "event=heartbeat&session_id=bad%20session&instance_id=instance&sequence=101"
        )
        .unwrap_err(),
        EventError {
            code: "invalid_identity",
            status: 400
        }
    );
    assert_eq!(
        decode_query_event("event=heartbeat&session_id=session&instance_id=instance&sequence=0")
            .unwrap_err(),
        EventError {
            code: "invalid_identity",
            status: 400
        }
    );
    let oversized = format!(
        "event=heartbeat&session_id=session&instance_id=instance&sequence=104&agent_id={}",
        "a".repeat(17 << 10)
    );
    assert_eq!(
        decode_query_event(&oversized).unwrap_err(),
        EventError {
            code: "event_too_large",
            status: 413
        }
    );
    let start = decode_query_event("event=session_start&session_id=s2&instance_id=i2&sequence=1&owner_id=owner-1&previous_instance_id=i1").unwrap();
    assert_eq!(
        (
            start.owner_id.as_deref(),
            start.previous_instance_id.as_deref()
        ),
        (Some("owner-1"), Some("i1"))
    );
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
        EventError {
            code: "unknown_event",
            status: 400
        }
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
        EventError {
            code: "invalid_state",
            status: 400
        }
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
    assert_eq!(
        stale.unwrap_err(),
        EventError {
            code: "stale_sequence",
            status: 409
        }
    );
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
    assert_eq!(
        other.unwrap_err(),
        EventError {
            code: "unknown_instance",
            status: 409
        }
    );
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
        EventError {
            code: "instance_conflict",
            status: 409
        },
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
        EventError {
            code: "unknown_previous_instance",
            status: 409
        }
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
    assert_eq!(
        jiff::Timestamp::from_str(&renewed.lease_expires)
            .unwrap()
            .as_millisecond(),
        r.now() + r.lease_ms()
    );
    assert_eq!(r.agent("worker").state, AgentState::Parked);
    // The lease renews on a heartbeat or a same-instance session_start only; an event about an agent
    // leaves the companion's own lease alone.
    r.advance(1_000);
    let untouched = r
        .event(Event {
            event: "activity".into(),
            agent_id: Some("worker".into()),
            request_generation: Some(r.agent("worker").request_generation),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(untouched.lease_expires, renewed.lease_expires);
    let reheartbeat = r
        .event(Event {
            event: "heartbeat".into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        jiff::Timestamp::from_str(&reheartbeat.lease_expires)
            .unwrap()
            .as_millisecond(),
        r.now() + r.lease_ms()
    );
    // Agents: a restart needs the released generation; activity and state need the current one.
    assert_eq!(
        r.event(Event {
            event: "agent_start".into(),
            agent_id: Some("worker".into()),
            ..Default::default()
        })
        .unwrap_err(),
        EventError {
            code: "agent_exists",
            status: 409
        }
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
        EventError {
            code: "unknown_agent",
            status: 409
        }
    );
    assert_eq!(
        r.event(Event {
            event: "activity".into(),
            agent_id: Some("worker".into()),
            request_generation: Some(7),
            ..Default::default()
        })
        .unwrap_err(),
        EventError {
            code: "stale_request_generation",
            status: 409
        }
    );
    r.state("worker", "released");
    assert_eq!(
        r.event(Event {
            event: "activity".into(),
            agent_id: Some("worker".into()),
            request_generation: Some(r.agent("worker").request_generation),
            ..Default::default()
        })
        .unwrap_err(),
        EventError {
            code: "agent_released",
            status: 409
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
        EventError {
            code: "agent_exists",
            status: 409
        },
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
    let snapshot = r.engine.snapshot("desktop", r.generation("desktop"));
    assert_eq!(snapshot.sessions[0].sequence, r.sequence);
    // session_end releases every agent and ends the instance; anything after it is refused.
    r.event(Event {
        event: "session_end".into(),
        ..Default::default()
    })
    .unwrap();
    assert!(
        r.engine
            .snapshot("desktop", r.generation("desktop"))
            .sessions[0]
            .ended
    );
    assert!(r
        .engine
        .snapshot("desktop", r.generation("desktop"))
        .sessions[0]
        .agents
        .iter()
        .all(|a| a.state == AgentState::Released));
    assert_eq!(
        r.event(Event {
            event: "heartbeat".into(),
            ..Default::default()
        })
        .unwrap_err(),
        EventError {
            code: "instance_ended",
            status: 409
        }
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
            r.engine
                .snapshot("desktop", r.generation("desktop"))
                .sessions[0]
                .agents
                .len()
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
    r.engine
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
    assert_eq!(
        r.engine
            .snapshot("desktop", r.generation("desktop"))
            .sessions
            .len(),
        1
    );
    assert_eq!(
        r.engine.snapshot("laptop", r.generation("laptop")).sessions[0].instance_id,
        "elsewhere"
    );
    assert_eq!(
        r.engine
            .snapshot("nobody", r.generation("nobody"))
            .sessions
            .len(),
        0
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
        EventError {
            code: "session_limit",
            status: 429
        }
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
        EventError {
            code: "agent_limit",
            status: 429
        },
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
        (armed.len(), armed[0].state, armed[0].reason),
        (1, RecipeState::Armed, None)
    );
    r.advance(90_000);
    r.engine.expire();
    let session = &r
        .engine
        .snapshot("desktop", r.generation("desktop"))
        .sessions[0];
    assert!(
        session.ended,
        "the companion sees an ended instance and re-registers"
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
        EventError {
            code: "instance_ended",
            status: 409
        }
    );
    let again = r
        .event(Event {
            event: "session_start".into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(again.instance_id, "instance");
    assert!(
        !r.engine
            .snapshot("desktop", r.generation("desktop"))
            .sessions[0]
            .ended,
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

#[tokio::test]
async fn the_snapshot_carries_only_what_the_companion_reconciles() {
    let mut r = rig(RigSpec::default()).await;
    r.event(Event {
        event: "agent_start".into(),
        agent_id: Some("worker".into()),
        ..Default::default()
    })
    .unwrap();
    let encoded =
        serde_json::to_string(&r.engine.snapshot("desktop", r.generation("desktop"))).unwrap();
    for forbidden in [
        "account",
        "usage",
        "reason",
        "probe",
        "next_refresh",
        "migrations",
        "idle_ticks",
        "cache_ttl",
        "private prompt",
        "owner",
    ] {
        assert!(
            !encoded.contains(forbidden),
            "snapshot leaks {forbidden}: {encoded}"
        );
    }
    let snapshot = r.engine.snapshot("desktop", r.generation("desktop"));
    assert_eq!(
        (
            snapshot.sessions.len(),
            snapshot.sessions[0].session_id.as_str(),
            snapshot.sessions[0].instance_id.as_str(),
            snapshot.sessions[0].ended
        ),
        (1, "session", "instance", false)
    );
    let agents = &snapshot.sessions[0].agents;
    assert_eq!(
        agents
            .iter()
            .map(|a| (a.agent_id.as_str(), a.state))
            .collect::<Vec<_>>(),
        [("main", AgentState::Busy), ("worker", AgentState::Busy)]
    );
}

#[tokio::test]
async fn the_session_expiry_sweep_removes_a_silent_session_and_close_quiesces_the_engine() {
    let r = rig(RigSpec {
        session_expiry: Duration::from_millis(1),
        ..RigSpec::default()
    })
    .await;
    r.advance(2);
    r.engine.expire();
    assert_eq!((r.engine.stats().sessions, r.engine.stats().agents), (0, 0));
    assert_eq!(
        r.engine
            .reasons()
            .get(&phoebis_keepalive::Reason::SessionExpired)
            .copied(),
        Some(1)
    );

    let mut r2 = rig(RigSpec::default()).await;
    r2.engine.observe(r2.observed("req", "main", BODY));
    r2.engine.observe(r2.selected("req", "a"));
    r2.engine
        .observe(phoebis_bus::Event::ResponseChunk(ResponseChunk {
            request_id: "req".into(),
            attempt: 1,
            index: 0,
            body: Default::default(),
            phase: Some(Phase::MessageStart {
                usage: usage(2_000, 0),
            }),
        }));
    let before = r2.engine.stats();
    assert!(
        before.agents == 1 && before.retained_bytes > 0,
        "the rig retains the request before the close: {before:?}"
    );
    r2.engine.close();
    assert_eq!(
        r2.event(Event {
            event: "heartbeat".into(),
            ..Default::default()
        })
        .unwrap_err(),
        EventError {
            code: "quiesced",
            status: 503
        }
    );
    assert_eq!(
        (r2.engine.stats().agents, r2.engine.stats().retained_bytes),
        (0, 0)
    );
}
