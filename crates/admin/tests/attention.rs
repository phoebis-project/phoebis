//! `GET /admin/attention`: the conditions that hold now, worst first, oldest first within a kind.
mod common;

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use common::{get, json, login, post};
use fake_upstream::Captured;
use http_body_util::BodyExt;
use phoebis_attention::{PairFacts, TlsPairs};
use phoebis_bus::{
    ClientFacts, Event, Kind, Origin, Outcome, RequestComplete, RequestObserved, RequestShape,
    ResponseFacts,
};
use phoebis_ledger::{
    AttemptRecord, LedgerDb, LedgerError, Metrics, Record, RequestRecord, READERS,
};
use phoebis_store::{AccountKind, AccountState, AttemptOutcome, StateCause};
use serde_json::json as j;
use tokio::task::JoinHandle;
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
        boundaries: Vec::new().into(),
        marked: Vec::new().into(),
        client: ClientFacts::default(),
        auto_mode: None,
        fork: phoebis_bus::Fork::default(),
        presented_retired: false,
        rotation_at: None,
        guardrail_id: None,
        guardrails: Vec::new(),
        received_at: 0,
        fallback_digest: None,
        cache_marker: false,
    })
}

fn complete(
    id: &str,
    key: &str,
    pool: &str,
    outcome: Outcome,
    drift: Option<&str>,
    at: i64,
) -> Event {
    Event::RequestComplete(RequestComplete {
        request_id: id.into(),
        key_id: Some(key.into()),
        pool_id: Some(pool.into()),
        origin: Origin::Client,
        kind: Kind::Inference,
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
        thinking_diagnostics: None,
        message_id: None,
        skipped_events: 0,
        interruption: None,
        classifier: None,
        finished_at: at,
    })
}

#[tokio::test]
async fn the_held_conditions_come_worst_first_then_oldest_first_in_their_shape() {
    let parts = common::admin_parts_full().await;
    let app = &parts.app;
    let cookie = login(app, "admin-secret").await;
    post(app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority","shed_cap":0.3,"five_hour_ceiling":0.9,"members":[{"account_id":"business","priority":0},{"account_id":"direct","priority":1}]}"#, Some(&cookie)).await;
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
            .resolve_refresh_attempt(
                attempt,
                1,
                AttemptOutcome::Failed {
                    cause: detail.into(),
                    detail: detail.into(),
                },
            )
            .unwrap();
        // Apart by more than a millisecond, so the run's start can only be read off the older attempt.
        parts
            .clock_cell
            .fetch_add(2, std::sync::atomic::Ordering::SeqCst);
    }
    let run = parts
        .store
        .refresh_histories(&[("business", 1)], 16)
        .unwrap()
        .remove("business")
        .unwrap()
        .attempts;
    assert!(run[1].started_at < run[0].started_at);
    let run_started = run.last().unwrap().started_at;
    let marked_at = parts.clock_cell.load(std::sync::atomic::Ordering::SeqCst);
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
            .observe(&complete(id, "laptop", pool, Outcome::Exhausted, None, at));
    }
    parts
        .attention
        .observe(&observed("r-drift", "laptop", "main"));
    parts.attention.observe(&complete(
        "r-drift",
        "laptop",
        "main",
        Outcome::Rejected,
        Some("hash"),
        later,
    ));
    common::park_unreadable(&parts.db, "main", "business", later - 1_000);
    // The four routing conditions as the ledger's fold writes them, each older than everything above, which they
    // still rank after: none of them stops traffic.
    let condition = |kind: phoebis_ledger::RoutingKind,
                     subject_kind: &str,
                     subject: &str,
                     pool: Option<&str>,
                     detail| {
        phoebis_ledger::RoutingCondition {
            kind,
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
                phoebis_ledger::RoutingKind::ShedNoRunway,
                "pool",
                "main",
                Some("main"),
                j!({
                    "accounts": ["business"],
                    "rounds": 1,
                    "setting": ["five_hour_ceiling", "weekly_ceiling"],
                }),
            ),
            condition(
                phoebis_ledger::RoutingKind::DestinationSpent,
                "account",
                "business",
                None,
                j!({
                    "fits": { "rewrite_only": 1 },
                    "moves": 1,
                    "setting": ["five_hour_ceiling"],
                    "windows": ["five_hour"],
                }),
            ),
            condition(
                phoebis_ledger::RoutingKind::WallReached,
                "account",
                "business",
                None,
                j!({
                    "projection_error": null,
                    "setting": ["shed_cap", "five_hour_ceiling"],
                    "walls": 1,
                    "windows": ["five_hour"],
                }),
            ),
        ],
    )
    .await;
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
            "since": marked_at,
            "detail": { "account": "business", "label": "Business" },
        }),
        "the item dates from the instant the registry recorded for the change"
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
                "setting": ["five_hour_ceiling", "weekly_ceiling"],
                "setting_values": { "five_hour_ceiling": 0.9, "weekly_ceiling": 1.0 },
            },
        }),
        "a pool's condition carries the pool's ceilings beside the settings it names"
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
            Some(&j!({ "shed_cap": 0.3, "five_hour_ceiling": 0.9 }))
        ),
        "an account's condition opens the account and names its settings with the values of the account's pool"
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
async fn a_bus_gap_of_the_running_engine_raises_the_keepalive_item_with_its_count_and_instant() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let now = parts.clock_cell.load(Ordering::SeqCst);
    parts.keepalive.bus_gap(1, 0);
    let body = json(get(&parts.app, "/admin/attention", Some(&cookie)).await).await;
    assert_eq!(
        body,
        j!({ "items": [{
            "kind": "keepalive_bus_gaps",
            "subject": { "type": "router", "id": "keepalive", "label": "Keepalive" },
            "link": null,
            "pools": [],
            "since": now,
            "detail": { "gaps": 1, "last_at": now },
        }] }),
        "the route reads the gap off the engine the harness runs, dated by the engine's clock"
    );
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

