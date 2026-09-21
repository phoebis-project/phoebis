//! A window sums one key's rollups alone, probes left out, and the route it backs answers only the caller's
//! own key, a deleted key's rows staying under its own generation.
mod common;

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use phoebis_ledger::{
    key_usage_windows, AttemptRecord, LedgerConfig, LedgerDb, RequestRecord, KEY_RANGE_PREDICATE,
};
use phoebis_store::{ApiKeyRow, StorageKey, Store, StoreError};
use common::*;
use fake_upstream::{FakeUpstream, Rule};
use http_body_util::BodyExt;
use tower::ServiceExt;

fn used(id: &str) -> AttemptRecord {
    AttemptRecord {
        request_id: id.into(),
        attempt: 1,
        account_id: "a".into(),
        account_kind: "oauth".into(),
        started_at: 1_000,
        outcome: "succeeded".into(),
        input_tokens: Some(100),
        output_tokens: Some(10),
        cache_read_tokens: Some(50),
        cache_write_5m_tokens: Some(20),
        estimated_usd: Some(0.5),
        ..Default::default()
    }
}

fn unused(id: &str) -> AttemptRecord {
    AttemptRecord {
        request_id: id.into(),
        attempt: 1,
        account_id: "a".into(),
        account_kind: "oauth".into(),
        started_at: 1_000,
        outcome: "failed".into(),
        ..Default::default()
    }
}

fn request(
    id: &str,
    key_id: &str,
    model: &str,
    kind: &str,
    outcome: &str,
    status: u16,
) -> RequestRecord {
    RequestRecord {
        id: id.into(),
        received_at: 1_000,
        key_id: Some(key_id.into()),
        pool_id: Some("p".into()),
        model: model.into(),
        canonical_model: model.into(),
        kind: kind.into(),
        outcome: outcome.into(),
        status: Some(status),
        attempts: 1,
        ..Default::default()
    }
}

#[test]
fn a_window_sums_one_keys_rollups_and_leaves_probes_and_other_keys_out() {
    let db = LedgerDb::open_in_memory().unwrap();
    db.insert_request(
        &request("r1", "k1", "claude-sonnet-5", "inference", "succeeded", 200),
        &[used("r1")],
        None,
    )
    .unwrap();
    db.insert_request(
        &request("r2", "k1", "claude-sonnet-5", "inference", "succeeded", 200),
        &[used("r2")],
        None,
    )
    .unwrap();
    db.insert_request(
        &request("r3", "k1", "claude-sonnet-5", "inference", "failed", 500),
        &[unused("r3")],
        None,
    )
    .unwrap();
    db.insert_request(
        &request("r4", "k1", "claude-opus-5", "inference", "succeeded", 200),
        &[used("r4")],
        None,
    )
    .unwrap();
    // A probe under the same key is folded into its own rollup row and must not reach the key's own totals,
    // the way `request_totals` already leaves probes out of the key list.
    db.insert_request(
        &request("r5", "k1", "claude-sonnet-5", "probe", "succeeded", 200),
        &[used("r5")],
        None,
    )
    .unwrap();
    db.insert_request(
        &request("r6", "k2", "claude-sonnet-5", "inference", "succeeded", 200),
        &[used("r6")],
        None,
    )
    .unwrap();

    // Rows an hour old, planted under the generation the builders stamp, so the day window is every row the
    // reading key filed under the id.
    let day = |id: &str| {
        db.read(|c| key_usage_windows(c, id, 0, 10_000))
            .map(|(day, _)| day)
            .unwrap()
    };

    let k1 = day("k1");
    assert_eq!((k1.requests, k1.succeeded, k1.failed), (4, 3, 1));
    assert_eq!(k1.tokens.input, 300);
    assert_eq!(k1.tokens.cache_write, 60);
    assert!((k1.cost_usd - 1.5).abs() < 1e-9);
    assert_eq!(
        k1.models
            .iter()
            .map(|m| m.model.as_str())
            .collect::<Vec<_>>(),
        ["claude-opus-5", "claude-sonnet-5"]
    );

    assert_eq!(day("k2").requests, 1);

    let k3 = day("k3");
    assert_eq!(
        (
            k3.requests,
            k3.succeeded,
            k3.failed,
            k3.cost_usd,
            k3.models.len()
        ),
        (0, 0, 0, 0.0, 0),
        "a key with no rows under its id answers an empty window"
    );
}

const DAY_MS: i64 = 86_400_000;
const HOUR_MS: i64 = 3_600_000;

