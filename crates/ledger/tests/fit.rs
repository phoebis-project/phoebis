//! The live fit of a window's size: tokens booked at completion against the readings the account answered, on
//! a stepped clock.

use std::sync::Arc;

use phoebis_ledger::{AttemptRecord, Fitter, PlanFactors, PlanWindows, MIN_INTERVAL_MS};
use phoebis_routing::{FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE};
use phoebis_store::FitRow;

const MINUTE: i64 = 60_000;
const HOUR: i64 = 60 * MINUTE;
/// Monday 2026-09-21 at 02:00 UTC, outside the peak band.
const EARLY: i64 = 1_789_956_000_000;
/// Monday 2026-09-21 at 12:00 UTC, the band's first minute.
const NOON: i64 = 1_789_992_000_000;
const OPUS: &str = "claude-opus-5";
const FABLE: &str = "claude-fable-5-1";
const MAX_5X_FIVE_HOUR: f64 = 11_890_000.0;
/// A sixteenth of a window: exact in binary, so an exact interval reads an exact scale.
const SHARE: f64 = 0.0625;
/// The Opus input tokens a sixteenth of the Max 5x 5-hour window holds.
const SIXTEENTH: u64 = 743_125;

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * b.abs().max(1.0)
}

fn fitter(seed: Vec<FitRow>) -> Fitter {
    Fitter::new(
        Arc::new(PlanFactors::shipped()),
        Arc::new(PlanWindows::shipped()),
        seed,
    )
}

fn corrections(row: &FitRow) -> serde_json::Map<String, serde_json::Value> {
    serde_json::from_str(&row.corrections_json).unwrap()
}

/// One account's traffic and readings on a clock the test steps by hand.
struct Run {
    fit: Fitter,
    account: &'static str,
    now: i64,
    next: u32,
}

impl Run {
    fn at(now: i64) -> Run {
        Run::over(fitter(Vec::new()), now)
    }

    fn over(fit: Fitter, now: i64) -> Run {
        Run {
            fit,
            account: "a",
            now,
            next: 0,
        }
    }

    fn step(&mut self, ms: i64) {
        self.now += ms;
    }

    /// An attempt goes out now and stays open.
    fn start(&mut self) -> String {
        self.next += 1;
        let id = format!("r{}", self.next);
        self.fit.started(self.account, &id, 1, self.now);
        id
    }

    /// The attempt that went out at `went_out` completes now with `tokens` plain input tokens of `model`.
    fn complete(&mut self, id: &str, went_out: i64, model: &str, tokens: u64) {
        let usage = AttemptRecord {
            input_tokens: Some(tokens),
            started_at: went_out,
            finished_at: Some(self.now),
            ..Default::default()
        };
        self.fit
            .dispatched(self.account, id, 1, model, &usage, self.now);
    }

    /// An attempt that goes out and completes now.
    fn send(&mut self, model: &str, tokens: u64) {
        let id = self.start();
        self.complete(&id, self.now, model, tokens);
    }

    fn read(&mut self, window: &str, utilization: f64) -> Option<FitRow> {
        self.read_reset(window, utilization, None)
    }

    fn read_reset(&mut self, window: &str, utilization: f64, reset: Option<i64>) -> Option<FitRow> {
        self.fit
            .sampled(self.account, 1, window, utilization, reset, self.now)
    }

    /// An interval of `tokens` Opus input tokens that the reading answers with a rise of `rise`.
    fn interval(&mut self, tokens: u64, reading: f64) -> Option<FitRow> {
        self.step(10 * MINUTE);
        self.send(OPUS, tokens);
        self.step(MINUTE);
        self.read(FIVE_HOUR, reading)
    }
}

#[test]
fn a_constant_model_mix_identifies_the_scale_alone_and_applies_no_per_model_correction() {
    let mut r = Run::at(EARLY);
    assert!(
        r.read(FIVE_HOUR, 0.0).is_none(),
        "the first reading opens the run"
    );
    let mut confidence = 0.0;
    let mut last = None;
    for n in 1..=4u32 {
        let row = r
            .interval(SIXTEENTH, n as f64 * SHARE)
            .expect("each interval folds");
        assert_eq!((row.intervals, row.window.as_str()), (n, FIVE_HOUR));
        assert!(
            row.confidence > confidence,
            "confidence rises with the intervals"
        );
        confidence = row.confidence;
        last = Some(row);
    }
    let row = last.unwrap();
    assert_eq!(
        row.scale, MAX_5X_FIVE_HOUR,
        "the scale is the tokens over the share"
    );
    assert_eq!(
        row.corrections_json, "{}",
        "one model throughout: nothing to tell its factor from the scale"
    );
    assert_eq!((row.account_id.as_str(), row.incarnation), ("a", 1));
}