/// Panics where the reader asks for the listener pairs, which it does on its blocking worker, so the worker ends with
/// no answer.
struct PanickingPairs;

impl TlsPairs for PanickingPairs {
    fn pairs(&self) -> Vec<PairFacts> {
        panic!("the listener pairs could not be read")
    }
}

#[tokio::test]
async fn a_read_whose_blocking_worker_dies_logs_the_failed_read_as_canceled_and_answers_500() {
    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(captured.clone())
        .finish();
    // The harness drives every request through `oneshot` on this test's own task, so a thread-local subscriber sees
    // the route's line.
    let _guard = tracing::subscriber::set_default(subscriber);
    let parts = common::admin_parts_full_with_pairs(Arc::new(PanickingPairs)).await;
    let cookie = login(&parts.app, "admin-secret").await;
    // Another case of this file can register the line's callsite as unwanted on a thread with no subscriber while this
    // case's subscriber is being set. A first read registers it here, so a rebuild then reads every subscriber's
    // interest again before the read the case checks.
    let _ = get(&parts.app, "/admin/attention", Some(&cookie)).await;
    tracing::callsite::rebuild_interest_cache();
    let before = captured.text().len();
    let response = get(&parts.app, "/admin/attention", Some(&cookie)).await;
    assert_eq!(
        response.status(),
        500,
        "an empty list would read as the quiet state"
    );
    assert_eq!(json(response).await, j!({ "error": "storage failure" }));
    let text = captured.text()[before..].to_string();
    assert!(
        text.lines().any(|line| line
            .contains("the attention read failed, so the route answers 500")
            && line.contains("error=the attention read did not finish")),
        "{text}"
    );
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

/// The route and the metrics listener read one reader, so the unpriced model's item lists on the route as its series
/// reads on the scrape.
#[tokio::test]
async fn a_held_model_unpriced_condition_lists_on_the_route_after_every_degraded_kind() {
    // The test cost model leaves `claude-opus-5` without a weight at the rig's clock, so the case reads no shipped
    // figure.
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
                account_kind: AccountKind::Oauth,
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
            .conditions_blocking(common::NOW)
            .unwrap()
            .iter()
            .any(|c| c.kind == phoebis_attention::Kind::ModelUnpriced && c.holds)
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
        ["funding_required", "model_unpriced"],
        "the model's item lists after every degraded kind: {body}"
    );
    let item = &body["items"][1];
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
            &j!(null),
            &j!([]),
            &j!(common::NOW),
            &j!({ "model": "claude-opus-5", "requests": 1 }),
        ),
        "the item opens no page, belongs to no pool and dates from the request: {item}"
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
        text.contains(r#"phoebis_attention{kind="model_unpriced",subject="claude-opus-5"} 1"#),
        "{text}"
    );
}

/// A read that holds its pooled reader until its release is sent.
type Hold = JoinHandle<Result<(), LedgerError>>;

/// Holds every pooled reader at once, each read signalling its take and then waiting on its own release. Each hold
/// must take its reader within a second, so a harness without the pool fails here instead of hanging.
async fn hold_every_reader(db: &Arc<LedgerDb>) -> (Vec<std::sync::mpsc::Sender<()>>, Vec<Hold>) {
    let (taken, mut takes) = tokio::sync::mpsc::channel::<()>(READERS);
    let mut releases = Vec::new();
    let mut held = Vec::new();
    for _ in 0..READERS {
        let (release, released) = std::sync::mpsc::channel::<()>();
        let taken = taken.clone();
        held.push(tokio::spawn({
            let db = db.clone();
            async move {
                db.read(move |_| {
                    taken.blocking_send(()).unwrap();
                    released.recv().unwrap();
                    Ok(())
                })
                .await
            }
        }));
        releases.push(release);
    }
    for _ in 0..READERS {
        tokio::time::timeout(Duration::from_secs(1), takes.recv())
            .await
            .expect(
                "a hold took no reader within a second, likely on the in-memory harness, whose one connection \
                 serves one read at a time",
            )
            .unwrap();
    }
    (releases, held)
}

