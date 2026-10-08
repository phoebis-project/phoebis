//! The three ceilings a retained recipe is probed under: the idle window of the main conversation in every state,
//! a park on a background command or a subagent, and a park on a scheduled wakeup, active while any of them says
//! probe. Every figure is the exact millisecond the break-even gives at the production timing, and a recipe ends at
//! the first tick at or after it.
mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use common::*;
use phoebis_keepalive::{AgentState, Event, KeepalivePolicy, KeepaliveTiming, Reason};
use phoebis_ledger::{
    hour_of, read_rollups, PlanTables, PriceTable, RollupSelect, SubscriptionCosts,
};

const HOUR: i64 = 3_600_000;
const OAUTH: &str = "sk-ant-oat01-OOOO";
const TIER: &str = "default_claude_max_20x";

/// The rig's model at the Sonnet shape: a read of 0.1 against writes of 1.25 and 2.0, so the break-even in dollars
/// is 2,760,000 ms (46 min) on a five-minute entry and 61,560,000 ms (17.1 h) on a one-hour one.
const PRICES: &str = r#"
effective: 2026-01-01
models:
  claude-sonnet-4-5: {input: 1, cache_write_5m: 1.25, cache_write_1h: 2.0, cache_read: 0.1, output: 5}
"#;

/// The same model on the plan: a read of 0.025 against both writes of 1.25, so the break-even is 11,760,000 ms
/// (3 h 16 min) on a five-minute entry and 158,760,000 ms (44.1 h) on a one-hour one.
const COSTS: &str = r#"
effective: 2026-01-01
unit:
  defined_by: {tier: default_claude_max_20x, window: five_hour, band: out_of_band}
  units_per_window: 100
capacity:
  default_claude_max_20x: {five_hour: 100, seven_day: 1000}
peak:
  zone: America/Los_Angeles
  weekdays: true
  from_hour: 5
  to_hour: 11
  capacity_factor: {five_hour: 0.5, seven_day: 1}
type_multiplier: {cache_read: 0.025, cache_write_1h: 1.25, cache_write_5m: 1.25, input: 1, output: 5}
model_weight:
  claude-sonnet-4-5: 1
model_windows: {}
overrides: {}
"#;

/// The production leads and jitters, so each figure below is the one the console shows; the parity cases of the
/// other files keep the rig's 30-second lead.
fn timing() -> KeepaliveTiming {
    KeepaliveTiming::default()
}

fn policy(edit: impl FnOnce(&mut KeepalivePolicy)) -> KeepalivePolicy {
    let mut policy = KeepalivePolicy {
        enabled: true,
        ..KeepalivePolicy::default()
    };
    edit(&mut policy);
    policy
}

fn tables() -> PlanTables {
    PlanTables {
        costs: SubscriptionCosts::from_yaml(COSTS).unwrap(),
        path: None,
        source: "test".to_string(),
    }
}

/// An Anthropic-key account: the recipe's break-even reads the dollar row.
fn dollars(policy: KeepalivePolicy) -> RigSpec {
    RigSpec {
        accounts: vec![api_key("a", A, 0)],
        policy,
        pricing: PriceTable::from_yaml(PRICES, "test").unwrap(),
        tables: tables(),
        timing: timing(),
        ..RigSpec::default()
    }
}

/// A subscription account on plan quota: the recipe's break-even reads the plan row.
fn plan(policy: KeepalivePolicy) -> RigSpec {
    RigSpec {
        accounts: vec![oauth("o", OAUTH, 0)],
        pool: PoolSpec {
            overage_policy: phoebis_store::OveragePolicy::Allow,
            ..PoolSpec::default()
        },
        tier: Some(TIER),
        ..dollars(policy)
    }
}

/// The rig's body with a one-hour marker.
fn hour_body() -> String {
    BODY.replace(
        r#""cache_control":{"type":"ephemeral"}"#,
        r#""cache_control":{"type":"ephemeral","ttl":"1h"}"#,
    )
}

