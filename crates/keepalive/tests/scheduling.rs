//! A due probe waits while its account is cooled, in overage, paused, or reauth-marked, while the pool's cap is
//! reached, or while the recipe's net is negative past its grace, and runs once the condition clears; the due
//! time and the idle limits come from the pool's tuning.
mod common;
use phoebis_keepalive::{Event, KeepaliveLimits, KeepalivePolicy, Reason};
use phoebis_ledger::{hour_of, read_rollups};
use phoebis_routing::{EvidenceSource, OverageEvidence, Report};
use phoebis_store::{AccountState, CooldownRow, OveragePolicy};
use common::*;

async fn armed(spec: RigSpec) -> Rig {
    let mut r = rig(spec).await;
    r.observe("main").await;
    r
}

#[tokio::test]
async fn a_cooled_account_holds_the_probe_until_the_cooldown_clears() {
    let mut r = armed(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    let until = r.now() + 600_000;
    r.h.router.report(Report::Cooldown {
        account_id: "a".into(),
        row: CooldownRow {
            account_id: "a".into(),
            scope: "account".into(),
            until,
            set_at: r.now(),
            reason: "rate_limit".into(),
            window: "five_hour".into(),
            source_request_id: None,
            incarnation: 1,
        },
        pin: None,
        lifecycle_generation: 1,
        credential_generation: 1,
        incarnation: 1,
    });
    settled(&r, |r| async move {
        r.h.router
            .cooldowns()
            .await
            .unwrap()
            .iter()
            .any(|c| c.account_id == "a")
    })
    .await;
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        (r.status("main").probes, r.status("main").reason),
        (0, Reason::Cooled)
    );
    assert_eq!(r.up.requests().len(), 1, "nothing went upstream");
    r.h.router.clear_cooldown("a", "account").await.unwrap();
    r.step().await;
    assert_eq!(
        (r.status("main").probes, r.status("main").reason),
        (1, Reason::Scheduled),
        "the probe ran once the account was clear"
    );
}

/// A report lands on the router task's own queue; the state it changes is visible once the handle says so.
///
/// The lifetime is named rather than elided: `check` is only ever called with this one borrow of `r`, and an
/// elided `Fn(&Rig) -> Fut` would desugar to a higher-ranked bound that no borrowing async closure can satisfy.
async fn settled<'a, F, Fut>(r: &'a Rig, check: F)
where
    F: Fn(&'a Rig) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while !check(r).await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the router never applied the report"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn overage_under_prefer_plan_holds_and_under_allow_does_not() {
    for (policy, held) in [
        (OveragePolicy::PreferPlan, true),
        (OveragePolicy::Never, true),
        (OveragePolicy::Allow, false),
    ] {
        let mut r = armed(RigSpec {
            accounts: vec![api_key("a", A, 0)],
            pool: PoolSpec {
                overage_policy: policy,
                ..PoolSpec::default()
            },
            ..RigSpec::default()
        })
        .await;
        r.h.router.report(Report::Overage {
            account_id: "a".into(),
            evidence: OverageEvidence {
                in_use: true,
                since: Some(r.now()),
                reset: None,
                observed_at: r.now(),
                source: EvidenceSource::Head,
                incarnation: 1,
            },
            incarnation: 1,
        });
        settled(&r, |r| async move {
            r.h.router
                .overage("a")
                .await
                .unwrap()
                .is_some_and(|e| e.in_use)
        })
        .await;
        r.advance(270_000);
        r.step().await;
        let status = r.status("main");
        assert_eq!(
            (policy, status.probes, status.reason),
            (
                policy,
                if held { 0 } else { 1 },
                if held {
                    Reason::Overage
                } else {
                    Reason::Scheduled
                }
            )
        );
        if held {
            r.h.router.report(Report::Overage {
                account_id: "a".into(),
                evidence: OverageEvidence {
                    in_use: false,
                    since: None,
                    reset: None,
                    observed_at: r.now() + 1,
                    source: EvidenceSource::Head,
                    incarnation: 1,
                },
                incarnation: 1,
            });
            settled(&r, |r| async move {
                r.h.router
                    .overage("a")
                    .await
                    .unwrap()
                    .is_none_or(|e| !e.in_use)
            })
            .await;
            r.step().await;
            assert_eq!(
                r.status("main").probes,
                1,
                "{policy:?}: ran once the overage ended"
            );
        }
    }
}

