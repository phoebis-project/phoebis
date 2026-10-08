//! The Sessions row the admin routes read from the engine: one of five states read off the agent's reason beside the
//! engine's own pause bit, the reason itself as the cause, the mod's lifecycle word with the park, the day's probes and
//! credits, and the seconds since the person was last seen at the terminal.
mod common;

use common::*;
use phoebis_keepalive::{
    recipe_state, AgentState, Event, ParkKind, Reason, RecipeState, RecipeView,
};

const DAY: i64 = 86_400_000;

/// The row `recipes()` answers for one agent of the rig's session.
fn row(r: &Rig, agent: &str) -> RecipeView {
    r.engine
        .recipes()
        .into_iter()
        .find(|v| v.session_id == "session" && v.agent_id == agent)
        .unwrap_or_else(|| panic!("no row for {agent}: {:?}", r.engine.recipes()))
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
fn jump_to(r: &mut Rig, at: i64) {
    let now = r.now();
    r.advance(at - now);
    r.reregister();
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
        Event {
            event: "state".into(),
            state: Some("parked".into()),
            park_kind: Some("background".into()),
            deadline: Some(deadline),
            ..Default::default()
        },
    );
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
            AgentState::Parked,
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
    r.advance_attended(270_000);
    r.step().await;
    let probed = row(&r, "main");
    assert_eq!(
        (probed.probes_today, probed.last_probe_at),
        (1, Some(start + 270_000)),
        "{probed:?}"
    );
    // Past the first request's five-minute TTL and inside the entry the probe kept, a real request reads it warm.
    r.advance_attended(40_000);
    r.observe("main").await;
    let credited = row(&r, "main");
    assert_eq!(
        (credited.probes_today, credited.credited_today),
        (1, 1),
        "{credited:?}"
    );
    // The next UTC day reads zeros before anything happens on it, and its first probe counts from zero.
    jump_to(&mut r, DAY + MINUTE);
    let next_day = row(&r, "main");
    assert_eq!(
        (next_day.probes_today, next_day.credited_today),
        (0, 0),
        "{next_day:?}"
    );
    r.observe("main").await;
    r.advance_attended(270_000);
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
    jump_to(&mut r, at - 270_000);
    r.observe("main").await;
    r.advance_attended(270_000);
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
    let status = r.status("main");
    assert_eq!(
        (
            status.probes_today,
            status.credited_today,
            status.counters_day
        ),
        (1, 0, 0),
        "the engine's counters are untouched: {status:?}"
    );
    // A tick is no event of the counters, whatever it does to the recipe.
    r.step().await;
    let status = r.status("main");
    assert_eq!(
        (status.probes_today, status.counters_day),
        (1, 0),
        "{status:?}"
    );
    assert_eq!(row(&r, "main").probes_today, 0);
    // The next probe is the event that starts the new day.
    r.reregister();
    r.observe("main").await;
    r.advance_attended(270_000);
    r.step().await;
    let status = r.status("main");
    assert_eq!(
        (status.probes_today, status.counters_day),
        (1, 1),
        "{status:?}"
    );
    assert_eq!(row(&r, "main").probes_today, 1);
}

#[tokio::test]
async fn the_five_states_follow_the_table() {
    for (reason, state) in [
        (Reason::Scheduled, RecipeState::Retained),
        (Reason::ForcedTool, RecipeState::NotRetained),
        (Reason::PoolDisabled, RecipeState::Held),
        (Reason::ProbeQuotaExhausted, RecipeState::Paused),
        (Reason::Released, RecipeState::Ended),
    ] {
        assert_eq!(recipe_state(reason, false), state, "{reason:?}");
    }
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    let read = |r: &Rig, agent: &str| {
        let v = row(r, agent);
        (v.state, v.cause)
    };
    assert_eq!(read(&r, "main"), (RecipeState::Retained, Reason::Scheduled));
    r.event(Event {
        event: "agent_start".into(),
        agent_id: Some("worker".into()),
        ..Default::default()
    })
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
    r.advance_attended(60_000);
    r.step().await;
    assert_eq!(read(&r, "main"), (RecipeState::Held, Reason::PoolDisabled));
    edit_pool(&r, |pool| pool.enabled = true).await;
    exhaust_both(&r);
    r.advance_attended(210_000);
    r.step().await;
    assert_eq!(
        read(&r, "main"),
        (RecipeState::Paused, Reason::ProbeQuotaExhausted)
    );
    r.engine.release("desktop", "session");
    assert_eq!(read(&r, "main"), (RecipeState::Ended, Reason::Released));
    assert_eq!(
        read(&r, "worker"),
        (RecipeState::Ended, Reason::Released),
        "the release ends every agent's row, the one the engine never kept a recipe for among them"
    );
}

