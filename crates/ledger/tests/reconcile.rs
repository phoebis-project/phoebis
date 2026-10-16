//! Each completed 5-hour, 7-day and Fable window of an account, reconciled: the rise its readings showed against the
//! share the router's own plan-funded attempts completed between those readings were priced at, each under its own
//! regime, on the ledger's test tables so every figure here is worked out by hand from the fixture.

use phoebis_ledger::test_tables::{api_prices, subscription_costs, JANUARY_2026};
use phoebis_ledger::{
    bucket_of, read_reconciliations, reconcile, AttemptRecord, LedgerDb, QuotaSampleRecord,
    ReconciliationRow, RequestRecord,
};
use phoebis_routing::{FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE};

const MINUTE: i64 = 60_000;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;
/// Saturday 2026-01-17 at 12:00 UTC: inside the fixture's first cost entry, and the peak band keeps to weekdays, so
/// every share here is read off the band.
const SATURDAY: i64 = JANUARY_2026 + 2 * DAY;
const MAX_20X: &str = "default_claude_max_20x";
/// Weight 1 in the fixture's first entry, so `n` input tokens are `n` units: `n / 100` of the Max 20x 5-hour window
/// and `n / 1000` of its week, `n / 200` of the 5-hour window under the `desk` regime.
const SONNET: &str = "claude-sonnet-5-5";
/// Weight 2.
const OPUS: &str = "claude-opus-5-5";
/// Weight 10, on a week of its own at half the 7-day capacity.
const FABLE: &str = "claude-fable-5-1";
/// Unweighted, so it prices through its model tier's latest weighted model, Opus 5.5 at weight 2, as an estimate.
const OPUS_UNWEIGHTED: &str = "claude-opus-5";

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * b.abs().max(1.0)
}

/// A request with no attempt, received at `at`: what tells the reconciliation the ledger's requests reach back that
/// far.
fn seed_request(db: &LedgerDb, id: &str, at: i64) {
    db.insert_request(
        &RequestRecord {
            id: id.into(),
            received_at: at,
            finished_at: Some(at),
            model: SONNET.into(),
            canonical_model: SONNET.into(),
            kind: "inference".into(),
            origin: "client".into(),
            outcome: "succeeded".into(),
            ..Default::default()
        },
        &[],
        None,
    )
    .unwrap();
}

/// One request of `client` whose single plan-funded attempt on Max 20x account `a` completed at `completed` with
/// `input` plain input tokens of `model`.
fn served(db: &LedgerDb, id: &str, model: &str, completed: i64, input: u64, client: Option<&str>) {
    answered(db, id, model, completed, input, client, None);
}

/// `served` with the overage flag the attempt's head carried, `Some(true)` for an answer billed in overage.
fn answered(
    db: &LedgerDb,
    id: &str,
    model: &str,
    completed: i64,
    input: u64,
    client: Option<&str>,
    overage_in_use: Option<bool>,
) {
    let started = completed - 30_000;
    db.insert_request(
        &RequestRecord {
            id: id.into(),
            received_at: started,
            finished_at: Some(completed),
            model: model.into(),
            canonical_model: model.into(),
            kind: "inference".into(),
            origin: "client".into(),
            outcome: "succeeded".into(),
            attempts: 1,
            client: client.map(String::from),
            ..Default::default()
        },
        &[AttemptRecord {
            request_id: id.into(),
            attempt: 1,
            account_id: "a".into(),
            account_kind: "oauth".into(),
            started_at: started,
            finished_at: Some(completed),
            outcome: "succeeded".into(),
            input_tokens: Some(input),
            output_tokens: Some(0),
            tier: Some(MAX_20X.into()),
            overage_in_use,
            ..Default::default()
        }],
        None,
    )
    .unwrap();
}

/// A poll's reading of account `a`'s `window` observed at `at`.
fn reading(window: &str, at: i64, utilization: f64, reset_at: i64) -> QuotaSampleRecord {
    QuotaSampleRecord {
        account_id: "a".into(),
        bucket: bucket_of(at),
        window: window.into(),
        utilization: Some(utilization),
        reset_at: Some(reset_at),
        source: "poll".into(),
        observed_at: Some(at),
    }
}

/// One hourly reconciliation at `now` on the test tables, on its own transaction as the fold's.
fn fold(db: &LedgerDb, now: i64) {
    let (prices, tables) = (api_prices(), subscription_costs());
    db.write(|c| {
        let tx = c.transaction()?;
        reconcile(&tx, &prices, &tables, now)?;
        tx.commit()?;
        Ok(())
    })
    .unwrap();
}

