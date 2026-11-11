//! The idle-age counters: per pool and idle-age bucket, the idle windows of the main conversation that reached the
//! bucket's start and those whose next request continued the prefix the window opened on. The windows are counted at
//! every tick and at the request that ends one, whether or not a recipe is still retained. Beside them, the chain
//! watch: a refreshing request whose chain holds none of the boundaries of that prefix while its answer reads the
//! cache.
mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::*;
use fake_upstream::{install, Captured};
use phoebis_bus::{ClientFacts, Event as BusEvent, Outcome, Usage};
use phoebis_keepalive::{
    continues_prefix, idle_age_bucket, AgentState, Event, Guard, IdleReturns, KeepalivePolicy,
    KeepaliveTiming, PrefixAnchor, Reason,
};

const LAPTOP_SECRET: &str = "phoebis-laptop-key";
const DAY: i64 = 86_400_000;

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

/// Main's request on two messages, so the prefix a window opens on holds two boundaries.
fn two_messages() -> String {
    BODY.replace(
        r#"{"role":"user","content":"hello"}]"#,
        r#"{"role":"user","content":"hello"},{"role":"user","content":"and then"}]"#,
    )
}

/// The next turn after `two_messages`, its last message re-sent whole.
fn after_two_messages() -> String {
    BODY.replace(
        r#"{"role":"user","content":"hello"}]"#,
        r#"{"role":"user","content":"hello"},{"role":"user","content":"and then"},{"role":"assistant","content":"hi"},{"role":"user","content":"more"}]"#,
    )
}

/// The next turn after `two_messages` with its last message re-sent as a text block carrying a member beyond `type`,
/// `text` and `cache_control`, a shape the canonical form keeps apart from the string: the chain holds the boundary
/// before the last alone.
fn reshaped_after_two_messages() -> String {
    BODY.replace(
        r#"{"role":"user","content":"hello"}]"#,
        r#"{"role":"user","content":"hello"},{"role":"user","content":[{"type":"text","text":"and then","citations":[]}]},{"role":"assistant","content":"hi"},{"role":"user","content":"more"}]"#,
    )
}

fn with_idle_window(seconds: u64) -> KeepalivePolicy {
    KeepalivePolicy {
        enabled: true,
        max_idle_seconds: seconds,
        ..KeepalivePolicy::default()
    }
}

/// One pool's days summed bucket by bucket.
fn summed(days: &BTreeMap<i64, IdleReturns>) -> IdleReturns {
    let mut total = IdleReturns::default();
    for counters in days.values() {
        for (sum, bucket) in total.buckets.iter_mut().zip(&counters.buckets) {
            sum.reached += bucket.reached;
            sum.returned += bucket.returned;
        }
    }
    total
}

/// The pool's counters over every day the engine holds.
fn counters(r: &Rig, pool: &str) -> IdleReturns {
    r.engine
        .idle_returns()
        .get(pool)
        .map(summed)
        .unwrap_or_default()
}

