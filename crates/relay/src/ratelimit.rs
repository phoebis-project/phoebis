//! The unified rate-limit headers, the per-bucket api_key headers, and `Retry-After`, read from a response head
//! before any byte reaches the caller.

use std::str::FromStr;

use phoebis_routing::{UtilizationSample, WindowReading, FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE};
use phoebis_store::AccountKind;
use phoebis_transport::response::ResponseHead;

const PREFIX: &str = "anthropic-ratelimit-unified-";
const BUCKET_PREFIX: &str = "anthropic-ratelimit-";
const FAST_PREFIX: &str = "anthropic-fast-";

/// The fast lane's two buckets, per organization and shared by every fast-capable model, so the names carry no
/// model suffix and neither `worst_bucket`'s prefixes nor the plan-window persistence match them.
pub const FAST_INPUT: &str = "fast_input";
pub const FAST_OUTPUT: &str = "fast_output";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct UnifiedHeaders {
    pub status: Option<String>,
    pub reset: Option<i64>,
    pub representative_claim: Option<String>,
    pub five_hour_status: Option<String>,
    pub five_hour_reset: Option<i64>,
    pub five_hour_utilization: Option<f64>,
    pub seven_day_status: Option<String>,
    pub seven_day_reset: Option<i64>,
    pub seven_day_utilization: Option<f64>,
    pub seven_day_oi_status: Option<String>,
    pub seven_day_oi_reset: Option<i64>,
    pub seven_day_oi_utilization: Option<f64>,
    pub overage_in_use: Option<bool>,
    pub overage_reset: Option<i64>,
    pub overage_disabled_reason: Option<String>,
}

impl UnifiedHeaders {
    pub fn parse(head: &ResponseHead) -> Option<UnifiedHeaders> {
        if !head
            .headers
            .iter()
            .any(|(n, _)| n.to_ascii_lowercase().starts_with(PREFIX))
        {
            return None;
        }
        let text = |suffix: &str| {
            head.get(&format!("{PREFIX}{suffix}"))
                .map(|v| v.trim().to_string())
        };
        let secs = |suffix: &str| text(suffix).as_deref().and_then(epoch_millis);
        let float = |suffix: &str| {
            text(suffix)
                .and_then(|v| v.parse::<f64>().ok())
                .filter(|f| f.is_finite())
        };
        Some(UnifiedHeaders {
            status: text("status"),
            reset: secs("reset"),
            representative_claim: text("representative-claim"),
            five_hour_status: text("5h-status"),
            five_hour_reset: secs("5h-reset"),
            five_hour_utilization: float("5h-utilization"),
            seven_day_status: text("7d-status"),
            seven_day_reset: secs("7d-reset"),
            seven_day_utilization: float("7d-utilization"),
            seven_day_oi_status: text("7d_oi-status"),
            seven_day_oi_reset: secs("7d_oi-reset"),
            seven_day_oi_utilization: float("7d_oi-utilization"),
            overage_in_use: text("overage-in-use").map(|v| v.eq_ignore_ascii_case("true")),
            overage_reset: secs("overage-reset"),
            overage_disabled_reason: text("overage-disabled-reason"),
        })
    }

    /// Whether the shared window headers are present, which is what makes an absent overage flag mean "off".
    pub fn present(&self) -> bool {
        self.status.is_some()
            || self.five_hour_status.is_some()
            || self.five_hour_reset.is_some()
            || self.seven_day_status.is_some()
            || self.seven_day_reset.is_some()
            || self.five_hour_utilization.is_some()
            || self.seven_day_utilization.is_some()
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bucket {
    pub limit: u64,
    pub remaining: u64,
    pub reset: Option<i64>,
}

/// `anthropic-ratelimit-{requests,input-tokens,output-tokens}-{limit,remaining,reset}`. A family is `None` when
/// its `limit` does not parse to a nonzero value or its `remaining` does not parse: a malformed header must not
/// move a pin, so a family it cannot trust is dropped rather than read as some particular utilization. The
/// whole answer is `None` only once every family has dropped.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BucketHeaders {
    pub requests: Option<Bucket>,
    pub input_tokens: Option<Bucket>,
    pub output_tokens: Option<Bucket>,
}

impl BucketHeaders {
    pub fn parse(head: &ResponseHead) -> Option<BucketHeaders> {
        let bucket =
            |family: &str| bucket_family(head, &format!("{BUCKET_PREFIX}{family}"), rfc3339_millis);
        let headers = BucketHeaders {
            requests: bucket("requests"),
            input_tokens: bucket("input-tokens"),
            output_tokens: bucket("output-tokens"),
        };
        (headers.requests.is_some()
            || headers.input_tokens.is_some()
            || headers.output_tokens.is_some())
        .then_some(headers)
    }
}

/// `anthropic-fast-{input,output}-tokens-{limit,remaining,reset}` of a fast request's answer. A family is dropped
/// as a `BucketHeaders` family is, when its `limit` or `remaining` does not parse, and a reset that does not parse
/// reads as absent. The reset reads epoch seconds or RFC 3339, whichever the header carries, since no capture of a
/// live fast answer settles the format yet.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FastHeaders {
    pub input: Option<Bucket>,
    pub output: Option<Bucket>,
}

