//! Each completed 5-hour and 7-day window of a subscription account, reconciled: the rise its readings showed
//! against the share the router's own requests completed between those readings were priced at.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use phoebis_bus::Bus;
use phoebis_ledger::{
    bucket_of, read_reconciliations, reconcile, run_retention, spawn, AttemptRecord, Ledger,
    LedgerConfig, LedgerDb, Metrics, PlanTables, PriceTable, Pricing, QuotaSampleRecord,
    ReconciliationRow, RequestRecord, Tiers, POLL_INTERVAL, RECONCILE_INTERVAL,
};
use phoebis_oauth::Clock;
use phoebis_routing::{PinUsage, RouterConfig, FIVE_HOUR, SEVEN_DAY};
use phoebis_store::{AccountKind, AccountRow, AccountState, StorageKey, Store, SubscriptionRow};
use fake_upstream::{capture_tracing, wait_for_async};

const MINUTE: i64 = 60_000;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;
/// Saturday 2026-09-26 at 12:00 UTC: the peak band keeps to weekdays, so every charge here is off it.
const SATURDAY: i64 = 1_790_424_000_000;
const MAX_5X: &str = "default_claude_max_5x";
const OPUS: &str = "claude-opus-5";
const MILLION: u64 = 1_000_000;

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * b.abs().max(1.0)
}

fn kinds() -> HashMap<String, AccountKind> {
    HashMap::from([("a".to_string(), AccountKind::Oauth)])
}

fn tiers(tier: Option<&str>) -> Tiers {
    HashMap::from([("a".to_string(), tier.map(String::from))])
}

/// The Max 5x share of `window` a million plain Opus input tokens read, off the band.
fn opus_share(window: &str) -> f64 {
    let usage = PinUsage {
        input: MILLION,
        ..PinUsage::default()
    };
    PlanTables::shipped()
        .share(window, MAX_5X, OPUS, usage)
        .unwrap()
        .share
}

