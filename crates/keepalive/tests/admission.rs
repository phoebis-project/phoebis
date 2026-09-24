//! What the engine retains from the bus, and what it refuses: the registration rule, the owner header, the
//! pool and key gates, the account kinds, and the storage bounds; the Go admission cases.
mod common;
use std::time::Duration;

use phoebis_bus::{Event as BusEvent, Kind, Outcome, Phase, ResponseChunk, Usage};
use phoebis_keepalive::{AgentState, Event, KeepaliveLimits, KeepalivePolicy, Reason};
use common::*;

/// The ledger row for a request, once the writer has landed it.
async fn landed(r: &Rig, id: &str) {
    fake_upstream::wait_for_async(Duration::from_secs(5), || async {
        r.db.request(id).await.unwrap()
    })
    .await;
}

fn usage(read: u64, write_5m: u64) -> Usage {
    Usage {
        cache_read_tokens: Some(read),
        cache_write_5m_tokens: Some(write_5m),
        output_tokens: Some(10),
        ..Default::default()
    }
}

#[tokio::test]
async fn a_real_request_through_the_relay_arms_a_recipe_and_a_probe_is_ignored() {
    let mut r = rig(RigSpec::default()).await;
    let id = r.observe("main").await;
    let status = r.status("main");
    assert_eq!(
        (
            status.reason,
            status.account_id.as_deref(),
            status.cache_ttl_ms
        ),
        (Reason::Scheduled, Some("a"), 300_000)
    );
    assert_eq!(
        status.next_probe_at,
        Some(r.now() + 270_000),
        "cache start plus TTL less the 30 s lead, no jitter"
    );
    assert!(r.engine.stats().retained_bytes > 0);
    // The ledger writes on its own subscriber task, a step behind the engine's, so the row lands a moment
    // after `observe` already saw the completion.
    landed(&r, &id).await;
    // The engine's own probe, published under origin keepalive, changes nothing.
    let resp =
        r.h.relay
            .probe(
                phoebis_keepalive::probe_marker(
                    &r.engine,
                    "desktop",
                    r.h.store.api_key("desktop").unwrap().unwrap().created_at,
                ),
                r.request("main"),
            )
            .await;
    assert_eq!(resp.status(), 200);
    r.pump(None).await;
    assert_eq!(r.status("main").probes, 0);
    assert_eq!(
        r.status("main").next_probe_at,
        status.next_probe_at,
        "an ignored probe did not move the schedule"
    );
}

