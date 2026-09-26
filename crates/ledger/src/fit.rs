//! The live fit of each subscription account's window sizes: the meter tokens the router dispatched, booked at
//! completion, against the utilization the account's readings answered, per account and window kind.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use phoebis_routing::{window_for, FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE};
use phoebis_store::FitRow;

use crate::plan::{PlanFactors, PlanWindows};
use crate::record::AttemptRecord;

/// The shortest interval judged: a closer reading only accumulates, since a delta over seconds is mostly the
/// readings' rounding.
pub const MIN_INTERVAL_MS: i64 = 60_000;

/// The readings' rounding step. The head and the poll can round one value a step apart, so a fall smaller than
/// this is noise rather than a reset.
const QUANTUM: f64 = 0.01;

/// Three quanta: a run folds once its delta reaches this, so no fold rests on a single rounding step.
const FOLD_DELTA: f64 = 3.0 * QUANTUM;

/// Slack under a whole number of quanta, so steps of 0.01 summed or subtracted in floating point still reach it.
const SLACK: f64 = 1e-9;

/// The floor of a running average's weight, so a fit keeps following a plan change after a few dozen folds
/// rather than freezing on its history.
const FLOOR_WEIGHT: f64 = 0.05;

/// The folds at which a fit with no residual reaches half confidence.
const HALF_CONFIDENCE_AT: f64 = 2.0;

/// The confidence a fit needs before a fold may move a per-model correction.
const CONFIDENT: f64 = 0.5;

/// The share of a run's meter tokens one model must carry to dominate it.
const DOMINANT_SHARE: f64 = 0.8;

/// Two readings whose resets differ by more than this name different windows; within it they are one reset as
/// the head's whole seconds and the endpoint's timestamp spell it.
const RESET_TOLERANCE_MS: i64 = 60_000;

/// Where a restored run's tokens sit: the persisted carry keeps their total, not their models.
const RESTORED: &str = "";

/// The fitter's window for a sample record's key: the head's `5h`, `7d` and `7d_oi`, the poll's `fable`, and the
/// canonical names the endpoint and the store spell; `None` for any other key.
fn window_key(key: &str) -> Option<&'static str> {
    match key {
        "5h" | FIVE_HOUR => Some(FIVE_HOUR),
        "7d" | SEVEN_DAY => Some(SEVEN_DAY),
        "7d_oi" | "fable" | SEVEN_DAY_FABLE => Some(SEVEN_DAY_FABLE),
        _ => None,
    }
}

fn window_ms(window: &str) -> i64 {
    const HOUR: i64 = 3_600_000;
    if window == FIVE_HOUR {
        5 * HOUR
    } else {
        7 * 24 * HOUR
    }
}

/// A weight that starts at one and shrinks as `count` grows, down to the floor.
fn weight(count: u32) -> f64 {
    (1.0 / (f64::from(count) + 1.0)).max(FLOOR_WEIGHT)
}

#[derive(Clone, Copy, Debug)]
struct Reading {
    at: i64,
    utilization: f64,
    reset: Option<i64>,
}

/// Meter tokens booked into a run: weighted by the band each charge landed in, and unweighted, split by band.
#[derive(Clone, Copy, Debug, Default)]
struct Tally {
    weighted: f64,
    inside: f64,
    outside: f64,
}

/// What a window has booked and seen since its last fold.
#[derive(Debug, Default)]
struct Run {
    tokens: BTreeMap<String, Tally>,
    delta: f64,
    since: Option<i64>,
    /// An attempt that went out before a reset completed into this run: its charge straddles the reset, so the
    /// rise that carries it is dropped.
    spanned: bool,
}

#[derive(Clone, Copy, Debug, Default)]
struct Correction {
    factor: f64,
    moves: u32,
}

#[derive(Debug, Default)]
struct WindowFit {
    incarnation: Option<u64>,
    scale: f64,
    confidence: f64,
    intervals: u32,
    fitted_at: i64,
    corrections: BTreeMap<String, Correction>,
    band_inside_tokens: f64,
    band_inside_delta: f64,
    band_outside_tokens: f64,
    band_outside_delta: f64,
    /// The model that dominated the previous fold; `None` when none did or the fitter does not know, as after a
    /// restart, and then no fold counts as a change of mix. A mixed baseline attributes no shift to one model, so
    /// a correction moves only between two single-model folds whose dominant models differ.
    dominant: Option<String>,
    /// The latest reading judged, which the next one's rise is measured from.
    last: Option<Reading>,
    run: Run,
    /// When a reset was last seen: an attempt that went out before it and completes after spans it.
    reset_at: Option<i64>,
}

