//! Pricing, settings, system, quota, traffic, and sessions.
mod common;
use std::time::Duration;

use common::{delete, get, json, login, post, priced_row, wait_json};
use fake_upstream::{stays_for, wait_for};
use phoebis_bus::Delivery;
use phoebis_ledger::{AttemptRecord, EventRecord, ParkedRecord, Record, RequestRecord};
use phoebis_store::{AccountKind, AccountRow, AccountState, StateCause};

const HOUR: i64 = 3_600_000;

fn record(id: &str, at: i64, model: &str, session: &str) -> Record {
    record_at(id, at, model, session, 0)
}

/// `key_generation` matters only to a caller that also plants a pin: the sessions view groups a request row
/// under it, and a row left at the default of 0 never joins a pin under the key's real generation.
fn record_at(id: &str, at: i64, model: &str, session: &str, key_generation: i64) -> Record {
    Record::Request {
        request: RequestRecord {
            id: id.into(),
            received_at: at,
            finished_at: Some(at + 500),
            key_id: Some("desktop".into()),
            key_generation,
            pool_id: Some("main".into()),
            session_id: Some(session.into()),
            model: model.into(),
            canonical_model: model.into(),
            kind: "inference".into(),
            origin: "client".into(),
            status: Some(200),
            outcome: "succeeded".into(),
            attempts: 1,
            ..Default::default()
        },
        attempts: vec![AttemptRecord {
            request_id: id.into(),
            attempt: 1,
            account_id: "business".into(),
            account_kind: "oauth".into(),
            started_at: at,
            outcome: "succeeded".into(),
            input_tokens: Some(1_000),
            output_tokens: Some(100),
            ..Default::default()
        }],
        bodies: None,
    }
}

/// A priced row's dollars, zero for a model the revision lacks.
fn usd_of(priced: &phoebis_ledger::Priced) -> f64 {
    priced.usd.as_ref().map_or(0.0, |usd| usd.total())
}

/// A priced row's 20x equivalent on `window`, zero for a billed row or a window it could not price.
fn equivalent_of(priced: &phoebis_ledger::Priced, window: &str) -> f64 {
    priced
        .equivalent
        .as_ref()
        .and_then(|windows| windows.get(window).copied().flatten())
        .unwrap_or(0.0)
}

fn close_to(got: &serde_json::Value, want: f64) -> bool {
    got.as_f64()
        .is_some_and(|got| (got - want).abs() <= 1e-12 + want.abs() * 1e-9)
}

/// A JSON object's keys as the body orders them; comparing two `Value`s ignores the order.
fn keys_in_order(object: &serde_json::Value) -> Vec<&str> {
    object
        .as_object()
        .map(|members| members.keys().map(String::as_str).collect())
        .unwrap_or_default()
}

#[tokio::test]
async fn settings_and_system_show_effective_values_ledger_health_and_gaps_without_secrets() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let settings = json(get(&parts.app, "/admin/settings", Some(&cookie)).await).await;
    let keys_of = |member: &serde_json::Value| {
        let mut keys: Vec<String> = member
            .as_object()
            .map(|body| body.keys().cloned().collect())
            .unwrap_or_default();
        keys.sort();
        keys
    };
    let mut members: Vec<(String, Vec<String>)> = settings
        .as_object()
        .map(|top| {
            top.iter()
                .map(|(name, body)| (name.clone(), keys_of(body)))
                .collect()
        })
        .unwrap_or_default();
    members.sort();
    let expected: Vec<(String, Vec<String>)> = [
        ("about", &["profile", "version"][..]),
        ("access", &["admin_configured"]),
        (
            "api_prices",
            &["content", "effective", "revisions", "source"],
        ),
        ("listeners", &["metrics", "proxy", "proxy_tls"]),
        ("retention", &["body_capture_max_hours"]),
        ("sessions", &["expire_after_seconds"]),
        (
            "storage",
            &[
                "api_prices_path",
                "ledger_path",
                "path",
                "subscription_costs_path",
            ],
        ),
        (
            "subscription_costs",
            &["content", "effective", "revisions", "source"],
        ),
    ]
    .iter()
    .map(|(name, keys)| {
        (
            name.to_string(),
            keys.iter().map(|key| key.to_string()).collect(),
        )
    })
    .collect();
    assert_eq!(
        members, expected,
        "the route answers what the console reads and no other member: {settings}"
    );
    assert_eq!(
        (
            settings["retention"]["body_capture_max_hours"].as_u64(),
            settings["sessions"]["expire_after_seconds"].as_u64(),
            settings["listeners"]["metrics"].as_str(),
            settings["listeners"]["proxy_tls"].as_bool(),
        ),
        (Some(72), Some(86_400), Some("127.0.0.1:9464"), Some(false))
    );
    let revisions = phoebis_ledger::PriceRevisions::shipped();
    let shipped = revisions.in_force(common::NOW);
    let prices = &settings["api_prices"];
    assert_eq!(
        (&prices["source"], &prices["effective"]),
        (
            &serde_json::json!("shipped"),
            &serde_json::json!(shipped.effective)
        )
    );
    assert_eq!(
        prices["content"],
        serde_json::json!({"effective": shipped.effective, "models": shipped.models}),
        "the echo answers the price table the harness loaded, key for key"
    );
    assert_eq!(
        keys_in_order(&prices["content"]["models"]),
        shipped
            .models
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        "in the file's order"
    );
    let loaded = phoebis_ledger::PlanTables::shipped();
    let cost_model = loaded.revisions.in_force(common::NOW);
    let costs = &settings["subscription_costs"];
    assert_eq!(
        (&costs["source"], &costs["effective"]),
        (
            &serde_json::json!(loaded.source),
            &serde_json::json!(cost_model.effective)
        )
    );
    assert_eq!(
        costs["content"],
        serde_json::to_value(cost_model).unwrap(),
        "the echo answers the cost model the harness loaded, key for key"
    );
    assert_eq!(
        keys_in_order(&costs["content"]["model_weight"]),
        cost_model
            .model_weight
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        "in the file's order"
    );
    assert_eq!(
        prices["revisions"],
        serde_json::Value::Array(
            revisions
                .iter()
                .map(|table| serde_json::json!({
                    "effective": table.effective,
                    "content": {"effective": table.effective, "models": table.models},
                }))
                .collect()
        ),
        "every revision the price list holds, ascending by date, each in the shape of `content`"
    );
    assert_eq!(
        costs["revisions"],
        serde_json::Value::Array(
            loaded
                .revisions
                .iter()
                .map(|entry| serde_json::json!({
                    "effective": entry.effective,
                    "content": serde_json::to_value(entry).unwrap(),
                }))
                .collect()
        ),
        "every revision of the cost model, ascending by date, each in the shape of `content`"
    );
    assert_eq!(
        (
            prices["revisions"].as_array().map(Vec::len),
            costs["revisions"].as_array().map(Vec::len),
        ),
        (Some(revisions.len()), Some(loaded.revisions.len())),
        "one entry per revision of each file"
    );
    assert_eq!(
        (settings.get("pricing"), settings.get("plan_tables")),
        (None, None),
        "each table answers under its file's name alone"
    );
    let pricing = json(get(&parts.app, "/admin/pricing", Some(&cookie)).await).await;
    assert_eq!(
        pricing,
        serde_json::to_value(phoebis_admin::table_envelopes(
            &revisions,
            &loaded,
            common::NOW
        ))
        .unwrap(),
        "the price route answers both envelopes the settings echo carries"
    );
    assert!(
        pricing["api_prices"].get("path").is_none()
            && pricing["subscription_costs"].get("path").is_none(),
        "the paths are the settings echo's and the system document's: {pricing}"
    );
    let text = settings.to_string();
    assert!(
        !text.contains("admin-secret") && !text.contains("secret_env"),
        "no secret and no secret's variable name"
    );
    parts
        .db
        .insert_event(&phoebis_ledger::EventRecord {
            at: common::NOW - HOUR,
            kind: "ledger_gap".into(),
            detail: Some(r#"{"dropped":7}"#.into()),
            ..Default::default()
        })
        .unwrap();
    let system = json(get(&parts.app, "/admin/system", Some(&cookie)).await).await;
    assert_eq!(
        (
            &system["pricing"]["api_prices"],
            &system["pricing"]["subscription_costs"]
        ),
        (
            &serde_json::json!({
                "path": null, "source": "shipped", "effective": shipped.effective,
                "revisions": revisions.len(), "next": null,
            }),
            &serde_json::json!({
                "path": null, "source": "shipped", "effective": cost_model.effective,
                "revisions": loaded.revisions.len(), "next": null,
            })
        ),
        "the harness configures neither path, so both tables are the shipped ones, and the clock is past every \
         shipped entry, so neither holds a pending one: {system}"
    );
    assert_eq!(
        (
            system["ledger"]["schema_version"].as_u64(),
            system["ledger"]["dropped"].as_u64(),
            system["ledger"]["channel_depth"].as_u64()
        ),
        (Some(41), Some(0), Some(4_096)),
        "the ledger schema is at the migration that flags the router's refusals before any account"
    );
    assert_eq!(
        (
            system["ledger"]["cache_map_orphans"].as_u64(),
            system["ledger"]["mirror_lagged"].as_u64()
        ),
        (Some(0), Some(0)),
        "every decision stored leaves no orphaned cache map, and a mirror that kept up missed nothing: {system}"
    );
    assert_eq!(
        (
            system["ledger"]["queued"].as_u64(),
            system["ledger"]["behind"].as_bool(),
            system["ledger"]["behind_refusals"].as_u64()
        ),
        (Some(0), Some(false), Some(0)),
        "a writer that keeps up holds nothing back and has refused nothing: {system}"
    );
    assert_eq!(
        (
            system["ledger"]["degraded"].as_bool(),
            system["ledger"]["retrying"].as_u64(),
            system["ledger"]["parked_priced"].as_u64()
        ),
        (Some(false), Some(0), Some(0)),
        "a ledger that lands every batch is current, retries nothing and parks nothing: {system}"
    );
    assert_eq!(system["router"]["command_depth"].as_u64(), Some(4_096));
    // A write in flight reads `depth` 1 for an instant, so the idle figures are waited for.
    wait_json(&parts.app, "/admin/system", &cookie, |system| {
        system["router"]["queued"].as_u64() == Some(0)
            && system["router"]["writer"]
                == serde_json::json!({
                    "depth": 0,
                    "deferred": 0,
                    "degraded": false,
                    "failed_batches": 0,
                })
    })
    .await;
    let dropped = system["router"]["dropped_reports"].as_object().unwrap();
    assert_eq!(
        dropped.keys().map(String::as_str).collect::<Vec<_>>(),
        [
            "account_state",
            "accounting",
            "affinity",
            "cooldown",
            "credits",
            "ended",
            "head",
            "member_spend",
            "overage",
            "pin_invalidated",
            "pin_released",
            "pin_renewed",
            "sample",
            "usage",
        ],
        "every kind of routing report, in name order"
    );
    assert!(
        dropped.values().all(|n| n.as_u64() == Some(0)),
        "nothing refused by a router that kept up: {dropped:?}"
    );
    assert_eq!(system["gaps"][0]["detail"]["dropped"].as_u64(), Some(7));
    assert!(system["bus"]
        .as_array()
        .unwrap()
        .iter()
        .any(|b| b["subscriber"] == "ledger" && b["dropped"].as_u64() == Some(0)));
    assert_eq!(system["storage_ok"], true);
    assert_eq!(
        (&system["ready"], &system["dead"]),
        (&serde_json::json!(true), &serde_json::json!([])),
        "the harness's router task, ledger consumer and store are all alive"
    );
}

#[tokio::test]
async fn a_price_file_with_an_entry_dated_ahead_names_it_as_next_and_lists_it_beside_the_one_in_force(
) {
    // The file lists its later entry first, so the route's ascending order is the loader's, not the file's.
    let prices = phoebis_ledger::PriceRevisions::from_yaml(
        "revisions:\n  - effective: 2099-01-01\n    models:\n      claude-sonnet-5: {input: 3, cache_write_5m: 3, \
         cache_write_1h: 3, cache_read: 3, output: 3}\n  - effective: 2026-01-01\n    models:\n      \
         claude-sonnet-5: {input: 2, cache_write_5m: 2, cache_write_1h: 2, cache_read: 2, output: 2}\n",
        "file",
    )
    .unwrap();
    let parts = common::admin_parts_full_priced(prices.clone()).await;
    let cookie = login(&parts.app, "admin-secret").await;
    let system = json(get(&parts.app, "/admin/system", Some(&cookie)).await).await;
    assert_eq!(
        (
            &system["pricing"]["api_prices"]["effective"],
            &system["pricing"]["api_prices"]["revisions"],
            &system["pricing"]["api_prices"]["next"],
        ),
        (
            &serde_json::json!("2026-01-01"),
            &serde_json::json!(prices.len()),
            &serde_json::json!("2099-01-01"),
        ),
        "the entry the clock has not reached is the next one, and the one before it stays in force: {system}"
    );
    assert_eq!(
        system["pricing"]["subscription_costs"]["next"],
        serde_json::Value::Null,
        "the shipped cost model holds no entry ahead of the clock: {system}"
    );
    let pricing = json(get(&parts.app, "/admin/pricing", Some(&cookie)).await).await;
    let dates: Vec<&str> = pricing["api_prices"]["revisions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["effective"].as_str().unwrap())
        .collect();
    assert_eq!(
        (dates, &pricing["api_prices"]["effective"]),
        (
            vec!["2026-01-01", "2099-01-01"],
            &serde_json::json!("2026-01-01")
        ),
        "the price route answers the pending entry beside the one in force: {pricing}"
    );
}

#[tokio::test]
async fn the_settings_answer_reads_proxy_tls_true_where_the_proxy_listener_terminates_tls() {
    let (app, _up) = common::admin_app_tls().await;
    let cookie = login(&app, "admin-secret").await;
    let settings = json(get(&app, "/admin/settings", Some(&cookie)).await).await;
    assert_eq!(
        settings["listeners"]["proxy_tls"].as_bool(),
        Some(true),
        "{settings}"
    );
}

#[tokio::test]
async fn a_stopped_ledger_reads_not_ready_on_the_system_document() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    parts.ledger.clone().shutdown().await.unwrap();
    let system = json(get(&parts.app, "/admin/system", Some(&cookie)).await).await;
    assert_eq!(
        (&system["ready"], &system["dead"], &system["storage_ok"]),
        (
            &serde_json::json!(false),
            &serde_json::json!(["ledger"]),
            &serde_json::json!(true)
        ),
        "the writer closed its channel while the router task and the store still answer: {system}"
    );
}

/// A batch the file refuses holds the ledger degraded under retry, and a parked record counts until a start lands it:
/// the system document reads all three away from their idle values.
#[tokio::test]
async fn the_system_document_reads_a_degraded_ledger_its_retry_and_its_parked_records() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    common::park_unreadable(&parts.db, "main", "business", common::NOW);
    parts
        .db
        .write(|c| {
            c.execute_batch(
                "CREATE TRIGGER refuse BEFORE INSERT ON requests BEGIN SELECT RAISE(ABORT, 'refused by test'); END;",
            )?;
            Ok(())
        })
        .unwrap();
    parts
        .ledger
        .record(record("r1", common::NOW, "claude-sonnet-5", "s1"));
    let system = wait_json(&parts.app, "/admin/system", &cookie, |system| {
        system["ledger"]["retrying"].as_u64() == Some(1)
    })
    .await;
    assert_eq!(
        (
            system["ledger"]["degraded"].as_bool(),
            system["ledger"]["retrying"].as_u64(),
            system["ledger"]["parked_priced"].as_u64()
        ),
        (Some(true), Some(1), Some(1)),
        "{system}"
    );
}

/// `POST` with the admin secret as a bearer and no cookie, the admin client's own call.
async fn post_bearer(
    app: &axum::Router,
    path: &str,
    bearer: &str,
) -> axum::http::Response<axum::body::Body> {
    use tower::ServiceExt;
    app.clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(path)
                .header("authorization", format!("Bearer {bearer}"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn the_replay_route_answers_the_counts_on_an_empty_table() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let replayed = post(&parts.app, "/admin/ledger/parked/replay", "", Some(&cookie)).await;
    assert_eq!(replayed.status(), 200);
    assert_eq!(
        json(replayed).await,
        serde_json::json!({"landed": 0, "remaining": 0})
    );
    let scripted = post_bearer(&parts.app, "/admin/ledger/parked/replay", "admin-secret").await;
    assert_eq!(scripted.status(), 200);
    assert_eq!(
        json(scripted).await,
        serde_json::json!({"landed": 0, "remaining": 0})
    );
    let anonymous = post(&parts.app, "/admin/ledger/parked/replay", "", None).await;
    assert_eq!(anonymous.status(), 401);
}

/// A parked row the replay tries and cannot land stays in the table with the error it met this time, and the answer
/// and the system document count it.
#[tokio::test]
async fn the_replay_route_counts_a_row_it_could_not_land_as_remaining() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    common::park_unreadable(&parts.db, "main", "business", common::NOW);
    let replayed = post(&parts.app, "/admin/ledger/parked/replay", "", Some(&cookie)).await;
    assert_eq!(replayed.status(), 200);
    assert_eq!(
        json(replayed).await,
        serde_json::json!({"landed": 0, "remaining": 1})
    );
    let error = parts
        .db
        .read(|c| {
            Ok(c.query_row("SELECT error FROM parked_records", [], |r| {
                r.get::<_, String>(0)
            })?)
        })
        .await
        .unwrap();
    assert_ne!(
        error, "refused by test",
        "the replay tried the row and kept the error it met"
    );
    let system = json(get(&parts.app, "/admin/system", Some(&cookie)).await).await;
    assert_eq!(
        system["ledger"]["parked_priced"].as_u64(),
        Some(1),
        "{system}"
    );
}

/// A pass that could not read the parked rows never ran, so it is a failure rather than an answer of nothing landed
/// and every row kept: a caller reading the counts would stop retrying.
#[tokio::test]
async fn the_replay_route_answers_500_when_the_parked_rows_cannot_be_read() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    common::park_unreadable(&parts.db, "main", "business", common::NOW);
    // The row read names the column; the count the answer would carry does not, so it still reads.
    parts
        .db
        .write(|c| {
            c.execute_batch("ALTER TABLE parked_records RENAME COLUMN record TO record_held;")?;
            Ok(())
        })
        .unwrap();
    let replayed = post(&parts.app, "/admin/ledger/parked/replay", "", Some(&cookie)).await;
    assert_eq!(replayed.status(), 500);
    assert_eq!(
        json(replayed).await,
        serde_json::json!({"error": "ledger failure"})
    );
}

