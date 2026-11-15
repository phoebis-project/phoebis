//! Each completed 5-hour, 7-day and Fable window of an account, reconciled: the rise its readings showed against the
//! share the router's own plan-funded attempts completed between those readings were priced at, each under its own
//! regime, on the ledger's test tables so every figure here is worked out by hand from the fixture.

use phoebis_ledger::test_tables::{api_prices, subscription_costs, JANUARY_2026};
use phoebis_ledger::{
    bucket_of, fold_routing_conditions, newest_rows, read_reconciliations, reconcile_read,
    reconcile_write, routing_conditions, AttemptRecord, LedgerDb, QuotaSampleRecord,
    ReconciliationRow, RequestRecord, SampleSource, MIN_RISE, RATIO_HIGH, RATIO_LOW,
};
use phoebis_routing::{FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE};
use phoebis_store::AccountKind;

const MINUTE: i64 = 60_000;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;
/// Saturday 2026-01-17 at 12:00 UTC: inside the fixture's first cost entry, and the peak band keeps to weekdays, so
/// every share here is read off the band.
const SATURDAY: i64 = JANUARY_2026 + 2 * DAY;
const MAX_20X: &str = "default_claude_max_20x";
/// Weight 1 in the fixture's first entry, so `n` input tokens are `n` units: `n / 100` of the Ultra 5-hour window
/// and `n / 1000` of its week, `n / 200` of the 5-hour window under the `desk` regime.
const SONNET: &str = "claude-sonnet-5-5";
/// Weight 2.
const OPUS: &str = "claude-opus-5-5";
/// Weight 10, on a week of its own at half the 7-day capacity.
const FABLE: &str = "claude-fable-5-1";
/// Of the model tier `mystery`, which no weighted model shares, so it prices to no share.
const MYSTERY: &str = "claude-mystery-2";

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * b.abs().max(1.0)
}

/// A request with no attempt, received at `at`.
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

/// One request of `client` whose single plan-funded attempt on Ultra account `a` completed at `completed` with
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
            account_kind: AccountKind::Oauth,
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

/// `served` as the client's own classifier call: the same plan-funded attempt, on a request the marker held.
fn served_as_classifier(db: &LedgerDb, id: &str, model: &str, completed: i64, input: u64) {
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
            classifier: Some("client".into()),
            ..Default::default()
        },
        &[AttemptRecord {
            request_id: id.into(),
            attempt: 1,
            account_id: "a".into(),
            account_kind: AccountKind::Oauth,
            started_at: started,
            finished_at: Some(completed),
            outcome: "succeeded".into(),
            input_tokens: Some(input),
            output_tokens: Some(0),
            tier: Some(MAX_20X.into()),
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
        source: SampleSource::Poll,
        observed_at: at,
    }
}

/// A head's reading of account `a`'s `window` observed at `at`. A head and a poll in one five-minute bucket leave the
/// head alone, so a case mixing the two puts every reading in a bucket of its own.
fn head_reading(window: &str, at: i64, utilization: f64, reset_at: i64) -> QuotaSampleRecord {
    QuotaSampleRecord {
        source: SampleSource::Head,
        ..reading(window, at, utilization, reset_at)
    }
}

/// One hourly reconciliation at `now` on the test tables with nothing purged.
fn fold(db: &LedgerDb, now: i64) {
    fold_under(db, None, now);
}

