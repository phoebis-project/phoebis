//! The Sessions row the admin routes read from the engine: one of five states read off the agent's reason beside the
//! engine's own pause bit, the reason itself as the cause, the agent's lifecycle word with the park, the day's probes
//! and credits, and the seconds since the person was last seen at the terminal.
mod common;

use common::*;
use phoebis_session_engine::{Lifecycle, ParkKind, Reason, RecipeState, RecipeView};
use serde_json::{json, Value};

const DAY: i64 = 86_400_000;

/// The row `recipes()` answers for one agent of the rig's session.
fn row(r: &Rig, agent: &str) -> RecipeView {
    r.engine
        .recipes()
        .into_iter()
        .find(|v| v.session_id == "session" && v.agent_id == agent)
        .unwrap_or_else(|| panic!("no row for {agent}: {:?}", r.engine.recipes()))
}

async fn report(r: &mut Rig, agent: &str, mut fields: Value) {
    fields["agent_id"] = agent.into();
    r.report(fields).await.unwrap();
}

async fn edit_pool(r: &Rig, edit: impl FnOnce(&mut phoebis_store::PoolRow)) {
    let mut pool = r.h.store.pool("main").unwrap().unwrap();
    edit(&mut pool);
    r.h.store
        .update_pool(&pool, pool.updated_at, "test")
        .unwrap();
    r.h.router.reload().await.unwrap();
}

/// Both accounts answer the next probe with the unified five-hour 429, whose reset at 4,600 s cools them both.
fn exhaust_both(r: &Rig) {
    for secret in [A, B] {
        r.up.push_credential_rule(
            secret,
            "/v1/messages",
            relay_common::json_429_unified(4_600, "five_hour"),
        );
    }
}

/// Moves the clock to `at`, outside any lease, and takes the session back as the mod's next start would.
async fn jump_to(r: &mut Rig, at: i64) {
    let now = r.now();
    r.advance(at - now);
    r.reregister().await;
}

#[tokio::test]
async fn a_recipe_row_carries_the_state_the_cause_the_park_and_the_counts() {
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    let start = r.now();
    let deadline = start + 2 * 60 * MINUTE;
    report(
        &mut r,
        "main",
        json!({
            "event": "state",
            "state": "parked",
            "park_kind": "background",
            "deadline": deadline
        }),
    )
    .await;
    let parked = row(&r, "main");
    assert_eq!(
        (
            parked.state,
            parked.cause,
            parked.agent_state,
            parked.park_kind,
            parked.deadline,
            parked.wake_at,
        ),
        (
            RecipeState::Retained,
            Reason::Scheduled,
            Lifecycle::Parked,
            Some(ParkKind::Background),
            Some(deadline),
            None,
        ),
        "{parked:?}"
    );
    assert_eq!(
        (
            parked.probes_today,
            parked.credited_today,
            parked.last_probe_at
        ),
        (0, 0, None)
    );
    assert_eq!(
        (
            parked.account_id.as_deref(),
            parked.model.as_str(),
            parked.ttl_seconds,
            parked.last_request_at
        ),
        (Some("a"), "claude-sonnet-4-5", 300, Some(start)),
        "the pinned account, the model, the TTL and the last request"
    );
    r.advance_attended(270_000).await;
    r.step().await;
    let probed = row(&r, "main");
    assert_eq!(
        (probed.probes_today, probed.last_probe_at),
        (1, Some(start + 270_000)),
        "{probed:?}"
    );
    // Past the first request's five-minute TTL and inside the entry the probe kept, a real request reads it warm.
    r.advance_attended(40_000).await;
    r.observe("main").await;
    let credited = row(&r, "main");
    assert_eq!(
        (credited.probes_today, credited.credited_today),
        (1, 1),
        "{credited:?}"
    );
    // The next UTC day reads zeros before anything happens on it, and its first probe counts from zero.
    jump_to(&mut r, DAY + MINUTE).await;
    let next_day = row(&r, "main");
    assert_eq!(
        (next_day.probes_today, next_day.credited_today),
        (0, 0),
        "{next_day:?}"
    );
    r.observe("main").await;
    r.advance_attended(270_000).await;
    r.step().await;
    let reset = row(&r, "main");
    assert_eq!(
        (
            reset.probes_today,
            reset.credited_today,
            reset.last_probe_at
        ),
        (1, 0, Some(DAY + MINUTE + 270_000)),
        "{reset:?}"
    );
}