/// Token counting carries the same session and agent headers as an inference request, and its body has no
/// `max_tokens`. Nothing about it proves or disproves a cache entry, so the recipe the last inference armed
/// stays exactly as it was, including the request time the kept-warm gap is measured from.
#[tokio::test]
async fn a_count_tokens_request_leaves_the_armed_recipe_alone() {
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    let armed = r.status("main");
    let recipes = r.engine.recipes();
    assert_eq!(recipes.len(), 1);
    let counting = BODY.replace(r#""max_tokens":1024,"#, "");
    let BusEvent::RequestObserved(mut observed) = r.observed("counting", "main", &counting) else {
        unreachable!()
    };
    observed.kind = Kind::CountTokens;
    r.engine.observe(BusEvent::RequestObserved(observed));
    assert_eq!(r.status("main"), armed);
    assert_eq!(r.engine.recipes(), recipes);
}

#[tokio::test]
async fn admission_needs_the_pool_toggle_and_no_key_opt_out() {
    let mut r = rig(RigSpec {
        policy: KeepalivePolicy {
            enabled: false,
            ..RigSpec::default().policy
        },
        ..RigSpec::default()
    })
    .await;
    r.observe("main").await;
    assert_eq!(
        (r.status("main").reason, r.status("main").next_probe_at),
        (Reason::KeepaliveOff, None)
    );
    assert_eq!(
        r.engine.reasons().get(&Reason::KeepaliveOff).copied(),
        Some(1),
        "the same refusal counted once, not once at selection and again at completion"
    );
    let mut pool = r.h.store.pool("main").unwrap().unwrap();
    pool.keepalive_json = serde_json::to_string(&RigSpec::default().policy).unwrap();
    r.h.store.update_pool(&pool, "test").unwrap();
    r.h.router.reload().await.unwrap();
    r.observe("main").await;
    assert_eq!(
        r.status("main").reason,
        Reason::Scheduled,
        "enabling the pool retains the next request"
    );
    let mut key = r.h.store.api_key("desktop").unwrap().unwrap();
    key.keepalive_opt_out = true;
    r.h.store.update_api_key(&key, "test").unwrap();
    r.h.router.reload().await.unwrap();
    r.observe("main").await;
    assert_eq!(
        (r.status("main").reason, r.status("main").next_probe_at),
        (Reason::OptedOut, None)
    );
    assert_eq!(
        r.engine.stats().retained_bytes,
        0,
        "a refused request retains nothing"
    );
}

#[tokio::test]
async fn an_unverified_oauth_account_waits_but_a_measured_one_retains() {
    let oauth_spec = || RigSpec {
        accounts: vec![oauth("o", "sk-ant-oat01-OOOO", 0)],
        ..RigSpec::default()
    };
    let mut r = rig(oauth_spec()).await;
    r.observe("main").await;
    assert_eq!(r.status("main").reason, Reason::OauthUnverified);
    let mut r = rig(RigSpec {
        limits: KeepaliveLimits {
            oauth_prewarm_verified: true,
            ..oauth_spec().limits
        },
        ..oauth_spec()
    })
    .await;
    r.observe("main").await;
    assert_eq!(r.status("main").reason, Reason::Scheduled);
}

#[tokio::test]
async fn a_replaced_registration_and_a_foreign_owner_header_are_not_retained() {
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    assert_eq!(r.status("main").reason, Reason::Scheduled);
    // A replacement from another instance discards the recipes and owns the session from here on.
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
                owner_id: Some("owner-2".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        (r.engine.stats().retained_bytes, r.status("main").reason),
        (0, Reason::AwaitingRequest)
    );
    // Traffic with the old owner's header is ledgered and not retained; traffic without the header is admitted
    // under the registration alone; traffic with the new owner's header is retained.
    let mut req = r.request("main");
    relay_common::set_header(&mut req, "x-keepalive-owner-id", "owner-1");
    let resp = r.h.app.clone().oneshot(req).await.unwrap();
    let id = relay_common::outcome(&resp).request_id;
    let _ = resp.into_body().collect().await.unwrap();
    r.pump(Some(&id)).await;
    // The ledger writes on its own subscriber task, a step behind the engine's, so the row lands a moment
    // after the completion `pump` already observed.
    landed(&r, &id).await;
    assert_eq!(
        (r.status("main").reason, r.status("main").next_probe_at),
        (Reason::AwaitingRequest, None)
    );
    assert_eq!(
        r.engine.reasons().get(&Reason::OwnerMismatch).copied(),
        Some(1)
    );
    r.observe("main").await;
    assert_eq!(
        r.status("main").reason,
        Reason::Scheduled,
        "no header: the registration rule alone admits it"
    );
    let mut req = r.request("main");
    relay_common::set_header(&mut req, "x-keepalive-owner-id", "owner-2");
    let resp = r.h.app.clone().oneshot(req).await.unwrap();
    let id = relay_common::outcome(&resp).request_id;
    let _ = resp.into_body().collect().await.unwrap();
    r.pump(Some(&id)).await;
    assert_eq!(r.status("main").reason, Reason::Scheduled);
    let sent = r.up.requests();
    assert!(
        sent.iter().all(|s| !s
            .headers
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case("x-keepalive-owner-id"))),
        "the owner header never goes upstream"
    );
    // A later owner-mismatched request against an armed recipe changes nothing about it either: not its
    // reason, not its schedule, not what it retains.
    let armed = r.status("main");
    assert!(armed.next_probe_at.is_some());
    let retained = r.engine.stats().retained_bytes;
    let mut req = r.request("main");
    relay_common::set_header(&mut req, "x-keepalive-owner-id", "owner-1");
    let resp = r.h.app.clone().oneshot(req).await.unwrap();
    let id = relay_common::outcome(&resp).request_id;
    let _ = resp.into_body().collect().await.unwrap();
    r.pump(Some(&id)).await;
    assert_eq!(
        (
            r.status("main").reason,
            r.status("main").next_probe_at,
            r.engine.stats().retained_bytes
        ),
        (armed.reason, armed.next_probe_at, retained)
    );
    assert_eq!(
        r.engine.reasons().get(&Reason::OwnerMismatch).copied(),
        Some(2),
        "once more than the earlier mismatch"
    );
}

