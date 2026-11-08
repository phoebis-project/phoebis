import type { Api } from '@/api/client';
import type { Account, AccountRollup, AnalyticsGroup, AnalyticsQuery, BurnGroup, BurnPoint, BurnUnit, FundedSum, HeldPoint, PinMigrationReason, Placement, PlacementRow, Reconciliation, RewritesAnswer, SubscriptionCosts, WindowSample } from '@/api/types';
import type { AnalyticsAddress } from '@/lib/address';
import { MODEL_WEEK_TIERS, tierCapacity, type CostWindow } from '@/lib/costs';
import { has, lookup } from '@/lib/counts';
import { earlierHolderLabel, fmtCompact, fmtCount, fmtInt, fmtPct, fmtShare, fmtUsd, fmtWeek, joinPresent, parseKeyLifetime, weekShort } from '@/lib/format';
import { planLabel, pricedTier } from '@/lib/quota';
import { ageWord, HOUR, hourCeil } from '@/lib/time';
import type { UnitPosition, WeekPosition } from '@/lib/unit';
import { WINDOW_KEYS } from '@/lib/windows';

/** Why a session's pin moved, as the pool card's moves tile, the rewrites legend and the routing log word it. */
export const PIN_MIGRATION_LABEL: Record<PinMigrationReason, string> = {
  cooldown: 'failed over',
  drain: 'winding down',
  fast_lane: 'fast lane cooling',
  mark: 'account marked',
  overage: 'overage',
  return_to_plan: 'returned to plan',
  shed: 'shed',
  spent: 'window spent',
};
/**
 * The pool's own balancing: what the pool card's move tile sums, the rule moves on the pool's member accounts in the
 * last hour. A keepalive probe's move counts among them, and the routing log lists it only with probe decisions chosen.
 */
export const RULE_MOVE_REASONS: readonly PinMigrationReason[] = ['drain', 'overage', 'return_to_plan', 'shed', 'spent'];
/** The moves an answer forced on a session, which the pool card's move tile names apart from its count. */
export const FAILOVER_MOVE_REASONS: readonly PinMigrationReason[] = ['cooldown', 'fast_lane', 'mark'];
/** A reason the console has no label for reads as the router sent it. */
export const moveLabel = (reason: string): string => lookup(PIN_MIGRATION_LABEL, reason) ?? reason;

/** The held bars: one per bucket with its milliseconds and request count, and the range's sum for the legend. */
export function heldBars(answer: RewritesAnswer): { bars: HeldPoint[]; totalMs: number; totalRequests: number } {
  const bars = answer.held;
  return { bars, totalMs: bars.reduce((n, b) => n + b.heldMs, 0), totalRequests: bars.reduce((n, b) => n + b.requests, 0) };
}

/** The held time of a bucket or a range as its hover and legend read it, at least a second where a request waited. */
export const heldSpan = (ms: number): string => ageWord(Math.max(1_000, ms));

/** A held bar's hover, `12 requests held 14 min in all`, and `no request held` for a bucket that held none. */
export const heldHover = (bar: Pick<HeldPoint, 'heldMs' | 'requests'>): string =>
  bar.requests === 0 ? 'no request held' : `${fmtCount(bar.requests, 'request')} held ${heldSpan(bar.heldMs)} in all`;

/** The classes that refresh a pin's warmth, the routing crate's `REFRESHING_CLASSES`: the conversation itself. */
export const CONVERSATION_CLASSES = ['compaction', 'main', 'subagent'] as const;

export type ClassLegend = 'conversation' | 'overhead';

/** Conversation or the overhead around it; `-`, the rollups' name for a request that sent no class, is overhead. */
export const classLegendOf = (requestClass: string): ClassLegend => ((CONVERSATION_CLASSES as readonly string[]).includes(requestClass) ? 'conversation' : 'overhead');

/** Lines a burn chart draws before the rest fold into one, the stacked cost chart's limit. */
export const MAX_BURN_LINES = 7;
/** The key the folded rest carries, suffixed by its unit under the account grouping. No group is looked up by it. */
export const OTHER_KEY = 'other';
/**
 * The unit a burn line charts in. `unknown` is a subscription account whose plan the router prices on no tier the cost
 * model sizes (`pricedTier`): a free plan, an account with no tier or one on a tier the model has no capacity row for.
 * It has no share to draw, and dollars only for what its fast-mode and overage answers spent.
 */
export type BurnLineUnit = BurnUnit | 'unknown';
const OTHER_LABEL: Record<BurnLineUnit, string> = { unknown: 'Other (tier unknown)', usd: 'Other (dollars)', window_share: 'Other (plan)' };

export interface BurnLine {
  key: string;
  label: string;
  /** Whether this is the folded rest rather than one group. */
  other: boolean;
  /** The highest figure per hour a bucket that sets the scale reached, null for a line with no figure in any bucket. */
  peak: number | null;
  /** The range's figure per hour over the buckets that read one, null for a line that never priced traffic. */
  perHour: number | null;
  tokensPerMinute: number;
  /**
   * The unit the line is charted in: under the account grouping a subscription account burns plan share and an
   * API-key one dollars; every other grouping mixes both, so it charts dollars.
   */
  unit: BurnLineUnit;
  /** Whether a bucket of the line holds traffic the route flagged as priced on no weight. False on a folded rest. */
  unpriced: boolean;
  usdPerMinute: number;
}

/**
 * One bucket, indexed like the lines: the cached and uncached tokens and the dollars per minute of the time it covers,
 * its raw token count, and the Pro-week figure per hour.
 */
