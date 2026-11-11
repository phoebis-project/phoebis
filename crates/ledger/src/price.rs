//! A row's counts priced in one call: the dollars at the price revision in force at the row's instant, the shares of
//! the account tier's windows and the Pro-week figure at the cost revision in force then. One function answers
//! every figure so the native rule (a plan row in the equivalent, a billed row in dollars) is written once by the
//! caller and never recomputed from the account tier the registry holds now.

use phoebis_bus::Classifier;
use phoebis_routing::{plan_windows, PinUsage, SEVEN_DAY};

use crate::plan::{
    entrypoint_of_client, free_equivalent, free_shares, price_equivalent, price_shares, Funding,
    PlanTables, WindowShares,
};
use crate::pricing::{Band, PriceRevisions, PriceTable, Speed};
use crate::record::{AttemptRecord, RequestRecord};
use crate::rollup::CostSplit;

/// The share reason of a plan row priced at no share by rule: the client's own classifier call.
pub const SHARE_REASON_CLIENT_CLASSIFIER: &str = "client_classifier";

/// Whether a stored classifier word marks a free call: `client`, the client's own classifier call on the account its
/// judged turn ended on, which takes no share of a plan window and the normal dollars elsewhere. A `client_off` call
/// and a row with no word price as any call.
pub(crate) fn is_free_classifier(classifier: Option<&str>) -> bool {
    classifier == Some(Classifier::Client.as_str())
}

/// What prices a row: its model, the cells its speed and band select, who paid, the account tier the row carries
/// (`None` for an api_key account, a profile not yet read, or the rollup's `-`), the entrypoint whose regime prices
/// its shares and the instant that picks the revision and the threshold. `band_at` is the instant the peak band is
/// read at where a reader has a completion (an attempt's), else `at` (a rollup row's hour).
#[derive(Clone, Copy, Debug)]
pub struct RowKey<'a> {
    pub model: &'a str,
    pub speed: Speed,
    pub band: Band,
    pub funding: Funding,
    pub tier: Option<&'a str>,
    /// The request's entrypoint from its `client` label, `None` the base regime.
    pub entrypoint: Option<&'a str>,
    /// The row is a free classifier call, as `is_free_classifier` reads its word.
    pub classifier: bool,
    pub at: i64,
    pub band_at: Option<i64>,
    /// The read's instant, which selects the revision whose anchor row divides the Pro-week figure. The queries, the
    /// rollup's readers and the routes fill it from the clock they price at.
    pub now: i64,
}

impl<'a> RowKey<'a> {
    /// The key one attempt of `request` prices under at `now`: the attempt's tier, funding, speed and band, the
    /// request's `canonical_model`, entrypoint and classifier word. `table` is the price revision in force at
    /// `request.received_at`.
    pub fn of_attempt(
        request: &'a RequestRecord,
        attempt: &'a AttemptRecord,
        table: &PriceTable,
        now: i64,
    ) -> Self {
        AttemptFacts {
            model: &request.canonical_model,
            client: request.client.as_deref(),
            classifier: request.classifier.as_deref(),
            received_at: request.received_at,
            completed_at: attempt
                .finished_at
                .or(request.finished_at)
                .unwrap_or(attempt.started_at),
            speed: attempt.speed.as_deref(),
            funding: attempt.funding(),
            tier: attempt.tier.as_deref(),
            usage: attempt.usage(),
        }
        .key(table, now)
    }
}

/// One attempt's facts toward its price key, borrowed from the records the request drawer holds or from the columns a
/// priced attempt was read from, so both readers key an attempt by the one rule in `key`.
pub(crate) struct AttemptFacts<'a> {
    pub model: &'a str,
    /// The request's `client` label, `None` on a row from before the column.
    pub client: Option<&'a str>,
    pub classifier: Option<&'a str>,
    /// The request's receipt, which selects both revisions.
    pub received_at: i64,
    /// The attempt's finish, else the request's, else the attempt's start, since the meter charges the peak band where
    /// the answer completed.
    pub completed_at: i64,
    pub speed: Option<&'a str>,
    pub funding: Funding,
    pub tier: Option<&'a str>,
    pub usage: PinUsage,
}