#[tokio::test]
async fn a_stale_lifecycle_report_cannot_override_a_new_request() {
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    let old = r.status("main").request_generation;
    r.engine.observe(r.observed("new", "main", BODY));
    let stale = r.engine.apply_event(
        "desktop",
        r.generation("desktop"),
        Event {
            event: "state".into(),
            session_id: "session".into(),
            instance_id: "instance".into(),
            sequence: r.sequence + 1,
            state: Some("released".into()),
            request_generation: Some(old),
            ..Default::default()
        },
    );
    assert!(stale.is_err());
    assert_eq!(r.status("main").state, AgentState::Busy);
    // A late completion about the old request arms nothing: it is no longer pending.
    r.engine
        .observe(r.complete("old", Outcome::Succeeded, Some(usage(2_000, 0))));
    assert_eq!(r.status("main").next_probe_at, None);
}

#[tokio::test]
async fn a_later_selection_supersedes_an_earlier_attempts_evidence() {
    let r = rig(RigSpec::default()).await;
    r.engine.observe(r.observed("req", "main", BODY));
    r.engine.observe(r.selected("req", "a"));
    r.engine
        .observe(phoebis_bus::Event::AccountSelected(phoebis_bus::AccountSelected {
            request_id: "req".into(),
            attempt: 2,
            account_id: "b".into(),
            account_kind: "api_key".into(),
            pin_epoch: Some(2),
            started_at: r.now(),
            cache_ttl_ms: None,
        }));
    // Evidence stamped with the first attempt arrives after the second selection and is ignored.
    r.engine
        .observe(phoebis_bus::Event::ResponseChunk(ResponseChunk {
            request_id: "req".into(),
            attempt: 1,
            index: 0,
            body: Default::default(),
            phase: Some(Phase::MessageStart {
                usage: usage(2_000, 0),
            }),
        }));
    assert_eq!(
        r.status("main").next_probe_at,
        None,
        "a stale attempt's evidence armed nothing"
    );
    r.engine
        .observe(phoebis_bus::Event::ResponseChunk(ResponseChunk {
            request_id: "req".into(),
            attempt: 2,
            index: 0,
            body: Default::default(),
            phase: Some(Phase::MessageStart {
                usage: usage(2_000, 0),
            }),
        }));
    assert_eq!(
        (
            r.status("main").next_probe_at,
            r.status("main").account_id.as_deref()
        ),
        (Some(r.now() + 270_000), Some("b"))
    );
}

#[tokio::test]
async fn a_stream_arms_before_completion_and_a_cancel_discards() {
    let r = rig(RigSpec::default()).await;
    r.engine.observe(r.observed("req", "main", BODY));
    r.engine.observe(r.selected("req", "a"));
    r.engine
        .observe(phoebis_bus::Event::ResponseChunk(ResponseChunk {
            request_id: "req".into(),
            attempt: 1,
            index: 0,
            body: Default::default(),
            phase: Some(Phase::MessageStart {
                usage: usage(2_000, 0),
            }),
        }));
    assert!(
        r.status("main").next_probe_at.is_some(),
        "the stream armed the schedule before completion"
    );
    r.engine.observe(r.complete("req", Outcome::Canceled, None));
    assert_eq!(
        (
            r.engine.stats().retained_bytes,
            r.status("main").next_probe_at,
            r.status("main").reason
        ),
        (0, None, Reason::RequestCanceled)
    );
    for (outcome, reason) in [
        (Outcome::Failed, Reason::RequestFailed),
        (Outcome::Rejected, Reason::RequestRejected),
        (Outcome::Exhausted, Reason::RequestFailed),
    ] {
        r.engine.observe(r.observed("again", "main", BODY));
        r.engine.observe(r.complete("again", outcome, None));
        assert_eq!(r.status("main").reason, reason);
    }
}