/// A request with no attempt, received at `at`: what tells the reconciliation the ledger's requests reach back
/// that far.
fn seed_request(db: &LedgerDb, id: &str, at: i64) {
    db.insert_request(
        &RequestRecord {
            id: id.into(),
            received_at: at,
            finished_at: Some(at),
            model: OPUS.into(),
            canonical_model: OPUS.into(),
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

/// One request on account `a` whose single attempt completed at `completed` with `input` plain input tokens of
/// Opus.
fn opus_attempt(db: &LedgerDb, id: &str, completed: i64, input: u64) {
    attempt_on(db, "a", id, OPUS, completed, input);
}

/// One request on `account` whose single attempt completed at `completed` with `input` plain input tokens of
/// `model`.
fn attempt_on(db: &LedgerDb, account: &str, id: &str, model: &str, completed: i64, input: u64) {
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
            ..Default::default()
        },
        &[AttemptRecord {
            request_id: id.into(),
            attempt: 1,
            account_id: account.into(),
            account_kind: "oauth".into(),
            started_at: started,
            finished_at: Some(completed),
            outcome: "succeeded".into(),
            input_tokens: Some(input),
            output_tokens: Some(0),
            ..Default::default()
        }],
        None,
    )
    .unwrap();
}

/// A reading of account `a` observed at `at`, under the key its source spells the window with.
fn reading(key: &str, source: &str, at: i64, utilization: f64, reset_at: i64) -> QuotaSampleRecord {
    reading_of("a", key, source, at, utilization, reset_at)
}

fn reading_of(
    account: &str,
    key: &str,
    source: &str,
    at: i64,
    utilization: f64,
    reset_at: i64,
) -> QuotaSampleRecord {
    QuotaSampleRecord {
        account_id: account.into(),
        bucket: bucket_of(at),
        window: key.into(),
        utilization: Some(utilization),
        reset_at: Some(reset_at),
        source: source.into(),
        observed_at: Some(at),
    }
}

fn head(key: &str, at: i64, utilization: f64, reset_at: i64) -> QuotaSampleRecord {
    reading(key, "header", at, utilization, reset_at)
}

fn run(db: &LedgerDb, tier: Option<&str>, now: i64) -> Vec<ReconciliationRow> {
    let tables = PlanTables::shipped();
    db.read_blocking(|conn| reconcile(conn, &tables, &tiers(tier), &kinds(), now))
        .unwrap()
}

/// The reset the steady-rise seed's readings name, and an instant by which its window has completed.
const STEADY_RESET: i64 = SATURDAY + 4 * HOUR;
const STEADY_DONE: i64 = STEADY_RESET + 6 * MINUTE;

/// Readings at 0.10, 0.20 and 0.30 in three buckets naming one reset, on the 5-hour window and, when asked, on
/// the 7-day one; two Opus attempts of a million input tokens each complete between the first and the last.
fn steady_rise(db: &LedgerDb, seven_day: bool) {
    steady_rise_of(db, "a", seven_day);
}

/// `steady_rise` on `account`, its request ids prefixed with it.
fn steady_rise_of(db: &LedgerDb, account: &str, seven_day: bool) {
    seed_request(db, &format!("{account}-seed"), SATURDAY - MINUTE);
    let mut readings = Vec::new();
    for (n, utilization) in [0.10, 0.20, 0.30].into_iter().enumerate() {
        let at = SATURDAY + n as i64 * 10 * MINUTE;
        readings.push(reading_of(
            account,
            "5h",
            "header",
            at,
            utilization,
            STEADY_RESET,
        ));
        if seven_day {
            readings.push(reading_of(
                account,
                "7d",
                "header",
                at,
                utilization / 10.0,
                STEADY_RESET,
            ));
        }
    }
    db.insert_samples(&readings, None).unwrap();
    for (n, completed) in [5 * MINUTE, 15 * MINUTE].into_iter().enumerate() {
        attempt_on(
            db,
            account,
            &format!("{account}-r{n}"),
            OPUS,
            SATURDAY + completed,
            MILLION,
        );
    }
}

#[test]
fn a_completed_window_with_a_steady_rise_and_known_router_usage_reads_its_ratio() {
    assert!(
        STEADY_DONE - STEADY_RESET > POLL_INTERVAL.as_millis() as i64,
        "the window closed more than a poll interval ago"
    );
    let db = LedgerDb::open_in_memory().unwrap();
    steady_rise(&db, true);
    let rows = run(&db, Some(MAX_5X), STEADY_DONE);
    assert_eq!(
        rows.iter()
            .map(|r| (r.window.as_str(), r.window_end))
            .collect::<Vec<_>>(),
        [(FIVE_HOUR, STEADY_RESET), (SEVEN_DAY, STEADY_RESET)]
    );
    let five = &rows[0];
    assert_eq!(
        (
            five.account_id.as_str(),
            five.first_reading_at,
            five.last_reading_at,
            five.requests,
            five.unmeasured,
            five.computed_at
        ),
        ("a", SATURDAY, SATURDAY + 20 * MINUTE, 2, false, STEADY_DONE)
    );
    assert!(close(five.observed_rise, 0.20), "{}", five.observed_rise);
    assert!(
        close(five.predicted_share, 0.1682),
        "twice 0.0841, the band's 1.45 left out: {}",
        five.predicted_share
    );
    assert!(
        (five.ratio - 1.19).abs() < 0.005,
        "0.20 over 0.1682: {}",
        five.ratio
    );
    assert!(!five.off(), "1.19 lies inside the band");
    let seven = &rows[1];
    assert!(close(seven.observed_rise, 0.02), "{}", seven.observed_rise);
    assert!(
        close(seven.predicted_share, 2.0 * opus_share(SEVEN_DAY)),
        "{}",
        seven.predicted_share
    );
    assert_eq!(
        (seven.requests, seven.unmeasured),
        (2, true),
        "no 7-day cell has been measured"
    );
}

#[test]
fn a_run_that_crosses_a_reset_is_two_runs_and_a_negative_rise_yields_nothing() {
    let reset = SATURDAY + 15 * MINUTE;
    let next = reset + 5 * HOUR;
    let db = LedgerDb::open_in_memory().unwrap();
    seed_request(&db, "seed", SATURDAY);
    db.insert_samples(
        &[
            head("5h", SATURDAY, 0.60, reset),
            head("5h", SATURDAY + 10 * MINUTE, 0.80, reset),
            head("5h", SATURDAY + 20 * MINUTE, 0.05, next),
            head("5h", SATURDAY + 30 * MINUTE, 0.25, next),
        ],
        None,
    )
    .unwrap();
    opus_attempt(&db, "r1", SATURDAY + 5 * MINUTE, MILLION);
    opus_attempt(&db, "r2", SATURDAY + 25 * MINUTE, MILLION);
    let rows = run(&db, Some(MAX_5X), next + HOUR);
    assert_eq!(
        rows.iter().map(|r| r.window_end).collect::<Vec<_>>(),
        [reset, next],
        "one row per reset the readings named"
    );
    for row in &rows {
        assert!(close(row.observed_rise, 0.20), "{}", row.observed_rise);
        assert!(
            close(row.ratio, 0.20 / opus_share(FIVE_HOUR)),
            "{}",
            row.ratio
        );
        assert_eq!(row.requests, 1);
    }

    let fell = LedgerDb::open_in_memory().unwrap();
    seed_request(&fell, "seed", SATURDAY);
    fell.insert_samples(
        &[
            head("5h", SATURDAY, 0.80, reset),
            head("5h", SATURDAY + 10 * MINUTE, 0.10, reset),
        ],
        None,
    )
    .unwrap();
    opus_attempt(&fell, "r1", SATURDAY + 5 * MINUTE, MILLION);
    assert_eq!(
        run(&fell, Some(MAX_5X), next + HOUR),
        [],
        "a fall under one reset is a reset the readings missed"
    );
}

#[tokio::test]
async fn a_window_with_no_router_traffic_yields_no_row_and_a_reconciled_run_is_not_computed_twice()
{
    let quiet = Arc::new(LedgerDb::open_in_memory().unwrap());
    seed_request(&quiet, "seed", SATURDAY);
    quiet
        .insert_samples(
            &[
                head("5h", SATURDAY, 0.10, STEADY_RESET),
                head("5h", SATURDAY + 10 * MINUTE, 0.30, STEADY_RESET),
            ],
            None,
        )
        .unwrap();
    let rows = run(&quiet, Some(MAX_5X), STEADY_DONE);
    assert_eq!(rows, [], "the router sent nothing in the window");
    quiet.insert_reconciliations(rows).await.unwrap();
    assert_eq!(quiet.reconciliations(0).await.unwrap(), []);

    let db = Arc::new(LedgerDb::open_in_memory().unwrap());
    steady_rise(&db, false);
    let rows = run(&db, Some(MAX_5X), STEADY_DONE);
    assert_eq!(rows.len(), 1);
    db.insert_reconciliations(rows).await.unwrap();
    assert_eq!(
        run(&db, Some(MAX_5X), STEADY_DONE + HOUR),
        [],
        "the window already has its row"
    );
    assert_eq!(db.reconciliations(0).await.unwrap().len(), 1);
}

#[test]
fn an_account_without_a_tier_yields_no_row() {
    let db = LedgerDb::open_in_memory().unwrap();
    steady_rise(&db, false);
    assert_eq!(run(&db, None, STEADY_DONE), []);
    assert_eq!(
        run(&db, Some(MAX_5X), STEADY_DONE).len(),
        1,
        "the same seed with the tier set answers its row"
    );
}

#[test]
fn a_burst_inside_each_boundary_bucket_reads_a_ratio_of_one() {
    let start = bucket_of(SATURDAY) * 1_000;
    let charge = opus_share(FIVE_HOUR);
    let reset = start + 4 * HOUR;
    let db = LedgerDb::open_in_memory().unwrap();
    seed_request(&db, "seed", start + 50_000);
    db.insert_samples(
        &[
            head("5h", start + 50_000, 0.10, reset),
            head("5h", start + 350_000, 0.10 + charge, reset),
            head("5h", start + 690_000, 0.10 + 3.0 * charge, reset),
        ],
        None,
    )
    .unwrap();
    // Before the first reading's bucket closes, and inside the last reading's bucket before it was read.
    for (n, completed) in [100_000, 400_000, 650_000].into_iter().enumerate() {
        opus_attempt(&db, &format!("r{n}"), start + completed, MILLION);
    }
    let rows = run(&db, Some(MAX_5X), reset + HOUR);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].requests, 3,
        "both boundary buckets' charges fall between the readings"
    );
    assert!(close(rows[0].observed_rise, 3.0 * charge));
    assert!(close(rows[0].ratio, 1.0), "{}", rows[0].ratio);
}

