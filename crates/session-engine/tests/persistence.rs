//! The engine's state across a restart: a flush writes the sessions, the agents with their recipes and the idle-age
//! counters to the registry, and a second engine over the same registry reads them back silent, within the limits it
//! starts under. The shutdown quiesces before its save, and the saves never overlap.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use fake_upstream::{install_at, Captured, Respond};
use phoebis_bus::{Event as BusEvent, Outcome, Usage};
use phoebis_ledger::{hour_of, read_rollups, KeepaliveDelta, Record, RollupSelect};
use phoebis_session_engine::{
    CompanionState, Engine, KeepaliveLimits, KeepalivePolicy, KeepaliveTiming, Lifecycle, ParkKind,
    Reason, SessionCaps, LEASE_MS,
};
use phoebis_store::engine_state::BodyWrite;
use phoebis_store::{
    CompanionSessionRow, EngineBatch, FactsKind, IdleReturnsRow, KeepaliveAgentRow,
};
use serde_json::{json, Value};

const DAY: i64 = 86_400_000;

fn on_disk() -> RigSpec {
    RigSpec {
        on_disk: true,
        ..RigSpec::default()
    }
}

/// The rig's registry file, opened beside the store as an operator's `sqlite3` would open it. Its own busy wait is
/// short, so the test never sits behind the store's.
fn registry(r: &Rig) -> rusqlite::Connection {
    let conn = rusqlite::Connection::open(r.store_path.as_ref().expect("an on-disk rig")).unwrap();
    conn.busy_timeout(Duration::from_millis(200)).unwrap();
    conn
}

fn saved_sessions(r: &Rig) -> Vec<CompanionSessionRow> {
    r.h.store.load_engine_state().unwrap().sessions
}

/// The warming's rows of the saved agents, each beside its row of the session engine, so a count of the rows or a row
/// that left holds for both of an agent's rows.
fn saved_agents(r: &Rig) -> Vec<KeepaliveAgentRow> {
    let state = r.h.store.load_engine_state().unwrap();
    assert_eq!(
        state
            .agents
            .iter()
            .map(|row| (
                &row.key_id,
                row.key_generation,
                &row.session_id,
                &row.agent_id
            ))
            .collect::<Vec<_>>(),
        state
            .warming_agents
            .iter()
            .map(|row| (
                &row.key_id,
                row.key_generation,
                &row.session_id,
                &row.agent_id
            ))
            .collect::<Vec<_>>(),
        "each saved agent has its row of the session engine and its row of the warming"
    );
    state.warming_agents
}

/// One member of the saved payload of `agent` on `session`, from the row that holds it.
fn saved_field(r: &Rig, session: &str, agent: &str, field: &str) -> serde_json::Value {
    saved_of(&r.h.store, session, agent)
        .unwrap_or_else(|| panic!("no saved row for {session} {agent}"))[field]
        .clone()
}

/// The probes the registry holds unanswered for `agent` on `session`: the open chain's and those kept for the next
/// request, read off the saved payload as it stands, with no flush of its own.
fn saved_unanswered(r: &Rig, session: &str, agent: &str) -> usize {
    let payload = saved_of(&r.h.store, session, agent)
        .unwrap_or_else(|| panic!("no saved row for {session} {agent}"));
    payload["unanswered"].as_array().map_or(0, Vec::len)
        + payload["kept"].as_array().map_or(0, Vec::len)
}