#[tokio::test]
async fn the_replay_route_answers_500_once_the_ledger_writer_has_stopped() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    parts.ledger.clone().shutdown().await.unwrap();
    let replayed = post(&parts.app, "/admin/ledger/parked/replay", "", Some(&cookie)).await;
    assert_eq!(replayed.status(), 500);
    assert_eq!(
        json(replayed).await,
        serde_json::json!({"error": "ledger failure"})
    );
}

/// Parks a priced record the writer can land, once its start-up pass is behind it: a replay answers from the writer's
/// loop alone, which starts after the pass, so the pass cannot take the row before the case asks.
async fn park_landable(parts: &common::Full, cookie: &str) {
    let replayed = post(&parts.app, "/admin/ledger/parked/replay", "", Some(cookie)).await;
    assert_eq!(
        json(replayed).await,
        serde_json::json!({"landed": 0, "remaining": 0})
    );
    let Record::Request {
        request, attempts, ..
    } = record("parked", common::NOW, "claude-sonnet-5", "s1")
    else {
        unreachable!("`record` builds a request")
    };
    let parked = serde_json::to_string(&ParkedRecord { request, attempts }).unwrap();
    parts
        .db
        .write(|c| {
            c.execute(
                "INSERT INTO parked_records (parked_at, last_tried_at, request_id, pool_id, account_id, record, error) \
                 VALUES (?1, ?1, 'parked', 'main', 'business', ?2, 'refused by test')",
                rusqlite::params![common::NOW, parked],
            )?;
            Ok(())
        })
        .unwrap();
}

/// The request the writer is held on, as `record` builds it.
fn held() -> (RequestRecord, Vec<AttemptRecord>) {
    let Record::Request {
        request, attempts, ..
    } = record("held", common::NOW, "claude-sonnet-5", "s1")
    else {
        unreachable!("`record` builds a request")
    };
    (request, attempts)
}

/// The replay route called on a task, so the case can step the clock while it waits.
fn replay_on_a_task(
    parts: &common::Full,
    cookie: &str,
) -> tokio::task::JoinHandle<axum::http::Response<axum::body::Body>> {
    let (app, cookie) = (parts.app.clone(), cookie.to_string());
    tokio::spawn(async move { post(&app, "/admin/ledger/parked/replay", "", Some(&cookie)).await })
}

/// The route waits its bound on the ledger's clock for the writer's turn. A writer still holding a batch past it is
/// answered `503`, and the replay the route gave up on never runs, so the replay after the writer is freed lands the
/// row.
#[tokio::test]
async fn the_replay_route_answers_503_after_its_bound_while_the_writer_is_held_and_replays_nothing()
{
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    park_landable(&parts, &cookie).await;
    let (request, attempts) = held();
    common::hold_the_writer(&parts, request, attempts).await;
    let asked = replay_on_a_task(&parts, &cookie);
    stays_for(Duration::from_millis(400), || !asked.is_finished()).await;
    common::step_past_replay_bound(&parts);
    let refused = tokio::time::timeout(Duration::from_secs(5), asked)
        .await
        .expect("the route answers once its bound passed")
        .unwrap();
    assert_eq!(refused.status(), 503);
    assert_eq!(
        json(refused).await,
        serde_json::json!({"error": "the ledger writer is held, try again"})
    );
    common::free_the_writer(&parts).await;
    // The control channel is read in order, so the message given up on reaches the writer ahead of this one: a landed
    // row here means the first never ran.
    let replayed = post(&parts.app, "/admin/ledger/parked/replay", "", Some(&cookie)).await;
    assert_eq!(replayed.status(), 200);
    assert_eq!(
        json(replayed).await,
        serde_json::json!({"landed": 1, "remaining": 0})
    );
}

/// A replay the writer took inside the bound answers its counts, even when the clock passed the bound before the
/// route's answer is read.
#[tokio::test]
async fn the_replay_route_answers_200_for_a_replay_the_writer_claimed_inside_the_bound() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    park_landable(&parts, &cookie).await;
    let (request, attempts) = held();
    common::hold_the_writer(&parts, request, attempts).await;
    let asked = replay_on_a_task(&parts, &cookie);
    stays_for(Duration::from_millis(400), || !asked.is_finished()).await;
    parts.db.free_the_disk();
    common::step(&parts, Duration::from_secs(15));
    let landed = || {
        parts
            .db
            .write(|c| {
                Ok(c.query_row("SELECT COUNT(*) FROM parked_records", [], |r| {
                    r.get::<_, i64>(0)
                })?)
            })
            .unwrap()
            == 0
    };
    wait_for(Duration::from_secs(5), || landed().then_some(())).await;
    common::step(&parts, Duration::from_secs(16));
    let answered = tokio::time::timeout(Duration::from_secs(5), asked)
        .await
        .expect("the claimed replay answers")
        .unwrap();
    assert_eq!(answered.status(), 200);
    assert_eq!(
        json(answered).await,
        serde_json::json!({"landed": 1, "remaining": 0})
    );
}

#[tokio::test]
async fn a_store_whose_read_fails_reads_not_ready_on_the_system_document_until_it_answers_again() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    parts.store.debug_fail_next_schema_version();
    let system = json(get(&parts.app, "/admin/system", Some(&cookie)).await).await;
    assert_eq!(
        (&system["ready"], &system["dead"], &system["storage_ok"]),
        (
            &serde_json::json!(false),
            &serde_json::json!(["store"]),
            &serde_json::json!(false)
        ),
        "the store's read failed while the ledger and the router task still answer: {system}"
    );
    let system = json(get(&parts.app, "/admin/system", Some(&cookie)).await).await;
    assert_eq!(
        (&system["ready"], &system["dead"], &system["storage_ok"]),
        (
            &serde_json::json!(true),
            &serde_json::json!([]),
            &serde_json::json!(true)
        ),
        "each answer reads the store afresh, so the next one is ready again: {system}"
    );
}

#[tokio::test]
async fn readiness_reads_the_ledger_dead_when_its_bus_subscriber_is_absent_or_panicked() {
    let parts = common::admin_parts_full().await;
    let readiness = |bus: phoebis_bus::Bus| phoebis_admin::Readiness {
        bus,
        ledger: parts.ledger.clone(),
        router: parts.router.clone(),
        store: parts.store.clone(),
    };
    assert_eq!(
        readiness(parts.bus.clone()).dead(),
        Vec::<&str>::new(),
        "the harness's own bus carries the ledger's live subscriber"
    );
    let bus = phoebis_bus::Bus::new();
    let _live = bus.subscribe("live", Delivery::Durable);
    assert_eq!(
        readiness(bus.clone()).dead(),
        vec!["ledger"],
        "a bus the ledger never subscribed to"
    );
    let assembler = bus.subscribe("ledger", Delivery::Durable);
    assert_eq!(
        readiness(bus.clone()).dead(),
        Vec::<&str>::new(),
        "a `ledger` subscriber with no panic counted"
    );
    phoebis_bus::spawn_subscriber(assembler, |_| async { panic!("the assembler died") })
        .await
        .unwrap();
    assert_eq!(
        readiness(bus).dead(),
        vec!["ledger"],
        "a `ledger` subscriber whose task panicked, while the writer's channel stays open"
    );
}

#[tokio::test]
async fn the_price_routes_answer_the_fast_cells_beside_the_standard_ones() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let settings = json(get(&parts.app, "/admin/settings", Some(&cookie)).await).await;
    let pricing = json(get(&parts.app, "/admin/pricing", Some(&cookie)).await).await;
    let costs = json(get(&parts.app, "/admin/sessions/costs", Some(&cookie)).await).await;
    // Picked by what they carry, so the test survives a price update that moves fast mode to another model.
    let revisions = phoebis_ledger::PriceRevisions::shipped();
    let shipped = revisions.in_force(common::NOW);
    let (fast_model, fast_row) = shipped
        .models
        .iter()
        .find(|(_, row)| row.fast.is_some())
        .expect("a shipped row serves fast mode");
    let (plain_model, plain_row) = shipped
        .models
        .iter()
        .find(|(_, row)| row.fast.is_none())
        .expect("a shipped row serves no fast mode");
    let fast = fast_row.fast.as_ref().unwrap();
    assert_eq!(
        (costs.get("version"), &costs["effective"]),
        (None, &serde_json::json!(shipped.effective)),
        "sessions/costs names the date of the revision in force and no version: {costs}"
    );
    for (route, models) in [
        ("settings", &settings["api_prices"]["content"]["models"]),
        ("pricing", &pricing["api_prices"]["content"]["models"]),
    ] {
        assert_eq!(
            models[fast_model],
            serde_json::to_value(fast_row).unwrap(),
            "{route}: a model that serves fast mode answers its standard and its five fast cells under the file's names"
        );
        assert_eq!(
            models[plain_model],
            serde_json::to_value(plain_row).unwrap(),
            "{route}: a model without them answers its standard cells alone"
        );
        assert!(
            models[plain_model].get("fast").is_none(),
            "{route}: as its file row carries no fast member: {models}"
        );
    }
    let row = |model: &str| {
        costs["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["model"] == model)
            .cloned()
            .unwrap_or_else(|| panic!("sessions/costs lists {model}: {costs}"))
    };
    let fast_model_costs = row(fast_model);
    assert_eq!(
        (
            &fast_model_costs["input_per_m"],
            &fast_model_costs["output_per_m"]
        ),
        (
            &serde_json::json!(fast_row.standard.input),
            &serde_json::json!(fast_row.standard.output)
        ),
        "sessions/costs: the standard cells keep their per-million names"
    );
    assert_eq!(
        fast_model_costs["fast"],
        serde_json::json!({
            "input_per_m": fast.input,
            "cache_write_5m_per_m": fast.cache_write_5m,
            "cache_write_1h_per_m": fast.cache_write_1h,
            "cache_read_per_m": fast.cache_read,
            "output_per_m": fast.output,
        }),
        "sessions/costs: a model that serves fast mode answers its five fast cells"
    );
    assert_eq!(
        row(plain_model).get("fast"),
        Some(&serde_json::Value::Null),
        "sessions/costs: a model without them answers null"
    );
}

