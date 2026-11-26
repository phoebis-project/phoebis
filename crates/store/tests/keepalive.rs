//! The keepalive engine's rows: four tables in one transaction, and the retained prompt and the reported facts sealed
//! under the storage key and bound to the row they were written for.

use phoebis_store::keepalive::BodyWrite;
use phoebis_store::{
    keepalive_aad, ApiKeyRow, CompanionPermissions, FactsKey, FactsKind, FactsRow, IdleReturnsRow,
    KeepaliveAgentRow, KeepaliveBatch, KeepaliveSessionRow, KeyFeatureAccounts, PoolRow, RulesMode,
    StorageKey, Store,
};

/// The wall clock, for a case that reads no stamp.
fn wall() -> phoebis_store::Clock {
    std::sync::Arc::new(|| jiff::Timestamp::now().as_millisecond())
}

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
        seen_at: None,
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
    let store = Store::open(&path, &StorageKey::generate(), wall()).unwrap();
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
        facts: Vec::new(),
        removed_facts: Vec::new(),
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
    let store = Store::open_in_memory(&StorageKey::generate(), wall()).unwrap();
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
    let store = Store::open_in_memory(&StorageKey::generate(), wall()).unwrap();
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
    let store = Store::open(&path, &StorageKey::generate(), wall()).unwrap();
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

/// The key `k` in the pool `main`, answering the `created_at` it stamped, which is the generation of its sets.
fn inserted_key(store: &Store) -> i64 {
    store.insert_pool(&PoolRow::new("main"), "test").unwrap();
    store
        .insert_api_key(
            &ApiKeyRow {
                id: "k".into(),
                label: "k".into(),
                notes: None,
                pool_id: "main".into(),
                enabled: true,
                revoked_at: None,
                expires_at: None,
                model_rules: Vec::new(),
                model_rules_mode: RulesMode::Extend,
                long_context_twins: true,
                companion_permissions: CompanionPermissions::default(),
                body_capture_hours: None,
                secret_digest: store.api_key_digest("phoebis-k").to_vec(),
                prefix: phoebis_store::key_prefix("phoebis-k"),
                incarnation: 1,
                created_at: 1,
                updated_at: 0,
                rotated_at: None,
                last_used_at: None,
                retired_secret_last_used_at: None,
                auto_mode_server: None,
                feature_accounts: KeyFeatureAccounts::default(),
            },
            "test",
        )
        .unwrap()
        .created_at
}