impl WindowFit {
    fn restored(row: &FitRow) -> WindowFit {
        let corrections = serde_json::from_str::<BTreeMap<String, f64>>(&row.corrections_json)
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, factor)| factor.is_finite() && *factor > 0.0)
            .map(|(model, factor)| (model, Correction { factor, moves: 1 }))
            .collect();
        let mut run = Run {
            delta: row.carry_delta,
            since: row.carry_since,
            ..Run::default()
        };
        if row.carry_weighted > 0.0 {
            run.tokens.insert(
                RESTORED.into(),
                Tally {
                    weighted: row.carry_weighted,
                    inside: row.carry_inside,
                    outside: row.carry_outside,
                },
            );
        }
        WindowFit {
            incarnation: Some(row.incarnation),
            scale: row.scale,
            confidence: row.confidence,
            intervals: row.intervals,
            fitted_at: row.fitted_at,
            corrections,
            band_inside_tokens: row.band_inside_tokens,
            band_inside_delta: row.band_inside_delta,
            band_outside_tokens: row.band_outside_tokens,
            band_outside_delta: row.band_outside_delta,
            dominant: None,
            // The reading's own time is not kept; the run's start is no later, which only lets the next reading
            // be judged at once.
            last: row.carry_utilization.map(|utilization| Reading {
                at: row.carry_since.unwrap_or(row.fitted_at),
                utilization,
                reset: row.carry_reset,
            }),
            run,
            reset_at: None,
        }
    }

    fn correction(&self, model: &str) -> f64 {
        self.corrections.get(model).map_or(1.0, |c| c.factor)
    }

    /// The run's tokens with each model's correction applied.
    fn booked(&self) -> Tally {
        let mut sum = Tally::default();
        for (model, tally) in &self.run.tokens {
            let correction = self.correction(model);
            sum.weighted += correction * tally.weighted;
            sum.inside += correction * tally.inside;
            sum.outside += correction * tally.outside;
        }
        sum
    }

    /// Opens a fresh run at `reading`: whatever was carried is dropped.
    fn restart(&mut self, reading: Reading) {
        self.last = Some(reading);
        self.run = Run {
            since: Some(reading.at),
            ..Run::default()
        };
    }

    /// One observation over the whole run, the band-weighted booked tokens over the summed delta. A fold whose
    /// dominant model differs from the previous fold's known one, on a confident fit, informs that model's
    /// correction and leaves the scale where it was; every other fold moves the scale.
    fn fold(&mut self, reading: Reading) {
        let booked = self.booked();
        let delta = self.run.delta;
        let observed = booked.weighted / delta;
        let total: f64 = self.run.tokens.values().map(|t| t.weighted).sum();
        let dominant = self
            .run
            .tokens
            .iter()
            .filter(|(model, _)| model.as_str() != RESTORED)
            .max_by(|a, b| a.1.weighted.total_cmp(&b.1.weighted))
            .filter(|(_, tally)| tally.weighted >= DOMINANT_SHARE * total)
            .map(|(model, _)| model.clone());
        let fitted = self.intervals > 0;
        let residual = if fitted {
            ((observed - self.scale) / self.scale).abs()
        } else {
            0.0
        };
        let corrected = fitted
            && self.confidence >= CONFIDENT
            && dominant.is_some()
            && self.dominant.is_some()
            && dominant != self.dominant
            && dominant
                .as_deref()
                .is_some_and(|model| self.correct(model, booked.weighted, delta));
        if !corrected {
            self.scale += weight(self.intervals) * (observed - self.scale);
        }
        self.intervals += 1;
        let n = f64::from(self.intervals);
        self.confidence = n / (n + HALF_CONFIDENCE_AT) / (1.0 + residual);
        self.fitted_at = reading.at;
        // A run that mixed both sides of the band joins neither sum.
        if booked.inside > 0.0 && booked.outside == 0.0 {
            self.band_inside_tokens += booked.inside;
            self.band_inside_delta += delta;
        } else if booked.outside > 0.0 && booked.inside == 0.0 {
            self.band_outside_tokens += booked.outside;
            self.band_outside_delta += delta;
        }
        self.dominant = dominant;
        self.restart(reading);
    }

    /// Moves `model`'s correction toward the factor that would have made the run read the current scale.
    fn correct(&mut self, model: &str, booked: f64, delta: f64) -> bool {
        let Some(own) = self.run.tokens.get(model).map(|t| t.weighted) else {
            return false;
        };
        let others = booked - self.correction(model) * own;
        let implied = (self.scale * delta - others) / own;
        if !(implied.is_finite() && implied > 0.0) {
            return false;
        }
        let correction = self.corrections.entry(model.to_string()).or_default();
        correction.factor += weight(correction.moves) * (implied - correction.factor);
        correction.moves += 1;
        true
    }

    /// The out-of-band scale over the in-band one, both on unweighted tokens: the row's factor times the
    /// out-of-band scale over the band-weighted in-band one, read without looking the row up again.
    fn band_factor(&self) -> Option<f64> {
        (self.band_inside_tokens > 0.0
            && self.band_inside_delta > 0.0
            && self.band_outside_tokens > 0.0
            && self.band_outside_delta > 0.0)
            .then(|| {
                (self.band_outside_tokens / self.band_outside_delta)
                    / (self.band_inside_tokens / self.band_inside_delta)
            })
    }

    /// The row to write, once the window has folded: before that there is no scale to persist, and every reader
    /// falls back to the tier's `plan_windows` row.
    fn report(&self, account_id: &str, window: &str) -> Option<FitRow> {
        if self.intervals == 0 {
            return None;
        }
        let corrections: BTreeMap<&str, f64> = self
            .corrections
            .iter()
            .map(|(model, c)| (model.as_str(), c.factor))
            .collect();
        let carry = self.booked();
        Some(FitRow {
            account_id: account_id.to_string(),
            window: window.to_string(),
            scale: self.scale,
            confidence: self.confidence,
            intervals: self.intervals,
            corrections_json: serde_json::to_string(&corrections).unwrap_or_else(|_| "{}".into()),
            fitted_at: self.fitted_at,
            incarnation: self.incarnation.unwrap_or_default(),
            band_factor: self.band_factor(),
            carry_weighted: carry.weighted,
            carry_inside: carry.inside,
            carry_outside: carry.outside,
            carry_delta: self.run.delta,
            carry_since: self.run.since,
            carry_utilization: self.last.map(|r| r.utilization),
            carry_reset: self.last.and_then(|r| r.reset),
            band_inside_tokens: self.band_inside_tokens,
            band_inside_delta: self.band_inside_delta,
            band_outside_tokens: self.band_outside_tokens,
            band_outside_delta: self.band_outside_delta,
        })
    }
}

