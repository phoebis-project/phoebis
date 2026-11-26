//! The three ceilings a retained recipe is probed under: the idle window of the main conversation in every state,
//! a park on a background command or a subagent, and a park on a scheduled wakeup, active while any of them says
//! probe. Every break-even figure is the exact millisecond the break-even gives at the production timing, and a recipe
//! ends at the first tick at or after its ceiling.
mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use common::*;
use phoebis_ledger::{
    hour_of, read_rollups, CostRevisions, PlanTables, PriceRevisions, RollupSelect,
};
use phoebis_session_engine::{KeepalivePolicy, KeepaliveTiming, Lifecycle, Reason};
use serde_json::{json, Value};

const HOUR: i64 = 3_600_000;
const OAUTH: &str = "sk-ant-oat01-OOOO";
const TIER: &str = "default_claude_max_20x";

/// The rig's model at the Sonnet shape: a read of 0.1 against writes of 1.25 and 2.0, so the break-even in dollars
/// is 2,760,000 ms (46 min) on a five-minute entry and 61,560,000 ms (17.1 h) on a one-hour one.
const PRICES: &str = r#"
revisions:
  - effective: 2026-01-01
    models:
      claude-sonnet-4-5: {input: 1, cache_write_5m: 1.25, cache_write_1h: 2.0, cache_read: 0.1, output: 5}
"#;

/// The same model on the plan: a read of 0.025 against both writes of 1.25, so the break-even is 11,760,000 ms
/// (3 h 16 min) on a five-minute entry and 158,760,000 ms (44.1 h) on a one-hour one.
const COSTS: &str = r#"
unit: {tier: default_claude_max_20x, window: seven_day}
revisions:
  - effective: 2026-01-01
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
    tier_order: [fable, opus, sonnet, haiku]
    model_tier:
      claude-sonnet-4-5: sonnet
"#;

/// The same model with fast cells that are no multiple of its standard ones: a read of 0.4 against a five-minute write
/// of 2.5, so a fast recipe's break-even in dollars is 1,260,000 ms (21 min) on a five-minute entry where the
/// standard cells give 46 min.
const PRICES_WITH_FAST: &str = r#"
revisions:
  - effective: 2026-01-01
    models:
      claude-sonnet-4-5:
        input: 1
        cache_write_5m: 1.25
        cache_write_1h: 2.0
        cache_read: 0.1
        output: 5
        fast: {input: 2, cache_write_5m: 2.5, cache_write_1h: 4.0, cache_read: 0.4, output: 10}
"#;

/// The production leads and jitters, so each figure below is the one the console shows. The parity cases of the
/// other files keep the rig's 30-second lead.
fn timing() -> KeepaliveTiming {
    KeepaliveTiming::default()
}

/// The production leads without the jitter, so a case that reads an entry's expiry off its due time finds it exactly:
/// a five-minute entry expires 60 seconds after its probe is due.
fn exact() -> KeepaliveTiming {
    KeepaliveTiming {
        jitter_5m_ms: 0,
        jitter_1h_ms: 0,
        ..KeepaliveTiming::default()
    }
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
        revisions: CostRevisions::from_yaml(COSTS).unwrap(),
        path: None,
        source: "test".to_string(),
    }
}

