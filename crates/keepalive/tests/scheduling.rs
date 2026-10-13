//! A due probe waits while its account is cooled, in overage, paused, or reauth-marked, and runs once the condition
//! clears. The due time follows the entry's expiry, and the idle limits follow the pool's policy.
mod common;
use common::*;
use phoebis_keepalive::{Event, KeepalivePolicy, KeepaliveTiming, Reason, LEASE_MS};
use phoebis_ledger::{
    hour_of, member_spends, read_rollups, BudgetedMember, PriceRevisions, RollupSelect,
};
use phoebis_relay::Accounting;
use phoebis_routing::{
    lane_key, resolve, ClientClass, EvidenceSource, OverageEvidence, PinRef, Report, SelectRequest,
    SelectResult,
};
use phoebis_store::{
    AccountState, BudgetPeriod, CooldownRow, MemberWrite, OveragePolicy, StateCause,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

async fn armed(spec: RigSpec) -> Rig {
    let mut r = rig(spec).await;
    r.observe("main").await;
    r
}

#[tokio::test]
async fn an_idle_tick_asks_the_router_nothing() {
    let mut r = rig(RigSpec::default()).await;
    let before = r.engine.router_views();
    r.step().await;
    assert_eq!(
        r.engine.router_views(),
        before,
        "a registered session with no recipe asks the router nothing"
    );
    r.observe("main").await;
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        r.engine.router_views(),
        before + 1,
        "a due recipe asks the router once"
    );
}

