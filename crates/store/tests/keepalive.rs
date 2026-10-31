//! The keepalive engine's rows: three tables in one transaction, and the retained prompt sealed under the storage key
//! and bound to the row it was written for.

use phoebis_store::keepalive::BodyWrite;
use phoebis_store::{
    keepalive_aad, IdleReturnsRow, KeepaliveAgentRow, KeepaliveBatch, KeepaliveSessionRow,
    StorageKey, Store,
};

const PROMPT: &[u8] =
    br#"{"system":"a private prompt the store must never hold in the clear","messages":[]}"#;

fn session(key_generation: i64, session_id: &str) -> KeepaliveSessionRow {
    KeepaliveSessionRow {
        key_id: "k".into(),
        key_generation,
        session_id: session_id.into(),
        record_version: 1,
        payload: format!(r#"{{"session_id":"{session_id}"}}"#),
        updated_at: 1_000,
    }
}

fn agent(
    key_generation: i64,
    session_id: &str,
    agent_id: &str,
    body: Option<&[u8]>,
) -> KeepaliveAgentRow {
    KeepaliveAgentRow {
        key_id: "k".into(),
        key_generation,
        session_id: session_id.into(),
        agent_id: agent_id.into(),
        record_version: 1,
        payload: format!(r#"{{"id":"{agent_id}"}}"#),
        body: body.map(<[u8]>::to_vec),
        body_write: body.map_or(BodyWrite::Keep, |body| BodyWrite::Seal(body.to_vec())),
        headers: r#"[["anthropic-beta","test-beta"]]"#.into(),
        body_size: body.map_or(0, <[u8]>::len),
        dispatched_ms: body.map(|_| 300_000),
        fast: false,
        updated_at: 2_000,
    }
}

/// A written row as `load_keepalive` answers it, which reads every body back as kept.
fn as_loaded(row: &KeepaliveAgentRow) -> KeepaliveAgentRow {
    KeepaliveAgentRow {
        body_write: BodyWrite::Keep,
        ..row.clone()
    }
}

/// Every row in primary-key order, as `load_keepalive` answers them.
fn sorted(
    mut rows: (
        Vec<KeepaliveSessionRow>,
        Vec<KeepaliveAgentRow>,
        Vec<IdleReturnsRow>,
    ),
) -> (
    Vec<KeepaliveSessionRow>,
    Vec<KeepaliveAgentRow>,
    Vec<IdleReturnsRow>,
) {
    rows.0.sort_by(|a, b| {
        (&a.key_id, a.key_generation, &a.session_id).cmp(&(
            &b.key_id,
            b.key_generation,
            &b.session_id,
        ))
    });
    rows.1.sort_by(|a, b| {
        (&a.key_id, a.key_generation, &a.session_id, &a.agent_id).cmp(&(
            &b.key_id,
            b.key_generation,
            &b.session_id,
            &b.agent_id,
        ))
    });
    rows.2
        .sort_by(|a, b| (&a.pool_id, a.day, a.bucket).cmp(&(&b.pool_id, b.day, b.bucket)));
    rows
}

/// The sealed blob and nonce of one agent row, read past the store.
fn blob(
    conn: &rusqlite::Connection,
    generation: i64,
    session: &str,
    agent: &str,
) -> (Vec<u8>, Vec<u8>) {
    conn.query_row(
        "SELECT body_sealed, body_nonce FROM keepalive_agents \
         WHERE key_id = 'k' AND key_generation = ?1 AND session_id = ?2 AND agent_id = ?3",
        rusqlite::params![generation, session, agent],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .unwrap()
}

fn set_blob(
    conn: &rusqlite::Connection,
    generation: i64,
    session: &str,
    agent: &str,
    (sealed, nonce): &(Vec<u8>, Vec<u8>),
) {
    conn.execute(
        "UPDATE keepalive_agents SET body_sealed = ?4, body_nonce = ?5 \
         WHERE key_id = 'k' AND key_generation = ?1 AND session_id = ?2 AND agent_id = ?3",
        rusqlite::params![generation, session, agent, sealed, nonce],
    )
    .unwrap();
}

/// The body `load_keepalive` opened for one agent row.
fn opened(store: &Store, generation: i64, session: &str, agent: &str) -> Option<Vec<u8>> {
    store
        .load_keepalive()
        .unwrap()
        .1
        .into_iter()
        .find(|row| {
            row.key_generation == generation && row.session_id == session && row.agent_id == agent
        })
        .unwrap_or_else(|| panic!("no row {generation} {session} {agent}"))
        .body
}

#[test]
fn the_keepalive_rows_round_trip_and_the_body_is_sealed_at_rest() {
    assert_eq!(
        keepalive_aad("k", 1, "s:x", "y"),
        "1:k1:13:s:x1:y9:keepalive"
    );
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    let store = Store::open(&path, &StorageKey::generate()).unwrap();
    // Two pairs of identity tuples whose plain concatenation would collide: `s:x` and `y` against `s` and `x:y`,
    // and generation 1 with session `1:s` against generation 11 with session `s`.
    let batch = KeepaliveBatch {
        sessions: vec![
            session(1, "s:x"),
            session(1, "s"),
            session(1, "1:s"),
            session(11, "s"),
        ],
        agents: vec![
            agent(1, "s:x", "y", Some(PROMPT)),
            agent(1, "s", "x:y", Some(b"the second body")),
            agent(1, "s", "main", None),
            agent(1, "1:s", "a", Some(b"the third body")),
            agent(11, "s", "a", Some(b"the fourth body")),
        ],
        removed_sessions: Vec::new(),
        removed_agents: Vec::new(),
        counters: vec![
            IdleReturnsRow {
                pool_id: "main".into(),
                day: 0,
                bucket: 0,
                reached: 3,
                returned: 1,
            },
            IdleReturnsRow {
                pool_id: "main".into(),
                day: 0,
                bucket: 1,
                reached: 2,
                returned: 0,
            },
        ],
        purge_counters_before: None,
    };
    store.save_keepalive(&batch).unwrap();
    assert_eq!(
        sorted(store.load_keepalive().unwrap()),
        sorted((
            batch.sessions.clone(),
            batch.agents.iter().map(as_loaded).collect(),
            batch.counters.clone()
        )),
        "every row reads back as it was written, each body opened"
    );

    let conn = rusqlite::Connection::open(&path).unwrap();
    let (sealed, nonce) = blob(&conn, 1, "s:x", "y");
    assert_eq!(nonce.len(), 12);
    assert!(
        !PROMPT
            .windows(8)
            .any(|run| sealed.windows(8).any(|at| at == run)),
        "no eight-byte run of the prompt sits in the sealed column"
    );

    // A blob copied onto another agent's row fails to open there, and stays readable where it was sealed.
    set_blob(&conn, 1, "s", "main", &(sealed.clone(), nonce.clone()));
    assert_eq!(opened(&store, 1, "s", "main"), None);
    assert_eq!(opened(&store, 1, "s:x", "y"), Some(PROMPT.to_vec()));

    // The colliding tuples, swapped both ways.
    let (first, second) = (blob(&conn, 1, "s:x", "y"), blob(&conn, 1, "s", "x:y"));
    set_blob(&conn, 1, "s:x", "y", &second);
    set_blob(&conn, 1, "s", "x:y", &first);
    assert_eq!(
        (opened(&store, 1, "s:x", "y"), opened(&store, 1, "s", "x:y")),
        (None, None),
        "a colon inside an id moves no boundary of the bound tuple"
    );
    let (one, eleven) = (blob(&conn, 1, "1:s", "a"), blob(&conn, 11, "s", "a"));
    set_blob(&conn, 1, "1:s", "a", &eleven);
    set_blob(&conn, 11, "s", "a", &one);
    assert_eq!(
        (opened(&store, 1, "1:s", "a"), opened(&store, 11, "s", "a")),
        (None, None),
        "nor does a generation's digit run into the session id"
    );

    // An upsert replaces its row, a removed session takes its agents with it, and a removed agent goes alone.
    let mut replaced = agent(11, "s", "a", Some(b"a newer body"));
    replaced.payload = r#"{"id":"a","newer":true}"#.into();
    store
        .save_keepalive(&KeepaliveBatch {
            agents: vec![replaced.clone()],
            removed_sessions: vec![("k".into(), 1, "s:x".into())],
            removed_agents: vec![("k".into(), 1, "s".into(), "x:y".into())],
            counters: vec![IdleReturnsRow {
                pool_id: "main".into(),
                day: 0,
                bucket: 1,
                reached: 5,
                returned: 2,
            }],
            ..KeepaliveBatch::default()
        })
        .unwrap();
    let (sessions, agents, counters) = sorted(store.load_keepalive().unwrap());
    assert_eq!(
        sessions
            .iter()
            .map(|s| (s.key_generation, s.session_id.as_str()))
            .collect::<Vec<_>>(),
        vec![(1, "1:s"), (1, "s"), (11, "s")]
    );
    assert_eq!(
        agents
            .iter()
            .map(|a| (a.key_generation, a.session_id.as_str(), a.agent_id.as_str()))
            .collect::<Vec<_>>(),
        vec![(1, "1:s", "a"), (1, "s", "main"), (11, "s", "a")]
    );
    assert_eq!(agents[2], as_loaded(&replaced));
    assert_eq!(
        counters
            .iter()
            .map(|c| (c.bucket, c.reached, c.returned))
            .collect::<Vec<_>>(),
        vec![(0, 3, 1), (1, 5, 2)]
    );
}

fn counter(pool_id: &str, day: i64, bucket: u32, reached: u64, returned: u64) -> IdleReturnsRow {
    IdleReturnsRow {
        pool_id: pool_id.into(),
        day,
        bucket,
        reached,
        returned,
    }
}

#[test]
fn the_idle_return_counters_keep_a_row_per_pool_day_and_bucket_and_load_in_that_order() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .save_keepalive(&KeepaliveBatch {
            counters: vec![
                counter("main", 11, 2, 5, 2),
                counter("main", 10, 2, 3, 1),
                counter("main", 12, 0, 1, 0),
                counter("aux", 10, 1, 4, 4),
            ],
            ..KeepaliveBatch::default()
        })
        .unwrap();
    assert_eq!(
        store.load_keepalive().unwrap().2,
        [
            counter("aux", 10, 1, 4, 4),
            counter("main", 10, 2, 3, 1),
            counter("main", 11, 2, 5, 2),
            counter("main", 12, 0, 1, 0)
        ],
        "one bucket of one pool on two days is two rows, read by pool, day and bucket"
    );
    store
        .save_keepalive(&KeepaliveBatch {
            counters: vec![counter("main", 11, 2, 6, 3)],
            ..KeepaliveBatch::default()
        })
        .unwrap();
    assert_eq!(
        store.load_keepalive().unwrap().2,
        [
            counter("aux", 10, 1, 4, 4),
            counter("main", 10, 2, 3, 1),
            counter("main", 11, 2, 6, 3),
            counter("main", 12, 0, 1, 0)
        ],
        "a save replaces its own day's row and leaves the other day's"
    );
}

#[test]
fn a_save_carrying_a_purge_drops_the_counters_of_the_days_before_its_day_and_keeps_that_day() {
    let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
    store
        .save_keepalive(&KeepaliveBatch {
            counters: vec![
                counter("aux", 10, 1, 4, 4),
                counter("main", 10, 2, 3, 1),
                counter("main", 11, 2, 5, 2),
                counter("main", 12, 0, 1, 0),
            ],
            ..KeepaliveBatch::default()
        })
        .unwrap();
    store
        .save_keepalive(&KeepaliveBatch {
            counters: vec![counter("main", 11, 2, 6, 3)],
            purge_counters_before: Some(11),
            ..KeepaliveBatch::default()
        })
        .unwrap();
    assert_eq!(
        store.load_keepalive().unwrap().2,
        [counter("main", 11, 2, 6, 3), counter("main", 12, 0, 1, 0)],
        "every pool's rows before the day leave, and the day itself stays with the batch's own row"
    );
    let purge = KeepaliveBatch {
        purge_counters_before: Some(12),
        ..KeepaliveBatch::default()
    };
    assert!(
        !purge.is_empty(),
        "a batch carrying the purge alone still has a write to make"
    );
    store.save_keepalive(&purge).unwrap();
    assert_eq!(
        store.load_keepalive().unwrap().2,
        [counter("main", 12, 0, 1, 0)],
        "the purge alone drops the days before its own"
    );
}

/// A row that keeps its body leaves the sealed body and its nonce as stored while the rest of the row moves, and a row
/// that clears it leaves no body, no nonce and no size behind.
#[test]
fn an_upsert_keeps_the_stored_body_unless_it_seals_a_new_one_and_a_clear_removes_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    let store = Store::open(&path, &StorageKey::generate()).unwrap();
    store
        .save_keepalive(&KeepaliveBatch {
            sessions: vec![session(1, "s")],
            agents: vec![agent(1, "s", "main", Some(PROMPT))],
            ..KeepaliveBatch::default()
        })
        .unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();
    let sealed = blob(&conn, 1, "s", "main");

    let mut kept = agent(1, "s", "main", Some(PROMPT));
    kept.body = None;
    kept.body_write = BodyWrite::Keep;
    kept.payload = r#"{"id":"main","newer":true}"#.into();
    store
        .save_keepalive(&KeepaliveBatch {
            agents: vec![kept],
            ..KeepaliveBatch::default()
        })
        .unwrap();
    assert_eq!(
        blob(&conn, 1, "s", "main"),
        sealed,
        "the sealed body and its nonce stay as stored"
    );
    let (_, agents, _) = store.load_keepalive().unwrap();
    assert_eq!(
        (agents[0].payload.as_str(), agents[0].body.as_deref()),
        (r#"{"id":"main","newer":true}"#, Some(PROMPT)),
        "the row moved and its body still opens"
    );

    let mut cleared = agent(1, "s", "main", None);
    cleared.body_write = BodyWrite::Clear;
    store
        .save_keepalive(&KeepaliveBatch {
            agents: vec![cleared],
            ..KeepaliveBatch::default()
        })
        .unwrap();
    let stored: (Option<Vec<u8>>, Option<Vec<u8>>, i64) = conn
        .query_row(
            "SELECT body_sealed, body_nonce, body_size FROM keepalive_agents WHERE agent_id = 'main'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(stored, (None, None, 0), "a clear leaves no body behind");
    assert_eq!(opened(&store, 1, "s", "main"), None);
}