/// The sealed body, its nonce and its size as the registry holds them for `agent` of the rig's session, read past the
/// store.
fn stored_body(r: &Rig, agent: &str) -> (Option<Vec<u8>>, Option<Vec<u8>>, i64) {
    registry(r)
        .query_row(
            "SELECT body_sealed, body_nonce, body_size FROM keepalive_agents \
             WHERE session_id = 'session' AND agent_id = ?1",
            [agent],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap()
}

fn heartbeat() -> Value {
    json!({"event": "heartbeat"})
}

/// A `parked` report of kind `background` with the command's deadline, for `Rig::report` to send under main's
/// generation.
fn parked_until(deadline: i64) -> Value {
    json!({
        "event": "state",
        "agent_id": "main",
        "state": "parked",
        "park_kind": "background",
        "deadline": deadline
    })
}

/// One real request on `session` rather than the rig's own.
async fn observe_on(r: &mut Rig, session: &str, agent: &str) {
    let mut req = r.request(agent);
    relay_common::set_header(&mut req, "x-claude-code-session-id", session);
    let resp = r.h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    let id = relay_common::outcome(&resp).request_id;
    let _ = resp.into_body().collect().await.unwrap();
    r.pump(Some(&id)).await;
}

/// A report filled as `Rig::event` fills it and posted under the harness key. It adopts nothing of the answer, so the
/// rig's instances and generations stay as the reports before it left them.
async fn report(r: &mut Rig, mut body: Value) -> Result<Acked, Refused> {
    r.sequence += 1;
    let fields = body
        .as_object_mut()
        .expect("a report's body is a JSON object");
    fields.insert("sequence".into(), r.sequence.into());
    let session_id = fields
        .entry("session_id")
        .or_insert("session".into())
        .as_str()
        .expect("a session id is a string")
        .to_string();
    if !fields.contains_key("instance_id") {
        fields.insert("instance_id".into(), r.instance_of(&session_id).into());
    }
    r.post(KEY, body).await
}

/// A `state` report for `agent` under the generation of the agent's last answer, which the caller holds current, so no
/// refused send runs beside the flushes it counts.
fn state_of(r: &Rig, agent: &str, state: &str) -> Value {
    let mut body = json!({
        "event": "state",
        "agent_id": agent,
        "state": state,
        "request_generation": r.request_generation(agent)
    });
    if state == "parked" {
        body["park_kind"] = "background".into();
    }
    body
}

/// Runs `work` to its end on a thread of its own. A save held in the store's busy wait keeps the store's connection,
/// and a runtime task that reads the store (the relay's catalog refresh does every few milliseconds) blocks the
/// worker it runs on behind it, so a flush or a shutdown left to the workers might never get to run while the test
/// holds the registry. The blocked worker can leave tokio's timer undriven too, so the test's own waits inside the hold
/// sleep on its thread's clock rather than on a tokio timer.
fn on_own_thread(
    work: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::thread::JoinHandle<()> {
    let runtime = tokio::runtime::Handle::current();
    std::thread::spawn(move || runtime.block_on(work))
}

#[tokio::test]
async fn a_flush_writes_the_sessions_the_agents_and_the_recipes_and_a_restart_loads_them_silent() {
    let mut r = rig(on_disk()).await;
    // The day's counts and the presence ride the payload too: a probe and a credit just before a UTC midnight, then a
    // presence report just after it, so the day the counts are for is not the day of the agent's last activity.
    let start = DAY - 400_000;
    let now = r.now();
    r.advance(start - now);
    r.reregister().await;
    r.observe("main").await;
    r.advance_attended(270_000).await;
    r.step().await;
    r.advance_attended(40_000).await;
    r.observe("main").await;
    r.advance_attended(120_000).await;
    let presence = json!({"event": "activity", "agent_id": "main", "kind": "presence"});
    r.report(presence).await.unwrap();
    let seen = r.now();
    let deadline = r.now() + 2 * 3_600_000;
    r.report(parked_until(deadline)).await.unwrap();
    r.advance(MINUTE);
    let stats = r.engine.stats();
    r.engine.flush_persisted().await;
    assert_eq!(
        (saved_sessions(&r).len(), saved_agents(&r).len()),
        (1, 1),
        "the flush wrote the session and its agent"
    );
    let agents = saved_agents(&r);
    let main = &agents[0];
    let payload: serde_json::Value = serde_json::from_str(&main.payload).unwrap();
    let lifecycle: serde_json::Value =
        serde_json::from_str(&r.h.store.load_engine_state().unwrap().agents[0].payload).unwrap();
    assert_eq!(
        (
            lifecycle["state"].as_str(),
            lifecycle["park_kind"].as_str(),
            lifecycle["deadline"].as_i64(),
            lifecycle["park_began"].as_i64(),
        ),
        (
            Some("parked"),
            Some("background"),
            Some(deadline),
            Some(r.now() - MINUTE)
        )
    );
    assert_eq!(
        (
            payload["probes_today"].as_u64(),
            payload["credited_today"].as_u64(),
            payload["counters_day"].as_i64(),
            payload["last_probe_at"].as_i64(),
            lifecycle["last_presence_at"].as_i64(),
        ),
        (Some(1), Some(1), Some(0), Some(start + 270_000), Some(seen)),
        "the counts are the day before the last activity's"
    );
    assert!(main.body.as_ref().is_some_and(|body| !body.is_empty()));
    assert_eq!(
        (main.body_size, main.fast),
        (stats.warming.retained_bytes, false),
        "the recipe's size is the retained bytes"
    );

    let second = r.restart().await;
    second.load_persisted().await;
    assert_eq!(
        view_of(&second, "session", "main").unwrap(),
        r.view("main"),
        "every field of the agent's view reads back as it stood"
    );
    assert_eq!(
        second.sessions()[0].presence_age_seconds,
        Some(60),
        "the presence a minute before the restart"
    );
    assert_eq!(
        second.stats().warming.retained_bytes,
        stats.warming.retained_bytes
    );
    assert_eq!(
        (
            second.stats().sessions.entries,
            second.stats().sessions.agents
        ),
        (1, 1)
    );
    assert_eq!(second.sessions()[0].companion, CompanionState::Silent);
}

#[tokio::test]
async fn a_heartbeat_after_the_restart_answers_instance_ended_and_the_same_instance_re_adopts_with_the_sequence_kept(
) {
    let mut r = rig(on_disk()).await;
    r.observe("main").await;
    r.event(heartbeat()).await.unwrap();
    let sequence = r.sequence;
    r.engine.flush_persisted().await;
    r.engine = r.restart().await;
    r.engine.load_persisted().await;

    assert_eq!(
        r.event(heartbeat()).await.unwrap_err().code,
        "instance_ended",
        "a silent session answers the mod's heartbeat as an ended instance"
    );
    let ack = r.reregister().await;
    assert_eq!(ack.sequence, sequence + 2, "the same instance re-adopts it");
    assert_eq!(r.engine.sessions()[0].companion, CompanionState::Reporting);
    let stale = r
        .post(
            KEY,
            json!({
                "event": "heartbeat",
                "session_id": "session",
                "instance_id": "instance",
                "sequence": sequence
            }),
        )
        .await;
    assert_eq!(
        stale.unwrap_err().code,
        "stale_sequence",
        "the sequence the save carried still fences a report the mod already sent"
    );
    r.event(heartbeat())
        .await
        .expect("the next sequence is accepted");
    assert_eq!(
        r.engine.recipes_for("desktop", "session"),
        vec!["main".to_string()],
        "the recipe the mod's session left stays retained"
    );
}

#[tokio::test]
async fn an_agents_facts_report_leaves_its_saved_rows_instant_where_it_was() {
    let mut r = rig(RigSpec::default()).await;
    let generation = r
        .event(json!({"event": "agent_start", "agent_id": "a1"}))
        .await
        .unwrap()
        .request_generation;
    r.engine.flush_persisted().await;
    let started_at = r.now();
    r.advance(MINUTE);
    r.event(json!({
        "event": "agent_facts",
        "agent_id": "a1",
        "name": "quay-inspector",
        "request_generation": generation
    }))
    .await
    .unwrap();
    r.engine.flush_persisted().await;
    assert_eq!(
        saved_agents(&r)
            .into_iter()
            .find(|row| row.agent_id == "a1")
            .map(|row| row.updated_at),
        Some(started_at),
        "a facts report moves no member of the agent's row, so the instant that ranks the agents at a load stays"
    );
}

#[tokio::test]
async fn the_generations_survive_so_the_next_state_report_is_not_a_409() {
    let mut r = rig(on_disk()).await;
    r.event(json!({"event": "agent_start", "agent_id": "sub"}))
        .await
        .unwrap();
    r.observe("main").await;
    r.observe("main").await;
    r.observe("sub").await;
    let listed = r
        .reregister()
        .await
        .agents
        .expect("a registration lists the session's agents");
    let generation_of = |agent: &str| {
        listed
            .iter()
            .find(|a| a.agent_id == agent)
            .map(|a| a.request_generation)
            .unwrap_or_else(|| panic!("the registration lists {agent}"))
    };
    let generations = (generation_of("main"), generation_of("sub"));
    assert!(generations.0 > 0 && generations.1 > 0, "{generations:?}");
    r.engine.flush_persisted().await;
    r.engine = r.restart().await;
    r.engine.load_persisted().await;

    let ack = r.reregister().await;
    let acked: Vec<(String, u64)> = ack
        .agents
        .unwrap()
        .into_iter()
        .map(|agent| (agent.agent_id, agent.request_generation))
        .collect();
    assert_eq!(
        acked,
        vec![
            ("main".to_string(), generations.0),
            ("sub".to_string(), generations.1)
        ]
    );
    for (agent, generation) in [("main", generations.0), ("sub", generations.1)] {
        r.event(json!({
            "event": "state",
            "agent_id": agent,
            "state": "idle",
            "request_generation": generation
        }))
        .await
        .unwrap_or_else(|e| panic!("{agent} under its saved generation: {e:?}"));
    }
}

#[tokio::test]
async fn an_ended_session_and_an_expired_recipe_are_dropped_at_the_load_under_expired_at_restart() {
    let mut r = rig(on_disk()).await;
    r.event(session_start("gone")).await.unwrap();
    r.event(session_end("gone")).await.unwrap();
    r.observe("main").await;
    r.engine.flush_persisted().await;
    assert_eq!(
        saved_sessions(&r).len(),
        2,
        "the ended session was saved too"
    );
    // The router is down past the entry's expiry.
    r.advance(400_000);
    let second = r.restart().await;
    second.load_persisted().await;

    assert_eq!(
        second.reasons().get(&Reason::ExpiredAtRestart).copied(),
        Some(1)
    );
    assert!(second.recipes_for("desktop", "session").is_empty());
    assert_eq!(
        (
            second.stats().sessions.entries,
            second.stats().warming.retained_bytes
        ),
        (1, 0)
    );
    assert_eq!(
        saved_sessions(&r)
            .iter()
            .map(|s| s.session_id.as_str())
            .collect::<Vec<_>>(),
        vec!["session"],
        "the ended session's row is gone"
    );
    let agents = saved_agents(&r);
    assert_eq!(
        agents
            .iter()
            .map(|a| (a.session_id.as_str(), a.body.is_some(), a.body_size))
            .collect::<Vec<_>>(),
        vec![("session", false, 0)],
        "the ended session's agent is gone and the expired recipe's prompt with it"
    );
    assert_eq!(
        saved_field(&r, "session", "main", "reason"),
        "expired_at_restart"
    );
}

/// The next turn of the rig's conversation.
fn next_turn() -> String {
    BODY.replace(
        r#"{"role":"user","content":"hello"}]"#,
        r#"{"role":"user","content":"hello"},{"role":"assistant","content":"hi"},{"role":"user","content":"and then"}]"#,
    )
}

#[tokio::test]
async fn the_idle_age_counters_round_trip() {
    // Keepalive off, so no recipe is retained and the ticks probe nothing: the counters count all the same.
    let mut r = rig(RigSpec {
        policy: KeepalivePolicy::default(),
        ..on_disk()
    })
    .await;
    let today = NOW / DAY;
    r.observe("main").await;
    r.state("main", "idle").await;
    r.advance_attended(6 * MINUTE).await;
    r.step().await;
    // The next turn of the same conversation, six minutes into the window.
    r.observe_with("main", &next_turn()).await;
    assert_eq!(
        r.engine.idle_returns_over(today * DAY, 1)["main"].buckets[..2]
            .iter()
            .map(|b| (b.reached, b.returned))
            .collect::<Vec<_>>(),
        vec![(1, 0), (1, 1)],
        "a window six minutes old reached the first two buckets and returned in the second"
    );
    // The window open from that return reaches its later buckets on the next UTC day.
    r.state("main", "idle").await;
    r.advance_attended(DAY).await;
    r.step().await;
    let counted = [today, today + 1].map(|day| day_counts(&r.engine, day));
    assert!(
        counted
            .iter()
            .all(|(reached, _)| reached.iter().any(|count| *count > 0)),
        "a day apiece: {counted:?}"
    );
    r.engine.flush_persisted().await;
    let second = r.restart().await;
    second.load_persisted().await;
    assert_eq!(
        [today, today + 1].map(|day| day_counts(&second, day)),
        counted
    );
}

/// Main's `reached` and `returned` on the UTC day `day`, through the overview's sum over that one day, which answers
/// zero buckets for a day main's pool holds no count on.
fn day_counts(engine: &Engine, day: i64) -> ([u64; 10], [u64; 10]) {
    engine
        .idle_returns_over(day * DAY, 1)
        .get("main")
        .map_or(([0; 10], [0; 10]), |counters| {
            (
                counters.buckets.map(|bucket| bucket.reached),
                counters.buckets.map(|bucket| bucket.returned),
            )
        })
}

/// The rows of each UTC day the registry holds.
fn saved_days(r: &Rig) -> BTreeSet<i64> {
    r.h.store
        .load_engine_state()
        .unwrap()
        .counters
        .into_iter()
        .map(|row| row.day)
        .collect()
}

/// A day of counters saved, then the roll at the next day's first tick, which drops it under a retention of one day and
/// leaves its purge the one change the next save carries. Answers the dropped day.
async fn the_only_day_rolled_off(r: &mut Rig) -> i64 {
    let today = NOW / DAY;
    r.observe("main").await;
    r.state("main", "idle").await;
    r.advance_attended(6 * MINUTE).await;
    r.step().await;
    r.observe_with("main", &next_turn()).await;
    r.engine.flush_persisted().await;
    assert_eq!(saved_days(r), BTreeSet::from([today]));
    r.advance_attended(DAY).await;
    r.engine.flush_persisted().await;
    r.step().await;
    assert!(
        r.engine.idle_returns_over(r.now(), 1).is_empty(),
        "the roll dropped the only day"
    );
    today
}

/// `spec` under a retention of one day with keepalive off, so no recipe or probe adds a row the saves carry.
fn one_day_retained(spec: RigSpec) -> RigSpec {
    RigSpec {
        policy: KeepalivePolicy::default(),
        timing: KeepaliveTiming {
            idle_return_retention_days: 1,
            ..spec.timing
        },
        ..spec
    }
}

/// The save that carried the purge settles it, so a later flush with nothing changed saves nothing and leaves a row of
/// the dropped day the registry holds again.
#[tokio::test]
async fn a_flush_after_the_purge_landed_carries_it_no_more() {
    let mut r = rig(one_day_retained(RigSpec::default())).await;
    let dropped = the_only_day_rolled_off(&mut r).await;
    r.engine.flush_persisted().await;
    assert_eq!(
        saved_days(&r),
        BTreeSet::new(),
        "the save carried the purge"
    );
    r.h.store
        .save_engine_state(&EngineBatch {
            counters: vec![IdleReturnsRow {
                pool_id: "main".into(),
                day: dropped,
                bucket: 0,
                reached: 1,
                returned: 0,
            }],
            ..EngineBatch::default()
        })
        .unwrap();
    r.engine.flush_persisted().await;
    assert_eq!(
        saved_days(&r),
        BTreeSet::from([dropped]),
        "a flush with nothing changed carries no purge"
    );
}

/// The tick a minute before midnight counts the open window's ninth bucket on the rig's day, and the save carries that
/// day with the window, so the return a restart later reads after midnight counts on it.
#[tokio::test]
async fn an_open_windows_bucket_day_round_trips() {
    let mut r = rig(RigSpec::default()).await;
    let today = NOW / DAY;
    r.observe("main").await;
    r.state("main", "idle").await;
    r.advance_attended((today + 1) * DAY - MINUTE - r.now())
        .await;
    r.step().await;
    r.engine.flush_persisted().await;
    assert_eq!(
        saved_field(&r, "session", "main", "idle_episode")["bucket_day"],
        today
    );
    let second = r.restart().await;
    second.load_persisted().await;
    r.advance(2 * MINUTE);
    let (static_hash, boundaries) = chain_of(BODY);
    second.observe(r.observed_with(
        "back",
        "main",
        None,
        Some(&static_hash),
        &[&boundaries[0], "next"],
        BODY.as_bytes(),
        "claude-sonnet-4-5",
    ));
    assert_eq!(day_counts(&second, today).1[8], 1, "the reach's day");
    assert_eq!(day_counts(&second, today + 1).1, [0; 10], "the closing day");
}

/// A saved open window whose row carries no day for its reached bucket loads, and its return, which raises no bucket,
/// counts nothing: the reach it would answer beside is held under no day.
#[tokio::test]
async fn a_return_of_a_window_loaded_without_its_buckets_day_counts_nothing() {
    let mut r = rig(on_disk()).await;
    r.observe("main").await;
    r.state("main", "idle").await;
    r.advance_attended(6 * MINUTE).await;
    r.step().await;
    r.engine.flush_persisted().await;
    let conn = registry(&r);
    let payload: String = conn
        .query_row(
            "SELECT payload FROM keepalive_agents WHERE agent_id = 'main'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let mut payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
    payload["idle_episode"]
        .as_object_mut()
        .unwrap()
        .remove("bucket_day");
    let rewritten = conn
        .execute(
            "UPDATE keepalive_agents SET payload = ?1 WHERE agent_id = 'main'",
            [payload.to_string()],
        )
        .unwrap();
    assert_eq!(rewritten, 1);
    let second = r.restart().await;
    second.load_persisted().await;
    let (static_hash, boundaries) = chain_of(BODY);
    second.observe(r.observed_with(
        "back",
        "main",
        None,
        Some(&static_hash),
        &[&boundaries[0], "next"],
        BODY.as_bytes(),
        "claude-sonnet-4-5",
    ));
    // The day's read and the wider sum beside it, so a return filed under another day fails too.
    let wider = second
        .idle_returns_over(r.now(), 30)
        .get("main")
        .map_or([0; 10], |counters| {
            counters.buckets.map(|bucket| bucket.returned)
        });
    let returned = (day_counts(&second, r.now() / DAY).1, wider);
    assert_eq!(returned, ([0; 10], [0; 10]), "no return counted");
}

/// Main's request on two messages, so its prefix holds a boundary before the last.
fn two_messages() -> String {
    BODY.replace(
        r#"{"role":"user","content":"hello"}]"#,
        r#"{"role":"user","content":"hello"},{"role":"user","content":"and then"}]"#,
    )
}

/// Main's request on three messages whose first carries the cache marker, so its prefix holds a marked boundary before
/// the last two.
fn marked_first_of_three() -> String {
    BODY.replace(
        r#"{"role":"user","content":"hello"}]"#,
        r#"{"role":"user","content":[{"type":"text","text":"hello","cache_control":{"type":"ephemeral"}}]},{"role":"user","content":"and then"},{"role":"user","content":"more"}]"#,
    )
}

/// The static hash and the message boundaries the relay reads off `body`.
fn chain_of(body: &str) -> (String, Vec<String>) {
    let chains = phoebis_relay::Chains::read(&serde_json::from_str(body).unwrap()).unwrap();
    (
        chains.static_hash().unwrap().to_string(),
        chains.message_boundaries.to_vec(),
    )
}

/// The rig's clock past the five-minute entry of main's recipe, so the next load discards it as lapsed and saves
/// main's row again from what it read.
fn lapse_while_down(r: &Rig) {
    r.advance(400_000);
}

#[tokio::test]
async fn the_anchor_pair_round_trips_in_the_last_prefix_and_the_open_windows_anchor() {
    let mut r = rig(on_disk()).await;
    r.observe_with("main", &marked_first_of_three()).await;
    r.state("main", "idle").await;
    let (static_hash, boundaries) = chain_of(&marked_first_of_three());
    let pair = serde_json::json!([static_hash, boundaries[2], boundaries[1]]);
    let marked = serde_json::json!(boundaries[0]);
    r.engine.flush_persisted().await;
    assert_eq!(saved_field(&r, "session", "main", "last_prefix"), pair);
    assert_eq!(
        saved_field(&r, "session", "main", "last_prefix_marked"),
        marked
    );
    let episode = saved_field(&r, "session", "main", "idle_episode");
    assert_eq!(
        (&episode["anchor"], &episode["anchor_marked"]),
        (&pair, &marked)
    );
    lapse_while_down(&r);
    let second = r.restart().await;
    second.load_persisted().await;
    assert_eq!(
        second.reasons().get(&Reason::ExpiredAtRestart).copied(),
        Some(1),
        "the load saved main's row again"
    );
    assert_eq!(
        saved_field(&r, "session", "main", "last_prefix"),
        pair,
        "the loaded pair"
    );
    assert_eq!(
        saved_field(&r, "session", "main", "last_prefix_marked"),
        marked,
        "the loaded marked boundary"
    );
    let episode = saved_field(&r, "session", "main", "idle_episode");
    assert_eq!(
        (&episode["anchor"], &episode["anchor_marked"]),
        (&pair, &marked),
        "the loaded window's anchor"
    );
}

#[tokio::test]
async fn the_previous_prompt_round_trips() {
    let mut r = rig(on_disk()).await;
    // The standing answer reads 2,000 tokens from the cache and nothing else.
    r.observe("main").await;
    r.engine.flush_persisted().await;
    assert_eq!(
        saved_field(&r, "session", "main", "last_prompt_tokens"),
        2_000
    );
    lapse_while_down(&r);
    let second = r.restart().await;
    second.load_persisted().await;
    assert_eq!(
        second.reasons().get(&Reason::ExpiredAtRestart).copied(),
        Some(1),
        "the load saved main's row again"
    );
    assert_eq!(
        saved_field(&r, "session", "main", "last_prompt_tokens"),
        2_000
    );
}

/// A row saved with the static hash and the tail alone loads, its window judging a return on the tail, and saves
/// again with no boundary before the last.
#[tokio::test]
async fn a_row_saved_with_the_static_hash_and_tail_pair_loads_with_no_boundary_before_the_last() {
    let mut r = rig(on_disk()).await;
    r.observe_with("main", &two_messages()).await;
    r.state("main", "idle").await;
    r.engine.flush_persisted().await;
    let (static_hash, boundaries) = chain_of(&two_messages());
    let tail_pair = serde_json::json!([static_hash, boundaries[1]]);
    let conn = registry(&r);
    let payload: String = conn
        .query_row(
            "SELECT payload FROM keepalive_agents WHERE agent_id = 'main'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let mut payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
    payload["last_prefix"] = tail_pair.clone();
    payload["idle_episode"]["anchor"] = tail_pair.clone();
    let rewritten = conn
        .execute(
            "UPDATE keepalive_agents SET payload = ?1 WHERE agent_id = 'main'",
            [payload.to_string()],
        )
        .unwrap();
    assert_eq!(rewritten, 1);
    lapse_while_down(&r);
    let second = r.restart().await;
    second.load_persisted().await;
    assert_eq!(
        (
            second.stats().sessions.entries,
            second.stats().sessions.agents
        ),
        (1, 1)
    );
    assert_eq!(
        second.reasons().get(&Reason::ExpiredAtRestart).copied(),
        Some(1),
        "the load saved main's row again"
    );
    for (field, saved) in [
        (
            "last_prefix",
            saved_field(&r, "session", "main", "last_prefix"),
        ),
        (
            "anchor",
            saved_field(&r, "session", "main", "idle_episode")["anchor"].clone(),
        ),
    ] {
        assert_eq!(
            (saved.get(0), saved.get(1)),
            (tail_pair.get(0), tail_pair.get(1)),
            "{field}"
        );
        assert!(
            saved.get(2).is_none_or(serde_json::Value::is_null),
            "{field}: {saved}"
        );
    }
    second.observe(r.observed_with(
        "back",
        "main",
        None,
        Some(&static_hash),
        &[&boundaries[0], &boundaries[1], "next"],
        two_messages().as_bytes(),
        "claude-sonnet-4-5",
    ));
    assert_eq!(
        day_counts(&second, NOW / DAY).1[1],
        1,
        "the loaded window counts a return on its tail"
    );
}

#[tokio::test]
async fn a_body_that_cannot_be_opened_leaves_the_agent_without_a_recipe() {
    let mut r = rig(on_disk()).await;
    r.observe("main").await;
    r.engine.flush_persisted().await;
    let damaged = registry(&r)
        .execute(
            "UPDATE keepalive_agents SET body_sealed = zeroblob(length(body_sealed)) WHERE agent_id = 'main'",
            [],
        )
        .unwrap();
    assert_eq!(damaged, 1);
    let second = r.restart().await;
    second.load_persisted().await;

    let main = view_of(&second, "session", "main").expect("the agent keeps its record");
    assert_eq!(main.cause, Reason::Reset);
    assert!(second.recipes_for("desktop", "session").is_empty());
    assert_eq!(
        (
            second.stats().sessions.agents,
            second.stats().warming.retained_bytes
        ),
        (1, 0)
    );
    assert_eq!(
        saved_field(&r, "session", "main", "current"),
        serde_json::Value::Null
    );
    assert_eq!(
        stored_body(&r, "main"),
        (None, None, 0),
        "the blob that no longer opens leaves the registry with the load's rewrite"
    );
}

/// A flush that rewrites an agent whose recipe did not change since its last landed save leaves the sealed body and its
/// nonce as stored, and the body still opens.
#[tokio::test]
async fn a_flush_that_rewrites_an_agent_with_an_unchanged_recipe_leaves_its_sealed_body_as_stored()
{
    let mut r = rig(on_disk()).await;
    r.observe("main").await;
    r.engine.flush_persisted().await;
    let sealed = stored_body(&r, "main");
    assert!(sealed.0.is_some() && sealed.1.is_some() && sealed.2 > 0);
    r.state("main", "idle").await;
    r.engine.flush_persisted().await;
    assert_eq!(
        saved_field(&r, "session", "main", "state"),
        "idle",
        "the flush rewrote the agent's row"
    );
    assert_eq!(
        stored_body(&r, "main"),
        sealed,
        "and left the sealed body and its nonce as they were"
    );
    assert!(saved_agents(&r)[0].body.is_some(), "the body still opens");
}

/// A flush after the recipe ended clears its sealed body, so the prompt leaves the disk with the recipe, whether the
/// running engine ended it, a second instance took the session over with a new record under the same row, or the load
/// ended it.
#[tokio::test]
async fn a_flush_after_the_recipe_ends_clears_its_sealed_body() {
    let mut r = rig(on_disk()).await;
    r.observe("main").await;
    r.engine.flush_persisted().await;
    assert!(stored_body(&r, "main").0.is_some());
    r.state("main", "released").await;
    r.engine.flush_persisted().await;
    assert_eq!(
        stored_body(&r, "main"),
        (None, None, 0),
        "a release ends the recipe and its prompt leaves the registry"
    );

    let mut r = rig(on_disk()).await;
    r.observe("main").await;
    r.engine.flush_persisted().await;
    assert!(stored_body(&r, "main").0.is_some());
    r.event(json!({
        "event": "session_start",
        "instance_id": "second",
        "previous_instance_id": "instance"
    }))
    .await
    .unwrap();
    r.engine.flush_persisted().await;
    assert_eq!(
        stored_body(&r, "main"),
        (None, None, 0),
        "the replaced record's prompt leaves the registry with the new record's first save"
    );

    let mut r = rig(on_disk()).await;
    r.observe("main").await;
    r.engine.flush_persisted().await;
    assert!(stored_body(&r, "main").0.is_some());
    // The router is down past the entry's expiry, so the load ends the recipe it restores.
    r.advance(400_000);
    let second = r.restart().await;
    second.load_persisted().await;
    assert_eq!(
        second.reasons().get(&Reason::ExpiredAtRestart).copied(),
        Some(1)
    );
    assert_eq!(
        stored_body(&r, "main"),
        (None, None, 0),
        "the recipe the load ended leaves the registry with the load's save"
    );
}

#[tokio::test]
async fn the_shutdown_quiesces_drains_saves_and_then_clears() {
    let mut r = rig(on_disk()).await;
    r.event(json!({"event": "agent_start", "agent_id": "sub"}))
        .await
        .unwrap();
    r.observe("main").await;
    r.observe("sub").await;
    r.state("main", "idle").await;
    r.engine.shutdown().await;

    assert_eq!(
        (
            r.engine.stats().sessions.entries,
            r.engine.stats().warming.retained_bytes
        ),
        (0, 0),
        "the clear came after the save"
    );
    assert_eq!(
        saved_sessions(&r)
            .iter()
            .map(|s| s.session_id.as_str())
            .collect::<Vec<_>>(),
        vec!["session"]
    );
    assert_eq!(
        saved_agents(&r)
            .iter()
            .map(|a| (a.agent_id.as_str(), a.body.is_some()))
            .collect::<Vec<_>>(),
        vec![("main", true), ("sub", true)],
        "each agent's row and its recipe were saved"
    );
    assert_eq!(
        (
            saved_field(&r, "session", "main", "state"),
            saved_field(&r, "session", "sub", "state")
        ),
        (serde_json::json!("idle"), serde_json::json!("busy"))
    );
}

#[tokio::test]
async fn a_request_that_completes_during_shutdown_is_refused_and_counted_and_the_save_holds_the_quiesced_state(
) {
    let mut r = rig(on_disk()).await;
    r.observe("main").await;
    r.engine.observe(r.observed("late", "main", BODY));
    r.engine.observe(r.selected("late", "a"));
    r.engine.flush_persisted().await;
    let agents = saved_agents(&r);
    r.engine.quiesce();
    // A failover onto the other account, which the quiesced state never saw.
    let BusEvent::AccountSelected(mut failover) = r.selected("late", "b") else {
        unreachable!("the rig builds a selection")
    };
    failover.attempt = 2;
    r.bus.publish(BusEvent::AccountSelected(failover));
    r.bus.publish(r.complete(
        "late",
        Outcome::Succeeded,
        Some(Usage {
            cache_read_tokens: Some(2_000),
            ..Default::default()
        }),
    ));
    r.pump(None).await;
    r.engine.flush_persisted().await;
    r.engine.close();

    assert_eq!(
        r.engine.stats().late_updates,
        2,
        "the selection and the completion are both refused and counted"
    );
    assert_eq!(
        saved_agents(&r),
        agents,
        "the saved agent is the quiesced one: the selection moved no account, and the completion armed nothing and \
         credited nothing"
    );
    assert_eq!(
        saved_field(&r, "session", "main", "reason"),
        "awaiting_cache_evidence"
    );
    assert_eq!(r.event(heartbeat()).await.unwrap_err().code, "quiesced");
    assert_eq!(r.engine.stats().late_updates, 3);
}

#[tokio::test]
async fn a_head_and_a_chunk_that_land_during_shutdown_are_refused_and_counted_and_the_save_holds_the_quiesced_state(
) {
    let r = rig(RigSpec::default()).await;
    r.engine.observe(r.observed("late", "main", BODY));
    r.engine.observe(r.selected("late", "a"));
    r.engine.quiesce();
    // Each would write the saved agent if it landed: the head moves the attempt into overage, and the stream's first
    // event arms the schedule.
    r.engine
        .observe(BusEvent::ResponseHead(phoebis_bus::ResponseHead {
            request_id: "late".into(),
            attempt: 1,
            status: 200,
            headers: vec![(
                "anthropic-ratelimit-unified-overage-in-use".into(),
                "true".into(),
            )],
            first_event: None,
            outcome: phoebis_bus::AttemptOutcome::Succeeded,
            cooldown: None,
            written_at: r.now(),
            observed_at: r.now(),
        }));
    r.engine
        .observe(BusEvent::ResponseChunk(phoebis_bus::ResponseChunk {
            request_id: "late".into(),
            attempt: 1,
            index: 0,
            body: Default::default(),
            phase: Some(phoebis_bus::Phase::MessageStart {
                usage: Usage {
                    cache_read_tokens: Some(2_000),
                    ..Default::default()
                },
            }),
        }));
    r.engine.flush_persisted().await;
    r.engine.close();

    assert_eq!(
        r.engine.stats().late_updates,
        2,
        "the head and the chunk are both refused and counted"
    );
    let current = saved_field(&r, "session", "main", "current");
    assert_eq!(
        (
            current["in_overage"].clone(),
            current["expected"].clone(),
            saved_field(&r, "session", "main", "expires"),
            saved_field(&r, "session", "main", "reason")
        ),
        (
            json!(false),
            json!(0),
            Value::Null,
            json!("awaiting_cache_evidence")
        ),
        "the saved agent is the quiesced one: the head moved no funding, and the chunk armed nothing"
    );
}

#[tokio::test]
async fn a_compaction_that_completes_during_shutdown_is_refused_and_counted_and_the_save_holds_the_armed_recipe(
) {
    let mut r = rig(RigSpec::default()).await;
    let turn = r.observe("main").await;
    r.engine.observe(r.observed_with(
        "compaction",
        "main",
        Some("compaction"),
        None,
        &[],
        BODY.as_bytes(),
        "claude-sonnet-4-5",
    ));
    r.engine.quiesce();
    // A succeeded compaction that landed would end the recipe under `compacted`.
    r.engine
        .observe(r.complete("compaction", Outcome::Succeeded, None));
    r.engine.flush_persisted().await;
    r.engine.close();

    assert_eq!(
        r.engine.stats().late_updates,
        1,
        "the compaction's completion is refused and counted"
    );
    assert_eq!(
        (
            saved_field(&r, "session", "main", "current")["id"].clone(),
            saved_field(&r, "session", "main", "expires"),
            saved_field(&r, "session", "main", "reason")
        ),
        (json!(turn), json!(1_300_000), json!("scheduled")),
        "the saved recipe is the turn's, armed until its five-minute entry ends"
    );
}

#[tokio::test]
async fn a_probe_result_that_lands_during_shutdown_leaves_the_save_as_the_quiesce_left_it() {
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    // The upstream holds the probe, so the shutdown's cancel ends its exchange and the result lands behind the quiesce.
    r.up.push_credential_rule(
        A,
        "/v1/messages",
        Respond::Gated(Box::new(probe_answer(2_000, 0))),
    );
    r.advance(270_000);
    r.engine.step().await;
    wait_for(Duration::from_secs(5), || {
        (r.up.requests().len() == 2).then_some(())
    })
    .await;
    r.engine.shutdown().await;

    assert_eq!(
        (
            saved_field(&r, "session", "main", "reason"),
            saved_field(&r, "session", "main", "failures"),
            saved_field(&r, "session", "main", "paused"),
            saved_field(&r, "session", "main", "retry_at")
        ),
        (json!("probing"), json!(0), json!(false), Value::Null),
        "the canceled probe's result put no retry and no cause into the last save"
    );
    r.up.release();
}

// The test's body runs on a thread apart from the workers, which a held save can leave blocked behind the store's
// connection (see `on_own_thread`), and it waits on the engine's flush counts, which need no timer the workers drive.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shutdown_while_an_earlier_save_is_held_lands_both_saves_in_order() {
    let mut r = rig(on_disk()).await;
    r.event(json!({"event": "agent_start", "agent_id": "sub"}))
        .await
        .unwrap();
    r.observe("main").await;
    r.engine.flush_persisted().await;
    r.state("main", "idle").await;
    r.state("sub", "idle").await;
    let held_at = r.now();
    let held = registry(&r);
    held.execute_batch("BEGIN IMMEDIATE").unwrap();
    let engine = r.engine.clone();
    let first = on_own_thread(async move { engine.flush_persisted().await });
    r.engine.persist_reached(4, 4).await;
    // After the held save read its batch: only a later save carries this.
    r.advance(MINUTE);
    let parked = state_of(&r, "main", "parked");
    report(&mut r, parked).await.unwrap();
    let engine = r.engine.clone();
    let shutdown = on_own_thread(async move { engine.shutdown().await });
    // The shutdown's flush has entered and waits on the flush lock the held save holds.
    r.engine.persist_reached(5, 4).await;
    // Released well inside the store's own busy wait, which the case never waits out.
    held.execute_batch("COMMIT").unwrap();
    first.join().unwrap();
    shutdown.join().unwrap();

    let saved = |agent: &str| {
        saved_agents(&r)
            .into_iter()
            .find(|row| row.agent_id == agent)
            .map(|row| row.updated_at)
    };
    assert_eq!(
        (
            saved_field(&r, "session", "main", "state"),
            saved("main")
        ),
        (serde_json::json!("parked"), Some(held_at + MINUTE)),
        "the shutdown's save carried the change the held save missed, so it was still dirty after the held save's \
         acknowledgment"
    );
    assert_eq!(
        (saved_field(&r, "session", "sub", "state"), saved("sub")),
        (serde_json::json!("idle"), Some(held_at)),
        "the held save landed first and retired what it carried, which the shutdown's save left as it found it"
    );
}

// Apart from the workers, and waiting on the flush counts, as the case above explains.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_flush_failure_keeps_the_rows_dirty_with_their_revisions_and_the_next_flush_writes_them()
{
    let mut r = rig(on_disk()).await;
    r.event(json!({"event": "agent_start", "agent_id": "sub"}))
        .await
        .unwrap();
    r.observe("main").await;
    r.engine.flush_persisted().await;
    assert_eq!(saved_field(&r, "session", "main", "state"), "busy");

    // A save that fails: a trigger in the registry file refuses every session row, which the store's own connection
    // meets on its next write whatever the file's journal mode. The subagent's change is carried by that save and
    // touched by nothing after it.
    r.state("main", "idle").await;
    r.state("sub", "idle").await;
    let conn = registry(&r);
    conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    let engine = r.engine.clone();
    let failed = on_own_thread(async move { engine.flush_persisted().await });
    r.engine.persist_reached(4, 4).await;
    // Between the failed save's snapshot and its outcome.
    let parked = state_of(&r, "main", "parked");
    report(&mut r, parked).await.unwrap();
    conn.execute_batch(
        "CREATE TRIGGER refuse_insert BEFORE INSERT ON companion_sessions BEGIN SELECT RAISE(ABORT, 'refused'); END; \
         CREATE TRIGGER refuse_update BEFORE UPDATE ON companion_sessions BEGIN SELECT RAISE(ABORT, 'refused'); END; \
         COMMIT;",
    )
    .unwrap();
    failed.join().unwrap();
    assert_eq!(
        (
            saved_field(&r, "session", "main", "state"),
            saved_field(&r, "session", "sub", "state")
        ),
        (serde_json::json!("busy"), serde_json::json!("busy")),
        "the failed save wrote nothing"
    );
    conn.execute_batch("DROP TRIGGER refuse_insert; DROP TRIGGER refuse_update;")
        .unwrap();

    // The next save carries the rows, at the revision the change since left them.
    let last_activity = saved_field(&r, "session", "main", "last_activity");
    conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    let engine = r.engine.clone();
    let next = on_own_thread(async move { engine.flush_persisted().await });
    r.engine.persist_reached(5, 5).await;
    r.advance(MINUTE);
    // Between this save's snapshot and its acknowledgment: a presence report moves the idle clock alone.
    let presence = json!({
        "event": "activity",
        "agent_id": "main",
        "kind": "presence",
        "request_generation": r.request_generation("main")
    });
    report(&mut r, presence).await.unwrap();
    conn.execute_batch("COMMIT").unwrap();
    next.join().unwrap();
    assert_eq!(
        (
            saved_field(&r, "session", "main", "state"),
            saved_field(&r, "session", "main", "last_activity"),
            saved_field(&r, "session", "sub", "state")
        ),
        (serde_json::json!("parked"), last_activity, serde_json::json!("idle")),
        "the rows the failure kept dirty landed, the subagent's at the revision the failed save carried, and main's \
         from the snapshot this save read"
    );

    r.engine.flush_persisted().await;
    assert_eq!(
        saved_field(&r, "session", "main", "last_activity"),
        serde_json::json!(r.now()),
        "the change the acknowledgment found newer than its revision stayed dirty and lands now"
    );
}

/// The rig's session as the registry keys its stored sets, read off its saved row.
fn rig_session(r: &Rig) -> (String, i64, String) {
    let row = saved_sessions(r)
        .into_iter()
        .find(|row| row.session_id == "session")
        .expect("the rig's registration is saved");
    (row.key_id, row.key_generation, row.session_id)
}

/// One stored set of `session`, read through the store and parsed as JSON, `None` where the registry holds no row of
/// that kind and agent.
fn stored_set(
    r: &Rig,
    session: &(String, i64, String),
    kind: FactsKind,
    agent_id: &str,
) -> Option<Value> {
    r.h.store
        .load_companion_facts(std::slice::from_ref(session))
        .unwrap()
        .into_iter()
        .find(|set| set.key.kind == kind && set.key.agent_id == agent_id)
        .map(|set| serde_json::from_slice(&set.value.expect("the set opens")).unwrap())
}

fn stored_title(r: &Rig, session: &(String, i64, String)) -> Option<Value> {
    stored_set(r, session, FactsKind::Title, "").map(|set| set["title"].clone())
}

// Apart from the workers, and waiting on the flush counts, as the held-save cases above explain. The held save keeps
// any save from reading its batch between the two reports, so the save that carries the set runs behind the
// registration that took the agent's record out of the map.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_set_reported_ahead_of_a_registration_by_another_instance_reaches_its_row() {
    let mut r = rig(on_disk()).await;
    let session = rig_session(&r);
    let generation = r
        .event(json!({"event": "agent_start", "agent_id": "a1"}))
        .await
        .unwrap()
        .request_generation;
    let held = registry(&r);
    held.execute_batch("BEGIN IMMEDIATE").unwrap();
    let engine = r.engine.clone();
    let first = on_own_thread(async move { engine.flush_persisted().await });
    r.engine.persist_reached(3, 3).await;
    r.event(json!({
        "event": "agent_facts",
        "agent_id": "a1",
        "name": "reviewer",
        "request_generation": generation
    }))
    .await
    .unwrap();
    r.event(json!({
        "event": "session_start",
        "instance_id": "second",
        "previous_instance_id": "instance"
    }))
    .await
    .unwrap();
    held.execute_batch("COMMIT").unwrap();
    first.join().unwrap();
    r.engine.flush_persisted().await;
    assert_eq!(
        stored_set(&r, &session, FactsKind::Agents, "a1").map(|set| set["name"].clone()),
        Some(json!("reviewer")),
        "the save wrote the set as the report filed it, though the registration replaced the agent's record"
    );
}

// Apart from the workers, and waiting on the flush counts, as the held-save cases above explain. The case starts no
// save of its own until the worker's second one has read its batch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_set_filed_behind_the_snapshot_of_a_held_save_reaches_its_row_with_no_further_report() {
    let mut r = rig(on_disk()).await;
    let session = rig_session(&r);
    let held = registry(&r);
    held.execute_batch("BEGIN IMMEDIATE").unwrap();
    r.event(json!({"event": "session_facts", "title": "Chart the estuary"}))
        .await
        .unwrap();
    // The worker's save for the first title has read its batch and waits in the store.
    r.engine.persist_reached(3, 3).await;
    r.event(json!({"event": "session_facts", "title": "Sound the channel"}))
        .await
        .unwrap();
    held.execute_batch("COMMIT").unwrap();
    // The bound is the failure path alone: a passing run meets the worker's second save at once.
    tokio::time::timeout(Duration::from_secs(10), r.engine.persist_reached(4, 4))
        .await
        .expect("the worker saves once more for the set filed while its save ran");
    r.engine.flush_persisted().await;
    assert_eq!(
        stored_title(&r, &session),
        Some(json!("Sound the channel")),
        "the second set, filed under the first one's key while its save ran, reached the row"
    );
}

#[tokio::test]
async fn a_set_whose_save_failed_reaches_its_row_with_the_next_save() {
    let mut r = rig(on_disk()).await;
    let session = rig_session(&r);
    let conn = registry(&r);
    conn.execute_batch(
        "CREATE TRIGGER refuse_facts BEFORE INSERT ON companion_facts \
         BEGIN SELECT RAISE(ABORT, 'refused'); END;",
    )
    .unwrap();
    r.event(json!({"event": "session_facts", "title": "Mark the shoal"}))
        .await
        .unwrap();
    // The worker's save, then one behind it, both under the trigger.
    r.engine.persist_reached(3, 3).await;
    r.engine.flush_persisted().await;
    assert_eq!(
        stored_title(&r, &session),
        None,
        "the failed saves wrote no row"
    );
    conn.execute_batch("DROP TRIGGER refuse_facts;").unwrap();
    r.engine.flush_persisted().await;
    assert_eq!(
        stored_title(&r, &session),
        Some(json!("Mark the shoal")),
        "the set the failed saves carried stayed filed for the save that lands"
    );
}

// Apart from the workers, and waiting on the flush counts, as the held-save cases above explain. The held save keeps
// any save from reading its batch between the facts report and the registration, so the next save reads both.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_set_filed_ahead_of_a_registration_that_leaves_its_kind_out_reaches_no_row() {
    let mut r = rig(on_disk()).await;
    let session = rig_session(&r);
    let generation = r
        .event(json!({"event": "agent_start", "agent_id": "a1"}))
        .await
        .unwrap()
        .request_generation;
    let held = registry(&r);
    held.execute_batch("BEGIN IMMEDIATE").unwrap();
    let engine = r.engine.clone();
    let first = on_own_thread(async move { engine.flush_persisted().await });
    r.engine.persist_reached(3, 3).await;
    r.event(json!({
        "event": "agent_facts",
        "agent_id": "a1",
        "name": "tide-gauge-reader",
        "request_generation": generation
    }))
    .await
    .unwrap();
    r.event(json!({"event": "session_start", "reported_facts": []}))
        .await
        .unwrap();
    held.execute_batch("COMMIT").unwrap();
    first.join().unwrap();
    r.engine.flush_persisted().await;
    assert_eq!(
        stored_set(&r, &session, FactsKind::Agents, "a1"),
        None,
        "the set filed ahead of the registration that left its kind out reached no row"
    );
}

