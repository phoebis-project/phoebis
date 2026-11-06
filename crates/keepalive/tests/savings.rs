//! The keepalive's ledger rows: a kept-warm request's credit, a probe's cost, a migrated probe's price, and the wasted
//! count settled when a recipe stops or the request after a probe returns cold.
mod common;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use common::*;
use fake_upstream::Respond;
use phoebis_bus::OTHER_CLIENT;
use phoebis_keepalive::{Event, KeepalivePolicy, Reason, RecipeState};
use phoebis_ledger::{hour_of, read_rollups, RollupRow, RollupSelect, UNATTRIBUTED};
use phoebis_routing::PinUsage;

/// The five counts summed.
fn tokens(usage: PinUsage) -> u64 {
    usage.input + usage.cache_read + usage.write_5m + usage.write_1h + usage.output
}

/// Every rollup row the rig wrote, once the writer has landed what the test waits for. The whole table
/// rather than the rig's own hour: a test that crosses into the next hour writes past it.
async fn rollups(r: &Rig, want: impl Fn(&[RollupRow]) -> bool) -> Vec<RollupRow> {
    wait_for(Duration::from_secs(5), || {
        let rows =
            r.db.read_blocking(|c| {
                read_rollups(c, hour_of(NOW) - 3_600, i64::MAX, &RollupSelect::default())
            })
            .unwrap();
        want(&rows).then_some(rows)
    })
    .await
}

fn probe_rows(rows: &[RollupRow]) -> RollupRow {
    let mut sum = RollupRow::key(
        0, "", 0, "", "", "", "probe", "", "", "", "", "", "", "", "", "",
    );
    for row in rows.iter().filter(|r| r.kind == "probe") {
        sum.add(row);
    }
    sum
}

fn inference_rows(rows: &[RollupRow]) -> RollupRow {
    let mut sum = RollupRow::key(
        0,
        "",
        0,
        "",
        "",
        "",
        "inference",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
    );
    for row in rows.iter().filter(|r| r.kind == "inference") {
        sum.add(row);
    }
    sum
}

// BODY asks for claude-sonnet-4-5, and the harness serves it under that id alone. A test that names a cost serves
// and requests claude-sonnet-5, which the table prices, so the cost is a number its assertions can name, and a test
// on a model the table does not price requests claude-opus-4-7.
const PRICED: &str = "claude-sonnet-5";
const UNPRICED: &str = "claude-opus-4-7";

fn priced_body() -> String {
    BODY.replace("claude-sonnet-4-5", PRICED)
}

fn unpriced_body() -> String {
    BODY.replace("claude-sonnet-4-5", UNPRICED)
}

/// The cells `model` prices at for an answer served at `speed` on a prompt under any threshold, in the table the
/// rig prices through.
fn rig_cells(model: &str, speed: phoebis_ledger::Speed) -> phoebis_ledger::ModelPrice {
    phoebis_ledger::PriceRevisions::shipped()
        .in_force(NOW)
        .price_at(model, speed, 0)
        .expect("the rig's table prices the model")
        .clone()
}

/// A real request answered with a five-minute write of 2,000 tokens (the cache was cold) or a read of 2,000
/// (the cache was warm); the standing rule answers reads.
fn cold_answer() -> Respond {
    Respond::Json { status: 200, headers: vec![], body: br#"{"id":"m","type":"message","content":[{"type":"text","text":"ok"}],"usage":{"input_tokens":10,"cache_creation_input_tokens":2000,"cache_creation":{"ephemeral_5m_input_tokens":2000,"ephemeral_1h_input_tokens":0},"output_tokens":10}}"#.to_vec() }
}

/// The request row, once the ledger landed it; a missing row is a failure, never a pass by absence.
async fn row(r: &Rig, id: &str) -> phoebis_ledger::RequestRecord {
    fake_upstream::wait_for_async(Duration::from_secs(5), || async {
        r.db.request(id).await.unwrap().map(|(row, _)| row)
    })
    .await
}

/// A probe's counts sit on its own request row, so one that migrated nothing adds no keepalive row: a row of zeros
/// under the probe's credit key would read as an empty group of its own.
#[tokio::test]
async fn a_probe_that_migrates_nothing_adds_no_keepalive_row_of_its_own() {
    let mut r = rig(RigSpec::default()).await;
    let body = priced_body();
    r.up.push_credential_rule(A, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 1);
    // The credit lands after the probe's own record, so once it shows, anything the probe recorded has landed too.
    r.advance(60_000);
    r.observe_with("main", &body).await;
    let rows = rollups(&r, |rows| {
        inference_rows(rows).kept_warm_requests == 1 && tokens(probe_rows(rows).usage()) > 0
    })
    .await;
    assert_eq!(probe_rows(&rows).probe_migrated, 0);
    let empty: Vec<&RollupRow> = rows.iter().filter(|row| row.holds_nothing()).collect();
    assert!(empty.is_empty(), "rows holding nothing: {empty:?}");
}

#[tokio::test]
async fn a_request_after_an_expiry_gap_with_a_covering_probe_is_kept_warm() {
    let mut r = rig(RigSpec::default()).await;
    let body = priced_body();
    r.up.push_credential_rule(A, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 1);
    // Past the five-minute TTL of the first request, a real request reads the cache the probe kept.
    r.advance(60_000);
    let second = r.observe_with("main", &body).await;
    let rows = rollups(&r, |rows| {
        inference_rows(rows).kept_warm_requests == 1 && tokens(probe_rows(rows).usage()) > 0
    })
    .await;
    let credit = inference_rows(&rows);
    assert_eq!(
        (
            credit.kept_warm_requests,
            credit.kept_warm_5m_tokens,
            credit.kept_warm_1h_tokens
        ),
        (1, 2_000, 0),
        "a five-minute recipe's credit books the five-minute count"
    );
    assert!(
        read_dollars(&r, &rows).0 > 0.0,
        "the probe's own request row prices its cost"
    );
    assert!(row(&r, &second).await.kept_warm);
    // The same gap without a probe credits nothing. A probe that read nothing and rewrote the prefix covers from its
    // own write, so the request that reads the entry it wrote is credited under it.
    let third = r.observe_with("main", &body).await;
    r.advance(330_000);
    let fourth = r.observe_with("main", &body).await;
    r.up.push_credential_rule(A, "/v1/messages", probe_answer(0, 2_000));
    r.advance(270_000);
    r.step().await;
    r.advance(60_000);
    let fifth = r.observe_with("main", &body).await;
    for id in [&third, &fourth] {
        assert!(
            !row(&r, id).await.kept_warm,
            "{id}: the probe-less gap credited nothing"
        );
    }
    // The credit lands in a batch of its own, which can follow the request row's, so the row is read once the credit
    // shows.
    rollups(&r, |rows| {
        inference_rows(rows).kept_warm_requests == 2 && probe_rows(rows).requests >= 2
    })
    .await;
    assert!(
        row(&r, &fifth).await.kept_warm,
        "the rewrite kept the entry the fifth request read"
    );
    // A model switch discards the recipe the probe warmed: after a covering probe on the first model, a request
    // on another model past the TTL is not credited, and a request back on the first model inside the second's
    // TTL is not either, since no probe covered it since the switch.
    let other = BODY.replace("claude-sonnet-4-5", "claude-opus-5");
    r.advance(270_000);
    r.step().await;
    r.advance(60_000);
    let sixth = r.observe_with("main", &other).await;
    r.advance(60_000);
    let seventh = r.observe_with("main", &body).await;
    assert!(
        !row(&r, &sixth).await.kept_warm && !row(&r, &seventh).await.kept_warm,
        "a switch of model earns no credit in either direction"
    );
    let rows = rollups(&r, |rows| probe_rows(rows).requests >= 3).await;
    assert_eq!(inference_rows(&rows).kept_warm_requests, 2);
}

#[tokio::test]
async fn a_request_under_the_dated_id_keeps_the_credit_of_a_probe_on_a_recipe_retained_under_the_spelling(
) {
    let dated = "claude-sonnet-4-5-20250929";
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    r.up.push_credential_rule(A, "/v1/messages", cold_answer());
    r.observe("main").await;
    assert_eq!(r.status("main").canonical_model, "claude-sonnet-4-5");
    // The recipe is retained before its model resolves: a listing that lands only now moves the spelling.
    let listing = relay_common::catalog_rules().remove(0);
    r.h.catalog_up
        .push_credential_rule(A, &listing.path_prefix, listing.respond);
    let a = r.h.store.account("a").unwrap().unwrap();
    r.h.catalog.refresh(&a).await;
    assert_eq!(
        r.h.catalog.lookup("claude-sonnet-4-5").as_deref(),
        Some(dated)
    );
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 1);
    // Past the first request's five-minute TTL, the next one is named by the dated id and reads what the probe
    // kept warm.
    r.advance(60_000);
    let second = r.observe("main").await;
    assert_eq!(r.status("main").canonical_model, dated);
    let rows = rollups(&r, |rows| inference_rows(rows).kept_warm_requests == 1).await;
    let credit = inference_rows(&rows);
    assert_eq!(
        (credit.kept_warm_5m_tokens, credit.kept_warm_1h_tokens),
        (2_000, 0),
        "the probe covered the recipe the request continues, whichever name each was taken under"
    );
    assert!(row(&r, &second).await.kept_warm);
}

/// A probe that landed under the spelling keeps its credit when the model resolves before the next request: the
/// scheduler's tick moves the unanswered probe to the dated id beside the recipe, so the request named by the dated
/// id reads the entry that probe wrote as its own lineage.
#[tokio::test]
async fn a_model_resolved_between_a_probe_and_the_next_request_keeps_the_warm_credit() {
    let dated = "claude-sonnet-4-5-20250929";
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    r.up.push_credential_rule(A, "/v1/messages", cold_answer());
    r.observe("main").await;
    r.advance(270_000);
    r.step().await;
    let status = r.status("main");
    assert_eq!(
        (status.probes, status.canonical_model.as_str()),
        (1, "claude-sonnet-4-5"),
        "the probe landed under the spelling"
    );
    let listing = relay_common::catalog_rules().remove(0);
    r.h.catalog_up
        .push_credential_rule(A, &listing.path_prefix, listing.respond);
    let a = r.h.store.account("a").unwrap().unwrap();
    r.h.catalog.refresh(&a).await;
    assert_eq!(
        r.h.catalog.lookup("claude-sonnet-4-5").as_deref(),
        Some(dated)
    );
    // A tick with nothing due still follows the resolution.
    r.step().await;
    assert_eq!(
        (
            r.status("main").probes,
            r.status("main").canonical_model.as_str()
        ),
        (1, dated)
    );
    // Past the first request's five-minute TTL and inside the probe's entry, which runs to 570 s.
    r.advance(60_000);
    let second = r.observe("main").await;
    let rows = final_rollups(&r).await;
    assert_eq!(
        (
            inference_rows(&rows).kept_warm_requests,
            probe_rows(&rows).wasted_probe_reasons
        ),
        (1, BTreeMap::new()),
        "the probe's lineage followed the resolution, so the request answered it and was credited"
    );
    assert!(row(&r, &second).await.kept_warm);
}