#[tokio::test]
async fn storage_bounds_hold_and_the_status_carries_no_prompt() {
    let mut r = rig(RigSpec {
        limits: KeepaliveLimits {
            max_agents: 1,
            max_sessions: 1,
            max_request_bytes: 1_024,
            max_retained_bytes: 1_024,
            lease_seconds: 3_600,
            ..KeepaliveLimits::default()
        },
        ..RigSpec::default()
    })
    .await;
    r.engine.observe(r.observed("req", "main", BODY));
    r.engine.observe(r.selected("req", "a"));
    r.engine
        .observe(r.complete("req", Outcome::Succeeded, Some(usage(0, 2_000))));
    let encoded = format!(
        "{:?}{:?}",
        r.status("main"),
        r.engine.snapshot("desktop", r.generation("desktop"))
    );
    assert!(
        !encoded.contains("private prompt")
            && !encoded.contains(KEY)
            && !encoded.contains("test-beta")
    );
    assert!(
        r.event(Event {
            event: "agent_start".into(),
            agent_id: Some("worker".into()),
            ..Default::default()
        })
        .is_err(),
        "the agent limit holds"
    );
    let held = r.engine.stats().retained_bytes;
    assert!(held > 0);
    let large = format!(
        r#"{{"model":"m","max_tokens":10,"messages":[{{"role":"user","content":"{}"}}]}}"#,
        "x".repeat(1_025)
    );
    r.engine.observe(r.observed("large", "main", &large));
    assert_eq!(
        (r.engine.stats().retained_bytes, r.status("main").reason),
        (0, Reason::RequestTooLarge),
        "an oversized request discards the old recipe and retains nothing"
    );
    let mut r2 = rig(RigSpec {
        limits: KeepaliveLimits {
            max_request_bytes: 8 << 20,
            max_retained_bytes: 1_024,
            lease_seconds: 3_600,
            ..KeepaliveLimits::default()
        },
        ..RigSpec::default()
    })
    .await;
    r2.event(Event {
        event: "agent_start".into(),
        agent_id: Some("worker".into()),
        ..Default::default()
    })
    .unwrap();
    let mid = format!(
        r#"{{"model":"m","max_tokens":10,"messages":[{{"role":"user","content":"{}"}}]}}"#,
        "y".repeat(600)
    );
    r2.engine.observe(r2.observed("one", "main", &mid));
    r2.engine.observe(r2.observed("two", "worker", &mid));
    assert_eq!(
        r2.status("worker").reason,
        Reason::StorageLimit,
        "the second recipe would pass the retained bound"
    );
    assert_eq!(r2.status("main").reason, Reason::AwaitingSelection);
}

#[tokio::test]
async fn an_oversized_request_invalidates_only_its_own_agent() {
    for child in [false, true] {
        let mut r = rig(RigSpec {
            limits: KeepaliveLimits {
                max_request_bytes: 1_024,
                lease_seconds: 3_600,
                ..KeepaliveLimits::default()
            },
            ..RigSpec::default()
        })
        .await;
        r.observe("main").await;
        let main_generation = r.status("main").request_generation;
        let agent = if child {
            r.event(Event {
                event: "agent_start".into(),
                agent_id: Some("worker".into()),
                ..Default::default()
            })
            .unwrap();
            r.observe("worker").await;
            "worker"
        } else {
            "main"
        };
        let old = r.status(agent).request_generation;
        let large = format!(
            r#"{{"model":"m","max_tokens":10,"messages":[{{"role":"user","content":"{}"}}]}}"#,
            "x".repeat(2_048)
        );
        r.engine.observe(r.observed("oversized", agent, &large));
        let status = r.status(agent);
        assert!(
            status.request_generation > old
                && status.next_probe_at.is_none()
                && status.reason == Reason::RequestTooLarge,
            "{status:?}"
        );
        if child {
            assert_eq!(
                r.status("main").request_generation,
                main_generation,
                "the child's request did not touch main"
            );
        }
    }
}

#[tokio::test]
async fn unknown_ownership_needs_a_lifecycle_snapshot_and_a_restart_rearms() {
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    r.state("main", "unknown");
    r.observe("main").await;
    assert_eq!(
        (
            r.status("main").state,
            r.status("main").next_probe_at,
            r.status("main").reason
        ),
        (AgentState::Unknown, None, Reason::AgentUnknown)
    );
    r.state("main", "busy");
    r.observe("main").await;
    assert!(
        r.status("main").next_probe_at.is_some(),
        "the snapshot restored observation"
    );
    r.event(Event {
        event: "agent_start".into(),
        agent_id: Some("worker".into()),
        ..Default::default()
    })
    .unwrap();
    r.observe("worker").await;
    r.state("worker", "released");
    let generation = r.status("worker").request_generation;
    r.event(Event {
        event: "agent_start".into(),
        agent_id: Some("worker".into()),
        request_generation: Some(generation),
        ..Default::default()
    })
    .unwrap();
    r.observe("worker").await;
    assert!(
        r.status("worker").next_probe_at.is_some(),
        "an explicit restart re-arms"
    );
}