fn request_at(id: &str, key_id: &str, at: i64) -> RequestRecord {
    RequestRecord {
        received_at: at,
        ..request(id, key_id, "claude-sonnet-5", "inference", "succeeded", 200)
    }
}

#[test]
fn the_day_window_is_the_tail_of_the_weeks_own_rows() {
    let db = LedgerDb::open_in_memory().unwrap();
    // Off the hour, so the reported bound can only match if it is rounded down to the bucket the sum starts
    // at rather than copied from the caller's instant.
    let now = 30 * DAY_MS + HOUR_MS + 1_234;
    for (id, at) in [
        ("r1", now - HOUR_MS),
        ("r2", now - 3 * DAY_MS),
        ("r3", now - 8 * DAY_MS),
    ] {
        db.insert_request(&request_at(id, "k1", at), &[used(id)], None)
            .unwrap();
    }

    let (day, week) = db.read(|c| key_usage_windows(c, "k1", 0, now)).unwrap();
    assert_eq!(
        (day.requests, week.requests),
        (1, 2),
        "the row three days back is the week's alone, and the row eight days back is neither's"
    );
    let hour_floor = |ms: i64| ms - ms.rem_euclid(HOUR_MS);
    assert_eq!(
        (day.from, week.from),
        (hour_floor(now - DAY_MS), hour_floor(now - 7 * DAY_MS)),
        "the bound each window reports is the hour its sum starts at"
    );
    assert_eq!((day.to, week.to), (now, now));
}

#[test]
fn the_windows_read_the_presenting_keys_generation_alone_and_start_where_their_name_says() {
    let db = LedgerDb::open_in_memory().unwrap();
    let now = 30 * DAY_MS + HOUR_MS + 1_234;
    // Two holders of one id inside one hour: the earlier one's row and the current one's.
    let earlier = RequestRecord {
        key_generation: 1_000,
        ..request_at("r1", "k1", now - 2 * HOUR_MS)
    };
    let current = RequestRecord {
        key_generation: 2_000,
        ..request_at("r2", "k1", now - 2 * HOUR_MS)
    };
    db.insert_request(&earlier, &[used("r1")], None).unwrap();
    db.insert_request(&current, &[used("r2")], None).unwrap();
    let (day, week) = db.read(|c| key_usage_windows(c, "k1", 2_000, now)).unwrap();
    assert_eq!(
        (day.requests, week.requests),
        (1, 1),
        "the current holder reads its own row and not the earlier one's"
    );
    let hour_floor = |ms: i64| ms - ms.rem_euclid(HOUR_MS);
    assert_eq!(
        (day.from, week.from),
        (hour_floor(now - DAY_MS), hour_floor(now - 7 * DAY_MS)),
        "nothing is left out of the window: the row it shares an hour with is another generation's"
    );
    let (day, _) = db.read(|c| key_usage_windows(c, "k1", 1_000, now)).unwrap();
    assert_eq!(
        day.requests, 1,
        "the earlier holder's row still answers to its own generation"
    );
}

#[test]
fn the_keyed_read_is_planned_on_an_index_that_starts_with_the_key() {
    let db = LedgerDb::open_in_memory().unwrap();
    let plan = db
        .read(|c| {
            let mut stmt = c.prepare(&format!(
                "EXPLAIN QUERY PLAN SELECT hour FROM rollups_hourly WHERE {KEY_RANGE_PREDICATE}"
            ))?;
            let rows = stmt.query_map(("k1", 0_i64, 0_i64, 1_i64), |r| r.get::<_, String>(3))?;
            Ok(rows.collect::<rusqlite::Result<Vec<String>>>()?)
        })
        .unwrap();
    assert!(
        plan.iter().any(|step| step.contains("rollups_hourly_key")),
        "one key's week costs its own rows, not a scan of every key's hours: {plan:?}"
    );
}

async fn get(
    app: &axum::Router,
    key: Option<&str>,
) -> (u16, Vec<(String, String)>, serde_json::Value) {
    let mut req = Request::builder().method("GET").uri("/v1/usage");
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

/// One raw credential header, so a shape `get` never sends reaches the route as a client would send it.
async fn get_with_header(
    app: &axum::Router,
    name: &str,
    value: &str,
) -> (u16, Vec<(String, String)>, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/usage")
                .header(name, value)
                .body(Body::empty())
                .unwrap(),
        )
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

/// Polls the route itself rather than the ledger underneath it, so the assertion covers the same read path a
/// caller does.
async fn wait_for_route(app: &axum::Router, key: &str) -> serde_json::Value {
    let give_up = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let (_, _, body) = get(app, Some(key)).await;
        if body["windows"]["24h"]["requests"].as_u64() == Some(1) {
            return body;
        }
        assert!(
            std::time::Instant::now() < give_up,
            "the route never saw the relayed request: {body}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// No key named `account_id`, and no string value naming the fixture's own account id or a secret's shape:
/// the self-service answer names the caller's key and nothing of the registry beyond it.
fn assert_nothing_leaks(value: &serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                assert_ne!(k, "account_id", "the body names an account: {value}");
                assert_nothing_leaks(v);
            }
        }
        serde_json::Value::Array(items) => items.iter().for_each(assert_nothing_leaks),
        serde_json::Value::String(s) => {
            assert!(
                s != "a" && !s.starts_with("sk-ant-") && s != KEY && !s.starts_with("phoebis-"),
                "an account id or a secret leaked: {s}"
            );
        }
        _ => {}
    }
}