export interface BurnRow {
  at: number;
  cached: number[];
  /**
   * The Pro-week figure per hour, which the page scales to the chosen week. Null where the slot's traffic could not be
   * priced, on a line that never priced traffic and on a folded rest. An idle slot reads zero on a line that priced.
   */
  equivalent: (number | null)[];
  /** The bucket covers less than its width, so its rate rests on that part alone. */
  partial: boolean;
  /**
   * The seconds the bucket's rollups cover: the bucket's width up to the hour ceiling of the range's end, no later than
   * the read's instant; the full width without an end.
   */
  seconds: number;
  /**
   * Whether the bucket sets the chart's axis and the line's peak: one covering less than a tenth of its width rates a
   * few seconds of traffic as if they ran the whole bucket.
   */
  setsScale: boolean;
  /**
   * Every token of the slot, all five types, as the route counts them: zero for a slot with no traffic the ledger can
   * attribute.
   */
  tokens: number[];
  uncached: number[];
  usd: number[];
}

/** A slot's running figure over its points: null until a point carries one, and latched once a point reads unpriced,
 *  since a partial sum would read as the slot's figure. */
interface FigureFold {
  sum: number | null;
  unpriced: boolean;
}

interface Sum {
  cached: number;
  equivalent: FigureFold;
  tokens: number;
  uncached: number;
  usd: number;
}

/**
 * A bucket's share of one window over its rows: null when any row is unpriced, whatever the order, or when there are
 * no rows; the sum otherwise.
 */
export function bucketShare(shares: (number | null)[]): number | null {
  if (shares.length === 0 || shares.some((s) => s === null)) return null;
  return (shares as number[]).reduce((n, s) => n + s, 0);
}

/**
 * Whether a plan slot holds traffic the cost model could not price: a null figure with tokens behind it. An idle slot
 * of a line that never priced reads null too, with no traffic to list as model unlisted.
 */
export const slotUnpriced = (share: number | null, tokens: number): boolean => share === null && tokens > 0;

/**
 * Which slots hold a figure with no figure beside them on either side, the series' ends counting as null: a line that
 * leaves its null slots unconnected draws such a slot as a zero-length path, so the chart marks it with a dot.
 */
export function isolatedSlots(values: (number | null)[]): boolean[] {
  return values.map((v, i) => v !== null && (values[i - 1] ?? null) === null && (values[i + 1] ?? null) === null);
}

/**
 * The burn route's answer with the accounts beside it, read together: a profile refresh can supply a tier the last
 * read lacked, and the plan chart sizes each account from the tiers of the read that answered its buckets. Only a
 * read whose groups each hold one account's rows names an account to size (the account grouping, or the Accounts
 * filter naming exactly one account), and the same read gives the tooltip's hint its plan, so any other reads no
 * account and a failed account read leaves it whole.
 */
export async function readBurn(
  source: Pick<Api, 'getAnalyticsBurn' | 'listAccounts'>,
  range: Pick<AnalyticsQuery, 'from' | 'to'>,
  groupBy: BurnGroup,
  filters: Pick<AnalyticsQuery, 'accountIds' | 'userAgentGroups'> = {},
): Promise<{ accounts: Account[]; answer: { bucketSeconds: number; points: BurnPoint[] } }> {
  const sizes = groupBy === 'account' || filters.accountIds?.length === 1;
  const [answer, accounts] = await Promise.all([source.getAnalyticsBurn(range, groupBy, filters), sizes ? source.listAccounts() : Promise.resolve<Account[]>([])]);
  return { accounts, answer };
}

/**
 * A burn group's label. A burn point carries no label of its own, so the words are the breakdown's: an earlier key
 * holder's, `no model` for the model grouping's empty key, and for the reserved id `none` under the classifier
 * grouping, as the Requests facet names it, and `unattributed` under every other.
 */
function burnLabelOf(groupBy: BurnGroup, key: string): string {
  if (key === '-') return groupBy === 'classifier' ? 'none' : 'unattributed';
  if (groupBy === 'model' && key === '') return 'no model';
  const holder = groupBy === 'key' ? parseKeyLifetime(key) : null;
  return holder === null ? key : earlierHolderLabel(holder.id, holder.generation);
}

/**
 * The accounts the router prices a tier for (`pricedTier`), whose stored tier the cost model lists, matched as the
 * ledger matches it: as stored, then with the `default_` prefix taken off or put on, since only one spelling has been
 * seen on the wire. The plan chart reads a listed tier as sized and draws a window it has no capacity on as model
 * unlisted. With `window`, a tier sizes an account only where it has a capacity on that window, as the shed preview
 * needs for the 5-hour window: the sessions route reads no burn for a pin of a tier without one.
 */
export function sizedAccounts(accounts: Account[], costs: SubscriptionCosts, window?: CostWindow): Set<string> {
  const sizes = (a: Account) => {
    if (a.subscription === null) return false;
    const priced = pricedTier(a.subscription.plan, a.subscription.rateLimitTier, costs);
    return priced.tier !== null && (window === undefined || tierCapacity(costs, priced.tier, window) !== null);
  };
  return new Set(accounts.filter(sizes).map((a) => a.id));
}

const SHARED_WINDOWS = ['five_hour', 'seven_day'];
/** The hint's words for a window, a sentence's words apart from `shareLabel`'s ("of 5-hour") on the quota views, which
 *  the allowance's week hover reads too. */
export const HINT_WINDOW: Readonly<Record<string, string>> = { five_hour: 'the 5-hour window', seven_day: 'the 7-day window', seven_day_fable: 'the Fable week' };

