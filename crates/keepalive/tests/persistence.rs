//! The engine's state across a restart: a flush writes the sessions, the agents with their recipes and the idle-age
//! counters to the registry, and a second engine over the same registry reads them back silent, within the limits it
//! starts under. The shutdown quiesces before its save, and the saves never overlap.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use phoebis_bus::{Event as BusEvent, Outcome, Usage};
use phoebis_keepalive::{
    Ack, CompanionState, Engine, Event, EventError, KeepaliveLimits, KeepalivePolicy, Reason,
    LEASE_MS,
};
use phoebis_ledger::{hour_of, read_rollups, KeepaliveDelta, Record, RollupSelect};
use phoebis_store::{KeepaliveAgentRow, KeepaliveSessionRow};

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

fn saved_sessions(r: &Rig) -> Vec<KeepaliveSessionRow> {
    r.h.store.load_keepalive().unwrap().0
}

fn saved_agents(r: &Rig) -> Vec<KeepaliveAgentRow> {
    r.h.store.load_keepalive().unwrap().1
}

/// One member of the saved payload of `agent` on `session`.
fn saved_field(r: &Rig, session: &str, agent: &str, field: &str) -> serde_json::Value {
    let row = saved_agents(r)
        .into_iter()
        .find(|row| row.session_id == session && row.agent_id == agent)
        .unwrap_or_else(|| panic!("no saved row for {session} {agent}"));
    serde_json::from_str::<serde_json::Value>(&row.payload).unwrap()[field].clone()
}

fn heartbeat() -> Event {
    Event {
        event: "heartbeat".into(),
        ..Default::default()
    }
}

/// A `parked` report of kind `background` with the command's deadline.
fn parked_until(r: &Rig, deadline: i64) -> Event {
    Event {
        event: "state".into(),
        agent_id: Some("main".into()),
        state: Some("parked".into()),
        park_kind: Some("background".into()),
        deadline: Some(deadline),
        request_generation: Some(r.agent("main").request_generation),
        ..Default::default()
    }
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

/// `Rig::event` under a key generation read beforehand. The rig reads it from the store, whose connection a save
/// waiting on the registry's write lock holds, so a report sent while the test holds that lock goes through here.
fn report(r: &mut Rig, key_generation: i64, mut event: Event) -> Result<Ack, EventError> {
    r.sequence += 1;
    event.sequence = r.sequence;
    if event.session_id.is_empty() {
        event.session_id = "session".into();
    }
    if event.instance_id.is_empty() {
        event.instance_id = r.instance_of(&event.session_id);
    }
    r.engine.apply_event("desktop", key_generation, event)
}

/// A `state` report for `agent` under its current generation.
fn state_of(r: &Rig, agent: &str, state: &str) -> Event {
    Event {
        event: "state".into(),
        agent_id: Some(agent.into()),
        state: Some(state.into()),
        request_generation: Some(r.agent(agent).request_generation),
        park_kind: (state == "parked").then(|| "background".into()),
        ..Default::default()
    }
}

/// Runs `work` to its end on a thread of its own. A save held in the store's busy wait keeps the store's connection,
/// and a runtime task that reads the store (the relay's catalog refresh does every few milliseconds) blocks the
/// worker it runs on behind it, so a flush or a shutdown left to the workers might never get to run while the test
/// holds the registry.
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
    r.reregister();
    r.observe("main").await;
    r.advance_attended(270_000);
    r.step().await;
    r.advance_attended(40_000);
    r.observe("main").await;
    r.advance_attended(120_000);
    let presence = Event {
        event: "activity".into(),
        agent_id: Some("main".into()),
        kind: Some("presence".into()),
        request_generation: Some(r.agent("main").request_generation),
        ..Default::default()
    };
    r.event(presence).unwrap();
    let seen = r.now();
    let deadline = r.now() + 2 * 3_600_000;
    r.event(parked_until(&r, deadline)).unwrap();
    r.advance(MINUTE);
    let (sessions, agents) = r.engine.persist_rows();
    let stats = r.engine.stats();
    r.engine.flush_persisted().await;
    assert_eq!(
        (saved_sessions(&r).len(), saved_agents(&r).len()),
        (1, 1),
        "the flush wrote the session and its agent"
    );

    let second = r.restart().await;
    second.load_persisted().await;
    let (loaded_sessions, loaded_agents) = second.persist_rows();
    assert_eq!(
        loaded_agents, agents,
        "every field of the agent and its recipe reads back as it stood"
    );
    let main = &loaded_agents[0];
    let payload: serde_json::Value = serde_json::from_str(&main.payload).unwrap();
    assert_eq!(
        (
            payload["state"].as_str(),
            payload["park_kind"].as_str(),
            payload["deadline"].as_i64(),
            payload["park_began"].as_i64(),
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
            payload["last_presence_at"].as_i64(),
        ),
        (Some(1), Some(1), Some(0), Some(start + 270_000), Some(seen)),
        "the counts are the day before the last activity's"
    );
    assert!(main.body.as_ref().is_some_and(|body| !body.is_empty()));
    assert_eq!(
        (main.body_size, main.fast),
        (stats.retained_bytes, false),
        "the recipe's size is the retained bytes"
    );
    let before: serde_json::Value = serde_json::from_str(&sessions[0].payload).unwrap();
    let after: serde_json::Value = serde_json::from_str(&loaded_sessions[0].payload).unwrap();
    assert_eq!(
        (after["attended"].as_bool(), after["lease"].as_i64()),
        (Some(false), Some(0)),
        "the session reads silent with its lease at zero"
    );
    assert_eq!(
        (&after["sequence"], &after["instance"], &after["owner_id"]),
        (
            &before["sequence"],
            &before["instance"],
            &before["owner_id"]
        )
    );
    assert_eq!(second.stats().retained_bytes, stats.retained_bytes);
    assert_eq!((second.stats().sessions, second.stats().agents), (1, 1));
    assert_eq!(second.sessions()[0].companion, CompanionState::Silent);
}

