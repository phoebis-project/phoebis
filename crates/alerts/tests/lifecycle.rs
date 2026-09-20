//! The session lifecycle kinds: what a request on the bus opens, what the companion's reports open and close,
//! the idle rules and their thresholds, the expiry sweep, and what a restart leaves behind.
mod common;
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::time::Duration;

use phoebis_alerts::{
    agent_subject, question_subject, AlertInputs, BusArm, Kind, LifecycleArm, Poller, ScopeType,
};
use phoebis_bus::{Event, Kind as RequestKind, Origin, Outcome, RequestObserved, SessionLifecycle};
use phoebis_store::{AlertEventRow, AlertRuleRow};
use common::*;

const KEY: &str = "desktop";

/// A request of `agent`, with the parent the client sent for a subagent's turn and a receipt time the fence
/// reads.
fn turn(id: &str, session: &str, agent: &str, parent: Option<&str>, received_at: i64) -> Event {
    match observed(id, KEY, "main", session, 0) {
        Event::RequestObserved(o) => Event::RequestObserved(RequestObserved {
            agent_id: Some(agent.into()),
            parent_agent_id: parent.map(String::from),
            received_at,
            ..o
        }),
        other => other,
    }
}

/// The session's own turn: the top-level agent, no parent.
fn main_turn(id: &str, session: &str) -> Event {
    turn(id, session, "main", None, NOW)
}

fn idle_rule(id: &str, minutes: f64, scope: ScopeType, scope_id: Option<&str>) -> AlertRuleRow {
    AlertRuleRow {
        threshold: Some(minutes),
        ..rule(id, Kind::SessionIdle, scope, scope_id)
    }
}

/// One rule of each kind on the desktop key, the idle one at half an hour.
fn lifecycle_rules() -> Vec<AlertRuleRow> {
    vec![
        rule("active", Kind::SessionActive, ScopeType::Key, Some(KEY)),
        idle_rule("idle", 30.0, ScopeType::Key, Some(KEY)),
        rule("parked", Kind::SessionParked, ScopeType::Key, Some(KEY)),
        rule("sub", Kind::SubagentActive, ScopeType::Key, Some(KEY)),
    ]
}

fn install(rig: &Rig, rows: &[AlertRuleRow]) {
    for row in rows {
        rig.store.upsert_alert_rule(row, None, "test").unwrap();
    }
    rig.alerts.reload().unwrap();
}

/// The open events as kind and subject, sorted: two events opened on the same millisecond are ordered by their
/// random ids, which is no order a test may assert.
fn open(rig: &Rig) -> Vec<(String, String)> {
    let mut open: Vec<(String, String)> = rig
        .store
        .open_alert_events()
        .unwrap()
        .into_iter()
        .map(|e| (e.kind, e.subject))
        .collect();
    open.sort();
    open
}

fn detail_of(rig: &Rig, kind: Kind) -> serde_json::Value {
    let event = rig
        .store
        .open_alert_events()
        .unwrap()
        .into_iter()
        .find(|e| e.kind == kind.as_str())
        .unwrap_or_else(|| panic!("no open {}", kind.as_str()));
    serde_json::from_str(&event.detail_json.unwrap()).unwrap()
}