/// The live fit of every subscription account's window sizes, one state per account and window kind.
///
/// Tokens are booked when an attempt completes, since the meter charges a request on the reading that follows
/// it, weighted by the band in force at that instant; an attempt still open tells the fitter that a rise
/// meanwhile is its own. A reading at least `MIN_INTERVAL_MS` past the previous one adds its rise to the run, and
/// the run folds once its delta reaches three quanta with tokens booked and no attempt open on the account: a
/// rise read while one is open may carry that attempt's charge, whose tokens are not booked yet, so the run
/// carries both until a reading with nothing open. A run is dropped when a reset falls inside it (the reset moved,
/// or the utilization fell by a quantum or more; a smaller fall reads as flat), when it outgrows the window, when
/// it rose with nothing booked and nothing open (spend the router did not send), or when a stream that straddles a
/// reset lands in it.
/// A window that has folded answers its row, carry included, at every reading judged, so a restart continues
/// the run rather than starting it over.
pub struct Fitter {
    factors: Arc<PlanFactors>,
    windows: Arc<PlanWindows>,
    /// The attempts selected and not yet completed or dropped, per account, with when each went out.
    open: HashMap<String, HashMap<(String, u32), i64>>,
    fits: HashMap<(String, &'static str), WindowFit>,
}

impl Fitter {
    /// A seed row whose `scale` or `confidence` is not finite is refused and its account and window start from
    /// the prior, and so is one carrying any other non-finite number: until it folds again, every reader reads
    /// the tier's `plan_windows` row.
    pub fn new(factors: Arc<PlanFactors>, windows: Arc<PlanWindows>, seed: Vec<FitRow>) -> Fitter {
        let mut fits = HashMap::new();
        for row in seed {
            let Some(window) = window_key(&row.window) else {
                continue;
            };
            if !row.sound() {
                tracing::warn!(
                    account = %row.account_id,
                    window,
                    "a persisted window fit holds a number that is not finite and starts over"
                );
                continue;
            }
            fits.insert((row.account_id.clone(), window), WindowFit::restored(&row));
        }
        Fitter {
            factors,
            windows,
            open: HashMap::new(),
            fits,
        }
    }

    pub fn started(&mut self, account_id: &str, request_id: &str, attempt: u32, at: i64) {
        self.open
            .entry(account_id.to_string())
            .or_default()
            .insert((request_id.to_string(), attempt), at);
    }

    /// Closes an open attempt without booking tokens.
    pub fn dropped(&mut self, account_id: &str, request_id: &str, attempt: u32) {
        self.close(account_id, request_id, attempt);
    }

