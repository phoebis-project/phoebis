//! The lifecycle channel: the companion's reports and the session expiry, as the alert inputs read them.
mod common;
use std::time::Duration;

use common::*;
use phoebis_bus::{lifecycle_channel, LifecycleReceiver, SessionLifecycle};
use phoebis_keepalive::Event;

const SESSION: &str = "session";
const KEY: &str = "desktop";

fn drain(rx: &mut LifecycleReceiver) -> Vec<SessionLifecycle> {
    std::iter::from_fn(|| rx.try_recv().ok()).collect()
}

fn parked() -> SessionLifecycle {
    SessionLifecycle::MainParked {
        key_id: KEY.into(),
        session_id: SESSION.into(),
    }
}

fn resumed() -> SessionLifecycle {
    SessionLifecycle::MainResumed {
        key_id: KEY.into(),
        session_id: SESSION.into(),
    }
}

fn started() -> SessionLifecycle {
    SessionLifecycle::SessionStarted {
        key_id: KEY.into(),
        session_id: SESSION.into(),
    }
}

fn ended() -> SessionLifecycle {
    SessionLifecycle::SessionEnded {
        key_id: KEY.into(),
        session_id: SESSION.into(),
    }
}

fn agent(id: &str, started: bool) -> SessionLifecycle {
    if started {
        SessionLifecycle::AgentStarted {
            key_id: KEY.into(),
            session_id: SESSION.into(),
            agent_id: id.into(),
        }
    } else {
        SessionLifecycle::AgentReleased {
            key_id: KEY.into(),
            session_id: SESSION.into(),
            agent_id: id.into(),
        }
    }
}