#[tokio::test]
async fn a_probe_exhaustion_reads_paused_and_a_gate_hold_reads_held_until_the_next_real_request() {
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    exhaust_both(&r);
    r.advance_attended(270_000);
    r.step().await;
    let exhausted = row(&r, "main");
    assert_eq!(
        (exhausted.state, exhausted.cause, exhausted.next_probe_at),
        (RecipeState::Paused, Reason::ProbeQuotaExhausted, None),
        "{exhausted:?}"
    );
    // The accounts' cooldown holds the paused recipe at the next tick, and the hold re-arms nothing.
    r.advance_attended(1_000);
    r.step().await;
    let cooled = row(&r, "main");
    assert_eq!(
        (cooled.state, cooled.cause, cooled.next_probe_at),
        (RecipeState::Paused, Reason::Cooled, None),
        "{cooled:?}"
    );
    // Past the accounts' reset the next real request selects one again.
    r.advance(5_000_000);
    r.reregister();
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
    r.advance_attended(270_000);
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
    let due = r.status("main").next_probe_at;
    assert!(due.is_some());
    edit_pool(&r, |pool| pool.enabled = false).await;
    r.advance_attended(60_000);
    r.step().await;
    let held = row(&r, "main");
    assert_eq!(
        (held.state, held.cause, held.next_probe_at),
        (RecipeState::Held, Reason::PoolDisabled, None),
        "{held:?}"
    );
    // Enabled again before the probe is due: the next tick shows the schedule the hold kept.
    edit_pool(&r, |pool| pool.enabled = true).await;
    r.advance_attended(60_000);
    r.step().await;
    let armed = row(&r, "main");
    assert_eq!(
        (armed.state, armed.cause, armed.next_probe_at),
        (RecipeState::Retained, Reason::Scheduled, due),
        "{armed:?}"
    );
    assert_eq!(r.status("main").probes, 0, "no probe was due yet");
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
        Event {
            event: "activity".into(),
            kind: Some("presence".into()),
            ..Default::default()
        },
    );
    r.advance(12_000);
    assert_eq!(r.engine.sessions()[0].presence_age_seconds, Some(12));
}

#[tokio::test]
async fn an_ended_recipe_keeps_its_row_with_the_cause() {
    let mut r = rig(RigSpec {
        policy: phoebis_keepalive::KeepalivePolicy {
            enabled: true,
            max_idle_seconds: 300,
            ..phoebis_keepalive::KeepalivePolicy::default()
        },
        ..RigSpec::default()
    })
    .await;
    r.observe("main").await;
    r.state("main", "idle");
    r.advance_attended(270_000);
    r.step().await;
    assert_eq!(row(&r, "main").state, RecipeState::Retained);
    r.advance_attended(60_000);
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
        (
            RecipeState::Ended,
            Reason::IdleLimit,
            AgentState::Idle,
            None
        ),
        "{ended:?}"
    );
}

#[tokio::test]
async fn a_request_refused_at_selection_reads_not_retained_and_one_awaiting_its_selection_reads_retained(
) {
    let mut r = rig(RigSpec {
        policy: phoebis_keepalive::KeepalivePolicy {
            enabled: false,
            ..phoebis_keepalive::KeepalivePolicy::default()
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
    assert_eq!(r.status("main").reason, Reason::AwaitingCacheEvidence);
    edit_pool(&r, |pool| pool.enabled = false).await;
    r.advance_attended(10_000);
    r.step().await;
    assert_eq!(r.status("main").reason, Reason::PoolDisabled);
    edit_pool(&r, |pool| pool.enabled = true).await;
    r.advance_attended(10_000);
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
    r.advance_attended(270_000);
    r.step().await;
    edit_pool(&r, |pool| pool.enabled = false).await;
    r.advance_attended(1_000);
    r.step().await;
    let disabled = row(&r, "main");
    assert_eq!(
        (disabled.state, disabled.cause),
        (RecipeState::Paused, Reason::ProbeQuotaExhausted),
        "the pause already withholds every probe, so the disabled pool leaves its cause: {disabled:?}"
    );
    // Enabled again, the paused recipe is never cleared to scheduled. The accounts' cooldown still holds it, as a
    // gate's hold lands on a paused recipe.
    edit_pool(&r, |pool| pool.enabled = true).await;
    r.advance_attended(1_000);
    r.step().await;
    let enabled = row(&r, "main");
    assert_eq!(
        (enabled.state, enabled.cause, r.status("main").paused),
        (RecipeState::Paused, Reason::Cooled, true),
        "{enabled:?}"
    );
}