#[tokio::test]
async fn the_price_routes_answer_the_long_context_set_beside_the_standard_cells() {
    // Served from the test table so a row with the block is there whatever the shipped table lists, and read from
    // that table key for key.
    let revisions = phoebis_ledger::test_tables::api_prices();
    let parts = common::admin_parts_full_priced(revisions.clone()).await;
    let table = revisions.in_force(common::NOW);
    let cookie = login(&parts.app, "admin-secret").await;
    let settings = json(get(&parts.app, "/admin/settings", Some(&cookie)).await).await;
    let pricing = json(get(&parts.app, "/admin/pricing", Some(&cookie)).await).await;
    let costs = json(get(&parts.app, "/admin/sessions/costs", Some(&cookie)).await).await;
    let (long_model, long_row) = table
        .models
        .iter()
        .find(|(_, row)| row.long_context.is_some())
        .expect("a row priced by prompt length");
    let (plain_model, plain_row) = table
        .models
        .iter()
        .find(|(_, row)| row.long_context.is_none())
        .expect("a row priced one way");
    let long = long_row.long_context.as_ref().unwrap();
    assert_eq!(
        (costs.get("version"), &costs["effective"]),
        (None, &serde_json::json!(table.effective)),
        "sessions/costs names the date of the test table's revision in force and no version: {costs}"
    );
    for (route, models) in [
        ("settings", &settings["api_prices"]["content"]["models"]),
        ("pricing", &pricing["api_prices"]["content"]["models"]),
    ] {
        assert_eq!(
            models[long_model],
            serde_json::to_value(long_row).unwrap(),
            "{route}: a banded model answers its threshold and the five cells over it under the file's names"
        );
        assert_eq!(
            models[plain_model],
            serde_json::to_value(plain_row).unwrap(),
            "{route}: a model priced one way answers its row as the file holds it"
        );
        assert!(
            models[plain_model].get("long_context").is_none(),
            "{route}: as its file row carries no long_context member: {models}"
        );
    }
    let row = |model: &str| {
        costs["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["model"] == model)
            .cloned()
            .unwrap_or_else(|| panic!("sessions/costs lists {model}: {costs}"))
    };
    let long_model_costs = row(long_model);
    assert_eq!(
        long_model_costs["long_context"],
        serde_json::json!({
            "over": long.over,
            "input_per_m": long.cells.input,
            "cache_write_5m_per_m": long.cells.cache_write_5m,
            "cache_write_1h_per_m": long.cells.cache_write_1h,
            "cache_read_per_m": long.cells.cache_read,
            "output_per_m": long.cells.output,
        }),
        "sessions/costs: a model priced by prompt length answers its threshold and the five cells over it"
    );
    assert_eq!(
        row(plain_model).get("long_context"),
        Some(&serde_json::Value::Null),
        "sessions/costs: a model priced one way answers null"
    );
}

#[tokio::test]
async fn quota_history_pool_traffic_and_sessions_read_the_ledger() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let now = common::NOW;
    let bucket = phoebis_ledger::bucket_of(now - HOUR);
    parts
        .db
        .insert_samples(
            &[
                phoebis_ledger::QuotaSampleRecord {
                    account_id: "business".into(),
                    bucket,
                    window: "five_hour".into(),
                    utilization: Some(0.4),
                    reset_at: Some(now + HOUR),
                    source: "head".into(),
                    observed_at: None,
                },
                phoebis_ledger::QuotaSampleRecord {
                    account_id: "business".into(),
                    bucket: bucket + 300,
                    window: "five_hour".into(),
                    utilization: Some(0.5),
                    reset_at: Some(now + HOUR),
                    source: "poll".into(),
                    observed_at: None,
                },
                phoebis_ledger::QuotaSampleRecord {
                    account_id: "business".into(),
                    bucket,
                    window: "seven_day_fable".into(),
                    utilization: Some(0.1),
                    reset_at: None,
                    source: "poll".into(),
                    observed_at: None,
                },
            ],
            Some(&phoebis_ledger::ExtraUsageSampleRecord {
                account_id: "business".into(),
                bucket,
                is_enabled: true,
                in_use: None,
                monthly_limit: Some(5_000),
                used_credits: Some(10),
                utilization: Some(0.002),
                currency: Some("USD".into()),
                disabled_reason: None,
                overage_reset: None,
                source: "poll".into(),
            }),
        )
        .unwrap();
    let quota = json(
        get(
            &parts.app,
            "/admin/accounts/business/quota?range=24h",
            Some(&cookie),
        )
        .await,
    )
    .await;
    let windows = quota["windows"].as_array().unwrap();
    assert_eq!(
        windows
            .iter()
            .map(|w| w["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["five_hour", "seven_day_fable"]
    );
    assert_eq!(windows[0]["samples"].as_array().unwrap().len(), 2);
    assert_eq!(
        (
            windows[0]["samples"][0]["at"].as_i64(),
            windows[0]["samples"][0]["utilization"].as_f64(),
            windows[0]["samples"][0]["source"].as_str()
        ),
        (Some(bucket * 1_000), Some(0.4), Some("head"))
    );
    assert_eq!(quota["extra_usage"][0]["used_credits"].as_u64(), Some(10));
    assert_eq!(
        get(&parts.app, "/admin/accounts/nope/quota", Some(&cookie))
            .await
            .status(),
        404
    );
    post(&parts.app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority","members":[{"account_id":"business","priority":0}]}"#, Some(&cookie)).await;
    // The oldest request is answered in overage, so the day holds billed dollars beside the plan rows' equivalent.
    const OVERAGE: i64 = 4;
    for i in 0..5 {
        let mut seeded = record(
            &format!("t{i}"),
            now - i * HOUR,
            "claude-sonnet-5",
            &format!("s{}", i % 2),
        );
        if i == OVERAGE {
            let Record::Request { attempts, .. } = &mut seeded else {
                unreachable!("record builds a request")
            };
            attempts[0].overage_in_use = Some(true);
        }
        parts.ledger.record(seeded);
    }
    fake_upstream::wait_for_async(Duration::from_secs(5), || async {
        parts.db.request("t4").await.unwrap()
    })
    .await;
    let seeded: Vec<phoebis_ledger::Priced> = (0..5)
        .map(|i| {
            let funding = if i == OVERAGE {
                phoebis_ledger::Funding::Overage
            } else {
                phoebis_ledger::Funding::Plan
            };
            priced_row(
                "claude-sonnet-5",
                funding,
                now - i * HOUR,
                phoebis_routing::PinUsage {
                    input: 1_000,
                    output: 100,
                    ..phoebis_routing::PinUsage::default()
                },
            )
        })
        .collect();
    let billed = usd_of(&seeded[OVERAGE as usize]);
    let dollars: f64 = seeded.iter().map(usd_of).sum();
    let five_hour: f64 = seeded
        .iter()
        .map(|p| equivalent_of(p, phoebis_routing::FIVE_HOUR))
        .sum();
    let seven_day: f64 = seeded
        .iter()
        .map(|p| equivalent_of(p, phoebis_routing::SEVEN_DAY))
        .sum();
    assert!(
        billed > 0.0 && billed < dollars && five_hour > 0.0 && seven_day > 0.0,
        "the seed prices both sides: {billed} of {dollars}, {five_hour} and {seven_day}"
    );
    // Whether the shipped cost model weighs the seeded model is its own fact, read rather than assumed.
    let estimated = seeded.iter().any(|p| p.estimated);
    let traffic = json(
        get(
            &parts.app,
            "/admin/pools/main/traffic?range=24h",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        (
            traffic["pool_id"].as_str(),
            traffic["bucket_seconds"].as_i64(),
            traffic["buckets"].as_array().unwrap().len()
        ),
        (Some("main"), Some(3_600), 25),
        "a 24-hour range off the hour boundary touches 25 hour buckets"
    );
    let total: u64 = traffic["buckets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["requests"].as_u64().unwrap())
        .sum();
    assert_eq!(total, 5);
    assert!(
        traffic["buckets"].as_array().unwrap().last().unwrap()["tokens"]
            .as_u64()
            .unwrap()
            > 0
    );
    let buckets = traffic["buckets"].as_array().unwrap();
    let summed = |read: &dyn Fn(&serde_json::Value) -> f64| -> serde_json::Value {
        serde_json::json!(buckets.iter().map(read).sum::<f64>())
    };
    assert!(
        close_to(&summed(&|b| b["billed_usd"].as_f64().unwrap()), billed)
            && close_to(&summed(&|b| b["cost_usd"].as_f64().unwrap()), dollars)
            && close_to(
                &summed(&|b| b["equivalent"]["five_hour"].as_f64().unwrap()),
                five_hour
            )
            && close_to(
                &summed(&|b| b["equivalent"]["seven_day"].as_f64().unwrap()),
                seven_day
            )
            && buckets.iter().any(|b| b["estimated"] == true) == estimated,
        "the buckets carry the overage row's dollars as billed and the plan rows' equivalent: {traffic}"
    );
    let pool = json(get(&parts.app, "/admin/pools", Some(&cookie)).await).await["pools"][0].clone();
    assert!(
        close_to(&pool["cost_24h"], dollars)
            && close_to(&pool["billed_24h"], billed)
            && close_to(&pool["equivalent_24h"]["five_hour"], five_hour)
            && close_to(&pool["equivalent_24h"]["seven_day"], seven_day)
            && pool["estimated_24h"] == estimated,
        "the pool's day answers its billed dollars and its 20x equivalent beside its dollars: {pool}"
    );
    let board = json(get(&parts.app, "/admin/dashboard", Some(&cookie)).await).await;
    assert!(
        close_to(&board["cost_24h"], dollars)
            && close_to(&board["billed_24h"], billed)
            && close_to(&board["equivalent_24h"]["five_hour"], five_hour)
            && close_to(&board["equivalent_24h"]["seven_day"], seven_day)
            && board["estimated_24h"] == estimated,
        "the dashboard's day answers the same figures: {board}"
    );
    let hourly = board["hourly"].as_array().unwrap();
    let hourly_sum = |read: &dyn Fn(&serde_json::Value) -> f64| -> serde_json::Value {
        serde_json::json!(hourly.iter().map(read).sum::<f64>())
    };
    assert!(
        close_to(&hourly_sum(&|h| h["billed"].as_f64().unwrap()), billed)
            && close_to(
                &hourly_sum(&|h| h["equivalent"]["five_hour"].as_f64().unwrap()),
                five_hour
            )
            && close_to(
                &hourly_sum(&|h| h["equivalent"]["seven_day"].as_f64().unwrap()),
                seven_day
            ),
        "the hours sum to the day: {board}"
    );
    assert_eq!(
        get(&parts.app, "/admin/pools/nope/traffic", Some(&cookie))
            .await
            .status(),
        404
    );
    let sessions = json(get(&parts.app, "/admin/sessions", Some(&cookie)).await).await;
    let rows = sessions["sessions"].as_array().unwrap();
    assert_eq!(
        rows.len(),
        2,
        "one row per (key, session) seen in the ledger, no pin needed"
    );
    assert_eq!(
        (
            rows[0]["session_id"].as_str(),
            rows[0]["requests"].as_u64(),
            rows[0]["last_request_at"].as_i64(),
            rows[0]["pinned"].as_bool(),
            rows[0]["account_id"].as_str()
        ),
        (
            Some("s0"),
            Some(3),
            Some(now),
            Some(false),
            Some("business")
        )
    );
    assert!(rows[0]["epoch"].is_null() && rows[0]["created_at"].is_null());
}

#[tokio::test]
async fn a_plan_request_on_a_model_the_cost_model_does_not_weight_reads_an_estimated_equivalent() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let now = common::NOW;
    let tables = phoebis_ledger::PlanTables::shipped();
    assert!(
        tables
            .revisions
            .in_force(now)
            .weight_or_fallback("claude-opus-5", phoebis_routing::FIVE_HOUR)
            .is_some_and(|(_, fallback)| fallback),
        "the shipped cost model has no weight of its own for Opus 5"
    );
    let donor = tables
        .tier_of("claude-opus-5", now)
        .and_then(|tier| tables.latest_weighted(tier, now))
        .expect("the shipped cost model weighs a model of Opus 5's model tier");
    // The landing records the account's Max 20x tier on each attempt. One request on the plan, one answered in
    // overage.
    set_max_20x(&parts).await;
    let plan = record("opus-plan", now - HOUR, "claude-opus-5", "s0");
    let mut billed = record("opus-billed", now - HOUR, "claude-opus-5", "s1");
    let Record::Request { attempts, .. } = &mut billed else {
        unreachable!("record builds a request")
    };
    attempts[0].overage_in_use = Some(true);
    parts.ledger.record(plan);
    parts.ledger.record(billed);
    fake_upstream::wait_for_async(Duration::from_secs(5), || async {
        parts.db.request("opus-billed").await.unwrap()
    })
    .await;
    let usage = phoebis_routing::PinUsage {
        input: 1_000,
        output: 100,
        ..phoebis_routing::PinUsage::default()
    };
    // The request finished 500 ms after its receipt, where its plan figures read the peak band.
    let band_at = now - HOUR + 500;
    let summary = json(get(&parts.app, "/admin/requests/opus-plan", Some(&cookie)).await).await
        ["request"]
        .clone();
    let windows = summary["cost_equivalent"]
        .as_object()
        .map(|m| m.keys().cloned().collect::<Vec<_>>());
    assert_eq!(
        windows,
        Some(vec!["five_hour".to_string(), "seven_day".to_string()]),
        "an Opus model's equivalent reads the two shared windows: {summary}"
    );
    for window in [phoebis_routing::FIVE_HOUR, phoebis_routing::SEVEN_DAY] {
        let at_the_donor = tables
            .equivalent_at(window, None, donor, usage, band_at)
            .unwrap();
        assert!(
            close_to(&summary["cost_equivalent"][window], at_the_donor),
            "{window}: the 20x equivalent at the weight of the model tier's latest weighted model, \
             {at_the_donor}: {summary}"
        );
    }
    assert_eq!(
        (&summary["estimated"], &summary["tier"]),
        (
            &serde_json::json!(true),
            &serde_json::json!("default_claude_max_20x")
        ),
        "the figure is marked estimated, and the summary names the tier the serving attempt recorded: {summary}"
    );
    assert!(
        summary["cost_shares"]["five_hour"]
            .as_f64()
            .is_some_and(|share| share > 0.0),
        "the shares price under that tier: {summary}"
    );
    let billed = json(get(&parts.app, "/admin/requests/opus-billed", Some(&cookie)).await).await
        ["request"]
        .clone();
    assert_eq!(
        (
            billed.get("cost_equivalent"),
            &billed["estimated"],
            billed.get("tier")
        ),
        (
            Some(&serde_json::Value::Null),
            &serde_json::json!(false),
            Some(&serde_json::json!("default_claude_max_20x"))
        ),
        "a request billed in dollars answers a null equivalent and nothing estimated, and still names the account \
         tier its attempt recorded: {billed}"
    );
}

/// The request count on a session row is the ledger's. A pin the ledger has no row for has no count, and
/// saying zero would read as "this session made no requests" rather than "nothing was recorded".
#[tokio::test]
async fn a_pin_the_ledger_has_no_row_for_reports_no_request_count() {
    // The router loads the pins at spawn and purges any whose key names no pool, so the pool, the key, and
    // the pins are written through one harness and the store is handed to the one under test.
    let seed = common::admin_parts_full().await;
    let seeding = login(&seed.app, "admin-secret").await;
    post(&seed.app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority","members":[{"account_id":"business","priority":0}]}"#, Some(&seeding)).await;
    post(
        &seed.app,
        "/admin/keys",
        r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
        Some(&seeding),
    )
    .await;
    let now = common::NOW;
    for session in ["served", "silent"] {
        seed.store
            .upsert_pin(&phoebis_store::PinRow {
                key_id: "desktop".into(),
                session_id: session.into(),
                agent_id: "main".into(),
                model: "claude-sonnet-5".into(),
                account_id: "business".into(),
                epoch: 1,
                created_at: now - 10,
                last_used_at: now - 5,
                key_generation: common::generation(&seed.store, "desktop"),
                incarnation: 1,
                burn: phoebis_store::PinBurn::default(),
                burn_at: None,
                burn_banded: phoebis_store::PinBurn::default(),
                burn_fast: phoebis_store::PinBurn::default(),
                burn_unmetered: phoebis_store::PinBurn::default(),
                last_input_tokens: 0,
                last_classifier: false,
                last_refresh_at: 0,
                cache_ttl_ms: 300_000,
                shed_from: None,
                shed_at: None,
                static_hash: None,
                tail_hash: None,
                lane: phoebis_routing::lane_key("claude-sonnet-5", "standard", "none", "-"),
                entrypoint: String::new(),
                marked_at: None,
                marked_on: None,
                marked_window: None,
            })
            .unwrap();
    }
    let parts = common::admin_parts_full_on(seed.store.clone()).await;
    let cookie = login(&parts.app, "admin-secret").await;
    let generation = common::generation(&seed.store, "desktop");
    for i in 0..3 {
        parts.ledger.record(record_at(
            &format!("p{i}"),
            now - i * HOUR,
            "claude-sonnet-5",
            "served",
            generation,
        ));
    }
    fake_upstream::wait_for_async(Duration::from_secs(5), || async {
        parts.db.request("p2").await.unwrap()
    })
    .await;
    let sessions = json(get(&parts.app, "/admin/sessions", Some(&cookie)).await).await;
    let row = |session: &str| {
        sessions["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["session_id"] == session)
            .unwrap_or_else(|| panic!("{session} is listed"))
            .clone()
    };
    assert_eq!(
        (
            row("served")["requests"].as_u64(),
            row("served")["pinned"].as_bool()
        ),
        (Some(3), Some(true)),
        "a pin the ledger saw carries the ledger's count"
    );
    assert!(
        row("silent")["requests"].is_null(),
        "a pin with no ledger row has no count to report"
    );
    assert_eq!(row("silent")["pinned"].as_bool(), Some(true));
}

/// A pin on plan quota is priced in its account's window, so a tier the store cannot read fails the route rather
/// than reading as a tier the table does not size.
#[tokio::test]
async fn the_sessions_route_answers_500_when_an_accounts_tier_cannot_be_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    let store = std::sync::Arc::new(
        phoebis_store::Store::open(&path, &phoebis_store::StorageKey::generate()).unwrap(),
    );
    // The router loads the pins at spawn, so the pool, the key and the pin are written through one harness and
    // the store is handed to the one under test.
    let seed = common::admin_parts_full_on(store.clone()).await;
    let seeding = login(&seed.app, "admin-secret").await;
    post(&seed.app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority","members":[{"account_id":"business","priority":0}]}"#, Some(&seeding)).await;
    post(
        &seed.app,
        "/admin/keys",
        r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
        Some(&seeding),
    )
    .await;
    let now = common::NOW;
    store
        .upsert_pin(&phoebis_store::PinRow {
            key_id: "desktop".into(),
            session_id: "planned".into(),
            agent_id: "main".into(),
            model: "claude-sonnet-5".into(),
            account_id: "business".into(),
            epoch: 1,
            created_at: now - 10,
            last_used_at: now - 5,
            key_generation: common::generation(&store, "desktop"),
            incarnation: 1,
            burn: phoebis_store::PinBurn::default(),
            burn_at: None,
            burn_banded: phoebis_store::PinBurn::default(),
            burn_fast: phoebis_store::PinBurn::default(),
            burn_unmetered: phoebis_store::PinBurn::default(),
            last_input_tokens: 0,
            last_classifier: false,
            last_refresh_at: 0,
            cache_ttl_ms: 300_000,
            shed_from: None,
            shed_at: None,
            static_hash: None,
            tail_hash: None,
            lane: phoebis_routing::lane_key("claude-sonnet-5", "standard", "none", "-"),
            entrypoint: String::new(),
            marked_at: None,
            marked_on: None,
            marked_window: None,
        })
        .unwrap();
    let parts = common::admin_parts_full_on(store).await;
    let cookie = login(&parts.app, "admin-secret").await;
    // The tier read names the column, and no other read the route makes does.
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "ALTER TABLE accounts RENAME COLUMN subscription_tier TO subscription_tier_held;",
    )
    .unwrap();
    drop(conn);
    let listed = get(&parts.app, "/admin/sessions", Some(&cookie)).await;
    assert_eq!(listed.status(), 500);
    assert_eq!(
        json(listed).await,
        serde_json::json!({"error": "storage failure"})
    );
}

/// Only a row priced in the window share reads a tier, so a subscription account with no pin in the window costs the
/// route no read and cannot fail it.
#[tokio::test]
async fn a_failed_tier_read_on_an_account_no_row_prices_leaves_the_sessions_route_at_200() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    let store = std::sync::Arc::new(
        phoebis_store::Store::open(&path, &phoebis_store::StorageKey::generate()).unwrap(),
    );
    let parts = common::admin_parts_full_on(store).await;
    let cookie = login(&parts.app, "admin-secret").await;
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "ALTER TABLE accounts RENAME COLUMN subscription_tier TO subscription_tier_held;",
    )
    .unwrap();
    drop(conn);
    let listed = get(&parts.app, "/admin/sessions", Some(&cookie)).await;
    assert_eq!(listed.status(), 200);
    assert_eq!(json(listed).await, serde_json::json!({"sessions": []}));
}

#[tokio::test]
async fn a_non_numeric_limit_on_the_request_log_is_refused() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let refused = get(&parts.app, "/admin/requests?limit=abc", Some(&cookie)).await;
    assert_eq!(refused.status(), 400);
    assert_eq!(json(refused).await["error"], "limit is not a number");
    assert_eq!(
        get(&parts.app, "/admin/requests?limit=5", Some(&cookie))
            .await
            .status(),
        200
    );
}

/// Whether the point answers `field` as null: a key the route left out indexes to null too, which a shape without
/// the field would pass.
fn null_field(point: &serde_json::Value, field: &str) -> bool {
    point.get(field) == Some(&serde_json::Value::Null)
}

/// Stores the Max 20x tier on `business`, as a profile read does, and reloads the router, whose registry snapshot the
/// landing records each attempt's tier from: a row landed before the reload keeps the tier it recorded.
async fn set_max_20x(parts: &common::Full) {
    parts
        .store
        .set_profile(
            "business",
            r#"{"account":{"uuid":"u","email":"business@example.invalid"},"organization":{}}"#,
            &phoebis_store::SubscriptionRow {
                plan: Some("max".into()),
                tier: Some("default_claude_max_20x".into()),
                since: Some(1_000_000),
                trial_ends_at: None,
            },
        )
        .unwrap();
    parts.router.reload().await.unwrap();
}

#[tokio::test]
async fn the_burn_route_answers_each_subscription_points_share_of_both_windows() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let now = common::NOW;
    // An hour of a million input tokens on `business`.
    let million = |id: &str, at: i64, model: &str| {
        let mut row = record(id, at, model, "s1");
        if let Record::Request { attempts, .. } = &mut row {
            attempts[0].input_tokens = Some(1_000_000);
            attempts[0].output_tokens = None;
        }
        row
    };
    let opus = |id: &str, at: i64| million(id, at, "claude-opus-5-5");
    let business = |v: &serde_json::Value, at: i64| {
        v["points"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["group"] == "business" && p["at"] == phoebis_ledger::hour_of(at) * 1_000)
            .cloned()
            .unwrap_or_else(|| panic!("a bucket carries the recorded attempt: {v}"))
    };
    let burn_url = "/admin/analytics/burn?range=24h&group_by=account";
    // No tier on file yet: the upstream is a subscription whose window no table sizes, and its row records none.
    parts.ledger.record(opus("burn-0", now - 2 * HOUR));
    landed(&parts, &["burn-0"]).await;
    let tierless = business(
        &json(get(&parts.app, burn_url, Some(&cookie)).await).await,
        now - 2 * HOUR,
    );
    assert!(
        tierless["unit"] == "window_share"
            && null_field(&tierless, "five_hour_share")
            && null_field(&tierless, "seven_day_share"),
        "a subscription with no tier prices no share: {tierless}"
    );
    set_max_20x(&parts).await;
    parts.ledger.record(opus("burn-1", now - HOUR));
    // Fable is the one model the shipped table gives a week of its own.
    parts
        .ledger
        .record(million("burn-fable", now - 3 * HOUR, "claude-fable-5-1"));
    landed(&parts, &["burn-1", "burn-fable"]).await;
    let burn = json(get(&parts.app, burn_url, Some(&cookie)).await).await;
    let earlier = business(&burn, now - 2 * HOUR);
    assert!(
        null_field(&earlier, "five_hour_share") && null_field(&earlier, "seven_day_share"),
        "the row landed before the tier keeps pricing no share: {earlier}"
    );
    assert_eq!(burn["group_by"], "account");
    let counts_every_token = |v: &serde_json::Value| {
        v["points"].as_array().unwrap().iter().all(|p| {
            let sides = p["cached_tokens"].as_u64().unwrap_or(0)
                + p["uncached_tokens"].as_u64().unwrap_or(0);
            p["tokens"].as_u64().is_some_and(|tokens| tokens >= sides)
        })
    };
    assert!(
        counts_every_token(&burn),
        "each point counts every token type, its cached and uncached input among them: {burn}"
    );
    let point = business(&burn, now - HOUR);
    let tables = phoebis_ledger::PlanTables::shipped();
    let hour = point["at"].as_i64().unwrap();
    let usage = phoebis_routing::PinUsage {
        input: 1_000_000,
        ..phoebis_routing::PinUsage::default()
    };
    let share = |window: &str| {
        tables
            .share_at(
                window,
                None,
                "default_claude_max_20x",
                "claude-opus-5-5",
                usage,
                hour,
            )
            .unwrap()
    };
    let five_hour = share(phoebis_routing::FIVE_HOUR);
    assert!(
        (point["five_hour_share"].as_f64().unwrap() - five_hour).abs() < 1e-9,
        "a million Opus 5.5 input tokens read the model's 5-hour share on Max 20x: {point} against {five_hour}"
    );
    let seven_day = share(phoebis_routing::SEVEN_DAY);
    assert!(
        (point["seven_day_share"].as_f64().unwrap() - seven_day).abs() < 1e-9,
        "and its 7-day share: {point} against {seven_day}"
    );
    assert_eq!(
        (&point["unit"], &point["usd"]),
        (&serde_json::json!("window_share"), &serde_json::json!(0.0)),
        "a subscription spends no dollars: {point}"
    );
    let equivalent = |window: &str, model: &str, at: i64| {
        tables
            .equivalent_at(window, None, model, usage, at)
            .unwrap()
    };
    let five_hour_equivalent = equivalent(phoebis_routing::FIVE_HOUR, "claude-opus-5-5", hour);
    let seven_day_equivalent = equivalent(phoebis_routing::SEVEN_DAY, "claude-opus-5-5", hour);
    assert!(
        close_to(&point["five_hour_equivalent"], five_hour_equivalent)
            && close_to(&point["seven_day_equivalent"], seven_day_equivalent)
            && point["estimated"] == false,
        "beside its shares the bucket reads the 20x equivalent of both windows, its model weighted: {point}"
    );
    assert!(
        null_field(&point, "model_week_share") && null_field(&point, "model_week_equivalent"),
        "Opus 5.5 has no week of its own: {point}"
    );
    let fable = business(&burn, now - 3 * HOUR);
    let fable_hour = fable["at"].as_i64().unwrap();
    let week_share = tables
        .share_at(
            phoebis_routing::SEVEN_DAY_FABLE,
            None,
            "default_claude_max_20x",
            "claude-fable-5-1",
            usage,
            fable_hour,
        )
        .unwrap();
    let week_equivalent = equivalent(
        phoebis_routing::SEVEN_DAY_FABLE,
        "claude-fable-5-1",
        fable_hour,
    );
    assert!(
        close_to(&fable["model_week_share"], week_share)
            && close_to(&fable["model_week_equivalent"], week_equivalent),
        "a Fable row reads its model week's share and equivalent beside the shared windows: {fable}"
    );
    let mut keys: Vec<&str> = point
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "at",
            "cached_tokens",
            "estimated",
            "five_hour_equivalent",
            "five_hour_share",
            "group",
            "model_week_equivalent",
            "model_week_share",
            "seven_day_equivalent",
            "seven_day_share",
            "tokens",
            "uncached_tokens",
            "unit",
            "usd"
        ],
        "a point carries its shares, its 20x equivalents and the estimated flag: {point}"
    );
    parts.ledger.record(Record::Request {
        request: RequestRecord {
            id: "burn-2".into(),
            received_at: now - HOUR,
            finished_at: Some(now - HOUR + 500),
            key_id: Some("desktop".into()),
            pool_id: Some("main".into()),
            session_id: Some("s2".into()),
            model: "claude-sonnet-5".into(),
            canonical_model: "claude-sonnet-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            status: Some(200),
            outcome: "succeeded".into(),
            attempts: 1,
            ..Default::default()
        },
        attempts: vec![AttemptRecord {
            request_id: "burn-2".into(),
            attempt: 1,
            account_id: "direct".into(),
            account_kind: "api_key".into(),
            started_at: now - HOUR,
            outcome: "succeeded".into(),
            input_tokens: Some(1_000),
            output_tokens: Some(100),
            ..Default::default()
        }],
        bodies: None,
    });
    let with_direct = wait_json(&parts.app, burn_url, &cookie, |v| {
        v["points"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["group"] == "direct" && p["usd"].as_f64().unwrap_or(0.0) > 0.0)
    })
    .await;
    assert!(
        with_direct["points"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| p["group"] == "direct")
            .all(|p| null_field(p, "five_hour_share")
                && null_field(p, "seven_day_share")
                && p["unit"] == "usd"),
        "an api_key account has no window to read a share of"
    );
    let by_key = json(
        get(
            &parts.app,
            "/admin/analytics/burn?range=24h&group_by=key",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert!(
        counts_every_token(&by_key),
        "under every grouping: {by_key}"
    );
    let keyed = by_key["points"].as_array().unwrap();
    assert!(
        keyed
            .iter()
            .any(|p| p["uncached_tokens"].as_u64().unwrap_or(0) >= 1_000_000),
        "the key's bucket carries the subscription's tokens: {by_key}"
    );
    assert!(
        keyed.iter().all(|p| null_field(p, "five_hour_share")
            && null_field(p, "seven_day_share")
            && null_field(p, "model_week_share")
            && null_field(p, "unit")),
        "a key spans accounts whose window shares do not add, so with no filter naming one account it answers no \
         share and no unit"
    );
    let bucket_at = |points: &[serde_json::Value], at: i64| {
        points
            .iter()
            .find(|p| p["at"] == at)
            .cloned()
            .unwrap_or_else(|| panic!("a bucket at {at}: {points:?}"))
    };
    let keyed_opus = bucket_at(keyed, hour);
    assert!(
        close_to(&keyed_opus["five_hour_equivalent"], five_hour_equivalent)
            && close_to(&keyed_opus["seven_day_equivalent"], seven_day_equivalent)
            && keyed_opus["estimated"] == false,
        "the 20x equivalent adds across accounts, the api_key row adding a zero to the subscription's: {keyed_opus}"
    );
    assert!(
        close_to(
            &bucket_at(keyed, fable_hour)["model_week_equivalent"],
            week_equivalent
        ),
        "and so does the model week's: {by_key}"
    );
    let one_account = json(
        get(
            &parts.app,
            "/admin/analytics/burn?range=24h&group_by=key&accounts=business",
            Some(&cookie),
        )
        .await,
    )
    .await;
    let named = one_account["points"].as_array().unwrap();
    let named_opus = bucket_at(named, hour);
    assert!(
        named_opus["unit"] == "window_share"
            && close_to(&named_opus["five_hour_share"], five_hour)
            && close_to(&named_opus["seven_day_share"], seven_day)
            && named_opus["usd"] == 0.0,
        "a filter naming one account answers its unit and its shares under any grouping, its rows alone: {named_opus}"
    );
    assert!(
        close_to(
            &bucket_at(named, fable_hour)["model_week_share"],
            week_share
        ),
        "the model week's share among them: {one_account}"
    );
    let twice = json(
        get(
            &parts.app,
            "/admin/analytics/burn?range=24h&group_by=key&accounts=business,business",
            Some(&cookie),
        )
        .await,
    )
    .await;
    let twice_opus = bucket_at(twice["points"].as_array().unwrap(), hour);
    assert!(
        twice_opus["unit"] == "window_share"
            && close_to(&twice_opus["five_hour_share"], five_hour)
            && close_to(&twice_opus["seven_day_share"], seven_day),
        "a name given twice counts once, so the filter still names one account: {twice_opus}"
    );
    let by_all = get(
        &parts.app,
        "/admin/analytics/burn?range=24h&group_by=all",
        Some(&cookie),
    )
    .await;
    assert_eq!(by_all.status(), 400, "the burn groups by no `all`");
    assert_eq!(
        json(by_all).await["error"],
        "group_by is account, classifier, client, effort, key, kind, model, pool, request_class, session, speed, or user_agent"
    );
    let by_pool = get(
        &parts.app,
        "/admin/analytics/burn?range=24h&group_by=pool",
        Some(&cookie),
    )
    .await;
    assert_eq!(
        by_pool.status(),
        200,
        "the burn groups by every dimension of the page's group-by"
    );
    let by_pool = json(by_pool).await;
    let pools: std::collections::BTreeSet<&str> = by_pool["points"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["group"].as_str())
        .collect();
    assert_eq!(
        pools,
        std::collections::BTreeSet::from(["main"]),
        "the groups are the pools the range's rows name: {by_pool}"
    );
}

#[tokio::test]
async fn the_reconciliations_route_lists_the_trailing_thirty_days_under_the_account_filter() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let now = common::NOW;
    const DAY: i64 = 24 * HOUR;
    // Three windows inside the trailing 30 days on two accounts, and one of `business` that ended a millisecond
    // before them.
    let planted = [
        ("business", "five_hour", now - DAY, 2.0),
        ("business", "seven_day", now - 3 * DAY, 1.1),
        ("other", "five_hour", now - 2 * DAY, 0.9),
        ("business", "five_hour", now - 30 * DAY - 1, 1.0),
    ];
    parts
        .db
        .write(|c| {
            for (account, window, window_end, ratio) in planted {
                c.execute(
                    "INSERT INTO window_reconciliations (account_id, window, window_end, first_reading_at, \
                     last_reading_at, observed_rise, predicted_share, ratio, requests, estimated, computed_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, 0.2, ?6, ?7, 3, 1, ?8)",
                    rusqlite::params![
                        account,
                        window,
                        window_end,
                        window_end - 4 * HOUR,
                        window_end - HOUR,
                        0.2 / ratio,
                        ratio,
                        window_end + HOUR
                    ],
                )?;
            }
            Ok(())
        })
        .unwrap();
    let ends = |body: &serde_json::Value| {
        body["reconciliations"]
            .as_array()
            .unwrap_or_else(|| panic!("a list of rows: {body}"))
            .iter()
            .map(|r| {
                (
                    r["account_id"].as_str().unwrap().to_string(),
                    r["window"].as_str().unwrap().to_string(),
                    r["window_end"].as_i64().unwrap(),
                )
            })
            .collect::<Vec<_>>()
    };
    let every = json(
        get(
            &parts.app,
            "/admin/analytics/reconciliations",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        every.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["reconciliations", "request_retention_days"],
        "{every}"
    );
    assert_eq!(
        every["request_retention_days"].as_u64(),
        Some(
            phoebis_ledger::LedgerConfig::default()
                .retention_days
                .into()
        ),
        "the requests the ledger keeps, which bound how far back a window can be reconciled"
    );
    assert_eq!(
        ends(&every),
        [
            ("business".to_string(), "five_hour".to_string(), now - DAY),
            ("other".to_string(), "five_hour".to_string(), now - 2 * DAY),
            (
                "business".to_string(),
                "seven_day".to_string(),
                now - 3 * DAY
            ),
        ],
        "newest first, the window that ended before the trailing 30 days left out"
    );
    assert_eq!(
        every["reconciliations"][0],
        serde_json::json!({
            "account_id": "business",
            "window": "five_hour",
            "window_end": now - DAY,
            "first_reading_at": now - DAY - 4 * HOUR,
            "last_reading_at": now - DAY - HOUR,
            "observed_rise": 0.2,
            "predicted_share": 0.1,
            "ratio": 2.0,
            "requests": 3,
            "estimated": 1,
            "computed_at": now - DAY + HOUR,
        }),
        "a row carries its readings, its figures and its counts"
    );
    let business = json(
        get(
            &parts.app,
            "/admin/analytics/reconciliations?accounts=business",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        ends(&business),
        [
            ("business".to_string(), "five_hour".to_string(), now - DAY),
            (
                "business".to_string(),
                "seven_day".to_string(),
                now - 3 * DAY
            ),
        ],
        "the filter keeps the named accounts' rows"
    );
}

#[tokio::test]
async fn a_fast_answer_moves_a_subscription_groups_dollars_and_no_share() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    set_max_20x(&parts).await;
    let mut fast = record("fast-1", common::NOW - HOUR, "claude-opus-5", "s1");
    if let Record::Request { attempts, .. } = &mut fast {
        attempts[0].input_tokens = Some(1_000_000);
        attempts[0].output_tokens = None;
        attempts[0].speed = Some("fast".into());
    }
    parts.ledger.record(fast);
    let burn_url = "/admin/analytics/burn?range=24h&group_by=account";
    let burn = wait_json(&parts.app, burn_url, &cookie, |v| {
        v["points"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["group"] == "business" && p["uncached_tokens"].as_u64().unwrap_or(0) > 0)
    })
    .await;
    let point = burn["points"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["group"] == "business" && p["uncached_tokens"].as_u64().unwrap_or(0) > 0)
        .cloned()
        .unwrap();
    // A million tokens at the fast cell's price per million read that price in dollars.
    let fast_input = phoebis_ledger::PriceRevisions::shipped()
        .in_force(common::NOW)
        .price_at("claude-opus-5", phoebis_ledger::Speed::Fast, 1_000_000)
        .unwrap()
        .input;
    assert!(
        (point["usd"].as_f64().unwrap() - fast_input).abs() < 1e-9,
        "a million Opus 5 input tokens served fast read the fast cell in dollars: {point}"
    );
    assert_eq!(
        (
            &point["unit"],
            &point["five_hour_share"],
            &point["seven_day_share"],
        ),
        (
            &serde_json::json!("window_share"),
            &serde_json::json!(0.0),
            &serde_json::json!(0.0),
        ),
        "the account still charts in its window unit at a zero share, so it reads as priced beside its dollars: \
         {point}"
    );
    assert_eq!(
        (
            &point["five_hour_equivalent"],
            &point["seven_day_equivalent"],
            &point["estimated"],
        ),
        (
            &serde_json::json!(0.0),
            &serde_json::json!(0.0),
            &serde_json::json!(false),
        ),
        "a bucket with tokens and no plan row reads a zero 20x equivalent, and a fast row marks no estimate: {point}"
    );
}

/// Waits until every named request row has landed, so a read that follows sees all of their rollups.
async fn landed(parts: &common::Full, ids: &[&str]) {
    for id in ids {
        fake_upstream::wait_for_async(Duration::from_secs(5), || async {
            parts.db.request(id).await.unwrap()
        })
        .await;
    }
}

/// A cached Sonnet 5.5 request and a probe on `business`, and a `direct` request, an hour ago.
fn share_rows() -> Vec<Record> {
    let at = common::NOW - HOUR;
    let mut cached = record("share-cached", at, "claude-sonnet-5-5", "s1");
    if let Record::Request { attempts, .. } = &mut cached {
        attempts[0].cache_read_tokens = Some(10_000);
    }
    let mut probe = record("share-probe", at, "claude-sonnet-5-5", "s1");
    if let Record::Request {
        request, attempts, ..
    } = &mut probe
    {
        request.kind = "probe".into();
        attempts[0].input_tokens = None;
        attempts[0].output_tokens = None;
        attempts[0].cache_read_tokens = Some(2_000);
    }
    let mut direct = record("share-direct", at, "claude-sonnet-5-5", "s2");
    if let Record::Request { attempts, .. } = &mut direct {
        attempts[0].account_id = "direct".into();
        attempts[0].account_kind = "api_key".into();
    }
    vec![cached, probe, direct]
}

/// An overage row, a fast row and a plan row on `business`, a `direct` row, and a plan row on an account the
/// registry does not hold, an hour ago.
fn funded_rows() -> Vec<Record> {
    let at = common::NOW - HOUR;
    let mut overage = record("fund-overage", at, "claude-sonnet-5-5", "s1");
    if let Record::Request { attempts, .. } = &mut overage {
        attempts[0].overage_in_use = Some(true);
    }
    let mut fast = record("fund-fast", at, "claude-sonnet-5-5", "s1");
    if let Record::Request { attempts, .. } = &mut fast {
        attempts[0].speed = Some("fast".into());
    }
    let mut direct = record("fund-direct", at, "claude-sonnet-5-5", "s2");
    if let Record::Request { attempts, .. } = &mut direct {
        attempts[0].account_id = "direct".into();
        attempts[0].account_kind = "api_key".into();
    }
    let plan = record("fund-plan", at, "claude-sonnet-5-5", "s1");
    let mut gone = record("fund-gone", at, "claude-sonnet-5-5", "s3");
    if let Record::Request { attempts, .. } = &mut gone {
        attempts[0].account_id = "gone".into();
    }
    vec![overage, fast, direct, plan, gone]
}

fn row_of<'a>(body: &'a serde_json::Value, key: &str) -> &'a serde_json::Value {
    body["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["key"] == key)
        .unwrap_or_else(|| panic!("a row for {key}: {body}"))
}