/// `fold` under the requests' retention cutoff `request_cutoff`, in the fold's order: the read pass, then the write on
/// a transaction of its own.
fn fold_under(db: &LedgerDb, request_cutoff: Option<i64>, now: i64) {
    let (prices, tables) = (api_prices(), subscription_costs());
    db.write(|c| {
        let rows = reconcile_read(c, &prices, &tables, request_cutoff, now)?;
        let tx = c.transaction()?;
        reconcile_write(&tx, &rows, now)?;
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
    db.insert_samples(
        &[reading(
            FIVE_HOUR,
            reset + 5 * MINUTE,
            0.0,
            reset + 5 * HOUR,
        )],
        None,
    )
    .unwrap();
    let now = reset + 10 * MINUTE;
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
fn every_account_with_a_reading_reconciles_its_runs() {
    let reset = SATURDAY + HOUR;
    let db = LedgerDb::open_in_memory().unwrap();
    seed_request(&db, "seed", SATURDAY - MINUTE);
    let of = |account: &str, sample: QuotaSampleRecord| QuotaSampleRecord {
        account_id: account.into(),
        ..sample
    };
    db.insert_samples(
        &[
            reading(FIVE_HOUR, SATURDAY, 0.10, reset),
            reading(FIVE_HOUR, SATURDAY + 20 * MINUTE, 0.20, reset),
            of("b", reading(FIVE_HOUR, SATURDAY, 0.10, reset)),
            of("b", reading(FIVE_HOUR, SATURDAY + 20 * MINUTE, 0.20, reset)),
        ],
        None,
    )
    .unwrap();
    served(&db, "a1", SONNET, SATURDAY + 5 * MINUTE, 10, None);
    let completed = SATURDAY + 5 * MINUTE;
    db.insert_request(
        &RequestRecord {
            id: "b1".into(),
            received_at: completed - 30_000,
            finished_at: Some(completed),
            model: SONNET.into(),
            canonical_model: SONNET.into(),
            kind: "inference".into(),
            origin: "client".into(),
            outcome: "succeeded".into(),
            attempts: 1,
            ..Default::default()
        },
        &[AttemptRecord {
            request_id: "b1".into(),
            attempt: 1,
            account_id: "b".into(),
            account_kind: AccountKind::Oauth,
            started_at: completed - 30_000,
            finished_at: Some(completed),
            outcome: "succeeded".into(),
            input_tokens: Some(10),
            output_tokens: Some(0),
            tier: Some(MAX_20X.into()),
            ..Default::default()
        }],
        None,
    )
    .unwrap();
    let now = reset + HOUR;
    fold(&db, now);
    let rows = rows(&db, now);
    assert_eq!(
        rows.iter()
            .map(|r| r.account_id.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"],
        "each account's run writes its row: {rows:?}"
    );
}

#[test]
fn a_five_point_rise_between_whole_percent_readings_meets_the_least_rise() {
    let reset = SATURDAY + HOUR;
    let db = LedgerDb::open_in_memory().unwrap();
    seed_request(&db, "seed", SATURDAY - MINUTE);
    db.insert_samples(
        &[
            reading(FIVE_HOUR, SATURDAY, 0.10, reset),
            reading(FIVE_HOUR, SATURDAY + 20 * MINUTE, 0.15, reset),
        ],
        None,
    )
    .unwrap();
    // 0.02 of the window from two Sonnet units, so the five points read 2.5 times the priced share.
    served(&db, "r1", SONNET, SATURDAY + 5 * MINUTE, 2, None);
    let now = reset + HOUR;
    fold(&db, now);
    let newest = db.read_blocking(|c| newest_rows(c, now)).unwrap();
    let rows = newest.get("a").map(Vec::as_slice).unwrap_or_default();
    assert_eq!(
        rows.iter().map(|r| r.window.as_str()).collect::<Vec<_>>(),
        [FIVE_HOUR],
        "a rise of five points counts toward the drift condition: {newest:?}"
    );
    let row = &rows[0];
    assert!(row.observed_rise >= MIN_RISE, "{}", row.observed_rise);
    assert!(close(row.ratio, 2.5), "0.05 over 0.02: {}", row.ratio);
    assert!(
        !(RATIO_LOW..=RATIO_HIGH).contains(&row.ratio),
        "the row reads off by its ratio only over a priced share above the floor: {}",
        row.ratio
    );
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
fn a_fall_inside_a_run_splits_it_and_the_row_sums_its_parts() {
    let reset = SATURDAY + HOUR;
    let db = LedgerDb::open_in_memory().unwrap();
    seed_request(&db, "seed", SATURDAY - MINUTE);
    // A grant empties the window between the second and third readings: a rise of 0.84 before it and 0.10 after.
    db.insert_samples(
        &[
            reading(FIVE_HOUR, SATURDAY, 0.05, reset),
            reading(FIVE_HOUR, SATURDAY + 10 * MINUTE, 0.89, reset),
            reading(FIVE_HOUR, SATURDAY + 20 * MINUTE, 0.0, reset),
            reading(FIVE_HOUR, SATURDAY + 30 * MINUTE, 0.10, reset),
        ],
        None,
    )
    .unwrap();
    // 0.84 of the window from 84 Sonnet units before the grant, and 0.10 after it from five tokens of Opus 5.5 at its
    // weight of 2.
    served(&db, "before", SONNET, SATURDAY + 5 * MINUTE, 84, None);
    served(&db, "after", OPUS, SATURDAY + 25 * MINUTE, 5, None);
    let now = reset + HOUR;
    fold(&db, now);
    let rows = rows(&db, now);
    assert_eq!(
        rows.iter()
            .map(|r| (
                r.window.as_str(),
                r.first_reading_at,
                r.last_reading_at,
                r.requests
            ))
            .collect::<Vec<_>>(),
        [(FIVE_HOUR, SATURDAY, SATURDAY + 30 * MINUTE, 2)],
        "one row for the window, from the first part's first reading to the second part's last: {rows:?}"
    );
    let row = &rows[0];
    assert!(
        close(row.observed_rise, 0.94),
        "0.84 and 0.10: {}",
        row.observed_rise
    );
    assert!(close(row.predicted_share, 0.94), "{}", row.predicted_share);
    assert!(close(row.ratio, 1.0), "{}", row.ratio);
}

/// The readings of a run split at a fall: 0.05 to 0.89 before a grant empties the window, then 0.0 to 0.10.
fn a_run_split_at_a_fall(db: &LedgerDb, reset: i64) {
    db.insert_samples(
        &[
            reading(FIVE_HOUR, SATURDAY, 0.05, reset),
            reading(FIVE_HOUR, SATURDAY + 10 * MINUTE, 0.89, reset),
            reading(FIVE_HOUR, SATURDAY + 20 * MINUTE, 0.0, reset),
            reading(FIVE_HOUR, SATURDAY + 30 * MINUTE, 0.10, reset),
        ],
        None,
    )
    .unwrap();
}

#[test]
fn an_attempt_the_tables_cannot_price_in_one_part_skips_the_whole_row() {
    let reset = SATURDAY + HOUR;
    let db = LedgerDb::open_in_memory().unwrap();
    seed_request(&db, "seed", SATURDAY - MINUTE);
    a_run_split_at_a_fall(&db, reset);
    // 0.84 of the window from 84 Sonnet units before the grant, and after it an attempt on a model of a tier with no
    // weighted model, which the tables price to no share whatever its tokens.
    served(&db, "before", SONNET, SATURDAY + 5 * MINUTE, 84, None);
    served(&db, "after", MYSTERY, SATURDAY + 25 * MINUTE, 20, None);
    let now = reset + HOUR;
    fold(&db, now);
    assert_eq!(
        rows(&db, now),
        [],
        "the second part's rise carries a draw no share prices, so the window writes no row"
    );
}

#[test]
fn a_part_with_no_priced_attempt_adds_nothing_and_the_other_part_writes_the_row() {
    let reset = SATURDAY + HOUR;
    let db = LedgerDb::open_in_memory().unwrap();
    seed_request(&db, "seed", SATURDAY - MINUTE);
    a_run_split_at_a_fall(&db, reset);
    // 0.84 of the window from 84 Sonnet units before the grant, and no attempt after it.
    served(&db, "before", SONNET, SATURDAY + 5 * MINUTE, 84, None);
    let now = reset + HOUR;
    fold(&db, now);
    let rows = rows(&db, now);
    assert_eq!(
        rows.iter()
            .map(|r| (
                r.window.as_str(),
                r.first_reading_at,
                r.last_reading_at,
                r.requests
            ))
            .collect::<Vec<_>>(),
        [(FIVE_HOUR, SATURDAY, SATURDAY + 10 * MINUTE, 1)],
        "the row reads the first part alone: {rows:?}"
    );
    let row = &rows[0];
    assert!(close(row.observed_rise, 0.84), "{}", row.observed_rise);
    assert!(close(row.predicted_share, 0.84), "{}", row.predicted_share);
}

/// Polls of account `a`'s 5-hour window ten minutes apart from Saturday noon, each naming `reset`.
fn polls(db: &LedgerDb, reset: i64, utilizations: &[f64]) {
    let readings: Vec<QuotaSampleRecord> = utilizations
        .iter()
        .zip(0i64..)
        .map(|(utilization, i)| reading(FIVE_HOUR, SATURDAY + i * 10 * MINUTE, *utilization, reset))
        .collect();
    db.insert_samples(&readings, None).unwrap();
}

/// `units` Sonnet units, `units / 100` of the 5-hour window, completed between the poll `after` of `polls` and the
/// next.
fn served_after_poll(db: &LedgerDb, after: i64, units: u64) {
    let completed = SATURDAY + after * 10 * MINUTE + 5 * MINUTE;
    served(
        db,
        &format!("after-{after}"),
        SONNET,
        completed,
        units,
        None,
    );
}

/// The one row a fold an hour past `reset` writes.
fn the_row(db: &LedgerDb, reset: i64) -> ReconciliationRow {
    let now = reset + HOUR;
    fold(db, now);
    let rows = rows(db, now);
    assert_eq!(rows.len(), 1, "one row for the window: {rows:?}");
    rows[0].clone()
}

/// A failed poll's zero between two readings at 0.54, with no attempt between the zero and the next reading: 0.04 to
/// 0.54 from fifty Sonnet units, then 0.54 to 0.67 from fourteen.
fn a_run_with_a_reversed_zero(db: &LedgerDb, reset: i64) {
    polls(db, reset, &[0.04, 0.54, 0.0, 0.54, 0.67]);
    served_after_poll(db, 0, 50);
    served_after_poll(db, 3, 14);
}

#[test]
fn a_zero_the_next_reading_reverses_leaves_the_run_and_the_row_sums_one_rise() {
    let reset = SATURDAY + 2 * HOUR;
    let db = LedgerDb::open_in_memory().unwrap();
    seed_request(&db, "seed", SATURDAY - MINUTE);
    a_run_with_a_reversed_zero(&db, reset);
    let row = the_row(&db, reset);
    assert_eq!(
        (row.first_reading_at, row.last_reading_at, row.requests),
        (SATURDAY, SATURDAY + 40 * MINUTE, 2),
        "{row:?}"
    );
    assert!(
        close(row.observed_rise, 0.63),
        "0.04 to 0.67, the climb back from the zero counted no second time: {}",
        row.observed_rise
    );
    assert!(close(row.predicted_share, 0.64), "{}", row.predicted_share);
}

#[test]
fn the_fold_holds_no_meter_drift_from_a_zero_the_next_reading_reverses() {
    let reset = SATURDAY + 2 * HOUR;
    let now = reset + HOUR;
    let db = LedgerDb::open_in_memory().unwrap();
    seed_request(&db, "seed", SATURDAY - MINUTE);
    a_run_with_a_reversed_zero(&db, reset);
    fold(&db, now);
    let tables = subscription_costs();
    db.write(|c| {
        let tx = c.transaction()?;
        fold_routing_conditions(&tx, &tables, now)?;
        tx.commit()?;
        Ok(())
    })
    .unwrap();
    let held = db.read_blocking(routing_conditions).unwrap();
    assert_eq!(
        held.iter()
            .filter(|r| r.kind.as_str() == "meter_drift")
            .map(|r| r.subject_id.as_str())
            .collect::<Vec<_>>(),
        Vec::<&str>::new(),
        "the window's one rise reads inside the band against its priced share: {held:?}"
    );
}

#[test]
fn a_dip_of_three_low_readings_reversed_past_the_prediction_leaves_the_run() {
    let reset = SATURDAY + 2 * HOUR;
    let db = LedgerDb::open_in_memory().unwrap();
    seed_request(&db, "seed", SATURDAY - MINUTE);
    // Three failed polls read zero while two Sonnet units complete, and the next reading climbs 0.42 from the first.
    polls(&db, reset, &[0.10, 0.40, 0.0, 0.0, 0.0, 0.42, 0.50]);
    served_after_poll(&db, 0, 30);
    served_after_poll(&db, 3, 2);
    served_after_poll(&db, 5, 8);
    let row = the_row(&db, reset);
    assert_eq!(
        (row.first_reading_at, row.last_reading_at, row.requests),
        (SATURDAY, SATURDAY + 60 * MINUTE, 3),
        "{row:?}"
    );
    assert!(
        close(row.observed_rise, 0.40),
        "0.10 to 0.50 with the three zeros gone: {}",
        row.observed_rise
    );
    assert!(close(row.predicted_share, 0.40), "{}", row.predicted_share);
}

#[test]
fn a_dip_of_several_failed_polls_across_parts_leaves_the_run_and_the_row_sums_one_rise() {
    // The failed polls fall twice before a reading climbs back to 0.54, so they span parts, and the one Sonnet unit
    // completed inside the dip predicts far less than its climb.
    let sequences: [&[f64]; 2] = [
        &[0.04, 0.54, 0.10, 0.20, 0.05, 0.54, 0.67],
        &[0.04, 0.54, 0.20, 0.10, 0.54, 0.67],
    ];
    let read: Vec<_> = sequences
        .iter()
        .map(|utilizations| {
            let reset = SATURDAY + 2 * HOUR;
            let last = utilizations.len() as i64 - 1;
            let db = LedgerDb::open_in_memory().unwrap();
            seed_request(&db, "seed", SATURDAY - MINUTE);
            polls(&db, reset, utilizations);
            served_after_poll(&db, 0, 50);
            served_after_poll(&db, 2, 1);
            served_after_poll(&db, last - 1, 13);
            let row = the_row(&db, reset);
            (
                row.last_reading_at,
                row.requests,
                row.observed_rise,
                close(row.predicted_share, 0.64),
            )
        })
        .collect();
    assert_eq!(
        read,
        [
            (SATURDAY + 60 * MINUTE, 3, 0.63, true),
            (SATURDAY + 50 * MINUTE, 3, 0.63, true)
        ],
        "each sequence reads 0.04 to 0.67 once over the three attempts' 0.64: {sequences:?}"
    );
}

#[test]
fn a_dip_whose_climb_the_attempts_between_its_ends_predict_keeps_its_split() {
    let reset = SATURDAY + 2 * HOUR;
    let db = LedgerDb::open_in_memory().unwrap();
    seed_request(&db, "seed", SATURDAY - MINUTE);
    // A grant empties the window at 0.60, and seventy Sonnet units refill it past that level to 0.70.
    polls(&db, reset, &[0.05, 0.60, 0.0, 0.30, 0.70]);
    served_after_poll(&db, 0, 55);
    served_after_poll(&db, 2, 30);
    served_after_poll(&db, 3, 40);
    let row = the_row(&db, reset);
    assert!(
        close(row.observed_rise, 1.25),
        "0.55 before the grant and 0.70 after it: {}",
        row.observed_rise
    );
    assert!(close(row.predicted_share, 1.25), "{}", row.predicted_share);
}

#[test]
fn a_dip_inside_one_the_attempts_predict_is_judged_on_its_own_and_leaves_the_run() {
    let reset = SATURDAY + 2 * HOUR;
    let db = LedgerDb::open_in_memory().unwrap();
    seed_request(&db, "seed", SATURDAY - MINUTE);
    // A grant empties the window at 0.60, and seventy Sonnet units refill it past that level to 0.70. Inside that dip a
    // failed poll reads 0.10 after 0.30, and the forty units completed after it predict far less than its 0.60 climb.
    polls(&db, reset, &[0.05, 0.60, 0.0, 0.30, 0.10, 0.70]);
    served_after_poll(&db, 0, 55);
    served_after_poll(&db, 2, 30);
    served_after_poll(&db, 4, 40);
    let row = the_row(&db, reset);
    assert!(
        close(row.observed_rise, 1.25),
        "0.55 before the grant and 0.70 after it, the failed poll's climb counted no second time: {}",
        row.observed_rise
    );
    assert!(close(row.predicted_share, 1.25), "{}", row.predicted_share);
}

#[test]
fn a_dip_whose_attempts_the_tables_cannot_price_keeps_its_split() {
    let reset = SATURDAY + 2 * HOUR;
    let db = LedgerDb::open_in_memory().unwrap();
    seed_request(&db, "seed", SATURDAY - MINUTE);
    // A grant drops the window from 0.50 to 0.20 and the next poll reads 0.10, so the dip spans two parts. Between them
    // an attempt completes on a model the tables price to no share, so nothing judges the dip, and the sum of the parts
    // leaves that attempt out.
    polls(&db, reset, &[0.10, 0.50, 0.20, 0.10, 0.60]);
    served_after_poll(&db, 0, 40);
    served(&db, "unpriced", MYSTERY, SATURDAY + 25 * MINUTE, 20, None);
    served_after_poll(&db, 3, 50);
    let row = the_row(&db, reset);
    assert!(
        close(row.observed_rise, 0.90),
        "0.40 before the grant and 0.50 from the poll after it: {}",
        row.observed_rise
    );
    assert!(close(row.predicted_share, 0.90), "{}", row.predicted_share);
}

#[test]
fn a_dip_that_never_closes_keeps_its_split() {
    let reset = SATURDAY + 2 * HOUR;
    let db = LedgerDb::open_in_memory().unwrap();
    seed_request(&db, "seed", SATURDAY - MINUTE);
    // A free reset reads the window at zero and the readings stay low to the run's end.
    polls(&db, reset, &[0.20, 0.70, 0.0, 0.0, 0.0, 0.10]);
    served_after_poll(&db, 0, 50);
    served_after_poll(&db, 4, 10);
    let row = the_row(&db, reset);
    assert!(
        close(row.observed_rise, 0.60),
        "0.50 before the reset and 0.10 after it: {}",
        row.observed_rise
    );
    assert!(close(row.predicted_share, 0.60), "{}", row.predicted_share);
}

#[test]
fn a_fall_inside_a_dip_that_never_closes_is_judged_on_its_own() {
    let reset = SATURDAY + 2 * HOUR;
    let db = LedgerDb::open_in_memory().unwrap();
    seed_request(&db, "seed", SATURDAY - MINUTE);
    // A grant empties the window at 0.89 and the readings never climb back to it. Inside that dip a failed poll reads
    // zero between two readings at 0.10, with no attempt between the zero and the next reading.
    polls(&db, reset, &[0.05, 0.89, 0.0, 0.10, 0.0, 0.10, 0.20]);
    served_after_poll(&db, 0, 84);
    served_after_poll(&db, 2, 10);
    served_after_poll(&db, 5, 10);
    let row = the_row(&db, reset);
    assert!(
        close(row.observed_rise, 1.04),
        "0.84 before the grant and 0.20 after it, the failed poll's climb counted no second time: {}",
        row.observed_rise
    );
    assert!(close(row.predicted_share, 1.04), "{}", row.predicted_share);
}

#[test]
fn a_dip_whose_climb_stays_under_the_least_rise_keeps_its_split() {
    let reset = SATURDAY + 2 * HOUR;
    let db = LedgerDb::open_in_memory().unwrap();
    seed_request(&db, "seed", SATURDAY - MINUTE);
    // One poll reads a point under the one before it, with no attempt between it and the next.
    polls(&db, reset, &[0.10, 0.50, 0.49, 0.50, 0.70]);
    served_after_poll(&db, 0, 40);
    served_after_poll(&db, 3, 20);
    let row = the_row(&db, reset);
    assert!(
        close(row.observed_rise, 0.61),
        "0.40 before the point and 0.21 from it, a climb of one point being a meter's quantum: {}",
        row.observed_rise
    );
    assert!(close(row.predicted_share, 0.60), "{}", row.predicted_share);
}

#[test]
fn a_failed_polls_zero_among_heads_is_a_dip_the_walk_judges_whatever_its_source() {
    let reset = SATURDAY + 2 * HOUR;
    let db = LedgerDb::open_in_memory().unwrap();
    seed_request(&db, "seed", SATURDAY - MINUTE);
    // A failed poll reads zero between heads at 0.30 and 0.32, and the two Sonnet units completed between the zero and
    // the next head predict far less than the climb back.
    db.insert_samples(
        &[
            head_reading(FIVE_HOUR, SATURDAY, 0.10, reset),
            head_reading(FIVE_HOUR, SATURDAY + 10 * MINUTE, 0.30, reset),
            reading(FIVE_HOUR, SATURDAY + 20 * MINUTE, 0.0, reset),
            head_reading(FIVE_HOUR, SATURDAY + 30 * MINUTE, 0.32, reset),
            head_reading(FIVE_HOUR, SATURDAY + 40 * MINUTE, 0.40, reset),
        ],
        None,
    )
    .unwrap();
    served(&db, "r1", SONNET, SATURDAY + 5 * MINUTE, 20, None);
    served(&db, "r2", SONNET, SATURDAY + 25 * MINUTE, 2, None);
    served(&db, "r3", SONNET, SATURDAY + 35 * MINUTE, 8, None);
    let row = the_row(&db, reset);
    assert_eq!(
        (row.first_reading_at, row.last_reading_at, row.requests),
        (SATURDAY, SATURDAY + 40 * MINUTE, 3),
        "{row:?}"
    );
    assert!(
        close(row.observed_rise, 0.30),
        "0.10 to 0.40 with the zero gone, its climb back counted no second time: {}",
        row.observed_rise
    );
}

/// A run its own reset closed reads as one that slid only where its key lies past the closing reading's instant by
/// more than the reset tolerance: the key rounds a fractional reset up by up to half a second, and a router clock that
/// lags moves the closing instant earlier, while a live slide moves the reset by whole 10-minute steps.
#[test]
fn a_run_closed_inside_the_reset_tolerance_before_its_key_writes_its_row_and_one_closed_earlier_slid(
) {
    let reset = SATURDAY + HOUR;
    for (early, written) in [(MINUTE, true), (MINUTE + 1_000, false)] {
        let db = LedgerDb::open_in_memory().unwrap();
        seed_request(&db, "seed", SATURDAY - MINUTE);
        db.insert_samples(
            &[
                reading(FIVE_HOUR, SATURDAY, 0.10, reset),
                reading(FIVE_HOUR, SATURDAY + 20 * MINUTE, 0.30, reset),
                reading(FIVE_HOUR, reset - early, 0.0, reset + 5 * HOUR),
            ],
            None,
        )
        .unwrap();
        served(&db, "r1", SONNET, SATURDAY + 10 * MINUTE, 20, None);
        let now = reset + HOUR;
        fold(&db, now);
        let expected: &[i64] = if written { &[reset] } else { &[] };
        assert_eq!(
            rows(&db, now)
                .iter()
                .map(|r| r.window_end)
                .collect::<Vec<_>>(),
            expected,
            "the next reset read {early} ms before the run's own"
        );
    }
}

#[test]
fn a_run_a_later_reset_closed_while_its_own_lay_ahead_writes_no_row() {
    let db = LedgerDb::open_in_memory().unwrap();
    seed_request(&db, "seed", SATURDAY - MINUTE);
    // The 5-hour reset slides ten minutes with each pair of polls, so each pair's run closes hours before its own
    // reset, until the last pair's reset passes.
    let slides = 4;
    let reset_of = |slide: i64| SATURDAY + 5 * HOUR + slide * 10 * MINUTE;
    for slide in 0..slides {
        let at = SATURDAY + slide * 10 * MINUTE;
        db.insert_samples(
            &[
                reading(FIVE_HOUR, at, 0.10, reset_of(slide)),
                reading(FIVE_HOUR, at + 5 * MINUTE, 0.20, reset_of(slide)),
            ],
            None,
        )
        .unwrap();
        served(&db, &format!("r{slide}"), SONNET, at + 2 * MINUTE, 10, None);
    }
    let last = reset_of(slides - 1);
    let now = last + HOUR;
    fold(&db, now);
    let rows = rows(&db, now);
    assert_eq!(
        rows.iter().map(|r| r.window_end).collect::<Vec<_>>(),
        [last],
        "a slid reset ended no window, and the one row is the window that passed: {rows:?}"
    );
}

#[test]
fn a_drift_holds_only_where_the_rise_and_the_priced_share_both_reach_the_least_rise() {
    let now = SATURDAY + DAY;
    let db = LedgerDb::open_in_memory().unwrap();
    // One ratio outside the band, 0.05 over 0.0381, on a light window and on a window priced at 0.06.
    let ratio = 0.05 / 0.0381;
    db.write(|c| {
        for (account, rise, predicted) in [("light", 0.05, 0.0381), ("priced", 0.06 * ratio, 0.06)] {
            c.execute(
                "INSERT INTO window_reconciliations (account_id, window, window_end, first_reading_at, \
                 last_reading_at, observed_rise, predicted_share, ratio, requests, computed_at) \
                 VALUES (?1, 'five_hour', ?2, ?2 - 10800000, ?2 - 3600000, ?3, ?4, ?5, 1, ?2)",
                rusqlite::params![account, now - HOUR, rise, predicted, ratio],
            )?;
        }
        Ok(())
    })
    .unwrap();
    let tables = subscription_costs();
    db.write(|c| {
        let tx = c.transaction()?;
        fold_routing_conditions(&tx, &tables, now)?;
        tx.commit()?;
        Ok(())
    })
    .unwrap();
    let held = db.read_blocking(routing_conditions).unwrap();
    assert_eq!(
        held.iter()
            .filter(|r| r.kind.as_str() == "meter_drift")
            .map(|r| r.subject_id.as_str())
            .collect::<Vec<_>>(),
        ["priced"],
        "a light window's one reading step reads far off its priced share, so it holds nothing: {held:?}"
    );
}

#[test]
fn a_quiet_stretch_of_polls_and_a_busy_stretch_of_heads_reconcile_from_the_first_poll_to_the_last_head(
) {
    let reset = SATURDAY + HOUR;
    let db = LedgerDb::open_in_memory().unwrap();
    seed_request(&db, "seed", SATURDAY - MINUTE);
    db.insert_samples(
        &[
            reading(FIVE_HOUR, SATURDAY, 0.10, reset),
            reading(FIVE_HOUR, SATURDAY + 10 * MINUTE, 0.12, reset),
            head_reading(FIVE_HOUR, SATURDAY + 20 * MINUTE, 0.30, reset),
            head_reading(FIVE_HOUR, SATURDAY + 30 * MINUTE, 0.45, reset),
        ],
        None,
    )
    .unwrap();
    // 0.02 of the window between the two polls, then 0.18 and 0.15 in the busy stretch the heads alone read.
    served(&db, "quiet", SONNET, SATURDAY + 5 * MINUTE, 2, None);
    served(&db, "busy-1", SONNET, SATURDAY + 15 * MINUTE, 18, None);
    served(&db, "busy-2", SONNET, SATURDAY + 25 * MINUTE, 15, None);
    let row = the_row(&db, reset);
    assert_eq!(
        (row.first_reading_at, row.last_reading_at, row.requests),
        (SATURDAY, SATURDAY + 30 * MINUTE, 3),
        "the first poll's and the last head's instants with every attempt between them: {row:?}"
    );
    assert!(
        close(row.observed_rise, 0.35),
        "0.10 to 0.45, the busy stretch's climb inside the rise: {}",
        row.observed_rise
    );
}

#[test]
fn a_run_of_heads_alone_reconciles_as_a_run_of_polls_does() {
    let reset = SATURDAY + HOUR;
    let db = LedgerDb::open_in_memory().unwrap();
    seed_request(&db, "seed", SATURDAY - MINUTE);
    db.insert_samples(
        &[
            head_reading(FIVE_HOUR, SATURDAY, 0.10, reset),
            head_reading(FIVE_HOUR, SATURDAY + 20 * MINUTE, 0.30, reset),
        ],
        None,
    )
    .unwrap();
    served(&db, "r1", SONNET, SATURDAY + 5 * MINUTE, 20, None);
    let now = reset + HOUR;
    fold(&db, now);
    let rows = rows(&db, now);
    assert_eq!(
        rows.iter()
            .map(|r| (r.first_reading_at, r.last_reading_at, r.requests))
            .collect::<Vec<_>>(),
        [(SATURDAY, SATURDAY + 20 * MINUTE, 1)],
        "{rows:?}"
    );
    assert!(
        close(rows[0].observed_rise, 0.20),
        "{}",
        rows[0].observed_rise
    );
    assert!(close(rows[0].ratio, 1.0), "{}", rows[0].ratio);
}

#[test]
fn a_run_of_one_head_and_one_poll_reconciles_between_the_two() {
    let reset = SATURDAY + HOUR;
    let db = LedgerDb::open_in_memory().unwrap();
    seed_request(&db, "seed", SATURDAY - MINUTE);
    db.insert_samples(
        &[
            head_reading(FIVE_HOUR, SATURDAY, 0.10, reset),
            reading(FIVE_HOUR, SATURDAY + 20 * MINUTE, 0.30, reset),
        ],
        None,
    )
    .unwrap();
    served(&db, "r1", SONNET, SATURDAY + 5 * MINUTE, 20, None);
    let now = reset + HOUR;
    fold(&db, now);
    let rows = rows(&db, now);
    assert_eq!(
        rows.iter()
            .map(|r| (r.first_reading_at, r.last_reading_at, r.requests))
            .collect::<Vec<_>>(),
        [(SATURDAY, SATURDAY + 20 * MINUTE, 1)],
        "a head and a poll bound the run as two readings of one source do: {rows:?}"
    );
    assert!(
        close(rows[0].observed_rise, 0.20),
        "the head to the poll: {}",
        rows[0].observed_rise
    );
}

#[test]
fn a_head_one_percent_under_the_poll_before_it_joins_the_run_and_two_percent_under_opens_a_part() {
    let reset = SATURDAY + HOUR;
    let now = reset + HOUR;
    // 0.40 of the window between the first two polls and 0.10 after the head.
    let fold_with = |head: f64| {
        let db = LedgerDb::open_in_memory().unwrap();
        seed_request(&db, "seed", SATURDAY - MINUTE);
        db.insert_samples(
            &[
                reading(FIVE_HOUR, SATURDAY, 0.10, reset),
                reading(FIVE_HOUR, SATURDAY + 10 * MINUTE, 0.50, reset),
                head_reading(FIVE_HOUR, SATURDAY + 20 * MINUTE, head, reset),
                reading(FIVE_HOUR, SATURDAY + 30 * MINUTE, 0.60, reset),
            ],
            None,
        )
        .unwrap();
        served(&db, "first", SONNET, SATURDAY + 5 * MINUTE, 40, None);
        served(&db, "second", SONNET, SATURDAY + 25 * MINUTE, 10, None);
        fold(&db, now);
        rows(&db, now)
            .iter()
            .map(|r| (r.last_reading_at, r.requests, r.observed_rise))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        fold_with(0.49),
        [(SATURDAY + 30 * MINUTE, 2, 0.5)],
        "the head truncates what the poll rounded, so one percent under it is the same draw"
    );
    assert_eq!(
        fold_with(0.48),
        [(SATURDAY + 30 * MINUTE, 2, 0.52)],
        "two percent under is a fall, and the part the head opens holds a head and a poll, so it adds its rise"
    );
}

#[test]
fn a_head_one_percent_under_the_poll_before_it_opens_no_dip() {
    let reset = SATURDAY + 2 * HOUR;
    let db = LedgerDb::open_in_memory().unwrap();
    seed_request(&db, "seed", SATURDAY - MINUTE);
    // The head reads a point under the poll before it, inside the sources' gap, then falls to 0.10, a dip the 39 Sonnet
    // units completed inside it refill. A dip opened at the head would close only at the last poll, on a climb of 0.49
    // those units cannot explain, and take the three heads with it.
    db.insert_samples(
        &[
            reading(FIVE_HOUR, SATURDAY, 0.50, reset),
            head_reading(FIVE_HOUR, SATURDAY + 10 * MINUTE, 0.49, reset),
            head_reading(FIVE_HOUR, SATURDAY + 20 * MINUTE, 0.10, reset),
            head_reading(FIVE_HOUR, SATURDAY + 30 * MINUTE, 0.49, reset),
            reading(FIVE_HOUR, SATURDAY + 40 * MINUTE, 0.98, reset),
        ],
        None,
    )
    .unwrap();
    served(&db, "refill", SONNET, SATURDAY + 25 * MINUTE, 39, None);
    let row = the_row(&db, reset);
    assert_eq!(
        (row.first_reading_at, row.last_reading_at, row.requests),
        (SATURDAY + 20 * MINUTE, SATURDAY + 40 * MINUTE, 1),
        "the run's first part holds no attempt, so the row is the part the fall to 0.10 opens: {row:?}"
    );
    assert!(
        close(row.observed_rise, 0.88),
        "0.10 to 0.98, the three heads kept: {}",
        row.observed_rise
    );
    assert!(close(row.predicted_share, 0.39), "{}", row.predicted_share);
}

#[test]
fn a_run_whose_first_reading_stands_within_the_longest_attempt_after_the_requests_cutoff_is_skipped(
) {
    let five_reset = SATURDAY + HOUR;
    let seven_reset = SATURDAY + 2 * HOUR;
    let now = seven_reset + HOUR;
    let windows_under = |request_cutoff: i64| {
        let db = LedgerDb::open_in_memory().unwrap();
        // The oldest request held is younger than the 5-hour run's first reading with nothing purged between them, as
        // on a fresh ledger.
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
        fold_under(&db, Some(request_cutoff), now);
        rows(&db, now)
            .into_iter()
            .map(|r| r.window)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        windows_under(SATURDAY - 10 * MINUTE - HOUR),
        [SEVEN_DAY, FIVE_HOUR],
        "the purge keeps the requests received from the cutoff on, so a run whose first reading stands a whole \
         hour after the cutoff writes its row"
    );
    assert_eq!(
        windows_under(SATURDAY - 10 * MINUTE - HOUR + 1),
        [SEVEN_DAY],
        "the 5-hour run began less than an hour after the cutoff, so its rise would meet part of what the router sent"
    );
}

/// A weekly reading taken as its week opened names a reset a week out, so where the floor sits an hour under that
/// reset the reading's bucket lies almost a week before the floor, and the read still takes it.
#[test]
fn a_weekly_reading_almost_a_week_before_the_floor_still_opens_its_run() {
    let reset = SATURDAY + 7 * DAY;
    let opened = SATURDAY + MINUTE;
    let db = LedgerDb::open_in_memory().unwrap();
    seed_request(&db, "seed", SATURDAY);
    db.insert_samples(
        &[
            reading(SEVEN_DAY, opened, 0.10, reset),
            reading(SEVEN_DAY, reset - HOUR, 0.20, reset),
        ],
        None,
    )
    .unwrap();
    // A hundred Sonnet units, 0.1 of the week.
    served(&db, "r1", SONNET, opened + HOUR, 100, None);
    // The 30-day cutoff lies an hour before the reset, and the floor a minute under it.
    let now = reset - HOUR + 30 * DAY;
    fold(&db, now);
    let rows = rows(&db, now);
    assert_eq!(
        rows.iter()
            .map(|r| (
                r.window.as_str(),
                r.window_end,
                r.first_reading_at,
                r.last_reading_at,
                r.requests
            ))
            .collect::<Vec<_>>(),
        [(SEVEN_DAY, reset, opened, reset - HOUR, 1)],
        "the run opens at the week's first reading: {rows:?}"
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
    // Twenty Sonnet units each: 0.1 of the window under `desk`, which sizes Ultra at 200, and 0.2 at the base.
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
                 last_reading_at, observed_rise, predicted_share, ratio, requests, computed_at) \
                 VALUES ('a', 'five_hour', ?1, ?1, ?1, 0.2, 0.2, 1.0, 1, ?1)",
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
fn the_drift_reads_the_windows_that_ended_inside_eight_days_while_the_route_keeps_thirty() {
    let now = SATURDAY + 40 * DAY;
    let db = LedgerDb::open_in_memory().unwrap();
    // Each account's one window charged twice the table: `fresh`'s ended a millisecond inside the eight days, `edge`'s
    // at their bound and `stale`'s twenty days before the fold, as an account that stopped serving leaves it.
    db.write(|c| {
        for (account, window_end) in [
            ("edge", now - 8 * DAY),
            ("fresh", now - 8 * DAY + 1),
            ("stale", now - 20 * DAY),
        ] {
            c.execute(
                "INSERT INTO window_reconciliations (account_id, window, window_end, first_reading_at, \
                 last_reading_at, observed_rise, predicted_share, ratio, requests, computed_at) \
                 VALUES (?1, 'five_hour', ?2, ?2 - 10800000, ?2 - 3600000, 0.5, 0.25, 2.0, 3, ?2)",
                rusqlite::params![account, window_end],
            )?;
        }
        Ok(())
    })
    .unwrap();
    let newest = db.read_blocking(|c| newest_rows(c, now)).unwrap();
    assert_eq!(
        newest.keys().map(String::as_str).collect::<Vec<_>>(),
        ["fresh"],
        "a window that ended eight days or more before the fold is not read, so its account is absent: {newest:?}"
    );
    assert_eq!(
        rows(&db, now)
            .iter()
            .map(|r| r.account_id.as_str())
            .collect::<Vec<_>>(),
        ["fresh", "edge", "stale"],
        "the route keeps every row of the trailing 30 days with its ratio"
    );
    let tables = subscription_costs();
    db.write(|c| {
        let tx = c.transaction()?;
        fold_routing_conditions(&tx, &tables, now)?;
        tx.commit()?;
        Ok(())
    })
    .unwrap();
    let held = db.read_blocking(routing_conditions).unwrap();
    assert_eq!(
        held.iter()
            .filter(|r| r.kind.as_str() == "meter_drift")
            .map(|r| r.subject_id.as_str())
            .collect::<Vec<_>>(),
        ["fresh"],
        "a row outside the eight days holds nothing: {held:?}"
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
fn a_plan_attempt_is_counted_and_an_overage_attempt_draws_nothing() {
    let reset = SATURDAY + HOUR;
    let db = LedgerDb::open_in_memory().unwrap();
    seed_request(&db, "seed", SATURDAY - MINUTE);
    db.insert_samples(
        &[
            reading(FIVE_HOUR, SATURDAY, 0.10, reset),
            reading(FIVE_HOUR, SATURDAY + 20 * MINUTE, 0.20, reset),
        ],
        None,
    )
    .unwrap();
    // 0.1 of the window from ten Sonnet units, and nothing from thirty Sonnet units the head billed in overage.
    served(&db, "sonnet", SONNET, SATURDAY + 5 * MINUTE, 10, None);
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
        rows.iter().map(|r| r.requests).collect::<Vec<_>>(),
        [1],
        "the plan-funded attempt alone: {rows:?}"
    );
    assert!(
        close(rows[0].predicted_share, 0.1),
        "the overage answer adds nothing: {}",
        rows[0].predicted_share
    );
    assert!(close(rows[0].ratio, 1.0), "{}", rows[0].ratio);
}

#[test]
fn a_classifier_attempt_beside_a_main_turn_predicts_the_main_turns_share_alone() {
    let reset = SATURDAY + HOUR;
    let db = LedgerDb::open_in_memory().unwrap();
    seed_request(&db, "seed", SATURDAY - MINUTE);
    db.insert_samples(
        &[
            reading(FIVE_HOUR, SATURDAY, 0.10, reset),
            reading(FIVE_HOUR, SATURDAY + 20 * MINUTE, 0.20, reset),
        ],
        None,
    )
    .unwrap();
    // 0.1 of the window from ten Sonnet units, and nothing from the thirty units of the classifier call beside it.
    served(&db, "main", SONNET, SATURDAY + 5 * MINUTE, 10, None);
    served_as_classifier(&db, "classifier", SONNET, SATURDAY + 10 * MINUTE, 30);
    let now = reset + HOUR;
    fold(&db, now);
    let rows = rows(&db, now);
    assert_eq!(
        rows.iter().map(|r| r.requests).collect::<Vec<_>>(),
        [1],
        "the main turn alone is counted: {rows:?}"
    );
    assert!(
        close(rows[0].predicted_share, 0.1),
        "the main turn's share alone: {}",
        rows[0].predicted_share
    );
    assert!(close(rows[0].ratio, 1.0), "{}", rows[0].ratio);
}

#[test]
fn a_run_with_a_plan_attempt_the_tables_cannot_price_is_skipped_unless_it_carries_no_token() {
    let reset = SATURDAY + HOUR;
    let now = reset + HOUR;
    // 0.1 of the window from ten Sonnet units, beside an attempt on a model of a tier with no weighted model, which
    // the tables price to no share whatever its tokens.
    let fold_with = |mystery_input: u64| {
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
        served(&db, "sonnet", SONNET, SATURDAY + 5 * MINUTE, 10, None);
        served(
            &db,
            "mystery",
            MYSTERY,
            SATURDAY + 10 * MINUTE,
            mystery_input,
            None,
        );
        fold(&db, now);
        rows(&db, now)
    };
    assert_eq!(
        fold_with(20),
        [],
        "the rise carries the unpriced attempt's draw, which a sum of the priced one alone would read as drift"
    );
    assert_eq!(
        fold_with(0)
            .iter()
            .map(|r| (r.window.as_str(), r.requests))
            .collect::<Vec<_>>(),
        [(FIVE_HOUR, 1)],
        "an attempt without a token drew nothing, so the run prices the rest"
    );
}