#[test]
fn readings_whose_resets_differ_only_in_precision_fall_into_one_run() {
    let reset = STEADY_RESET;
    let db = LedgerDb::open_in_memory().unwrap();
    seed_request(&db, "seed", SATURDAY);
    db.insert_samples(
        &[
            reading("five_hour", "poll", SATURDAY, 0.10, reset + 250),
            head("5h", SATURDAY + 10 * MINUTE, 0.20, reset),
        ],
        None,
    )
    .unwrap();
    opus_attempt(&db, "r1", SATURDAY + 5 * MINUTE, MILLION);
    let rows = run(&db, Some(MAX_5X), STEADY_DONE);
    assert_eq!(
        rows.iter()
            .map(|r| (r.window_end, r.first_reading_at, r.last_reading_at))
            .collect::<Vec<_>>(),
        [(reset, SATURDAY, SATURDAY + 10 * MINUTE)],
        "the endpoint's fraction and the head's whole second name one reset"
    );
}

#[test]
fn a_run_older_than_the_retained_requests_is_skipped() {
    let now = SATURDAY + 6 * DAY;
    let db = LedgerDb::open_in_memory().unwrap();
    seed_request(&db, "old", now - 5 * DAY - MINUTE);
    seed_request(&db, "recent", now - DAY - MINUTE);
    let five_reset = now - DAY + 4 * HOUR;
    let seven_reset = now - HOUR;
    db.insert_samples(
        &[
            head("7d", now - 5 * DAY, 0.10, seven_reset),
            head("5h", now - DAY, 0.10, five_reset),
            head("5h", now - DAY + 10 * MINUTE, 0.20, five_reset),
            head("7d", now - DAY + 20 * MINUTE, 0.20, seven_reset),
        ],
        None,
    )
    .unwrap();
    opus_attempt(&db, "r1", now - DAY + 5 * MINUTE, MILLION);
    assert_eq!(
        run(&db, Some(MAX_5X), now)
            .iter()
            .map(|r| r.window.as_str())
            .collect::<Vec<_>>(),
        [FIVE_HOUR, SEVEN_DAY],
        "with every request of the week retained, both runs read"
    );
    let config = LedgerConfig {
        retention_days: 3,
        sample_retention_days: 30,
        ..LedgerConfig::default()
    };
    run_retention(&db, &config, now, &[]).unwrap();
    assert_eq!(
        run(&db, Some(MAX_5X), now)
            .iter()
            .map(|r| r.window.as_str())
            .collect::<Vec<_>>(),
        [FIVE_HOUR],
        "the 7-day run's first reading is five days old, past the three the requests are kept"
    );
}

