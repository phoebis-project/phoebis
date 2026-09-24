//! Savings accounting: a kept-warm request's gross saving, a probe's cost, a migrated probe's exemption, the
//! wasted count settled when a recipe stops, the net that can go negative, and the daily cap.
mod common;
use std::collections::BTreeMap;
use std::time::Duration;

use phoebis_keepalive::{Event, KeepalivePolicy, Reason};
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
    let mut sum = RollupRow::key(0, "", 0, "", "", "", "probe", "", "");
    for row in rows.iter().filter(|r| r.kind == "probe") {
        sum.add(row);
    }
    sum
}

fn inference_rows(rows: &[RollupRow]) -> RollupRow {
    let mut sum = RollupRow::key(0, "", 0, "", "", "", "inference", "", "");
    for row in rows.iter().filter(|r| r.kind == "inference") {
        sum.add(row);
    }
    sum
}

// claude-sonnet-4-5 is not in the shipped price table; the harness serves it under that id alone. Every test
// here serves and requests claude-sonnet-5, which the table prices, so the saving and the cost are numbers the
// assertions can name. The routing snapshot is rebuilt after the served list changes, or the relay would still
// resolve against the old one.
const PRICED: &str = "claude-sonnet-5";

async fn priced(r: &mut Rig) {
    // A manual served row, selected: a sync after the harness's first one would add the id unselected, and an
    // unselected id is not served.
    let served = |id: &str| phoebis_store::ServedModelWrite {
        upstream_id: id.into(),
        selected: true,
        long_context: false,
    };
    for account in ["a", "b"] {
        r.h.store
            .put_served_models(
                account,
                &[
                    served("claude-sonnet-4-5"),
                    served("claude-opus-5"),
                    served(PRICED),
                ],
                "test",
            )
            .unwrap();
    }
    r.h.router.reload().await.unwrap();
}

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
    priced(&mut r).await;
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
async fn a_probe_during_a_long_stream_and_a_shortened_ttl_credit_nothing() {
    // A stream arms the schedule before it completes; a probe that runs while it is open must not credit it.
    let mut r = rig(RigSpec::default()).await;
    priced(&mut r).await;
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
    priced(&mut r).await;
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
            cache_ttl: r#"{"enabled":true,"main":"1h"}"#,
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
    priced(&mut r).await;
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
    priced(&mut r).await;
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
    priced(&mut r).await;
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
    priced(&mut r).await;
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
    priced(&mut r).await;
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
        priced(&mut r).await;
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
        priced(&mut r).await;
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
    priced(&mut r).await;
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
    priced(&mut r).await;
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
    priced(&mut r).await;
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
    // A restart sums the day from the rollups, so the wait names the row that sum reads: the third probe's
    // cost is a record of its own and lands in a batch the three request rows need not share.
    let rows = rollups(&r, |rows| {
        rows.iter().any(|row| {
            row.kind == "probe" && row.hour >= midnight / 1_000 && row.probe_cost_usd > 0.0
        })
    })
    .await;
    assert!(probe_rows(&rows).probe_cost_usd > 0.0);
    let spent = r.engine.load_spent_today().await;
    assert!(
        (spent.get("main").copied().unwrap_or(0.0) - 0.0004).abs() < 1e-9,
        "the row after midnight: {spent:?}"
    );
}

#[tokio::test]
async fn the_canonical_model_prices_a_probe_whatever_the_caller_spelled() {
    // A caller that asks for `Claude-Sonnet-5[1m]` is served under `claude-sonnet-5`: the replay keeps the
    // spelling, and the cost and the rollup key take the canonical id.
    let mut r = rig(RigSpec::default()).await;
    priced(&mut r).await;
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
    priced(&mut r).await;
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
    priced(&mut r).await;
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
                strategy: phoebis_store::Strategy::PriorityRandom,
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
                api_key_sub_strategy: phoebis_store::SubStrategy::PriorityRandom,
                cache_ttl_json: "{}".into(),
                cache_diagnostics: None,
                created_at: NOW,
                updated_at: NOW,
            },
            "test",
        )
        .unwrap();
    r.h.store
        .set_pool_members("warm", &[("a".to_string(), 0)], "test")
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
    priced(&mut r).await;
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
    priced(&mut r).await;
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
    priced(&mut r).await;
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
    priced(&mut r).await;
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
    priced(&mut r).await;
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
