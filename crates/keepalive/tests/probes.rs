//! Probes through the relay's failover loop: what a probe carries, what its result does to the schedule, and
//! how a late or a failed one is classified; the Go probe suite.
mod common;
use std::str::FromStr;

use phoebis_keepalive::{Event, KeepaliveLimits, KeepalivePolicy, Reason};
use common::*;
use fake_upstream::Respond;

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
    let snapshot = r.engine.snapshot("desktop");
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
            r.db.read(|c| {
                phoebis_ledger::list_requests(
                    c,
                    &phoebis_ledger::RequestFilter {
                        include_probes: true,
                        ..Default::default()
                    },
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
        r.engine.snapshot("desktop").sessions[0].ended,
        "the companion sees an ended instance"
    );
    assert_eq!(
        r.engine
            .apply_event(
                "desktop",
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
    tokio::time::advance(phoebis_keepalive::PROBE_DEADLINE + std::time::Duration::from_secs(1)).await;
    tokio::time::resume();
    r.engine.wait_idle().await;
    r.pump(None).await;
    assert_eq!(
        (r.status("main").reason, r.status("main").next_probe_at),
        (Reason::ProbeTransportError, None)
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
    // An upstream 401 never reaches the engine as a 401: the relay marks the account and fails over, and with
    // no account left answers the untimed exhaustion 503, which the engine reads as the authentication pause.
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