#[tokio::test]
async fn the_companions_reports_become_the_lifecycle_events_the_alerts_read() {
    let (tx, mut rx) = lifecycle_channel();
    let mut r = rig(RigSpec {
        lifecycle: Some(tx),
        ..RigSpec::default()
    })
    .await;
    assert_eq!(drain(&mut rx), [started()]);
    r.observe("main").await;
    assert_eq!(
        drain(&mut rx),
        [],
        "a request of the session says nothing here: the request side is the alerts' own"
    );
    r.event(Event {
        event: "agent_start".into(),
        agent_id: Some("sub-1".into()),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(drain(&mut rx), [agent("sub-1", true)]);
    r.state("sub-1", "released");
    assert_eq!(drain(&mut rx), [agent("sub-1", false)]);
    r.state("main", "parked");
    assert_eq!(drain(&mut rx), [parked()]);
    r.state("main", "parked");
    assert_eq!(
        drain(&mut rx),
        [],
        "a report of the state the agent is already in is no transition"
    );
    r.event(Event {
        event: "activity".into(),
        agent_id: Some("main".into()),
        request_generation: Some(r.agent("main").request_generation),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(drain(&mut rx), [resumed()]);
    r.event(Event {
        event: "session_end".into(),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(drain(&mut rx), [ended()]);
    // The entry lives on past the report so a late result still finds it; the end was already told.
    r.advance(86_401_000);
    r.engine.expire();
    assert_eq!(
        drain(&mut rx),
        [],
        "a session the companion ended is not ended a second time by its expiry"
    );
}

#[tokio::test]
async fn a_main_agent_released_while_parked_leaves_the_parked_state() {
    let (tx, mut rx) = lifecycle_channel();
    let mut r = rig(RigSpec {
        lifecycle: Some(tx),
        ..RigSpec::default()
    })
    .await;
    assert_eq!(drain(&mut rx), [started()]);
    r.state("main", "parked");
    assert_eq!(drain(&mut rx), [parked()]);
    r.state("main", "released");
    assert_eq!(
        drain(&mut rx),
        [resumed()],
        "leaving parked in any direction ends the parked condition"
    );
}

#[tokio::test]
async fn an_instance_replacement_releases_its_agents_and_ends_its_parked_state() {
    let (tx, mut rx) = lifecycle_channel();
    let mut r = rig(RigSpec {
        lifecycle: Some(tx),
        ..RigSpec::default()
    })
    .await;
    assert_eq!(drain(&mut rx), [started()]);
    r.event(Event {
        event: "agent_start".into(),
        agent_id: Some("sub-1".into()),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(drain(&mut rx), [agent("sub-1", true)]);
    r.state("main", "parked");
    assert_eq!(drain(&mut rx), [parked()]);
    r.sequence += 1;
    let sequence = r.sequence;
    r.engine
        .apply_event(
            KEY,
            r.generation(KEY),
            Event {
                event: "session_start".into(),
                instance_id: "instance-2".into(),
                previous_instance_id: Some("instance".into()),
                session_id: SESSION.into(),
                sequence,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        drain(&mut rx),
        [agent("sub-1", false), resumed(), started()],
        "the replacement releases the agents it discarded and ends the parked state it inherited"
    );
}

#[tokio::test]
async fn a_forgotten_session_ends_at_the_expiry() {
    let (tx, mut rx) = lifecycle_channel();
    let r = rig(RigSpec {
        lifecycle: Some(tx),
        session_expiry: Duration::from_secs(3_600),
        ..RigSpec::default()
    })
    .await;
    assert_eq!(drain(&mut rx), [started()]);
    r.advance(3_599_000);
    r.engine.expire();
    assert_eq!(drain(&mut rx), [], "the session is still remembered");
    r.advance(2_000);
    r.engine.expire();
    assert_eq!(drain(&mut rx), [ended()]);
}

#[tokio::test]
async fn a_close_ends_every_live_session_once_and_an_announced_one_not_again() {
    let (tx, mut rx) = lifecycle_channel();
    let mut r = rig(RigSpec {
        lifecycle: Some(tx),
        ..RigSpec::default()
    })
    .await;
    // The rig's own session is live; a second one is started and ended through the companion's reports, so
    // the close finds one session to end and one already announced.
    r.event(Event {
        event: "session_end".into(),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        drain(&mut rx),
        [started(), ended()],
        "the rig's session, started then ended"
    );
    r.engine
        .apply_event(
            "desktop",
            r.generation("desktop"),
            Event {
                session_id: "s2".into(),
                instance_id: "i2".into(),
                event: "session_start".into(),
                sequence: 1,
                ..Default::default()
            },
        )
        .unwrap();
    drain(&mut rx);
    r.engine.close();
    let ends: Vec<SessionLifecycle> = drain(&mut rx)
        .into_iter()
        .filter(|e| matches!(e, SessionLifecycle::SessionEnded { .. }))
        .collect();
    assert_eq!(
        ends,
        [SessionLifecycle::SessionEnded {
            key_id: KEY.into(),
            session_id: "s2".into(),
        }],
        "the live session ends at the close; the announced one ended before it"
    );
    assert!(
        matches!(
            rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
        ),
        "the close drops the sender too: the alerts consumer's drain ends on that and nothing else"
    );
}

fn heartbeat() -> SessionLifecycle {
    SessionLifecycle::Heartbeat {
        key_id: KEY.into(),
        session_id: SESSION.into(),
    }
}

#[tokio::test]
async fn a_parked_main_agent_that_sends_a_request_resumes_once() {
    let (tx, mut rx) = lifecycle_channel();
    let mut r = rig(RigSpec {
        lifecycle: Some(tx),
        ..RigSpec::default()
    })
    .await;
    assert_eq!(drain(&mut rx), [started()]);
    r.state("main", "parked");
    assert_eq!(drain(&mut rx), [parked()]);
    r.observe("main").await;
    assert_eq!(
        drain(&mut rx),
        [resumed()],
        "the request itself is the session coming back, whatever the companion reports later"
    );
    let generation = r.agent("main").request_generation;
    r.event(Event {
        event: "activity".into(),
        agent_id: Some("main".into()),
        request_generation: Some(generation),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        drain(&mut rx),
        [],
        "the companion's report finds the agent already busy, so it is no second transition"
    );
}

#[tokio::test]
async fn an_accepted_heartbeat_says_the_session_is_still_there() {
    let (tx, mut rx) = lifecycle_channel();
    let mut r = rig(RigSpec {
        lifecycle: Some(tx),
        ..RigSpec::default()
    })
    .await;
    assert_eq!(drain(&mut rx), [started()]);
    r.event(Event {
        event: "heartbeat".into(),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(drain(&mut rx), [heartbeat()]);
    let stale = r.sequence;
    r.engine
        .apply_event(
            KEY,
            r.generation(KEY),
            Event {
                event: "heartbeat".into(),
                instance_id: "instance".into(),
                session_id: SESSION.into(),
                sequence: stale,
                ..Default::default()
            },
        )
        .unwrap_err();
    assert_eq!(
        drain(&mut rx),
        [],
        "a heartbeat the engine refused says nothing about the session"
    );
}

#[tokio::test]
async fn a_deleted_keys_sessions_end_at_the_next_tick() {
    let (tx, mut rx) = lifecycle_channel();
    let mut r = rig(RigSpec {
        lifecycle: Some(tx),
        ..RigSpec::default()
    })
    .await;
    assert_eq!(drain(&mut rx), [started()]);
    r.observe("main").await;
    assert!(
        !r.engine.recipes_for(KEY, SESSION).is_empty(),
        "the request retained a recipe for the session"
    );
    r.h.store.delete_api_key(KEY, "test").unwrap();
    r.h.router.reload().await.unwrap();
    r.step().await;
    assert_eq!(
        drain(&mut rx),
        [ended()],
        "the key is gone, so the session it held ends without waiting for its expiry"
    );
    let stats = r.engine.stats();
    assert_eq!(
        (stats.sessions, stats.agents, stats.retained_bytes),
        (0, 0, 0),
        "and its agents and their recipes go with it"
    );
    assert!(r
        .engine
        .snapshot(KEY, r.generation(KEY))
        .sessions
        .is_empty());
}