/**
 * The hint's parts: the plan's label, then one line per window the route answered a share of, 5-hour, 7-day, then the
 * model week, "(in the band)" after the 5-hour share where `inBand` holds. A model week reads only for a tier
 * `MODEL_WEEK_TIERS` lists, since the cost model sizes the Fable week on Pro, which has no Fable access. The route
 * bounds the shares by the account's window set, so no line names a window the account lacks. No line where `shares`
 * is null or names no window, and a share the route carried as null names nothing.
 */
export function shareHintLines(plan: string, shares: Record<string, number | null> | null, tier: string | null, inBand: boolean | null): string[] {
  if (shares === null) return [];
  const weeks = Object.keys(shares).filter((key) => !SHARED_WINDOWS.includes(key) && tier !== null && (lookup(MODEL_WEEK_TIERS, key)?.includes(tier) ?? false));
  const lines = [...SHARED_WINDOWS, ...weeks].flatMap((key) => {
    const share = lookup(shares, key) ?? null;
    if (share === null) return [];
    const words = `${fmtShare(share)} of ${lookup(HINT_WINDOW, key) ?? key}`;
    return [key === 'five_hour' && inBand === true ? `${words} (in the band)` : words];
  });
  return lines.length === 0 ? [] : [plan, ...lines];
}
/**
 * The hint of a figure where one account is named, its parts joined: "Ultra: 0.11% of the 7-day window, 0.22% of the
 * Fable week". Null where `shares` is null or names no window.
 */
export function shareHint(plan: string, shares: Record<string, number | null> | null, tier: string | null, inBand: boolean | null): string | null {
  const [label, ...lines] = shareHintLines(plan, shares, tier, inBand);
  return label === undefined ? null : `${label}: ${lines.join(', ')}`;
}
/** The hint's plan label: the account's display name where the surface holds its profile, else the tier's label. */
export const hintPlan = (account: Pick<Account, 'subscription'> | undefined, tier: string | null): string | null => account?.subscription?.planDisplayName ?? (tier === null ? null : planLabel(tier));
/** An account's share hint at its own current tier, null for an account or a plan the console does not know. */
export const ownShareHint = (account: Pick<Account, 'subscription'> | undefined, shares: Record<string, number | null> | null): string | null => {
  const tier = account?.subscription?.rateLimitTier ?? null;
  const plan = hintPlan(account, tier);
  return plan === null ? null : shareHint(plan, shares, tier, null);
};

/**
 * One named account's shares over the groups the route answered them on, the Accounts filter naming that account:
 * each window summed over the groups, since every group is the one account's. A window carried as null is left out,
 * and so is a shared window that a group `required` to carry it lacks, since a partial sum would read as the
 * account's. A model week sums where a group carries one, a group off such a model carrying none. Null where no group
 * answered a map, and where a required group answered none, whatever the others priced.
 */
export function oneAccountShares(groups: AnalyticsGroup[], required: (g: AnalyticsGroup) => boolean): Record<string, number> | null {
  // The required test runs over every group before a missing map drops one, or the others would read as the whole.
  const answered = groups.map((g) => ({ map: g.costShares, required: required(g) }));
  if (answered.some((a) => a.map === null && a.required)) return null;
  const maps = answered.flatMap((a) => (a.map === null ? [] : [{ map: a.map, required: a.required }]));
  if (maps.length === 0) return null;
  const out: Record<string, number> = {};
  const left = new Set<string>();
  for (const { map, required: carries } of maps) {
    if (carries) for (const window of SHARED_WINDOWS) if (!has(map, window)) left.add(window);
    for (const [window, v] of Object.entries(map)) {
      if (v === null) left.add(window);
      else out[window] = has(out, window) ? out[window] + v : v;
    }
  }
  for (const window of left) delete out[window];
  return out;
}

/**
 * Whether a group carries chargeable plan usage, so it must carry both shared windows on its shares and equivalents and
 * one it lacks is a window the ledger could not price a plan row on. The rule the named account's whole reads, since
 * the shares carry no flag of their own. The ledger prices nothing on a row with no token, and of a group's plan
 * tokens the routes answer the output count alone, so a group whose plan rows wrote no output token is let off. A
 * group that carries no plan count has plan rows where its Pro-week figure is not zero, as `sidesOf` reads it.
 */
export const planRowsRequired = (g: AnalyticsGroup): boolean => (g.planRequests === null ? g.costEquivalent !== 0 : g.planRequests > 0 && g.planOutputTokens !== 0);
/**
 * The failed share of a group's requests, null for a group with no request, which the cells draw as a dash. The refused
 * requests are in neither count, since the fold never counted them.
 */
export const errorRateOf = (g: Pick<AnalyticsGroup, 'requests' | 'succeeded'>): number | null => (g.requests > 0 ? (g.requests - g.succeeded) / g.requests : null);
/**
 * The answered and the failed probes, the count a side's probe cost latches on. The wire's `failed` count also holds
 * the exhausted and rejected probes the relay refuses itself. A held or skipped probe never ran, so a group whose
 * probes were all held or skipped spent nothing on probes and its saving stands.
 */
export const upstreamProbes = (g: Pick<AnalyticsGroup, 'probes'>): number => g.probes.ok + g.probes.failed;
/**
 * The named account's whole: the position's totals with its shares summed over the groups the route answered them on.
 */
export const oneAccountGroupOf = (totals: AnalyticsGroup, groups: AnalyticsGroup[]): AnalyticsGroup => ({
  ...totals,
  costShares: oneAccountShares(groups, planRowsRequired),
});

