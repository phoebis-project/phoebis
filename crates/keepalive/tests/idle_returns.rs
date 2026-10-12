//! The idle-age counters: per pool and idle-age bucket, the idle windows of the main conversation that reached the
//! bucket's start and those whose next request continued the prefix the window opened on. The windows are counted at
//! every tick and at the request that ends one, whether or not a recipe is still retained.
mod common;

use common::*;
use phoebis_keepalive::{
    continues_prefix, idle_age_bucket, AgentState, Event, Guard, IdleReturns, KeepalivePolicy,
    Reason, IDLE_AGE_BOUNDS_SECONDS,
};

const OAUTH: &str = "sk-ant-oat01-OOOO";
const TIER: &str = "default_claude_max_20x";
const LAPTOP_SECRET: &str = "phoebis-laptop-key";

/// Main's next turn on the rig's conversation: the first prompt, its answer and a new prompt, so the request's message
/// boundaries begin with the boundary the rig's first request ended on.
fn next_turn() -> String {
    BODY.replace(
        r#"{"role":"user","content":"hello"}]"#,
        r#"{"role":"user","content":"hello"},{"role":"assistant","content":"hi"},{"role":"user","content":"and then"}]"#,
    )
}

/// The rig's system prompt and model on a fresh message chain: a new prefix under the same static one.
fn fresh_chain() -> String {
    BODY.replace("hello", "a fresh conversation")
}

fn with_idle_window(seconds: u64) -> KeepalivePolicy {
    KeepalivePolicy {
        enabled: true,
        max_idle_seconds: seconds,
        ..KeepalivePolicy::default()
    }
}

/// A subscription account on plan quota, so the recipe's ceilings read the plan row.
fn plan_row(policy: KeepalivePolicy) -> RigSpec {
    RigSpec {
        accounts: vec![oauth("o", OAUTH, 0)],
        policy,
        pool: PoolSpec {
            overage_policy: phoebis_store::OveragePolicy::Allow,
            ..PoolSpec::default()
        },
        tier: Some(TIER),
        ..RigSpec::default()
    }
}

fn counters(r: &Rig, pool: &str) -> IdleReturns {
    r.engine
        .idle_returns()
        .get(pool)
        .cloned()
        .unwrap_or_default()
}

fn reached_in(r: &Rig, pool: &str) -> [u64; 10] {
    counters(r, pool).buckets.map(|bucket| bucket.reached)
}

fn returned_in(r: &Rig, pool: &str) -> [u64; 10] {
    counters(r, pool).buckets.map(|bucket| bucket.returned)
}

fn reached(r: &Rig) -> [u64; 10] {
    reached_in(r, "main")
}

fn returned(r: &Rig) -> [u64; 10] {
    returned_in(r, "main")
}

fn retained(r: &Rig) -> bool {
    r.engine
        .recipes_for("desktop", "session")
        .iter()
        .any(|id| id == "main")
}

fn last_guard(r: &Rig) -> Option<Guard> {
    r.engine
        .recipes()
        .into_iter()
        .find(|view| view.agent_id == "main")
        .and_then(|view| view.last_guard)
}

/// A request on main, then the mod's idle report: the window main's next return is judged in starts at the request.
async fn idle_after_request(r: &mut Rig) -> i64 {
    r.observe("main").await;
    let start = r.now();
    r.state("main", "idle");
    start
}

/// Ticks once a minute and at every due probe until `minutes` after `start`, the lease renewed between them as an
/// attending mod renews it, so a recipe stays warm until a ceiling ends it.
async fn tick_to(r: &mut Rig, start: i64, minutes: i64) {
    let end = start + minutes * MINUTE;
    while r.now() < end {
        let now = r.now();
        let due = r
            .engine
            .sessions()
            .iter()
            .filter_map(|session| session.keepalive.next_probe_at)
            .filter(|due| *due > now)
            .min();
        let next = due.unwrap_or(end).min(now + MINUTE).min(end);
        r.advance_attended(next - now);
        r.step().await;
    }
}

/// `tick_to` with no heartbeat, so the lease lapses 90 seconds after the last one.
async fn tick_silent_to(r: &mut Rig, start: i64, minutes: i64) {
    let end = start + minutes * MINUTE;
    while r.now() < end {
        r.advance((end - r.now()).min(MINUTE));
        r.step().await;
    }
}