/// Swaps the sealed value and its nonce between two rows of the session `s` under the key `k`, past the store.
fn swap_sets(
    conn: &rusqlite::Connection,
    generation: i64,
    (first_kind, first_agent): (&str, &str),
    (second_kind, second_agent): (&str, &str),
) {
    let read = |kind: &str, agent: &str| -> (Vec<u8>, Vec<u8>) {
        conn.query_row(
            "SELECT facts_sealed, facts_nonce FROM companion_facts \
             WHERE key_id = 'k' AND key_generation = ?1 AND session_id = 's' AND kind = ?2 AND agent_id = ?3",
            rusqlite::params![generation, kind, agent],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    };
    let write = |kind: &str, agent: &str, (sealed, nonce): &(Vec<u8>, Vec<u8>)| {
        conn.execute(
            "UPDATE companion_facts SET facts_sealed = ?4, facts_nonce = ?5 \
             WHERE key_id = 'k' AND key_generation = ?1 AND session_id = 's' AND kind = ?2 AND agent_id = ?3",
            rusqlite::params![generation, kind, agent, sealed, nonce],
        )
        .unwrap();
    };
    let (first, second) = (
        read(first_kind, first_agent),
        read(second_kind, second_agent),
    );
    write(first_kind, first_agent, &second);
    write(second_kind, second_agent, &first);
}

/// The two agents' rows differ by their agent ids alone, and the session's project row and title row by their kinds
/// alone, so a seal bound to less than the whole key opens one row's set under the other's.
#[test]
fn sealed_sets_swapped_between_two_rows_of_one_session_read_as_no_fact() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    let store = Store::open(&path, &StorageKey::generate(), wall()).unwrap();
    let generation = inserted_key(&store);
    let set = |kind: FactsKind, agent_id: &str, value: &[u8]| FactsRow {
        key: FactsKey {
            key_id: "k".into(),
            key_generation: generation,
            session_id: "s".into(),
            kind,
            agent_id: agent_id.into(),
        },
        record_version: 1,
        value: value.to_vec(),
        seen_at: 1_000,
    };
    let tide: &[u8] = br#"{"name":"tide-reader","type":"Explore"}"#;
    let buoy: &[u8] = br#"{"name":"buoy-keeper","type":"Plan"}"#;
    let project: &[u8] = br#"{"branch":"chart-room","cwd":"/work/lighthouse"}"#;
    let title: &[u8] = br#"{"title":"Map the reef"}"#;
    store
        .save_keepalive(&KeepaliveBatch {
            facts: vec![
                set(FactsKind::Agents, "a1", tide),
                set(FactsKind::Agents, "a2", buoy),
                set(FactsKind::Project, "", project),
                set(FactsKind::Title, "", title),
            ],
            ..KeepaliveBatch::default()
        })
        .unwrap();
    let values = || -> Vec<Option<Vec<u8>>> {
        store
            .load_companion_facts(&[("k".into(), generation, "s".into())])
            .unwrap()
            .into_iter()
            .map(|stored| stored.value)
            .collect()
    };
    assert_eq!(
        values(),
        vec![
            Some(tide.to_vec()),
            Some(buoy.to_vec()),
            Some(project.to_vec()),
            Some(title.to_vec())
        ],
        "each set opens under the row it was written for"
    );

    let conn = rusqlite::Connection::open(&path).unwrap();
    swap_sets(&conn, generation, ("agents", "a1"), ("agents", "a2"));
    swap_sets(&conn, generation, ("project", ""), ("title", ""));
    assert_eq!(
        values(),
        vec![None, None, None, None],
        "no swapped set opens under the other row"
    );
}

/// One stored set of `session_id` under the key `k`, stamped `seen_at`.
fn facts_set(
    generation: i64,
    session_id: &str,
    kind: FactsKind,
    agent_id: &str,
    value: &[u8],
    seen_at: i64,
) -> FactsRow {
    FactsRow {
        key: FactsKey {
            key_id: "k".into(),
            key_generation: generation,
            session_id: session_id.into(),
            kind,
            agent_id: agent_id.into(),
        },
        record_version: 1,
        value: value.to_vec(),
        seen_at,
    }
}

/// Each stored set of `sessions` under the key `k` that stands, as `(session, kind, agent)`, in the order of the read.
fn standing(store: &Store, generation: i64, sessions: &[&str]) -> Vec<(String, FactsKind, String)> {
    let sessions: Vec<(String, i64, String)> = sessions
        .iter()
        .map(|session| ("k".to_string(), generation, session.to_string()))
        .collect();
    store
        .load_companion_facts(&sessions)
        .unwrap()
        .into_iter()
        .map(|set| (set.key.session_id, set.key.kind, set.key.agent_id))
        .collect()
}

#[test]
fn an_agents_set_stamped_at_the_cutoff_stays_and_one_a_millisecond_before_it_leaves() {
    let store = Store::open_in_memory(&StorageKey::generate(), wall()).unwrap();
    let generation = inserted_key(&store);
    store
        .save_keepalive(&KeepaliveBatch {
            facts: vec![
                facts_set(
                    generation,
                    "s",
                    FactsKind::Agents,
                    "a1",
                    br#"{"name":"pier-gauge"}"#,
                    1_000_000,
                ),
                facts_set(
                    generation,
                    "s",
                    FactsKind::Agents,
                    "a2",
                    br#"{"name":"tide-clerk"}"#,
                    999_999,
                ),
            ],
            ..KeepaliveBatch::default()
        })
        .unwrap();
    store
        .purge_companion_facts(Some(1_000_000), 1_000_000)
        .unwrap();
    assert_eq!(
        standing(&store, generation, &["s"]),
        vec![("s".to_string(), FactsKind::Agents, "a1".to_string())],
        "the set stamped at the cutoff stays and the one a millisecond before it leaves"
    );
}