#[test]
fn a_stale_reading_inside_a_run_closes_nothing_and_joins_nothing() {
    let db = LedgerDb::open_in_memory().unwrap();
    seed_request(&db, "seed", SATURDAY);
    db.insert_samples(
        &[
            head("5h", SATURDAY, 0.10, STEADY_RESET),
            // An answer naming the window before this one, a reset only ever moving forward.
            reading(
                "five_hour",
                "poll",
                SATURDAY + 10 * MINUTE,
                0.95,
                STEADY_RESET - 5 * HOUR,
            ),
            head("5h", SATURDAY + 20 * MINUTE, 0.30, STEADY_RESET),
        ],
        None,
    )
    .unwrap();
    opus_attempt(&db, "r1", SATURDAY + 5 * MINUTE, MILLION);
    opus_attempt(&db, "r2", SATURDAY + 15 * MINUTE, MILLION);
    let rows = run(&db, Some(MAX_5X), STEADY_DONE);
    assert_eq!(
        rows.iter()
            .map(|r| (
                r.window_end,
                r.first_reading_at,
                r.last_reading_at,
                r.requests
            ))
            .collect::<Vec<_>>(),
        [(STEADY_RESET, SATURDAY, SATURDAY + 20 * MINUTE, 2)],
        "the run keeps its first and last reading and both attempts"
    );
    assert!(
        close(rows[0].observed_rise, 0.20),
        "{}",
        rows[0].observed_rise
    );
}

