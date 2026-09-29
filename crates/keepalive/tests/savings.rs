//! Savings accounting: a kept-warm request's gross saving, a probe's cost, a migrated probe's exemption, the
//! wasted count settled when a recipe stops, the net that can go negative, and the daily cap.
mod common;
use std::collections::BTreeMap;
use std::time::Duration;

use phoebis_keepalive::{Event, KeepaliveLimits, KeepalivePolicy, Reason};
use phoebis_ledger::{hour_of, read_rollups, RollupRow};
use common::*;
use fake_upstream::Respond;

/// Every rollup row the rig wrote, once the writer has landed what the test waits for. The whole table
/// rather than the rig's own hour: a test that moves the clock to the next UTC day writes past it.
async fn rollups(r: &Rig, want: impl Fn(&[RollupRow]) -> bool) -> Vec<RollupRow> {
    wait_for(Duration::from_secs(5), || {
        let rows =
            r.db.read_blocking(|c| read_rollups(c, hour_of(NOW) - 3_600, i64::MAX))
                .unwrap();
        want(&rows).then_some(rows)
    })
    .await
}

fn probe_rows(rows: &[RollupRow]) -> RollupRow {
    let mut sum = RollupRow::key(0, "", 0, "", "", "", "probe", "", "", "", "");
    for row in rows.iter().filter(|r| r.kind == "probe") {
        sum.add(row);
    }
    sum
}

fn inference_rows(rows: &[RollupRow]) -> RollupRow {
    let mut sum = RollupRow::key(0, "", 0, "", "", "", "inference", "", "", "", "");
    for row in rows.iter().filter(|r| r.kind == "inference") {
        sum.add(row);
    }
    sum
}

// claude-sonnet-4-5 is not in the shipped price table; the harness serves it under that id alone. Every test
// here serves and requests claude-sonnet-5, which the table prices, so the saving and the cost are numbers the
// assertions can name.
const PRICED: &str = "claude-sonnet-5";

fn priced_body() -> String {
    BODY.replace("claude-sonnet-4-5", PRICED)
}