impl FastHeaders {
    pub fn parse(head: &ResponseHead) -> Option<FastHeaders> {
        let bucket = |family: &str| {
            bucket_family(head, &format!("{FAST_PREFIX}{family}"), |v| {
                epoch_millis(v).or_else(|| rfc3339_millis(v))
            })
        };
        let headers = FastHeaders {
            input: bucket("input-tokens"),
            output: bucket("output-tokens"),
        };
        (headers.input.is_some() || headers.output.is_some()).then_some(headers)
    }
}

/// One family of three headers, `<name>-limit`, `<name>-remaining` and `<name>-reset`: `None` when the limit does
/// not parse to a nonzero value or the remaining does not parse, since a malformed header must not move a pin; a
/// reset `reset_of` cannot read is absent rather than refusing the family.
fn bucket_family(
    head: &ResponseHead,
    name: &str,
    reset_of: fn(&str) -> Option<i64>,
) -> Option<Bucket> {
    let limit = head
        .get(&format!("{name}-limit"))?
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|&limit| limit != 0)?;
    let remaining = head
        .get(&format!("{name}-remaining"))?
        .trim()
        .parse::<u64>()
        .ok()?;
    let reset = head.get(&format!("{name}-reset")).and_then(reset_of);
    Some(Bucket {
        limit,
        remaining,
        reset,
    })
}

/// A reset in epoch seconds, as the unified family writes it, in milliseconds.
fn epoch_millis(value: &str) -> Option<i64> {
    value
        .trim()
        .parse::<i64>()
        .ok()
        .map(|s| s.saturating_mul(1_000))
}

/// A reset in RFC 3339, as the per-bucket family writes it, in milliseconds.
fn rfc3339_millis(value: &str) -> Option<i64> {
    jiff::Timestamp::from_str(value.trim())
        .ok()
        .map(|t| t.as_millisecond())
}

/// The selection sample a head implies: the unified windows for an OAuth account, the buckets under `rpm:`,
/// `itpm:`, `otpm:` plus the canonical model for an api_key account, and for either kind the fast lane's two
/// buckets under `FAST_INPUT` and `FAST_OUTPUT`; `None` when the head carries none of them.
pub fn head_sample(
    head: &ResponseHead,
    kind: AccountKind,
    canonical_model: &str,
    observed_at: i64,
) -> Option<UtilizationSample> {
    let mut sample = UtilizationSample {
        observed_at,
        ..Default::default()
    };
    match kind {
        AccountKind::Oauth => {
            if let Some(unified) = UnifiedHeaders::parse(head) {
                if let Some(utilization) = unified.five_hour_utilization {
                    sample = sample.with(
                        FIVE_HOUR,
                        WindowReading {
                            utilization,
                            reset: unified.five_hour_reset,
                            observed_at,
                        },
                    );
                }
                if let Some(utilization) = unified.seven_day_utilization {
                    sample = sample.with(
                        SEVEN_DAY,
                        WindowReading {
                            utilization,
                            reset: unified.seven_day_reset,
                            observed_at,
                        },
                    );
                }
                // Unlike the shared windows, the overage-included 7-day reading carries no status of its own to
                // gate on, only the utilization header itself.
                if let Some(utilization) = unified.seven_day_oi_utilization {
                    sample = sample.with(
                        SEVEN_DAY_FABLE,
                        WindowReading {
                            utilization,
                            reset: unified.seven_day_oi_reset,
                            observed_at,
                        },
                    );
                }
            }
        }
        AccountKind::ApiKey => {
            if let Some(buckets) = BucketHeaders::parse(head) {
                for (prefix, bucket) in [
                    ("itpm", buckets.input_tokens),
                    ("otpm", buckets.output_tokens),
                    ("rpm", buckets.requests),
                ] {
                    if let Some(b) = bucket {
                        sample = sample.with(
                            &format!("{prefix}:{canonical_model}"),
                            bucket_reading(b, observed_at),
                        );
                    }
                }
            }
        }
    }
    if let Some(fast) = FastHeaders::parse(head) {
        for (window, bucket) in [(FAST_INPUT, fast.input), (FAST_OUTPUT, fast.output)] {
            if let Some(b) = bucket {
                sample = sample.with(window, bucket_reading(b, observed_at));
            }
        }
    }
    (!sample.windows.is_empty()).then_some(sample)
}

