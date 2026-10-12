//! Probes through the relay's failover loop: what a probe carries, what its result does to the schedule, and
//! how a late or a failed one is classified; the Go probe suite.
mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use common::*;
use fake_upstream::{Respond, SseEvent};
use phoebis_bus::Outcome;
use phoebis_keepalive::{
    CompanionState, Event, KeepalivePolicy, KeepaliveTiming, Reason, FIVE_MINUTES_MS, MAIN_AGENT,
    MAX_WORKERS, ONE_HOUR_MS,
};
use phoebis_relay::Accounting;
use phoebis_store::AccountState;

fn json(status: u16, body: &str) -> Respond {
    Respond::Json {
        status,
        headers: vec![],
        body: body.as_bytes().to_vec(),
    }
}

/// A `message_start` phase carrying cache-read evidence for `request_id`, which is how a streamed real request
/// arms a schedule.
fn read_evidence(request_id: &str, read: u64) -> phoebis_bus::Event {
    phoebis_bus::Event::ResponseChunk(phoebis_bus::ResponseChunk {
        request_id: request_id.into(),
        attempt: 1,
        index: 0,
        body: Default::default(),
        phase: Some(phoebis_bus::Phase::MessageStart {
            usage: phoebis_bus::Usage {
                cache_read_tokens: Some(read),
                ..Default::default()
            },
        }),
    })
}

/// The dollars a reader prices the probe rows at, once the probe's own request row has landed with its counts: the
/// wait names the cost rather than a row.
async fn ledger_probe_cost(r: &Rig) -> f64 {
    wait_for(std::time::Duration::from_secs(5), || {
        let rows =
            r.db.read_blocking(|c| {
                phoebis_ledger::read_rollups(
                    c,
                    phoebis_ledger::hour_of(NOW) - 3_600,
                    i64::MAX,
                    &phoebis_ledger::RollupSelect::default(),
                )
            })
            .unwrap();
        let (cost, _) = read_dollars(r, &rows);
        (cost > 0.0).then_some(cost)
    })
    .await
}

/// The bus publishes every caller header outside its denylist and the recipe replays all of it, so a header
/// the client starts sending after this code was written reaches the upstream on a probe exactly as it does
/// on the request. The caller's credential and `accept-encoding` are not among them: the relay attaches the
/// account's own credential, and a probe that asked for a compressed answer could not read its usage back.
#[tokio::test]
async fn a_probe_replays_every_published_header_and_none_of_the_denied_ones() {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    let mut req = r.request("main");
    relay_common::set_header(&mut req, "x-claude-code-future", "1");
    relay_common::set_header(&mut req, "x-foo", "bar");
    let resp = r.h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    let id = relay_common::outcome(&resp).request_id;
    let _ = resp.into_body().collect().await.unwrap();
    r.pump(Some(&id)).await;
    r.advance(270_000);
    r.step().await;
    let sent = r.up.requests();
    assert_eq!(sent.len(), 2, "the real request and one probe");
    for (label, req) in [("request", &sent[0]), ("probe", &sent[1])] {
        let header = |name: &str| {
            req.headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(
            (header("x-claude-code-future"), header("x-foo")),
            (Some("1"), Some("bar")),
            "{label}"
        );
    }
    let probe = sent.last().unwrap();
    let header = |name: &str| {
        probe
            .headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(
        (header("x-api-key"), header("authorization")),
        (Some(A), None),
        "the account's credential, and nothing the caller sent"
    );
    assert!(
        !probe.headers.iter().any(|(_, v)| v.contains(KEY)),
        "the router's own key reaches no probe: {:?}",
        probe.headers
    );
    assert_eq!(
        header("accept-encoding"),
        None,
        "a compressed answer would hand ProbeUsage::parse gzip: {:?}",
        probe.headers
    );
}

/// The relay inflated the request before the bus saw it, so the recipe holds the plain body with no
/// `content-encoding` line and the probe replays it plain.
#[tokio::test]
async fn a_gzip_request_observed_is_replayed_plain_by_its_probe() {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    let compressed = phoebis_transport::encode::gzip(BODY.as_bytes());
    let (parts, _) = r.request("main").into_parts();
    let mut req =
        axum::http::Request::from_parts(parts, axum::body::Body::from(compressed.clone()));
    relay_common::set_header(&mut req, "content-encoding", "gzip");
    relay_common::set_header(&mut req, "content-length", &compressed.len().to_string());
    let resp = r.h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200, "the gzip request is admitted");
    let id = relay_common::outcome(&resp).request_id;
    let _ = resp.into_body().collect().await.unwrap();
    r.pump(Some(&id)).await;
    r.advance(270_000);
    r.step().await;
    let sent = r.up.requests();
    assert_eq!(sent.len(), 2, "the real request and one probe");
    let probe = &sent[1];
    assert!(
        !probe
            .headers
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case("content-encoding")),
        "{:?}",
        probe.headers
    );
    let replayed: serde_json::Value =
        serde_json::from_slice(&probe.body).expect("the probe carries the plain body");
    assert_eq!(
        (
            replayed["messages"][0]["content"].as_str(),
            replayed["max_tokens"].as_u64()
        ),
        (Some("hello"), Some(0)),
        "the recipe's own body, edited for a probe"
    );
    let status = r.status("main");
    assert_eq!(
        (status.probes, status.failures, status.probe_usage.read),
        (1, 0, 2_000),
        "the probe was answered 200 and its usage read"
    );
}

