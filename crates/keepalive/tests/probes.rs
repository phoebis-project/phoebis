//! Probes through the relay's failover loop: what a probe carries, what its result does to the schedule, and
//! how a late or a failed one is classified; the Go probe suite.
mod common;
use std::str::FromStr;

use common::*;
use fake_upstream::{Respond, SseEvent};
use phoebis_keepalive::{Event, KeepaliveLimits, KeepalivePolicy, Reason};
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
    let lease_before = r.now() + 3_600_000;
    r.advance(270_000);
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
    let snapshot = r.engine.snapshot("desktop", r.generation("desktop"));
    assert!(!snapshot.sessions[0].ended);
    let ack = r
        .event(Event {
            event: "heartbeat".into(),
            ..Default::default()
        })
        .unwrap();
    assert!(
        jiff::Timestamp::from_str(&ack.lease_expires)
            .unwrap()
            .as_millisecond()
            > lease_before,
        "only the heartbeat renewed the lease"
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

/// A client-sent placeholder is kept whatever the switch: `proxy.sign_cch_placeholder` governs the relay's
/// restored placeholder alone and never reaches the keepalive, so the recipe keeps the client's line and the
/// probe reaches the upstream with it. The relay's side over both switch positions is
/// `a_client_sent_placeholder_on_a_full_shape_line_is_relayed_intact` in the relay's restoration suite.
#[tokio::test]
async fn a_client_sent_placeholder_is_kept_whatever_the_switch() {
    let body = placeholder_body();
    let recipe =
        phoebis_keepalive::make_recipe(&[], body.as_bytes(), "claude-sonnet-4-5", 1 << 20).unwrap();
    assert_eq!(
        billing_line(&recipe.body),
        billing_line(body.as_bytes()),
        "the recipe keeps the client's line"
    );
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    let probe = probe_of_a_placeholder_request(&mut r).await;
    assert_eq!(
        billing_line(&probe),
        billing_line(body.as_bytes()),
        "the probe reaches the upstream with the client's line"
    );
}

#[tokio::test]
async fn a_heartbeat_does_not_reset_idle_or_refresh() {
    let mut r = rig(RigSpec {
        policy: KeepalivePolicy {
            max_idle_ticks: 1,
            ..RigSpec::default().policy
        },
        ..RigSpec::default()
    })
    .await;
    r.observe("main").await;
    r.state("main", "idle");
    let original = r.status("main");
    r.advance(270_000);
    r.step().await;
    r.event(Event {
        event: "heartbeat".into(),
        ..Default::default()
    })
    .unwrap();
    r.advance(270_000);
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

#[tokio::test]
async fn idle_seconds_include_parked_time_and_exclude_subagents() {
    let mut r = rig(RigSpec {
        policy: KeepalivePolicy {
            max_idle_seconds: 60,
            max_idle_ticks: 1,
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
    r.observe("main").await;
    r.observe("worker").await;
    r.state("main", "parked");
    r.state("worker", "idle");
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        r.status("main").probes,
        1,
        "a parked main is not under the idle budget"
    );
    r.state("main", "idle");
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        (r.status("main").probes, r.status("worker").probes),
        (1, 2),
        "the idle budget applies to the idle main agent alone"
    );
}

#[tokio::test]
async fn a_lapsed_lease_keeps_probing_under_the_idle_limit_and_a_release_discards_at_once() {
    // The lapse case: the recipe stays, the probe at 270 s still runs, a heartbeat is refused, and the idle
    // limit counted from the last real request ends it with the wasted label idle_limit.
    let mut r = rig(RigSpec {
        limits: KeepaliveLimits {
            lease_seconds: 90,
            ..KeepaliveLimits::default()
        },
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
    assert!(
        r.engine
            .snapshot("desktop", r.generation("desktop"))
            .sessions[0]
            .ended,
        "the companion sees an ended instance"
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
        r.advance(270_000);
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
                    status.unanswered,
                    status.net_usd
                ),
                (Some("a"), None, 0, 0.0),
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
        text.contains(r#"phoebis_keepalive_probes_total{account="a",outcome="timed_out"} 1"#),
        "{text}"
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
    let mut r = rig(RigSpec {
        limits: KeepaliveLimits {
            max_workers: 1,
            lease_seconds: 3_600,
            ..KeepaliveLimits::default()
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
    r.observe("main").await;
    r.observe("worker").await;
    r.up.push_credential_rule(
        A,
        "/v1/messages",
        Respond::Gated(Box::new(probe_answer(2_000, 0))),
    );
    r.advance(270_000);
    r.engine.step().await;
    wait_for(std::time::Duration::from_secs(5), || {
        (r.up.requests().len() == 3).then_some(())
    })
    .await;
    r.engine.step().await;
    assert_eq!(
        (
            r.engine.stats().running_probes,
            r.status("main").probes + r.status("worker").probes
        ),
        (1, 1),
        "one worker, one probe in flight, the other waits"
    );
    r.up.release();
    r.engine.wait_idle().await;
    r.step().await;
    assert_eq!(
        r.status("main").probes + r.status("worker").probes,
        2,
        "the capacity recovered"
    );
}

#[tokio::test]
async fn a_migration_does_not_accumulate_a_cold_streak() {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0), api_key("b", B, 0)],
        policy: KeepalivePolicy {
            cold_probe_limit: 3,
            ..RigSpec::default().policy
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
    let other_id = if first == "a" { "b" } else { "a" };
    assert!(
        text.contains(&format!(
            r#"phoebis_keepalive_probes_total{{account="{other_id}",outcome="migrated"}} 1"#
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            r#"phoebis_keepalive_probes_total{{account="{other_id}",outcome="ok"}} 2"#
        )),
        "{text}"
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
        .set_state("a", AccountState::Disconnected)
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

/// The day the cap charges against is read under the same lock that charges it, so a charge stamped with a
/// day before the one already summed must not roll the sum back to it.
#[tokio::test]
async fn a_probe_that_read_its_clock_before_midnight_does_not_roll_the_day_back() {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        // The jump across midnight below is many times the default idle budget and the session expiry, and
        // this case is about the day roll alone.
        policy: KeepalivePolicy {
            max_idle_seconds: 0,
            ..RigSpec::default().policy
        },
        session_expiry: std::time::Duration::from_secs(1_000_000_000),
        ..RigSpec::default()
    })
    .await;
    r.observe("main").await;
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 1, "the day's first charge");
    let midnight = (r.now() / 86_400_000 + 1) * 86_400_000;
    r.clock
        .store(midnight + 1_000, std::sync::atomic::Ordering::SeqCst);
    r.step().await;
    assert_eq!(r.status("main").probes, 2, "the next day's own charge");
    let after_the_roll = r.engine.spent_today();
    // Nothing is due at this stamp, so only `roll_day`'s own comparison is exercised: a day already summed
    // must not read as behind a day that came before it.
    r.clock
        .store(NOW + 270_000, std::sync::atomic::Ordering::SeqCst);
    r.engine.step().await;
    assert_eq!(
        r.engine.spent_today(),
        after_the_roll,
        "the earlier stamp left the day already summed alone"
    );
}

#[tokio::test]
async fn retries_back_off_with_the_pools_settings_and_stop_at_the_limit() {
    let mut r = rig(RigSpec {
        policy: KeepalivePolicy {
            cold_probe_limit: 3,
            retry_seconds: 30,
            retry_max_seconds: 45,
            ..RigSpec::default().policy
        },
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
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        (r.status("main").failures, r.status("main").next_probe_at),
        (1, Some(r.now() + 30_000)),
        "retry_seconds after the first failure"
    );
    r.up.push_credential_rule(A, "/v1/messages", server_error());
    r.advance(30_000);
    r.step().await;
    assert_eq!(
        (r.status("main").failures, r.status("main").next_probe_at),
        (2, Some(r.now() + 45_000)),
        "doubled, then capped at retry_max_seconds"
    );
    r.up.push_credential_rule(A, "/v1/messages", server_error());
    r.advance(45_000);
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
        .set_state("a", AccountState::Disconnected)
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
    let (cost, _) = phoebis_ledger::PriceTable::shipped()
        .estimate(FAST_PRICED, speed, 0, 2_000, 0, 0, 0)
        .expect("a priced model");
    (r, cost)
}

/// A fast-mode request draws usage credits and no plan window, and its probe replays the body as sent, so the
/// probe does too: it settles in dollars at the fast cells on an account whose standard traffic spends plan
/// quota, and the account's window share is untouched.
#[tokio::test]
async fn a_fast_recipes_probe_settles_in_dollars_at_the_fast_cells_on_a_plan_funded_account() {
    let (r, fast) = fast_probe_served("fast", phoebis_ledger::Speed::Fast).await;
    let (standard, _) = phoebis_ledger::PriceTable::shipped()
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
    let status = r.status("main");
    assert!(
        (status.net_usd + fast).abs() < 1e-12,
        "the probe settled in dollars at the fast cells: {status:?}"
    );
    assert_eq!(
        status.net_five_hour_share, 0.0,
        "the account's share balance is unchanged"
    );
    assert_eq!(
        r.engine
            .share_spent_today()
            .get("o")
            .copied()
            .unwrap_or(0.0),
        0.0,
        "no window share was spent"
    );
    assert!(
        (r.engine.spent_today().get("main").copied().unwrap_or(0.0) - fast).abs() < 1e-12,
        "the day's dollar figure carries it: {:?}",
        r.engine.spent_today()
    );
}

/// The answer's own `usage.speed` picks the cells, as it does for a request: a fast recipe the upstream served
/// at standard speed still settles in dollars, at the standard cells.
#[tokio::test]
async fn a_fast_recipe_served_standard_prices_at_the_standard_cells() {
    let (r, standard) = fast_probe_served("standard", phoebis_ledger::Speed::Standard).await;
    let status = r.status("main");
    assert!(
        (status.net_usd + standard).abs() < 1e-12 && status.net_five_hour_share == 0.0,
        "dollars at the standard cells: {status:?}"
    );
}