#[tokio::test]
async fn the_day_counters_read_zero_after_midnight_without_a_probe_or_a_credit() {
    let mut r = rig(RigSpec::default()).await;
    // The rig's probe is due 270 s after the request, so it goes out at 23:50 UTC.
    let at = DAY - 10 * MINUTE;
    jump_to(&mut r, at - 270_000).await;
    r.observe("main").await;
    r.advance_attended(270_000).await;
    r.step().await;
    let before = row(&r, "main");
    assert_eq!(
        (before.probes_today, before.last_probe_at),
        (1, Some(at)),
        "{before:?}"
    );
    // 00:10 UTC with no probe and no credit since: the row reads the new day's zeros, and the engine has not moved.
    r.advance(20 * MINUTE);
    let after = row(&r, "main");
    assert_eq!(
        (
            after.probes_today,
            after.credited_today,
            after.last_probe_at
        ),
        (0, 0, Some(at)),
        "{after:?}"
    );
    let saved = r.saved("main").await;
    assert_eq!(
        (
            &saved["probes_today"],
            &saved["credited_today"],
            &saved["counters_day"]
        ),
        (
            &serde_json::json!(1),
            &serde_json::json!(0),
            &serde_json::json!(0)
        ),
        "the engine's counters are untouched: {saved}"
    );
    // A tick is no event of the counters, whatever it does to the recipe.
    r.step().await;
    let saved = r.saved("main").await;
    assert_eq!(
        (&saved["probes_today"], &saved["counters_day"]),
        (&serde_json::json!(1), &serde_json::json!(0)),
        "{saved}"
    );
    assert_eq!(row(&r, "main").probes_today, 0);
    // The next probe is the event that starts the new day.
    r.reregister().await;
    r.observe("main").await;
    r.advance_attended(270_000).await;
    r.step().await;
    let saved = r.saved("main").await;
    assert_eq!(
        (&saved["probes_today"], &saved["counters_day"]),
        (&serde_json::json!(1), &serde_json::json!(1)),
        "{saved}"
    );
    assert_eq!(row(&r, "main").probes_today, 1);
}

#[tokio::test]
async fn the_retained_not_retained_held_and_paused_states_follow_the_table() {
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    let read = |r: &Rig, agent: &str| {
        let v = row(r, agent);
        (v.state, v.cause)
    };
    assert_eq!(read(&r, "main"), (RecipeState::Retained, Reason::Scheduled));
    r.event(json!({"event": "agent_start", "agent_id": "worker"}))
        .await
        .unwrap();
    r.observe_with(
        "worker",
        &BODY.replace(
            r#""max_tokens":1024,"#,
            r#""max_tokens":1024,"tool_choice":{"type":"any"},"#,
        ),
    )
    .await;
    assert_eq!(
        read(&r, "worker"),
        (RecipeState::NotRetained, Reason::ForcedTool)
    );
    edit_pool(&r, |pool| pool.enabled = false).await;
    // The events route refuses the key of a disabled pool, so the companion sends no report while the pool is
    // disabled. After that refusal it sends none behind the enable either, and the lease runs out at its 90 seconds.
    r.advance(60_000);
    r.step().await;
    assert_eq!(read(&r, "main"), (RecipeState::Held, Reason::PoolDisabled));
    edit_pool(&r, |pool| pool.enabled = true).await;
    exhaust_both(&r);
    r.advance(210_000);
    r.step().await;
    assert_eq!(
        read(&r, "main"),
        (RecipeState::Paused, Reason::ProbeRetryLimit),
        "the 429's first rung would land at the entry's expiry"
    );
}