fn activity(r: &mut Rig, kind: &str) {
    let generation = r.status("main").request_generation;
    r.event(Event {
        event: "activity".into(),
        agent_id: Some("main".into()),
        kind: Some(kind.into()),
        request_generation: Some(generation),
        ..Default::default()
    })
    .unwrap();
}

/// A prompt a local command answered: the mod reports the prompt, then idle, and no request is sent.
fn local_command(r: &mut Rig) {
    activity(r, "prompt");
    r.state("main", "idle");
}

/// Moves the rig's key into a new pool `warm`, with the router reloaded so the next tick reads it there.
async fn move_key_to_warm(r: &Rig) {
    r.h.store
        .insert_pool(
            &phoebis_store::PoolRow {
                created_at: NOW,
                updated_at: NOW,
                ..phoebis_store::PoolRow::for_tests("warm")
            },
            "test",
        )
        .unwrap();
    let mut key = r.h.store.api_key("desktop").unwrap().unwrap();
    key.pool_id = "warm".into();
    r.h.store
        .update_api_key(&key, key.updated_at, "test")
        .unwrap();
    r.h.router.reload().await.unwrap();
}

/// A real request on main through the relay, naming `class` in the client's request class header.
async fn observe_classed(r: &mut Rig, class: &str, body: &str) {
    let mut request = r.request_with("main", body);
    relay_common::set_header(&mut request, "x-claude-code-request-class", class);
    let response = r.h.app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), 200);
    let id = relay_common::outcome(&response).request_id;
    let _ = response.into_body().collect().await.unwrap();
    r.pump(Some(&id)).await;
}