#[tokio::test]
async fn a_cooled_account_holds_the_probe_until_the_cooldown_clears() {
    let mut r = armed(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    let until = r.now() + 600_000;
    r.h.router
        .report(Report::Cooldown {
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
        })
        .await;
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

/// An entry the engine knows has expired is not probed: a probe past the expiry would warm nothing. The probe the
/// recipe already sent, which no request answered, settles wasted under the lapse.
#[tokio::test]
async fn a_recipe_whose_entry_lapsed_is_held_under_expired_and_sends_no_probe() {
    let mut r = armed(RigSpec::default()).await;
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 1, "the one probe before the lapse");
    let sent = r.up.requests().len();
    r.advance(20 * MINUTE);
    r.step().await;
    let status = r.status("main");
    assert_eq!(
        (status.probes, status.reason, status.next_probe_at),
        (1, Reason::Expired, None),
        "{status:?}"
    );
    assert_eq!(r.up.requests().len(), sent, "no request after the lapse");
    let wasted = wait_for(std::time::Duration::from_secs(5), || {
        let rows =
            r.db.read_blocking(|c| {
                read_rollups(c, hour_of(NOW) - 3_600, i64::MAX, &RollupSelect::default())
            })
            .unwrap();
        let wasted: u64 = rows
            .iter()
            .filter_map(|row| row.wasted_probe_reasons.get("expired"))
            .sum();
        (wasted > 0).then_some(wasted)
    })
    .await;
    assert_eq!(wasted, 1, "the unanswered probe carries the lapse's label");
    // Drained rather than polled: a second skip row landing late must not read as one.
    r.ledger.clone().shutdown().await.unwrap();
    let rows =
        r.db.read_blocking(|c| {
            read_rollups(c, hour_of(NOW) - 3_600, i64::MAX, &RollupSelect::default())
        })
        .unwrap();
    assert_eq!(
        rows.iter().map(|row| row.probes_skipped).sum::<u64>(),
        1,
        "the lapse counts the probe it withheld, with no hold behind it"
    );
    // A fast recipe's skip files under the account, the pool and the speed it was retained with, read before the
    // lapse drops the recipe, never under an empty id.
    let mut r = rig(RigSpec::default()).await;
    let fast = BODY.replacen('{', r#"{"speed":"fast","#, 1);
    r.engine.observe(r.observed("fast", "main", &fast));
    r.engine.observe(r.selected("fast", "a"));
    r.engine.observe(r.complete(
        "fast",
        phoebis_bus::Outcome::Succeeded,
        Some(phoebis_bus::Usage {
            cache_write_5m_tokens: Some(2_000),
            ..Default::default()
        }),
    ));
    r.advance(20 * MINUTE);
    r.step().await;
    assert_eq!(r.status("main").reason, Reason::Expired);
    r.ledger.clone().shutdown().await.unwrap();
    let rows =
        r.db.read_blocking(|c| {
            read_rollups(c, hour_of(NOW) - 3_600, i64::MAX, &RollupSelect::default())
        })
        .unwrap();
    let skipped: Vec<(&str, &str, &str, u64)> = rows
        .iter()
        .filter(|row| row.probes_skipped > 0)
        .map(|row| {
            (
                row.account_id.as_str(),
                row.pool_id.as_str(),
                row.speed.as_str(),
                row.probes_skipped,
            )
        })
        .collect();
    assert_eq!(skipped, [("a", "main", "fast", 1)]);
}

/// A hold that outlasts the entry never ends in a probe: once the entry has expired the recipe is held under
/// `expired` before the gate is read, and the skip the hold already counted for its withheld probe stays one.
#[tokio::test]
async fn a_hold_lifted_after_the_expiry_sends_no_probe() {
    let mut r = armed(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    cool_account(&r).await;
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        (r.status("main").probes, r.status("main").reason),
        (0, Reason::Cooled)
    );
    // The entry written at the start expired at 300 s; the cooldown lifts after it.
    r.advance(60_000);
    r.h.router.clear_cooldown("a", "account").await.unwrap();
    r.step().await;
    assert_eq!(
        (r.status("main").probes, r.status("main").reason),
        (0, Reason::Expired)
    );
    assert_eq!(r.up.requests().len(), 1, "only the arming request went out");
    // Drained rather than polled: a second skip row landing late must not read as one.
    r.ledger.clone().shutdown().await.unwrap();
    let rows =
        r.db.read_blocking(|c| {
            read_rollups(c, hour_of(NOW) - 3_600, i64::MAX, &RollupSelect::default())
        })
        .unwrap();
    assert_eq!(
        rows.iter().map(|row| row.probes_skipped).sum::<u64>(),
        1,
        "one withheld probe, counted by the hold"
    );
}

/// A failed probe's retry that falls past the entry's expiry is not sent: the due time is the later of the two, so
/// the first tick at or after the expiry holds the recipe under `expired`.
#[tokio::test]
async fn a_retry_re_armed_past_the_expiry_is_held_under_expired_and_sends_no_probe() {
    let mut r = armed(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        timing: KeepaliveTiming {
            retry_ms: 60_000,
            retry_max_ms: 60_000,
            ..RigSpec::default().timing
        },
        ..RigSpec::default()
    })
    .await;
    r.up.push_credential_rule(
        A,
        "/v1/messages",
        fake_upstream::Respond::Json {
            status: 503,
            headers: vec![],
            body: br#"{"type":"error","error":{"type":"api_error","message":"x"}}"#.to_vec(),
        },
    );
    r.advance(270_000);
    r.step().await;
    let status = r.status("main");
    assert_eq!(
        (status.probes, status.reason, status.next_probe_at),
        (1, Reason::RetryableProbeError, Some(r.now() + 60_000)),
        "the retry falls at 330 s, past the entry's expiry at 300 s"
    );
    r.advance(60_000);
    r.step().await;
    assert_eq!(
        (r.status("main").probes, r.status("main").reason),
        (1, Reason::Expired)
    );
    assert_eq!(
        r.up.requests().len(),
        2,
        "the arming request and the failed probe, no retry"
    );
}

/// A probe the router refused while its ledger was behind holds under `ledger_behind` and goes again after the
/// answer's wait, with no failure counted. The entry's own expiry still bounds the re-sends: when the ledger catches
/// up only after it, no probe goes out and the recipe ends under `expired`, as under any other hold.
#[tokio::test]
async fn a_ledger_behind_hold_lifted_after_the_expiry_sends_no_probe() {
    let behind = Arc::new(AtomicBool::new(false));
    let reading = behind.clone();
    let mut r = armed(RigSpec {
        accounting: Some(Arc::new(move |_| Accounting {
            behind: reading.load(Ordering::SeqCst),
        })),
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    behind.store(true, Ordering::SeqCst);
    r.advance(270_000);
    r.step().await;
    let status = r.status("main");
    assert_eq!(
        (
            status.probes,
            status.reason,
            status.failures,
            status.next_probe_at
        ),
        (1, Reason::LedgerBehind, 0, Some(r.now() + 5_000)),
        "held for the answer's five seconds"
    );
    r.advance(5_000);
    r.step().await;
    let status = r.status("main");
    assert_eq!(
        (
            status.probes,
            status.reason,
            status.failures,
            status.next_probe_at
        ),
        (2, Reason::LedgerBehind, 0, Some(r.now() + 5_000)),
        "sent again after the wait, and refused again"
    );
    // The entry written at the start expired at 300 s; the ledger catches up after it.
    r.advance(30_000);
    behind.store(false, Ordering::SeqCst);
    r.step().await;
    let status = r.status("main");
    assert_eq!(
        (status.probes, status.reason, status.next_probe_at),
        (2, Reason::Expired, None)
    );
    assert_eq!(
        r.up.requests().len(),
        1,
        "only the arming request went out: both refusals stopped at the router"
    );
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
        r.h.router
            .report(Report::Overage {
                account_id: "a".into(),
                evidence: OverageEvidence {
                    in_use: true,
                    since: Some(r.now()),
                    // Ahead of the `now` the gate reads after the advance below: this test is about the policy,
                    // not the predicate's own reset or fallback handling.
                    reset: Some(r.now() + 1_000_000),
                    observed_at: r.now(),
                    source: EvidenceSource::Head,
                    incarnation: 1,
                },
                incarnation: 1,
            })
            .await;
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
            r.h.router
                .report(Report::Overage {
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
                })
                .await;
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

/// A selection for the session `main`'s pin belongs to, through the same router handle the scheduler reads.
async fn select_main(r: &Rig) -> SelectResult {
    let pins = r.h.router.pins().await.unwrap();
    let pin = pins
        .iter()
        .find(|p| p.session_id == "session")
        .expect("observe pinned a session");
    let model = pin.model.clone();
    let snapshot = r.h.router.snapshot();
    let key = snapshot.key("desktop").unwrap();
    let candidates = resolve(&snapshot, key, &model, &model).unwrap();
    let lane = lane_key(&model, "standard", "adaptive", "-");
    r.h.router
        .select(SelectRequest {
            request_id: "r-1".into(),
            attempt: 1,
            seq: 1,
            key_id: "desktop".into(),
            key_generation: key.created_at,
            agent_id: "main".into(),
            session_id: Some("session".into()),
            canonical_model: model.clone(),
            folded_model: model,
            candidates,
            portable: true,
            attempted: Vec::new(),
            now: r.now(),
            fallback_ms: 60_000,
            revision: snapshot.revision,
            probe: false,
            count: false,
            request_class: None,
            context_compacted: false,
            static_hash: None,
            boundaries: Arc::from([]),
            client: ClientClass::ClaudeCode,
            static_boundaries: Arc::from([]),
            marker: false,
            parent_agent_id: None,
            holds: Vec::new(),
            dialed: false,
            held_out: false,
            lane,
            fast: false,
            body_tokens: 0,
        })
        .await
        .unwrap()
        .result
}

#[tokio::test]
async fn after_the_named_reset_passes_the_probe_is_no_longer_held_for_overage_and_selection_agrees()
{
    let mut r = armed(RigSpec {
        accounts: vec![api_key("a", A, 0), api_key("b", B, 1)],
        ..RigSpec::default()
    })
    .await;
    r.h.router
        .report(Report::Overage {
            account_id: "a".into(),
            evidence: OverageEvidence {
                in_use: true,
                since: Some(r.now()),
                reset: Some(r.now() - 1),
                observed_at: r.now(),
                source: EvidenceSource::Head,
                incarnation: 1,
            },
            incarnation: 1,
        })
        .await;
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
    assert_eq!(
        (r.status("main").probes, r.status("main").reason),
        (1, Reason::Scheduled),
        "the named reset has passed, so the probe is no longer held for overage"
    );
    let SelectResult::Selected(sel) = select_main(&r).await else {
        panic!("a serves the model")
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.migrated, sel.in_overage),
        ("a", None, false),
        "and a selection through the same handle reads the evidence as passed too"
    );

    let mut r = armed(RigSpec {
        accounts: vec![api_key("a", A, 0), api_key("b", B, 1)],
        ..RigSpec::default()
    })
    .await;
    r.h.router
        .report(Report::Overage {
            account_id: "a".into(),
            evidence: OverageEvidence {
                in_use: true,
                since: Some(r.now()),
                // Ahead of the `now` the gate reads after the advance below, not just of the report's own `now`.
                reset: Some(r.now() + 270_001),
                observed_at: r.now(),
                source: EvidenceSource::Head,
                incarnation: 1,
            },
            incarnation: 1,
        })
        .await;
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
    assert_eq!(
        (r.status("main").probes, r.status("main").reason),
        (0, Reason::Overage),
        "the named reset still stands, so the probe holds"
    );
    let SelectResult::Selected(sel) = select_main(&r).await else {
        panic!("a serves the model")
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.migrated, sel.in_overage),
        ("a", None, true),
        "and a selection through the same handle reads the evidence as standing too; only a subscription \
         account's overage moves a pin, so the api_key pin renews"
    );
}

/// Lands a second real request straight through the store, without the channel `plant_spend` uses, then reads
/// `a`'s current-period spend the way the ledger's own background refresh would (the same query, the same rows)
/// and relays it to the router at once: neither step waits, so a caller racing this against the scheduler
/// controls exactly when the router sees it, with a real sum a later background round can only confirm, never
/// contradict.
async fn plant_and_report_spend(r: &Rig, id: &str, usd: f64) {
    let (request, attempt) = spend_of(id, r.now(), usd);
    r.db.insert_request(&request, &[attempt], None).unwrap();
    let rows =
        r.db.read_blocking(|conn| {
            member_spends(
                conn,
                &[BudgetedMember {
                    pool_id: "main".into(),
                    account_id: "a".into(),
                    period: BudgetPeriod::Month,
                    metered_only: false,
                }],
                r.now(),
                &PriceRevisions::shipped(),
            )
        })
        .unwrap();
    r.h.router.report(Report::MemberSpend(rows)).await;
}

/// `a` at a budget of 5.0 beside `b`, no budget: the shorthand both budget-hold tests below seed the pool with.
async fn with_budget_member(r: &Rig) {
    r.h.store
        .set_pool_members(
            "main",
            &[
                MemberWrite {
                    account_id: "a".into(),
                    priority: 0,
                    budget_daily_usd: None,
                    budget_monthly_usd: Some(5.0),
                },
                MemberWrite {
                    account_id: "b".into(),
                    priority: 1,
                    budget_daily_usd: None,
                    budget_monthly_usd: None,
                },
            ],
            "test",
        )
        .unwrap();
    r.h.router.reload().await.unwrap();
}

/// Raises `a`'s budget past any spend this suite plants and reloads the router, so the refusal clears through the
/// budget while `a`'s reported spend stays where it was.
async fn raise_budget(r: &Rig) {
    r.h.store
        .set_pool_members(
            "main",
            &[
                MemberWrite {
                    account_id: "a".into(),
                    priority: 0,
                    budget_daily_usd: None,
                    budget_monthly_usd: Some(1_000.0),
                },
                MemberWrite {
                    account_id: "b".into(),
                    priority: 1,
                    budget_daily_usd: None,
                    budget_monthly_usd: None,
                },
            ],
            "test",
        )
        .unwrap();
    r.h.router.reload().await.unwrap();
}

/// Whether the router holds a pin for session `session` on account `a`.
async fn named_a(r: &Rig) -> bool {
    r.h.router
        .pins()
        .await
        .unwrap()
        .iter()
        .any(|p| p.session_id == "session" && p.account_id == "a")
}

#[tokio::test]
async fn a_probe_pinned_to_a_member_at_its_budget_holds_until_the_budget_no_longer_refuses() {
    let mut r = armed(RigSpec::default()).await;
    with_budget_member(&r).await;
    plant_spend(&r, "over-budget", 5.01).await;
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        (r.status("main").probes, r.status("main").reason),
        (0, Reason::Budget)
    );
    assert_eq!(
        r.up.requests().len(),
        1,
        "nothing more went upstream than the arming request"
    );
    // The ledger's own spend for `a` still reads over 5.0 here; nothing lowers it.
    raise_budget(&r).await;
    r.step().await;
    assert_eq!(
        (r.status("main").probes, r.status("main").reason),
        (1, Reason::Scheduled),
        "the probe ran once the raised budget no longer refused"
    );
}

#[tokio::test]
async fn a_budget_crossed_between_the_gate_and_the_selection_holds_the_probe_and_then_releases_it()
{
    let mut r = rig(RigSpec::default()).await;
    with_budget_member(&r).await;
    // Arms the recipe on the engine's own side, with no request against the fake upstream: the same synthetic
    // path `the_due_time_follows_the_pools_lead_and_jitter` uses for a session the test builds by hand.
    r.engine.observe(r.observed("req", "main", BODY));
    r.engine.observe(r.selected("req", "a"));
    r.engine.observe(r.complete(
        "req",
        phoebis_bus::Outcome::Succeeded,
        Some(phoebis_bus::Usage {
            cache_write_5m_tokens: Some(2_000),
            ..Default::default()
        }),
    ));
    // Pins the session on `a` in the router's own state, in process and upstream-free, so the probe-only budget
    // rule (which only ever answers a request that already has a pin) has one to find.
    let snapshot = r.h.router.snapshot();
    let key = snapshot.key("desktop").unwrap();
    let candidates = resolve(
        &snapshot,
        key,
        "claude-sonnet-4-5",
        "claude-sonnet-4-5",
    )
    .unwrap();
    r.h.router
        .select(SelectRequest {
            request_id: "r-1".into(),
            attempt: 1,
            seq: 1,
            key_id: "desktop".into(),
            key_generation: key.created_at,
            agent_id: "main".into(),
            session_id: Some("session".into()),
            canonical_model: "claude-sonnet-4-5".into(),
            folded_model: "claude-sonnet-4-5".into(),
            candidates,
            portable: true,
            attempted: Vec::new(),
            now: r.now(),
            fallback_ms: 60_000,
            revision: snapshot.revision,
            probe: false,
            count: false,
            request_class: None,
            context_compacted: false,
            static_hash: None,
            boundaries: Arc::from([]),
            client: ClientClass::ClaudeCode,
            static_boundaries: Arc::from([]),
            marker: false,
            parent_agent_id: None,
            holds: Vec::new(),
            dialed: false,
            held_out: false,
            lane: lane_key("claude-sonnet-4-5", "standard", "none", "-"),
            fast: false,
            body_tokens: 0,
        })
        .await
        .unwrap();
    plant_spend(&r, "under-budget", 2.0).await;
    r.advance(270_000);
    // Drives the tick itself rather than `r.step()`: the gate passes on the view read here, and the job it
    // spawns has not run its own `select` when this returns, on the current-thread runtime the test builds.
    r.engine.step().await;
    // A second real request pushes the ledger's own sum for `a` past the budget; relaying it here, with
    // nothing awaited in between, queues the report on the router's one command channel ahead of the spawned
    // job's own `select`, so the selection that follows reads the crossed spend. A later background refresh
    // would compute the same real figure, so nothing after this can disagree with it.
    plant_and_report_spend(&r, "crossing", 3.5).await;
    r.engine.wait_idle().await;
    r.pump(None).await;
    assert!(
        r.up.requests().is_empty(),
        "the selection read the crossed spend, so the probe never reached the upstream"
    );
    let status = r.status("main");
    assert_eq!(
        (status.probes, status.in_flight, status.reason),
        (1, false, Reason::Budget)
    );
    assert!(named_a(&r).await, "the pin still names a");
    r.step().await;
    assert_eq!(
        (r.status("main").probes, r.status("main").reason),
        (1, Reason::Budget),
        "the gate holds on the view again with nothing changed: no further probe was dispatched"
    );
    assert!(r.up.requests().is_empty());
    raise_budget(&r).await;
    r.step().await;
    assert_eq!(r.status("main").reason, Reason::Scheduled);
    let sent = r.up.requests();
    assert_eq!(
        sent.len(),
        1,
        "the sole upstream request is the probe: this test sends no real one"
    );
    assert!(
        sent[0].body.windows(14).any(|w| w == b"\"max_tokens\":0"),
        "the one upstream request carries the probe's own marker, a max_tokens of zero"
    );
    assert!(named_a(&r).await, "the pin still names a");
}

/// While the ledger is degraded the selector cannot trust a budgeted member's spend and refuses a probe pinned there
/// as `budget_unknown`. That clears once the ledger recovers, so the agent retries rather than pausing as exhausted.
#[tokio::test]
async fn a_probe_refused_for_an_unknown_budget_retries_once_the_ledger_recovers() {
    let mut r = armed(RigSpec {
        timing: KeepaliveTiming {
            retry_ms: 10_000,
            ..RigSpec::default().timing
        },
        ..RigSpec::default()
    })
    .await;
    with_budget_member(&r).await;
    // Waits out the writer's own spend refresh, whose standing report would otherwise land after the one below.
    plant_spend(&r, "under-budget", 1.0).await;
    r.h.router
        .report(Report::Accounting { degraded: true })
        .await;
    settled(&r, |r| async move {
        r.h.router.accounting_degraded().await.unwrap()
    })
    .await;
    r.advance(270_000);
    r.step().await;
    let status = r.status("main");
    assert_eq!(
        (status.probes, status.reason, status.next_probe_at.is_some()),
        (1, Reason::RetryableProbeError, true),
        "{status:?}"
    );
    assert_eq!(
        r.up.requests().len(),
        1,
        "the refused probe reached no upstream"
    );
    r.h.router
        .report(Report::Accounting { degraded: false })
        .await;
    settled(&r, |r| async move {
        !r.h.router.accounting_degraded().await.unwrap()
    })
    .await;
    r.advance(10_000);
    r.step().await;
    assert_eq!(
        (r.status("main").probes, r.status("main").reason),
        (2, Reason::Scheduled),
        "the retry ran once the ledger recovered"
    );
    assert!(named_a(&r).await, "the pin still names a");
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
        .set_state("a", AccountState::ReauthRequired, StateCause::Operator)
        .unwrap();
    r.h.router.reload().await.unwrap();
    r.step().await;
    assert_eq!(
        (r.status("main").probes, r.status("main").reason),
        (0, Reason::Reauth)
    );
    r.h.store
        .set_state("a", AccountState::Ready, StateCause::Operator)
        .unwrap();
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
    r.h.store
        .update_pool(&pool, pool.updated_at, "test")
        .unwrap();
    r.h.router.reload().await.unwrap();
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        (r.status("main").probes, r.status("main").reason),
        (0, Reason::AffinityTooShort)
    );
    pool.affinity_ttl_seconds = 0;
    r.h.store
        .update_pool(
            &pool,
            r.h.store.pool("main").unwrap().unwrap().updated_at,
            "test",
        )
        .unwrap();
    r.h.router.reload().await.unwrap();
    r.step().await;
    assert_eq!(r.status("main").probes, 0);
    pool.affinity_ttl_seconds = 3_600;
    r.h.store
        .update_pool(
            &pool,
            r.h.store.pool("main").unwrap().unwrap().updated_at,
            "test",
        )
        .unwrap();
    r.h.router.reload().await.unwrap();
    r.step().await;
    assert_eq!(r.status("main").probes, 1);
    // Turning the pool's toggle off holds too, and the recipe survives for the toggle coming back.
    pool.keepalive_json = serde_json::to_string(&KeepalivePolicy {
        enabled: false,
        ..RigSpec::default().policy
    })
    .unwrap();
    r.h.store
        .update_pool(
            &pool,
            r.h.store.pool("main").unwrap().unwrap().updated_at,
            "test",
        )
        .unwrap();
    r.h.router.reload().await.unwrap();
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        (r.status("main").probes, r.status("main").reason),
        (1, Reason::KeepaliveOff)
    );
}