impl<'a> AttemptFacts<'a> {
    /// The key the attempt prices under at `now`. `table` is the price revision in force at `received_at`, whose
    /// `band_of` reads the band off the attempt's own prompt, so a custom or a historical long-context threshold prices
    /// the attempt as the fold prices it.
    pub(crate) fn key(&self, table: &PriceTable, now: i64) -> RowKey<'a> {
        RowKey {
            model: self.model,
            speed: Speed::of(self.speed),
            band: table.band_of(self.model, self.usage.prompt_tokens()),
            funding: self.funding,
            tier: self.tier,
            entrypoint: self.client.and_then(entrypoint_of_client),
            classifier: is_free_classifier(self.classifier),
            at: self.received_at,
            band_at: Some(self.completed_at),
            now,
        }
    }
}

/// Every figure a reader may want, so the native rule (a plan row in the equivalent, a billed row in dollars) is
/// written once by the caller and never recomputed from the account tier the registry holds now.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Priced {
    /// The price revision in force at `at` over the five counts at the selected cells, `None` for a model it lacks.
    /// A client classifier call under the plan reads the zero split on any model.
    pub usd: Option<CostSplit>,
    /// The shares of the account tier's windows for `Funding::Plan` with an account tier the cost revision sizes,
    /// else `None`.
    pub shares: Option<WindowShares>,
    /// The Pro-week figure for `Funding::Plan`, `Some(0.0)` for a classifier call, `None` for a billed row and for a
    /// plan row the cost model cannot price.
    pub equivalent: Option<f64>,
    /// Why the shares read zero by rule rather than by usage: `client_classifier` under the plan for a classifier
    /// call, `None` for a row priced from its counts.
    pub share_reason: Option<&'static str>,
}

impl Priced {
    /// The window shares of a plan row priced under `key`: `shares` where the cost revision sizes the row's tier, else,
    /// for a recorded tier the revision does not size, every window the model keys at `None`, as `price_usage` latches
    /// a bucket's. `None` for another funding, for a row with no tier and for a free call on a tier that sizes no
    /// window, which spends none.
    pub fn plan_shares(&self, key: &RowKey<'_>) -> Option<WindowShares> {
        if self.shares.is_some() {
            return self.shares.clone();
        }
        if key.funding != Funding::Plan || key.tier.is_none() || self.share_reason.is_some() {
            return None;
        }
        Some(
            plan_windows(key.model)
                .map(|window| (window, None))
                .collect(),
        )
    }
}

/// Every figure for one row.
pub fn price_row(
    prices: &PriceRevisions,
    costs: &PlanTables,
    key: &RowKey<'_>,
    counts: PinUsage,
) -> Priced {
    let usd = price_usd(prices, key, counts);
    if key.funding != Funding::Plan {
        return Priced {
            usd,
            ..Priced::default()
        };
    }
    let in_force = costs.revisions.in_force(key.at);
    let sized = key.tier.filter(|tier| in_force.sizes(key.entrypoint, tier));
    if key.classifier {
        // A free call spends no window and costs no dollars, so every figure reads zero.
        let shares = sized.map(|tier| free_shares(costs, key.entrypoint, tier, key.model, key.at));
        return Priced {
            usd,
            shares,
            equivalent: price_equivalent_at(costs, key, counts),
            share_reason: Some(SHARE_REASON_CLIENT_CLASSIFIER),
        };
    }
    let band_at = key.band_at.unwrap_or(key.at);
    // The account tier under the row's regime alone decides whether shares exist, so on a sized tier a window the
    // model cannot price stays `None` inside the map, which the readers tell apart from a tier with no window to share.
    let shares = sized.map(|tier| {
        price_shares(
            costs,
            key.entrypoint,
            tier,
            key.model,
            counts,
            key.at,
            band_at,
        )
    });
    // On the unit's own tier under one revision at `at` and `now`, the 7-day share divides the same units by the same
    // anchor row in the figure's order, so it is the figure to the bit.
    let unit_week = shares
        .as_ref()
        .filter(|_| {
            key.tier == Some(in_force.unit.tier.as_str())
                && std::ptr::eq(in_force, costs.revisions.in_force(key.now))
        })
        .and_then(|shares| shares.get(SEVEN_DAY).copied());
    let equivalent = unit_week.unwrap_or_else(|| price_equivalent_at(costs, key, counts));
    Priced {
        usd,
        shares,
        equivalent,
        share_reason: None,
    }
}