/// Every delivery as kind, subject, and state, sorted for the same reason the open events are.
fn posted(rig: &Rig) -> Vec<(String, String, String)> {
    let mut posts: Vec<(String, String, String)> = rig
        .webhook
        .posts()
        .into_iter()
        .map(|p| {
            (
                p.body["kind"].as_str().unwrap().to_string(),
                p.body["subject"].as_str().unwrap().to_string(),
                p.body["state"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    posts.sort();
    posts
}

fn started(session: &str) -> SessionLifecycle {
    SessionLifecycle::SessionStarted {
        key_id: KEY.into(),
        session_id: session.into(),
    }
}

#[tokio::test]
async fn a_sessions_first_request_opens_session_active_and_a_subagents_opens_subagent_active() {
    let rig = rig().await;
    install(&rig, &lifecycle_rules());
    let mut arm = BusArm::new(rig.alerts.clone(), &inputs(&rig, BTreeMap::new()));
    arm.observe(main_turn("r1", "s1"));
    arm.observe(complete("r1", Outcome::Succeeded, 200, None, None));
    settled(&rig).await;
    assert_eq!(
        open(&rig),
        [("session_active".to_string(), question_subject(KEY, "s1"))]
    );
    assert_eq!(
        detail_of(&rig, Kind::SessionActive),
        serde_json::json!({ "key": KEY, "pool": "main", "session": "s1", "source": "request" })
    );
    arm.observe(turn("r2", "s1", "sub-1", Some("main"), NOW));
    settled(&rig).await;
    assert_eq!(
        open(&rig),
        [
            ("session_active".to_string(), question_subject(KEY, "s1")),
            (
                "subagent_active".to_string(),
                agent_subject(KEY, "s1", "sub-1")
            ),
        ]
    );
    assert_eq!(
        detail_of(&rig, Kind::SubagentActive),
        serde_json::json!({ "agent": "sub-1", "key": KEY, "parent_agent": "main", "pool": "main", "session": "s1" })
    );
    arm.observe(main_turn("r3", "s1"));
    arm.observe(turn("r4", "s1", "sub-1", Some("main"), NOW));
    settled(&rig).await;
    assert_eq!(
        open(&rig).len(),
        2,
        "a second request of either is the same condition still holding"
    );
    assert_eq!(
        posted(&rig).len(),
        2,
        "and posts nothing a receiver has not already been told"
    );
    arm.observe(match main_turn("p1", "s2") {
        Event::RequestObserved(o) => Event::RequestObserved(RequestObserved {
            origin: Origin::Keepalive,
            kind: RequestKind::Probe,
            ..o
        }),
        other => other,
    });
    arm.observe(match main_turn("c1", "s2") {
        Event::RequestObserved(o) => Event::RequestObserved(RequestObserved {
            kind: RequestKind::CountTokens,
            ..o
        }),
        other => other,
    });
    settled(&rig).await;
    assert_eq!(
        open(&rig).len(),
        2,
        "a probe replays a session and a token count precedes a turn: neither is the session in use"
    );
}

#[tokio::test]
async fn the_companion_ends_a_session_releases_an_agent_and_parks_and_resumes_the_main_agent() {
    let rig = rig().await;
    install(&rig, &lifecycle_rules());
    let arm = BusArm::new(rig.alerts.clone(), &inputs(&rig, BTreeMap::new()));
    let mut companion = LifecycleArm::new(rig.alerts.clone(), rig.router.clone(), arm.tracked());
    companion.apply(started("s1"));
    settled(&rig).await;
    assert_eq!(
        open(&rig),
        [("session_active".to_string(), question_subject(KEY, "s1"))]
    );
    assert_eq!(
        detail_of(&rig, Kind::SessionActive),
        serde_json::json!({ "key": KEY, "pool": "main", "session": "s1", "source": "companion" })
    );
    for agent in ["sub-1", "sub-2"] {
        companion.apply(SessionLifecycle::AgentStarted {
            key_id: KEY.into(),
            session_id: "s1".into(),
            agent_id: agent.into(),
        });
    }
    settled(&rig).await;
    assert_eq!(
        detail_of(&rig, Kind::SubagentActive)["parent_agent"],
        serde_json::Value::Null,
        "the companion's report names no parent"
    );
    companion.apply(SessionLifecycle::AgentReleased {
        key_id: KEY.into(),
        session_id: "s1".into(),
        agent_id: "sub-1".into(),
    });
    companion.apply(SessionLifecycle::MainParked {
        key_id: KEY.into(),
        session_id: "s1".into(),
    });
    settled(&rig).await;
    assert_eq!(
        open(&rig),
        [
            ("session_active".to_string(), question_subject(KEY, "s1")),
            ("session_parked".to_string(), question_subject(KEY, "s1")),
            (
                "subagent_active".to_string(),
                agent_subject(KEY, "s1", "sub-2")
            ),
        ],
        "the released agent's event is resolved and the parked main agent's is open"
    );
    companion.apply(SessionLifecycle::MainResumed {
        key_id: KEY.into(),
        session_id: "s1".into(),
    });
    settled(&rig).await;
    assert!(
        !open(&rig).iter().any(|(kind, _)| kind == "session_parked"),
        "leaving parked resolves it"
    );
    companion.apply(SessionLifecycle::SessionEnded {
        key_id: KEY.into(),
        session_id: "s1".into(),
    });
    settled(&rig).await;
    assert_eq!(
        open(&rig),
        [],
        "the session's end resolves every kind of it"
    );
    assert_eq!(
        posted(&rig),
        [
            (
                "session_active".to_string(),
                question_subject(KEY, "s1"),
                "opened".to_string()
            ),
            (
                "session_active".to_string(),
                question_subject(KEY, "s1"),
                "resolved".to_string()
            ),
            (
                "session_parked".to_string(),
                question_subject(KEY, "s1"),
                "opened".to_string()
            ),
            (
                "session_parked".to_string(),
                question_subject(KEY, "s1"),
                "resolved".to_string()
            ),
            (
                "subagent_active".to_string(),
                agent_subject(KEY, "s1", "sub-1"),
                "opened".to_string()
            ),
            (
                "subagent_active".to_string(),
                agent_subject(KEY, "s1", "sub-1"),
                "resolved".to_string()
            ),
            (
                "subagent_active".to_string(),
                agent_subject(KEY, "s1", "sub-2"),
                "opened".to_string()
            ),
            (
                "subagent_active".to_string(),
                agent_subject(KEY, "s1", "sub-2"),
                "resolved".to_string()
            ),
        ],
        "every event was delivered opened and then resolved"
    );
}

#[tokio::test]
async fn session_idle_opens_after_the_threshold_and_resolves_on_the_next_request() {
    let rig = rig().await;
    install(&rig, &lifecycle_rules());
    let mut arm = BusArm::new(rig.alerts.clone(), &inputs(&rig, BTreeMap::new()));
    let mut poller = Poller::new(
        rig.alerts.clone(),
        &inputs(&rig, BTreeMap::new()),
        arm.tracked(),
    );
    arm.observe(main_turn("r1", "s1"));
    settled(&rig).await;
    rig.clock.store(NOW + 29 * 60_000, Ordering::SeqCst);
    poller.tick().await;
    settled(&rig).await;
    assert!(
        !open(&rig).iter().any(|(kind, _)| kind == "session_idle"),
        "a minute short of the threshold is not idle"
    );
    rig.clock.store(NOW + 30 * 60_000, Ordering::SeqCst);
    poller.tick().await;
    settled(&rig).await;
    assert_eq!(
        detail_of(&rig, Kind::SessionIdle),
        serde_json::json!({ "idle_minutes": 30.0, "key": KEY, "last_request_at": NOW, "pool": "main", "session": "s1" })
    );
    rig.clock.store(NOW + 31 * 60_000, Ordering::SeqCst);
    arm.observe(turn("r2", "s1", "sub-1", Some("main"), NOW + 31 * 60_000));
    settled(&rig).await;
    assert!(
        !open(&rig).iter().any(|(kind, _)| kind == "session_idle"),
        "a subagent's request is the session in use"
    );
    rig.clock.store(NOW + 61 * 60_000, Ordering::SeqCst);
    poller.tick().await;
    settled(&rig).await;
    assert_eq!(
        detail_of(&rig, Kind::SessionIdle)["last_request_at"],
        serde_json::json!(NOW + 31 * 60_000),
        "the second idle is measured from the request that cleared the first"
    );
}

#[tokio::test]
async fn idle_rules_open_each_at_their_own_threshold_and_a_disjoint_scope_opens_nothing() {
    let rig = rig().await;
    install(
        &rig,
        &[
            idle_rule("idle30", 30.0, ScopeType::Key, Some(KEY)),
            idle_rule("idle60", 60.0, ScopeType::Key, Some(KEY)),
            idle_rule("other30", 30.0, ScopeType::Pool, Some("other")),
        ],
    );
    // A day's expiry: the sweep at the hour would otherwise retire the session the sixty-minute rule is about
    // to open for.
    let long_lived = || AlertInputs {
        session_expiry: Duration::from_secs(86_400),
        ..inputs(&rig, BTreeMap::new())
    };
    let mut arm = BusArm::new(rig.alerts.clone(), &long_lived());
    let mut poller = Poller::new(rig.alerts.clone(), &long_lived(), arm.tracked());
    arm.observe(main_turn("r1", "s1"));
    settled(&rig).await;
    rig.clock.store(NOW + 30 * 60_000, Ordering::SeqCst);
    poller.tick().await;
    settled(&rig).await;
    let rules_open = |rig: &Rig| {
        let mut ids: Vec<String> = rig
            .store
            .open_alert_events()
            .unwrap()
            .into_iter()
            .map(|e| e.rule_id)
            .collect();
        ids.sort();
        ids
    };
    assert_eq!(rules_open(&rig), ["idle30"]);
    rig.clock.store(NOW + 60 * 60_000, Ordering::SeqCst);
    poller.tick().await;
    settled(&rig).await;
    assert_eq!(
        rules_open(&rig),
        ["idle30", "idle60"],
        "each rule opens at its own threshold, and a pool the key is not on opens nothing"
    );
    rig.clock.store(NOW + 61 * 60_000, Ordering::SeqCst);
    arm.observe(main_turn("r2", "s1"));
    settled(&rig).await;
    assert_eq!(rules_open(&rig), Vec::<String>::new());
    let mut resolved: Vec<String> = rig
        .webhook
        .posts()
        .into_iter()
        .filter(|p| p.body["state"] == "resolved")
        .map(|p| p.body["rule"].as_str().unwrap().to_string())
        .collect();
    resolved.sort();
    assert_eq!(
        resolved,
        ["idle30", "idle60"],
        "the resolved deliveries name each rule once"
    );
}

#[tokio::test]
async fn the_expiry_sweep_resolves_every_lifecycle_event_of_a_forgotten_session() {
    let rig = rig().await;
    install(&rig, &lifecycle_rules());
    let mut arm = BusArm::new(rig.alerts.clone(), &inputs(&rig, BTreeMap::new()));
    let mut poller = Poller::new(
        rig.alerts.clone(),
        &inputs(&rig, BTreeMap::new()),
        arm.tracked(),
    );
    arm.observe(main_turn("r1", "s1"));
    arm.observe(turn("r2", "s1", "sub-1", Some("main"), NOW));
    settled(&rig).await;
    rig.clock.store(NOW + 30 * 60_000, Ordering::SeqCst);
    poller.tick().await;
    settled(&rig).await;
    assert_eq!(open(&rig).len(), 3, "active, its subagent, and idle");
    rig.clock.store(NOW + 3_601_000, Ordering::SeqCst);
    arm.observe(main_turn("r3", "s2"));
    settled(&rig).await;
    assert_eq!(
        open(&rig),
        [("session_active".to_string(), question_subject(KEY, "s2"))],
        "the forgotten session leaves nothing open and the live one is untouched"
    );
}

#[tokio::test]
async fn a_session_with_no_companion_and_no_further_traffic_expires_from_the_poller_alone() {
    let rig = rig().await;
    install(
        &rig,
        &[rule(
            "active",
            Kind::SessionActive,
            ScopeType::Key,
            Some(KEY),
        )],
    );
    let mut arm = BusArm::new(rig.alerts.clone(), &inputs(&rig, BTreeMap::new()));
    let mut poller = Poller::new(
        rig.alerts.clone(),
        &inputs(&rig, BTreeMap::new()),
        arm.tracked(),
    );
    arm.observe(main_turn("r1", "s1"));
    settled(&rig).await;
    rig.clock.store(NOW + 3_599_000, Ordering::SeqCst);
    poller.tick().await;
    settled(&rig).await;
    assert_eq!(
        open(&rig),
        [("session_active".to_string(), question_subject(KEY, "s1"))],
        "a second short of the expiry the session is still live"
    );
    rig.clock.store(NOW + 3_600_000, Ordering::SeqCst);
    poller.tick().await;
    settled(&rig).await;
    assert_eq!(open(&rig), []);
    assert_eq!(
        posted(&rig),
        [
            (
                "session_active".to_string(),
                question_subject(KEY, "s1"),
                "opened".to_string()
            ),
            (
                "session_active".to_string(),
                question_subject(KEY, "s1"),
                "resolved".to_string()
            ),
        ]
    );
}

#[tokio::test]
async fn open_lifecycle_events_from_before_a_restart_are_resolved_at_start_with_the_reason() {
    let rig = rig().await;
    let mut rows = lifecycle_rules();
    rows.push(rule(
        "ask",
        Kind::AskUserQuestion,
        ScopeType::Key,
        Some(KEY),
    ));
    install(&rig, &rows);
    let before = [
        (
            "e1",
            "active",
            Kind::SessionActive,
            question_subject(KEY, "s1"),
        ),
        (
            "e2",
            "sub",
            Kind::SubagentActive,
            agent_subject(KEY, "s1", "sub-1"),
        ),
        ("e3", "idle", Kind::SessionIdle, question_subject(KEY, "s1")),
        (
            "e4",
            "ask",
            Kind::AskUserQuestion,
            question_subject(KEY, "s1"),
        ),
    ];
    for (id, rule_id, kind, subject) in &before {
        rig.store
            .insert_alert_event(&AlertEventRow {
                id: (*id).into(),
                rule_id: (*rule_id).into(),
                kind: kind.as_str().into(),
                subject: subject.clone(),
                state: "open".into(),
                opened_at: NOW,
                resolved_at: None,
                detail_json: Some(serde_json::json!({ "key": KEY, "session": "s1" }).to_string()),
            })
            .unwrap();
    }
    let mut arm = BusArm::new(rig.alerts.clone(), &inputs(&rig, BTreeMap::new()));
    settled(&rig).await;
    assert_eq!(
        open(&rig),
        [("ask_user_question".to_string(), question_subject(KEY, "s1"))],
        "the question is still waiting for the user; the lifecycle events cannot be known to be"
    );
    let posts = rig.webhook.posts();
    assert_eq!(posts.len(), 3, "one resolved delivery per lifecycle event");
    for post in &posts {
        assert_eq!(
            (
                post.body["state"].as_str(),
                post.body["detail"]["reason"].as_str()
            ),
            (Some("resolved"), Some("restart")),
            "the reason reaches the receiver in the resolved delivery"
        );
    }
    arm.observe(main_turn("r1", "s1"));
    settled(&rig).await;
    assert_eq!(
        open(&rig),
        [("session_active".to_string(), question_subject(KEY, "s1"))],
        "a live session opens a fresh event on its next request, which also answers the question"
    );
}

#[tokio::test]
async fn a_request_observed_before_the_sessions_end_but_applied_after_it_reopens_nothing() {
    let rig = rig().await;
    install(&rig, &lifecycle_rules());
    let mut arm = BusArm::new(rig.alerts.clone(), &inputs(&rig, BTreeMap::new()));
    let mut companion = LifecycleArm::new(rig.alerts.clone(), rig.router.clone(), arm.tracked());
    arm.observe(main_turn("r1", "s1"));
    settled(&rig).await;
    rig.clock.store(NOW + 10_000, Ordering::SeqCst);
    companion.apply(SessionLifecycle::SessionEnded {
        key_id: KEY.into(),
        session_id: "s1".into(),
    });
    settled(&rig).await;
    assert_eq!(open(&rig), []);
    arm.observe(turn("r2", "s1", "main", None, NOW + 5_000));
    settled(&rig).await;
    assert_eq!(
        open(&rig),
        [],
        "the request was received before the end the bus consumer applied after it"
    );
    arm.observe(turn("r3", "s1", "main", None, NOW + 20_000));
    settled(&rig).await;
    assert_eq!(
        open(&rig),
        [("session_active".to_string(), question_subject(KEY, "s1"))],
        "a request of the session's own life opens it again"
    );
}