/** The two counts summed, the one figure the charts and the by-pool table print per group. */
export const keptWarmTokens = (k: { tokens1h: number; tokens5m: number }): number => k.tokens1h + k.tokens5m;
/** The net saving, the gross less the probe cost, in whatever unit both figures came in. */
export const netSaving = (gross: number, probeCost: number): number => gross - probeCost;
/** The two counts' note: "12,345 tokens read warm, 8,000 at five minutes and 4,345 at an hour". */
export function keptWarmNote(k: { tokens1h: number; tokens5m: number }): string {
  return `${fmtInt(keptWarmTokens(k))} tokens read warm, ${fmtInt(k.tokens5m)} at five minutes and ${fmtInt(k.tokens1h)} at an hour`;
}

/** The positions a dollar figure of the Analytics page reads under. */
export type DollarPosition = Exclude<UnitPosition, 'native'>;
/** Native reads both sides of one answer with no funding filter, so its dollar figures are the whole's. */
export const dollarsOf = (unit: UnitPosition): DollarPosition => (unit === 'native' ? 'all' : unit);
/** A dollar position's requests, as the Cost chart's and table's subtitles and the first tile's note name them. */
export const UNIT_SUBSET: Record<DollarPosition, string> = { all: 'all requests', equivalent: "the subscription accounts' requests at the API price list", usd: 'the API key, fast mode, overage and usage-based requests at the API price list' };
/**
 * Native's requests, each side in the unit its funding pays in, as the Cost table's subtitle and the first tile's note
 * name them.
 */
export const NATIVE_SUBSET = 'the subscription requests as shares of the chosen week and the billed requests in dollars';
/** The Cost section's meta line per dollar position, after the dollars in range. */
const UNIT_META: Record<DollarPosition, string> = { all: 'all requests', equivalent: 'equivalent', usd: 'billed' };
/** The first Cost tile's label per dollar position. */
const UNIT_TILE_LABEL: Record<DollarPosition, string> = { all: 'Estimated cost', equivalent: 'Dollar equivalent', usd: 'Billed dollars' };

/** The chosen week a page's figures read in, and `scale` its factor over the Pro-week figure, null while the cost model
 *  is out or where it sizes no week for the chosen tier. `failure` is the settings read's failure while the scale is
 *  out, or the unpriced sentence where the cost model loaded without that week, which a figure draws in its skeleton's
 *  place. Its `unpriced` tells the second apart, which no retry clears, so a surface names it unpriced. */
export interface WeekReading {
  failure: { text: string; unpriced: boolean } | null;
  scale: number | null;
  week: WeekPosition;
}
/** A failed surface's head: "Unpriced: x" for the week reading's unpriced sentence, "Could not load x" for the rest. */
export const failureHead = (failure: { unpriced: boolean }, what: string): string => (failure.unpriced ? `Unpriced: ${what}` : `Could not load ${what}`);
/**
 * A Pro-week figure in the chosen week, `scale` its factor (`weekScale`), or a dash where the figure is null or the
 * side has no rows, as a dollar figure's `usdOrDash`. A dash reads unpriced and nothing else: a surface whose scale is
 * still out draws its skeleton and calls this with none.
 */
export const equivalentOrDash = (v: number | null | undefined, week: WeekPosition, scale: number): string => (v === null || v === undefined ? '-' : fmtWeek(v * scale, week));
/** Parts of a title as sentences, an absent part left out. */
export const sentences = (parts: (string | null)[]): string => joinPresent(parts, '. ');
/**
 * A native table cell, short enough that the table's columns fit its card: the figure in the chosen week's short form
 * ("0.0244% Ultra week") and for a cell that sums both fundings the billed dollars under it. `undefined` leaves out a
 * side with no rows, and null reads a dash for a side with nothing priced. The title carries the full reading, after
 * the hint where the cell is one account's.
 */
export function nativeCellOf(equivalent: number | null | undefined, billed: number | null | undefined, week: WeekPosition, scale: number, hint: string | null = null): { main: string; sub: string; title: string } {
  const sides = [
    ...(equivalent === undefined ? [] : [equivalent === null ? { full: null, short: '-' } : { full: fmtWeek(equivalent * scale, week), short: weekShort(equivalent * scale, week) }]),
    ...(billed === undefined ? [] : [billed === null ? { full: null, short: '-' } : { full: `${fmtUsd(billed)} billed`, short: fmtUsd(billed) }]),
  ];
  if (sides.length === 0) return { main: '-', sub: '', title: '' };
  const [main, second] = sides;
  const reading = sides.flatMap((s) => (s.full === null ? [] : [s.full])).join(', ');
  return { main: main.short, sub: second === undefined ? '' : (second.full ?? second.short), title: sentences([hint, reading]) };
}
/**
 * The two sides of a sum over both fundings: the plan rows' Pro-week figure and the billed rows' dollars, in
 * `nativeCellOf`'s vocabulary: undefined leaves out a side with no rows, and a null figure reads a dash for plan rows
 * the route priced nothing on. The route and the session fold count the plan requests, and a group that carries no
 * count tells the sides apart by their figures. A sum with no row on either side reads both, so an empty range reads
 * its zero in each unit rather than nothing.
 */
export function sidesOf(g: Pick<AnalyticsGroup, 'billedUsd' | 'costEquivalent' | 'planRequests' | 'requests'>): { billed: number | undefined; equivalent: number | null | undefined } {
  const plan = g.planRequests === null ? g.costEquivalent !== 0 : g.planRequests > 0;
  const billed = g.planRequests === null ? g.billedUsd > 0 : g.requests > g.planRequests;
  return plan || billed ? { billed: billed ? g.billedUsd : undefined, equivalent: plan ? g.costEquivalent : undefined } : { billed: g.billedUsd, equivalent: g.costEquivalent ?? 0 };
}