#[tokio::test]
async fn a_probe_during_a_long_stream_and_a_shortened_ttl_credit_nothing() {
    // A stream arms the schedule before it completes; a probe that runs while it is open must not credit it.
    let mut r = rig(RigSpec::default()).await;
    let body = priced_body();
    r.up.push_credential_rule(A, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    r.advance(270_000);
    r.step().await;
    assert!(r.status("main").probes == 1);
    r.advance(60_000);
    let events = vec![
        fake_upstream::SseEvent { event: "message_start".into(), data: r#"{"type":"message_start","message":{"id":"msg_s","usage":{"input_tokens":10,"cache_read_input_tokens":2000}}}"#.into() },
        fake_upstream::SseEvent { event: "message_delta".into(), data: r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#.into() },
        fake_upstream::SseEvent { event: "message_stop".into(), data: r#"{"type":"message_stop"}"#.into() },
    ];
    r.up.push_credential_rule(
        A,
        "/v1/messages",
        Respond::SseGated {
            status: 200,
            headers: vec![],
            events,
        },
    );
    // The fixture body already asks for a stream, so the gated rule answers this one event by event.
    let streamed = body.as_str();
    let resp =
        r.h.app
            .clone()
            .oneshot(r.request_with("main", streamed))
            .await
            .unwrap();
    assert_eq!(resp.status(), 200);
    let id = relay_common::outcome(&resp).request_id;
    let mut streaming = resp.into_body();
    r.pump_until(|r| r.status("main").next_probe_at.is_some())
        .await;
    // The stream is open, the schedule is armed from its first event; the probe due now runs against the open
    // request's own prefix.
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 2);
    r.up.release();
    while let Some(Ok(_)) = http_body_util::BodyExt::frame(&mut streaming).await {}
    r.pump(Some(&id)).await;
    let rows = rollups(&r, |rows| inference_rows(rows).kept_warm_requests == 1).await;
    let credit = inference_rows(&rows);
    assert_eq!(
        (credit.kept_warm_5m_tokens, credit.kept_warm_1h_tokens),
        (2_000, 0)
    );
    assert!(
        row(&r, &id).await.kept_warm,
        "the covering probe before the stream started credits it"
    );
    // A one-hour recipe with a covering probe, then a request inside that hour whose own evidence is a
    // five-minute write: the gap is measured against the hour the prior recipe had, so nothing is credited.
    let mut r = rig(RigSpec::default()).await;
    let hour = priced_body().replace(
        r#""cache_control":{"type":"ephemeral"}"#,
        r#""cache_control":{"type":"ephemeral","ttl":"1h"}"#,
    );
    r.up.push_credential_rule(A, "/v1/messages", Respond::Json { status: 200, headers: vec![], body: br#"{"id":"m","type":"message","content":[],"usage":{"input_tokens":10,"cache_creation_input_tokens":2000,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":2000},"output_tokens":10}}"#.to_vec() });
    r.observe_with("main", &hour).await;
    assert_eq!(r.status("main").cache_ttl_ms, 3_600_000);
    r.advance(3_600_000 - 360_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 1);
    r.advance(300_000);
    r.up.push_credential_rule(A, "/v1/messages", Respond::Json { status: 200, headers: vec![], body: br#"{"id":"m","type":"message","content":[],"usage":{"input_tokens":10,"cache_read_input_tokens":2000,"cache_creation_input_tokens":100,"cache_creation":{"ephemeral_5m_input_tokens":100,"ephemeral_1h_input_tokens":0},"output_tokens":10}}"#.to_vec() });
    let later = r.observe_with("main", &priced_body()).await;
    assert!(
        !row(&r, &later).await.kept_warm,
        "fifty-nine minutes is inside the hour the prior recipe was warm for"
    );
    assert_eq!(
        r.status("main").cache_ttl_ms,
        300_000,
        "this request's own evidence shortened the TTL after the check"
    );
}

/// An hour recipe's credit books the hour's count: past the hour the first request wrote for, a request inside the
/// entry the probe refreshed reads it warm under the hour its own markers ask for.
#[tokio::test]
async fn a_request_kept_warm_by_an_hour_recipes_probe_books_the_hours_count() {
    let mut r = rig(RigSpec::default()).await;
    let hour = priced_body().replace(
        r#""cache_control":{"type":"ephemeral"}"#,
        r#""cache_control":{"type":"ephemeral","ttl":"1h"}"#,
    );
    r.up.push_credential_rule(A, "/v1/messages", Respond::Json { status: 200, headers: vec![], body: br#"{"id":"m","type":"message","content":[],"usage":{"input_tokens":10,"cache_creation_input_tokens":2000,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":2000},"output_tokens":10}}"#.to_vec() });
    r.observe_with("main", &hour).await;
    assert_eq!(r.status("main").cache_ttl_ms, 3_600_000);
    r.advance(3_600_000 - 360_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 1);
    // Past the hour the first request's entry lived, and inside the hour the probe refreshed.
    r.advance(420_000);
    r.up.push_credential_rule(A, "/v1/messages", warm_answer());
    let kept = r.observe_with("main", &hour).await;
    let rows = rollups(&r, |rows| inference_rows(rows).kept_warm_requests == 1).await;
    let credit = inference_rows(&rows);
    assert_eq!(
        (credit.kept_warm_5m_tokens, credit.kept_warm_1h_tokens),
        (0, 2_000),
        "an hour recipe's credit books the hour's count alone"
    );
    assert!(row(&r, &kept).await.kept_warm);
}

#[tokio::test]
async fn a_pool_policy_dispatches_the_hour_a_five_minute_body_asked_for_and_the_engine_learns_it() {
    // The fixture body's own marker carries no ttl (five minutes); with the cap reading the recipe's own
    // received TTL, a one-hour write would have been capped back down to it. The cap now reads the TTL the
    // relay actually dispatched, so the engine learns the hour the pool rewrote the request to.
    let mut r = rig(RigSpec {
        pool: PoolSpec {
            // The account is api_key, so its writes take the metered set's own `main`, the rule this test is
            // about.
            cache_ttl: r#"{"enabled":true,"metered":{"main":"1h"}}"#,
            ..PoolSpec::default()
        },
        ..RigSpec::default()
    })
    .await;
    r.up.push_credential_rule(
        A,
        "/v1/messages",
        Respond::Json {
            status: 200,
            headers: vec![],
            body: br#"{"id":"m","type":"message","content":[],"usage":{"input_tokens":10,"cache_creation_input_tokens":2000,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":2000},"output_tokens":10}}"#.to_vec(),
        },
    );
    r.observe("main").await;
    assert_eq!(r.status("main").cache_ttl_ms, 3_600_000);
    r.advance(3_600_000 - 360_000);
    r.step().await;
    assert_eq!(
        r.status("main").probes,
        1,
        "the schedule armed off the dispatched hour, not the body's own five-minute marker"
    );
}

#[tokio::test]
async fn a_migrated_probe_is_priced_and_the_next_request_counts_as_kept_warm() {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0), api_key("b", B, 0)],
        ..RigSpec::default()
    })
    .await;
    let body = priced_body();
    // The two accounts rank equally, so which one serves the first request is not known until it has: the
    // standing rule answers it on either, and nothing is left queued in front of the probe's own answer.
    r.observe_with("main", &body).await;
    let first = r.status("main").account_id.clone().unwrap();
    let (mine, other) = if first == "a" { (A, B) } else { (B, A) };
    let other_id = if first == "a" { "b" } else { "a" };
    r.up.push_credential_rule(
        mine,
        "/v1/messages",
        relay_common::json_429_unified(4_600, "five_hour"),
    );
    r.up.push_credential_rule(other, "/v1/messages", probe_answer(0, 2_000));
    r.advance(270_000);
    r.step().await;
    let status = r.status("main");
    assert_eq!(
        (
            status.migrations,
            status.cold,
            status.account_id.as_deref(),
            status.unanswered
        ),
        (1, 0, Some(other_id), 1),
        "{status:?}"
    );
    let rows = rollups(&r, |rows| {
        probe_rows(rows).probe_migrated == 1 && tokens(probe_rows(rows).usage()) > 0
    })
    .await;
    assert!(
        read_dollars(&r, &rows).0 > 0.0,
        "a migrated probe is priced from its usage"
    );
    r.advance(60_000);
    let second = r.observe_with("main", &body).await;
    assert_eq!(
        r.status("main").account_id.as_deref(),
        Some(other_id),
        "the pin moved with the probe"
    );
    let rows = rollups(&r, |rows| inference_rows(rows).kept_warm_requests == 1).await;
    let credit = inference_rows(&rows);
    assert_eq!(
        (credit.kept_warm_5m_tokens, credit.kept_warm_1h_tokens),
        (2_000, 0)
    );
    assert!(
        row(&r, &second).await.kept_warm,
        "the write the probe paid on the new account is the one the request would have paid"
    );
}

/// A migrated probe's mark files under the funding and the speed its own answer was served at, as the probe's request
/// row does: `billed` on the api_key account it moved to whatever overage the answer reported, and `overage` on a
/// subscription account in overage whose answer reported it. A fast recipe's probe answered fast files under `fast`.
#[tokio::test]
async fn a_migrated_probes_mark_files_under_the_funding_and_the_speed_its_answer_was_served_at() {
    for (target, funding, speed) in [
        (api_key("b", B, 1), "billed", "standard"),
        (oauth("o", OAUTH_SECRET, 1), "overage", "standard"),
        (api_key("b", B, 1), "billed", "fast"),
    ] {
        let (target_id, secret) = (target.id, target.secret);
        let subscription = target.kind == phoebis_store::AccountKind::Oauth;
        let mut r = rig(RigSpec {
            accounts: vec![api_key("a", A, 0), target],
            pool: PoolSpec {
                overage_policy: phoebis_store::OveragePolicy::Allow,
                ..PoolSpec::default()
            },
            ..RigSpec::default()
        })
        .await;
        if subscription {
            r.enter_overage(target_id, r.now() + 10_000_000).await;
            settled(&r, |r| async move {
                r.h.router
                    .overage(target_id)
                    .await
                    .unwrap()
                    .is_some_and(|e| e.in_use)
            })
            .await;
        }
        let fast = speed == "fast";
        let body = if fast {
            priced_body().replacen('{', r#"{"speed":"fast","#, 1)
        } else {
            priced_body()
        };
        r.observe_with("main", &body).await;
        assert_eq!(r.status("main").account_id.as_deref(), Some("a"));
        r.up.push_credential_rule(
            A,
            "/v1/messages",
            relay_common::json_429_unified(4_600, "five_hour"),
        );
        let Respond::Json { body: written, .. } = probe_answer(0, 2_000) else {
            unreachable!("the standing probe answer is a JSON body")
        };
        let written = if fast {
            String::from_utf8(written)
                .unwrap()
                .replace(
                    r#""output_tokens":0}"#,
                    r#""output_tokens":0,"speed":"fast"}"#,
                )
                .into_bytes()
        } else {
            written
        };
        r.up.push_credential_rule(
            secret,
            "/v1/messages",
            Respond::Json {
                status: 200,
                headers: vec![(
                    "anthropic-ratelimit-unified-overage-in-use".into(),
                    "true".into(),
                )],
                body: written,
            },
        );
        r.advance(270_000);
        r.step().await;
        assert_eq!(
            (r.status("main").migrations, r.status("main").account_id),
            (1, Some(target_id.to_string())),
            "{target_id}: the probe migrated"
        );
        let rows = rollups(&r, |rows| rows.iter().any(|row| row.probe_migrated > 0)).await;
        let marks: Vec<(&str, &str, &str)> = rows
            .iter()
            .filter(|row| row.probe_migrated > 0)
            .map(|row| {
                (
                    row.account_id.as_str(),
                    row.funding.as_str(),
                    row.speed.as_str(),
                )
            })
            .collect();
        assert_eq!(
            marks,
            [(target_id, funding, speed)],
            "{target_id}: the mark files under its probe's funding and speed: {rows:?}"
        );
    }
}

#[tokio::test]
async fn unanswered_probes_are_settled_as_wasted_when_the_recipe_stops() {
    // The probes land at 270, 540 and 810 s and the window closes before the fourth at 1,080 s.
    let mut r = rig(RigSpec {
        policy: KeepalivePolicy {
            max_idle_seconds: 1_000,
            ..RigSpec::default().policy
        },
        ..RigSpec::default()
    })
    .await;
    let body = priced_body();
    r.up.push_credential_rule(A, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    r.state("main", "idle");
    for _ in 0..3 {
        r.advance(270_000);
        r.step().await;
    }
    assert_eq!(r.status("main").probes, 3);
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        (r.status("main").reason, r.status("main").unanswered),
        (Reason::IdleLimit, 3),
        "the ceiling keeps the three probes pending"
    );
    // The last probe's entry, written 810 s in, ends at 1,110 s. The lapse's settlements reach the ledger with the save
    // that drops the probes, which the production flusher runs on its own.
    r.advance(30_000);
    r.step().await;
    r.engine.flush_persisted().await;
    let rows = rollups(&r, |rows| probe_rows(rows).wasted_probes == 3).await;
    let probe = probe_rows(&rows);
    assert!(
        probe.wasted_usage() == probe.usage(),
        "every probe since the last real request"
    );
    assert_eq!(
        probe.wasted_probe_reasons,
        BTreeMap::from([("idle_limit".to_string(), 3)])
    );
    // A real request before the stop resets the count: two probes, a request, one probe, then a release.
    let mut r = rig(RigSpec::default()).await;
    r.up.push_credential_rule(A, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    for _ in 0..2 {
        r.advance_attended(270_000);
        r.step().await;
    }
    r.observe_with("main", &body).await;
    r.advance_attended(270_000);
    r.step().await;
    r.state("main", "released");
    let rows = rollups(&r, |rows| probe_rows(rows).wasted_probes == 1).await;
    assert_eq!(
        probe_rows(&rows).wasted_probe_reasons,
        BTreeMap::from([("released".to_string(), 1)])
    );
    // session_end and a replacement carry their own labels.
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    r.advance_attended(270_000);
    r.step().await;
    r.event(Event {
        event: "session_end".into(),
        ..Default::default()
    })
    .unwrap();
    let rows = rollups(&r, |rows| probe_rows(rows).wasted_probes == 1).await;
    assert_eq!(
        probe_rows(&rows)
            .wasted_probe_reasons
            .keys()
            .collect::<Vec<_>>(),
        ["session_ended"]
    );
    // A migrated probe nobody came back for is a wasted probe too, at its price.
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0), api_key("b", B, 0)],
        ..RigSpec::default()
    })
    .await;
    r.observe_with("main", &body).await;
    let first = r.status("main").account_id.clone().unwrap();
    let (mine, other) = if first == "a" { (A, B) } else { (B, A) };
    r.up.push_credential_rule(
        mine,
        "/v1/messages",
        relay_common::json_429_unified(4_600, "five_hour"),
    );
    r.up.push_credential_rule(other, "/v1/messages", probe_answer(0, 2_000));
    r.advance_attended(270_000);
    r.step().await;
    assert_eq!(r.status("main").migrations, 1);
    r.event(Event {
        event: "session_end".into(),
        ..Default::default()
    })
    .unwrap();
    let rows = rollups(&r, |rows| probe_rows(rows).wasted_probes == 1).await;
    let probe = probe_rows(&rows);
    assert_eq!(
        probe.wasted_probe_reasons.get("session_ended").copied(),
        Some(1)
    );
    assert!(
        tokens(probe.wasted_usage()) > 0 && probe.wasted_usage() == probe.usage(),
        "{probe:?}"
    );
}

/// A covering probe whose next request read no cache kept nothing that request used: the probe settles wasted
/// under `cold_return` when the request completes.
#[tokio::test]
async fn a_request_that_reads_no_cache_after_a_covering_probe_settles_the_probe_as_a_cold_return() {
    let mut r = rig(RigSpec::default()).await;
    let body = priced_body();
    r.up.push_credential_rule(A, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        (r.status("main").probes, r.status("main").unanswered),
        (1, 1)
    );
    // A request the router refused before any upstream read nothing either way, so it leaves the probe kept for the
    // request after it.
    r.engine.observe(r.observed("refused", "main", &body));
    r.engine
        .observe(r.complete("refused", phoebis_bus::Outcome::Rejected, None));
    assert_eq!(
        r.status("main").unanswered,
        1,
        "a refused request decides nothing"
    );
    r.advance(60_000);
    r.up.push_credential_rule(A, "/v1/messages", cold_answer());
    let returned = r.observe_with("main", &body).await;
    assert!(!row(&r, &returned).await.kept_warm);
    let rows = rollups(&r, |rows| probe_rows(rows).wasted_probes == 1).await;
    let probe = probe_rows(&rows);
    assert_eq!(
        probe.wasted_probe_reasons,
        BTreeMap::from([("cold_return".to_string(), 1)])
    );
    assert!(
        tokens(probe.wasted_usage()) > 0 && probe.wasted_usage() == probe.usage(),
        "{probe:?}"
    );
    assert_eq!(r.status("main").unanswered, 0);
}

/// The two halves of a request the tests below drive on the engine's side alone: its observation as sent by the
/// client the relay classed `class` (`OTHER_CLIENT` where a case names no client) and its selection on `a`, then the
/// first stream event, which arms the schedule from the request's start with the cache it read and wrote.
fn open_request_as(r: &Rig, id: &str, class: &str, read: u64, write: u64) {
    r.engine
        .observe(sent_by(r.observed(id, "main", BODY), class));
    r.engine.observe(r.selected(id, "a"));
    r.engine.observe(message_start(
        id,
        phoebis_bus::Usage {
            cache_read_tokens: Some(read),
            cache_write_5m_tokens: Some(write),
            ..Default::default()
        },
    ));
}

/// The first stream event of `id`'s first attempt, carrying the cache it read and wrote.
fn message_start(id: &str, usage: phoebis_bus::Usage) -> phoebis_bus::Event {
    phoebis_bus::Event::ResponseChunk(phoebis_bus::ResponseChunk {
        request_id: id.into(),
        attempt: 1,
        index: 0,
        body: Default::default(),
        phase: Some(phoebis_bus::Phase::MessageStart { usage }),
    })
}

fn completed(r: &Rig, id: &str, read: u64) -> phoebis_bus::Event {
    r.complete(
        id,
        phoebis_bus::Outcome::Succeeded,
        Some(phoebis_bus::Usage {
            cache_read_tokens: Some(read),
            cache_write_5m_tokens: Some(if read == 0 { 2_000 } else { 0 }),
            ..Default::default()
        }),
    )
}

/// A probe sent while a request streams pays for the request after it, so that request's own completion decides
/// nothing about it, whatever it read: the probe waits for the next request, or settles under the recipe's stop.
#[tokio::test]
async fn a_probe_sent_while_a_request_is_open_waits_for_the_request_after_it() {
    for read in [0, 2_000] {
        let mut r = rig(RigSpec {
            accounts: vec![api_key("a", A, 0)],
            ..RigSpec::default()
        })
        .await;
        open_request_as(&r, "first", OTHER_CLIENT, 0, 2_000);
        r.engine.observe(completed(&r, "first", 0));
        r.advance_attended(60_000);
        open_request_as(&r, "second", OTHER_CLIENT, read, 2_000 - read);
        r.advance_attended(270_000);
        r.step().await;
        assert_eq!(
            (r.status("main").probes, r.status("main").unanswered),
            (1, 1),
            "read {read}: the probe landed while the second request streamed"
        );
        r.engine.observe(completed(&r, "second", read));
        assert_eq!(
            r.status("main").unanswered,
            1,
            "read {read}: the second request's completion left the probe alone"
        );
        r.event(Event {
            event: "session_end".into(),
            ..Default::default()
        })
        .unwrap();
        let rows = rollups(&r, |rows| probe_rows(rows).wasted_probes > 0).await;
        assert_eq!(
            probe_rows(&rows).wasted_probe_reasons,
            BTreeMap::from([("session_ended".to_string(), 1)]),
            "read {read}"
        );
    }
}

/// `completed`'s failed counterpart: the upstream answered with a head after `ttfb_ms`, or never answered at all.
fn failed(r: &Rig, id: &str, ttfb_ms: Option<i64>) -> phoebis_bus::Event {
    let mut event = r.complete(
        id,
        phoebis_bus::Outcome::Failed,
        Some(phoebis_bus::Usage {
            cache_read_tokens: Some(0),
            ..Default::default()
        }),
    );
    if let phoebis_bus::Event::RequestComplete(complete) = &mut event {
        complete.ttfb_ms = ttfb_ms;
    }
    event
}

/// A failed request decides none of the probes it kept, past a head or not, since a failed turn is no evidence about
/// the cache. The probe waits for the next successful request, which reads its coverage inside its entry and is
/// credited, or for the lapse, which settles it `expired` at the entry's end at 570 s.
#[tokio::test]
async fn a_failed_request_decides_none_of_the_probes_it_kept_and_leaves_them_to_the_next_success_or_the_lapse(
) {
    for head in [Some(40), None] {
        for next in [true, false] {
            let mut r = rig(RigSpec {
                accounts: vec![api_key("a", A, 0)],
                ..RigSpec::default()
            })
            .await;
            open_request_as(&r, "first", OTHER_CLIENT, 0, 2_000);
            r.engine.observe(completed(&r, "first", 0));
            r.advance_attended(270_000);
            r.step().await;
            assert_eq!(r.status("main").unanswered, 1, "a covering probe");
            r.advance_attended(10_000);
            open_request_as(&r, "second", OTHER_CLIENT, 0, 2_000);
            r.engine.observe(failed(&r, "second", head));
            assert_eq!(
                r.status("main").unanswered,
                1,
                "head {head:?}: the failed request decided nothing"
            );
            let expected = if next {
                // Past the first request's five-minute TTL and inside the probe's entry.
                r.advance_attended(50_000);
                open_request_as(&r, "third", OTHER_CLIENT, 2_000, 0);
                r.engine.observe(completed(&r, "third", 2_000));
                (1, BTreeMap::new())
            } else {
                r.advance_attended(290_000);
                r.step().await;
                r.engine.flush_persisted().await;
                (0, BTreeMap::from([("expired".to_string(), 1)]))
            };
            let rows = final_rollups(&r).await;
            assert_eq!(
                (
                    inference_rows(&rows).kept_warm_requests,
                    probe_rows(&rows).wasted_probe_reasons
                ),
                expected,
                "head {head:?}, a successful request next: {next}"
            );
        }
    }
}

/// A canceled request between two probes restores the recipe, and the restored recipe's next probe reads the entry
/// back, so the first probe's chain runs on through it. The tick at the first probe's own entry's end at 570 s
/// settles nothing, and a warm turn at 600 s inside the second probe's entry is credited with neither probe wasted.
#[tokio::test]
async fn a_canceled_request_between_two_probes_leaves_the_first_to_the_chain_the_restored_recipe_extends(
) {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    open_request_as(&r, "first", OTHER_CLIENT, 0, 2_000);
    r.engine.observe(completed(&r, "first", 0));
    r.advance_attended(270_000);
    r.step().await;
    assert_eq!(r.status("main").unanswered, 1, "a covering probe");
    r.advance_attended(10_000);
    open_request_as(&r, "canceled", OTHER_CLIENT, 0, 2_000);
    r.engine
        .observe(r.complete("canceled", phoebis_bus::Outcome::Canceled, None));
    r.step().await;
    let status = r.status("main");
    assert_eq!(
        (status.next_probe_at, status.reason),
        (Some(NOW + 540_000), Reason::RequestCanceled),
        "the cancel restored the recipe on the first probe's entry"
    );
    r.advance_attended(260_000);
    r.step().await;
    let status = r.status("main");
    assert_eq!(
        (status.unanswered, status.next_probe_at, status.reason),
        (2, Some(NOW + 810_000), Reason::Scheduled),
        "the kept probe and the restored recipe's probe, which read the entry back 540 s in and kept it to 840 s"
    );
    r.advance_attended(30_000);
    r.step().await;
    r.engine.flush_persisted().await;
    r.advance_attended(30_000);
    open_request_as(&r, "warm", OTHER_CLIENT, 2_000, 0);
    r.engine.observe(completed(&r, "warm", 2_000));
    let rows = final_rollups(&r).await;
    assert_eq!(
        (
            inference_rows(&rows).kept_warm_requests,
            probe_rows(&rows).wasted_probe_reasons
        ),
        (1, BTreeMap::new()),
        "the tick at 570 s settled nothing and the warm turn answered both probes"
    );
}

/// A probe sent while a request streams reads the entry back past the end of the recipe the arrival set aside, and
/// the request fails before that end. The restored recipe lapses at 570 s while the probe's entry lives to 850 s, so
/// the lapse leaves both probes to their chain, and a warm turn at 600 s is credited with neither probe wasted. With
/// `catch_up_fails` the restored recipe's catch-up probe at 561 s answers a 5xx, whose first rung would land past the
/// entry's end, so the recipe pauses and its lapse drops the paused prompt.
async fn a_failed_request_between_two_probes_then_the_restored_recipes_lapse(catch_up_fails: bool) {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    open_request_as(&r, "first", OTHER_CLIENT, 0, 2_000);
    r.engine.observe(completed(&r, "first", 0));
    r.advance_attended(270_000);
    r.step().await;
    assert_eq!(
        r.status("main").unanswered,
        1,
        "a covering probe, its entry to 570 s"
    );
    r.advance_attended(10_000);
    // At 280 s the first recipe goes aside on its entry to 570 s, and this request's first stream event arms its own
    // recipe on an entry to 580 s, due at 550 s.
    open_request_as(&r, "second", OTHER_CLIENT, 0, 2_000);
    r.advance_attended(270_000);
    r.step().await;
    assert_eq!(
        (r.status("main").probes, r.status("main").unanswered),
        (2, 2),
        "the kept probe and the probe that landed at 550 s while the second request streamed, its entry to 850 s"
    );
    r.advance_attended(10_000);
    r.engine.observe(failed(&r, "second", Some(40)));
    assert_eq!(
        r.status("main").next_probe_at,
        Some(NOW + 540_000),
        "the failure at 560 s restored the first recipe on its entry to 570 s"
    );
    if catch_up_fails {
        r.up.push_credential_rule(
            A,
            "/v1/messages",
            Respond::Json {
                status: 503,
                headers: vec![],
                body: br#"{"type":"error","error":{"type":"api_error","message":"x"}}"#.to_vec(),
            },
        );
        r.advance_attended(1_000);
        r.step().await;
        assert!(
            r.status("main").paused,
            "the catch-up probe's 5xx paused the restored recipe"
        );
        r.advance_attended(9_000);
    } else {
        r.advance_attended(10_000);
    }
    r.step().await;
    r.engine.flush_persisted().await;
    r.advance_attended(30_000);
    open_request_as(&r, "warm", OTHER_CLIENT, 2_000, 0);
    r.engine.observe(completed(&r, "warm", 2_000));
    let rows = final_rollups(&r).await;
    assert_eq!(
        (
            inference_rows(&rows).kept_warm_requests,
            probe_rows(&rows).wasted_probe_reasons
        ),
        (1, BTreeMap::new()),
        "catch-up fails: {catch_up_fails}: the lapse at 570 s settled neither probe and the warm turn at 600 s \
         answered both"
    );
}

#[tokio::test]
async fn a_probe_sent_during_a_failed_request_outlives_the_restored_recipes_lapse() {
    a_failed_request_between_two_probes_then_the_restored_recipes_lapse(false).await;
}

#[tokio::test]
async fn a_probe_sent_during_a_failed_request_outlives_the_restored_paused_recipes_lapse() {
    a_failed_request_between_two_probes_then_the_restored_recipes_lapse(true).await;
}

/// The rig's body with `content` as its one message, so the upstream stub tells one turn's probe from another's.
fn body_saying(content: &str) -> String {
    BODY.replace(r#""content":"hello""#, &format!(r#""content":"{content}""#))
}

/// The one message a recorded request's body carries.
fn sent_content(request: &fake_upstream::RecordedRequest) -> String {
    let body: serde_json::Value = serde_json::from_slice(&request.body).expect("a JSON body");
    body["messages"][0]["content"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// The Sessions row of the rig's agent `agent`.
fn recipe_row(r: &Rig, agent: &str) -> phoebis_keepalive::RecipeView {
    r.engine
        .recipes()
        .into_iter()
        .find(|v| v.session_id == "session" && v.agent_id == agent)
        .unwrap_or_else(|| panic!("no row for {agent}"))
}

/// A real request that fails or is canceled leaves the recipe of the last answered request armed on its schedule:
/// the row reads `retained` under the request's own word, and the next probe replays the answered turn's body, never
/// the failed one's, though the failed turn streamed far enough to arm a recipe of its own.
#[tokio::test]
async fn a_failed_or_canceled_request_leaves_the_last_answered_recipe_armed_and_its_next_probe_replays_that_body(
) {
    for outcome in [phoebis_bus::Outcome::Failed, phoebis_bus::Outcome::Canceled] {
        let word = match outcome {
            phoebis_bus::Outcome::Canceled => Reason::RequestCanceled,
            _ => Reason::RequestFailed,
        };
        let mut r = rig(RigSpec {
            accounts: vec![api_key("a", A, 0)],
            ..RigSpec::default()
        })
        .await;
        r.observe_with("main", &body_saying("answered")).await;
        let schedule = r.status("main").next_probe_at;
        assert!(
            schedule.is_some(),
            "{outcome:?}: the answered turn armed the recipe"
        );
        r.advance(60_000);
        open_request_with(&r, "failing", &body_saying("failing"), 2_000, 0);
        r.engine.observe(r.complete("failing", outcome, None));
        let status = r.status("main");
        assert_eq!(
            (status.next_probe_at, status.paused, status.reason),
            (schedule, false, word),
            "{outcome:?}: the answered turn's schedule stands"
        );
        let row = recipe_row(&r, "main");
        assert_eq!(
            (row.state, row.cause),
            (RecipeState::Retained, word),
            "{outcome:?}"
        );
        r.advance(schedule.unwrap() - r.now());
        r.step().await;
        let sent = r.up.requests();
        assert_eq!(
            (sent.len(), r.status("main").probes),
            (2, 1),
            "{outcome:?}: the answered request and the probe on its schedule"
        );
        assert_eq!(
            sent_content(&sent[1]),
            "answered",
            "{outcome:?}: the probe replays the answered turn's body"
        );
    }
}

/// A rejected request stays a pause: the recipe it found leaves with it, the row reads `paused` and no probe goes.
#[tokio::test]
async fn a_rejected_request_pauses_and_leaves_no_recipe_armed() {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    r.observe("main").await;
    let schedule = r.status("main").next_probe_at.unwrap();
    r.advance(60_000);
    r.engine.observe(r.observed("rejected", "main", BODY));
    r.engine
        .observe(r.complete("rejected", phoebis_bus::Outcome::Rejected, None));
    let status = r.status("main");
    assert_eq!(
        (
            status.next_probe_at,
            status.reason,
            r.engine.stats().retained_bytes
        ),
        (None, Reason::RequestRejected, 0)
    );
    let row = recipe_row(&r, "main");
    assert_eq!(
        (row.state, row.cause),
        (RecipeState::Paused, Reason::RequestRejected)
    );
    r.advance(schedule - r.now());
    r.step().await;
    assert_eq!(r.up.requests().len(), 1, "no probe after the rejection");
}

/// A request open past the expiry of the last answered recipe's entry finds nothing left to arm when it fails: the
/// row reads `not_retained` under `request_failed`, and nothing stays in memory.
#[tokio::test]
async fn a_failure_whose_last_recipe_expired_while_it_was_open_leaves_nothing_armed() {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    r.observe("main").await;
    r.advance(60_000);
    r.engine.observe(r.observed("failing", "main", BODY));
    r.engine.observe(r.selected("failing", "a"));
    // Past the answered turn's five-minute entry, with the request still open.
    r.advance(300_000);
    r.engine.observe(failed(&r, "failing", None));
    let status = r.status("main");
    assert_eq!(
        (
            status.next_probe_at,
            status.reason,
            r.engine.stats().retained_bytes
        ),
        (None, Reason::RequestFailed, 0)
    );
    let row = recipe_row(&r, "main");
    assert_eq!(
        (row.state, row.cause),
        (RecipeState::NotRetained, Reason::RequestFailed)
    );
}

/// The arrival moves the agent to its request's model and the selection to its account, and the recipe a failure
/// leaves armed comes back under the model and the account it was armed under: an old body probed under the new pair
/// would read the wrong pin, cooldown and price. The probe reaches the first turn's account with its body and model.
#[tokio::test]
async fn a_failed_turn_on_another_model_and_account_leaves_the_first_turns_recipe_armed_under_its_own_pair(
) {
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    let first = r.status("main");
    assert_eq!(first.account_id.as_deref(), Some("a"));
    r.advance(10_000);
    r.engine.observe(r.observed("other", "main", &opus_body()));
    r.engine.observe(r.selected("other", "b"));
    let moved = r.status("main");
    assert_eq!(
        (moved.canonical_model.as_str(), moved.account_id.as_deref()),
        (OPUS, Some("b")),
        "the turn moved the agent to its own pair"
    );
    r.engine.observe(failed(&r, "other", None));
    let status = r.status("main");
    assert_eq!(
        (
            status.canonical_model.as_str(),
            status.account_id.as_deref(),
            status.cache_ttl_ms,
            status.next_probe_at
        ),
        (
            first.canonical_model.as_str(),
            Some("a"),
            first.cache_ttl_ms,
            first.next_probe_at
        ),
        "the failure restored the first turn's model, account, TTL and schedule"
    );
    r.advance(first.next_probe_at.unwrap() - r.now());
    r.step().await;
    let sent = r.up.requests();
    assert_eq!(sent.len(), 2, "the first turn and its probe");
    let probe: serde_json::Value = serde_json::from_slice(&sent[1].body).unwrap();
    assert_eq!(
        (sent[1].credential(), probe["model"].as_str()),
        (Some(A), Some(first.canonical_model.as_str())),
        "the probe went to the first turn's account with its body and model"
    );
    let status = r.status("main");
    assert_eq!(
        (
            status.probes,
            status.migrations,
            status.account_id.as_deref(),
            status.canonical_model.as_str(),
            status.reason
        ),
        (
            1,
            0,
            Some("a"),
            first.canonical_model.as_str(),
            Reason::Scheduled
        )
    );
}

/// The recipe a failed request leaves armed is swept as any armed recipe is: its request ended before the failed one
/// arrived, so no open request holds it. Past its entry's expiry with no probe answered it lapses under `expired`,
/// and at the memory bound a new body evicts it from an idle agent under `evicted`.
#[tokio::test]
async fn a_recipe_a_failed_request_left_armed_lapses_and_is_evicted_as_any_armed_one() {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    r.observe("main").await;
    let schedule = r.status("main").next_probe_at;
    r.advance(60_000);
    r.engine.observe(r.observed("failing", "main", BODY));
    r.engine.observe(r.selected("failing", "a"));
    r.engine.observe(failed(&r, "failing", None));
    assert_eq!(r.status("main").next_probe_at, schedule, "armed again");
    let sent = r.up.requests().len();
    // Past the entry's expiry at 300 s, with no tick between.
    r.advance(260_000);
    r.step().await;
    let status = r.status("main");
    assert_eq!(
        (
            status.reason,
            status.next_probe_at,
            r.engine.stats().retained_bytes
        ),
        (Reason::Expired, None, 0)
    );
    assert_eq!(r.up.requests().len(), sent, "no probe past the expiry");
    // Room for two recipes and a half: a third agent's body evicts the idle agent's restored recipe.
    let one = {
        let mut sized = rig(RigSpec::default()).await;
        sized.observe("main").await;
        sized.engine.stats().retained_bytes
    };
    let mut r = rig(RigSpec {
        limits: phoebis_keepalive::KeepaliveLimits {
            max_retained_bytes: one * 5 / 2,
            ..phoebis_keepalive::KeepaliveLimits::default()
        },
        ..RigSpec::default()
    })
    .await;
    for agent in ["worker", "third"] {
        r.event(Event {
            event: "agent_start".into(),
            agent_id: Some(agent.into()),
            ..Default::default()
        })
        .unwrap();
    }
    r.observe("main").await;
    r.advance_attended(10_000);
    r.engine.observe(r.observed("failing", "main", BODY));
    r.engine.observe(r.selected("failing", "a"));
    r.engine.observe(failed(&r, "failing", None));
    assert!(
        r.status("main").next_probe_at.is_some(),
        "the failure left main's recipe armed"
    );
    r.state("main", "idle");
    r.observe("worker").await;
    r.observe("third").await;
    assert_eq!(
        (r.status("main").reason, r.status("third").reason),
        (Reason::Evicted, Reason::Scheduled)
    );
    let mut kept = r.engine.recipes_for("desktop", "session");
    kept.sort();
    assert_eq!(kept, ["third", "worker"]);
}

/// The two halves of a turn of `agent`'s on `a`, observed and placed and its first stream event written, open for a
/// completion the case sends or withholds.
fn open_turn(r: &Rig, id: &str, agent: &str) {
    r.engine.observe(r.observed(id, agent, BODY));
    r.engine.observe(r.selected(id, "a"));
    r.engine.observe(message_start(
        id,
        phoebis_bus::Usage {
            cache_write_5m_tokens: Some(2_000),
            ..Default::default()
        },
    ));
}

/// The recipe an arrival puts aside stays in memory, so it counts against `max_retained_bytes` beside the agent's new
/// body, and where the new body does not fit the agent's own aside recipe leaves first. Under a bound of three and a
/// half recipes, two agents armed and then each sent a request left open: the first agent's aside body fits beside its
/// new one, the second's new body takes the room its own aside recipe leaves, and the first agent's failure restores
/// its recipe, the failed body alone leaving, while the second's failure arms nothing. The agent ids have one length,
/// so every recipe is one size.
#[tokio::test]
async fn an_aside_recipe_counts_against_the_memory_bound_and_yields_first_to_its_own_agents_new_body(
) {
    let one = {
        let sized = rig(RigSpec::default()).await;
        open_turn(&sized, "sized", "main");
        sized.engine.observe(completed(&sized, "sized", 0));
        sized.engine.stats().retained_bytes
    };
    let limit = one * 7 / 2;
    let mut r = rig(RigSpec {
        limits: phoebis_keepalive::KeepaliveLimits {
            max_retained_bytes: limit,
            ..phoebis_keepalive::KeepaliveLimits::default()
        },
        ..RigSpec::default()
    })
    .await;
    r.event(Event {
        event: "agent_start".into(),
        agent_id: Some("side".into()),
        ..Default::default()
    })
    .unwrap();
    let retained = |r: &Rig| {
        let bytes = r.engine.stats().retained_bytes;
        assert!(
            bytes <= limit,
            "{bytes} retained against a bound of {limit}"
        );
        bytes
    };
    for agent in ["main", "side"] {
        let id = format!("{agent}-answered");
        open_turn(&r, &id, agent);
        r.engine.observe(completed(&r, &id, 0));
    }
    let schedule = r.status("main").next_probe_at;
    assert_eq!(retained(&r), 2 * one, "two armed recipes");
    open_turn(&r, "main-open", "main");
    assert_eq!(
        retained(&r),
        3 * one,
        "main's aside body fits beside its new one"
    );
    open_turn(&r, "side-open", "side");
    assert_eq!(
        (retained(&r), r.status("side").reason),
        (3 * one, Reason::Scheduled),
        "side's new body took the room its own aside recipe left"
    );
    r.engine.observe(failed(&r, "main-open", None));
    assert_eq!(
        retained(&r),
        2 * one,
        "main's failure restored its recipe, the failed body alone leaving"
    );
    assert_eq!(
        (r.status("main").next_probe_at, r.status("main").reason),
        (schedule, Reason::RequestFailed)
    );
    r.engine.observe(failed(&r, "side-open", None));
    assert_eq!(retained(&r), one, "side's failure arms nothing");
    let row = recipe_row(&r, "side");
    assert_eq!(
        (row.state, row.cause),
        (RecipeState::NotRetained, Reason::RequestFailed)
    );
}

/// A probe sent while a request streams pays for the request after it, so that request failing past its head leaves
/// the probe on the books for the next request or the recipe's stop, as a bus gap does, rather than dropping it.
#[tokio::test]
async fn a_probe_sent_while_a_request_is_open_outlives_that_request_failing_past_its_head() {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    open_request_as(&r, "first", OTHER_CLIENT, 0, 2_000);
    r.engine.observe(completed(&r, "first", 0));
    r.advance_attended(60_000);
    open_request_as(&r, "second", OTHER_CLIENT, 0, 2_000);
    r.advance_attended(270_000);
    r.step().await;
    assert_eq!(
        (r.status("main").probes, r.status("main").unanswered),
        (1, 1),
        "the probe landed while the second request streamed"
    );
    r.engine.observe(failed(&r, "second", Some(40)));
    assert_eq!(
        r.status("main").unanswered,
        1,
        "the probe waits for the request after the failed one"
    );
    r.event(Event {
        event: "session_end".into(),
        ..Default::default()
    })
    .unwrap();
    let rows = rollups(&r, |rows| probe_rows(rows).wasted_probes > 0).await;
    assert_eq!(
        probe_rows(&rows).wasted_probe_reasons,
        BTreeMap::from([("session_ended".to_string(), 1)])
    );
    assert_eq!(
        rows.iter()
            .filter(|row| row.kind == "probe" && row.wasted_probes > 0)
            .map(|row| row.client.as_str())
            .collect::<Vec<_>>(),
        [UNATTRIBUTED],
        "a probe's settlement answers no request, so it files under the reserved client"
    );
}

/// `open_request_as` under `OTHER_CLIENT`, on a body of the test's choosing.
fn open_request_with(r: &Rig, id: &str, body: &str, read: u64, write: u64) {
    r.engine.observe(r.observed(id, "main", body));
    r.engine.observe(r.selected(id, "a"));
    r.engine.observe(message_start(
        id,
        phoebis_bus::Usage {
            cache_read_tokens: Some(read),
            cache_write_5m_tokens: Some(write),
            ..Default::default()
        },
    ));
}

/// The rig after the two requests, the covering probe, the pausing probe and the retry whose first stream event
/// carries `start`, for the two cases that read the credit it books.
async fn paused_retry(start: phoebis_bus::Usage) -> Rig {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    let hour = priced_body().replace(
        r#""cache_control":{"type":"ephemeral"}"#,
        r#""cache_control":{"type":"ephemeral","ttl":"1h"}"#,
    );
    open_request_with(&r, "first", &hour, 0, 2_000);
    r.engine.observe(completed(&r, "first", 0));
    assert_eq!(r.status("main").cache_ttl_ms, 3_600_000);
    r.advance_attended(3_600_000 - 360_000);
    r.step().await;
    assert_eq!(r.status("main").unanswered, 1, "a covering probe");
    // Past the hour the first request's entry lived, and inside the hour the probe refreshed.
    r.advance_attended(420_000);
    open_request_with(&r, "second", &hour, 2_000, 0);
    // The probe due while the second request streams reads half the prefix back, which pauses the agent.
    r.up.push_credential_rule(A, "/v1/messages", probe_answer(1_000, 1_000));
    r.advance_attended(3_600_000 - 360_000);
    r.step().await;
    assert_eq!(
        (r.status("main").probes, r.status("main").paused),
        (2, true),
        "the probe sent while the second request streamed paused the agent"
    );
    // A later attempt of the same request starts its observation's TTL over, and its usage arrives under the pause.
    let mut retry = r.selected("second", "a");
    if let phoebis_bus::Event::AccountSelected(selected) = &mut retry {
        selected.attempt = 2;
    }
    r.engine.observe(retry);
    let mut start = message_start("second", start);
    if let phoebis_bus::Event::ResponseChunk(chunk) = &mut start {
        chunk.attempt = 2;
    }
    r.engine.observe(start);
    assert!(r.status("main").paused, "the usage arrived under the pause");
    let mut done = completed(&r, "second", 2_000);
    if let phoebis_bus::Event::RequestComplete(complete) = &mut done {
        complete.attempt = 2;
    }
    r.engine.observe(done);
    r
}

/// A probe sent while a request streams can pause the agent on its answer, and a later attempt of that request then
/// reports its usage under the pause. The observation records the TTL that usage settled on all the same, so the
/// request's credit books an hour recipe's count under the hour, where a TTL of zero would file it under five minutes.
#[tokio::test]
async fn a_request_whose_usage_arrives_while_its_agent_is_paused_books_its_credit_at_the_ttl_it_settled_on(
) {
    let r = paused_retry(phoebis_bus::Usage {
        cache_read_tokens: Some(2_000),
        ..Default::default()
    })
    .await;
    let rows = final_rollups(&r).await;
    let credit = inference_rows(&rows);
    assert_eq!(
        (
            credit.kept_warm_requests,
            credit.kept_warm_5m_tokens,
            credit.kept_warm_1h_tokens
        ),
        (1, 0, 2_000),
        "the credit books the hour the request's usage settled on"
    );
}

/// Under the pause each phase of a request's usage can only shorten the TTL the observation records: a retry whose
/// first stream event writes five minutes of the prefix books its credit under five minutes, though its completion's
/// read alone would settle on the recipe's hour.
#[tokio::test]
async fn a_paused_requests_credit_books_at_the_shorter_ttl_an_earlier_phase_settled_on() {
    let r = paused_retry(phoebis_bus::Usage {
        cache_read_tokens: Some(2_000),
        cache_write_5m_tokens: Some(100),
        cache_write_1h_tokens: Some(0),
        ..Default::default()
    })
    .await;
    let rows = final_rollups(&r).await;
    let credit = inference_rows(&rows);
    assert_eq!(
        (
            credit.kept_warm_requests,
            credit.kept_warm_5m_tokens,
            credit.kept_warm_1h_tokens
        ),
        (1, 2_000, 0),
        "the credit books the five minutes the first stream event settled on, the shorter of the two phases"
    );
}

/// The probes a bus gap leaves keep the speed of the recipe that sent them: a fast recipe's warm return reads them
/// on the fast lane they kept, though the gap took the recipe before the next request answered them.
#[tokio::test]
async fn the_probes_a_bus_gap_leaves_keep_their_recipes_speed() {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    let fast = opus_body().replacen('{', r#"{"speed":"fast","#, 1);
    open_request_with(&r, "first", &fast, 0, 2_000);
    r.engine.observe(completed(&r, "first", 0));
    r.advance_attended(60_000);
    open_request_with(&r, "second", &fast, 2_000, 0);
    r.up.push_credential_rule(A, "/v1/messages", fast_read_back());
    r.advance_attended(270_000);
    r.step().await;
    assert_eq!(
        (r.status("main").probes, r.status("main").unanswered),
        (1, 1),
        "the probe landed while the second request streamed"
    );
    r.engine.bus_gap(1, 0);
    r.advance_attended(10_000);
    open_request_with(&r, "third", &fast, 2_000, 0);
    r.engine.observe(completed(&r, "third", 2_000));
    assert_eq!(
        r.status("main").unanswered,
        0,
        "the warm return on the fast lane answered the probe"
    );
    r.event(Event {
        event: "session_end".into(),
        ..Default::default()
    })
    .unwrap();
    r.ledger.clone().shutdown().await.unwrap();
    let rows =
        r.db.read_blocking(|c| {
            read_rollups(c, hour_of(NOW) - 3_600, i64::MAX, &RollupSelect::default())
        })
        .unwrap();
    assert_eq!(
        probe_rows(&rows).wasted_probe_reasons,
        BTreeMap::new(),
        "nothing settled as a cold return"
    );
}

/// An entry that expires while the request it serves is still open is not lapsed under that request: the completion
/// earns its credit and decides the probes it kept, and the next tick lapses the recipe.
#[tokio::test]
async fn a_request_open_past_its_entry_keeps_its_credit_and_the_recipe_lapses_after_it() {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    open_request_as(&r, "first", OTHER_CLIENT, 0, 2_000);
    r.engine.observe(completed(&r, "first", 0));
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").unanswered, 1, "a covering probe");
    r.advance(60_000);
    open_request_as(&r, "second", OTHER_CLIENT, 2_000, 0);
    // The second request's entry expires 300 s after it started, with the request still open.
    r.advance(300_000);
    r.step().await;
    r.engine.observe(completed(&r, "second", 2_000));
    r.step().await;
    assert_eq!(r.status("main").reason, Reason::Expired);
    r.ledger.clone().shutdown().await.unwrap();
    let rows =
        r.db.read_blocking(|c| {
            read_rollups(c, hour_of(NOW) - 3_600, i64::MAX, &RollupSelect::default())
        })
        .unwrap();
    assert_eq!(
        (
            inference_rows(&rows).kept_warm_requests,
            probe_rows(&rows).wasted_probe_reasons
        ),
        (1, BTreeMap::new()),
        "the request earned its credit and the probe it kept was answered"
    );
}

/// A return inside the TTL that reads the cache earns no credit, the gap being too short to need the probe, but it
/// answers the probe all the same: nothing is settled wasted.
#[tokio::test]
async fn a_warm_return_without_the_credit_answers_the_probes_it_kept() {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    open_request_as(&r, "first", OTHER_CLIENT, 0, 2_000);
    r.engine.observe(completed(&r, "first", 0));
    r.advance_attended(270_000);
    r.step().await;
    assert_eq!(r.status("main").unanswered, 1);
    r.advance_attended(10_000);
    open_request_as(&r, "second", OTHER_CLIENT, 2_000, 0);
    r.engine.observe(completed(&r, "second", 2_000));
    assert_eq!(r.status("main").unanswered, 0);
    r.event(Event {
        event: "session_end".into(),
        ..Default::default()
    })
    .unwrap();
    r.ledger.clone().shutdown().await.unwrap();
    let rows =
        r.db.read_blocking(|c| {
            read_rollups(c, hour_of(NOW) - 3_600, i64::MAX, &RollupSelect::default())
        })
        .unwrap();
    assert_eq!(
        (
            inference_rows(&rows).kept_warm_requests,
            probe_rows(&rows).wasted_probes
        ),
        (0, 0)
    );
}

/// Probes kept on one model answer only to a return on that model. A request on another model that failed before
/// any head leaves the first model's recipe armed and the probes kept. The next request on the other model reads that
/// model's own entry inside its TTL, which reads nothing the probes paid for, so they settle as a cold return and
/// nothing is credited.
#[tokio::test]
async fn a_cache_read_on_another_model_settles_the_probes_kept_on_the_first_as_a_cold_return() {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    open_request_as(&r, "first", OTHER_CLIENT, 0, 2_000);
    r.engine.observe(completed(&r, "first", 0));
    r.advance_attended(270_000);
    r.step().await;
    assert_eq!(r.status("main").unanswered, 1, "a covering probe");
    r.advance_attended(10_000);
    let other = opus_body();
    r.engine.observe(r.observed("second", "main", &other));
    r.engine.observe(r.selected("second", "a"));
    r.engine.observe(failed(&r, "second", None));
    let status = r.status("main");
    assert_eq!(
        (status.canonical_model.as_str(), status.unanswered),
        ("claude-sonnet-4-5", 1),
        "the failed request restored the first turn's recipe and decided nothing"
    );
    r.advance_attended(10_000);
    open_request_with(&r, "third", &other, 2_000, 0);
    r.engine.observe(completed(&r, "third", 2_000));
    assert_eq!(r.status("main").unanswered, 0);
    r.ledger.clone().shutdown().await.unwrap();
    let rows =
        r.db.read_blocking(|c| {
            read_rollups(c, hour_of(NOW) - 3_600, i64::MAX, &RollupSelect::default())
        })
        .unwrap();
    let probe = probe_rows(&rows);
    assert_eq!(
        (
            inference_rows(&rows).kept_warm_requests,
            probe.wasted_probes,
            probe.wasted_probe_reasons.clone()
        ),
        (0, 1, BTreeMap::from([("cold_return".to_string(), 1)])),
        "the read on the other model answered nothing the first model's probe paid for"
    );
    assert!(
        tokens(probe.wasted_usage()) > 0 && probe.wasted_usage() == probe.usage(),
        "{probe:?}"
    );
}

/// The probe rows of one speed, summed.
fn probe_rows_at(rows: &[RollupRow], fast: bool) -> RollupRow {
    let mut sum = RollupRow::key(
        0, "", 0, "", "", "", "probe", "", "", "", "", "", "", "", "", "",
    );
    for row in rows
        .iter()
        .filter(|r| r.kind == "probe" && (r.speed == "fast") == fast)
    {
        sum.add(row);
    }
    sum
}

/// A credit answers only the probe whose lineage it read. A fast probe, kept by a standard request that failed before
/// any head, waits beside a standard probe a bus gap left. The standard request that earns the credit arrives 600 s
/// in and reads the standard lane, after the fast probe's entry lapsed 570 s in, so the fast probe settles by its own
/// entry as expired, at its price on the fast lane's row.
#[tokio::test]
async fn a_credit_on_the_standard_lane_leaves_the_fast_probe_kept_beside_it_to_settle_by_its_own_entry(
) {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    let standard = opus_body();
    let fast = standard.replacen('{', r#"{"speed":"fast","#, 1);
    open_request_with(&r, "first", &fast, 0, 2_000);
    r.engine.observe(completed(&r, "first", 0));
    r.up.push_credential_rule(A, "/v1/messages", fast_read_back());
    r.advance_attended(270_000);
    r.step().await;
    assert_eq!(
        (r.status("main").probes, r.status("main").unanswered),
        (1, 1),
        "a covering fast probe"
    );
    r.advance_attended(10_000);
    r.engine.observe(r.observed("second", "main", &standard));
    r.engine.observe(r.selected("second", "a"));
    r.engine.observe(failed(&r, "second", None));
    assert_eq!(r.status("main").unanswered, 1, "the fast probe stays kept");
    r.advance_attended(10_000);
    open_request_with(&r, "third", &standard, 0, 2_000);
    r.advance_attended(270_000);
    r.step().await;
    assert_eq!(
        (r.status("main").probes, r.status("main").unanswered),
        (2, 2),
        "a covering standard probe landed while the third request streamed"
    );
    r.engine.bus_gap(1, 0);
    r.advance_attended(40_000);
    open_request_with(&r, "fourth", &standard, 2_000, 0);
    r.engine.observe(completed(&r, "fourth", 2_000));
    assert_eq!(r.status("main").unanswered, 0);
    r.ledger.clone().shutdown().await.unwrap();
    let rows =
        r.db.read_blocking(|c| {
            read_rollups(c, hour_of(NOW) - 3_600, i64::MAX, &RollupSelect::default())
        })
        .unwrap();
    let (fast_probes, standard_probes) = (probe_rows_at(&rows, true), probe_rows_at(&rows, false));
    assert_eq!(
        (
            inference_rows(&rows).kept_warm_requests,
            fast_probes.wasted_probe_reasons.clone(),
            standard_probes.wasted_probes
        ),
        (1, BTreeMap::from([("expired".to_string(), 1)]), 0),
        "the credit answered the standard probe and nothing on the fast lane"
    );
    assert!(
        tokens(fast_probes.wasted_usage()) > 0 && fast_probes.wasted_usage() == fast_probes.usage(),
        "{fast_probes:?}"
    );
}

/// The rollups of the lineage trace above when the fourth request arrives `offset` ms after the standard probe
/// refreshed its five-minute entry 560 s in.
async fn fourth_request_rows(offset: i64) -> Vec<RollupRow> {
    let arrived = 560_000 + offset;
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    let standard = opus_body();
    let fast = standard.replacen('{', r#"{"speed":"fast","#, 1);
    open_request_with(&r, "first", &fast, 0, 2_000);
    r.engine.observe(completed(&r, "first", 0));
    r.up.push_credential_rule(A, "/v1/messages", fast_read_back());
    r.advance_attended(270_000);
    r.step().await;
    assert_eq!(
        (r.status("main").probes, r.status("main").unanswered),
        (1, 1),
        "arrived {arrived} ms in: a covering fast probe"
    );
    r.advance_attended(10_000);
    r.engine.observe(r.observed("second", "main", &standard));
    r.engine.observe(r.selected("second", "a"));
    r.engine.observe(failed(&r, "second", None));
    r.advance_attended(10_000);
    open_request_with(&r, "third", &standard, 0, 2_000);
    r.advance_attended(270_000);
    r.step().await;
    assert_eq!(
        (r.status("main").probes, r.status("main").unanswered),
        (2, 2),
        "arrived {arrived} ms in: a covering standard probe landed while the third request streamed"
    );
    r.engine.bus_gap(1, 0);
    // The standard probe refreshed its five-minute entry 560 s in, so it lapses 860 s in.
    r.advance_attended(offset);
    open_request_with(&r, "fourth", &standard, 2_000, 0);
    r.engine.observe(completed(&r, "fourth", 2_000));
    assert_eq!(r.status("main").unanswered, 0, "arrived {arrived} ms in");
    final_rollups(&r).await
}

/// A kept-warm credit needs the entry the covering probe kept to be there when the credited request arrives, on the
/// instant the settlement and the scheduler read as the entry gone. On the lineage trace above, the standard probe
/// refreshes its five-minute entry 560 s in and nothing is admitted until the fourth request, which arrives one
/// millisecond before that entry lapses or as it lapses: only the first is credited, and at the lapse the standard
/// probe settles as expired. The fast probe's entry lapsed 570 s in, before either, so it settles as expired in both.
#[tokio::test]
async fn a_kept_warm_credit_needs_the_covering_entry_alive_when_the_request_arrives() {
    let expired = BTreeMap::from([("expired".to_string(), 1)]);
    let rows = fourth_request_rows(299_999).await;
    let (fast_probes, standard_probes) = (probe_rows_at(&rows, true), probe_rows_at(&rows, false));
    assert_eq!(
        (
            inference_rows(&rows).kept_warm_requests,
            fast_probes.wasted_probe_reasons.clone(),
            (
                standard_probes.wasted_probes,
                standard_probes.wasted_probe_reasons.clone()
            ),
        ),
        (1, expired.clone(), (0, BTreeMap::new())),
        "arrived 859999 ms in"
    );
    assert!(
        tokens(fast_probes.wasted_usage()) > 0 && fast_probes.wasted_usage() == fast_probes.usage(),
        "arrived 859999 ms in: {fast_probes:?}"
    );
    let rows = fourth_request_rows(300_000).await;
    let (fast_probes, standard_probes) = (probe_rows_at(&rows, true), probe_rows_at(&rows, false));
    assert_eq!(
        (
            inference_rows(&rows).kept_warm_requests,
            fast_probes.wasted_probe_reasons.clone(),
            (
                standard_probes.wasted_probes,
                standard_probes.wasted_probe_reasons.clone()
            ),
        ),
        (0, expired.clone(), (1, expired)),
        "arrived 860000 ms in"
    );
    assert!(
        tokens(fast_probes.wasted_usage()) > 0 && fast_probes.wasted_usage() == fast_probes.usage(),
        "arrived 860000 ms in: {fast_probes:?}"
    );
    assert!(
        tokens(standard_probes.wasted_usage()) > 0
            && standard_probes.wasted_usage() == standard_probes.usage(),
        "arrived 860000 ms in: {standard_probes:?}"
    );
}

/// The lane check refuses a credit on its own. A fast recipe on a one-hour write is covered by a fast probe, and a
/// standard request on the same model and account arrives past the hour since the last real request, while the
/// entry the probe refreshed is still there, and reads the cache: the fast probe kept nothing on the standard lane,
/// so nothing is credited and the probe settles as a cold return.
#[tokio::test]
async fn a_fast_probe_inside_its_entry_earns_no_credit_on_the_standard_lane() {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    let standard = opus_body().replace(
        r#""cache_control":{"type":"ephemeral"}"#,
        r#""cache_control":{"type":"ephemeral","ttl":"1h"}"#,
    );
    let fast = standard.replacen('{', r#"{"speed":"fast","#, 1);
    r.engine.observe(r.observed("first", "main", &fast));
    r.engine.observe(r.selected("first", "a"));
    r.engine.observe(message_start(
        "first",
        phoebis_bus::Usage {
            cache_read_tokens: Some(0),
            cache_write_1h_tokens: Some(2_000),
            ..Default::default()
        },
    ));
    r.engine.observe(completed(&r, "first", 0));
    assert_eq!(r.status("main").cache_ttl_ms, 3_600_000, "a one-hour entry");
    r.up.push_credential_rule(A, "/v1/messages", fast_read_back());
    r.advance_attended(3_600_000 - 360_000);
    r.step().await;
    assert_eq!(
        (r.status("main").probes, r.status("main").unanswered),
        (1, 1),
        "a covering fast probe"
    );
    // The probe refreshed its one-hour entry 3,240 s in, so it is there until 6,840 s in; the request arrives
    // 3,640 s in, past the hour since the first.
    r.advance_attended(400_000);
    open_request_with(&r, "second", &standard, 2_000, 0);
    r.engine.observe(completed(&r, "second", 2_000));
    assert_eq!(r.status("main").unanswered, 0);
    r.ledger.clone().shutdown().await.unwrap();
    let rows =
        r.db.read_blocking(|c| {
            read_rollups(c, hour_of(NOW) - 3_600, i64::MAX, &RollupSelect::default())
        })
        .unwrap();
    assert_eq!(
        (
            inference_rows(&rows).kept_warm_requests,
            probe_rows(&rows).wasted_probe_reasons
        ),
        (0, BTreeMap::from([("cold_return".to_string(), 1)])),
        "the standard request read nothing the fast probe kept"
    );
}

/// An api_key account's keepalive rows file beside its requests, under `billed`: the credit of a request its probe kept
/// warm, the settlement of the probes nobody came back for after the idle window closed, and the skip of a due probe
/// the entry's lapse withheld, each filed by the kind the probe or the request ran under.
#[tokio::test]
async fn an_api_key_account_s_credit_settlement_and_skip_rows_file_under_billed() {
    let body = priced_body();
    let fundings = |rows: &[RollupRow], pick: fn(&RollupRow) -> bool| {
        rows.iter()
            .filter(|row| pick(row))
            .map(|row| row.funding.clone())
            .collect::<BTreeSet<_>>()
    };
    let billed = BTreeSet::from(["billed".to_string()]);
    // A probe at 270 s, and a real request past the TTL that reads what it kept.
    let mut r = rig(RigSpec::default()).await;
    r.up.push_credential_rule(A, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    r.advance(270_000);
    r.step().await;
    r.advance(60_000);
    r.observe_with("main", &body).await;
    let rows = rollups(&r, |rows| inference_rows(rows).kept_warm_requests == 1).await;
    assert_eq!(
        fundings(&rows, |row| row.kept_warm_requests > 0),
        billed,
        "the kept-warm credit: {rows:?}"
    );
    // Three probes nobody comes back for, pending when the idle window closes before the fourth and settled at the
    // last one's lapse.
    let mut r = rig(RigSpec {
        policy: KeepalivePolicy {
            max_idle_seconds: 1_000,
            ..RigSpec::default().policy
        },
        ..RigSpec::default()
    })
    .await;
    r.up.push_credential_rule(A, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    r.state("main", "idle");
    for _ in 0..4 {
        r.advance(270_000);
        r.step().await;
    }
    assert_eq!(
        (r.status("main").reason, r.status("main").unanswered),
        (Reason::IdleLimit, 3),
        "the ceiling keeps the three probes pending"
    );
    // The last probe's entry, written 810 s in, ends at 1,110 s. The lapse's settlements reach the ledger with the save
    // that drops the probes, which the production flusher runs on its own.
    r.advance(30_000);
    r.step().await;
    r.engine.flush_persisted().await;
    let rows = rollups(&r, |rows| probe_rows(rows).wasted_probes == 3).await;
    assert_eq!(
        fundings(&rows, |row| row.wasted_probes > 0),
        billed,
        "the settlement: {rows:?}"
    );
    // A recipe whose entry lapses before its probe is due, so the probe is withheld.
    let mut r = rig(RigSpec::default()).await;
    r.up.push_credential_rule(A, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    r.advance(20 * MINUTE);
    r.step().await;
    assert_eq!(r.status("main").reason, Reason::Expired);
    let rows = rollups(&r, |rows| probe_rows(rows).probes_skipped == 1).await;
    assert_eq!(
        fundings(&rows, |row| row.probes_skipped > 0),
        billed,
        "the skip: {rows:?}"
    );
}

/// A due probe a gate withholds files one skip row under the keys its recipe held when the hold placed it, the key and
/// its generation, the pool, the account, the model and the speed, and counts once under the pool and the state the
/// hold leaves.
#[tokio::test]
async fn a_withheld_probe_files_its_skip_row_under_the_keys_its_recipe_held() {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    r.observe_with("main", &priced_body()).await;
    let mut pool = r.h.store.pool("main").unwrap().unwrap();
    pool.enabled = false;
    r.h.store
        .update_pool(&pool, pool.updated_at, "test")
        .unwrap();
    r.h.router.reload().await.unwrap();
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").reason, Reason::PoolDisabled);
    let generation = r.generation("desktop");
    let rows = final_rollups(&r).await;
    let skipped: Vec<_> = rows
        .iter()
        .filter(|row| row.probes_skipped > 0)
        .map(|row| {
            (
                row.key_id.as_str(),
                row.key_generation,
                row.pool_id.as_str(),
                row.account_id.as_str(),
                row.canonical_model.as_str(),
                row.speed.as_str(),
                row.funding.as_str(),
                row.probes_skipped,
            )
        })
        .collect();
    assert_eq!(
        skipped,
        [("desktop", generation, "main", "a", PRICED, "standard", "billed", 1)]
    );
    let counted: Vec<String> = r
        .metrics
        .render(&phoebis_ledger::RoutingGauges::default(), &[], 0)
        .lines()
        .filter(|line| line.starts_with("phoebis_keepalive_skipped_total{"))
        .map(str::to_string)
        .collect();
    assert_eq!(
        counted,
        [r#"phoebis_keepalive_skipped_total{pool="main",state="held"} 1"#]
    );
}

/// The account a probe served under leaves the registry while the probe is in flight: the probe's row files under the
/// kind the answer carried and asks the registry nothing.
#[tokio::test]
async fn a_probe_whose_account_is_deleted_in_flight_files_its_row_under_the_kind_its_answer_carried(
) {
    let mut r = rig(RigSpec::default()).await;
    let body = priced_body();
    r.up.push_credential_rule(A, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    r.up.push_credential_rule(
        A,
        "/v1/messages",
        Respond::Gated(Box::new(probe_answer(2_000, 0))),
    );
    r.advance(270_000);
    r.engine.step().await;
    wait_for(Duration::from_secs(5), || {
        (r.up.requests().len() == 2).then_some(())
    })
    .await;
    r.h.store.delete_account("a", "test").unwrap();
    r.h.router.reload().await.unwrap();
    r.up.release();
    r.engine.wait_idle().await;
    r.pump(None).await;
    // The probe's own request row: a probe that migrated nothing adds no keepalive row beside it.
    let probe_rows_of_a = |rows: &[RollupRow]| {
        rows.iter()
            .filter(|row| row.kind == "probe" && row.account_id == "a" && row.requests > 0)
            .map(|row| row.funding.clone())
            .collect::<Vec<_>>()
    };
    let rows = rollups(&r, |rows| !probe_rows_of_a(rows).is_empty()).await;
    assert_eq!(
        probe_rows_of_a(&rows),
        ["billed".to_string()],
        "the probe's row files under the api_key account's billing: {rows:?}"
    );
}

/// The funding of the request row of one probe on `o`, after a probe placed with the account in overage or on the
/// plan as `in_overage` says, whose answer carried the unified overage header at `head_flag`.
async fn probe_row_funding(in_overage: bool, head_flag: &str) -> String {
    let mut r = rig(plan_funded_spec()).await;
    if in_overage {
        r.enter_overage("o", r.now() + 10_000_000).await;
        settled(&r, |r| async move {
            r.h.router
                .overage("o")
                .await
                .unwrap()
                .is_some_and(|e| e.in_use)
        })
        .await;
    }
    let body = priced_body();
    r.up.push_credential_rule(OAUTH_SECRET, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    // The probe's answer carries the unified overage header the ledger reads the request row's funding from.
    let Respond::Json {
        body: read_back, ..
    } = probe_answer(2_000, 0)
    else {
        unreachable!("the standing probe answer is a JSON body")
    };
    r.up.push_credential_rule(
        OAUTH_SECRET,
        "/v1/messages",
        Respond::Json {
            status: 200,
            headers: vec![(
                "anthropic-ratelimit-unified-overage-in-use".into(),
                head_flag.into(),
            )],
            body: read_back,
        },
    );
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 1, "one covering probe");
    let probe_rows_of_o = |rows: &[RollupRow]| {
        rows.iter()
            .filter(|row| row.kind == "probe" && row.account_id == "o" && row.requests > 0)
            .map(|row| row.funding.clone())
            .collect::<Vec<_>>()
    };
    let rows = rollups(&r, |rows| !probe_rows_of_o(rows).is_empty()).await;
    let fundings = probe_rows_of_o(&rows);
    assert_eq!(fundings.len(), 1, "one request row: {rows:?}");
    fundings[0].clone()
}

/// The head decides. A probe placed on the plan whose answer reports overage in use (the account entered overage
/// between the selection and the answer) files its row under `overage`, never under the plan the selection stood on.
#[tokio::test]
async fn a_probe_placed_on_the_plan_whose_answer_reports_overage_files_its_row_under_overage() {
    assert_eq!(
        probe_row_funding(false, "true").await,
        "overage",
        "the row follows the head, not the selection"
    );
}

/// And the other way: a probe placed in overage whose answer reports the plan paying (the window reset between the
/// selection and the answer) files its row under `plan`.
#[tokio::test]
async fn a_probe_placed_in_overage_whose_answer_reports_the_plan_files_its_row_under_plan() {
    assert_eq!(
        probe_row_funding(true, "false").await,
        "plan",
        "the row follows the head, not the selection"
    );
}

/// The agent's overage follows a real request's head as the relay's unified parse reads it, so the header spelled in
/// mixed case with spaces around `TRUE` puts the current observation in overage, and the skip of a due probe a gate
/// withholds from that recipe files under `overage`. A probe's answer never reaches that reading, so the request's own
/// answer carries the header.
#[tokio::test]
async fn a_request_whose_answer_spells_the_overage_header_in_mixed_case_reads_the_agent_in_overage()
{
    let mut r = rig(plan_funded_spec()).await;
    let Respond::Json { body, .. } = cold_answer() else {
        unreachable!("the cold answer is a JSON body")
    };
    r.up.push_credential_rule(
        OAUTH_SECRET,
        "/v1/messages",
        Respond::Json {
            status: 200,
            headers: vec![(
                "Anthropic-RateLimit-Unified-Overage-In-Use".into(),
                " TRUE ".into(),
            )],
            body,
        },
    );
    r.observe_with("main", &priced_body()).await;
    let mut pool = r.h.store.pool("main").unwrap().unwrap();
    pool.enabled = false;
    r.h.store
        .update_pool(&pool, pool.updated_at, "test")
        .unwrap();
    r.h.router.reload().await.unwrap();
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").reason, Reason::PoolDisabled);
    let rows = final_rollups(&r).await;
    let skipped: Vec<&str> = rows
        .iter()
        .filter(|row| row.probes_skipped > 0)
        .map(|row| row.funding.as_str())
        .collect();
    assert_eq!(skipped, ["overage"], "the head put the agent in overage");
}

/// Each wasted probe files under the funding its own answer took, as its cost row does: one probe on the plan and one in
/// overage, both wasted at one cost, write one settlement row per funding, and each funding's waste reads against its
/// own probe dollars at a ratio of one.
#[tokio::test]
async fn a_plan_probe_and_an_overage_probe_both_wasted_file_one_row_per_funding_at_a_ratio_of_one()
{
    let mut r = rig(plan_funded_spec()).await;
    let body = priced_body();
    r.up.push_credential_rule(OAUTH_SECRET, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    // The first probe's answer reports the plan paying and the second's overage in use, each reading the prefix back.
    for head_flag in ["false", "true"] {
        let Respond::Json {
            body: read_back, ..
        } = probe_answer(2_000, 0)
        else {
            unreachable!("the standing probe answer is a JSON body")
        };
        r.up.push_credential_rule(
            OAUTH_SECRET,
            "/v1/messages",
            Respond::Json {
                status: 200,
                headers: vec![(
                    "anthropic-ratelimit-unified-overage-in-use".into(),
                    head_flag.into(),
                )],
                body: read_back,
            },
        );
        r.advance_attended(270_000);
        r.step().await;
    }
    assert_eq!(
        (r.status("main").probes, r.status("main").unanswered),
        (2, 2),
        "two probes nobody answered"
    );
    r.event(Event {
        event: "session_end".into(),
        ..Default::default()
    })
    .unwrap();
    let rows = final_rollups(&r).await;
    let mut by_funding: BTreeMap<String, (u64, u64, u64)> = BTreeMap::new();
    for row in rows.iter().filter(|row| row.kind == "probe") {
        let sum = by_funding.entry(row.funding.clone()).or_default();
        sum.0 += row.wasted_probes;
        sum.1 += tokens(row.wasted_usage());
        sum.2 += tokens(row.usage());
    }
    let mut settled: Vec<(String, u64)> = rows
        .iter()
        .filter(|row| row.wasted_probes > 0)
        .map(|row| (row.funding.clone(), row.wasted_probes))
        .collect();
    settled.sort();
    assert_eq!(
        settled,
        [("overage".to_string(), 1), ("plan".to_string(), 1)],
        "one settlement row per funding: {rows:?}"
    );
    for (funding, (_, wasted, used)) in by_funding.iter().filter(|(_, sum)| sum.0 > 0) {
        assert!(
            *used > 0 && wasted == used,
            "{funding}: the waste carries its own probe's counts, {wasted} of {used}"
        );
    }
}

/// A settlement files under the kind the probe was served under, whatever the registry holds by then: an api_key
/// account's probe, settled after the account left the registry, still files its waste under `billed`.
#[tokio::test]
async fn a_settlement_after_its_account_is_deleted_files_under_the_kind_the_probe_ran_under() {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    let body = priced_body();
    r.up.push_credential_rule(A, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    r.advance_attended(270_000);
    r.step().await;
    assert_eq!(r.status("main").unanswered, 1, "one probe nobody answered");
    r.h.store.delete_account("a", "test").unwrap();
    r.h.router.reload().await.unwrap();
    r.event(Event {
        event: "session_end".into(),
        ..Default::default()
    })
    .unwrap();
    let rows = final_rollups(&r).await;
    let settled: Vec<(&str, &str, u64)> = rows
        .iter()
        .filter(|row| row.wasted_probes > 0)
        .map(|row| {
            (
                row.account_id.as_str(),
                row.funding.as_str(),
                row.wasted_probes,
            )
        })
        .collect();
    assert_eq!(
        settled,
        [("a", "billed", 1)],
        "the waste files under the api_key kind its probe ran under: {rows:?}"
    );
}

/// The account check refuses a credit on its own. A probe warms the recipe's entry on `a`, and the next request on
/// the same model and speed arrives past the TTL since the last real request, while that entry is still there, and is
/// served on `b`: what it read there ordinary traffic cached, not the probe, so nothing is credited and the probe
/// settles as a cold return.
#[tokio::test]
async fn a_credit_binds_to_the_account_the_probe_warmed() {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0), api_key("b", B, 1)],
        ..RigSpec::default()
    })
    .await;
    open_request_as(&r, "first", OTHER_CLIENT, 0, 2_000);
    r.engine.observe(completed(&r, "first", 0));
    r.advance_attended(270_000);
    r.step().await;
    let status = r.status("main");
    assert_eq!(
        (status.probes, status.unanswered, status.migrations),
        (1, 1, 0),
        "a covering probe on a"
    );
    // The probe refreshed its five-minute entry 270 s in, so it is there until 570 s in; the request arrives 400 s
    // in, past the five minutes since the first.
    r.advance_attended(130_000);
    r.engine.observe(r.observed("second", "main", BODY));
    r.engine.observe(r.selected("second", "b"));
    let mut served = completed(&r, "second", 2_000);
    if let phoebis_bus::Event::RequestComplete(complete) = &mut served {
        complete.account_id = Some("b".into());
    }
    r.engine.observe(served);
    assert_eq!(r.status("main").unanswered, 0);
    r.ledger.clone().shutdown().await.unwrap();
    let rows =
        r.db.read_blocking(|c| {
            read_rollups(c, hour_of(NOW) - 3_600, i64::MAX, &RollupSelect::default())
        })
        .unwrap();
    assert_eq!(
        (
            inference_rows(&rows).kept_warm_requests,
            probe_rows(&rows).wasted_probe_reasons
        ),
        (0, BTreeMap::from([("cold_return".to_string(), 1)])),
        "the read on b answered nothing the probe on a kept"
    );
}

/// A probe answers only to a read that arrived while the entry it wrote was still there. A request that failed before
/// any head leaves the probe kept. The next request on the same model, speed and account arrives as that entry expires
/// and reads the cache, which nothing the probe paid for kept, so the probe settles as expired.
#[tokio::test]
async fn a_read_arriving_as_the_kept_entry_expires_settles_its_probes_as_expired() {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    open_request_as(&r, "first", OTHER_CLIENT, 0, 2_000);
    r.engine.observe(completed(&r, "first", 0));
    r.advance_attended(270_000);
    r.step().await;
    assert_eq!(r.status("main").unanswered, 1, "a covering probe");
    r.advance_attended(10_000);
    r.engine.observe(r.observed("second", "main", BODY));
    r.engine.observe(r.selected("second", "a"));
    r.engine.observe(failed(&r, "second", None));
    assert_eq!(r.status("main").unanswered, 1, "the probe stays kept");
    // The probe refreshed its five-minute entry 270 s in, so it expires 570 s in.
    r.advance_attended(290_000);
    open_request_as(&r, "third", OTHER_CLIENT, 2_000, 0);
    r.engine.observe(completed(&r, "third", 2_000));
    assert_eq!(r.status("main").unanswered, 0);
    r.ledger.clone().shutdown().await.unwrap();
    let rows =
        r.db.read_blocking(|c| {
            read_rollups(c, hour_of(NOW) - 3_600, i64::MAX, &RollupSelect::default())
        })
        .unwrap();
    let probe = probe_rows(&rows);
    assert_eq!(
        (
            inference_rows(&rows).kept_warm_requests,
            probe.wasted_probe_reasons.clone()
        ),
        (0, BTreeMap::from([("expired".to_string(), 1)])),
        "the read arrived as the probe's entry expired"
    );
    assert!(
        tokens(probe.wasted_usage()) > 0 && probe.wasted_usage() == probe.usage(),
        "{probe:?}"
    );
}

/// The rig of the pending-probe cases: an api_key account under a 500-second idle window, a request at 0 s whose
/// five-minute entry the probe at 270 s reads back and keeps to 570 s, and the tick at 540 s, past the window, that ends
/// the recipe. The session went silent after its lease, so the window counts from the request.
async fn ended_by_the_idle_limit() -> Rig {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        policy: KeepalivePolicy {
            max_idle_seconds: 500,
            ..RigSpec::default().policy
        },
        ..RigSpec::default()
    })
    .await;
    open_request_as(&r, "first", OTHER_CLIENT, 0, 2_000);
    r.engine.observe(completed(&r, "first", 0));
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 1, "one covering probe");
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        (r.status("main").reason, r.engine.stats().retained_bytes),
        (Reason::IdleLimit, 0),
        "the window ended the recipe"
    );
    r
}

/// A probe the idle limit wrote off stays pending until its entry lapses or a turn reads it. A turn inside the entry
/// on the probe's lineage reads the coverage the kept probe carries and earns the credit with nothing settled, the
/// lapse with no turn settles it under `idle_limit`, and a turn past the entry settles it `expired`.
#[tokio::test]
async fn a_probe_past_the_idle_limit_is_credited_inside_its_entry_and_settles_at_its_lapse_or_as_expired(
) {
    let r = ended_by_the_idle_limit().await;
    r.advance(20_000);
    open_request_as(&r, "second", OTHER_CLIENT, 2_000, 0);
    r.engine.observe(completed(&r, "second", 2_000));
    let rows = final_rollups(&r).await;
    assert_eq!(
        (
            inference_rows(&rows).kept_warm_requests,
            probe_rows(&rows).wasted_probe_reasons
        ),
        (1, BTreeMap::new()),
        "the warm return after the ceiling, inside the probe's entry, is credited"
    );

    let mut r = ended_by_the_idle_limit().await;
    r.advance(29_999);
    r.step().await;
    assert_eq!(
        r.status("main").unanswered,
        1,
        "pending a millisecond before its entry ends"
    );
    r.advance(1);
    r.step().await;
    r.engine.flush_persisted().await;
    let rows = final_rollups(&r).await;
    assert_eq!(
        (
            inference_rows(&rows).kept_warm_requests,
            probe_rows(&rows).wasted_probe_reasons
        ),
        (0, BTreeMap::from([("idle_limit".to_string(), 1)])),
        "the entry lapsed with no turn"
    );

    // No tick between the entry's end at 570 s and the turn at 600 s.
    let r = ended_by_the_idle_limit().await;
    r.advance(60_000);
    open_request_as(&r, "second", OTHER_CLIENT, 2_000, 0);
    r.engine.observe(completed(&r, "second", 2_000));
    let rows = final_rollups(&r).await;
    assert_eq!(
        (
            inference_rows(&rows).kept_warm_requests,
            probe_rows(&rows).wasted_probe_reasons
        ),
        (0, BTreeMap::from([("expired".to_string(), 1)])),
        "the turn arrived past the probe's entry"
    );
}

/// A turn that arrives while a pending probe's entry lives takes the probe off the lapse until its completion decides
/// it: ticks past the entry's end while the turn streams settle nothing, and the completion earns the credit.
#[tokio::test]
async fn a_turn_streaming_across_a_pending_probes_entry_end_keeps_it_off_the_lapse_and_earns_the_credit(
) {
    let mut r = ended_by_the_idle_limit().await;
    r.advance(20_000);
    open_request_as(&r, "second", OTHER_CLIENT, 2_000, 0);
    // Past the probe's entry at 570 s, then past the turn's own at 860 s, with the turn still open.
    for _ in 0..2 {
        r.advance(160_000);
        r.step().await;
    }
    assert_eq!(
        r.status("main").unanswered,
        1,
        "the probe waits for the turn's completion"
    );
    r.engine.observe(completed(&r, "second", 2_000));
    let rows = final_rollups(&r).await;
    assert_eq!(
        (
            inference_rows(&rows).kept_warm_requests,
            probe_rows(&rows).wasted_probe_reasons
        ),
        (1, BTreeMap::new()),
        "the credit lands and nothing settles under the idle limit"
    );
}

/// A session whose only recipe the idle limit ended holds its pending probe alone, with nothing retained for a tick to
/// ask the router about: the first tick at the entry's end still settles it under `idle_limit` with no router view
/// asked, and the settlement reaches the ledger with the save after it.
#[tokio::test]
async fn a_session_holding_a_pending_probe_alone_settles_it_at_the_lapse_without_asking_the_router()
{
    let r = ended_by_the_idle_limit().await;
    assert_eq!(
        (r.engine.stats().retained_bytes, r.status("main").unanswered),
        (0, 1),
        "nothing retained, the probe pending"
    );
    let views = r.engine.router_views();
    r.advance(30_000);
    r.engine.step().await;
    assert_eq!(
        (r.engine.router_views(), r.status("main").unanswered),
        (views, 0),
        "the tick settled the probe and asked the router nothing"
    );
    r.engine.flush_persisted().await;
    let rows = rollups(&r, |rows| probe_rows(rows).wasted_probes == 1).await;
    assert_eq!(
        probe_rows(&rows).wasted_probe_reasons,
        BTreeMap::from([("idle_limit".to_string(), 1)])
    );
}

/// A turn that ends with no answer from the upstream decides nothing about the pending probe it found: one canceled
/// before its head and one a bus gap ended both hand the probe back to the lapse, which settles it under `idle_limit`
/// at its entry's end.
#[tokio::test]
async fn a_turn_ended_before_its_head_or_by_a_bus_gap_leaves_the_pending_probe_to_its_lapse() {
    for gap in [false, true] {
        let mut r = ended_by_the_idle_limit().await;
        r.advance(10_000);
        r.engine.observe(r.observed("second", "main", BODY));
        r.engine.observe(r.selected("second", "a"));
        if gap {
            r.engine.bus_gap(1, 0);
        } else {
            r.engine
                .observe(r.complete("second", phoebis_bus::Outcome::Canceled, None));
        }
        r.advance(19_999);
        r.step().await;
        assert_eq!(
            r.status("main").unanswered,
            1,
            "pending a millisecond before its entry ends (bus gap: {gap})"
        );
        r.advance(1);
        r.step().await;
        assert_eq!(
            r.status("main").unanswered,
            0,
            "the lapse settled the probe (bus gap: {gap})"
        );
        r.engine.flush_persisted().await;
        let rows = final_rollups(&r).await;
        assert_eq!(
            (
                inference_rows(&rows).kept_warm_requests,
                probe_rows(&rows).wasted_probe_reasons
            ),
            (0, BTreeMap::from([("idle_limit".to_string(), 1)])),
            "the entry lapsed after a turn that read nothing (bus gap: {gap})"
        );
    }
}

/// A probe pending past its recipe's end sits on an agent that holds no recipe, which no tick asks the router about, so
/// a resolution that lands then reaches the probe at the next request's arrival: the request under the dated id reads
/// the entry the probe kept as its own lineage and is credited, with nothing settled as a cold return.
#[tokio::test]
async fn a_probe_pending_past_its_recipes_end_follows_a_resolution_to_the_next_requests_credit() {
    let dated = "claude-sonnet-4-5-20250929";
    let mut r = ended_by_the_idle_limit().await;
    let listing = relay_common::catalog_rules().remove(0);
    r.h.catalog_up
        .push_credential_rule(A, &listing.path_prefix, listing.respond);
    let a = r.h.store.account("a").unwrap().unwrap();
    r.h.catalog.refresh(&a).await;
    assert_eq!(
        r.h.catalog.lookup("claude-sonnet-4-5").as_deref(),
        Some(dated)
    );
    let views = r.engine.router_views();
    r.advance(10_000);
    r.step().await;
    assert_eq!(
        (r.engine.router_views(), r.status("main").unanswered),
        (views, 1),
        "the tick asked the router nothing and the probe still waits"
    );
    let second = r.observe("main").await;
    assert_eq!(r.status("main").canonical_model, dated);
    let rows = final_rollups(&r).await;
    assert_eq!(
        (
            inference_rows(&rows).kept_warm_requests,
            probe_rows(&rows).wasted_probe_reasons
        ),
        (1, BTreeMap::new()),
        "the probe's lineage followed the resolution, so the request answered it and was credited"
    );
    assert!(row(&r, &second).await.kept_warm);
}

/// A request that names the dated id itself carries no resolution of its own for a probe kept under the spelling, so
/// the arrival dates the names the agent holds through the catalog's acknowledged resolution: the probe pending past
/// its recipe's end follows the twin spelling to the dated id, and the request reads the entry it kept as its own
/// lineage.
#[tokio::test]
async fn a_probe_under_a_twin_spelling_of_the_dated_id_a_request_names_is_dated_at_its_arrival_and_keeps_the_credit(
) {
    let dated = "claude-sonnet-4-5-20250929";
    let mut r = ended_by_the_idle_limit().await;
    let listing = relay_common::catalog_rules().remove(0);
    r.h.catalog_up
        .push_credential_rule(A, &listing.path_prefix, listing.respond);
    let a = r.h.store.account("a").unwrap().unwrap();
    r.h.catalog.refresh(&a).await;
    assert_eq!(
        r.h.catalog.lookup("claude-sonnet-4-5").as_deref(),
        Some(dated)
    );
    r.advance(10_000);
    let second = r
        .observe_with("main", &BODY.replace("claude-sonnet-4-5", dated))
        .await;
    assert_eq!(r.status("main").canonical_model, dated);
    let rows = final_rollups(&r).await;
    assert_eq!(
        (
            inference_rows(&rows).kept_warm_requests,
            probe_rows(&rows).wasted_probe_reasons
        ),
        (1, BTreeMap::new()),
        "the probe's spelling was dated, so the request under the dated id answered it and was credited"
    );
    assert!(row(&r, &second).await.kept_warm);
}

/// A compaction's observation on `main` under the wire spelling `claude-sonnet-4-5`, named `canonical` as the relay
/// resolved it.
fn compaction_named(r: &Rig, id: &str, canonical: &str) -> phoebis_bus::Event {
    let mut event = r.observed_with(
        id,
        "main",
        Some("compaction"),
        None,
        &[],
        BODY.as_bytes(),
        "claude-sonnet-4-5",
    );
    if let phoebis_bus::Event::RequestObserved(observed) = &mut event {
        observed.canonical_model = canonical.into();
    }
    event
}

/// `id`'s answer: a `message_start` that reads 2,000, then the completion.
fn a_warm_answer(r: &Rig, id: &str) {
    r.engine.observe(message_start(
        id,
        phoebis_bus::Usage {
            cache_read_tokens: Some(2_000),
            ..Default::default()
        },
    ));
    r.engine.observe(completed(r, id, 2_000));
}

/// The models of the rows that carry a kept-warm credit.
fn credited_models(rows: &[RollupRow]) -> Vec<&str> {
    rows.iter()
        .filter(|row| row.kept_warm_requests > 0)
        .map(|row| row.canonical_model.as_str())
        .collect()
}

/// A compaction follows the resolution its own request carries as a turn does: past its recipe's end, with no tick
/// asking the router, a compaction under the dated id reads the entry the pending probe kept under the spelling as its
/// own lineage, so it earns the credit and settles nothing as a cold return.
#[tokio::test]
async fn a_compaction_past_its_recipes_end_follows_its_own_resolution_to_the_pending_probes_credit()
{
    let dated = "claude-sonnet-4-5-20250929";
    let mut r = ended_by_the_idle_limit().await;
    let listing = relay_common::catalog_rules().remove(0);
    r.h.catalog_up
        .push_credential_rule(A, &listing.path_prefix, listing.respond);
    let a = r.h.store.account("a").unwrap().unwrap();
    r.h.catalog.refresh(&a).await;
    let views = r.engine.router_views();
    r.advance(10_000);
    r.step().await;
    assert_eq!(
        (r.engine.router_views(), r.status("main").unanswered),
        (views, 1),
        "the tick asked the router nothing and the probe still waits"
    );
    r.engine.observe(compaction_named(&r, "compaction", dated));
    r.engine.observe(r.selected("compaction", "a"));
    a_warm_answer(&r, "compaction");
    let rows = final_rollups(&r).await;
    assert_eq!(
        (
            inference_rows(&rows).kept_warm_requests,
            probe_rows(&rows).wasted_probe_reasons
        ),
        (1, BTreeMap::new()),
        "the probe's lineage followed the compaction's resolution, so the compaction answered it and was credited"
    );
    assert_eq!(credited_models(&rows), [dated]);
}

/// A resolution that lands while a compaction is open moves the compaction with the recipe and its probes: the probe
/// under the spelling 270 s in, a compaction under the spelling 330 s in, then the dated id and a tick while it
/// streams. The completion credits under the dated id and answers the probe, so no cold return sits beside the credit.
#[tokio::test]
async fn a_model_resolved_while_a_compaction_is_open_moves_its_credit_and_its_answer_together() {
    let dated = "claude-sonnet-4-5-20250929";
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    r.up.push_credential_rule(A, "/v1/messages", cold_answer());
    r.observe("main").await;
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        r.status("main").probes,
        1,
        "the probe landed under the spelling"
    );
    r.advance(60_000);
    r.engine
        .observe(compaction_named(&r, "compaction", "claude-sonnet-4-5"));
    r.engine.observe(r.selected("compaction", "a"));
    let listing = relay_common::catalog_rules().remove(0);
    r.h.catalog_up
        .push_credential_rule(A, &listing.path_prefix, listing.respond);
    let a = r.h.store.account("a").unwrap().unwrap();
    r.h.catalog.refresh(&a).await;
    r.step().await;
    assert_eq!(
        r.status("main").canonical_model,
        dated,
        "the tick followed the resolution while the compaction was open"
    );
    a_warm_answer(&r, "compaction");
    let rows = final_rollups(&r).await;
    assert_eq!(
        (
            inference_rows(&rows).kept_warm_requests,
            probe_rows(&rows).wasted_probe_reasons
        ),
        (1, BTreeMap::new()),
        "the compaction answered the probe it was credited for"
    );
    assert_eq!(credited_models(&rows), [dated]);
}

/// A tick asks the router about an open compaction's models even where its agent's recipe has ended: another agent's
/// recipe makes the tick ask, and the resolution of the compaction's spelling moves the compaction and the pending
/// probe together, so the credit files under the dated id. Main's recipe ends as `ended_by_the_idle_limit` ends it,
/// with a worker registered while the session was attended.
#[tokio::test]
async fn a_tick_follows_a_resolution_for_a_compaction_open_on_an_ended_recipe() {
    let dated = "claude-sonnet-4-5-20250929";
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        policy: KeepalivePolicy {
            max_idle_seconds: 500,
            ..RigSpec::default().policy
        },
        ..RigSpec::default()
    })
    .await;
    r.event(Event {
        event: "agent_start".into(),
        agent_id: Some("worker".into()),
        ..Default::default()
    })
    .unwrap();
    open_request_as(&r, "first", OTHER_CLIENT, 0, 2_000);
    r.engine.observe(completed(&r, "first", 0));
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 1, "one covering probe");
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        (r.status("main").reason, r.engine.stats().retained_bytes),
        (Reason::IdleLimit, 0),
        "the window ended main's recipe"
    );
    // A recipe on a model the catalog leaves as it is, so the tick asks the router.
    r.engine
        .observe(r.observed("worker-first", "worker", &opus_body()));
    r.engine.observe(r.selected("worker-first", "a"));
    r.engine.observe(message_start(
        "worker-first",
        phoebis_bus::Usage {
            cache_read_tokens: Some(0),
            cache_write_5m_tokens: Some(2_000),
            ..Default::default()
        },
    ));
    r.engine.observe(completed(&r, "worker-first", 0));
    r.engine
        .observe(compaction_named(&r, "compaction", "claude-sonnet-4-5"));
    r.engine.observe(r.selected("compaction", "a"));
    let listing = relay_common::catalog_rules().remove(0);
    r.h.catalog_up
        .push_credential_rule(A, &listing.path_prefix, listing.respond);
    let a = r.h.store.account("a").unwrap().unwrap();
    r.h.catalog.refresh(&a).await;
    let views = r.engine.router_views();
    r.advance(10_000);
    r.step().await;
    assert_eq!(
        (r.engine.router_views(), r.status("main").unanswered),
        (views + 1, 1),
        "the tick asked the router and the probe still waits"
    );
    assert_eq!(
        r.status("main").canonical_model,
        dated,
        "the tick followed the resolution of the compaction's spelling"
    );
    a_warm_answer(&r, "compaction");
    let rows = final_rollups(&r).await;
    assert_eq!(
        (
            inference_rows(&rows).kept_warm_requests,
            probe_rows(&rows).wasted_probe_reasons
        ),
        (1, BTreeMap::new()),
        "the compaction answered the probe it was credited for"
    );
    assert_eq!(credited_models(&rows), [dated]);
}

/// A compaction's kept-warm credit files under the TTL its own write proves, as a turn's does. An hour recipe, a
/// covering probe 3,240 s in, and a compaction 3,640 s in that reads 2,000 and writes 100 under five minutes: the
/// credit books five minutes whether the selection named no TTL or the hour, and whether the write reached the engine
/// on the stream or on the completion alone.
#[tokio::test]
async fn a_compaction_credit_files_under_the_ttl_its_own_usage_proves() {
    let five_minute_write = || phoebis_bus::Usage {
        cache_read_tokens: Some(2_000),
        cache_write_5m_tokens: Some(100),
        cache_write_1h_tokens: Some(0),
        ..Default::default()
    };
    for (dispatched, streamed) in [(None, true), (Some(3_600_000), true), (None, false)] {
        let mut r = rig(RigSpec {
            accounts: vec![api_key("a", A, 0)],
            ..RigSpec::default()
        })
        .await;
        let hour = opus_body().replace(
            r#""cache_control":{"type":"ephemeral"}"#,
            r#""cache_control":{"type":"ephemeral","ttl":"1h"}"#,
        );
        r.engine.observe(r.observed("first", "main", &hour));
        r.engine.observe(r.selected("first", "a"));
        r.engine.observe(message_start(
            "first",
            phoebis_bus::Usage {
                cache_read_tokens: Some(0),
                cache_write_1h_tokens: Some(2_000),
                ..Default::default()
            },
        ));
        r.engine.observe(completed(&r, "first", 0));
        assert_eq!(r.status("main").cache_ttl_ms, 3_600_000, "a one-hour entry");
        r.advance_attended(3_600_000 - 360_000);
        r.step().await;
        assert_eq!(
            (r.status("main").probes, r.status("main").unanswered),
            (1, 1),
            "a covering probe"
        );
        // The probe refreshed its one-hour entry 3,240 s in, so it is there until 6,840 s in, and the compaction
        // arrives 3,640 s in, past the hour since the first.
        r.advance_attended(400_000);
        r.engine.observe(r.observed_with(
            "compaction",
            "main",
            Some("compaction"),
            None,
            &[],
            hour.as_bytes(),
            OPUS,
        ));
        let mut selected = r.selected("compaction", "a");
        if let phoebis_bus::Event::AccountSelected(selected) = &mut selected {
            selected.cache_ttl_ms = dispatched;
        }
        r.engine.observe(selected);
        let done = if streamed {
            r.engine.observe(head(
                &r,
                "compaction",
                1,
                200,
                phoebis_bus::AttemptOutcome::Succeeded,
            ));
            r.engine
                .observe(message_start("compaction", five_minute_write()));
            completed(&r, "compaction", 2_000)
        } else {
            r.complete(
                "compaction",
                phoebis_bus::Outcome::Succeeded,
                Some(five_minute_write()),
            )
        };
        r.engine.observe(done);
        let credit = inference_rows(&final_rollups(&r).await);
        assert_eq!(
            (
                credit.kept_warm_requests,
                credit.kept_warm_5m_tokens,
                credit.kept_warm_1h_tokens
            ),
            (1, 2_000, 0),
            "dispatched {dispatched:?}, the write on the stream: {streamed}"
        );
    }
}

/// The head of `id`'s attempt `attempt`, answered at `status`.
fn head(
    r: &Rig,
    id: &str,
    attempt: u8,
    status: u16,
    outcome: phoebis_bus::AttemptOutcome,
) -> phoebis_bus::Event {
    phoebis_bus::Event::ResponseHead(phoebis_bus::ResponseHead {
        request_id: id.into(),
        attempt,
        status,
        headers: Vec::new(),
        first_event: None,
        outcome,
        cooldown: None,
        written_at: r.now(),
        observed_at: r.now(),
    })
}

/// A turn on `id`, observed and placed on `a`, that the upstream fails with a 500: the head, then the failed completion
/// past it.
fn a_turn_failed_with_a_500(r: &Rig, id: &str) {
    r.engine.observe(r.observed(id, "main", BODY));
    r.engine.observe(r.selected(id, "a"));
    r.engine.observe(head(
        r,
        id,
        1,
        500,
        phoebis_bus::AttemptOutcome::UpstreamError,
    ));
    let mut event = failed(r, id, Some(40));
    if let phoebis_bus::Event::RequestComplete(complete) = &mut event {
        complete.status = 500;
    }
    r.engine.observe(event);
}

/// A turn the upstream failed is no evidence about the cache, so a retry reads the gap from the last turn that
/// answered. A request at 0 s, a covering probe at 270 s keeping its entry to 570 s, and a turn at 330 s that fails with
/// a 500: a retry inside the entry, past the first request's five-minute TTL, earns the credit the failed turn could
/// not, and a retry past the entry reads nothing the probe kept, earns none, and settles the probe `expired`.
#[tokio::test]
async fn a_retry_after_a_failed_turn_earns_the_credit_inside_the_probes_entry_and_none_past_it() {
    for (retry_after, expected) in [
        (10_000, (1, BTreeMap::new())),
        (270_000, (0, BTreeMap::from([("expired".to_string(), 1)]))),
    ] {
        let mut r = rig(RigSpec {
            accounts: vec![api_key("a", A, 0)],
            ..RigSpec::default()
        })
        .await;
        open_request_as(&r, "first", OTHER_CLIENT, 0, 2_000);
        r.engine.observe(completed(&r, "first", 0));
        r.advance_attended(270_000);
        r.step().await;
        assert_eq!(r.status("main").unanswered, 1, "a covering probe");
        r.advance_attended(60_000);
        a_turn_failed_with_a_500(&r, "failed");
        r.advance_attended(retry_after);
        open_request_as(&r, "retry", OTHER_CLIENT, 2_000, 0);
        r.engine.observe(completed(&r, "retry", 2_000));
        let rows = final_rollups(&r).await;
        assert_eq!(
            (
                inference_rows(&rows).kept_warm_requests,
                probe_rows(&rows).wasted_probe_reasons
            ),
            expected,
            "a retry {retry_after} ms after the failed turn"
        );
    }
}

/// The retry's credit holds for a probe the idle limit left pending too: a turn inside its entry that fails with a 500
/// hands the probe back under its label, and the retry after it reads the probe's coverage and is credited.
#[tokio::test]
async fn a_retry_after_a_failed_turn_reads_the_probe_the_idle_limit_left_pending() {
    let r = ended_by_the_idle_limit().await;
    r.advance(10_000);
    a_turn_failed_with_a_500(&r, "failed");
    r.advance(10_000);
    open_request_as(&r, "retry", OTHER_CLIENT, 2_000, 0);
    r.engine.observe(completed(&r, "retry", 2_000));
    let rows = final_rollups(&r).await;
    assert_eq!(
        (
            inference_rows(&rows).kept_warm_requests,
            probe_rows(&rows).wasted_probe_reasons
        ),
        (1, BTreeMap::new()),
        "the retry inside the probe's entry is credited and nothing settles"
    );
}

/// A failed attempt the request's own replay answered is no failed turn: a 401 on the first attempt and a 200 on the
/// refreshed credential's second, inside a covered entry past the first request's TTL, is credited as a plain success,
/// and its arrival is the instant the next request's gap runs from.
#[tokio::test]
async fn a_request_whose_replay_answers_a_401_is_credited_as_a_plain_success() {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    open_request_as(&r, "first", OTHER_CLIENT, 0, 2_000);
    r.engine.observe(completed(&r, "first", 0));
    r.advance_attended(270_000);
    r.step().await;
    assert_eq!(r.status("main").unanswered, 1, "a covering probe");
    r.advance_attended(60_000);
    let arrived = r.now();
    r.engine.observe(r.observed("replayed", "main", BODY));
    r.engine.observe(r.selected("replayed", "a"));
    r.engine.observe(head(
        &r,
        "replayed",
        1,
        401,
        phoebis_bus::AttemptOutcome::AuthFailed,
    ));
    let mut replay = r.selected("replayed", "a");
    if let phoebis_bus::Event::AccountSelected(selected) = &mut replay {
        selected.attempt = 2;
    }
    r.engine.observe(replay);
    r.engine.observe(head(
        &r,
        "replayed",
        2,
        200,
        phoebis_bus::AttemptOutcome::Succeeded,
    ));
    let mut start = message_start(
        "replayed",
        phoebis_bus::Usage {
            cache_read_tokens: Some(2_000),
            ..Default::default()
        },
    );
    if let phoebis_bus::Event::ResponseChunk(chunk) = &mut start {
        chunk.attempt = 2;
    }
    r.engine.observe(start);
    let mut done = completed(&r, "replayed", 2_000);
    if let phoebis_bus::Event::RequestComplete(complete) = &mut done {
        complete.attempt = 2;
    }
    r.engine.observe(done);
    let (_, agents) = r.engine.persist_rows();
    let payload: serde_json::Value = serde_json::from_str(&agents[0].payload).unwrap();
    let rows = final_rollups(&r).await;
    assert_eq!(
        (
            inference_rows(&rows).kept_warm_requests,
            probe_rows(&rows).wasted_probe_reasons
        ),
        (1, BTreeMap::new()),
        "the replayed request is credited as a plain success"
    );
    assert_eq!(
        payload["last_answered_at"].as_i64(),
        Some(arrived),
        "the replayed request answered from its arrival"
    );
}

/// A turn whose completion the engine never saw counts as the turn that answered at its start, whether the next
/// request's arrival or a bus gap that may have lost the completion ends it: a request at 0 s, a turn at 200 s that
/// reads the cache and arms the schedule from its first event, a probe at 470 s while that turn is open, and a request
/// at 490 s that reads the entry. Measured from the turn at 200 s, the gap stays inside that turn's own five-minute
/// entry, so the probe kept nothing the request needed and earns no credit. A failed completion the engine observed
/// moves nothing: the gap runs from the request at 0 s, and the probe earns the credit.
#[tokio::test]
async fn a_turn_whose_completion_the_engine_missed_starts_the_next_kept_warm_gap_and_a_failed_one_does_not(
) {
    for (ending, credited) in [("missed", 0), ("bus gap", 0), ("failed", 1)] {
        let mut r = rig(RigSpec {
            accounts: vec![api_key("a", A, 0)],
            ..RigSpec::default()
        })
        .await;
        let first_at = r.now();
        open_request_as(&r, "first", OTHER_CLIENT, 0, 2_000);
        r.engine.observe(completed(&r, "first", 0));
        r.advance_attended(200_000);
        let second_at = r.now();
        open_request_as(&r, "second", OTHER_CLIENT, 2_000, 0);
        r.advance_attended(270_000);
        r.step().await;
        assert_eq!(
            (r.status("main").probes, r.status("main").unanswered),
            (1, 1),
            "{ending}: the probe landed while the second turn was open"
        );
        match ending {
            "bus gap" => r.engine.bus_gap(1, 0),
            "failed" => r.engine.observe(failed(&r, "second", Some(40))),
            _ => {}
        }
        r.advance_attended(20_000);
        open_request_as(&r, "third", OTHER_CLIENT, 2_000, 0);
        let (_, agents) = r.engine.persist_rows();
        let payload: serde_json::Value = serde_json::from_str(&agents[0].payload).unwrap();
        assert_eq!(
            payload["last_answered_at"].as_i64(),
            Some(if credited == 0 { second_at } else { first_at }),
            "{ending}: the turn the third request's gap runs from"
        );
        r.engine.observe(completed(&r, "third", 2_000));
        let rows = final_rollups(&r).await;
        assert_eq!(
            (
                inference_rows(&rows).kept_warm_requests,
                probe_rows(&rows).wasted_probe_reasons
            ),
            (credited, BTreeMap::new()),
            "{ending}: the warm return answered the probe either way"
        );
    }
}

/// A compaction that arrives while a turn is open ends no turn: the turn stays pending for its own completion to
/// decide, so the compaction moves no answered instant, and a failure of that turn the engine then observes leaves the
/// gap to the turn that answered before it. On the timeline of the case above, a compaction at 480 s over the turn
/// opened at 200 s leaves the request at 490 s a gap from the request at 0 s, so the probe at 470 s earns the credit.
/// The compaction arrives after the probe because an open compaction holds the recipe unprobed.
#[tokio::test]
async fn a_compaction_over_an_open_turn_moves_no_answered_instant_and_that_turns_failure_keeps_it()
{
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    open_request_as(&r, "first", OTHER_CLIENT, 0, 2_000);
    r.engine.observe(completed(&r, "first", 0));
    r.advance_attended(200_000);
    open_request_as(&r, "second", OTHER_CLIENT, 2_000, 0);
    r.advance_attended(270_000);
    r.step().await;
    assert_eq!(
        (r.status("main").probes, r.status("main").unanswered),
        (1, 1),
        "the probe landed while the second turn was open"
    );
    r.advance_attended(10_000);
    r.engine
        .observe(compaction_named(&r, "compaction", "claude-sonnet-4-5"));
    r.engine.observe(failed(&r, "second", Some(40)));
    r.advance_attended(10_000);
    open_request_as(&r, "third", OTHER_CLIENT, 2_000, 0);
    r.engine.observe(completed(&r, "third", 2_000));
    let rows = final_rollups(&r).await;
    assert_eq!(
        (
            inference_rows(&rows).kept_warm_requests,
            probe_rows(&rows).wasted_probe_reasons
        ),
        (1, BTreeMap::new()),
        "the compaction and the open turn's failure kept the gap on the turn that answered"
    );
}

/// Every rollup row the rig wrote, read once the ledger has landed everything the test sent it.
async fn final_rollups(r: &Rig) -> Vec<RollupRow> {
    r.ledger.clone().shutdown().await.unwrap();
    r.db.read_blocking(|c| {
        read_rollups(c, hour_of(NOW) - 3_600, i64::MAX, &RollupSelect::default())
    })
    .unwrap()
}

/// The fixture of the per-probe settlement cases: a request at 0 s writes a five-minute entry, the first probe reads
/// it back 270 s in and keeps its own entry to 570 s, and the second, due 540 s in inside that entry, reads nothing
/// and rewrites the prefix, keeping its own entry to 840 s. Both stay unanswered for the request each case sends.
/// Answers the first probe's counts, read off the rollup before the second probe runs.
async fn a_covering_probe_then_a_rewrite(r: &mut Rig) -> PinUsage {
    open_request_as(r, "first", OTHER_CLIENT, 0, 2_000);
    r.engine.observe(completed(r, "first", 0));
    r.advance_attended(270_000);
    r.step().await;
    let first_cost =
        probe_rows(&rollups(r, |rows| tokens(probe_rows(rows).usage()) > 0).await).usage();
    r.up.push_credential_rule(A, "/v1/messages", probe_answer(0, 2_000));
    r.advance_attended(270_000);
    r.step().await;
    let status = r.status("main");
    assert_eq!(
        (status.probes, status.unanswered, status.cold, status.reason),
        (2, 2, 1, Reason::Scheduled),
        "the second probe rewrote the entry 540 s in: {status:?}"
    );
    first_cost
}

/// The request reads the entry the newest probe wrote. The first probe's entry lapsed 570 s in, before the request
/// arrived 600 s in, so it settles as expired whatever the request read. The rewrite 540 s in kept the entry the
/// request read, so it is answered and the request is credited: the credit and the wasted figure name different
/// probes and never contradict each other.
#[tokio::test]
async fn a_covering_probe_whose_entry_lapsed_settles_expired_while_the_rewrite_after_it_is_answered(
) {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    let first_cost = a_covering_probe_then_a_rewrite(&mut r).await;
    r.advance_attended(60_000);
    open_request_as(&r, "second", OTHER_CLIENT, 2_000, 0);
    r.engine.observe(completed(&r, "second", 2_000));
    let status = r.status("main");
    assert_eq!(status.unanswered, 0);
    let rows = final_rollups(&r).await;
    let (credit, probe) = (inference_rows(&rows), probe_rows(&rows));
    assert_eq!(
        (
            credit.kept_warm_requests,
            credit.kept_warm_5m_tokens,
            credit.kept_warm_1h_tokens,
            probe.wasted_probes,
            probe.wasted_probe_reasons.clone()
        ),
        (1, 2_000, 0, 1, BTreeMap::from([("expired".to_string(), 1)])),
        "the rewrite kept the entry the request read, and the first probe's had lapsed"
    );
    assert!(
        probe.wasted_usage() == first_cost,
        "the first probe is the wasted one: {first_cost:?} against {probe:?}"
    );
}

/// A request past both probes' entries reads nothing either probe kept, whatever it reads: both settle as expired, and
/// the newest probe's lapsed entry earns no credit.
#[tokio::test]
async fn a_request_past_both_entries_settles_both_probes_expired_and_earns_no_credit() {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    a_covering_probe_then_a_rewrite(&mut r).await;
    r.advance_attended(360_000);
    open_request_as(&r, "second", OTHER_CLIENT, 2_000, 0);
    r.engine.observe(completed(&r, "second", 2_000));
    assert_eq!(r.status("main").unanswered, 0);
    let rows = final_rollups(&r).await;
    let probe = probe_rows(&rows);
    assert_eq!(
        (
            inference_rows(&rows).kept_warm_requests,
            probe.wasted_probes,
            probe.wasted_probe_reasons.clone()
        ),
        (0, 2, BTreeMap::from([("expired".to_string(), 2)])),
        "the request arrived 900 s in, past 570 s and 840 s"
    );
    assert!(probe.wasted_usage() == probe.usage(), "{probe:?}");
}

/// A request inside both probes' entries that reads nothing kept no prefix either probe paid for: both settle as cold
/// returns.
#[tokio::test]
async fn a_request_inside_both_entries_that_reads_nothing_settles_both_as_cold_returns() {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    a_covering_probe_then_a_rewrite(&mut r).await;
    r.advance_attended(20_000);
    open_request_as(&r, "second", OTHER_CLIENT, 0, 2_000);
    r.engine.observe(completed(&r, "second", 0));
    assert_eq!(r.status("main").unanswered, 0);
    let rows = final_rollups(&r).await;
    let probe = probe_rows(&rows);
    assert_eq!(
        (
            inference_rows(&rows).kept_warm_requests,
            probe.wasted_probes,
            probe.wasted_probe_reasons.clone()
        ),
        (0, 2, BTreeMap::from([("cold_return".to_string(), 2)])),
        "the request arrived 560 s in, before 570 s and 840 s"
    );
}

/// Each probe settles by its own entry. A request that reads nothing 600 s in arrives after the first probe's entry
/// lapsed and inside the rewrite's: the first settles as expired and the second as a cold return.
#[tokio::test]
async fn a_request_past_the_first_entry_and_inside_the_second_that_reads_nothing_settles_one_expired_and_one_cold_return(
) {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    a_covering_probe_then_a_rewrite(&mut r).await;
    r.advance_attended(60_000);
    open_request_as(&r, "second", OTHER_CLIENT, 0, 2_000);
    r.engine.observe(completed(&r, "second", 0));
    assert_eq!(r.status("main").unanswered, 0);
    let rows = final_rollups(&r).await;
    let probe = probe_rows(&rows);
    assert_eq!(
        (
            inference_rows(&rows).kept_warm_requests,
            probe.wasted_probes,
            probe.wasted_probe_reasons.clone()
        ),
        (
            0,
            2,
            BTreeMap::from([("cold_return".to_string(), 1), ("expired".to_string(), 1)])
        ),
        "the request arrived 600 s in, after 570 s and before 840 s"
    );
}

/// The fixture of the chain cases: a request at 0 s writes a five-minute entry, and three probes 270, 540 and 810 s in
/// each read it back, their own entries ending 570, 840 and 1,110 s in. Each later probe's read extended the entry the
/// earlier ones kept, so the three are one chain whose last entry ends 1,110 s in.
async fn a_chain_of_three_probes(r: &mut Rig) {
    open_request_as(r, "first", OTHER_CLIENT, 0, 2_000);
    r.engine.observe(completed(r, "first", 0));
    for _ in 0..3 {
        r.advance_attended(270_000);
        r.step().await;
    }
    let status = r.status("main");
    assert_eq!(
        (status.probes, status.unanswered, status.cold, status.reason),
        (3, 3, 0, Reason::Scheduled),
        "the three probes read the entry back: {status:?}"
    );
}

/// Every probe of a chain is judged against the chain's last entry: a request 900 s in, past the first two probes' own
/// entries and inside the last's, that reads the cache answers all three.
#[tokio::test]
async fn a_chain_of_three_probes_answered_warm_inside_its_last_entry_settles_none_as_expired() {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    a_chain_of_three_probes(&mut r).await;
    r.advance_attended(90_000);
    open_request_as(&r, "second", OTHER_CLIENT, 2_000, 0);
    r.engine.observe(completed(&r, "second", 2_000));
    assert_eq!(r.status("main").unanswered, 0);
    let rows = final_rollups(&r).await;
    let probe = probe_rows(&rows);
    assert_eq!(
        (
            inference_rows(&rows).kept_warm_requests,
            probe.wasted_probes,
            probe.wasted_probe_reasons.clone()
        ),
        (1, 0, BTreeMap::new()),
        "the request arrived 900 s in, past 570 s and 840 s and before 1,110 s"
    );
}

/// A request past the chain's last entry reads nothing the chain kept, whatever it reads: all three probes settle as
/// expired and the request earns no credit.
#[tokio::test]
async fn a_chain_whose_last_entry_lapsed_before_the_request_settles_all_three_probes_expired() {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    a_chain_of_three_probes(&mut r).await;
    // Past the last entry's end with no tick, so no fourth probe joins the chain at 1,080 s.
    r.advance_attended(310_000);
    open_request_as(&r, "second", OTHER_CLIENT, 2_000, 0);
    r.engine.observe(completed(&r, "second", 2_000));
    assert_eq!(r.status("main").unanswered, 0);
    let rows = final_rollups(&r).await;
    let probe = probe_rows(&rows);
    assert_eq!(
        (
            inference_rows(&rows).kept_warm_requests,
            probe.wasted_probes,
            probe.wasted_probe_reasons.clone()
        ),
        (0, 3, BTreeMap::from([("expired".to_string(), 3)])),
        "the request arrived 1,120 s in, past 1,110 s"
    );
    assert!(probe.wasted_usage() == probe.usage(), "{probe:?}");
}

/// A compaction request reads the old prefix, so its completion answers the chain before it by the same test: one
/// 900 s in, inside the last probe's entry, that reads the cache leaves none of the three wasted.
#[tokio::test]
async fn a_compaction_request_inside_the_chains_last_entry_answers_every_probe_of_the_chain() {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    a_chain_of_three_probes(&mut r).await;
    r.advance_attended(90_000);
    r.engine.observe(r.observed_with(
        "compaction",
        "main",
        Some("compaction"),
        None,
        &[],
        BODY.as_bytes(),
        "claude-sonnet-4-5",
    ));
    r.engine.observe(r.selected("compaction", "a"));
    r.engine.observe(message_start(
        "compaction",
        phoebis_bus::Usage {
            cache_read_tokens: Some(2_000),
            ..Default::default()
        },
    ));
    r.engine.observe(completed(&r, "compaction", 2_000));
    let rows = final_rollups(&r).await;
    let probe = probe_rows(&rows);
    assert_eq!(
        (
            probe.requests,
            probe.wasted_probes,
            probe.wasted_probe_reasons.clone()
        ),
        (3, 0, BTreeMap::new()),
        "the compaction arrived 900 s in, past 570 s and 840 s and before 1,110 s"
    );
}

/// `event`, a request's observation, as sent by the client the relay classed `class`.
fn sent_by(mut event: phoebis_bus::Event, class: &str) -> phoebis_bus::Event {
    if let phoebis_bus::Event::RequestObserved(observed) = &mut event {
        observed.client.class = class.into();
    }
    event
}

/// A kept-warm credit keys the client of the request it credits, so its saving prices under that client's regime as
/// the request's own row does: a turn's credit takes the turn's own client over the one the turn before it named, and
/// a compaction's credit takes the compaction's own.
#[tokio::test]
async fn a_kept_warm_credit_keys_the_client_of_the_request_it_credits() {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    open_request_as(&r, "first", "claude-code/desk", 0, 2_000);
    r.engine.observe(completed(&r, "first", 0));
    r.advance_attended(270_000);
    r.step().await;
    // Past each request's five minutes and inside the entry the probe after it kept.
    r.advance_attended(60_000);
    open_request_as(&r, "second", "claude-code/cli", 2_000, 0);
    r.engine.observe(completed(&r, "second", 2_000));
    r.advance_attended(270_000);
    r.step().await;
    assert_eq!(
        r.status("main").probes,
        2,
        "a probe kept the second request's entry"
    );
    r.advance_attended(60_000);
    r.engine.observe(sent_by(
        r.observed_with(
            "compaction",
            "main",
            Some("compaction"),
            None,
            &[],
            BODY.as_bytes(),
            "claude-sonnet-4-5",
        ),
        "claude-code/pane",
    ));
    r.engine.observe(r.selected("compaction", "a"));
    r.engine.observe(message_start(
        "compaction",
        phoebis_bus::Usage {
            cache_read_tokens: Some(2_000),
            ..Default::default()
        },
    ));
    r.engine.observe(completed(&r, "compaction", 2_000));
    let rows = final_rollups(&r).await;
    let mut credits: Vec<(String, u64)> = rows
        .iter()
        .filter(|row| row.kept_warm_requests > 0)
        .map(|row| (row.client.clone(), row.kept_warm_requests))
        .collect();
    credits.sort();
    assert_eq!(
        credits,
        [
            ("claude-code/cli".to_string(), 1),
            ("claude-code/pane".to_string(), 1)
        ],
        "each credit keys the client of the request it credits"
    );
}

/// A chain the idle limit wrote off stays pending until its last entry lapses: the ceiling ends the recipe 1,080 s in
/// with three probes pending, a tick 1,090 s in, past the first two probes' own entries, settles none of them, and a
/// turn 1,100 s in, inside the last probe's entry, that reads the cache answers all three.
#[tokio::test]
async fn a_chain_the_idle_limit_wrote_off_waits_for_its_last_entry_and_a_turn_inside_it_answers_every_probe(
) {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        policy: KeepalivePolicy {
            max_idle_seconds: 1_000,
            ..RigSpec::default().policy
        },
        ..RigSpec::default()
    })
    .await;
    open_request_as(&r, "first", OTHER_CLIENT, 0, 2_000);
    r.engine.observe(completed(&r, "first", 0));
    for _ in 0..3 {
        r.advance(270_000);
        r.step().await;
    }
    let status = r.status("main");
    assert_eq!(
        (status.probes, status.cold),
        (3, 0),
        "the three probes read the entry back: {status:?}"
    );
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        (r.status("main").reason, r.status("main").unanswered),
        (Reason::IdleLimit, 3),
        "the ceiling kept the chain's three probes pending"
    );
    r.advance(10_000);
    r.step().await;
    assert_eq!(
        r.status("main").unanswered,
        3,
        "the tick 1,090 s in settled none of the chain's probes"
    );
    // The sweep's settlements reach the ledger with the save after them.
    r.engine.flush_persisted().await;
    r.advance(10_000);
    open_request_as(&r, "second", OTHER_CLIENT, 2_000, 0);
    r.engine.observe(completed(&r, "second", 2_000));
    let rows = final_rollups(&r).await;
    assert_eq!(
        (
            inference_rows(&rows).kept_warm_requests,
            probe_rows(&rows).wasted_probe_reasons
        ),
        (1, BTreeMap::new()),
        "the turn arrived 1,100 s in, before the chain's last entry ended at 1,110 s"
    );
}

/// A later probe that did not cover the entry never shadows an older covered one whose entry still lives. The first
/// probe reads the entry back 270 s in and keeps it to 570 s, the second reads a hundred tokens of the prefix 540 s in
/// and falls short of it, and a request 550 s in that reads the cache is credited on the first probe's coverage.
#[tokio::test]
async fn a_later_probe_that_did_not_cover_the_entry_leaves_the_credit_to_the_older_covered_probe() {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    open_request_as(&r, "first", OTHER_CLIENT, 0, 2_000);
    r.engine.observe(completed(&r, "first", 0));
    r.advance_attended(270_000);
    r.step().await;
    r.up.push_credential_rule(A, "/v1/messages", probe_answer(100, 1_900));
    r.advance_attended(270_000);
    r.step().await;
    let status = r.status("main");
    assert_eq!(
        (status.probes, status.unanswered, status.reason),
        (2, 2, Reason::PartialCacheCoverage),
        "the second probe fell short of the entry 540 s in: {status:?}"
    );
    r.advance_attended(10_000);
    open_request_as(&r, "second", OTHER_CLIENT, 2_000, 0);
    r.engine.observe(completed(&r, "second", 2_000));
    let rows = final_rollups(&r).await;
    assert_eq!(
        (
            inference_rows(&rows).kept_warm_requests,
            probe_rows(&rows).wasted_probe_reasons
        ),
        (1, BTreeMap::new()),
        "the request inside the first probe's entry is credited on its coverage"
    );
}

/// A later probe that read only part of the prefix found the entry gone in part and wrote a new one, so it starts a
/// chain of its own. The first probe reads the entry back 270 s in and keeps it to 570 s, the second reads a hundred
/// tokens of the prefix 540 s in and keeps its own entry to 840 s, and a request 600 s in that reads the cache arrives
/// past the first probe's entry, which settles it as expired, and inside the second's, which answers it.
#[tokio::test]
async fn a_later_probe_that_read_part_of_the_prefix_starts_a_chain_of_its_own_so_the_probe_before_it_settles_by_its_own_entry(
) {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    open_request_as(&r, "first", OTHER_CLIENT, 0, 2_000);
    r.engine.observe(completed(&r, "first", 0));
    r.advance_attended(270_000);
    r.step().await;
    let first_cost =
        probe_rows(&rollups(&r, |rows| tokens(probe_rows(rows).usage()) > 0).await).usage();
    r.up.push_credential_rule(A, "/v1/messages", probe_answer(100, 1_900));
    r.advance_attended(270_000);
    r.step().await;
    let status = r.status("main");
    assert_eq!(
        (status.probes, status.unanswered, status.reason),
        (2, 2, Reason::PartialCacheCoverage),
        "the second probe fell short of the entry 540 s in: {status:?}"
    );
    r.advance_attended(60_000);
    open_request_as(&r, "second", OTHER_CLIENT, 2_000, 0);
    r.engine.observe(completed(&r, "second", 2_000));
    let rows = final_rollups(&r).await;
    let probe = probe_rows(&rows);
    assert_eq!(
        (
            inference_rows(&rows).kept_warm_requests,
            probe.wasted_probes,
            probe.wasted_probe_reasons.clone()
        ),
        (0, 1, BTreeMap::from([("expired".to_string(), 1)])),
        "the request arrived 600 s in, past 570 s and before 840 s"
    );
    assert!(
        probe.wasted_usage() == first_cost,
        "the first probe is the wasted one: {first_cost:?} against {probe:?}"
    );
}

/// A session that lapses is swept by the expiry the next observation runs, and nothing after it on that path
/// reports: no selection, no completion, no tick. The settlement the sweep leaves has to reach the ledger from
/// the observation itself, stamped when the sweep happened rather than whenever the next caller drains.
#[tokio::test]
async fn an_expiry_swept_by_an_observation_settles_from_that_observation() {
    let mut r = rig(RigSpec {
        session_expiry: Duration::from_secs(600),
        ..RigSpec::default()
    })
    .await;
    let body = priced_body();
    r.observe_with("main", &body).await;
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 1, "one probe nobody came back for");
    r.advance(400_000);
    // A bare observation: the engine sees the request and nothing else about it, so only the sweep this call
    // runs can settle the recipe it drops.
    r.engine.observe(r.observed("later", "main", &body));
    let rows = rollups(&r, |rows| probe_rows(rows).wasted_probes == 1).await;
    let probe = probe_rows(&rows);
    assert_eq!(
        probe.wasted_probe_reasons.get("session_ended").copied(),
        Some(1)
    );
    assert!(
        tokens(probe.wasted_usage()) > 0,
        "the probe the lapsed session paid for: {probe:?}"
    );
}