/// The rows of the trailing 30 days at `now`, every account's.
fn rows(db: &LedgerDb, now: i64) -> Vec<ReconciliationRow> {
    db.read_blocking(|c| read_reconciliations(c, &[], now))
        .unwrap()
}

#[test]
fn a_complete_run_prices_its_attempts_and_writes_the_ratio() {
    let reset = SATURDAY + 4 * HOUR;
    let db = LedgerDb::open_in_memory().unwrap();
    seed_request(&db, "seed", SATURDAY - MINUTE);
    db.insert_samples(
        &[
            reading(FIVE_HOUR, SATURDAY, 0.20, reset),
            reading(FIVE_HOUR, SATURDAY + 20 * MINUTE, 0.50, reset),
        ],
        None,
    )
    .unwrap();
    // 0.10 and 0.15 of the window: ten and fifteen Sonnet units over the tier's hundred.
    served(&db, "r1", SONNET, SATURDAY + 5 * MINUTE, 10, None);
    served(&db, "r2", SONNET, SATURDAY + 15 * MINUTE, 15, None);
    let now = SATURDAY + 30 * MINUTE;
    fold(&db, now);
    assert_eq!(
        rows(&db, now),
        [],
        "the reset lies ahead and no later one was read, so the window can still take charges"
    );
    // The window reset early: a reading names the next reset, which closes the run.
    db.insert_samples(
        &[reading(
            FIVE_HOUR,
            SATURDAY + 25 * MINUTE,
            0.0,
            reset + 5 * HOUR,
        )],
        None,
    )
    .unwrap();
    fold(&db, now);
    let rows = rows(&db, now);
    assert_eq!(
        rows.iter()
            .map(|r| (
                r.account_id.as_str(),
                r.window.as_str(),
                r.window_end,
                r.first_reading_at,
                r.last_reading_at,
                r.requests,
                r.estimated,
                r.computed_at,
            ))
            .collect::<Vec<_>>(),
        [(
            "a",
            FIVE_HOUR,
            reset,
            SATURDAY,
            SATURDAY + 20 * MINUTE,
            2,
            0,
            now
        )],
        "one row for the closed run, the new run of one reading left open: {rows:?}"
    );
    let row = &rows[0];
    assert!(close(row.observed_rise, 0.30), "{}", row.observed_rise);
    assert!(close(row.predicted_share, 0.25), "{}", row.predicted_share);
    assert!(close(row.ratio, 1.2), "0.30 over 0.25: {}", row.ratio);
}

#[test]
fn a_run_of_one_reading_is_skipped() {
    let reset = SATURDAY + HOUR;
    let db = LedgerDb::open_in_memory().unwrap();
    seed_request(&db, "seed", SATURDAY - MINUTE);
    db.insert_samples(&[reading(FIVE_HOUR, SATURDAY, 0.10, reset)], None)
        .unwrap();
    served(&db, "r1", SONNET, SATURDAY + 5 * MINUTE, 20, None);
    let now = reset + HOUR;
    fold(&db, now);
    assert_eq!(
        rows(&db, now),
        [],
        "one reading shows no rise, however long ago its reset passed"
    );
    db.insert_samples(
        &[reading(FIVE_HOUR, SATURDAY + 10 * MINUTE, 0.30, reset)],
        None,
    )
    .unwrap();
    fold(&db, now);
    assert_eq!(
        rows(&db, now)
            .iter()
            .map(|r| (r.window.as_str(), r.requests))
            .collect::<Vec<_>>(),
        [(FIVE_HOUR, 1)],
        "a second reading of the same reset makes the run"
    );
}

#[test]
fn a_fall_in_utilization_is_skipped() {
    let reset = SATURDAY + HOUR;
    let db = LedgerDb::open_in_memory().unwrap();
    seed_request(&db, "seed", SATURDAY - MINUTE);
    db.insert_samples(
        &[
            reading(FIVE_HOUR, SATURDAY, 0.80, reset),
            reading(SEVEN_DAY, SATURDAY, 0.10, reset),
            reading(FIVE_HOUR, SATURDAY + 10 * MINUTE, 0.10, reset),
            reading(SEVEN_DAY, SATURDAY + 10 * MINUTE, 0.12, reset),
        ],
        None,
    )
    .unwrap();
    served(&db, "r1", SONNET, SATURDAY + 5 * MINUTE, 20, None);
    let now = reset + HOUR;
    fold(&db, now);
    assert_eq!(
        rows(&db, now)
            .iter()
            .map(|r| r.window.as_str())
            .collect::<Vec<_>>(),
        [SEVEN_DAY],
        "a fall under one reset is a reset the readings missed, while the week's rise reads"
    );
}