#[tokio::test]
async fn a_probe_preserves_routing_and_constraints() {
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    r.state("main", "idle");
    let lease_before = r.now() + r.lease_ms();
    r.advance_attended(270_000);
    r.step().await;
    let sent = r.up.requests();
    let probe = sent.last().unwrap();
    assert_eq!(sent.len(), 2, "the real request and one probe");
    assert_eq!(
        probe.credential(),
        Some(A),
        "the account's credential, never the router key"
    );
    let header = |name: &str| {
        probe
            .headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(
        (
            header("x-claude-code-session-id"),
            header("x-claude-code-agent-id"),
            header("anthropic-beta")
        ),
        (Some("session"), Some("main"), Some("test-beta"))
    );
    assert!(header("x-keepalive-owner-id").is_none());
    let body = std::str::from_utf8(&probe.body).unwrap();
    assert!(
        body.contains(r#""max_tokens":0"#) && body.contains(r#""stream":false"#),
        "{body}"
    );
    let status = r.status("main");
    assert_eq!(
        (
            status.probes,
            status.probe_usage.read,
            status.probe_usage.output,
            status.reason
        ),
        (1, 2_000, 0, Reason::Scheduled)
    );
    assert_eq!(
        status.next_probe_at,
        Some(r.now() + 270_000),
        "the next probe is due 270 s after the probe's start"
    );
    assert_eq!(
        status.last_activity,
        r.now() - 270_000,
        "a probe does not renew activity"
    );
    assert_eq!(r.engine.sessions()[0].companion, CompanionState::Reporting);
    let ack = r
        .event(Event {
            event: "heartbeat".into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(ack.lease_expires_ms, r.now() + r.lease_ms());
    assert!(
        ack.lease_expires_ms > lease_before,
        "the heartbeats carried the lease past the one the session started with"
    );
    // The ledger has the probe under kind probe with the same session, and the pin is the real request's.
    let rows = wait_for(std::time::Duration::from_secs(5), || {
        let (rows, _) =
            r.db.read_blocking(|c| {
                phoebis_ledger::list_requests(
                    c,
                    &phoebis_ledger::RequestFilter {
                        include_probes: true,
                        ..Default::default()
                    },
                    &phoebis_ledger::LiveKeys::default(),
                    None,
                    10,
                )
            })
            .unwrap();
        (rows.len() == 2).then_some(rows)
    })
    .await;
    let probe_row = rows.iter().find(|(row, _)| row.kind == "probe").unwrap();
    assert_eq!(
        (
            probe_row.0.session_id.as_deref(),
            probe_row.0.origin.as_str(),
            probe_row.1[0].account_id.as_str()
        ),
        (Some("session"), "keepalive", "a")
    );
    let pins = r.h.router.pins().await.unwrap();
    assert_eq!(
        (
            pins.len(),
            pins[0].account_id.as_str(),
            pins[0].last_used_at
        ),
        (1, "a", r.now()),
        "the probe renewed the pin"
    );
}

/// The rig's request with a reduced-shape billing line ahead of the system prompt, carrying the placeholder the
/// client sends wherever its native patch does not run. `2.1.269.2b2` is the suffix the client computes for
/// `hello` under 2.1.269 (units 4, 7 and 20: `o` and two padding zeros), so the version check passes.
fn placeholder_body() -> String {
    BODY.replacen(
        r#""system":[{"type":"text","text":"private prompt""#,
        r#""system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.269.2b2; cc_entrypoint=sdk-cli; cch=00000;"},{"type":"text","text":"private prompt""#,
        1,
    )
}

/// Observes `placeholder_body()` on the main agent, runs the probe its recipe schedules, and returns the probe's
/// body as the fake upstream received it.
async fn probe_of_a_placeholder_request(r: &mut Rig) -> Vec<u8> {
    r.observe_with("main", &placeholder_body()).await;
    r.state("main", "idle");
    r.advance(270_000);
    r.step().await;
    let sent = r.up.requests();
    assert_eq!(sent.len(), 2, "the real request and one probe");
    sent.last().unwrap().body.clone()
}

fn cch_digits(body: &[u8]) -> Option<&[u8]> {
    phoebis_cch::find_cch(body).map(|l| &body[l.digits_start..l.digits_start + 5])
}

/// The recipe keeps the client's placeholder rather than signing it, so the probe reaches the relay as the
/// request did and the relay relays it unsigned, never reading a recipe's hash as the client's.
#[tokio::test]
async fn a_probe_built_from_a_placeholder_request_carries_the_placeholder_and_the_relay_relays_it()
{
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    let probe = probe_of_a_placeholder_request(&mut r).await;
    assert_eq!(
        cch_digits(&probe),
        Some(&b"00000"[..]),
        "the probe carries the placeholder: {}",
        String::from_utf8_lossy(&probe)
    );
    let status = r.status("main");
    assert_eq!((status.probes, status.reason), (1, Reason::Scheduled));
    let rows = wait_for(std::time::Duration::from_secs(5), || {
        let (rows, _) =
            r.db.read_blocking(|c| {
                phoebis_ledger::list_requests(
                    c,
                    &phoebis_ledger::RequestFilter {
                        include_probes: true,
                        ..Default::default()
                    },
                    &phoebis_ledger::LiveKeys::default(),
                    None,
                    10,
                )
            })
            .unwrap();
        (rows.len() == 2).then_some(rows)
    })
    .await;
    let (probe_row, _) = rows.iter().find(|(row, _)| row.kind == "probe").unwrap();
    assert_eq!(
        (probe_row.status, probe_row.client_drift.as_deref()),
        (Some(200), None)
    );
}

/// The decoded `system[0].text` of `body`, its billing line.
fn billing_line(body: &[u8]) -> String {
    let root: serde_json::Value = serde_json::from_slice(body).unwrap();
    root["system"][0]["text"].as_str().unwrap().to_string()
}

/// Signing a placeholder is the relay's call over a line it restored, never the recipe's, so the recipe keeps a
/// client-sent placeholder line as the client wrote it. The relay's side is
/// `a_client_sent_placeholder_on_a_full_shape_line_is_relayed_intact` in the relay's restoration suite.
#[test]
fn the_recipe_keeps_a_client_sent_placeholder_line() {
    let body = placeholder_body();
    let recipe =
        phoebis_keepalive::make_recipe(&[], body.as_bytes(), "claude-sonnet-4-5", 1 << 20).unwrap();
    assert_eq!(
        billing_line(&recipe.body),
        billing_line(body.as_bytes()),
        "the recipe keeps the client's line"
    );
}

#[tokio::test]
async fn a_heartbeat_does_not_reset_idle_or_refresh() {
    let mut r = rig(RigSpec {
        policy: KeepalivePolicy {
            max_idle_seconds: 500,
            ..RigSpec::default().policy
        },
        ..RigSpec::default()
    })
    .await;
    r.observe("main").await;
    r.state("main", "idle");
    let original = r.status("main");
    r.advance_attended(270_000);
    r.step().await;
    r.event(Event {
        event: "heartbeat".into(),
        ..Default::default()
    })
    .unwrap();
    r.advance_attended(270_000);
    r.step().await;
    let status = r.status("main");
    assert_eq!(
        (
            status.probes,
            status.idle_ticks,
            status.last_activity,
            status.reason
        ),
        (1, 1, original.last_activity, Reason::IdleLimit),
        "{status:?}"
    );
}

/// A probe the router refused with `503 ledger_behind` gives back the idle tick its dispatch counted, so a ledger
/// catching up never runs the count up: three refusals in a row and the served probe after them count one tick, and
/// a second served idle probe counts the second. The refusal is the relay's own, under the rig's accounting reading,
/// and the probe counts under its own outcome.
#[tokio::test]
async fn held_re_sends_spend_no_idle_tick_until_the_router_serves_one() {
    let behind = Arc::new(AtomicBool::new(false));
    let reading = behind.clone();
    let mut r = rig(RigSpec {
        accounting: Some(Arc::new(move |_| Accounting {
            behind: reading.load(Ordering::SeqCst),
        })),
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    r.observe("main").await;
    r.state("main", "idle");
    behind.store(true, Ordering::SeqCst);
    r.advance_attended(270_000);
    r.step().await;
    for _ in 0..2 {
        r.advance_attended(5_000);
        r.step().await;
    }
    let status = r.status("main");
    assert_eq!(
        (status.probes, status.idle_ticks, status.reason),
        (3, 0, Reason::LedgerBehind),
        "three refusals in a row spent no tick: {status:?}"
    );
    let text = r
        .metrics
        .render(&phoebis_ledger::RoutingGauges::default(), &[], 0);
    assert!(
        text.contains(r#"phoebis_keepalive_probes_total{outcome="held",pool="main"} 3"#)
            && !text.contains(r#"outcome="failed""#),
        "each refusal counts under its own outcome and none as failed: {text}"
    );
    behind.store(false, Ordering::SeqCst);
    r.advance_attended(5_000);
    r.step().await;
    let status = r.status("main");
    assert_eq!(
        (status.probes, status.idle_ticks, status.reason),
        (4, 1, Reason::Scheduled),
        "the served probe counts the one tick: {status:?}"
    );
    r.advance_attended(270_000);
    r.step().await;
    let status = r.status("main");
    assert_eq!(
        (status.probes, status.idle_ticks, status.reason),
        (5, 2, Reason::Scheduled),
        "a second served idle probe counts the second tick: {status:?}"
    );
    assert_eq!(
        r.up.requests().len(),
        3,
        "the arming request and the two served probes went out; the refusals stopped at the router"
    );
}

/// A hold gives back only the tick its own dispatch counted: a `state` report moves the main agent off idle without
/// resetting the ticks its idle probes counted, and a probe the router refuses while the agent is busy counted none,
/// so the ticks stay where the idle probes left them.
#[tokio::test]
async fn a_hold_while_the_main_agent_is_busy_gives_back_no_tick_its_dispatch_never_counted() {
    let behind = Arc::new(AtomicBool::new(false));
    let reading = behind.clone();
    let mut r = rig(RigSpec {
        accounting: Some(Arc::new(move |_| Accounting {
            behind: reading.load(Ordering::SeqCst),
        })),
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    r.observe("main").await;
    r.state("main", "idle");
    for _ in 0..2 {
        r.advance_attended(270_000);
        r.step().await;
    }
    assert_eq!(
        (r.status("main").probes, r.status("main").idle_ticks),
        (2, 2),
        "two served idle probes count two ticks"
    );
    r.state("main", "busy");
    behind.store(true, Ordering::SeqCst);
    r.advance_attended(270_000);
    r.step().await;
    r.advance_attended(5_000);
    r.step().await;
    let status = r.status("main");
    assert_eq!(
        (status.probes, status.idle_ticks, status.reason),
        (4, 2, Reason::LedgerBehind),
        "two holds while busy leave the idle probes' ticks alone: {status:?}"
    );
}

#[tokio::test]
async fn a_lapsed_lease_keeps_probing_under_the_idle_limit_and_a_release_discards_at_once() {
    // The lapse case: the recipe stays, the probe at 270 s still runs, a heartbeat is refused, and the idle
    // limit counted from the last real request ends it with the wasted label idle_limit.
    let mut r = rig(RigSpec {
        policy: KeepalivePolicy {
            max_idle_seconds: 500,
            ..RigSpec::default().policy
        },
        ..RigSpec::default()
    })
    .await;
    r.observe("main").await;
    r.advance(90_000);
    r.step().await;
    assert!(
        r.engine.stats().retained_bytes > 0,
        "a lapsed lease keeps the recipe"
    );
    assert_eq!(
        r.engine.sessions()[0].companion,
        CompanionState::Silent,
        "the lapse leaves the session unattended"
    );
    assert_eq!(
        r.engine
            .apply_event(
                "desktop",
                r.generation("desktop"),
                Event {
                    event: "heartbeat".into(),
                    session_id: "session".into(),
                    instance_id: "instance".into(),
                    sequence: 99,
                    ..Default::default()
                }
            )
            .unwrap_err()
            .code,
        "instance_ended"
    );
    r.advance(180_000);
    r.step().await;
    assert_eq!(
        r.status("main").probes,
        1,
        "probed as the idle main agent while unattended"
    );
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        (
            r.status("main").probes,
            r.status("main").reason,
            r.engine.stats().retained_bytes
        ),
        (1, Reason::IdleLimit, 0),
        "540 s past the last real request, over the 500 s limit"
    );
    // The release case: nothing revives a released agent, and the prompt is gone at once.
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    r.state("main", "released");
    assert_eq!(r.engine.stats().retained_bytes, 0);
    r.engine.observe(r.observed("late", "main", BODY));
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        (r.status("main").probes, r.engine.stats().retained_bytes),
        (0, 0)
    );
}

#[tokio::test]
async fn a_late_probe_cannot_revive_a_new_generation() {
    for action in ["new_request", "close", "release"] {
        let mut r = rig(RigSpec::default()).await;
        r.observe("main").await;
        r.up.push_credential_rule(
            A,
            "/v1/messages",
            Respond::Gated(Box::new(probe_answer(2_000, 0))),
        );
        r.advance_attended(270_000);
        r.engine.step().await;
        wait_for(std::time::Duration::from_secs(5), || {
            (r.up.requests().len() == 2).then_some(())
        })
        .await;
        match action {
            "new_request" => r.engine.observe(r.observed("new", "main", BODY)),
            "close" => r.engine.close(),
            _ => r.state("main", "released"),
        }
        r.up.release();
        r.engine.wait_idle().await;
        assert_eq!(
            r.engine.stats().running_probes,
            0,
            "{action}: no worker leaked"
        );
        if action != "close" {
            let status = r.status("main");
            assert_eq!(
                (
                    status.account_id.as_deref(),
                    status.next_probe_at,
                    status.unanswered
                ),
                (Some("a"), None, 0),
                "{action}: the late result touched no recipe state: {status:?}"
            );
            assert!(
                !status.in_flight,
                "{action}: the orphaned result gave the record's slot back"
            );
        } else {
            assert_eq!(r.engine.stats().retained_bytes, 0);
        }
        // The record the orphaned probe left behind is probed again once the new request arms it: a result that
        // failed the fence must not strand the agent it started on.
        if action == "new_request" {
            r.engine.observe(r.selected("new", "a"));
            r.engine.observe(read_evidence("new", 2_000));
            assert!(
                r.status("main").next_probe_at.is_some(),
                "the new request armed the record"
            );
            r.advance(270_000);
            r.step().await;
            assert_eq!(
                (r.status("main").probes, r.status("main").reason),
                (2, Reason::Scheduled),
                "the re-armed record probes again"
            );
        }
    }
}

/// A probe whose recipe a release stopped while it was in flight was still served and its ledger row carries its
/// cost, so with the released record still in the session it settles as wasted at that cost under the release.
#[tokio::test]
async fn an_orphaned_probe_whose_agent_survives_settles_under_its_stop() {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    r.observe("main").await;
    r.up.push_credential_rule(
        A,
        "/v1/messages",
        Respond::Gated(Box::new(probe_answer(2_000, 0))),
    );
    r.advance_attended(270_000);
    r.engine.step().await;
    wait_for(std::time::Duration::from_secs(5), || {
        (r.up.requests().len() == 2).then_some(())
    })
    .await;
    r.state("main", "released");
    r.up.release();
    r.engine.wait_idle().await;
    r.pump(None).await;
    let charged = ledger_probe_cost(&r).await;
    let status = r.status("main");
    assert_eq!(
        (status.in_flight, status.next_probe_at, status.unanswered),
        (false, None, 0),
        "the late result touched no recipe state: {status:?}"
    );
    let (wasted, wasted_usd, reasons) = wait_for(std::time::Duration::from_secs(5), || {
        let rows =
            r.db.read_blocking(|c| {
                phoebis_ledger::read_rollups(
                    c,
                    phoebis_ledger::hour_of(NOW) - 3_600,
                    i64::MAX,
                    &phoebis_ledger::RollupSelect::default(),
                )
            })
            .unwrap();
        let mut sum = (
            0,
            read_dollars(&r, &rows).1,
            std::collections::BTreeMap::new(),
        );
        for row in rows.iter().filter(|row| row.kind == "probe") {
            sum.0 += row.wasted_probes;
            for (reason, n) in &row.wasted_probe_reasons {
                *sum.2.entry(reason.clone()).or_insert(0) += n;
            }
        }
        (sum.0 > 0).then_some(sum)
    })
    .await;
    assert_eq!(
        (wasted, reasons),
        (
            1,
            std::collections::BTreeMap::from([("released".to_string(), 1)])
        ),
        "the orphaned probe settles under the release that left it"
    );
    assert!(
        (wasted_usd - charged).abs() < 1e-12,
        "at the cost its ledger row carries: {wasted_usd} against {charged}"
    );
}

/// A probe still in flight when the idle limit ends its recipe joins the probes the ceiling kept once it is answered:
/// it settles nothing as it lands, waits in the kept list under the pending label `idle_limit` with the funding its
/// answer carried and the entry it wrote, and settles under that label, at its cost, on the tick its entry ends.
#[tokio::test]
async fn a_probe_answered_after_the_idle_limit_stays_pending_until_its_entry_lapses() {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        policy: KeepalivePolicy {
            max_idle_seconds: 500,
            ..RigSpec::default().policy
        },
        ..RigSpec::default()
    })
    .await;
    r.observe("main").await;
    r.up.push_credential_rule(
        A,
        "/v1/messages",
        Respond::Gated(Box::new(probe_answer(2_000, 0))),
    );
    r.advance(270_000);
    r.engine.step().await;
    wait_for(std::time::Duration::from_secs(5), || {
        (r.up.requests().len() == 2).then_some(())
    })
    .await;
    // 540 s past the last real request, over the 500 s limit, with the probe still in flight.
    r.advance(270_000);
    r.engine.step().await;
    assert_eq!(
        (r.status("main").reason, r.engine.stats().retained_bytes),
        (Reason::IdleLimit, 0),
        "the ceiling ended the recipe while its probe was in flight"
    );
    r.up.release();
    r.engine.wait_idle().await;
    r.pump(None).await;
    let charged = ledger_probe_cost(&r).await;
    let (_, agents) = r.engine.persist_rows();
    let payload: serde_json::Value = serde_json::from_str(&agents[0].payload).unwrap();
    let kept = &payload["kept"];
    // The probe started 270 s in and its answer kept the five-minute entry the real request proved.
    assert_eq!(
        (
            kept.as_array().map(Vec::len),
            kept[0]["pending"].as_str(),
            kept[0]["kind"].as_str(),
            kept[0]["in_overage"].as_bool(),
            kept[0]["entry_expires"].as_i64(),
            kept[0]["covered"].as_bool(),
        ),
        (
            Some(1),
            Some("idle_limit"),
            Some("api_key"),
            Some(false),
            Some(NOW + 570_000),
            Some(false)
        ),
        "the late answer waits as a pending probe: {kept}"
    );
    r.advance(NOW + 570_000 - 1 - r.now());
    r.step().await;
    assert_eq!(
        r.status("main").unanswered,
        1,
        "still pending a millisecond before its entry ends"
    );
    r.advance(1);
    r.step().await;
    assert_eq!(r.status("main").unanswered, 0, "settled at its entry's end");
    // The settlement reaches the ledger with the save that drops the probe, which the production flusher runs.
    r.engine.flush_persisted().await;
    r.ledger.clone().shutdown().await.unwrap();
    let rows =
        r.db.read_blocking(|c| {
            phoebis_ledger::read_rollups(
                c,
                phoebis_ledger::hour_of(NOW) - 3_600,
                i64::MAX,
                &phoebis_ledger::RollupSelect::default(),
            )
        })
        .unwrap();
    let mut reasons = std::collections::BTreeMap::new();
    let (_, wasted_usd) = read_dollars(&r, &rows);
    for row in rows.iter().filter(|row| row.kind == "probe") {
        for (reason, n) in &row.wasted_probe_reasons {
            *reasons.entry(reason.clone()).or_insert(0) += n;
        }
    }
    assert_eq!(
        reasons,
        std::collections::BTreeMap::from([("idle_limit".to_string(), 1)]),
        "one settlement, at the lapse, under the ceiling's label"
    );
    assert!(
        (wasted_usd - charged).abs() < 1e-12,
        "at the cost its ledger row carries: {wasted_usd} against {charged}"
    );
}

/// A release stops the recipe while its probe is in flight, and the mod starts the agent again before the probe
/// answers. The late result still settles under the release, once, and leaves the revived agent's bookkeeping alone:
/// the worker slot comes back, no probe is left unanswered, and the next request arms the record again.
#[tokio::test]
async fn a_probe_orphaned_by_a_release_and_then_a_revival_settles_under_the_release() {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    r.observe("main").await;
    r.up.push_credential_rule(
        A,
        "/v1/messages",
        Respond::Gated(Box::new(probe_answer(2_000, 0))),
    );
    r.advance_attended(270_000);
    r.engine.step().await;
    wait_for(std::time::Duration::from_secs(5), || {
        (r.up.requests().len() == 2).then_some(())
    })
    .await;
    r.state("main", "released");
    let generation = r.agent("main").request_generation;
    r.event(Event {
        event: "agent_start".into(),
        agent_id: Some("main".into()),
        request_generation: Some(generation),
        ..Default::default()
    })
    .expect("a released agent starts again under the generation its release named");
    r.up.release();
    r.engine.wait_idle().await;
    r.pump(None).await;
    let status = r.status("main");
    assert_eq!(
        (status.in_flight, status.unanswered),
        (false, 0),
        "the late result gave the slot back and kept nothing: {status:?}"
    );
    r.observe("main").await;
    assert!(
        r.status("main").next_probe_at.is_some(),
        "the next request arms the revived record"
    );
    let reasons = wait_for(std::time::Duration::from_secs(5), || {
        let rows =
            r.db.read_blocking(|c| {
                phoebis_ledger::read_rollups(
                    c,
                    phoebis_ledger::hour_of(NOW) - 3_600,
                    i64::MAX,
                    &phoebis_ledger::RollupSelect::default(),
                )
            })
            .unwrap();
        let mut reasons = std::collections::BTreeMap::new();
        for row in rows.iter().filter(|row| row.kind == "probe") {
            for (reason, n) in &row.wasted_probe_reasons {
                *reasons.entry(reason.clone()).or_insert(0) += n;
            }
        }
        (!reasons.is_empty()).then_some(reasons)
    })
    .await;
    assert_eq!(
        reasons,
        std::collections::BTreeMap::from([("released".to_string(), 1)]),
        "the orphaned probe settles once, under the release that left it"
    );
}

#[tokio::test]
async fn a_replacement_registration_during_a_probe_keeps_the_new_agents_in_flight_flag() {
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    r.up.push_credential_rule(
        A,
        "/v1/messages",
        Respond::Gated(Box::new(probe_answer(2_000, 0))),
    );
    r.advance(270_000);
    r.engine.step().await;
    wait_for(std::time::Duration::from_secs(5), || {
        (r.up.requests().len() == 2).then_some(())
    })
    .await;
    assert!(r.status("main").in_flight);
    // A second instance takes the session: a new main agent under the same ids, its own recipe, its own probe.
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
    r.observe("main").await;
    r.up.push_credential_rule(
        A,
        "/v1/messages",
        Respond::Gated(Box::new(probe_answer(2_000, 0))),
    );
    r.advance(270_000);
    r.engine.step().await;
    wait_for(std::time::Duration::from_secs(5), || {
        (r.up.requests().len() == 4).then_some(())
    })
    .await;
    assert!(
        r.status("main").in_flight,
        "the new agent's probe is in flight"
    );
    // Both gated probes answer; the old worker's result must leave the new agent's bookkeeping alone.
    r.up.release();
    r.engine.wait_idle().await;
    let status = r.status("main");
    assert_eq!(
        (status.in_flight, status.probes, status.unanswered),
        (false, 1, 1),
        "one probe on the new record: {status:?}"
    );
}

#[tokio::test]
async fn an_oversized_or_stalled_probe_body_releases_the_worker_and_shutdown_finishes() {
    // A body that crosses the bound without ending: the read stops at the first frame past it.
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    let oversized = format!(
        r#"{{"id":"m","type":"message","content":[],"usage":{{"cache_read_input_tokens":2000,"output_tokens":0}},"pad":"{}"}}"#,
        "x".repeat(300 << 10)
    );
    r.up.push_credential_rule(
        A,
        "/v1/messages",
        Respond::Chunked {
            status: 200,
            headers: vec![("content-type".into(), "application/json".into())],
            body: oversized.into_bytes(),
        },
    );
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        (
            r.engine.stats().running_probes,
            r.status("main").reason,
            r.status("main").next_probe_at
        ),
        (0, Reason::InvalidProbeUsage, None)
    );
    // A head followed by nothing: the deadline ends the exchange and the worker comes back.
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    r.up.push_credential_rule(
        A,
        "/v1/messages",
        Respond::Stall {
            status: 200,
            headers: vec![],
            events: vec![],
        },
    );
    r.advance(270_000);
    r.engine.step().await;
    wait_for(std::time::Duration::from_secs(5), || {
        (r.up.requests().len() == 2).then_some(())
    })
    .await;
    assert_eq!(r.engine.stats().running_probes, 1);
    tokio::time::pause();
    tokio::time::advance(phoebis_keepalive::PROBE_DEADLINE + std::time::Duration::from_secs(1))
        .await;
    tokio::time::resume();
    r.engine.wait_idle().await;
    r.pump(None).await;
    assert_eq!(
        (r.status("main").reason, r.status("main").next_probe_at),
        (Reason::ProbeTransportError, None)
    );
    let text = r
        .metrics
        .render(&phoebis_ledger::RoutingGauges::default(), &[], 0);
    assert!(
        text.contains(r#"phoebis_keepalive_probes_total{outcome="failed",pool="main"} 1"#),
        "a probe that never answered counts as failed under its pool: {text}"
    );
    r.up.release();
    // Shutdown with a stalled probe in flight ends inside the drain window: the cancellation wins the select.
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    r.up.push_credential_rule(
        A,
        "/v1/messages",
        Respond::Stall {
            status: 200,
            headers: vec![],
            events: vec![],
        },
    );
    r.advance(270_000);
    r.engine.step().await;
    wait_for(std::time::Duration::from_secs(5), || {
        (r.up.requests().len() == 2).then_some(())
    })
    .await;
    let done = tokio::time::timeout(std::time::Duration::from_secs(5), r.engine.shutdown()).await;
    assert!(done.is_ok(), "shutdown waited on a stalled probe");
    assert_eq!(r.engine.stats().running_probes, 0);
    r.up.release();
}

#[tokio::test]
async fn concurrency_is_bounded_by_max_workers_alone() {
    let mut r = rig(RigSpec::default()).await;
    // One due agent more than there are workers.
    let mut agents = vec![MAIN_AGENT.to_string()];
    for n in 1..=MAX_WORKERS {
        let agent = format!("worker-{n}");
        r.event(Event {
            event: "agent_start".into(),
            agent_id: Some(agent.clone()),
            ..Default::default()
        })
        .unwrap();
        agents.push(agent);
    }
    for agent in &agents {
        r.observe(agent).await;
    }
    for _ in 0..MAX_WORKERS {
        r.up.push_credential_rule(
            A,
            "/v1/messages",
            Respond::Gated(Box::new(probe_answer(2_000, 0))),
        );
    }
    let probes = |r: &Rig| {
        agents
            .iter()
            .map(|agent| r.status(agent).probes)
            .sum::<u64>()
    };
    r.advance(270_000);
    r.engine.step().await;
    wait_for(std::time::Duration::from_secs(5), || {
        (r.up.requests().len() == agents.len() + MAX_WORKERS).then_some(())
    })
    .await;
    r.engine.step().await;
    assert_eq!(
        (r.engine.stats().running_probes, probes(&r)),
        (MAX_WORKERS, MAX_WORKERS as u64),
        "every worker holds a probe in flight, and the fifth due agent waits"
    );
    r.up.release();
    r.engine.wait_idle().await;
    r.step().await;
    assert_eq!(probes(&r), agents.len() as u64, "the capacity recovered");
}

#[tokio::test]
async fn a_migration_does_not_accumulate_a_cold_streak() {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0), api_key("b", B, 0)],
        timing: KeepaliveTiming {
            cold_probe_limit: 3,
            ..RigSpec::default().timing
        },
        ..RigSpec::default()
    })
    .await;
    r.observe("main").await;
    let first = r.status("main").account_id.clone().unwrap();
    let (mine, other) = if first == "a" { (A, B) } else { (B, A) };
    // Probe 1: the pinned account rejects, the sibling serves it cold (a write, no read): migrated.
    // Probes 2 and 3: cold on the new account. Probe 3 reaches the limit.
    r.up.push_credential_rule(
        mine,
        "/v1/messages",
        relay_common::json_429_unified(4_600, "five_hour"),
    );
    r.up.push_credential_rule(other, "/v1/messages", probe_answer(0, 2_000));
    r.up.push_credential_rule(other, "/v1/messages", probe_answer(0, 2_000));
    r.up.push_credential_rule(other, "/v1/messages", probe_answer(0, 2_000));
    for _ in 0..3 {
        r.advance(270_000);
        r.step().await;
    }
    let status = r.status("main");
    assert_eq!(
        (
            status.migrations,
            status.probes,
            status.cold,
            status.reason,
            status.probe_usage.write
        ),
        (1, 3, 2, Reason::Scheduled, 6_000),
        "{status:?}"
    );
    let text = r
        .metrics
        .render(&phoebis_ledger::RoutingGauges::default(), &[], 0);
    assert!(
        text.contains(r#"phoebis_keepalive_probes_total{outcome="landed",pool="main"} 1"#),
        "the migrated probe's write covers the prefix on its new account: {text}"
    );
    assert!(
        text.contains(r#"phoebis_keepalive_probes_total{outcome="cold",pool="main"} 2"#),
        "the two probes after it read nothing back: {text}"
    );
    assert_eq!(
        status.account_id.as_deref(),
        Some(if first == "a" { "b" } else { "a" })
    );
    r.up.push_credential_rule(other, "/v1/messages", probe_answer(0, 2_000));
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        (
            r.status("main").cold,
            r.status("main").reason,
            r.status("main").next_probe_at
        ),
        (3, Reason::RepeatedColdProbe, None)
    );
}

/// The series of one family in a scrape of the rig's metrics, in the order the scrape renders them.
fn series(r: &Rig, family: &str) -> Vec<String> {
    r.metrics
        .render(&phoebis_ledger::RoutingGauges::default(), &[], 0)
        .lines()
        .filter(|line| line.starts_with(&format!("{family}{{")))
        .map(str::to_string)
        .collect()
}

/// A probe counts once under its pool: `landed` when it read the prefix back, `cold` when the upstream served it
/// without that read, and `failed` when the upstream did not serve it.
#[tokio::test]
async fn each_probe_counts_once_under_its_pool_as_landed_cold_or_failed() {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    r.observe("main").await;
    // The standing answer reads the whole prefix back.
    r.advance(270_000);
    r.step().await;
    r.up.push_credential_rule(A, "/v1/messages", probe_answer(0, 2_000));
    r.advance(270_000);
    r.step().await;
    r.up.push_credential_rule(
        A,
        "/v1/messages",
        json(
            503,
            r#"{"type":"error","error":{"type":"api_error","message":"x"}}"#,
        ),
    );
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        (r.status("main").probes, r.status("main").reason),
        (3, Reason::RetryableProbeError)
    );
    assert_eq!(
        series(&r, "phoebis_keepalive_probes_total"),
        [
            r#"phoebis_keepalive_probes_total{outcome="cold",pool="main"} 1"#,
            r#"phoebis_keepalive_probes_total{outcome="failed",pool="main"} 1"#,
            r#"phoebis_keepalive_probes_total{outcome="landed",pool="main"} 1"#,
        ]
    );
}

/// A probe a gate withholds counts once under its pool and the state the hold leaves the recipe in, however many ticks
/// the hold spans, and a probe a lapsed entry withholds counts under `ended`, the state the lapse ends the recipe in.
#[tokio::test]
async fn a_withheld_probe_counts_under_its_pool_and_the_state_that_withheld_it() {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    r.observe("main").await;
    over_budget(&r).await;
    r.advance(270_000);
    r.step().await;
    r.advance(1_000);
    r.step().await;
    assert_eq!(
        (r.status("main").reason, r.status("main").probes),
        (Reason::Budget, 0)
    );
    assert_eq!(
        series(&r, "phoebis_keepalive_skipped_total"),
        [r#"phoebis_keepalive_skipped_total{pool="main",state="held"} 1"#]
    );
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    r.advance(20 * MINUTE);
    r.step().await;
    assert_eq!(r.status("main").reason, Reason::Expired);
    assert_eq!(
        series(&r, "phoebis_keepalive_skipped_total"),
        [r#"phoebis_keepalive_skipped_total{pool="main",state="ended"} 1"#]
    );
}

/// A probe that read nothing and rewrote the whole prefix covers from its own write, and the cold streak still
/// counts it: a rewrite every probe is a prefix never kept warm, so two in a row pause the recipe at the limit.
#[tokio::test]
async fn a_rewrite_still_counts_toward_the_cold_probe_limit() {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        timing: KeepaliveTiming {
            cold_probe_limit: 2,
            ..RigSpec::default().timing
        },
        ..RigSpec::default()
    })
    .await;
    r.observe("main").await;
    r.up.push_credential_rule(A, "/v1/messages", probe_answer(0, 2_000));
    r.up.push_credential_rule(A, "/v1/messages", probe_answer(0, 2_000));
    r.advance(270_000);
    r.step().await;
    let status = r.status("main");
    assert_eq!(
        (status.probes, status.cold, status.reason),
        (1, 1, Reason::Scheduled),
        "the first rewrite lands and counts one: {status:?}"
    );
    r.advance(270_000);
    r.step().await;
    let status = r.status("main");
    assert_eq!(
        (
            status.probes,
            status.unanswered,
            status.cold,
            status.reason,
            status.next_probe_at
        ),
        (2, 2, 2, Reason::RepeatedColdProbe, None),
        "the second rewrite reaches the limit: {status:?}"
    );
}

#[tokio::test]
async fn probe_outcomes_are_classified() {
    for (name, respond, reason, armed) in [
        (
            "missing_usage",
            json(200, r#"{"id":"m","type":"message","content":[]}"#),
            Reason::InvalidProbeUsage,
            false,
        ),
        (
            "output",
            json(
                200,
                r#"{"id":"m","type":"message","content":[],"usage":{"cache_read_input_tokens":2000,"output_tokens":1}}"#,
            ),
            Reason::UnexpectedOutput,
            false,
        ),
        (
            "partial",
            probe_answer(100, 0),
            Reason::PartialCacheCoverage,
            false,
        ),
        (
            "server",
            json(
                503,
                r#"{"type":"error","error":{"type":"api_error","message":"x"}}"#,
            ),
            Reason::RetryableProbeError,
            true,
        ),
        (
            "unsupported",
            json(
                400,
                r#"{"type":"error","error":{"type":"invalid_request_error","message":"x"}}"#,
            ),
            Reason::UnsupportedProbe,
            false,
        ),
    ] {
        let mut r = rig(RigSpec::default()).await;
        r.observe("main").await;
        r.up.push_credential_rule(A, "/v1/messages", respond);
        r.advance(270_000);
        r.step().await;
        let status = r.status("main");
        assert_eq!(
            (name, status.reason, status.next_probe_at.is_some()),
            (name, reason, armed),
            "{status:?}"
        );
    }
    // An upstream 401 never reaches the engine as a 401: the relay marks the account, fails over, and with no
    // account left answers the untimed exhaustion 503 naming the mark, which pauses under the authentication
    // reason.
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    r.observe("main").await;
    r.up.push_credential_rule(
        A,
        "/v1/messages",
        json(
            401,
            r#"{"type":"error","error":{"type":"authentication_error","message":"x"}}"#,
        ),
    );
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        (r.status("main").reason, r.status("main").next_probe_at),
        (Reason::ProbeAuthenticationError, None)
    );
    // A 429 on every account is the relay's exhaustion 429: the same pause a single 429 after failover gets.
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    r.up.push_credential_rule(
        A,
        "/v1/messages",
        relay_common::json_429_unified(4_600, "five_hour"),
    );
    r.up.push_credential_rule(
        B,
        "/v1/messages",
        relay_common::json_429_unified(4_600, "five_hour"),
    );
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        (r.status("main").reason, r.status("main").next_probe_at),
        (Reason::ProbeQuotaExhausted, None)
    );
    r.advance(5_000_000);
    r.step().await;
    assert_eq!(
        r.engine
            .reasons()
            .get(&Reason::ProbeQuotaExhausted)
            .copied(),
        Some(1),
        "no probe ran again after a quota pause"
    );
    r.observe("main").await;
    assert_eq!(
        r.status("main").reason,
        Reason::Scheduled,
        "a real request re-arms the agent"
    );
}

/// `a` alone at a monthly budget of 5.0, with 5.01 of priced spend the router's view of `a` reads, so the next tick's
/// gate holds a probe pinned to `a` under `budget`.
async fn over_budget(r: &Rig) {
    r.h.store
        .set_pool_members(
            "main",
            &[phoebis_store::MemberWrite {
                account_id: "a".into(),
                priority: 0,
                budget_daily_usd: None,
                budget_monthly_usd: Some(5.0),
            }],
            "test",
        )
        .unwrap();
    r.h.router.reload().await.unwrap();
    plant_spend(r, "over-budget", 5.01).await;
}

/// An exhaustion pauses the recipe until the next real request, while a gate hold keeps it armed for the tick that
/// finds the hold gone: the `paused` bit tells the two apart, whatever the reason reads.
#[tokio::test]
async fn a_probe_exhaustion_pauses_the_recipe_and_a_gate_hold_leaves_it_armed() {
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    for secret in [A, B] {
        r.up.push_credential_rule(
            secret,
            "/v1/messages",
            relay_common::json_429_unified(4_600, "five_hour"),
        );
    }
    r.advance(270_000);
    r.step().await;
    let status = r.status("main");
    assert_eq!(
        (status.paused, status.next_probe_at, status.reason),
        (true, None, Reason::ProbeQuotaExhausted),
        "{status:?}"
    );
    // Past the accounts' reset the next real request selects one again.
    r.advance(5_000_000);
    r.observe("main").await;
    let status = r.status("main");
    assert_eq!(
        (status.paused, status.reason),
        (false, Reason::Scheduled),
        "{status:?}"
    );
    assert!(
        status.next_probe_at.is_some_and(|at| at > r.now()),
        "the next real request re-arms a probe: {status:?}"
    );
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    r.observe("main").await;
    over_budget(&r).await;
    r.advance(270_000);
    r.step().await;
    let status = r.status("main");
    assert_eq!(
        (status.paused, status.reason, status.probes),
        (false, Reason::Budget, 0),
        "{status:?}"
    );
    assert!(
        status.next_probe_at.is_some(),
        "the schedule is kept for the tick that finds the budget gone: {status:?}"
    );
}

/// A pinned account that disconnects between admission and the probe's own selection leaves the pool with
/// nothing to serve: the relay answers its untimed exhaustion, and the pause reads that reason rather than the
/// authentication one a marked account gets.
#[tokio::test]
async fn an_exhaustion_503_pauses_under_its_own_reason() {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    r.observe("main").await;
    r.h.store
        .set_state(
            "a",
            AccountState::Disconnected,
            phoebis_store::StateCause::Operator,
        )
        .unwrap();
    r.h.router.reload().await.unwrap();
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        (r.status("main").reason, r.status("main").next_probe_at),
        (Reason::ProbePoolExhausted, None)
    );
}

