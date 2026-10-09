//! The keepalive's ledger rows: a kept-warm request's credit, a probe's cost, a migrated probe's price, and the wasted
//! count settled when a recipe stops or the request after a probe returns cold.
mod common;
use std::collections::BTreeMap;
use std::time::Duration;

use common::*;
use fake_upstream::Respond;
use phoebis_keepalive::{Event, KeepalivePolicy, Reason};
use phoebis_ledger::{hour_of, read_rollups, RollupRow, RollupSelect};

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
    let mut sum = RollupRow::key(0, "", 0, "", "", "", "probe", "", "", "", "", "", "");
    for row in rows.iter().filter(|r| r.kind == "probe") {
        sum.add(row);
    }
    sum
}

fn inference_rows(rows: &[RollupRow]) -> RollupRow {
    let mut sum = RollupRow::key(0, "", 0, "", "", "", "inference", "", "", "", "", "", "");
    for row in rows.iter().filter(|r| r.kind == "inference") {
        sum.add(row);
    }
    sum
}

// claude-sonnet-4-5 is not in the shipped price table; the harness serves it under that id alone. Every test
// here serves and requests claude-sonnet-5, which the table prices, so the cost is a number the assertions can
// name.
const PRICED: &str = "claude-sonnet-5";

fn priced_body() -> String {
    BODY.replace("claude-sonnet-4-5", PRICED)
}

