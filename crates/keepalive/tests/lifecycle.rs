//! The lifecycle channel: the companion's reports and the session expiry, as the alert inputs read them.
mod common;
use std::time::Duration;

use phoebis_bus::{lifecycle_channel, LifecycleReceiver, SessionLifecycle};
use phoebis_keepalive::Event;
use common::*;

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
async fn an_instance_replacement_releases_the_agents_it_discards() {
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
    r.sequence += 1;
    let sequence = r.sequence;
    r.engine
        .apply_event(
            KEY,
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
        [agent("sub-1", false), started()],
        "the agents the replacement discarded are released before the new instance starts"
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