/** One figure of a Cost tile, with the word naming its side where the tile stacks two. */
export interface CostFigure {
  text: string;
  word?: string;
}
/** The dash's title where a plan row priced nothing. */
export const UNPRICED_TITLE = 'No subscription request priced: its model has no weight in the cost model';
/**
 * A sum's native figures, each with its funding's word: the plan rows' figure in the chosen week, then the billed rows'
 * dollars. The sides read as `sidesOf` answers them: a side undefined for want of rows is left out, and a sum with no
 * row on either side reads both, so it shows its zero in each unit rather than nothing. A null figure is a plan row
 * that priced nothing, so the subscription side reads the dash whatever the other rows summed, since a partial sum would
 * read as the whole, and `unpriced` says so, so the figure's element takes `UNPRICED_TITLE`.
 */
export function nativeFiguresOf(sides: { billed: number | undefined; equivalent: number | null | undefined }, week: WeekPosition, scale: number): { figures: CostFigure[]; unpriced: boolean } {
  const unpriced = sides.equivalent === null;
  const none = sides.billed === undefined && sides.equivalent === undefined;
  const equivalent = none ? 0 : sides.equivalent;
  const billed = none ? 0 : sides.billed;
  return {
    figures: [...(equivalent === undefined ? [] : [{ text: equivalentOrDash(equivalent, week, scale), word: 'subscription' }]), ...(billed === undefined ? [] : [{ text: fmtUsd(billed), word: 'billed' }])],
    unpriced,
  };
}
/**
 * A side pair in one line, the plan side's figure in the chosen week and the billed side's dollars marked billed, as a
 * tooltip or a title reads it.
 */
export const sidesLine = (sides: { billed: number | undefined; equivalent: number | null | undefined }, week: WeekPosition, scale: number): string =>
  nativeFiguresOf(sides, week, scale)
    .figures.map((f) => (f.word === 'billed' ? `${f.text} billed` : f.text))
    .join(', ');
/** A dollar position's dollars of a sum: every row's, the billed rows', or the plan rows' at the API price list. */
export const dollarsUnder = (unit: DollarPosition, s: FundedSum): number => (unit === 'all' ? s.cost : unit === 'usd' ? s.billedUsd : s.cost - s.billedUsd);
/**
 * A sum's two sides, each undefined where it has no rows, so a sum of one funding shows one figure and no "0%" side.
 * A null figure is a plan row priced nothing, a side with rows whatever the other rows priced, and a zero reads as no
 * plan rows.
 */
export const nativeSidesOf = (s: FundedSum): { billed: number | undefined; equivalent: number | null | undefined } => ({
  billed: s.billedUsd > 0 ? s.billedUsd : undefined,
  equivalent: s.equivalent === null || s.equivalent > 0 ? s.equivalent : undefined,
});
/** Several sums as one. One that priced nothing nulls the total, since the priced ones alone would read as the whole. */
export const sumFunded = (sums: readonly FundedSum[]): FundedSum => ({
  billedUsd: sums.reduce((s, x) => s + x.billedUsd, 0),
  cost: sums.reduce((s, x) => s + x.cost, 0),
  equivalent: sums.reduce<number | null>((s, x) => (s === null || x.equivalent === null ? null : s + x.equivalent), 0),
});
/**
 * The Cost by chart's note on the groups it draws as gaps: one label ("research unpriced in 3 buckets, drawn as gaps",
 * "1 bucket, drawn as a gap"), up to three joined by middle dots, a count past three ("5 groups unpriced in 12 buckets,
 * drawn as gaps"), null where no bucket is a gap. `labels` are the unpriced groups' labels in the chart's order.
 */
export function unpricedNote(labels: readonly string[], buckets: number): string | null {
  if (labels.length === 0 || buckets === 0) return null;
  // The middle dot joins the labels, since a deleted holder's label carries a comma of its own ("desktop (deleted,
  // created Oct 11, 2026, 12:15)") and the console already sets a bar's share and its reset apart with it.
  const named = labels.length > 3 ? `${fmtInt(labels.length)} groups` : labels.join(' · ');
  return `${named} unpriced in ${fmtCount(buckets, 'bucket, drawn as a gap', 'buckets, drawn as gaps')}`;
}
/** The Cost section's head under a position, as `costHeadOf` reads it. */
export interface CostHead {
  meta: string;
  /** The requests the Cost table's subtitle names. */
  subset: string;
  /** The first tile. */
  tile: { figures: CostFigure[]; label: string; note: string };
  /** Whether the subscription side reads the dash of a plan row the totals priced nothing on, so the head's elements
   *  take `UNPRICED_TITLE`. */
  unpriced: boolean;
}
/**
 * The Cost section's meta line, its first tile and the requests its table reads, from the position's totals. Native
 * pairs the plan rows' figure in the chosen week with the billed dollars, a side with no rows left out and a dash
 * where the route priced none. It waits on `scale`, so null answers native while the scale is out, which the section
 * draws as its skeleton. A dollar position reads its dollars under its own words, and reads no scale.
 */
export function costHeadOf(unit: UnitPosition, totals: AnalyticsGroup, week: WeekPosition, scale: number | null): CostHead | null {
  if (unit === 'native') {
    if (scale === null) return null;
    const { figures, unpriced } = nativeFiguresOf(sidesOf(totals), week, scale);
    return { meta: `${figures.map((f) => f.text).join(' and ')} in range`, subset: NATIVE_SUBSET, tile: { figures, label: 'Cost', note: NATIVE_SUBSET }, unpriced };
  }
  const dollars = dollarsOf(unit);
  const figure = fmtUsd(totals.costUsd);
  return { meta: `${figure} in range, ${UNIT_META[dollars]}`, subset: UNIT_SUBSET[dollars], tile: { figures: [{ text: figure }], label: UNIT_TILE_LABEL[dollars], note: dollars === 'all' ? 'at the effective price list' : UNIT_SUBSET[dollars] }, unpriced: false };
}