/// A sweep run outside a tick settles from that call: `expire`, which every reader runs first, and a report, which
/// sweeps before it applies, reach `Inner::expire` without a tick behind them, and what they queue has to land while
/// the ledger is still up rather than wait for a `step` that a shutdown may never run.
#[tokio::test]
async fn an_expiry_swept_by_a_reader_settles_from_that_call() {
    for sweep in ["expire", "report"] {
        let mut r = rig(RigSpec {
            session_expiry: Duration::from_secs(600),
            ..RigSpec::default()
        })
        .await;
        let body = priced_body();
        r.observe_with("main", &body).await;
        r.advance(270_000);
        r.step().await;
        assert_eq!(r.status("main").probes, 1, "{sweep}");
        r.advance(400_000);
        match sweep {
            "expire" => r.engine.expire(),
            _ => {
                // The sweep ends the session the report names, so the report itself is refused after it.
                let refused = r.event(Event {
                    event: "heartbeat".into(),
                    ..Default::default()
                });
                assert!(refused.is_err(), "{refused:?}");
            }
        }
        let rows = rollups(&r, |rows| probe_rows(rows).wasted_probes == 1).await;
        let probe = probe_rows(&rows);
        assert_eq!(
            probe.wasted_probe_reasons.get("session_ended").copied(),
            Some(1),
            "{sweep}: {probe:?}"
        );
        assert!(tokens(probe.wasted_usage()) > 0, "{sweep}: {probe:?}");
    }
}