// Apart from the workers, and waiting on the flush counts, as the held-save cases above explain. The held save keeps
// any save from reading its batch between the registration and the facts report, so the next save carries the kind's
// leave and the set of that kind in one batch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_set_filed_behind_a_registration_that_leaves_its_kind_out_reaches_its_row() {
    let mut r = rig(on_disk()).await;
    let session = rig_session(&r);
    r.event(json!({"event": "agent_start", "agent_id": "a1"}))
        .await
        .unwrap();
    let held = registry(&r);
    held.execute_batch("BEGIN IMMEDIATE").unwrap();
    let engine = r.engine.clone();
    let first = on_own_thread(async move { engine.flush_persisted().await });
    r.engine.persist_reached(3, 3).await;
    r.event(json!({"event": "session_start", "reported_facts": ["agents", "project"]}))
        .await
        .unwrap();
    r.event(json!({"event": "session_facts", "title": "Rig the storm shutters"}))
        .await
        .unwrap();
    held.execute_batch("COMMIT").unwrap();
    first.join().unwrap();
    r.engine.flush_persisted().await;
    assert_eq!(
        stored_title(&r, &session),
        Some(json!("Rig the storm shutters")),
        "the title reported behind the registration that left its kind out reached its row"
    );
}

#[tokio::test]
async fn a_refused_registration_of_another_instance_drops_no_stored_kind() {
    let mut r = rig(RigSpec::default()).await;
    let session = rig_session(&r);
    r.event(json!({"event": "session_facts", "title": "Lash the deck cargo"}))
        .await
        .unwrap();
    r.engine.flush_persisted().await;
    let refused = r
        .event(json!({
            "event": "session_start",
            "instance_id": "second",
            "reported_facts": []
        }))
        .await
        .expect_err("a second instance that names none it replaces is refused");
    assert_eq!(
        (refused.status, refused.code.as_str()),
        (409, "instance_conflict")
    );
    r.engine.flush_persisted().await;
    assert_eq!(
        stored_title(&r, &session),
        Some(json!("Lash the deck cargo")),
        "the refused registration that named no kind left the stored title in place"
    );
}