/// Sonnet 5: input 2, cache read 0.2 per million; 2,000 cached tokens save (2 - 0.2) * 2000 / 1e6.
const SAVING_2000: f64 = 2_000.0 * (2.0 - 0.2) / 1_000_000.0;

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
    let cost = r.status("main").net_usd;
    assert!(
        cost < 0.0,
        "the probe's cost counts against the net: {cost}"
    );
    // Past the five-minute TTL of the first request, a real request reads the cache the probe kept.
    r.advance(60_000);
    let second = r.observe_with("main", &body).await;
    let rows = rollups(&r, |rows| inference_rows(rows).kept_warm_requests == 1).await;
    let credit = inference_rows(&rows);
    assert_eq!(
        (credit.kept_warm_requests, credit.kept_warm_tokens),
        (1, 2_000)
    );
    assert!(
        (credit.saved_usd_gross - SAVING_2000).abs() < 1e-12,
        "{}",
        credit.saved_usd_gross
    );
    let probe = probe_rows(&rows);
    assert!(
        probe.probe_cost_usd > 0.0
            && (r.status("main").net_usd - (SAVING_2000 - probe.probe_cost_usd)).abs() < 1e-12
    );
    assert!(row(&r, &second).await.kept_warm);
    // The same gap without a probe, or with a cold probe, credits nothing.
    let third = r.observe_with("main", &body).await;
    r.advance(330_000);
    let fourth = r.observe_with("main", &body).await;
    r.up.push_credential_rule(A, "/v1/messages", probe_answer(0, 2_000));
    r.advance(270_000);
    r.step().await;
    r.advance(60_000);
    let fifth = r.observe_with("main", &body).await;
    for id in [&third, &fourth, &fifth] {
        assert!(
            !row(&r, id).await.kept_warm,
            "{id}: neither the probe-less gap nor the cold probe credited"
        );
    }
    let rows = rollups(&r, |rows| probe_rows(rows).requests >= 2).await;
    assert_eq!(inference_rows(&rows).kept_warm_requests, 1);
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
    assert_eq!(inference_rows(&rows).kept_warm_requests, 1);
    let text = r
        .metrics
        .render(&phoebis_ledger::RoutingGauges::default(), &[], 0);
    assert!(
        text.contains(&format!(
            r#"phoebis_keepalive_saved_usd_total{{pool="main"}} {SAVING_2000}"#
        )),
        "{text}"
    );
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
async fn a_migrated_probe_is_exempt_and_the_next_request_counts_as_kept_warm() {
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
            status.net_usd,
            status.account_id.as_deref(),
            status.unanswered
        ),
        (1, 0, 0.0, Some(other_id), 1),
        "{status:?}"
    );
    let rows = rollups(&r, |rows| probe_rows(rows).probe_migrated == 1).await;
    assert_eq!(
        probe_rows(&rows).probe_cost_usd,
        0.0,
        "a migrated probe's cost is left out"
    );
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
    let mut r = rig(RigSpec {
        policy: KeepalivePolicy {
            max_idle_ticks: 3,
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
        r.advance(270_000);
        r.step().await;
    }
    r.observe_with("main", &body).await;
    r.advance(270_000);
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
    r.advance(270_000);
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
    // A migrated probe nobody came back for is a wasted probe too, at no cost.
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
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").migrations, 1);
    r.event(Event {
        event: "session_end".into(),
        ..Default::default()
    })
    .unwrap();
    let rows = rollups(&r, |rows| probe_rows(rows).wasted_probes == 1).await;
    assert_eq!(
        (
            probe_rows(&rows).wasted_probe_usd,
            probe_rows(&rows)
                .wasted_probe_reasons
                .get("session_ended")
                .copied()
        ),
        (0.0, Some(1))
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

/// A sweep run by a reader settles from that reader: `expire` and `snapshot` reach `Inner::expire` without a
/// tick behind them, and what they queue has to land while the ledger is still up rather than wait for a
/// `step` that a shutdown may never run.
#[tokio::test]
async fn an_expiry_swept_by_a_reader_settles_from_that_call() {
    for sweep in ["expire", "snapshot"] {
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
                r.engine.snapshot("desktop", r.generation("desktop"));
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
        r.advance(270_000);
        r.engine.step().await;
        wait_for(Duration::from_secs(5), || {
            (r.up.requests().len() == 2).then_some(())
        })
        .await;
        match action {
            "close" => r.engine.close(),
            "expire" => {
                r.advance(400_000);
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
                r.db.read_blocking(|c| read_rollups(c, hour_of(NOW) - 3_600, i64::MAX))
                    .unwrap();
            let probe = probe_rows(&rows);
            assert_eq!(
                (probe.wasted_probes, probe.probe_cost_usd),
                (0, 0.0),
                "{action}: {probe:?}"
            );
            continue;
        }
        // The upstream served it, so it is paid for whatever the fence said: the pool's spend and the probe
        // row carry its cost, and a result whose recipe had stopped settles as one wasted probe under that stop.
        let rows = rollups(&r, |rows| probe_rows(rows).probe_cost_usd > 0.0).await;
        let probe = probe_rows(&rows);
        assert!(probe.probe_cost_usd > 0.0, "{action}");
        assert!(
            r.engine.spent_today().get("main").copied().unwrap_or(0.0) > 0.0,
            "{action}"
        );
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

#[tokio::test]
async fn a_negative_net_holds_after_the_grace_probes_and_a_real_request_regrants_them() {
    let mut r = rig(RigSpec {
        policy: KeepalivePolicy {
            net_negative_grace_probes: 2,
            ..RigSpec::default().policy
        },
        ..RigSpec::default()
    })
    .await;
    let body = priced_body();
    r.up.push_credential_rule(A, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    // The first paid probe turns the net negative and grants the two grace probes; the second and third spend
    // them; the fourth holds.
    for (tick, expected) in [1, 2, 3, 3].into_iter().enumerate() {
        r.advance(270_000);
        r.step().await;
        assert_eq!(r.status("main").probes, expected, "tick {tick}");
    }
    assert_eq!(
        (r.status("main").reason, r.status("main").grace_left),
        (Reason::NetNegative, 0)
    );
    assert!(r.status("main").net_usd < 0.0);
    let rows = rollups(&r, |rows| probe_rows(rows).probes_skipped >= 1).await;
    assert!(
        probe_rows(&rows).probes_skipped >= 1,
        "the withheld probe is ledgered as skipped"
    );
    let skipped = rows
        .iter()
        .find(|row| row.probes_skipped > 0)
        .expect("the skipped row");
    assert_eq!(
        (skipped.account_id.as_str(), skipped.pool_id.as_str()),
        ("a", "main"),
        "a delta keys on the account the recipe held, never on an empty id"
    );
    r.up.push_credential_rule(A, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    assert_eq!(
        r.status("main").grace_left,
        2,
        "granted again on a real request while the net stays negative"
    );
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 4);
    let mut r = rig(RigSpec {
        policy: KeepalivePolicy {
            stop_when_net_negative: false,
            ..RigSpec::default().policy
        },
        ..RigSpec::default()
    })
    .await;
    r.up.push_credential_rule(A, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    for _ in 0..5 {
        r.advance(270_000);
        r.step().await;
    }
    assert_eq!(
        r.status("main").probes,
        5,
        "with the rule off a negative net never holds"
    );
}

#[tokio::test]
async fn the_daily_cost_cap_pauses_probes_until_midnight() {
    // The clock crosses a UTC midnight below, which the idle limit would read as an abandoned session; this
    // case is about the cap alone, so the idle limit is off.
    let mut r = rig(RigSpec {
        policy: KeepalivePolicy {
            probe_cost_cap_usd_per_day: Some(0.0005),
            max_idle_seconds: 0,
            ..RigSpec::default().policy
        },
        ..RigSpec::default()
    })
    .await;
    let body = priced_body();
    r.up.push_credential_rule(A, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    // Each probe reads 2,000 tokens at 0.2 per million: 0.0004; the second probe crosses the cap.
    r.advance(270_000);
    r.step().await;
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 2);
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        (r.status("main").probes, r.status("main").reason),
        (2, Reason::CapReached)
    );
    let midnight = (r.now() / 86_400_000 + 1) * 86_400_000;
    r.clock
        .store(midnight + 1_000, std::sync::atomic::Ordering::SeqCst);
    r.step().await;
    assert_eq!(r.status("main").probes, 3, "the cap resets at UTC midnight");
    let spent = r.engine.load_spent_today().await;
    assert!(
        (spent.get("main").copied().unwrap_or(0.0) - 0.0004).abs() < 1e-9,
        "the row after midnight: {spent:?}"
    );
}

/// A plan-funded probe's would-be price is quota, never money: the dollar cap that bounds an api_key
/// account's own spend must stay at zero for it, whatever the price table would have charged.
#[tokio::test]
async fn a_plan_funded_probe_leaves_the_dollar_cap_unchanged() {
    let mut r = rig(subscription_spec()).await;
    r.up.push_credential_rule(OAUTH_SECRET, "/v1/messages", cold_answer());
    r.observe_with("main", &priced_body()).await;
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 1);
    assert_eq!(
        r.engine.spent_today().get("main").copied().unwrap_or(0.0),
        0.0,
        "a plan-funded probe's would-be price is quota, never money"
    );
}

/// A restart restores the plan-side figure beside the dollar one: an oauth account's probe counts toward it
/// mid-day, not only while the engine has run continuously.
#[tokio::test]
async fn a_restart_mid_day_restores_the_plan_side_figure() {
    let mut r = rig(subscription_spec()).await;
    let body = priced_body();
    r.up.push_credential_rule(OAUTH_SECRET, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    r.advance(270_000);
    r.step().await;
    let live = r
        .engine
        .share_spent_today()
        .get("o")
        .copied()
        .unwrap_or(0.0);
    assert!(live > 0.0, "{live}");
    // A genuinely fresh engine over the same store: its own `Inner` starts empty, so the figure below is
    // whatever `load_spent_today` restored, never the first engine's own running total.
    let restarted = r.restart().await;
    restarted.load_spent_today().await;
    let rebuilt = restarted
        .share_spent_today()
        .get("o")
        .copied()
        .unwrap_or(0.0);
    assert!(
        (rebuilt - live).abs() < 1e-6,
        "the restart reproduces the figure the running engine already had: {rebuilt} vs {live}"
    );
}

/// Both figures equal key for key within a rounding step, a zero entry reading as an absent one.
fn same_budgets(a: &BTreeMap<String, f64>, b: &BTreeMap<String, f64>) -> bool {
    let nonzero = |m: &BTreeMap<String, f64>| -> BTreeMap<String, f64> {
        m.iter()
            .filter(|(_, v)| **v != 0.0)
            .map(|(k, v)| (k.clone(), *v))
            .collect()
    };
    let (a, b) = (nonzero(a), nonzero(b));
    a.len() == b.len()
        && a.iter()
            .all(|(k, v)| b.get(k).is_some_and(|w| (v - w).abs() < 1e-12))
}

/// A subscription account's probes served in overage settle in dollars, so the pool's dollar cap counts them
/// live; a restart restores both budgets as they were settled, and the cap they reached refuses the first probe
/// the restarted engine would send.
#[tokio::test]
async fn a_restart_after_overage_probes_reached_the_dollar_cap_keeps_both_budgets() {
    let mut r = rig(RigSpec {
        policy: KeepalivePolicy {
            probe_cost_cap_usd_per_day: Some(0.0005),
            ..RigSpec::default().policy
        },
        ..subscription_spec()
    })
    .await;
    r.enter_overage("o", r.now() + 10_000_000);
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
    // Each probe reads 2,000 tokens at 0.2 per million, 0.0004 in dollars; the second crosses the cap.
    for _ in 0..3 {
        r.advance(270_000);
        r.step().await;
    }
    assert_eq!(
        (r.status("main").probes, r.status("main").reason),
        (2, Reason::CapReached)
    );
    let spent = r.engine.spent_today();
    let share_spent = r.engine.share_spent_today();
    assert!(
        spent.get("main").copied().unwrap_or(0.0) >= 0.0005,
        "{spent:?}"
    );
    assert!(
        same_budgets(&share_spent, &BTreeMap::new()),
        "a probe served in overage never counts toward the plan side: {share_spent:?}"
    );
    // The rollups carry the probes' tokens under the subscription account; the restore must not read them there.
    rollups(&r, |rows| {
        rows.iter()
            .filter(|row| row.kind == "probe" && row.account_id == "o")
            .map(|row| row.cache_read_tokens)
            .sum::<u64>()
            >= 4_000
    })
    .await;
    let restarted = r.restart().await;
    restarted.load_spent_today().await;
    assert!(
        same_budgets(&restarted.spent_today(), &spent),
        "the dollar figure the running engine had: {:?} vs {spent:?}",
        restarted.spent_today()
    );
    assert!(
        same_budgets(&restarted.share_spent_today(), &share_spent),
        "and the plan-side one: {:?} vs {share_spent:?}",
        restarted.share_spent_today()
    );
    r.engine = restarted;
    r.event(session_start("session")).unwrap();
    r.up.push_credential_rule(OAUTH_SECRET, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        (r.status("main").probes, r.status("main").reason),
        (0, Reason::CapReached),
        "the cap the probes reached before the restart refuses the next one"
    );
}

/// A plan-funded probe on a model the plan table lacks settles in dollars, so the dollar cap counts it live and a
/// restart restores it there.
#[tokio::test]
async fn a_restart_restores_the_dollar_delta_of_a_plan_funded_probe_on_an_unlisted_model() {
    let mut r = rig(RigSpec {
        tables: plan_pricing_no_model(),
        ..subscription_spec()
    })
    .await;
    let body = BODY.replace("claude-sonnet-4-5", "claude-haiku-4-5-20251001");
    r.up.push_credential_rule(OAUTH_SECRET, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    r.advance(270_000);
    r.step().await;
    let spent = r.engine.spent_today();
    assert!(spent.get("main").copied().unwrap_or(0.0) > 0.0, "{spent:?}");
    rollups(&r, |rows| {
        rows.iter()
            .any(|row| row.kind == "probe" && row.account_id == "o" && row.cache_read_tokens > 0)
    })
    .await;
    let restarted = r.restart().await;
    restarted.load_spent_today().await;
    assert!(
        same_budgets(&restarted.spent_today(), &spent),
        "the probe's dollar delta survives the restart: {:?} vs {spent:?}",
        restarted.spent_today()
    );
    assert!(
        same_budgets(&restarted.share_spent_today(), &BTreeMap::new()),
        "and reaches no plan-side figure: {:?}",
        restarted.share_spent_today()
    );
}

/// A migrated probe costs nothing in either unit, live or after a restart, though the upstream served it.
#[tokio::test]
async fn a_restart_restores_nothing_for_a_migrated_probe() {
    const OTHER_SECRET: &str = "sk-ant-oat01-PPPP";
    let mut r = rig(RigSpec {
        accounts: vec![oauth("o", OAUTH_SECRET, 0), oauth("p", OTHER_SECRET, 0)],
        ..subscription_spec()
    })
    .await;
    let body = priced_body();
    // The two accounts rank equally, so which one serves the first request is not known until it has.
    r.observe_with("main", &body).await;
    let first = r.status("main").account_id.clone().unwrap();
    let (mine, other) = if first == "o" {
        (OAUTH_SECRET, OTHER_SECRET)
    } else {
        (OTHER_SECRET, OAUTH_SECRET)
    };
    r.up.push_credential_rule(
        mine,
        "/v1/messages",
        relay_common::json_429_unified(4_600, "five_hour"),
    );
    r.up.push_credential_rule(other, "/v1/messages", probe_answer(0, 2_000));
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").migrations, 1, "{:?}", r.status("main"));
    assert!(
        same_budgets(&r.engine.spent_today(), &BTreeMap::new())
            && same_budgets(&r.engine.share_spent_today(), &BTreeMap::new()),
        "a migrated probe charges neither budget live"
    );
    // The rollups carry the migrated probe's write under the account that served it.
    rollups(&r, |rows| {
        rows.iter()
            .any(|row| row.kind == "probe" && row.cache_write_5m_tokens > 0)
    })
    .await;
    let restarted = r.restart().await;
    restarted.load_spent_today().await;
    assert!(
        same_budgets(&restarted.spent_today(), &BTreeMap::new())
            && same_budgets(&restarted.share_spent_today(), &BTreeMap::new()),
        "nor after a restart: {:?} {:?}",
        restarted.spent_today(),
        restarted.share_spent_today()
    );
}

/// The UTC date a budget row is filed under.
fn budget_day(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms)
        .unwrap()
        .strftime("%Y-%m-%d")
        .to_string()
}

/// The budgets are one UTC day's: the day roll deletes the rows of the day before, and the first charge after
/// it starts the new day's row, which is all a restart restores.
#[tokio::test]
async fn the_day_roll_deletes_yesterdays_budget_rows() {
    let mut r = rig(RigSpec {
        // The clock crosses a UTC midnight below, which the idle limit would read as an abandoned session.
        policy: KeepalivePolicy {
            max_idle_seconds: 0,
            ..RigSpec::default().policy
        },
        ..RigSpec::default()
    })
    .await;
    r.up.push_credential_rule(A, "/v1/messages", cold_answer());
    r.observe_with("main", &priced_body()).await;
    r.advance(270_000);
    r.step().await;
    let yesterday = budget_day(r.now());
    let rows = r.h.store.keepalive_budgets(&yesterday).unwrap();
    assert!(
        rows.len() == 1 && rows[0].id == "main" && (rows[0].usd - 0.0004).abs() < 1e-12,
        "the probe's charge is on its pool's row for the day: {rows:?}"
    );
    let midnight = (r.now() / 86_400_000 + 1) * 86_400_000;
    r.clock
        .store(midnight + 1_000, std::sync::atomic::Ordering::SeqCst);
    r.step().await;
    assert!(
        r.h.store.keepalive_budgets(&yesterday).unwrap().is_empty(),
        "the roll deleted the day before"
    );
    let today = r.h.store.keepalive_budgets(&budget_day(r.now())).unwrap();
    assert!(
        today.len() == 1 && (today[0].usd - 0.0004).abs() < 1e-12,
        "the probe due after midnight started the new day's row: {today:?}"
    );
    let restarted = r.restart().await;
    restarted.load_spent_today().await;
    assert!(
        same_budgets(
            &restarted.spent_today(),
            &BTreeMap::from([("main".to_string(), 0.0004)])
        ),
        "a restart restores the new day alone: {:?}",
        restarted.spent_today()
    );
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
    let expected = 2_000.0 * 0.2 / 1_000_000.0;
    assert!(
        (row.probe_cost_usd - expected).abs() < 1e-12,
        "priced as the canonical model: {}",
        row.probe_cost_usd
    );
    assert!((r.status("main").net_usd + expected).abs() < 1e-12);
}

#[tokio::test]
async fn a_kept_warm_request_on_an_unpriced_model_is_marked_and_counted_with_zero_saving() {
    // claude-sonnet-4-5 is BODY's own model and stays out of the shipped price table for this test, unlike
    // the priced tests above.
    let mut r = rig(RigSpec::default()).await;
    r.up.push_credential_rule(A, "/v1/messages", cold_answer());
    r.observe_with("main", BODY).await;
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 1);
    let before = r.status("main").net_usd;
    r.advance(60_000);
    let second = r.observe_with("main", BODY).await;
    let rows = rollups(&r, |rows| inference_rows(rows).kept_warm_requests == 1).await;
    let credit = inference_rows(&rows);
    assert_eq!(
        (credit.kept_warm_requests, credit.kept_warm_tokens),
        (1, 2_000),
        "the mark and the tokens count on a model the price table does not know"
    );
    assert_eq!(credit.saved_usd_gross, 0.0);
    assert_eq!(
        r.status("main").net_usd,
        before,
        "no saving to add for an unpriced model"
    );
    assert!(row(&r, &second).await.kept_warm);
}

#[tokio::test]
async fn an_unpriced_model_costs_nothing_and_still_lands_a_probe_row() {
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        r.status("main").net_usd,
        0.0,
        "claude-sonnet-4-5 is not in the shipped table, so nothing is estimated"
    );
    let rows = rollups(&r, |rows| probe_rows(rows).requests == 1).await;
    assert_eq!(
        (
            probe_rows(&rows).probe_cost_usd,
            r.status("main").unanswered
        ),
        (0.0, 1),
        "a probe at no cost is still a probe nobody answered yet"
    );
}

/// A cost table lacking every model, so "claude-haiku-4-5-20251001" (which the shipped dollar table prices) is
/// unlisted on purpose rather than by a coincidence of the shipped one's date-suffix fall-back.
fn plan_pricing_no_model() -> phoebis_ledger::PlanTables {
    phoebis_ledger::PlanTables {
        cost: phoebis_ledger::CostTable::from_yaml(
            "effective: 2026-09-29\ntier: default_claude_max_5x\nbands: []\nwindows:\n  five_hour: {}\n  seven_day: {}\n",
        )
        .unwrap(),
        tiers: phoebis_ledger::TierFactors::shipped(),
    }
}

/// A model the dollar table prices but the cost table lacks settles a plan-funded probe in dollars, the same
/// fall-back the settlement takes; the stop and the dollar cap must read that unit too, or nothing bounds it.
#[tokio::test]
async fn a_plan_funded_recipe_on_an_unlisted_model_stops_on_a_negative_dollar_net() {
    let mut r = rig(RigSpec {
        tables: plan_pricing_no_model(),
        policy: KeepalivePolicy {
            net_negative_grace_probes: 0,
            ..RigSpec::default().policy
        },
        ..subscription_spec()
    })
    .await;
    let body = BODY.replace("claude-sonnet-4-5", "claude-haiku-4-5-20251001");
    r.up.push_credential_rule(OAUTH_SECRET, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    r.advance(270_000);
    r.step().await;
    let status = r.status("main");
    assert!(
        status.net_usd < 0.0 && status.net_five_hour_share == 0.0,
        "the cost table lacks this model, so the probe settles in dollars whatever the funding: {status:?}"
    );
    assert!(
        r.engine.spent_today().get("main").copied().unwrap_or(0.0) > 0.0,
        "the dollar cap is the only one that bounds this probe: the plan-side cap counts the window share alone"
    );
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        r.status("main").reason,
        Reason::NetNegative,
        "a negative dollar net stops the recipe even though the funding is plan: {:?}",
        r.status("main")
    );
}

/// The credit for a probe that settled in dollars on the unlisted-model fall-back must land in dollars too, in
/// the unit `covered_unit` actually captured rather than the account's own nominal funding.
#[tokio::test]
async fn a_plan_funded_credit_on_an_unlisted_model_lands_in_dollars() {
    let mut r = rig(RigSpec {
        tables: plan_pricing_no_model(),
        ..subscription_spec()
    })
    .await;
    let body = BODY.replace("claude-sonnet-4-5", "claude-haiku-4-5-20251001");
    r.up.push_credential_rule(OAUTH_SECRET, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    r.advance(270_000);
    r.step().await;
    let status = r.status("main");
    assert!(
        status.net_usd < 0.0 && status.net_five_hour_share == 0.0,
        "the cost table lacks this model, so the probe settles in dollars: {status:?}"
    );
    r.advance(60_000);
    r.up.push_credential_rule(OAUTH_SECRET, "/v1/messages", warm_answer());
    r.observe_with("main", &body).await;
    let credited = r.status("main");
    assert!(
        credited.net_usd > status.net_usd && credited.net_five_hour_share == 0.0,
        "the credit for a probe settled in dollars lands in dollars too, the share balance untouched: {credited:?}"
    );
}

/// The day roll belongs to the lock that charges: a probe that lands after UTC midnight before any tick
/// charges the new day, and the tick that follows must not clear the charge it just took.
#[tokio::test]
async fn a_probe_that_lands_after_midnight_charges_the_new_day() {
    let mut r = rig(RigSpec {
        // The clock crosses a UTC midnight below, which the idle limit would read as an abandoned session.
        policy: KeepalivePolicy {
            max_idle_seconds: 0,
            ..RigSpec::default().policy
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
    r.advance(270_000);
    r.engine.step().await;
    wait_for(Duration::from_secs(5), || {
        (r.up.requests().len() == 2).then_some(())
    })
    .await;
    let midnight = (r.now() / 86_400_000 + 1) * 86_400_000;
    r.clock
        .store(midnight + 1_000, std::sync::atomic::Ordering::SeqCst);
    r.up.release();
    r.engine.wait_idle().await;
    let spent = r.engine.spent_today().get("main").copied().unwrap_or(0.0);
    assert!(spent > 0.0, "the probe after midnight charged its pool");
    // The opt-out keeps the next tick from starting another probe, so the sum it leaves is the probe's own.
    let mut key = r.h.store.api_key("desktop").unwrap().unwrap();
    key.keepalive_opt_out = true;
    r.h.store.update_api_key(&key, "test").unwrap();
    r.h.router.reload().await.unwrap();
    r.step().await;
    assert_eq!(
        r.engine.spent_today().get("main").copied(),
        Some(spent),
        "the first tick of the new day kept the charge the probe already took"
    );
}

/// A key moved to another pool takes that pool's budget with it: the cap the gate reads and the delta the
/// probe charges belong to the pool that serves the probe, not to the one the last real request selected.
#[tokio::test]
async fn a_key_moved_to_another_pool_is_capped_and_charged_there() {
    let mut r = rig(RigSpec::default()).await;
    let body = priced_body();
    r.up.push_credential_rule(A, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    // Two probes at 0.0004 each under the key's own pool, past the cap the warm pool carries below.
    for _ in 0..2 {
        r.advance(270_000);
        r.step().await;
    }
    assert_eq!(r.status("main").probes, 2);
    assert!(r.engine.spent_today().get("main").copied().unwrap_or(0.0) > 0.0003);
    r.h.store
        .insert_pool(
            &phoebis_store::PoolRow {
                id: "warm".into(),
                label: "warm".into(),
                enabled: true,
                strategy: phoebis_store::Strategy::Priority,
                affinity_ttl_seconds: 3_600,
                overage_policy: phoebis_store::OveragePolicy::PreferPlan,
                migrate_above_utilization: None,
                select_below_utilization: None,
                model_rules: Vec::new(),
                keepalive_json: serde_json::to_string(&KeepalivePolicy {
                    probe_cost_cap_usd_per_day: Some(0.0003),
                    ..RigSpec::default().policy
                })
                .unwrap(),
                body_capture_hours: 0,
                prefer_subscription: false,
                shedding: phoebis_store::Shedding::default(),
                failover: phoebis_store::Failover::default(),
                cache_ttl_json: "{}".into(),
                cache_diagnostics: None,
                affinity: phoebis_store::Affinity::default(),
                quota_headers: phoebis_store::QuotaHeaders::Pass,
                created_at: NOW,
                updated_at: NOW,
            },
            "test",
        )
        .unwrap();
    r.h.store
        .set_pool_members(
            "warm",
            &[phoebis_store::MemberWrite {
                account_id: "a".to_string(),
                priority: 0,
                budget_usd: None,
                budget_period: phoebis_store::BudgetPeriod::Month,
            }],
            "test",
        )
        .unwrap();
    let mut key = r.h.store.api_key("desktop").unwrap().unwrap();
    key.pool_id = "warm".into();
    r.h.store.update_api_key(&key, "test").unwrap();
    r.h.router.reload().await.unwrap();
    // The warm pool has spent nothing of its own cap, so the recipe the moved key holds probes once more.
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        (r.status("main").probes, r.status("main").reason),
        (3, Reason::Scheduled),
        "the destination pool's cap is not spent yet"
    );
    assert_eq!(
        r.engine.recipes().first().map(|v| v.pool_id.clone()),
        Some("warm".to_string()),
        "the recipe moved with its key, without another request"
    );
    let spent = r.engine.spent_today();
    assert!(
        spent.get("warm").copied().unwrap_or(0.0) > 0.0,
        "the probe's delta landed on the pool that served it: {spent:?}"
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
    // And that one probe spends the destination pool's cap, which holds the next one without a request.
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        (r.status("main").probes, r.status("main").reason),
        (3, Reason::CapReached)
    );
}

/// A probe the upstream served and charged for is on the recipe's own books before any branch that pauses
/// it: the agent's net and the settlement a release leaves would otherwise disagree with the ledger.
#[tokio::test]
async fn a_probe_answered_with_output_is_debited_and_settled_as_wasted() {
    let mut r = rig(RigSpec::default()).await;
    let body = priced_body();
    r.up.push_credential_rule(A, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    // A 2xx answer with valid usage that generated output: paid for, and no evidence about the entry.
    r.up.push_credential_rule(A, "/v1/messages", Respond::Json { status: 200, headers: vec![], body: br#"{"id":"m","type":"message","content":[{"type":"text","text":"ok"}],"usage":{"input_tokens":0,"cache_read_input_tokens":2000,"output_tokens":1}}"#.to_vec() });
    r.advance(270_000);
    r.step().await;
    let status = r.status("main");
    assert_eq!(
        (status.reason, status.unanswered),
        (Reason::UnexpectedOutput, 1),
        "{status:?}"
    );
    let rows = rollups(&r, |rows| probe_rows(rows).probe_cost_usd > 0.0).await;
    let cost = probe_rows(&rows).probe_cost_usd;
    assert!(
        (status.net_usd + cost).abs() < 1e-12,
        "the net carries what the ledger charged: {status:?}"
    );
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
    let status = r.status("main");
    assert_eq!(status.account_id.as_deref(), Some("b"));
    assert!(
        status.net_usd < 0.0,
        "no probe of this recipe warmed b, so nothing rose the net: {status:?}"
    );
    assert!(!row(&r, &elsewhere).await.kept_warm);
    // The same gap and the same covering probe, with the request served by the account the probe warmed.
    let mut r = rig(RigSpec::default()).await;
    r.up.push_credential_rule(A, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    r.advance(270_000);
    r.step().await;
    r.advance(60_000);
    r.up.push_credential_rule(A, "/v1/messages", warm_answer());
    let here = r.observe_with("main", &body).await;
    let status = r.status("main");
    assert_eq!(status.account_id.as_deref(), Some("a"));
    assert!(
        status.net_usd > 0.0,
        "the account the probe warmed served it: {status:?}"
    );
    let rows = rollups(&r, |rows| inference_rows(rows).kept_warm_requests == 1).await;
    assert_eq!(inference_rows(&rows).kept_warm_tokens, 2_000);
    assert!(row(&r, &here).await.kept_warm);
}

/// A probe answer that reports only the aggregate cache-write field is charged for that write: the relay's
/// own parser reads the field as five-minute writes, and the cap has to see the same charge.
#[tokio::test]
async fn an_aggregate_only_cache_write_reaches_the_cap() {
    let mut r = rig(RigSpec {
        policy: KeepalivePolicy {
            probe_cost_cap_usd_per_day: Some(0.0005),
            ..RigSpec::default().policy
        },
        ..RigSpec::default()
    })
    .await;
    let body = priced_body();
    r.up.push_credential_rule(A, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    r.up.push_credential_rule(A, "/v1/messages", Respond::Json { status: 200, headers: vec![], body: br#"{"id":"m","type":"message","content":[],"usage":{"input_tokens":0,"cache_read_input_tokens":2000,"cache_creation_input_tokens":500,"output_tokens":0}}"#.to_vec() });
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 1);
    // Sonnet 5: 2,000 read at 0.2 and 500 written at 2.5 per million.
    let expected = (2_000.0 * 0.2 + 500.0 * 2.5) / 1_000_000.0;
    let rows = rollups(&r, |rows| probe_rows(rows).probe_cost_usd > 0.0).await;
    let probe = probe_rows(&rows);
    assert!(
        (probe.probe_cost_usd - expected).abs() < 1e-12,
        "{probe:?} against {expected}"
    );
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        (r.status("main").probes, r.status("main").reason),
        (1, Reason::CapReached),
        "one aggregate-only write spent the day's cap"
    );
}

/// Waits for the router to have applied a report this test just sent it.
async fn settled<'a, F, Fut>(r: &'a Rig, check: F)
where
    F: Fn(&'a Rig) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !check(r).await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the router never applied the report"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

const OAUTH_SECRET: &str = "sk-ant-oat01-OOOO";

fn subscription_spec() -> RigSpec {
    RigSpec {
        accounts: vec![oauth("o", OAUTH_SECRET, 0)],
        limits: KeepaliveLimits {
            oauth_prewarm_verified: true,
            ..KeepaliveLimits::default()
        },
        pool: PoolSpec {
            overage_policy: phoebis_store::OveragePolicy::Allow,
            ..PoolSpec::default()
        },
        tier: Some("default_claude_max_5x"),
        ..RigSpec::default()
    }
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
    let before = r.status("main").net_usd;
    r.up.push_credential_rule(A, "/v1/messages", warm_answer());
    let mut auxiliary = r.request_with("main", &body);
    relay_common::set_header(&mut auxiliary, "x-claude-code-request-class", "title");
    let resp = r.h.app.clone().oneshot(auxiliary).await.unwrap();
    assert_eq!(resp.status(), 200);
    let aux_id = relay_common::outcome(&resp).request_id;
    let _ = resp.into_body().collect().await.unwrap();
    r.pump(Some(&aux_id)).await;
    assert_eq!(
        r.status("main").net_usd,
        before,
        "an auxiliary request never re-armed, so it earns nothing"
    );
    assert!(!row(&r, &aux_id).await.kept_warm);
    r.up.push_credential_rule(A, "/v1/messages", warm_answer());
    let credited = r.observe_with("main", &body).await;
    assert!(
        r.status("main").net_usd > before,
        "the next continuing request earns the credit the auxiliary one left behind"
    );
    rollups(&r, |rows| inference_rows(rows).kept_warm_requests == 1).await;
    assert!(row(&r, &credited).await.kept_warm);
}

/// A subscription account's probes and credits are a proxy for quota, priced as a share of its 5-hour window;
/// an api_key account's stay in dollars.
#[tokio::test]
async fn a_subscription_accounts_net_is_in_five_hour_share_and_an_api_key_accounts_in_dollars() {
    let mut r = rig(subscription_spec()).await;
    let body = priced_body();
    r.up.push_credential_rule(OAUTH_SECRET, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 1);
    let after_probe = r.status("main");
    assert_eq!(
        after_probe.net_usd, 0.0,
        "a plan-funded probe costs no dollars"
    );
    assert!(after_probe.net_five_hour_share < 0.0, "{after_probe:?}");
    r.advance(60_000);
    r.up.push_credential_rule(OAUTH_SECRET, "/v1/messages", warm_answer());
    let credited = r.observe_with("main", &body).await;
    let after_credit = r.status("main");
    assert!(
        after_credit.net_five_hour_share > after_probe.net_five_hour_share,
        "{after_credit:?}"
    );
    assert_eq!(after_credit.net_usd, 0.0);
    rollups(&r, |rows| inference_rows(rows).kept_warm_requests == 1).await;
    assert!(row(&r, &credited).await.kept_warm);

    let mut r = rig(RigSpec::default()).await;
    let body = priced_body();
    r.up.push_credential_rule(A, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    r.advance(270_000);
    r.step().await;
    let status = r.status("main");
    assert!(status.net_usd < 0.0, "{status:?}");
    assert_eq!(
        status.net_five_hour_share, 0.0,
        "an api_key account never touches the share balance"
    );
}

/// A subscription agent's two balances move independently of each other, and the stop always judges the
/// balance the next probe would actually be charged against.
#[tokio::test]
async fn plan_traffic_then_metered_traffic_then_a_return_to_plan_keep_two_balances_and_the_stop_reads_the_next_probes_unit(
) {
    let mut r = rig(RigSpec {
        policy: KeepalivePolicy {
            net_negative_grace_probes: 0,
            ..RigSpec::default().policy
        },
        ..subscription_spec()
    })
    .await;
    let body = priced_body();
    // Leg 1: on plan, the probe's cost and the next request's credit both land in the 5-hour share.
    r.up.push_credential_rule(OAUTH_SECRET, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    r.advance(270_000);
    r.step().await;
    let after_plan_probe = r.status("main");
    assert!(
        after_plan_probe.net_five_hour_share < 0.0,
        "{after_plan_probe:?}"
    );
    assert_eq!(after_plan_probe.net_usd, 0.0);
    r.advance(60_000);
    r.up.push_credential_rule(OAUTH_SECRET, "/v1/messages", warm_answer());
    r.observe_with("main", &body).await;
    let after_plan_credit = r.status("main");
    assert!(
        after_plan_credit.net_five_hour_share > after_plan_probe.net_five_hour_share,
        "{after_plan_credit:?}"
    );
    assert_eq!(after_plan_credit.net_usd, 0.0);
    let share_balance = after_plan_credit.net_five_hour_share;

    // Leg 2: the account enters overage; the next probe and credit land in dollars, the share balance stands.
    r.enter_overage("o", r.now() + 10_000_000);
    settled(&r, |r| async move {
        r.h.router
            .overage("o")
            .await
            .unwrap()
            .is_some_and(|e| e.in_use && e.reset.unwrap() > r.now())
    })
    .await;
    r.up.push_credential_rule(OAUTH_SECRET, "/v1/messages", probe_answer(2_000, 0));
    r.advance(270_000);
    r.step().await;
    let after_overage_probe = r.status("main");
    assert!(after_overage_probe.net_usd < 0.0, "{after_overage_probe:?}");
    assert_eq!(after_overage_probe.net_five_hour_share, share_balance);
    // With no grace, the very next due tick holds under the dollar balance alone: a positive share balance
    // never masks it, since the account is in overage and the stop reads the unit the next probe would be
    // charged in.
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        r.status("main").reason,
        Reason::NetNegative,
        "{:?}",
        r.status("main")
    );
    assert_eq!(
        r.status("main").net_five_hour_share,
        share_balance,
        "still positive relative to where it started, and untouched by the dollar-side stop"
    );
    // A real request still reads the held recipe's own cache normally: its credit lands in the unit the
    // withheld probe settled in, dollars here, and leaves the share balance it never touched alone.
    r.up.push_credential_rule(OAUTH_SECRET, "/v1/messages", warm_answer());
    r.observe_with("main", &body).await;
    let after_overage_credit = r.status("main");
    assert!(
        after_overage_credit.net_usd > after_overage_probe.net_usd,
        "the credit for a probe that settled in dollars lands in dollars too: {after_overage_credit:?}"
    );
    assert_eq!(
        after_overage_credit.net_five_hour_share, share_balance,
        "a warm request while in overage leaves the share balance untouched"
    );
    // The credit paid back past zero; one more dollar-funded probe returns the stop before the reset test.
    r.up.push_credential_rule(OAUTH_SECRET, "/v1/messages", cold_answer());
    r.advance(270_000);
    r.step().await;
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        r.status("main").reason,
        Reason::NetNegative,
        "{:?}",
        r.status("main")
    );

    // Leg 3: the overage's reset passes; a real request re-arms, and the next probe settles in the share again.
    r.leave_overage("o");
    settled(&r, |r| async move {
        r.h.router
            .overage("o")
            .await
            .unwrap()
            .is_some_and(|e| e.reset.unwrap() < r.now())
    })
    .await;
    r.up.push_credential_rule(OAUTH_SECRET, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    assert_eq!(
        r.status("main").reason,
        Reason::Scheduled,
        "the reset passing lifts the dollar-side stop"
    );
    r.advance(270_000);
    r.step().await;
    let after_return = r.status("main");
    assert!(
        after_return.net_five_hour_share < share_balance,
        "the next probe's cost landed in the share again: {after_return:?}"
    );
}

/// The funding a probe settles under is the one its own serving attempt captured, whatever the router's
/// evidence says by the time the result is processed.
#[tokio::test]
async fn a_probe_settles_in_the_funding_captured_with_its_serving_attempt() {
    // Dispatched on plan; overage evidence lands before the gated response is released: still the share.
    let mut r = rig(subscription_spec()).await;
    let body = priced_body();
    r.up.push_credential_rule(OAUTH_SECRET, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    r.up.push_credential_rule(
        OAUTH_SECRET,
        "/v1/messages",
        Respond::Gated(Box::new(probe_answer(2_000, 0))),
    );
    r.advance(270_000);
    r.engine.step().await;
    wait_for(Duration::from_secs(5), || {
        (r.up.requests().len() == 2).then_some(())
    })
    .await;
    r.enter_overage("o", r.now() + 10_000_000);
    settled(&r, |r| async move {
        r.h.router
            .overage("o")
            .await
            .unwrap()
            .is_some_and(|e| e.in_use)
    })
    .await;
    r.up.release();
    r.engine.wait_idle().await;
    r.pump(None).await;
    let status = r.status("main");
    assert!(
        status.net_five_hour_share < 0.0 && status.net_usd == 0.0,
        "dispatched before the flip, so it settles in the unit it was dispatched under: {status:?}"
    );
    r.advance(60_000);
    r.up.push_credential_rule(OAUTH_SECRET, "/v1/messages", warm_answer());
    r.observe_with("main", &body).await;
    let credited = r.status("main");
    assert!(
        credited.net_five_hour_share > status.net_five_hour_share && credited.net_usd == 0.0,
        "the credit for a probe settled on plan lands in the share too, whatever the account is now: {credited:?}"
    );

    // The reverse: dispatched in overage; the reset passes before the gated response is released.
    let mut r = rig(subscription_spec()).await;
    r.enter_overage("o", r.now() + 10_000_000);
    settled(&r, |r| async move {
        r.h.router
            .overage("o")
            .await
            .unwrap()
            .is_some_and(|e| e.in_use)
    })
    .await;
    r.up.push_credential_rule(OAUTH_SECRET, "/v1/messages", cold_answer());
    r.observe_with("main", &body).await;
    r.up.push_credential_rule(
        OAUTH_SECRET,
        "/v1/messages",
        Respond::Gated(Box::new(probe_answer(2_000, 0))),
    );
    r.advance(270_000);
    r.engine.step().await;
    wait_for(Duration::from_secs(5), || {
        (r.up.requests().len() == 2).then_some(())
    })
    .await;
    r.leave_overage("o");
    settled(&r, |r| async move {
        r.h.router
            .overage("o")
            .await
            .unwrap()
            .is_some_and(|e| e.reset.unwrap() < r.now())
    })
    .await;
    r.up.release();
    r.engine.wait_idle().await;
    r.pump(None).await;
    let status = r.status("main");
    assert!(
        status.net_usd < 0.0 && status.net_five_hour_share == 0.0,
        "dispatched in overage, so it settles in dollars whatever the reset does meanwhile: {status:?}"
    );
    r.advance(60_000);
    r.up.push_credential_rule(OAUTH_SECRET, "/v1/messages", warm_answer());
    r.observe_with("main", &body).await;
    let credited = r.status("main");
    assert!(
        credited.net_usd > status.net_usd && credited.net_five_hour_share == 0.0,
        "the credit for a probe settled in dollars lands in dollars too, whatever the reset did meanwhile: {credited:?}"
    );
}

const OPUS: &str = "claude-opus-5";

fn opus_body() -> String {
    BODY.replace("claude-sonnet-4-5", OPUS)
}

/// A probe answer that reads the 2,000-token prefix back and bills a million input tokens beside it, so the
/// probe stays covered while its cost is the cost table's own input cell.
fn million_input_answer() -> Respond {
    Respond::Json { status: 200, headers: vec![], body: br#"{"id":"msg_p","type":"message","content":[],"usage":{"input_tokens":1000000,"cache_read_input_tokens":2000,"cache_creation_input_tokens":0,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":0},"output_tokens":0}}"#.to_vec() }
}

/// That probe's share of the Max 5x 5-hour window outside the band: a million input tokens at 0.0841 and 2,000
/// cache reads at 0.00219 per million, both measured cells.
const OPUS_PROBE_SHARE: f64 = 0.0841 + 2_000.0 * 0.00219 / 1_000_000.0;

/// One such probe on account `o` settled at `at`, and the share of the day it spent there.
async fn one_opus_probe(spec: RigSpec, at: i64) -> (Rig, f64) {
    let mut r = rig(spec).await;
    r.clock.store(at, std::sync::atomic::Ordering::SeqCst);
    r.up.push_credential_rule(OAUTH_SECRET, "/v1/messages", cold_answer());
    r.observe_with("main", &opus_body()).await;
    r.up.push_credential_rule(OAUTH_SECRET, "/v1/messages", million_input_answer());
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 1);
    let spent = r
        .engine
        .share_spent_today()
        .get("o")
        .copied()
        .unwrap_or(0.0);
    (r, spent)
}

/// The plan-side cap counts a subscription account's probes as the share of its own 5-hour window they
/// consumed, weighed by the band at each probe's settlement, and stops probes for the day once the share is
/// spent.
#[tokio::test]
async fn the_window_share_cap_stops_probes_for_the_day_and_a_probe_in_the_band_counts_more() {
    let tables = phoebis_ledger::PlanTables::shipped();
    let hour_ms = 3_600_000i64;
    let week: Vec<i64> = (0..7 * 24).map(|h| NOW + h as i64 * hour_ms).collect();
    // The probe settles 270 s after the observation, so both instants are checked there.
    let band_at = |at: i64| tables.cost.band_factor("five_hour", at + 270_000);
    let in_band = *week
        .iter()
        .find(|&&at| band_at(at) > 1.0)
        .expect("the shipped table names a peak band");
    let out_of_band = *week
        .iter()
        .find(|&&at| band_at(at) == 1.0)
        .expect("the shipped table leaves most hours at par");

    let (r, outside) = one_opus_probe(subscription_spec(), out_of_band).await;
    assert!(
        (outside - OPUS_PROBE_SHARE).abs() < 1e-12,
        "a million Opus input tokens on Max 5x outside the band: {outside} against {OPUS_PROBE_SHARE}"
    );
    let status = r.status("main");
    assert!(
        (status.net_five_hour_share + OPUS_PROBE_SHARE).abs() < 1e-12
            && !status.net_five_hour_share_unmeasured
            && status.net_usd == 0.0,
        "the probe's cost is the agent's share balance, measured: {status:?}"
    );

    let (_, inside) = one_opus_probe(subscription_spec(), in_band).await;
    assert!(
        (inside - OPUS_PROBE_SHARE * 1.45).abs() < 1e-12,
        "the same probe inside the band counts 1.45 times: {inside}"
    );

    let (_, on_pro) = one_opus_probe(
        RigSpec {
            tier: Some("default_claude_pro"),
            ..subscription_spec()
        },
        out_of_band,
    )
    .await;
    assert!(
        (on_pro - OPUS_PROBE_SHARE / 0.215).abs() < 1e-12,
        "on Pro the same tokens are that share over Pro's 5-hour factor: {on_pro}"
    );

    let (mut r, spent) = one_opus_probe(
        RigSpec {
            policy: KeepalivePolicy {
                probe_cap_window_share_per_day: Some(0.08),
                ..RigSpec::default().policy
            },
            ..subscription_spec()
        },
        out_of_band,
    )
    .await;
    assert!(
        spent >= 0.08,
        "the first probe alone reaches the cap: {spent}"
    );
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        (r.status("main").probes, r.status("main").reason),
        (1, Reason::CapReached),
        "the day's share reached the cap, so the next probe waits for midnight"
    );
}

/// A real request on `body` re-arms the recipe on its model, and the due probe runs.
async fn probe_on(r: &mut Rig, body: &str) {
    r.up.push_credential_rule(OAUTH_SECRET, "/v1/messages", cold_answer());
    r.observe_with("main", body).await;
    r.advance(270_000);
    r.step().await;
}

/// A share priced through a cell the measurement campaign has not filled marks the agent's balance and the
/// day's budget row, and a measured probe after it clears neither: a figure with one estimate in it is an
/// estimate. The next day's row starts measured; the balance keeps its mark.
#[tokio::test]
async fn a_stand_in_share_marks_the_balance_and_the_days_budget_until_the_day_ends() {
    let mut r = rig(RigSpec {
        // The clock crosses a UTC midnight below, which the idle limit would read as an abandoned session.
        policy: KeepalivePolicy {
            max_idle_seconds: 0,
            ..RigSpec::default().policy
        },
        ..subscription_spec()
    })
    .await;
    let marks = |r: &Rig| {
        let day =
            r.h.store
                .keepalive_budgets(&budget_day(r.now()))
                .unwrap()
                .into_iter()
                .find(|row| row.id == "o")
                .map(|row| row.five_hour_share_unmeasured);
        (r.status("main").net_five_hour_share_unmeasured, day)
    };
    probe_on(&mut r, &opus_body()).await;
    assert_eq!(r.status("main").probes, 1);
    assert_eq!(
        marks(&r),
        (false, Some(false)),
        "an Opus probe reads measured cells alone"
    );
    probe_on(&mut r, &priced_body()).await;
    assert_eq!(r.status("main").probes, 2);
    assert_eq!(
        marks(&r),
        (true, Some(true)),
        "a Sonnet probe reads a cell the campaign has not filled"
    );
    probe_on(&mut r, &opus_body()).await;
    assert_eq!(r.status("main").probes, 3);
    assert_eq!(
        marks(&r),
        (true, Some(true)),
        "a measured probe after it clears neither mark"
    );
    let midnight = (r.now() / 86_400_000 + 1) * 86_400_000;
    r.clock
        .store(midnight + 1_000, std::sync::atomic::Ordering::SeqCst);
    r.step().await;
    assert_eq!(r.status("main").probes, 4, "the Opus recipe probed again");
    assert_eq!(
        marks(&r),
        (true, Some(false)),
        "the new day's row starts measured, and the balance keeps its mark"
    );
}