#[tokio::test]
async fn an_idle_window_raises_reached_at_each_bucket_it_crosses() {
    let mut r = rig(RigSpec::default()).await;
    let start = idle_after_request(&mut r).await;
    tick_to(&mut r, start, 20).await;
    assert_eq!(reached(&r), [1, 1, 1, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(returned(&r), [0; 10]);
}

#[tokio::test]
async fn a_return_on_the_same_prefix_raises_returned_in_its_bucket() {
    let mut r = rig(RigSpec::default()).await;
    let start = idle_after_request(&mut r).await;
    tick_to(&mut r, start, 12).await;
    let before = reached(&r);
    assert_eq!(before, [1, 1, 0, 0, 0, 0, 0, 0, 0, 0]);
    r.observe_with("main", &next_turn()).await;
    assert_eq!(last_guard(&r), Some(Guard::Rearm));
    assert_eq!(returned(&r), [0, 1, 0, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(
        reached(&r),
        before,
        "the ticks had raised the return's bucket already"
    );
    // The turn ends and main is idle again: the next window opens from the return.
    r.state("main", "idle");
    tick_to(&mut r, start, 13).await;
    assert_eq!(
        (reached(&r), returned(&r)),
        (
            [2, 1, 0, 0, 0, 0, 0, 0, 0, 0],
            [0, 1, 0, 0, 0, 0, 0, 0, 0, 0]
        ),
        "a new window a minute old"
    );
}

#[tokio::test]
async fn a_request_at_its_prompts_instant_starts_the_next_window() {
    let mut r = rig(RigSpec::default()).await;
    let start = idle_after_request(&mut r).await;
    tick_to(&mut r, start, 12).await;
    // The prompt report and its request read the same clock instant.
    activity(&mut r, "prompt");
    r.observe_with("main", &next_turn()).await;
    r.state("main", "idle");
    tick_to(&mut r, start, 13).await;
    assert_eq!(
        (reached(&r), returned(&r)),
        (
            [2, 1, 0, 0, 0, 0, 0, 0, 0, 0],
            [0, 1, 0, 0, 0, 0, 0, 0, 0, 0]
        ),
        "the next window starts at the request"
    );
}

#[tokio::test]
async fn a_request_on_an_unknown_main_moves_no_start() {
    let mut r = rig(RigSpec::default()).await;
    let start = idle_after_request(&mut r).await;
    tick_to(&mut r, start, 12).await;
    r.state("main", "unknown");
    r.advance_attended(MINUTE);
    r.observe_with("main", &next_turn()).await;
    assert_eq!(
        (reached(&r), returned(&r)),
        (
            [1, 1, 0, 0, 0, 0, 0, 0, 0, 0],
            [0, 1, 0, 0, 0, 0, 0, 0, 0, 0]
        ),
        "the request ends the window"
    );
    r.advance_attended(MINUTE);
    r.state("main", "idle");
    tick_to(&mut r, start, 15).await;
    assert_eq!(
        reached(&r),
        [1, 1, 0, 0, 0, 0, 0, 0, 0, 0],
        "the request moved no start, so no window opens from the old one"
    );
}

#[tokio::test]
async fn a_clock_that_steps_back_counts_no_bucket_twice() {
    let mut r = rig(RigSpec::default()).await;
    let start = idle_after_request(&mut r).await;
    tick_to(&mut r, start, 5).await;
    assert_eq!(reached(&r), [1, 1, 0, 0, 0, 0, 0, 0, 0, 0]);
    // The wall clock steps back across the 5-minute bound, then forward past it again.
    r.advance(-MINUTE);
    r.step().await;
    assert_eq!(reached(&r), [1, 1, 0, 0, 0, 0, 0, 0, 0, 0], "back");
    r.advance(2 * MINUTE);
    r.step().await;
    assert_eq!(reached(&r), [1, 1, 0, 0, 0, 0, 0, 0, 0, 0], "forward");
    r.advance(-2 * MINUTE);
    r.observe_with("main", &next_turn()).await;
    assert_eq!(
        (reached(&r), returned(&r)),
        (
            [1, 1, 0, 0, 0, 0, 0, 0, 0, 0],
            [0, 1, 0, 0, 0, 0, 0, 0, 0, 0]
        ),
        "a return read at 4 minutes counts in the bucket the window had reached"
    );
    assert_eq!(
        r.status("main").state,
        AgentState::Busy,
        "the engine still answers"
    );
}

#[tokio::test]
async fn a_return_between_ticks_raises_the_buckets_it_crossed() {
    let mut r = rig(RigSpec::default()).await;
    let start = idle_after_request(&mut r).await;
    tick_to(&mut r, start, 4).await;
    assert_eq!(reached(&r), [1, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    r.advance_attended(8 * MINUTE);
    r.observe_with("main", &next_turn()).await;
    assert_eq!(reached(&r), [1, 1, 0, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(returned(&r), [0, 1, 0, 0, 0, 0, 0, 0, 0, 0]);
}

#[tokio::test]
async fn a_request_on_a_new_prefix_a_session_end_and_a_lapse_count_as_no_return() {
    let counted = [1, 1, 0, 0, 0, 0, 0, 0, 0, 0];
    // A restart: the same system prompt on a fresh message chain.
    let mut r = rig(RigSpec::default()).await;
    let start = idle_after_request(&mut r).await;
    tick_to(&mut r, start, 12).await;
    r.observe_with("main", &fresh_chain()).await;
    assert_eq!(last_guard(&r), Some(Guard::Restart));
    tick_to(&mut r, start, 40).await;
    assert_eq!((reached(&r), returned(&r)), (counted, [0; 10]), "restart");
    // A foreign request: neither the chain nor the static prefix. It leaves the recipe and the window's start alone,
    // and no prompt moves the start, so the ticks after it open no window from the same start again. A local command
    // earlier in the window moved the start to its prompt, and that start is the one in force at the close.
    let mut r = rig(RigSpec::default()).await;
    let start = idle_after_request(&mut r).await;
    tick_to(&mut r, start, 3).await;
    local_command(&mut r);
    tick_to(&mut r, start, 12).await;
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
    assert_eq!(last_guard(&r), Some(Guard::Foreign));
    tick_to(&mut r, start, 40).await;
    assert_eq!((reached(&r), returned(&r)), (counted, [0; 10]), "foreign");
    // The session's end.
    let mut r = rig(RigSpec::default()).await;
    let start = idle_after_request(&mut r).await;
    tick_to(&mut r, start, 12).await;
    r.event(session_end("session")).unwrap();
    tick_silent_to(&mut r, start, 40).await;
    assert_eq!(
        (reached(&r), returned(&r)),
        (counted, [0; 10]),
        "session end"
    );
    // The same instance registers the session again, and its window starts at the revival, never at the old start.
    r.event(Event {
        event: "session_start".into(),
        ..Default::default()
    })
    .unwrap();
    r.state("main", "idle");
    tick_to(&mut r, start, 41).await;
    assert_eq!(
        (reached(&r), returned(&r)),
        ([2, 1, 0, 0, 0, 0, 0, 0, 0, 0], [0; 10]),
        "revived"
    );
    // The lease lapses at 13 minutes 30 seconds, still inside bucket 1, and the session goes silent. A same-instance
    // registration adopts it again at 40 minutes with the window's start unmoved, so no window opens until a prompt
    // moves it, and the next one counts from the prompt.
    let mut r = rig(RigSpec::default()).await;
    let start = idle_after_request(&mut r).await;
    tick_to(&mut r, start, 3).await;
    local_command(&mut r);
    tick_to(&mut r, start, 12).await;
    tick_silent_to(&mut r, start, 40).await;
    r.event(Event {
        event: "session_start".into(),
        ..Default::default()
    })
    .unwrap();
    tick_to(&mut r, start, 45).await;
    assert_eq!((reached(&r), returned(&r)), (counted, [0; 10]), "lapse");
    activity(&mut r, "prompt");
    r.state("main", "idle");
    tick_to(&mut r, start, 52).await;
    assert_eq!(
        (reached(&r), returned(&r)),
        ([2, 2, 0, 0, 0, 0, 0, 0, 0, 0], [0; 10]),
        "a window from the prompt at 45 minutes, 7 minutes old"
    );
}

#[tokio::test]
async fn a_released_main_closes_its_window() {
    let mut r = rig(RigSpec::default()).await;
    let start = idle_after_request(&mut r).await;
    tick_to(&mut r, start, 12).await;
    r.state("main", "released");
    let generation = r.status("main").request_generation;
    r.event(Event {
        event: "agent_start".into(),
        agent_id: Some("main".into()),
        request_generation: Some(generation),
        ..Default::default()
    })
    .unwrap();
    r.state("main", "idle");
    tick_to(&mut r, start, 20).await;
    assert_eq!(
        (reached(&r), returned(&r)),
        ([2, 2, 0, 0, 0, 0, 0, 0, 0, 0], [0; 10]),
        "the next window counts from the restart at 12 minutes"
    );
}

#[tokio::test]
async fn a_key_that_moves_pools_keeps_the_open_window_in_its_first_pool() {
    for keepalive in [true, false] {
        let mut r = rig(RigSpec {
            policy: KeepalivePolicy {
                enabled: keepalive,
                ..KeepalivePolicy::default()
            },
            ..RigSpec::default()
        })
        .await;
        let start = idle_after_request(&mut r).await;
        tick_to(&mut r, start, 6).await;
        move_key_to_warm(&r).await;
        tick_to(&mut r, start, 20).await;
        assert_eq!(
            (reached_in(&r, "main"), reached_in(&r, "warm")),
            ([1, 1, 1, 0, 0, 0, 0, 0, 0, 0], [0; 10]),
            "the open window, keepalive {keepalive}"
        );
        activity(&mut r, "presence");
        tick_to(&mut r, start, 21).await;
        assert_eq!(
            (reached_in(&r, "main"), reached_in(&r, "warm")),
            (
                [1, 1, 1, 0, 0, 0, 0, 0, 0, 0],
                [1, 0, 0, 0, 0, 0, 0, 0, 0, 0]
            ),
            "the next window, keepalive {keepalive}"
        );
    }
    // Keepalive on, with the recipe the idle ceiling discarded before the move: nothing refreshes the agent's pool any
    // more, and the next window still finds the key's new one.
    let mut r = rig(RigSpec {
        policy: with_idle_window(600),
        ..RigSpec::default()
    })
    .await;
    let start = idle_after_request(&mut r).await;
    tick_to(&mut r, start, 10).await;
    assert_eq!(
        (r.status("main").reason, retained(&r)),
        (Reason::IdleLimit, false)
    );
    move_key_to_warm(&r).await;
    tick_to(&mut r, start, 12).await;
    activity(&mut r, "presence");
    tick_to(&mut r, start, 13).await;
    assert_eq!(
        (reached_in(&r, "main"), reached_in(&r, "warm")),
        (
            [1, 1, 0, 0, 0, 0, 0, 0, 0, 0],
            [1, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        ),
        "the next window after the ceiling's discard"
    );
}

#[tokio::test]
async fn a_return_counts_on_a_pool_with_keepalive_off() {
    let mut r = rig(RigSpec {
        policy: KeepalivePolicy::default(),
        ..RigSpec::default()
    })
    .await;
    let start = idle_after_request(&mut r).await;
    assert!(!retained(&r), "keepalive off retains no recipe");
    tick_to(&mut r, start, 12).await;
    r.observe_with("main", &next_turn()).await;
    assert_eq!(
        (reached(&r), returned(&r)),
        (
            [1, 1, 0, 0, 0, 0, 0, 0, 0, 0],
            [0, 1, 0, 0, 0, 0, 0, 0, 0, 0]
        )
    );
}

#[tokio::test]
async fn a_changed_prefix_counts_as_no_return_before_and_after_the_recipes_disposal() {
    // The recipe is retained, and the request's boundaries lack its tail hash.
    let mut r = rig(RigSpec::default()).await;
    let start = idle_after_request(&mut r).await;
    tick_to(&mut r, start, 12).await;
    assert!(retained(&r));
    r.observe_with("main", &fresh_chain()).await;
    assert_eq!(
        (reached(&r), returned(&r)),
        ([1, 1, 0, 0, 0, 0, 0, 0, 0, 0], [0; 10]),
        "retained"
    );
    // The idle ceiling discarded the recipe at 10 minutes, so the guard has nothing to compare against and re-arms
    // on any chain, and the window still judges the request against the prefix it opened on.
    let mut r = rig(RigSpec {
        policy: with_idle_window(600),
        ..RigSpec::default()
    })
    .await;
    let start = idle_after_request(&mut r).await;
    tick_to(&mut r, start, 20).await;
    assert_eq!(
        (r.status("main").reason, retained(&r)),
        (Reason::IdleLimit, false)
    );
    r.observe_with("main", &fresh_chain()).await;
    assert_eq!(last_guard(&r), Some(Guard::Rearm));
    assert_eq!(
        (reached(&r), returned(&r)),
        ([1, 1, 1, 0, 0, 0, 0, 0, 0, 0], [0; 10]),
        "disposed"
    );
    // A refreshing class re-arms whatever its chain says, and says nothing about the prefix.
    let mut r = rig(RigSpec::default()).await;
    let start = idle_after_request(&mut r).await;
    tick_to(&mut r, start, 12).await;
    observe_classed(&mut r, "main", &fresh_chain()).await;
    assert_eq!(last_guard(&r), Some(Guard::Rearm));
    assert_eq!(
        (reached(&r), returned(&r)),
        ([1, 1, 0, 0, 0, 0, 0, 0, 0, 0], [0; 10]),
        "refreshing class"
    );
}

#[tokio::test]
async fn a_main_class_request_that_continues_the_prefix_counts_as_a_return() {
    let mut r = rig(RigSpec::default()).await;
    let start = idle_after_request(&mut r).await;
    tick_to(&mut r, start, 12).await;
    observe_classed(&mut r, "main", &next_turn()).await;
    assert_eq!(returned(&r), [0, 1, 0, 0, 0, 0, 0, 0, 0, 0]);
}

#[tokio::test]
async fn a_return_after_the_ceiling_discarded_the_recipe_still_counts() {
    let mut r = rig(plan_row(with_idle_window(600))).await;
    let start = idle_after_request(&mut r).await;
    tick_to(&mut r, start, 10).await;
    assert_eq!(
        (r.status("main").reason, retained(&r)),
        (Reason::IdleLimit, false),
        "the recipe ends at 10 minutes"
    );
    tick_to(&mut r, start, 65).await;
    assert_eq!(reached(&r), [1, 1, 1, 1, 1, 0, 0, 0, 0, 0]);
    r.observe_with("main", &next_turn()).await;
    assert_eq!(returned(&r), [0, 0, 0, 0, 1, 0, 0, 0, 0, 0]);
}

#[tokio::test]
async fn an_attended_session_without_a_recipe_still_reaches_buckets() {
    let mut r = rig(RigSpec::default()).await;
    // The session's key moves to a pool of another name before any request, so the counters can only find it
    // through the key.
    move_key_to_warm(&r).await;
    let start = r.now();
    r.state("main", "idle");
    tick_to(&mut r, start, 6).await;
    let pools: Vec<String> = r.engine.idle_returns().into_keys().collect();
    assert_eq!(pools, vec!["warm".to_string()]);
    assert_eq!(
        (reached_in(&r, "warm"), returned_in(&r, "warm")),
        ([1, 1, 0, 0, 0, 0, 0, 0, 0, 0], [0; 10])
    );
    // The session's first request: the window opened before any request, so it has no prefix to compare against.
    r.engine.observe(r.observed_with(
        "first",
        "main",
        None,
        Some("static"),
        &["tail"],
        BODY.as_bytes(),
        "claude-sonnet-4-5",
    ));
    assert_eq!(
        (reached_in(&r, "warm"), returned_in(&r, "warm")),
        ([1, 1, 0, 0, 0, 0, 0, 0, 0, 0], [0; 10]),
        "no return before the first request"
    );
}

#[tokio::test]
async fn a_prompt_report_before_the_request_keeps_the_episodes_age() {
    for tick_between in [true, false] {
        let mut r = rig(RigSpec::default()).await;
        let start = idle_after_request(&mut r).await;
        tick_to(&mut r, start, 20).await;
        activity(&mut r, "prompt");
        if tick_between {
            // The request lands at 30 minutes, in the next bucket, and is still judged at the prompt's age.
            r.advance_attended(10 * MINUTE);
            r.step().await;
        }
        r.observe_with("main", &next_turn()).await;
        // 20 minutes is 1,200 seconds, inside the bucket from 900 to 1,800.
        assert_eq!(
            (reached(&r), returned(&r)),
            (
                [1, 1, 1, 0, 0, 0, 0, 0, 0, 0],
                [0, 0, 1, 0, 0, 0, 0, 0, 0, 0]
            ),
            "a tick between: {tick_between}"
        );
    }
}

#[tokio::test]
async fn a_prompt_that_sends_no_request_lets_the_window_run_on() {
    let mut r = rig(RigSpec::default()).await;
    let start = idle_after_request(&mut r).await;
    tick_to(&mut r, start, 3).await;
    activity(&mut r, "prompt");
    tick_to(&mut r, start, 10).await;
    assert_eq!(
        reached(&r),
        [1, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        "the prompt holds the count for its request"
    );
    // A local command answered the prompt, and the mod reports idle with no request sent.
    r.state("main", "idle");
    tick_to(&mut r, start, 20).await;
    assert_eq!(
        reached(&r),
        [1, 1, 1, 0, 0, 0, 0, 0, 0, 0],
        "the same window goes on from its start"
    );
}

#[tokio::test]
async fn the_end_of_a_park_without_a_request_starts_a_new_window() {
    for mod_first in [false, true] {
        let mut r = rig(RigSpec::default()).await;
        r.observe("main").await;
        let start = r.now();
        let generation = r.status("main").request_generation;
        r.event(Event {
            event: "state".into(),
            agent_id: Some("main".into()),
            state: Some("parked".into()),
            park_kind: Some("background".into()),
            deadline: Some(start + 10 * MINUTE),
            request_generation: Some(generation),
            ..Default::default()
        })
        .unwrap();
        if mod_first {
            // The mod's heartbeat closes the command at its deadline and reports idle before the router ticks.
            tick_to(&mut r, start, 9).await;
            r.advance_attended(MINUTE);
            r.state("main", "idle");
        }
        tick_to(&mut r, start, 12).await;
        assert_eq!(
            (reached(&r), returned(&r)),
            ([2, 1, 0, 0, 0, 0, 0, 0, 0, 0], [0; 10]),
            "mod first: {mod_first}"
        );
    }
}

#[tokio::test]
async fn a_presence_report_starts_a_new_window() {
    let mut r = rig(RigSpec::default()).await;
    let start = idle_after_request(&mut r).await;
    tick_to(&mut r, start, 10).await;
    assert_eq!(reached(&r), [1, 1, 0, 0, 0, 0, 0, 0, 0, 0]);
    activity(&mut r, "presence");
    r.advance_attended(MINUTE);
    r.step().await;
    assert_eq!(reached(&r), [2, 1, 0, 0, 0, 0, 0, 0, 0, 0]);
}

/// One report of the second key's session, under its own instance and sequence.
fn laptop(r: &Rig, sequence: &mut u64, event: Event) {
    *sequence += 1;
    r.engine
        .apply_event(
            "laptop",
            r.generation("laptop"),
            Event {
                sequence: *sequence,
                session_id: "laptop".into(),
                instance_id: "laptop".into(),
                ..event
            },
        )
        .unwrap();
}

#[tokio::test]
async fn the_counters_are_per_pool() {
    let mut r = rig(RigSpec::default()).await;
    r.h.store
        .insert_pool(
            &phoebis_store::PoolRow {
                created_at: NOW,
                updated_at: NOW,
                ..phoebis_store::PoolRow::for_tests("warm")
            },
            "test",
        )
        .unwrap();
    let mut key = r.h.store.api_key("desktop").unwrap().unwrap();
    key.id = "laptop".into();
    key.label = "laptop".into();
    key.pool_id = "warm".into();
    key.secret_digest = r.h.store.api_key_digest(LAPTOP_SECRET).to_vec();
    key.prefix = phoebis_store::key_prefix(LAPTOP_SECRET);
    r.h.store.insert_api_key(&key, "test").unwrap();
    r.h.router.reload().await.unwrap();
    let start = idle_after_request(&mut r).await;
    tick_to(&mut r, start, 8).await;
    let mut sequence = 0;
    laptop(
        &r,
        &mut sequence,
        Event {
            event: "session_start".into(),
            ..Default::default()
        },
    );
    laptop(
        &r,
        &mut sequence,
        Event {
            event: "state".into(),
            agent_id: Some("main".into()),
            state: Some("idle".into()),
            request_generation: Some(0),
            ..Default::default()
        },
    );
    for _ in 0..8 {
        r.advance(MINUTE / 2);
        r.event(Event {
            event: "heartbeat".into(),
            ..Default::default()
        })
        .unwrap();
        laptop(
            &r,
            &mut sequence,
            Event {
                event: "heartbeat".into(),
                ..Default::default()
            },
        );
        r.step().await;
    }
    r.observe_with("main", &next_turn()).await;
    assert_eq!(
        (reached_in(&r, "main"), returned_in(&r, "main")),
        (
            [1, 1, 0, 0, 0, 0, 0, 0, 0, 0],
            [0, 1, 0, 0, 0, 0, 0, 0, 0, 0]
        ),
        "the rig's key, idle 12 minutes and back"
    );
    assert_eq!(
        (reached_in(&r, "warm"), returned_in(&r, "warm")),
        ([1, 0, 0, 0, 0, 0, 0, 0, 0, 0], [0; 10]),
        "the second key, idle 4 minutes"
    );
}

#[tokio::test]
async fn a_busy_main_a_subagent_and_a_silent_session_count_nothing() {
    // A busy report keeps the window open and counts nothing while it lasts.
    let mut r = rig(RigSpec::default()).await;
    let start = idle_after_request(&mut r).await;
    tick_to(&mut r, start, 3).await;
    r.state("main", "busy");
    tick_to(&mut r, start, 20).await;
    assert_eq!(reached(&r), [1, 0, 0, 0, 0, 0, 0, 0, 0, 0], "busy");
    r.state("main", "idle");
    tick_to(&mut r, start, 21).await;
    assert_eq!(
        reached(&r),
        [1, 1, 1, 0, 0, 0, 0, 0, 0, 0],
        "the same window goes on from its start"
    );
    // A subagent's park opens no window, and main never left busy.
    let mut r = rig(RigSpec::default()).await;
    let start = r.now();
    r.event(Event {
        event: "agent_start".into(),
        agent_id: Some("worker".into()),
        ..Default::default()
    })
    .unwrap();
    r.state("worker", "parked");
    tick_to(&mut r, start, 20).await;
    assert!(r.engine.idle_returns().is_empty(), "subagent");
    // A session whose lease lapsed before any tick opens none.
    let mut r = rig(RigSpec::default()).await;
    let start = r.now();
    r.state("main", "idle");
    r.advance(2 * MINUTE);
    tick_silent_to(&mut r, start, 20).await;
    assert!(r.engine.idle_returns().is_empty(), "silent");
}

#[test]
fn the_bucket_bounds_are_the_specs() {
    assert_eq!(
        IDLE_AGE_BOUNDS_SECONDS,
        [0, 300, 900, 1_800, 3_600, 7_200, 14_400, 28_800, 57_600, 115_200]
    );
    assert_eq!(
        [
            299_999,
            300_000,
            3_599_999,
            3_600_000,
            115_200_000,
            i64::MAX
        ]
        .map(idle_age_bucket),
        [0, 1, 3, 4, 9, 9]
    );
}

#[test]
fn continues_prefix_needs_the_anchors_tail_hash() {
    let anchor = Some(("static", "tail"));
    let holding = ["first".to_string(), "tail".to_string()];
    assert!(continues_prefix(anchor, &holding));
    assert!(!continues_prefix(anchor, &[]));
    assert!(!continues_prefix(
        anchor,
        &["first".to_string(), "static".to_string()]
    ));
    assert!(!continues_prefix(None, &holding));
}

/// The window opens at the idle report itself, so a person who prompts inside the same second still ends a window:
/// the first bucket counts the reach and the return.
#[tokio::test]
async fn an_idle_report_and_a_prompt_inside_the_same_second_count_the_first_bucket_and_the_return()
{
    let mut r = rig(RigSpec::default()).await;
    idle_after_request(&mut r).await;
    activity(&mut r, "prompt");
    r.observe_with("main", &next_turn()).await;
    assert_eq!(
        (reached(&r), returned(&r)),
        (
            [1, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            [1, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        ),
        "no tick ran between the idle report and the return"
    );
}

/// A presence report starts the window over on purpose, so the floor the closed window left does not refuse a start
/// that a backward clock step placed behind it. The park's end and a revival restart the window the same way.
#[tokio::test]
async fn a_presence_report_after_a_backward_clock_step_opens_a_window_that_counts_its_return() {
    let mut r = rig(RigSpec::default()).await;
    let start = idle_after_request(&mut r).await;
    tick_to(&mut r, start, 6).await;
    assert_eq!(reached(&r), [1, 1, 0, 0, 0, 0, 0, 0, 0, 0]);
    // The wall clock steps back to a second before the window began, and the person types.
    r.advance(start - 1_000 - r.now());
    activity(&mut r, "presence");
    tick_to(&mut r, start, 12).await;
    assert_eq!(
        reached(&r),
        [2, 2, 0, 0, 0, 0, 0, 0, 0, 0],
        "the window from the presence reaches its buckets"
    );
    r.observe_with("main", &next_turn()).await;
    assert_eq!(
        returned(&r),
        [0, 1, 0, 0, 0, 0, 0, 0, 0, 0],
        "and its return counts at the window's age"
    );
    assert_eq!(r.status("main").state, AgentState::Busy);
}

/// The session expiry counts from the later of the last request and the last report, and a session is held until the
/// whole expiry has passed: a millisecond short of it the session and its recipe stay, and at it both go.
#[tokio::test]
async fn a_session_is_held_a_millisecond_short_of_its_expiry_and_goes_at_it() {
    let mut r = rig(RigSpec {
        session_expiry: std::time::Duration::from_secs(3_600),
        ..RigSpec::default()
    })
    .await;
    r.observe("main").await;
    let retained = r.engine.stats().retained_bytes;
    assert!(retained > 0);
    r.advance(3_600_000 - 1);
    r.engine.expire();
    let stats = r.engine.stats();
    assert_eq!(
        (stats.sessions, stats.agents, stats.retained_bytes),
        (1, 1, retained),
        "a millisecond short of the expiry the session is held"
    );
    r.advance(1);
    r.engine.expire();
    let stats = r.engine.stats();
    assert_eq!(
        (stats.sessions, stats.agents, stats.retained_bytes),
        (0, 0, 0)
    );
}

/// A key the registry no longer holds takes its sessions, their agents and their recipes with it at the next tick.
#[tokio::test]
async fn a_deleted_key_leaves_no_session_agent_or_retained_byte() {
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    assert!(r.engine.stats().retained_bytes > 0);
    r.h.store.delete_api_key("desktop", "test").unwrap();
    r.h.router.reload().await.unwrap();
    r.step().await;
    let stats = r.engine.stats();
    assert_eq!(
        (stats.sessions, stats.agents, stats.retained_bytes),
        (0, 0, 0)
    );
}