#[tokio::test]
async fn a_sessions_title_and_its_project_set_rest_in_two_rows_each_with_its_own_members() {
    let mut r = rig(on_disk()).await;
    let session = rig_session(&r);
    r.event(json!({
        "event": "session_facts",
        "title": "Trim the mainsail",
        "branch": "harbor-survey"
    }))
    .await
    .unwrap();
    r.engine.flush_persisted().await;
    assert_eq!(
        (
            stored_set(&r, &session, FactsKind::Title, ""),
            stored_set(&r, &session, FactsKind::Project, "")
        ),
        (
            Some(json!({"title": "Trim the mainsail"})),
            Some(json!({
                "branch": "harbor-survey",
                "cwd": null,
                "project": null,
                "repository": null,
                "repository_root": null
            }))
        ),
        "each row holds the members of its own kind and no other"
    );
}

#[tokio::test]
async fn a_title_reported_once_stays_while_its_session_reports_past_the_cutoff() {
    let mut r = rig(RigSpec::default()).await;
    let session = rig_session(&r);
    let reported_at = r.now();
    r.event(json!({"event": "session_facts", "title": "Survey the breakwater"}))
        .await
        .unwrap();
    // The title lands in a save of its own ahead of the heartbeat, so only a later save's stamp moves its instant.
    r.engine.persist_reached(3, 3).await;
    r.engine.flush_persisted().await;
    r.advance(MINUTE);
    r.event(heartbeat()).await.unwrap();
    let heartbeat_at = r.now();
    r.advance(20_000);
    r.engine.flush_persisted().await;
    let title = || async {
        r.engine
            .stored_facts(vec![session.clone()])
            .await
            .remove(&session)
            .and_then(|stored| stored.session.title().map(str::to_string))
    };
    r.h.store
        .purge_companion_facts(None, reported_at + 30_000)
        .unwrap();
    assert_eq!(
        title().await.as_deref(),
        Some("Survey the breakwater"),
        "the title ages from its session's last report and not from its own"
    );
    r.h.store
        .purge_companion_facts(None, heartbeat_at + 1)
        .unwrap();
    assert_eq!(
        title().await,
        None,
        "the title leaves once the cutoff passes its session's last report"
    );
}

