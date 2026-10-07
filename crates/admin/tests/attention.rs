//! `GET /admin/attention`: the conditions that hold now, worst first, oldest first within a kind.
mod common;

use common::{get, json, login, post};
use phoebis_bus::{
    ClientFacts, Event, Kind, Origin, Outcome, RequestComplete, RequestObserved, RequestShape,
    ResponseFacts,
};
use phoebis_store::{AccountState, AttemptOutcome};
use serde_json::json as j;

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
        client_version: drift.map(|_| "2.1.274".to_string()),
        response_bytes: None,
        response_chunks: None,
        held: None,
        message_id: None,
        finished_at: at,
    })
}

#[tokio::test]
async fn the_held_conditions_come_worst_first_then_oldest_first_in_their_shape() {
    let parts = common::admin_parts_full().await;
    let app = &parts.app;
    let cookie = login(app, "admin-secret").await;
    post(app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority","members":[{"account_id":"business","priority":0},{"account_id":"direct","priority":1}]}"#, Some(&cookie)).await;
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
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    let run = parts
        .store
        .newest_resolved_refresh_attempts("business", 1, 16)
        .unwrap();
    assert!(run[1].started_at < run[0].started_at);
    let run_started = run.last().unwrap().started_at;
    parts
        .store
        .set_state("business", AccountState::ReauthRequired)
        .unwrap();
    parts
        .store
        .set_state("direct", AccountState::FundingRequired)
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
            "since": common::NOW,
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
}

#[tokio::test]
async fn an_api_key_the_upstream_refused_reads_as_an_account_to_reauthorize() {
    let parts = common::admin_parts_full().await;
    let app = &parts.app;
    let cookie = login(app, "admin-secret").await;
    post(app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority","members":[{"account_id":"business","priority":0},{"account_id":"direct","priority":1}]}"#, Some(&cookie)).await;
    parts
        .store
        .set_state("direct", AccountState::ReauthRequired)
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