/// Each row's key and request count, in the route's order.
fn request_counts(body: &serde_json::Value) -> Vec<(String, u64)> {
    body["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["key"].as_str().unwrap().to_string(),
                r["requests"].as_u64().unwrap(),
            )
        })
        .collect()
}

const SHARE_MEMBERS: [&str; 6] = [
    "cache_shares",
    "cost_shares",
    "plan_output_tokens",
    "plan_requests",
    "probe_shares",
    "saving_shares",
];

/// The share members alone: the plan-funded counts beside them answer under every grouping.
const SHARES: [&str; 4] = [
    "cache_shares",
    "cost_shares",
    "probe_shares",
    "saving_shares",
];

fn positive(v: &serde_json::Value) -> bool {
    v.as_f64().is_some_and(|x| x > 0.0)
}

#[tokio::test]
async fn the_breakdown_answers_shares_for_an_oauth_group_under_the_account_grouping_and_null_elsewhere(
) {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    set_max_20x(&parts).await;
    for row in share_rows() {
        parts.ledger.record(row);
    }
    landed(&parts, &["share-cached", "share-probe", "share-direct"]).await;
    let url = "/admin/analytics/breakdown?range=24h&group_by=account";
    let body = json(get(&parts.app, url, Some(&cookie)).await).await;
    let business = row_of(&body, "business");
    assert!(
        positive(&business["cost_shares"]["five_hour"])
            && positive(&business["cost_shares"]["seven_day"])
            && business["cost_shares"]
                .as_object()
                .is_some_and(|m| m.len() == 2),
        "a Sonnet account prices the two shared windows and no week of its own: {business}"
    );
    assert!(
        positive(&business["cache_shares"]["five_hour"]["read"]),
        "the cached row's read share: {business}"
    );
    assert!(
        positive(&business["probe_shares"]["five_hour"]["cost"])
            && business["probe_shares"]["five_hour"]["wasted"].is_null(),
        "the probe's share with no waste recorded: {business}"
    );
    assert_eq!(
        business["kept_warm"].as_object().map(|kept| kept
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>()),
        Some(std::collections::BTreeSet::from([
            "requests",
            "tokens_1h",
            "tokens_5m"
        ])),
        "the kept-warm requests and their tokens per TTL, with no sum: {business}"
    );
    assert!(
        business["saving_usd"].is_number()
            && business["saving_shares"].is_object()
            && business["saving_equivalent"].is_object(),
        "the saving's three members beside the probe cost: {business}"
    );
    assert_eq!(
        (&business["plan_requests"], &business["plan_output_tokens"]),
        (&serde_json::json!(1), &serde_json::json!(100)),
        "the probe row stays out of the request side by default: {business}"
    );
    let with_probes = json(
        get(
            &parts.app,
            &format!("{url}&include_probes=true"),
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        row_of(&with_probes, "business")["plan_requests"],
        2,
        "and joins it under include_probes: {with_probes}"
    );
    let direct = row_of(&body, "direct");
    for member in SHARES {
        assert!(
            null_field(direct, member),
            "an api_key group answers {member} null: {direct}"
        );
    }
    assert_eq!(
        (&direct["plan_requests"], &direct["plan_output_tokens"]),
        (&serde_json::json!(0), &serde_json::json!(0)),
        "and counts no plan-funded request: {direct}"
    );
    let by_model = json(
        get(
            &parts.app,
            "/admin/analytics/breakdown?range=24h&group_by=model",
            Some(&cookie),
        )
        .await,
    )
    .await;
    for row in by_model["rows"].as_array().unwrap() {
        for member in SHARES {
            assert!(
                null_field(row, member),
                "a model spans accounts, so with no filter naming one it answers {member} null: {row}"
            );
        }
    }
    let sonnet = row_of(&by_model, "claude-sonnet-5-5");
    assert_eq!(
        (&sonnet["plan_requests"], &sonnet["plan_output_tokens"]),
        (&serde_json::json!(1), &serde_json::json!(100)),
        "the plan-funded counts answer under every grouping: {sonnet}"
    );
    assert!(
        positive(&sonnet["cost_equivalent"]["five_hour"])
            && positive(&sonnet["billed_usd"])
            && sonnet["estimated"] == false,
        "the plan row's equivalent and the api_key row's dollars answer beside today's members: {sonnet}"
    );
    let one_account = json(
        get(
            &parts.app,
            "/admin/analytics/breakdown?range=24h&group_by=model&accounts=business",
            Some(&cookie),
        )
        .await,
    )
    .await;
    let named = row_of(&one_account, "claude-sonnet-5-5");
    assert!(
        positive(&named["cost_shares"]["five_hour"]),
        "a filter naming one account answers the shares under the model grouping: {named}"
    );
    for member in SHARES {
        assert_eq!(
            named[member], business[member],
            "the one account's model row answers the account row's {member}, its rows the same: {named} vs \
             {business}"
        );
    }
}

#[tokio::test]
async fn the_series_points_carry_the_same_shares_per_bucket() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    set_max_20x(&parts).await;
    for row in share_rows() {
        parts.ledger.record(row);
    }
    landed(&parts, &["share-cached", "share-probe", "share-direct"]).await;
    let breakdown = json(
        get(
            &parts.app,
            "/admin/analytics/breakdown?range=24h&group_by=account",
            Some(&cookie),
        )
        .await,
    )
    .await;
    let series = json(
        get(
            &parts.app,
            "/admin/analytics/series?range=24h&group_by=account",
            Some(&cookie),
        )
        .await,
    )
    .await;
    let point = series["points"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|p| {
            p["groups"]
                .as_array()
                .unwrap()
                .iter()
                .find(|g| g["key"] == "business")
        })
        .unwrap_or_else(|| panic!("a bucket carries the business group: {series}"));
    let business = row_of(&breakdown, "business");
    for member in SHARE_MEMBERS {
        assert_eq!(
            point[member], business[member],
            "the bucket's {member} is the breakdown row's: {point} vs {business}"
        );
    }
    assert!(
        positive(&point["cost_shares"]["five_hour"]),
        "and it is priced: {point}"
    );
    let model_group = |by_model: &serde_json::Value| {
        by_model["points"]
            .as_array()
            .unwrap()
            .iter()
            .find_map(|p| {
                p["groups"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|g| g["key"] == "claude-sonnet-5-5")
                    .cloned()
            })
            .unwrap_or_else(|| panic!("a bucket carries the model's group: {by_model}"))
    };
    let unfiltered = model_group(
        &json(
            get(
                &parts.app,
                "/admin/analytics/series?range=24h&group_by=model",
                Some(&cookie),
            )
            .await,
        )
        .await,
    );
    let named = model_group(
        &json(
            get(
                &parts.app,
                "/admin/analytics/series?range=24h&group_by=model&accounts=business",
                Some(&cookie),
            )
            .await,
        )
        .await,
    );
    for member in SHARES {
        assert!(
            null_field(&unfiltered, member),
            "a model spans accounts, so with no filter naming one its bucket answers {member} null: {unfiltered}"
        );
        assert_eq!(
            named[member], point[member],
            "a filter naming one account answers the account bucket's {member} under the model grouping: {named} \
             vs {point}"
        );
    }
}