/// The gate reads no subscription term, so a probe due after the pinned account's subscription ended reaches
/// the selection, which answers the obstacle rather than placing the probe on `b`: the relay hands the name over,
/// the agent pauses under the ended subscription, and the pin still names `a`.
#[tokio::test]
async fn a_probe_on_a_pin_whose_subscription_ended_holds_the_agent_and_keeps_the_pin() {
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    assert_eq!(r.status("main").account_id.as_deref(), Some("a"));
    let sent = r.up.requests().len();
    r.h.store
        .set_cancelled("a", true, Some(r.now() + 1_000))
        .unwrap();
    r.h.router.reload().await.unwrap();
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        (r.status("main").reason, r.status("main").next_probe_at),
        (Reason::IneligibleCredential, None)
    );
    assert_eq!(r.up.requests().len(), sent, "the probe reached no upstream");
    assert!(
        r.h.router
            .pins()
            .await
            .unwrap()
            .iter()
            .any(|p| p.session_id == "session" && p.account_id == "a"),
        "the pin still names a"
    );
}

#[tokio::test]
async fn retries_back_off_by_the_timing_and_pause_at_the_failure_limit() {
    let timing = KeepaliveTiming {
        // A lead of two minutes leaves room for both retries before the entry expires at 300 s, past which none
        // would be sent.
        lead_5m_ms: 120_000,
        ..RigSpec::default().timing
    };
    assert_eq!(
        (timing.retry_ms, timing.retry_max_ms, timing.failure_limit),
        (30_000, 300_000, 3),
        "the rig keeps the spec's backoff"
    );
    let mut r = rig(RigSpec {
        timing,
        ..RigSpec::default()
    })
    .await;
    r.observe("main").await;
    let server_error = || {
        json(
            503,
            r#"{"type":"error","error":{"type":"api_error","message":"x"}}"#,
        )
    };
    r.up.push_credential_rule(A, "/v1/messages", server_error());
    r.advance(180_000);
    r.step().await;
    assert_eq!(
        (r.status("main").failures, r.status("main").next_probe_at),
        (1, Some(r.now() + 30_000)),
        "30 s after the first failure"
    );
    r.up.push_credential_rule(A, "/v1/messages", server_error());
    r.advance(30_000);
    r.step().await;
    assert_eq!(
        (r.status("main").failures, r.status("main").next_probe_at),
        (2, Some(r.now() + 60_000)),
        "doubled after the second"
    );
    r.up.push_credential_rule(A, "/v1/messages", server_error());
    r.advance(60_000);
    r.step().await;
    assert_eq!(
        (
            r.status("main").failures,
            r.status("main").reason,
            r.status("main").next_probe_at
        ),
        (3, Reason::ProbeRetryLimit, None)
    );
}