#[tokio::test]
async fn a_heartbeat_after_the_restart_answers_instance_ended_and_the_same_instance_re_adopts_with_the_sequence_kept(
) {
    let mut r = rig(on_disk()).await;
    r.observe("main").await;
    r.event(heartbeat()).unwrap();
    let sequence = r.sequence;
    r.engine.flush_persisted().await;
    r.engine = r.restart().await;
    r.engine.load_persisted().await;

    assert_eq!(
        r.event(heartbeat()).unwrap_err().code,
        "instance_ended",
        "a silent session answers the mod's heartbeat as an ended instance"
    );
    let ack = r.reregister();
    assert_eq!(ack.sequence, sequence + 2, "the same instance re-adopts it");
    assert_eq!(r.engine.sessions()[0].companion, CompanionState::Reporting);
    let generation = r.generation("desktop");
    let stale = r.engine.apply_event(
        "desktop",
        generation,
        Event {
            event: "heartbeat".into(),
            session_id: "session".into(),
            instance_id: "instance".into(),
            sequence,
            ..Default::default()
        },
    );
    assert_eq!(
        stale.unwrap_err().code,
        "stale_sequence",
        "the sequence the save carried still fences a report the mod already sent"
    );
    r.event(heartbeat()).expect("the next sequence is accepted");
    assert_eq!(
        r.engine.recipes_for("desktop", "session"),
        vec!["main".to_string()],
        "the recipe the mod's session left stays retained"
    );
}

#[tokio::test]
async fn the_generations_survive_so_the_next_state_report_is_not_a_409() {
    let mut r = rig(on_disk()).await;
    r.event(Event {
        event: "agent_start".into(),
        agent_id: Some("sub".into()),
        ..Default::default()
    })
    .unwrap();
    r.observe("main").await;
    r.observe("main").await;
    r.observe("sub").await;
    let generations = (
        r.agent("main").request_generation,
        r.agent("sub").request_generation,
    );
    assert!(generations.0 > 0 && generations.1 > 0, "{generations:?}");
    r.engine.flush_persisted().await;
    r.engine = r.restart().await;
    r.engine.load_persisted().await;

    let ack = r.reregister();
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
        r.event(Event {
            event: "state".into(),
            agent_id: Some(agent.into()),
            state: Some("idle".into()),
            request_generation: Some(generation),
            ..Default::default()
        })
        .unwrap_or_else(|e| panic!("{agent} under its saved generation: {e:?}"));
    }
}

