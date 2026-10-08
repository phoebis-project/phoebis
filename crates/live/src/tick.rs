//! The metric tile: per-second counters over the last minute, each labeled with how it was obtained.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

const SERIES: usize = 60;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LiveMetric {
    pub quality: String,
    pub series: Vec<Option<f64>>,
    pub value: Option<f64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LiveMetrics {
    pub active_streams: LiveMetric,
    pub cost_per_minute: LiveMetric,
    pub error_rate: LiveMetric,
    pub output_tps: LiveMetric,
    pub requests_per_second: LiveMetric,
    pub ttfb_p50: LiveMetric,
    pub ttfb_p95: LiveMetric,
    pub ttfb_p99: LiveMetric,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LiveTick {
    pub at: i64,
    pub metrics: LiveMetrics,
}

struct Completion {
    at: i64,
    ok: bool,
    ttfb_ms: Option<i64>,
    cost_usd: f64,
}

/// What every metric read at one second. Each series is one column of the ring of these, so a metric whose
/// value is a trailing-window aggregate still has one point per tick.
#[derive(Clone, Copy, Debug)]
struct Reading {
    active_streams: Option<f64>,
    cost_per_minute: Option<f64>,
    error_rate: Option<f64>,
    output_tps: Option<f64>,
    requests_per_second: Option<f64>,
    ttfb_p50: Option<f64>,
    ttfb_p95: Option<f64>,
    ttfb_p99: Option<f64>,
}

pub(crate) struct Tile {
    completions: VecDeque<Completion>,
    /// (at, output tokens added) over the last minute.
    output: VecDeque<(i64, u64)>,
    /// One reading per second for the last minute, keyed by the second; a second no tick ran in has no entry.
    history: VecDeque<(i64, Reading)>,
}

impl Tile {
    pub(crate) fn new() -> Tile {
        Tile {
            completions: VecDeque::new(),
            output: VecDeque::new(),
            history: VecDeque::new(),
        }
    }

    pub(crate) fn completed(&mut self, at: i64, ok: bool, ttfb_ms: Option<i64>, cost_usd: f64) {
        self.completions.push_back(Completion {
            at,
            ok,
            ttfb_ms,
            cost_usd,
        });
    }

    /// `added` is what one chunk raised its request's output count by; the tile sums these, so concurrent
    /// streams add up instead of overwriting one another's running counter.
    pub(crate) fn output_tokens(&mut self, at: i64, added: u64) {
        self.output.push_back((at, added));
    }

    fn prune(&mut self, now: i64) {
        while self
            .completions
            .front()
            .is_some_and(|c| now - c.at > 60_000)
        {
            self.completions.pop_front();
        }
        while self.output.front().is_some_and(|(at, _)| now - at > 60_000) {
            self.output.pop_front();
        }
        while self
            .history
            .front()
            .is_some_and(|(s, _)| now / 1_000 - s >= SERIES as i64)
        {
            self.history.pop_front();
        }
    }

    pub(crate) fn tick(&mut self, now: i64, active_streams: usize) -> LiveTick {
        self.prune(now);
        let second = now / 1_000;
        let reading = self.reading(now, active_streams);
        // The latest reading in a second is the one its series carries.
        if self.history.back().is_some_and(|(s, _)| *s == second) {
            self.history.pop_back();
        }
        self.history.push_back((second, reading));
        self.current(now, active_streams)
    }

    /// Every metric over its trailing window as of `now`: the last second for the request rate, the last five
    /// seconds for output tokens, the last minute for the rest.
    fn reading(&self, now: i64, active_streams: usize) -> Reading {
        let recent: Vec<&Completion> = self
            .completions
            .iter()
            .filter(|c| now - c.at <= 60_000)
            .collect();
        let mut ttfbs: Vec<f64> = recent
            .iter()
            .filter_map(|c| c.ttfb_ms.map(|t| t as f64))
            .collect();
        ttfbs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let percentile = |p: f64| -> Option<f64> {
            if ttfbs.is_empty() {
                None
            } else {
                Some(ttfbs[(((p * ttfbs.len() as f64).ceil() as usize).max(1)) - 1])
            }
        };
        Reading {
            active_streams: Some(active_streams as f64),
            cost_per_minute: Some(recent.iter().map(|c| c.cost_usd).sum::<f64>()),
            error_rate: if recent.is_empty() {
                None
            } else {
                Some(recent.iter().filter(|c| !c.ok).count() as f64 / recent.len() as f64)
            },
            output_tps: Some(
                self.output
                    .iter()
                    .filter(|(at, _)| now - at <= 5_000)
                    .map(|(_, n)| *n)
                    .sum::<u64>() as f64
                    / 5.0,
            ),
            requests_per_second: Some(
                self.completions
                    .iter()
                    .filter(|c| now - c.at < 1_000)
                    .count() as f64,
            ),
            ttfb_p50: percentile(0.5),
            ttfb_p95: percentile(0.95),
            ttfb_p99: percentile(0.99),
        }
    }

    pub(crate) fn current(&self, now: i64, active_streams: usize) -> LiveTick {
        let second = now / 1_000;
        let value = self.reading(now, active_streams);
        let series = |pick: fn(&Reading) -> Option<f64>| -> Vec<Option<f64>> {
            (0..SERIES)
                .map(|i| {
                    let s = second - (SERIES as i64 - 1 - i as i64);
                    self.history
                        .iter()
                        .find(|(at, _)| *at == s)
                        .and_then(|(_, r)| pick(r))
                })
                .collect()
        };
        let metric = |quality: &str, series: Vec<Option<f64>>, value: Option<f64>| LiveMetric {
            quality: quality.into(),
            series,
            value,
        };
        LiveTick {
            at: now,
            metrics: LiveMetrics {
                active_streams: metric(
                    "measured",
                    series(|r| r.active_streams),
                    value.active_streams,
                ),
                cost_per_minute: metric(
                    "estimated",
                    series(|r| r.cost_per_minute),
                    value.cost_per_minute,
                ),
                error_rate: metric("derived", series(|r| r.error_rate), value.error_rate),
                output_tps: metric("derived", series(|r| r.output_tps), value.output_tps),
                requests_per_second: metric(
                    "measured",
                    series(|r| r.requests_per_second),
                    value.requests_per_second,
                ),
                ttfb_p50: metric("measured", series(|r| r.ttfb_p50), value.ttfb_p50),
                ttfb_p95: metric("measured", series(|r| r.ttfb_p95), value.ttfb_p95),
                ttfb_p99: metric("measured", series(|r| r.ttfb_p99), value.ttfb_p99),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_metric_keeps_a_per_second_history_and_a_second_without_a_reading_is_a_gap() {
        let mut tile = Tile::new();
        tile.completed(1_000, true, Some(120), 0.01);
        tile.completed(1_500, true, Some(480), 0.02);
        let tick = tile.tick(3_000, 0);
        let last = |m: &LiveMetric| *m.series.last().unwrap();
        assert_eq!(tick.metrics.ttfb_p50.series.len(), SERIES);
        assert_eq!(
            (last(&tick.metrics.ttfb_p50), last(&tick.metrics.ttfb_p99)),
            (Some(120.0), Some(480.0))
        );
        assert_eq!(last(&tick.metrics.error_rate), Some(0.0));
        assert!((last(&tick.metrics.cost_per_minute).unwrap() - 0.03).abs() < 1e-12);
        assert_eq!(
            last(&tick.metrics.requests_per_second),
            Some(0.0),
            "no completion in the last second"
        );
        assert_eq!(
            tick.metrics.ttfb_p50.series[SERIES - 2],
            None,
            "the second before had no tick"
        );
        // A minute on, the completions have aged out of every window: the value and the newest reading are gaps,
        // while the reading taken at second 3 is still the oldest point of the series.
        let later = tile.tick(62_500, 0);
        assert_eq!(
            (
                later.metrics.ttfb_p50.value,
                last(&later.metrics.ttfb_p50),
                last(&later.metrics.error_rate)
            ),
            (None, None, None)
        );
        assert_eq!(later.metrics.ttfb_p50.series[0], Some(120.0));
    }
}