/// A bucket's share used. `limit` is never 0 here (the family parse drops it), but `remaining` past `limit` is
/// still a reading the provider could send; clamped rather than read negative.
fn bucket_reading(bucket: Bucket, observed_at: i64) -> WindowReading {
    WindowReading {
        utilization: (1.0 - (bucket.remaining as f64 / bucket.limit as f64)).clamp(0.0, 1.0),
        reset: bucket.reset,
        observed_at,
    }
}

pub fn upstream_date(head: &ResponseHead) -> Option<i64> {
    jiff::fmt::rfc2822::DateTimeParser::new()
        .parse_timestamp(head.get("date")?.trim())
        .ok()
        .map(|t| t.as_millisecond())
}

pub fn retry_after(head: &ResponseHead, now: i64) -> Option<i64> {
    let value = head.get("retry-after")?.trim();
    if let Ok(seconds) = value.parse::<f64>() {
        if !seconds.is_finite() || seconds < 0.0 {
            return None;
        }
        return Some((seconds.ceil() as i64).saturating_mul(1_000));
    }
    let at = jiff::fmt::rfc2822::DateTimeParser::new()
        .parse_timestamp(value)
        .ok()?
        .as_millisecond();
    let base = upstream_date(head).unwrap_or(now);
    let delay = at.saturating_sub(base);
    (delay >= 0).then_some(delay)
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoebis_transport::response::ResponseHead;

    fn head(status: u16, headers: &[(&str, &str)]) -> ResponseHead {
        ResponseHead {
            status,
            headers: headers
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[test]
    fn retry_after_reads_seconds_fractions_and_dates_and_refuses_junk() {
        let now = 1_700_000_000_000;
        assert_eq!(
            retry_after(&head(429, &[("Retry-After", "7")]), now),
            Some(7_000)
        );
        assert_eq!(
            retry_after(&head(429, &[("retry-after", "0.2")]), now),
            Some(1_000),
            "fractions round up"
        );
        assert_eq!(
            retry_after(
                &head(
                    429,
                    &[
                        ("RETRY-AFTER", "Wed, 21 Oct 2015 07:28:00 GMT"),
                        ("Date", "Wed, 21 Oct 2015 07:27:30 GMT")
                    ]
                ),
                now
            ),
            Some(30_000),
            "a date is read against the upstream clock"
        );
        assert_eq!(retry_after(&head(429, &[("retry-after", "-3")]), now), None);
        assert_eq!(
            retry_after(&head(429, &[("retry-after", "soon")]), now),
            None
        );
        assert_eq!(
            retry_after(&head(429, &[("retry-after", "inf")]), now),
            None
        );
        assert_eq!(retry_after(&head(429, &[]), now), None);
        assert_eq!(
            retry_after(&head(429, &[("retry-after", "100000")]), now),
            Some(100_000_000),
            "a long delay is never shortened"
        );
    }

    #[test]
    fn unified_headers_parse_every_window_and_absence_is_none() {
        let h = head(
            429,
            &[
                ("anthropic-ratelimit-unified-status", "rejected"),
                ("anthropic-ratelimit-unified-reset", "1700000100"),
                (
                    "anthropic-ratelimit-unified-representative-claim",
                    "seven_day_opus",
                ),
                ("anthropic-ratelimit-unified-5h-status", "allowed"),
                ("anthropic-ratelimit-unified-5h-reset", "1700000300"),
                ("anthropic-ratelimit-unified-5h-utilization", "0.42"),
                ("anthropic-ratelimit-unified-7d-status", "allowed"),
                ("anthropic-ratelimit-unified-7d-reset", "1700000400"),
                ("anthropic-ratelimit-unified-7d-utilization", "0.13"),
                ("anthropic-ratelimit-unified-7d_oi-status", "rejected"),
                ("anthropic-ratelimit-unified-7d_oi-reset", "1700000200"),
                ("anthropic-ratelimit-unified-7d_oi-utilization", "0.91"),
                ("anthropic-ratelimit-unified-overage-in-use", "true"),
                ("anthropic-ratelimit-unified-overage-reset", "1700000500"),
                (
                    "anthropic-ratelimit-unified-overage-disabled-reason",
                    "no_payment_method",
                ),
            ],
        );
        let u = UnifiedHeaders::parse(&h).unwrap();
        assert_eq!(
            (
                u.status.as_deref(),
                u.reset,
                u.representative_claim.as_deref()
            ),
            (
                Some("rejected"),
                Some(1_700_000_100_000),
                Some("seven_day_opus")
            )
        );
        assert_eq!(
            (
                u.five_hour_status.as_deref(),
                u.five_hour_reset,
                u.five_hour_utilization
            ),
            (Some("allowed"), Some(1_700_000_300_000), Some(0.42))
        );
        assert_eq!(
            (
                u.seven_day_status.as_deref(),
                u.seven_day_reset,
                u.seven_day_utilization
            ),
            (Some("allowed"), Some(1_700_000_400_000), Some(0.13))
        );
        assert_eq!(
            (
                u.seven_day_oi_status.as_deref(),
                u.seven_day_oi_reset,
                u.seven_day_oi_utilization
            ),
            (Some("rejected"), Some(1_700_000_200_000), Some(0.91))
        );
        assert_eq!(
            (
                u.overage_in_use,
                u.overage_reset,
                u.overage_disabled_reason.as_deref()
            ),
            (
                Some(true),
                Some(1_700_000_500_000),
                Some("no_payment_method")
            )
        );
        assert!(u.present());
        assert!(UnifiedHeaders::parse(&head(529, &[("retry-after", "1")])).is_none());
    }

    #[test]
    fn bucket_headers_parse_each_family_and_absence_is_none() {
        let h = head(
            200,
            &[
                ("anthropic-ratelimit-requests-limit", "50"),
                ("anthropic-ratelimit-requests-remaining", "10"),
                ("anthropic-ratelimit-requests-reset", "2026-09-25T00:00:00Z"),
                ("anthropic-ratelimit-input-tokens-limit", "100000"),
                ("anthropic-ratelimit-input-tokens-remaining", "25000"),
                (
                    "anthropic-ratelimit-input-tokens-reset",
                    "2026-09-25T00:05:00Z",
                ),
                ("anthropic-ratelimit-output-tokens-limit", "20000"),
                ("anthropic-ratelimit-output-tokens-remaining", "20000"),
                (
                    "anthropic-ratelimit-output-tokens-reset",
                    "2026-09-25T00:10:00Z",
                ),
            ],
        );
        let ts = |s: &str| jiff::Timestamp::from_str(s).unwrap().as_millisecond();
        let b = BucketHeaders::parse(&h).unwrap();
        assert_eq!(
            b.requests,
            Some(Bucket {
                limit: 50,
                remaining: 10,
                reset: Some(ts("2026-09-25T00:00:00Z"))
            })
        );
        assert_eq!(
            b.input_tokens,
            Some(Bucket {
                limit: 100_000,
                remaining: 25_000,
                reset: Some(ts("2026-09-25T00:05:00Z"))
            })
        );
        assert_eq!(
            b.output_tokens,
            Some(Bucket {
                limit: 20_000,
                remaining: 20_000,
                reset: Some(ts("2026-09-25T00:10:00Z"))
            })
        );
        assert!(BucketHeaders::parse(&head(200, &[])).is_none());
        // A family with no reset still parses; an unparseable reset reads as absent rather than refusing the
        // family.
        let no_reset = head(
            200,
            &[
                ("anthropic-ratelimit-requests-limit", "50"),
                ("anthropic-ratelimit-requests-remaining", "10"),
                ("anthropic-ratelimit-requests-reset", "not a timestamp"),
            ],
        );
        assert_eq!(
            BucketHeaders::parse(&no_reset).unwrap().requests,
            Some(Bucket {
                limit: 50,
                remaining: 10,
                reset: None
            })
        );
        // A missing or unparseable `remaining` drops the family: a malformed header must not move a pin by
        // reading as some particular utilization. A sibling family (input-tokens) keeps the answer `Some` so
        // the dropped one is checked directly rather than through the whole-struct `None` it would fall to alone.
        let sibling = [
            ("anthropic-ratelimit-input-tokens-limit", "1"),
            ("anthropic-ratelimit-input-tokens-remaining", "1"),
        ];
        let no_remaining = head(
            200,
            &[
                ("anthropic-ratelimit-requests-limit", "50"),
                sibling[0],
                sibling[1],
            ],
        );
        assert_eq!(BucketHeaders::parse(&no_remaining).unwrap().requests, None);
        let bad_remaining = head(
            200,
            &[
                ("anthropic-ratelimit-requests-limit", "50"),
                ("anthropic-ratelimit-requests-remaining", "not a number"),
                sibling[0],
                sibling[1],
            ],
        );
        assert_eq!(BucketHeaders::parse(&bad_remaining).unwrap().requests, None);
        // A `limit` of 0 drops the family too, rather than reading as the opposite convention's 0.0.
        let zero_limit = head(
            200,
            &[
                ("anthropic-ratelimit-requests-limit", "0"),
                ("anthropic-ratelimit-requests-remaining", "0"),
                sibling[0],
                sibling[1],
            ],
        );
        assert_eq!(BucketHeaders::parse(&zero_limit).unwrap().requests, None);
    }

    #[test]
    fn the_six_fast_headers_read_into_two_buckets_and_a_reset_reads_epoch_seconds_or_rfc_3339() {
        let h = head(
            429,
            &[
                ("anthropic-fast-input-tokens-limit", "400000"),
                ("anthropic-fast-input-tokens-remaining", "0"),
                ("anthropic-fast-input-tokens-reset", "1700000060"),
                ("anthropic-fast-output-tokens-limit", "80000"),
                ("anthropic-fast-output-tokens-remaining", "12000"),
                ("anthropic-fast-output-tokens-reset", "2023-11-14T22:14:50Z"),
            ],
        );
        let fast = FastHeaders::parse(&h).unwrap();
        assert_eq!(
            fast.input,
            Some(Bucket {
                limit: 400_000,
                remaining: 0,
                reset: Some(1_700_000_060_000)
            }),
            "epoch seconds"
        );
        assert_eq!(
            fast.output,
            Some(Bucket {
                limit: 80_000,
                remaining: 12_000,
                reset: Some(1_700_000_090_000)
            }),
            "RFC 3339"
        );
        assert!(FastHeaders::parse(&head(429, &[("retry-after", "3")])).is_none());
    }

    #[test]
    fn a_fast_family_with_one_malformed_value_is_dropped_and_the_other_kept() {
        let output = [
            ("anthropic-fast-output-tokens-limit", "80000"),
            ("anthropic-fast-output-tokens-remaining", "12000"),
            ("anthropic-fast-output-tokens-reset", "1700000090"),
        ];
        let h = head(
            429,
            &[
                ("anthropic-fast-input-tokens-limit", "400000"),
                ("anthropic-fast-input-tokens-remaining", "none"),
                ("anthropic-fast-input-tokens-reset", "1700000060"),
                output[0],
                output[1],
                output[2],
            ],
        );
        let fast = FastHeaders::parse(&h).unwrap();
        assert_eq!(fast.input, None, "a remaining that does not parse");
        assert_eq!(
            fast.output.map(|b| (b.limit, b.remaining, b.reset)),
            Some((80_000, 12_000, Some(1_700_000_090_000)))
        );
        let no_family_left = head(
            429,
            &[
                ("anthropic-fast-input-tokens-limit", "x"),
                ("anthropic-fast-input-tokens-remaining", "0"),
            ],
        );
        assert!(FastHeaders::parse(&no_family_left).is_none());
    }

    #[test]
    fn an_api_key_head_samples_its_buckets_by_model_and_an_oauth_head_its_windows() {
        let api_key_head = head(
            200,
            &[
                ("anthropic-ratelimit-requests-limit", "50"),
                ("anthropic-ratelimit-requests-remaining", "40"),
                ("anthropic-ratelimit-input-tokens-limit", "100000"),
                ("anthropic-ratelimit-input-tokens-remaining", "75000"),
                ("anthropic-ratelimit-output-tokens-limit", "20000"),
                ("anthropic-ratelimit-output-tokens-remaining", "5000"),
            ],
        );
        let sample = head_sample(
            &api_key_head,
            AccountKind::ApiKey,
            "claude-sonnet-4-5-20250929",
            1_700_000_000_000,
        )
        .unwrap();
        let reading = |window: &str| sample.reading(window).map(|r| r.utilization);
        let close = |a: Option<f64>, b: f64| (a.unwrap() - b).abs() < 1e-9;
        assert!(close(reading("rpm:claude-sonnet-4-5-20250929"), 0.2));
        assert!(close(reading("itpm:claude-sonnet-4-5-20250929"), 0.25));
        assert!(close(reading("otpm:claude-sonnet-4-5-20250929"), 0.75));

        let oauth_head = head(
            200,
            &[
                ("anthropic-ratelimit-unified-5h-status", "allowed"),
                ("anthropic-ratelimit-unified-5h-utilization", "0.42"),
                ("anthropic-ratelimit-unified-5h-reset", "1700003600"),
                ("anthropic-ratelimit-unified-7d-status", "allowed"),
                ("anthropic-ratelimit-unified-7d-utilization", "0.13"),
                ("anthropic-ratelimit-unified-7d_oi-utilization", "0.05"),
                ("anthropic-ratelimit-unified-7d_oi-reset", "1700007200"),
            ],
        );
        let sample = head_sample(
            &oauth_head,
            AccountKind::Oauth,
            "claude-opus-5",
            1_700_000_000_000,
        )
        .unwrap();
        assert_eq!(sample.reading(FIVE_HOUR).map(|r| r.utilization), Some(0.42));
        assert_eq!(sample.reading(SEVEN_DAY).map(|r| r.utilization), Some(0.13));
        assert_eq!(
            sample.reading(SEVEN_DAY_FABLE).map(|r| r.utilization),
            Some(0.05)
        );

        assert!(head_sample(&head(200, &[]), AccountKind::Oauth, "m", 0).is_none());
        assert!(head_sample(&head(200, &[]), AccountKind::ApiKey, "m", 0).is_none());

        // Each reading is keyed on its own model, which is what lets the router fold a second model's sample
        // in without touching the first's (proven end to end in head_samples.rs).
        let second = head_sample(
            &head(
                200,
                &[
                    ("anthropic-ratelimit-requests-limit", "10"),
                    ("anthropic-ratelimit-requests-remaining", "0"),
                ],
            ),
            AccountKind::ApiKey,
            "claude-opus-5",
            1_700_000_001_000,
        )
        .unwrap();
        assert_eq!(
            second.reading("rpm:claude-opus-5").map(|r| r.utilization),
            Some(1.0)
        );
        assert!(second.reading("rpm:claude-sonnet-4-5-20250929").is_none());
    }

    #[test]
    fn a_non_finite_seven_day_oi_utilization_never_enters_the_sample() {
        let h = head(
            200,
            &[
                ("anthropic-ratelimit-unified-7d_oi-status", "rejected"),
                ("anthropic-ratelimit-unified-7d_oi-utilization", "nan"),
            ],
        );
        assert_eq!(
            UnifiedHeaders::parse(&h).unwrap().seven_day_oi_utilization,
            None
        );
        assert!(head_sample(&h, AccountKind::Oauth, "claude-opus-5", 0).is_none());
    }

    #[test]
    fn a_remaining_past_its_limit_clamps_the_utilization_rather_than_reading_negative() {
        let h = head(
            200,
            &[
                ("anthropic-ratelimit-requests-limit", "50"),
                ("anthropic-ratelimit-requests-remaining", "80"),
            ],
        );
        let sample = head_sample(&h, AccountKind::ApiKey, "claude-opus-5", 0).unwrap();
        assert_eq!(
            sample.reading("rpm:claude-opus-5").map(|r| r.utilization),
            Some(0.0)
        );
    }
}
