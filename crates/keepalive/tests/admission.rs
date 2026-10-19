//! What the engine retains from the bus, and what it refuses: the registration rule, the owner header, the
//! pool and key gates, the account kinds, and the storage bounds; the Go admission cases.
mod common;
use std::collections::BTreeMap;
use std::time::Duration;

use common::*;
use phoebis_bus::{
    AttemptOutcome, Event as BusEvent, Kind, Outcome, Phase, ResponseChunk, ResponseHead, Usage,
};
use phoebis_keepalive::{
    AgentState, Event, Guard, KeepaliveLimits, KeepalivePolicy, Reason, RecipeState,
};
use phoebis_ledger::{hour_of, read_rollups, RollupRow, RollupSelect};

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

/// The guard's verdict on the last request the named agent observed, off the recipe view.
fn last_guard(r: &Rig, agent_id: &str) -> Option<Guard> {
    r.engine
        .recipes()
        .into_iter()
        .find(|v| v.agent_id == agent_id)
        .and_then(|v| v.last_guard)
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
    r.h.store
        .update_pool(&pool, pool.updated_at, "test")
        .unwrap();
    r.h.router.reload().await.unwrap();
    r.observe("main").await;
    assert_eq!(
        r.status("main").reason,
        Reason::Scheduled,
        "enabling the pool retains the next request"
    );
    let mut key = r.h.store.api_key("desktop").unwrap().unwrap();
    key.keepalive_opt_out = true;
    r.h.store
        .update_api_key(&key, key.updated_at, "test")
        .unwrap();
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
async fn an_oauth_account_on_an_enabled_pool_retains_its_recipe() {
    let mut r = rig(RigSpec {
        accounts: vec![oauth("o", "sk-ant-oat01-OOOO", 0)],
        ..RigSpec::default()
    })
    .await;
    r.observe("main").await;
    let status = r.status("main");
    assert_eq!(
        (status.reason, status.account_id.as_deref()),
        (Reason::Scheduled, Some("o"))
    );
    assert!(r.engine.stats().retained_bytes > 0);
}

/// The selection sets `OptedOut` and returns before the observation is ever marked ready; a title request the
/// guard sets aside in that window must leave it for the completion to read back unchanged.
#[tokio::test]
async fn a_title_request_between_an_opted_out_selection_and_its_completion_leaves_the_reason_alone()
{
    let r = rig(RigSpec::default()).await;
    let mut key = r.h.store.api_key("desktop").unwrap().unwrap();
    key.keepalive_opt_out = true;
    r.h.store
        .update_api_key(&key, key.updated_at, "test")
        .unwrap();
    r.h.router.reload().await.unwrap();
    r.engine.observe(r.observed("req", "main", BODY));
    r.engine.observe(r.selected("req", "a"));
    assert_eq!(r.status("main").reason, Reason::OptedOut);
    r.engine.observe(r.observed_with(
        "aux",
        "main",
        Some("title"),
        None,
        &[],
        BODY.as_bytes(),
        "claude-sonnet-4-5",
    ));
    assert_eq!(
        r.status("main").reason,
        Reason::OptedOut,
        "the title request left the selection's reason alone"
    );
    r.engine
        .observe(r.complete("req", Outcome::Succeeded, None));
    assert_eq!(
        r.status("main").reason,
        Reason::OptedOut,
        "the completion read the reason the selection set, not the guard's own auxiliary note"
    );
}

/// A request the guard sets aside never touches `reason`, so the admin view still reads the recipe armed
/// rather than held under the guard's own note.
#[tokio::test]
async fn an_armed_recipe_that_saw_an_auxiliary_request_still_reports_armed() {
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    r.engine.observe(r.observed_with(
        "aux",
        "main",
        Some("title"),
        None,
        &[],
        BODY.as_bytes(),
        "claude-sonnet-4-5",
    ));
    let recipe = r
        .engine
        .recipes()
        .into_iter()
        .find(|v| v.agent_id == "main")
        .expect("the armed recipe");
    assert_eq!(
        (recipe.state, recipe.cause, recipe.last_guard),
        (
            RecipeState::Retained,
            Reason::Scheduled,
            Some(Guard::Auxiliary)
        ),
        "{recipe:?}"
    );
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
    r.engine.observe(phoebis_bus::Event::AccountSelected(
        phoebis_bus::AccountSelected {
            request_id: "req".into(),
            attempt: 2,
            account_id: "b".into(),
            account_kind: "api_key".into(),
            pin_epoch: Some(2),
            started_at: r.now(),
            cache_ttl_ms: None,
            selection: None,
        },
    ));
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
        (Outcome::Incomplete, Reason::RequestFailed),
        (Outcome::Canceled, Reason::RequestCanceled),
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
            max_retained_bytes: 32 << 20,
        },
        ..RigSpec::default()
    })
    .await;
    r.engine.observe(r.observed("req", "main", BODY));
    r.engine.observe(r.selected("req", "a"));
    r.engine
        .observe(r.complete("req", Outcome::Succeeded, Some(usage(0, 2_000))));
    let encoded = format!("{:?}{:?}", r.status("main"), r.reregister());
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
    // Past the relay's 32 MiB body bound, which is the largest body a recipe may retain.
    let large = format!(
        r#"{{"model":"m","max_tokens":10,"messages":[{{"role":"user","content":"{}"}}]}}"#,
        "x".repeat(32 << 20)
    );
    r.engine.observe(r.observed("large", "main", &large));
    assert_eq!(
        (r.engine.stats().retained_bytes, r.status("main").reason),
        (0, Reason::RequestTooLarge),
        "an oversized request discards the old recipe and retains nothing"
    );
    let mut r2 = rig(RigSpec {
        limits: KeepaliveLimits {
            max_retained_bytes: 32 << 20,
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
    // Two recipes over 16 MiB each pass the 32 MiB budget together.
    let mid = format!(
        r#"{{"model":"m","max_tokens":10,"messages":[{{"role":"user","content":"{}"}}]}}"#,
        "y".repeat(17 << 20)
    );
    r2.engine.observe(r2.observed("one", "main", &mid));
    r2.engine.observe(r2.observed("two", "worker", &mid));
    assert_eq!(
        r2.status("worker").reason,
        Reason::StorageLimit,
        "the second recipe would pass the retained bound"
    );
    assert_eq!(
        r2.status("main").reason,
        Reason::AwaitingSelection,
        "a busy agent's recipe is never evicted, so at the bound the newest one is refused"
    );
    // Room for two recipes: the third evicts the oldest idle one, whose unanswered probe settles under the
    // eviction, and keeps the active agent's.
    let one = {
        let mut sized = rig(RigSpec::default()).await;
        sized.observe("main").await;
        sized.engine.stats().retained_bytes
    };
    let mut r3 = rig(RigSpec {
        limits: KeepaliveLimits {
            max_retained_bytes: one * 5 / 2,
            ..KeepaliveLimits::default()
        },
        ..RigSpec::default()
    })
    .await;
    for agent in ["active", "newest"] {
        r3.event(Event {
            event: "agent_start".into(),
            agent_id: Some(agent.into()),
            ..Default::default()
        })
        .unwrap();
    }
    r3.observe("main").await;
    r3.state("main", "idle");
    r3.advance_attended(270_000);
    r3.step().await;
    assert_eq!(
        r3.status("main").unanswered,
        1,
        "one probe nobody came back for"
    );
    r3.observe("active").await;
    r3.observe("newest").await;
    assert_eq!(r3.status("main").reason, Reason::Evicted);
    assert_eq!(r3.status("newest").reason, Reason::Scheduled);
    let mut kept = r3.engine.recipes_for("desktop", "session");
    kept.sort();
    assert_eq!(kept, ["active", "newest"]);
    let evicted = fake_upstream::wait_for(Duration::from_secs(5), || {
        let rows = r3
            .db
            .read_blocking(|c| {
                read_rollups(c, hour_of(NOW) - 3_600, i64::MAX, &RollupSelect::default())
            })
            .unwrap();
        let evicted: u64 = rows
            .iter()
            .filter_map(|row| row.wasted_probe_reasons.get("evicted"))
            .sum();
        (evicted > 0).then_some(evicted)
    })
    .await;
    assert_eq!(
        evicted, 1,
        "the evicted recipe's probe is wasted under the eviction"
    );
}

/// A recipe of about `bytes` bytes armed on the engine's side alone, for `agent` of `session`, on account `a`.
fn arm(r: &Rig, id: &str, session: &str, agent: &str, bytes: usize) {
    let body = format!(
        r#"{{"model":"claude-sonnet-4-5","max_tokens":10,"messages":[{{"role":"user","content":"{}"}}]}}"#,
        "y".repeat(bytes)
    );
    let BusEvent::RequestObserved(mut observed) = r.observed(id, agent, &body) else {
        unreachable!("observed builds an observation")
    };
    observed.session_id = Some(session.into());
    r.engine.observe(BusEvent::RequestObserved(observed));
    r.engine.observe(r.selected(id, "a"));
    r.engine
        .observe(r.complete(id, Outcome::Succeeded, Some(usage(0, 2_000))));
}

fn start_agent(r: &mut Rig, agent: &str) {
    r.event(Event {
        event: "agent_start".into(),
        agent_id: Some(agent.into()),
        ..Default::default()
    })
    .unwrap();
}

/// With room for two recipes and a half, a third evicts one: the oldest idle recipe before a newer one, an unattended
/// session's before an older idle one of an attended session, and none at all when evicting every candidate would
/// still leave the newest without room, which is then refused.
#[tokio::test]
async fn eviction_takes_unattended_then_the_oldest_idle_and_nothing_when_it_cannot_make_room() {
    let spec = || RigSpec {
        limits: KeepaliveLimits {
            max_retained_bytes: 25_000,
            ..KeepaliveLimits::default()
        },
        ..RigSpec::default()
    };
    let mut r = rig(spec()).await;
    for agent in ["older", "newer", "third"] {
        start_agent(&mut r, agent);
    }
    arm(&r, "older", "session", "older", 10_000);
    r.state("older", "idle");
    r.advance_attended(10_000);
    arm(&r, "newer", "session", "newer", 10_000);
    r.state("newer", "idle");
    r.advance_attended(10_000);
    arm(&r, "third", "session", "third", 10_000);
    let mut kept = r.engine.recipes_for("desktop", "session");
    kept.sort();
    assert_eq!(
        (r.status("older").reason, kept),
        (
            Reason::Evicted,
            vec!["newer".to_string(), "third".to_string()]
        ),
        "the oldest idle recipe goes, the newer one stays"
    );

    let mut r = rig(spec()).await;
    r.event(session_start_as("other", "second", None)).unwrap();
    for agent in ["idle", "newest"] {
        start_agent(&mut r, agent);
    }
    arm(&r, "idle", "session", "idle", 10_000);
    r.state("idle", "idle");
    // Past the lease of `other`, which no heartbeat renews, while `session` stays attended.
    r.advance_attended(100_000);
    arm(&r, "unattended", "other", "main", 10_000);
    arm(&r, "newest", "session", "newest", 10_000);
    let unattended = r.engine.agent_status("desktop", "other", "main").unwrap();
    assert_eq!(
        (unattended.reason, r.status("idle").reason),
        (Reason::Evicted, Reason::Scheduled),
        "the unattended recipe goes before the older idle one of the attended session"
    );

    let mut r = rig(spec()).await;
    for agent in ["busy", "idle", "large"] {
        start_agent(&mut r, agent);
    }
    arm(&r, "busy", "session", "busy", 10_000);
    arm(&r, "idle", "session", "idle", 10_000);
    r.state("idle", "idle");
    arm(&r, "large", "session", "large", 16_000);
    let mut kept = r.engine.recipes_for("desktop", "session");
    kept.sort();
    assert_eq!(
        (r.status("large").reason, kept),
        (
            Reason::StorageLimit,
            vec!["busy".to_string(), "idle".to_string()]
        ),
        "evicting the idle recipe would not make room, so nothing is evicted"
    );
}

#[tokio::test]
async fn an_oversized_request_invalidates_only_its_own_agent() {
    for child in [false, true] {
        let mut r = rig(RigSpec {
            limits: KeepaliveLimits {
                max_retained_bytes: 32 << 20,
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
            "x".repeat(32 << 20)
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

/// A billing line stamping a hash; its digits need not reproduce, since a stamp the replay cannot reach is
/// refused before any restamp.
const STAMPED_LINE: &str =
    "x-anthropic-billing-header: cc_version=2.1.269.517; cc_entrypoint=sdk-cli; cch=abcde;";

#[tokio::test]
async fn a_body_whose_edited_member_is_spelled_with_an_escape_is_not_retained() {
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    for body in [
        // `text` escaped: the decoded walk reads the stamp, the raw walk the restamp edits through does not.
        format!(
            r#"{{"model":"m","max_tokens":10,"system":[{{"type":"text","\u0074ext":"{STAMPED_LINE}"}}],"messages":[{{"role":"user","content":"hi"}}]}}"#
        ),
        // `stream` escaped: the replay would insert a second flag beside it.
        r#"{"model":"m","max_tokens":10,"\u0073tream":true,"messages":[{"role":"user","content":"hi"}]}"#
            .to_string(),
    ] {
        r.engine.observe(r.observed("escaped", "main", &body));
        let status = r.status("main");
        assert!(
            status.next_probe_at.is_none() && status.reason == Reason::InvalidRequest,
            "{body}: {status:?}"
        );
    }
}

#[tokio::test]
async fn unknown_ownership_needs_a_state_report_and_a_restart_rearms() {
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
        "the state report restored observation"
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
    // Past the relay's 32 MiB body bound, the largest body a recipe may retain.
    let oversized = format!(r#"{{"pad":"{}"}}"#, "x".repeat(32 << 20));
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

/// An auxiliary or a foreign request must leave an armed recipe's schedule and retained bytes exactly as they
/// were, proving the guard set the request aside before it ever reached `make_recipe`.
#[tokio::test]
async fn a_title_shaped_request_on_another_model_leaves_the_recipe_alone() {
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    let armed_probe_at = r.status("main").next_probe_at;
    let armed_bytes = r.engine.stats().retained_bytes;
    r.engine.observe(r.observed_with(
        "aux",
        "main",
        Some("auxiliary"),
        None,
        &[],
        BODY.as_bytes(),
        "claude-sonnet-4-5",
    ));
    assert_eq!(
        (
            r.status("main").next_probe_at,
            r.engine.stats().retained_bytes
        ),
        (armed_probe_at, armed_bytes),
        "an auxiliary class leaves the schedule and the recipe alone"
    );
    assert_eq!(last_guard(&r, "main"), Some(Guard::Auxiliary));
    let other = r#"{"model":"claude-opus-5","max_tokens":1024,"messages":[{"role":"user","content":"a different conversation"}]}"#;
    r.engine.observe(r.observed_with(
        "foreign",
        "main",
        None,
        Some("a-different-static-prefix"),
        &["a-different-boundary"],
        other.as_bytes(),
        "claude-opus-5",
    ));
    assert_eq!(
        (r.status("main").next_probe_at, r.engine.stats().retained_bytes),
        (armed_probe_at, armed_bytes),
        "a request matching neither the chain nor the static prefix leaves the schedule and the recipe alone"
    );
    assert_eq!(last_guard(&r, "main"), Some(Guard::Foreign));
}

#[tokio::test]
async fn a_tool_loop_request_re_arms_the_recipe() {
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    let armed_bytes = r.engine.stats().retained_bytes;
    let extended = BODY.replace(
        r#"{"role":"user","content":"hello"}]"#,
        r#"{"role":"user","content":"hello"},{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"x","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}]"#,
    );
    assert_ne!(extended, BODY, "the extension actually changed the body");
    r.observe_with("main", &extended).await;
    assert_eq!(
        r.status("main").reason,
        Reason::Scheduled,
        "a tool round continues the conversation, so it re-arms normally"
    );
    assert!(
        r.engine.stats().retained_bytes > armed_bytes,
        "the recipe body is the new, larger one"
    );
}

/// A `compaction`-class request on main, observed on the engine's side alone.
fn compaction(r: &Rig, id: &str) -> BusEvent {
    r.observed_with(
        id,
        "main",
        Some("compaction"),
        None,
        &[],
        BODY.as_bytes(),
        "claude-sonnet-4-5",
    )
}

/// The head of `id`'s first attempt, answered at `status`.
fn head(r: &Rig, id: &str, status: u16) -> BusEvent {
    BusEvent::ResponseHead(ResponseHead {
        request_id: id.into(),
        attempt: 1,
        status,
        headers: Vec::new(),
        first_event: None,
        outcome: if (200..300).contains(&status) {
            AttemptOutcome::Succeeded
        } else {
            AttemptOutcome::UpstreamError
        },
        cooldown: None,
        written_at: r.now(),
        observed_at: r.now(),
    })
}

/// The first stream event of `id`'s first attempt, carrying the cache it read and wrote.
fn message_start(id: &str, read: u64, write: u64) -> BusEvent {
    BusEvent::ResponseChunk(ResponseChunk {
        request_id: id.into(),
        attempt: 1,
        index: 0,
        body: Default::default(),
        phase: Some(Phase::MessageStart {
            usage: usage(read, write),
        }),
    })
}

/// A turn of main on the engine's side alone: its observation, its selection on `a` and its first stream event, which
/// arms the schedule from the turn's start.
fn open_turn(r: &Rig, id: &str, read: u64, write: u64) {
    r.engine.observe(r.observed(id, "main", BODY));
    r.engine.observe(r.selected(id, "a"));
    r.engine.observe(message_start(id, read, write));
}

/// `id`'s successful completion, reading `read` and writing the prefix where it read nothing.
fn completed(r: &Rig, id: &str, read: u64) -> BusEvent {
    r.complete(
        id,
        Outcome::Succeeded,
        Some(usage(read, if read == 0 { 2_000 } else { 0 })),
    )
}

/// A compaction's whole sequence on main: its admission, its selection on `account`, a head answered at `status`, a
/// usage chunk reading `read`, and its completion under `outcome`.
fn compact(r: &Rig, id: &str, account: &str, status: u16, read: u64, outcome: Outcome) {
    r.engine.observe(compaction(r, id));
    r.engine.observe(r.selected(id, account));
    r.engine.observe(head(r, id, status));
    r.engine.observe(message_start(id, read, 0));
    r.engine.observe(r.complete(
        id,
        outcome,
        Some(usage(read, if read == 0 { 2_000 } else { 0 })),
    ));
}

/// Every rollup row, read once the ledger has landed everything the case sent it.
async fn final_rollups(r: &Rig) -> Vec<RollupRow> {
    r.ledger.clone().shutdown().await.unwrap();
    r.db.read_blocking(|c| {
        read_rollups(c, hour_of(NOW) - 3_600, i64::MAX, &RollupSelect::default())
    })
    .unwrap()
}

/// The probes the rows count, the wasted ones among them by label, and the requests credited as kept warm.
fn probes_and_credits(rows: &[RollupRow]) -> (u64, BTreeMap<String, u64>, u64) {
    let mut wasted = BTreeMap::new();
    for row in rows.iter().filter(|row| row.kind == "probe") {
        for (label, count) in &row.wasted_probe_reasons {
            *wasted.entry(label.clone()).or_insert(0) += count;
        }
    }
    (
        rows.iter()
            .filter(|row| row.kind == "probe")
            .map(|row| row.requests)
            .sum(),
        wasted,
        rows.iter().map(|row| row.kept_warm_requests).sum(),
    )
}

/// The recipes route's row of main.
fn main_recipe(r: &Rig) -> phoebis_keepalive::RecipeView {
    r.engine
        .recipes()
        .into_iter()
        .find(|v| v.agent_id == "main")
        .expect("main's row")
}

/// A compaction request neither re-arms nor restarts the recipe: the armed recipe, its schedule, its retained bytes and
/// its reason stay as the turn before left them, and the guard's verdict reads `compaction` on the recipes route and on
/// the Sessions row while the compaction is open.
#[tokio::test]
async fn a_compaction_leaves_the_armed_recipe_its_retained_bytes_and_its_reason_unchanged() {
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    let armed = r.status("main");
    let retained = r.engine.stats().retained_bytes;
    r.engine.observe(compaction(&r, "compaction"));
    assert_eq!(
        (r.status("main"), r.engine.stats().retained_bytes),
        (armed, retained),
        "the compaction left the recipe, its schedule and its bytes as they stood"
    );
    assert_eq!(last_guard(&r, "main"), Some(Guard::Compaction));
    assert_eq!(
        r.engine.sessions()[0].keepalive.last_guard,
        Some(Guard::Compaction),
        "the Sessions row reads the verdict while the compaction is open"
    );
}

/// A compaction that fails or is canceled leaves the old context the live one: after its selection on another account
/// than the recipe's, its head and its usage, the recipe, its schedule, its account and the probe it paid for stand as
/// they stood before the compaction, nothing settled, and the schedule resumes on that recipe.
async fn a_compaction_that_ends_short_keeps_the_previous_recipe(
    outcome: Outcome,
    status: u16,
    read: u64,
) {
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    r.advance_attended(270_000);
    r.step().await;
    let before = r.status("main");
    assert_eq!(
        (
            before.account_id.as_deref(),
            before.probes,
            before.unanswered,
            before.reason
        ),
        (Some("a"), 1, 1, Reason::Scheduled),
        "{before:?}"
    );
    let retained = r.engine.stats().retained_bytes;
    r.advance_attended(10_000);
    compact(&r, "compaction", "b", status, read, outcome);
    assert_eq!(
        (r.status("main"), r.engine.stats().retained_bytes),
        (before, retained),
        "{outcome:?}: the recipe, its schedule, its account and its probe stand as they stood"
    );
    assert_eq!(last_guard(&r, "main"), Some(Guard::Compaction));
    // 540 s in, the probe the first probe's entry scheduled.
    r.advance_attended(260_000);
    r.step().await;
    let resumed = r.status("main");
    assert_eq!(
        (
            resumed.probes,
            resumed.unanswered,
            resumed.reason,
            resumed.account_id.as_deref()
        ),
        (2, 2, Reason::Scheduled, Some("a")),
        "{outcome:?}: the schedule resumed on the previous recipe"
    );
}

#[tokio::test]
async fn a_failed_compaction_keeps_the_previous_recipe_its_schedule_and_its_probes() {
    a_compaction_that_ends_short_keeps_the_previous_recipe(Outcome::Failed, 529, 0).await;
}

#[tokio::test]
async fn a_canceled_compaction_keeps_the_previous_recipe_its_schedule_and_its_probes() {
    a_compaction_that_ends_short_keeps_the_previous_recipe(Outcome::Canceled, 200, 2_000).await;
}

/// A recipe armed on the engine's side 0 s in, and two probes 270 and 540 s in that each read the entry back, their own
/// entries ending 570 and 840 s in: one chain, its last entry ending 840 s in.
async fn a_chain_of_two_probes(r: &mut Rig) {
    open_turn(r, "first", 0, 2_000);
    r.engine.observe(completed(r, "first", 0));
    for _ in 0..2 {
        r.advance_attended(270_000);
        r.step().await;
    }
    let status = r.status("main");
    assert_eq!(
        (status.probes, status.unanswered, status.reason),
        (2, 2, Reason::Scheduled),
        "{status:?}"
    );
}

/// A completed compaction reads the old prefix, so it answers the chain before it: 600 s in, past the first probe's own
/// entry and inside the second's, a compaction that read the cache leaves neither wasted and earns its own credit. It
/// then stops the recipe under `compacted`, no probe goes out on the old context, and the next main turn re-arms.
#[tokio::test]
async fn a_completed_compaction_answers_the_chain_earns_its_credit_and_stops_the_recipe_until_the_next_turn(
) {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    a_chain_of_two_probes(&mut r).await;
    r.advance_attended(60_000);
    compact(&r, "compaction", "a", 200, 2_000, Outcome::Succeeded);
    let status = r.status("main");
    assert_eq!(
        (
            status.reason,
            status.next_probe_at,
            status.unanswered,
            r.engine.stats().retained_bytes
        ),
        (Reason::Compacted, None, 0, 0),
        "{status:?}"
    );
    let view = main_recipe(&r);
    assert_eq!(
        (view.state, view.cause, view.last_guard),
        (
            RecipeState::Ended,
            Reason::Compacted,
            Some(Guard::Compaction)
        ),
        "{view:?}"
    );
    assert_eq!(
        r.engine.sessions()[0].keepalive.cause,
        None,
        "the Sessions row reads null once the recipe stopped"
    );
    // 870 s in, past the stopped recipe's next due time.
    r.advance_attended(270_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 2, "no probe on the old context");
    open_turn(&r, "next", 0, 2_000);
    r.engine.observe(completed(&r, "next", 0));
    let rearmed = r.status("main");
    assert_eq!(
        (rearmed.reason, last_guard(&r, "main")),
        (Reason::Scheduled, Some(Guard::Rearm)),
        "the next main turn re-armed: {rearmed:?}"
    );
    assert!(rearmed.next_probe_at.is_some(), "{rearmed:?}");
    assert_eq!(
        probes_and_credits(&final_rollups(&r).await),
        (2, BTreeMap::new(), 1),
        "two probes, none wasted, and the compaction's own credit"
    );
}

/// The completion judges the probes a failed turn handed back and the ones sent while that turn was open as one chain:
/// the first probe's own entry ended 570 s in, and the second, which read the entry back 550 s in, extended it to 850 s,
/// so a compaction 600 s in that read the cache answers both.
#[tokio::test]
async fn a_completed_compaction_judges_the_kept_and_the_unanswered_probes_as_one_chain() {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    open_turn(&r, "first", 0, 2_000);
    r.engine.observe(completed(&r, "first", 0));
    r.advance_attended(270_000);
    r.step().await;
    r.advance_attended(10_000);
    open_turn(&r, "second", 2_000, 0);
    r.advance_attended(270_000);
    r.step().await;
    r.engine
        .observe(r.complete("second", Outcome::Failed, Some(usage(0, 0))));
    let status = r.status("main");
    assert_eq!(
        (status.probes, status.unanswered, status.reason),
        (2, 2, Reason::RequestFailed),
        "one probe handed back, one sent while the failed turn was open: {status:?}"
    );
    r.advance_attended(50_000);
    compact(&r, "compaction", "a", 200, 2_000, Outcome::Succeeded);
    let status = r.status("main");
    assert_eq!(
        (status.unanswered, status.reason),
        (0, Reason::Compacted),
        "{status:?}"
    );
    assert_eq!(
        probes_and_credits(&final_rollups(&r).await),
        (2, BTreeMap::new(), 1),
        "both probes answered as one chain, and the compaction's credit"
    );
}

/// A completed compaction that read nothing settles the probes before it under the standard test, the count the Cache
/// warming card reads: 600 s in, the first probe's chain ended with its own entry 570 s in, since the second probe read
/// nothing and wrote a new entry, so the first settles `expired` and the second, whose entry ends 840 s in, a cold
/// return. The recipe stops under `compacted` all the same.
#[tokio::test]
async fn a_completed_compaction_that_reads_nothing_settles_the_probes_before_it_and_stops_the_recipe(
) {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    open_turn(&r, "first", 0, 2_000);
    r.engine.observe(completed(&r, "first", 0));
    r.advance_attended(270_000);
    r.step().await;
    r.up.push_credential_rule(A, "/v1/messages", probe_answer(0, 2_000));
    r.advance_attended(270_000);
    r.step().await;
    assert_eq!(
        (r.status("main").probes, r.status("main").unanswered),
        (2, 2)
    );
    r.advance_attended(60_000);
    compact(&r, "compaction", "a", 200, 0, Outcome::Succeeded);
    let status = r.status("main");
    assert_eq!(
        (status.reason, status.unanswered),
        (Reason::Compacted, 0),
        "{status:?}"
    );
    let view = main_recipe(&r);
    assert_eq!(
        (view.state, view.last_guard),
        (RecipeState::Ended, Some(Guard::Compaction)),
        "{view:?}"
    );
    assert_eq!(
        probes_and_credits(&final_rollups(&r).await),
        (
            2,
            BTreeMap::from([("cold_return".to_string(), 1), ("expired".to_string(), 1)]),
            0
        )
    );
}

/// While a compaction is open the recipe waits as it stood: a tick past its due time sends no probe and moves nothing.
/// A bus gap may have dropped the compaction's completion, so the gap ends it as a failed one, its completion lands
/// nowhere, and the schedule resumes on the recipe.
#[tokio::test]
async fn an_open_compaction_holds_the_recipe_unprobed_and_a_bus_gap_ends_it_as_a_failed_one() {
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    let armed = r.status("main");
    r.engine.observe(compaction(&r, "compaction"));
    r.engine.observe(r.selected("compaction", "a"));
    r.engine.observe(head(&r, "compaction", 200));
    // 280 s in, past the probe due 270 s in and inside the entry, which ends 300 s in.
    r.advance_attended(280_000);
    r.step().await;
    assert_eq!(
        r.status("main"),
        armed,
        "the open compaction held the recipe unprobed and unmoved"
    );
    r.engine.bus_gap(1, 0);
    r.engine.observe(completed(&r, "compaction", 2_000));
    assert_eq!(
        (r.status("main").reason, r.engine.stats().retained_bytes > 0),
        (Reason::Scheduled, true),
        "the completion of the compaction the gap ended landed nowhere"
    );
    r.step().await;
    assert_eq!(
        (r.status("main").probes, r.status("main").reason),
        (1, Reason::Scheduled),
        "the schedule resumed on the recipe"
    );
}

/// A turn admitted while a compaction is open moves the agent past the context the compaction was reading, so the
/// compaction ends as a failed one: its later events land nowhere, and the turn's recipe and the probe it keeps stand.
#[tokio::test]
async fn a_turn_admitted_while_a_compaction_is_open_ends_the_compaction_as_a_failed_one() {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    open_turn(&r, "first", 0, 2_000);
    r.engine.observe(completed(&r, "first", 0));
    r.advance_attended(270_000);
    r.step().await;
    r.advance_attended(10_000);
    r.engine.observe(compaction(&r, "compaction"));
    r.advance_attended(10_000);
    open_turn(&r, "next", 2_000, 0);
    r.engine.observe(r.selected("compaction", "a"));
    r.engine.observe(head(&r, "compaction", 200));
    r.engine.observe(message_start("compaction", 2_000, 0));
    r.engine.observe(completed(&r, "compaction", 2_000));
    let status = r.status("main");
    assert_eq!(
        (
            status.reason,
            status.unanswered,
            r.engine.stats().retained_bytes > 0
        ),
        (Reason::Scheduled, 1, true),
        "the turn's recipe and the probe it keeps stand: {status:?}"
    );
    r.engine.observe(completed(&r, "next", 2_000));
    let status = r.status("main");
    assert_eq!(
        (status.reason, status.unanswered),
        (Reason::Scheduled, 0),
        "the turn's completion answered the probe: {status:?}"
    );
}

/// Without a class, the same static prefix on a fresh message chain restarts the recipe, replacing its body.
#[tokio::test]
async fn the_same_static_prefix_on_a_fresh_chain_restarts_the_recipe() {
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    let before_restart = r.engine.stats().retained_bytes;
    let restarted = BODY.replace("hello", "a fresh conversation");
    assert_ne!(restarted, BODY);
    r.observe_with("main", &restarted).await;
    assert_eq!(
        r.status("main").reason,
        Reason::Scheduled,
        "the same system prompt on a fresh chain restarts rather than staying foreign"
    );
    assert_ne!(
        r.engine.stats().retained_bytes,
        before_restart,
        "the restart replaced the body"
    );
}

#[tokio::test]
async fn without_a_chain_every_request_re_arms() {
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    r.engine.observe(r.observed_with(
        "no-chain",
        "main",
        None,
        None,
        &[],
        BODY.as_bytes(),
        "claude-sonnet-4-5",
    ));
    assert_eq!(
        r.status("main").reason,
        Reason::AwaitingSelection,
        "a body without a chain re-arms on every request, the routing crate's own warmth fallback"
    );
}