/**
 * A group's color slot among the range's keys, the empty key skipped so the first named key takes slot 0. The empty
 * key, a key not listed and any key past the eighth take slot 7.
 */
export function slotIndex(keys: readonly string[], key: string): number {
  const i = key === '' ? -1 : keys.filter((k) => k !== '').indexOf(key);
  return Math.min(7, i < 0 ? 7 : i);
}

/** The extra-usage week, offered only while a row holds a sample of it, since the heads alone write it. */
const EXTRA_USAGE_WINDOW = 'seven_day_overage_included';
/** The window ids the Accounts cards offer, in order, the extra-usage week only while a row holds it. */
export function sampledWindows(rollups: AccountRollup[]): string[] {
  return rollups.some((r) => r.windows[EXTRA_USAGE_WINDOW] !== undefined) ? [...WINDOW_KEYS, EXTRA_USAGE_WINDOW] : [...WINDOW_KEYS];
}

/**
 * The Pace score beside a lead: the remaining share over the hours from the sample's instant to the reset, floored at
 * nothing remaining, the ratio the routing preview scores an account by (the preview adds its own deadline cap and
 * minute floor over the engine's prediction rather than a raw sample). Null without a reset or a utilization, and null
 * for a reset at or before the sample, since the window has reset since and left the sample stale.
 */
export const paceScoreOf = (sample: WindowSample): number | null => {
  if (sample.resetsAt === null || sample.utilization === null) return null;
  const hours = (Date.parse(sample.resetsAt) - Date.parse(sample.sampledAt)) / HOUR;
  return hours > 0 ? Math.max(1 - sample.utilization, 0) / hours : null;
};

/**
 * One row per bucket from the bucket holding the span's start through its end, each account's keys at null, the rows
 * by their start, and the floor from an instant to its bucket's start, so the charts that bucket one span draw on one
 * grid.
 */
function bucketRows(accounts: string[], span: { fromMs: number; toMs: number }, bucketMs: number, keysOf: (account: string) => string[] = (a) => [a]): { bucketOf: (ms: number) => number; byAt: Map<number, Record<string, number | null>>; data: Record<string, number | null>[] } {
  const bucketOf = (ms: number) => Math.floor(ms / bucketMs) * bucketMs;
  const start = bucketOf(span.fromMs);
  const data: Record<string, number | null>[] = [];
  const byAt = new Map<number, Record<string, number | null>>();
  for (let t = start; t <= span.toMs; t += bucketMs) {
    const row: Record<string, number | null> = { at: t };
    for (const a of accounts) for (const key of keysOf(a)) row[key] = null;
    data.push(row);
    byAt.set(t, row);
  }
  return { bucketOf, byAt, data };
}

/**
 * Per account per bucket of the span the chosen window's utilization (`usage`) or lead over pace (`pace`) in percent,
 * null for a gap: a bucket no hour of which holds a sample of the window, or a sample without the figure (a
 * reset-only sample has neither). A bucket wider than an hour reads the latest hour inside it that holds a sample.
 * Under `pace` each row also carries `${account}:score`, the Pace score of the sample the lead was read from, null
 * where the lead is, so the tooltip reads both off the row.
 */
export function windowLines(rollups: AccountRollup[], window: string, read: 'pace' | 'usage', span: { fromMs: number; toMs: number }, bucketMs: number): { accounts: string[]; data: Record<string, number | null>[] } {
  const accounts = [...new Set(rollups.map((r) => r.accountId))].sort((a, b) => a.localeCompare(b));
  const { bucketOf, byAt, data } = bucketRows(accounts, span, bucketMs, read === 'pace' ? (a) => [a, `${a}:score`] : undefined);
  const drawn = new Map<string, number>();
  for (const r of rollups) {
    const sample = r.windows[window];
    if (sample === undefined) continue;
    const hourMs = Date.parse(r.hour);
    const at = bucketOf(hourMs);
    const row = byAt.get(at);
    if (row === undefined) continue;
    const key = `${at}:${r.accountId}`;
    if ((drawn.get(key) ?? -Infinity) > hourMs) continue;
    drawn.set(key, hourMs);
    const figure = read === 'usage' ? sample.utilization : sample.lead;
    row[r.accountId] = figure === null ? null : figure * 100;
    if (read === 'pace') row[`${r.accountId}:score`] = sample.lead === null ? null : paceScoreOf(sample);
  }
  return { accounts, data };
}

/**
 * Per account per bucket of the span the highest 5-hour peak in percent, null for a bucket no peak of the account
 * falls in.
 */
export function peakLines(rollups: AccountRollup[], accounts: string[], span: { fromMs: number; toMs: number }, bucketMs: number): Record<string, number | null>[] {
  const { bucketOf, byAt, data } = bucketRows(accounts, span, bucketMs);
  for (const r of rollups) {
    const row = byAt.get(bucketOf(Date.parse(r.hour)));
    const peak = r.peak['5h'];
    if (!row || peak === null) continue;
    row[r.accountId] = Math.max(row[r.accountId] ?? 0, peak * 100);
  }
  return data;
}