/// Ticks at every due probe and at `at` itself, the clock moved in steps an attending mod's heartbeat renews the
/// lease inside, so each probe lands when due and the last tick lands on `at` to the millisecond.
async fn tick_to(r: &mut Rig, at: i64) {
    tick(r, at, true).await;
}

/// `tick_to` with no heartbeat: the lease lapses after its 90 seconds and the session goes silent.
async fn tick_silent_to(r: &mut Rig, at: i64) {
    tick(r, at, false).await;
}

async fn tick(r: &mut Rig, at: i64, attended: bool) {
    loop {
        let now = r.now();
        let due = r
            .engine
            .sessions()
            .iter()
            .filter_map(|s| s.keepalive.next_probe_at)
            .filter(|due| *due > now)
            .min();
        let next = due.map_or(at, |due| due.min(at)).max(now);
        if attended {
            r.advance_attended(next - now);
        } else {
            r.advance(next - now);
        }
        r.step().await;
        if next >= at {
            return;
        }
    }
}

fn retained(r: &Rig, agent: &str) -> bool {
    r.engine
        .recipes_for("desktop", "session")
        .iter()
        .any(|id| id == agent)
}

/// The wasted probes the ledger has settled, by reason, once `probes` of them have landed.
async fn wasted(r: &Rig, probes: u64) -> BTreeMap<String, u64> {
    wait_for(Duration::from_secs(5), || {
        let rows =
            r.db.read_blocking(|c| {
                read_rollups(c, hour_of(NOW) - 3_600, i64::MAX, &RollupSelect::default())
            })
            .unwrap();
        let mut reasons: BTreeMap<String, u64> = BTreeMap::new();
        for row in rows.iter().filter(|row| row.kind == "probe") {
            for (reason, count) in &row.wasted_probe_reasons {
                *reasons.entry(reason.clone()).or_default() += count;
            }
        }
        (reasons.values().sum::<u64>() == probes).then_some(reasons)
    })
    .await
}

fn report(r: &mut Rig, agent: &str, fields: Event) {
    let generation = r.status(agent).request_generation;
    r.event(Event {
        agent_id: Some(agent.into()),
        request_generation: Some(generation),
        ..fields
    })
    .unwrap();
}

fn park(r: &mut Rig, agent: &str, kind: &str, deadline: Option<i64>, wake_at: Option<i64>) {
    report(
        r,
        agent,
        Event {
            event: "state".into(),
            state: Some("parked".into()),
            park_kind: Some(kind.into()),
            deadline,
            wake_at,
            ..Default::default()
        },
    );
}

fn presence(r: &mut Rig) {
    report(
        r,
        "main",
        Event {
            event: "activity".into(),
            kind: Some("presence".into()),
            ..Default::default()
        },
    );
}

/// The recipe is still retained at `end - 1` and ends with `reason` on the tick at `end`, its unanswered probes
/// settled under `label`.
async fn ends_at(r: &mut Rig, agent: &str, end: i64, reason: Reason, label: &str) {
    tick_to(r, end - 1).await;
    assert!(
        retained(r, agent),
        "{agent} is still probed a millisecond before {end}: {:?}",
        r.status(agent)
    );
    tick_to(r, end).await;
    let status = r.status(agent);
    assert_eq!(
        (status.reason, retained(r, agent)),
        (reason, false),
        "{agent} ends at {end}: {status:?}"
    );
    assert!(status.probes > 0, "{status:?}");
    assert_eq!(
        wasted(r, status.probes).await,
        BTreeMap::from([(label.to_string(), status.probes)]),
        "every probe since the last real request is wasted under the reason"
    );
}

#[tokio::test]
async fn main_idle_is_probed_until_the_window_ends_with_idle_limit() {
    let mut r = rig(dollars(policy(|p| p.max_idle_seconds = 1_800))).await;
    r.observe("main").await;
    let start = r.now();
    r.state("main", "idle");
    tick_to(&mut r, start + 1_800_000 - 1).await;
    let status = r.status("main");
    assert_eq!(
        (status.state, status.reason, status.cache_ttl_ms),
        (AgentState::Idle, Reason::Scheduled, 300_000),
        "{status:?}"
    );
    ends_at(
        &mut r,
        "main",
        start + 1_800_000,
        Reason::IdleLimit,
        "idle_limit",
    )
    .await;
}

