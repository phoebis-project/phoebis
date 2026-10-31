//! Rows written before the rollup key carried a generation are stamped with the live key's once at startup,
//! by the key's own lifetime: an earlier holder's rows under a reused id stay at 0.
use phoebis_ledger::{
    read_rollups, reconcile_key_generations, request_by_id, AttemptRecord, KeySelector, LedgerDb,
    LiveKeyLifetime, RequestRecord, RollupSelect,
};

const HOUR_MS: i64 = 3_600_000;

/// A key whose holder is its first: every generation-0 row under the id is its own.
fn first(id: &str, created_at: i64) -> LiveKeyLifetime {
    LiveKeyLifetime {
        id: id.into(),
        created_at,
        floor: None,
    }
}

/// A key created under an id an earlier holder used: its rows start at its first whole hour.
fn reused(id: &str, created_at: i64) -> LiveKeyLifetime {
    LiveKeyLifetime {
        id: id.into(),
        created_at,
        floor: Some(created_at),
    }
}

fn request(id: &str, key_id: &str, key_generation: i64) -> RequestRecord {
    request_at(id, key_id, key_generation, 1_000)
}

fn request_at(id: &str, key_id: &str, key_generation: i64, received_at: i64) -> RequestRecord {
    RequestRecord {
        id: id.into(),
        received_at,
        key_id: Some(key_id.into()),
        key_generation,
        pool_id: Some("p".into()),
        model: "claude-sonnet-5".into(),
        canonical_model: "claude-sonnet-5".into(),
        kind: "inference".into(),
        outcome: "succeeded".into(),
        status: Some(200),
        attempts: 1,
        ..Default::default()
    }
}

/// The generation the request row `id` is filed under, as the request log reads it.
fn generation_of(db: &LedgerDb, id: &str) -> i64 {
    db.read_blocking(|c| request_by_id(c, id))
        .unwrap()
        .expect("the request is on record")
        .0
        .key_generation
}

fn attempt(id: &str) -> AttemptRecord {
    AttemptRecord {
        request_id: id.into(),
        attempt: 1,
        account_id: "a".into(),
        account_kind: "oauth".into(),
        started_at: 1_000,
        outcome: "succeeded".into(),
        input_tokens: Some(100),
        ..Default::default()
    }
}

#[test]
fn generation_zero_rows_of_a_live_key_are_stamped_once_and_the_rest_stay() {
    let db = LedgerDb::open_in_memory().unwrap();
    for (id, key) in [("r1", "k1"), ("r2", "k1"), ("r3", "k2")] {
        db.insert_request(&request(id, key, 0), &[attempt(id)], None)
            .unwrap();
    }
    // A row the new binary already filed under k1's real generation, beside the unstamped ones.
    db.insert_request(&request("r4", "k1", 5_000), &[attempt("r4")], None)
        .unwrap();
    let moved = reconcile_key_generations(&db, &[first("k1", 5_000)]).unwrap();
    assert_eq!(moved, 1, "one rollup row of k1 stood at generation 0");
    let rows = db
        .read_blocking(|c| read_rollups(c, 0, i64::MAX, &RollupSelect::default()))
        .unwrap();
    let by_key: Vec<(&str, i64, u64)> = rows
        .iter()
        .map(|r| (r.key_id.as_str(), r.key_generation, r.requests))
        .collect();
    assert_eq!(
        by_key,
        vec![("k1", 5_000, 3), ("k2", 0, 1)],
        "k1's two unstamped requests joined the row r4 opened; k2 has no live key and stays at 0"
    );
    assert_eq!(
        ["r1", "r2", "r4"].map(|id| generation_of(&db, id)),
        [5_000, 5_000, 5_000]
    );
    assert_eq!(
        reconcile_key_generations(&db, &[first("k1", 9_000), first("k2", 7_000)]).unwrap(),
        0,
        "a second start stamps nothing, whatever keys it brings"
    );
}

#[test]
fn a_reused_ids_rows_before_the_live_keys_first_whole_hour_stay_the_earlier_holders() {
    let db = LedgerDb::open_in_memory().unwrap();
    // The earlier holder's hours, then the live key created mid-hour, then the live key's own hours.
    let created_at = 10 * HOUR_MS + 1_234;
    for (id, at) in [
        ("r1", 5 * HOUR_MS),
        ("r2", 9 * HOUR_MS),
        ("r3", 10 * HOUR_MS + 500),
        ("r4", 11 * HOUR_MS),
        ("r5", 12 * HOUR_MS),
    ] {
        db.insert_request(&request_at(id, "k1", 0, at), &[attempt(id)], None)
            .unwrap();
    }
    let moved = reconcile_key_generations(&db, &[reused("k1", created_at)]).unwrap();
    assert_eq!(
        moved, 2,
        "the two hours at or after the first whole hour of the live key's life"
    );
    let rows = db
        .read_blocking(|c| read_rollups(c, 0, i64::MAX, &RollupSelect::default()))
        .unwrap();
    let by_hour: Vec<(i64, i64)> = rows
        .iter()
        .map(|r| (r.hour / 3_600, r.key_generation))
        .collect();
    assert_eq!(
        by_hour,
        vec![(5, 0), (9, 0), (10, 0), (11, created_at), (12, created_at)],
        "the earlier holder's hours and the mixed creation hour stay at 0; the later hours are the live key's"
    );
    assert_eq!(
        ["r1", "r2", "r3", "r4", "r5"].map(|id| generation_of(&db, id)),
        [0, 0, 0, created_at, created_at],
        "the request rows take the same bound as the rollups"
    );
    assert_eq!(
        reconcile_key_generations(&db, &[reused("k1", created_at)]).unwrap(),
        0,
        "a second start stamps nothing"
    );
}

#[test]
fn a_key_with_more_rows_than_one_page_has_every_row_moved() {
    let db = LedgerDb::open_in_memory().unwrap();
    // One rollup row per hour, over more hours than one page of the fold holds.
    for i in 0..1_250i64 {
        let id = format!("r{i}");
        db.insert_request(
            &request_at(&id, "k1", 0, i * HOUR_MS),
            &[attempt(&id)],
            None,
        )
        .unwrap();
    }
    let moved = reconcile_key_generations(&db, &[first("k1", 5_000)]).unwrap();
    assert_eq!(moved, 1_250);
    let rows_at = |key_generation: i64| {
        let select = RollupSelect {
            keys: vec![KeySelector {
                key_id: "k1".into(),
                key_generation: Some(key_generation),
            }],
            ..RollupSelect::default()
        };
        db.read_blocking(|c| read_rollups(c, 0, i64::MAX, &select))
            .unwrap()
            .len()
    };
    assert_eq!(rows_at(0), 0, "no row is left behind by the page boundary");
    assert_eq!(rows_at(5_000), 1_250);
}