#[test]
fn a_reading_without_a_reset_or_a_utilization_leaves_the_run_as_it_was() {
    let db = LedgerDb::open_in_memory().unwrap();
    steady_rise(&db, false);
    db.insert_samples(
        &[
            // A poll that named no reset, and a head that carried a reset and no utilization.
            QuotaSampleRecord {
                reset_at: None,
                ..reading(
                    "five_hour",
                    "poll",
                    SATURDAY + 15 * MINUTE,
                    0.90,
                    STEADY_RESET,
                )
            },
            QuotaSampleRecord {
                utilization: None,
                ..head("5h", SATURDAY + 5 * MINUTE, 0.0, STEADY_RESET)
            },
        ],
        None,
    )
    .unwrap();
    let rows = run(&db, Some(MAX_5X), STEADY_DONE);
    assert_eq!(
        rows.iter()
            .map(|r| (
                r.window_end,
                r.first_reading_at,
                r.last_reading_at,
                r.requests
            ))
            .collect::<Vec<_>>(),
        [(STEADY_RESET, SATURDAY, SATURDAY + 20 * MINUTE, 2)]
    );
    assert!(
        close(rows[0].observed_rise, 0.20),
        "{}",
        rows[0].observed_rise
    );
}

#[test]
fn readings_of_one_bucket_run_in_the_order_they_were_observed() {
    let start = bucket_of(SATURDAY) * 1_000;
    let db = LedgerDb::open_in_memory().unwrap();
    seed_request(&db, "seed", start);
    // The head lands first but was observed last: its row comes before the poll's in the table.
    db.insert_samples(&[head("5h", start + 200_000, 0.30, STEADY_RESET)], None)
        .unwrap();
    db.insert_samples(
        &[reading(
            "five_hour",
            "poll",
            start + 50_000,
            0.10,
            STEADY_RESET,
        )],
        None,
    )
    .unwrap();
    opus_attempt(&db, "r1", start + 100_000, MILLION);
    opus_attempt(&db, "r2", start + 150_000, MILLION);
    let rows = run(&db, Some(MAX_5X), STEADY_DONE);
    assert_eq!(
        rows.iter()
            .map(|r| (r.first_reading_at, r.last_reading_at, r.requests))
            .collect::<Vec<_>>(),
        [(start + 50_000, start + 200_000, 2)],
        "the poll's reading opens the run"
    );
    assert!(
        close(rows[0].observed_rise, 0.20),
        "{}",
        rows[0].observed_rise
    );
}