/// The pin a probe follows has to outlive the probe's due time, the recipe's own TTL less the lead: half an hour of
/// pin carries a five-minute recipe's probe but not a one-hour recipe's, due 54 minutes after its request.
#[tokio::test]
async fn a_one_hour_recipe_holds_under_affinity_too_short_on_a_thirty_minute_pin_while_a_five_minute_one_probes(
) {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    let mut pool = r.h.store.pool("main").unwrap().unwrap();
    pool.affinity_ttl_seconds = 1_800;
    r.h.store
        .update_pool(&pool, pool.updated_at, "test")
        .unwrap();
    r.h.router.reload().await.unwrap();
    r.event(Event {
        event: "agent_start".into(),
        agent_id: Some("sub".into()),
        ..Default::default()
    })
    .unwrap();
    let hour = BODY.replace(
        r#""cache_control":{"type":"ephemeral"}"#,
        r#""cache_control":{"type":"ephemeral","ttl":"1h"}"#,
    );
    r.up.push_credential_rule(
        A,
        "/v1/messages",
        fake_upstream::Respond::Json {
            status: 200,
            headers: vec![],
            body: br#"{"id":"m","type":"message","content":[],"usage":{"input_tokens":10,"cache_creation_input_tokens":2000,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":2000},"output_tokens":10}}"#.to_vec(),
        },
    );
    r.observe_with("main", &hour).await;
    assert_eq!(r.status("main").cache_ttl_ms, 3_600_000);
    r.observe("sub").await;
    assert_eq!(r.status("sub").cache_ttl_ms, 300_000);
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        (r.status("sub").probes, r.status("sub").reason),
        (1, Reason::Scheduled),
        "the five-minute probe is due 270 s in, inside the pin"
    );
    assert_eq!(
        (r.status("main").probes, r.status("main").reason),
        (0, Reason::AffinityTooShort),
        "the hour recipe's probe is due 3,240 s in, past the pin"
    );
    // A pin as long as the hour recipe's due time carries its probe.
    pool.affinity_ttl_seconds = 3_240;
    r.h.store
        .update_pool(
            &pool,
            r.h.store.pool("main").unwrap().unwrap().updated_at,
            "test",
        )
        .unwrap();
    r.h.router.reload().await.unwrap();
    r.advance(3_240_000 - 270_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 1);
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
    r.h.store
        .update_api_key(&key, key.updated_at, "test")
        .unwrap();
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
            r.db.read_blocking(|c| {
                phoebis_ledger::read_rollups(c, 0, i64::MAX, &RollupSelect::default())
            })
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
async fn two_agents_of_one_session_pinned_to_two_accounts_each_probe_their_own_account() {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0), api_key("b", B, 0)],
        pool: PoolSpec {
            strategy: phoebis_store::Strategy::RoundRobin,
            ..PoolSpec::default()
        },
        ..RigSpec::default()
    })
    .await;
    // This test is about the per-agent gate, so it pins the placement it depends on: both agents observe the same
    // fixture body, and cohort placement would land them on one account through the shared static boundary.
    let mut pool = r.h.store.pool("main").unwrap().unwrap();
    pool.affinity.subagent_placement = phoebis_store::SubagentPlacement::ByStrategy;
    r.h.store
        .update_pool(&pool, pool.updated_at, "test")
        .unwrap();
    r.h.router.reload().await.unwrap();
    r.observe("main").await;
    r.event(Event {
        event: "agent_start".into(),
        agent_id: Some("worker".into()),
        ..Default::default()
    })
    .unwrap();
    // Round robin lands the session's second fresh placement on the pool's other account, so main and its
    // subagent hold two pins on two accounts rather than sharing one.
    r.observe("worker").await;
    let main_account = r.status("main").account_id.clone().unwrap();
    let worker_account = r.status("worker").account_id.clone().unwrap();
    assert_ne!(main_account, worker_account);
    let credential_of = |account: &str| if account == "a" { A } else { B };

    r.advance(270_000);
    r.step().await;
    let probe_credentials: Vec<Option<String>> =
        r.up.requests()
            .into_iter()
            .filter(|q| q.body.windows(14).any(|w| w == b"\"max_tokens\":0"))
            .map(|q| q.credential().map(str::to_string))
            .collect();
    assert_eq!(probe_credentials.len(), 2, "both agents' recipes are due");
    assert!(
        probe_credentials.contains(&Some(credential_of(&main_account).to_string())),
        "the gate reads main's own pin"
    );
    assert!(
        probe_credentials.contains(&Some(credential_of(&worker_account).to_string())),
        "the gate reads the subagent's own pin"
    );

    // A cooldown on the subagent's account holds its recipe alone.
    let until = r.now() + 600_000;
    r.h.router
        .report(Report::Cooldown {
            account_id: worker_account.clone(),
            row: CooldownRow {
                account_id: worker_account.clone(),
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
        })
        .await;
    settled(&r, |r| {
        let worker_account = worker_account.clone();
        async move {
            r.h.router
                .cooldowns()
                .await
                .unwrap()
                .iter()
                .any(|c| c.account_id == worker_account)
        }
    })
    .await;
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        (r.status("worker").probes, r.status("worker").reason),
        (1, Reason::Cooled),
        "the subagent's own account is cooled, so its recipe alone holds"
    );
    assert_eq!(
        (r.status("main").probes, r.status("main").reason),
        (2, Reason::Scheduled),
        "the main agent's own account is untouched by its sibling's cooldown"
    );
}