#[tokio::test]
async fn a_bus_gap_discards_the_pending_observation_and_keeps_an_armed_recipe() {
    let mut r = rig(RigSpec::default()).await;
    r.event(Event {
        event: "agent_start".into(),
        agent_id: Some("worker".into()),
        ..Default::default()
    })
    .unwrap();
    r.observe("worker").await;
    assert_eq!(r.status("worker").reason, Reason::Scheduled);
    // The main agent's request is observed and selected but not completed when the queue overflows.
    r.engine.observe(r.observed("open", "main", BODY));
    r.engine.observe(r.selected("open", "a"));
    assert_eq!(r.status("main").reason, Reason::AwaitingCacheEvidence);
    // The engine's own subscriber, unread, fills past the queue depth; the observer task then sees the drop
    // counter rise before the next event it reads.
    let observer = phoebis_keepalive::spawn_observer(r.engine.clone());
    let filler = r.engine.parts_bus();
    for i in 0..(phoebis_bus::QUEUE_DEPTH + 64) {
        filler.publish(r.chunk("noise", i as u32, ""));
    }
    r.pump_until(|r| r.engine.stats().bus_gaps >= 1).await;
    assert_eq!(
        (r.status("main").reason, r.status("main").next_probe_at),
        (Reason::BusGap, None),
        "the half-observed request is gone"
    );
    assert_eq!(
        r.status("worker").reason,
        Reason::Scheduled,
        "the armed recipe stays"
    );
    assert!(r.engine.stats().retained_bytes > 0);
    // A completion for the discarded request lands nowhere.
    r.engine
        .observe(r.complete("open", Outcome::Succeeded, Some(usage(2_000, 0))));
    assert_eq!(r.status("main").next_probe_at, None);
    observer.abort();
}

#[tokio::test]
async fn reason_counters_measure_the_gates_and_survive_close() {
    let mut r = rig(RigSpec::default()).await;
    let oversized = format!(r#"{{"pad":"{}"}}"#, "x".repeat(8 << 20));
    r.engine
        .observe(r.observed("oversized", "main", &oversized));
    assert_eq!(
        r.engine.reasons().get(&Reason::RequestTooLarge).copied(),
        Some(1)
    );
    r.observe("main").await;
    assert!(
        r.engine
            .reasons()
            .get(&Reason::Scheduled)
            .copied()
            .unwrap_or(0)
            > 0
    );
    r.engine.close();
    assert_eq!(
        r.engine.reasons().get(&Reason::RequestTooLarge).copied(),
        Some(1),
        "close keeps the counters"
    );
    assert_eq!(r.engine.stats().retained_bytes, 0);
}

/// A gap drops one event and leaves the rest of the queue behind it. A request observed inside that backlog
/// may be one whose terminal event the gap dropped, so nothing it carries may arm a recipe; once the backlog
/// has drained, the next request arms as before.
#[tokio::test]
async fn a_request_observed_inside_a_gaps_backlog_never_arms() {
    let r = rig(RigSpec::default()).await;
    // Three events were queued behind the gap: this request's own completion may be the one it dropped.
    r.engine.bus_gap(1, 3);
    r.engine.observe(r.observed("inside", "main", BODY));
    r.engine.observe(r.selected("inside", "a"));
    r.engine
        .observe(r.complete("inside", Outcome::Succeeded, Some(usage(2_000, 0))));
    assert_eq!(
        (
            r.status("main").reason,
            r.status("main").next_probe_at,
            r.engine.stats().retained_bytes
        ),
        (Reason::BusGap, None, 0),
        "the request inside the backlog armed nothing"
    );
    // The backlog has drained, so the next request is evidence again.
    r.engine.observe(r.observed("after", "main", BODY));
    r.engine.observe(r.selected("after", "a"));
    r.engine
        .observe(r.complete("after", Outcome::Succeeded, Some(usage(2_000, 0))));
    assert_eq!(r.status("main").reason, Reason::Scheduled);
    assert!(r.status("main").next_probe_at.is_some() && r.engine.stats().retained_bytes > 0);
}

#[tokio::test]
async fn warm_subagents_off_retains_the_main_agent_and_declines_a_subagent() {
    let mut r = rig(RigSpec {
        policy: KeepalivePolicy {
            enabled: true,
            warm_subagents: false,
            ..Default::default()
        },
        ..Default::default()
    })
    .await;
    r.event(Event {
        event: "agent_start".into(),
        agent_id: Some("sub-1".into()),
        ..Default::default()
    })
    .unwrap();
    r.observe("main").await;
    assert_eq!(r.status("main").reason, Reason::Scheduled);
    r.observe("sub-1").await;
    assert_eq!(
        r.status("sub-1").reason,
        Reason::SubagentsOff,
        "declined before a recipe is built"
    );
    assert_eq!(
        r.engine
            .recipes()
            .iter()
            .filter(|v| v.agent_id == "sub-1")
            .count(),
        0
    );
}