#[tokio::test]
async fn a_paused_or_reauth_marked_account_serves_no_probe() {
    let mut r = armed(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    r.h.store.set_enabled("a", false).unwrap();
    r.h.router.reload().await.unwrap();
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        (r.status("main").probes, r.status("main").reason),
        (0, Reason::Paused)
    );
    r.h.store.set_enabled("a", true).unwrap();
    r.h.store
        .set_state("a", AccountState::ReauthRequired)
        .unwrap();
    r.h.router.reload().await.unwrap();
    r.step().await;
    assert_eq!(
        (r.status("main").probes, r.status("main").reason),
        (0, Reason::Reauth)
    );
    r.h.store.set_state("a", AccountState::Ready).unwrap();
    r.h.router.reload().await.unwrap();
    r.step().await;
    assert_eq!(r.status("main").probes, 1);
    assert_eq!(r.engine.reasons().get(&Reason::Paused).copied(), Some(1));
}

#[tokio::test]
async fn affinity_off_or_short_stops_probing_until_raised() {
    let mut r = armed(RigSpec::default()).await;
    let mut pool = r.h.store.pool("main").unwrap().unwrap();
    pool.affinity_ttl_seconds = 120;
    r.h.store.update_pool(&pool, "test").unwrap();
    r.h.router.reload().await.unwrap();
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        (r.status("main").probes, r.status("main").reason),
        (0, Reason::AffinityTooShort)
    );
    pool.affinity_ttl_seconds = 0;
    r.h.store.update_pool(&pool, "test").unwrap();
    r.h.router.reload().await.unwrap();
    r.step().await;
    assert_eq!(r.status("main").probes, 0);
    pool.affinity_ttl_seconds = 3_600;
    r.h.store.update_pool(&pool, "test").unwrap();
    r.h.router.reload().await.unwrap();
    r.step().await;
    assert_eq!(r.status("main").probes, 1);
    // Turning the pool's toggle off holds too, and the recipe survives for the toggle coming back.
    pool.keepalive_json = serde_json::to_string(&KeepalivePolicy {
        enabled: false,
        ..RigSpec::default().policy
    })
    .unwrap();
    r.h.store.update_pool(&pool, "test").unwrap();
    r.h.router.reload().await.unwrap();
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        (r.status("main").probes, r.status("main").reason),
        (1, Reason::KeepaliveOff)
    );
}

#[tokio::test]
async fn turning_warm_subagents_off_drops_an_armed_subagent_recipe_at_the_gate() {
    let mut r = armed(RigSpec::default()).await;
    r.event(Event {
        event: "agent_start".into(),
        agent_id: Some("sub-1".into()),
        ..Default::default()
    })
    .unwrap();
    r.observe("sub-1").await;
    assert_eq!(r.status("sub-1").reason, Reason::Scheduled);
    let mut pool = r.h.store.pool("main").unwrap().unwrap();
    pool.keepalive_json = serde_json::to_string(&KeepalivePolicy {
        warm_subagents: false,
        ..RigSpec::default().policy
    })
    .unwrap();
    r.h.store.update_pool(&pool, "test").unwrap();
    r.h.router.reload().await.unwrap();
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        (r.status("sub-1").reason, r.status("sub-1").probes),
        (Reason::SubagentsOff, 0),
        "dropped at the gate, no paid probe"
    );
    assert_eq!(
        r.status("main").probes,
        1,
        "the main agent's recipe is untouched"
    );
}

#[tokio::test]
async fn opting_the_key_out_discards_an_armed_recipe_without_another_request() {
    let mut r = armed(RigSpec::default()).await;
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        r.status("main").probes,
        1,
        "one probe, so the recipe has an unanswered probe to settle"
    );
    let mut key = r.h.store.api_key("desktop").unwrap().unwrap();
    key.keepalive_opt_out = true;
    r.h.store.update_api_key(&key, "test").unwrap();
    r.h.router.reload().await.unwrap();
    let sent = r.up.requests().len();
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        r.up.requests().len(),
        sent,
        "no probe went upstream after the opt-out"
    );
    assert_eq!(
        (
            r.status("main").reason,
            r.status("main").next_probe_at,
            r.engine.stats().retained_bytes
        ),
        (Reason::KeyOptedOut, None, 0),
        "the recipe is gone"
    );
    assert!(r.engine.recipes_for("desktop", "session").is_empty());
    let rows = wait_for(std::time::Duration::from_secs(5), || {
        let rows =
            r.db.read_blocking(|c| phoebis_ledger::read_rollups(c, 0, i64::MAX))
                .unwrap();
        rows.iter()
            .any(|row| row.wasted_probes == 1)
            .then_some(rows)
    })
    .await;
    assert_eq!(
        rows.iter()
            .find(|row| row.wasted_probes == 1)
            .unwrap()
            .wasted_probe_reasons
            .get("released")
            .copied(),
        Some(1)
    );
}