/** Each value weighed by its weight, so a bucket counts for the part of its width it covers. */
const weighedMean = (pairs: { value: number; weight: number }[]) => pairs.reduce((n, p) => n + p.value * p.weight, 0) / pairs.reduce((n, p) => n + p.weight, 0);
/** The share of its width a bucket covers before it sets the axis and the peak: 6 minutes of an hour, 36 of six. */
const SCALE_FLOOR = 0.1;

const LEGEND_ORDER: ClassLegend[] = ['conversation', 'overhead'];

/**
 * The burn route's points as chart lines and rows. The request-class grouping folds into its two legend entries, and
 * every other grouping keeps the groups with the most tokens and sums the rest as other, which draws no plan line, so
 * its plan figure stays empty. Under the account grouping the rest folds once per unit, since a subscription account's
 * plan share and an API-key account's dollars chart on different axes. Where each group holds one account's rows (that
 * grouping, or `account`, the one account the Accounts filter names) a line's unit is its account's. `sized` names the
 * accounts whose plan the router prices on a tier the cost model sizes (`pricedTier`, so a free plan and a usage-based
 * seat are out). Each slot carries the Pro-week figure per hour, which adds across accounts and tiers.
 *
 * `rangeEndMs` is the end of the range the points were read for and `nowMs` the instant of that read, the end when
 * absent. The route reads whole hours through the hour ceiling of the end, so a bucket covers its width up to the hour
 * ceiling of the range's end, no later than the read's instant, and its full width without an end.
 */
export function foldBurn(
  points: BurnPoint[],
  groupBy: BurnGroup,
  bucketSeconds: number,
  sized: ReadonlySet<string>,
  rangeEndMs?: number,
  nowMs = rangeEndMs,
  options: { account?: string | null } = {},
): { lines: BurnLine[]; rows: BurnRow[] } {
  const account = options.account ?? null;
  const lineOf = (group: string) => (groupBy === 'request_class' ? classLegendOf(group) : group);
  const tokens = new Map<string, number>();
  for (const p of points) tokens.set(lineOf(p.group), (tokens.get(lineOf(p.group)) ?? 0) + p.cachedTokens + p.uncachedTokens);
  const byAccount = groupBy === 'account';
  const shared = byAccount || account !== null;
  // Keyed by the line, so the request class grouping's two folded lines read their classes' unit.
  const subscribed = new Set(shared ? points.filter((p) => p.unit === 'window_share').map((p) => lineOf(p.group)) : []);
  // A subscription line charts in the plan's unit where the account is in the sized set or its points carry a figure:
  // the sized set keeps a line whose range ran on unpriced models alone, its gaps shown, and a figure keeps the buckets
  // the ledger priced from the tier on their rows for an account the router no longer sizes, such as a plan that ended.
  const figured = new Set(points.filter((p) => p.equivalent !== null).map((p) => lineOf(p.group)));
  const unitOf = (group: string): BurnLineUnit => (subscribed.has(group) ? (sized.has(byAccount ? group : (account ?? '')) || figured.has(group) ? 'window_share' : 'unknown') : 'usd');
  const ranked = [...tokens.entries()].sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0])).map(([k]) => k);
  const kept: string[] = groupBy === 'request_class' ? LEGEND_ORDER : ranked.slice(0, MAX_BURN_LINES);
  const folded = groupBy === 'request_class' ? [] : ranked.slice(MAX_BURN_LINES);
  const rests = (['window_share', 'unknown', 'usd'] as const).filter((unit) => folded.some((g) => unitOf(g) === unit));
  // The rests take the slots after the kept lines, so a group whose id happens to read `other` stays its own line.
  const slots = kept.length + rests.length;
  const slotOf = (group: string) => (kept.includes(lineOf(group)) ? kept.indexOf(lineOf(group)) : kept.length + rests.indexOf(unitOf(group)));

  const byAt = new Map<number, Sum[]>();
  for (const p of points) {
    const at = Date.parse(p.at);
    const sums = byAt.get(at) ?? Array.from({ length: slots }, (): Sum => ({ cached: 0, equivalent: { sum: null, unpriced: false }, tokens: 0, uncached: 0, usd: 0 }));
    byAt.set(at, sums);
    const slot = slotOf(p.group);
    const s = sums[slot];
    s.cached += p.cachedTokens;
    s.tokens += p.tokens;
    s.uncached += p.uncachedTokens;
    s.usd += p.usd;
    if (slot >= kept.length) continue;
    if (p.equivalent !== null) s.equivalent.sum = (s.equivalent.sum ?? 0) + p.equivalent;
    if (p.unpriced) s.equivalent.unpriced = true;
  }
  const secondsOf = (at: number) => {
    if (rangeEndMs === undefined) return bucketSeconds;
    const covered = Math.min(at + bucketSeconds * 1_000, hourCeil(rangeEndMs), nowMs ?? rangeEndMs);
    return Math.max(1, (covered - at) / 1_000);
  };
  const perHour = (f: FigureFold, hours: number) => (f.unpriced || f.sum === null ? null : f.sum / hours);
  const buckets = [...byAt.entries()].sort((a, b) => a[0] - b[0]);
  const rows = buckets
    .map(([at, sums]): BurnRow => {
      const seconds = secondsOf(at);
      const minutes = seconds / 60;
      const hours = seconds / 3_600;
      return {
        at,
        cached: sums.map((s) => s.cached / minutes),
        equivalent: sums.map((s) => perHour(s.equivalent, hours)),
        partial: seconds < bucketSeconds,
        seconds,
        setsScale: seconds >= bucketSeconds * SCALE_FLOOR,
        tokens: sums.map((s) => s.tokens),
        uncached: sums.map((s) => s.uncached / minutes),
        usd: sums.map((s) => s.usd / minutes),
      };
    });
  // Weighed by the share of its width a bucket covers rather than its seconds: the same mean, and a range of full
  // buckets reads the plain mean to the last bit.
  const weightOf = (r: BurnRow) => r.seconds / bucketSeconds;

  const mean = (read: (r: BurnRow) => number) => (rows.length ? weighedMean(rows.map((r) => ({ value: read(r), weight: weightOf(r) }))) : 0);
  const lines = Array.from({ length: slots }, (_, i): BurnLine => {
    const rest = i >= kept.length ? rests[i - kept.length] : null;
    // A line that never priced traffic stays null in every slot, idle ones included, so its legend reads a dash. One
    // that priced counts an idle slot as zero and keeps an unpriced one null. An idle slot's own figure never makes the
    // line priced: a failed request's row carries no token and a listed model prices it at zero. The route flags a slot
    // it could not price, so a slot with no token or with no figure and no flag is idle.
    const flagged = (k: number) => buckets[k][1][i].equivalent.unpriced;
    const idle = (r: BurnRow, k: number) => r.tokens[i] === 0 || (r.equivalent[i] === null && !flagged(k));
    const priced = rows.some((r) => r.tokens[i] > 0 && r.equivalent[i] !== null);
    rows.forEach((r, k) => {
      if (idle(r, k)) r.equivalent[i] = priced ? 0 : null;
    });
    const figures = rows.flatMap((r) => (r.equivalent[i] === null ? [] : [{ value: r.equivalent[i] as number, weight: weightOf(r) }]));
    const peaks = rows.filter((r) => r.setsScale).map((r) => r.equivalent[i]).filter((v): v is number => v !== null);
    const figuresOf = {
      peak: peaks.length ? Math.max(...peaks) : null,
      perHour: priced ? weighedMean(figures) : null,
      tokensPerMinute: mean((r) => r.cached[i] + r.uncached[i]),
      unpriced: rest === null && rows.some((_, k) => flagged(k)),
      usdPerMinute: mean((r) => r.usd[i]),
    };
    if (rest === null) return { ...figuresOf, key: kept[i], label: burnLabelOf(groupBy, kept[i]), other: false, unit: unitOf(kept[i]) };
    return { ...figuresOf, key: byAccount ? `${OTHER_KEY}-${rest}` : OTHER_KEY, label: byAccount ? OTHER_LABEL[rest] : 'Other', other: true, unit: rest };
  });
  return { lines, rows };
}