/// The wasted probes the ledger has settled, by reason, once `probes` of them have landed.
async fn wasted(r: &Rig, probes: u64) -> BTreeMap<String, u64> {
    wait_for(Duration::from_secs(30), || {
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

/// Two sessions under the default limits: `session` with its main agent and a recipe probed once, saved first, and
/// `newer`, saved a minute later, with its main agent's recipe and a subagent that retains nothing. Answers the two
/// recipes' sizes, the older first.
async fn two_sessions() -> (Rig, usize, usize) {
    let mut r = rig(on_disk()).await;
    r.observe("main").await;
    // One probe the next request has not answered yet, which a disposal settles as wasted.
    r.advance_attended(270_000).await;
    r.step().await;
    let older = r.engine.stats().warming.retained_bytes;
    r.engine.flush_persisted().await;
    r.advance(MINUTE);
    r.event(session_start("newer")).await.unwrap();
    r.event(json!({"event": "agent_start", "session_id": "newer", "agent_id": "sub"}))
        .await
        .unwrap();
    observe_on(&mut r, "newer", "main").await;
    let newer = r.engine.stats().warming.retained_bytes - older;
    r.engine.flush_persisted().await;
    assert_eq!((saved_sessions(&r).len(), saved_agents(&r).len()), (2, 3));
    (r, older, newer)
}

#[tokio::test]
async fn a_restart_under_smaller_limits_restores_the_newest_rows_and_disposes_the_rest() {
    let rows = |r: &Rig| {
        saved_agents(r)
            .iter()
            .map(|a| (a.session_id.clone(), a.agent_id.clone(), a.body.is_some()))
            .collect::<Vec<_>>()
    };
    let row =
        |session: &str, agent: &str, recipe: bool| (session.to_string(), agent.to_string(), recipe);

    for limits in [
        SessionCaps {
            max_sessions: 1,
            ..SessionCaps::default()
        },
        SessionCaps {
            max_agents: 2,
            ..SessionCaps::default()
        },
    ] {
        let (r, _, newer) = two_sessions().await;
        let second = r
            .restart_with_limits(limits.clone(), KeepaliveLimits::default())
            .await;
        second.load_persisted().await;
        let stats = second.stats();
        assert_eq!(
            (
                stats.sessions.entries,
                stats.sessions.agents,
                stats.warming.retained_bytes
            ),
            (1, 2, newer),
            "{limits:?}"
        );
        assert_eq!(
            second.reasons().get(&Reason::Evicted).copied(),
            Some(1),
            "{limits:?}"
        );
        assert_eq!(
            wasted(&r, 1).await,
            BTreeMap::from([("evicted".to_string(), 1)]),
            "the disposed recipe's probe is wasted under the eviction: {limits:?}"
        );
        assert_eq!(
            rows(&r),
            vec![row("newer", "main", true), row("newer", "sub", false)],
            "{limits:?}"
        );
        assert_eq!(
            saved_sessions(&r)
                .iter()
                .map(|s| s.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["newer"],
            "{limits:?}"
        );
    }

    let (r, older, newer) = two_sessions().await;
    let limits = KeepaliveLimits {
        max_retained_bytes: older + newer - 1,
    };
    let second = r.restart_with_limits(SessionCaps::default(), limits).await;
    second.load_persisted().await;
    let stats = second.stats();
    assert_eq!(
        (
            stats.sessions.entries,
            stats.sessions.agents,
            stats.warming.retained_bytes
        ),
        (2, 3, newer)
    );
    assert_eq!(second.reasons().get(&Reason::Evicted).copied(), Some(1));
    assert_eq!(
        wasted(&r, 1).await,
        BTreeMap::from([("evicted".to_string(), 1)])
    );
    assert_eq!(
        second.recipes_for("desktop", "newer"),
        vec!["main".to_string()]
    );
    assert!(second.recipes_for("desktop", "session").is_empty());
    assert_eq!(
        rows(&r),
        vec![
            row("newer", "main", true),
            row("newer", "sub", false),
            row("session", "main", false)
        ],
        "the older recipe's prompt left the registry with it"
    );
}

/// A session holds a slot from its first real request, so a restart under a session cap of one restores the session
/// that sent one with its recipe, though the load reaches the newer session that registered and sent nothing first,
/// and the newer one keeps its entry.
#[tokio::test]
async fn a_restart_under_the_session_cap_restores_the_session_that_sent_a_request_beside_a_newer_one_that_sent_none(
) {
    let mut r = rig(on_disk()).await;
    r.observe("main").await;
    r.engine.flush_persisted().await;
    r.advance(MINUTE);
    r.event(session_start("newer")).await.unwrap();
    r.engine.flush_persisted().await;
    let second = r
        .restart_with_limits(
            SessionCaps {
                max_sessions: 1,
                ..SessionCaps::default()
            },
            KeepaliveLimits::default(),
        )
        .await;
    second.load_persisted().await;
    assert_eq!(
        (
            second.recipes_for("desktop", "session"),
            second.stats().sessions.entries
        ),
        (vec!["main".to_string()], 2)
    );
}

/// A session row an earlier build saved carries no `slotted`, and that build wrote the request time only at a request
/// that took the slots, so a session that sent a request before the upgrade loads holding its slot: under a session cap
/// of one, a second session's first request is refused.
#[tokio::test]
async fn a_saved_session_without_its_slot_member_holds_its_slot_where_it_carries_a_request_time() {
    let mut r = rig(on_disk()).await;
    r.event(session_start("second"))
        .await
        .expect("the default caps leave room for a second session");
    r.observe("main").await;
    r.engine.flush_persisted().await;
    let mut row = saved_sessions(&r)
        .into_iter()
        .find(|row| row.session_id == "session")
        .expect("the rig's session row");
    let mut payload: serde_json::Value = serde_json::from_str(&row.payload).unwrap();
    payload
        .as_object_mut()
        .expect("the payload is an object")
        .remove("slotted")
        .expect("this build saves the member");
    row.payload = payload.to_string();
    r.h.store
        .save_engine_state(&EngineBatch {
            sessions: vec![row],
            ..EngineBatch::default()
        })
        .unwrap();
    let second = r
        .restart_with_limits(
            SessionCaps {
                max_sessions: 1,
                ..SessionCaps::default()
            },
            KeepaliveLimits::default(),
        )
        .await;
    second.load_persisted().await;
    let BusEvent::RequestObserved(mut observed) = r.observed("first", "main", BODY) else {
        unreachable!("observed builds an observation")
    };
    observed.session_id = Some("second".into());
    second.observe(BusEvent::RequestObserved(observed));
    let cause = second
        .recipes()
        .into_iter()
        .find(|line| line.session_id == "second" && line.agent_id == "main")
        .map(|line| line.cause);
    assert_eq!(
        cause,
        Some(Reason::SessionLimit),
        "the restored session holds the one slot"
    );
}

/// A request refused the slot saves the session's request time, so a refused session that comes back from a restart is
/// held until one expiry after its newest refused request, as the engine that refused it holds it, and leaves then.
#[tokio::test]
async fn a_refused_session_restored_by_a_restart_leaves_one_expiry_after_its_newest_refused_request(
) {
    let mut r = rig(RigSpec {
        session_caps: SessionCaps {
            max_sessions: 1,
            ..SessionCaps::default()
        },
        ..on_disk()
    })
    .await;
    r.event(session_start("other"))
        .await
        .expect("the rig's session has sent nothing, so it holds no slot");
    r.observe("main").await;
    r.engine.flush_persisted().await;
    // Inside the lease, so no lapse saves the session's row beside the refused requests.
    r.advance(30_000);
    observe_on(&mut r, "other", "main").await;
    r.advance(30_000);
    observe_on(&mut r, "other", "main").await;
    r.engine.flush_persisted().await;
    let second = r.restart().await;
    second.load_persisted().await;
    let cause = || {
        second
            .recipes()
            .into_iter()
            .find(|line| line.session_id == "other" && line.agent_id == "main")
            .map(|line| line.cause)
    };
    r.advance(DAY - 1);
    assert_eq!(
        cause(),
        Some(Reason::SessionLimit),
        "one millisecond short of one expiry after the newest refused request"
    );
    r.advance(1);
    assert_eq!(cause(), None, "one expiry after the newest refused request");
}

/// A session the load admits can still carry more agents than the limit leaves room for: the subagent past it is
/// disposed on its own, and the probe it paid for and nobody answered settles under the eviction.
#[tokio::test]
async fn a_subagent_past_the_agent_limit_is_disposed_alone_and_its_unanswered_probe_settles() {
    let mut r = rig(on_disk()).await;
    r.event(json!({"event": "agent_start", "agent_id": "sub"}))
        .await
        .unwrap();
    r.observe("main").await;
    r.observe("sub").await;
    r.advance_attended(270_000).await;
    r.step().await;
    r.engine.flush_persisted().await;
    let main_bytes = {
        let agents = saved_agents(&r);
        agents
            .iter()
            .find(|a| a.agent_id == "main")
            .map(|a| a.body_size)
            .unwrap()
    };
    assert_eq!(
        (
            saved_unanswered(&r, "session", "main"),
            saved_unanswered(&r, "session", "sub")
        ),
        (1, 1)
    );
    let second = r
        .restart_with_limits(
            SessionCaps {
                max_agents: 1,
                ..SessionCaps::default()
            },
            KeepaliveLimits::default(),
        )
        .await;
    second.load_persisted().await;
    let stats = second.stats();
    assert_eq!(
        (
            stats.sessions.entries,
            stats.sessions.agents,
            stats.warming.retained_bytes
        ),
        (1, 1, main_bytes),
        "the session and its main agent load"
    );
    assert_eq!(second.reasons().get(&Reason::Evicted).copied(), Some(1));
    assert_eq!(
        wasted(&r, 1).await,
        BTreeMap::from([("evicted".to_string(), 1)]),
        "the subagent's probe is wasted under the eviction, and main's stays unanswered"
    );
    second.flush_persisted().await;
    assert_eq!(saved_unanswered(&r, "session", "main"), 1);
    assert_eq!(
        saved_agents(&r)
            .iter()
            .map(|a| a.agent_id.as_str())
            .collect::<Vec<_>>(),
        vec!["main"],
        "the disposed subagent's row left the registry"
    );
}

/// An `agent_start` for `agent` on the rig's own session, with the refusal's code and status.
async fn agent_start(r: &mut Rig, agent: &str) -> Result<(), (String, u16)> {
    r.event(json!({"event": "agent_start", "agent_id": agent}))
        .await
        .map(|_| ())
        .map_err(|refused| (refused.code, refused.status))
}

/// A released record holds its slot of the agent cap until it leaves one cache TTL after its release, so a session at
/// the cap admits a new agent once its released records have left.
#[tokio::test]
async fn a_session_at_the_agent_limit_frees_a_slot_when_its_released_record_leaves() {
    let mut r = rig(RigSpec {
        session_caps: SessionCaps {
            max_agents: 2,
            ..SessionCaps::default()
        },
        ..RigSpec::default()
    })
    .await;
    r.observe("main").await;
    agent_start(&mut r, "worker").await.unwrap();
    assert_eq!(
        agent_start(&mut r, "other").await,
        Err(("agent_limit".to_string(), 429)),
        "main and the worker fill the cap"
    );
    r.state("worker", "released").await;
    assert_eq!(
        agent_start(&mut r, "other").await,
        Err(("agent_limit".to_string(), 429)),
        "the released record still holds its slot"
    );
    r.advance_attended(3_599_999).await;
    assert_eq!(
        (
            agent_start(&mut r, "other").await,
            r.engine.stats().sessions.agents
        ),
        (Err(("agent_limit".to_string(), 429)), 2),
        "until one cache TTL has passed since the release"
    );
    r.advance_attended(1).await;
    r.step().await;
    assert_eq!(
        r.engine.stats().sessions.agents,
        1,
        "the leave frees the released record's slot"
    );
    assert_eq!(agent_start(&mut r, "other").await, Ok(()));
    assert_eq!(r.engine.stats().sessions.agents, 2);
}

/// A flush saves a probe nobody answered with the eight members read after a restart: its coverage, entry, unit and
/// model for the credit, and its usage, its account tier, its exchange instant and its band for its settlement. A
/// restarted engine then loads the agent with the view the first engine gives.
#[tokio::test]
async fn a_flush_saves_an_unanswered_probe_with_what_its_credit_and_its_settlement_read_and_a_restart_loads_it(
) {
    const OAUTH: &str = "sk-ant-oat01-OOOO";
    let mut r = rig(RigSpec {
        accounts: vec![oauth("o", OAUTH, 0)],
        tier: Some("default_claude_max_20x"),
        ..on_disk()
    })
    .await;
    r.observe("main").await;
    r.advance_attended(270_000).await;
    r.step().await;
    r.engine.flush_persisted().await;
    assert_eq!(
        saved_unanswered(&r, "session", "main"),
        1,
        "the saved row holds the one unanswered probe"
    );
    let payload = saved_of(&r.h.store, "session", "main").unwrap();
    let probe = &payload["unanswered"][0];
    assert_eq!(
        (
            payload["unanswered"].as_array().map(Vec::len),
            probe["covered"].as_bool(),
            probe["expires"].as_i64(),
            probe["unit"].as_str().is_some(),
            probe["model"].as_str(),
        ),
        (
            Some(1),
            Some(true),
            Some(NOW + 270_000 + 300_000),
            true,
            Some("claude-sonnet-4-5")
        ),
        "{probe}"
    );
    assert_eq!(
        (
            probe["usage"]["read"].as_u64(),
            probe["tier"].as_str(),
            probe["at"].as_i64(),
            probe["band"].as_str(),
        ),
        (
            Some(2_000),
            Some("default_claude_max_20x"),
            Some(NOW + 270_000),
            Some("standard")
        ),
        "{probe}"
    );
    // The probe's own request id, which its ledger row carries, so a settlement can be told from a repeat of it.
    assert!(
        probe["id"].as_str().is_some_and(|id| !id.is_empty()),
        "the probe names its request: {probe}"
    );
    let second = r.restart().await;
    second.load_persisted().await;
    assert_eq!(
        view_of(&second, "session", "main").unwrap(),
        r.view("main"),
        "the restarted engine's view of the agent equals the first engine's"
    );
}

/// A released record's leave rides its saved row, so a restart restores the record with it and the first tick past
/// the leave removes the record and its row.
#[tokio::test]
async fn a_restored_released_record_leaves_at_the_first_tick_past_its_saved_leave() {
    let mut r = rig(on_disk()).await;
    r.observe("main").await;
    agent_start(&mut r, "sub").await.unwrap();
    let released_at = r.now();
    r.state("sub", "released").await;
    r.engine.flush_persisted().await;
    assert_eq!(
        saved_field(&r, "session", "sub", "leaves_at"),
        serde_json::json!(released_at + 3_600_000),
        "the save writes the leave"
    );
    let second = r.restart().await;
    second.load_persisted().await;
    r.advance(3_599_999);
    second.step().await;
    assert_eq!(
        lifecycle_of(&second, "session", "sub"),
        Some(Lifecycle::Released),
        "the restored record is held until its leave"
    );
    r.advance(1);
    second.step().await;
    assert!(
        lifecycle_of(&second, "session", "sub").is_none(),
        "the first tick past the leave removes the record"
    );
    assert_eq!(second.stats().sessions.agents, 1);
    second.flush_persisted().await;
    assert_eq!(
        saved_agents(&r)
            .iter()
            .map(|a| a.agent_id.as_str())
            .collect::<Vec<_>>(),
        vec!["main"],
        "the record's row left the registry with it"
    );
}

/// A saved released record whose leave passed while the router was down is left out at the load, as the first tick's
/// sweep would have removed it, so it takes no slot of the agent cap: with two such records saved beside main and a
/// live agent, the live agent loads under a cap of three, and a new agent registers at once.
#[tokio::test]
async fn a_saved_released_record_whose_leave_passed_while_the_router_was_down_does_not_load() {
    let mut r = rig(on_disk()).await;
    r.observe("main").await;
    agent_start(&mut r, "worker").await.unwrap();
    r.engine.flush_persisted().await;
    // Saved after the worker, so the load reaches the released records before it.
    r.advance_attended(1_000).await;
    for sub in ["sub-a", "sub-b"] {
        agent_start(&mut r, sub).await.unwrap();
        r.state(sub, "released").await;
    }
    r.engine.flush_persisted().await;
    r.advance(3_600_000);
    r.engine = r
        .restart_with_limits(
            SessionCaps {
                max_agents: 3,
                ..SessionCaps::default()
            },
            KeepaliveLimits::default(),
        )
        .await;
    r.engine.load_persisted().await;
    let loaded = |r: &Rig, agent: &str| r.lifecycle(agent).is_some();
    assert_eq!(
        (
            loaded(&r, "sub-a"),
            loaded(&r, "sub-b"),
            loaded(&r, "worker"),
            r.engine.stats().sessions.agents
        ),
        (false, false, true, 2),
        "the released records do not load, and the live worker loads under the cap"
    );
    r.reregister().await;
    assert_eq!(
        agent_start(&mut r, "other").await,
        Ok(()),
        "a new agent registers under the cap at once"
    );
    assert_eq!(r.engine.stats().sessions.agents, 3);
    r.engine.flush_persisted().await;
    let mut saved: Vec<String> = saved_agents(&r)
        .into_iter()
        .map(|agent| agent.agent_id)
        .collect();
    saved.sort();
    assert_eq!(
        saved,
        vec!["main", "other", "worker"],
        "the released records' rows left the registry"
    );
}

/// A saved released record whose payload carries no leave loads with none and stays past the hour a leave would have
/// ended, as serde reads a missing `Option` member.
#[tokio::test]
async fn a_saved_released_record_without_a_leave_loads_with_none_and_stays() {
    let mut r = rig(on_disk()).await;
    agent_start(&mut r, "sub").await.unwrap();
    r.state("sub", "released").await;
    r.engine.flush_persisted().await;
    let (sessions, mut agents) = (
        saved_sessions(&r),
        r.h.store.load_engine_state().unwrap().agents,
    );
    for row in agents.iter_mut().filter(|row| row.agent_id == "sub") {
        let mut payload: serde_json::Value = serde_json::from_str(&row.payload).unwrap();
        payload
            .as_object_mut()
            .expect("the payload is an object")
            .remove("leaves_at");
        row.payload = payload.to_string();
    }
    r.h.store
        .save_engine_state(&EngineBatch {
            sessions,
            agents,
            ..EngineBatch::default()
        })
        .unwrap();
    let second = r.restart().await;
    second.load_persisted().await;
    assert!(
        lifecycle_of(&second, "session", "sub").is_some(),
        "the record without a leave loads"
    );
    r.advance(3_600_000);
    second.step().await;
    assert_eq!(
        lifecycle_of(&second, "session", "sub"),
        Some(Lifecycle::Released),
        "a record with no leave stays"
    );
}

/// A flush saves a probe the idle limit left pending with what its settlement and a later turn read: the kind and the
/// overage its answer carried, its coverage and unit, and its pending label. A restarted engine then loads the agent
/// with the view the first engine gives.
#[tokio::test]
async fn a_flush_saves_a_pending_probe_with_its_funding_coverage_unit_and_label_and_a_restart_loads_it(
) {
    const OAUTH: &str = "sk-ant-oat01-OOOO";
    let mut r = rig(RigSpec {
        accounts: vec![oauth("o", OAUTH, 0)],
        pool: PoolSpec {
            overage_policy: phoebis_store::OveragePolicy::Allow,
            ..PoolSpec::default()
        },
        tier: Some("default_claude_max_5x"),
        policy: KeepalivePolicy {
            enabled: true,
            max_idle_seconds: 500,
            ..KeepalivePolicy::default()
        },
        ..on_disk()
    })
    .await;
    r.observe("main").await;
    // The probe reads the prefix back and its answer reports overage in use.
    let Respond::Json {
        body: read_back, ..
    } = probe_answer(2_000, 0)
    else {
        unreachable!("the standing probe answer is a JSON body")
    };
    r.up.push_credential_rule(
        OAUTH,
        "/v1/messages",
        Respond::Json {
            status: 200,
            headers: vec![(
                "anthropic-ratelimit-unified-overage-in-use".into(),
                "true".into(),
            )],
            body: read_back,
        },
    );
    r.advance(270_000);
    r.step().await;
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        r.view("main").cause,
        Reason::IdleLimit,
        "the idle limit left the probe pending"
    );
    r.engine.flush_persisted().await;
    assert_eq!(
        saved_unanswered(&r, "session", "main"),
        1,
        "the saved row holds the one probe the idle limit left pending"
    );
    let payload = saved_of(&r.h.store, "session", "main").unwrap();
    let kept = &payload["kept"][0];
    assert_eq!(
        (
            payload["kept"].as_array().map(Vec::len),
            kept["kind"].as_str(),
            kept["in_overage"].as_bool(),
            kept["covered"].as_bool(),
            kept["unit"].as_str(),
            kept["pending"].as_str(),
        ),
        (
            Some(1),
            Some("oauth"),
            Some(true),
            Some(true),
            Some("usd"),
            Some("idle_limit")
        ),
        "{kept}"
    );
    // A probe in overage settles in dollars, so the settlement read no account tier.
    assert_eq!(
        (
            kept["usage"]["read"].as_u64(),
            kept["tier"].is_null(),
            kept["at"].as_i64(),
            kept["band"].as_str(),
        ),
        (Some(2_000), true, Some(NOW + 270_000), Some("standard")),
        "{kept}"
    );
    let second = r.restart().await;
    second.load_persisted().await;
    assert_eq!(
        view_of(&second, "session", "main").unwrap(),
        r.view("main"),
        "the restarted engine's view of the agent equals the first engine's"
    );
}

/// A flush saves the class that a usage-based seat's facts carried on its observation, its unanswered probe and the
/// probe the idle limit left pending, which its probes settle under. A restarted engine loads the agent with the view
/// the first engine gives, and it loads a payload written before the class rode the records too.
#[tokio::test]
async fn a_flush_saves_the_class_of_a_seats_records_and_a_payload_from_before_the_class_loads() {
    const OAUTH: &str = "sk-ant-oat01-OOOO";
    let mut r = rig(RigSpec {
        accounts: vec![oauth("o", OAUTH, 0)],
        tier: Some("default_claude_zero"),
        policy: KeepalivePolicy {
            enabled: true,
            max_idle_seconds: 500,
            ..KeepalivePolicy::default()
        },
        ..on_disk()
    })
    .await;
    // The record's kind and class at `pointer` in a saved agent's payload.
    let class_at = |payload: &str, pointer: &str| {
        let payload: serde_json::Value = serde_json::from_str(payload).unwrap();
        let record = payload.pointer(pointer).cloned().unwrap_or_default();
        (
            record["kind"].as_str().map(String::from),
            record["usage_based"].as_bool(),
        )
    };
    // A payload from before the class: the member leaves each record it rides on.
    let from_before = |rows: &[KeepaliveAgentRow], pointers: &[&str]| {
        let mut rows = rows.to_vec();
        let mut payload: serde_json::Value = serde_json::from_str(&rows[0].payload).unwrap();
        for pointer in pointers {
            payload
                .pointer_mut(pointer)
                .and_then(serde_json::Value::as_object_mut)
                .unwrap()
                .remove("usage_based");
        }
        rows[0].payload = payload.to_string();
        rows
    };
    let seat = (Some("oauth".to_string()), Some(true));
    r.observe("main").await;
    r.up.push_credential_rule(OAUTH, "/v1/messages", probe_answer(2_000, 0));
    r.advance(270_000);
    r.step().await;
    r.engine.flush_persisted().await;
    let (open_sessions, open_agents) = (saved_sessions(&r), saved_agents(&r));
    assert_eq!(
        (
            class_at(&open_agents[0].payload, "/current"),
            class_at(&open_agents[0].payload, "/unanswered/0")
        ),
        (seat.clone(), seat.clone()),
        "the save writes the class the selection and the probe's answer carried"
    );
    r.h.store
        .save_engine_state(&EngineBatch {
            sessions: open_sessions,
            warming_agents: from_before(&open_agents, &["/current", "/unanswered/0"]),
            ..EngineBatch::default()
        })
        .unwrap();
    let older = r.restart().await;
    older.load_persisted().await;
    assert_eq!(
        older.stats().warming.retained_bytes,
        r.engine.stats().warming.retained_bytes,
        "an observation from before the class loads"
    );
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        r.view("main").cause,
        Reason::IdleLimit,
        "the idle limit left the probe pending"
    );
    r.engine.flush_persisted().await;
    let (sessions, agents) = (saved_sessions(&r), saved_agents(&r));
    assert_eq!(
        class_at(&agents[0].payload, "/kept/0"),
        seat,
        "the pending probe keeps the class its answer carried"
    );
    assert_eq!(saved_unanswered(&r, "session", "main"), 1);
    let second = r.restart().await;
    second.load_persisted().await;
    assert_eq!(
        view_of(&second, "session", "main").unwrap(),
        r.view("main"),
        "the restarted engine's view of the agent equals the first engine's"
    );
    r.h.store
        .save_engine_state(&EngineBatch {
            sessions,
            warming_agents: from_before(&agents, &["/kept/0"]),
            ..EngineBatch::default()
        })
        .unwrap();
    let third = r.restart().await;
    third.load_persisted().await;
    assert!(
        view_of(&third, "session", "main").is_some(),
        "a pending probe from before the class loads"
    );
}

/// A probe saved with a null speed, as a payload an earlier build saved holds one whose answer named no speed, comes
/// back from a restart and settles at the standard speed.
#[tokio::test]
async fn a_saved_probe_whose_speed_reads_null_restores_and_settles_at_standard() {
    let mut r = rig(on_disk()).await;
    r.observe("main").await;
    r.advance_attended(270_000).await;
    r.step().await;
    r.engine.flush_persisted().await;
    assert_eq!(saved_unanswered(&r, "session", "main"), 1);
    let (sessions, mut agents) = (saved_sessions(&r), saved_agents(&r));
    let mut payload: serde_json::Value = serde_json::from_str(&agents[0].payload).unwrap();
    payload["unanswered"][0]["usage"]["speed"] = serde_json::Value::Null;
    agents[0].payload = payload.to_string();
    r.h.store
        .save_engine_state(&EngineBatch {
            sessions,
            warming_agents: agents,
            ..EngineBatch::default()
        })
        .unwrap();
    r.engine = r.restart().await;
    r.engine.load_persisted().await;
    // The companion takes the restored session back and reports main released, which settles the probe as wasted, so
    // the one wasted row below is the probe saved with a null speed coming back.
    r.reregister().await;
    r.state("main", "released").await;
    r.engine.flush_persisted().await;
    let speeds = wait_for(Duration::from_secs(30), || {
        let rows =
            r.db.read_blocking(|c| {
                read_rollups(c, hour_of(NOW) - 3_600, i64::MAX, &RollupSelect::default())
            })
            .unwrap();
        let speeds: Vec<String> = rows
            .iter()
            .filter(|row| row.kind == "probe" && row.wasted_probes > 0)
            .map(|row| row.speed.clone())
            .collect();
        (!speeds.is_empty()).then_some(speeds)
    })
    .await;
    assert_eq!(
        speeds,
        vec!["standard".to_string()],
        "the restored probe settled at the standard speed"
    );
}

/// A probe left unanswered at a save settles as wasted after the load, and a due probe the restored recipe withholds
/// files its skip. Both rows key the client and the user agent of the request the recipe came from, so a restarted
/// router files neither under the reserved ones.
#[tokio::test]
async fn a_restored_recipes_settlement_and_skip_key_the_client_and_the_user_agent_of_its_request() {
    let mut r = rig(on_disk()).await;
    let BusEvent::RequestObserved(mut observed) = r.observed("first", "main", BODY) else {
        unreachable!("the rig builds an observation")
    };
    observed.client.class = "claude-code/desk".into();
    for (name, value) in &mut observed.headers {
        if name == "user-agent" {
            *value = "claude-cli/2.1.301 (external, desk)".into();
        }
    }
    r.pin("main", "claude-sonnet-4-5", "a").await;
    r.engine.observe(BusEvent::RequestObserved(observed));
    r.engine.observe(r.selected("first", "a"));
    let written = Usage {
        cache_write_5m_tokens: Some(2_000),
        ..Default::default()
    };
    r.engine
        .observe(BusEvent::ResponseChunk(phoebis_bus::ResponseChunk {
            request_id: "first".into(),
            attempt: 1,
            index: 0,
            body: Default::default(),
            phase: Some(phoebis_bus::Phase::MessageStart {
                usage: written.clone(),
            }),
        }));
    r.engine
        .observe(r.complete("first", Outcome::Succeeded, Some(written)));
    r.advance_attended(270_000).await;
    r.step().await;
    r.engine.flush_persisted().await;
    assert_eq!(
        saved_unanswered(&r, "session", "main"),
        1,
        "one probe left unanswered"
    );
    r.engine = r.restart().await;
    r.engine.load_persisted().await;
    r.reregister().await;
    // The events route refuses the key of a disabled pool, so the companion sends no report there: the heartbeats go
    // out ahead of the disable, and the release behind the pool's enable.
    r.advance_attended(270_000).await;
    let mut pool = r.h.store.pool("main").unwrap().unwrap();
    pool.enabled = false;
    r.h.store
        .update_pool(&pool, pool.updated_at, "test")
        .unwrap();
    r.h.router.reload().await.unwrap();
    r.step().await;
    assert_eq!(
        r.view("main").cause,
        Reason::PoolDisabled,
        "the restored recipe's due probe was withheld"
    );
    let mut pool = r.h.store.pool("main").unwrap().unwrap();
    pool.enabled = true;
    r.h.store
        .update_pool(&pool, pool.updated_at, "test")
        .unwrap();
    r.h.router.reload().await.unwrap();
    r.state("main", "released").await;
    r.ledger.clone().shutdown().await.unwrap();
    let rows =
        r.db.read_blocking(|c| {
            read_rollups(c, hour_of(NOW) - 3_600, i64::MAX, &RollupSelect::default())
        })
        .unwrap();
    let keys = |held: fn(&phoebis_ledger::RollupRow) -> bool| {
        rows.iter()
            .filter(|row| held(row))
            .map(|row| (row.client.clone(), row.user_agent.clone()))
            .collect::<Vec<_>>()
    };
    let recipe = [(
        "claude-code/desk".to_string(),
        "claude-cli/2.1.301".to_string(),
    )];
    assert_eq!(
        keys(|row| row.wasted_probes > 0),
        recipe,
        "the settlement: {rows:?}"
    );
    assert_eq!(
        keys(|row| row.probes_skipped > 0),
        recipe,
        "the skip: {rows:?}"
    );
}

/// A saved session of record version 2 carries a probe's dollars in place of its usage, its account tier, its
/// instant and its band, so nothing would settle its waste under the cells and the revision the probe ran at: the
/// load drops it whole.
#[tokio::test]
async fn a_saved_session_of_record_version_two_is_dropped_at_the_load_with_one_line() {
    let r = rig(on_disk()).await;
    let kept = serde_json::json!({
        "key_generation": r.generation("desktop"),
        "pool_id": "main",
        "canonical_model": "claude-sonnet-4-5",
        "account": "a",
        "fast": false,
        "cost": "0.0006",
        "entry_expires": NOW + 570_000,
        "kind": "oauth",
        "in_overage": false,
        "covered": false,
        "unit": "usd",
        "pending": null,
    });
    dropped_at_the_load_with_one_line(r, kept).await;
}

/// The rig's session as a save writes it, its main agent holding no recipe, rewritten to record version 2 with
/// `kept` as its one kept probe, then loaded by a restart: nothing comes back, nothing settles, and one warn line
/// names it.
async fn dropped_at_the_load_with_one_line(r: Rig, kept: serde_json::Value) {
    r.engine.flush_persisted().await;
    let (mut sessions, mut agents) = (saved_sessions(&r), saved_agents(&r));
    assert_eq!((sessions.len(), agents.len()), (1, 1));
    let mut payload: serde_json::Value = serde_json::from_str(&agents[0].payload).unwrap();
    payload["kept"] = serde_json::json!([kept]);
    agents[0].payload = payload.to_string();
    sessions[0].record_version = 2;
    agents[0].record_version = 2;
    let mut lifecycles = r.h.store.load_engine_state().unwrap().agents;
    lifecycles[0].record_version = 2;
    r.h.store
        .save_engine_state(&EngineBatch {
            sessions,
            agents: lifecycles,
            warming_agents: agents,
            ..EngineBatch::default()
        })
        .unwrap();
    let captured = Captured::default();
    let second = r.restart().await;
    {
        let _guard = install_at(&captured, tracing::Level::INFO);
        second.load_persisted().await;
    }
    let stats = second.stats();
    assert_eq!(
        (stats.sessions.entries, stats.sessions.agents),
        (0, 0),
        "the older session is not restored"
    );
    assert_eq!(
        (saved_sessions(&r).len(), saved_agents(&r).len()),
        (0, 0),
        "both its rows left the registry"
    );
    settle_ledger(&r, &mut 0).await;
    assert_eq!(wasted_now(&r), BTreeMap::new(), "nothing settles");
    let text = captured.text();
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| line.contains("saved keepalive"))
        .collect();
    assert_eq!(
        lines.len(),
        1,
        "one line for the load, none per row: {text}"
    );
    assert!(
        lines[0].contains("WARN")
            && lines[0].contains("older record version")
            && lines[0].contains("dropped=1"),
        "{text}"
    );
}

#[tokio::test]
async fn a_loaded_recipe_probes_warm_when_due() {
    let mut r = rig(on_disk()).await;
    r.observe("main").await;
    r.engine.flush_persisted().await;
    let second = r.restart().await;
    second.load_persisted().await;
    let sent = r.up.requests().len();
    r.advance(270_000);
    second.step().await;
    no_probe_running(&second).await;

    assert_eq!(
        r.up.requests().len(),
        sent + 1,
        "the restored recipe's probe reached the upstream"
    );
    let main = view_of(&second, "session", "main").unwrap();
    second.flush_persisted().await;
    let saved = saved_of(&r.h.store, "session", "main").unwrap();
    assert_eq!(
        (main.probes_today, saved["cold"].clone(), main.cause),
        (1, serde_json::json!(0), Reason::Scheduled),
        "and read the whole prefix back: {main:?}"
    );
}

#[tokio::test]
async fn a_start_under_a_new_key_generation_leaves_one_saved_row_for_the_session() {
    let mut r = rig(on_disk()).await;
    r.observe("main").await;
    r.engine.flush_persisted().await;
    let old = r.generation("desktop");
    // A later holder of the key id registers the same session id, which replaces the earlier holder's session in place.
    // The operator deletes the key and creates it again, and the route reads the key off the router's snapshot, so the
    // router reloads behind the creation.
    const LATER_SECRET: &str = "phoebis-later-holder";
    let mut key = r.h.store.api_key("desktop").unwrap().unwrap();
    r.h.store.delete_api_key("desktop", "test").unwrap();
    key.secret_digest = r.h.store.api_key_digest(LATER_SECRET).to_vec();
    key.prefix = phoebis_store::key_prefix(LATER_SECRET);
    r.h.store.insert_api_key(&key, "test").unwrap();
    r.h.router.reload().await.unwrap();
    let later = r.generation("desktop");
    assert_ne!(
        later, old,
        "the key created again takes a generation of its own"
    );
    r.event_as(
        LATER_SECRET,
        json!({
            "event": "session_start",
            "session_id": "session",
            "instance_id": "later"
        }),
    )
    .await
    .unwrap();
    r.engine.flush_persisted().await;

    assert_eq!(
        saved_sessions(&r)
            .into_iter()
            .map(|s| (s.session_id, s.key_generation))
            .collect::<Vec<_>>(),
        vec![("session".to_string(), later)],
        "the earlier holder's row went as the later one's was written"
    );
    assert!(saved_agents(&r).iter().all(|a| a.key_generation == later));
    let second = r.restart().await;
    second.load_persisted().await;
    assert_eq!(second.stats().sessions.entries, 1);
    assert_eq!(second.sessions()[0].key_generation, later);
}

#[tokio::test]
async fn an_expired_session_leaves_the_registry_at_the_next_flush() {
    let mut r = rig(RigSpec {
        session_expiry: Duration::from_secs(600),
        ..on_disk()
    })
    .await;
    r.observe("main").await;
    r.engine.flush_persisted().await;
    assert_eq!(saved_sessions(&r).len(), 1);
    r.advance(700_000);
    r.engine.expire();
    assert_eq!(r.engine.stats().sessions.entries, 0);
    r.engine.flush_persisted().await;
    assert!(saved_sessions(&r).is_empty());
    assert!(
        saved_agents(&r).is_empty(),
        "the session's agents and its recipe went with it"
    );
}

#[tokio::test]
async fn a_second_load_leaves_the_sessions_the_engine_holds_and_their_rows() {
    let mut r = rig(on_disk()).await;
    r.observe("main").await;
    r.engine.flush_persisted().await;
    let second = r.restart().await;
    second.load_persisted().await;
    second.load_persisted().await;
    second.flush_persisted().await;
    assert_eq!(
        (
            second.stats().sessions.entries,
            second.stats().sessions.agents
        ),
        (1, 1)
    );
    assert_eq!(
        (saved_sessions(&r).len(), saved_agents(&r).len()),
        (1, 1),
        "the second load filed no removal of a row the engine holds"
    );
}

/// A request can arrive between a restart and the mod's next registration, so the saved row carries what the last
/// registration said of the session's own option: the session that holds the warming off names its cause on its row
/// after the load, and the rig's session, which holds it on and sent nothing, names none.
#[tokio::test]
async fn a_restart_keeps_what_each_sessions_registration_said_of_its_cache_warming() {
    let mut r = rig(on_disk()).await;
    r.event(json!({"event": "session_start", "session_id": "quiet", "cache_warming": false}))
        .await
        .unwrap();
    r.engine.flush_persisted().await;
    let second = r.restart().await;
    second.load_persisted().await;
    let causes: BTreeMap<String, Option<Reason>> = second
        .sessions()
        .into_iter()
        .map(|view| (view.session_id, view.keepalive.cause))
        .collect();
    assert_eq!(
        causes,
        BTreeMap::from([
            ("quiet".to_string(), Some(Reason::SessionOptedOut)),
            ("session".to_string(), None)
        ])
    );
}

/// A registration that turns the session's warming off drops the held prompt behind an answer that the mod takes as
/// done. It asks for a save of its own even where its list names every kind and so files no leave: a crash ahead of
/// the next periodic save would otherwise load the prompt back with the warming on.
#[tokio::test]
async fn a_registration_that_turns_the_warming_off_saves_the_drop_of_the_held_prompt() {
    let mut r = rig(on_disk()).await;
    r.observe("main").await;
    // The registry holds the prompt and the warming on, as a crash behind the registration would leave them.
    r.engine.flush_persisted().await;
    r.event(json!({"event": "session_start", "cache_warming": false}))
        .await
        .unwrap();
    // The rig's two flushes and the one above stand ahead of it. The bound is the failure path alone: a passing run
    // meets the registration's save at once.
    tokio::time::timeout(Duration::from_secs(10), r.engine.persist_reached(4, 4))
        .await
        .expect("the registration asks for a save");
    // The signal fires as the save reads its batch, so the case reads the registry for its landing and runs no save.
    wait_for(Duration::from_secs(10), || {
        saved_of(&r.h.store, "session", "main").filter(|saved| saved["current"].is_null())
    })
    .await;
    let second = r.restart().await;
    second.load_persisted().await;
    assert_eq!(
        (
            second.sessions()[0].keepalive.cause,
            second.recipes_for("desktop", "session")
        ),
        (Some(Reason::SessionOptedOut), Vec::<String>::new()),
        "the load reads the session's option off and no prompt held"
    );
}

#[tokio::test]
async fn a_session_whose_main_agent_cannot_be_read_is_dropped_whole() {
    let mut r = rig(on_disk()).await;
    r.event(json!({"event": "agent_start", "agent_id": "sub"}))
        .await
        .unwrap();
    r.observe("main").await;
    r.observe("sub").await;
    r.engine.flush_persisted().await;
    let damaged = registry(&r)
        .execute(
            "UPDATE keepalive_agents SET payload = 'unreadable' WHERE agent_id = 'main'",
            [],
        )
        .unwrap();
    assert_eq!(damaged, 1);
    let second = r.restart().await;
    second.load_persisted().await;

    let stats = second.stats();
    assert_eq!(
        (
            stats.sessions.entries,
            stats.sessions.agents,
            stats.warming.retained_bytes
        ),
        (0, 0, 0),
        "no session comes back without the main agent its reports name"
    );
    assert_eq!(
        (saved_sessions(&r).len(), saved_agents(&r).len()),
        (0, 0),
        "and its rows, the subagent's with it, are gone"
    );
}

#[tokio::test]
async fn an_agent_whose_warming_row_is_of_an_older_version_leaves_with_its_other_row_and_takes_mains_session(
) {
    let r = rig(on_disk()).await;
    let mut warming =
        r.h.store
            .load_engine_state()
            .unwrap()
            .warming_agents
            .into_iter()
            .find(|row| row.session_id == "session" && row.agent_id == "main")
            .expect("the registration's save holds main's row");
    warming.record_version = 2;
    r.h.store
        .save_engine_state(&EngineBatch {
            warming_agents: vec![warming],
            ..EngineBatch::default()
        })
        .unwrap();
    let second = r.restart().await;
    second.load_persisted().await;
    assert!(
        second.sessions().is_empty(),
        "main goes, and its session with it"
    );
    let state = r.h.store.load_engine_state().unwrap();
    assert_eq!(
        (
            state.sessions.len(),
            state.agents.len(),
            state.warming_agents.len()
        ),
        (0, 0, 0),
        "the session's row and both of main's rows left the registry"
    );
}

#[tokio::test]
async fn an_agent_beside_main_whose_session_engine_row_is_of_an_older_version_leaves_with_its_warming_row_and_the_session_stays(
) {
    let mut r = rig(on_disk()).await;
    agent_start(&mut r, "sub").await.unwrap();
    r.engine.flush_persisted().await;
    let mut lifecycle =
        r.h.store
            .load_engine_state()
            .unwrap()
            .agents
            .into_iter()
            .find(|row| row.session_id == "session" && row.agent_id == "sub")
            .expect("the flush saved the subagent's row");
    lifecycle.record_version = 2;
    r.h.store
        .save_engine_state(&EngineBatch {
            agents: vec![lifecycle],
            ..EngineBatch::default()
        })
        .unwrap();
    let second = r.restart().await;
    second.load_persisted().await;
    let state = r.h.store.load_engine_state().unwrap();
    assert_eq!(
        (
            state
                .sessions
                .iter()
                .map(|row| row.session_id.as_str())
                .collect::<Vec<_>>(),
            state
                .agents
                .iter()
                .map(|row| row.agent_id.as_str())
                .collect::<Vec<_>>(),
            state
                .warming_agents
                .iter()
                .map(|row| row.agent_id.as_str())
                .collect::<Vec<_>>()
        ),
        (vec!["session"], vec!["main"], vec!["main"]),
        "both of the subagent's rows left the registry, and the session's row and main's two rows stand"
    );
    assert_eq!(
        second
            .sessions()
            .into_iter()
            .map(|view| (
                view.session_id,
                view.lifecycles.into_keys().collect::<Vec<_>>()
            ))
            .collect::<Vec<_>>(),
        vec![("session".to_string(), vec!["main".to_string()])],
        "the session restores with main alone"
    );
}

#[tokio::test]
async fn an_agent_beside_main_with_its_warming_row_alone_leaves_the_registry_and_the_session_stays()
{
    let r = rig(on_disk()).await;
    let main =
        r.h.store
            .load_engine_state()
            .unwrap()
            .warming_agents
            .into_iter()
            .find(|row| row.session_id == "session" && row.agent_id == "main")
            .expect("the registration's save holds main's row");
    // A warming row whose agent holds no row of the session engine, with a sealed prompt that leaves with it.
    r.h.store
        .save_engine_state(&EngineBatch {
            warming_agents: vec![KeepaliveAgentRow {
                agent_id: "worker".into(),
                body_write: BodyWrite::Seal(b"{\"messages\":[]}".to_vec()),
                body_size: 15,
                ..main
            }],
            ..EngineBatch::default()
        })
        .unwrap();
    let second = r.restart().await;
    second.load_persisted().await;
    let state = r.h.store.load_engine_state().unwrap();
    assert_eq!(
        (
            state
                .sessions
                .iter()
                .map(|row| row.session_id.as_str())
                .collect::<Vec<_>>(),
            state
                .agents
                .iter()
                .map(|row| row.agent_id.as_str())
                .collect::<Vec<_>>(),
            state
                .warming_agents
                .iter()
                .map(|row| row.agent_id.as_str())
                .collect::<Vec<_>>()
        ),
        (vec!["session"], vec!["main"], vec!["main"]),
        "the lone row left the registry, and the session's row and main's two rows stand"
    );
    assert_eq!(
        second
            .sessions()
            .into_iter()
            .map(|view| (
                view.session_id,
                view.lifecycles.into_keys().collect::<Vec<_>>()
            ))
            .collect::<Vec<_>>(),
        vec![("session".to_string(), vec!["main".to_string()])],
        "the session restores with main alone"
    );
}

/// Lands a marker row and waits for it. The ledger writes its records in order, so everything queued before the marker
/// has landed once it shows, and a negative read after it proves a write did not happen rather than that it had not
/// happened yet. `marks` counts the markers this test landed so far.
async fn settle_ledger(r: &Rig, marks: &mut u64) {
    *marks += 1;
    r.ledger.record(Record::Keepalive(KeepaliveDelta {
        at: NOW,
        key_id: "desktop".into(),
        pool_id: "main".into(),
        account_id: "a".into(),
        canonical_model: "marker".into(),
        kind: "marker".into(),
        probes_skipped: 1,
        ..Default::default()
    }));
    let want = *marks;
    wait_for(Duration::from_secs(30), || {
        let rows =
            r.db.read_blocking(|c| {
                read_rollups(c, hour_of(NOW) - 3_600, i64::MAX, &RollupSelect::default())
            })
            .unwrap();
        let landed: u64 = rows
            .iter()
            .filter(|row| row.kind == "marker")
            .map(|row| row.probes_skipped)
            .sum();
        (landed == want).then_some(())
    })
    .await;
}

/// `settle_ledger` on the test thread's own clock, for a wait inside the held registry, and kept going until at least
/// `hold` has passed. The case calling it runs `multi_thread`, so its body runs on the thread that called `block_on`
/// while the ledger writer runs on the workers, and a tokio timer inside the hold can stall while a worker blocks on
/// the store's mutex.
fn settle_ledger_in_hold(r: &Rig, marks: &mut u64, hold: Duration) {
    let since = Instant::now();
    *marks += 1;
    r.ledger.record(Record::Keepalive(KeepaliveDelta {
        at: NOW,
        key_id: "desktop".into(),
        pool_id: "main".into(),
        account_id: "a".into(),
        canonical_model: "marker".into(),
        kind: "marker".into(),
        probes_skipped: 1,
        ..Default::default()
    }));
    let want = *marks;
    let deadline = Duration::from_secs(30);
    loop {
        let rows =
            r.db.read_blocking(|c| {
                read_rollups(c, hour_of(NOW) - 3_600, i64::MAX, &RollupSelect::default())
            })
            .unwrap();
        let landed: u64 = rows
            .iter()
            .filter(|row| row.kind == "marker")
            .map(|row| row.probes_skipped)
            .sum();
        if landed == want && since.elapsed() >= hold {
            return;
        }
        assert!(
            since.elapsed() < deadline,
            "the marker never landed within {deadline:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The wasted probes the ledger holds right now, by reason, read once.
fn wasted_now(r: &Rig) -> BTreeMap<String, u64> {
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
    reasons
}

/// Ticks `engine` at every due probe and at least every half minute until `end`, the clock moved on the rig with no
/// heartbeat: the engine is a restarted one, whose session the mod has not re-adopted.
async fn tick_until(r: &Rig, engine: &Arc<Engine>, end: i64) {
    while r.now() < end {
        let now = r.now();
        let due = engine
            .sessions()
            .iter()
            .filter_map(|session| session.keepalive.next_probe_at)
            .filter(|due| *due > now)
            .min();
        let next = due.unwrap_or(end).min(now + MINUTE / 2).min(end);
        r.advance(next - now);
        engine.step().await;
        no_probe_running(engine).await;
    }
}

/// A restart must not end a park the mod is about to re-adopt. The save said the session was attended, and the ticks
/// after the load read it so for one lease length, until the mod registers it again or the window ends. The session
/// itself comes back silent, so the mod's heartbeat still answers `instance_ended` and the registration follows.
#[tokio::test]
async fn a_restored_park_past_the_idle_window_survives_one_lease_length_and_ends_at_the_windows_end_unregistered(
) {
    let mut r = rig(RigSpec {
        policy: KeepalivePolicy {
            enabled: true,
            max_idle_seconds: 600,
            max_parked_seconds: Some(7_200),
            ..KeepalivePolicy::default()
        },
        ..on_disk()
    })
    .await;
    r.observe("main").await;
    let start = r.now();
    r.report(parked_until(start + 2 * 3_600_000)).await.unwrap();
    // Fifteen minutes into the park, past the ten-minute idle window, probed at every due time under the parked
    // ceiling with the lease renewed.
    let end = start + 15 * MINUTE;
    while r.now() < end {
        let now = r.now();
        let due = r
            .engine
            .sessions()
            .iter()
            .filter_map(|session| session.keepalive.next_probe_at)
            .filter(|due| *due > now)
            .min();
        let next = due.unwrap_or(end).min(now + MINUTE / 2).min(end);
        r.advance_attended(next - now).await;
        r.step().await;
    }
    assert_eq!(
        r.engine.recipes_for("desktop", "session"),
        vec!["main".to_string()],
        "the parked recipe is still probed before the restart"
    );
    r.engine.flush_persisted().await;

    let second = r.restart().await;
    second.load_persisted().await;
    let loaded = r.now();
    second.step().await;
    let main = view_of(&second, "session", "main").unwrap();
    assert_eq!(
        (second.recipes_for("desktop", "session"), main.cause),
        (vec!["main".to_string()], Reason::Scheduled),
        "the first tick after the load keeps the park: {main:?}"
    );
    assert_eq!(
        second.sessions()[0].companion,
        CompanionState::Silent,
        "the session itself comes back silent, for the mod's heartbeat to re-adopt it"
    );
    tick_until(&r, &second, loaded + LEASE_MS - 1).await;
    assert_eq!(
        second.recipes_for("desktop", "session"),
        vec!["main".to_string()],
        "still probed a millisecond before the window's end"
    );
    tick_until(&r, &second, loaded + LEASE_MS).await;
    let main = view_of(&second, "session", "main").unwrap();
    assert_eq!(
        (second.recipes_for("desktop", "session"), main.cause),
        (Vec::new(), Reason::IdleLimit),
        "no registration came, so the silent session's idle window applies at the window's end: {main:?}"
    );
}

#[tokio::test]
async fn a_restored_both_park_keeps_its_deadline_and_its_wake() {
    let mut r = rig(on_disk()).await;
    r.observe("main").await;
    let deadline = r.now() + 10 * MINUTE;
    let wake = r.now() + 60 * MINUTE;
    r.report(json!({
        "event": "state",
        "agent_id": "main",
        "state": "parked",
        "park_kind": "both",
        "deadline": deadline,
        "wake_at": wake
    }))
    .await
    .unwrap();
    r.engine.flush_persisted().await;

    let second = r.restart().await;
    second.load_persisted().await;
    second.step().await;
    let main = view_of(&second, "session", "main").unwrap();
    assert_eq!(
        (
            main.agent_state,
            main.park_kind,
            main.deadline,
            main.wake_at
        ),
        (
            Lifecycle::Parked,
            Some(ParkKind::Both),
            Some(deadline),
            Some(wake)
        ),
        "the restored both park keeps its deadline and its wake: {main:?}"
    );
}

/// A row an earlier build saved can hold a wake on a background park or a deadline on a scheduled one, and the load
/// keeps each part only where the park's kind takes it, so the registration's answer carries no stray part.
#[tokio::test]
async fn a_restored_park_drops_a_saved_part_its_kind_does_not_take() {
    let mut r = rig(on_disk()).await;
    let worker = r
        .event(json!({"event": "agent_start", "agent_id": "worker"}))
        .await
        .unwrap()
        .request_generation;
    let main = r.event(heartbeat()).await.unwrap().request_generation;
    let wake = r.now() + 10 * MINUTE;
    let deadline = r.now() + 60 * MINUTE;
    r.event(json!({
        "event": "state",
        "agent_id": "main",
        "state": "parked",
        "park_kind": "background",
        "deadline": deadline,
        "request_generation": main
    }))
    .await
    .unwrap();
    r.event(json!({
        "event": "state",
        "agent_id": "worker",
        "state": "parked",
        "park_kind": "scheduled",
        "wake_at": wake,
        "request_generation": worker
    }))
    .await
    .unwrap();
    r.engine.flush_persisted().await;
    let saved = r.h.store.load_engine_state().unwrap().agents;
    let with_stray = |agent: &str, member: &str, at: i64| {
        let mut row = saved
            .iter()
            .find(|row| row.agent_id == agent)
            .unwrap_or_else(|| panic!("no saved row for {agent}"))
            .clone();
        let mut payload: serde_json::Value = serde_json::from_str(&row.payload).unwrap();
        payload[member] = at.into();
        row.payload = payload.to_string();
        row
    };
    r.h.store
        .save_engine_state(&EngineBatch {
            agents: vec![
                with_stray("main", "wake_at", wake),
                with_stray("worker", "deadline", deadline),
            ],
            ..EngineBatch::default()
        })
        .unwrap();
    r.engine = r.restart().await;
    r.engine.load_persisted().await;

    let parks: Vec<(
        String,
        serde_json::Value,
        serde_json::Value,
        serde_json::Value,
    )> = r
        .reregister()
        .await
        .agents
        .expect("a registration answers the session's agents")
        .into_iter()
        .map(|agent| {
            let answered = serde_json::to_value(&agent).unwrap();
            (
                agent.agent_id,
                answered["park_kind"].clone(),
                answered["deadline"].clone(),
                answered["wake_at"].clone(),
            )
        })
        .collect();
    assert_eq!(
        parks,
        vec![
            (
                "main".to_string(),
                serde_json::json!("background"),
                serde_json::json!(deadline),
                serde_json::Value::Null
            ),
            (
                "worker".to_string(),
                serde_json::json!("scheduled"),
                serde_json::Value::Null,
                serde_json::json!(wake)
            ),
        ],
        "each restored park keeps the parts its kind takes"
    );
}

// The test's body runs apart from the workers and waits on the flush counts, as `on_own_thread` explains.
/// The settlements of the recipes a load ends reach the registry before the ledger: the cleared agent is saved first,
/// so a crash between the two leaves the probe's wasted row unwritten rather than written twice by the next load.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_loads_settlement_reaches_the_ledger_only_once_the_registry_holds_the_cleared_agent() {
    let mut r = rig(on_disk()).await;
    r.observe("main").await;
    r.advance_attended(270_000).await;
    r.step().await;
    r.engine.flush_persisted().await;
    assert_eq!(saved_unanswered(&r, "session", "main"), 1);
    // The router is down past the entry's expiry, so the load ends the recipe and settles its probe as expired.
    r.advance(400_000);
    let second = r.restart().await;
    let held = registry(&r);
    held.execute_batch("BEGIN IMMEDIATE").unwrap();
    let engine = second.clone();
    let load = on_own_thread(async move { engine.load_persisted().await });
    // The load's save has read its batch and waits on the held registry.
    second.persist_reached(1, 1).await;
    let mut marks = 0;
    settle_ledger_in_hold(&r, &mut marks, Duration::ZERO);
    assert_eq!(
        wasted_now(&r),
        BTreeMap::new(),
        "nothing reaches the ledger while the registry still holds the probe"
    );
    // The pause is the hold across one catalog refresh tick (every 20 ms in the rig), so a wait in the hold that runs
    // on tokio's timer fails the case on every run rather than by chance.
    settle_ledger_in_hold(&r, &mut marks, Duration::from_millis(60));
    held.execute_batch("COMMIT").unwrap();
    load.join().unwrap();
    assert_eq!(
        saved_field(&r, "session", "main", "unanswered"),
        serde_json::json!([]),
        "the load's save landed once the registry was free"
    );
    assert_eq!(
        wasted(&r, 1).await,
        BTreeMap::from([("expired".to_string(), 1)]),
        "the settlement follows the landed save"
    );
    assert_eq!(
        (
            saved_field(&r, "session", "main", "unanswered"),
            saved_field(&r, "session", "main", "reason")
        ),
        (
            serde_json::json!([]),
            serde_json::json!("expired_at_restart")
        )
    );
    // A later load finds the probe settled, so the row is counted once.
    let third = r.restart().await;
    third.load_persisted().await;
    third.flush_persisted().await;
    settle_ledger(&r, &mut marks).await;
    assert_eq!(wasted_now(&r), BTreeMap::from([("expired".to_string(), 1)]));
}

/// A save that fails at the load holds the settlements back: the ledger learns of a wasted probe only from the flush
/// whose save dropped it from the agent, so a restart before that counts it once at most.
#[tokio::test]
async fn a_loads_settlement_waits_out_a_failed_save_and_goes_out_with_the_save_that_lands() {
    let mut r = rig(on_disk()).await;
    r.observe("main").await;
    r.advance_attended(270_000).await;
    r.step().await;
    r.engine.flush_persisted().await;
    assert_eq!(saved_unanswered(&r, "session", "main"), 1);
    // The router is down past the entry's expiry, and the registry refuses every agent row when it comes back.
    r.advance(400_000);
    let conn = registry(&r);
    conn.execute_batch(
        "CREATE TRIGGER refuse_insert BEFORE INSERT ON keepalive_agents BEGIN SELECT RAISE(ABORT, 'refused'); END; \
         CREATE TRIGGER refuse_update BEFORE UPDATE ON keepalive_agents BEGIN SELECT RAISE(ABORT, 'refused'); END;",
    )
    .unwrap();
    let mut marks = 0;
    let second = r.restart().await;
    second.load_persisted().await;
    assert_eq!(
        second.reasons().get(&Reason::ExpiredAtRestart).copied(),
        Some(1)
    );
    settle_ledger(&r, &mut marks).await;
    assert_eq!(
        wasted_now(&r),
        BTreeMap::new(),
        "a save that failed sends nothing to the ledger"
    );
    // A second flush that fails holds them as the first did.
    second.flush_persisted().await;
    settle_ledger(&r, &mut marks).await;
    assert_eq!(
        wasted_now(&r),
        BTreeMap::new(),
        "a second failed save sends nothing either"
    );
    // The process dies before another save: the next one finds the probe still unanswered in the registry.
    let third = r.restart().await;
    third.load_persisted().await;
    settle_ledger(&r, &mut marks).await;
    assert_eq!(wasted_now(&r), BTreeMap::new());
    conn.execute_batch("DROP TRIGGER refuse_insert; DROP TRIGGER refuse_update;")
        .unwrap();
    third.flush_persisted().await;
    assert_eq!(
        wasted(&r, 1).await,
        BTreeMap::from([("expired".to_string(), 1)]),
        "the settlement goes out with the save that lands"
    );
    assert_eq!(
        saved_field(&r, "session", "main", "unanswered"),
        serde_json::json!([])
    );
    let fourth = r.restart().await;
    fourth.load_persisted().await;
    settle_ledger(&r, &mut marks).await;
    assert_eq!(
        wasted_now(&r),
        BTreeMap::from([("expired".to_string(), 1)]),
        "a later load finds the probe settled"
    );
}

/// The idle limit's pending probe settles at its entry's end only once the registry holds the agent without it, as a
/// load's settlements do: a save that fails sends nothing to the ledger, so a restart that restores the probe and
/// lapses it again counts it once.
#[tokio::test]
async fn a_lapsed_pending_probe_reaches_the_ledger_once_across_a_failed_save_and_a_restart() {
    let mut r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        policy: KeepalivePolicy {
            max_idle_seconds: 500,
            ..RigSpec::default().policy
        },
        ..on_disk()
    })
    .await;
    r.observe("main").await;
    r.advance(270_000);
    r.step().await;
    r.advance(270_000);
    r.step().await;
    assert_eq!(
        r.view("main").cause,
        Reason::IdleLimit,
        "the idle limit left the probe pending"
    );
    r.engine.flush_persisted().await;
    assert_eq!(saved_unanswered(&r, "session", "main"), 1);
    let agents = saved_agents(&r);
    let payload: serde_json::Value = serde_json::from_str(&agents[0].payload).unwrap();
    let lapse = payload["kept"][0]["entry_expires"]
        .as_i64()
        .expect("the pending probe's entry");
    // The registry refuses every agent row from here on, so no save drops the probe.
    let conn = registry(&r);
    conn.execute_batch(
        "CREATE TRIGGER refuse_insert BEFORE INSERT ON keepalive_agents BEGIN SELECT RAISE(ABORT, 'refused'); END; \
         CREATE TRIGGER refuse_update BEFORE UPDATE ON keepalive_agents BEGIN SELECT RAISE(ABORT, 'refused'); END;",
    )
    .unwrap();
    // The sweep at the entry's end settles the probe, and the settlement waits for a save that lands.
    r.advance(lapse - r.now());
    r.step().await;
    r.engine.flush_persisted().await;
    // The process dies before another save: the next one restores the probe still pending in the registry.
    let second = r.restart().await;
    second.load_persisted().await;
    second.step().await;
    conn.execute_batch("DROP TRIGGER refuse_insert; DROP TRIGGER refuse_update;")
        .unwrap();
    second.flush_persisted().await;
    let mut marks = 0;
    settle_ledger(&r, &mut marks).await;
    assert_eq!(
        wasted_now(&r),
        BTreeMap::from([("idle_limit".to_string(), 1)]),
        "one settlement, with the save that dropped the probe"
    );
}

/// A save that fails keeps the roll's cutoff, so the dropped day's counters stay in the registry until the save that
/// lands carries the purge.
#[tokio::test]
async fn a_failed_save_keeps_the_purge_for_the_save_that_lands() {
    let mut r = rig(one_day_retained(on_disk())).await;
    let dropped = the_only_day_rolled_off(&mut r).await;
    let conn = registry(&r);
    conn.execute_batch(
        "CREATE TRIGGER refuse_delete BEFORE DELETE ON keepalive_idle_returns \
         BEGIN SELECT RAISE(ABORT, 'refused'); END;",
    )
    .unwrap();
    r.engine.flush_persisted().await;
    assert_eq!(
        saved_days(&r),
        BTreeSet::from([dropped]),
        "the failed save purged nothing"
    );
    conn.execute_batch("DROP TRIGGER refuse_delete;").unwrap();
    r.engine.flush_persisted().await;
    assert_eq!(
        saved_days(&r),
        BTreeSet::new(),
        "the save that lands carries the purge"
    );
}