/// A plan key's Pro-week figure over `counts`, as `price_row` answers it off the unit's tier: zero by rule for a free
/// classifier call, which reads no weight, else the cost model's figure at the key's instants.
pub(crate) fn price_equivalent_at(
    costs: &PlanTables,
    key: &RowKey<'_>,
    counts: PinUsage,
) -> Option<f64> {
    if key.classifier {
        return Some(free_equivalent());
    }
    price_equivalent(
        costs,
        key.entrypoint,
        key.model,
        counts,
        key.at,
        key.band_at.unwrap_or(key.at),
        key.now,
    )
}

/// `price_row`'s dollars alone: the counts at the price revision in force at the row's instant, `None` for a
/// model that revision lacks, and a zero split for a client classifier call the plan paid, which is free on its
/// account in every unit whatever the revision lists. The `/v1/usage` sums read these and no other figure.
pub(crate) fn price_usd(
    prices: &PriceRevisions,
    key: &RowKey<'_>,
    counts: PinUsage,
) -> Option<CostSplit> {
    if key.classifier && key.funding == Funding::Plan {
        return Some(CostSplit::default());
    }
    prices
        .in_force(key.at)
        .estimate_banded(key.model, key.speed, key.band, counts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_tables::{
        api_prices, first_entry_costs_text, subscription_costs, FEBRUARY_2026, JANUARY_2026,
        SUBSCRIPTION_COSTS,
    };
    use crate::CostRevisions;
    use phoebis_routing::{FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE};

    const MAX_20X: &str = "default_claude_max_20x";
    const PRO: &str = "default_claude_ai";
    const TEAM: &str = "default_claude_team";
    const OPUS: &str = "claude-opus-5-5";
    /// Thursday 2026-01-15 at 14:00 UTC, 06:00 in Los Angeles: inside the band and the fixtures' first entries.
    const JANUARY_IN_BAND: i64 = 1_768_485_600_000;
    /// Monday 2026-02-02 at 14:00 UTC, 06:00 in Los Angeles: inside the band and the second entries.
    const FEBRUARY_IN_BAND: i64 = 1_770_040_800_000;
    /// Saturday 2026-01-31 at 23:30 UTC, half an hour before the second entries take effect.
    const BEFORE_MIDNIGHT: i64 = 1_769_902_200_000;
    /// Sunday 2026-02-01 at 00:10 UTC, ten minutes into the second entries, still Saturday afternoon in Los Angeles.
    const AFTER_MIDNIGHT: i64 = 1_769_904_600_000;

    fn input(tokens: u64) -> PinUsage {
        PinUsage {
            input: tokens,
            ..PinUsage::default()
        }
    }

    /// A standard, plan-funded Opus 5.5 inference row on Ultra, out of band in January, read in February.
    fn plan_row() -> RowKey<'static> {
        RowKey {
            model: OPUS,
            speed: Speed::Standard,
            band: Band::Standard,
            funding: Funding::Plan,
            tier: Some(MAX_20X),
            entrypoint: None,
            classifier: false,
            at: JANUARY_2026,
            band_at: None,
            now: FEBRUARY_2026,
        }
    }

    fn price(key: &RowKey<'_>, counts: PinUsage) -> Priced {
        price_row(&api_prices(), &subscription_costs(), key, counts)
    }

    fn close(got: Option<f64>, want: f64) -> bool {
        got.is_some_and(|got| (got - want).abs() <= 1e-12 * want.abs().max(1.0))
    }

    fn windows(shares: &Option<WindowShares>) -> Vec<(&'static str, Option<f64>)> {
        shares
            .iter()
            .flatten()
            .map(|(window, share)| (*window, *share))
            .collect()
    }

    // One Opus 5.5 input token: $10 per million, 2 units over Ultra's 100 and 1000, and over the anchor Pro's 50 in
    // the Pro-week figure.
    #[test]
    fn a_plan_row_on_a_sized_tier_answers_dollars_shares_and_the_equivalent() {
        let priced = price(&plan_row(), input(1));
        let usd = priced.usd.expect("Opus 5.5 is listed");
        assert!(close(Some(usd.input_usd), 10e-6), "{usd:?}");
        assert_eq!(usd.total(), usd.input_usd);
        let shares = priced.shares.as_ref().expect("Ultra is sized");
        assert!(close(shares[FIVE_HOUR], 0.02) && close(shares[SEVEN_DAY], 0.002));
        assert!(close(priced.equivalent, 0.04), "{:?}", priced.equivalent);
    }

    // The test regime holds 200 and 1,250 units on Ultra against the base 100 and 1,000, so the one token's 2 units
    // read half the base 5-hour share and four fifths of the week's. The Pro-week figure reads the regime's Pro row,
    // 2 / 62.5.
    #[test]
    fn a_rows_entrypoint_prices_its_shares_and_its_equivalent_under_the_regime() {
        let priced = price(
            &RowKey {
                entrypoint: Some("desk"),
                ..plan_row()
            },
            input(1),
        );
        let shares = priced.shares.as_ref().expect("a plan row on Ultra");
        assert!(close(shares[FIVE_HOUR], 0.01), "{shares:?}");
        assert!(close(shares[SEVEN_DAY], 0.0016), "{shares:?}");
        assert!(close(priced.equivalent, 0.032), "{priced:?}");
    }

    // The fast block's input is 20 per million, the standard cell 10.
    #[test]
    fn a_fast_an_overage_a_usage_based_and_an_api_key_row_answer_dollars_alone() {
        for (funding, speed, per_million) in [
            (Funding::Fast, Speed::Fast, 20.0),
            (Funding::Overage, Speed::Standard, 10.0),
            (Funding::UsageBased, Speed::Standard, 10.0),
            (Funding::ApiKey, Speed::Standard, 10.0),
        ] {
            let priced = price(
                &RowKey {
                    funding,
                    speed,
                    ..plan_row()
                },
                input(1_000_000),
            );
            assert!(
                close(priced.usd.as_ref().map(|u| u.input_usd), per_million),
                "{funding:?}: {priced:?}"
            );
            assert_eq!(
                (&priced.shares, priced.equivalent),
                (&None, None),
                "{funding:?}"
            );
        }
    }

    #[test]
    fn a_plan_row_of_a_classifier_call_answers_zero_dollars_and_zero_on_every_window_with_the_reason(
    ) {
        let plain = price(&plan_row(), input(1_000));
        let free = price(
            &RowKey {
                classifier: true,
                ..plan_row()
            },
            input(1_000),
        );
        assert!(
            plain.usd.as_ref().is_some_and(|usd| usd.total() > 0.0),
            "Opus 5.5 is listed: {plain:?}"
        );
        assert_eq!(
            free.usd,
            Some(CostSplit::default()),
            "a free call costs no dollars: {free:?}"
        );
        // The `/v1/usage` sums call `price_usd` without `price_row`, so the zero must come from `price_usd` itself.
        assert_eq!(
            price_usd(
                &api_prices(),
                &RowKey {
                    classifier: true,
                    ..plan_row()
                },
                input(1_000),
            ),
            Some(CostSplit::default()),
            "the dollars alone read zero for a free call"
        );
        let plain_windows = windows(&plain.shares);
        assert!(!plain_windows.is_empty(), "{plain:?}");
        assert_eq!(
            windows(&free.shares),
            plain_windows
                .iter()
                .map(|(window, _)| (*window, Some(0.0)))
                .collect::<Vec<_>>(),
            "every window the plain row prices reads zero: {free:?}"
        );
        assert_eq!(free.equivalent, Some(0.0), "{free:?}");
        assert_eq!(free.share_reason, Some("client_classifier"));
        assert_eq!(plain.share_reason, None);
    }

    #[test]
    fn an_overage_a_usage_based_and_an_api_key_row_of_a_classifier_call_answer_their_dollars_and_no_reason(
    ) {
        for funding in [Funding::Overage, Funding::UsageBased, Funding::ApiKey] {
            let row = RowKey {
                funding,
                ..plan_row()
            };
            let plain = price(&row, input(1_000_000));
            let marked = price(
                &RowKey {
                    classifier: true,
                    ..row
                },
                input(1_000_000),
            );
            assert!(plain.usd.is_some(), "{funding:?}");
            assert_eq!(marked, plain, "{funding:?}");
            assert_eq!(marked.share_reason, None, "{funding:?}");
        }
    }

    // Mystery 2 sits on a model tier with no weighted model, so a priced row reads both windows unpriced, while the
    // marker's zero maps come from Ultra's capacity alone. The price list lacks it too, so its zero dollars come from
    // the free rule ahead of the list's lookup.
    #[test]
    fn a_classifier_call_on_a_model_the_weight_table_lacks_answers_the_full_zero_maps() {
        let row = RowKey {
            model: "claude-mystery-2",
            ..plan_row()
        };
        let free = price(
            &RowKey {
                classifier: true,
                ..row
            },
            input(1_000),
        );
        assert_eq!(free.usd, Some(CostSplit::default()), "{free:?}");
        assert_eq!(
            (windows(&free.shares), free.equivalent),
            (
                vec![(FIVE_HOUR, Some(0.0)), (SEVEN_DAY, Some(0.0))],
                Some(0.0)
            ),
            "{free:?}"
        );
        assert_eq!(free.share_reason, Some("client_classifier"));
        let plain = price(&row, input(1_000));
        assert_eq!(plain.usd, None, "the price list lacks Mystery 2: {plain:?}");
        assert_eq!(
            (windows(&plain.shares), plain.equivalent),
            (vec![(FIVE_HOUR, None), (SEVEN_DAY, None)], None),
            "{plain:?}"
        );
    }

    // With Sonnet 5.5's weight nulled the model prices nothing, while the marker's zero maps come from the tier's
    // capacity alone.
    #[test]
    fn a_classifier_call_on_a_model_with_a_null_weight_answers_the_full_zero_maps() {
        let text = first_entry_costs_text().replace(
            "      claude-sonnet-5-5: 1\n",
            "      claude-sonnet-5-5: null\n",
        );
        let costs = PlanTables {
            revisions: CostRevisions::from_yaml(&text).expect("the edited model loads"),
            path: None,
            source: "test".to_string(),
        };
        let row = RowKey {
            model: "claude-sonnet-5-5",
            at: JANUARY_2026,
            ..plan_row()
        };
        let free = price_row(
            &api_prices(),
            &costs,
            &RowKey {
                classifier: true,
                ..row
            },
            input(1_000),
        );
        assert_eq!(
            (windows(&free.shares), free.equivalent),
            (
                vec![(FIVE_HOUR, Some(0.0)), (SEVEN_DAY, Some(0.0))],
                Some(0.0)
            ),
            "{free:?}"
        );
        assert_eq!(free.share_reason, Some("client_classifier"));
        let plain = price_row(&api_prices(), &costs, &row, input(1_000));
        assert_eq!(
            (windows(&plain.shares), plain.equivalent),
            (vec![(FIVE_HOUR, None), (SEVEN_DAY, None)], None),
            "{plain:?}"
        );
    }

    // Haiku 5.5's long-context input is 5 per million and its standard input 1, whatever the counts.
    #[test]
    fn the_band_selects_the_cells_with_no_prompt_handed_in() {
        for (band, per_million) in [(Band::LongContext, 5.0), (Band::Standard, 1.0)] {
            let priced = price(
                &RowKey {
                    model: "claude-haiku-5-5",
                    band,
                    funding: Funding::ApiKey,
                    tier: None,
                    ..plan_row()
                },
                input(1_000_000),
            );
            assert!(
                close(priced.usd.as_ref().map(|u| u.input_usd), per_million),
                "{band:?}: {priced:?}"
            );
        }
    }

    #[test]
    fn a_plan_row_without_a_tier_answers_no_shares_and_an_equivalent() {
        let priced = price(
            &RowKey {
                tier: None,
                ..plan_row()
            },
            input(1),
        );
        assert_eq!(priced.shares, None);
        // The anchor Pro's 50.
        assert!(close(priced.equivalent, 0.04), "{priced:?}");
    }

    #[test]
    fn a_plan_row_on_a_tier_the_revision_does_not_size_answers_no_shares() {
        let priced = price(
            &RowKey {
                tier: Some("claude_enterprise"),
                ..plan_row()
            },
            input(1),
        );
        assert_eq!(priced.shares, None);
    }

    // An enterprise row null on both base windows that the test regime sizes at 40 and 400 units: the one Opus 5.5
    // token's 2 units read 2 / 40 and 2 / 400 under the regime's entrypoint, and the base sizes nothing.
    #[test]
    fn a_plan_row_on_a_tier_only_a_regime_sizes_answers_shares_under_that_regime_alone() {
        let text = first_entry_costs_text()
            .replace(
                "      default_claude_team:    {five_hour: 10, seven_day: null}\n",
                "      default_claude_team:    {five_hour: 10, seven_day: null}\n      \
                 default_claude_enterprise: {five_hour: null, seven_day: null}\n",
            )
            .replace(
                "          default_claude_max_20x: {five_hour: 200, seven_day: 1250}\n",
                "          default_claude_max_20x: {five_hour: 200, seven_day: 1250}\n          \
                 default_claude_enterprise: {five_hour: 40, seven_day: 400}\n",
            );
        let costs = PlanTables {
            revisions: CostRevisions::from_yaml(&text).expect("the edited model loads"),
            path: None,
            source: "test".to_string(),
        };
        let enterprise = |entrypoint| RowKey {
            tier: Some("default_claude_enterprise"),
            entrypoint,
            ..plan_row()
        };
        let desk = price_row(&api_prices(), &costs, &enterprise(Some("desk")), input(1));
        let shares = desk.shares.as_ref().expect("the regime sizes the tier");
        assert!(close(shares[FIVE_HOUR], 0.05), "{shares:?}");
        assert!(close(shares[SEVEN_DAY], 0.005), "{shares:?}");
        let base = price_row(&api_prices(), &costs, &enterprise(None), input(1));
        assert_eq!(base.shares, None);
    }

    // Mystery 2's model tier has no weighted model, so a sized tier keeps both windows, each unpriced, and Team,
    // sized on the 5-hour window alone, keeps its unsized week the same way.
    #[test]
    fn a_plan_row_on_a_sized_tier_keeps_every_window_it_cannot_price_as_null() {
        let mystery = price(
            &RowKey {
                model: "claude-mystery-2",
                ..plan_row()
            },
            input(1),
        );
        assert_eq!(
            windows(&mystery.shares),
            [(FIVE_HOUR, None), (SEVEN_DAY, None)]
        );
        assert_eq!(mystery.equivalent, None);
        let team = price(
            &RowKey {
                tier: Some(TEAM),
                ..plan_row()
            },
            input(1),
        );
        let shares = windows(&team.shares);
        assert!(close(shares[0].1, 0.2), "2 / 10: {shares:?}");
        assert_eq!(shares[1], (SEVEN_DAY, None));
    }

    #[test]
    fn the_equivalent_is_the_same_under_every_account_tier_while_the_share_follows_the_tier() {
        let [max, pro, team] = [MAX_20X, PRO, TEAM].map(|tier| {
            price(
                &RowKey {
                    tier: Some(tier),
                    ..plan_row()
                },
                input(1),
            )
        });
        assert_eq!(max.equivalent, pro.equivalent);
        assert_eq!(max.equivalent, team.equivalent);
        assert_ne!(max.shares, pro.shares);
        assert_ne!(max.shares, team.shares);
    }

    // `at` and `now` both in January: one Opus 5.5 input token is 2 units at January's weight over January's Pro week
    // of 50, out of band (factor 1) and on the 7-day window itself (divisor 1), so 0.04 to the bit, where the row's
    // 5-hour share reads 2 / 5.
    #[test]
    fn a_pro_row_priced_inside_one_revision_reads_the_equivalent_it_would_have_priced() {
        let priced = price(
            &RowKey {
                tier: Some(PRO),
                at: JANUARY_2026,
                now: BEFORE_MIDNIGHT,
                ..plan_row()
            },
            input(1),
        );
        assert_eq!(priced.equivalent, Some(0.04), "{priced:?}");
    }

    // Opus 5 carries a null weight, so it prices nothing on any window though Opus 5.5 weighs its model tier, and its
    // dollars read its own row.
    #[test]
    fn a_model_without_a_weight_prices_nothing() {
        let unweighted = price(
            &RowKey {
                model: "claude-opus-5",
                ..plan_row()
            },
            input(1),
        );
        assert_eq!(
            (windows(&unweighted.shares), unweighted.equivalent),
            (vec![(FIVE_HOUR, None), (SEVEN_DAY, None)], None),
            "{unweighted:?}"
        );
        assert!(close(unweighted.usd.as_ref().map(|u| u.input_usd), 5e-6));
    }

    #[test]
    fn a_row_on_a_model_the_revision_lacks_reads_no_dollars() {
        let counts = PinUsage {
            input: 1_000,
            cache_read: 2_000,
            write_5m: 300,
            write_1h: 400,
            output: 50,
        };
        for at in [JANUARY_2026, FEBRUARY_2026] {
            let priced = price(
                &RowKey {
                    model: "claude-mystery-9",
                    at,
                    ..plan_row()
                },
                counts,
            );
            assert_eq!(priced.usd, None, "{at}");
        }
    }

    // `at` in January keeps Opus 5.5's weight of 2, and `band_at` inside the band divides the 5-hour window's units
    // by its 0.5, so the share reads 0.04 where the revision at `band_at` would read 0.08 and the band at `at` 0.02.
    // The test tables leave the week unbanded, so the Pro-week figure reads 2 / 50 either way. A first entry that bands
    // the week at 0.5 reads the figure at twice its out-of-band 0.04 under `band_at` inside the band.
    #[test]
    fn band_at_inside_the_band_raises_the_five_hour_share_while_at_picks_the_revision() {
        let banded_week = PlanTables {
            revisions: CostRevisions::from_yaml(&first_entry_costs_text().replacen(
                "capacity_factor: {five_hour: 0.5, seven_day: 1}",
                "capacity_factor: {five_hour: 0.5, seven_day: 0.5}",
                1,
            ))
            .expect("the edited model loads"),
            path: None,
            source: "test".to_string(),
        };
        let out_of_band = price_row(&api_prices(), &banded_week, &plan_row(), input(1));
        assert!(close(out_of_band.equivalent, 0.04), "{out_of_band:?}");
        for band_at in [JANUARY_IN_BAND, FEBRUARY_IN_BAND] {
            let key = RowKey {
                band_at: Some(band_at),
                ..plan_row()
            };
            let priced = price(&key, input(1));
            let shares = priced.shares.as_ref().unwrap();
            assert!(close(shares[FIVE_HOUR], 0.04), "{band_at}: {shares:?}");
            assert!(close(shares[SEVEN_DAY], 0.002), "{band_at}: {shares:?}");
            assert!(close(priced.equivalent, 0.04), "{band_at}: {priced:?}");
            let banded = price_row(&api_prices(), &banded_week, &key, input(1));
            assert!(
                close(banded.equivalent, 2.0 * 0.04),
                "{band_at}: {banded:?}"
            );
        }
    }

    #[test]
    fn a_request_received_before_a_revisions_midnight_and_completed_after_it_prices_at_the_earlier_revision(
    ) {
        let counts = PinUsage {
            input: 1_000,
            output: 100,
            ..PinUsage::default()
        };
        let at = |at: i64, band_at: i64| {
            price(
                &RowKey {
                    at,
                    band_at: Some(band_at),
                    ..plan_row()
                },
                counts,
            )
        };
        let crossing = at(BEFORE_MIDNIGHT, AFTER_MIDNIGHT);
        assert_eq!(crossing, at(JANUARY_2026, JANUARY_2026));
        let february = at(AFTER_MIDNIGHT, AFTER_MIDNIGHT);
        let total = |p: &Priced| p.usd.as_ref().unwrap().total();
        assert!(close(Some(total(&february)), 2.0 * total(&crossing)));
        for window in [FIVE_HOUR, SEVEN_DAY] {
            let doubled = |of: &Option<WindowShares>, base: &Option<WindowShares>| {
                close(
                    of.as_ref().unwrap()[window],
                    2.0 * base.as_ref().unwrap()[window].unwrap(),
                )
            };
            assert!(doubled(&february.shares, &crossing.shares), "{window}");
        }
        assert!(close(
            february.equivalent,
            2.0 * crossing.equivalent.unwrap()
        ));
        assert!(!crossing
            .shares
            .as_ref()
            .unwrap()
            .contains_key(SEVEN_DAY_FABLE));
    }

    // The test tables size the Pro week at 50 in both revisions, so the case edits the second entry's Pro row to 200.
    // One Opus 5.5 input token in January is 2 units, the January weight, over the February anchor row of 200: 0.01,
    // where a divisor read at `at` would read 2 / 50 and units read at `now` 4 / 200. The shares keep January's rows.
    #[test]
    fn a_row_of_the_first_revision_read_at_the_second_divides_by_the_second_revisions_anchor() {
        let (first, second) = SUBSCRIPTION_COSTS
            .split_once("  - effective: 2026-02-01\n")
            .expect("the cost fixture carries a second entry");
        let resized = second.replacen(
            "      default_claude_ai:      {five_hour: 5, seven_day: 50}\n",
            "      default_claude_ai:      {five_hour: 5, seven_day: 200}\n",
            1,
        );
        assert_ne!(
            resized, second,
            "the edit lands on the second entry's Pro row"
        );
        let costs = PlanTables {
            revisions: CostRevisions::from_yaml(&format!(
                "{first}  - effective: 2026-02-01\n{resized}"
            ))
            .expect("a revision that resizes the anchor row loads"),
            path: None,
            source: "test".to_string(),
        };
        let key = RowKey {
            tier: Some(PRO),
            at: JANUARY_2026,
            now: FEBRUARY_2026,
            ..plan_row()
        };
        let priced = price_row(&api_prices(), &costs, &key, input(1));
        assert!(close(priced.equivalent, 2.0 / 200.0), "{priced:?}");
        let shares = priced.shares.as_ref().expect("Pro is sized");
        assert!(close(shares[SEVEN_DAY], 2.0 / 50.0), "{shares:?}");
    }
}