    /// The attempt completed at `at`: closes it and books its tokens, band-weighted at `at`, into the
    /// accumulators of the windows `phoebis_routing::window_for(canonical_model)` applies to the model (the shared
    /// two always, `seven_day_fable` for a Fable model, in Fable tokens without the model factor). A model the
    /// factor table lacks books at the reference's row, as the burn prices it.
    pub fn dispatched(
        &mut self,
        account_id: &str,
        request_id: &str,
        attempt: u32,
        canonical_model: &str,
        usage: &AttemptRecord,
        at: i64,
    ) {
        let went_out = self
            .close(account_id, request_id, attempt)
            .unwrap_or(usage.started_at);
        let Some(row) = self
            .factors
            .factor(canonical_model)
            .or_else(|| self.factors.models.get(&self.factors.reference))
        else {
            return;
        };
        let count = |n: Option<u64>| n.unwrap_or(0);
        let typed = row.typed_tokens(
            count(usage.input_tokens),
            count(usage.cache_read_tokens),
            count(usage.cache_write_5m_tokens),
            count(usage.cache_write_1h_tokens),
            count(usage.output_tokens),
        );
        if !(typed > 0.0 && typed.is_finite()) {
            return;
        }
        let fable = window_for(canonical_model) == SEVEN_DAY_FABLE;
        for window in [FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE] {
            let tokens = match window {
                SEVEN_DAY_FABLE if fable => typed,
                SEVEN_DAY_FABLE => continue,
                _ => row.factor * typed,
            };
            let band = self.windows.band_factor(window, at);
            let fit = self
                .fits
                .entry((account_id.to_string(), window))
                .or_default();
            if fit.reset_at.is_some_and(|reset_at| went_out < reset_at) {
                fit.run.spanned = true;
                continue;
            }
            let tally = fit
                .run
                .tokens
                .entry(canonical_model.to_string())
                .or_default();
            tally.weighted += band * tokens;
            if band == 1.0 {
                tally.outside += tokens;
            } else {
                tally.inside += tokens;
            }
        }
    }

    /// A sample landed; answers the row to report when the window has folded and the reading moved its state
    /// (a fold, a dropped run, or the carry), `None` otherwise.
    pub fn sampled(
        &mut self,
        account_id: &str,
        incarnation: u64,
        window: &str,
        utilization: f64,
        reset: Option<i64>,
        at: i64,
    ) -> Option<FitRow> {
        let window = window_key(window)?;
        if !utilization.is_finite() {
            return None;
        }
        let open = self
            .open
            .get(account_id)
            .is_some_and(|attempts| !attempts.is_empty());
        let fit = self
            .fits
            .entry((account_id.to_string(), window))
            .or_default();
        // An id deleted and created again is another holder, whose meter has nothing to do with the old one's.
        if fit.incarnation.is_some_and(|held| held != incarnation) {
            *fit = WindowFit::default();
        }
        fit.incarnation = Some(incarnation);
        let reading = Reading {
            at,
            utilization,
            reset,
        };
        let Some(last) = fit.last else {
            // Tokens booked before the first reading belong to no measured interval.
            fit.restart(reading);
            return fit.report(account_id, window);
        };
        if at < last.at {
            return None;
        }
        let reset_moved = matches!(
            (last.reset, reset),
            (Some(was), Some(now)) if (was - now).abs() > RESET_TOLERANCE_MS
        );
        if reset_moved || last.utilization - utilization + SLACK >= QUANTUM {
            fit.restart(reading);
            // The old window ended at the reset it announced, not when a reading showed it: an attempt sent in
            // between belongs to the new window. A reset that came early is bounded by the reading that saw it.
            fit.reset_at = Some(last.reset.map_or(at, |announced| announced.min(at)));
            return fit.report(account_id, window);
        }
        if fit
            .run
            .since
            .is_some_and(|since| at - since > window_ms(window))
        {
            fit.restart(reading);
            return fit.report(account_id, window);
        }
        if at - last.at < MIN_INTERVAL_MS {
            return None;
        }
        // A smaller fall is one value rounded two ways: the run keeps the level it had and carries on.
        let reading = Reading {
            utilization: utilization.max(last.utilization),
            ..reading
        };
        let rise = reading.utilization - last.utilization;
        fit.last = Some(reading);
        fit.run.delta += rise;
        let booked = fit.booked().weighted;
        if fit.run.spanned {
            // Flat or not: a run holding a straddling stream's charge cannot fold on it.
            fit.restart(reading);
        } else if booked <= 0.0 {
            if !open {
                if fit.run.delta > 0.0 {
                    // Spend the router did not send: nothing booked and nothing that could still land.
                    fit.restart(reading);
                } else {
                    // Idle: nothing carried, so the run starts over here and its length counts from now.
                    fit.run.since = Some(at);
                }
            }
        } else if !open && booked.is_finite() && fit.run.delta + SLACK >= FOLD_DELTA {
            fit.fold(reading);
        }
        fit.report(account_id, window)
    }

    fn close(&mut self, account_id: &str, request_id: &str, attempt: u32) -> Option<i64> {
        let attempts = self.open.get_mut(account_id)?;
        let went_out = attempts.remove(&(request_id.to_string(), attempt));
        if attempts.is_empty() {
            self.open.remove(account_id);
        }
        went_out
    }
}
