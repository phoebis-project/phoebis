//! `GET /admin/attention`: the conditions that hold now, worst first, oldest first within a kind.
mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use common::{get, json, login, post};
use http_body_util::BodyExt;
use phoebis_bus::{
    ClientFacts, Event, Kind, Origin, Outcome, RequestComplete, RequestObserved, RequestShape,
    ResponseFacts,
};
use phoebis_ledger::{AttemptRecord, Metrics, Record, RequestRecord};
use phoebis_store::{AccountState, AttemptOutcome, StateCause};
use serde_json::json as j;
use tower::ServiceExt;

fn observed(id: &str, key: &str, pool: &str) -> Event {
    Event::RequestObserved(RequestObserved {
        request_id: id.into(),
        origin: Origin::Client,
        kind: Kind::Inference,
        key_id: Some(key.into()),
        key_generation: 0,
        pool_id: Some(pool.into()),
        session_id: Some("s1".into()),
        agent_id: Some("main".into()),
        parent_agent_id: None,
        owner_id: None,
        model: "claude-sonnet-5".into(),
        canonical_model: "claude-sonnet-5".into(),
        wire_model: "claude-sonnet-5".into(),
        stream: false,
        headers: vec![],
        body: bytes::Bytes::from_static(b"{}"),
        body_capture_hours: 0,
        shape: RequestShape::default(),
        static_hash: None,
        boundaries: Vec::new(),
        client: ClientFacts::default(),
        auto_mode: None,
        fork: phoebis_bus::Fork::default(),
        presented_retired: false,
        rotation_at: None,
        guardrail_id: None,
        guardrails: Vec::new(),
        received_at: 0,
    })
}

fn complete(id: &str, outcome: Outcome, drift: Option<&str>, at: i64) -> Event {
    Event::RequestComplete(RequestComplete {
        request_id: id.into(),
        attempt: 0,
        account_id: None,
        outcome,
        status: 429,
        usage: None,
        response: ResponseFacts::default(),
        ttfb_ms: None,
        ttfc_ms: None,
        error_code: None,
        client_drift: drift.map(String::from),
        client_contract: None,
        client_version: drift.map(|_| "2.1.274".to_string()),
        response_bytes: None,
        response_chunks: None,
        held: None,
        diagnostics: None,
        message_id: None,
        skipped_events: 0,
        interruption: None,
        finished_at: at,
    })
}