#[test]
fn a_run_whose_first_reading_predates_the_oldest_retained_request_is_skipped() {
    let five_reset = SATURDAY + HOUR;
    let seven_reset = SATURDAY + 2 * HOUR;
    let db = LedgerDb::open_in_memory().unwrap();
    seed_request(&db, "seed", SATURDAY - 5 * MINUTE);
    db.insert_samples(
        &[
            reading(FIVE_HOUR, SATURDAY - 10 * MINUTE, 0.10, five_reset),
            reading(SEVEN_DAY, SATURDAY, 0.01, seven_reset),
            reading(FIVE_HOUR, SATURDAY + 20 * MINUTE, 0.30, five_reset),
            reading(SEVEN_DAY, SATURDAY + 20 * MINUTE, 0.03, seven_reset),
        ],
        None,
    )
    .unwrap();
    served(&db, "r1", SONNET, SATURDAY + 10 * MINUTE, 20, None);
    let now = seven_reset + HOUR;
    fold(&db, now);
    assert_eq!(
        rows(&db, now)
            .iter()
            .map(|r| r.window.as_str())
            .collect::<Vec<_>>(),
        [SEVEN_DAY],
        "the 5-hour run began before the oldest request kept, so its rise would meet part of what the router sent"
    );
}

#[test]
fn the_fable_weeks_run_prices_the_fable_attempts_alone() {
    let reset = SATURDAY + HOUR;
    let db = LedgerDb::open_in_memory().unwrap();
    seed_request(&db, "seed", SATURDAY - MINUTE);
    db.insert_samples(
        &[
            reading(SEVEN_DAY, SATURDAY, 0.10, reset),
            reading(SEVEN_DAY_FABLE, SATURDAY, 0.10, reset),
            reading(SEVEN_DAY, SATURDAY + 20 * MINUTE, 0.25, reset),
            reading(SEVEN_DAY_FABLE, SATURDAY + 20 * MINUTE, 0.30, reset),
        ],
        None,
    )
    .unwrap();
    // Fable: 100 units, 0.1 of the week and 0.2 of the Fable week at half the week's capacity. Opus: 50 units, 0.05
    // of the week and none of the Fable week.
    served(&db, "fable", FABLE, SATURDAY + 5 * MINUTE, 10, None);
    served(&db, "opus", OPUS, SATURDAY + 15 * MINUTE, 25, None);
    let now = reset + HOUR;
    fold(&db, now);
    let rows = rows(&db, now);
    let window = |name: &str| {
        rows.iter()
            .find(|r| r.window == name)
            .unwrap_or_else(|| panic!("no {name} row: {rows:?}"))
    };
    let (fable, week) = (window(SEVEN_DAY_FABLE), window(SEVEN_DAY));
    assert!(
        close(fable.predicted_share, 0.2) && fable.requests == 1,
        "the Fable week prices the Fable attempt alone: {fable:?}"
    );
    assert!(close(fable.ratio, 1.0), "{}", fable.ratio);
    assert!(
        close(week.predicted_share, 0.15) && week.requests == 2,
        "the week prices both: {week:?}"
    );
}

#[test]
fn a_run_mixing_two_regimes_prices_each_attempt_under_its_own() {
    let reset = SATURDAY + HOUR;
    let db = LedgerDb::open_in_memory().unwrap();
    seed_request(&db, "seed", SATURDAY - MINUTE);
    db.insert_samples(
        &[
            reading(FIVE_HOUR, SATURDAY, 0.10, reset),
            reading(FIVE_HOUR, SATURDAY + 20 * MINUTE, 0.40, reset),
        ],
        None,
    )
    .unwrap();
    // Twenty Sonnet units each: 0.1 of the window under `desk`, which sizes Max 20x at 200, and 0.2 at the base.
    served(
        &db,
        "desk",
        SONNET,
        SATURDAY + 5 * MINUTE,
        20,
        Some("claude-code/desk"),
    );
    served(&db, "base", SONNET, SATURDAY + 15 * MINUTE, 20, None);
    let now = reset + HOUR;
    fold(&db, now);
    let rows = rows(&db, now);
    assert_eq!(rows.len(), 1, "one row per account and window: {rows:?}");
    assert!(
        close(rows[0].predicted_share, 0.3),
        "0.1 under the regime and 0.2 at the base: {}",
        rows[0].predicted_share
    );
    assert!(
        close(rows[0].ratio, 1.0),
        "the rise the two drew: {}",
        rows[0].ratio
    );
}