#[tokio::test]
async fn the_idle_window_never_runs_past_the_break_even() {
    let mut r = rig(dollars(policy(|p| p.max_idle_seconds = 3_600))).await;
    r.observe("main").await;
    let start = r.now();
    r.state("main", "idle");
    ends_at(
        &mut r,
        "main",
        start + 2_760_000,
        Reason::IdleLimit,
        "idle_limit",
    )
    .await;
}

#[tokio::test]
async fn a_zero_idle_window_resolves_to_the_break_even() {
    let mut r = rig(plan(policy(|p| p.max_idle_seconds = 0))).await;
    r.observe("main").await;
    let start = r.now();
    r.state("main", "idle");
    ends_at(
        &mut r,
        "main",
        start + 11_760_000,
        Reason::IdleLimit,
        "idle_limit",
    )
    .await;
}

#[tokio::test]
async fn presence_restarts_the_window_and_marks_nothing_busy() {
    let (tx, mut rx) = phoebis_bus::lifecycle_channel();
    let mut r = rig(RigSpec {
        lifecycle: Some(tx),
        ..plan(policy(|_| {}))
    })
    .await;
    r.observe("main").await;
    let start = r.now();
    r.state("main", "idle");
    tick_to(&mut r, start + 50 * MINUTE).await;
    while rx.try_recv().is_ok() {}
    presence(&mut r);
    assert_eq!(r.status("main").state, AgentState::Idle);
    tick_to(&mut r, start + 110 * MINUTE - 1).await;
    assert_eq!(
        (r.status("main").state, retained(&r, "main")),
        (AgentState::Idle, true),
        "probed past the hour the window would have ended at without the presence"
    );
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    assert!(
        events
            .iter()
            .all(|e| matches!(e, phoebis_bus::SessionLifecycle::Heartbeat { .. })),
        "presence resumes nothing and parks nothing: {events:?}"
    );
    ends_at(
        &mut r,
        "main",
        start + 110 * MINUTE,
        Reason::IdleLimit,
        "idle_limit",
    )
    .await;
}

#[tokio::test]
async fn a_park_on_a_task_is_probed_past_the_window_until_max_parked_seconds_or_the_break_even() {
    // A ceiling of 5,400 s ends the park at 90 minutes, the idle window having ended at 60.
    let mut r = rig(plan(policy(|p| p.max_parked_seconds = Some(5_400)))).await;
    r.observe("main").await;
    let start = r.now();
    park(&mut r, "main", "background", Some(start + 2 * HOUR), None);
    tick_to(&mut r, start + HOUR + 5 * MINUTE).await;
    assert!(
        retained(&r, "main") && r.status("main").state == AgentState::Parked,
        "the window ended at an hour and the park goes on: {:?}",
        r.status("main")
    );
    ends_at(
        &mut r,
        "main",
        start + 90 * MINUTE,
        Reason::ParkedLimit,
        "parked_limit",
    )
    .await;
    // A null ceiling is the break-even, 3 h 16 min on the plan row, so the park is probed to its deadline, which
    // ends the park and restarts the window there.
    let mut r = rig(plan(policy(|_| {}))).await;
    r.observe("main").await;
    let start = r.now();
    park(&mut r, "main", "background", Some(start + 2 * HOUR), None);
    tick_to(&mut r, start + 2 * HOUR - 1).await;
    assert_eq!(
        (r.status("main").state, retained(&r, "main")),
        (AgentState::Parked, true)
    );
    tick_to(&mut r, start + 2 * HOUR).await;
    assert_eq!(
        (r.status("main").state, retained(&r, "main")),
        (AgentState::Idle, true),
        "the deadline ends the park and the window restarts"
    );
    ends_at(
        &mut r,
        "main",
        start + 3 * HOUR,
        Reason::IdleLimit,
        "idle_limit",
    )
    .await;
}

