//! `key_usage` sums one key's rollups alone, probes left out, and the route it backs answers only the caller's
//! own key, a deleted key's usage staying with the retired id.
mod common;

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use phoebis_ledger::{
    key_usage, key_usage_windows, AttemptRecord, KeyUsageWindow, LedgerConfig, LedgerDb,
    RequestRecord, KEY_RANGE_PREDICATE,
};
use phoebis_store::{ApiKeyRow, StoreError};
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
fn key_usage_sums_one_keys_rollups_and_leaves_probes_and_other_keys_out() {
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

    let k1 = db.read(|c| key_usage(c, "k1", 0, 10_000)).unwrap();
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

    assert_eq!(
        db.read(|c| key_usage(c, "k2", 0, 10_000)).unwrap().requests,
        1
    );

    let k3 = db.read(|c| key_usage(c, "k3", 0, 10_000)).unwrap();
    assert_eq!(
        k3,
        KeyUsageWindow {
            from: 0,
            to: 10_000,
            ..KeyUsageWindow::default()
        }
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

    // A key as old as the rows themselves, so the creation bound narrows neither window here.
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
    assert_eq!(
        day,
        db.read(|c| key_usage(c, "k1", now - DAY_MS, now)).unwrap(),
        "the derived window is the one a direct read of the same range answers"
    );
}

#[test]
fn the_windows_leave_out_the_hour_the_key_was_created_in() {
    let db = LedgerDb::open_in_memory().unwrap();
    let now = 30 * DAY_MS + HOUR_MS + 1_234;
    // Two hours under one id: the earlier one is the bucket a recreated key would share with the holder
    // before it, the later one is wholly the new key's.
    for (id, at) in [("r1", now - 2 * HOUR_MS), ("r2", now - HOUR_MS)] {
        db.insert_request(&request_at(id, "k1", at), &[used(id)], None)
            .unwrap();
    }

    let (day, week) = db.read(|c| key_usage_windows(c, "k1", now, now)).unwrap();
    assert_eq!(
        (day.requests, week.requests),
        (0, 0),
        "a key created after every row under its id holds none of them"
    );

    let created = now - 2 * HOUR_MS + 60_000;
    let (day, week) = db
        .read(|c| key_usage_windows(c, "k1", created, now))
        .unwrap();
    assert_eq!(
        (day.requests, week.requests),
        (1, 1),
        "the hour the key was created in is left out whole, the hour after it is the key's own"
    );
    assert_eq!(
        (day.from, week.from),
        (hour_ceil_ms(created), hour_ceil_ms(created)),
        "each window reports the bound it started at"
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
            let rows = stmt.query_map(("k1", 0_i64, 1_i64), |r| r.get::<_, String>(3))?;
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
    start_inside_the_lifetime_of(&h, &["desktop"]);

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
    start_inside_the_lifetime_of(&h, &["desktop", "laptop"]);

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

/// The store stamps a key's creation from the host clock while this suite drives its own, and a window starts
/// at the key's creation, so the clock moves to the first whole hour after every key named here was created;
/// what is recorded from there lies inside the lifetime the route reads.
fn start_inside_the_lifetime_of(h: &Harness, keys: &[&str]) -> i64 {
    let created = keys
        .iter()
        .map(|id| h.store.api_key(id).unwrap().unwrap().created_at)
        .max()
        .expect("a key to start after");
    // A second into that hour rather than on its edge: a window is summed up to the hour boundary the
    // clock stands on, so a row recorded exactly there falls outside it.
    let at = (created.div_euclid(HOUR_MS) + 1) * HOUR_MS + 1_000;
    h.clock.store(at, Ordering::SeqCst);
    at
}

/// The hour boundary at or after `ms`, in milliseconds: the bound a window reports when the key's own
/// creation is where it starts.
fn hour_ceil_ms(ms: i64) -> i64 {
    (ms.div_euclid(HOUR_MS) + i64::from(ms.rem_euclid(HOUR_MS) != 0)) * HOUR_MS
}

#[tokio::test]
async fn the_route_reads_no_further_back_than_the_key_that_presented_the_secret() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let app = phoebis_ledger::usage_router(h.db.clone(), h.router.clone(), clock_of(&h));

    // A row filed under the id before the key that holds it now was created. An id deleted and recreated
    // before the store retired ids for good leaves exactly this, and a rollup names no holder of its own.
    let created_at = h.store.api_key("desktop").unwrap().unwrap().created_at;
    h.db.insert_request(
        &request_at("r1", "desktop", created_at - HOUR_MS),
        &[used("r1")],
        None,
    )
    .unwrap();
    h.clock.store(created_at + 60_000, Ordering::SeqCst);

    let (status, _, body) = get(&app, Some(KEY)).await;
    assert_eq!(status, 200);
    assert_eq!(
        body["windows"]["24h"]["requests"], 0,
        "an hour before this key existed is the previous holder's: {body}"
    );
    assert_eq!(body["windows"]["7d"]["requests"], 0, "{body}");

    // Its own traffic, from the first whole hour of its lifetime on, is answered.
    let now = start_inside_the_lifetime_of(&h, &["desktop"]);
    h.db.insert_request(&request_at("r2", "desktop", now), &[used("r2")], None)
        .unwrap();
    let (_, _, body) = get(&app, Some(KEY)).await;
    assert_eq!(body["windows"]["24h"]["requests"], 1, "{body}");
    assert_eq!(
        body["windows"]["7d"]["from"],
        hour_ceil_ms(created_at),
        "the window says it starts where the key does: {body}"
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
    start_inside_the_lifetime_of(&h, &["desktop"]);

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
        h.db.read(|c| key_usage(c, "desktop", 0, now + 1))
            .unwrap()
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
    start_inside_the_lifetime_of(&h, &["desktop"]);

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
    h.db.insert_request(
        &RequestRecord {
            canonical_model: "claude-<b>5</b>".into(),
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
