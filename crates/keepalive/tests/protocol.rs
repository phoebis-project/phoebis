//! The keepalive mod's one route: a JSON report under the router key, scoped to that key, answered with the
//! acknowledgment the mod reconciles from or with a code and the status it carries.
mod common;
use axum::body::Body;
use axum::http::Request;
use common::*;
use http_body_util::BodyExt;
use phoebis_keepalive::{keepalive_router, AgentState, CompanionState, ParkKind, EVENTS_PATH};
use phoebis_store::{ApiKeyRow, RulesMode};
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

/// A report as the mod sends it: the five members every body carries, then the event's own.
fn report(event: &str, session: &str, instance: &str, sequence: u64, fields: Value) -> Value {
    let mut body = json!({
        "protocol": 1,
        "event": event,
        "session_id": session,
        "instance_id": instance,
        "sequence": sequence,
    });
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
        keepalive_opt_out: false,
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
    }
}

#[tokio::test]
async fn events_need_the_key_and_a_key_cannot_read_or_alter_anothers_sessions() {
    let r = rig(RigSpec::default()).await;
    let app = keepalive_router(r.engine.clone());
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
    assert_eq!(
        post(&app, heartbeat(2), Some("phoebis-nobody"))
            .await
            .status,
        401
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
    // key_id is no member of the body, so the decoder refuses it before any scoping runs; the key that owns a
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
    assert_eq!(
        post(
            &app,
            report("heartbeat", "session", "mine", 2, json!({})),
            Some(OTHER_KEY)
        )
        .await
        .status,
        401,
        "a revoked key is refused at once"
    );
}

#[tokio::test]
async fn a_key_created_again_under_a_used_id_inherits_none_of_its_sessions() {
    let r = rig(RigSpec::default()).await;
    let app = keepalive_router(r.engine.clone());
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
    assert_eq!(
        post(
            &app,
            report("heartbeat", "session", "instance", 3, json!({})),
            Some(KEY)
        )
        .await
        .status,
        401,
        "the deleted secret is refused outright"
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
    let app = keepalive_router(r.engine.clone());
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
            "agent_id",
            "instance_id",
            "lease_expires_ms",
            "request_generation",
            "sequence",
            "session_id"
        ]
    );
    assert_eq!(
        (
            ack.body["sequence"].as_u64(),
            ack.body["agent_id"].as_str(),
            ack.body["request_generation"].as_u64(),
            ack.body["lease_expires_ms"].as_i64()
        ),
        (Some(2), Some("main"), Some(0), Some(r.now() + 90_000))
    );
    // What the 409 rows below refuse against: a live subagent, a released one, and an ended session.
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
        assert_eq!(
            post(&app, body.clone(), Some(key)).await.status,
            200,
            "{body}"
        );
    }
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
            "an owner off session_start",
            Some(JSON),
            json_body(heartbeat(json!({"owner_id": "o"}))),
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
        limits: phoebis_keepalive::KeepaliveLimits {
            max_sessions: 1,
            max_agents: 1,
            ..RigSpec::default().limits
        },
        ..RigSpec::default()
    })
    .await;
    let small = keepalive_router(bounded.engine.clone());
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
async fn a_registration_acknowledgment_carries_the_sessions_agents_and_nothing_else() {
    let mut r = rig(RigSpec::default()).await;
    let app = keepalive_router(r.engine.clone());
    let owner = json!({"owner_id": "owner-1"});
    let first = post(
        &app,
        report("session_start", "fresh", "i", 1, owner.clone()),
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
        report("session_start", "fresh", "i", 3, owner.clone()),
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
        report("session_start", "fresh", "i", 5, owner.clone()),
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
    // A recipe on the rig's own session: its prompt, account, usage and owner stay out of the answer.
    r.observe("main").await;
    let armed = post(
        &app,
        report("session_start", "session", "instance", 2, owner),
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
            "migrations",
            "idle_ticks",
            "cache_ttl",
            "owner",
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
async fn a_parked_state_records_its_kind_wake_and_deadline_and_presence_moves_the_clock_alone() {
    let mut r = rig(RigSpec::default()).await;
    // An idle main probed once, so the presence below meets an idle tick count it must leave alone.
    r.observe("main").await;
    r.state("main", "idle");
    r.advance_attended(270_000);
    r.step().await;
    assert_eq!(r.status("main").idle_ticks, 1);
    let generation = r.status("main").request_generation;
    let base = r.sequence;
    let app = keepalive_router(r.engine.clone());
    let deadline = r.now() + 600_000;
    let wake = r.now() + 300_000;
    let state = |sequence: u64, fields: Value| {
        let mut fields = fields;
        fields["agent_id"] = json!("main");
        fields["request_generation"] = json!(generation);
        report("state", "session", "instance", base + sequence, fields)
    };
    let park = |r: &Rig| {
        let status = r.status("main");
        (
            status.state,
            status.park_kind,
            status.wake_at,
            status.deadline,
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
            AgentState::Parked,
            Some(ParkKind::Background),
            None,
            Some(deadline)
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
            AgentState::Parked,
            Some(ParkKind::Both),
            Some(wake),
            Some(deadline + 1)
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
        (AgentState::Idle, None, None, None),
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
        (AgentState::Idle, None, None, None),
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
    let status = r.status("main");
    assert_eq!(
        (
            status.last_activity,
            status.state,
            status.park_kind,
            status.wake_at,
            status.idle_ticks
        ),
        (
            r.now(),
            AgentState::Parked,
            Some(ParkKind::Scheduled),
            Some(wake),
            1
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
        r.status("main").state,
        AgentState::Busy,
        "a prompt resumes the parked main agent"
    );
}

#[tokio::test]
async fn instance_conflict_names_an_ended_or_silent_holder_and_the_generation_refusals_name_the_current_one(
) {
    let r = rig(RigSpec::default()).await;
    let app = keepalive_router(r.engine.clone());
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
    let app = keepalive_router(r.engine.clone());
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