#[tokio::test]
async fn a_deadline_ends_the_park_and_restarts_the_window_in_both_orderings() {
    for mod_first in [false, true] {
        let mut r = rig(plan(policy(|_| {}))).await;
        r.observe("main").await;
        let start = r.now();
        park(
            &mut r,
            "main",
            "background",
            Some(start + 30 * MINUTE),
            None,
        );
        if mod_first {
            // The mod's heartbeat closes the command past its deadline and reports idle before the router ticks.
            tick_to(&mut r, start + 30 * MINUTE - 1).await;
            r.advance_attended(1);
            r.state("main", "idle");
        }
        tick_to(&mut r, start + 30 * MINUTE).await;
        assert_eq!(
            (r.status("main").state, retained(&r, "main")),
            (AgentState::Idle, true),
            "mod first: {mod_first}"
        );
        // Never at 60 minutes, the end of a window counted from the request.
        ends_at(
            &mut r,
            "main",
            start + 90 * MINUTE,
            Reason::IdleLimit,
            "idle_limit",
        )
        .await;
    }
}

#[tokio::test]
async fn a_scheduled_park_is_probed_to_a_wake_within_the_horizon_and_held_with_horizon_beyond_it() {
    // A wake 90 minutes out is inside the plan row's break-even: probed to the wake, past the window's hour.
    let mut r = rig(plan(policy(|_| {}))).await;
    r.observe("main").await;
    let start = r.now();
    park(&mut r, "main", "scheduled", None, Some(start + 90 * MINUTE));
    tick_to(&mut r, start + 90 * MINUTE - 1).await;
    assert_eq!(
        (r.status("main").state, retained(&r, "main")),
        (AgentState::Parked, true)
    );
    // A wake three days out is past it: probed for the window's hour, then held with its probes written off.
    let mut r = rig(plan(policy(|_| {}))).await;
    r.observe("main").await;
    let start = r.now();
    park(&mut r, "main", "scheduled", None, Some(start + 72 * HOUR));
    tick_to(&mut r, start + HOUR - 1).await;
    assert_eq!(r.status("main").reason, Reason::Scheduled);
    tick_to(&mut r, start + HOUR).await;
    let held = r.status("main");
    assert_eq!(
        (
            held.reason,
            held.state,
            retained(&r, "main"),
            held.unanswered
        ),
        (Reason::Horizon, AgentState::Parked, true, 0),
        "{held:?}"
    );
    assert_eq!(
        wasted(&r, held.probes).await,
        BTreeMap::from([("horizon".to_string(), held.probes)])
    );
    tick_to(&mut r, start + HOUR + MINUTE).await;
    assert_eq!(
        (r.status("main").reason, r.status("main").probes),
        (Reason::Horizon, held.probes),
        "the hold sends nothing"
    );
    assert_eq!(
        wasted(&r, held.probes).await,
        BTreeMap::from([("horizon".to_string(), held.probes)]),
        "the hold settles its probes once"
    );
    // A nearer wake re-opens it while the entry the last probe kept is still warm.
    let now = r.now();
    park(&mut r, "main", "scheduled", None, Some(now + 2 * HOUR));
    tick_to(&mut r, now + 5 * MINUTE).await;
    let reopened = r.status("main");
    assert_eq!(
        (
            reopened.reason,
            reopened.state,
            reopened.probes > held.probes
        ),
        (Reason::Scheduled, AgentState::Parked, true),
        "{reopened:?}"
    );
}

/// A recipe held under `horizon`: the plan row with a one-hour horizon and a wake 150 minutes out, so at the end of
/// the window's hour the wake is 90 minutes away, past the horizon.
async fn held_under_a_one_hour_horizon() -> (Rig, i64) {
    let mut r = rig(plan(policy(|p| p.scheduled_horizon_seconds = Some(3_600)))).await;
    r.observe("main").await;
    let start = r.now();
    park(
        &mut r,
        "main",
        "scheduled",
        None,
        Some(start + 150 * MINUTE),
    );
    tick_to(&mut r, start + HOUR).await;
    assert_eq!(
        (r.status("main").reason, retained(&r, "main")),
        (Reason::Horizon, true)
    );
    (r, start)
}