#[tokio::test]
async fn a_probe_exhaustion_reads_paused_and_a_gate_hold_reads_held_until_the_next_real_request() {
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    exhaust_both(&r);
    r.advance_attended(270_000).await;
    r.step().await;
    let exhausted = row(&r, "main");
    assert_eq!(
        (exhausted.state, exhausted.cause, exhausted.next_probe_at),
        (RecipeState::Paused, Reason::ProbeRetryLimit, None),
        "the 429's first rung would land at the entry's expiry: {exhausted:?}"
    );
    // The accounts' cooldown holds the paused recipe at the next tick, and the hold re-arms nothing.
    r.advance_attended(1_000).await;
    r.step().await;
    let cooled = row(&r, "main");
    assert_eq!(
        (cooled.state, cooled.cause, cooled.next_probe_at),
        (RecipeState::Paused, Reason::Cooled, None),
        "{cooled:?}"
    );
    // Past the accounts' reset the next real request selects one again.
    r.advance(5_000_000);
    r.reregister().await;
    r.observe("main").await;
    let rearmed = row(&r, "main");
    assert_eq!(
        (rearmed.state, rearmed.cause),
        (RecipeState::Retained, Reason::Scheduled),
        "{rearmed:?}"
    );
    assert!(rearmed.next_probe_at.is_some(), "{rearmed:?}");
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        ..RigSpec::default()
    })
    .await;
    r.observe("main").await;
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
    plant_spend(&r, "over-budget", 5.01).await;
    r.advance_attended(270_000).await;
    r.step().await;
    let held = row(&r, "main");
    assert_eq!(
        (held.state, held.cause),
        (RecipeState::Held, Reason::Budget),
        "{held:?}"
    );
    assert!(
        held.next_probe_at.is_some(),
        "the gate re-reads the budget at every tick, so the schedule stays: {held:?}"
    );
}

#[tokio::test]
async fn a_recipe_held_on_a_disabled_pool_reads_retained_once_the_pool_is_enabled_again() {
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    let due = row(&r, "main").next_probe_at;
    assert!(due.is_some());
    edit_pool(&r, |pool| pool.enabled = false).await;
    // The events route refuses the key of a disabled pool, so the companion sends no report while the pool is
    // disabled. After that refusal it sends none behind the enable either, and the lease runs out at its 90 seconds.
    r.advance(60_000);
    r.step().await;
    let held = row(&r, "main");
    assert_eq!(
        (held.state, held.cause, held.next_probe_at),
        (RecipeState::Held, Reason::PoolDisabled, None),
        "{held:?}"
    );
    // Enabled again before the probe is due: the next tick shows the schedule the hold kept.
    edit_pool(&r, |pool| pool.enabled = true).await;
    r.advance(60_000);
    r.step().await;
    let armed = row(&r, "main");
    assert_eq!(
        (armed.state, armed.cause, armed.next_probe_at),
        (RecipeState::Retained, Reason::Scheduled, due),
        "{armed:?}"
    );
    assert_eq!(row(&r, "main").probes_today, 0, "no probe was due yet");
}

#[tokio::test]
async fn the_session_row_carries_the_presence_age() {
    let mut r = rig(RigSpec::default()).await;
    assert_eq!(
        r.engine.sessions()[0].presence_age_seconds,
        None,
        "no presence reported yet"
    );
    report(
        &mut r,
        "main",
        json!({"event": "activity", "kind": "presence"}),
    )
    .await;
    r.advance(12_000);
    assert_eq!(r.engine.sessions()[0].presence_age_seconds, Some(12));
}

/// Main holds no recipe, so the row's summary reads the recipe of the other agent whose real request is newest. That
/// agent reports `idle` while the older one stays `busy`, so a summary read from main answers no state and one read
/// from the older agent answers `busy`.
#[tokio::test]
async fn a_session_whose_main_holds_no_recipe_reads_the_newest_other_agents_recipe() {
    let mut r = rig(RigSpec::default()).await;
    for agent in ["worker-1", "worker-2"] {
        r.event(json!({"event": "agent_start", "agent_id": agent}))
            .await
            .unwrap();
        r.observe(agent).await;
        r.advance_attended(1_000).await;
    }
    r.state("worker-2", "idle").await;
    assert_eq!(
        serde_json::to_value(r.engine.sessions()[0].keepalive.state).unwrap(),
        json!("idle"),
        "{:?}",
        r.engine.sessions()
    );
}