#[tokio::test]
async fn the_held_conditions_come_worst_first_then_oldest_first_in_their_shape() {
    let parts = common::admin_parts_full().await;
    let app = &parts.app;
    let cookie = login(app, "admin-secret").await;
    post(app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority","window_ceiling":0.9,"members":[{"account_id":"business","priority":0},{"account_id":"direct","priority":1}]}"#, Some(&cookie)).await;
    post(
        app,
        "/admin/pools",
        r#"{"id":"spare","label":"Spare","strategy":"priority"}"#,
        Some(&cookie),
    )
    .await;
    post(
        app,
        "/admin/keys",
        r#"{"id":"laptop","label":"Laptop","pool_id":"main"}"#,
        Some(&cookie),
    )
    .await;
    for detail in ["first", "invalid_grant"] {
        let attempt = parts
            .store
            .open_refresh_attempt("business", 1, 1, 1)
            .unwrap();
        parts
            .store
            .resolve_refresh_attempt(attempt, 1, AttemptOutcome::Failed(detail.into()))
            .unwrap();
        // Apart by more than a millisecond, so the run's start can only be read off the older attempt.
        parts
            .clock_cell
            .fetch_add(2, std::sync::atomic::Ordering::SeqCst);
    }
    let run = parts
        .store
        .newest_resolved_refresh_attempts("business", 1, 16)
        .unwrap();
    assert!(run[1].started_at < run[0].started_at);
    let run_started = run.last().unwrap().started_at;
    parts
        .store
        .set_state(
            "business",
            AccountState::ReauthRequired,
            StateCause::Operator,
        )
        .unwrap();
    parts
        .store
        .set_state(
            "direct",
            AccountState::FundingRequired,
            StateCause::Operator,
        )
        .unwrap();
    // Both exhaustions are newer than the refresh run, which still ranks after them; the spare pool's is older.
    let later = run_started + 60_000;
    for (id, pool, at) in [
        ("r-main", "main", later + 2_000),
        ("r-spare", "spare", later + 1_000),
    ] {
        parts.attention.observe(&observed(id, "laptop", pool));
        parts
            .attention
            .observe(&complete(id, Outcome::Exhausted, None, at));
    }
    parts
        .attention
        .observe(&observed("r-drift", "laptop", "main"));
    parts
        .attention
        .observe(&complete("r-drift", Outcome::Rejected, Some("hash"), later));
    common::park_unreadable(&parts.db, "main", "business", later - 1_000);
    // The four routing conditions as the ledger's fold writes them, each older than everything above, which they
    // still rank after: none of them stops traffic.
    let condition = |kind: &str, subject_kind: &str, subject: &str, pool: Option<&str>, detail| {
        phoebis_ledger::RoutingCondition {
            kind: kind.into(),
            subject_kind: subject_kind.into(),
            subject_id: subject.into(),
            pool_id: pool.map(String::from),
            since: run_started - 60_000,
            count: 1,
            detail,
            computed_at: later,
        }
    };
    common::seed_conditions(
        &parts,
        &[
            condition(
                "shed_no_runway",
                "pool",
                "main",
                Some("main"),
                j!({ "accounts": ["business"], "rounds": 1, "setting": ["window_ceiling"] }),
            ),
            condition(
                "destination_spent",
                "account",
                "business",
                None,
                j!({
                    "fits": { "rewrite_only": 1 },
                    "moves": 1,
                    "setting": ["window_ceiling"],
                    "windows": ["five_hour"],
                }),
            ),
            condition(
                "wall_reached",
                "account",
                "business",
                None,
                j!({
                    "projection_error": null,
                    "setting": ["shed_cap", "window_ceiling"],
                    "walls": 1,
                    "windows": ["five_hour"],
                }),
            ),
        ],
    )
    .await;
    let first_read = parts.clock_cell.load(std::sync::atomic::Ordering::SeqCst);
    let body = json(get(app, "/admin/attention", Some(&cookie)).await).await;
    let items = body["items"].as_array().unwrap();
    assert_eq!(
        items
            .iter()
            .map(|i| (
                i["kind"].as_str().unwrap(),
                i["subject"]["id"].as_str().unwrap()
            ))
            .collect::<Vec<_>>(),
        [
            ("pool_exhausted", "spare"),
            ("pool_exhausted", "main"),
            ("client_drift", "2.1.274"),
            ("reauth_required", "business"),
            ("funding_required", "direct"),
            ("refresh_failed", "business"),
            ("ledger_record_parked", "main"),
            ("wall_reached", "business"),
            ("destination_spent", "business"),
            ("shed_no_runway", "main"),
        ],
        "{body}"
    );
    assert_eq!(
        items[0],
        j!({
            "kind": "pool_exhausted",
            "subject": { "type": "pool", "id": "spare", "label": "Spare" },
            "link": { "page": "pools", "id": "spare" },
            "pools": ["spare"],
            "since": later + 1_000,
            "detail": { "key": "laptop", "pool": "spare" },
        })
    );
    assert_eq!(
        items[2],
        j!({
            "kind": "client_drift",
            "subject": { "type": "client", "id": "2.1.274", "label": "2.1.274" },
            "link": { "page": "keys", "id": "laptop" },
            "pools": ["main"],
            "since": later,
            "detail": { "client_version": "2.1.274", "key": "laptop", "trigger": "hash" },
        }),
        "a drift item opens the refused request's key and belongs to that key's pool"
    );
    assert_eq!(
        items[3],
        j!({
            "kind": "reauth_required",
            "subject": { "type": "account", "id": "business", "label": "Business" },
            "link": { "page": "accounts", "id": "business" },
            "pools": ["main"],
            "since": first_read,
            "detail": { "account": "business", "label": "Business" },
        }),
        "the registry records no time for a state change, so the first read that saw it dates it"
    );
    assert_eq!(
        items[4]["pools"],
        j!(["main"]),
        "an account item lists the pool the account is a member of"
    );
    assert_eq!(
        (&items[5]["since"], &items[5]["detail"]),
        (
            &j!(run_started),
            &j!({ "account": "business", "failures": 2, "last_error": "invalid_grant", "outcome": "failed" })
        ),
        "the run dates from its first failed attempt"
    );
    assert_eq!(
        items[6],
        j!({
            "kind": "ledger_record_parked",
            "subject": { "type": "pool", "id": "main", "label": "Main" },
            "link": { "page": "pools", "id": "main" },
            "pools": ["main"],
            "since": later - 1_000,
            "detail": { "accounts": ["business"], "count": 1, "pool": "main" },
        }),
        "a parked record opens its pool's card and dates from the oldest row"
    );
    assert_eq!(
        items[9],
        j!({
            "kind": "shed_no_runway",
            "subject": { "type": "pool", "id": "main", "label": "Main" },
            "link": { "page": "pools", "id": "main" },
            "pools": ["main"],
            "since": run_started - 60_000,
            "detail": {
                "accounts": ["business"],
                "rounds": 1,
                "setting": ["window_ceiling"],
                "setting_values": { "window_ceiling": 0.9 },
            },
        }),
        "a pool's condition carries the pool's ceiling beside the setting it names"
    );
    assert_eq!(
        (
            &items[7]["link"],
            &items[7]["pools"],
            items[7]["detail"].get("setting_values")
        ),
        (
            &j!({ "page": "accounts", "id": "business" }),
            &j!(["main"]),
            None
        ),
        "an account's condition opens the account and names its setting with no pool's value"
    );
}

#[tokio::test]
async fn an_api_key_the_upstream_refused_reads_as_an_account_to_reauthorize() {
    let parts = common::admin_parts_full().await;
    let app = &parts.app;
    let cookie = login(app, "admin-secret").await;
    post(app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority","members":[{"account_id":"business","priority":0},{"account_id":"direct","priority":1}]}"#, Some(&cookie)).await;
    parts
        .store
        .set_state("direct", AccountState::ReauthRequired, StateCause::Operator)
        .unwrap();
    let body = json(get(app, "/admin/attention", Some(&cookie)).await).await;
    assert_eq!(
        body,
        j!({ "items": [{
            "kind": "reauth_required",
            "subject": { "type": "account", "id": "direct", "label": "Direct" },
            "link": { "page": "accounts", "id": "direct" },
            "pools": ["main"],
            "since": common::NOW,
            "detail": { "account": "direct", "label": "Direct" },
        }] })
    );
}

#[tokio::test]
async fn nothing_held_answers_an_empty_list() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    // The harness's accounts read clear: the OAuth slot unpaired, never refreshed and never polled, the key slot
    // ready; no pool exists, and no request has gone by.
    let accounts = parts.store.accounts().unwrap();
    assert!(accounts
        .iter()
        .all(|a| matches!(a.state, AccountState::Unpaired | AccountState::Ready)));
    let response = get(&parts.app, "/admin/attention", Some(&cookie)).await;
    assert_eq!(response.status(), 200);
    assert_eq!(json(response).await, j!({ "items": [] }));
}

#[tokio::test]
async fn a_failed_holder_read_answers_500_rather_than_an_empty_list() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    parts.store.debug_fail_next_refresh_read();
    let response = get(&parts.app, "/admin/attention", Some(&cookie)).await;
    assert_eq!(
        response.status(),
        500,
        "an empty list would read as the quiet state"
    );
    assert_eq!(json(response).await, j!({ "error": "storage failure" }));
    let response = get(&parts.app, "/admin/attention", Some(&cookie)).await;
    assert_eq!(response.status(), 200, "the next read answers again");
}