#[tokio::test]
async fn a_pin_the_router_moved_on_the_agents_own_request_moves_the_next_probe() {
    let mut r = armed(RigSpec {
        accounts: vec![api_key("a", A, 0), api_key("b", B, 0)],
        ..RigSpec::default()
    })
    .await;
    let first = r.status("main").account_id.clone().unwrap();
    let (other, other_id) = if first == "a" { (B, "b") } else { (A, "a") };
    let pin =
        r.h.router
            .pins()
            .await
            .unwrap()
            .into_iter()
            .find(|p| p.session_id == "session" && p.agent_id == "main")
            .expect("the first observed request left a pin");
    r.h.router
        .report(Report::Cooldown {
            account_id: first.clone(),
            row: CooldownRow {
                account_id: first.clone(),
                scope: "account".into(),
                until: r.now() + 600_000,
                set_at: r.now(),
                reason: "rate_limit".into(),
                window: "five_hour".into(),
                source_request_id: None,
                incarnation: 1,
            },
            pin: Some(PinRef {
                key_id: pin.key_id.clone(),
                session_id: pin.session_id.clone(),
                agent_id: pin.agent_id.clone(),
                model: pin.model.clone(),
                epoch: pin.epoch,
                lane: pin.lane.clone(),
            }),
            lifecycle_generation: 1,
            credential_generation: 1,
            incarnation: 1,
        })
        .await;
    settled(&r, |r| {
        let first = first.clone();
        async move {
            r.h.router
                .cooldowns()
                .await
                .unwrap()
                .iter()
                .any(|c| c.account_id == first)
        }
    })
    .await;
    // A direct select the engine never observes, so the recipe below still names the account the pin left.
    let snapshot = r.h.router.snapshot();
    let key = snapshot.key(&pin.key_id).unwrap();
    let answer =
        r.h.router
            .select(SelectRequest {
                request_id: "r-1".into(),
                attempt: 1,
                seq: 1,
                key_id: pin.key_id.clone(),
                key_generation: key.created_at,
                agent_id: pin.agent_id.clone(),
                session_id: Some(pin.session_id.clone()),
                canonical_model: pin.model.clone(),
                folded_model: pin.model.clone(),
                candidates: resolve(&snapshot, key, &pin.model, &pin.model).unwrap(),
                portable: true,
                attempted: vec![],
                now: r.now(),
                fallback_ms: 60_000,
                revision: snapshot.revision,
                probe: false,
                count: false,
                request_class: None,
                context_compacted: false,
                static_hash: None,
                boundaries: Arc::from([]),
                client: ClientClass::ClaudeCode,
                static_boundaries: Arc::from([]),
                marker: false,
                parent_agent_id: None,
                holds: Vec::new(),
                dialed: false,
                held_out: false,
                lane: lane_key(&pin.model, "standard", "none", "-"),
                fast: false,
                body_tokens: 0,
            })
            .await
            .unwrap();
    assert!(
        matches!(&answer.result, SelectResult::Selected(selection) if selection.account.id == other_id),
        "the router places the freed session on the other account"
    );
    assert_eq!(
        r.status("main").account_id.as_deref(),
        Some(first.as_str()),
        "the recipe still names the old account until the tick reads the pin"
    );
    r.h.router.clear_cooldown(&first, "account").await.unwrap();
    r.advance(270_000);
    r.step().await;
    let probes: Vec<_> =
        r.up.requests()
            .into_iter()
            .filter(|q| q.body.windows(14).any(|w| w == b"\"max_tokens\":0"))
            .collect();
    assert_eq!(probes.len(), 1);
    assert_eq!(
        probes[0].credential(),
        Some(other),
        "the probe followed the pin to the new account"
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
async fn the_due_time_follows_the_timings_lead_and_jitter() {
    let timing = KeepaliveTiming {
        lead_5m_ms: 90_000,
        jitter_5m_ms: 10_000,
        lead_1h_ms: 600_000,
        jitter_1h_ms: 0,
        ..KeepaliveTiming::default()
    };
    let mut r = armed(RigSpec {
        timing,
        ..RigSpec::default()
    })
    .await;
    let start = r.now();
    let due = r.status("main").next_probe_at.unwrap();
    assert!(
        (start + 200_000..=start + 210_000).contains(&due),
        "cache start plus 210 s less at most 10 s of jitter: {}",
        due - start
    );
    r.advance(199_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 0, "not yet due");
    r.advance(11_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 1);
    // A one-hour entry takes the one-hour lead.
    let r = rig(RigSpec {
        timing,
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
        Some(r.now() + 3_600_000 - 600_000)
    );
}

#[tokio::test]
async fn the_session_expiry_discards_recipes_and_the_scheduler_ticks() {
    // The expiry ends the session long before the idle window would.
    let mut r = armed(RigSpec {
        session_expiry: std::time::Duration::from_secs(600),
        ..RigSpec::default()
    })
    .await;
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 1);
    r.advance(330_000);
    r.step().await;
    assert!(
        r.engine.sessions().is_empty(),
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
            r.db.read_blocking(|c| {
                read_rollups(c, hour_of(NOW) - 3_600, i64::MAX, &RollupSelect::default())
            })
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
    r.h.router
        .report(Report::Cooldown {
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
        })
        .await;
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

/// A request the guard sets aside never touches `reason`, so it must not read as the hold beginning again: an
/// overage hold that spans an auxiliary request still withholds exactly the one probe it always did.
#[tokio::test]
async fn an_auxiliary_request_mid_hold_leaves_the_skipped_count_at_one() {
    let mut r = armed(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    r.enter_overage("a", r.now() + 600_000).await;
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
    assert_eq!(
        (r.status("main").probes, r.status("main").reason),
        (0, Reason::Overage)
    );
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
        r.status("main").reason,
        Reason::Overage,
        "the auxiliary request left the hold's own reason alone"
    );
    for _ in 0..5 {
        r.step().await;
    }
    assert_eq!(
        (r.status("main").probes, r.status("main").reason),
        (0, Reason::Overage)
    );
    assert_eq!(
        skipped(&r, 1).await,
        1,
        "one withheld probe, not one per auxiliary request the hold spanned"
    );
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
    r.h.store
        .update_api_key(&key, key.updated_at, "test")
        .unwrap();
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
        ..RigSpec::default()
    })
    .await;
    r.advance(270_000);
    assert!(
        r.now() - NOW > LEASE_MS,
        "the first step outlasts the session's lease"
    );
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
        (
            r.status("main").reason,
            r.engine.stats().retained_bytes,
            r.status("main").unanswered
        ),
        (Reason::IdleLimit, 0, 1),
        "the ceiling keeps the probe pending"
    );
    // The probe's entry ended long before the ceiling, so the next tick's lapse settles it, and the save that drops
    // the probe, which the production flusher runs on its own, sends the settlement to the ledger.
    r.advance(1_000);
    r.step().await;
    r.engine.flush_persisted().await;
    let rows = wait_for(std::time::Duration::from_secs(5), || {
        let rows =
            r.db.read_blocking(|c| {
                read_rollups(c, hour_of(NOW) - 3_600, i64::MAX, &RollupSelect::default())
            })
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

/// The idle limit ends the recipe and keeps what it paid for: the probe stays among the kept ones under the pending label
/// `idle_limit` with its funding and its coverage, and the first tick at its entry's end settles it under that label.
#[tokio::test]
async fn the_idle_limit_ends_the_recipe_with_its_probe_kept_pending_until_the_entry_lapses() {
    let mut r = armed(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        policy: KeepalivePolicy {
            max_idle_seconds: 500,
            ..RigSpec::default().policy
        },
        ..RigSpec::default()
    })
    .await;
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").probes, 1, "one probe before the limit");
    r.advance(270_000);
    r.step().await;
    let status = r.status("main");
    assert_eq!(
        (
            status.reason,
            r.engine.stats().retained_bytes,
            status.unanswered
        ),
        (Reason::IdleLimit, 0, 1),
        "the recipe ended with its probe kept: {status:?}"
    );
    let (_, agents) = r.engine.persist_rows();
    let payload: serde_json::Value = serde_json::from_str(&agents[0].payload).unwrap();
    let kept = &payload["kept"];
    assert_eq!(
        (
            kept.as_array().map(Vec::len),
            kept[0]["pending"].as_str(),
            kept[0]["kind"].as_str(),
            kept[0]["in_overage"].as_bool(),
            kept[0]["covered"].as_bool(),
            kept[0]["unit"].as_str(),
            kept[0]["entry_expires"].as_i64(),
        ),
        (
            Some(1),
            Some("idle_limit"),
            Some("api_key"),
            Some(false),
            Some(true),
            Some("usd"),
            Some(NOW + 570_000)
        ),
        "{kept}"
    );
    r.advance(29_999);
    r.step().await;
    assert_eq!(
        r.status("main").unanswered,
        1,
        "pending a millisecond before its entry ends"
    );
    r.advance(1);
    r.step().await;
    assert_eq!(r.status("main").unanswered, 0, "settled at its entry's end");
    // The settlement reaches the ledger with the save that drops the probe, which the production flusher runs.
    r.engine.flush_persisted().await;
    let rows = wait_for(std::time::Duration::from_secs(5), || {
        let rows =
            r.db.read_blocking(|c| {
                read_rollups(c, hour_of(NOW) - 3_600, i64::MAX, &RollupSelect::default())
            })
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
            .wasted_probe_reasons,
        std::collections::BTreeMap::from([("idle_limit".to_string(), 1)])
    );
}

/// A five-hour cooldown on the rig's one account, applied and settled.
async fn cool_account(r: &Rig) {
    r.h.router
        .report(Report::Cooldown {
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
        })
        .await;
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
        r.db.read_blocking(|c| {
            read_rollups(c, hour_of(NOW) - 3_600, i64::MAX, &RollupSelect::default())
        })
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

/// The id `catalog_rules` resolves the rig's `claude-sonnet-4-5` spelling to.
const DATED: &str = "claude-sonnet-4-5-20250929";

#[tokio::test]
async fn a_recipe_retained_before_its_model_resolved_is_held_by_a_cooldown_under_the_dated_id() {
    let mut r = armed(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    assert_eq!(r.status("main").canonical_model, "claude-sonnet-4-5");
    // The refresh lands the dated id; the relay names requests by it only once the router has moved the
    // spelling's pins and cooldowns.
    let listing = relay_common::catalog_rules().remove(0);
    r.h.catalog_up
        .push_credential_rule(A, &listing.path_prefix, listing.respond);
    let a = r.h.store.account("a").unwrap().unwrap();
    r.h.catalog.refresh(&a).await;
    assert_eq!(
        r.h.catalog.lookup("claude-sonnet-4-5").as_deref(),
        Some(DATED)
    );
    r.h.router
        .report(Report::Cooldown {
            account_id: "a".into(),
            row: CooldownRow {
                account_id: "a".into(),
                scope: format!("model:{DATED}"),
                until: r.now() + 600_000,
                set_at: r.now(),
                reason: phoebis_routing::MODEL_REJECTED_REASON.into(),
                window: "model_rejection".into(),
                source_request_id: None,
                incarnation: 1,
            },
            pin: None,
            lifecycle_generation: 1,
            credential_generation: 1,
            incarnation: 1,
        })
        .await;
    settled(&r, |r| async move {
        r.h.router
            .cooldowns()
            .await
            .unwrap()
            .iter()
            .any(|c| c.scope == format!("model:{DATED}"))
    })
    .await;
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        (r.status("main").probes, r.status("main").reason),
        (0, Reason::Cooled),
        "the gate reads the cooldown under the name the router holds, so no probe is dispatched"
    );
    assert_eq!(r.status("main").canonical_model, DATED);
}

/// A tier read that keeps failing warns once per account and streak. An account deleted while its streak lasts leaves
/// the streak at the next tick, so a later account under the same id starts its own streak with its own first line.
#[tokio::test]
async fn an_account_deleted_during_a_failing_tier_streak_leaves_it_and_a_later_account_warns_again()
{
    deleted_during_a_failing_tier_streak(false).await;
}

/// A tick with no recipe retained skips the router's view, so the streak has to leave at that tick's first read of
/// the registry for a later account under the same id to warn again.
#[tokio::test]
async fn an_account_deleted_while_nothing_is_retained_leaves_its_tier_streak_too() {
    deleted_during_a_failing_tier_streak(true).await;
}

/// With `session_ended` the rig's session ends before the deletion, so no recipe is retained at the deletion's tick.
async fn deleted_during_a_failing_tier_streak(session_ended: bool) {
    const OAUTH_SECRET: &str = "sk-ant-oat01-OOOO";
    let captured = Captured::default();
    let _guard = captured.install();
    let mut r = rig(RigSpec {
        accounts: vec![oauth("o", OAUTH_SECRET, 0)],
        on_disk: true,
        pool: PoolSpec {
            overage_policy: OveragePolicy::Allow,
            ..PoolSpec::default()
        },
        tier: Some("default_claude_max_5x"),
        ..RigSpec::default()
    })
    .await;
    let registry =
        rusqlite::Connection::open(r.store_path.as_ref().expect("an on-disk rig")).unwrap();
    // The tier reads name the column, and so do the registry's reload and an account's insert, so the column is back
    // while the account goes and comes again.
    let unreadable = |held: bool| {
        registry
            .execute_batch(if held {
                "ALTER TABLE accounts RENAME COLUMN subscription_tier TO subscription_tier_held;"
            } else {
                "ALTER TABLE accounts RENAME COLUMN subscription_tier_held TO subscription_tier;"
            })
            .unwrap();
    };
    let warnings = || {
        captured
            .text()
            .lines()
            .filter(|line| line.contains("the account's tier could not be read"))
            .count()
    };
    r.observe("main").await;
    unreadable(true);
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        (r.status("main").probes, warnings()),
        (1, 1),
        "the first failing read starts the streak:\n{}",
        captured.text()
    );

    unreadable(false);
    if session_ended {
        // The lease lapsed during the wait, and only a start takes the session back for its end to land.
        r.reregister();
        r.event(session_end("session")).unwrap();
        assert_eq!(r.status("main").reason, Reason::SessionEnded);
    }
    r.h.store.delete_account("o", "test").unwrap();
    r.h.router.reload().await.unwrap();
    r.step().await;
    r.h.store
        .insert_account(&relay_common::test_account_row(&oauth(
            "o",
            OAUTH_SECRET,
            0,
        )))
        .unwrap();
    let incarnation = r.h.store.account("o").unwrap().unwrap().incarnation;
    r.h.store
        .commit_credentials(
            "o",
            incarnation,
            1,
            None,
            None,
            &phoebis_store::NewBundle {
                access_token: phoebis_store::Secret::new(OAUTH_SECRET),
                refresh_token: None,
                issued_at: NOW,
                expires_at: i64::MAX / 2,
                refresh_expires_at: None,
                scopes: "user:inference".into(),
            },
        )
        .unwrap();
    r.h.store
        .set_pool_members(
            "main",
            &[MemberWrite {
                account_id: "o".into(),
                priority: 0,
                budget_daily_usd: None,
                budget_monthly_usd: None,
            }],
            "test",
        )
        .unwrap();
    r.h.router.reload().await.unwrap();
    unreadable(true);
    if session_ended {
        r.event(session_start("session")).unwrap();
    }

    r.observe("main").await;
    r.advance(270_000);
    r.step().await;
    assert_eq!(r.status("main").account_id.as_deref(), Some("o"));
    assert_eq!(
        warnings(),
        2,
        "the later account's first failing read warns:\n{}",
        captured.text()
    );
}