#[test]
fn a_varied_mix_corrects_the_model_factor() {
    let mut r = Run::at(EARLY);
    r.read(FIVE_HOUR, 0.0);
    for n in 1..=3u32 {
        r.interval(SIXTEENTH, n as f64 * SHARE).unwrap();
    }
    // Fable 5.1 weighs 3.61 by the table; the meter charges it 20 % more than that.
    let fable_meter = 3.61 * 100_000.0;
    let rise = 1.2 * fable_meter / MAX_5X_FIVE_HOUR;
    let mut reading = 3.0 * SHARE;
    for fold in 1..=2 {
        r.step(10 * MINUTE);
        r.send(FABLE, 100_000);
        r.step(MINUTE);
        reading += rise;
        let row = r
            .read(FIVE_HOUR, reading)
            .expect("the Fable interval folds");
        let correction = corrections(&row)[FABLE].as_f64().unwrap();
        assert!(
            close(correction, 1.2),
            "fold {fold}: the Fable correction reads {correction}"
        );
        assert!(
            close(row.scale, MAX_5X_FIVE_HOUR),
            "fold {fold}: the scale holds at {}",
            row.scale
        );
    }
}

#[test]
fn a_mixed_fable_and_opus_interval_fits_the_fable_window_on_the_fable_tokens_alone() {
    let mut r = Run::at(EARLY);
    // The poll's own names open the runs and the head's names close them: both reach the same window.
    assert!(r.read(SEVEN_DAY, 0.25).is_none());
    assert!(r.read("fable", 0.25).is_none());
    r.step(10 * MINUTE);
    r.send(OPUS, 1_000_000);
    r.send(FABLE, 100_000);
    r.step(MINUTE);
    let shared = r.read("7d", 0.25 + SHARE).expect("the shared week folds");
    assert_eq!(shared.window, SEVEN_DAY);
    assert!(
        close(shared.scale, (1_000_000.0 + 3.61 * 100_000.0) / SHARE),
        "the shared week books every model at its factor: {}",
        shared.scale
    );
    let fable = r
        .read("7d_oi", 0.25 + SHARE)
        .expect("the Fable-scoped week folds");
    assert_eq!(fable.window, SEVEN_DAY_FABLE);
    assert!(
        close(fable.scale, 100_000.0 / SHARE),
        "the Fable-scoped week books the Fable tokens alone and without the model factor: {}",
        fable.scale
    );
}

#[test]
fn an_interval_spanning_a_reset_is_dropped() {
    let (first, second) = (EARLY + 3 * HOUR, EARLY + 8 * HOUR);
    let mut r = Run::at(EARLY);
    r.read_reset(FIVE_HOUR, 0.5, Some(first));
    r.step(10 * MINUTE);
    r.send(OPUS, 3 * SIXTEENTH);
    r.step(MINUTE);
    assert!(
        r.read_reset(FIVE_HOUR, 0.5 + SHARE, Some(second)).is_none(),
        "the reset moved: the interval is dropped"
    );
    r.step(10 * MINUTE);
    r.send(OPUS, SIXTEENTH);
    r.step(MINUTE);
    let row = r
        .read_reset(FIVE_HOUR, 0.5 + 2.0 * SHARE, Some(second))
        .expect("the run restarted at the reset");
    assert_eq!(
        (row.scale, row.intervals),
        (MAX_5X_FIVE_HOUR, 1),
        "only the tokens after the reset"
    );

    let mut r = Run::at(EARLY);
    r.read(FIVE_HOUR, 0.5);
    r.step(10 * MINUTE);
    r.send(OPUS, 3 * SIXTEENTH);
    r.step(MINUTE);
    assert!(
        r.read(FIVE_HOUR, 0.125).is_none(),
        "the utilization fell: the interval is dropped"
    );
    let row = r
        .interval(SIXTEENTH, 0.125 + SHARE)
        .expect("the run restarted at the fall");
    assert_eq!((row.scale, row.intervals), (MAX_5X_FIVE_HOUR, 1));
}