/// A read held on every pooled connection leaves the route's read waiting on the runtime for a connection, holding no
/// worker: the runtime thread stays free to run the release, after which the route answers. A handler that blocks the
/// runtime thread instead is released by the watchdog alone, which its flag records.
#[tokio::test]
async fn the_attention_route_answers_while_every_pooled_reader_is_held() {
    let (_dir, parts) = common::admin_parts_full_on_disk().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let (releases, held) = hold_every_reader(&parts.db).await;
    let releases = Arc::new(Mutex::new(releases));
    let (done, fired) = watchdog(releases.clone(), Duration::from_secs(5));
    tokio::spawn({
        let releases = releases.clone();
        async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            for release in releases.lock().unwrap().drain(..) {
                release.send(()).unwrap();
            }
        }
    });
    let response = get(&parts.app, "/admin/attention", Some(&cookie)).await;
    done.send(()).unwrap();
    assert!(
        !fired.load(Ordering::SeqCst),
        "the route parked the runtime thread, so the watchdog released the readers"
    );
    assert_eq!(response.status(), 200);
    for task in held {
        task.await.unwrap().unwrap();
    }
}

/// Off the runtime, so a handler that blocks the runtime thread cannot stall it. After `bound` without `done` it
/// releases every held reader, which lets the blocked read finish and the test reach its assertion, and sets the
/// flag that assertion reads. A test that ends before then, through `done` or through its own panic, lets the thread
/// exit.
fn watchdog(
    releases: Arc<Mutex<Vec<std::sync::mpsc::Sender<()>>>>,
    bound: Duration,
) -> (std::sync::mpsc::Sender<()>, Arc<AtomicBool>) {
    use std::sync::mpsc::RecvTimeoutError::Timeout;
    let (done, finished) = std::sync::mpsc::channel::<()>();
    let fired = Arc::new(AtomicBool::new(false));
    std::thread::spawn({
        let fired = fired.clone();
        move || {
            if finished.recv_timeout(bound) != Err(Timeout) {
                return;
            }
            fired.store(true, Ordering::SeqCst);
            for release in releases.lock().unwrap().drain(..) {
                let _ = release.send(());
            }
            // Released and still no answer after a second bound: nothing on the runtime can end the test, so the
            // process ends with the cause in its output rather than hanging the run.
            if finished.recv_timeout(bound) == Err(Timeout) {
                eprintln!(
                    "the held readers were released {bound:?} ago and the route has not answered"
                );
                std::process::abort();
            }
        }
    });
    (done, fired)
}

/// Polls `step` once on the test's own thread, so its synchronous part runs up to its first wait before the next step
/// starts. A spawned task would run on the runtime's worker instead, where the next step could overtake it.
async fn reach_wait(mut step: Pin<&mut impl Future>) {
    std::future::poll_fn(|cx| {
        assert!(
            step.as_mut().poll(cx).is_pending(),
            "the step finished while the one blocking worker was still held"
        );
        Poll::Ready(())
    })
    .await;
}

/// The route's read holds no blocking worker while the pool is out. The runtime gets one blocking worker, held on a
/// gate. The route's read starts first and takes a connection, its closure queued behind the gate, then `READERS`
/// reads take the other connections or wait for one, their closures queued behind the route's, and then the gate
/// opens. A route that waits for its connection on a blocking worker instead takes the one worker ahead of the
/// closures that would return a connection, so it waits for good and never answers. That order is what deadlocks
/// such a route, so the steps keep it.
#[test]
fn the_attention_route_answers_when_queued_reads_hold_the_pool_behind_its_job() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let status = runtime.block_on(async {
        let (_dir, parts) = common::admin_parts_full_on_disk().await;
        let cookie = login(&parts.app, "admin-secret").await;
        let (open, gate) = std::sync::mpsc::channel::<()>();
        let _held = tokio::task::spawn_blocking(move || gate.recv());
        let mut request = std::pin::pin!(get(&parts.app, "/admin/attention", Some(&cookie)));
        reach_wait(request.as_mut()).await;
        let mut reads: Vec<_> = (0..READERS)
            .map(|_| Box::pin(parts.db.read(|_| Ok(()))))
            .collect();
        for read in &mut reads {
            reach_wait(read.as_mut()).await;
        }
        open.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), request)
            .await
            .map(|response| response.status().as_u16())
    });
    // A runtime's drop waits for its blocking jobs without a bound, and a route that deadlocked leaves one that never
    // ends.
    runtime.shutdown_timeout(Duration::from_secs(1));
    assert_eq!(
        status.ok(),
        Some(200),
        "the route waited for a pooled reader on the one blocking worker the queued reads needed to return theirs"
    );
}