#[tokio::test]
async fn funding_billed_keeps_the_overage_fast_and_api_key_rows_and_drops_the_plan_row() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    for row in funded_rows() {
        parts.ledger.record(row);
    }
    landed(
        &parts,
        &[
            "fund-overage",
            "fund-fast",
            "fund-direct",
            "fund-plan",
            "fund-gone",
        ],
    )
    .await;
    let billed = json(
        get(
            &parts.app,
            "/admin/analytics/breakdown?range=24h&group_by=account&funding=billed",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        request_counts(&billed),
        [("business".to_string(), 2), ("direct".to_string(), 1)],
        "the overage and fast rows and the api_key row, and neither plan row: {billed}"
    );
}

#[tokio::test]
async fn funding_plan_keeps_the_plan_row_and_one_of_an_account_the_registry_no_longer_holds() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    for row in funded_rows() {
        parts.ledger.record(row);
    }
    landed(
        &parts,
        &[
            "fund-overage",
            "fund-fast",
            "fund-direct",
            "fund-plan",
            "fund-gone",
        ],
    )
    .await;
    let read = |funding: &str| {
        let url = format!("/admin/analytics/breakdown?range=24h&group_by=account{funding}");
        let (app, cookie) = (&parts.app, &cookie);
        async move { request_counts(&json(get(app, &url, Some(cookie)).await).await) }
    };
    let plan = read("&funding=plan").await;
    assert_eq!(
        plan,
        [("business".to_string(), 1), ("gone".to_string(), 1)],
        "the plan row and the row of the account the registry does not hold"
    );
    let sum = |rows: &[(String, u64)]| rows.iter().map(|(_, n)| n).sum::<u64>();
    let billed = read("&funding=billed").await;
    let every = read("").await;
    assert_eq!(
        (sum(&billed) + sum(&plan), sum(&every)),
        (5, 5),
        "the two filtered sums add up to the unfiltered sum: {billed:?} and {plan:?}"
    );
}

/// The partition is recorded with each row, so the registry's later state moves no dollar between the two sides: the
/// api_key account's row stays billed once the account is deleted and once its id comes back as an oauth account.
#[tokio::test]
async fn the_funding_partition_holds_when_an_api_key_account_is_deleted_and_its_id_recreated_as_oauth(
) {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    for row in funded_rows() {
        parts.ledger.record(row);
    }
    landed(
        &parts,
        &[
            "fund-overage",
            "fund-fast",
            "fund-direct",
            "fund-plan",
            "fund-gone",
        ],
    )
    .await;
    let read = |funding: &str| {
        let url =
            format!("/admin/analytics/breakdown?range=24h&group_by=account&funding={funding}");
        let (app, cookie) = (&parts.app, &cookie);
        async move { request_counts(&json(get(app, &url, Some(cookie)).await).await) }
    };
    let partition = || async { (read("billed").await, read("plan").await) };
    let recorded = partition().await;
    assert_eq!(
        recorded,
        (
            vec![("business".to_string(), 2), ("direct".to_string(), 1)],
            vec![("business".to_string(), 1), ("gone".to_string(), 1)]
        ),
        "the partition as the rows were recorded"
    );
    assert_eq!(
        delete(&parts.app, "/admin/accounts/direct", Some(&cookie))
            .await
            .status(),
        204
    );
    assert_eq!(
        partition().await,
        recorded,
        "a deleted account's rows keep their funding"
    );
    parts
        .store
        .insert_account(&AccountRow {
            id: "direct".into(),
            kind: AccountKind::Oauth,
            label: "Direct again".into(),
            enabled: true,
            identity: None,
            lifecycle_generation: 1,
            incarnation: 1,
            revision: 1,
            state: AccountState::Unpaired,
            state_cause: StateCause::Operator,
        })
        .unwrap();
    assert_eq!(
        partition().await,
        recorded,
        "an id recreated as an oauth account keeps its api_key rows billed"
    );
}

#[tokio::test]
async fn an_unknown_funding_value_is_refused() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    for route in ["breakdown", "series"] {
        let refused = get(
            &parts.app,
            &format!("/admin/analytics/{route}?range=24h&funding=credits"),
            Some(&cookie),
        )
        .await;
        assert_eq!(refused.status(), 400, "{route}");
        assert_eq!(json(refused).await["error"], "funding is billed or plan");
    }
}

#[tokio::test]
async fn the_burn_route_ranks_sessions_by_tokens_and_prices_each_by_its_accounts_kind() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    // A tier on file, so the shares the session grouping leaves null are the grouping's and not a missing tier's.
    set_max_20x(&parts).await;
    let now = common::NOW;
    let session_request =
        |id: &str, at: i64, session: &str, account: &str, input: u64, output: u64| {
            Record::Request {
                request: RequestRecord {
                    id: id.into(),
                    received_at: at,
                    finished_at: Some(at + 500),
                    key_id: Some("desktop".into()),
                    pool_id: Some("main".into()),
                    session_id: Some(session.into()),
                    model: "claude-sonnet-5".into(),
                    canonical_model: "claude-sonnet-5".into(),
                    kind: "inference".into(),
                    origin: "client".into(),
                    status: Some(200),
                    outcome: "succeeded".into(),
                    attempts: 1,
                    ..Default::default()
                },
                attempts: vec![AttemptRecord {
                    request_id: id.into(),
                    attempt: 1,
                    account_id: account.into(),
                    account_kind: if account == "business" {
                        "oauth"
                    } else {
                        "api_key"
                    }
                    .into(),
                    started_at: at,
                    outcome: "succeeded".into(),
                    input_tokens: Some(input),
                    output_tokens: Some(output),
                    ..Default::default()
                }],
                bodies: None,
            }
        };
    // The most tokens, split an hour apart so the series carries it in two buckets. Each attempt's counts price to
    // nonzero dollars too, so the assertion below proves the oauth branch ignores them rather than merely never
    // having seen a value to add.
    parts.ledger.record(session_request(
        "r-big-1",
        now - HOUR - 100,
        "s-big",
        "business",
        2_000,
        200,
    ));
    parts.ledger.record(session_request(
        "r-big-2",
        now - 100,
        "s-big",
        "business",
        2_000,
        200,
    ));
    // Fewer tokens than "s-big", more than "s-small", on an api_key account.
    parts.ledger.record(session_request(
        "r-mid",
        now - 100,
        "s-mid",
        "direct",
        1_000,
        100,
    ));
    // The fewest tokens: last in the ranking.
    parts.ledger.record(session_request(
        "r-small",
        now - 100,
        "s-small",
        "business",
        10,
        1,
    ));
    fake_upstream::wait_for_async(Duration::from_secs(5), || async {
        parts.db.request("r-mid").await.unwrap()
    })
    .await;
    let burn = json(
        get(
            &parts.app,
            "/admin/analytics/burn?range=24h&group_by=session",
            Some(&cookie),
        )
        .await,
    )
    .await;
    let points = burn["points"].as_array().unwrap();
    let first_index = |group: &str| {
        points
            .iter()
            .position(|p| p["group"] == group)
            .unwrap_or_else(|| panic!("{group} is not in the ranked sessions: {points:?}"))
    };
    assert!(
        first_index("s-big") < first_index("s-mid")
            && first_index("s-mid") < first_index("s-small"),
        "ranked by tokens, most first: {points:?}"
    );
    let big_buckets: Vec<serde_json::Value> = points
        .iter()
        .filter(|p| p["group"] == "s-big" && p["uncached_tokens"].as_u64().unwrap_or(0) > 0)
        .cloned()
        .collect();
    assert_eq!(
        big_buckets.len(),
        2,
        "the hour-apart attempts land in two different buckets: {points:?}"
    );
    assert!(
        big_buckets.iter().all(|p| p["usd"].as_f64() == Some(0.0)),
        "an oauth session spends no dollars: {big_buckets:?}"
    );
    let mid_point = points
        .iter()
        .find(|p| p["group"] == "s-mid" && p["usd"].as_f64().unwrap_or(0.0) > 0.0)
        .unwrap_or_else(|| panic!("s-mid's bucket carries its dollar price: {points:?}"));
    assert_eq!(mid_point["uncached_tokens"].as_u64(), Some(1_000));
    // The attempt's counts at the shipped revision in force when its request arrived.
    let mid_usd = phoebis_ledger::PriceRevisions::shipped()
        .in_force(now - 100)
        .estimate(
            "claude-sonnet-5",
            phoebis_ledger::Speed::Standard,
            1_000,
            0,
            0,
            0,
            100,
        )
        .expect("the shipped list carries the model")
        .0;
    assert!(
        (mid_point["usd"].as_f64().unwrap() - mid_usd).abs() < 1e-12,
        "s-mid's bucket reads its attempt's dollars at read time: {mid_point} vs {mid_usd}"
    );
    assert_eq!(
        (
            &mid_point["five_hour_equivalent"],
            &mid_point["seven_day_equivalent"]
        ),
        (&serde_json::json!(0.0), &serde_json::json!(0.0)),
        "an api_key session draws no plan window: {mid_point}"
    );
    // A session can move between upstreams, and shares of different accounts' windows do not add.
    assert!(
        points.iter().all(|p| null_field(p, "five_hour_share")
            && null_field(p, "seven_day_share")
            && null_field(p, "unit")),
        "the session grouping answers no share with no filter naming one account, a tier on file or not: \
         {points:?}"
    );
    let tables = phoebis_ledger::PlanTables::shipped();
    let usage = phoebis_routing::PinUsage {
        input: 2_000,
        output: 200,
        ..phoebis_routing::PinUsage::default()
    };
    let one_account = json(
        get(
            &parts.app,
            "/admin/analytics/burn?range=24h&group_by=session&accounts=business",
            Some(&cookie),
        )
        .await,
    )
    .await;
    let named = one_account["points"].as_array().unwrap();
    assert!(
        named.iter().all(|p| p["group"] != "s-mid"),
        "the sessions rank by the named account's attempts alone: {one_account}"
    );
    for started in [now - HOUR - 100, now - 100] {
        let bucket_of = |points: &[serde_json::Value]| {
            points
                .iter()
                .find(|p| {
                    p["group"] == "s-big" && p["at"] == phoebis_ledger::hour_of(started) * 1_000
                })
                .cloned()
                .unwrap_or_else(|| panic!("s-big's bucket of {started}: {points:?}"))
        };
        // The band reads at the attempt's completion, the request's finish half a second after its start.
        let completed = started + 500;
        let equivalent = |window: &str| {
            tables
                .equivalent_at(window, None, "claude-sonnet-5", usage, completed)
                .unwrap()
        };
        let any = bucket_of(points);
        assert!(
            close_to(
                &any["five_hour_equivalent"],
                equivalent(phoebis_routing::FIVE_HOUR)
            ) && close_to(
                &any["seven_day_equivalent"],
                equivalent(phoebis_routing::SEVEN_DAY)
            ) && any["estimated"] == false,
            "a session's bucket reads its attempt's 20x equivalent with no account named: {any}"
        );
        let share = |window: &str| {
            tables
                .share_at(
                    window,
                    None,
                    "default_claude_max_20x",
                    "claude-sonnet-5",
                    usage,
                    completed,
                )
                .unwrap()
        };
        let one = bucket_of(named);
        assert!(
            one["unit"] == "window_share"
                && close_to(&one["five_hour_share"], share(phoebis_routing::FIVE_HOUR))
                && close_to(&one["seven_day_share"], share(phoebis_routing::SEVEN_DAY)),
            "with one account named, the bucket reads the shares of the tier its attempt recorded: {one}"
        );
    }
}