#[tokio::test]
async fn a_pin_migrated_by_a_sibling_agent_moves_the_next_probe_to_the_new_account() {
    let mut r = armed(RigSpec {
        accounts: vec![api_key("a", A, 0), api_key("b", B, 0)],
        ..RigSpec::default()
    })
    .await;
    let first = r.status("main").account_id.clone().unwrap();
    let (mine, other) = if first == "a" { (A, B) } else { (B, A) };
    let other_id = if first == "a" { "b" } else { "a" };
    r.event(Event {
        event: "agent_start".into(),
        agent_id: Some("worker".into()),
        ..Default::default()
    })
    .unwrap();
    // The worker's real request meets a 429 on the pinned account and migrates the session's pin.
    r.up.push_credential_rule(
        mine,
        "/v1/messages",
        relay_common::json_429_unified(4_600, "five_hour"),
    );
    r.observe("worker").await;
    assert_eq!(r.status("worker").account_id.as_deref(), Some(other_id));
    assert_eq!(
        r.status("main").account_id.as_deref(),
        Some(first.as_str()),
        "the main recipe still names the old account until the tick reads the pin"
    );
    r.h.router.clear_cooldown(&first, "account").await.unwrap();
    r.advance(270_000);
    r.step().await;
    let probes: Vec<_> =
        r.up.requests()
            .into_iter()
            .filter(|q| q.body.windows(14).any(|w| w == b"\"max_tokens\":0"))
            .collect();
    assert_eq!(probes.len(), 2);
    assert!(
        probes.iter().all(|q| q.credential() == Some(other)),
        "both probes followed the pin to the new account"
    );
    assert_eq!(
        (
            r.status("main").account_id.as_deref(),
            r.status("main").migrations
        ),
        (Some(other_id), 0),
        "the account moved with the pin, which is not a probe migration"
    );
}

#[tokio::test]
async fn the_due_time_follows_the_pools_lead_and_jitter() {
    let mut r = armed(RigSpec {
        policy: KeepalivePolicy {
            lead_seconds_5m: 60,
            jitter_seconds_5m: 5,
            ..RigSpec::default().policy
        },
        ..RigSpec::default()
    })
    .await;
    let start = r.now();
    let due = r.status("main").next_probe_at.unwrap();
    assert!(
        (start + 235_000..=start + 240_000).contains(&due),
        "cache start plus 240 s less at most 5 s of jitter: {}",
        due - start
    );
    r.advance(234_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 0, "not yet due");
    r.advance(6_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 1);
    // A tuning edit reaches the next due time on the next tick.
    let mut pool = r.h.store.pool("main").unwrap().unwrap();
    pool.keepalive_json = serde_json::to_string(&KeepalivePolicy {
        lead_seconds_5m: 120,
        jitter_seconds_5m: 0,
        ..RigSpec::default().policy
    })
    .unwrap();
    r.h.store.update_pool(&pool, "test").unwrap();
    r.h.router.reload().await.unwrap();
    let probed_at = r.now();
    r.step().await;
    assert_eq!(r.status("main").next_probe_at, Some(probed_at + 180_000));
    // A one-hour entry takes the one-hour lead.
    let r = rig(RigSpec {
        policy: KeepalivePolicy {
            lead_seconds_1h: 360,
            jitter_seconds_1h: 0,
            ..RigSpec::default().policy
        },
        ..RigSpec::default()
    })
    .await;
    let body = BODY.replace(
        r#""cache_control":{"type":"ephemeral"}"#,
        r#""cache_control":{"type":"ephemeral","ttl":"1h"}"#,
    );
    r.engine.observe(r.observed("req", "main", &body));
    r.engine.observe(r.selected("req", "a"));
    r.engine.observe(r.complete(
        "req",
        phoebis_bus::Outcome::Succeeded,
        Some(phoebis_bus::Usage {
            cache_write_1h_tokens: Some(2_000),
            ..Default::default()
        }),
    ));
    assert_eq!(
        r.status("main").next_probe_at,
        Some(r.now() + 3_600_000 - 360_000)
    );
}

