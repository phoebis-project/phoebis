//! The account's paid term: the operator's cancelled flag and saved end, the trial end the profile carries, and
//! the derivation the account form prefills from.

use phoebis_store::SubscriptionTerm;
use jiff::tz::TimeZone;
use jiff::{Span, Timestamp};

/// The end an engine rule reads: the stored end while the account is cancelled, else a trial end still ahead.
pub fn end_of(term: &SubscriptionTerm, now: i64) -> Option<i64> {
    if term.cancelled {
        return term.ends_at;
    }
    term.trial_ends_at.filter(|&end| end > now)
}

/// The end the account form prefills: the first instant after `now` on the anchor's day of the month and time
/// of day (UTC), the anchor the later of the start and a trial end that has passed, clamped to the last day of
/// a shorter month. `None` without a start.
pub fn derived_end(term: &SubscriptionTerm, now: i64) -> Option<i64> {
    let since = term.since?;
    let anchor_ms = match term.trial_ends_at {
        Some(trial) if trial <= now => since.max(trial),
        _ => since,
    };
    let anchor = Timestamp::from_millisecond(anchor_ms)
        .ok()?
        .to_zoned(TimeZone::UTC);
    let today = Timestamp::from_millisecond(now)
        .ok()?
        .to_zoned(TimeZone::UTC);
    // The month difference is a starting guess close enough that the loop below settles within two checks: a
    // reference date is required because a month's length varies, and `checked_add`'s constrain semantics clamp
    // the day to the target month's last one.
    let mut months = (i32::from(today.year()) - i32::from(anchor.year())) * 12
        + (i32::from(today.month()) - i32::from(anchor.month()));
    loop {
        let candidate = anchor.checked_add(Span::new().months(months)).ok()?;
        let candidate_ms = candidate.timestamp().as_millisecond();
        if candidate_ms > now {
            return Some(candidate_ms);
        }
        months += 1;
    }
}

/// Whether a fresh session should place on the account only as a last resort: inside `wind_down_minutes` of its
/// end, cancelled or trial alike.
pub fn winding_down(term: &SubscriptionTerm, now: i64, wind_down_minutes: u32) -> bool {
    end_of(term, now).is_some_and(|end| now >= end - i64::from(wind_down_minutes) * 60_000)
}

/// Cancelled and the stored end passed: the account is the obstacle `subscription_ended`.
pub fn ended(term: &SubscriptionTerm, now: i64) -> bool {
    term.cancelled && term.ends_at.is_some_and(|end| end <= now)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term(
        since: Option<i64>,
        trial_ends_at: Option<i64>,
        cancelled: bool,
        ends_at: Option<i64>,
    ) -> SubscriptionTerm {
        SubscriptionTerm {
            account_id: "a".into(),
            since,
            trial_ends_at,
            cancelled,
            ends_at,
        }
    }

    fn ms(y: i16, m: i8, d: i8, h: i8, min: i8) -> i64 {
        jiff::civil::date(y, m, d)
            .at(h, min, 0, 0)
            .to_zoned(TimeZone::UTC)
            .unwrap()
            .timestamp()
            .as_millisecond()
    }

    #[test]
    fn the_derived_end_falls_on_the_anchors_day_of_the_month_and_clamps_to_a_shorter_month() {
        let start = ms(2026, 1, 31, 9, 15);
        let t = term(Some(start), None, false, None);
        assert_eq!(
            derived_end(&t, ms(2026, 2, 14, 0, 0)),
            Some(ms(2026, 2, 28, 9, 15)),
            "February has no 31st, so the anchor clamps to its last day"
        );
        assert_eq!(
            derived_end(&t, ms(2026, 3, 14, 0, 0)),
            Some(ms(2026, 3, 31, 9, 15)),
            "March has a 31st, so the anchor lands on it exactly"
        );
        // A trial end that has passed becomes the new anchor day.
        let trial = ms(2026, 2, 10, 12, 0);
        let with_trial = term(Some(start), Some(trial), false, None);
        assert_eq!(
            derived_end(&with_trial, ms(2026, 2, 20, 0, 0)),
            Some(ms(2026, 3, 10, 12, 0)),
            "the passed trial end anchors the derivation, not the start"
        );
    }

    #[test]
    fn the_end_is_the_stored_end_while_cancelled_else_a_trial_end_ahead() {
        let now = ms(2026, 2, 1, 0, 0);
        let end = ms(2026, 2, 15, 0, 0);
        assert_eq!(end_of(&term(None, None, true, Some(end)), now), Some(end));
        assert_eq!(end_of(&term(None, None, true, None), now), None);
        let trial_ahead = ms(2026, 2, 10, 0, 0);
        assert_eq!(
            end_of(&term(None, Some(trial_ahead), false, None), now),
            Some(trial_ahead)
        );
        let trial_passed = ms(2026, 1, 20, 0, 0);
        assert_eq!(
            end_of(&term(None, Some(trial_passed), false, None), now),
            None
        );
    }

    #[test]
    fn winding_down_and_ended_read_the_same_terms() {
        let now = ms(2026, 2, 1, 0, 0);
        let end = now + 20 * 60_000;
        let cancelled = term(None, None, true, Some(end));
        assert!(
            winding_down(&cancelled, now, 30),
            "20 minutes out is inside a 30-minute window"
        );
        assert!(
            !winding_down(&cancelled, now, 10),
            "but not inside a 10-minute one"
        );
        assert!(!ended(&cancelled, now), "the end has not passed yet");
        let passed = term(None, None, true, Some(now - 1));
        assert!(ended(&passed, now));
        assert!(
            !ended(&term(None, None, false, None), now),
            "never cancelled, never ended"
        );
    }
}