#[tokio::test]
async fn a_probe_pause_survives_late_stream_usage() {
    let r = rig(RigSpec::default()).await;
    r.engine.observe(r.observed("req", "main", BODY));
    r.engine.observe(r.selected("req", "a"));
    let chunk = phoebis_bus::Event::ResponseChunk(phoebis_bus::ResponseChunk {
        request_id: "req".into(),
        attempt: 1,
        index: 0,
        body: Default::default(),
        phase: Some(phoebis_bus::Phase::MessageStart {
            usage: phoebis_bus::Usage {
                cache_read_tokens: Some(2_000),
                ..Default::default()
            },
        }),
    });
    r.engine.observe(chunk.clone());
    r.up.push_credential_rule(
        A,
        "/v1/messages",
        json(
            400,
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"x"}}"#,
        ),
    );
    r.advance(270_000);
    r.engine.step().await;
    r.engine.wait_idle().await;
    r.engine.observe(chunk);
    assert_eq!(
        r.status("main").next_probe_at,
        None,
        "late stream usage did not undo the pause"
    );
}

#[tokio::test]
async fn shorter_stream_evidence_recomputes_from_the_last_refresh() {
    let r = rig(RigSpec::default()).await;
    let body = BODY.replace(
        r#""cache_control":{"type":"ephemeral"}"#,
        r#""cache_control":{"type":"ephemeral","ttl":"1h"}"#,
    );
    r.engine.observe(r.observed("req", "main", &body));
    r.engine.observe(r.selected("req", "a"));
    let emit = |bucket: &str| {
        phoebis_bus::Event::ResponseChunk(phoebis_bus::ResponseChunk {
            request_id: "req".into(),
            attempt: 1,
            index: 0,
            body: Default::default(),
            phase: Some(phoebis_bus::Phase::MessageDelta {
                usage: phoebis_bus::Usage {
                    cache_write_5m_tokens: Some(if bucket == "5m" { 2_000 } else { 0 }),
                    cache_write_1h_tokens: Some(if bucket == "1h" { 2_000 } else { 0 }),
                    ..Default::default()
                },
            }),
        })
    };
    let start = r.now();
    r.engine.observe(emit("1h"));
    assert_eq!(
        r.status("main").next_probe_at,
        Some(start + 3_600_000 - 360_000)
    );
    r.engine.observe(emit("5m"));
    assert_eq!(
        r.status("main").next_probe_at,
        Some(start + 270_000),
        "the shorter evidence wins"
    );
    r.advance(270_000);
    r.engine.step().await;
    r.engine.wait_idle().await;
    r.advance(10_000);
    r.engine.observe(emit("5m"));
    assert_eq!(
        r.status("main").next_probe_at,
        Some(start + 540_000),
        "late usage did not undo the completed refresh"
    );
}