#[tokio::test]
async fn the_session_expiry_discards_recipes_and_the_scheduler_ticks() {
    let mut r = armed(RigSpec {
        session_expiry: std::time::Duration::from_secs(600),
        policy: KeepalivePolicy {
            max_idle_seconds: 0,
            ..RigSpec::default().policy
        },
        ..RigSpec::default()
    })
    .await;
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 1);
    r.advance(330_000);
    r.step().await;
    assert_eq!(
        r.engine
            .snapshot("desktop", r.generation("desktop"))
            .sessions
            .len(),
        0,
        "600 s past the last real request the session is gone"
    );
    assert_eq!(
        (
            r.engine.stats().retained_bytes,
            r.engine.reasons().get(&Reason::SessionExpired).copied()
        ),
        (0, Some(1))
    );
    assert_eq!(r.engine.stats().sessions, 0);
    let r = rig(RigSpec::default()).await;
    let scheduler = r.engine.spawn_scheduler();
    r.engine.shutdown().await;
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(2), scheduler)
            .await
            .is_ok(),
        "the ticker stops on shutdown"
    );
}

/// The skipped-probe total the ledger writer has landed, once it has reached `want`.
async fn skipped(r: &Rig, want: u64) -> u64 {
    wait_for(std::time::Duration::from_secs(5), || {
        let rows =
            r.db.read_blocking(|c| read_rollups(c, hour_of(NOW) - 3_600, i64::MAX))
                .unwrap();
        let total: u64 = rows.iter().map(|row| row.probes_skipped).sum();
        (total >= want).then_some(total)
    })
    .await
}

/// A hold is a transition, not a state the ticker re-counts: a five-hour cooldown withholds one probe, not
/// one per second of it, and the reason behind it is counted once per hold rather than once per tick.
#[tokio::test]
async fn a_hold_counts_one_skipped_probe_however_many_ticks_it_spans() {
    let mut r = armed(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    let until = r.now() + 600_000;
    r.h.router.report(Report::Cooldown {
        account_id: "a".into(),
        row: CooldownRow {
            account_id: "a".into(),
            scope: "account".into(),
            until,
            set_at: r.now(),
            reason: "rate_limit".into(),
            window: "five_hour".into(),
            source_request_id: None,
            incarnation: 1,
        },
        pin: None,
        lifecycle_generation: 1,
        credential_generation: 1,
        incarnation: 1,
    });
    settled(&r, |r| async move {
        r.h.router
            .cooldowns()
            .await
            .unwrap()
            .iter()
            .any(|c| c.account_id == "a")
    })
    .await;
    r.advance(270_000);
    for _ in 0..10 {
        r.step().await;
    }
    assert_eq!(
        (r.status("main").probes, r.status("main").reason),
        (0, Reason::Cooled)
    );
    assert_eq!(
        r.engine.reasons().get(&Reason::Cooled).copied(),
        Some(1),
        "ten ticks of one hold"
    );
    assert_eq!(skipped(&r, 1).await, 1, "one withheld probe, not ten");
    // A hold whose reason changes is a new hold: the account comes out of the cooldown disabled.
    r.h.router.clear_cooldown("a", "account").await.unwrap();
    r.h.store.set_enabled("a", false).unwrap();
    r.h.router.reload().await.unwrap();
    for _ in 0..5 {
        r.step().await;
    }
    assert_eq!(
        (r.status("main").probes, r.status("main").reason),
        (0, Reason::Paused)
    );
    assert_eq!(
        (
            r.engine.reasons().get(&Reason::Cooled).copied(),
            r.engine.reasons().get(&Reason::Paused).copied()
        ),
        (Some(1), Some(1)),
        "once per reason"
    );
    assert_eq!(skipped(&r, 2).await, 2, "one withheld probe per hold");
}

/// A pause clears the schedule, so a recipe paused by a 429 is not armed; the retention rules still have to
/// reach it, or an opt-out could never discard it and the idle limit could never settle it while the
/// companion keeps reporting.
#[tokio::test]
async fn the_retention_rules_reach_a_paused_recipe() {
    let mut r = armed(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    r.up.push_credential_rule(
        A,
        "/v1/messages",
        relay_common::json_429_unified(4_600, "five_hour"),
    );
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        (r.status("main").reason, r.status("main").next_probe_at),
        (Reason::ProbeQuotaExhausted, None),
        "the 429 paused the recipe and cleared its schedule"
    );
    assert!(!r.engine.recipes_for("desktop", "session").is_empty());
    let mut key = r.h.store.api_key("desktop").unwrap().unwrap();
    key.keepalive_opt_out = true;
    r.h.store.update_api_key(&key, "test").unwrap();
    r.h.router.reload().await.unwrap();
    r.step().await;
    assert_eq!(
        (r.status("main").reason, r.engine.stats().retained_bytes),
        (Reason::KeyOptedOut, 0),
        "the opt-out discarded the paused recipe"
    );
    // The idle limit settles one too, once the companion has gone silent for longer than the limit.
    let mut r = armed(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        limits: KeepaliveLimits {
            lease_seconds: 10,
            ..KeepaliveLimits::default()
        },
        ..RigSpec::default()
    })
    .await;
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 1, "one probe to settle as wasted");
    r.up.push_credential_rule(
        A,
        "/v1/messages",
        relay_common::json_429_unified(4_600, "five_hour"),
    );
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").next_probe_at, None, "paused by the 429");
    r.advance(3_600_000);
    r.step().await;
    assert_eq!(
        (r.status("main").reason, r.engine.stats().retained_bytes),
        (Reason::IdleLimit, 0)
    );
    let rows = wait_for(std::time::Duration::from_secs(5), || {
        let rows =
            r.db.read_blocking(|c| read_rollups(c, hour_of(NOW) - 3_600, i64::MAX))
                .unwrap();
        rows.iter()
            .any(|row| row.wasted_probes == 1)
            .then_some(rows)
    })
    .await;
    assert_eq!(
        rows.iter()
            .find(|row| row.wasted_probes == 1)
            .unwrap()
            .wasted_probe_reasons
            .get("idle_limit")
            .copied(),
        Some(1)
    );
}