async fn edit_pool(r: &Rig, edit: impl FnOnce(&mut phoebis_store::PoolRow)) {
    let mut pool = r.h.store.pool("main").unwrap().unwrap();
    edit(&mut pool);
    r.h.store
        .update_pool(&pool, pool.updated_at, "test")
        .unwrap();
    r.h.router.reload().await.unwrap();
}

#[tokio::test]
async fn a_raised_horizon_releases_a_held_recipe_and_a_key_opt_out_discards_it() {
    let (mut r, _) = held_under_a_one_hour_horizon().await;
    let held = r.status("main").probes;
    edit_pool(&r, |pool| {
        pool.keepalive_json =
            serde_json::to_string(&policy(|p| p.scheduled_horizon_seconds = Some(7_200))).unwrap()
    })
    .await;
    let now = r.now();
    tick_to(&mut r, now + 5 * MINUTE).await;
    assert_eq!(
        (r.status("main").reason, r.status("main").probes > held),
        (Reason::Scheduled, true),
        "the raised horizon reaches the held recipe on the next tick"
    );
    let (mut r, _) = held_under_a_one_hour_horizon().await;
    let held = r.status("main").probes;
    let mut key = r.h.store.api_key("desktop").unwrap().unwrap();
    key.keepalive_opt_out = true;
    r.h.store
        .update_api_key(&key, key.updated_at, "test")
        .unwrap();
    r.h.router.reload().await.unwrap();
    let now = r.now();
    tick_to(&mut r, now + 1).await;
    assert_eq!(
        (
            r.status("main").reason,
            retained(&r, "main"),
            r.engine.stats().retained_bytes
        ),
        (Reason::KeyOptedOut, false, 0),
        "the opt-out discards the held prompt"
    );
    assert_eq!(
        wasted(&r, held).await,
        BTreeMap::from([("horizon".to_string(), held)]),
        "the hold had written every probe off, so the release settles none"
    );
    let (mut r, _) = held_under_a_one_hour_horizon().await;
    edit_pool(&r, |pool| pool.enabled = false).await;
    let now = r.now();
    tick_to(&mut r, now + 1).await;
    assert_eq!(
        (r.status("main").reason, retained(&r, "main")),
        (Reason::PoolDisabled, true),
        "a disabled pool holds the recipe it held under the horizon"
    );
}

#[tokio::test]
async fn a_passed_wake_ends_the_park_like_a_deadline_in_both_orderings() {
    for mod_first in [false, true] {
        let mut r = rig(plan(policy(|_| {}))).await;
        r.observe("main").await;
        let start = r.now();
        park(&mut r, "main", "scheduled", None, Some(start + 30 * MINUTE));
        if mod_first {
            tick_to(&mut r, start + 30 * MINUTE - 1).await;
            r.advance_attended(1);
            r.state("main", "idle");
        }
        tick_to(&mut r, start + 30 * MINUTE).await;
        assert_eq!(
            (r.status("main").state, retained(&r, "main")),
            (AgentState::Idle, true),
            "mod first: {mod_first}"
        );
        ends_at(
            &mut r,
            "main",
            start + 90 * MINUTE,
            Reason::IdleLimit,
            "idle_limit",
        )
        .await;
    }
}

#[tokio::test]
async fn both_kinds_are_active_while_any_rule_says_probe() {
    // The parked ceiling ends the command's part at 90 minutes while the wake at 100 is inside the horizon.
    let mut r = rig(plan(policy(|p| p.max_parked_seconds = Some(5_400)))).await;
    r.observe("main").await;
    let start = r.now();
    park(
        &mut r,
        "main",
        "both",
        Some(start + 3 * HOUR),
        Some(start + 100 * MINUTE),
    );
    tick_to(&mut r, start + 95 * MINUTE).await;
    assert_eq!(
        (r.status("main").state, retained(&r, "main")),
        (AgentState::Parked, true),
        "the near wake keeps it probed past the parked ceiling"
    );
    // The passed wake leaves the command's part alone, whose ceiling has run out: both are exhausted.
    ends_at(
        &mut r,
        "main",
        start + 100 * MINUTE,
        Reason::ParkedLimit,
        "parked_limit",
    )
    .await;
}