/// The stream's `message_delta` repeats the write total without its split, which the relay files under the
/// five-minute column alone. That column without a one-hour column proves no TTL, so a recipe whose start proved
/// the hour keeps it through the delta and through a second turn that reads beside a smaller write. A probe whose
/// answer reports a five-minute write then moves the TTL to five minutes. Each change writes one debug line, two in
/// all: the first arming from no TTL to the hour, and the probe's landing from the hour to five minutes.
#[tokio::test]
async fn a_running_total_without_the_split_keeps_the_hour() {
    let captured = Captured::default();
    let _guard = captured.install_at(tracing::Level::DEBUG);
    let mut r = rig(RigSpec::default()).await;
    let body = BODY.replace(
        r#""cache_control":{"type":"ephemeral"}"#,
        r#""cache_control":{"type":"ephemeral","ttl":"1h"}"#,
    );
    let selected_hour = |id: &str| {
        phoebis_bus::Event::AccountSelected(phoebis_bus::AccountSelected {
            request_id: id.into(),
            attempt: 1,
            account_id: "a".into(),
            account_kind: "api_key".into(),
            pin_epoch: Some(1),
            started_at: r.now(),
            cache_ttl_ms: Some(ONE_HOUR_MS),
            selection: None,
        })
    };
    let phase = |id: &str, phase: phoebis_bus::Phase| {
        phoebis_bus::Event::ResponseChunk(phoebis_bus::ResponseChunk {
            request_id: id.into(),
            attempt: 1,
            index: 0,
            body: Default::default(),
            phase: Some(phase),
        })
    };
    let split = |read: u64, one_hour: u64| phoebis_bus::Usage {
        cache_read_tokens: Some(read),
        cache_write_5m_tokens: Some(0),
        cache_write_1h_tokens: Some(one_hour),
        ..Default::default()
    };
    let bare = |read: u64, total: u64| phoebis_bus::Usage {
        cache_read_tokens: Some(read),
        cache_write_5m_tokens: Some(total),
        cache_write_1h_tokens: None,
        ..Default::default()
    };
    // The first turn: the start proves the hour, the delta repeats the total unsplit.
    let first = r.now();
    r.engine.observe(r.observed("one", "main", &body));
    r.engine.observe(selected_hour("one"));
    r.engine.observe(phase(
        "one",
        phoebis_bus::Phase::MessageStart {
            usage: split(0, 51_177),
        },
    ));
    r.engine.observe(phase(
        "one",
        phoebis_bus::Phase::MessageDelta {
            usage: bare(0, 51_177),
        },
    ));
    r.engine
        .observe(r.complete("one", Outcome::Succeeded, Some(split(0, 51_177))));
    assert_eq!(
        r.status("main").cache_ttl_ms,
        ONE_HOUR_MS,
        "the delta's bare total keeps the hour"
    );
    assert_eq!(
        r.status("main").next_probe_at,
        Some(first + 3_600_000 - 360_000)
    );
    // The second turn reads the first's prefix beside a smaller one-hour write, and its delta is bare again.
    r.advance(5_000);
    let second = r.now();
    r.engine.observe(r.observed("two", "main", &body));
    r.engine.observe(selected_hour("two"));
    r.engine.observe(phase(
        "two",
        phoebis_bus::Phase::MessageStart {
            usage: split(51_177, 11_186),
        },
    ));
    r.engine.observe(phase(
        "two",
        phoebis_bus::Phase::MessageDelta {
            usage: bare(51_177, 11_186),
        },
    ));
    r.engine
        .observe(r.complete("two", Outcome::Succeeded, Some(split(51_177, 11_186))));
    assert_eq!(
        r.status("main").cache_ttl_ms,
        ONE_HOUR_MS,
        "a read beside a one-hour write keeps the hour"
    );
    assert_eq!(
        r.status("main").next_probe_at,
        Some(second + 3_600_000 - 360_000)
    );
    // The probe at that due time reads nothing back and writes the whole prefix for five minutes.
    let prefix = 51_177 + 11_186;
    let due = second + 3_600_000 - 360_000;
    r.up.push_credential_rule(A, "/v1/messages", probe_answer(0, prefix));
    r.advance(due - r.now());
    r.step().await;
    assert_eq!(
        r.status("main").cache_ttl_ms,
        FIVE_MINUTES_MS,
        "the probe's five-minute write moves the TTL"
    );
    assert_eq!(r.status("main").next_probe_at, Some(due + 300_000 - 30_000));
    let text = captured.text();
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("a keepalive recipe's TTL changed on cache evidence"))
        .collect();
    assert_eq!(
        lines.len(),
        2,
        "the first arming and the probe's landing:\n{text}"
    );
    assert!(
        lines[0].contains("DEBUG")
            && lines[0].contains("agent=main")
            && lines[0].contains("previous_ms=0")
            && lines[0].contains("ttl_ms=3600000")
            && lines[0].contains("one_hour=51177"),
        "{}",
        lines[0]
    );
    assert!(
        lines[1].contains("previous_ms=3600000")
            && lines[1].contains("ttl_ms=300000")
            && lines[1].contains(&format!("five_minutes={prefix}")),
        "{}",
        lines[1]
    );
}

