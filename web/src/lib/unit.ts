import type { FundingFilter } from '@/api/types';

/**
 * The console-wide unit position. Named apart from `CostUnit` in `web/src/lib/requests.ts` (a request's native unit),
 * since the test entry re-exports both modules and one name for two types would clash.
 */
export type UnitPosition = 'all' | 'equivalent' | 'native' | 'usd';
/** The select's order: native first, the default, then the Cost section's three. */
export const UNIT_POSITIONS: readonly UnitPosition[] = ['native', 'all', 'usd', 'equivalent'];
export const UNIT_LABEL: Record<UnitPosition, string> = { all: 'All in dollars', equivalent: 'Dollar equivalent', native: 'Native', usd: 'Billed dollars' };
/** The labels the top bar's select shows at 900 px and below, where the bar spans a viewport as narrow as a phone's. */
export const UNIT_SHORT: Record<UnitPosition, string> = { all: 'All $', equivalent: 'Equivalent $', native: 'Native', usd: 'Billed $' };
/**
 * The funding side a position's second read asks for: the Analytics page's second query and the session fold's under
 * `usd` and `equivalent`, the page's own reads serving under `all`. `native` reads both sides of one answer.
 */
export const UNIT_FUNDING: Record<UnitPosition, FundingFilter | undefined> = { all: undefined, equivalent: 'plan', native: undefined, usd: 'billed' };
/** One line per position in the legend, beside the labels so the console and its checks read one source. */
export const UNIT_LEGEND: Record<UnitPosition, string> = {
  all: 'Every request in dollars at the API price list, a subscription request at its dollar equivalent.',
  equivalent: 'The subscription requests alone at the API price list.',
  native: 'Each request in the unit its funding pays in: a subscription request as a share of the week the week control names, an API key, fast-mode, overage or usage-based request in dollars.',
  usd: "The API key, fast-mode, overage and usage-based requests alone, the dollars the account holder is charged, a billed keepalive probe's among them.",
};
/**
 * The legend's closing paragraph. The figure is a share of the Pro week whatever the account tier, and the week
 * control rescales it to the week the operator reads in.
 */
export const UNIT_LEGEND_NOTE =
  "A subscription figure is the share of a Pro 7-day window the same tokens would spend, the cost model's units over the Pro capacity, so figures of different account tiers compare and add. The week control reads it as a share of a Max week, five Pro weeks, or an Ultra week, ten Pro weeks. On a request or one account's row the figure's hover names the account's plan and its share of each window the account has.";
/**
 * The stored value as a position: the browser hands back whatever string it holds, so a value the console does not know
 * reads native.
 */
export const positionOf = (value: string | null | undefined): UnitPosition => UNIT_POSITIONS.find((p) => p === value) ?? 'native';

export type WeekPosition = 'max' | 'pro' | 'ultra';
/** The select's order. */
export const WEEK_POSITIONS: readonly WeekPosition[] = ['pro', 'max', 'ultra'];
export const WEEK_LABEL: Record<WeekPosition, string> = { max: 'Max week', pro: 'Pro week', ultra: 'Ultra week' };
/** The labels at 900 px and below. */
export const WEEK_SHORT: Record<WeekPosition, string> = { max: 'Max', pro: 'Pro', ultra: 'Ultra' };
export const WEEK_TIER: Record<WeekPosition, string> = { max: 'default_claude_max_5x', pro: 'default_claude_ai', ultra: 'default_claude_max_20x' };
/** A stored value the console does not know reads ultra, the default. */
export const weekOf = (value: string | null | undefined): WeekPosition => WEEK_POSITIONS.find((w) => w === value) ?? 'ultra';
