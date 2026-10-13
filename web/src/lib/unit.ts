import type { FundingFilter } from '@/api/types';

/**
 * The console-wide unit position. Named apart from `CostUnit` in `web/src/lib/requests.ts` (a request's native unit),
 * since the test entry re-exports both modules and one name for two types would clash.
 */
export type UnitPosition = 'all' | 'equivalent' | 'native' | 'share' | 'usd';
/** The select's order: native first, the default, then the Cost section's four. */
export const UNIT_POSITIONS: readonly UnitPosition[] = ['native', 'all', 'usd', 'share', 'equivalent'];
export const UNIT_LABEL: Record<UnitPosition, string> = { all: 'All in dollars', equivalent: 'Dollar equivalent', native: 'Native', share: 'Window shares', usd: 'Billed dollars' };
/** The labels the top bar's select shows at 900 px and below, where the bar spans a viewport as narrow as a phone's. */
export const UNIT_SHORT: Record<UnitPosition, string> = { all: 'All $', equivalent: 'Equivalent $', native: 'Native', share: 'Shares', usd: 'Billed $' };
/**
 * The funding side a position's second read asks for: the Analytics page's second query under `usd` and `equivalent`
 * (its own reads serve under `all` and `share`, since a share sums plan-funded rows alone whatever the filter) and the
 * session fold's under `usd`, `share` and `equivalent`. `native` reads both sides of one answer.
 */
export const UNIT_FUNDING: Record<UnitPosition, FundingFilter | undefined> = { all: undefined, equivalent: 'plan', native: undefined, share: 'plan', usd: 'billed' };
/** One line per position in the legend, beside the labels so the console and its checks read one source. */
export const UNIT_LEGEND: Record<UnitPosition, string> = {
  all: 'Every request in dollars at the API price list, a subscription request at its dollar equivalent.',
  equivalent: 'The subscription requests alone at the API price list.',
  native: 'Each request in the unit its funding pays in: a subscription request as 20x window equivalents, an API key, overage or fast-mode request in dollars.',
  share: "A subscription account's requests as shares of its own 5-hour, 7-day and model-week windows. Reads where one account is named, since shares of different accounts' windows do not add.",
  usd: 'The overage, fast-mode and API key requests alone, the dollars the account holder is charged.',
};
/**
 * The legend's closing paragraph. It names the account tiers the equivalent compares across apart from the model tier
 * an estimate reads.
 */
export const UNIT_LEGEND_NOTE =
  "A 20x window equivalent is the share of a Max 20x account's window the same tokens would spend, the cost model's unit over the Max 20x capacity, so figures of different account tiers compare and add. The 5-hour column carries the peak band's weight, the 7-day column the week's factor, and a model week its own capacity factor. A figure marked ~ is estimated: its model has no measured weight and reads the latest weighed model of its model tier.";
/**
 * The stored value as a position: the browser hands back whatever string it holds, so a value the console does not know
 * reads native.
 */
export const positionOf = (value: string | null | undefined): UnitPosition => UNIT_POSITIONS.find((p) => p === value) ?? 'native';