#[test]
fn a_stream_across_a_reset_fits_nothing() {
    let (first, second) = (EARLY + 3 * HOUR, EARLY + 8 * HOUR);
    let mut r = Run::at(EARLY);
    r.read_reset(FIVE_HOUR, 0.5, Some(first));
    r.step(10 * MINUTE);
    let went_out = r.now;
    let id = r.start();
    r.step(2 * MINUTE);
    assert!(r.read_reset(FIVE_HOUR, SHARE, Some(second)).is_none());
    r.step(MINUTE);
    r.complete(&id, went_out, OPUS, SIXTEENTH);
    r.step(MINUTE);
    assert!(
        r.read_reset(FIVE_HOUR, 2.0 * SHARE, Some(second)).is_none(),
        "a stream that straddles the reset fits neither side"
    );
    r.step(10 * MINUTE);
    r.send(OPUS, SIXTEENTH);
    r.step(MINUTE);
    let row = r
        .read_reset(FIVE_HOUR, 3.0 * SHARE, Some(second))
        .expect("the next interval fits on its own tokens");
    assert_eq!((row.scale, row.intervals), (MAX_5X_FIVE_HOUR, 1));
}

#[test]
fn foreign_spend_with_nothing_open_drops_the_interval() {
    let mut r = Run::at(EARLY);
    r.read(FIVE_HOUR, 0.25);
    r.step(2 * MINUTE);
    assert!(
        r.read(FIVE_HOUR, 0.25 + SHARE).is_none(),
        "a rise the router sent nothing for is another client's"
    );
    let row = r
        .interval(SIXTEENTH, 0.25 + 2.0 * SHARE)
        .expect("the next interval fits");
    assert_eq!(
        row.scale, MAX_5X_FIVE_HOUR,
        "the foreign rise is not in the delta"
    );
}

#[test]
fn an_idle_interval_fits_nothing_and_the_scale_stays_finite() {
    let mut r = Run::at(EARLY);
    r.read(FIVE_HOUR, 0.25);
    let first = r.interval(SIXTEENTH, 0.25 + SHARE).unwrap();
    r.step(HOUR);
    let idle = r.read(FIVE_HOUR, 0.25 + SHARE);
    assert!(
        idle.as_ref()
            .is_none_or(|row| (row.intervals, row.scale, row.confidence)
                == (first.intervals, first.scale, first.confidence)),
        "no tokens and no delta is no interval: {idle:?}"
    );
    let second = r.interval(SIXTEENTH, 0.25 + 2.0 * SHARE).unwrap();
    assert_eq!(second.intervals, 2);
    assert_eq!(second.scale, first.scale);
    assert!(second.scale.is_finite() && second.confidence.is_finite());
}

#[test]
fn a_rounded_flat_interval_carries_its_tokens_into_the_next_measurable_rise() {
    let mut r = Run::at(EARLY);
    r.read(FIVE_HOUR, 0.25);
    assert!(
        r.interval(400_000, 0.25).is_none(),
        "tokens booked, the reading unchanged"
    );
    let row = r
        .interval(SIXTEENTH - 400_000, 0.25 + SHARE)
        .expect("the rise folds");
    assert_eq!(
        (row.scale, row.intervals),
        (MAX_5X_FIVE_HOUR, 1),
        "one fold over both intervals' tokens"
    );
}

#[test]
fn a_head_sample_before_the_streams_completion_is_not_foreign_and_its_tokens_reach_the_fit() {
    let mut r = Run::at(EARLY);
    r.read("5h", 0.25);
    r.step(10 * MINUTE);
    let went_out = r.now;
    let id = r.start();
    r.step(2 * MINUTE);
    assert!(
        r.read("5h", 0.25 + SHARE).is_none(),
        "the rise is the open attempt's: carried, not dropped"
    );
    r.step(MINUTE);
    r.complete(&id, went_out, OPUS, SIXTEENTH);
    r.step(MINUTE);
    let row = r
        .read("5h", 0.25 + SHARE)
        .expect("the flat reading folds the carried delta");
    assert_eq!(
        (row.scale, row.window.as_str()),
        (MAX_5X_FIVE_HOUR, FIVE_HOUR)
    );
}