/// A stream held past `message_start`, so a request that reaches it commits before it ever completes.
fn held_stream() -> Respond {
    Respond::SseGated {
        status: 200,
        headers: vec![],
        events: vec![
            SseEvent {
                event: "message_start".into(),
                data: r#"{"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":1}}}"#
                    .into(),
            },
            SseEvent {
                event: "message_stop".into(),
                data: r#"{"type":"message_stop"}"#.into(),
            },
        ],
    }
}

/// A pool at its cap answers a probe `429 accounts_at_cap` with `Retry-After` the bound: momentarily full,
/// never out of quota, so the engine retries at the header's seconds rather than pausing under a quota reason.
#[tokio::test]
async fn a_probe_refused_by_a_full_pool_retries_after_the_header_rather_than_pausing() {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0), api_key("b", B, 1)],
        pool: PoolSpec {
            failover: phoebis_store::Failover {
                in_flight_cap: 1,
                retry_in_place_seconds: 2,
                ..phoebis_store::Failover::default()
            },
            ..PoolSpec::default()
        },
        ..RigSpec::default()
    })
    .await;
    r.observe("main").await;
    assert_eq!(r.status("main").account_id.as_deref(), Some("a"));
    // One-shot: a regression that sent the probe to `b` gets a fast wrong answer here, never a stream that
    // holds it for the full probe deadline.
    r.up.push_credential_rule(B, "/v1/messages", held_stream());
    r.h.store
        .set_state(
            "a",
            AccountState::Disconnected,
            phoebis_store::StateCause::Operator,
        )
        .unwrap();
    r.h.router.reload().await.unwrap();

    // A second session takes `b`, over its cap of one: the probe's own selection can find nothing free.
    let mut req = r.request("main");
    relay_common::set_header(&mut req, "x-claude-code-session-id", "other");
    let other = r.h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(relay_common::outcome(&other).account_id, "b");
    assert_eq!(r.h.router.in_flight().account("b"), 1, "b is at its cap");

    r.advance(270_000);
    r.step().await;
    let status = r.status("main");
    assert_eq!(
        (status.failures, status.next_probe_at.map(|at| at - r.now())),
        (1, Some(2_000)),
        "retried after the header rather than paused: {status:?}"
    );
    assert_eq!(
        relay_common::credentials_seen(&r.up),
        [A, B],
        "the pool answered at_cap before either account was dialled for the probe"
    );
    drop(other);
    r.up.release();
}

