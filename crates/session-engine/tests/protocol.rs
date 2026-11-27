//! The companion mod's one route: a JSON report under the router key, scoped to that key, answered with the
//! acknowledgment the mod reconciles from or with a code and the status it carries.
mod common;
use std::collections::BTreeMap;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use common::*;
use fake_upstream::Respond;
use http_body_util::BodyExt;
use phoebis_ledger::{hour_of, read_rollups, RollupSelect};
use phoebis_session_engine::{
    events_router, AgentState, CompanionState, Lifecycle, ParkKind, EVENTS_PATH,
};
use phoebis_store::{ApiKeyRow, CompanionPermissions, KeyFeatureAccounts, RulesMode};
use serde_json::{json, Value};
use tower::ServiceExt;

const OTHER_KEY: &str = "phoebis-otherkey";
const JSON: &str = "application/json";

/// One refusal: what it is, the content type and the body it posts, and the status and body it answers.
type Row = (&'static str, Option<&'static str>, Vec<u8>, u16, Value);

struct Answer {
    status: u16,
    headers: Vec<(String, String)>,
    body: Value,
    text: String,
}

impl Answer {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

async fn send(
    app: &axum::Router,
    content_type: Option<&str>,
    body: Vec<u8>,
    key: Option<&str>,
) -> Answer {
    let mut req = Request::builder().method("POST").uri(EVENTS_PATH);
    if let Some(content_type) = content_type {
        req = req.header("content-type", content_type);
    }
    if let Some(key) = key {
        req = req.header("authorization", format!("Bearer {key}"));
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let headers = resp
        .headers()
        .iter()
        .map(|(n, v)| (n.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    Answer {
        status,
        headers,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        text: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

async fn post(app: &axum::Router, body: Value, key: Option<&str>) -> Answer {
    send(app, Some(JSON), serde_json::to_vec(&body).unwrap(), key).await
}

/// Posts an agent report under the harness key as the mod sends it: a `stale_request_generation` refusal names the
/// agent's current generation, and the report goes once more under it and under the same sequence, which the refusal
/// did not take.
async fn post_current(app: &axum::Router, mut body: Value) -> Answer {
    let answer = post(app, body.clone(), Some(KEY)).await;
    if answer.status == 409 && answer.body["code"] == "stale_request_generation" {
        body["request_generation"] = answer.body["request_generation"].clone();
        return post(app, body, Some(KEY)).await;
    }
    answer
}

/// The generation a registration answer lists for an agent is the agent's current one: a report under it is taken,
/// and the report's answer names the same generation.
async fn listed_generation_is_current(app: &axum::Router, sequence: u64, listed: &Value) {
    let facts = report(
        "agent_facts",
        "session",
        "instance",
        sequence,
        json!({
            "agent_id": listed["agent_id"],
            "request_generation": listed["request_generation"]
        }),
    );
    let answer = post(app, facts, Some(KEY)).await;
    assert_eq!(
        (answer.status, &answer.body["request_generation"]),
        (200, &listed["request_generation"]),
        "{}",
        answer.text
    );
}

/// A report as the mod sends it: the five members every body carries, then the event's own, and on a `session_start`
/// `cache_warming` with `true` where it names none and the three kinds where it names no `reported_facts`.
fn report(event: &str, session: &str, instance: &str, sequence: u64, fields: Value) -> Value {
    let mut body = json!({
        "protocol": 1,
        "event": event,
        "session_id": session,
        "instance_id": instance,
        "sequence": sequence,
    });
    if event == "session_start" {
        body["cache_warming"] = true.into();
        body["reported_facts"] = json!(["agents", "project", "title"]);
    }
    if let Value::Object(fields) = fields {
        body.as_object_mut().unwrap().extend(fields);
    }
    body
}

fn without(mut body: Value, member: &str) -> Value {
    body.as_object_mut().unwrap().remove(member);
    body
}

fn other_key_row(r: &Rig, id: &str) -> ApiKeyRow {
    ApiKeyRow {
        id: id.into(),
        label: id.into(),
        notes: None,
        pool_id: "main".into(),
        enabled: true,
        revoked_at: None,
        expires_at: None,
        model_rules: Vec::new(),
        model_rules_mode: RulesMode::Extend,
        long_context_twins: true,
        companion_permissions: CompanionPermissions::default(),
        body_capture_hours: None,
        secret_digest: r.h.store.api_key_digest(OTHER_KEY).to_vec(),
        prefix: phoebis_store::key_prefix(OTHER_KEY),
        incarnation: 1,
        created_at: NOW,
        updated_at: 0,
        rotated_at: None,
        last_used_at: None,
        retired_secret_last_used_at: None,
        auto_mode_server: None,
        feature_accounts: KeyFeatureAccounts::default(),
    }
}

/// An `agent_start` for `agent` on the rig's own session, under `generation` where the mod names one.
async fn agent_start(r: &mut Rig, agent: &str, generation: Option<u64>) -> Result<(), Refused> {
    let mut body = json!({"event": "agent_start", "agent_id": agent});
    if let Some(generation) = generation {
        body["request_generation"] = generation.into();
    }
    r.event(body).await.map(|_| ())
}

/// An `idle` report for `agent` on the rig's own session under `generation`: the refusal's code and generation, or
/// `None` where the engine took the report.
async fn idle_report(r: &mut Rig, agent: &str, generation: u64) -> Option<(String, Option<u64>)> {
    r.event(json!({
        "event": "state",
        "agent_id": agent,
        "state": "idle",
        "request_generation": generation
    }))
    .await
    .err()
    .map(|refused| (refused.code, refused.request_generation))
}

/// The agent count the Sessions read shows for the rig's session.
fn session_agents(r: &Rig) -> usize {
    r.engine
        .sessions()
        .iter()
        .find(|s| s.session_id == "session")
        .expect("the rig's session")
        .agents
}

#[tokio::test]
async fn events_need_the_key_and_a_key_cannot_read_or_alter_anothers_sessions() {
    let r = rig(RigSpec::default()).await;
    let app = events_router(r.engine.clone());
    let heartbeat = |sequence| report("heartbeat", "session", "instance", sequence, json!({}));
    let refused = post(&app, heartbeat(2), None).await;
    assert_eq!(
        (
            refused.status,
            refused.body["type"].as_str(),
            refused.body["error"]["type"].as_str()
        ),
        (401, Some("error"), Some("authentication_error"))
    );
    assert_eq!(refused.header("cache-control"), Some("no-store"));
    assert_eq!(refused.header("x-should-retry"), Some("false"));
    let unknown = post(&app, heartbeat(2), Some("phoebis-nobody")).await;
    assert_eq!(
        (unknown.status, unknown.header("x-should-retry")),
        (401, Some("false"))
    );
    let worker = report(
        "agent_start",
        "session",
        "instance",
        2,
        json!({"agent_id": "worker"}),
    );
    assert_eq!(post(&app, worker, Some(KEY)).await.status, 200);
    // A second key: the same session id is a different session, and the first key's session is not visible.
    r.h.store
        .insert_api_key(&other_key_row(&r, "laptop"), "test")
        .unwrap();
    r.h.router.reload().await.unwrap();
    let (status, code) = {
        let answer = post(&app, heartbeat(3), Some(OTHER_KEY)).await;
        (answer.status, answer.body["code"].clone())
    };
    assert_eq!(
        (status, code),
        (409, json!("unknown_instance")),
        "the other key cannot renew the first key's lease"
    );
    let mine = post(
        &app,
        report("session_start", "session", "mine", 1, json!({})),
        Some(OTHER_KEY),
    )
    .await;
    assert_eq!(
        (mine.status, mine.body["instance_id"].as_str()),
        (200, Some("mine")),
        "and registers its own session under the same id"
    );
    assert_eq!(
        mine.body["agents"],
        json!([{"agent_id": "main", "request_generation": 0, "state": "busy"}]),
        "with none of the first key's agents"
    );
    let theirs = post(&app, heartbeat(3), Some(KEY)).await;
    assert_eq!(
        (theirs.status, theirs.body["instance_id"].as_str()),
        (200, Some("instance")),
        "the first key's session keeps its own instance"
    );
    // key_id is no member of the body, so the decoder refuses it before any scoping runs. The key that owns a
    // session comes from the credential alone, never from a field naming one.
    let named = post(
        &app,
        report(
            "heartbeat",
            "session",
            "mine",
            2,
            json!({"key_id": "desktop"}),
        ),
        Some(OTHER_KEY),
    )
    .await;
    assert_eq!(
        (named.status, named.body.clone()),
        (400, json!({"code": "invalid_event"}))
    );
    r.h.store.revoke_api_key("laptop", NOW, "test").unwrap();
    r.h.router.reload().await.unwrap();
    let revoked = post(
        &app,
        report("heartbeat", "session", "mine", 2, json!({})),
        Some(OTHER_KEY),
    )
    .await;
    assert_eq!(
        (revoked.status, revoked.header("x-should-retry")),
        (401, Some("false")),
        "a revoked key is refused at once and told not to resend"
    );
}

#[tokio::test]
async fn a_key_created_again_under_a_used_id_inherits_none_of_its_sessions() {
    let r = rig(RigSpec::default()).await;
    let app = events_router(r.engine.clone());
    let worker = report(
        "agent_start",
        "session",
        "instance",
        2,
        json!({"agent_id": "worker"}),
    );
    assert_eq!(
        post(&app, worker, Some(KEY)).await.status,
        200,
        "the registered session is the first holder's to report on"
    );
    r.h.store.delete_api_key("desktop", "test").unwrap();
    r.h.store
        .insert_api_key(&other_key_row(&r, "desktop"), "test")
        .unwrap();
    r.h.router.reload().await.unwrap();
    let deleted = post(
        &app,
        report("heartbeat", "session", "instance", 3, json!({})),
        Some(KEY),
    )
    .await;
    assert_eq!(
        (deleted.status, deleted.header("x-should-retry")),
        (401, Some("false")),
        "the deleted secret is refused outright and told not to resend"
    );
    let ended = post(
        &app,
        report("session_end", "session", "instance", 9, json!({})),
        Some(OTHER_KEY),
    )
    .await;
    assert_eq!(
        (ended.status, ended.body.clone()),
        (409, json!({"code": "unknown_instance"})),
        "an instance of the previous holder reads as one that was never registered"
    );
    let fresh = post(
        &app,
        report("session_start", "session", "fresh", 1, json!({})),
        Some(OTHER_KEY),
    )
    .await;
    assert_eq!(
        (
            fresh.status,
            fresh.body["instance_id"].as_str(),
            fresh.body["agents"].clone()
        ),
        (
            200,
            Some("fresh"),
            json!([{"agent_id": "main", "request_generation": 0, "state": "busy"}])
        ),
        "the replacement registers the same session id under its own generation, with none of the previous \
         holder's agents"
    );
}

#[tokio::test]
async fn the_acknowledgment_and_the_refusals_follow_the_json_contract() {
    let r = rig(RigSpec::default()).await;
    let app = events_router(r.engine.clone());
    let ack = post(
        &app,
        report("heartbeat", "session", "instance", 2, json!({})),
        Some(KEY),
    )
    .await;
    assert_eq!(ack.status, 200);
    assert_eq!(
        (ack.header("cache-control"), ack.header("content-type")),
        (Some("no-store"), Some(JSON))
    );
    assert_eq!(
        ack.body
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        [
            "instance_id",
            "lease_expires_ms",
            "request_generation",
            "sequence",
            "session_id"
        ],
        "a session-level event names no agent"
    );
    assert_eq!(
        (
            ack.body["sequence"].as_u64(),
            ack.body["request_generation"].as_u64(),
            ack.body["lease_expires_ms"].as_i64()
        ),
        (Some(2), Some(0), Some(r.now() + 90_000)),
        "and carries main's generation"
    );
    // What the 409 rows below refuse against: a live subagent, a released one, and an ended session. An agent
    // event's answer names its agent, and a registration's and an end's name none.
    let mut named = Vec::new();
    for (body, key) in [
        (
            report(
                "agent_start",
                "session",
                "instance",
                3,
                json!({"agent_id": "worker"}),
            ),
            KEY,
        ),
        (
            report(
                "agent_start",
                "session",
                "instance",
                4,
                json!({"agent_id": "leaving"}),
            ),
            KEY,
        ),
        (
            report(
                "state",
                "session",
                "instance",
                5,
                json!({"agent_id": "leaving", "state": "released", "request_generation": 0}),
            ),
            KEY,
        ),
        (report("session_start", "over", "i-over", 1, json!({})), KEY),
        (report("session_end", "over", "i-over", 2, json!({})), KEY),
    ] {
        let answer = post(&app, body.clone(), Some(key)).await;
        assert_eq!(answer.status, 200, "{body}");
        named.push(answer.body["agent_id"].as_str().map(String::from));
    }
    assert_eq!(
        named,
        [
            Some("worker".to_string()),
            Some("leaving".to_string()),
            Some("leaving".to_string()),
            None,
            None
        ]
    );
    // Every refusal names the session's next sequence, and none of them may take it.
    let next = 6;
    let main = |fields: Value| report("state", "session", "instance", next, fields);
    let heartbeat = |fields: Value| report("heartbeat", "session", "instance", next, fields);
    let json_body = |body: Value| serde_json::to_vec(&body).unwrap();
    let code = |code: &str| json!({ "code": code });
    let rows: Vec<Row> = vec![
        (
            "an unknown member",
            Some(JSON),
            json_body(heartbeat(json!({"surprise": 1}))),
            400,
            code("invalid_event"),
        ),
        (
            "a missing sequence",
            Some(JSON),
            json_body(without(heartbeat(json!({})), "sequence")),
            400,
            code("invalid_event"),
        ),
        (
            "a negative sequence",
            Some(JSON),
            json_body(heartbeat(json!({"sequence": -1}))),
            400,
            code("invalid_event"),
        ),
        (
            "a repeated member",
            Some(JSON),
            br#"{"protocol":1,"event":"heartbeat","session_id":"session","instance_id":"instance","sequence":6,"sequence":7}"#.to_vec(),
            400,
            code("invalid_event"),
        ),
        (
            "a body that names protocol twice",
            Some(JSON),
            br#"{"protocol":2,"event":"heartbeat","session_id":"session","instance_id":"instance","sequence":6,"protocol":1}"#.to_vec(),
            400,
            code("invalid_event"),
        ),
        (
            "a missing session id",
            Some(JSON),
            json_body(without(heartbeat(json!({})), "session_id")),
            400,
            code("invalid_event"),
        ),
        (
            "a missing instance id",
            Some(JSON),
            json_body(without(heartbeat(json!({})), "instance_id")),
            400,
            code("invalid_event"),
        ),
        (
            "a missing event",
            Some(JSON),
            json_body(without(heartbeat(json!({})), "event")),
            400,
            code("invalid_event"),
        ),
        (
            "a missing protocol",
            Some(JSON),
            json_body(without(heartbeat(json!({})), "protocol")),
            400,
            code("invalid_event"),
        ),
        (
            "a body that is not json",
            Some(JSON),
            b"not json".to_vec(),
            400,
            code("invalid_event"),
        ),
        (
            "a body that is not an object",
            Some(JSON),
            b"[]".to_vec(),
            400,
            code("invalid_event"),
        ),
        (
            "a text/plain body",
            Some("text/plain"),
            json_body(heartbeat(json!({}))),
            400,
            code("invalid_event"),
        ),
        (
            "a body without a content type",
            None,
            json_body(heartbeat(json!({}))),
            400,
            code("invalid_event"),
        ),
        (
            "a kind off activity",
            Some(JSON),
            json_body(heartbeat(json!({"kind": "prompt"}))),
            400,
            code("invalid_event"),
        ),
        (
            "an activity without its kind",
            Some(JSON),
            json_body(report(
                "activity",
                "session",
                "instance",
                next,
                json!({"agent_id": "main", "request_generation": 0}),
            )),
            400,
            code("invalid_event"),
        ),
        (
            "an activity with a null kind",
            Some(JSON),
            json_body(report(
                "activity",
                "session",
                "instance",
                next,
                json!({"agent_id": "main", "kind": null, "request_generation": 0}),
            )),
            400,
            code("invalid_event"),
        ),
        (
            "an agent event without its agent",
            Some(JSON),
            json_body(report(
                "activity",
                "session",
                "instance",
                next,
                json!({"kind": "prompt"}),
            )),
            400,
            code("invalid_event"),
        ),
        (
            "an owner on session_start",
            Some(JSON),
            json_body(report(
                "session_start",
                "session",
                "instance",
                next,
                json!({"owner_id": "o"}),
            )),
            400,
            code("invalid_event"),
        ),
        (
            "a previous instance off session_start",
            Some(JSON),
            json_body(heartbeat(json!({"previous_instance_id": "i"}))),
            400,
            code("invalid_event"),
        ),
        (
            "a registration that names no reported facts",
            Some(JSON),
            json_body(json!({
                "protocol": 1,
                "event": "session_start",
                "session_id": "unlisted",
                "instance_id": "i-unlisted",
                "sequence": 1,
                "cache_warming": true
            })),
            400,
            code("invalid_event"),
        ),
        (
            "a registration whose list holds a word that is no kind",
            Some(JSON),
            json_body(json!({
                "protocol": 1,
                "event": "session_start",
                "session_id": "misspelled",
                "instance_id": "i-misspelled",
                "sequence": 1,
                "cache_warming": true,
                "reported_facts": ["agent"]
            })),
            400,
            code("invalid_event"),
        ),
        (
            "a registration that names no cache warming",
            Some(JSON),
            json_body(json!({
                "protocol": 1,
                "event": "session_start",
                "session_id": "unwarmed",
                "instance_id": "i-unwarmed",
                "sequence": 1,
                "reported_facts": ["agents", "project", "title"]
            })),
            400,
            code("invalid_event"),
        ),
        (
            "an agent id on a session event",
            Some(JSON),
            json_body(heartbeat(json!({"agent_id": "main"}))),
            400,
            code("invalid_event"),
        ),
        (
            "a generation on a session event",
            Some(JSON),
            json_body(heartbeat(json!({"request_generation": 0}))),
            400,
            code("invalid_event"),
        ),
        (
            "a null agent_type on a heartbeat",
            Some(JSON),
            json_body(heartbeat(json!({ "agent_type": null }))),
            400,
            code("invalid_event"),
        ),
        (
            "a null branch on a heartbeat",
            Some(JSON),
            json_body(heartbeat(json!({ "branch": null }))),
            400,
            code("invalid_event"),
        ),
        (
            "a null cwd on a heartbeat",
            Some(JSON),
            json_body(heartbeat(json!({ "cwd": null }))),
            400,
            code("invalid_event"),
        ),
        (
            "a null description on a heartbeat",
            Some(JSON),
            json_body(heartbeat(json!({ "description": null }))),
            400,
            code("invalid_event"),
        ),
        (
            "a null name on a heartbeat",
            Some(JSON),
            json_body(heartbeat(json!({ "name": null }))),
            400,
            code("invalid_event"),
        ),
        (
            "a null project on a heartbeat",
            Some(JSON),
            json_body(heartbeat(json!({ "project": null }))),
            400,
            code("invalid_event"),
        ),
        (
            "a null repository on a heartbeat",
            Some(JSON),
            json_body(heartbeat(json!({ "repository": null }))),
            400,
            code("invalid_event"),
        ),
        (
            "a null repository_root on a heartbeat",
            Some(JSON),
            json_body(heartbeat(json!({ "repository_root": null }))),
            400,
            code("invalid_event"),
        ),
        (
            "a null title on a heartbeat",
            Some(JSON),
            json_body(heartbeat(json!({ "title": null }))),
            400,
            code("invalid_event"),
        ),
        (
            "a state off a state event",
            Some(JSON),
            json_body(report(
                "activity",
                "session",
                "instance",
                next,
                json!({"agent_id": "main", "kind": "prompt", "state": "busy"}),
            )),
            400,
            code("invalid_event"),
        ),
        (
            "a state event without its agent",
            Some(JSON),
            json_body(main(json!({"state": "idle", "request_generation": 0}))),
            400,
            code("invalid_event"),
        ),
        (
            "a state event without its state",
            Some(JSON),
            json_body(main(json!({"agent_id": "main", "request_generation": 0}))),
            400,
            code("invalid_event"),
        ),
        (
            "a park kind off a parked state",
            Some(JSON),
            json_body(main(
                json!({"agent_id": "main", "state": "idle", "park_kind": "background", "request_generation": 0}),
            )),
            400,
            code("invalid_event"),
        ),
        (
            "a wake time off a parked state",
            Some(JSON),
            json_body(main(
                json!({"agent_id": "main", "state": "idle", "wake_at": 1, "request_generation": 0}),
            )),
            400,
            code("invalid_event"),
        ),
        (
            "a deadline off a parked state",
            Some(JSON),
            json_body(main(
                json!({"agent_id": "main", "state": "busy", "deadline": 1, "request_generation": 0}),
            )),
            400,
            code("invalid_event"),
        ),
        (
            "a parked state without its kind",
            Some(JSON),
            json_body(main(
                json!({"agent_id": "main", "state": "parked", "request_generation": 0}),
            )),
            400,
            code("invalid_event"),
        ),
        (
            "a scheduled park without its wake time",
            Some(JSON),
            json_body(main(
                json!({"agent_id": "main", "state": "parked", "park_kind": "scheduled", "request_generation": 0}),
            )),
            400,
            code("invalid_event"),
        ),
        (
            "a park of both kinds without its wake time",
            Some(JSON),
            json_body(main(
                json!({"agent_id": "main", "state": "parked", "park_kind": "both", "deadline": 1, "request_generation": 0}),
            )),
            400,
            code("invalid_event"),
        ),
        (
            "a wake time that is not a whole number",
            Some(JSON),
            json_body(main(
                json!({"agent_id": "main", "state": "parked", "park_kind": "scheduled", "wake_at": 1.5, "request_generation": 0}),
            )),
            400,
            code("invalid_event"),
        ),
        (
            "a deadline that is not a whole number",
            Some(JSON),
            json_body(main(
                json!({"agent_id": "main", "state": "parked", "park_kind": "background", "deadline": 1.5, "request_generation": 0}),
            )),
            400,
            code("invalid_event"),
        ),
        (
            "protocol 2",
            Some(JSON),
            json_body(heartbeat(json!({"protocol": 2}))),
            400,
            code("unsupported_protocol"),
        ),
        (
            "protocol 2 with a member this protocol lacks",
            Some(JSON),
            json_body(heartbeat(json!({"protocol": 2, "surprise": 1}))),
            400,
            code("unsupported_protocol"),
        ),
        (
            "protocol 2 with a repeated member",
            Some(JSON),
            br#"{"protocol":2,"event":"heartbeat","session_id":"session","instance_id":"instance","sequence":6,"sequence":7}"#.to_vec(),
            400,
            code("unsupported_protocol"),
        ),
        (
            "an id the identity rules refuse",
            Some(JSON),
            json_body(report("heartbeat", "bad id", "instance", next, json!({}))),
            400,
            code("invalid_identity"),
        ),
        (
            "sequence 0",
            Some(JSON),
            json_body(report("heartbeat", "session", "instance", 0, json!({}))),
            400,
            code("invalid_identity"),
        ),
        (
            "an unknown state word",
            Some(JSON),
            json_body(main(
                json!({"agent_id": "main", "state": "asleep", "request_generation": 0}),
            )),
            400,
            code("invalid_state"),
        ),
        (
            "an unknown park kind",
            Some(JSON),
            json_body(main(
                json!({"agent_id": "main", "state": "parked", "park_kind": "nap", "wake_at": 1, "request_generation": 0}),
            )),
            400,
            code("invalid_state"),
        ),
        (
            "an unknown activity kind",
            Some(JSON),
            json_body(report(
                "activity",
                "session",
                "instance",
                next,
                json!({"agent_id": "main", "kind": "nudge", "request_generation": 0}),
            )),
            400,
            code("invalid_state"),
        ),
        (
            "an unknown event",
            Some(JSON),
            json_body(report("wander", "session", "instance", next, json!({}))),
            400,
            code("unknown_event"),
        ),
        (
            "a body over 16 KiB",
            Some(JSON),
            json_body(main(
                json!({"agent_id": "a".repeat(17 << 10), "state": "idle", "request_generation": 0}),
            )),
            413,
            code("event_too_large"),
        ),
        (
            "a stale sequence",
            Some(JSON),
            json_body(report(
                "heartbeat",
                "session",
                "instance",
                next - 1,
                json!({}),
            )),
            409,
            code("stale_sequence"),
        ),
        (
            "an instance the session does not hold",
            Some(JSON),
            json_body(report("heartbeat", "session", "other", next, json!({}))),
            409,
            code("unknown_instance"),
        ),
        (
            "an ended instance",
            Some(JSON),
            json_body(report("heartbeat", "over", "i-over", 3, json!({}))),
            409,
            code("instance_ended"),
        ),
        (
            "a second instance that names none it replaces",
            Some(JSON),
            json_body(report("session_start", "session", "other", 1, json!({}))),
            409,
            code("instance_conflict"),
        ),
        (
            "a replacement of an instance nobody registered",
            Some(JSON),
            json_body(report(
                "session_start",
                "fresh",
                "i",
                1,
                json!({"previous_instance_id": "gone"}),
            )),
            409,
            code("unknown_previous_instance"),
        ),
        (
            "a second start of a live agent",
            Some(JSON),
            json_body(report(
                "agent_start",
                "session",
                "instance",
                next,
                json!({"agent_id": "worker"}),
            )),
            409,
            code("agent_exists"),
        ),
        (
            "an agent the session does not hold",
            Some(JSON),
            json_body(main(
                json!({"agent_id": "ghost", "state": "idle", "request_generation": 0}),
            )),
            409,
            code("unknown_agent"),
        ),
        (
            "a stale generation",
            Some(JSON),
            json_body(report(
                "activity",
                "session",
                "instance",
                next,
                json!({"agent_id": "worker", "kind": "prompt", "request_generation": 7}),
            )),
            409,
            json!({"code": "stale_request_generation", "request_generation": 0}),
        ),
        (
            "a released agent",
            Some(JSON),
            json_body(report(
                "activity",
                "session",
                "instance",
                next,
                json!({"agent_id": "leaving", "kind": "prompt", "request_generation": 1}),
            )),
            409,
            json!({"code": "agent_released", "request_generation": 1}),
        ),
    ];
    let mut wrong = Vec::new();
    for (name, content_type, body, status, expected) in rows {
        let answer = send(&app, content_type, body, Some(KEY)).await;
        let got = (
            answer.status,
            answer.body.clone(),
            answer.header("cache-control").map(String::from),
            answer.header("content-type").map(String::from),
        );
        let want = (
            status,
            expected,
            Some("no-store".to_string()),
            Some(JSON.to_string()),
        );
        if got != want {
            wrong.push(format!("{name}: got {got:?}, want {want:?}"));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    let after = post(&app, heartbeat(json!({})), Some(KEY)).await;
    assert_eq!(
        (after.status, after.body["sequence"].as_u64()),
        (200, Some(next)),
        "no refusal took the session's next sequence"
    );
    let bounded = rig(RigSpec {
        session_caps: phoebis_session_engine::SessionCaps {
            max_sessions: 1,
            max_agents: 1,
        },
        ..RigSpec::default()
    })
    .await;
    // The rig's session takes the one session slot and the one agent slot at its first request.
    bounded
        .engine
        .observe(bounded.observed("bounded", "main", BODY));
    let small = events_router(bounded.engine.clone());
    for (body, expected) in [
        (
            report("session_start", "second", "i2", 1, json!({})),
            "session_limit",
        ),
        (
            report(
                "agent_start",
                "session",
                "instance",
                2,
                json!({"agent_id": "worker"}),
            ),
            "agent_limit",
        ),
    ] {
        let answer = post(&small, body, Some(KEY)).await;
        assert_eq!((answer.status, answer.body.clone()), (429, code(expected)));
    }
    r.engine.close();
    let closed = post(
        &app,
        report("heartbeat", "session", "instance", next + 1, json!({})),
        Some(KEY),
    )
    .await;
    assert_eq!(
        (closed.status, closed.body.clone()),
        (503, code("quiesced"))
    );
}

#[tokio::test]
async fn an_agent_id_takes_an_at_sign_and_a_previous_instance_id_does_not() {
    let mut r = rig(RigSpec::default()).await;
    agent_start(&mut r, "twin@session-22222222", None)
        .await
        .expect("a teammate's start is taken");
    let refused = r
        .event(json!({"event": "session_start", "previous_instance_id": "instance@1"}))
        .await
        .expect_err("a previous instance id with an at sign is refused");
    assert_eq!(refused.code, "invalid_identity");
}

#[tokio::test]
async fn a_registration_acknowledgment_carries_the_sessions_agents_and_nothing_else() {
    let mut r = rig(RigSpec::default()).await;
    let app = events_router(r.engine.clone());
    let first = post(
        &app,
        report("session_start", "fresh", "i", 1, json!({})),
        Some(KEY),
    )
    .await;
    assert_eq!(
        (first.status, first.body["agents"].clone()),
        (
            200,
            json!([{"agent_id": "main", "request_generation": 0, "state": "busy"}])
        )
    );
    let worker = report(
        "agent_start",
        "fresh",
        "i",
        2,
        json!({"agent_id": "worker"}),
    );
    assert!(
        post(&app, worker, Some(KEY))
            .await
            .body
            .get("agents")
            .is_none(),
        "only a registration's answer carries the agents"
    );
    let again = post(
        &app,
        report("session_start", "fresh", "i", 3, json!({})),
        Some(KEY),
    )
    .await;
    assert_eq!(
        (again.status, again.body["agents"].clone()),
        (
            200,
            json!([
                {"agent_id": "main", "request_generation": 0, "state": "busy"},
                {"agent_id": "worker", "request_generation": 0, "state": "busy"}
            ])
        )
    );
    let released = post(
        &app,
        report(
            "state",
            "fresh",
            "i",
            4,
            json!({"agent_id": "worker", "state": "released", "request_generation": 0}),
        ),
        Some(KEY),
    )
    .await;
    assert_eq!(
        (
            released.status,
            released.body["request_generation"].as_u64()
        ),
        (200, Some(1))
    );
    let listed = post(
        &app,
        report("session_start", "fresh", "i", 5, json!({})),
        Some(KEY),
    )
    .await;
    assert_eq!(
        listed.body["agents"],
        json!([
            {"agent_id": "main", "request_generation": 0, "state": "busy"},
            {"agent_id": "worker", "request_generation": 1, "state": "released"}
        ]),
        "a released agent stays listed with the generation its release left"
    );
    // A recipe on the rig's own session: its prompt, account and usage stay out of the answer.
    r.observe("main").await;
    let armed = post(
        &app,
        report("session_start", "session", "instance", 2, json!({})),
        Some(KEY),
    )
    .await;
    assert_eq!(armed.status, 200);
    for answer in [&first, &again, &listed, &armed] {
        for forbidden in [
            "account",
            "usage",
            "reason",
            "probe",
            "next_refresh",
            "cache_ttl",
            "private prompt",
        ] {
            assert!(
                !answer.text.contains(forbidden),
                "the acknowledgment leaks {forbidden}: {}",
                answer.text
            );
        }
    }
}

#[tokio::test]
async fn a_registration_answer_carries_a_parked_agents_park() {
    let r = rig(RigSpec::default()).await;
    let app = events_router(r.engine.clone());
    let wake = r.now() + 300_000;
    let deadline = r.now() + 600_000;
    for body in [
        report("session_start", "fresh", "i", 1, json!({})),
        report(
            "agent_start",
            "fresh",
            "i",
            2,
            json!({"agent_id": "worker"}),
        ),
        report(
            "state",
            "fresh",
            "i",
            3,
            json!({
                "agent_id": "worker",
                "state": "parked",
                "park_kind": "both",
                "wake_at": wake,
                "deadline": deadline,
                "request_generation": 0
            }),
        ),
    ] {
        assert_eq!(post(&app, body, Some(KEY)).await.status, 200);
    }
    let again = post(
        &app,
        report("session_start", "fresh", "i", 4, json!({})),
        Some(KEY),
    )
    .await;
    assert_eq!(
        (again.status, again.body["agents"].clone()),
        (
            200,
            json!([
                {"agent_id": "main", "request_generation": 0, "state": "busy"},
                {
                    "agent_id": "worker",
                    "request_generation": 0,
                    "state": "parked",
                    "park_kind": "both",
                    "wake_at": wake,
                    "deadline": deadline
                }
            ])
        ),
        "the parked worker carries its park and the busy main carries no park key"
    );
}

/// Under the default policy, whose longest TTL is the hour, a released record answers `agent_released` for an hour
/// after the release and then leaves: a report under its id answers `unknown_agent`, and the mod's start registers the
/// agent anew under the cap.
#[tokio::test]
async fn a_released_agent_answers_agent_released_for_one_cache_ttl_and_then_registers_anew() {
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    agent_start(&mut r, "worker", None).await.unwrap();
    r.state("worker", "released").await;
    let released = r.request_generation("worker");
    assert_eq!((released, r.engine.stats().sessions.agents), (1, 2));
    r.advance_attended(3_599_999).await;
    assert_eq!(
        idle_report(&mut r, "worker", released).await,
        Some(("agent_released".to_string(), Some(released))),
        "the record refuses a late report until one cache TTL has passed since the release"
    );
    r.advance_attended(1).await;
    r.step().await;
    assert!(
        r.lifecycle("worker").is_none(),
        "the sweep at the leave removes the record"
    );
    assert_eq!(
        (r.engine.stats().sessions.agents, session_agents(&r)),
        (1, 1),
        "the engine's count and the session's drop by the record"
    );
    assert_eq!(
        idle_report(&mut r, "worker", released).await,
        Some(("unknown_agent".to_string(), None)),
        "a report under the id that left answers as for any id the session does not hold"
    );
    agent_start(&mut r, "worker", None)
        .await
        .expect("a start after the leave registers anew");
    // The start's answer carries the generation, and the session view words Busy under `AwaitingRequest` as
    // `registered`.
    assert_eq!(
        (r.request_generation("worker"), r.lifecycle("worker")),
        (0, Some(Lifecycle::Registered)),
        "a new record, not the released one"
    );
    assert_eq!(
        (r.engine.stats().sessions.agents, session_agents(&r)),
        (2, 2)
    );
}

/// A start under the released record's generation before the record left reuses it, and the reuse clears the leave,
/// so the agent is still there when the release's hour has passed.
#[tokio::test]
async fn a_start_before_a_released_agent_leaves_reuses_its_record_and_clears_the_leave() {
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    agent_start(&mut r, "worker", None).await.unwrap();
    r.state("worker", "released").await;
    let released = r.request_generation("worker");
    r.advance_attended(1_000).await;
    agent_start(&mut r, "worker", Some(released))
        .await
        .expect("the start reuses the released record");
    // Busy under `AwaitingRequest`, which the session view words `registered`.
    assert_eq!(
        (
            r.request_generation("worker"),
            r.lifecycle("worker"),
            r.engine.stats().sessions.agents
        ),
        (released + 1, Some(Lifecycle::Registered), 2)
    );
    r.advance_attended(3_600_000).await;
    r.step().await;
    assert_eq!(
        (r.lifecycle("worker"), r.engine.stats().sessions.agents),
        (Some(Lifecycle::Registered), 2),
        "the reused record stays past the release's leave"
    );
}

/// The leave is the longest TTL the pool's cache TTL policy names, read whatever its `enabled` says: a pool whose four
/// TTLs are five minutes lets a released record go five minutes after the release.
#[tokio::test]
async fn a_released_agent_leaves_after_the_longest_ttl_of_its_pools_cache_ttl_policy() {
    let mut r = rig(RigSpec {
        pool: PoolSpec {
            cache_ttl: r#"{"metered":{"main":"5m","subagent":"5m"},"plan":{"main":"5m","subagent":"5m"}}"#,
            ..PoolSpec::default()
        },
        ..RigSpec::default()
    })
    .await;
    agent_start(&mut r, "worker", None).await.unwrap();
    // The record takes its pool from its last real request.
    r.observe("worker").await;
    r.state("worker", "released").await;
    let released = r.request_generation("worker");
    r.advance_attended(299_999).await;
    assert_eq!(
        idle_report(&mut r, "worker", released).await,
        Some(("agent_released".to_string(), Some(released))),
        "held until five minutes after the release"
    );
    r.advance_attended(1).await;
    assert_eq!(
        idle_report(&mut r, "worker", released).await,
        Some(("unknown_agent".to_string(), None)),
        "and gone at five minutes"
    );
}

/// A released agent that sent a real request keeps its recipe line in the Sessions read while its record is held,
/// and the line leaves with the record.
#[tokio::test]
async fn a_released_agents_recipe_line_leaves_the_sessions_read_with_its_record() {
    let mut r = rig(RigSpec::default()).await;
    agent_start(&mut r, "worker", None).await.unwrap();
    r.observe("worker").await;
    r.state("worker", "released").await;
    let lines = |r: &Rig| {
        r.engine
            .recipes()
            .into_iter()
            .filter(|line| line.agent_id == "worker")
            .map(|line| line.agent_state)
            .collect::<Vec<_>>()
    };
    r.advance_attended(3_599_999).await;
    assert_eq!(lines(&r), vec![Lifecycle::Released]);
    r.advance_attended(1).await;
    r.step().await;
    assert!(lines(&r).is_empty(), "the line went with the record");
}

/// A released record whose probe is still in flight at its leave stays until the result lands, so the result settles
/// under the release's label rather than as a replaced record's, and the next sweep removes it.
#[tokio::test]
async fn a_released_record_with_a_probe_in_flight_at_its_leave_waits_for_the_result() {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    agent_start(&mut r, "worker", None).await.unwrap();
    r.observe("worker").await;
    r.up.push_credential_rule(
        A,
        "/v1/messages",
        Respond::Gated(Box::new(probe_answer(2_000, 0))),
    );
    r.advance_attended(270_000).await;
    r.engine.step().await;
    wait_for(Duration::from_secs(5), || {
        (r.up.requests().len() == 2).then_some(())
    })
    .await;
    r.state("worker", "released").await;
    r.advance_attended(3_600_000).await;
    r.engine.step().await;
    // The probe in flight is the request the upstream still holds, which the wait above saw arrive.
    assert_eq!(
        r.lifecycle("worker"),
        Some(Lifecycle::Released),
        "the sweep leaves a record whose probe is still in flight"
    );
    r.up.release();
    no_probe_running(&r.engine).await;
    r.pump(None).await;
    r.step().await;
    assert!(
        r.lifecycle("worker").is_none(),
        "the sweep after the result removes the record"
    );
    let wasted = wait_for(Duration::from_secs(30), || {
        let rows =
            r.db.read_blocking(|c| {
                read_rollups(c, hour_of(NOW) - 3_600, i64::MAX, &RollupSelect::default())
            })
            .unwrap();
        let mut reasons: BTreeMap<String, u64> = Default::default();
        for row in rows.iter().filter(|row| row.kind == "probe") {
            for (reason, count) in &row.wasted_probe_reasons {
                *reasons.entry(reason.clone()).or_default() += count;
            }
        }
        (!reasons.is_empty()).then_some(reasons)
    })
    .await;
    assert_eq!(
        wasted,
        BTreeMap::from([("released".to_string(), 1)]),
        "the late result settles under the release"
    );
}

/// A session's end keeps the leave of a record the mod released before it, so that record leaves one cache TTL after
/// the mod's release, while main, which the end released, leaves one cache TTL after the end.
#[tokio::test]
async fn a_session_end_keeps_the_leave_of_a_record_the_mod_released_before_it() {
    let mut r = rig(RigSpec::default()).await;
    agent_start(&mut r, "worker", None).await.unwrap();
    r.state("worker", "released").await;
    r.advance_attended(1_000).await;
    r.event(session_end("session")).await.unwrap();
    r.advance(3_599_000);
    r.step().await;
    assert!(
        r.lifecycle("worker").is_none(),
        "the sweep at the mod's release plus one cache TTL removes the record"
    );
    assert!(
        r.lifecycle("main").is_some(),
        "main stays until its own leave"
    );
    r.advance(1_000);
    r.step().await;
    assert!(
        r.lifecycle("main").is_none(),
        "the sweep at the end plus one cache TTL removes main"
    );
}

#[tokio::test]
async fn a_parked_state_records_its_kind_wake_and_deadline_and_presence_moves_the_clock_alone() {
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    r.state("main", "idle").await;
    r.advance_attended(270_000).await;
    r.step().await;
    let generation = r.request_generation("main");
    let base = r.sequence;
    let app = events_router(r.engine.clone());
    let deadline = r.now() + 600_000;
    let wake = r.now() + 300_000;
    let state = |sequence: u64, fields: Value| {
        let mut fields = fields;
        fields["agent_id"] = json!("main");
        fields["request_generation"] = json!(generation);
        report("state", "session", "instance", base + sequence, fields)
    };
    let park = |r: &Rig| {
        let view = r.view("main");
        (
            view.agent_state,
            view.park_kind,
            view.deadline,
            view.wake_at,
        )
    };
    let background = state(
        2,
        json!({"state": "parked", "park_kind": "background", "deadline": deadline}),
    );
    assert_eq!(post(&app, background, Some(KEY)).await.status, 200);
    assert_eq!(
        park(&r),
        (
            Lifecycle::Parked,
            Some(ParkKind::Background),
            Some(deadline),
            None
        )
    );
    let both = state(
        3,
        json!({"state": "parked", "park_kind": "both", "wake_at": wake, "deadline": deadline + 1}),
    );
    assert_eq!(post(&app, both, Some(KEY)).await.status, 200);
    assert_eq!(
        park(&r),
        (
            Lifecycle::Parked,
            Some(ParkKind::Both),
            Some(deadline + 1),
            Some(wake)
        )
    );
    assert_eq!(
        post(&app, state(4, json!({"state": "idle"})), Some(KEY))
            .await
            .status,
        200
    );
    assert_eq!(
        park(&r),
        (Lifecycle::Idle, None, None, None),
        "any state but parked clears the park"
    );
    for (fields, expected) in [
        (
            json!({"state": "parked", "park_kind": "scheduled"}),
            "invalid_event",
        ),
        (
            json!({"state": "idle", "park_kind": "background"}),
            "invalid_event",
        ),
        (
            json!({"state": "parked", "park_kind": "nap", "wake_at": wake}),
            "invalid_state",
        ),
    ] {
        let answer = post(&app, state(5, fields.clone()), Some(KEY)).await;
        assert_eq!(
            (answer.status, answer.body.clone()),
            (400, json!({ "code": expected })),
            "{fields}"
        );
    }
    assert_eq!(
        park(&r),
        (Lifecycle::Idle, None, None, None),
        "a refused park records nothing"
    );
    let scheduled = state(
        5,
        json!({"state": "parked", "park_kind": "scheduled", "wake_at": wake}),
    );
    assert_eq!(post(&app, scheduled, Some(KEY)).await.status, 200);
    r.advance(5_000);
    let presence = report(
        "activity",
        "session",
        "instance",
        base + 6,
        json!({"agent_id": "main", "kind": "presence", "request_generation": generation}),
    );
    assert_eq!(post(&app, presence, Some(KEY)).await.status, 200);
    assert_eq!(
        (r.saved("main").await["last_activity"].clone(), park(&r)),
        (
            json!(r.now()),
            (
                Lifecycle::Parked,
                Some(ParkKind::Scheduled),
                None,
                Some(wake)
            )
        ),
        "presence moves the idle clock and nothing else: the agent stays parked"
    );
    let prompt = report(
        "activity",
        "session",
        "instance",
        base + 7,
        json!({"agent_id": "main", "kind": "prompt", "request_generation": generation}),
    );
    assert_eq!(post(&app, prompt, Some(KEY)).await.status, 200);
    assert_eq!(
        r.view("main").agent_state,
        Lifecycle::Busy,
        "a prompt resumes the parked main agent"
    );
}

#[tokio::test]
async fn a_scheduled_park_ends_whole_when_its_wake_passes_and_the_answer_carries_no_park() {
    let mut r = rig(RigSpec::default()).await;
    agent_start(&mut r, "worker", None).await.unwrap();
    r.observe("worker").await;
    let base = r.sequence;
    let app = events_router(r.engine.clone());
    let wake = r.now() + 10 * MINUTE;
    let park = report(
        "state",
        "session",
        "instance",
        base + 1,
        json!({
            "agent_id": "worker",
            "state": "parked",
            "park_kind": "scheduled",
            "wake_at": wake,
            "request_generation": r.request_generation("worker")
        }),
    );
    let parked = post_current(&app, park).await;
    assert_eq!(parked.status, 200, "{}", parked.text);
    r.advance(11 * MINUTE);
    r.step().await;
    let worker = r.view("worker");
    assert_eq!(
        (
            worker.agent_state,
            worker.park_kind,
            worker.deadline,
            worker.wake_at
        ),
        (Lifecycle::Parked, None, None, None),
        "the passed wake ends the scheduled park whole"
    );
    let again = post(
        &app,
        report("session_start", "session", "instance", base + 2, json!({})),
        Some(KEY),
    )
    .await;
    let listed = &again.body["agents"][1];
    assert_eq!(
        (again.status, without(listed.clone(), "request_generation")),
        (200, json!({"agent_id": "worker", "state": "parked"})),
        "the registration answer carries no key of the ended park"
    );
    listed_generation_is_current(&app, base + 3, listed).await;
}

/// A main that has sent no request holds no recipe, as each main of a session with its warming off does, and the tick
/// still ends its park once the wake passes, so main reads idle without waiting for the mod's own report.
#[tokio::test]
async fn a_scheduled_park_of_a_main_without_a_recipe_ends_whole_when_its_wake_passes() {
    let mut r = rig(RigSpec::default()).await;
    let base = r.sequence;
    let app = events_router(r.engine.clone());
    let wake = r.now() + 10 * MINUTE;
    let park = report(
        "state",
        "session",
        "instance",
        base + 1,
        json!({
            "agent_id": "main",
            "state": "parked",
            "park_kind": "scheduled",
            "wake_at": wake,
            "request_generation": r.request_generation("main")
        }),
    );
    let parked = post_current(&app, park).await;
    assert_eq!(parked.status, 200, "{}", parked.text);
    r.advance(11 * MINUTE);
    r.step().await;
    let again = post(
        &app,
        report("session_start", "session", "instance", base + 2, json!({})),
        Some(KEY),
    )
    .await;
    assert_eq!(
        (
            again.status,
            without(again.body["agents"][0].clone(), "request_generation")
        ),
        (200, json!({"agent_id": "main", "state": "idle"})),
        "the passed wake ended main's park with no recipe held"
    );
}

#[tokio::test]
async fn a_both_park_whose_deadline_passes_first_narrows_to_scheduled_and_keeps_its_wake() {
    let mut r = rig(RigSpec::default()).await;
    agent_start(&mut r, "worker", None).await.unwrap();
    r.observe("worker").await;
    let base = r.sequence;
    let app = events_router(r.engine.clone());
    let deadline = r.now() + 10 * MINUTE;
    let wake = r.now() + 60 * MINUTE;
    let park = report(
        "state",
        "session",
        "instance",
        base + 1,
        json!({
            "agent_id": "worker",
            "state": "parked",
            "park_kind": "both",
            "wake_at": wake,
            "deadline": deadline,
            "request_generation": r.request_generation("worker")
        }),
    );
    let parked = post_current(&app, park).await;
    assert_eq!(parked.status, 200, "{}", parked.text);
    r.advance(11 * MINUTE);
    r.step().await;
    let worker = r.view("worker");
    assert_eq!(
        (
            worker.agent_state,
            worker.park_kind,
            worker.deadline,
            worker.wake_at
        ),
        (
            Lifecycle::Parked,
            Some(ParkKind::Scheduled),
            None,
            Some(wake)
        ),
        "the passed deadline leaves the wake's part of the park"
    );
    let again = post(
        &app,
        report("session_start", "session", "instance", base + 2, json!({})),
        Some(KEY),
    )
    .await;
    let listed = &again.body["agents"][1];
    assert_eq!(
        (again.status, without(listed.clone(), "request_generation")),
        (
            200,
            json!({
                "agent_id": "worker",
                "park_kind": "scheduled",
                "state": "parked",
                "wake_at": wake
            })
        ),
        "the registration answer carries the narrowed park without its deadline"
    );
    listed_generation_is_current(&app, base + 3, listed).await;
}

#[tokio::test]
async fn a_both_park_whose_wake_passes_first_narrows_to_background_and_drops_its_wake() {
    let mut r = rig(RigSpec::default()).await;
    agent_start(&mut r, "worker", None).await.unwrap();
    r.observe("worker").await;
    let base = r.sequence;
    let app = events_router(r.engine.clone());
    let wake = r.now() + 10 * MINUTE;
    let deadline = r.now() + 60 * MINUTE;
    let park = report(
        "state",
        "session",
        "instance",
        base + 1,
        json!({
            "agent_id": "worker",
            "state": "parked",
            "park_kind": "both",
            "wake_at": wake,
            "deadline": deadline,
            "request_generation": r.request_generation("worker")
        }),
    );
    let parked = post_current(&app, park).await;
    assert_eq!(parked.status, 200, "{}", parked.text);
    r.advance(11 * MINUTE);
    r.step().await;
    let worker = r.view("worker");
    assert_eq!(
        (
            worker.agent_state,
            worker.park_kind,
            worker.deadline,
            worker.wake_at
        ),
        (
            Lifecycle::Parked,
            Some(ParkKind::Background),
            Some(deadline),
            None
        ),
        "the passed wake leaves the deadline's part of the park"
    );
    let again = post(
        &app,
        report("session_start", "session", "instance", base + 2, json!({})),
        Some(KEY),
    )
    .await;
    let listed = &again.body["agents"][1];
    assert_eq!(
        (again.status, without(listed.clone(), "request_generation")),
        (
            200,
            json!({
                "agent_id": "worker",
                "deadline": deadline,
                "park_kind": "background",
                "state": "parked"
            })
        ),
        "the registration answer carries the narrowed park without its wake"
    );
    listed_generation_is_current(&app, base + 3, listed).await;
}

#[tokio::test]
async fn a_park_report_with_a_part_its_kind_does_not_take_answers_invalid_event() {
    let mut r = rig(RigSpec::default()).await;
    // No turn runs between the start and the reports, since a completed turn moves the worker's generation, so each
    // report carries the generation the start answered and its stray part is its one fault.
    let generation = r
        .event(json!({"event": "agent_start", "agent_id": "worker"}))
        .await
        .unwrap()
        .request_generation;
    let base = r.sequence;
    let app = events_router(r.engine.clone());
    let wake = r.now() + 10 * MINUTE;
    let deadline = r.now() + 60 * MINUTE;
    let background = report(
        "state",
        "session",
        "instance",
        base + 1,
        json!({
            "agent_id": "worker",
            "state": "parked",
            "park_kind": "background",
            "wake_at": wake,
            "request_generation": generation
        }),
    );
    let answer = post(&app, background, Some(KEY)).await;
    assert_eq!(
        (answer.status, answer.body),
        (400, json!({"code": "invalid_event"})),
        "a background park takes no wake"
    );
    let scheduled = report(
        "state",
        "session",
        "instance",
        base + 2,
        json!({
            "agent_id": "worker",
            "state": "parked",
            "park_kind": "scheduled",
            "wake_at": wake,
            "deadline": deadline,
            "request_generation": generation
        }),
    );
    let answer = post(&app, scheduled, Some(KEY)).await;
    assert_eq!(
        (answer.status, answer.body),
        (400, json!({"code": "invalid_event"})),
        "a scheduled park takes no deadline"
    );
}

#[tokio::test]
async fn a_null_member_is_refused_off_its_event_and_reads_as_absent_on_it() {
    let r = rig(RigSpec::default()).await;
    let base = r.sequence;
    let app = events_router(r.engine.clone());
    let heartbeat = report(
        "heartbeat",
        "session",
        "instance",
        base + 1,
        json!({"deadline": null}),
    );
    let answer = post(&app, heartbeat, Some(KEY)).await;
    assert_eq!(
        (answer.status, answer.body),
        (400, json!({"code": "invalid_event"})),
        "a heartbeat takes no deadline, whatever its value"
    );
    let parked = report(
        "state",
        "session",
        "instance",
        base + 2,
        json!({
            "agent_id": "main",
            "state": "parked",
            "park_kind": "background",
            "deadline": null,
            "request_generation": r.request_generation("main")
        }),
    );
    let answer = post(&app, parked, Some(KEY)).await;
    assert_eq!(
        answer.status, 200,
        "a background park reads a null deadline as none: {}",
        answer.text
    );
}

#[tokio::test]
async fn instance_conflict_names_an_ended_or_silent_holder_and_the_generation_refusals_name_the_current_one(
) {
    let r = rig(RigSpec::default()).await;
    let app = events_router(r.engine.clone());
    let takeover = |session: &str, instance: &str, previous: Option<&str>| {
        let fields = previous.map_or(json!({}), |p| json!({ "previous_instance_id": p }));
        report("session_start", session, instance, 1, fields)
    };
    let live = post(&app, takeover("session", "other", None), Some(KEY)).await;
    assert_eq!(
        (live.status, live.body.clone()),
        (409, json!({"code": "instance_conflict"})),
        "a live holder is not named"
    );
    r.advance(r.lease_ms() + 1_000);
    let silent = post(&app, takeover("session", "other", None), Some(KEY)).await;
    assert_eq!(
        (silent.status, silent.body.clone()),
        (
            409,
            json!({"code": "instance_conflict", "instance_id": "instance"})
        ),
        "a holder whose lease lapsed is named"
    );
    let replaced = post(
        &app,
        takeover("session", "other", Some("instance")),
        Some(KEY),
    )
    .await;
    assert_eq!(
        (replaced.status, replaced.body["instance_id"].as_str()),
        (200, Some("other")),
        "the retry naming the holder replaces it"
    );
    for body in [
        report("session_start", "gone", "i1", 1, json!({})),
        report("session_end", "gone", "i1", 2, json!({})),
    ] {
        assert_eq!(post(&app, body, Some(KEY)).await.status, 200);
    }
    let ended = post(&app, takeover("gone", "i2", None), Some(KEY)).await;
    assert_eq!(
        (ended.status, ended.body.clone()),
        (
            409,
            json!({"code": "instance_conflict", "instance_id": "i1"})
        ),
        "an ended holder is named"
    );
    assert_eq!(
        post(&app, takeover("gone", "i2", Some("i1")), Some(KEY))
            .await
            .status,
        200
    );
    let stale = post(
        &app,
        report(
            "activity",
            "session",
            "other",
            2,
            json!({"agent_id": "main", "kind": "prompt", "request_generation": 7}),
        ),
        Some(KEY),
    )
    .await;
    assert_eq!(
        (stale.status, stale.body.clone()),
        (
            409,
            json!({"code": "stale_request_generation", "request_generation": 0})
        )
    );
    let worker = report(
        "agent_start",
        "session",
        "other",
        2,
        json!({"agent_id": "worker"}),
    );
    assert_eq!(post(&app, worker, Some(KEY)).await.status, 200);
    let released = post(
        &app,
        report(
            "state",
            "session",
            "other",
            3,
            json!({"agent_id": "worker", "state": "released", "request_generation": 0}),
        ),
        Some(KEY),
    )
    .await;
    let generation = released.body["request_generation"].as_u64().unwrap();
    let refused = post(
        &app,
        report(
            "activity",
            "session",
            "other",
            4,
            json!({"agent_id": "worker", "kind": "prompt", "request_generation": generation}),
        ),
        Some(KEY),
    )
    .await;
    assert_eq!(
        (refused.status, refused.body.clone()),
        (
            409,
            json!({"code": "agent_released", "request_generation": generation})
        )
    );
    let restarted = post(
        &app,
        report(
            "agent_start",
            "session",
            "other",
            4,
            json!({"agent_id": "worker", "request_generation": generation}),
        ),
        Some(KEY),
    )
    .await;
    assert_eq!(
        (restarted.status, restarted.body["agent_id"].as_str()),
        (200, Some("worker")),
        "a released agent starts again under the generation its release named"
    );
}

#[tokio::test]
async fn a_same_instance_start_revives_an_ended_session_so_a_clear_then_a_resume_never_loops() {
    let mut r = rig(RigSpec::default()).await;
    let app = events_router(r.engine.clone());
    // One process, one instance: session A, then a /clear to B, then a /resume back to A.
    for body in [
        report(
            "agent_start",
            "session",
            "instance",
            2,
            json!({"agent_id": "worker"}),
        ),
        report("session_end", "session", "instance", 3, json!({})),
        report("session_start", "b", "instance", 4, json!({})),
        report("session_end", "b", "instance", 5, json!({})),
    ] {
        assert_eq!(
            post(&app, body.clone(), Some(KEY)).await.status,
            200,
            "{body}"
        );
    }
    let revived = post(
        &app,
        report("session_start", "session", "instance", 6, json!({})),
        Some(KEY),
    )
    .await;
    assert_eq!(
        (revived.status, revived.body["agents"].clone()),
        (
            200,
            json!([
                {"agent_id": "main", "request_generation": 1, "state": "busy"},
                {"agent_id": "worker", "request_generation": 1, "state": "released"}
            ])
        ),
        "the revival answers the session's agents with the generations the end left"
    );
    assert_eq!(
        revived.body["lease_expires_ms"].as_i64(),
        Some(r.now() + r.lease_ms())
    );
    let views = r.engine.sessions();
    let a = views
        .iter()
        .find(|s| s.session_id == "session")
        .expect("session A");
    assert_eq!(a.companion, CompanionState::Reporting);
    let heartbeat = post(
        &app,
        report("heartbeat", "session", "instance", 7, json!({})),
        Some(KEY),
    )
    .await;
    assert_eq!(heartbeat.status, 200);
    for sequence in [7, 5] {
        let stale = post(
            &app,
            report("session_start", "session", "instance", sequence, json!({})),
            Some(KEY),
        )
        .await;
        assert_eq!(
            (stale.status, stale.body.clone()),
            (409, json!({"code": "stale_sequence"})),
            "a start at or below the session's last sequence"
        );
    }
    let b = post(
        &app,
        report("heartbeat", "b", "instance", 8, json!({})),
        Some(KEY),
    )
    .await;
    assert_eq!(
        (b.status, b.body.clone()),
        (409, json!({"code": "instance_ended"})),
        "an ended session nobody revived stays ended"
    );
    r.observe("main").await;
    assert_eq!(
        r.engine
            .recipes()
            .iter()
            .map(|recipe| recipe.session_id.as_str())
            .collect::<Vec<_>>(),
        ["session"],
        "the revived session's next real request arms main again"
    );
}

/// The end releases main with the session's other agents, and a same-instance start inside the cache TTL revives main
/// as before and clears its leave, so main is still there when the end's hour has passed.
#[tokio::test]
async fn a_revival_inside_the_cache_ttl_keeps_main_and_clears_its_leave() {
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    r.event(session_end("session")).await.unwrap();
    r.advance(1_000);
    r.reregister().await;
    r.advance_attended(3_599_000).await;
    r.step().await;
    // The revival leaves main busy under `AwaitingRequest`, which the session view words `registered`.
    assert_eq!(
        (r.lifecycle("main"), r.engine.stats().sessions.agents),
        (Some(Lifecycle::Registered), 1),
        "the revived main stays past the end's leave"
    );
}

/// Once main's record left one cache TTL after the session's end, a same-instance start registers main anew, so a
/// resume past the hour finds its main.
#[tokio::test]
async fn a_revival_after_mains_record_left_registers_main_anew() {
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    r.event(session_end("session")).await.unwrap();
    r.advance(3_600_000);
    r.step().await;
    assert!(
        r.lifecycle("main").is_none(),
        "main's record left one cache TTL after the end"
    );
    assert_eq!(r.engine.stats().sessions.agents, 0);
    let revived = r.reregister().await;
    assert_eq!(
        serde_json::to_value(&revived.agents).unwrap(),
        json!([{"agent_id": "main", "request_generation": 0, "state": "busy"}]),
        "the acknowledgment lists the new main"
    );
    // Busy under `AwaitingRequest`, which the session view words `registered`.
    assert_eq!(r.lifecycle("main"), Some(Lifecycle::Registered));
    assert_eq!(
        (r.engine.stats().sessions.agents, session_agents(&r)),
        (1, 1)
    );
}

/// The revival that registers main anew counts against the agent cap as any new record does, and a refused one leaves
/// the session ended.
#[tokio::test]
async fn a_revival_that_would_register_main_past_the_agent_cap_answers_agent_limit() {
    let mut r = rig(RigSpec {
        session_caps: phoebis_session_engine::SessionCaps {
            max_agents: 1,
            ..RigSpec::default().session_caps
        },
        ..RigSpec::default()
    })
    .await;
    r.observe("main").await;
    r.event(session_end("session")).await.unwrap();
    r.advance(3_600_000);
    r.step().await;
    r.event(session_start("other"))
        .await
        .expect("the slot main's record left takes another session");
    // The other session's main takes the agent slot at its first request.
    let mut first = r.observed("other", "main", BODY);
    if let phoebis_bus::Event::RequestObserved(request) = &mut first {
        request.session_id = Some("other".into());
    }
    r.engine.observe(first);
    let refused = r.event(session_start("session")).await.unwrap_err();
    assert_eq!(
        (refused.code.as_str(), refused.status),
        ("agent_limit", 429)
    );
    assert!(
        r.lifecycle("main").is_none(),
        "the refused revival registered no main"
    );
    assert_eq!(
        r.event(json!({"event": "heartbeat"}))
            .await
            .unwrap_err()
            .code,
        "instance_ended",
        "and left the session ended"
    );
}

/// The end of a session that sent a request releases its agents, and each released record keeps its slot of the agent
/// cap until it leaves one cache TTL after the end, so a registration that needs main's slot is refused until the leave
/// and admitted at it.
#[tokio::test]
async fn an_ended_sessions_released_records_hold_their_agent_slots_until_they_leave() {
    let mut r = rig(RigSpec {
        session_caps: phoebis_session_engine::SessionCaps {
            max_agents: 1,
            ..RigSpec::default().session_caps
        },
        ..RigSpec::default()
    })
    .await;
    r.observe("main").await;
    r.event(session_end("session")).await.unwrap();
    r.advance(3_599_999);
    r.step().await;
    let refused = r.event(session_start("other")).await.unwrap_err();
    assert_eq!(
        (refused.code.as_str(), refused.status),
        ("agent_limit", 429),
        "main's released record still holds its slot"
    );
    r.advance(1);
    r.step().await;
    r.event(session_start("other"))
        .await
        .expect("the leave frees main's slot");
}

#[tokio::test]
async fn a_facts_report_replaces_the_whole_set() {
    let mut r = rig(RigSpec::default()).await;
    let session_facts = |title: Option<&str>| {
        let mut body = json!({"event": "session_facts", "branch": "main"});
        // The key is left out where the case clears the title.
        if let Some(title) = title {
            body["title"] = title.into();
        }
        body
    };
    r.event(session_facts(Some("Checkout retry")))
        .await
        .unwrap();
    r.event(session_facts(None)).await.unwrap();
    let session = (
        "desktop".to_string(),
        r.generation("desktop"),
        "session".to_string(),
    );
    r.engine.flush_persisted().await;
    let stored = r
        .engine
        .stored_facts(vec![session.clone()])
        .await
        .remove(&session)
        .expect("the rig's stored sets");
    assert_eq!(
        (stored.session.branch(), stored.session.title()),
        (Some("main"), None),
        "a member the later report leaves out is cleared"
    );
}

#[tokio::test]
async fn agent_facts_for_an_unheld_agent_is_refused_and_adds_no_record() {
    let mut r = rig(RigSpec::default()).await;
    let agents = session_agents(&r);
    let refused = r
        .event(json!({"event": "agent_facts", "agent_id": "ghost", "name": "reviewer"}))
        .await
        .unwrap_err();
    assert_eq!(
        (refused.code.as_str(), refused.status),
        ("unknown_agent", 409)
    );
    assert_eq!(
        session_agents(&r),
        agents,
        "a facts report registers no agent"
    );
}

#[tokio::test]
async fn agent_facts_leaves_a_parked_agent_parked() {
    let mut r = rig(RigSpec::default()).await;
    let generation = r
        .event(json!({"event": "agent_start", "agent_id": "a1"}))
        .await
        .unwrap()
        .request_generation;
    let deadline = r.now() + 60_000;
    r.event(json!({
        "event": "state",
        "agent_id": "a1",
        "state": "parked",
        "park_kind": "background",
        "deadline": deadline,
        "request_generation": generation
    }))
    .await
    .unwrap();
    r.event(json!({
        "event": "agent_facts",
        "agent_id": "a1",
        "name": "reviewer",
        "request_generation": generation
    }))
    .await
    .unwrap();
    let agents = r
        .reregister()
        .await
        .agents
        .expect("a registration lists the session's agents");
    let a1 = agents
        .iter()
        .find(|a| a.agent_id == "a1")
        .expect("the registration lists a1");
    let answered = serde_json::to_value(a1).unwrap();
    assert_eq!(
        (
            a1.state,
            &answered["park_kind"],
            &answered["deadline"],
            &answered["wake_at"]
        ),
        (
            AgentState::Parked,
            &serde_json::json!("background"),
            &serde_json::json!(deadline),
            &serde_json::Value::Null
        ),
        "a facts report is no activity, so the park stands"
    );
}

#[tokio::test]
async fn session_facts_renews_no_lease() {
    let mut r = rig(RigSpec::default()).await;
    let lease = r.reregister().await.lease_expires_ms;
    r.advance(30_000);
    let ack = r.event(json!({"event": "session_facts"})).await.unwrap();
    assert_eq!(
        ack.lease_expires_ms, lease,
        "a facts report says nothing of the companion's liveness"
    );
}