#[test]
fn a_run_longer_than_the_window_is_dropped() {
    let mut r = Run::at(EARLY);
    r.read(FIVE_HOUR, 0.25);
    assert!(r.interval(SIXTEENTH, 0.25).is_none());
    r.now = EARLY + 5 * HOUR + MINUTE;
    assert!(
        r.read(FIVE_HOUR, 0.25 + SHARE).is_none(),
        "a run past the window's length is dropped"
    );
    let row = r.interval(SIXTEENTH, 0.25 + 2.0 * SHARE).unwrap();
    assert_eq!((row.scale, row.intervals), (MAX_5X_FIVE_HOUR, 1));
}

#[test]
fn a_non_finite_persisted_row_is_refused_at_load_and_rebuilt_from_the_prior() {
    let row = |account: &str, scale: f64, confidence: f64| FitRow {
        account_id: account.into(),
        window: FIVE_HOUR.into(),
        scale,
        confidence,
        intervals: 10,
        corrections_json: "{}".into(),
        fitted_at: 0,
        incarnation: 1,
        ..Default::default()
    };
    let mut r = Run::over(
        fitter(vec![
            row("a", f64::NAN, 0.9),
            row("b", 20_000_000.0, f64::INFINITY),
            row("c", 0.0, 0.9),
        ]),
        EARLY,
    );
    for account in ["a", "b", "c"] {
        r.account = account;
        r.read(FIVE_HOUR, 0.25);
        let fitted = r.interval(SIXTEENTH, 0.25 + SHARE).unwrap();
        assert_eq!(
            (fitted.scale, fitted.intervals),
            (MAX_5X_FIVE_HOUR, 1),
            "{account} starts over rather than from its persisted row"
        );
        assert!(fitted.confidence.is_finite(), "{account}");
    }
}

#[test]
fn samples_closer_than_a_minute_accumulate_and_fold_at_the_next() {
    let mut r = Run::at(EARLY);
    r.read(FIVE_HOUR, 0.25);
    r.step(10_000);
    r.send(OPUS, SIXTEENTH);
    r.step(20_000);
    assert!(
        r.read(FIVE_HOUR, 0.25 + SHARE / 2.0).is_none(),
        "thirty seconds is under the shortest interval"
    );
    r.step(MIN_INTERVAL_MS - 20_000);
    let row = r
        .read(FIVE_HOUR, 0.25 + SHARE)
        .expect("a minute past the run's start");
    assert_eq!(
        row.scale, MAX_5X_FIVE_HOUR,
        "the delta runs from the run's start"
    );
}

#[test]
fn the_drift_indicator_moves_when_the_table_and_the_fit_disagree() {
    let predicted = PlanWindows::shipped()
        .predicted("default_claude_max_5x", FIVE_HOUR)
        .unwrap();
    for (drift, tokens) in [(1.0, SIXTEENTH), (1.3, 966_063)] {
        let mut r = Run::at(EARLY);
        r.read(FIVE_HOUR, 0.0);
        let mut last = None;
        for n in 1..=4u32 {
            last = r.interval(tokens, n as f64 * SHARE);
        }
        let read = last.unwrap().scale / predicted;
        assert!(
            (read - drift).abs() < 1e-5,
            "a fit of {drift} times the row reads {read}"
        );
    }
}

#[test]
fn a_dropped_attempt_closes_and_the_foreign_spend_gate_reopens() {
    let mut r = Run::at(EARLY);
    r.read(FIVE_HOUR, 0.25);
    r.step(10 * MINUTE);
    let id = r.start();
    r.step(MINUTE);
    r.fit.dropped("a", &id, 1);
    r.step(2 * MINUTE);
    assert!(
        r.read(FIVE_HOUR, 0.25 + SHARE).is_none(),
        "nothing open any more: the rise is foreign"
    );
    let row = r.interval(SIXTEENTH, 0.25 + 2.0 * SHARE).unwrap();
    assert_eq!(
        row.scale, MAX_5X_FIVE_HOUR,
        "the foreign rise was dropped, not carried as own spend"
    );
}