#[tokio::test]
async fn a_failed_parked_read_answers_500_as_a_failed_holder_read_does() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    parts
        .db
        .write(|c| {
            c.execute_batch("DROP TABLE parked_records;")?;
            Ok(())
        })
        .unwrap();
    let response = get(&parts.app, "/admin/attention", Some(&cookie)).await;
    assert_eq!(
        response.status(),
        500,
        "an empty list would read as the quiet state"
    );
    assert_eq!(json(response).await, j!({ "error": "storage failure" }));
}

#[tokio::test]
async fn the_route_sits_behind_the_admin_session() {
    let parts = common::admin_parts_full().await;
    let response = get(&parts.app, "/admin/attention", None).await;
    assert_eq!(response.status(), 401);
}

#[tokio::test]
async fn the_system_route_answers_without_an_engine_stats_member() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let system = json(get(&parts.app, "/admin/system", Some(&cookie)).await).await;
    assert!(system["keepalive"].is_object(), "{system}");
    assert!(system.get("alerts").is_none(), "{system}"); // removed name
}

/// The route and the metrics listener read one reader, so the unweighted model's item lists on the route as its series
/// reads on the scrape.
#[tokio::test]
async fn a_held_model_unweighted_condition_lists_on_the_route_after_every_degraded_kind() {
    // The test cost model leaves `claude-opus-5` unweighted under the `opus` model tier at the rig's clock, so the
    // case reads no shipped figure.
    let parts =
        common::admin_parts_full_costed(phoebis_ledger::test_tables::subscription_costs()).await;
    let cookie = login(&parts.app, "admin-secret").await;
    parts
        .store
        .set_state(
            "direct",
            AccountState::FundingRequired,
            StateCause::Operator,
        )
        .unwrap();
    parts
        .ledger
        .record_request(Record::Request {
            request: RequestRecord {
                id: "r1".into(),
                received_at: common::NOW,
                finished_at: Some(common::NOW + 500),
                model: "claude-opus-5".into(),
                canonical_model: "claude-opus-5".into(),
                kind: "inference".into(),
                origin: "client".into(),
                outcome: "succeeded".into(),
                attempts: 1,
                ..Default::default()
            },
            attempts: vec![AttemptRecord {
                request_id: "r1".into(),
                attempt: 1,
                account_id: "business".into(),
                account_kind: "oauth".into(),
                outcome: "succeeded".into(),
                input_tokens: Some(1_000),
                ..Default::default()
            }],
            bodies: None,
        })
        .await;
    fake_upstream::wait_for(Duration::from_secs(5), || {
        parts
            .attention
            .conditions(common::NOW)
            .unwrap()
            .iter()
            .any(|c| c.kind == phoebis_attention::Kind::ModelUnweighted && c.holds)
            .then_some(())
    })
    .await;
    let body = json(get(&parts.app, "/admin/attention", Some(&cookie)).await).await;
    // The kind ranks after every degraded kind in the reader's order, which the attention crate's tests pin, so the
    // degraded account seeded beside it is enough to read the item after a degraded kind.
    assert_eq!(
        body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["kind"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["funding_required", "model_unweighted"],
        "the model's item lists after every degraded kind: {body}"
    );
    let item = &body["items"][1];
    let costs = phoebis_ledger::test_tables::subscription_costs();
    let weight_of = costs
        .revisions
        .in_force(common::NOW)
        .latest_weighted("opus")
        .expect("the test cost model weights an opus model");
    assert_eq!(
        (
            &item["subject"],
            &item["link"],
            &item["pools"],
            &item["since"],
            &item["detail"]
        ),
        (
            &j!({ "type": "model", "id": "claude-opus-5", "label": "claude-opus-5" }),
            &j!({ "page": "models", "id": "claude-opus-5" }),
            &j!([]),
            &j!(common::NOW),
            &j!({ "model": "claude-opus-5", "requests": 1, "tier": "opus", "weight_of": weight_of }),
        ),
        "the item opens the Models page on the model, belongs to no pool and dates from the request: {item}"
    );
    let metrics = phoebis_ledger::metrics_router(
        Arc::new(Metrics::new()),
        parts.router.clone(),
        parts.pricing.clone(),
        parts.bus.clone(),
        Arc::new(|| common::NOW),
        Duration::from_secs(60),
        None,
        Some(parts.attention.gauge_source()),
        Some(parts.ledger.clone()),
    );
    let response = metrics
        .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let text = String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(
        text.contains(r#"phoebis_attention{kind="model_unweighted",subject="claude-opus-5"} 1"#),
        "{text}"
    );
}