/// An API-key account: the recipe's break-even reads the dollar row.
fn dollars(policy: KeepalivePolicy) -> RigSpec {
    RigSpec {
        accounts: vec![api_key("a", A, 0)],
        policy,
        pricing: PriceRevisions::from_yaml(PRICES, "test").unwrap(),
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
            r.advance_attended(next - now).await;
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

/// The due probes the ledger counts as withheld so far.
fn skipped_now(r: &Rig) -> u64 {
    r.db.read_blocking(|c| {
        read_rollups(c, hour_of(NOW) - 3_600, i64::MAX, &RollupSelect::default())
    })
    .unwrap()
    .iter()
    .map(|row| row.probes_skipped)
    .sum()
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

async fn report(r: &mut Rig, agent: &str, mut fields: Value) {
    fields["agent_id"] = agent.into();
    r.report(fields).await.unwrap();
}

async fn park(r: &mut Rig, agent: &str, kind: &str, deadline: Option<i64>, wake_at: Option<i64>) {
    let mut fields = json!({"event": "state", "state": "parked", "park_kind": kind});
    if let Some(deadline) = deadline {
        fields["deadline"] = deadline.into();
    }
    if let Some(wake_at) = wake_at {
        fields["wake_at"] = wake_at.into();
    }
    report(r, agent, fields).await;
}

async fn presence(r: &mut Rig) {
    report(r, "main", json!({"event": "activity", "kind": "presence"})).await;
}

/// The end of the newest entry the agent's pending probes wrote, read off the payload a save wrote.
async fn pending_lapse(r: &Rig, agent: &str) -> i64 {
    let payload = r.saved(agent).await;
    payload["kept"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|kept| kept["pending"].as_str().is_some())
        .filter_map(|kept| kept["entry_expires"].as_i64())
        .max()
        .unwrap_or_else(|| panic!("{agent} holds no pending probe: {}", payload["kept"]))
}

/// The recipe is still retained at `end - 1` and ends with `reason` on the tick at `end`, its unanswered probes
/// settled under `label`: at once, or under `idle_limit` once the newest entry they wrote has ended, every probe
/// pending until its own entry's end. The wasted rows read every probe since the request under `label`.
async fn ends_at(r: &mut Rig, agent: &str, end: i64, reason: Reason, label: &str) {
    tick_to(r, end - 1).await;
    assert!(
        retained(r, agent),
        "{agent} is still probed a millisecond before {end}: {:?}",
        r.view(agent)
    );
    tick_to(r, end).await;
    let view = r.view(agent);
    assert_eq!(
        (view.cause, retained(r, agent)),
        (reason, false),
        "{agent} ends at {end}: {view:?}"
    );
    // The lifetime count, since a break-even can run past a UTC midnight that the view's day count restarts at.
    let probes = r.saved(agent).await["probes"].as_u64().unwrap();
    assert!(probes > 0, "{view:?}");
    if label == "idle_limit" {
        let lapse = pending_lapse(r, agent).await;
        tick_to(r, lapse).await;
        // The sweep's settlements reach the ledger with the save that drops the probes, which the production
        // flusher runs on its own.
        r.engine.flush_persisted().await;
    }
    assert_eq!(
        wasted(r, probes).await,
        BTreeMap::from([(label.to_string(), probes)]),
        "every probe since the last real request is wasted under the reason"
    );
}

#[tokio::test]
async fn main_idle_is_probed_until_the_window_ends_with_idle_limit() {
    let mut r = rig(dollars(policy(|p| p.max_idle_seconds = 1_800))).await;
    r.observe("main").await;
    let start = r.now();
    r.state("main", "idle").await;
    tick_to(&mut r, start + 1_800_000 - 1).await;
    let view = r.view("main");
    assert_eq!(
        (view.agent_state, view.cause, view.ttl_seconds),
        (Lifecycle::Idle, Reason::Scheduled, 300),
        "{view:?}"
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
async fn an_idle_window_above_the_break_even_runs_to_its_set_limit() {
    // The hour sits 14 minutes past the dollar row's break-even of 2,760,000 ms on a five-minute entry.
    let mut r = rig(dollars(policy(|p| p.max_idle_seconds = 3_600))).await;
    r.observe("main").await;
    let start = r.now();
    r.state("main", "idle").await;
    ends_at(
        &mut r,
        "main",
        start + 3_600_000,
        Reason::IdleLimit,
        "idle_limit",
    )
    .await;
}

#[tokio::test]
async fn presence_restarts_the_window_and_marks_nothing_busy() {
    let mut r = rig(plan(policy(|_| {}))).await;
    r.observe("main").await;
    let start = r.now();
    r.state("main", "idle").await;
    tick_to(&mut r, start + 50 * MINUTE).await;
    presence(&mut r).await;
    assert_eq!(
        (
            r.view("main").agent_state,
            r.saved("main").await["last_activity"].clone()
        ),
        (Lifecycle::Idle, serde_json::json!(r.now())),
        "presence moves the idle clock and resumes nothing"
    );
    tick_to(&mut r, start + 110 * MINUTE - 1).await;
    assert_eq!(
        (r.view("main").agent_state, retained(&r, "main")),
        (Lifecycle::Idle, true),
        "probed past the hour the window would have ended at without the presence, and parked by nothing"
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
    park(&mut r, "main", "background", Some(start + 2 * HOUR), None).await;
    tick_to(&mut r, start + HOUR + 5 * MINUTE).await;
    assert!(
        retained(&r, "main") && r.view("main").agent_state == Lifecycle::Parked,
        "the window ended at an hour and the park goes on: {:?}",
        r.view("main")
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
    park(&mut r, "main", "background", Some(start + 2 * HOUR), None).await;
    tick_to(&mut r, start + 2 * HOUR - 1).await;
    assert_eq!(
        (r.view("main").agent_state, retained(&r, "main")),
        (Lifecycle::Parked, true)
    );
    tick_to(&mut r, start + 2 * HOUR).await;
    assert_eq!(
        (r.view("main").agent_state, retained(&r, "main")),
        (Lifecycle::Idle, true),
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
        )
        .await;
        if mod_first {
            // The mod's heartbeat closes the command past its deadline and reports idle before the router ticks.
            tick_to(&mut r, start + 30 * MINUTE - 1).await;
            r.advance_attended(1).await;
            r.state("main", "idle").await;
        }
        tick_to(&mut r, start + 30 * MINUTE).await;
        assert_eq!(
            (r.view("main").agent_state, retained(&r, "main")),
            (Lifecycle::Idle, true),
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
    park(&mut r, "main", "scheduled", None, Some(start + 90 * MINUTE)).await;
    tick_to(&mut r, start + HOUR).await;
    let at_hour = r.view("main").probes_today;
    tick_to(&mut r, start + 90 * MINUTE - 1).await;
    let near = r.view("main");
    assert_eq!(
        (near.agent_state, near.cause, retained(&r, "main")),
        (Lifecycle::Parked, Reason::Scheduled, true),
        "{near:?}"
    );
    assert!(
        near.probes_today > at_hour,
        "probed past the window's hour: {at_hour} then {near:?}"
    );
    // A wake three days out is past it: probed for the window's hour, then held with its probes written off. Without
    // the jitter a probe is due every four minutes from the request, one of them at the hour, so the entry the last
    // one kept expires 60 seconds into the hold.
    let mut r = rig(RigSpec {
        timing: exact(),
        ..plan(policy(|_| {}))
    })
    .await;
    r.observe("main").await;
    let start = r.now();
    park(&mut r, "main", "scheduled", None, Some(start + 72 * HOUR)).await;
    tick_to(&mut r, start + HOUR - 1).await;
    assert_eq!(
        (r.view("main").cause, r.view("main").next_probe_at),
        (Reason::Scheduled, Some(start + HOUR))
    );
    tick_to(&mut r, start + HOUR).await;
    // Every probe the hold wrote off reads `horizon` in the wasted rows, so a probe the hold left unanswered fails the
    // read below them.
    let held = r.view("main");
    assert_eq!(
        (
            held.cause,
            held.agent_state,
            retained(&r, "main"),
            held.next_probe_at
        ),
        (Reason::Horizon, Lifecycle::Parked, true, None),
        "{held:?}"
    );
    assert_eq!(
        wasted(&r, held.probes_today).await,
        BTreeMap::from([("horizon".to_string(), held.probes_today)])
    );
    for at in [10_000, 20_000] {
        tick_to(&mut r, start + HOUR + at).await;
    }
    assert_eq!(
        (r.view("main").cause, r.view("main").probes_today),
        (Reason::Horizon, held.probes_today),
        "the hold sends nothing"
    );
    assert_eq!(
        r.engine.reasons().get(&Reason::Horizon).copied(),
        Some(1),
        "three held ticks are one hold"
    );
    assert_eq!(
        wasted(&r, held.probes_today).await,
        BTreeMap::from([("horizon".to_string(), held.probes_today)]),
        "the hold settles its probes once"
    );
    // A nearer wake re-opens it while the entry the last probe kept is still warm.
    park(&mut r, "main", "scheduled", None, Some(start + 3 * HOUR)).await;
    tick_to(&mut r, start + HOUR + 30_000).await;
    let reopened = r.view("main");
    assert_eq!(
        (
            reopened.cause,
            reopened.agent_state,
            reopened.probes_today > held.probes_today
        ),
        (Reason::Scheduled, Lifecycle::Parked, true),
        "{reopened:?}"
    );
}

#[tokio::test]
async fn a_horizon_hold_ends_expired_once_its_entry_lapses_and_shows_no_probe_due() {
    let mut r = rig(RigSpec {
        timing: exact(),
        ..plan(policy(|_| {}))
    })
    .await;
    r.observe("main").await;
    let start = r.now();
    park(&mut r, "main", "scheduled", None, Some(start + 72 * HOUR)).await;
    tick_to(&mut r, start + HOUR - 1).await;
    // The probe due at the hour is the one the hold withholds, and the entry the one before it kept expires a lead
    // after that due time.
    let expires = r.view("main").next_probe_at.unwrap() + 60_000;
    tick_to(&mut r, start + HOUR).await;
    let held = r.view("main");
    assert_eq!(
        (held.cause, held.next_probe_at),
        (Reason::Horizon, None),
        "a held recipe has no probe due: {held:?}"
    );
    tick_to(&mut r, expires - 1).await;
    assert!(
        retained(&r, "main") && r.engine.stats().warming.retained_bytes > 0,
        "{:?}",
        r.view("main")
    );
    tick_to(&mut r, expires).await;
    assert_eq!(
        (
            r.view("main").cause,
            retained(&r, "main"),
            r.engine.stats().warming.retained_bytes
        ),
        (Reason::Expired, false, 0),
        "the cold body is freed at its expiry"
    );
    assert_eq!(
        wasted(&r, held.probes_today).await,
        BTreeMap::from([("horizon".to_string(), held.probes_today)]),
        "the hold settled every probe, so the lapse settles none"
    );
    // The ledger takes its rows a moment after the tick, so a second count would land behind the first: the count is
    // read once the ledger's shutdown has landed every row it was sent.
    r.ledger.clone().shutdown().await.unwrap();
    assert_eq!(skipped_now(&r), 1, "the withheld probe counts once");
}

#[tokio::test]
async fn a_disabled_pool_lapses_a_recipe_at_its_expiry_and_one_enabled_again_inside_the_ttl_probes_on(
) {
    // Without the jitter the request's probe is due at 240 s and its entry expires at 300 s.
    let mut r = rig(RigSpec {
        timing: exact(),
        ..plan(policy(|_| {}))
    })
    .await;
    r.observe("main").await;
    let start = r.now();
    park(&mut r, "main", "background", Some(start + 2 * MINUTE), None).await;
    edit_pool(&r, |pool| pool.enabled = false).await;
    // The events route refuses the key of a disabled pool, so the companion sends no report while the pool is
    // disabled, and none behind the enable once the lease lapsed under the disabled pool.
    tick_silent_to(&mut r, start + 2 * MINUTE).await;
    assert_eq!(
        (r.view("main").agent_state, r.view("main").cause),
        (Lifecycle::Idle, Reason::PoolDisabled),
        "the deadline that passes under the disabled pool ends the park on time"
    );
    tick_silent_to(&mut r, start + 300_000 - 1).await;
    assert_eq!(
        (r.view("main").cause, retained(&r, "main")),
        (Reason::PoolDisabled, true)
    );
    tick_silent_to(&mut r, start + 300_000).await;
    assert_eq!(
        (
            r.view("main").cause,
            retained(&r, "main"),
            r.view("main").probes_today
        ),
        (Reason::Expired, false, 0)
    );
    let mut r = rig(RigSpec {
        timing: exact(),
        ..plan(policy(|_| {}))
    })
    .await;
    r.observe("main").await;
    let start = r.now();
    edit_pool(&r, |pool| pool.enabled = false).await;
    tick_silent_to(&mut r, start + 3 * MINUTE).await;
    assert_eq!(r.view("main").cause, Reason::PoolDisabled);
    edit_pool(&r, |pool| pool.enabled = true).await;
    tick_silent_to(&mut r, start + 4 * MINUTE).await;
    assert_eq!(
        (r.view("main").cause, r.view("main").probes_today),
        (Reason::Scheduled, 1),
        "enabled again inside the entry's TTL, the recipe probes on"
    );
}

#[tokio::test]
async fn a_park_with_no_deadline_ends_at_the_break_even_under_a_null_parked_ceiling() {
    let mut r = rig(plan(policy(|_| {}))).await;
    r.observe("main").await;
    let start = r.now();
    park(&mut r, "main", "background", None, None).await;
    ends_at(
        &mut r,
        "main",
        start + 11_760_000,
        Reason::ParkedLimit,
        "parked_limit",
    )
    .await;
}

#[tokio::test]
async fn a_paused_recipe_held_under_horizon_settles_its_probes_there_and_keeps_its_pause() {
    // A paused recipe loses its prompt at its entry's expiry, a minute after the probe that paused it, so the window
    // ends inside that minute here: without the jitter a five-minute entry is probed every 240 s, the probe at 3,600 s
    // pauses it with the entry the 3,360 s probe wrote still warm until 3,660 s, and the window ends at 3,630 s.
    let mut r = rig(RigSpec {
        timing: exact(),
        ..plan(policy(|p| p.max_idle_seconds = 3_630))
    })
    .await;
    r.observe("main").await;
    let start = r.now();
    park(&mut r, "main", "scheduled", None, Some(start + 72 * HOUR)).await;
    tick_to(&mut r, start + 3_500_000).await;
    // An answer with no usage pauses the recipe and cools no account, so no gate hold rewrites the pause's reason.
    r.up.push_credential_rule(
        OAUTH,
        "/v1/messages",
        fake_upstream::Respond::Json {
            status: 200,
            headers: vec![],
            body: br#"{"id":"m","type":"message","content":[]}"#.to_vec(),
        },
    );
    tick_to(&mut r, start + 3_600_000).await;
    let paused = r.view("main");
    let saved = r.saved("main").await;
    assert_eq!(
        (saved["paused"].clone(), paused.cause),
        (serde_json::json!(true), Reason::InvalidProbeUsage),
        "{paused:?}"
    );
    let unanswered = saved["unanswered"].as_array().unwrap().len() as u64
        + saved["kept"].as_array().unwrap().len() as u64;
    assert!(unanswered > 0, "{saved}");
    tick_to(&mut r, start + 3_630_000).await;
    // The hold writes off every probe the pause left, which the wasted rows below read whole under `horizon`.
    let held = r.view("main");
    assert_eq!(
        (
            r.saved("main").await["paused"].clone(),
            held.cause,
            retained(&r, "main")
        ),
        (serde_json::json!(true), Reason::InvalidProbeUsage, true),
        "the pause keeps its reason and the warm entry its prompt: {held:?}"
    );
    assert_eq!(
        wasted(&r, unanswered).await,
        BTreeMap::from([("horizon".to_string(), unanswered)])
    );
    tick_to(&mut r, start + 3_660_000).await;
    assert_eq!(
        (
            r.saved("main").await["paused"].clone(),
            retained(&r, "main")
        ),
        (serde_json::json!(true), false),
        "the cold entry takes the prompt and leaves the pause"
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
    )
    .await;
    tick_to(&mut r, start + HOUR).await;
    assert_eq!(
        (r.view("main").cause, retained(&r, "main")),
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
    let held = r.view("main").probes_today;
    edit_pool(&r, |pool| {
        pool.keepalive_json =
            serde_json::to_string(&policy(|p| p.scheduled_horizon_seconds = Some(7_200))).unwrap()
    })
    .await;
    let now = r.now();
    tick_to(&mut r, now + 1).await;
    assert_eq!(
        r.view("main").cause,
        Reason::Scheduled,
        "the raised horizon reaches the held recipe on the next tick"
    );
    tick_to(&mut r, now + 5 * MINUTE).await;
    assert!(
        r.view("main").probes_today > held,
        "and probing goes on while the entry is warm: {:?}",
        r.view("main")
    );
    let (mut r, _) = held_under_a_one_hour_horizon().await;
    let held = r.view("main").probes_today;
    let mut key = r.h.store.api_key("desktop").unwrap().unwrap();
    key.companion_permissions.keepalive = false;
    r.h.store
        .update_api_key(&key, key.updated_at, "test")
        .unwrap();
    r.h.router.reload().await.unwrap();
    let now = r.now();
    tick_to(&mut r, now + 1).await;
    assert_eq!(
        (
            r.view("main").cause,
            retained(&r, "main"),
            r.engine.stats().warming.retained_bytes
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
    // The events route refuses the key of a disabled pool, so the companion sends no report there.
    tick_silent_to(&mut r, now + 1).await;
    assert_eq!(
        (r.view("main").cause, retained(&r, "main")),
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
        park(&mut r, "main", "scheduled", None, Some(start + 30 * MINUTE)).await;
        if mod_first {
            tick_to(&mut r, start + 30 * MINUTE - 1).await;
            r.advance_attended(1).await;
            r.state("main", "idle").await;
        }
        tick_to(&mut r, start + 30 * MINUTE).await;
        assert_eq!(
            (r.view("main").agent_state, retained(&r, "main")),
            (Lifecycle::Idle, true),
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

/// A report can write a park while the tick waits for the router's view between its two locked sections. A subagent's
/// wake that had passed by the tick's instant leaves no rule that says probe once its park ends, so the recipe ends
/// with `parked_limit` at that tick.
#[tokio::test]
async fn a_park_reported_while_the_tick_waits_for_the_routers_view_ends_before_its_ceiling_is_read()
{
    let mut r = rig(plan(policy(|_| {}))).await;
    r.event(json!({"event": "agent_start", "agent_id": "worker"}))
        .await
        .unwrap();
    r.observe("worker").await;
    let now = r.now();
    let engine = r.engine.clone();
    let mut tick = std::pin::pin!(engine.step());
    // The runtime runs one thread, so the router task cannot answer the tick's first ask inside this poll, and the tick
    // stands between its two sections.
    let first = std::future::poll_fn(|cx| {
        std::task::Poll::Ready(std::future::Future::poll(tick.as_mut(), cx))
    })
    .await;
    assert!(first.is_pending(), "the tick waits for the router's view");
    park(&mut r, "worker", "scheduled", None, Some(now - MINUTE)).await;
    tick.await;
    no_probe_running(&r.engine).await;
    assert_eq!(
        (r.view("worker").cause, retained(&r, "worker")),
        (Reason::ParkedLimit, false),
        "the wake had passed at the tick's instant"
    );
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
    )
    .await;
    tick_to(&mut r, start + 95 * MINUTE).await;
    assert_eq!(
        (r.view("main").agent_state, retained(&r, "main")),
        (Lifecycle::Parked, true),
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
async fn a_repeated_park_of_the_same_kind_keeps_the_parked_ceilings_start() {
    // The repeat at 30 minutes moves the deadline alone, so the ceiling of 5,400 s still counts from the first park.
    let mut r = rig(plan(policy(|p| p.max_parked_seconds = Some(5_400)))).await;
    r.observe("main").await;
    let start = r.now();
    park(&mut r, "main", "background", Some(start + 3 * HOUR), None).await;
    tick_to(&mut r, start + 30 * MINUTE).await;
    park(
        &mut r,
        "main",
        "background",
        Some(start + 3 * HOUR + 30 * MINUTE),
        None,
    )
    .await;
    ends_at(
        &mut r,
        "main",
        start + 90 * MINUTE,
        Reason::ParkedLimit,
        "parked_limit",
    )
    .await;
}

#[tokio::test]
async fn a_subagents_park_follows_the_park_rules_and_no_idle_window_applies_to_it() {
    let mut r = rig(plan(policy(|_| {}))).await;
    r.event(json!({"event": "agent_start", "agent_id": "worker"}))
        .await
        .unwrap();
    r.observe("worker").await;
    let start = r.now();
    park(
        &mut r,
        "worker",
        "background",
        Some(start + 90 * MINUTE),
        None,
    )
    .await;
    tick_to(&mut r, start + 90 * MINUTE - 1).await;
    assert!(
        retained(&r, "worker"),
        "probed past the hour the main conversation's window would hold"
    );
    // At the deadline nothing says probe for a subagent: it ends at once.
    tick_to(&mut r, start + 90 * MINUTE).await;
    assert_eq!(
        (r.view("worker").cause, retained(&r, "worker")),
        (Reason::ParkedLimit, false)
    );
    // A busy subagent has no ceiling of its own.
    let mut r = rig(dollars(policy(|_| {}))).await;
    r.event(json!({"event": "agent_start", "agent_id": "worker"}))
        .await
        .unwrap();
    r.observe_with("worker", &hour_body()).await;
    let start = r.now();
    tick_to(&mut r, start + 24 * HOUR).await;
    let view = r.view("worker");
    assert_eq!(
        (
            view.agent_state,
            view.cause,
            view.ttl_seconds,
            retained(&r, "worker")
        ),
        (Lifecycle::Busy, Reason::Scheduled, 3_600, true),
        "{view:?}"
    );
    // The lifetime count: the day ran past a UTC midnight, where the view's day count starts again.
    let probes = r.saved("worker").await["probes"].as_u64().unwrap();
    assert!(probes >= 26, "{view:?}");
}

#[tokio::test]
async fn a_silent_session_is_probed_under_the_window_from_its_last_real_request_and_the_park_rules_do_not_apply(
) {
    let mut r = rig(plan(policy(|_| {}))).await;
    r.observe("main").await;
    let start = r.now();
    park(&mut r, "main", "background", Some(start + 3 * HOUR), None).await;
    tick_silent_to(&mut r, start + HOUR - 1).await;
    assert_eq!(
        r.engine.sessions()[0].companion,
        phoebis_session_engine::CompanionState::Silent
    );
    assert!(retained(&r, "main"));
    tick_silent_to(&mut r, start + HOUR).await;
    let view = r.view("main");
    assert_eq!(
        (view.cause, retained(&r, "main")),
        (Reason::IdleLimit, false),
        "the park would probe to its deadline, but a silent session reads the window alone: {view:?}"
    );
}

#[tokio::test]
async fn a_busy_main_past_the_window_ends_with_idle_limit() {
    let mut r = rig(plan(policy(|_| {}))).await;
    r.observe("main").await;
    let start = r.now();
    tick_to(&mut r, start + HOUR - 1).await;
    assert_eq!(r.view("main").agent_state, Lifecycle::Busy);
    ends_at(
        &mut r,
        "main",
        start + HOUR,
        Reason::IdleLimit,
        "idle_limit",
    )
    .await;
}

/// A price file that prices no row leaves no break-even of its own: the ceilings read the shipped table's tightest
/// row, whose excess of 11.5 over the read gives 2,760,000 ms on a five-minute entry, the end a zero idle window
/// resolves to.
#[tokio::test]
async fn a_price_file_with_no_usable_row_falls_back_to_the_shipped_tightest_row() {
    let mut r = rig(RigSpec {
        pricing: PriceRevisions::from_yaml(
            "revisions:\n  - effective: 2026-01-01\n    models: {}\n",
            "test",
        )
        .unwrap(),
        ..dollars(policy(|p| p.max_idle_seconds = 0))
    })
    .await;
    r.observe("main").await;
    let start = r.now();
    r.state("main", "idle").await;
    tick_to(&mut r, start + 2_760_000 - 1).await;
    assert!(retained(&r, "main"), "{:?}", r.view("main"));
    tick_to(&mut r, start + 2_760_000).await;
    assert_eq!(
        (r.view("main").cause, retained(&r, "main")),
        (Reason::IdleLimit, false),
        "a zero idle window resolves to the shipped tightest row's break-even"
    );
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
        r.state("main", "idle").await;
        tick_to(&mut r, start + window - 1).await;
        assert!(retained(&r, "main"), "{name}: {:?}", r.view("main"));
        tick_to(&mut r, start + window).await;
        assert_eq!(
            (r.view("main").cause, retained(&r, "main")),
            (Reason::IdleLimit, false),
            "{name}"
        );
    }
}

/// The rig's body sent in fast mode.
fn fast_body() -> String {
    BODY.replacen('{', r#"{"speed":"fast","#, 1)
}

/// A fast recipe draws usage credits at the fast cells, so its break-even reads them: 21 minutes here, where the
/// standard cells of the same row give 46.
#[tokio::test]
async fn a_fast_recipes_ceilings_read_the_fast_cells() {
    let mut r = rig(RigSpec {
        pricing: PriceRevisions::from_yaml(PRICES_WITH_FAST, "test").unwrap(),
        ..plan(policy(|p| p.max_idle_seconds = 0))
    })
    .await;
    r.observe_with("main", &fast_body()).await;
    let start = r.now();
    r.state("main", "idle").await;
    ends_at(
        &mut r,
        "main",
        start + 1_260_000,
        Reason::IdleLimit,
        "idle_limit",
    )
    .await;
}