#[test]
fn rows_older_than_thirty_days_leave_with_the_fold() {
    let now = SATURDAY + 40 * DAY;
    let db = LedgerDb::open_in_memory().unwrap();
    db.write(|c| {
        for window_end in [now - 30 * DAY - 1, now - 30 * DAY] {
            c.execute(
                "INSERT INTO window_reconciliations (account_id, window, window_end, first_reading_at, \
                 last_reading_at, observed_rise, predicted_share, ratio, requests, estimated, computed_at) \
                 VALUES ('a', 'five_hour', ?1, ?1, ?1, 0.2, 0.2, 1.0, 1, 0, ?1)",
                [window_end],
            )?;
        }
        Ok(())
    })
    .unwrap();
    fold(&db, now);
    let kept: Vec<i64> = db
        .read_blocking(|c| {
            let mut stmt = c.prepare("SELECT window_end FROM window_reconciliations")?;
            let ends = stmt
                .query_map([], |r| r.get(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(ends)
        })
        .unwrap();
    assert_eq!(
        kept,
        [now - 30 * DAY],
        "a row whose window ended more than 30 days before the fold leaves, one at the edge stays"
    );
}

#[test]
fn a_second_fold_writes_no_duplicate_row() {
    let reset = SATURDAY + HOUR;
    let db = LedgerDb::open_in_memory().unwrap();
    seed_request(&db, "seed", SATURDAY - MINUTE);
    db.insert_samples(
        &[
            reading(FIVE_HOUR, SATURDAY, 0.10, reset),
            reading(FIVE_HOUR, SATURDAY + 20 * MINUTE, 0.30, reset),
        ],
        None,
    )
    .unwrap();
    served(&db, "r1", SONNET, SATURDAY + 5 * MINUTE, 20, None);
    fold(&db, reset);
    assert_eq!(
        rows(&db, reset),
        [],
        "a reset at the fold's instant has not passed"
    );
    fold(&db, reset + 1);
    fold(&db, reset + HOUR);
    assert_eq!(
        rows(&db, reset + HOUR)
            .iter()
            .map(|r| (r.window_end, r.computed_at))
            .collect::<Vec<_>>(),
        [(reset, reset + 1)],
        "the reset a millisecond behind closes the run, and the next fold keeps the first row"
    );
}

#[test]
fn an_estimated_attempt_is_counted_and_an_overage_attempt_draws_nothing() {
    let reset = SATURDAY + HOUR;
    let db = LedgerDb::open_in_memory().unwrap();
    seed_request(&db, "seed", SATURDAY - MINUTE);
    db.insert_samples(
        &[
            reading(FIVE_HOUR, SATURDAY, 0.10, reset),
            reading(FIVE_HOUR, SATURDAY + 20 * MINUTE, 0.40, reset),
        ],
        None,
    )
    .unwrap();
    // 0.1 of the window from ten Sonnet units, 0.2 from ten tokens of Opus 5 at Opus 5.5's weight of 2, and nothing
    // from thirty Sonnet units the head billed in overage.
    served(&db, "sonnet", SONNET, SATURDAY + 5 * MINUTE, 10, None);
    served(
        &db,
        "fallback",
        OPUS_UNWEIGHTED,
        SATURDAY + 10 * MINUTE,
        10,
        None,
    );
    answered(
        &db,
        "overage",
        SONNET,
        SATURDAY + 15 * MINUTE,
        30,
        None,
        Some(true),
    );
    let now = reset + HOUR;
    fold(&db, now);
    let rows = rows(&db, now);
    assert_eq!(
        rows.iter()
            .map(|r| (r.requests, r.estimated))
            .collect::<Vec<_>>(),
        [(2, 1)],
        "the two plan-funded attempts, one priced through its model tier: {rows:?}"
    );
    assert!(
        close(rows[0].predicted_share, 0.3),
        "0.1 and 0.2, the overage answer adding nothing: {}",
        rows[0].predicted_share
    );
    assert!(close(rows[0].ratio, 1.0), "{}", rows[0].ratio);
}