#[test]
fn a_window_is_reconciled_once_its_reset_lies_a_poll_interval_behind_now() {
    let db = LedgerDb::open_in_memory().unwrap();
    steady_rise(&db, false);
    let poll = POLL_INTERVAL.as_millis() as i64;
    assert_eq!(
        run(&db, Some(MAX_5X), STEADY_RESET + poll - 1),
        [],
        "a window whose reset is under a poll interval behind may still take a reading"
    );
    assert_eq!(
        run(&db, Some(MAX_5X), STEADY_RESET + poll + 1)
            .iter()
            .map(|r| r.window_end)
            .collect::<Vec<_>>(),
        [STEADY_RESET]
    );
}

#[test]
fn a_run_whose_first_reading_has_no_instant_yields_nothing() {
    let db = LedgerDb::open_in_memory().unwrap();
    seed_request(&db, "seed", SATURDAY - MINUTE);
    db.insert_samples(
        &[
            // A reading stored before the instant was kept, at its bucket's start.
            QuotaSampleRecord {
                observed_at: None,
                ..head("5h", SATURDAY, 0.10, STEADY_RESET)
            },
            head("5h", SATURDAY + 10 * MINUTE, 0.20, STEADY_RESET),
            head("5h", SATURDAY + 20 * MINUTE, 0.30, STEADY_RESET),
        ],
        None,
    )
    .unwrap();
    opus_attempt(&db, "r1", SATURDAY + 5 * MINUTE, MILLION);
    opus_attempt(&db, "r2", SATURDAY + 15 * MINUTE, MILLION);
    assert_eq!(run(&db, Some(MAX_5X), STEADY_DONE), []);
}

#[test]
fn an_attempt_on_a_model_the_cost_table_lacks_marks_the_window_unmeasured() {
    let db = LedgerDb::open_in_memory().unwrap();
    seed_request(&db, "seed", SATURDAY - MINUTE);
    db.insert_samples(
        &[
            head("5h", SATURDAY, 0.10, STEADY_RESET),
            head("5h", SATURDAY + 20 * MINUTE, 0.30, STEADY_RESET),
        ],
        None,
    )
    .unwrap();
    opus_attempt(&db, "r1", SATURDAY + 5 * MINUTE, MILLION);
    attempt_on(&db, "a", "r2", "gpt-5", SATURDAY + 15 * MINUTE, MILLION);
    let rows = run(&db, Some(MAX_5X), STEADY_DONE);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].requests, rows[0].unmeasured),
        (1, true),
        "the unpriced attempt's charge is in the rise and not in the prediction"
    );
    assert!(
        close(rows[0].ratio, 0.20 / opus_share(FIVE_HOUR)),
        "{}",
        rows[0].ratio
    );
}

/// An OAuth account on Max 5x in `store`.
fn subscribed(store: &Store, id: &str) {
    store
        .insert_account(&AccountRow {
            id: id.into(),
            kind: AccountKind::Oauth,
            label: id.into(),
            enabled: true,
            identity: None,
            lifecycle_generation: 1,
            incarnation: 1,
            revision: 1,
            state: AccountState::Ready,
        })
        .unwrap();
    store
        .set_profile(
            id,
            r#"{"account":{"uuid":"u","email_address":"a@example.invalid"},"organization":{}}"#,
            &SubscriptionRow {
                plan: Some("max".into()),
                tier: Some(MAX_5X.into()),
                since: None,
                trial_ends_at: None,
            },
        )
        .unwrap();
}