/// Each `reached` and `returned` of `pool` by the UTC day the engine keys them under.
fn by_day(r: &Rig, pool: &str) -> BTreeMap<i64, ([u64; 10], [u64; 10])> {
    r.engine
        .idle_returns()
        .get(pool)
        .map(|days| {
            days.iter()
                .map(|(day, counters)| {
                    (
                        *day,
                        (
                            counters.buckets.map(|bucket| bucket.reached),
                            counters.buckets.map(|bucket| bucket.returned),
                        ),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The rig's timing with the idle-return retention set to `days`.
fn retained_for(days: u32) -> RigSpec {
    RigSpec {
        timing: KeepaliveTiming {
            idle_return_retention_days: days,
            ..RigSpec::default().timing
        },
        ..RigSpec::default()
    }
}

/// The clock to `at` with the lease renewed on the way and no tick between, so the next tick or request counts at `at`.
fn attend_to(r: &mut Rig, at: i64) {
    let now = r.now();
    r.advance_attended(at - now);
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
    idle_after_request_with(r, BODY).await
}

async fn idle_after_request_with(r: &mut Rig, body: &str) -> i64 {
    r.observe_with("main", body).await;
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

/// An observation on main that names no request class, as a count's or a probe's observation carries none,
/// so the guard reads the request's own chain against the armed recipe.
fn observe_unclassed(r: &Rig, body: &str) {
    let value: serde_json::Value = serde_json::from_str(body).unwrap();
    let chains = phoebis_relay::Chains::read(&value).unwrap();
    let boundaries: Vec<&str> = chains
        .message_boundaries
        .iter()
        .map(String::as_str)
        .collect();
    let model = value["model"].as_str().unwrap();
    r.engine.observe(r.observed_with(
        "unclassed",
        "main",
        None,
        chains.static_hash(),
        &boundaries,
        body.as_bytes(),
        model,
    ));
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

/// A window returns on the rig's first UTC day, and the next, open from that return, ticks into its ninth bucket and
/// returns on the day after: each day holds its own buckets, and the save writes a row per pool, day and bucket.
#[tokio::test]
async fn two_windows_closing_on_two_utc_days_write_two_rows_per_bucket() {
    let mut r = rig(RigSpec::default()).await;
    let start = idle_after_request(&mut r).await;
    tick_to(&mut r, start, 12).await;
    r.observe_with("main", &next_turn()).await;
    r.state("main", "idle");
    attend_to(&mut r, DAY + 12 * MINUTE);
    r.step().await;
    r.observe_with("main", &next_turn()).await;
    let first = (
        [1, 1, 0, 0, 0, 0, 0, 0, 0, 0],
        [0, 1, 0, 0, 0, 0, 0, 0, 0, 0],
    );
    let second = (
        [1, 1, 1, 1, 1, 1, 1, 1, 1, 0],
        [0, 0, 0, 0, 0, 0, 0, 0, 1, 0],
    );
    assert_eq!(
        by_day(&r, "main"),
        BTreeMap::from([(0, first), (1, second)])
    );
    r.engine.flush_persisted().await;
    let rows: Vec<(i64, u32, u64, u64)> =
        r.h.store
            .load_keepalive()
            .unwrap()
            .2
            .into_iter()
            .map(|row| (row.day, row.bucket, row.reached, row.returned))
            .collect();
    let expected: Vec<(i64, u32, u64, u64)> = [(0, first), (1, second)]
        .into_iter()
        .flat_map(|(day, (reached, returned))| {
            (0..10).map(move |bucket| (day, bucket as u32, reached[bucket], returned[bucket]))
        })
        .collect();
    assert_eq!(rows, expected, "one row per pool, day and bucket");
}

/// The ticks count a window's first two buckets on the rig's first UTC day, and the request that ends it on the next
/// raises the rest: the buckets it raised and its return count on the closing day.
#[tokio::test]
async fn a_window_whose_closing_request_raised_its_bucket_the_next_day_counts_that_reach_and_its_return_there(
) {
    let mut r = rig(RigSpec::default()).await;
    let start = idle_after_request(&mut r).await;
    tick_to(&mut r, start, 12).await;
    attend_to(&mut r, DAY + MINUTE);
    r.observe_with("main", &next_turn()).await;
    assert_eq!(
        by_day(&r, "main"),
        BTreeMap::from([
            (0, ([1, 1, 0, 0, 0, 0, 0, 0, 0, 0], [0; 10])),
            (
                1,
                (
                    [0, 0, 1, 1, 1, 1, 1, 1, 1, 0],
                    [0, 0, 0, 0, 0, 0, 0, 0, 1, 0]
                )
            ),
        ])
    );
}

/// A tick a minute before midnight counts the window's ninth bucket, and the return a minute after midnight lands in
/// it: the return counts on the reach's day, so no day answers a return without its reach.
#[tokio::test]
async fn a_return_after_midnight_in_a_bucket_a_tick_reached_before_it_counts_on_the_reachs_day() {
    let mut r = rig(RigSpec::default()).await;
    idle_after_request(&mut r).await;
    attend_to(&mut r, DAY - MINUTE);
    r.step().await;
    attend_to(&mut r, DAY + MINUTE);
    r.observe_with("main", &next_turn()).await;
    let days = by_day(&r, "main");
    assert_eq!(
        days.get(&0),
        Some(&(
            [1, 1, 1, 1, 1, 1, 1, 1, 1, 0],
            [0, 0, 0, 0, 0, 0, 0, 0, 1, 0]
        )),
        "the reach's day"
    );
    assert_eq!(
        days.get(&1).map_or([0; 10], |(_, returned)| *returned),
        [0; 10],
        "the closing day"
    );
}

/// Under a retention of two days, the tick on the third day drops the first day's counters from the engine and,
/// through the next save, from the registry: a range over the days kept answers none of the first, and a load reads
/// the two days kept.
#[tokio::test]
async fn a_day_past_the_retention_leaves_at_the_roll_and_stays_gone_after_a_save_and_a_load() {
    let mut r = rig(retained_for(2)).await;
    let start = idle_after_request(&mut r).await;
    tick_to(&mut r, start, 12).await;
    r.observe_with("main", &next_turn()).await;
    r.state("main", "idle");
    attend_to(&mut r, DAY + 12 * MINUTE);
    r.step().await;
    r.engine.flush_persisted().await;
    assert_eq!(
        by_day(&r, "main").into_keys().collect::<Vec<_>>(),
        vec![0, 1],
        "both days before the roll"
    );
    attend_to(&mut r, 2 * DAY + 12 * MINUTE);
    r.step().await;
    assert_eq!(
        by_day(&r, "main").into_keys().collect::<Vec<_>>(),
        vec![1, 2],
        "the first day leaves at the roll"
    );
    let over = &r.engine.idle_returns_over(r.now(), 30)["main"];
    assert_eq!(
        (
            over.buckets.map(|bucket| bucket.reached),
            over.buckets.map(|bucket| bucket.returned)
        ),
        ([1; 10], [0; 10]),
        "the second window alone, its last bucket on the third day"
    );
    r.engine.flush_persisted().await;
    let saved: BTreeSet<i64> =
        r.h.store
            .load_keepalive()
            .unwrap()
            .2
            .into_iter()
            .map(|row| row.day)
            .collect();
    assert_eq!(
        saved,
        BTreeSet::from([1, 2]),
        "the save took the first day's rows"
    );
    let second = r.restart().await;
    second.load_persisted().await;
    assert_eq!(
        second.idle_returns(),
        r.engine.idle_returns(),
        "the load reads the days kept"
    );
}

/// A guard, which holds on any code without a roll: under a retention of 0 the tick on the third day drops nothing,
/// and the save and the load keep the first day's counts.
#[tokio::test]
async fn a_retention_of_0_keeps_every_day() {
    let mut r = rig(retained_for(0)).await;
    let start = idle_after_request(&mut r).await;
    tick_to(&mut r, start, 12).await;
    r.observe_with("main", &next_turn()).await;
    attend_to(&mut r, 2 * DAY + 12 * MINUTE);
    r.step().await;
    let counted = (
        [1, 1, 0, 0, 0, 0, 0, 0, 0, 0],
        [0, 1, 0, 0, 0, 0, 0, 0, 0, 0],
    );
    assert_eq!((reached(&r), returned(&r)), counted, "the roll");
    r.engine.flush_persisted().await;
    let second = r.restart().await;
    second.load_persisted().await;
    let loaded = summed(&second.idle_returns()["main"]);
    assert_eq!(
        (
            loaded.buckets.map(|bucket| bucket.reached),
            loaded.buckets.map(|bucket| bucket.returned)
        ),
        counted,
        "the save and the load"
    );
}

/// Under a retention of two days, a window reaches its last bucket on the rig's second UTC day and stays in it until it
/// returns on the fourth, after the roll took that day: the return leaves with its reach and counts nowhere, so no day
/// answers a return above its reach.
#[tokio::test]
async fn a_return_whose_reach_day_left_at_the_roll_counts_nothing() {
    let mut r = rig(retained_for(2)).await;
    let start = idle_after_request(&mut r).await;
    attend_to(&mut r, start + 32 * 60 * MINUTE + MINUTE);
    r.step().await;
    assert_eq!(
        by_day(&r, "main").into_keys().collect::<Vec<_>>(),
        vec![1],
        "every bucket reached on the second day"
    );
    attend_to(&mut r, 3 * DAY + MINUTE);
    r.observe_with("main", &next_turn()).await;
    assert_eq!(by_day(&r, "main").get(&1), None, "the reach's day left");
    assert_eq!(returned(&r), [0; 10], "no return counted anywhere");
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
    observe_unclassed(&r, &fresh_chain());
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
    // A refreshing class re-arms whatever its chain says, and says nothing about the prefix: the class decides the
    // re-arm, so the recipe's schedule runs from this request, and the chain decides the return, so a request whose
    // boundaries lack the tail the window opened on counts none in the buckets the window reached.
    let mut r = rig(RigSpec::default()).await;
    let start = idle_after_request(&mut r).await;
    let before = reached(&r);
    tick_to(&mut r, start, 12).await;
    observe_classed(&mut r, "main", &fresh_chain()).await;
    assert_eq!(last_guard(&r), Some(Guard::Rearm));
    assert_eq!(
        (retained(&r), r.status("main").next_probe_at),
        (true, Some(r.now() + 270_000)),
        "the class re-armed the recipe on the fresh chain"
    );
    assert_eq!(
        (before, reached(&r), returned(&r)),
        ([0; 10], [1, 1, 0, 0, 0, 0, 0, 0, 0, 0], [0; 10]),
        "refreshing class"
    );
}

/// The window keeps the pair apart from the recipe, so the boundary before the last counts a return while the recipe
/// is retained, after the idle ceiling discarded it, and under a refreshing class, where the class decides the re-arm
/// and the chain the return.
#[tokio::test]
async fn the_boundary_before_the_last_counts_a_return_before_and_after_the_recipes_disposal() {
    let mut r = rig(RigSpec::default()).await;
    let start = idle_after_request_with(&mut r, &two_messages()).await;
    tick_to(&mut r, start, 12).await;
    assert!(retained(&r));
    r.observe_with("main", &reshaped_after_two_messages()).await;
    assert_eq!(
        (reached(&r), returned(&r)),
        (
            [1, 1, 0, 0, 0, 0, 0, 0, 0, 0],
            [0, 1, 0, 0, 0, 0, 0, 0, 0, 0]
        ),
        "retained"
    );
    let mut r = rig(RigSpec {
        policy: with_idle_window(600),
        ..RigSpec::default()
    })
    .await;
    let start = idle_after_request_with(&mut r, &two_messages()).await;
    tick_to(&mut r, start, 20).await;
    assert_eq!(
        (r.status("main").reason, retained(&r)),
        (Reason::IdleLimit, false)
    );
    r.observe_with("main", &reshaped_after_two_messages()).await;
    assert_eq!(
        (reached(&r), returned(&r)),
        (
            [1, 1, 1, 0, 0, 0, 0, 0, 0, 0],
            [0, 0, 1, 0, 0, 0, 0, 0, 0, 0]
        ),
        "disposed"
    );
    let mut r = rig(RigSpec::default()).await;
    let start = idle_after_request_with(&mut r, &two_messages()).await;
    tick_to(&mut r, start, 12).await;
    observe_classed(&mut r, "main", &reshaped_after_two_messages()).await;
    assert_eq!(last_guard(&r), Some(Guard::Rearm));
    assert_eq!(
        returned(&r),
        [0, 1, 0, 0, 0, 0, 0, 0, 0, 0],
        "refreshing class"
    );
}

#[tokio::test]
async fn a_return_holding_the_last_of_two_boundaries_counts() {
    let mut r = rig(RigSpec::default()).await;
    let start = idle_after_request_with(&mut r, &two_messages()).await;
    tick_to(&mut r, start, 12).await;
    r.observe_with("main", &after_two_messages()).await;
    assert_eq!(returned(&r), [0, 1, 0, 0, 0, 0, 0, 0, 0, 0]);
}

#[tokio::test]
async fn a_request_holding_neither_of_the_last_two_boundaries_counts_no_return() {
    let mut r = rig(RigSpec::default()).await;
    let start = idle_after_request_with(&mut r, &two_messages()).await;
    tick_to(&mut r, start, 12).await;
    r.observe_with("main", &fresh_chain()).await;
    assert_eq!(
        (reached(&r), returned(&r)),
        ([1, 1, 0, 0, 0, 0, 0, 0, 0, 0], [0; 10])
    );
}

/// A one-message chain leaves no boundary before its last, so a request with no chain counts no return on the absent
/// one.
#[tokio::test]
async fn a_request_with_no_chain_counts_no_return_on_a_one_message_anchor() {
    let mut r = rig(RigSpec::default()).await;
    let start = idle_after_request(&mut r).await;
    tick_to(&mut r, start, 12).await;
    r.engine.observe(r.observed_with(
        "chainless",
        "main",
        None,
        None,
        &[],
        BODY.as_bytes(),
        "claude-sonnet-4-5",
    ));
    assert_eq!(returned(&r), [0; 10], "no chain");
}

/// A one-message request's anchor holds no boundary before its last, whatever the anchor before it held, so a later
/// request that holds the earlier chain's boundary before its last alone counts no return.
#[tokio::test]
async fn a_one_message_request_keeps_no_boundary_before_the_last_from_an_earlier_chain() {
    let mut r = rig(RigSpec::default()).await;
    r.observe_with("main", &two_messages()).await;
    let start = idle_after_request_with(&mut r, &fresh_chain()).await;
    tick_to(&mut r, start, 12).await;
    // `BODY`'s one message is the first of `two_messages`, so its one boundary is the older of that request's two.
    r.observe("main").await;
    assert_eq!(
        (reached(&r), returned(&r)),
        ([1, 1, 0, 0, 0, 0, 0, 0, 0, 0], [0; 10])
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

const VERSION: &str = "2.1.305";
const WATCH_LINE: &str = "a refreshing request's chain holds none of the anchor's boundaries";

/// A conversation of its own under the rig's static prefix, whose chain holds no boundary of another.
fn conversation(n: u32) -> String {
    BODY.replace("hello", &format!("conversation {n}"))
}

/// A main request handed to the engine as the relay publishes one: the chain the relay reads off `body`, the request
/// class, and the client's other hint facts as `facts` sets them.
fn arrive(r: &Rig, id: &str, class: &str, body: &str, facts: impl FnOnce(&mut ClientFacts)) {
    let value: serde_json::Value = serde_json::from_str(body).unwrap();
    let chains = phoebis_relay::Chains::read(&value).unwrap();
    let boundaries: Vec<&str> = chains
        .message_boundaries
        .iter()
        .map(String::as_str)
        .collect();
    let BusEvent::RequestObserved(mut observed) = r.observed_with(
        id,
        "main",
        Some(class),
        chains.static_hash(),
        &boundaries,
        body.as_bytes(),
        value["model"].as_str().unwrap(),
    ) else {
        unreachable!("observed_with builds an observation")
    };
    observed.marked = chains.marked.clone();
    facts(&mut observed.client);
    r.engine.observe(BusEvent::RequestObserved(observed));
}

/// The completion of `id` under the client `version`, with the prompt `usage` reports.
fn answer(r: &Rig, id: &str, version: &str, usage: Usage) {
    let BusEvent::RequestComplete(mut complete) = r.complete(id, Outcome::Succeeded, Some(usage))
    else {
        unreachable!("complete builds a completion")
    };
    complete.client_version = Some(version.into());
    r.engine.observe(BusEvent::RequestComplete(complete));
}

/// An answer of 200 uncached tokens and `read` cache reads.
fn reading(read: u64) -> Usage {
    Usage {
        input_tokens: Some(200),
        cache_read_tokens: Some(read),
        ..Usage::default()
    }
}

/// Main's first request on two messages, answered with a prompt of 2,000 tokens: 500 uncached, 1,000 read and 500
/// written, the prompt the watch reads the next completion against.
fn first_turn(r: &Rig) {
    arrive(r, "first", "main", &two_messages(), |_| {});
    answer(
        r,
        "first",
        VERSION,
        Usage {
            input_tokens: Some(500),
            cache_read_tokens: Some(1_000),
            cache_write_5m_tokens: Some(500),
            ..Usage::default()
        },
    );
}

/// The watch's series as the scrape renders them.
fn contradictions(r: &Rig) -> Vec<String> {
    r.metrics
        .render(&phoebis_ledger::RoutingGauges::default(), &[], 0)
        .lines()
        .filter(|line| line.starts_with("phoebis_keepalive_chain_contradictions_total{"))
        .map(String::from)
        .collect()
}

fn series(version: &str, count: u64) -> String {
    format!(r#"phoebis_keepalive_chain_contradictions_total{{client_version="{version}"}} {count}"#)
}

#[tokio::test]
async fn a_refreshing_request_whose_chain_holds_neither_boundary_and_whose_answer_reads_the_cache_is_counted_and_logged(
) {
    let r = rig(RigSpec::default()).await;
    let captured = Captured::default();
    let _log = install(&captured);
    first_turn(&r);
    r.advance(MINUTE);
    arrive(&r, "moved", "main", &conversation(1), |_| {});
    answer(&r, "moved", VERSION, reading(1_800));
    assert_eq!(contradictions(&r), [series(VERSION, 1)]);
    let text = captured.text();
    assert_eq!(text.matches(WATCH_LINE).count(), 1, "{text}");
    for field in [
        "client_version=2.1.305",
        "session=session",
        "cache_read_tokens=1800",
        "previous_prompt_tokens=2000",
    ] {
        assert!(text.contains(field), "missing {field} in {text}");
    }
}

#[tokio::test]
async fn a_second_contradiction_within_the_hour_under_the_same_version_is_counted_without_a_line() {
    let r = rig(RigSpec::default()).await;
    let captured = Captured::default();
    let _log = install(&captured);
    first_turn(&r);
    r.advance(MINUTE);
    arrive(&r, "moved", "main", &conversation(1), |_| {});
    answer(&r, "moved", VERSION, reading(1_800));
    r.advance(30 * MINUTE);
    arrive(&r, "moved again", "main", &conversation(2), |_| {});
    answer(&r, "moved again", VERSION, reading(1_800));
    assert_eq!(contradictions(&r), [series(VERSION, 2)]);
    let text = captured.text();
    assert_eq!(text.matches(WATCH_LINE).count(), 1, "{text}");
}

#[tokio::test]
async fn another_client_version_logs_its_own_contradiction() {
    let r = rig(RigSpec::default()).await;
    let captured = Captured::default();
    let _log = install(&captured);
    first_turn(&r);
    r.advance(MINUTE);
    arrive(&r, "moved", "main", &conversation(1), |_| {});
    answer(&r, "moved", VERSION, reading(1_800));
    r.advance(MINUTE);
    arrive(&r, "moved again", "main", &conversation(2), |_| {});
    answer(&r, "moved again", "2.1.310", reading(1_800));
    assert_eq!(
        contradictions(&r),
        [series(VERSION, 1), series("2.1.310", 1)]
    );
    let text = captured.text();
    assert_eq!(text.matches(WATCH_LINE).count(), 2, "{text}");
    assert!(text.contains("client_version=2.1.310"), "{text}");
}

/// The mark is taken at the arrival before the engine's refusals, so a request whose recipe the engine refuses still
/// reaches the watch at its completion.
#[tokio::test]
async fn a_marked_request_whose_recipe_the_engine_refuses_still_reaches_the_watch() {
    let r = rig(RigSpec::default()).await;
    first_turn(&r);
    r.advance(MINUTE);
    let forced = conversation(1).replace(
        r#""max_tokens":1024,"#,
        r#""max_tokens":1024,"tool_choice":{"type":"tool","name":"x"},"#,
    );
    arrive(&r, "forced", "main", &forced, |_| {});
    assert_eq!(r.status("main").reason, Reason::ForcedTool);
    answer(&r, "forced", VERSION, reading(1_800));
    assert_eq!(contradictions(&r), [series(VERSION, 1)]);
}

#[tokio::test]
async fn a_compactions_first_request_logs_no_contradiction() {
    let r = rig(RigSpec::default()).await;
    let captured = Captured::default();
    let _log = install(&captured);
    first_turn(&r);
    r.advance(MINUTE);
    arrive(&r, "compaction", "compaction", &conversation(1), |facts| {
        facts.compaction = Some("true".into());
    });
    answer(&r, "compaction", VERSION, reading(1_800));
    arrive(&r, "compacted", "main", &conversation(2), |facts| {
        facts.context_compacted = Some("true".into());
    });
    answer(&r, "compacted", VERSION, reading(1_900));
    assert_eq!(contradictions(&r), Vec::<String>::new());
    assert!(!captured.text().contains(WATCH_LINE), "{}", captured.text());
}

#[tokio::test]
async fn a_completion_under_nine_tenths_of_the_previous_prompt_logs_no_contradiction() {
    let r = rig(RigSpec::default()).await;
    let captured = Captured::default();
    let _log = install(&captured);
    first_turn(&r);
    r.advance(MINUTE);
    arrive(&r, "moved", "main", &conversation(1), |_| {});
    answer(&r, "moved", VERSION, reading(1_799));
    assert_eq!(contradictions(&r), Vec::<String>::new());
    assert!(!captured.text().contains(WATCH_LINE), "{}", captured.text());
}

#[tokio::test]
async fn the_contradiction_line_names_no_hash() {
    let r = rig(RigSpec::default()).await;
    let captured = Captured::default();
    let _log = install(&captured);
    first_turn(&r);
    r.advance(MINUTE);
    arrive(&r, "moved", "main", &conversation(1), |_| {});
    answer(&r, "moved", VERSION, reading(1_800));
    let text = captured.text();
    assert_eq!(
        text.matches(WATCH_LINE).count(),
        1,
        "the line the claim reads is printed: {text}"
    );
    let longest_hex_run = text
        .split(|c: char| !c.is_ascii_hexdigit())
        .map(str::len)
        .max()
        .unwrap_or(0);
    assert!(longest_hex_run < 64, "a 64-hex run in the log: {text}");
}

/// Main's turn on five messages whose first, third and fourth carry the cache marker, so the deepest marked
/// boundary before its last two is the third's. With `BODY`'s system marker the request holds four breakpoints,
/// the most the upstream takes.
fn marked_turns() -> String {
    BODY.replace(
        r#"{"role":"user","content":"hello"}]"#,
        r#"{"role":"user","content":[{"type":"text","text":"hello","cache_control":{"type":"ephemeral"}}]},{"role":"assistant","content":"hi"},{"role":"user","content":[{"type":"text","text":"and then","cache_control":{"type":"ephemeral"}}]},{"role":"assistant","content":[{"type":"text","text":"working","cache_control":{"type":"ephemeral"}}]},{"role":"user","content":"steer"}]"#,
    )
}

/// The request after `marked_turns` was interrupted: the first three messages re-sent and the interrupt in place of
/// the last two, so the chain holds the third's boundary and neither of the last two.
fn after_the_interrupt() -> String {
    BODY.replace(
        r#"{"role":"user","content":"hello"}]"#,
        r#"{"role":"user","content":"hello"},{"role":"assistant","content":"hi"},{"role":"user","content":"and then"},{"role":"user","content":"[Request interrupted by user]"}]"#,
    )
}

/// A request that rewinds past the third message of `marked_turns`: its chain holds the first two boundaries alone,
/// the first of them marked.
fn past_the_marked_boundary() -> String {
    BODY.replace(
        r#"{"role":"user","content":"hello"}]"#,
        r#"{"role":"user","content":"hello"},{"role":"assistant","content":"hi"},{"role":"user","content":"something else"}]"#,
    )
}

/// The upstream reads the cache through the deepest marked boundary, so an interrupted turn's next request, which drops
/// the last two messages, continues the prefix. The anchor's request goes through the relay, so the marked positions
/// are the ones it publishes. A request that rewinds past that boundary holds none of the anchor's boundaries, since
/// the anchor keeps the deepest marked one and not the shallower one the two requests share.
#[tokio::test]
async fn an_interrupted_turns_next_request_counts_a_return_and_marks_nothing_while_a_rewind_past_the_marked_boundary_marks(
) {
    let mut r = rig(RigSpec::default()).await;
    let start = idle_after_request_with(&mut r, &marked_turns()).await;
    tick_to(&mut r, start, 12).await;
    observe_classed(&mut r, "main", &after_the_interrupt()).await;
    assert_eq!(
        (returned(&r), contradictions(&r)),
        ([0, 1, 0, 0, 0, 0, 0, 0, 0, 0], Vec::<String>::new()),
        "after the interrupt"
    );
    let r = rig(RigSpec::default()).await;
    arrive(&r, "first", "main", &marked_turns(), |_| {});
    answer(&r, "first", VERSION, reading(1_800));
    r.advance(MINUTE);
    arrive(&r, "rewound", "main", &past_the_marked_boundary(), |_| {});
    answer(&r, "rewound", VERSION, reading(1_800));
    assert_eq!(
        contradictions(&r),
        [series(VERSION, 1)],
        "past the marked boundary"
    );
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
            }
            .try_into()
            .unwrap(),
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
fn continues_prefix_holds_either_of_the_anchors_two_boundaries() {
    let chain =
        |boundaries: &[&str]| -> Vec<String> { boundaries.iter().map(|b| b.to_string()).collect() };
    let pair = PrefixAnchor {
        static_hash: "static".into(),
        last: "tail".into(),
        before_last: Some("before".into()),
        marked: None,
    };
    let one = PrefixAnchor {
        before_last: None,
        ..pair.clone()
    };
    assert!(continues_prefix(Some(&pair), &chain(&["first", "tail"])));
    assert!(continues_prefix(
        Some(&pair),
        &chain(&["first", "before", "reshaped"])
    ));
    assert!(!continues_prefix(Some(&pair), &[]));
    assert!(!continues_prefix(Some(&pair), &chain(&["first", "static"])));
    assert!(continues_prefix(Some(&one), &chain(&["first", "tail"])));
    assert!(!continues_prefix(Some(&one), &chain(&["first", "before"])));
    assert!(!continues_prefix(None, &chain(&["before", "tail"])));
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

/// A park report opens the window as an idle report does, so a prompt inside the same second ends that window too.
#[tokio::test]
async fn a_park_report_and_a_prompt_inside_the_same_second_count_the_first_bucket_and_the_return() {
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    let start = r.now();
    let generation = r.status("main").request_generation;
    r.event(Event {
        event: "state".into(),
        agent_id: Some("main".into()),
        state: Some("parked".into()),
        park_kind: Some("background".into()),
        deadline: Some(start + 60 * MINUTE),
        request_generation: Some(generation),
        ..Default::default()
    })
    .unwrap();
    activity(&mut r, "prompt");
    r.observe_with("main", &next_turn()).await;
    assert_eq!(
        (reached(&r), returned(&r)),
        (
            [1, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            [1, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        ),
        "no tick ran between the park report and the return"
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
