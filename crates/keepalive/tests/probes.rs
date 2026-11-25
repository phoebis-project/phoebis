//! Probes through the relay's failover loop: what a probe carries, what its result does to the schedule, and
//! how a late or a failed one is classified; the Go probe suite.
mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use common::*;
use fake_upstream::{install_at, Captured, Respond};
use phoebis_bus::Outcome;
use phoebis_keepalive::{
    CompanionState, KeepalivePolicy, KeepaliveTiming, Reason, RecipeState, MAIN_AGENT, MAX_WORKERS,
    ONE_HOUR_MS,
};
use phoebis_relay::Accounting;
use phoebis_store::{AccountState, Cancellation};
use serde_json::json;

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

/// The hour's rollups under the default select, the read every settlement case makes.
fn rollups(r: &Rig) -> Vec<phoebis_ledger::RollupRow> {
    r.db.read_blocking(|c| {
        phoebis_ledger::read_rollups(
            c,
            phoebis_ledger::hour_of(NOW) - 3_600,
            i64::MAX,
            &phoebis_ledger::RollupSelect::default(),
        )
    })
    .unwrap()
}

/// The dollars a reader prices the probe rows at, once the probe's own request row has landed with its counts: the
/// wait names the cost rather than a row.
async fn ledger_probe_cost(r: &Rig) -> f64 {
    wait_for(std::time::Duration::from_secs(5), || {
        let rows = rollups(r);
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
    let saved = r.saved("main").await;
    assert_eq!(
        (
            r.view("main").probes_today,
            saved["failures"].as_u64(),
            saved["usage"]["read"].as_u64()
        ),
        (1, Some(0), Some(2_000)),
        "the probe was answered 200 and its usage read"
    );
}

#[tokio::test]
async fn a_probe_preserves_routing_and_constraints() {
    let mut r = rig(RigSpec::default()).await;
    // The holder's own instance, since a request that names another one arms no recipe to probe.
    let mut req = r.request("main");
    relay_common::set_header(&mut req, "x-phoebis-instance-id", &r.instance_of("session"));
    let resp = r.h.app.clone().oneshot(req).await.unwrap();
    let id = relay_common::outcome(&resp).request_id;
    let _ = resp.into_body().collect().await.unwrap();
    r.pump(Some(&id)).await;
    r.state("main", "idle").await;
    let lease_before = r.now() + r.lease_ms();
    r.advance_attended(270_000).await;
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
    assert!(header("x-phoebis-instance-id").is_none());
    let body = std::str::from_utf8(&probe.body).unwrap();
    assert!(
        body.contains(r#""max_tokens":0"#) && body.contains(r#""stream":false"#),
        "{body}"
    );
    let status = r.view("main");
    let saved = r.saved("main").await;
    assert_eq!(
        (
            status.probes_today,
            saved["usage"]["read"].as_u64(),
            saved["usage"]["output"].as_u64(),
            status.cause
        ),
        (1, Some(2_000), Some(0), Reason::Scheduled)
    );
    assert_eq!(
        status.next_probe_at,
        Some(r.now() + 270_000),
        "the next probe is due 270 s after the probe's start"
    );
    assert_eq!(
        saved["last_activity"].as_i64(),
        Some(r.now() - 270_000),
        "a probe does not renew activity"
    );
    assert_eq!(r.engine.sessions()[0].companion, CompanionState::Reporting);
    let ack = r.event(json!({"event": "heartbeat"})).await.unwrap();
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
                    phoebis_ledger::RequestOrder::ReceivedAt,
                    phoebis_ledger::Direction::Desc,
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

/// The rig's request with its billing line ahead of the system prompt carrying the placeholder the client sends
/// wherever its native patch does not run. `2.1.269.2b2` is the suffix the client computes for `hello` under
/// 2.1.269 (units 4, 7 and 20: `o` and two padding zeros), so the version check passes.
fn placeholder_body() -> String {
    BODY.replacen(
        r#"{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.269.517; cc_entrypoint=sdk-cli;"}"#,
        r#"{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.269.2b2; cc_entrypoint=sdk-cli; cch=00000;"}"#,
        1,
    )
}

/// Observes `placeholder_body()` on the main agent, runs the probe its recipe schedules, and returns the probe's
/// body as the fake upstream received it.
async fn probe_of_a_placeholder_request(r: &mut Rig) -> Vec<u8> {
    r.observe_with("main", &placeholder_body()).await;
    r.state("main", "idle").await;
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
    let status = r.view("main");
    assert_eq!((status.probes_today, status.cause), (1, Reason::Scheduled));
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
                    phoebis_ledger::RequestOrder::ReceivedAt,
                    phoebis_ledger::Direction::Desc,
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
    r.state("main", "idle").await;
    let original = r.saved("main").await["last_activity"].clone();
    r.advance_attended(270_000).await;
    r.step().await;
    r.event(json!({"event": "heartbeat"})).await.unwrap();
    r.advance_attended(270_000).await;
    r.step().await;
    let status = r.view("main");
    assert_eq!(
        (
            status.probes_today,
            r.saved("main").await["last_activity"].clone(),
            status.cause
        ),
        (1, original, Reason::IdleLimit),
        "{status:?}"
    );
}

/// A probe the router refused with `503 ledger_behind` holds under `ledger_behind` and goes again after the answer's
/// wait, so three refusals in a row reach no upstream and the probes after them are served. The refusal is the relay's
/// own, under the rig's accounting reading, and the probe counts under its own outcome.
#[tokio::test]
async fn held_re_sends_stop_at_the_router_until_it_serves_one() {
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
    r.state("main", "idle").await;
    behind.store(true, Ordering::SeqCst);
    r.advance_attended(270_000).await;
    r.step().await;
    for _ in 0..2 {
        r.advance_attended(5_000).await;
        r.step().await;
    }
    let status = r.view("main");
    assert_eq!(
        (status.probes_today, status.cause),
        (3, Reason::LedgerBehind),
        "three refusals in a row: {status:?}"
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
    r.advance_attended(5_000).await;
    r.step().await;
    let status = r.view("main");
    assert_eq!(
        (status.probes_today, status.cause),
        (4, Reason::Scheduled),
        "the router serves the probe once its ledger caught up: {status:?}"
    );
    r.advance_attended(270_000).await;
    r.step().await;
    let status = r.view("main");
    assert_eq!(
        (status.probes_today, status.cause),
        (5, Reason::Scheduled),
        "a second served idle probe: {status:?}"
    );
    assert_eq!(
        r.up.requests().len(),
        3,
        "the arming request and the two served probes went out; the refusals stopped at the router"
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
        r.post(
            KEY,
            json!({
                "event": "heartbeat",
                "session_id": "session",
                "instance_id": "instance",
                "sequence": 99
            })
        )
        .await
        .unwrap_err()
        .code,
        "instance_ended"
    );
    r.advance(180_000);
    r.step().await;
    assert_eq!(
        r.view("main").probes_today,
        1,
        "probed as the idle main agent while unattended"
    );
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        (
            r.view("main").probes_today,
            r.view("main").cause,
            r.engine.stats().retained_bytes
        ),
        (1, Reason::IdleLimit, 0),
        "540 s past the last real request, over the 500 s limit"
    );
    // The release case: nothing revives a released agent, and the prompt is gone at once.
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    r.state("main", "released").await;
    assert_eq!(r.engine.stats().retained_bytes, 0);
    r.engine.observe(r.observed("late", "main", BODY));
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        (r.view("main").probes_today, r.engine.stats().retained_bytes),
        (0, 0)
    );
}

/// The members of main's saved payload that the recipe view leaves out.
#[derive(Debug)]
struct Saved {
    cold: u64,
    failures: u64,
    paused: bool,
    /// The probes no request has answered yet, the open chain's and those kept for the next request.
    unanswered: usize,
}

/// Main's saved payload, read back once a save landed.
async fn saved_main(r: &Rig) -> Saved {
    let payload = r.saved("main").await;
    let count = |member: &str| payload[member].as_array().map_or(0, Vec::len);
    Saved {
        cold: payload["cold"].as_u64().expect("a saved cold count"),
        failures: payload["failures"].as_u64().expect("a saved failure count"),
        paused: payload["paused"].as_bool().expect("a saved pause"),
        unanswered: count("unanswered") + count("kept"),
    }
}

/// Arms `agent` with a real request, drives the tick its probe is due at and waits for the upstream to receive that
/// probe: a record that a finished probe left in flight skips the dispatch, so the count would stay where it was.
async fn probes_again(r: &mut Rig, agent: &str) {
    r.observe(agent).await;
    let sent = r.up.requests().len();
    r.advance(270_000);
    r.step().await;
    wait_for(std::time::Duration::from_secs(5), || {
        (r.up.requests().len() == sent + 1).then_some(())
    })
    .await;
}

/// Starts the released main again under the generation its release answered, as the mod does, so a real request can
/// arm the record.
async fn start_main_again(r: &mut Rig) {
    let generation = r.request_generation("main");
    r.event(json!({"event": "agent_start", "agent_id": "main", "request_generation": generation}))
        .await
        .expect("a released agent starts again under the generation its release named");
}

/// A rig whose first probe of main the upstream has received and holds until `release`.
async fn a_probe_held_upstream() -> Rig {
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    r.up.push_credential_rule(
        A,
        "/v1/messages",
        Respond::Gated(Box::new(probe_answer(2_000, 0))),
    );
    r.advance_attended(270_000).await;
    r.engine.step().await;
    wait_for(std::time::Duration::from_secs(5), || {
        (r.up.requests().len() == 2).then_some(())
    })
    .await;
    r
}

#[tokio::test]
async fn a_late_probe_cannot_revive_a_new_generation() {
    let mut r = a_probe_held_upstream().await;
    r.engine.observe(r.observed("new", "main", BODY));
    r.up.release();
    no_probe_running(&r.engine).await;
    assert_eq!(
        r.engine.stats().running_probes,
        0,
        "new_request: no worker leaked"
    );
    let status = r.view("main");
    let saved = r.saved("main").await;
    assert_eq!(
        (
            status.account_id.as_deref(),
            status.next_probe_at,
            saved["unanswered"].as_array().map(Vec::len),
            saved["kept"].as_array().map(Vec::len)
        ),
        (Some("a"), None, Some(0), Some(0)),
        "new_request: the late result touched no recipe state: {status:?}"
    );
    // The record the orphaned probe left behind is probed again once the new request arms it: a result that
    // failed the fence must not strand the agent it started on, and a record it left in flight would send none.
    r.engine.observe(r.selected("new", "a"));
    r.engine.observe(read_evidence("new", 2_000));
    assert!(
        r.view("main").next_probe_at.is_some(),
        "the new request armed the record"
    );
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        (r.view("main").probes_today, r.view("main").cause),
        (2, Reason::Scheduled),
        "the re-armed record probes again"
    );

    let r = a_probe_held_upstream().await;
    r.engine.close();
    r.up.release();
    no_probe_running(&r.engine).await;
    assert_eq!(
        r.engine.stats().running_probes,
        0,
        "close: no worker leaked"
    );
    assert_eq!(r.engine.stats().retained_bytes, 0);

    let mut r = a_probe_held_upstream().await;
    r.state("main", "released").await;
    r.up.release();
    no_probe_running(&r.engine).await;
    assert_eq!(
        r.engine.stats().running_probes,
        0,
        "release: no worker leaked"
    );
    let status = r.view("main");
    let saved = r.saved("main").await;
    assert_eq!(
        (
            status.account_id.as_deref(),
            status.next_probe_at,
            saved["unanswered"].as_array().map(Vec::len),
            saved["kept"].as_array().map(Vec::len)
        ),
        (Some("a"), None, Some(0), Some(0)),
        "release: the late result touched no recipe state: {status:?}"
    );
    // The orphaned result gave the record's slot back: started again and armed, the record probes.
    start_main_again(&mut r).await;
    probes_again(&mut r, "main").await;
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
    r.advance_attended(270_000).await;
    r.engine.step().await;
    wait_for(std::time::Duration::from_secs(5), || {
        (r.up.requests().len() == 2).then_some(())
    })
    .await;
    r.state("main", "released").await;
    r.up.release();
    no_probe_running(&r.engine).await;
    r.pump(None).await;
    let charged = ledger_probe_cost(&r).await;
    // The wasted rows below read the late result settled under the release, so it kept nothing unanswered.
    let status = r.view("main");
    assert_eq!(
        status.next_probe_at, None,
        "the late result touched no recipe state: {status:?}"
    );
    let (wasted, wasted_usd, reasons) = wait_for(std::time::Duration::from_secs(5), || {
        let rows = rollups(&r);
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
    // The late result gave the record's slot back: started again and armed, the record probes.
    start_main_again(&mut r).await;
    probes_again(&mut r, "main").await;
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
        (r.view("main").cause, r.engine.stats().retained_bytes),
        (Reason::IdleLimit, 0),
        "the ceiling ended the recipe while its probe was in flight"
    );
    r.up.release();
    no_probe_running(&r.engine).await;
    r.pump(None).await;
    let charged = ledger_probe_cost(&r).await;
    let payload = r.saved("main").await;
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
    // No settlement waits for a save yet, so this save sends nothing to the ledger.
    let saved = r.saved("main").await;
    assert_eq!(
        (
            saved["unanswered"].as_array().map(Vec::len),
            saved["kept"].as_array().map(Vec::len)
        ),
        (Some(0), Some(1)),
        "still pending a millisecond before its entry ends"
    );
    r.advance(1);
    r.step().await;
    // The settlement reaches the ledger with the save that drops the probe, which the production flusher runs.
    r.engine.flush_persisted().await;
    let saved = saved_of(&r.h.store, "session", "main").unwrap();
    assert_eq!(
        (
            saved["unanswered"].as_array().map(Vec::len),
            saved["kept"].as_array().map(Vec::len)
        ),
        (Some(0), Some(0)),
        "settled at its entry's end"
    );
    r.ledger.clone().shutdown().await.unwrap();
    let rows = rollups(&r);
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
    r.advance_attended(270_000).await;
    r.engine.step().await;
    wait_for(std::time::Duration::from_secs(5), || {
        (r.up.requests().len() == 2).then_some(())
    })
    .await;
    r.state("main", "released").await;
    let generation = r.request_generation("main");
    r.event(json!({"event": "agent_start", "agent_id": "main", "request_generation": generation}))
        .await
        .expect("a released agent starts again under the generation its release named");
    r.up.release();
    no_probe_running(&r.engine).await;
    r.pump(None).await;
    // The late result kept nothing: the wasted rows below read it settled under the release.
    r.observe("main").await;
    assert!(
        r.view("main").next_probe_at.is_some(),
        "the next request arms the revived record"
    );
    // The late result gave the slot back: the armed record's probe reaches the upstream when it is due.
    let sent = r.up.requests().len();
    r.advance(270_000);
    r.step().await;
    wait_for(std::time::Duration::from_secs(5), || {
        (r.up.requests().len() == sent + 1).then_some(())
    })
    .await;
    let reasons = wait_for(std::time::Duration::from_secs(5), || {
        let rows = rollups(&r);
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
    // The probe the upstream holds is the old agent's probe in flight.
    wait_for(std::time::Duration::from_secs(5), || {
        (r.up.requests().len() == 2).then_some(())
    })
    .await;
    // A second instance takes the session: a new main agent under the same ids, its own recipe, its own probe.
    r.post(
        KEY,
        json!({
            "event": "session_start",
            "session_id": "session",
            "instance_id": "second",
            "sequence": 1,
            "previous_instance_id": "instance"
        }),
    )
    .await
    .unwrap();
    r.observe("main").await;
    r.up.push_credential_rule(
        A,
        "/v1/messages",
        Respond::Gated(Box::new(probe_answer(2_000, 0))),
    );
    r.advance(270_000);
    r.engine.step().await;
    // The new agent's probe is in flight: the upstream holds it beside the old agent's.
    wait_for(std::time::Duration::from_secs(5), || {
        (r.up.requests().len() == 4).then_some(())
    })
    .await;
    // Both gated probes answer; the old worker's result must leave the new agent's bookkeeping alone.
    r.up.release();
    no_probe_running(&r.engine).await;
    let status = r.view("main");
    let saved = r.saved("main").await;
    let unanswered = saved["unanswered"].as_array().map_or(0, Vec::len)
        + saved["kept"].as_array().map_or(0, Vec::len);
    assert_eq!(
        (status.probes_today, unanswered),
        (1, 1),
        "one probe on the new record: {status:?}"
    );
    // Neither result left the new record in flight: armed again, it probes.
    probes_again(&mut r, "main").await;
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
            r.view("main").cause,
            r.view("main").next_probe_at
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
    no_probe_running(&r.engine).await;
    r.pump(None).await;
    // The transport error's first rung would land at the five-minute entry's expiry, so the ladder pauses at once.
    assert_eq!(
        (r.view("main").cause, r.view("main").next_probe_at),
        (Reason::ProbeRetryLimit, None)
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
        r.event(json!({"event": "agent_start", "agent_id": agent.clone()}))
            .await
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
            .map(|agent| r.view(agent).probes_today)
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
    no_probe_running(&r.engine).await;
    r.step().await;
    assert_eq!(probes(&r), agents.len() as u64, "the capacity recovered");
}

/// A probe on its own account that reads the whole prefix and writes beside it covers, and the next probe is held to
/// the same count.
#[tokio::test]
async fn a_probe_on_its_own_account_that_also_writes_leaves_the_next_probe_the_same_count() {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    // The recipe counts the standing answer's 2,000-token read. The probe reads all of it and writes 500 beside it.
    r.observe("main").await;
    r.up.push_credential_rule(A, "/v1/messages", probe_answer(2_000, 500));
    r.advance(270_000);
    r.step().await;
    let status = r.view("main");
    let saved = saved_main(&r).await;
    assert_eq!(
        (status.probes_today, saved.cold, saved.paused, status.cause),
        (1, 0, false, Reason::Scheduled),
        "{status:?}"
    );
    // The next probe reads the 2,000 tokens back and writes nothing.
    r.up.push_credential_rule(A, "/v1/messages", probe_answer(2_000, 0));
    r.advance(270_000);
    r.step().await;
    let status = r.view("main");
    let saved = saved_main(&r).await;
    assert_eq!(
        (status.probes_today, saved.cold, saved.paused, status.cause),
        (2, 0, false, Reason::Scheduled),
        "the count stays the last request's 2,000 tokens: {status:?}"
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
    // The second probe's entry ends at 840 s, where the failure's first rung would land.
    assert_eq!(
        (r.view("main").probes_today, r.view("main").cause),
        (3, Reason::ProbeRetryLimit)
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
        (r.view("main").cause, r.view("main").probes_today),
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
    assert_eq!(r.view("main").cause, Reason::Expired);
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
    let status = r.view("main");
    let saved = saved_main(&r).await;
    assert_eq!(
        (status.probes_today, saved.cold, status.cause),
        (1, 1, Reason::Scheduled),
        "the first rewrite lands and counts one: {status:?}"
    );
    r.advance(270_000);
    r.step().await;
    let status = r.view("main");
    let saved = saved_main(&r).await;
    assert_eq!(
        (
            status.probes_today,
            saved.unanswered,
            saved.cold,
            status.cause,
            status.next_probe_at
        ),
        (2, 2, 2, Reason::RepeatedColdProbe, None),
        "the second rewrite reaches the limit: {status:?}"
    );
}

/// The relay answers a probe without the session header with its 403 `client_contract`, which refuses the probe's
/// own request shape rather than the credential, so the recipe pauses under `UnsupportedProbe` and not under the
/// authentication reason the status alone would pick.
#[tokio::test]
async fn a_probe_the_relay_refuses_for_the_client_contract_pauses_as_an_unsupported_probe() {
    let mut r = rig(RigSpec::default()).await;
    let phoebis_bus::Event::RequestObserved(mut observed) = r.observed("contract", "main", BODY)
    else {
        unreachable!()
    };
    observed
        .headers
        .retain(|(name, _)| name != "x-claude-code-session-id");
    r.engine
        .observe(phoebis_bus::Event::RequestObserved(observed));
    r.engine.observe(r.selected("contract", "a"));
    r.engine.observe(read_evidence("contract", 2_000));
    r.advance(270_000);
    r.step().await;
    assert!(
        r.up.requests().is_empty(),
        "the relay refused the probe before any account"
    );
    let status = r.view("main");
    let saved = saved_main(&r).await;
    assert_eq!(
        (saved.paused, status.cause, status.next_probe_at),
        (true, Reason::UnsupportedProbe, None),
        "{status:?}"
    );
    let probe = wait_for(std::time::Duration::from_secs(5), || {
        let (rows, _) =
            r.db.read_blocking(|c| {
                phoebis_ledger::list_requests(
                    c,
                    &phoebis_ledger::RequestFilter {
                        include_probes: true,
                        ..Default::default()
                    },
                    &phoebis_ledger::LiveKeys::default(),
                    phoebis_ledger::RequestOrder::ReceivedAt,
                    phoebis_ledger::Direction::Desc,
                    None,
                    10,
                )
            })
            .unwrap();
        rows.into_iter()
            .map(|(row, _)| row)
            .find(|row| row.kind == "probe")
    })
    .await;
    assert_eq!(
        (
            probe.status,
            probe.error_code.as_deref(),
            probe.client_contract.as_deref()
        ),
        (Some(403), Some("client_contract"), Some("session_header"))
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

/// An exhaustion whose first retry would land at the entry's expiry pauses the recipe until the next real request,
/// while a gate hold keeps it armed for the tick that finds the hold gone: the `paused` bit tells the two apart,
/// whatever the reason reads.
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
    let status = r.view("main");
    let saved = saved_main(&r).await;
    assert_eq!(
        (saved.paused, status.next_probe_at, status.cause),
        (true, None, Reason::ProbeRetryLimit),
        "{status:?}"
    );
    // Past the accounts' reset the next real request selects one again.
    r.advance(5_000_000);
    r.step().await;
    assert_eq!(
        r.engine.reasons().get(&Reason::ProbeRetryLimit).copied(),
        Some(1),
        "no probe ran again after the ladder's pause"
    );
    r.observe("main").await;
    let status = r.view("main");
    let saved = saved_main(&r).await;
    assert_eq!(
        (saved.paused, status.cause),
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
    let status = r.view("main");
    let saved = saved_main(&r).await;
    assert_eq!(
        (saved.paused, status.cause, status.probes_today),
        (false, Reason::Budget, 0),
        "{status:?}"
    );
    assert!(
        status.next_probe_at.is_some(),
        "the schedule is kept for the tick that finds the budget gone: {status:?}"
    );
}

/// A pinned account that disconnects between admission and the probe's own selection leaves the pool with
/// nothing to serve: the relay answers its untimed exhaustion, a transient cause rather than the authentication one a
/// marked account gets, and on a five-minute entry its first retry would land at the expiry, so the ladder pauses
/// at once.
#[tokio::test]
async fn an_exhaustion_503_whose_first_retry_would_reach_the_expiry_pauses_under_the_retry_limit() {
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
        (r.view("main").cause, r.view("main").next_probe_at),
        (Reason::ProbeRetryLimit, None)
    );
}

/// The gate reads the pinned account's subscription term on every poll, so a probe due after the term ended is held
/// before it is sent, ahead of the selection, rather than placed on `b`: no probe is dispatched, the schedule stays
/// for the tick that finds the term cleared, the agent reads the ended subscription, and the pin still names `a`.
#[tokio::test]
async fn a_probe_on_a_pin_whose_subscription_ended_holds_the_agent_and_keeps_the_pin() {
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    assert_eq!(r.view("main").account_id.as_deref(), Some("a"));
    let due = r.now() + 270_000;
    let sent = r.up.requests().len();
    r.h.store
        .set_canceled(
            "a",
            Cancellation::On {
                ends_at: Some(r.now() + 1_000),
            },
        )
        .unwrap();
    r.h.router.reload().await.unwrap();
    r.advance(270_000);
    r.step().await;
    let status = r.view("main");
    assert_eq!(
        (status.cause, status.next_probe_at, status.probes_today),
        (Reason::IneligibleCredential, Some(due), 0),
        "{status:?}"
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

fn server_error() -> Respond {
    json(
        503,
        r#"{"type":"error","error":{"type":"api_error","message":"x"}}"#,
    )
}

/// The rig's body under a one-hour marker, so its entry expires an hour after its request.
fn hour_body() -> String {
    BODY.replace(
        r#""cache_control":{"type":"ephemeral"}"#,
        r#""cache_control":{"type":"ephemeral","ttl":"1h"}"#,
    )
}

/// One account and a one-hour entry probed 900 s before it expires, room for the ladder's five rungs, under an idle
/// window longer than the entry, so no ceiling ends the recipe before the ladder or the lapse does.
fn hour_spec() -> RigSpec {
    RigSpec {
        accounts: vec![api_key("a", A, 0)],
        policy: KeepalivePolicy {
            enabled: true,
            max_idle_seconds: 86_400,
            ..KeepalivePolicy::default()
        },
        timing: KeepaliveTiming {
            lead_1h_ms: 900_000,
            ..RigSpec::default().timing
        },
        ..RigSpec::default()
    }
}

/// `hour_spec` with no wait in place, so a 429 with a short `Retry-After` cools the account and reaches the probe as
/// the pool's timed exhaustion, the header the cooldown's seconds, rather than as a retry the relay waits out.
fn quota_spec() -> RigSpec {
    RigSpec {
        pool: PoolSpec {
            failover: phoebis_store::Failover {
                retry_in_place_seconds: 0,
                ..phoebis_store::Failover::default()
            },
            ..PoolSpec::default()
        },
        ..hour_spec()
    }
}

/// An API key account's 429 with `Retry-After`, which cools the account for those seconds and a second more.
fn quota_429(retry_after_secs: u64) -> Respond {
    Respond::Json {
        status: 429,
        headers: vec![("retry-after".into(), retry_after_secs.to_string())],
        body: br#"{"type":"error","error":{"type":"rate_limit_error","message":"limit"}}"#.to_vec(),
    }
}

/// The Sessions row of the rig's main agent.
fn recipe_row(r: &Rig) -> phoebis_keepalive::RecipeView {
    r.engine
        .recipes()
        .into_iter()
        .find(|v| v.session_id == "session" && v.agent_id == MAIN_AGENT)
        .expect("a row for main")
}

/// Moves the clock to the recipe's next probe and runs the tick that sends it.
async fn step_to_the_next_probe(r: &mut Rig) {
    let due = r.view("main").next_probe_at.expect("a probe is scheduled");
    r.advance(due - r.now());
    r.step().await;
}

/// The ladder over `hour_spec`'s entry from its first probe, every probe answered by `failure` and driven through
/// `Engine::step`: each failure plans the next rung 30, 60, 120, 240 and then 300 s out while the rung lands before
/// the expiry, the row reading `held` under `cause`, and the fifth retry's failure, whose 300 s rung would land 150 s
/// past the expiry, pauses the recipe under `probe_retry_limit` 150 s before it.
async fn climb_the_ladder(r: &mut Rig, failure: impl Fn() -> Respond, cause: Reason) {
    let first = r.view("main").next_probe_at.expect("an armed recipe");
    let expires = first + 900_000;
    for (failures, wait) in [
        (1, 30_000),
        (2, 60_000),
        (3, 120_000),
        (4, 240_000),
        (5, 300_000),
    ] {
        r.up.push_credential_rule(A, "/v1/messages", failure());
        step_to_the_next_probe(r).await;
        let status = r.view("main");
        let saved = saved_main(r).await;
        assert_eq!(
            (
                saved.failures,
                saved.paused,
                status.cause,
                status.next_probe_at
            ),
            (failures, false, cause, Some(r.now() + wait)),
            "failure {failures}: the next rung {wait} ms out, before the expiry at {expires}"
        );
        let row = recipe_row(r);
        assert_eq!(
            (row.state, row.cause),
            (RecipeState::Held, cause),
            "failure {failures}: the row reads held while the ladder runs"
        );
    }
    r.up.push_credential_rule(A, "/v1/messages", failure());
    step_to_the_next_probe(r).await;
    assert_eq!(
        r.now(),
        first + 750_000,
        "the fifth retry fails 750 s after the first probe"
    );
    let status = r.view("main");
    let saved = saved_main(r).await;
    assert_eq!(
        (
            saved.failures,
            saved.paused,
            status.cause,
            status.next_probe_at
        ),
        (6, true, Reason::ProbeRetryLimit, None),
        "a 300 s rung would land 150 s past the expiry, so this failure pauses the recipe"
    );
    assert_eq!(
        expires - r.now(),
        150_000,
        "the pause comes before the expiry"
    );
    let row = recipe_row(r);
    assert_eq!(
        (row.state, row.cause),
        (RecipeState::Paused, Reason::ProbeRetryLimit)
    );
}

#[tokio::test]
async fn a_5xx_probe_retries_on_a_doubling_ladder_and_pauses_at_the_failure_whose_next_rung_would_reach_the_expiry(
) {
    let spec = hour_spec();
    assert_eq!(
        (spec.timing.retry_ms, spec.timing.retry_max_ms),
        (30_000, 300_000),
        "the rig keeps the spec's backoff"
    );
    let mut r = rig(spec).await;
    r.observe_with("main", &hour_body()).await;
    climb_the_ladder(&mut r, server_error, Reason::RetryableProbeError).await;
}

#[tokio::test]
async fn a_quota_429_retries_on_the_same_ladder_with_the_row_held_under_probe_quota_exhausted() {
    let mut r = rig(quota_spec()).await;
    r.observe_with("main", &hour_body()).await;
    climb_the_ladder(&mut r, || quota_429(1), Reason::ProbeQuotaExhausted).await;
}

/// The pool's exhaustion answers the first 429 with the two seconds its cooldown runs, shorter than the 30 s step,
/// and the second with 90 s, longer than the 60 s step it replaces.
#[tokio::test]
async fn a_quota_429s_retry_after_longer_than_the_step_replaces_it() {
    let mut r = rig(quota_spec()).await;
    r.observe_with("main", &hour_body()).await;
    r.up.push_credential_rule(A, "/v1/messages", quota_429(1));
    step_to_the_next_probe(&mut r).await;
    assert_eq!(
        (saved_main(&r).await.failures, r.view("main").next_probe_at),
        (1, Some(r.now() + 30_000)),
        "a header shorter than the step leaves the step"
    );
    r.up.push_credential_rule(A, "/v1/messages", quota_429(89));
    step_to_the_next_probe(&mut r).await;
    let status = r.view("main");
    let saved = saved_main(&r).await;
    assert_eq!(
        (
            saved.failures,
            saved.paused,
            status.cause,
            status.next_probe_at
        ),
        (
            2,
            false,
            Reason::ProbeQuotaExhausted,
            Some(r.now() + 90_000)
        ),
        "the 90 s header replaces the 60 s step"
    );
}

/// The ladder counts consecutive errors: an answered probe between two failures starts the count over, so the next
/// failure waits the first rung again, while a real request that fails leaves the count where the ladder stood, so
/// the failure after it waits the second rung.
#[tokio::test]
async fn an_answered_probe_resets_the_ladder_and_a_failed_request_leaves_it_where_it_stood() {
    let mut r = rig(RigSpec {
        timing: KeepaliveTiming {
            // A lead of two minutes leaves room for two rungs inside a five-minute entry.
            lead_5m_ms: 120_000,
            ..RigSpec::default().timing
        },
        ..RigSpec::default()
    })
    .await;
    r.observe("main").await;
    r.up.push_credential_rule(A, "/v1/messages", server_error());
    step_to_the_next_probe(&mut r).await;
    assert_eq!(
        (saved_main(&r).await.failures, r.view("main").next_probe_at),
        (1, Some(r.now() + 30_000))
    );
    // The standing rule answers the retry.
    step_to_the_next_probe(&mut r).await;
    assert_eq!(
        (saved_main(&r).await.failures, r.view("main").cause),
        (0, Reason::Scheduled),
        "the answered probe starts the count over"
    );
    r.up.push_credential_rule(A, "/v1/messages", server_error());
    step_to_the_next_probe(&mut r).await;
    let rung = r.now() + 30_000;
    assert_eq!(
        (saved_main(&r).await.failures, r.view("main").next_probe_at),
        (1, Some(rung)),
        "the failure after an answered probe waits the first rung"
    );
    r.advance(10_000);
    r.engine.observe(r.observed("failed", "main", BODY));
    r.engine.observe(r.selected("failed", "a"));
    r.engine
        .observe(r.complete("failed", Outcome::Failed, None));
    let status = r.view("main");
    let saved = saved_main(&r).await;
    assert_eq!(
        (
            saved.failures,
            status.next_probe_at,
            saved.paused,
            status.cause
        ),
        (1, Some(rung), false, Reason::RequestFailed),
        "the failed request left the recipe on its rung with its count"
    );
    r.up.push_credential_rule(A, "/v1/messages", server_error());
    step_to_the_next_probe(&mut r).await;
    assert_eq!(
        (saved_main(&r).await.failures, r.view("main").next_probe_at),
        (2, Some(r.now() + 60_000)),
        "the count the request left makes the next wait the second rung"
    );
}

/// A probe whose exchange the probe deadline ends is a transport error (status 0), which retries on the ladder.
#[tokio::test]
async fn a_probe_the_deadline_ends_retries_on_the_first_rung_under_probe_transport_error() {
    let mut r = rig(hour_spec()).await;
    r.observe_with("main", &hour_body()).await;
    r.up.push_credential_rule(
        A,
        "/v1/messages",
        Respond::Stall {
            status: 200,
            headers: vec![],
            events: vec![],
        },
    );
    let due = r.view("main").next_probe_at.unwrap();
    r.advance(due - r.now());
    r.engine.step().await;
    wait_for(std::time::Duration::from_secs(5), || {
        (r.up.requests().len() == 2).then_some(())
    })
    .await;
    tokio::time::pause();
    tokio::time::advance(phoebis_keepalive::PROBE_DEADLINE + std::time::Duration::from_secs(1))
        .await;
    tokio::time::resume();
    r.probe_answered("main").await;
    r.pump(None).await;
    let status = r.view("main");
    let saved = saved_main(&r).await;
    assert_eq!(
        (
            saved.failures,
            saved.paused,
            status.cause,
            status.next_probe_at
        ),
        (
            1,
            false,
            Reason::ProbeTransportError,
            Some(r.now() + 30_000)
        )
    );
    let row = recipe_row(&r);
    assert_eq!(
        (row.state, row.cause),
        (RecipeState::Held, Reason::ProbeTransportError)
    );
    r.up.release();
}

/// A probe whose pool has no account left to select is answered the untimed exhaustion 503, which retries on the
/// ladder under its own reason.
#[tokio::test]
async fn an_exhausted_selection_retries_on_the_first_rung_under_probe_pool_exhausted() {
    let mut r = rig(hour_spec()).await;
    r.observe_with("main", &hour_body()).await;
    r.h.store
        .set_state(
            "a",
            AccountState::Disconnected,
            phoebis_store::StateCause::Operator,
        )
        .unwrap();
    r.h.router.reload().await.unwrap();
    step_to_the_next_probe(&mut r).await;
    let status = r.view("main");
    let saved = saved_main(&r).await;
    assert_eq!(
        (
            saved.failures,
            saved.paused,
            status.cause,
            status.next_probe_at
        ),
        (1, false, Reason::ProbePoolExhausted, Some(r.now() + 30_000))
    );
    let row = recipe_row(&r);
    assert_eq!(
        (row.state, row.cause),
        (RecipeState::Held, Reason::ProbePoolExhausted)
    );
}

/// A probe answered with no cache evidence retries on the ladder too, and pauses where its next rung would reach the
/// expiry. The cold limit is raised past the ladder's six probes, each of which reads nothing.
#[tokio::test]
async fn a_probe_with_no_cache_evidence_retries_on_the_ladder_to_the_expiry() {
    let spec = hour_spec();
    let mut r = rig(RigSpec {
        timing: KeepaliveTiming {
            cold_probe_limit: 10,
            ..spec.timing
        },
        ..spec
    })
    .await;
    r.observe_with("main", &hour_body()).await;
    climb_the_ladder(&mut r, || probe_answer(0, 0), Reason::NoProbeCacheEvidence).await;
}

/// A probe whose answer lands with its refresh interval consumed retries after the first rung where that rung lands
/// before the entry it wrote expires, and pauses under `probe_retry_limit` where the rung would reach it.
#[tokio::test]
async fn a_slow_probe_retries_on_the_first_rung_and_pauses_where_that_rung_would_reach_the_expiry()
{
    // The probe writes its own hour-long entry from its start, due 2,700 s in: one landing 2,701 s in leaves a
    // 30 s rung before the expiry, one landing 3,580 s in does not.
    for (late, paused) in [(2_701_000, false), (3_580_000, true)] {
        let mut r = rig(hour_spec()).await;
        r.observe_with("main", &hour_body()).await;
        r.up.push_credential_rule(
            A,
            "/v1/messages",
            Respond::Gated(Box::new(probe_answer(2_000, 0))),
        );
        let due = r.view("main").next_probe_at.unwrap();
        r.advance(due - r.now());
        r.engine.step().await;
        wait_for(std::time::Duration::from_secs(5), || {
            (r.up.requests().len() == 2).then_some(())
        })
        .await;
        r.advance(late);
        r.up.release();
        r.probe_answered("main").await;
        r.pump(None).await;
        let status = r.view("main");
        let saved = saved_main(&r).await;
        let expected = if paused {
            (1, true, Reason::ProbeRetryLimit, None)
        } else {
            (1, false, Reason::SlowProbe, Some(r.now() + 30_000))
        };
        assert_eq!(
            (
                saved.failures,
                saved.paused,
                status.cause,
                status.next_probe_at
            ),
            expected,
            "a landing {late} ms after the probe's start"
        );
    }
}

/// The structural errors are no passing fault, so each pauses the recipe at once though the entry leaves room for
/// every rung of the ladder: a 400, a 401 (the relay's untimed exhaustion naming the account's mark), an answer
/// without usage, one with output, one that read part of the prefix, and the second cold probe in a row.
#[tokio::test]
async fn a_structural_probe_error_pauses_the_recipe_at_once_with_the_ladder_left_unclimbed() {
    for (name, respond, reason) in [
        (
            "unsupported",
            json(
                400,
                r#"{"type":"error","error":{"type":"invalid_request_error","message":"x"}}"#,
            ),
            Reason::UnsupportedProbe,
        ),
        (
            "authentication",
            json(
                401,
                r#"{"type":"error","error":{"type":"authentication_error","message":"x"}}"#,
            ),
            Reason::ProbeAuthenticationError,
        ),
        (
            "missing_usage",
            json(200, r#"{"id":"m","type":"message","content":[]}"#),
            Reason::InvalidProbeUsage,
        ),
        (
            "output",
            json(
                200,
                r#"{"id":"m","type":"message","content":[],"usage":{"cache_read_input_tokens":2000,"output_tokens":1}}"#,
            ),
            Reason::UnexpectedOutput,
        ),
        (
            "partial",
            probe_answer(100, 0),
            Reason::PartialCacheCoverage,
        ),
    ] {
        let mut r = rig(hour_spec()).await;
        r.observe_with("main", &hour_body()).await;
        r.up.push_credential_rule(A, "/v1/messages", respond);
        step_to_the_next_probe(&mut r).await;
        let status = r.view("main");
        let saved = saved_main(&r).await;
        assert_eq!(
            (
                name,
                saved.failures,
                saved.paused,
                status.cause,
                status.next_probe_at
            ),
            (name, 0, true, reason, None),
            "{status:?}"
        );
        let row = recipe_row(&r);
        assert_eq!(
            (row.state, row.cause),
            (RecipeState::Paused, reason),
            "{name}"
        );
    }
    let mut r = rig(hour_spec()).await;
    r.observe_with("main", &hour_body()).await;
    r.up.push_credential_rule(A, "/v1/messages", probe_answer(0, 2_000));
    r.up.push_credential_rule(A, "/v1/messages", probe_answer(0, 2_000));
    step_to_the_next_probe(&mut r).await;
    assert_eq!(
        (saved_main(&r).await.cold, r.view("main").cause),
        (1, Reason::Scheduled)
    );
    step_to_the_next_probe(&mut r).await;
    let status = r.view("main");
    let saved = saved_main(&r).await;
    assert_eq!(
        (
            saved.cold,
            saved.failures,
            saved.paused,
            status.cause,
            status.next_probe_at
        ),
        (2, 0, true, Reason::RepeatedColdProbe, None),
        "the cold limit pauses at once too"
    );
}

/// A tick that comes past the entry's expiry with a rung still due finds the entry gone: no probe is sent, and the
/// recipe lapses under `expired`.
#[tokio::test]
async fn a_tick_past_the_expiry_with_a_rung_still_due_sends_no_probe_and_lapses_the_recipe() {
    let mut r = rig(hour_spec()).await;
    r.observe_with("main", &hour_body()).await;
    let expires = r.view("main").next_probe_at.unwrap() + 900_000;
    r.up.push_credential_rule(A, "/v1/messages", server_error());
    step_to_the_next_probe(&mut r).await;
    assert_eq!(
        (saved_main(&r).await.failures, r.view("main").next_probe_at),
        (1, Some(r.now() + 30_000)),
        "a rung is due"
    );
    let sent = r.up.requests().len();
    r.advance(expires + 1_000 - r.now());
    r.step().await;
    let status = r.view("main");
    let saved = saved_main(&r).await;
    assert_eq!(
        (status.cause, saved.paused, status.next_probe_at),
        (Reason::Expired, false, None),
        "{status:?}"
    );
    assert_eq!(r.up.requests().len(), sent, "no probe after the expiry");
}

#[tokio::test]
async fn a_probe_pause_survives_late_stream_usage() {
    let r = rig(RigSpec::default()).await;
    r.pin("main", "claude-sonnet-4-5", "a").await;
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
    no_probe_running(&r.engine).await;
    r.engine.observe(chunk);
    assert_eq!(
        r.view("main").next_probe_at,
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
    r.pin("main", "claude-sonnet-4-5", "a").await;
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
        r.view("main").next_probe_at,
        Some(start + 3_600_000 - 360_000)
    );
    r.engine.observe(emit("5m"));
    assert_eq!(
        r.view("main").next_probe_at,
        Some(start + 270_000),
        "the shorter evidence wins"
    );
    r.advance(270_000);
    r.engine.step().await;
    no_probe_running(&r.engine).await;
    r.advance(10_000);
    r.engine.observe(emit("5m"));
    assert_eq!(
        r.view("main").next_probe_at,
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
    let _guard = install_at(&captured, tracing::Level::DEBUG);
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
            incarnation: r.incarnation("a"),
            account_kind: "api_key".into(),
            usage_based: false,
            pin_epoch: Some(1),
            started_at: r.now(),
            cache_ttl_ms: Some(ONE_HOUR_MS),
            selection: None,
            previous_input_tokens: None,
            entry_expired: None,
            agent_released: None,
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
    r.pin("main", "claude-sonnet-4-5", "a").await;
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
        r.view("main").ttl_seconds,
        3_600,
        "the delta's bare total keeps the hour"
    );
    assert_eq!(
        r.view("main").next_probe_at,
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
        r.view("main").ttl_seconds,
        3_600,
        "a read beside a one-hour write keeps the hour"
    );
    assert_eq!(
        r.view("main").next_probe_at,
        Some(second + 3_600_000 - 360_000)
    );
    // The probe at that due time reads nothing back and writes the whole prefix for five minutes.
    let prefix = 51_177 + 11_186;
    let due = second + 3_600_000 - 360_000;
    r.up.push_credential_rule(A, "/v1/messages", probe_answer(0, prefix));
    r.advance(due - r.now());
    r.step().await;
    assert_eq!(
        r.view("main").ttl_seconds,
        300,
        "the probe's five-minute write moves the TTL"
    );
    assert_eq!(r.view("main").next_probe_at, Some(due + 300_000 - 30_000));
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
/// the price table says 2,000 cache reads cost at that speed. The session stays attended, so a case can still stop
/// the recipe with the probe unanswered.
async fn fast_probe_served(served: &str, speed: phoebis_ledger::Speed) -> (Rig, f64) {
    let mut r = rig(plan_funded_spec()).await;
    r.up.push_credential_rule(OAUTH_SECRET, "/v1/messages", real_answer());
    r.observe_with("main", &fast_body()).await;
    r.up.push_credential_rule(OAUTH_SECRET, "/v1/messages", read_back_at(served));
    r.advance_attended(270_000).await;
    r.step().await;
    assert_eq!(r.view("main").probes_today, 1);
    let cost = phoebis_ledger::PriceRevisions::shipped()
        .in_force(r.now())
        .estimate(
            FAST_PRICED,
            speed,
            phoebis_routing::PinUsage {
                input: 0,
                cache_read: 2_000,
                write_5m: 0,
                write_1h: 0,
                output: 0,
            },
        )
        .expect("a priced model")
        .total();
    (r, cost)
}

/// A fast-mode request draws usage credits and no plan window, and its probe replays the body as sent, so the
/// probe does too: its ledger row prices it in dollars at the fast cells on an account whose standard traffic spends
/// plan quota.
#[tokio::test]
async fn a_fast_recipes_probe_settles_in_dollars_at_the_fast_cells_on_a_plan_funded_account() {
    let (r, fast) = fast_probe_served("fast", phoebis_ledger::Speed::Fast).await;
    let standard = phoebis_ledger::PriceRevisions::shipped()
        .in_force(r.now())
        .estimate(
            FAST_PRICED,
            phoebis_ledger::Speed::Standard,
            phoebis_routing::PinUsage {
                input: 0,
                cache_read: 2_000,
                write_5m: 0,
                write_1h: 0,
                output: 0,
            },
        )
        .unwrap()
        .total();
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
/// at standard speed prices at the standard cells.
#[tokio::test]
async fn a_fast_recipe_served_standard_prices_at_the_standard_cells() {
    let (r, standard) = fast_probe_served("standard", phoebis_ledger::Speed::Standard).await;
    let charged = ledger_probe_cost(&r).await;
    assert!(
        (charged - standard).abs() < 1e-12,
        "dollars at the standard cells: {charged} against {standard}"
    );
}

/// `fast_probe_served`'s probe settled wasted by a release, and every rollup row once its settlement has landed.
async fn released_and_settled(r: &mut Rig) -> Vec<phoebis_ledger::RollupRow> {
    r.state("main", "released").await;
    wait_for(std::time::Duration::from_secs(5), || {
        let rows = rollups(r);
        rows.iter().any(|row| row.wasted_probes > 0).then_some(rows)
    })
    .await
}

/// The speed, the funding and the account tier each settlement row files under.
fn settlement_keys(rows: &[phoebis_ledger::RollupRow]) -> Vec<(&str, &str, &str)> {
    rows.iter()
        .filter(|row| row.wasted_probes > 0)
        .map(|row| (row.speed.as_str(), row.funding.as_str(), row.tier.as_str()))
        .collect()
}

/// A wasted probe files beside its own request row, at the speed its answer reported: a fast recipe the upstream
/// served at standard speed on an account without overage settles as `plan` under the account's tier, so its waste
/// prices at the standard cells and reads in the plan's probe figure beside the probe's cost.
#[tokio::test]
async fn a_fast_recipe_served_standard_settles_its_waste_as_plan_at_the_standard_cells() {
    let (mut r, standard) = fast_probe_served("standard", phoebis_ledger::Speed::Standard).await;
    let rows = released_and_settled(&mut r).await;
    assert_eq!(
        settlement_keys(&rows),
        [("standard", "plan", "default_claude_max_5x")],
        "the settlement's speed, funding and account tier"
    );
    let (cost, wasted) = read_dollars(&r, &rows);
    assert!(
        (cost - standard).abs() < 1e-12 && (wasted - standard).abs() < 1e-12,
        "the cost and the waste at the standard cells: {cost} and {wasted} against {standard}"
    );
    let groups = phoebis_ledger::breakdown(
        &rows,
        phoebis_ledger::GroupBy::Account,
        true,
        &phoebis_ledger::LiveKeys::default(),
        &[],
        &r.pricing,
        &r.tables,
        r.now(),
    );
    let figure = groups
        .iter()
        .find(|group| group.key == "o")
        .and_then(|group| group.probe_equivalent.as_ref())
        .expect("the plan-funded probe's Pro-week figure");
    assert!(
        figure.cost > 0.0 && (figure.wasted - figure.cost).abs() < 1e-12,
        "the whole probe wasted in the plan's figure: {figure:?}"
    );
}

/// A fast answer draws usage credits whatever funds the account, so its waste still files under `fast` with no account
/// tier and prices in dollars at the fast cells.
#[tokio::test]
async fn a_fast_recipe_served_fast_settles_its_waste_in_dollars_at_the_fast_cells() {
    let (mut r, fast) = fast_probe_served("fast", phoebis_ledger::Speed::Fast).await;
    let rows = released_and_settled(&mut r).await;
    assert_eq!(
        settlement_keys(&rows),
        [("fast", "fast", "-")],
        "the settlement's speed, funding and account tier"
    );
    let (cost, wasted) = read_dollars(&r, &rows);
    assert!(
        (cost - fast).abs() < 1e-12 && (wasted - fast).abs() < 1e-12,
        "the cost and the waste at the fast cells: {cost} and {wasted} against {fast}"
    );
}
