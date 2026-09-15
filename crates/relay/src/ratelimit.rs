//! The unified rate-limit headers and `Retry-After`, read from a response head before any byte reaches the
//! caller.

use phoebis_transport::response::ResponseHead;

const PREFIX: &str = "anthropic-ratelimit-unified-";

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
        let secs = |suffix: &str| {
            text(suffix)
                .and_then(|v| v.parse::<i64>().ok())
                .map(|s| s.saturating_mul(1_000))
        };
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
            overage_in_use: text("overage-in-use").map(|v| v.eq_ignore_ascii_case("true")),
            overage_reset: secs("overage-reset"),
            overage_disabled_reason: text("overage-disabled-reason"),
        })
    }

    /// Whether the shared window headers are present, which is what makes an absent overage flag mean "off".
    pub fn present(&self) -> bool {
        self.status.is_some()
            || self.five_hour_status.is_some()
            || self.seven_day_status.is_some()
            || self.five_hour_utilization.is_some()
            || self.seven_day_utilization.is_some()
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
                ("anthropic-ratelimit-unified-5h-utilization", "0.42"),
                ("anthropic-ratelimit-unified-7d_oi-status", "rejected"),
                ("anthropic-ratelimit-unified-7d_oi-reset", "1700000200"),
                ("anthropic-ratelimit-unified-overage-in-use", "true"),
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
            (u.five_hour_status.as_deref(), u.five_hour_utilization),
            (Some("allowed"), Some(0.42))
        );
        assert_eq!(
            (u.seven_day_oi_status.as_deref(), u.seven_day_oi_reset),
            (Some("rejected"), Some(1_700_000_200_000))
        );
        assert_eq!(u.overage_in_use, Some(true));
        assert!(u.present());
        assert!(UnifiedHeaders::parse(&head(529, &[("retry-after", "1")])).is_none());
    }
}