/// A rig whose first probe of main the upstream has received and holds until `release`, its sessions expiring after
/// `session_expiry`.
async fn a_paid_probe_held_upstream(session_expiry: Duration) -> Rig {
    let mut r = rig(RigSpec {
        session_expiry,
        ..RigSpec::default()
    })
    .await;
    r.up.push_credential_rule(A, "/v1/messages", cold_answer());
    r.observe_with("main", &priced_body()).await;
    r.up.push_credential_rule(
        A,
        "/v1/messages",
        Respond::Gated(Box::new(probe_answer(2_000, 0))),
    );
    r.advance_attended(270_000);
    r.engine.step().await;
    wait_for(Duration::from_secs(5), || {
        (r.up.requests().len() == 2).then_some(())
    })
    .await;
    r
}

/// Lets the held probe's answer through and returns the rollups once its counts landed: the upstream served it, so
/// it is paid for whatever the fence said, and a result whose recipe had stopped settles as one wasted probe under
/// that stop.
async fn served_probe_rows(r: &Rig) -> Vec<RollupRow> {
    r.up.release();
    r.engine.wait_idle().await;
    rollups(r, |rows| tokens(probe_rows(rows).usage()) > 0).await
}

#[tokio::test]
async fn a_late_or_orphaned_probe_is_still_paid_for() {
    let day = RigSpec::default().session_expiry;
    // A restart cancels the exchange before any answer, so nothing was served and no recipe is left for the probe
    // to be wasted by. The ledger is drained rather than polled, since what this case asserts is that no row lands
    // at all.
    let r = a_paid_probe_held_upstream(day).await;
    r.engine.close();
    r.up.release();
    r.engine.wait_idle().await;
    let probe = probe_rows(&final_rollups(&r).await);
    assert_eq!(
        (probe.wasted_probes, tokens(probe.usage())),
        (0, 0),
        "close: {probe:?}"
    );

    // The expiry case sweeps the session out from under the probe in flight, past the 600-second expiry from the
    // last heartbeat.
    let r = a_paid_probe_held_upstream(Duration::from_secs(600)).await;
    r.advance(600_000);
    r.engine.expire();
    let rows = served_probe_rows(&r).await;
    let probe = probe_rows(&rows);
    assert!(read_dollars(&r, &rows).0 > 0.0, "expire");
    assert_eq!(
        probe.wasted_probe_reasons.get("session_ended").copied(),
        Some(1),
        "expire: {probe:?}"
    );

    let mut r = a_paid_probe_held_upstream(day).await;
    r.up.push_credential_rule(A, "/v1/messages", cold_answer());
    r.observe_with("main", &priced_body()).await;
    let rows = served_probe_rows(&r).await;
    let probe = probe_rows(&rows);
    assert!(read_dollars(&r, &rows).0 > 0.0, "new_request");
    assert_eq!(
        probe.wasted_probes, 0,
        "new_request: the request answered it"
    );

    let mut r = a_paid_probe_held_upstream(day).await;
    r.state("main", "released");
    let rows = served_probe_rows(&r).await;
    let probe = probe_rows(&rows);
    assert!(read_dollars(&r, &rows).0 > 0.0, "release");
    assert_eq!(
        probe.wasted_probe_reasons.get("released").copied(),
        Some(1),
        "release: {probe:?}"
    );

    let r = a_paid_probe_held_upstream(day).await;
    r.engine
        .apply_event(
            "desktop",
            r.generation("desktop"),
            Event {
                event: "session_start".into(),
                session_id: "session".into(),
                instance_id: "second".into(),
                sequence: 1,
                previous_instance_id: Some("instance".into()),
                ..Default::default()
            }
            .try_into()
            .unwrap(),
        )
        .unwrap();
    let rows = served_probe_rows(&r).await;
    let probe = probe_rows(&rows);
    assert!(read_dollars(&r, &rows).0 > 0.0, "replace");
    assert_eq!(
        probe.wasted_probe_reasons.get("replaced").copied(),
        Some(1),
        "replace: {probe:?}"
    );
}