/// Main's word stands on the row whether or not a recipe stands, so a registered main that reports `idle` before any
/// request reads `idle` beside an empty summary. A state report leaves the reason at `AwaitingRequest`, so main's word
/// reads `idle` here, where a word read from the reason alone would read `registered`.
#[tokio::test]
async fn mains_lifecycle_word_stands_beside_an_empty_summary_while_main_holds_no_recipe() {
    let mut r = rig(RigSpec::default()).await;
    r.state("main", "idle").await;
    assert_eq!(
        serde_json::from_value::<Reason>(r.saved("main").await["reason"].clone()).unwrap(),
        Reason::AwaitingRequest
    );
    let view = &r.engine.sessions()[0];
    assert_eq!(
        (
            view.lifecycles.get("main").copied(),
            view.keepalive.state.is_none()
        ),
        (Some(Lifecycle::Idle), true),
        "{view:?}"
    );
}

/// A registration with no real request is main `Busy` under `AwaitingRequest`, which reads `registered`. Main's first
/// real request keeps it `Busy` under the recipe's own reason, which does not, so a word read from the state alone
/// would still read `registered`.
#[tokio::test]
async fn a_registered_main_reads_registered_until_its_first_real_request() {
    let mut r = rig(RigSpec::default()).await;
    let registered = &r.engine.sessions()[0];
    assert_eq!(
        registered.lifecycles.get("main").copied(),
        Some(Lifecycle::Registered),
        "{registered:?}"
    );
    r.observe("main").await;
    let requested = &r.engine.sessions()[0];
    assert_eq!(
        requested.lifecycles.get("main").copied(),
        Some(Lifecycle::Busy),
        "{requested:?}"
    );
}

#[tokio::test]
async fn an_ended_recipe_keeps_its_row_with_the_cause() {
    let mut r = rig(RigSpec {
        policy: phoebis_session_engine::KeepalivePolicy {
            enabled: true,
            max_idle_seconds: 300,
            ..phoebis_session_engine::KeepalivePolicy::default()
        },
        ..RigSpec::default()
    })
    .await;
    r.observe("main").await;
    r.state("main", "idle").await;
    r.advance_attended(270_000).await;
    r.step().await;
    assert_eq!(row(&r, "main").state, RecipeState::Retained);
    r.advance_attended(60_000).await;
    r.step().await;
    assert!(
        r.engine.recipes_for("desktop", "session").is_empty(),
        "the idle window ended the recipe"
    );
    let ended = row(&r, "main");
    assert_eq!(
        (
            ended.state,
            ended.cause,
            ended.agent_state,
            ended.next_probe_at
        ),
        (RecipeState::Ended, Reason::IdleLimit, Lifecycle::Idle, None),
        "{ended:?}"
    );
}

#[tokio::test]
async fn a_request_refused_at_selection_reads_not_retained_and_one_awaiting_its_selection_reads_retained(
) {
    let mut r = rig(RigSpec {
        policy: phoebis_session_engine::KeepalivePolicy {
            enabled: false,
            ..phoebis_session_engine::KeepalivePolicy::default()
        },
        ..RigSpec::default()
    })
    .await;
    r.observe("main").await;
    let refused = row(&r, "main");
    assert_eq!(
        (refused.state, refused.cause, refused.next_probe_at),
        (RecipeState::NotRetained, Reason::KeepaliveOff, None),
        "nothing was kept, so the held word reads not_retained: {refused:?}"
    );
    // A request the engine follows still holds its observation before any selection, so it reads retained.
    let r = rig(RigSpec::default()).await;
    let pending = r.observed("pending", "main", BODY);
    r.engine.observe(pending);
    let awaiting = row(&r, "main");
    assert_eq!(
        (awaiting.state, awaiting.cause),
        (RecipeState::Retained, Reason::AwaitingSelection),
        "{awaiting:?}"
    );
}

