//! The session lifecycle events: what a request on the bus posts, what the companion's reports post, the idle
//! rules and their thresholds, the expiry sweep, and what a restart and a shutdown leave behind.
mod common;
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use phoebis_alerts::{
    agent_subject, question_subject, spawn_inputs, AlertInputs, BusArm, Kind, LifecycleArm, Poller,
    ScopeType,
};
use phoebis_bus::{
    lifecycle_channel, Event, Kind as RequestKind, Origin, Outcome, RequestObserved,
    SessionLifecycle,
};
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

/// The session's own turn received at a given time, for a request that must clear the terminal fence.
fn turn_at(id: &str, session: &str, received_at: i64) -> Event {
    turn(id, session, "main", None, received_at)
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

/// Every lifecycle delivery as kind, subject, and event, in the order they were posted.
fn posted(rig: &Rig) -> Vec<(String, String, String)> {
    rig.webhook
        .posts()
        .into_iter()
        .map(|p| {
            (
                p.body["kind"].as_str().unwrap().to_string(),
                p.body["subject"].as_str().unwrap().to_string(),
                p.body["detail"]["event"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

/// The detail of the newest delivery that posted `event`.
fn detail_of(rig: &Rig, event: &str) -> serde_json::Value {
    rig.webhook
        .posts()
        .into_iter()
        .rev()
        .find(|p| p.body["detail"]["event"] == event)
        .unwrap_or_else(|| panic!("no {event} delivery"))
        .body["detail"]
        .clone()
}

fn active(session: &str, event: &str) -> (String, String, String) {
    (
        "session_active".into(),
        question_subject(KEY, session),
        event.into(),
    )
}

fn sub(session: &str, agent: &str, event: &str) -> (String, String, String) {
    (
        "subagent_active".into(),
        agent_subject(KEY, session, agent),
        event.into(),
    )
}

fn started(session: &str) -> SessionLifecycle {
    SessionLifecycle::SessionStarted {
        key_id: KEY.into(),
        session_id: session.into(),
    }
}

#[tokio::test]
async fn a_sessions_first_request_posts_session_started_and_a_subagents_posts_subagent_started() {
    let rig = rig().await;
    install(&rig, &lifecycle_rules());
    let mut arm = BusArm::new(rig.alerts.clone(), &inputs(&rig, BTreeMap::new()));
    arm.observe(main_turn("r1", "s1"));
    arm.observe(complete("r1", Outcome::Succeeded, 200, None, None));
    settled(&rig).await;
    assert_eq!(posted(&rig), [active("s1", "session_started")]);
    assert_eq!(
        detail_of(&rig, "session_started"),
        serde_json::json!({ "event": "session_started", "key": KEY, "pool": "main", "session": "s1", "source": "request" })
    );
    arm.observe(turn("r2", "s1", "sub-1", Some("main"), NOW));
    settled(&rig).await;
    assert_eq!(
        posted(&rig),
        [
            active("s1", "session_started"),
            sub("s1", "sub-1", "subagent_started"),
        ]
    );
    assert_eq!(
        detail_of(&rig, "subagent_started")["parent_agent"],
        serde_json::json!("main")
    );
    arm.observe(main_turn("r3", "s1"));
    arm.observe(turn("r4", "s1", "sub-1", Some("main"), NOW));
    settled(&rig).await;
    assert_eq!(
        posted(&rig).len(),
        2,
        "a second request of either is the same session still running"
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
        posted(&rig).len(),
        2,
        "a probe replays a session and a token count precedes a turn: neither is the session in use"
    );
    assert!(
        rig.store.open_alert_events().unwrap().is_empty(),
        "nothing opens"
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
    assert_eq!(posted(&rig), [active("s1", "session_started")]);
    assert_eq!(
        detail_of(&rig, "session_started"),
        serde_json::json!({ "event": "session_started", "key": KEY, "pool": "main", "session": "s1", "source": "companion" })
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
        detail_of(&rig, "subagent_started")["parent_agent"],
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
    companion.apply(SessionLifecycle::MainResumed {
        key_id: KEY.into(),
        session_id: "s1".into(),
    });
    settled(&rig).await;
    companion.apply(SessionLifecycle::SessionEnded {
        key_id: KEY.into(),
        session_id: "s1".into(),
    });
    settled(&rig).await;
    assert_eq!(
        posted(&rig),
        [
            active("s1", "session_started"),
            sub("s1", "sub-1", "subagent_started"),
            sub("s1", "sub-2", "subagent_started"),
            sub("s1", "sub-1", "subagent_ended"),
            (
                "session_parked".to_string(),
                question_subject(KEY, "s1"),
                "session_parked".to_string()
            ),
            (
                "session_parked".to_string(),
                question_subject(KEY, "s1"),
                "session_resumed".to_string()
            ),
            active("s1", "session_ended"),
            sub("s1", "sub-2", "subagent_ended"),
        ]
    );
    for post in rig.webhook.posts() {
        assert_eq!(post.body["state"], "event");
        assert_eq!(post.body["resolved_at"], serde_json::Value::Null);
    }
    assert!(rig.store.open_alert_events().unwrap().is_empty());
}

#[tokio::test]
async fn session_idle_posts_after_the_threshold_and_the_next_request_arms_it_again() {
    let rig = rig().await;
    install(&rig, &lifecycle_rules());
    let mut arm = BusArm::new(rig.alerts.clone(), &inputs(&rig, BTreeMap::new()));
    let mut companion = LifecycleArm::new(rig.alerts.clone(), rig.router.clone(), arm.tracked());
    let mut poller = Poller::new(
        rig.alerts.clone(),
        &inputs(&rig, BTreeMap::new()),
        arm.tracked(),
    );
    let idle_count = |rig: &Rig| {
        posted(rig)
            .iter()
            .filter(|(_, _, e)| e == "session_idle")
            .count()
    };
    arm.observe(main_turn("r1", "s1"));
    settled(&rig).await;
    rig.clock.store(NOW + 29 * 60_000, Ordering::SeqCst);
    poller.tick().await;
    settled(&rig).await;
    assert_eq!(
        idle_count(&rig),
        0,
        "a minute short of the threshold posts nothing"
    );
    // A report about the session is not the session being used, so it moves no idle clock.
    companion.apply(SessionLifecycle::MainParked {
        key_id: KEY.into(),
        session_id: "s1".into(),
    });
    settled(&rig).await;
    assert_eq!(posted(&rig).last().unwrap().2, "session_parked");
    rig.clock.store(NOW + 30 * 60_000, Ordering::SeqCst);
    poller.tick().await;
    settled(&rig).await;
    assert_eq!(
        detail_of(&rig, "session_idle"),
        serde_json::json!({ "event": "session_idle", "idle_minutes": 30.0, "key": KEY, "last_request_at": NOW, "pool": "main", "session": "s1" }),
        "the companion's report a minute earlier postponed nothing"
    );
    rig.clock.store(NOW + 31 * 60_000, Ordering::SeqCst);
    arm.observe(turn("r2", "s1", "sub-1", Some("main"), NOW + 31 * 60_000));
    settled(&rig).await;
    assert_eq!(
        idle_count(&rig),
        1,
        "a subagent's request is the session in use"
    );
    rig.clock.store(NOW + 61 * 60_000, Ordering::SeqCst);
    poller.tick().await;
    settled(&rig).await;
    assert_eq!(idle_count(&rig), 2);
    assert_eq!(
        detail_of(&rig, "session_idle")["last_request_at"],
        serde_json::json!(NOW + 31 * 60_000),
        "the second idle is measured from the request that cleared the first"
    );
}

/// The tick's post rate: a rule that has posted for a session is not dispatched again until that session's
/// next request, which is the crossing the idle set records against a second tick that would otherwise repeat
/// it.
#[tokio::test]
async fn an_idle_rule_posts_once_per_crossing_and_the_next_request_arms_it_again() {
    let rig = rig().await;
    install(&rig, &lifecycle_rules());
    let mut arm = BusArm::new(rig.alerts.clone(), &inputs(&rig, BTreeMap::new()));
    let mut poller = Poller::new(
        rig.alerts.clone(),
        &inputs(&rig, BTreeMap::new()),
        arm.tracked(),
    );
    let idle_count = |rig: &Rig| {
        posted(rig)
            .iter()
            .filter(|(_, _, e)| e == "session_idle")
            .count()
    };
    arm.observe(main_turn("r1", "s1"));
    settled(&rig).await;
    rig.clock.store(NOW + 30 * 60_000, Ordering::SeqCst);
    poller.tick().await;
    settled(&rig).await;
    assert_eq!(idle_count(&rig), 1, "the threshold posts once");
    rig.clock.store(NOW + 31 * 60_000, Ordering::SeqCst);
    poller.tick().await;
    settled(&rig).await;
    assert_eq!(
        idle_count(&rig),
        1,
        "a later tick over the same idle session posts the rule no second time"
    );
    rig.clock.store(NOW + 32 * 60_000, Ordering::SeqCst);
    arm.observe(main_turn("r2", "s1"));
    settled(&rig).await;
    rig.clock.store(NOW + 62 * 60_000, Ordering::SeqCst);
    poller.tick().await;
    settled(&rig).await;
    assert_eq!(
        idle_count(&rig),
        2,
        "the session's next request arms the rule for its next crossing"
    );
}

#[tokio::test]
async fn idle_rules_post_each_at_their_own_threshold_and_a_disjoint_scope_posts_nothing() {
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
    // to post for.
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
    // The rule id, not the subject, is what distinguishes the two thresholds; read it off the raw body.
    let rules_posted = |rig: &Rig| {
        let mut ids: Vec<String> = rig
            .webhook
            .posts()
            .iter()
            .filter(|p| p.body["detail"]["event"] == "session_idle")
            .map(|p| p.body["rule"].as_str().unwrap().to_string())
            .collect();
        ids.sort();
        ids
    };
    assert_eq!(rules_posted(&rig), ["idle30"]);
    rig.clock.store(NOW + 60 * 60_000, Ordering::SeqCst);
    poller.tick().await;
    settled(&rig).await;
    assert_eq!(
        rules_posted(&rig),
        ["idle30", "idle60"],
        "each rule posts at its own threshold, and a pool the key is not on posts nothing"
    );
    rig.clock.store(NOW + 61 * 60_000, Ordering::SeqCst);
    arm.observe(main_turn("r2", "s1"));
    settled(&rig).await;
    assert_eq!(
        rules_posted(&rig),
        ["idle30", "idle60"],
        "the session's next request posts nothing more"
    );
}

#[tokio::test]
async fn the_expiry_sweep_posts_the_ends_of_a_forgotten_session() {
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
    assert_eq!(posted(&rig).len(), 3, "started, subagent started, idle");
    rig.clock.store(NOW + 3_601_000, Ordering::SeqCst);
    arm.observe(main_turn("r3", "s2"));
    settled(&rig).await;
    assert_eq!(
        posted(&rig)[3..].to_vec(),
        vec![
            active("s1", "session_ended"),
            sub("s1", "sub-1", "subagent_ended"),
            active("s2", "session_started"),
        ],
        "the forgotten session's ends post before the live one's start, and the sweep runs before the event is read"
    );
}

#[tokio::test]
async fn an_expired_sessions_ends_post_on_an_event_the_arm_has_no_request_for() {
    let rig = rig().await;
    install(&rig, &lifecycle_rules());
    let mut arm = BusArm::new(rig.alerts.clone(), &inputs(&rig, BTreeMap::new()));
    arm.observe(main_turn("r1", "s1"));
    arm.observe(turn("r2", "s1", "sub-1", Some("main"), NOW));
    settled(&rig).await;
    assert_eq!(
        posted(&rig).len(),
        2,
        "the session and its subagent started"
    );
    rig.clock.store(NOW + 3_601_000, Ordering::SeqCst);
    // A completion of a request the arm never saw: the bus drops under pressure and an in-flight entry ages
    // out, so a completion with nothing to match is the ordinary shape of both.
    arm.observe(complete("ghost", Outcome::Succeeded, 200, None, None));
    settled(&rig).await;
    assert_eq!(
        posted(&rig)[2..].to_vec(),
        vec![
            active("s1", "session_ended"),
            sub("s1", "sub-1", "subagent_ended"),
        ],
        "the sweep's ends post on whatever event carried them"
    );
}

#[tokio::test]
async fn a_session_with_no_companion_and_no_further_traffic_ends_from_the_poller_alone() {
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
        posted(&rig),
        [active("s1", "session_started")],
        "a second short of the expiry the session is still live"
    );
    rig.clock.store(NOW + 3_600_000, Ordering::SeqCst);
    poller.tick().await;
    settled(&rig).await;
    assert_eq!(
        posted(&rig),
        [
            active("s1", "session_started"),
            active("s1", "session_ended")
        ]
    );
    assert_eq!(detail_of(&rig, "session_ended")["source"], "expiry");
}

#[tokio::test]
async fn a_restart_posts_nothing_for_what_a_previous_process_tracked_and_keeps_its_question() {
    let rig = rig().await;
    install(&rig, &lifecycle_rules());
    // The row's rule id must exist for the foreign key, but stays out of the reloaded cache: the question's
    // own age-based resolve, later in this test, then closes the row without a delivery no cached rule is
    // left to receive.
    rig.store
        .upsert_alert_rule(
            &rule("ask", Kind::AskUserQuestion, ScopeType::Key, Some(KEY)),
            None,
            "test",
        )
        .unwrap();
    rig.store
        .insert_alert_event(&AlertEventRow {
            id: "q1".into(),
            rule_id: "ask".into(),
            kind: Kind::AskUserQuestion.as_str().into(),
            subject: question_subject(KEY, "s1"),
            state: "open".into(),
            opened_at: NOW - 10 * 60_000,
            resolved_at: None,
            detail_json: Some(serde_json::json!({ "key": KEY, "session": "s1" }).to_string()),
        })
        .unwrap();
    let question_open = |rig: &Rig| {
        rig.store
            .open_alert_events()
            .unwrap()
            .iter()
            .any(|e| e.kind == Kind::AskUserQuestion.as_str())
    };
    let mut arm = BusArm::new(rig.alerts.clone(), &inputs(&rig, BTreeMap::new()));
    settled(&rig).await;
    assert!(
        question_open(&rig),
        "the question is still waiting for the user; the lifecycle events cannot be known to be"
    );
    assert!(rig.webhook.posts().is_empty());
    let mut poller = Poller::new(
        rig.alerts.clone(),
        &inputs(&rig, BTreeMap::new()),
        arm.tracked(),
    );
    poller.tick().await;
    settled(&rig).await;
    assert!(
        rig.webhook.posts().is_empty(),
        "the restart itself posts nothing"
    );
    assert!(question_open(&rig));
    assert_eq!(arm.tracked_sessions(), 1);
    rig.clock.store(NOW + 31 * 60_000, Ordering::SeqCst);
    poller.tick().await;
    settled(&rig).await;
    assert!(
        posted(&rig)
            .iter()
            .all(|(_, _, event)| event != "session_idle"),
        "this process has seen no request of that session, so it cannot call it idle"
    );
    assert_eq!(arm.tracked_sessions(), 1);
    rig.clock.store(NOW + 61 * 60_000, Ordering::SeqCst);
    poller.tick().await;
    settled(&rig).await;
    assert_eq!(arm.tracked_sessions(), 0, "the entry aged past the expiry");
    assert!(
        posted(&rig).is_empty(),
        "an expiry ends a session this process never announced, so it posts nothing"
    );
    arm.observe(turn_at("r1", "s1", NOW + 61 * 60_000 + 1_000));
    settled(&rig).await;
    assert_eq!(
        posted(&rig),
        [active("s1", "session_started")],
        "a live session posts a fresh start on its next request, which also answers the question"
    );
    assert_eq!(
        detail_of(&rig, "session_started")["pool"],
        serde_json::json!("main"),
        "the request names the pool a pool-scoped rule would have to match"
    );
    assert!(
        !question_open(&rig),
        "the request still answers the question the entry no longer tracks"
    );
}

/// The bus arm and the companion's arm on tasks of their own over one map, as the router runs them: whichever
/// order the two reach the engine in, a session's end leaves no start posted behind it, and a later request of
/// that session still posts its own start rather than meeting a fence nothing will ever lift.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_request_racing_a_sessions_end_posts_the_start_before_the_end_or_nothing() {
    let rig = rig().await;
    install(&rig, &lifecycle_rules());
    let arm = Arc::new(Mutex::new(BusArm::new(
        rig.alerts.clone(),
        &inputs(&rig, BTreeMap::new()),
    )));
    let tracked = arm.lock().unwrap().tracked();
    let companion = Arc::new(Mutex::new(LifecycleArm::new(
        rig.alerts.clone(),
        rig.router.clone(),
        tracked,
    )));
    for i in 0..50 {
        let session = format!("s{i}");
        let request = tokio::spawn({
            let arm = arm.clone();
            let session = session.clone();
            async move {
                arm.lock().unwrap().observe(main_turn("r", &session));
            }
        });
        let end = tokio::spawn({
            let companion = companion.clone();
            async move {
                companion
                    .lock()
                    .unwrap()
                    .apply(SessionLifecycle::SessionEnded {
                        key_id: KEY.into(),
                        session_id: session,
                    });
            }
        });
        let (request, end) = tokio::join!(request, end);
        request.unwrap();
        end.unwrap();
    }
    settled(&rig).await;
    let all_posted = posted(&rig);
    for i in 0..50 {
        let subject = question_subject(KEY, &format!("s{i}"));
        let events: Vec<&str> = all_posted
            .iter()
            .filter(|(_, s, _)| s == &subject)
            .map(|(_, _, e)| e.as_str())
            .collect();
        assert!(
            events == ["session_started", "session_ended"] || events.is_empty(),
            "session s{i}: {events:?}"
        );
    }
    rig.clock.store(NOW + 1_000, Ordering::SeqCst);
    arm.lock()
        .unwrap()
        .observe(turn("later", "s0", "main", None, NOW + 1_000));
    settled(&rig).await;
    let s0_subject = question_subject(KEY, "s0");
    assert_eq!(
        posted(&rig)
            .into_iter()
            .rfind(|(_, s, _)| s == &s0_subject)
            .unwrap()
            .2,
        "session_started",
        "the later request posts a fresh start"
    );
}

#[tokio::test]
async fn a_rule_enabled_while_a_session_runs_receives_its_later_events_and_no_synthetic_start() {
    let rig = rig().await;
    // An enabled rule of the kind, watching another key: the kind being watched somewhere is not this
    // session's event reaching this rule.
    install(
        &rig,
        &[rule(
            "elsewhere",
            Kind::SessionActive,
            ScopeType::Key,
            Some("laptop"),
        )],
    );
    let mut arm = BusArm::new(rig.alerts.clone(), &inputs(&rig, BTreeMap::new()));
    arm.observe(main_turn("r1", "s1"));
    settled(&rig).await;
    assert_eq!(
        posted(&rig),
        [],
        "no rule this session matches is enabled yet"
    );
    install(
        &rig,
        &[rule(
            "active",
            Kind::SessionActive,
            ScopeType::Key,
            Some(KEY),
        )],
    );
    arm.observe(main_turn("r2", "s1"));
    settled(&rig).await;
    assert_eq!(
        posted(&rig),
        [],
        "the session's start was posted to nobody and is not repeated"
    );
    let mut companion = LifecycleArm::new(rig.alerts.clone(), rig.router.clone(), arm.tracked());
    companion.apply(SessionLifecycle::SessionEnded {
        key_id: KEY.into(),
        session_id: "s1".into(),
    });
    settled(&rig).await;
    assert_eq!(posted(&rig), [active("s1", "session_ended")]);
}

#[tokio::test]
async fn a_session_end_enqueued_at_the_close_is_posted_before_the_inputs_stop() {
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
    let (tx, rx) = lifecycle_channel();
    let mut handles = spawn_inputs(
        rig.alerts.clone(),
        AlertInputs {
            lifecycle: Some(rx),
            ..inputs(&rig, BTreeMap::new())
        },
    );
    tx.send(started("s1")).unwrap();
    fake_upstream::wait_for(Duration::from_secs(5), || {
        (posted(&rig) == [active("s1", "session_started")]).then_some(())
    })
    .await;
    // The end is queued and the sender dropped with no await point between them and the shutdown, the way the
    // close leaves the channel: the consumer has not read the end when the teardown starts.
    tx.send(SessionLifecycle::SessionEnded {
        key_id: KEY.into(),
        session_id: "s1".into(),
    })
    .unwrap();
    drop(tx);
    handles.stop();
    handles.drain().await;
    settled(&rig).await;
    assert_eq!(
        posted(&rig),
        [
            active("s1", "session_started"),
            active("s1", "session_ended")
        ],
        "the end the queue held was posted before the consumer stopped"
    );
    assert!(rig.store.alert_events(None, 10).unwrap().is_empty());
}

#[tokio::test]
async fn a_request_observed_before_the_sessions_end_but_applied_after_it_posts_nothing() {
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
    assert_eq!(
        posted(&rig),
        [
            active("s1", "session_started"),
            active("s1", "session_ended")
        ]
    );
    arm.observe(turn("r2", "s1", "main", None, NOW + 5_000));
    settled(&rig).await;
    assert_eq!(
        posted(&rig).len(),
        2,
        "the request was received before the end the bus consumer applied after it"
    );
    arm.observe(turn("r3", "s1", "main", None, NOW + 20_000));
    settled(&rig).await;
    assert_eq!(
        posted(&rig),
        [
            active("s1", "session_started"),
            active("s1", "session_ended"),
            active("s1", "session_started"),
        ],
        "a request of the session's own life posts a fresh start"
    );
}

#[tokio::test]
async fn a_request_of_a_released_agent_applied_after_the_release_posts_nothing() {
    let rig = rig().await;
    install(&rig, &lifecycle_rules());
    let mut arm = BusArm::new(rig.alerts.clone(), &inputs(&rig, BTreeMap::new()));
    let mut companion = LifecycleArm::new(rig.alerts.clone(), rig.router.clone(), arm.tracked());
    let released = SessionLifecycle::AgentReleased {
        key_id: KEY.into(),
        session_id: "s1".into(),
        agent_id: "sub-1".into(),
    };
    arm.observe(main_turn("r1", "s1"));
    arm.observe(turn("r2", "s1", "sub-1", Some("main"), NOW));
    settled(&rig).await;
    assert_eq!(
        posted(&rig),
        [
            active("s1", "session_started"),
            sub("s1", "sub-1", "subagent_started")
        ]
    );
    rig.clock.store(NOW + 10_000, Ordering::SeqCst);
    companion.apply(released.clone());
    settled(&rig).await;
    assert_eq!(
        posted(&rig).last().unwrap(),
        &sub("s1", "sub-1", "subagent_ended")
    );
    // The agent's own earlier request, applied after the release that ended its run.
    arm.observe(turn("r3", "s1", "sub-1", Some("main"), NOW + 5_000));
    settled(&rig).await;
    assert_eq!(
        posted(&rig).len(),
        3,
        "a request the release outran posts nothing the release would not close"
    );
    // A new run of the same agent: the start is the agent working again, and its requests count once more.
    rig.clock.store(NOW + 20_000, Ordering::SeqCst);
    companion.apply(SessionLifecycle::AgentStarted {
        key_id: KEY.into(),
        session_id: "s1".into(),
        agent_id: "sub-1".into(),
    });
    arm.observe(turn("r4", "s1", "sub-1", Some("main"), NOW + 21_000));
    settled(&rig).await;
    assert_eq!(posted(&rig).len(), 4, "the agent runs again, and once");
    rig.clock.store(NOW + 30_000, Ordering::SeqCst);
    companion.apply(released);
    settled(&rig).await;
    assert_eq!(
        posted(&rig).len(),
        5,
        "and its second run closes like the first"
    );
}

#[tokio::test]
async fn heartbeats_keep_a_session_from_ending_and_leave_its_idle_clock_alone() {
    let rig = rig().await;
    install(&rig, &lifecycle_rules());
    let mut arm = BusArm::new(rig.alerts.clone(), &inputs(&rig, BTreeMap::new()));
    let mut companion = LifecycleArm::new(rig.alerts.clone(), rig.router.clone(), arm.tracked());
    let mut poller = Poller::new(
        rig.alerts.clone(),
        &inputs(&rig, BTreeMap::new()),
        arm.tracked(),
    );
    let heartbeat = SessionLifecycle::Heartbeat {
        key_id: KEY.into(),
        session_id: "s1".into(),
    };
    arm.observe(main_turn("r1", "s1"));
    settled(&rig).await;
    assert_eq!(posted(&rig), [active("s1", "session_started")]);

    // A session whose user has stepped away: the companion reports it is still there, and nothing else does.
    for minute in [25, 31, 59, 90, 200] {
        rig.clock.store(NOW + minute * 60_000, Ordering::SeqCst);
        companion.apply(heartbeat.clone());
        poller.tick().await;
        settled(&rig).await;
        let idle_count = posted(&rig)
            .iter()
            .filter(|(_, _, e)| e == "session_idle")
            .count();
        let expected = if minute < 30 { 0 } else { 1 };
        assert_eq!(
            idle_count, expected,
            "at minute {minute}: the expiry measures from the report, the idle rule from the request"
        );
        assert!(
            posted(&rig).iter().all(|(_, _, e)| e != "session_ended"),
            "at minute {minute}: the heartbeat keeps the session from ending"
        );
    }
}

/// A key's pool moves mid-session: `session_started` is posted once, to whoever matches at that moment, so the
/// key's move to a pool whose rules would have matched gets no synthetic start; the session's next crossing is
/// what still proves the pool the rules read updated.
#[tokio::test]
async fn a_key_moved_to_another_pool_matches_that_pools_rules_from_its_next_request() {
    let rig = rig().await;
    install(
        &rig,
        &[
            rule("spare", Kind::SessionActive, ScopeType::Pool, Some("spare")),
            idle_rule("spare-idle", 30.0, ScopeType::Pool, Some("spare")),
        ],
    );
    let mut arm = BusArm::new(rig.alerts.clone(), &inputs(&rig, BTreeMap::new()));
    let mut poller = Poller::new(
        rig.alerts.clone(),
        &inputs(&rig, BTreeMap::new()),
        arm.tracked(),
    );
    arm.observe(main_turn("r1", "s1"));
    settled(&rig).await;
    assert_eq!(posted(&rig), [], "the rules watch a pool the key is not on");

    // The operator moves the key to the pool those rules watch.
    let mut spare = rig.store.pool("main").unwrap().unwrap();
    spare.id = "spare".into();
    spare.label = "Spare".into();
    rig.store.insert_pool(&spare, "test").unwrap();
    let mut key = rig.store.api_key(KEY).unwrap().unwrap();
    key.pool_id = "spare".into();
    rig.store.update_api_key(&key, "test").unwrap();
    rig.router.reload().await.unwrap();

    arm.observe(observed("r2", KEY, "spare", "s1", 0));
    settled(&rig).await;
    assert_eq!(
        posted(&rig),
        [],
        "a start is posted once, to whoever matched then, and the moved key gets no synthetic one"
    );
    rig.clock.store(NOW + 31 * 60_000, Ordering::SeqCst);
    poller.tick().await;
    settled(&rig).await;
    assert_eq!(
        detail_of(&rig, "session_idle")["pool"],
        "spare",
        "an idle crossing names the pool the session is on now"
    );
}

#[tokio::test]
async fn an_observation_during_the_drain_is_not_posted_after_the_ends_the_close_enqueued() {
    let rig = rig().await;
    install(&rig, &lifecycle_rules());
    let (tx, rx) = lifecycle_channel();
    let mut handles = spawn_inputs(
        rig.alerts.clone(),
        AlertInputs {
            lifecycle: Some(rx),
            ..inputs(&rig, BTreeMap::new())
        },
    );
    tx.send(started("s1")).unwrap();
    fake_upstream::wait_for(Duration::from_secs(5), || {
        (posted(&rig) == [active("s1", "session_started")]).then_some(())
    })
    .await;
    tx.send(SessionLifecycle::SessionEnded {
        key_id: KEY.into(),
        session_id: "s1".into(),
    })
    .unwrap();
    // The binary's order: the bus arm and the poller stop before the keepalive close enqueues the ends, the
    // consumer drains after it. A request of the session published between the two, received after the end,
    // is never applied and posts no start after the end.
    handles.stop();
    rig.bus.publish(turn_at("late", "s1", NOW + 1_000));
    drop(tx);
    handles.drain().await;
    settled(&rig).await;
    assert_eq!(
        posted(&rig),
        [
            active("s1", "session_started"),
            active("s1", "session_ended")
        ],
        "the end the queue held was posted, and the late observation was not"
    );
}

#[tokio::test]
async fn a_late_observation_posts_a_start_after_the_end_when_the_bus_arm_is_not_stopped() {
    let rig = rig().await;
    install(&rig, &lifecycle_rules());
    let (tx, rx) = lifecycle_channel();
    let mut handles = spawn_inputs(
        rig.alerts.clone(),
        AlertInputs {
            lifecycle: Some(rx),
            ..inputs(&rig, BTreeMap::new())
        },
    );
    tx.send(started("s1")).unwrap();
    fake_upstream::wait_for(Duration::from_secs(5), || {
        (posted(&rig) == [active("s1", "session_started")]).then_some(())
    })
    .await;
    tx.send(SessionLifecycle::SessionEnded {
        key_id: KEY.into(),
        session_id: "s1".into(),
    })
    .unwrap();
    drop(tx);
    handles.drain().await;
    // The bus arm is still running: the same late request is applied and starts the session again, which is
    // what stopping the arm before the close prevents.
    rig.bus.publish(turn_at("late", "s1", NOW + 1_000));
    fake_upstream::wait_for(Duration::from_secs(5), || {
        (posted(&rig).len() == 3).then_some(())
    })
    .await;
    settled(&rig).await;
    assert_eq!(
        posted(&rig),
        [
            active("s1", "session_started"),
            active("s1", "session_ended"),
            active("s1", "session_started"),
        ]
    );
    handles.abort();
}