/// A probe's answer reading the 2,000-token prefix back, served fast.
fn fast_read_back() -> Respond {
    Respond::Json { status: 200, headers: vec![], body: br#"{"id":"msg_p","type":"message","content":[],"usage":{"input_tokens":0,"cache_read_input_tokens":2000,"cache_creation_input_tokens":0,"output_tokens":0,"speed":"fast"}}"#.to_vec() }
}

/// A fast session's cache lives on the fast lane, so a fast probe keeps a fast request warm and nothing on the
/// standard lane.
#[tokio::test]
async fn a_fast_probe_keeps_a_fast_request_warm_and_a_switch_of_speed_earns_nothing() {
    let mut r = rig(RigSpec::default()).await;
    let fast = opus_body().replacen('{', r#"{"speed":"fast","#, 1);
    r.up.push_credential_rule(A, "/v1/messages", cold_answer());
    r.observe_with("main", &fast).await;
    r.up.push_credential_rule(A, "/v1/messages", fast_read_back());
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 1);
    r.advance(60_000);
    r.up.push_credential_rule(A, "/v1/messages", fast_read_back());
    let kept = r.observe_with("main", &fast).await;
    let rows = rollups(&r, |rows| inference_rows(rows).kept_warm_requests == 1).await;
    let mut on_fast = RollupRow::key(
        0,
        "",
        0,
        "",
        "",
        "",
        "inference",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
    );
    for row in rows
        .iter()
        .filter(|row| row.kind == "inference" && row.speed == "fast")
    {
        on_fast.add(row);
    }
    assert_eq!(
        (
            on_fast.kept_warm_requests,
            on_fast.kept_warm_5m_tokens,
            on_fast.kept_warm_1h_tokens
        ),
        (1, 2_000, 0),
        "the credit files under speed fast"
    );
    let probe_speeds: Vec<&str> = rows
        .iter()
        .filter(|row| row.kind == "probe")
        .map(|row| row.speed.as_str())
        .collect();
    assert!(
        !probe_speeds.is_empty() && probe_speeds.iter().all(|speed| *speed == "fast"),
        "and the probe that kept it under speed fast too: {probe_speeds:?}"
    );
    assert!(row(&r, &kept).await.kept_warm);

    r.up.push_credential_rule(A, "/v1/messages", fast_read_back());
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 2);
    r.advance(60_000);
    let switched = r.observe_with("main", &opus_body()).await;
    // The next probe's delta reaches the ledger behind any credit the switched request earned.
    r.advance(270_000);
    r.step().await;
    let rows = rollups(&r, |rows| probe_rows(rows).requests >= 3).await;
    assert_eq!(
        inference_rows(&rows).kept_warm_requests,
        1,
        "a standard request reads no cache the fast probe kept, whatever it reads back"
    );
    assert!(!row(&r, &switched).await.kept_warm);
}