#[tokio::test]
async fn the_usage_route_answers_the_callers_own_key_and_refuses_the_rest() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let app = phoebis_ledger::usage_router(h.db.clone(), h.router.clone(), clock_of(&h));

    let (status, headers, body) = get(&app, None).await;
    assert_eq!(status, 401);
    assert_eq!(body["error"]["type"], "authentication_error");
    assert_eq!(header(&headers, "cache-control"), Some("no-store"));

    assert_eq!(get(&app, Some("phoebis-nobody")).await.0, 401);

    // A scheme the relay does not accept is refused in the same shape a missing header is, and says as
    // little.
    let (status, headers, body) = get_with_header(&app, "authorization", "Basic x").await;
    assert_eq!(status, 401);
    assert_eq!(body["error"]["type"], "authentication_error");
    assert_eq!(header(&headers, "x-phoebis-error-code"), Some("auth_malformed"));
    assert_eq!(header(&headers, "cache-control"), Some("no-store"));

    let (status, headers, body) = get(&app, Some(KEY)).await;
    assert_eq!(status, 200);
    assert_eq!(header(&headers, "cache-control"), Some("no-store"));
    assert_eq!(
        body["key"],
        serde_json::json!({ "id": "desktop", "label": "desktop", "pool_id": "main" })
    );
    assert_eq!(body["windows"]["24h"]["requests"], 0);

    // The page says the route takes the key header the relay takes, either shape.
    let (status, _, x_api_key) = get_with_header(&app, "x-api-key", KEY).await;
    assert_eq!(status, 200);
    assert_eq!(x_api_key["key"], body["key"]);

    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.into_body().collect().await.unwrap();

    let body = wait_for_route(&app, KEY).await;
    assert_eq!(
        body["windows"]["24h"]["models"][0]["model"],
        "claude-sonnet-4-5"
    );
    assert_eq!(body["windows"]["7d"]["requests"], 1);
    assert_nothing_leaks(&body);
}