const OAUTH_SECRET: &str = "sk-ant-oat01-OOOO";
/// A model whose price row carries fast cells.
const FAST_PRICED: &str = "claude-opus-5";

/// A subscription account on plan quota, so a standard probe of its sessions would settle in a window share.
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

/// The rig's body on `FAST_PRICED`, sent in fast mode.
fn fast_body() -> String {
    BODY.replace("claude-sonnet-4-5", FAST_PRICED)
        .replacen('{', r#"{"speed":"fast","#, 1)
}

/// A probe's answer reading the 2,000-token prefix back, served at `speed`.
fn read_back_at(speed: &str) -> Respond {
    json(
        200,
        &format!(
            r#"{{"id":"msg_p","type":"message","content":[],"usage":{{"input_tokens":0,"cache_read_input_tokens":2000,"cache_creation_input_tokens":0,"output_tokens":0,"speed":"{speed}"}}}}"#
        ),
    )
}

/// One fast-mode request on the plan-funded account and the probe after it, answered at `served`, and what
/// the price table says 2,000 cache reads cost at that speed.
async fn fast_probe_served(served: &str, speed: phoebis_ledger::Speed) -> (Rig, f64) {
    let mut r = rig(plan_funded_spec()).await;
    r.up.push_credential_rule(OAUTH_SECRET, "/v1/messages", real_answer());
    r.observe_with("main", &fast_body()).await;
    r.up.push_credential_rule(OAUTH_SECRET, "/v1/messages", read_back_at(served));
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 1);
    let (cost, _) = phoebis_ledger::PriceRevisions::shipped()
        .in_force(r.now())
        .estimate(FAST_PRICED, speed, 0, 2_000, 0, 0, 0)
        .expect("a priced model");
    (r, cost)
}

