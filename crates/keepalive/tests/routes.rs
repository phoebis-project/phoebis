//! The two companion routes: authenticated with the API key exactly as `/v1/messages` is, scoped to that key,
//! and answering the Go engine's ack and snapshot shapes with the Go status codes.
mod common;
use axum::body::Body;
use axum::http::Request;
use phoebis_keepalive::{companion_router, EVENTS_PATH, SNAPSHOT_PATH};
use phoebis_store::ApiKeyRow;
use common::*;
use http_body_util::BodyExt;
use tower::ServiceExt;

const OTHER_KEY: &str = "phoebis-otherkey";

async fn get(
    app: &axum::Router,
    path: &str,
    query: &str,
    key: Option<&str>,
) -> (u16, Vec<(String, String)>, serde_json::Value) {
    let mut req = Request::builder()
        .method("GET")
        .uri(format!("{path}?{query}"));
    if let Some(key) = key {
        req = req.header("authorization", format!("Bearer {key}"));
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let headers = resp
        .headers()
        .iter()
        .map(|(n, v)| (n.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        headers,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

#[tokio::test]
async fn events_need_the_key_and_a_key_cannot_read_or_alter_anothers_sessions() {
    let r = rig(RigSpec::default()).await;
    let app = companion_router(r.engine.clone());
    let (status, headers, body) = get(
        &app,
        EVENTS_PATH,
        "event=heartbeat&session_id=session&instance_id=instance&sequence=2",
        None,
    )
    .await;
    assert_eq!(
        (
            status,
            body["type"].as_str(),
            body["error"]["type"].as_str()
        ),
        (401, Some("error"), Some("authentication_error"))
    );
    assert_eq!(header(&headers, "cache-control"), Some("no-store"));
    assert_eq!(
        get(
            &app,
            EVENTS_PATH,
            "event=heartbeat&session_id=session&instance_id=instance&sequence=2",
            Some("phoebis-nobody")
        )
        .await
        .0,
        401
    );
    assert_eq!(get(&app, SNAPSHOT_PATH, "", None).await.0, 401);
    // A second key: the same session id is a different session, and the first key's session is not visible.
    r.h.store
        .insert_api_key(
            &ApiKeyRow {
                id: "laptop".into(),
                label: "laptop".into(),
                notes: None,
                pool_id: "main".into(),
                enabled: true,
                revoked_at: None,
                expires_at: None,
                allowed_models: None,
                keepalive_opt_out: false,
                body_capture_hours: None,
                secret_digest: r.h.store.api_key_digest(OTHER_KEY).to_vec(),
                prefix: phoebis_store::key_prefix(OTHER_KEY),
                created_at: NOW,
                rotated_at: None,
                last_used_at: None,
            },
            "test",
        )
        .unwrap();
    r.h.router.reload().await.unwrap();
    let (status, _, body) = get(&app, SNAPSHOT_PATH, "", Some(OTHER_KEY)).await;
    assert_eq!(
        (status, body["sessions"].as_array().map(Vec::len)),
        (200, Some(0))
    );
    let (status, _, body) = get(
        &app,
        EVENTS_PATH,
        "event=heartbeat&session_id=session&instance_id=instance&sequence=2",
        Some(OTHER_KEY),
    )
    .await;
    assert_eq!(
        (status, body["error"].as_str()),
        (409, Some("unknown_instance")),
        "the other key cannot renew the first key's lease"
    );
    let (status, _, body) = get(
        &app,
        EVENTS_PATH,
        "event=session_start&session_id=session&instance_id=mine&sequence=1",
        Some(OTHER_KEY),
    )
    .await;
    assert_eq!(
        (status, body["instance_id"].as_str()),
        (200, Some("mine")),
        "and registers its own session under the same id"
    );
    let (_, _, mine) = get(&app, SNAPSHOT_PATH, "", Some(OTHER_KEY)).await;
    let (_, _, theirs) = get(&app, SNAPSHOT_PATH, "", Some(KEY)).await;
    assert_eq!(
        (
            mine["sessions"][0]["instance_id"].as_str(),
            theirs["sessions"][0]["instance_id"].as_str()
        ),
        (Some("mine"), Some("instance"))
    );
    // key_id is not in EVENT_FIELDS, so decode_query_event refuses the query before any scoping runs; the key
    // that owns a session comes from the credential alone, never from a field naming one.
    let (status, _, _) = get(
        &app,
        EVENTS_PATH,
        "event=heartbeat&session_id=session&instance_id=instance&sequence=3&key_id=desktop",
        Some(OTHER_KEY),
    )
    .await;
    assert_eq!(status, 400);
    r.h.store.revoke_api_key("laptop", NOW, "test").unwrap();
    r.h.router.reload().await.unwrap();
    assert_eq!(
        get(&app, SNAPSHOT_PATH, "", Some(OTHER_KEY)).await.0,
        401,
        "a revoked key is refused at once"
    );
}

#[tokio::test]
async fn acks_and_errors_keep_the_go_contract() {
    let r = rig(RigSpec::default()).await;
    let app = companion_router(r.engine.clone());
    let (status, headers, ack) = get(
        &app,
        EVENTS_PATH,
        "event=heartbeat&session_id=session&instance_id=instance&sequence=2",
        Some(KEY),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        (
            header(&headers, "cache-control"),
            header(&headers, "content-type")
        ),
        (Some("no-store"), Some("application/json"))
    );
    assert_eq!(
        ack.as_object().unwrap().keys().cloned().collect::<Vec<_>>(),
        [
            "agent_id",
            "instance_id",
            "lease_expires",
            "request_generation",
            "sequence",
            "session_id"
        ]
    );
    assert_eq!(
        (
            ack["sequence"].as_u64(),
            ack["agent_id"].as_str(),
            ack["request_generation"].as_u64()
        ),
        (Some(2), Some("main"), Some(0))
    );
    let lease = ack["lease_expires"].as_str().unwrap();
    assert!(
        lease.ends_with('Z') && lease.contains('T'),
        "RFC 3339 for datetime.fromisoformat: {lease}"
    );
    for (query, status, code) in [
        ("event=heartbeat&session_id=session&instance_id=instance&sequence=2", 409, "stale_sequence"),
        ("event=heartbeat&session_id=session&instance_id=instance&sequence=3&surprise=1", 400, "invalid_event"),
        ("event=heartbeat&session_id=session&instance_id=instance", 400, "invalid_event"),
        ("event=heartbeat&session_id=bad%20id&instance_id=instance&sequence=3", 400, "invalid_identity"),
        ("event=wander&session_id=session&instance_id=instance&sequence=3", 400, "unknown_event"),
        ("event=state&session_id=session&instance_id=instance&sequence=3&state=asleep&request_generation=0", 400, "invalid_state"),
        ("event=heartbeat&session_id=session&instance_id=other&sequence=3", 409, "unknown_instance"),
    ] {
        let (got, headers, body) = get(&app, EVENTS_PATH, query, Some(KEY)).await;
        assert_eq!((got, body["error"].as_str()), (status, Some(code)), "{query}");
        assert_eq!(header(&headers, "cache-control"), Some("no-store"));
    }
    let oversized = format!(
        "event=heartbeat&session_id=session&instance_id=instance&sequence=3&agent_id={}",
        "a".repeat(17 << 10)
    );
    let (status, _, body) = get(&app, EVENTS_PATH, &oversized, Some(KEY)).await;
    assert_eq!(
        (status, body["error"].as_str()),
        (413, Some("event_too_large"))
    );
    let (status, _, snapshot) = get(&app, SNAPSHOT_PATH, "", Some(KEY)).await;
    assert_eq!(status, 200);
    assert_eq!(
        snapshot["sessions"][0]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        ["agents", "ended", "instance_id", "sequence", "session_id"]
    );
    assert_eq!(
        snapshot["sessions"][0]["agents"][0]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        ["agent_id", "request_generation", "state"]
    );
    assert_eq!(
        get(&app, "/v0/management/cache-keepalive/status", "", Some(KEY))
            .await
            .0,
        404,
        "the status route is not served; the admin view replaces it"
    );
    r.engine.close();
    let (status, _, body) = get(
        &app,
        EVENTS_PATH,
        "event=heartbeat&session_id=session&instance_id=instance&sequence=4",
        Some(KEY),
    )
    .await;
    assert_eq!((status, body["error"].as_str()), (503, Some("quiesced")));
}