/// The cells `model` prices at for an answer served at `speed` on a prompt under any threshold, in the table the
/// rig prices through.
fn rig_cells(model: &str, speed: phoebis_ledger::Speed) -> phoebis_ledger::ModelPrice {
    phoebis_ledger::PriceTable::shipped()
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
    let rows = rollups(&r, |rows| inference_rows(rows).kept_warm_requests == 1).await;
    let credit = inference_rows(&rows);
    assert_eq!(
        (credit.kept_warm_requests, credit.kept_warm_tokens),
        (1, 2_000)
    );
    assert_eq!(
        credit.saved_usd_gross, 0.0,
        "the credit counts the request and its tokens and prices no saving"
    );
    assert!(probe_rows(&rows).probe_cost_usd > 0.0);
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
    assert!(
        row(&r, &fifth).await.kept_warm,
        "the rewrite kept the entry the fifth request read"
    );
    let rows = rollups(&r, |rows| probe_rows(rows).requests >= 2).await;
    assert_eq!(inference_rows(&rows).kept_warm_requests, 2);
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
    assert_eq!(
        inference_rows(&rows).kept_warm_tokens,
        2_000,
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
    assert_eq!(inference_rows(&rows).kept_warm_tokens, 2_000);
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
    let rows = rollups(&r, |rows| probe_rows(rows).probe_migrated == 1).await;
    let cost = probe_rows(&rows).probe_cost_usd;
    assert!(cost > 0.0, "a migrated probe is priced from its usage");
    r.advance(60_000);
    let second = r.observe_with("main", &body).await;
    assert_eq!(
        r.status("main").account_id.as_deref(),
        Some(other_id),
        "the pin moved with the probe"
    );
    let rows = rollups(&r, |rows| inference_rows(rows).kept_warm_requests == 1).await;
    assert_eq!(inference_rows(&rows).kept_warm_tokens, 2_000);
    assert!(
        row(&r, &second).await.kept_warm,
        "the write the probe paid on the new account is the one the request would have paid"
    );
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
    assert_eq!(r.status("main").reason, Reason::IdleLimit);
    let rows = rollups(&r, |rows| probe_rows(rows).wasted_probes == 3).await;
    let probe = probe_rows(&rows);
    assert!(
        (probe.wasted_probe_usd - probe.probe_cost_usd).abs() < 1e-12,
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
        probe.wasted_probe_usd > 0.0
            && (probe.wasted_probe_usd - probe.probe_cost_usd).abs() < 1e-12,
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
        probe.wasted_probe_usd > 0.0
            && (probe.wasted_probe_usd - probe.probe_cost_usd).abs() < 1e-12,
        "{probe:?}"
    );
    assert_eq!(r.status("main").unanswered, 0);
}

/// The two halves of a request the tests below drive on the engine's side alone: its observation and selection on
/// `a`, then the first stream event, which arms the schedule from the request's start with the cache it read and
/// wrote.
fn open_request(r: &Rig, id: &str, read: u64, write: u64) {
    r.engine.observe(r.observed(id, "main", BODY));
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
        open_request(&r, "first", 0, 2_000);
        r.engine.observe(completed(&r, "first", 0));
        r.advance_attended(60_000);
        open_request(&r, "second", read, 2_000 - read);
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

/// A failed request decides the probes it kept only once the upstream answered it: past a head the read is the
/// cache's answer, while one that failed before any reached nothing and leaves them for the request after it.
#[tokio::test]
async fn a_failed_request_decides_the_probes_it_kept_only_past_a_head() {
    for (head, left) in [(Some(40), 0), (None, 1)] {
        let mut r = rig(RigSpec {
            accounts: vec![api_key("a", A, 0)],
            ..RigSpec::default()
        })
        .await;
        open_request(&r, "first", 0, 2_000);
        r.engine.observe(completed(&r, "first", 0));
        r.advance_attended(270_000);
        r.step().await;
        assert_eq!(r.status("main").unanswered, 1, "a covering probe");
        r.advance_attended(10_000);
        open_request(&r, "second", 0, 2_000);
        r.engine.observe(failed(&r, "second", head));
        assert_eq!(r.status("main").unanswered, left, "head {head:?}");
        if head.is_some() {
            let rows = rollups(&r, |rows| probe_rows(rows).wasted_probes > 0).await;
            assert_eq!(
                probe_rows(&rows).wasted_probe_reasons,
                BTreeMap::from([("cold_return".to_string(), 1)])
            );
        }
    }
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
    open_request(&r, "first", 0, 2_000);
    r.engine.observe(completed(&r, "first", 0));
    r.advance_attended(60_000);
    open_request(&r, "second", 0, 2_000);
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
}

/// `open_request` on a body of the test's choosing.
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
    open_request(&r, "first", 0, 2_000);
    r.engine.observe(completed(&r, "first", 0));
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").unanswered, 1, "a covering probe");
    r.advance(60_000);
    open_request(&r, "second", 2_000, 0);
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
    open_request(&r, "first", 0, 2_000);
    r.engine.observe(completed(&r, "first", 0));
    r.advance_attended(270_000);
    r.step().await;
    assert_eq!(r.status("main").unanswered, 1);
    r.advance_attended(10_000);
    open_request(&r, "second", 2_000, 0);
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
/// any head moves the agent to that model and leaves the probes kept; the next request on it reads that model's own
/// entry inside its TTL, which reads nothing the probes paid for, so they settle as a cold return and nothing is
/// credited.
#[tokio::test]
async fn a_cache_read_on_another_model_settles_the_probes_kept_on_the_first_as_a_cold_return() {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    open_request(&r, "first", 0, 2_000);
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
        (OPUS, 1),
        "the failed request moved the model and decided nothing"
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
        probe.wasted_probe_usd > 0.0
            && (probe.wasted_probe_usd - probe.probe_cost_usd).abs() < 1e-12,
        "{probe:?}"
    );
}

/// The probe rows of one speed, summed.
fn probe_rows_at(rows: &[RollupRow], fast: bool) -> RollupRow {
    let mut sum = RollupRow::key(0, "", 0, "", "", "", "probe", "", "", "", "", "", "");
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
        fast_probes.wasted_probe_usd > 0.0
            && (fast_probes.wasted_probe_usd - fast_probes.probe_cost_usd).abs() < 1e-12,
        "{fast_probes:?}"
    );
}