#[tokio::test]
async fn the_burn_key_grouping_names_the_live_holder_by_its_id_and_an_earlier_one_by_its_generation(
) {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    post(
        &parts.app,
        "/admin/pools",
        r#"{"id":"main","label":"Main","strategy":"priority"}"#,
        Some(&cookie),
    )
    .await;
    post(
        &parts.app,
        "/admin/keys",
        r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
        Some(&cookie),
    )
    .await;
    let generation = common::generation(&parts.store, "desktop");
    let earlier = generation - 1;
    let at = common::NOW - HOUR;
    parts.ledger.record(record_at(
        "key-live",
        at,
        "claude-sonnet-5-5",
        "s1",
        generation,
    ));
    parts.ledger.record(record_at(
        "key-earlier",
        at,
        "claude-sonnet-5-5",
        "s2",
        earlier,
    ));
    landed(&parts, &["key-live", "key-earlier"]).await;
    let burn = json(
        get(
            &parts.app,
            "/admin/analytics/burn?range=24h&group_by=key",
            Some(&cookie),
        )
        .await,
    )
    .await;
    let tokens: std::collections::BTreeMap<String, u64> = burn["points"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["tokens"].as_u64().unwrap_or(0) > 0)
        .map(|p| {
            (
                p["group"].as_str().unwrap().to_string(),
                p["tokens"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        tokens,
        std::collections::BTreeMap::from([
            ("desktop".to_string(), 1_100),
            (format!("desktop@{earlier}"), 1_100),
        ]),
        "the key that holds the id now reads under the id and an earlier holder under its generation"
    );
}

#[tokio::test]
async fn the_burn_files_an_agent_under_the_user_agent_group_its_pattern_names() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let agent = |id: &str, user_agent: &str| {
        let mut row = record(id, common::NOW - HOUR, "claude-sonnet-5-5", "s1");
        if let Record::Request { request, .. } = &mut row {
            request.user_agent = Some(user_agent.into());
        }
        row
    };
    parts
        .ledger
        .record(agent("agent-cli", "claude-cli/2.1.300 (external, cli)"));
    parts.ledger.record(agent("agent-curl", "curl/8.5.0"));
    landed(&parts, &["agent-cli", "agent-curl"]).await;
    let response = get(
        &parts.app,
        "/admin/analytics/burn?range=24h&group_by=user_agent&user_agent_groups=claude-cli%2F*",
        Some(&cookie),
    )
    .await;
    assert_eq!(
        response.status(),
        200,
        "the burn groups by the user agent as the page does"
    );
    let burn = json(response).await;
    let tokens: std::collections::BTreeMap<&str, u64> = burn["points"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["tokens"].as_u64().unwrap_or(0) > 0)
        .map(|p| (p["group"].as_str().unwrap(), p["tokens"].as_u64().unwrap()))
        .collect();
    assert_eq!(
        tokens,
        std::collections::BTreeMap::from([("claude-cli/*", 1_100), ("other", 1_100)]),
        "a pattern names its group and an agent no pattern matches files under other"
    );
    let too_many = (0..33)
        .map(|i| format!("p{i}"))
        .collect::<Vec<_>>()
        .join("%0A");
    let refused = get(
        &parts.app,
        &format!("/admin/analytics/burn?range=24h&user_agent_groups={too_many}"),
        Some(&cookie),
    )
    .await;
    assert_eq!(
        refused.status(),
        400,
        "the patterns are held to the bound the breakdown holds them to"
    );
    assert_eq!(
        json(refused).await["error"],
        phoebis_ledger::PATTERNS_TOO_LONG
    );
}

#[tokio::test]
async fn the_burn_route_answers_only_the_twenty_busiest_sessions() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let now = common::NOW;
    let session_request = |id: &str, session: &str, input: u64| Record::Request {
        request: RequestRecord {
            id: id.into(),
            received_at: now - 100,
            finished_at: Some(now - 50),
            key_id: Some("desktop".into()),
            pool_id: Some("main".into()),
            session_id: Some(session.into()),
            model: "claude-sonnet-5".into(),
            canonical_model: "claude-sonnet-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            status: Some(200),
            outcome: "succeeded".into(),
            attempts: 1,
            ..Default::default()
        },
        attempts: vec![AttemptRecord {
            request_id: id.into(),
            attempt: 1,
            account_id: "business".into(),
            account_kind: "oauth".into(),
            started_at: now - 100,
            outcome: "succeeded".into(),
            input_tokens: Some(input),
            output_tokens: Some(10),
            ..Default::default()
        }],
        bodies: None,
    };
    // Twenty-one sessions with distinct token counts, written so all twenty-one land in the writer's one
    // batch; the smallest carries the fewest tokens and is the one the twenty-wide cut must drop.
    for i in 0..21u64 {
        let id = format!("r-cut-{i:02}");
        let session = format!("s-cut-{i:02}");
        parts.ledger.record(session_request(&id, &session, 100 + i));
    }
    fake_upstream::wait_for_async(Duration::from_secs(5), || async {
        parts.db.request("r-cut-20").await.unwrap()
    })
    .await;
    let burn = json(
        get(
            &parts.app,
            "/admin/analytics/burn?range=24h&group_by=session",
            Some(&cookie),
        )
        .await,
    )
    .await;
    let points = burn["points"].as_array().unwrap();
    let groups: std::collections::BTreeSet<&str> = points
        .iter()
        .filter_map(|p| p["group"].as_str())
        .filter(|g| g.starts_with("s-cut-"))
        .collect();
    assert_eq!(
        groups.len(),
        20,
        "the twenty busiest sessions survive the cut: {groups:?}"
    );
    assert!(
        !groups.contains("s-cut-00"),
        "the session with the fewest tokens is the one the cut drops: {groups:?}"
    );
}

#[tokio::test]
async fn the_session_series_sizes_its_buckets_like_the_account_series_on_long_ranges() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    set_max_20x(&parts).await;
    let now = common::NOW;
    // A 7-day range buckets in 6 hours; `now` always falls in the trailing partial bucket the ceiling of the
    // hour leaves, since the newest bucket ends at `now`'s own hour rather than a 6-hour edge.
    parts.ledger.record(Record::Request {
        request: RequestRecord {
            id: "r-trailing".into(),
            received_at: now,
            finished_at: Some(now + 500),
            key_id: Some("desktop".into()),
            pool_id: Some("main".into()),
            session_id: Some("s-trailing".into()),
            model: "claude-sonnet-5-5".into(),
            canonical_model: "claude-sonnet-5-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            status: Some(200),
            outcome: "succeeded".into(),
            attempts: 1,
            ..Default::default()
        },
        attempts: vec![AttemptRecord {
            request_id: "r-trailing".into(),
            attempt: 1,
            account_id: "business".into(),
            account_kind: "oauth".into(),
            started_at: now,
            outcome: "succeeded".into(),
            input_tokens: Some(500),
            output_tokens: Some(50),
            ..Default::default()
        }],
        bodies: None,
    });
    fake_upstream::wait_for_async(Duration::from_secs(5), || async {
        parts.db.request("r-trailing").await.unwrap()
    })
    .await;
    let by_account = json(
        get(
            &parts.app,
            "/admin/analytics/burn?range=7d&group_by=account",
            Some(&cookie),
        )
        .await,
    )
    .await;
    let by_session = json(
        get(
            &parts.app,
            "/admin/analytics/burn?range=7d&group_by=session",
            Some(&cookie),
        )
        .await,
    )
    .await;
    let account_bucket_count = by_account["points"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["group"] == "business")
        .count();
    let session_points = by_session["points"].as_array().unwrap();
    let session_bucket_count = session_points
        .iter()
        .filter(|p| p["group"] == "s-trailing")
        .count();
    assert_eq!(
        session_bucket_count, account_bucket_count,
        "the session series answers as many points over the range as the account series does"
    );
    let trailing = session_points
        .iter()
        .rfind(|p| p["group"] == "s-trailing")
        .unwrap();
    assert_eq!(
        trailing["uncached_tokens"].as_u64(),
        Some(500),
        "the trailing partial bucket still carries the attempt inside it: {trailing}"
    );
    let account_trailing = by_account["points"]
        .as_array()
        .unwrap()
        .iter()
        .rfind(|p| p["group"] == "business")
        .unwrap();
    let seven_day = phoebis_ledger::PlanTables::shipped()
        .share_at(
            phoebis_routing::SEVEN_DAY,
            None,
            "default_claude_max_20x",
            "claude-sonnet-5-5",
            phoebis_routing::PinUsage {
                input: 500,
                output: 50,
                ..phoebis_routing::PinUsage::default()
            },
            phoebis_ledger::hour_of(now) * 1_000,
        )
        .unwrap();
    assert!(
        account_trailing["five_hour_share"].as_f64().unwrap_or(0.0) > 0.0
            && (account_trailing["seven_day_share"].as_f64().unwrap() - seven_day).abs() < 1e-9,
        "the account series prices the same attempt's share of both windows in its trailing bucket: \
         {account_trailing} against {seven_day}"
    );
}

#[tokio::test]
async fn the_rewrites_route_answers_the_series_and_the_moves_by_reason_and_by_fit() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let now = common::NOW;
    let hour_ago = now - HOUR;
    parts.ledger.record(Record::Request {
        request: RequestRecord {
            id: "r-rewrite".into(),
            received_at: hour_ago,
            finished_at: Some(hour_ago + 500),
            key_id: Some("desktop".into()),
            pool_id: Some("main".into()),
            session_id: Some("s1".into()),
            model: "claude-sonnet-5".into(),
            canonical_model: "claude-sonnet-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            status: Some(200),
            outcome: "succeeded".into(),
            attempts: 1,
            held: Some(
                r#"{"waited_on":["business","business"],"cohort":null,"held_ms":2500,"waits":[{"reason":"cap","held_ms":500},{"reason":"slow_down","held_ms":2000}]}"#
                    .into(),
            ),
            ..Default::default()
        },
        attempts: vec![AttemptRecord {
            request_id: "r-rewrite".into(),
            attempt: 1,
            account_id: "business".into(),
            account_kind: "oauth".into(),
            started_at: hour_ago,
            outcome: "succeeded".into(),
            cache_write_5m_tokens: Some(500),
            selection: Some(
                r#"{"strategy":"pace","tier":null,"renewed":false,"moved":"shed","skipped":[]}"#
                    .into(),
            ),
            ..Default::default()
        }],
        bodies: None,
    });
    parts.ledger.record(Record::Event(EventRecord {
        at: hour_ago + 100,
        kind: "pin_migrated".into(),
        account_id: Some("business".into()),
        key_id: Some("desktop".into()),
        session_id: Some("s1".into()),
        model: Some("claude-sonnet-5".into()),
        actor: None,
        detail: Some(
            serde_json::json!({ "from_account": "direct", "epoch": 2, "reason": "shed", "fit": "least_bad" })
                .to_string(),
        ),
        pool_id: None,
        agent_id: None,
        request_id: None,
    }));
    parts.ledger.record(Record::Event(EventRecord {
        at: hour_ago + 200,
        kind: "pin_migrated".into(),
        account_id: Some("direct".into()),
        key_id: Some("desktop".into()),
        session_id: Some("s2".into()),
        model: Some("claude-sonnet-5".into()),
        actor: None,
        detail: Some(
            serde_json::json!({ "from_account": "business", "epoch": 1, "reason": "spent", "fit": "fits" })
                .to_string(),
        ),
        pool_id: None,
        agent_id: None,
        request_id: None,
    }));
    fake_upstream::wait_for_async(Duration::from_secs(5), || async {
        parts.db.request("r-rewrite").await.unwrap()
    })
    .await;
    // The two moves land in the writer's batches after the request, so the read waits for both.
    let rewrites = wait_json(
        &parts.app,
        "/admin/analytics/rewrites?range=24h",
        &cookie,
        |v| v["moves"]["spent"] == 1,
    )
    .await;
    let total: u64 = rewrites["points"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["rewrite_tokens"].as_u64().unwrap())
        .sum();
    assert_eq!(total, 500, "the moved attempt's cache write is the rewrite");
    assert_eq!(
        rewrites["moves"],
        serde_json::json!({ "shed": 1, "spent": 1 }),
        "the moves by reason, each the sum over its fits"
    );
    assert_eq!(
        rewrites["moves_by_reason_and_fit"],
        serde_json::json!({ "shed": { "least_bad": 1 }, "spent": { "fits": 1 } }),
        "each reason keeps its destinations' own fits"
    );
    let points = rewrites["points"].as_array().unwrap();
    let held = rewrites["held"]
        .as_array()
        .expect("the held time per bucket");
    assert_eq!(
        held.iter().map(|p| &p["at"]).collect::<Vec<_>>(),
        points.iter().map(|p| &p["at"]).collect::<Vec<_>>(),
        "one held point per bucket, at the rewrite points' edges"
    );
    let waited: Vec<&serde_json::Value> = held.iter().filter(|p| p["requests"] != 0).collect();
    assert_eq!(
        waited
            .iter()
            .map(|p| (p["held_ms"].as_u64(), p["requests"].as_u64()))
            .collect::<Vec<_>>(),
        [(Some(2_000), Some(1))],
        "the request's slow-down wait alone, in its bucket: {held:?}"
    );
}

#[tokio::test]
async fn a_pool_row_answers_moves_per_hour_by_reason() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    post(
        &parts.app,
        "/admin/accounts",
        r#"{"id":"outside","kind":"oauth","label":"Outside"}"#,
        Some(&cookie),
    )
    .await;
    post(&parts.app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority","members":[{"account_id":"business","priority":0},{"account_id":"direct","priority":1}]}"#, Some(&cookie)).await;
    post(
        &parts.app,
        "/admin/pools",
        r#"{"id":"other","label":"Other","strategy":"priority","members":[{"account_id":"outside","priority":0}]}"#,
        Some(&cookie),
    )
    .await;
    post(
        &parts.app,
        "/admin/pools",
        r#"{"id":"empty","label":"Empty","strategy":"priority"}"#,
        Some(&cookie),
    )
    .await;
    let now = common::NOW;
    let event_at = |account: &str, reason: &str, at: i64| {
        Record::Event(EventRecord {
            at,
            kind: "pin_migrated".into(),
            account_id: Some(account.into()),
            key_id: Some("desktop".into()),
            session_id: Some("s1".into()),
            model: Some("claude-sonnet-5".into()),
            actor: None,
            detail: Some(serde_json::json!({ "reason": reason }).to_string()),
            pool_id: None,
            agent_id: None,
            request_id: None,
        })
    };
    let event = |account: &str, reason: &str| event_at(account, reason, now - 100);
    parts.ledger.record(event("business", "shed"));
    parts.ledger.record(event("direct", "shed"));
    parts.ledger.record(event("business", "spent"));
    // A shed of another pool's account, left out of "main"'s count.
    parts.ledger.record(event("outside", "shed"));
    // 61 minutes ago: outside the last hour, so this one is not counted either.
    parts
        .ledger
        .record(event_at("business", "shed", now - 61 * 60_000));
    let pools = wait_json(&parts.app, "/admin/pools", &cookie, |v| {
        v["pools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == "main")
            .is_some_and(|p| p["moves_per_hour"]["shed"] == 2.0)
    })
    .await;
    let pool = |id: &str| {
        pools["pools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == id)
            .unwrap()
            .clone()
    };
    assert_eq!(
        (
            pool("main")["moves_per_hour"]["shed"].as_f64(),
            pool("main")["moves_per_hour"]["spent"].as_f64()
        ),
        (Some(2.0), Some(1.0)),
        "the hour-old move and another pool's account are both left out"
    );
    assert_eq!(
        pool("empty")["moves_per_hour"],
        serde_json::json!({}),
        "a memberless pool answers an empty map without a query to attribute moves against"
    );
}

#[tokio::test]
async fn a_request_answers_its_class_and_each_attempts_trace_with_skipped_as_named_objects_on_the_wire(
) {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    parts.ledger.record(Record::Request {
        request: RequestRecord {
            id: "r-trace".into(),
            received_at: common::NOW,
            finished_at: Some(common::NOW + 500),
            key_id: Some("desktop".into()),
            pool_id: Some("main".into()),
            session_id: Some("s1".into()),
            model: "claude-sonnet-5".into(),
            canonical_model: "claude-sonnet-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            status: Some(200),
            outcome: "succeeded".into(),
            attempts: 1,
            request_class: Some("main".into()),
            ..Default::default()
        },
        attempts: vec![AttemptRecord {
            request_id: "r-trace".into(),
            attempt: 1,
            account_id: "business".into(),
            account_kind: "oauth".into(),
            started_at: common::NOW,
            outcome: "succeeded".into(),
            selection: Some(
                r#"{"strategy":"pace","tier":null,"renewed":false,"moved":null,"skipped":[{"account_id":"direct","obstacle":"cooldown"}]}"#
                    .into(),
            ),
            ..Default::default()
        }],
        bodies: None,
    });
    fake_upstream::wait_for_async(Duration::from_secs(5), || async {
        parts.db.request("r-trace").await.unwrap()
    })
    .await;
    let raw = get(&parts.app, "/admin/requests/r-trace", Some(&cookie)).await;
    let body = json(raw).await;
    assert_eq!(body["request"]["request_class"], "main");
    let skipped = &body["request"]["attempts"][0]["selection"]["skipped"];
    assert!(skipped.is_array());
    assert_eq!(
        skipped[0],
        serde_json::json!({ "account_id": "direct", "obstacle": "cooldown" }),
        "a named object on the wire, never a tuple: {body}"
    );
}

#[tokio::test]
async fn the_request_log_answers_the_diagnostics_source_and_narrows_by_reason_effort_and_class() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let planted = [
        (
            "d-client",
            Some("client"),
            Some("high"),
            Some("main"),
            "none",
        ),
        ("d-router", Some("router"), None, None, "messages_changed"),
        ("d-none", None, Some("ultra"), Some("bad class"), "none"),
    ];
    for ((id, source, effort, class, reason), age) in planted.into_iter().zip(1..) {
        let mut row = record(id, common::NOW - age * 1_000, "claude-sonnet-5", "s1");
        if let Record::Request {
            request, attempts, ..
        } = &mut row
        {
            request.diagnostics_source = source.map(String::from);
            request.effort = effort.map(String::from);
            request.request_class = class.map(String::from);
            attempts[0].cache_miss_reason = Some(reason.into());
        }
        parts.ledger.record(row);
    }
    fake_upstream::wait_for_async(Duration::from_secs(5), || async {
        parts.db.request("d-none").await.unwrap()
    })
    .await;
    let words = [
        ("d-client", serde_json::json!("client")),
        ("d-router", serde_json::json!("router")),
        ("d-none", serde_json::Value::Null),
    ];
    let listed = json(get(&parts.app, "/admin/requests", Some(&cookie)).await).await;
    let rows = listed["requests"].as_array().unwrap();
    assert_eq!(rows.len(), words.len(), "{listed}");
    for (row, (id, word)) in rows.iter().zip(&words) {
        assert_eq!(
            (row["id"].as_str(), row.get("diagnostics_source")),
            (Some(*id), Some(word)),
            "the list answers each row's source"
        );
        let shown =
            json(get(&parts.app, &format!("/admin/requests/{id}"), Some(&cookie)).await).await;
        assert_eq!(
            shown["request"].get("diagnostics_source"),
            Some(word),
            "the detail answers {id}'s source as the list does"
        );
    }
    for (query, expected) in [
        ("cache_miss_reasons=messages_changed", &["d-router"][..]),
        ("efforts=none,other", &["d-router", "d-none"]),
        ("request_classes=-", &["d-router"]),
        ("request_classes=other", &["d-none"]),
    ] {
        let body = json(
            get(
                &parts.app,
                &format!("/admin/requests?{query}"),
                Some(&cookie),
            )
            .await,
        )
        .await;
        let ids: Vec<&str> = body["requests"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["id"].as_str())
            .collect();
        assert_eq!(ids, expected, "{query}");
    }
}

#[tokio::test]
async fn budgets_route_answers_a_members_spend_against_its_budget() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    // "metered" carries a budget in "aux" (proving its row sorts by pool ahead of "main") while "business" rides
    // "main" unbudgeted (proving the route excludes it).
    parts
        .store
        .insert_account(&phoebis_store::AccountRow {
            id: "metered".into(),
            kind: phoebis_store::AccountKind::ApiKey,
            label: "Metered".into(),
            enabled: true,
            identity: None,
            lifecycle_generation: 1,
            incarnation: 1,
            revision: 1,
            state: phoebis_store::AccountState::Ready,
            state_cause: phoebis_store::StateCause::Operator,
        })
        .unwrap();
    post(
        &parts.app,
        "/admin/pools",
        r#"{"id":"main","label":"Main","strategy":"priority",
        "members":[{"account_id":"business","priority":0},
                   {"account_id":"direct","priority":1,"budget_daily_usd":20.0,"budget_monthly_usd":50.0}]}"#,
        Some(&cookie),
    )
    .await;
    post(
        &parts.app,
        "/admin/pools",
        r#"{"id":"aux","label":"Aux","strategy":"priority",
        "members":[{"account_id":"metered","priority":0,"budget_daily_usd":5.0}]}"#,
        Some(&cookie),
    )
    .await;
    // A real priced attempt, so the writer's own refresh reports a real spend; the route's job is reading it
    // back, not computing it.
    let mut spend = record("r1", common::NOW, "claude-sonnet-5", "s1");
    if let Record::Request { attempts, .. } = &mut spend {
        attempts[0].account_id = "direct".into();
        attempts[0].account_kind = "api_key".into();
    }
    parts.ledger.record(spend);
    let budgets = wait_json(&parts.app, "/admin/analytics/budgets", &cookie, |v| {
        v["members"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["spent_usd"].as_f64().is_some_and(|s| s > 0.0))
    })
    .await;
    let members = budgets["members"].as_array().unwrap();
    assert_eq!(
        members.len(),
        3,
        "business's unbudgeted membership in main is excluded; one row answers per budgeted period: {budgets}"
    );
    assert_eq!(
        (
            members[0]["pool_id"].as_str(),
            members[0]["account_id"].as_str(),
            members[0]["budget_usd"].as_f64(),
            members[0]["budget_period"].as_str(),
            members[0]["kind"].as_str(),
        ),
        (
            Some("aux"),
            Some("metered"),
            Some(5.0),
            Some("day"),
            Some("api_key")
        ),
        "sorted by pool id ahead of main"
    );
    assert_eq!(
        (
            members[1]["pool_id"].as_str(),
            members[1]["account_id"].as_str(),
            members[1]["budget_usd"].as_f64(),
            members[1]["budget_period"].as_str(),
            members[1]["kind"].as_str(),
        ),
        (
            Some("main"),
            Some("direct"),
            Some(20.0),
            Some("day"),
            Some("api_key")
        ),
        "a member with both budgets answers its day ahead of its month"
    );
    let member = &members[2];
    assert_eq!(
        (
            member["pool_id"].as_str(),
            member["account_id"].as_str(),
            member["budget_usd"].as_f64(),
            member["budget_period"].as_str(),
            member["kind"].as_str(),
        ),
        (
            Some("main"),
            Some("direct"),
            Some(50.0),
            Some("month"),
            Some("api_key")
        ),
        "a budgeted period names its pool, account, and budget"
    );
    assert_eq!(
        member["period_end"].as_i64(),
        Some(common::month_end()),
        "the period end is the calendar month's own boundary at the fixture's clock"
    );
}