#[tokio::test]
async fn a_second_key_sees_its_own_usage_and_not_the_firsts() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let app = phoebis_ledger::usage_router(h.db.clone(), h.router.clone(), clock_of(&h));
    const LAPTOP: &str = "phoebis-laptop";
    h.store
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
                secret_digest: h.store.api_key_digest(LAPTOP).to_vec(),
                prefix: phoebis_store::key_prefix(LAPTOP),
                created_at: NOW,
                rotated_at: None,
                last_used_at: None,
            },
            "test",
        )
        .unwrap();
    h.router.reload().await.unwrap();

    for (key, model) in [(KEY, "claude-sonnet-4-5"), (LAPTOP, "claude-opus-5")] {
        let resp = h
            .app
            .clone()
            .oneshot(request_for(key, model, "s1"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let _ = resp.into_body().collect().await.unwrap();
    }

    let desktop = wait_for_route(&app, KEY).await;
    assert_eq!(desktop["key"]["id"], "desktop");
    assert_eq!(desktop["windows"]["24h"]["requests"], 1);
    let laptop = wait_for_route(&app, LAPTOP).await;
    assert_eq!(laptop["key"]["id"], "laptop");
    assert_eq!(laptop["windows"]["24h"]["requests"], 1);
}

/// The generation the store stamped the harness's key with: `insert_api_key` stamps the wall clock and ignores
/// the row's `created_at`, so a planted row that is to read as the live key's carries this and not `NOW`.
fn generation_of(store: &Store, id: &str) -> i64 {
    store.api_key(id).unwrap().unwrap().created_at
}

/// The key row the suite's harness seeds, rebuilt for an id that is being created a second time.
fn desktop_row(store: &Store) -> ApiKeyRow {
    ApiKeyRow {
        id: "desktop".into(),
        label: "desktop".into(),
        notes: None,
        pool_id: "main".into(),
        enabled: true,
        revoked_at: None,
        expires_at: None,
        allowed_models: None,
        keepalive_opt_out: false,
        body_capture_hours: None,
        secret_digest: store.api_key_digest(KEY).to_vec(),
        prefix: phoebis_store::key_prefix(KEY),
        created_at: NOW,
        rotated_at: None,
        last_used_at: None,
    }
}

#[tokio::test]
async fn the_route_reads_the_generation_of_the_key_that_presented_the_secret() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    let store = Arc::new(Store::open(&path, &StorageKey::generate()).unwrap());
    let h = harness_on(
        store,
        Arc::new(LedgerDb::open_in_memory().unwrap()),
        &up,
        &[oauth("a", A, 0)],
        (0, None),
        LedgerConfig::default(),
        false,
    )
    .await;
    let app = phoebis_ledger::usage_router(h.db.clone(), h.router.clone(), clock_of(&h));

    let now = h.clock.load(Ordering::SeqCst);
    // The store stamps the key from the wall clock, so the live generation is read back rather than assumed.
    let first = generation_of(&h.store, "desktop");
    h.db.insert_request(
        &RequestRecord {
            key_generation: first,
            ..request_at("r1", "desktop", now)
        },
        &[used("r1")],
        None,
    )
    .unwrap();
    let (status, _, body) = get(&app, Some(KEY)).await;
    assert_eq!(status, 200);
    assert_eq!(
        body["windows"]["24h"]["requests"], 1,
        "the holder that filed the row reads it: {body}"
    );

    // The one way a live key shares an id with an earlier holder: a database written before a delete
    // retired the id. The tombstone is what forbids it now, so the row carrying it is dropped here to
    // reach the state the upgrade to that rule preserves.
    h.store.delete_api_key("desktop", "test").unwrap();
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute("DELETE FROM api_key_tombstones WHERE id = 'desktop'", [])
        .unwrap();
    // The store stamps a key from the wall clock, and the read below is only meaningful where the second
    // holder's generation differs from the first's.
    std::thread::sleep(Duration::from_millis(2));
    h.store
        .insert_api_key(&desktop_row(&h.store), "test")
        .unwrap();
    h.router.reload().await.unwrap();

    let (status, _, body) = get(&app, Some(KEY)).await;
    assert_eq!(status, 200);
    assert_eq!(
        body["key"]["id"], "desktop",
        "the secret still authenticates: {body}"
    );
    let second = generation_of(&h.store, "desktop");
    assert!(
        second > first,
        "the store stamped the new holder past the old"
    );
    assert_eq!(
        body["windows"]["24h"]["requests"], 0,
        "what the id spent before this holder existed is not this holder's: {body}"
    );
    assert_eq!(body["windows"]["7d"]["requests"], 0, "{body}");
    let hour_floor = |ms: i64| ms - ms.rem_euclid(HOUR_MS);
    assert_eq!(
        body["windows"]["7d"]["from"],
        hour_floor(now - 7 * DAY_MS),
        "the window starts where its name says; the earlier holder's row is another generation's: {body}"
    );
}

#[tokio::test]
async fn a_deleted_keys_usage_stays_with_it() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let app = phoebis_ledger::usage_router(h.db.clone(), h.router.clone(), clock_of(&h));
    // The store stamps the key from the wall clock, so the live generation is read back rather than assumed.
    let first = generation_of(&h.store, "desktop");

    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.into_body().collect().await.unwrap();
    let _ = wait_for_route(&app, KEY).await;

    h.store.delete_api_key("desktop", "test").unwrap();
    h.router.reload().await.unwrap();

    assert_eq!(
        get(&app, Some(KEY)).await.0,
        401,
        "the key is gone from the snapshot the route authenticates against"
    );

    assert!(matches!(
        h.store.insert_api_key(
            &ApiKeyRow {
                id: "desktop".into(),
                label: "desktop".into(),
                notes: None,
                pool_id: "main".into(),
                enabled: true,
                revoked_at: None,
                expires_at: None,
                allowed_models: None,
                keepalive_opt_out: false,
                body_capture_hours: None,
                secret_digest: h.store.api_key_digest("phoebis-desktop-reissued").to_vec(),
                prefix: phoebis_store::key_prefix("phoebis-desktop-reissued"),
                created_at: NOW,
                rotated_at: None,
                last_used_at: None,
            },
            "test",
        ),
        Err(StoreError::KeyIdRetired(id)) if id == "desktop"
    ));

    let now = h.clock.load(Ordering::SeqCst);
    assert_eq!(
        h.db.read(|c| key_usage_windows(c, "desktop", first, now + 1))
            .unwrap()
            .1
            .requests,
        1,
        "the history stays under an id no key can present"
    );
}