#[tokio::test]
async fn a_subagents_park_follows_the_park_rules_and_no_idle_window_applies_to_it() {
    let mut r = rig(plan(policy(|_| {}))).await;
    r.event(Event {
        event: "agent_start".into(),
        agent_id: Some("worker".into()),
        ..Default::default()
    })
    .unwrap();
    r.observe("worker").await;
    let start = r.now();
    park(
        &mut r,
        "worker",
        "background",
        Some(start + 90 * MINUTE),
        None,
    );
    tick_to(&mut r, start + 90 * MINUTE - 1).await;
    assert!(
        retained(&r, "worker"),
        "probed past the hour the main conversation's window would hold"
    );
    // At the deadline nothing says probe for a subagent: it ends at once.
    tick_to(&mut r, start + 90 * MINUTE).await;
    assert_eq!(
        (r.status("worker").reason, retained(&r, "worker")),
        (Reason::ParkedLimit, false)
    );
    // A busy subagent has no ceiling of its own.
    let mut r = rig(dollars(policy(|_| {}))).await;
    r.event(Event {
        event: "agent_start".into(),
        agent_id: Some("worker".into()),
        ..Default::default()
    })
    .unwrap();
    r.observe_with("worker", &hour_body()).await;
    let start = r.now();
    tick_to(&mut r, start + 24 * HOUR).await;
    let status = r.status("worker");
    assert_eq!(
        (
            status.state,
            status.reason,
            status.cache_ttl_ms,
            retained(&r, "worker")
        ),
        (AgentState::Busy, Reason::Scheduled, HOUR, true),
        "{status:?}"
    );
    assert!(status.probes >= 26, "{status:?}");
}

#[tokio::test]
async fn a_silent_session_is_probed_under_the_window_from_its_last_real_request_and_the_park_rules_do_not_apply(
) {
    let mut r = rig(plan(policy(|_| {}))).await;
    r.observe("main").await;
    let start = r.now();
    park(&mut r, "main", "background", Some(start + 3 * HOUR), None);
    tick_silent_to(&mut r, start + HOUR - 1).await;
    assert_eq!(
        r.engine.sessions()[0].companion,
        phoebis_keepalive::CompanionState::Silent
    );
    assert!(retained(&r, "main"));
    tick_silent_to(&mut r, start + HOUR).await;
    let status = r.status("main");
    assert_eq!(
        (status.reason, retained(&r, "main")),
        (Reason::IdleLimit, false),
        "the park would probe to its deadline, but a silent session reads the window alone: {status:?}"
    );
}

#[tokio::test]
async fn a_busy_main_past_the_window_ends_with_idle_limit() {
    let mut r = rig(plan(policy(|_| {}))).await;
    r.observe("main").await;
    let start = r.now();
    tick_to(&mut r, start + HOUR - 1).await;
    assert_eq!(r.status("main").state, AgentState::Busy);
    ends_at(
        &mut r,
        "main",
        start + HOUR,
        Reason::IdleLimit,
        "idle_limit",
    )
    .await;
}

#[tokio::test]
async fn funding_and_ttl_pick_the_break_even() {
    let open = || policy(|p| p.max_idle_seconds = 0);
    for (name, spec, body, window) in [
        (
            "dollars, five minutes",
            dollars(open()),
            BODY.to_string(),
            2_760_000,
        ),
        (
            "dollars, one hour",
            dollars(open()),
            hour_body(),
            61_560_000,
        ),
        (
            "plan, five minutes",
            plan(open()),
            BODY.to_string(),
            11_760_000,
        ),
        ("plan, one hour", plan(open()), hour_body(), 158_760_000),
    ] {
        let mut r = rig(spec).await;
        r.observe_with("main", &body).await;
        let start = r.now();
        r.state("main", "idle");
        tick_to(&mut r, start + window - 1).await;
        assert!(retained(&r, "main"), "{name}: {:?}", r.status("main"));
        tick_to(&mut r, start + window).await;
        assert_eq!(
            (r.status("main").reason, retained(&r, "main")),
            (Reason::IdleLimit, false),
            "{name}"
        );
    }
}