#[test]
fn a_sessions_title_leaves_at_the_session_cutoff_and_its_agents_set_stays() {
    let store = Store::open_in_memory(&StorageKey::generate(), wall()).unwrap();
    let generation = inserted_key(&store);
    store
        .save_keepalive(&KeepaliveBatch {
            facts: vec![
                facts_set(
                    generation,
                    "s1",
                    FactsKind::Agents,
                    "a1",
                    br#"{"name":"rigging-checker"}"#,
                    999_999,
                ),
                facts_set(
                    generation,
                    "s1",
                    FactsKind::Project,
                    "",
                    br#"{"branch":"sea-wall-survey","cwd":null,"project":null,"repository":null,"repository_root":null}"#,
                    999_999,
                ),
                facts_set(
                    generation,
                    "s1",
                    FactsKind::Title,
                    "",
                    br#"{"title":"Caulk the hull seams"}"#,
                    999_999,
                ),
                facts_set(
                    generation,
                    "s2",
                    FactsKind::Title,
                    "",
                    br#"{"title":"Chart the tide rips"}"#,
                    1_000_000,
                ),
            ],
            ..KeepaliveBatch::default()
        })
        .unwrap();
    store
        .purge_companion_facts(Some(500_000), 1_000_000)
        .unwrap();
    assert_eq!(
        standing(&store, generation, &["s1", "s2"]),
        vec![
            ("s1".to_string(), FactsKind::Agents, "a1".to_string()),
            ("s2".to_string(), FactsKind::Title, String::new())
        ],
        "the project set and the title before the session cutoff leave, the agent's set inside the request cutoff \
         stays, and so does a title stamped at the session cutoff"
    );
}

#[test]
fn a_save_stamps_each_set_of_the_session_whose_row_it_writes_and_no_other_sessions() {
    let store = Store::open_in_memory(&StorageKey::generate(), wall()).unwrap();
    let generation = inserted_key(&store);
    store
        .save_keepalive(&KeepaliveBatch {
            facts: vec![
                facts_set(
                    generation,
                    "s1",
                    FactsKind::Title,
                    "",
                    br#"{"title":"Sweep the slipway"}"#,
                    1_000,
                ),
                facts_set(
                    generation,
                    "s2",
                    FactsKind::Title,
                    "",
                    br#"{"title":"Grease the capstan"}"#,
                    1_000,
                ),
            ],
            ..KeepaliveBatch::default()
        })
        .unwrap();
    store
        .save_keepalive(&KeepaliveBatch {
            sessions: vec![KeepaliveSessionRow {
                seen_at: Some(2_000),
                ..session(generation, "s1")
            }],
            facts: vec![facts_set(
                generation,
                "s1",
                FactsKind::Agents,
                "a1",
                br#"{"name":"winch-tender"}"#,
                1_500,
            )],
            ..KeepaliveBatch::default()
        })
        .unwrap();
    // Each cutoff lies a millisecond past a filing instant, so a set stands only where the stamp moved it to 2,000.
    store.purge_companion_facts(Some(1_501), 1_001).unwrap();
    assert_eq!(
        standing(&store, generation, &["s1", "s2"]),
        vec![
            ("s1".to_string(), FactsKind::Agents, "a1".to_string()),
            ("s1".to_string(), FactsKind::Title, String::new())
        ],
        "the session's row stamps its earlier set and the one its own batch writes, and no set of another session"
    );
}