/**
 * A capped axis's top raised to the last tick recharts would draw over [0, top] with `ticks` ticks: handed [0, top] as
 * it stands, recharts closes the round ticks with `top` itself as one more, unround tick.
 */
export function niceAxisTop(top: number, ticks = 5): number {
  if (!(top > 0)) return top;
  // recharts-scale's step: the rough step over its power of ten, that ratio raised to the next 0.05, or 0.1 when the
  // rough step has one whole digit.
  const rough = top / (ticks - 1);
  const digits = Math.floor(Math.log10(rough)) + 1;
  const unit = 10 ** digits;
  const scale = digits === 1 ? 0.1 : 0.05;
  // Rounded before the ceiling, so a quotient a float's error over a whole step does not climb to the next one.
  const step = Math.ceil(Number((rough / unit / scale).toFixed(9))) * scale * unit;
  return Number((step * (ticks - 1)).toPrecision(12));
}

/** A placement rung as the request drawer words its placed phrase. */
export const PLACEMENT_RUNG_LABEL: Record<Placement['rung'], string> = { content: 'by its messages', static: 'by its static prefix', with_main: 'with the main conversation' };
/** A rung the console has no label for reads as the router sent it. */
export const placementRungLabel = (rung: string): string => lookup(PLACEMENT_RUNG_LABEL, rung) ?? rung;

/**
 * A placements row's figures in column order: placements, hits, hit share, tokens read, tokens promised. A rung
 * with no placement has no share, and a placement beside the main conversation names no entry and so no size.
 */
export const placementCells = (row: PlacementRow): string[] => [
  fmtInt(row.placements),
  fmtInt(row.hits),
  row.hitShare === null ? '-' : fmtPct(row.hitShare),
  fmtCompact(row.cacheReadTokens),
  row.rung === 'with_main' ? '-' : fmtCompact(row.promisedTokens),
];

/** The least rise a ratio counts after: the readings move in whole percents, so a smaller rise makes a coarse ratio. */
export const MIN_RISE = 0.05;
/** The band a completed window's ratio agrees with the table inside, symmetric in log terms, as the ledger reads it. */
export const RATIO_HIGH = 1.25;
export const RATIO_LOW = 0.8;
/** Whether the band judges a row: the rise and the priced share each reach MIN_RISE. */
export const judged = (row: Pick<Reconciliation, 'observedRise' | 'predictedShare'>): boolean => row.observedRise >= MIN_RISE && row.predictedShare >= MIN_RISE;
/**
 * Off once the rise and the priced share each reach MIN_RISE and the ratio leaves the band, as the engine badges it.
 */
export const ratioOff = (row: Pick<Reconciliation, 'observedRise' | 'predictedShare' | 'ratio'>): boolean => judged(row) && (row.ratio < RATIO_LOW || row.ratio > RATIO_HIGH);

/** The patch the bar commits: a patch that moves the session drops the key lifetime and the agent, since each names one
 *  session's. */
export function analyticsPatch(
  current: Pick<AnalyticsAddress, 'session'>,
  patch: Partial<AnalyticsAddress>,
): Partial<AnalyticsAddress> {
  return 'session' in patch && patch.session !== current.session ? { ...patch, agentId: '', key: '' } : patch;
}