#[test]
fn the_fitted_band_factor_reads_the_row_back_while_the_band_holds_and_one_when_it_went_away() {
    let fitted = |band_rise: f64| {
        let mut r = Run::at(EARLY);
        r.read(FIVE_HOUR, 0.0);
        let mut reading = 0.0;
        for _ in 0..4 {
            reading += SHARE;
            let row = r.interval(SIXTEENTH, reading).unwrap();
            assert_eq!(
                row.band_factor, None,
                "no in-band fold yet, so no band factor"
            );
        }
        r.now = NOON + 5 * MINUTE;
        assert!(r
            .read(FIVE_HOUR, reading)
            .is_none_or(|row| row.intervals == 4));
        let mut rows = Vec::new();
        for _ in 0..4 {
            reading += band_rise * SHARE;
            rows.push(r.interval(SIXTEENTH, reading).unwrap());
        }
        rows
    };
    let holds = fitted(1.45);
    assert!(
        holds.iter().all(|row| row.band_factor.is_some()),
        "both sides have folded from the first in-band fold on"
    );
    let factor = holds.last().unwrap().band_factor.unwrap();
    assert!(
        close(factor, 1.45),
        "a band that weighs as the row says reads the row: {factor}"
    );
    assert!(
        holds.iter().all(|row| close(row.scale, MAX_5X_FIVE_HOUR)),
        "weighted by the band, an in-band fold reads the scale the out-of-band ones did"
    );
    let gone = fitted(1.0).last().unwrap().band_factor.unwrap();
    assert!(close(gone, 1.0), "a band that went away reads one: {gone}");
}

#[test]
fn a_fold_waits_for_three_quanta_of_delta_and_reads_the_ratio_of_sums() {
    let mut r = Run::at(EARLY);
    r.read(FIVE_HOUR, 0.25);
    assert!(
        r.interval(100_000, 0.26).is_none(),
        "one quantum of delta is carried"
    );
    assert!(
        r.interval(150_000, 0.27).is_none(),
        "two quanta are carried"
    );
    let row = r.interval(106_700, 0.28).expect("three quanta fold");
    assert_eq!(row.intervals, 1, "one fold over the three intervals");
    let expected = (100_000.0 + 150_000.0 + 106_700.0) / (0.28 - 0.25);
    assert!(
        close(row.scale, expected),
        "the scale is the summed tokens over the summed delta: {}",
        row.scale
    );
}

#[test]
fn a_fall_smaller_than_a_quantum_is_rounding_noise_and_the_run_keeps_carrying() {
    let mut r = Run::at(EARLY);
    r.read(FIVE_HOUR, 0.25);
    assert!(
        r.interval(200_000, 0.27).is_none(),
        "two hundredths carried"
    );
    assert!(
        r.interval(28_040, 0.266).is_none(),
        "a reading 0.004 lower is rounding between the head and the poll: carried as flat"
    );
    let row = r
        .interval(200_000, 0.286)
        .expect("the run folds over all three intervals");
    assert_eq!(
        row.intervals, 1,
        "one fold, the accumulator never restarted"
    );
    let expected = (200_000.0 + 28_040.0 + 200_000.0) / (0.286 - 0.25);
    assert!(
        close(row.scale, expected),
        "every interval's tokens over the run's net rise: {} against {expected}",
        row.scale
    );
}

#[test]
fn a_charge_is_weighed_by_the_band_at_its_completion() {
    let mut r = Run::at(NOON - 10 * MINUTE);
    r.read(FIVE_HOUR, 0.25);
    let went_out = r.now;
    let id = r.start();
    r.step(20 * MINUTE);
    r.complete(&id, went_out, OPUS, SIXTEENTH);
    r.step(MINUTE);
    let row = r.read(FIVE_HOUR, 0.25 + SHARE).expect("the interval folds");
    assert!(
        close(row.scale, 1.45 * MAX_5X_FIVE_HOUR),
        "an attempt sent before noon and answered after it lands inside the band: {}",
        row.scale
    );
}