#[tokio::test]
async fn budgets_route_refuses_once_the_router_is_gone() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    parts.router.shutdown().await.unwrap();
    let refused = get(&parts.app, "/admin/analytics/budgets", Some(&cookie)).await;
    assert_eq!(refused.status(), 503);
    assert_eq!(
        json(refused).await["error"].as_str(),
        Some("the router is not available")
    );
}

#[tokio::test]
async fn placements_route_answers_the_seeded_ledgers_rows_by_rung() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    post(
        &parts.app,
        "/admin/pools",
        r#"{"id":"main","label":"Main","strategy":"priority"}"#,
        Some(&cookie),
    )
    .await;
    post(
        &parts.app,
        "/admin/keys",
        r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
        Some(&cookie),
    )
    .await;
    post(
        &parts.app,
        "/admin/keys",
        r#"{"id":"other","label":"Other","pool_id":"main"}"#,
        Some(&cookie),
    )
    .await;
    let generation_of = |key: &str| common::generation(&parts.store, key);
    let plant = |id: &str,
                 key: &str,
                 at: i64,
                 rung: &str,
                 prefix_tokens: u64,
                 cache_read_tokens: u64| {
        Record::Request {
            request: RequestRecord {
                id: id.into(),
                received_at: at,
                finished_at: Some(at + 500),
                key_id: Some(key.into()),
                key_generation: generation_of(key),
                pool_id: Some("main".into()),
                canonical_model: "claude-sonnet-5".into(),
                model: "claude-sonnet-5".into(),
                kind: "inference".into(),
                origin: "client".into(),
                outcome: "succeeded".into(),
                attempts: 1,
                ..Default::default()
            },
            attempts: vec![AttemptRecord {
                request_id: id.into(),
                attempt: 1,
                account_id: "business".into(),
                account_kind: "oauth".into(),
                outcome: "succeeded".into(),
                cache_read_tokens: Some(cache_read_tokens),
                selection: Some(format!(
                    r#"{{"placement":{{"rung":"{rung}","depth":1,"prefix_tokens":{prefix_tokens}}}}}"#
                )),
                ..Default::default()
            }],
            bodies: None,
        }
    };
    // A hit and a miss on the same rung, so the ratio the route answers is not a coincidence of the seed:
    // inverting or fixing `hit_share` at either extreme would still pass a seed of one hit alone.
    parts.ledger.record(plant(
        "p-content",
        "desktop",
        common::NOW - 5_000,
        "content",
        500,
        500,
    ));
    parts.ledger.record(plant(
        "p-content-miss",
        "desktop",
        common::NOW - 4_500,
        "content",
        500,
        200,
    ));
    parts.ledger.record(plant(
        "p-static",
        "desktop",
        common::NOW - 4_000,
        "static",
        200,
        50,
    ));
    // Under a different key, so the `keys` filter has something of its own to exclude.
    parts.ledger.record(plant(
        "p-other-key",
        "other",
        common::NOW - 3_000,
        "with_main",
        0,
        10,
    ));
    for id in ["p-content", "p-content-miss", "p-static", "p-other-key"] {
        fake_upstream::wait_for_async(Duration::from_secs(5), || async {
            parts.db.request(id).await.unwrap()
        })
        .await;
    }
    let placed = json(
        get(
            &parts.app,
            "/admin/analytics/placements?range=24h",
            Some(&cookie),
        )
        .await,
    )
    .await;
    let row = |placed: &serde_json::Value, rung: &str| {
        placed["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["rung"] == rung)
            .cloned()
    };
    assert_eq!(
        (
            row(&placed, "content").as_ref().unwrap()["placements"].as_u64(),
            row(&placed, "content").as_ref().unwrap()["hits"].as_u64(),
            row(&placed, "content").as_ref().unwrap()["hit_share"].as_f64(),
            row(&placed, "content").as_ref().unwrap()["cache_read_tokens"].as_u64(),
            row(&placed, "content").as_ref().unwrap()["promised_tokens"].as_u64(),
        ),
        (Some(2), Some(1), Some(0.5), Some(700), Some(1_000)),
        "one hit among two placements reads the true ratio, not the extremes a single-row seed cannot tell apart"
    );
    assert_eq!(
        (
            row(&placed, "static").as_ref().unwrap()["placements"].as_u64(),
            row(&placed, "static").as_ref().unwrap()["hits"].as_u64(),
            row(&placed, "static").as_ref().unwrap()["hit_share"].as_f64(),
        ),
        (Some(1), Some(1), Some(1.0)),
        "a static placement is a hit on any read at all"
    );
    assert!(
        row(&placed, "with_main").is_some(),
        "the other key's placement is answered without a keys filter"
    );
    let filtered = json(
        get(
            &parts.app,
            "/admin/analytics/placements?range=24h&keys=desktop",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert!(
        row(&filtered, "with_main").is_none(),
        "a keys filter naming desktop excludes the other key's placement"
    );
    assert_eq!(
        row(&filtered, "content").as_ref().unwrap()["placements"].as_u64(),
        Some(2),
        "desktop's own rows still answer under the filter"
    );
}

/// One probe the harness's relay refused at admission with its own `503 ledger_behind`, sent by the harness's
/// keepalive engine: the pool `main` warming `direct`, the key `desktop`, and one client request that arms the recipe
/// while the reading is clear, the probe going out once the reading is behind. Answers when the ledger holds the
/// probe's row.
async fn one_held_probe() -> (common::Full, String) {
    use http_body_util::BodyExt;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tower::ServiceExt;
    let behind = std::sync::Arc::new(AtomicBool::new(false));
    let reading = behind.clone();
    let parts = common::admin_parts_full_with_accounting(std::sync::Arc::new(move |_| {
        phoebis_relay::Accounting {
            behind: reading.load(Ordering::SeqCst),
        }
    }))
    .await;
    let cookie = login(&parts.app, "admin-secret").await;
    post(&parts.app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"priority","members":[{"account_id":"direct","priority":0}],"keepalive":{"enabled":true}}"#, Some(&cookie)).await;
    parts.router.reload().await.unwrap();
    let created = json(
        post(
            &parts.app,
            "/admin/keys",
            r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
            Some(&cookie),
        )
        .await,
    )
    .await;
    let secret = created["secret"].as_str().unwrap().to_string();
    let generation = common::generation(&parts.store, "desktop");
    parts
        .keepalive
        .apply_event(
            "desktop",
            generation,
            phoebis_keepalive::Event {
                event: "session_start".into(),
                session_id: "s1".into(),
                instance_id: "i1".into(),
                sequence: 1,
                ..Default::default()
            },
        )
        .unwrap();
    phoebis_keepalive::spawn_observer(parts.keepalive.clone());
    parts.up.add_credential_rule(
        common::API_KEY,
        "/v1/messages",
        fake_upstream::Respond::Json {
            status: 200,
            headers: vec![],
            body: br#"{"id":"m","type":"message","content":[],"usage":{"input_tokens":10,"cache_creation_input_tokens":2000,"cache_creation":{"ephemeral_5m_input_tokens":2000,"ephemeral_1h_input_tokens":0},"output_tokens":10}}"#.to_vec(),
        },
    );
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("x-api-key", &secret)
        .header("content-type", "application/json")
        .header("x-claude-code-session-id", "s1")
        .header("x-claude-code-agent-id", "main")
        .body(axum::body::Body::from(
            r#"{"model":"claude-sonnet-4-5","max_tokens":1,"messages":[{"role":"user","content":"hi"}]}"#,
        ))
        .unwrap();
    let resp = phoebis_relay::router(parts.relay.clone())
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.into_body().collect().await.unwrap();
    let due = wait_for(Duration::from_secs(5), || {
        parts
            .keepalive
            .agent_status("desktop", "s1", "main")
            .and_then(|s| s.next_probe_at)
    })
    .await;
    behind.store(true, Ordering::SeqCst);
    parts.clock_cell.store(due + 1_000, Ordering::SeqCst);
    parts.keepalive.step().await;
    parts.keepalive.wait_idle().await;
    let status = parts
        .keepalive
        .agent_status("desktop", "s1", "main")
        .unwrap();
    assert_eq!(
        (status.probes, status.reason),
        (1, phoebis_keepalive::Reason::LedgerBehind),
        "the relay's admission refused the one probe"
    );
    wait_for(Duration::from_secs(5), || {
        parts
            .db
            .read_blocking(|c| {
                phoebis_ledger::read_rollups(
                    c,
                    0,
                    i64::MAX,
                    &phoebis_ledger::RollupSelect::default(),
                )
            })
            .unwrap()
            .iter()
            .any(|r| r.kind == "probe")
            .then_some(())
    })
    .await;
    (parts, cookie)
}

/// The router's own `503 ledger_behind` is no answer from the account, so the overview's day counts the probe under
/// `held` and leaves it out of `failed`.
#[tokio::test]
async fn the_overview_counts_a_probe_the_router_held_under_held_and_not_as_failed() {
    let (parts, cookie) = one_held_probe().await;
    let overview = json(get(&parts.app, "/admin/keepalive/overview", Some(&cookie)).await).await;
    let probes = &overview["probes_24h"];
    assert_eq!(
        (
            probes["held"].as_u64(),
            probes["failed"].as_u64(),
            probes["ok"].as_u64(),
            probes["migrated"].as_u64(),
            probes["skipped"].as_u64()
        ),
        (Some(1), Some(0), Some(0), Some(0), Some(0)),
        "{overview}"
    );
}

/// The analytics groups count the same held probe under `probes.held`, and with probes counted as requests its row
/// counts under no request, no failure and no rejection, on the breakdown and on the series alike.
#[tokio::test]
async fn an_analytics_group_counts_a_probe_the_router_held_under_held_and_not_as_failed_or_rejected(
) {
    let (parts, cookie) = one_held_probe().await;
    let breakdown = json(
        get(
            &parts.app,
            "/admin/analytics/breakdown?range=24h&group_by=key&include_probes=true",
            Some(&cookie),
        )
        .await,
    )
    .await;
    let row = breakdown["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["key"] == "desktop")
        .unwrap_or_else(|| panic!("no desktop group: {breakdown}"));
    let counts = |g: &serde_json::Value| {
        (
            g["probes"]["held"].as_u64(),
            g["probes"]["failed"].as_u64(),
            g["requests"].as_u64(),
            g["succeeded"].as_u64(),
            g["failed"].as_u64(),
            g["rejected"].as_u64(),
        )
    };
    assert_eq!(
        counts(row),
        (Some(1), Some(0), Some(1), Some(1), Some(0), Some(0)),
        "the client request alone, the held probe outside the requests: {row}"
    );
    let series = json(
        get(
            &parts.app,
            "/admin/analytics/series?range=24h&group_by=key&include_probes=true",
            Some(&cookie),
        )
        .await,
    )
    .await;
    let groups: Vec<&serde_json::Value> = series["points"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|p| p["groups"].as_array().unwrap())
        .filter(|g| g["key"] == "desktop")
        .collect();
    assert_eq!(groups.len(), 1, "one bucket holds both rows: {series}");
    assert_eq!(
        counts(groups[0]),
        (Some(1), Some(0), Some(1), Some(1), Some(0), Some(0)),
        "{series}"
    );
}

/// A pool of the one API-key member and the key that reaches it, so a selection through the harness's router places
/// on `direct` without a pairing.
async fn routed(parts: &common::Full, cookie: &str) {
    post(
        &parts.app,
        "/admin/pools",
        r#"{"id":"main","label":"Main","strategy":"priority","members":[{"account_id":"direct","priority":0}]}"#,
        Some(cookie),
    )
    .await;
    post(
        &parts.app,
        "/admin/keys",
        r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
        Some(cookie),
    )
    .await;
}

/// One selection through the harness's router as the relay asks for it, at `at`: no session, so no pin and no pin
/// event beside the decision.
async fn decide(parts: &common::Full, request_id: &str, seq: u32, attempt: u32, at: i64) {
    let snapshot = parts.router.snapshot();
    let key = snapshot
        .key("desktop")
        .expect("the key is in the snapshot")
        .clone();
    let candidates = phoebis_routing::resolve(
        &snapshot,
        &key,
        "claude-sonnet-5",
        "claude-sonnet-5",
    )
    .expect("the pool allows the model");
    let answer = parts
        .router
        .select(phoebis_routing::SelectRequest {
            request_id: request_id.into(),
            attempt,
            seq,
            key_id: "desktop".into(),
            key_generation: common::generation(&parts.store, "desktop"),
            agent_id: "main".into(),
            session_id: None,
            canonical_model: "claude-sonnet-5".into(),
            folded_model: "claude-sonnet-5".into(),
            candidates,
            portable: true,
            attempted: Vec::new(),
            now: at,
            received_at: at,
            fallback_ms: 60_000,
            revision: snapshot.revision,
            probe: false,
            count: false,
            request_class: None,
            context_compacted: false,
            static_hash: None,
            boundaries: std::sync::Arc::from([]),
            client: phoebis_routing::ClientClass::ClaudeCode,
            entrypoint: None,
            static_boundaries: std::sync::Arc::from([]),
            marker: false,
            parent_agent_id: None,
            failed_over: None,
            holds: Vec::new(),
            dialed: false,
            held_out: false,
            lane: phoebis_routing::lane_key("claude-sonnet-5", "standard", "none", "-"),
            fast: false,
            classifier: false,
            body_tokens: 0,
        })
        .await
        .unwrap();
    assert!(
        matches!(answer.result, phoebis_routing::SelectResult::Selected(_)),
        "the pool's member answers a selection"
    );
}

fn routing_event(at: i64, kind: &str) -> Record {
    Record::Event(EventRecord {
        at,
        kind: kind.into(),
        account_id: Some("direct".into()),
        detail: Some(r#"{"scope":"account"}"#.into()),
        ..Default::default()
    })
}

/// A request whose attempts answered on `direct`: each `(status, cache read)`, usage only where a read is named.
fn answered(id: &str, at: i64, attempts: &[(u16, Option<u64>)]) -> Record {
    Record::Request {
        request: RequestRecord {
            id: id.into(),
            received_at: at,
            finished_at: Some(at + 5_000),
            key_id: Some("desktop".into()),
            pool_id: Some("main".into()),
            model: "claude-sonnet-5".into(),
            canonical_model: "claude-sonnet-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            status: attempts.last().map(|(status, _)| *status),
            outcome: "succeeded".into(),
            attempts: attempts.len() as u32,
            ..Default::default()
        },
        attempts: attempts
            .iter()
            .zip(1..)
            .map(|((status, read), n)| AttemptRecord {
                request_id: id.into(),
                attempt: n,
                account_id: "direct".into(),
                account_kind: "api_key".into(),
                started_at: at,
                finished_at: Some(at + i64::from(n) * 1_000),
                upstream_status: Some(*status),
                outcome: if *status == 200 {
                    "succeeded"
                } else {
                    "rate_limited"
                }
                .into(),
                input_tokens: read.map(|_| 2_000),
                output_tokens: read.map(|_| 100),
                cache_read_tokens: *read,
                ..Default::default()
            })
            .collect(),
        bodies: None,
    }
}

const DECISION_ROW_KEYS: &[&str] = &[
    "type",
    "request_id",
    "seq",
    "attempt",
    "attempt_count",
    "at",
    "kind",
    "probe",
    "key_id",
    "pool_id",
    "session_id",
    "agent_id",
    "model",
    "account_id",
    "from_account_id",
    "by",
    "fact",
    "rule",
    "strategy",
    "tier",
    "rung",
    "score",
    "runner_up",
    "skipped",
    "warmth",
    "hold",
    "exhaustion",
    "refusal",
    "fast",
    "cache",
    "status",
];

const EVENT_ROW_KEYS: &[&str] = &[
    "type",
    "id",
    "at",
    "kind",
    "account_id",
    "key_id",
    "pool_id",
    "session_id",
    "agent_id",
    "model",
    "request_id",
    "actor",
    "detail",
];

fn keys_of(row: &serde_json::Value) -> Vec<&str> {
    row.as_object()
        .expect("a row is an object")
        .keys()
        .map(String::as_str)
        .collect()
}

#[tokio::test]
async fn the_routing_log_answers_the_merged_page_with_the_api_page_s_field_names() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    routed(&parts, &cookie).await;
    let now = common::NOW;
    decide(&parts, "r1", 1, 1, now + 1_000).await;
    decide(&parts, "r2", 1, 1, now + 3_000).await;
    parts
        .ledger
        .record(routing_event(now + 2_000, "cooldown_set"));
    parts
        .ledger
        .record(routing_event(now + 3_000, "cooldown_cleared"));
    // `from` as an RFC 3339 instant, `to` as Unix milliseconds: the route reads both. The page opens a millisecond
    // past `now`, the instant the store stamped the pool and the key with, so their registry rows stay off it.
    let page = wait_json(
        &parts.app,
        &format!(
            "/admin/routing-log?from=2026-10-11T12:16:40.001Z&to={}",
            now + 10_000
        ),
        &cookie,
        |v| v["rows"].as_array().is_some_and(|rows| rows.len() == 4),
    )
    .await;
    let rows = page["rows"].as_array().unwrap();
    assert_eq!(
        rows.iter()
            .map(|row| (row["type"].as_str().unwrap(), row["at"].as_i64().unwrap()))
            .collect::<Vec<_>>(),
        [
            ("decision", now + 3_000),
            ("event", now + 3_000),
            ("event", now + 2_000),
            ("decision", now + 1_000)
        ],
        "newest first, a decision before an event at one instant"
    );
    for row in rows {
        let expected = if row["type"] == "decision" {
            DECISION_ROW_KEYS
        } else {
            EVENT_ROW_KEYS
        };
        assert_eq!(keys_of(row), expected, "{row}");
    }
    assert_eq!(
        (rows[0]["request_id"].as_str(), rows[0]["kind"].as_str()),
        (Some("r2"), Some("placed"))
    );
    assert_eq!(rows[0]["account_id"], "direct");
    assert_eq!(rows[1]["detail"], serde_json::json!({ "scope": "account" }));
    assert!(page["next_cursor"].is_null());
    assert!(
        page.get("counts").is_none(),
        "counts are answered under a request filter alone"
    );
}

#[tokio::test]
async fn the_routing_log_pages_through_its_cursor() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    routed(&parts, &cookie).await;
    let now = common::NOW;
    decide(&parts, "r1", 1, 1, now + 1_000).await;
    decide(&parts, "r2", 1, 1, now + 4_000).await;
    parts
        .ledger
        .record(routing_event(now + 2_000, "cooldown_set"));
    parts
        .ledger
        .record(routing_event(now + 3_000, "cooldown_cleared"));
    // A millisecond past `now`, the instant the store stamped the pool and the key with, so their registry rows stay
    // off the pages.
    let range = format!("from={}&to={}", now + 1, now + 10_000);
    wait_json(
        &parts.app,
        &format!("/admin/routing-log?{range}"),
        &cookie,
        |v| v["rows"].as_array().is_some_and(|rows| rows.len() == 4),
    )
    .await;
    let first = json(
        get(
            &parts.app,
            &format!("/admin/routing-log?{range}&limit=2"),
            Some(&cookie),
        )
        .await,
    )
    .await;
    let ats = |page: &serde_json::Value| {
        page["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["at"].as_i64().unwrap())
            .collect::<Vec<_>>()
    };
    assert_eq!(ats(&first), [now + 4_000, now + 3_000]);
    let cursor = first["next_cursor"]
        .as_str()
        .expect("two rows lie past the first page");
    let second = json(
        get(
            &parts.app,
            &format!("/admin/routing-log?{range}&limit=2&cursor={cursor}"),
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(ats(&second), [now + 2_000, now + 1_000]);
    assert!(
        second["next_cursor"].is_null(),
        "the last page names no cursor"
    );
}

#[tokio::test]
async fn the_routing_log_refuses_an_unknown_kind_a_bad_time_a_bad_cursor_and_a_limit_out_of_range()
{
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    for (query, error) in [
        ("kinds=placed,bogus", "unknown kind bogus"),
        ("from=yesterday", "from is not a time"),
        ("to=2026-13-40T00:00:00Z", "to is not a time"),
        ("cursor=abc", "cursor is <at>:<rank>:<id>"),
        ("cursor=100:2:r1/1", "cursor is <at>:<rank>:<id>"),
        ("limit=0", "limit is 1 to 200"),
        ("limit=201", "limit is 1 to 200"),
        ("limit=many", "limit is 1 to 200"),
    ] {
        let refused = get(
            &parts.app,
            &format!("/admin/routing-log?{query}"),
            Some(&cookie),
        )
        .await;
        assert_eq!(refused.status(), 400, "{query}");
        assert_eq!(json(refused).await["error"], error, "{query}");
    }
    for query in ["limit=1", "limit=200", "kinds=probe,warm_prefix_expired"] {
        assert_eq!(
            get(
                &parts.app,
                &format!("/admin/routing-log?{query}"),
                Some(&cookie)
            )
            .await
            .status(),
            200,
            "{query}"
        );
    }
}

#[tokio::test]
async fn an_empty_kinds_reads_as_every_kind() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    routed(&parts, &cookie).await;
    let now = common::NOW;
    decide(&parts, "r1", 1, 1, now - 3_000).await;
    // A warm prefix expiry, the kind a narrower set would drop, so the row proves an absent and an emptied kinds read
    // one set, every kind.
    parts
        .ledger
        .record(routing_event(now - 2_000, "warm_prefix_expired"));
    parts
        .ledger
        .record(routing_event(now - 1_000, "cooldown_set"));
    // The window closes a millisecond before `now`, the instant the store stamped the pool and the key with, so
    // their registry rows cannot land between the two reads.
    let window = format!("from={}&to={}", now - 10_000, now - 1);
    let absent = wait_json(
        &parts.app,
        &format!("/admin/routing-log?{window}"),
        &cookie,
        |v| v["rows"].as_array().is_some_and(|rows| rows.len() == 3),
    )
    .await;
    let emptied = json(
        get(
            &parts.app,
            &format!("/admin/routing-log?{window}&kinds="),
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert_eq!(
        emptied["rows"], absent["rows"],
        "an empty kinds answers the page an absent one does"
    );
}

#[tokio::test]
async fn the_routing_log_refuses_a_filter_outside_its_keys() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let refused = get(&parts.app, "/admin/routing-log?statuses=429", Some(&cookie)).await;
    assert_eq!(refused.status(), 400);
    assert_eq!(
        json(refused).await["error"],
        "unknown filter statuses",
        "a filter the route does not read is refused rather than ignored"
    );
}

#[tokio::test]
async fn a_missing_record_answers_404() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let missing = get(
        &parts.app,
        "/admin/requests/r-none/decisions/1",
        Some(&cookie),
    )
    .await;
    assert_eq!(missing.status(), 404);
    assert_eq!(
        json(missing).await["error"],
        "no decision 1 for request r-none"
    );
}

#[tokio::test]
async fn a_decision_seq_that_is_no_sequence_number_answers_400() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    for seq in ["first", "-1", "1.5"] {
        let path = format!("/admin/requests/r-none/decisions/{seq}");
        let refused = get(&parts.app, &path, Some(&cookie)).await;
        assert_eq!(refused.status(), 400, "{path}");
        assert_eq!(
            json(refused).await["error"],
            "seq is the decision's sequence number",
            "{path}"
        );
    }
}

#[tokio::test]
async fn a_decision_record_answers_record_v2_with_its_completion() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    routed(&parts, &cookie).await;
    let now = common::NOW;
    decide(&parts, "r1", 1, 1, now).await;
    parts
        .ledger
        .record(answered("r1", now, &[(200, Some(700))]));
    let record = wait_json(&parts.app, "/admin/requests/r1/decisions/1", &cookie, |v| {
        v["outcome"]["completion"]["status"] == 200
    })
    .await;
    assert_eq!(
        keys_of(&record),
        [
            "request_id",
            "seq",
            "version",
            "request",
            "pin",
            "pin_reason",
            "settings",
            "candidates",
            "walk",
            "outcome"
        ]
    );
    assert_eq!(
        (&record["request_id"], &record["seq"], &record["version"]),
        (
            &serde_json::json!("r1"),
            &serde_json::json!(1),
            &serde_json::json!(2)
        )
    );
    assert_eq!(record["outcome"]["kind"], "placed");
    assert_eq!(
        record["outcome"]["completion"],
        serde_json::json!({
            "status": 200,
            "cache_read_tokens": 700,
            "input_tokens": 2_000,
            "promised_tokens": null,
            "hit": true,
            "held_ms": null,
            "freed": false,
            "cache_map_written": null,
            "completed_at": now + 1_000,
        })
    );
}

#[tokio::test]
async fn the_routing_log_s_decision_row_and_record_read_the_answering_attempt_after_a_retry_in_place(
) {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    routed(&parts, &cookie).await;
    let now = common::NOW;
    decide(&parts, "r1", 1, 1, now).await;
    // The same decision's account answered a 429 and, retried in place, a 200.
    parts
        .ledger
        .record(answered("r1", now, &[(429, None), (200, Some(900))]));
    let page = wait_json(
        &parts.app,
        &format!("/admin/routing-log?from={now}&to={now}&request=r1"),
        &cookie,
        |v| v["rows"][0]["status"] == 200,
    )
    .await;
    let row = &page["rows"][0];
    assert_eq!(
        (&row["attempt"], &row["attempt_count"], &row["status"]),
        (
            &serde_json::json!(1),
            &serde_json::json!(2),
            &serde_json::json!(200)
        ),
        "the row keeps its own attempt and reads the attempt that answered"
    );
    assert_eq!(
        row["cache"],
        serde_json::json!({ "hit": true, "read_tokens": 900, "promised_tokens": null, "input_tokens": 2_000 })
    );
    assert_eq!(
        page["counts"],
        serde_json::json!({ "decisions": 1, "events": 0 })
    );
    let record = json(get(&parts.app, "/admin/requests/r1/decisions/1", Some(&cookie)).await).await;
    let completion = &record["outcome"]["completion"];
    assert_eq!(
        (
            &completion["status"],
            &completion["cache_read_tokens"],
            &completion["completed_at"]
        ),
        (
            &serde_json::json!(200),
            &serde_json::json!(900),
            &serde_json::json!(now + 2_000)
        )
    );
}

/// A selection of `s1` through the harness's router as the relay asks for it, at `at`, with the accounts the request
/// already tried and the holds it waited through.
async fn select_s1(
    parts: &common::Full,
    request_id: &str,
    seq: u32,
    at: i64,
    attempted: &[&str],
    holds: Vec<phoebis_routing::HoldFacts>,
) -> phoebis_routing::SelectResult {
    let snapshot = parts.router.snapshot();
    let key = snapshot
        .key("desktop")
        .expect("the key is in the snapshot")
        .clone();
    let candidates =
        phoebis_routing::resolve(&snapshot, &key, "claude-sonnet-5", "claude-sonnet-5")
            .expect("the pool allows the model");
    parts
        .router
        .select(phoebis_routing::SelectRequest {
            request_id: request_id.into(),
            attempt: 1,
            seq,
            key_id: "desktop".into(),
            key_generation: common::generation(&parts.store, "desktop"),
            agent_id: "main".into(),
            session_id: Some("s1".into()),
            canonical_model: "claude-sonnet-5".into(),
            folded_model: "claude-sonnet-5".into(),
            candidates,
            portable: true,
            attempted: attempted.iter().map(|a| a.to_string()).collect(),
            now: at,
            received_at: at,
            fallback_ms: 60_000,
            revision: snapshot.revision,
            probe: false,
            count: false,
            request_class: None,
            context_compacted: false,
            static_hash: None,
            boundaries: std::sync::Arc::from([]),
            client: phoebis_routing::ClientClass::ClaudeCode,
            entrypoint: None,
            static_boundaries: std::sync::Arc::from([]),
            marker: false,
            parent_agent_id: None,
            failed_over: None,
            holds,
            dialed: false,
            held_out: false,
            lane: phoebis_routing::lane_key("claude-sonnet-5", "standard", "none", "-"),
            fast: false,
            classifier: false,
            body_tokens: 0,
        })
        .await
        .unwrap()
        .result
}

#[tokio::test]
async fn the_routing_log_reads_a_rewrite_hold_and_the_request_it_waited_for() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    // The harness's other account is unpaired, so the session's second landing needs a sibling of its own.
    parts
        .store
        .insert_account(&AccountRow {
            id: "sibling".into(),
            kind: AccountKind::ApiKey,
            label: "Sibling".into(),
            enabled: true,
            identity: None,
            lifecycle_generation: 1,
            incarnation: 1,
            revision: 1,
            state: AccountState::Ready,
            state_cause: StateCause::Operator,
        })
        .unwrap();
    parts
        .store
        .set_api_key_secret(
            "sibling",
            &phoebis_store::Secret::new("sk-ant-api03-SIBLING"),
        )
        .unwrap();
    post(
        &parts.app,
        "/admin/pools",
        r#"{"id":"main","label":"Main","strategy":"priority","slow_down_hold_seconds":120,"members":[{"account_id":"direct","priority":0},{"account_id":"sibling","priority":1}]}"#,
        Some(&cookie),
    )
    .await;
    post(
        &parts.app,
        "/admin/keys",
        r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
        Some(&cookie),
    )
    .await;
    let now = common::NOW;
    let placed = select_s1(&parts, "r0", 1, now - 1_000, &[], Vec::new()).await;
    assert!(
        matches!(&placed, phoebis_routing::SelectResult::Selected(sel) if sel.account.id == "direct"),
        "s1 pins direct: {placed:?}"
    );
    // The relay passed direct over for r1, so r1 lands the session on the sibling.
    let landed = select_s1(&parts, "r1", 1, now, &["direct"], Vec::new()).await;
    assert!(
        matches!(&landed, phoebis_routing::SelectResult::Selected(sel) if sel.left.as_deref() == Some("direct")),
        "r1 leaves direct: {landed:?}"
    );
    let hold = match select_s1(&parts, "r2", 1, now + 200, &[], Vec::new()).await {
        phoebis_routing::SelectResult::Held { hold, .. } => hold,
        other => panic!("the session's next request waits for r1's head, not {other:?}"),
    };
    let renewed = select_s1(
        &parts,
        "r2",
        2,
        now + 1_200,
        &[],
        vec![phoebis_routing::HoldFacts {
            held_ms: 1_000,
            ..hold
        }],
    )
    .await;
    assert!(
        matches!(&renewed, phoebis_routing::SelectResult::Selected(sel) if sel.trace.renewed),
        "{renewed:?}"
    );
    wait_json(
        &parts.app,
        &format!("/admin/routing-log?from={now}&to={}&request=r2", now + 200),
        &cookie,
        |v| v["rows"][0]["kind"] == "held",
    )
    .await;
    let held = json(get(&parts.app, "/admin/requests/r2/decisions/1", Some(&cookie)).await).await;
    let hold = &held["outcome"]["hold"];
    assert_eq!(
        (
            &hold["reason"],
            &hold["waits_for"],
            &hold["until"],
            &hold["bound_ms"]
        ),
        (
            &serde_json::json!("rewrite"),
            &serde_json::json!("r1"),
            &serde_json::json!(now + 120_000),
            &serde_json::json!(120_000)
        )
    );
    for figure in ["factor", "speed", "stretch_ms", "threshold_ms", "cycle_ms"] {
        assert!(hold[figure].is_null(), "{figure} is null: {hold}");
    }
    let renewal = wait_json(&parts.app, "/admin/requests/r2/decisions/2", &cookie, |v| {
        v["outcome"]["kind"] == "renewed"
    })
    .await;
    let waited = &renewal["request"]["holds"][0];
    assert_eq!(
        (&waited["reason"], &waited["waits_for"], &waited["held_ms"]),
        (
            &serde_json::json!("rewrite"),
            &serde_json::json!("r1"),
            &serde_json::json!(1_000)
        ),
        "the renewal names the wait it followed: {renewal}"
    );
    assert_eq!(renewal["outcome"]["hold"], serde_json::Value::Null);
}

#[tokio::test]
async fn the_request_s_events_carry_their_three_columns() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let now = common::NOW;
    parts
        .ledger
        .record(record("r1", now, "claude-sonnet-5", "s1"));
    parts.ledger.record(Record::Event(EventRecord {
        at: now + 100,
        kind: "window_spent".into(),
        account_id: Some("business".into()),
        pool_id: Some("main".into()),
        agent_id: Some("main".into()),
        request_id: Some("r1".into()),
        session_id: Some("s1".into()),
        detail: Some(r#"{"window":"five_hour","source_request_id":"r1"}"#.into()),
        ..Default::default()
    }));
    let shown = wait_json(&parts.app, "/admin/requests/r1", &cookie, |v| {
        v["events"].as_array().is_some_and(|e| !e.is_empty())
    })
    .await;
    let event = &shown["events"][0];
    assert_eq!(
        (&event["pool_id"], &event["agent_id"], &event["request_id"]),
        (
            &serde_json::json!("main"),
            &serde_json::json!("main"),
            &serde_json::json!("r1")
        )
    );
}

#[tokio::test]
async fn the_system_page_counts_the_refusals_the_router_s_mailbox_refused() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let system = json(get(&parts.app, "/admin/system", Some(&cookie)).await).await;
    assert_eq!(system["router"]["dropped_refusals"], 0);
}