/// A kept-warm credit needs the entry the covering probe kept to be there when the credited request arrives, on the
/// instant the settlement and the scheduler read as the entry gone. On the lineage trace above, the standard probe
/// refreshes its five-minute entry 560 s in and nothing is admitted until the fourth request, which arrives one
/// millisecond before that entry lapses, as it lapses, or a minute after: only the first is credited, and from the
/// lapse on the standard probe settles as expired. The fast probe's entry lapsed 570 s in, before any of them, so it
/// settles as expired in all three.
#[tokio::test]
async fn a_kept_warm_credit_needs_the_covering_entry_alive_when_the_request_arrives() {
    for (offset, credited) in [(299_999, 1), (300_000, 0), (360_000, 0)] {
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
        r.ledger.clone().shutdown().await.unwrap();
        let rows =
            r.db.read_blocking(|c| {
                read_rollups(c, hour_of(NOW) - 3_600, i64::MAX, &RollupSelect::default())
            })
            .unwrap();
        let (fast_probes, standard_probes) =
            (probe_rows_at(&rows, true), probe_rows_at(&rows, false));
        let expired = BTreeMap::from([("expired".to_string(), 1)]);
        let standard_settled = if credited == 1 {
            (0, BTreeMap::new())
        } else {
            (1, expired.clone())
        };
        assert_eq!(
            (
                inference_rows(&rows).kept_warm_requests,
                fast_probes.wasted_probe_reasons.clone(),
                (
                    standard_probes.wasted_probes,
                    standard_probes.wasted_probe_reasons.clone()
                ),
            ),
            (credited, expired, standard_settled),
            "arrived {arrived} ms in"
        );
        assert!(
            fast_probes.wasted_probe_usd > 0.0
                && (fast_probes.wasted_probe_usd - fast_probes.probe_cost_usd).abs() < 1e-12,
            "arrived {arrived} ms in: {fast_probes:?}"
        );
        if credited == 0 {
            assert!(
                standard_probes.wasted_probe_usd > 0.0
                    && (standard_probes.wasted_probe_usd - standard_probes.probe_cost_usd).abs()
                        < 1e-12,
                "arrived {arrived} ms in: {standard_probes:?}"
            );
        }
    }
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

/// A probe's credit row files under the funding its selection stood under, the account's overage state when the
/// probe was placed, since the answer's rate-limit headers never reach the engine. The probe's own request row
/// reads the head's overage fact, so with the account in overage both rows file under `overage`.
#[tokio::test]
async fn an_overage_probe_s_request_and_credit_rows_file_under_overage_as_the_selection_s_state_decides(
) {
    let mut r = rig(subscription_spec()).await;
    r.enter_overage("o", r.now() + 10_000_000).await;
    settled(&r, |r| async move {
        r.h.router
            .overage("o")
            .await
            .unwrap()
            .is_some_and(|e| e.in_use)
    })
    .await;
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
                "true".into(),
            )],
            body: read_back,
        },
    );
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 1, "one covering probe");
    let probe_rows_of_o = |rows: &[RollupRow]| {
        rows.iter()
            .filter(|row| row.kind == "probe" && row.account_id == "o")
            .map(|row| {
                (
                    if row.probe_cost_usd > 0.0 {
                        "credit"
                    } else {
                        "request"
                    },
                    row.funding.clone(),
                )
            })
            .collect::<Vec<_>>()
    };
    let rows = rollups(&r, |rows| probe_rows_of_o(rows).len() == 2).await;
    let mut fundings = probe_rows_of_o(&rows);
    fundings.sort();
    assert_eq!(
        fundings,
        [
            ("credit", "overage".to_string()),
            ("request", "overage".to_string())
        ],
        "both rows of the probe file under the overage the selection stood in: {rows:?}"
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
    open_request(&r, "first", 0, 2_000);
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
    open_request(&r, "first", 0, 2_000);
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
    open_request(&r, "third", 2_000, 0);
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
        probe.wasted_probe_usd > 0.0
            && (probe.wasted_probe_usd - probe.probe_cost_usd).abs() < 1e-12,
        "{probe:?}"
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
/// Answers the first probe's cost, read off the rollup before the second probe runs.
async fn a_covering_probe_then_a_rewrite(r: &mut Rig) -> f64 {
    open_request(r, "first", 0, 2_000);
    r.engine.observe(completed(r, "first", 0));
    r.advance_attended(270_000);
    r.step().await;
    let first_cost =
        probe_rows(&rollups(r, |rows| probe_rows(rows).probe_cost_usd > 0.0).await).probe_cost_usd;
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
    open_request(&r, "second", 2_000, 0);
    r.engine.observe(completed(&r, "second", 2_000));
    let status = r.status("main");
    assert_eq!(status.unanswered, 0);
    let rows = final_rollups(&r).await;
    let (credit, probe) = (inference_rows(&rows), probe_rows(&rows));
    assert_eq!(
        (
            credit.kept_warm_requests,
            credit.kept_warm_tokens,
            probe.wasted_probes,
            probe.wasted_probe_reasons.clone()
        ),
        (1, 2_000, 1, BTreeMap::from([("expired".to_string(), 1)])),
        "the rewrite kept the entry the request read, and the first probe's had lapsed"
    );
    assert!(
        (probe.wasted_probe_usd - first_cost).abs() < 1e-12,
        "the first probe is the wasted one: {first_cost} against {probe:?}"
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
    open_request(&r, "second", 2_000, 0);
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
    assert!(
        (probe.wasted_probe_usd - probe.probe_cost_usd).abs() < 1e-12,
        "{probe:?}"
    );
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
    open_request(&r, "second", 0, 2_000);
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
    open_request(&r, "second", 0, 2_000);
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
        probe.wasted_probe_usd > 0.0,
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
        assert!(probe.wasted_probe_usd > 0.0, "{sweep}: {probe:?}");
    }
}

#[tokio::test]
async fn a_late_or_orphaned_probe_is_still_paid_for() {
    for action in ["close", "expire", "new_request", "release", "replace"] {
        let mut r = rig(RigSpec {
            // The expiry case sweeps the session out from under the probe in flight; the others keep the
            // rig's day-long expiry.
            session_expiry: match action {
                "expire" => Duration::from_secs(600),
                _ => RigSpec::default().session_expiry,
            },
            ..RigSpec::default()
        })
        .await;
        let body = priced_body();
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
        match action {
            "close" => r.engine.close(),
            // Past the 600-second expiry from the last heartbeat.
            "expire" => {
                r.advance(600_000);
                r.engine.expire();
            }
            "new_request" => {
                r.up.push_credential_rule(A, "/v1/messages", cold_answer());
                r.observe_with("main", &body).await;
            }
            "release" => r.state("main", "released"),
            _ => {
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
                        },
                    )
                    .unwrap();
            }
        }
        r.up.release();
        r.engine.wait_idle().await;
        if action == "close" {
            // A restart cancels the exchange before any answer, so nothing was served and no recipe is left
            // for the probe to be wasted by. The ledger is drained rather than polled, since what this case
            // asserts is that no row lands at all.
            r.ledger.clone().shutdown().await.unwrap();
            let rows =
                r.db.read_blocking(|c| {
                    read_rollups(c, hour_of(NOW) - 3_600, i64::MAX, &RollupSelect::default())
                })
                .unwrap();
            let probe = probe_rows(&rows);
            assert_eq!(
                (probe.wasted_probes, probe.probe_cost_usd),
                (0, 0.0),
                "{action}: {probe:?}"
            );
            continue;
        }
        // The upstream served it, so it is paid for whatever the fence said: the probe row carries its cost,
        // and a result whose recipe had stopped settles as one wasted probe under that stop.
        let rows = rollups(&r, |rows| probe_rows(rows).probe_cost_usd > 0.0).await;
        let probe = probe_rows(&rows);
        assert!(probe.probe_cost_usd > 0.0, "{action}");
        match action {
            "expire" => assert_eq!(
                probe.wasted_probe_reasons.get("session_ended").copied(),
                Some(1),
                "{action}: {probe:?}"
            ),
            "new_request" => {
                assert_eq!(probe.wasted_probes, 0, "{action}: the request answered it")
            }
            "release" => assert_eq!(
                probe.wasted_probe_reasons.get("released").copied(),
                Some(1),
                "{action}: {probe:?}"
            ),
            _ => assert_eq!(
                probe.wasted_probe_reasons.get("replaced").copied(),
                Some(1),
                "{action}: {probe:?}"
            ),
        }
    }
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
    let mut on_fast = RollupRow::key(0, "", 0, "", "", "", "inference", "", "", "", "", "", "");
    for row in rows
        .iter()
        .filter(|row| row.kind == "inference" && row.speed == "fast")
    {
        on_fast.add(row);
    }
    assert_eq!(
        (on_fast.kept_warm_requests, on_fast.kept_warm_tokens),
        (1, 2_000),
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
    let rows = rollups(&r, |rows| probe_rows(rows).probe_cost_usd > 0.0).await;
    let row = rows.iter().find(|row| row.kind == "probe").unwrap();
    assert_eq!(row.canonical_model, "claude-sonnet-5");
    let expected = 2_000.0
        * rig_cells("claude-sonnet-5", phoebis_ledger::Speed::Standard).cache_read
        / 1_000_000.0;
    assert!(
        (row.probe_cost_usd - expected).abs() < 1e-12,
        "priced as the canonical model: {}",
        row.probe_cost_usd
    );
}