#[test]
fn a_restart_continues_the_carried_run_and_the_band_sums_from_the_persisted_row() {
    let mut r = Run::at(EARLY);
    r.read(FIVE_HOUR, 0.0);
    r.interval(SIXTEENTH, SHARE).unwrap();
    let carried = r
        .interval(200_000, SHARE + 0.01)
        .expect("a fitted window reports its carry");
    assert_eq!(carried.intervals, 1);
    assert!(close(carried.carry_delta, 0.01));
    assert!(close(carried.carry_weighted, 200_000.0));

    let mut after = Run::over(fitter(vec![carried]), r.now);
    let row = after
        .interval(156_700, SHARE + 0.03)
        .expect("the carried run folds across the restart");
    assert_eq!(row.intervals, 2);
    assert!(
        close(row.scale, MAX_5X_FIVE_HOUR),
        "the fold reads both sides' tokens over both sides' delta: {}",
        row.scale
    );
    assert!(
        close(row.band_outside_delta, SHARE + 0.03)
            && close(row.band_outside_tokens, (SIXTEENTH + 356_700) as f64),
        "the band sums continue: {row:?}"
    );
}

#[test]
fn a_restored_window_moves_its_scale_rather_than_a_correction_on_its_first_fold() {
    let seed = FitRow {
        account_id: "a".into(),
        window: FIVE_HOUR.into(),
        scale: MAX_5X_FIVE_HOUR,
        confidence: 0.6,
        intervals: 4,
        corrections_json: "{}".into(),
        fitted_at: EARLY - HOUR,
        incarnation: 1,
        ..Default::default()
    };
    let mut r = Run::over(fitter(vec![seed]), EARLY);
    r.read(FIVE_HOUR, 0.25);
    // One fold whose reading runs 10 % high against its tokens: noise, on the model every fold has used.
    let row = r
        .interval(SIXTEENTH, 0.25 + 1.1 * SHARE)
        .expect("the restored window folds");
    assert_eq!(
        row.corrections_json, "{}",
        "the fold before the restart is unknown, so nothing says the mix changed"
    );
    let observed = SIXTEENTH as f64 / (1.1 * SHARE);
    assert!(
        close(
            row.scale,
            MAX_5X_FIVE_HOUR + (observed - MAX_5X_FIVE_HOUR) / 5.0
        ),
        "the scale takes the fold at its weight: {}",
        row.scale
    );
    assert_eq!(row.intervals, 5);
}

#[test]
fn an_attempt_after_the_announced_reset_folds_and_one_straddling_it_drops_the_run() {
    let (announced, next) = (EARLY + HOUR, EARLY + 6 * HOUR);

    // Idle through the reset: the new window's first attempt is what reveals it, through its own head.
    let mut r = Run::at(EARLY);
    r.read_reset(FIVE_HOUR, 0.5, Some(announced));
    r.now = EARLY + 2 * HOUR;
    let went_out = r.now;
    let id = r.start();
    r.step(MINUTE);
    assert!(r.read_reset(FIVE_HOUR, 0.0, Some(next)).is_none());
    r.step(MINUTE);
    r.complete(&id, went_out, OPUS, SIXTEENTH);
    r.step(MINUTE);
    let row = r
        .read_reset(FIVE_HOUR, SHARE, Some(next))
        .expect("an attempt sent after the announced reset belongs to the new window");
    assert_eq!((row.scale, row.intervals), (MAX_5X_FIVE_HOUR, 1));

    // A stream sent before the announced reset and answered after it, landing in a run that already carries
    // three quanta and another attempt's tokens.
    let mut r = Run::at(EARLY);
    r.read_reset(FIVE_HOUR, 0.5, Some(announced));
    r.now = EARLY + 50 * MINUTE;
    let straddler_out = r.now;
    let straddler = r.start();
    r.now = EARLY + 65 * MINUTE;
    assert!(r.read_reset(FIVE_HOUR, 0.0, Some(next)).is_none());
    r.step(MINUTE);
    let later_out = r.now;
    let later = r.start();
    r.step(2 * MINUTE);
    assert!(
        r.read_reset(FIVE_HOUR, 0.04, Some(next)).is_none(),
        "the rise is carried while the later attempt is open"
    );
    r.step(MINUTE);
    r.complete(&later, later_out, OPUS, SIXTEENTH);
    r.step(MINUTE);
    r.complete(&straddler, straddler_out, OPUS, SIXTEENTH);
    r.step(2 * MINUTE);
    assert!(
        r.read_reset(FIVE_HOUR, 0.04, Some(next)).is_none(),
        "the straddling stream drops the run on a flat reading too"
    );
    let row = r
        .interval(SIXTEENTH, 0.04 + SHARE)
        .expect("the run after the drop fits on its own tokens");
    assert_eq!((row.scale, row.intervals), (MAX_5X_FIVE_HOUR, 1));
}