#[tokio::test]
async fn an_ended_session_and_an_expired_recipe_are_dropped_at_the_load_under_expired_at_restart() {
    let mut r = rig(on_disk()).await;
    r.event(session_start("gone")).unwrap();
    r.event(session_end("gone")).unwrap();
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
        (second.stats().sessions, second.stats().retained_bytes),
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

#[tokio::test]
async fn the_idle_age_counters_round_trip() {
    // Keepalive off, so no recipe is retained and the ticks probe nothing: the counters count all the same.
    let mut r = rig(RigSpec {
        policy: KeepalivePolicy::default(),
        ..on_disk()
    })
    .await;
    r.observe("main").await;
    r.state("main", "idle");
    r.advance_attended(6 * MINUTE);
    r.step().await;
    // The next turn of the same conversation, six minutes into the window.
    let next_turn = BODY.replace(
        r#"{"role":"user","content":"hello"}]"#,
        r#"{"role":"user","content":"hello"},{"role":"assistant","content":"hi"},{"role":"user","content":"and then"}]"#,
    );
    r.observe_with("main", &next_turn).await;
    let counters = r.engine.idle_returns();
    assert_eq!(
        counters["main"].buckets[..2]
            .iter()
            .map(|b| (b.reached, b.returned))
            .collect::<Vec<_>>(),
        vec![(1, 0), (1, 1)],
        "a window six minutes old reached the first two buckets and returned in the second"
    );
    r.engine.flush_persisted().await;
    let second = r.restart().await;
    second.load_persisted().await;
    assert_eq!(second.idle_returns(), counters);
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

    let main = second
        .agent_status("desktop", "session", "main")
        .expect("the agent keeps its record");
    assert_eq!(main.reason, Reason::Reset);
    assert!(second.recipes_for("desktop", "session").is_empty());
    assert_eq!(
        (second.stats().agents, second.stats().retained_bytes),
        (1, 0)
    );
    assert_eq!(
        saved_field(&r, "session", "main", "current"),
        serde_json::Value::Null
    );
}

#[tokio::test]
async fn the_shutdown_quiesces_drains_saves_and_then_clears() {
    let mut r = rig(on_disk()).await;
    r.event(Event {
        event: "agent_start".into(),
        agent_id: Some("sub".into()),
        ..Default::default()
    })
    .unwrap();
    r.observe("main").await;
    r.observe("sub").await;
    r.state("main", "idle");
    let (sessions, agents) = r.engine.persist_rows();
    r.engine.shutdown().await;

    assert_eq!(
        (r.engine.stats().sessions, r.engine.stats().retained_bytes),
        (0, 0),
        "the clear came after the save"
    );
    assert_eq!(saved_sessions(&r), sessions);
    assert_eq!(
        saved_agents(&r),
        agents,
        "every agent as it stood at the quiesce, recipes and all"
    );
}

#[tokio::test]
async fn a_request_that_completes_during_shutdown_is_refused_and_counted_and_the_save_holds_the_quiesced_state(
) {
    let mut r = rig(on_disk()).await;
    r.observe("main").await;
    r.engine.observe(r.observed("late", "main", BODY));
    r.engine.observe(r.selected("late", "a"));
    let (_, agents) = r.engine.persist_rows();
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
    assert_eq!(r.event(heartbeat()).unwrap_err().code, "quiesced");
    assert_eq!(r.engine.stats().late_updates, 3);
}

// The test's body runs on a thread apart from the workers, which a held save can leave blocked behind the store's
// connection (see `on_own_thread`), and it waits on the engine's flush counts, which need no timer the workers drive.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shutdown_while_an_earlier_save_is_held_lands_both_saves_in_order() {
    let mut r = rig(on_disk()).await;
    r.event(Event {
        event: "agent_start".into(),
        agent_id: Some("sub".into()),
        ..Default::default()
    })
    .unwrap();
    r.observe("main").await;
    r.engine.flush_persisted().await;
    r.state("main", "idle");
    r.state("sub", "idle");
    let held_at = r.now();
    let generation = r.generation("desktop");
    let held = registry(&r);
    held.execute_batch("BEGIN IMMEDIATE").unwrap();
    let engine = r.engine.clone();
    let first = on_own_thread(async move { engine.flush_persisted().await });
    r.engine.persist_reached(2, 2).await;
    // After the held save read its batch: only a later save carries this.
    r.advance(MINUTE);
    let parked = state_of(&r, "main", "parked");
    report(&mut r, generation, parked).unwrap();
    let engine = r.engine.clone();
    let shutdown = on_own_thread(async move { engine.shutdown().await });
    // The shutdown's flush has entered and waits on the flush lock the held save holds.
    r.engine.persist_reached(3, 2).await;
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
    r.event(Event {
        event: "agent_start".into(),
        agent_id: Some("sub".into()),
        ..Default::default()
    })
    .unwrap();
    r.observe("main").await;
    r.engine.flush_persisted().await;
    assert_eq!(saved_field(&r, "session", "main", "state"), "busy");

    // A save that fails: a trigger in the registry file refuses every session row, which the store's own connection
    // meets on its next write whatever the file's journal mode. The subagent's change is carried by that save and
    // touched by nothing after it.
    r.state("main", "idle");
    r.state("sub", "idle");
    let generation = r.generation("desktop");
    let conn = registry(&r);
    conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    let engine = r.engine.clone();
    let failed = on_own_thread(async move { engine.flush_persisted().await });
    r.engine.persist_reached(2, 2).await;
    // Between the failed save's snapshot and its outcome.
    let parked = state_of(&r, "main", "parked");
    report(&mut r, generation, parked).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER refuse_insert BEFORE INSERT ON keepalive_sessions BEGIN SELECT RAISE(ABORT, 'refused'); END; \
         CREATE TRIGGER refuse_update BEFORE UPDATE ON keepalive_sessions BEGIN SELECT RAISE(ABORT, 'refused'); END; \
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
    r.engine.persist_reached(3, 3).await;
    r.advance(MINUTE);
    // Between this save's snapshot and its acknowledgment: a presence report moves the idle clock alone.
    let presence = Event {
        event: "activity".into(),
        agent_id: Some("main".into()),
        kind: Some("presence".into()),
        request_generation: Some(r.agent("main").request_generation),
        ..Default::default()
    };
    report(&mut r, generation, presence).unwrap();
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

/// Two sessions under the default limits: `session` with its main agent and a recipe probed once, saved first, and
/// `newer`, saved a minute later, with its main agent's recipe and a subagent that retains nothing. Answers the two
/// recipes' sizes, the older first.
async fn two_sessions() -> (Rig, usize, usize) {
    let mut r = rig(on_disk()).await;
    r.observe("main").await;
    // One probe the next request has not answered yet, which a disposal settles as wasted.
    r.advance_attended(270_000);
    r.step().await;
    assert_eq!(r.status("main").unanswered, 1);
    let older = r.engine.stats().retained_bytes;
    r.engine.flush_persisted().await;
    r.advance(MINUTE);
    r.event(session_start("newer")).unwrap();
    r.event(Event {
        event: "agent_start".into(),
        session_id: "newer".into(),
        agent_id: Some("sub".into()),
        ..Default::default()
    })
    .unwrap();
    observe_on(&mut r, "newer", "main").await;
    let newer = r.engine.stats().retained_bytes - older;
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
        KeepaliveLimits {
            max_sessions: 1,
            ..KeepaliveLimits::default()
        },
        KeepaliveLimits {
            max_agents: 2,
            ..KeepaliveLimits::default()
        },
    ] {
        let (r, _, newer) = two_sessions().await;
        let second = r.restart_with_limits(limits.clone()).await;
        second.load_persisted().await;
        let stats = second.stats();
        assert_eq!(
            (stats.sessions, stats.agents, stats.retained_bytes),
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
        ..KeepaliveLimits::default()
    };
    let second = r.restart_with_limits(limits).await;
    second.load_persisted().await;
    let stats = second.stats();
    assert_eq!(
        (stats.sessions, stats.agents, stats.retained_bytes),
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

/// A session the load admits can still carry more agents than the limit leaves room for: the subagent past it is
/// disposed on its own, and the probe it paid for and nobody answered settles under the eviction.
#[tokio::test]
async fn a_subagent_past_the_agent_limit_is_disposed_alone_and_its_unanswered_probe_settles() {
    let mut r = rig(on_disk()).await;
    r.event(Event {
        event: "agent_start".into(),
        agent_id: Some("sub".into()),
        ..Default::default()
    })
    .unwrap();
    r.observe("main").await;
    r.observe("sub").await;
    r.advance_attended(270_000);
    r.step().await;
    assert_eq!(
        (r.status("main").unanswered, r.status("sub").unanswered),
        (1, 1)
    );
    let main_bytes = {
        let (_, agents) = r.engine.persist_rows();
        agents
            .iter()
            .find(|a| a.agent_id == "main")
            .map(|a| a.body_size)
            .unwrap()
    };
    r.engine.flush_persisted().await;
    let second = r
        .restart_with_limits(KeepaliveLimits {
            max_agents: 1,
            ..KeepaliveLimits::default()
        })
        .await;
    second.load_persisted().await;
    let stats = second.stats();
    assert_eq!(
        (stats.sessions, stats.agents, stats.retained_bytes),
        (1, 1, main_bytes),
        "the session and its main agent load"
    );
    assert_eq!(second.reasons().get(&Reason::Evicted).copied(), Some(1));
    assert_eq!(
        wasted(&r, 1).await,
        BTreeMap::from([("evicted".to_string(), 1)]),
        "the subagent's probe is wasted under the eviction, and main's stays unanswered"
    );
    assert_eq!(
        second
            .agent_status("desktop", "session", "main")
            .unwrap()
            .unanswered,
        1
    );
    second.flush_persisted().await;
    assert_eq!(
        saved_agents(&r)
            .iter()
            .map(|a| a.agent_id.as_str())
            .collect::<Vec<_>>(),
        vec!["main"],
        "the disposed subagent's row left the registry"
    );
}

/// The credit after a restart reads the newest unanswered probe's coverage, entry, unit and model, so a probe nobody
/// answered before the flush comes back with all four.
#[tokio::test]
async fn a_restart_restores_an_unanswered_probe_with_its_coverage_entry_unit_and_model() {
    let mut r = rig(on_disk()).await;
    r.observe("main").await;
    r.advance_attended(270_000);
    r.step().await;
    assert_eq!(r.status("main").unanswered, 1);
    let (_, agents) = r.engine.persist_rows();
    r.engine.flush_persisted().await;
    let second = r.restart().await;
    second.load_persisted().await;
    let (_, loaded) = second.persist_rows();
    assert_eq!(
        loaded, agents,
        "the agent with its unanswered probe reads back as it stood"
    );
    let payload: serde_json::Value = serde_json::from_str(&loaded[0].payload).unwrap();
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
    // The probe's own request id, which its ledger row carries, so a settlement can be told from a repeat of it.
    assert!(
        probe["id"].as_str().is_some_and(|id| !id.is_empty()),
        "the probe names its request: {probe}"
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
    second.wait_idle().await;

    assert_eq!(
        r.up.requests().len(),
        sent + 1,
        "the restored recipe's probe reached the upstream"
    );
    let main = second.agent_status("desktop", "session", "main").unwrap();
    assert_eq!(
        (main.probes, main.cold, main.reason),
        (1, 0, Reason::Scheduled),
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
    r.engine
        .apply_event(
            "desktop",
            old + 1,
            Event {
                event: "session_start".into(),
                session_id: "session".into(),
                instance_id: "later".into(),
                sequence: 1,
                ..Default::default()
            },
        )
        .unwrap();
    r.engine.flush_persisted().await;

    assert_eq!(
        saved_sessions(&r)
            .into_iter()
            .map(|s| (s.session_id, s.key_generation))
            .collect::<Vec<_>>(),
        vec![("session".to_string(), old + 1)],
        "the earlier holder's row went as the later one's was written"
    );
    assert!(saved_agents(&r).iter().all(|a| a.key_generation == old + 1));
    let second = r.restart().await;
    second.load_persisted().await;
    assert_eq!(second.stats().sessions, 1);
    assert_eq!(second.sessions()[0].key_generation, old + 1);
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
    assert_eq!(r.engine.stats().sessions, 0);
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
    assert_eq!((second.stats().sessions, second.stats().agents), (1, 1));
    assert_eq!(
        (saved_sessions(&r).len(), saved_agents(&r).len()),
        (1, 1),
        "the second load filed no removal of a row the engine holds"
    );
}

#[tokio::test]
async fn a_session_whose_main_agent_cannot_be_read_is_dropped_whole() {
    let mut r = rig(on_disk()).await;
    r.event(Event {
        event: "agent_start".into(),
        agent_id: Some("sub".into()),
        ..Default::default()
    })
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
        (stats.sessions, stats.agents, stats.retained_bytes),
        (0, 0, 0),
        "no session comes back without the main agent its reports name"
    );
    assert_eq!(
        (saved_sessions(&r).len(), saved_agents(&r).len()),
        (0, 0),
        "and its rows, the subagent's with it, are gone"
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
    wait_for(Duration::from_secs(5), || {
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
        engine.wait_idle().await;
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
    r.event(parked_until(&r, start + 2 * 3_600_000)).unwrap();
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
        r.advance_attended(next - now);
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
    second.wait_idle().await;
    let main = second.agent_status("desktop", "session", "main").unwrap();
    assert_eq!(
        (second.recipes_for("desktop", "session"), main.reason),
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
    let main = second.agent_status("desktop", "session", "main").unwrap();
    assert_eq!(
        (second.recipes_for("desktop", "session"), main.reason),
        (Vec::new(), Reason::IdleLimit),
        "no registration came, so the silent session's idle window applies at the window's end: {main:?}"
    );
}

// The test's body runs apart from the workers and waits on the flush counts, as `on_own_thread` explains.
/// The settlements of the recipes a load ends reach the registry before the ledger: the cleared agent is saved first,
/// so a crash between the two leaves the probe's wasted row unwritten rather than written twice by the next load.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_loads_settlement_reaches_the_ledger_only_once_the_registry_holds_the_cleared_agent() {
    let mut r = rig(on_disk()).await;
    r.observe("main").await;
    r.advance_attended(270_000);
    r.step().await;
    assert_eq!(r.status("main").unanswered, 1);
    r.engine.flush_persisted().await;
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
    settle_ledger(&r, &mut marks).await;
    assert_eq!(
        wasted_now(&r),
        BTreeMap::new(),
        "nothing reaches the ledger while the registry still holds the probe"
    );
    held.execute_batch("COMMIT").unwrap();
    load.join().unwrap();
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

// Apart from the workers, and waiting on the flush counts, as the case above explains.
/// An operator's release that lands after the shutdown's snapshot is refused: the last save holds the recipe either
/// way, and a success would tell the operator a recipe is gone that the next process restores and probes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_release_after_the_quiesce_is_refused_and_leaves_the_final_save_authoritative() {
    let mut r = rig(on_disk()).await;
    r.observe("main").await;
    r.engine.flush_persisted().await;
    r.state("main", "idle");
    let retained = r.engine.stats().retained_bytes;
    assert!(retained > 0);
    let held = registry(&r);
    held.execute_batch("BEGIN IMMEDIATE").unwrap();
    let engine = r.engine.clone();
    let shutdown = on_own_thread(async move { engine.shutdown().await });
    // The shutdown's save has read its snapshot and waits on the held registry.
    r.engine.persist_reached(2, 2).await;
    let refused = r.engine.release("desktop", "session");
    assert!(
        refused.is_err(),
        "a release after the quiesce answers unavailable: {refused:?}"
    );
    assert_eq!(
        r.engine.stats().retained_bytes,
        retained,
        "the recipe stays as the snapshot read it"
    );
    held.execute_batch("COMMIT").unwrap();
    shutdown.join().unwrap();
    assert!(
        saved_agents(&r)[0].body.is_some(),
        "the final save carries the recipe"
    );
    assert_eq!(saved_field(&r, "session", "main", "reason"), "scheduled");
    settle_ledger(&r, &mut 0).await;
    assert_eq!(
        wasted_now(&r),
        BTreeMap::new(),
        "no probe settles under a release that never happened"
    );
}

/// A save that fails at the load holds the settlements back: the ledger learns of a wasted probe only from the flush
/// whose save dropped it from the agent, so a restart before that counts it once at most.
#[tokio::test]
async fn a_loads_settlement_waits_out_a_failed_save_and_goes_out_with_the_save_that_lands() {
    let mut r = rig(on_disk()).await;
    r.observe("main").await;
    r.advance_attended(270_000);
    r.step().await;
    assert_eq!(r.status("main").unanswered, 1);
    r.engine.flush_persisted().await;
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