/// The ledger's writer over `db` and `store`, pricing through `tables`, its clock at `STEADY_DONE`, once it has
/// taken its first turn. Called with tokio's clock paused, so only an advance moves its reconciliation tick.
async fn writer_over(db: Arc<LedgerDb>, store: Arc<Store>, tables: PlanTables) -> (Bus, Ledger) {
    let clock: Clock = Arc::new(|| STEADY_DONE);
    let router = phoebis_routing::spawn(store.clone(), clock.clone(), RouterConfig::default()).unwrap();
    let bus = Bus::new();
    let ledger = spawn(
        db,
        Arc::new(Pricing::new(PriceTable::shipped())),
        store,
        &bus,
        router,
        clock,
        Arc::new(Metrics::new()),
        LedgerConfig {
            tables: Arc::new(tables),
            ..LedgerConfig::default()
        },
    );
    // A sleep, unlike a yield, turns the time driver, which fires the first ticks the writer awaits before it
    // arms its reconciliation interval.
    for _ in 0..10 {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    (bus, ledger)
}

/// Advances the paused clock past one reconciliation interval, then waits with time moving for the rows the
/// writer's pass lands.
async fn after_one_interval(db: &Arc<LedgerDb>) -> Vec<ReconciliationRow> {
    tokio::time::advance(RECONCILE_INTERVAL + Duration::from_millis(1)).await;
    tokio::time::resume();
    wait_for_async(Duration::from_secs(10), || {
        let db = db.clone();
        async move {
            let rows = db.reconciliations(0).await.unwrap();
            (!rows.is_empty()).then_some(rows)
        }
    })
    .await
}

#[tokio::test]
async fn the_writer_reconciles_on_its_tick_through_the_tables_its_config_carries() {
    let store = Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap());
    subscribed(&store, "a");
    let db = Arc::new(LedgerDb::open_in_memory().unwrap());
    steady_rise(&db, false);
    // Opus input at a tenth of the window per million, so the two attempts predict 0.2 against the rise's 0.2,
    // where the shipped table's 0.0841 would read 1.19.
    let mut tables = PlanTables::shipped();
    tables
        .cost
        .windows
        .get_mut(FIVE_HOUR)
        .and_then(|models| models.get_mut(OPUS))
        .unwrap()
        .input
        .share_per_million = 0.1;
    tokio::time::pause();
    let (_bus, _ledger) = writer_over(db.clone(), store, tables).await;
    assert_eq!(
        db.read_blocking(|conn| read_reconciliations(conn, 0))
            .unwrap(),
        [],
        "the first pass waits a whole interval"
    );
    let rows = after_one_interval(&db).await;
    assert_eq!(
        rows.iter()
            .map(|r| (r.account_id.as_str(), r.window_end, r.computed_at))
            .collect::<Vec<_>>(),
        [("a", STEADY_RESET, STEADY_DONE)]
    );
    assert!(
        close(rows[0].predicted_share, 0.2) && close(rows[0].ratio, 1.0),
        "priced through the config's tables: {} at {}",
        rows[0].predicted_share,
        rows[0].ratio
    );
}

#[tokio::test]
async fn the_writer_skips_an_account_whose_subscription_cannot_be_read_and_reconciles_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("router.sqlite");
    let key = StorageKey::generate();
    {
        let store = Store::open(&path, &key).unwrap();
        subscribed(&store, "a");
        subscribed(&store, "b");
    }
    // A plan the store cannot read back as text: `b`'s subscription read fails, `a`'s answers.
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute(
            "UPDATE accounts SET subscription_plan = x'00' WHERE id = 'b'",
            [],
        )
        .unwrap();
    let store = Arc::new(Store::open(&path, &key).unwrap());
    assert!(store.subscription("b").is_err());
    let db = Arc::new(LedgerDb::open_in_memory().unwrap());
    steady_rise_of(&db, "a", false);
    steady_rise_of(&db, "b", false);
    let (lines, _capture) = capture_tracing();
    tokio::time::pause();
    let (_bus, _ledger) = writer_over(db.clone(), store, PlanTables::shipped()).await;
    let rows = after_one_interval(&db).await;
    assert_eq!(
        rows.iter()
            .map(|r| r.account_id.as_str())
            .collect::<Vec<_>>(),
        ["a"],
        "the readable account reconciles"
    );
    let lines = lines.lock().unwrap().clone();
    assert!(
        lines
            .iter()
            .any(|l| l.contains("account=b") && l.contains("subscription could not be read")),
        "the skipped account is named: {lines:?}"
    );
}