/// A five-hour cooldown on the rig's one account, applied and settled.
async fn cool_account(r: &Rig) {
    r.h.router.report(Report::Cooldown {
        account_id: "a".into(),
        row: CooldownRow {
            account_id: "a".into(),
            scope: "account".into(),
            until: r.now() + 600_000,
            set_at: r.now(),
            reason: "rate_limit".into(),
            window: "five_hour".into(),
            source_request_id: None,
            incarnation: 1,
        },
        pin: None,
        lifecycle_generation: 1,
        credential_generation: 1,
        incarnation: 1,
    });
    settled(r, |r| async move {
        r.h.router
            .cooldowns()
            .await
            .unwrap()
            .iter()
            .any(|c| c.account_id == "a")
    })
    .await;
}

/// A hold that begins before its probe is due withholds nothing yet: the skip is counted once the due time
/// passes, whichever tick the hold started on.
#[tokio::test]
async fn a_hold_counts_its_skip_when_the_held_probe_comes_due() {
    let mut r = armed(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    // The account is cooled ten seconds in, with the probe due at 270 s.
    r.advance(10_000);
    cool_account(&r).await;
    for _ in 0..3 {
        r.step().await;
    }
    assert_eq!(
        (r.status("main").probes, r.status("main").reason),
        (0, Reason::Cooled)
    );
    // Drained rather than polled: what this half asserts is that no skipped row lands at all.
    r.ledger.clone().shutdown().await.unwrap();
    let rows =
        r.db.read_blocking(|c| read_rollups(c, hour_of(NOW) - 3_600, i64::MAX))
            .unwrap();
    assert_eq!(
        rows.iter().map(|row| row.probes_skipped).sum::<u64>(),
        0,
        "no probe was due while the hold began"
    );
    // The same hold once the due time has passed: one withheld probe, however many ticks follow it.
    let mut r = armed(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    r.advance(10_000);
    cool_account(&r).await;
    for _ in 0..3 {
        r.step().await;
    }
    r.advance(270_000);
    for _ in 0..5 {
        r.step().await;
    }
    assert_eq!(
        (r.status("main").probes, r.status("main").reason),
        (0, Reason::Cooled)
    );
    assert_eq!(
        skipped(&r, 1).await,
        1,
        "one withheld probe, counted when it came due"
    );
}