#[tokio::test]
async fn a_kept_warm_request_on_an_unpriced_model_counts_and_its_recipe_keeps_probing() {
    // claude-sonnet-4-5 is BODY's own model and stays out of the shipped price table for this test, unlike
    // the priced tests above.
    let mut r = rig(RigSpec::default()).await;
    r.up.push_credential_rule(A, "/v1/messages", cold_answer());
    r.observe_with("main", BODY).await;
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 1);
    r.advance(60_000);
    let second = r.observe_with("main", BODY).await;
    let rows = rollups(&r, |rows| inference_rows(rows).kept_warm_requests == 1).await;
    let credit = inference_rows(&rows);
    assert_eq!(
        (credit.kept_warm_requests, credit.kept_warm_tokens),
        (1, 2_000),
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
    let dearest = phoebis_ledger::PriceTable::shipped().rate_table().fallback;
    2_000.0 * dearest.cache_read.expect("the fallback prices every type")
}

#[tokio::test]
async fn an_unpriced_model_costs_the_dearest_cells_and_still_lands_a_probe_row() {
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    r.advance(270_000);
    r.step().await;
    let rows = rollups(&r, |rows| probe_rows(rows).requests == 1).await;
    assert!(
        (probe_rows(&rows).probe_cost_usd - dearest_read_2000()).abs() < 1e-12,
        "claude-sonnet-4-5 is not in the shipped table, so its probe row prices at the dearest cells: {}",
        probe_rows(&rows).probe_cost_usd
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
    // The probe's own request row reaches the rollups through the assembler with no cost on it, ahead of the
    // engine's keepalive delta, so the wait has to name the charge rather than the row.
    let rows = rollups(&r, |rows| {
        rows.iter()
            .any(|row| row.kind == "probe" && row.pool_id == "warm" && row.probe_cost_usd > 0.0)
    })
    .await;
    assert!(rows
        .iter()
        .any(|row| row.pool_id == "warm" && row.probe_cost_usd > 0.0));
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
    let rows = rollups(&r, |rows| probe_rows(rows).probe_cost_usd > 0.0).await;
    let cost = probe_rows(&rows).probe_cost_usd;
    r.state("main", "released");
    let rows = rollups(&r, |rows| probe_rows(rows).wasted_probes == 1).await;
    let probe = probe_rows(&rows);
    assert_eq!(
        probe.wasted_probe_reasons.get("released").copied(),
        Some(1),
        "{probe:?}"
    );
    assert!(
        (probe.wasted_probe_usd - cost).abs() < 1e-12,
        "settled as wasted at what it cost: {probe:?}"
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
    assert_eq!(inference_rows(&rows).kept_warm_tokens, 2_000);
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
    let rows = rollups(&r, |rows| probe_rows(rows).probe_cost_usd > 0.0).await;
    let probe = probe_rows(&rows);
    assert!(
        (probe.probe_cost_usd - expected).abs() < 1e-12,
        "{probe:?} against {expected}"
    );
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
/// weigh settle in a share of the 5-hour window.
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
    r.advance(60_000);
    r.up.push_credential_rule(OAUTH_SECRET, "/v1/messages", warm_answer());
    let kept = r.observe_with("main", &body).await;
    let rows = rollups(&r, |rows| inference_rows(rows).kept_warm_requests == 1).await;
    assert_eq!(inference_rows(&rows).kept_warm_tokens, 2_000);
    assert!(row(&r, &kept).await.kept_warm);
}

const OPUS: &str = "claude-opus-5-5";

fn opus_body() -> String {
    body_on(OPUS)
}

fn body_on(model: &str) -> String {
    BODY.replace("claude-sonnet-4-5", model)
}

/// A probe costs at the cells its own prompt selects: on a row priced by prompt length, a probe whose answer reads
/// back more than the threshold costs at the long-context cache read.
#[tokio::test]
async fn a_probe_on_a_model_priced_by_prompt_length_costs_at_the_cells_its_own_prompt_selects() {
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
    let rows = rollups(&r, |rows| probe_rows(rows).probe_cost_usd > 0.0).await;
    let cost = probe_rows(&rows).probe_cost_usd;
    // The test price table's Haiku 5.5 row, per million tokens: cache read 0.1 up to 1,000 prompt tokens and 0.5
    // over them. The probe read the standing 2,000 tokens back, so its own prompt is over the threshold.
    let over = 2_000.0 * 0.5 / 1_000_000.0;
    let standard = 2_000.0 * 0.1 / 1_000_000.0;
    assert!((cost - over).abs() < 1e-12, "{cost}");
    assert!(
        (cost - standard).abs() > 1e-12,
        "a lookup that ignores the prompt prices the standard cells: {cost}"
    );
}