#[tokio::test]
async fn a_pool_disabled_hold_on_a_recipe_awaiting_its_evidence_ends_awaiting_it() {
    let mut r = rig(RigSpec::default()).await;
    let observed = r.observed("r1", "main", BODY);
    r.engine.observe(observed);
    let selected = r.selected("r1", "a");
    r.engine.observe(selected);
    assert_eq!(row(&r, "main").cause, Reason::AwaitingCacheEvidence);
    edit_pool(&r, |pool| pool.enabled = false).await;
    // The events route refuses the key of a disabled pool, so the companion sends no report there.
    r.advance(10_000);
    r.step().await;
    assert_eq!(row(&r, "main").cause, Reason::PoolDisabled);
    edit_pool(&r, |pool| pool.enabled = true).await;
    r.advance_attended(10_000).await;
    r.step().await;
    let awaiting = row(&r, "main");
    assert_eq!(
        (awaiting.state, awaiting.cause, awaiting.next_probe_at),
        (RecipeState::Retained, Reason::AwaitingCacheEvidence, None),
        "the request's evidence has not come, so nothing is scheduled yet: {awaiting:?}"
    );
}

#[tokio::test]
async fn a_paused_recipe_keeps_its_cause_through_a_disabled_pool() {
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    exhaust_both(&r);
    r.advance_attended(270_000).await;
    r.step().await;
    edit_pool(&r, |pool| pool.enabled = false).await;
    // The events route refuses the key of a disabled pool, so the companion sends no report there.
    r.advance(1_000);
    r.step().await;
    let disabled = row(&r, "main");
    assert_eq!(
        (disabled.state, disabled.cause),
        (RecipeState::Paused, Reason::ProbeRetryLimit),
        "the pause already withholds every probe, so the disabled pool leaves its cause: {disabled:?}"
    );
    // Enabled again, the accounts' cooldown holds the recipe at the gate, and the pause bit keeps it `paused` under
    // the hold's cause.
    edit_pool(&r, |pool| pool.enabled = true).await;
    r.advance_attended(1_000).await;
    r.step().await;
    let enabled = row(&r, "main");
    assert_eq!(
        (
            enabled.state,
            enabled.cause,
            r.saved("main").await["paused"].clone()
        ),
        (RecipeState::Paused, Reason::Cooled, serde_json::json!(true)),
        "{enabled:?}"
    );
}

/// A pause withholds every probe until the next real request, and the prompt it keeps is worth nothing once the entry
/// goes cold, so the prompt leaves at the entry's expiry while the row keeps reading `paused` with its cause.
#[tokio::test]
async fn a_paused_recipe_drops_its_prompt_at_its_entrys_expiry_and_still_reads_paused() {
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    let start = r.now();
    exhaust_both(&r);
    r.advance_attended(270_000).await;
    r.step().await;
    let paused = row(&r, "main");
    assert_eq!(
        (paused.state, paused.cause),
        (RecipeState::Paused, Reason::ProbeRetryLimit),
        "{paused:?}"
    );
    let probes = paused.probes_today;
    // The request's entry is five minutes long and the failed probe wrote none, so it expires 300 s after the request.
    let now = r.now();
    r.advance_attended(start + 300_000 - 1 - now).await;
    r.step().await;
    let warm = row(&r, "main");
    assert!(
        r.engine.stats().warming.retained_bytes > 0,
        "the entry is warm a millisecond short of its expiry: {warm:?}"
    );
    r.advance_attended(1).await;
    r.step().await;
    let cold = row(&r, "main");
    let saved = r.saved("main").await;
    assert_eq!(
        (
            cold.state,
            cold.cause,
            saved["paused"].clone(),
            r.engine.stats().warming.retained_bytes,
            cold.probes_today,
            cold.next_probe_at
        ),
        (
            RecipeState::Paused,
            warm.cause,
            serde_json::json!(true),
            0,
            probes,
            None
        ),
        "{cold:?}"
    );
}