/// The upstream may serve a request asked fast at standard speed. The credit reads the recipe's speed, the lane the
/// fast probe kept, so the request is kept warm whatever speed its answer reports.
#[tokio::test]
async fn a_request_asked_fast_and_served_standard_is_kept_warm() {
    let mut r = rig(RigSpec::default()).await;
    let fast = opus_body().replacen('{', r#"{"speed":"fast","#, 1);
    r.up.push_credential_rule(A, "/v1/messages", cold_answer());
    r.observe_with("main", &fast).await;
    r.up.push_credential_rule(A, "/v1/messages", fast_read_back());
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 1);
    r.advance(60_000);
    r.up.push_credential_rule(A, "/v1/messages", warm_answer());
    let kept = r.observe_with("main", &fast).await;
    rollups(&r, |rows| inference_rows(rows).kept_warm_requests == 1).await;
    assert!(row(&r, &kept).await.kept_warm);
}

#[tokio::test]
async fn the_canonical_model_prices_a_probe_whatever_the_caller_spelled() {
    // A caller that asks for `Claude-Sonnet-5[1m]` is served under `claude-sonnet-5`: the replay keeps the
    // spelling, and the cost and the rollup key take the canonical id.
    let mut r = rig(RigSpec::default()).await;
    let spelled = BODY.replace("claude-sonnet-4-5", "Claude-Sonnet-5[1m]");
    r.up.push_credential_rule(A, "/v1/messages", cold_answer());
    r.observe_with("main", &spelled).await;
    assert_eq!(r.status("main").canonical_model, "claude-sonnet-5");
    r.advance(270_000);
    r.step().await;
    let probe = r.up.requests().into_iter().last().unwrap();
    assert!(
        std::str::from_utf8(&probe.body)
            .unwrap()
            .contains(r#""model":"claude-sonnet-5""#),
        "the relay's edit resolved the served id"
    );
    let rows = rollups(&r, |rows| tokens(probe_rows(rows).usage()) > 0).await;
    let row = rows.iter().find(|row| row.kind == "probe").unwrap();
    assert_eq!(row.canonical_model, "claude-sonnet-5");
    let expected = 2_000.0
        * rig_cells("claude-sonnet-5", phoebis_ledger::Speed::Standard).cache_read
        / 1_000_000.0;
    let (cost, _) = read_dollars(&r, &rows);
    assert!(
        (cost - expected).abs() < 1e-12,
        "priced as the canonical model: {cost}"
    );
}

#[tokio::test]
async fn a_kept_warm_request_on_an_unpriced_model_counts_and_its_recipe_keeps_probing() {
    let mut r = rig(RigSpec::default()).await;
    let body = unpriced_body();
    r.up.push_credential_rule(A, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 1);
    r.advance(60_000);
    let second = r.observe_with("main", &body).await;
    let rows = rollups(&r, |rows| inference_rows(rows).kept_warm_requests == 1).await;
    let credit = inference_rows(&rows);
    assert_eq!(
        (
            credit.kept_warm_requests,
            credit.kept_warm_5m_tokens,
            credit.kept_warm_1h_tokens
        ),
        (1, 2_000, 0),
        "the mark and the tokens count on a model the price table does not know"
    );
    assert!(row(&r, &second).await.kept_warm);
    for _ in 0..4 {
        r.advance(270_000);
        r.step().await;
    }
    let status = r.status("main");
    assert_eq!(status.probes, 5, "{status:?}");
}

/// The standing answer's 2,000 cached tokens read back at the dearest cache read of the table the rig prices
/// through: what a probe on a model the table does not list costs.
fn dearest_read_2000() -> f64 {
    let dearest = phoebis_ledger::PriceRevisions::shipped()
        .in_force(NOW)
        .rate_table()
        .fallback;
    2_000.0 * dearest.cache_read.expect("the fallback prices every type")
}

#[tokio::test]
async fn an_unpriced_model_costs_the_dearest_cells_and_still_lands_a_probe_row() {
    let mut r = rig(RigSpec::default()).await;
    r.observe_with("main", &unpriced_body()).await;
    r.advance(270_000);
    r.step().await;
    let rows = rollups(&r, |rows| probe_rows(rows).requests == 1).await;
    let (cost, _) = read_dollars(&r, &rows);
    assert!(
        (cost - dearest_read_2000()).abs() < 1e-12,
        "claude-opus-4-7 is not in the shipped table, so its probe row prices at the dearest cells: {cost}"
    );
    assert_eq!(r.status("main").unanswered, 1);
}

/// A key moved to another pool takes its probes with it: the cost the probe's ledger row carries belongs to the pool
/// that serves the probe, not to the one the last real request selected.
#[tokio::test]
async fn a_key_moved_to_another_pool_is_charged_there() {
    let mut r = rig(RigSpec::default()).await;
    let body = priced_body();
    r.up.push_credential_rule(A, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    for _ in 0..2 {
        r.advance(270_000);
        r.step().await;
    }
    assert_eq!(r.status("main").probes, 2);
    r.h.store
        .insert_pool(
            &phoebis_store::PoolRow {
                strategy: phoebis_store::Strategy::Priority,
                keepalive_json: serde_json::to_string(&RigSpec::default().policy).unwrap(),
                prefer_subscription: false,
                created_at: NOW,
                updated_at: NOW,
                ..phoebis_store::PoolRow::for_tests("warm")
            },
            "test",
        )
        .unwrap();
    // An account belongs to one pool, so `a` leaves `main` before `warm` takes it.
    r.h.store.set_pool_members("main", &[], "test").unwrap();
    r.h.store
        .set_pool_members(
            "warm",
            &[phoebis_store::MemberWrite {
                account_id: "a".to_string(),
                priority: 0,
                budget_daily_usd: None,
                budget_monthly_usd: None,
            }],
            "test",
        )
        .unwrap();
    let mut key = r.h.store.api_key("desktop").unwrap().unwrap();
    key.pool_id = "warm".into();
    r.h.store
        .update_api_key(&key, key.updated_at, "test")
        .unwrap();
    r.h.router.reload().await.unwrap();
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        (r.status("main").probes, r.status("main").reason),
        (3, Reason::Scheduled)
    );
    assert_eq!(
        r.engine.recipes().first().map(|v| v.pool_id.clone()),
        Some("warm".to_string()),
        "the recipe moved with its key, without another request"
    );
    // The probe's own request row carries the counts its charge prices from.
    let rows = rollups(&r, |rows| {
        rows.iter()
            .any(|row| row.kind == "probe" && row.pool_id == "warm" && tokens(row.usage()) > 0)
    })
    .await;
    assert!(rows
        .iter()
        .any(|row| row.pool_id == "warm" && tokens(row.usage()) > 0));
}

/// A probe the upstream served and charged for is on the recipe's own books before any branch that pauses
/// it: the settlement a release leaves would otherwise disagree with the ledger.
#[tokio::test]
async fn a_probe_answered_with_output_is_settled_as_wasted() {
    let mut r = rig(RigSpec::default()).await;
    let body = priced_body();
    r.up.push_credential_rule(A, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    // A 2xx answer with valid usage that generated output: paid for, and no evidence about the entry.
    r.up.push_credential_rule(A, "/v1/messages", Respond::Json { status: 200, headers: vec![], body: br#"{"id":"m","type":"message","content":[{"type":"text","text":"ok"}],"usage":{"input_tokens":0,"cache_read_input_tokens":2000,"output_tokens":1}}"#.to_vec() });
    r.advance_attended(270_000);
    r.step().await;
    let status = r.status("main");
    assert_eq!(
        (status.reason, status.unanswered),
        (Reason::UnexpectedOutput, 1),
        "{status:?}"
    );
    let rows = rollups(&r, |rows| tokens(probe_rows(rows).usage()) > 0).await;
    let cost = probe_rows(&rows).usage();
    r.state("main", "released");
    let rows = rollups(&r, |rows| probe_rows(rows).wasted_probes == 1).await;
    let probe = probe_rows(&rows);
    assert_eq!(
        probe.wasted_probe_reasons.get("released").copied(),
        Some(1),
        "{probe:?}"
    );
    assert!(
        probe.wasted_usage() == cost,
        "settled as wasted at what it used: {probe:?}"
    );
}

/// Coverage is the prefix read back, not reads plus writes: a probe that read a hundred tokens of a two
/// thousand token prefix and rebuilt the rest never warmed it, so it is partial coverage, it counts against
/// the cold streak, and the request that follows earns no credit.
#[tokio::test]
async fn a_probe_that_rebuilt_most_of_the_prefix_covered_nothing() {
    let mut r = rig(RigSpec::default()).await;
    let body = priced_body();
    r.up.push_credential_rule(A, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    r.up.push_credential_rule(A, "/v1/messages", probe_answer(100, 1_900));
    r.advance(270_000);
    r.step().await;
    let status = r.status("main");
    assert_eq!(
        (
            status.probes,
            status.reason,
            status.cold,
            status.next_probe_at
        ),
        (1, Reason::PartialCacheCoverage, 1, None),
        "{status:?}"
    );
    // Past the TTL of the last real request; the credited case of the same gap is
    // `a_request_after_an_expiry_gap_with_a_covering_probe_is_kept_warm`.
    r.advance(60_000);
    let next = r.observe_with("main", &body).await;
    assert!(
        !row(&r, &next).await.kept_warm,
        "a partial read is no coverage to credit"
    );
}

/// A warm answer for a real request: the prefix read back, with a little output.
fn warm_answer() -> Respond {
    Respond::Json { status: 200, headers: vec![], body: br#"{"id":"m","type":"message","content":[{"type":"text","text":"ok"}],"usage":{"input_tokens":10,"cache_read_input_tokens":2000,"output_tokens":10}}"#.to_vec() }
}

/// The kept-warm credit belongs to the account the covering probe warmed. A request that failed over reads a
/// prefix ordinary traffic cached on the other account, which no probe of this recipe paid for.
#[tokio::test]
async fn a_request_that_failed_over_earns_no_credit_from_the_probes_account() {
    let mut r = rig(RigSpec::default()).await;
    let body = priced_body();
    r.up.push_credential_rule(A, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    assert_eq!(r.status("main").account_id.as_deref(), Some("a"));
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 1, "one covering probe on a");
    // Past the TTL the request meets a 429 on a and reads the cache on b.
    r.advance(60_000);
    r.up.push_credential_rule(
        A,
        "/v1/messages",
        relay_common::json_429_unified(4_600, "five_hour"),
    );
    r.up.push_credential_rule(B, "/v1/messages", warm_answer());
    let elsewhere = r.observe_with("main", &body).await;
    assert_eq!(r.status("main").account_id.as_deref(), Some("b"));
    assert!(
        !row(&r, &elsewhere).await.kept_warm,
        "no probe of this recipe warmed b"
    );
    // The same gap and the same covering probe, with the request served by the account the probe warmed.
    let mut r = rig(RigSpec::default()).await;
    r.up.push_credential_rule(A, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    r.advance(270_000);
    r.step().await;
    r.advance(60_000);
    r.up.push_credential_rule(A, "/v1/messages", warm_answer());
    let here = r.observe_with("main", &body).await;
    assert_eq!(r.status("main").account_id.as_deref(), Some("a"));
    let rows = rollups(&r, |rows| inference_rows(rows).kept_warm_requests == 1).await;
    let credit = inference_rows(&rows);
    assert_eq!(
        (credit.kept_warm_5m_tokens, credit.kept_warm_1h_tokens),
        (2_000, 0)
    );
    assert!(row(&r, &here).await.kept_warm);
}

/// A probe answer that reports only the aggregate cache-write field is charged for that write: the relay's
/// own parser reads the field as five-minute writes, and the probe's cost has to see the same charge.
#[tokio::test]
async fn an_aggregate_only_cache_write_is_charged_as_a_five_minute_write() {
    let mut r = rig(RigSpec::default()).await;
    let body = priced_body();
    r.up.push_credential_rule(A, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    r.up.push_credential_rule(A, "/v1/messages", Respond::Json { status: 200, headers: vec![], body: br#"{"id":"m","type":"message","content":[],"usage":{"input_tokens":0,"cache_read_input_tokens":2000,"cache_creation_input_tokens":500,"output_tokens":0}}"#.to_vec() });
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 1);
    // Sonnet 5: 2,000 read at its cache read and 500 written at its five-minute write.
    let cells = rig_cells(PRICED, phoebis_ledger::Speed::Standard);
    let expected = (2_000.0 * cells.cache_read + 500.0 * cells.cache_write_5m) / 1_000_000.0;
    let rows = rollups(&r, |rows| tokens(probe_rows(rows).usage()) > 0).await;
    let (cost, _) = read_dollars(&r, &rows);
    assert!((cost - expected).abs() < 1e-12, "{cost} against {expected}");
}

/// A title or a summary reads the cache but never re-armed the recipe, so it earns nothing; the next request
/// that continues the conversation reads the same cache and earns the credit the auxiliary one could not.
#[tokio::test]
async fn the_credit_follows_only_a_re_arming_request() {
    let mut r = rig(RigSpec::default()).await;
    let body = priced_body();
    r.up.push_credential_rule(A, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 1, "one covering probe");
    r.advance(60_000);
    r.up.push_credential_rule(A, "/v1/messages", warm_answer());
    let mut auxiliary = r.request_with("main", &body);
    relay_common::set_header(&mut auxiliary, "x-claude-code-request-class", "title");
    let resp = r.h.app.clone().oneshot(auxiliary).await.unwrap();
    assert_eq!(resp.status(), 200);
    let aux_id = relay_common::outcome(&resp).request_id;
    let _ = resp.into_body().collect().await.unwrap();
    r.pump(Some(&aux_id)).await;
    assert!(
        !row(&r, &aux_id).await.kept_warm,
        "an auxiliary request never re-armed, so it earns nothing"
    );
    r.up.push_credential_rule(A, "/v1/messages", warm_answer());
    let credited = r.observe_with("main", &body).await;
    rollups(&r, |rows| inference_rows(rows).kept_warm_requests == 1).await;
    assert!(
        row(&r, &credited).await.kept_warm,
        "the next continuing request earns the credit the auxiliary one left behind"
    );
}

const OAUTH_SECRET: &str = "sk-ant-oat01-OOOO";

/// A subscription account on plan quota under a tier the shipped tables price, so its standard probes on a model they
/// weigh settle in a share of the 7-day window.
fn plan_funded_spec() -> RigSpec {
    RigSpec {
        accounts: vec![oauth("o", OAUTH_SECRET, 0)],
        pool: PoolSpec {
            overage_policy: phoebis_store::OveragePolicy::Allow,
            ..PoolSpec::default()
        },
        tier: Some("default_claude_max_5x"),
        ..RigSpec::default()
    }
}

/// A probe on plan quota settles in a window share where an api_key account's settles in dollars, and the request
/// it kept warm is credited all the same.
#[tokio::test]
async fn a_request_kept_warm_on_a_plan_funded_account_is_credited() {
    let mut r = rig(plan_funded_spec()).await;
    let body = opus_body();
    r.up.push_credential_rule(OAUTH_SECRET, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 1);
    let (_, agents) = r.engine.persist_rows();
    let payload: serde_json::Value = serde_json::from_str(&agents[0].payload).unwrap();
    assert_eq!(
        payload["unanswered"][0]["unit"].as_str(),
        Some("window_share"),
        "the probe settled in a share of the window: {}",
        payload["unanswered"]
    );
    assert_eq!(
        payload["unanswered"][0]["window"].as_str(),
        Some("seven_day"),
        "the share is of the 7-day window, which every plan keeps: {}",
        payload["unanswered"]
    );
    r.advance(60_000);
    r.up.push_credential_rule(OAUTH_SECRET, "/v1/messages", warm_answer());
    let kept = r.observe_with("main", &body).await;
    let rows = rollups(&r, |rows| inference_rows(rows).kept_warm_requests == 1).await;
    let credit = inference_rows(&rows);
    assert_eq!(
        (credit.kept_warm_5m_tokens, credit.kept_warm_1h_tokens),
        (2_000, 0)
    );
    assert!(row(&r, &kept).await.kept_warm);
}

/// The Pro-week figure the wasted probes of `rows` spent on account `o`, as the analytics answer it under the account
/// grouping, `None` where a wasted count priced nothing.
fn wasted_probe_figure(r: &Rig, rows: &[RollupRow]) -> Option<f64> {
    phoebis_ledger::breakdown(
        rows,
        phoebis_ledger::GroupBy::Account,
        true,
        &phoebis_ledger::LiveKeys::default(),
        &[],
        &r.pricing,
        &r.tables,
        r.now(),
    )
    .into_iter()
    .find(|group| group.key == "o")?
    .probe_equivalent
    .map(|probe| probe.wasted)
}

/// The tier the settlement of the first probe a plan-funded account on the Pro tier leaves unanswered files under, its
/// stored profile naming `organization_type`, on the suite's own cost tables, and the Pro-week figure the settlement's
/// counts price at. The operator's release settles the probe as wasted.
async fn probe_on_the_pro_tier_of(organization_type: &str) -> (String, Option<f64>) {
    let mut r = rig(RigSpec {
        tables: phoebis_ledger::test_tables::subscription_costs(),
        tier: Some("default_claude_ai"),
        ..plan_funded_spec()
    })
    .await;
    let document = serde_json::json!({
        "organization": {"organization_type": organization_type, "rate_limit_tier": "default_claude_ai"},
    });
    r.h.store
        .set_profile(
            "o",
            &document.to_string(),
            &phoebis_store::SubscriptionRow {
                tier: Some("default_claude_ai".into()),
                ..phoebis_store::SubscriptionRow::default()
            },
        )
        .unwrap();
    r.h.router.reload().await.unwrap();
    let body = opus_body();
    r.up.push_credential_rule(OAUTH_SECRET, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 1, "{organization_type}");
    r.engine.release("desktop", "session").unwrap();
    let rows = final_rollups(&r).await;
    let settled = rows
        .iter()
        .find(|row| row.wasted_probes > 0)
        .expect("the settlement's row");
    (settled.tier.clone(), wasted_probe_figure(&r, &rows))
}

/// A free organization reports the Pro tier and its plan prices no capacity, so its probe files under no tier, as its
/// request row does, while a Pro organization's files under the stored tier. The Pro-week figure reads no account tier,
/// so both wasted probes price one figure: the probe read the 2,000-token Opus 5.5 prefix back, 2,000 x 2 x 0.1 = 400
/// units over the Pro tier's 50 on the week of the test table.
#[tokio::test]
async fn a_free_organizations_probe_files_under_no_tier_beside_a_pro_ones_under_its_tier() {
    assert_eq!(
        (
            probe_on_the_pro_tier_of("claude_free").await,
            probe_on_the_pro_tier_of("claude_pro").await,
        ),
        (
            (UNATTRIBUTED.to_string(), Some(8.0)),
            ("default_claude_ai".to_string(), Some(8.0)),
        ),
        "the free organization's probe files under no tier, the Pro one's under its tier, both at one figure"
    );
}

/// A usage-based seat's kept-warm credit files under no tier, as the seat's request rows do, so the seat's rows open no
/// share map.
#[tokio::test]
async fn a_usage_based_seats_kept_warm_credit_files_under_no_tier_as_its_request_rows_do() {
    let mut r = rig(RigSpec {
        tier: Some("default_claude_zero"),
        ..plan_funded_spec()
    })
    .await;
    let body = opus_body();
    r.up.push_credential_rule(OAUTH_SECRET, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 1);
    r.advance(60_000);
    r.up.push_credential_rule(OAUTH_SECRET, "/v1/messages", warm_answer());
    let kept = r.observe_with("main", &body).await;
    let rows = rollups(&r, |rows| inference_rows(rows).kept_warm_requests == 1).await;
    assert!(row(&r, &kept).await.kept_warm);
    let credit = rows
        .iter()
        .find(|row| row.kept_warm_requests > 0)
        .expect("the credit's row");
    assert_eq!(
        (credit.funding.as_str(), credit.tier.as_str()),
        (phoebis_ledger::USAGE_BASED_FUNDING, UNATTRIBUTED),
        "the seat's credit files under its own funding and no tier"
    );
    let tiers: BTreeSet<&str> = rows
        .iter()
        .filter(|row| row.kind == "inference")
        .map(|row| row.tier.as_str())
        .collect();
    assert_eq!(
        tiers,
        BTreeSet::from([UNATTRIBUTED]),
        "the credit sits beside the seat's request rows under no tier"
    );
}

/// A credit files beside its request's own row: a Haiku 5.5 request in January whose prompt passes that month's
/// long-context threshold, kept warm on an Ultra account, keys the credit under `long` and the account's tier. The test
/// cost model weighs no Haiku 5.5, and a model without a weight prices nothing, so the case weighs it as the model
/// weighs Haiku 4.5.
#[tokio::test]
async fn a_credit_keys_its_requests_tier_and_band() {
    const HAIKU: &str = "claude-haiku-5-5";
    let costs = phoebis_ledger::test_tables::SUBSCRIPTION_COSTS
        .replace(
            "      claude-haiku-4-5: 0.5\n",
            "      claude-haiku-4-5: 0.5\n      claude-haiku-5-5: 0.5\n",
        )
        .replace(
            "      claude-haiku-4-5: 1\n",
            "      claude-haiku-4-5: 1\n      claude-haiku-5-5: 1\n",
        )
        .replace(
            "      claude-haiku-4-5: haiku\n",
            "      claude-haiku-4-5: haiku\n      claude-haiku-5-5: haiku\n",
        );
    let mut r = rig(RigSpec {
        pricing: phoebis_ledger::test_tables::api_prices(),
        tables: phoebis_ledger::PlanTables {
            revisions: phoebis_ledger::CostRevisions::from_yaml(&costs)
                .expect("the test cost model with Haiku 5.5 weighed loads"),
            path: None,
            source: "test".to_string(),
        },
        tier: Some("default_claude_max_20x"),
        ..plan_funded_spec()
    })
    .await;
    r.advance(phoebis_ledger::test_tables::JANUARY_2026 - r.now());
    r.event(Event {
        event: "session_start".into(),
        ..Default::default()
    })
    .unwrap();
    let body = body_on(HAIKU);
    r.up.push_credential_rule(OAUTH_SECRET, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    r.advance_attended(270_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 1);
    r.advance_attended(60_000);
    r.up.push_credential_rule(OAUTH_SECRET, "/v1/messages", warm_answer());
    let kept = r.observe_with("main", &body).await;
    let rows = rollups(&r, |rows| inference_rows(rows).kept_warm_requests == 1).await;
    assert!(row(&r, &kept).await.kept_warm);
    let long = r.pricing.in_force(r.now()).models[HAIKU]
        .long_context
        .as_ref()
        .expect("the test list prices Haiku 5.5 by prompt length")
        .over;
    assert!(
        2_000 > long,
        "the request read back more than the threshold"
    );
    let credit = rows
        .iter()
        .find(|row| row.kept_warm_requests > 0)
        .expect("the credit's row");
    assert_eq!(
        (credit.band.as_str(), credit.tier.as_str()),
        ("long", "default_claude_max_20x"),
        "the credited request's band and the serving account's tier"
    );
}

/// A credit files under the tier its request row files under, the one the router prices the account by: the stored
/// term moves to another tier after the probe and the router loads nothing before the kept-warm request, so the
/// credit stays beside its request's row under the tier the router holds.
#[tokio::test]
async fn a_credit_files_under_its_requests_tier_while_the_stored_term_names_a_tier_the_router_has_not_loaded(
) {
    let mut r = rig(plan_funded_spec()).await;
    let body = opus_body();
    r.up.push_credential_rule(OAUTH_SECRET, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 1);
    name_the_tier(&r.h.store, "o", "default_claude_max_20x");
    r.advance(60_000);
    r.up.push_credential_rule(OAUTH_SECRET, "/v1/messages", warm_answer());
    let kept = r.observe_with("main", &body).await;
    let rows = rollups(&r, |rows| inference_rows(rows).kept_warm_requests == 1).await;
    assert!(row(&r, &kept).await.kept_warm);
    let credit = rows
        .iter()
        .find(|row| row.kept_warm_requests > 0)
        .expect("the credit's row");
    let tiers: BTreeSet<&str> = rows
        .iter()
        .filter(|row| row.kind == "inference")
        .map(|row| row.tier.as_str())
        .collect();
    assert_eq!(
        (credit.tier.as_str(), tiers),
        (
            "default_claude_max_5x",
            BTreeSet::from(["default_claude_max_5x"])
        ),
        "the credit files under the tier the router still holds, beside its request's row"
    );
}

/// A credit keys the hour and the band its request's own row keys, read at the relay's receipt: a Haiku 5.5 request
/// received a minute and a half before February, whose prompt passes January's long-context threshold and not
/// February's, and observed by the engine and answered after midnight, files its credit in the January hour under
/// `long` beside its request row, so its saving prices at the January long-context cells.
#[tokio::test]
async fn a_credit_answered_across_a_revision_boundary_files_at_its_requests_arrival() {
    const HAIKU: &str = "claude-haiku-5-5";
    const PROMPT: u64 = 1_510;
    let mut r = rig(RigSpec {
        pricing: phoebis_ledger::test_tables::api_prices(),
        ..RigSpec::default()
    })
    .await;
    let february = phoebis_ledger::effective_instant("2026-02-01").expect("a date");
    // The first request at 23:53 on January 31, its probe at 23:57:30 and the credited request at 23:58:30, the
    // session started again past the jump.
    r.advance(february - 420_000 - r.now());
    r.event(Event {
        event: "session_start".into(),
        ..Default::default()
    })
    .unwrap();
    let body = body_on(HAIKU);
    r.up.push_credential_rule(A, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    r.advance_attended(270_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 1);
    r.advance_attended(60_000);
    r.up.push_credential_rule(
        A,
        "/v1/messages",
        Respond::Gated(Box::new(Respond::Json { status: 200, headers: vec![], body: br#"{"id":"m","type":"message","content":[{"type":"text","text":"ok"}],"usage":{"input_tokens":10,"cache_read_input_tokens":1500,"output_tokens":10}}"#.to_vec() })),
    );
    let sent = r.up.requests().len();
    let app = r.h.app.clone();
    let request = r.request_with("main", &body);
    let pending = tokio::spawn(async move { app.oneshot(request).await.unwrap() });
    wait_for(Duration::from_secs(5), || {
        (r.up.requests().len() > sent).then_some(())
    })
    .await;
    let received = r.now();
    assert!(
        received < february,
        "the relay received the request on January 31"
    );
    // The engine observes the request only after midnight, so its own clock at the observation reads February.
    r.advance_attended(february + 60_000 - received);
    r.pump(None).await;
    r.up.release();
    let resp = pending.await.unwrap();
    assert_eq!(resp.status(), 200);
    let id = relay_common::outcome(&resp).request_id;
    let _ = http_body_util::BodyExt::collect(resp.into_body())
        .await
        .unwrap();
    r.pump(Some(&id)).await;
    assert_eq!(
        (
            r.pricing.in_force(received).band_of(HAIKU, PROMPT),
            r.pricing.in_force(r.now()).band_of(HAIKU, PROMPT)
        ),
        (
            phoebis_ledger::Band::LongContext,
            phoebis_ledger::Band::Standard
        ),
        "the prompt passes January's threshold and not February's"
    );
    let rows = rollups(&r, |rows| inference_rows(rows).kept_warm_requests == 1).await;
    let request = row(&r, &id).await;
    assert!(request.kept_warm);
    let credit = rows
        .iter()
        .find(|row| row.kept_warm_requests > 0)
        .expect("the credit's row");
    assert_eq!(
        credit.band, "long",
        "the credit keys the band of its request's arrival, not of its answer"
    );
    assert_eq!(
        (credit.hour, hour_of(request.received_at)),
        (hour_of(received), hour_of(received)),
        "the credit files in its request row's January hour, not in the hour of its answer"
    );
    // The answer read 1,500 tokens back on a five-minute entry, so the credit saved the write at the January
    // long-context cells less the read it paid.
    assert_eq!(
        (credit.kept_warm_5m_tokens, credit.kept_warm_1h_tokens),
        (1_500, 0)
    );
    let long = r.pricing.in_force(received).models[HAIKU]
        .long_context
        .as_ref()
        .expect("the test list prices Haiku 5.5 by prompt length")
        .clone();
    let january = 1_500.0 * (long.cells.cache_write_5m - long.cells.cache_read) / 1_000_000.0;
    let saving = phoebis_ledger::breakdown(
        &rows,
        phoebis_ledger::GroupBy::All,
        true,
        &phoebis_ledger::LiveKeys::default(),
        &[],
        &r.pricing,
        &r.tables,
        r.now(),
    )
    .first()
    .map_or(0.0, |group| group.saving_usd);
    assert!(
        (saving - january).abs() < 1e-12,
        "the saving at the January long-context cells: {saving} against {january}"
    );
}

/// A plan-funded probe on a model without a weight prices nothing whatever its model tier: the test table leaves Opus 5
/// null beside Opus 5.5 at 2, so the wasted probe's Pro-week figure reads null. The operator's release settles the
/// probe as wasted.
#[tokio::test]
async fn a_plan_funded_probe_on_a_model_without_a_weight_prices_no_figure() {
    let mut r = rig(RigSpec {
        tables: phoebis_ledger::test_tables::subscription_costs(),
        tier: Some("default_claude_max_20x"),
        ..plan_funded_spec()
    })
    .await;
    let body = body_on("claude-opus-5");
    r.up.push_credential_rule(OAUTH_SECRET, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 1);
    r.engine.release("desktop", "session").unwrap();
    let rows = final_rollups(&r).await;
    let settled = rows
        .iter()
        .find(|row| row.wasted_probes > 0)
        .expect("the settlement's row");
    assert_eq!(
        (
            settled.canonical_model.as_str(),
            settled.tier.as_str(),
            wasted_probe_figure(&r, &rows)
        ),
        ("claude-opus-5", "default_claude_max_20x", None),
        "the probe settled on the Ultra account and prices no Pro-week figure"
    );
}

const OPUS: &str = "claude-opus-5-5";

fn opus_body() -> String {
    body_on(OPUS)
}

fn body_on(model: &str) -> String {
    BODY.replace("claude-sonnet-4-5", model)
}

/// A settlement keys the probe's own exchange: a Haiku 5.5 probe over the first entry's threshold, exchanged on
/// January 31 and settled wasted after midnight, when the second entry raises the threshold above its prompt, files
/// its wasted counts in the January hour under `long` and the tier it ran under, so they price at the January
/// long-context cells.
#[tokio::test]
async fn a_long_context_probe_settled_wasted_the_next_day_files_under_its_own_hour_band_and_revision(
) {
    const OAUTH: &str = "sk-ant-oat01-OOOO";
    let mut r = rig(RigSpec {
        accounts: vec![oauth("o", OAUTH, 0)],
        pricing: phoebis_ledger::test_tables::api_prices(),
        tier: Some("default_claude_max_20x"),
        ..RigSpec::default()
    })
    .await;
    // 2026-01-31 at 23:50 UTC, the session started again past the jump.
    let late_january = 1_769_903_400_000;
    r.advance(late_january - r.now());
    r.event(Event {
        event: "session_start".into(),
        ..Default::default()
    })
    .unwrap();
    let body = BODY.replace("claude-sonnet-4-5", "claude-haiku-5-5");
    r.up.push_credential_rule(OAUTH, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    r.advance_attended(270_000);
    r.step().await;
    assert_eq!(r.status("main").unanswered, 1);
    let exchanged = r.now();
    let prices = r.pricing.clone();
    let february = phoebis_ledger::effective_instant("2026-02-01").expect("a date");
    assert!(exchanged < february, "the probe ran on January 31");
    r.advance_attended(600_000);
    assert!(r.now() >= february, "the settlement runs on February 1");
    r.state("main", "released");
    let rows = rollups(&r, |rows| probe_rows(rows).wasted_probes == 1).await;
    let settled: Vec<&RollupRow> = rows.iter().filter(|row| row.wasted_probes > 0).collect();
    assert_eq!(
        settled
            .iter()
            .map(|row| (row.hour, row.band.as_str(), row.tier.as_str()))
            .collect::<Vec<_>>(),
        [(hour_of(exchanged), "long", "default_claude_max_20x")],
        "the probe's hour, band and tier"
    );
    let long = prices.in_force(exchanged).models["claude-haiku-5-5"]
        .long_context
        .as_ref()
        .expect("the test list prices Haiku 5.5 by prompt length")
        .clone();
    assert!(
        2_000 > long.over,
        "the probe read back more than January's threshold"
    );
    let (_, wasted) = read_dollars(&r, &rows);
    assert!(
        (wasted - 2_000.0 * long.cells.cache_read / 1_000_000.0).abs() < 1e-12,
        "the January long-context cache read: {wasted}"
    );
}

/// A probe keys the relay's receipt of its request, the instant its request row files under: a Haiku 5.5 probe the
/// relay received a minute before midnight on January 31 and answered after it, when the second entry raises the
/// threshold above its prompt, files its request row and its settlement in the January hour under `long`, so its waste
/// prices at the January long-context cells its cost does.
#[tokio::test]
async fn a_probe_answered_across_a_revision_boundary_files_its_cost_and_its_waste_at_its_receipt() {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        pricing: phoebis_ledger::test_tables::api_prices(),
        ..RigSpec::default()
    })
    .await;
    const HAIKU: &str = "claude-haiku-5-5";
    let february = phoebis_ledger::effective_instant("2026-02-01").expect("a date");
    assert_eq!(
        r.pricing.in_force(february).band_of(HAIKU, 2_000),
        phoebis_ledger::Band::Standard,
        "February's threshold holds the probe's prompt"
    );
    // A request at 23:54:30 UTC on January 31, so its probe is due at 23:59, the session started again past the jump.
    r.advance(february - 330_000 - r.now());
    r.event(Event {
        event: "session_start".into(),
        ..Default::default()
    })
    .unwrap();
    let body = BODY.replace("claude-sonnet-4-5", HAIKU);
    r.up.push_credential_rule(A, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    r.up.push_credential_rule(
        A,
        "/v1/messages",
        Respond::Gated(Box::new(probe_answer(2_000, 0))),
    );
    r.advance_attended(270_000);
    r.engine.step().await;
    wait_for(Duration::from_secs(5), || {
        (r.up.requests().len() == 2).then_some(())
    })
    .await;
    let received = r.now();
    assert!(
        received < february,
        "the relay received the probe on January 31"
    );
    r.advance_attended(february + 30_000 - received);
    r.up.release();
    r.engine.wait_idle().await;
    r.pump(None).await;
    assert_eq!(
        r.status("main").unanswered,
        1,
        "the probe landed after midnight"
    );
    r.state("main", "released");
    let rows = rollups(&r, |rows| probe_rows(rows).wasted_probes == 1).await;
    let mut keyed: Vec<(i64, &str, u64, u64)> = rows
        .iter()
        .filter(|row| row.kind == "probe")
        .map(|row| (row.hour, row.band.as_str(), row.requests, row.wasted_probes))
        .collect();
    keyed.sort();
    assert_eq!(
        keyed,
        [
            (hour_of(received), "long", 0, 1),
            (hour_of(received), "long", 1, 0)
        ],
        "the settlement keys its request row's hour and band"
    );
    let long = r.pricing.in_force(received).models[HAIKU]
        .long_context
        .as_ref()
        .expect("the test list prices Haiku 5.5 by prompt length")
        .clone();
    let january = 2_000.0 * long.cells.cache_read / 1_000_000.0;
    let (cost, wasted) = read_dollars(&r, &rows);
    assert!(
        (cost - january).abs() < 1e-12 && (wasted - january).abs() < 1e-12,
        "the cost and the waste at the January long-context cache read: {cost} and {wasted} against {january}"
    );
}

/// A probe costs at the cells its own prompt selects: on a row priced by prompt length, a probe whose answer reads
/// back more than the threshold costs at the long-context cache read.
#[tokio::test]
async fn a_probe_on_a_model_with_long_context_cells_costs_at_the_cells_its_own_prompt_selects() {
    let mut r = rig(RigSpec {
        pricing: phoebis_ledger::test_tables::api_prices(),
        ..RigSpec::default()
    })
    .await;
    let body = BODY.replace("claude-sonnet-4-5", "claude-haiku-5-5");
    r.up.push_credential_rule(A, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 1);
    let rows = rollups(&r, |rows| tokens(probe_rows(rows).usage()) > 0).await;
    let (cost, _) = read_dollars(&r, &rows);
    // The test price table's Haiku 5.5 row, per million tokens: cache read 0.1 up to 1,000 prompt tokens and 0.5
    // over them. The probe read the standing 2,000 tokens back, so its own prompt is over the threshold.
    let over = 2_000.0 * 0.5 / 1_000_000.0;
    assert!((cost - over).abs() < 1e-12, "{cost}");
}