/// A fast-mode request draws usage credits and no plan window, and its probe replays the body as sent, so the
/// probe does too: its ledger row prices it in dollars at the fast cells on an account whose standard traffic spends
/// plan quota.
#[tokio::test]
async fn a_fast_recipes_probe_settles_in_dollars_at_the_fast_cells_on_a_plan_funded_account() {
    let (r, fast) = fast_probe_served("fast", phoebis_ledger::Speed::Fast).await;
    let (standard, _) = phoebis_ledger::PriceRevisions::shipped()
        .in_force(r.now())
        .estimate(
            FAST_PRICED,
            phoebis_ledger::Speed::Standard,
            0,
            2_000,
            0,
            0,
            0,
        )
        .unwrap();
    assert!(
        fast > standard,
        "the fast cells are dearer: {fast} against {standard}"
    );
    let charged = ledger_probe_cost(&r).await;
    assert!(
        (charged - fast).abs() < 1e-12,
        "the probe's row carries dollars at the fast cells: {charged} against {fast}"
    );
}

/// The answer's own `usage.speed` picks the cells, as it does for a request: a fast recipe the upstream served
/// at standard speed still settles in dollars, at the standard cells.
#[tokio::test]
async fn a_fast_recipe_served_standard_prices_at_the_standard_cells() {
    let (r, standard) = fast_probe_served("standard", phoebis_ledger::Speed::Standard).await;
    let charged = ledger_probe_cost(&r).await;
    assert!(
        (charged - standard).abs() < 1e-12,
        "dollars at the standard cells: {charged} against {standard}"
    );
}

/// A tier the store cannot read leaves a plan-funded probe no window to size a share by, so its unit falls back to
/// dollars as on an account the store names no tier for, and the log says why, naming the account and the error. One
/// line marks the start of a failing streak, and a restart alone starts the next.
#[tokio::test]
async fn a_failed_tier_read_warns_once_and_settles_the_probe_in_dollars() {
    let captured = Captured::default();
    let _guard = captured.install();
    // The rig's model weighed in the cost model, so a readable tier would settle the probe in the share.
    let mut weighted = phoebis_ledger::CostRevisions::shipped().latest().clone();
    weighted
        .model_weight
        .insert("claude-sonnet-4-5".to_string(), Some(1.0));
    let tables = phoebis_ledger::PlanTables {
        revisions: phoebis_ledger::CostRevisions::new(vec![weighted]).unwrap(),
        ..phoebis_ledger::PlanTables::shipped()
    };
    let mut r = rig(RigSpec {
        on_disk: true,
        tables,
        ..plan_funded_spec()
    })
    .await;
    r.up.push_credential_rule(OAUTH_SECRET, "/v1/messages", real_answer());
    r.observe("main").await;
    // The tier reads name the column, and the router serves on from the registry it already loaded.
    let conn = rusqlite::Connection::open(r.store_path.as_ref().expect("an on-disk rig")).unwrap();
    conn.execute_batch(
        "ALTER TABLE accounts RENAME COLUMN subscription_tier TO subscription_tier_held;",
    )
    .unwrap();
    drop(conn);
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 1);
    let warnings = || -> Vec<String> {
        captured
            .text()
            .lines()
            .filter(|line| {
                line.contains(
                    "the account's tier could not be read, so the settlement falls back to dollars",
                )
            })
            .map(String::from)
            .collect()
    };
    let settled = warnings();
    assert_eq!(
        settled.len(),
        1,
        "one line for the one settlement:\n{}",
        captured.text()
    );
    assert!(
        settled[0].contains(" WARN ")
            && settled[0].contains("account=o")
            && settled[0].contains("no such column: subscription_tier")
            && !settled[0].contains(OAUTH_SECRET),
        "{}",
        settled[0]
    );
    let (_, agents) = r.engine.persist_rows();
    let payload: serde_json::Value = serde_json::from_str(&agents[0].payload).unwrap();
    assert_eq!(
        payload["unanswered"][0]["unit"].as_str(),
        Some("usd"),
        "the probe settled in dollars: {}",
        payload["unanswered"]
    );
    // The read still fails at the next probe's settlement, which is the same streak.
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 2);
    assert_eq!(warnings().len(), 1, "{:?}", warnings());
    // A restart starts with no streak, so the registry that is still unreadable warns once more.
    r.engine.flush_persisted().await;
    let second = r.restart().await;
    second.load_persisted().await;
    r.advance(270_000);
    second.step().await;
    second.wait_idle().await;
    assert_eq!(
        second
            .agent_status("desktop", "session", "main")
            .unwrap()
            .probes,
        3
    );
    assert_eq!(warnings().len(), 2, "{:?}", warnings());
}
