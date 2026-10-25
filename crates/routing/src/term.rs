//! The account's paid term: the operator's canceled flag and saved end, the trial end the profile carries, and the
//! instant a profile read found no plan.

use phoebis_store::SubscriptionTerm;

/// The end an engine rule reads: the stored end while the account is canceled, else a trial end still ahead, and the
/// plan-ended flag where it comes first, so the rules for an account past its end apply to a flagged one.
pub fn end_of(term: &SubscriptionTerm, now: i64) -> Option<i64> {
    let end = term_end(term, now);
    match (end, term.plan_ended_at) {
        (Some(end), Some(flagged)) => Some(end.min(flagged)),
        (end, flagged) => end.or(flagged),
    }
}

fn term_end(term: &SubscriptionTerm, now: i64) -> Option<i64> {
    if term.canceled {
        return term.ends_at;
    }
    term.trial_ends_at.filter(|&end| end > now)
}

/// Whether a fresh session should place on the account only as a last resort: inside `wind_down_minutes` of its
/// end, canceled or trial alike.
pub fn winding_down(term: &SubscriptionTerm, now: i64, wind_down_minutes: u32) -> bool {
    end_of(term, now).is_some_and(|end| now >= end - i64::from(wind_down_minutes) * 60_000)
}

/// Canceled and the stored end passed, or flagged plan ended at or before `now`: the account is the obstacle
/// `subscription_ended`.
pub fn ended(term: &SubscriptionTerm, now: i64) -> bool {
    (term.canceled && term.ends_at.is_some_and(|end| end <= now))
        || term.plan_ended_at.is_some_and(|at| at <= now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::tz::TimeZone;

    fn term(
        since: Option<i64>,
        trial_ends_at: Option<i64>,
        canceled: bool,
        ends_at: Option<i64>,
    ) -> SubscriptionTerm {
        SubscriptionTerm {
            account_id: "a".into(),
            since,
            trial_ends_at,
            canceled,
            ends_at,
            tier: None,
            usage_based: false,
            plan: None,
            plan_ended_at: None,
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
    fn the_end_is_the_stored_end_while_canceled_else_a_trial_end_ahead() {
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
        let canceled = term(None, None, true, Some(end));
        assert!(
            winding_down(&canceled, now, 30),
            "20 minutes out is inside a 30-minute window"
        );
        assert!(
            !winding_down(&canceled, now, 10),
            "but not inside a 10-minute one"
        );
        assert!(!ended(&canceled, now), "the end has not passed yet");
        let passed = term(None, None, true, Some(now - 1));
        assert!(ended(&passed, now));
        assert!(
            !ended(&term(None, None, false, None), now),
            "never canceled, never ended"
        );
    }

    #[test]
    fn the_plan_ended_flag_reads_as_a_passed_end_and_as_the_end_where_it_comes_first() {
        let now = ms(2026, 2, 1, 0, 0);
        let flagged = |base: SubscriptionTerm, at: i64| SubscriptionTerm {
            plan_ended_at: Some(at),
            ..base
        };
        let running = || term(None, None, false, None);
        assert!(
            ended(&flagged(running(), now - 1), now),
            "a flag before now"
        );
        assert!(ended(&flagged(running(), now), now), "a flag at now");
        assert!(
            !ended(&flagged(running(), now + 1), now),
            "a flag ahead of now is no passed end"
        );
        assert_eq!(
            end_of(&flagged(running(), now - 1), now),
            Some(now - 1),
            "the flag is the end of a term that names none"
        );
        let saved = ms(2026, 2, 15, 0, 0);
        assert_eq!(
            end_of(&flagged(term(None, None, true, Some(saved)), now - 1), now),
            Some(now - 1),
            "a flag before the saved end is the end"
        );
        assert_eq!(
            end_of(
                &flagged(term(None, None, true, Some(now - 10)), now - 1),
                now
            ),
            Some(now - 10),
            "a saved end before the flag stays the end"
        );
        let trial = ms(2026, 2, 10, 0, 0);
        assert_eq!(
            end_of(&flagged(term(None, Some(trial), false, None), now - 1), now),
            Some(now - 1),
            "a flag before a trial end ahead is the end"
        );
    }
}