#[tokio::test]
async fn a_ledger_the_route_cannot_read_is_refused_in_the_relays_error_shape() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    // A ledger without the table the window reads is the cheapest stand-in for the disk failure this branch
    // exists for; the key still authenticates against the router's own snapshot.
    let unreadable = Arc::new(LedgerDb::open_in_memory().unwrap());
    unreadable
        .write(|c| {
            c.execute_batch("DROP TABLE rollups_hourly")?;
            Ok(())
        })
        .unwrap();
    let app = phoebis_ledger::usage_router(unreadable, h.router.clone(), clock_of(&h));

    let (status, headers, body) = get(&app, Some(KEY)).await;
    assert_eq!(status, 500);
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "api_error");
    assert_eq!(
        header(&headers, "x-phoebis-error-code"),
        Some("usage_unavailable")
    );
    assert_eq!(header(&headers, "cache-control"), Some("no-store"));
    assert!(
        body["request_id"].as_str().is_some_and(|id| !id.is_empty()),
        "the request id an operator greps for is missing: {body}"
    );
    assert_nothing_leaks(&body);
}

async fn get_with_accept(
    app: &axum::Router,
    key: &str,
    accept: Option<&str>,
) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let mut req = Request::builder()
        .method("GET")
        .uri("/v1/usage")
        .header("authorization", format!("Bearer {key}"));
    if let Some(accept) = accept {
        req = req.header("accept", accept);
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
    (status, headers, bytes.to_vec())
}

#[tokio::test]
async fn the_usage_page_is_html_when_accept_prefers_it_and_escapes_the_label() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let app = phoebis_ledger::usage_router(h.db.clone(), h.router.clone(), clock_of(&h));

    let row = h.store.api_key("desktop").unwrap().unwrap();
    h.store
        .update_api_key(
            &ApiKeyRow {
                label: "<b>Desk</b> & \"Co\"".into(),
                ..row
            },
            "test",
        )
        .unwrap();
    h.router.reload().await.unwrap();

    // A model name is operator-visible text like the label, and the table body is where the page renders it.
    let now = h.clock.load(Ordering::SeqCst);
    // The store stamps the key from the wall clock, so the live generation is read back rather than assumed.
    h.db.insert_request(
        &RequestRecord {
            canonical_model: "claude-<b>5</b>".into(),
            key_generation: generation_of(&h.store, "desktop"),
            ..request_at("m1", "desktop", now)
        },
        &[used("m1")],
        None,
    )
    .unwrap();

    let (status, headers, body) = get_with_accept(
        &app,
        KEY,
        Some("text/html,application/xhtml+xml,application/json;q=0.9"),
    )
    .await;
    assert_eq!(status, 200);
    assert!(header(&headers, "content-type")
        .unwrap()
        .starts_with("text/html"));
    assert_eq!(
        header(&headers, "content-security-policy"),
        Some("default-src 'none'; style-src 'unsafe-inline'")
    );
    assert_eq!(header(&headers, "cache-control"), Some("no-store"));
    assert_eq!(header(&headers, "x-content-type-options"), Some("nosniff"));
    let body = String::from_utf8(body).unwrap();
    assert!(body.contains("&lt;b&gt;Desk&lt;/b&gt; &amp; &quot;Co&quot;"));
    assert!(!body.contains("<b>Desk"));
    assert!(body.contains("Last 24 hours"));
    assert!(body.contains("Last 7 days"));
    assert!(!body.contains("<script"));
    assert!(!body.contains(KEY));
    assert!(
        body.contains("<td>claude-&lt;b&gt;5&lt;/b&gt;</td>"),
        "the model cell is rendered and escaped: {body}"
    );
    assert!(
        body.contains(&jiff::Timestamp::from_millisecond(now).unwrap().to_string()),
        "a page saved to a file says when it was generated: {body}"
    );

    let (status, headers, _) =
        get_with_accept(&app, KEY, Some("application/json, text/html")).await;
    assert_eq!(status, 200);
    assert!(header(&headers, "content-type")
        .unwrap()
        .starts_with("application/json"));
    assert_eq!(header(&headers, "x-content-type-options"), Some("nosniff"));

    let (status, headers, _) = get_with_accept(&app, KEY, None).await;
    assert_eq!(status, 200);
    assert!(header(&headers, "content-type")
        .unwrap()
        .starts_with("application/json"));
}
