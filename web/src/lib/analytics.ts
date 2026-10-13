import type { Api } from '@/api/client';
import type { Account, AccountRollup, AnalyticsGroup, AnalyticsGroupBy, AnalyticsPoint, AnalyticsQuery, BurnGroup, BurnPoint, BurnUnit, PinMigrationReason, Placement, PlacementRow, SubscriptionCosts, WindowSample } from '@/api/types';
import { tierCapacity, tierRow, type CostWindow } from '@/lib/costs';
import { equivalentShort, fmtCompact, fmtEquivalent, fmtInt, fmtPct, fmtShare, fmtUsd, withEstimate } from '@/lib/format';
import type { UnitPosition } from '@/lib/unit';
import { WINDOW_LABEL, windowKeyId } from '@/lib/windows';

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
/**
 * Whether a label map holds the name as its own key. The names are the router's data, and one could name a member
 * every object inherits.
 */
export const own = (map: object, key: string): boolean => Object.prototype.hasOwnProperty.call(map, key);
/** A reason the console has no label for reads as the router sent it. */
export const moveLabel = (reason: string): string => (own(PIN_MIGRATION_LABEL, reason) ? PIN_MIGRATION_LABEL[reason as PinMigrationReason] : reason);

/** The classes that refresh a pin's warmth, the routing crate's `REFRESHING_CLASSES`: the conversation itself. */
export const CONVERSATION_CLASSES = ['compaction', 'main', 'subagent'] as const;

export type ClassLegend = 'conversation' | 'overhead';

/** Conversation or the overhead around it; `-`, the rollups' name for a request that sent no class, is overhead. */
export const classLegendOf = (requestClass: string): ClassLegend => ((CONVERSATION_CLASSES as readonly string[]).includes(requestClass) ? 'conversation' : 'overhead');

/** Lines a burn chart draws before the rest fold into one, the stacked cost chart's limit. */
export const MAX_BURN_LINES = 7;
/** The key the folded rest carries, suffixed by its unit under the account grouping; no group is looked up by it. */
export const OTHER_KEY = 'other';
/**
 * The unit a burn line charts in. `unknown` is a subscription account the cost model does not size, its tier missing
 * or one the model has no capacity row for: it has no share to draw, and dollars only for what its fast-mode and
 * overage answers spent.
 */
export type BurnLineUnit = BurnUnit | 'unknown';
const OTHER_LABEL: Record<BurnLineUnit, string> = { unknown: 'Other (tier unknown)', usd: 'Other (dollars)', window_share: 'Other (plan)' };

export interface BurnLine {
  /** The highest 5-hour share per hour the range reached; null for a line with no share in any bucket. */
  fiveHourPeak: number | null;
  /** The range's 5-hour share per hour over the buckets that read one; null on a window the line never priced. */
  fiveHourPerHour: number | null;
  key: string;
  label: string;
  /** Whether this is the folded rest rather than one group. */
  other: boolean;
  sevenDayPerHour: number | null;
  tokensPerMinute: number;
  /**
   * The unit the line is charted in: under the account grouping a subscription account burns window share and an
   * API-key one dollars; every other grouping mixes both, so it charts dollars.
   */
  unit: BurnLineUnit;
  /**
   * Per window, whether a bucket with tokens has no share on it: a model the cost model has no weight for, or a tier it
   * has no capacity for on that window. False on a line that does not chart window share.
   */
  unpriced: { fiveHour: boolean; sevenDay: boolean };
  usdPerMinute: number;
}

/**
 * One bucket, indexed like the lines: the cached and uncached tokens and the dollars per minute of the time it covers,
 * its raw token count, and the shares per hour.
 */
export interface BurnRow {
  at: number;
  cached: number[];
  /**
   * Null where the slot's traffic could not be priced, or on a window the line never priced; an idle slot reads zero
   * on a window the line priced.
   */
  fiveHour: (number | null)[];
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
  sevenDay: (number | null)[];
  /**
   * Every token of the slot, all five types, as the route counts them: zero for a slot with no traffic the ledger can
   * attribute.
   */
  tokens: number[];
  uncached: number[];
  usd: number[];
}

interface Sum {
  cached: number;
  fiveHour: number | null;
  sevenDay: number | null;
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
 * Whether a window-share slot holds traffic the table could not price: a null share with tokens behind it. An idle
 * slot on a window the line never priced reads null too, with no traffic to list as model unlisted.
 */
export const slotUnpriced = (share: number | null, tokens: number): boolean => share === null && tokens > 0;

/**
 * Which slots hold a share with no share beside them on either side, the series' ends counting as null: a line that
 * leaves its null slots unconnected draws such a slot as a zero-length path, so the chart marks it with a dot.
 */
export function isolatedSlots(values: (number | null)[]): boolean[] {
  return values.map((v, i) => v !== null && (values[i - 1] ?? null) === null && (values[i + 1] ?? null) === null);
}

/**
 * The burn route's answer with the accounts beside it, read together: a profile refresh can supply a tier the last
 * read lacked, and the plan chart sizes each account from the tiers of the read that answered its buckets. Only the
 * account grouping sizes its lines, so another grouping reads no account and a failed account read leaves it whole.
 */
export async function readBurn(
  source: Pick<Api, 'getAnalyticsBurn' | 'listAccounts'>,
  range: Pick<AnalyticsQuery, 'from' | 'to'>,
  groupBy: BurnGroup,
): Promise<{ accounts: Account[]; answer: { bucketSeconds: number; points: BurnPoint[] } }> {
  const [answer, accounts] = await Promise.all([source.getAnalyticsBurn(range, groupBy), groupBy === 'account' ? source.listAccounts() : Promise.resolve<Account[]>([])]);
  return { accounts, answer };
}

/**
 * The accounts whose stored tier the cost model sizes, matched as the ledger matches it: as stored, then with the
 * `default_` prefix taken off or put on, since only one spelling has been seen on the wire. The plan chart reads a
 * listed tier as sized and draws a window it has no capacity on as model unlisted. With `window`, a tier sizes an
 * account only where it has a capacity on that window, as the shed preview needs for the 5-hour window: the sessions
 * route reads no burn for a pin of a tier without one.
 */
export function sizedAccounts(accounts: Account[], costs: SubscriptionCosts, window?: CostWindow): Set<string> {
  const sizes = (tier: string) => (window === undefined ? tierRow(costs, tier) !== undefined : tierCapacity(costs, tier, window) !== null);
  return new Set(accounts.filter((a) => a.subscription !== null && sizes(a.subscription.rateLimitTier)).map((a) => a.id));
}

export const SHARE_NOTE = "Shares read under the account grouping alone, since shares of different accounts' windows do not add.";
/** The cache card's empty note under Window shares: the card needs one account named as well as the grouping. */
export const CACHE_SHARE_NOTE = "Shares of one account's windows alone: name one account in the Accounts filter under the account grouping.";
/**
 * Cache by model's note under Window shares with no account named: the route answers one named account's shares by
 * model under any grouping, so the filter alone is needed.
 */
export const CACHE_MODEL_SHARE_NOTE = "Shares of one account's windows alone: name one account in the Accounts filter.";
/**
 * The one account whose shares the tiles read: the account filter's one entry under the account grouping, and null
 * for none or several, where the tiles read "per account below".
 */
export const oneAccountOf = (groupBy: AnalyticsGroupBy, accountIds: string[]): string | null => (groupBy === 'account' && accountIds.length === 1 ? accountIds[0] : null);

const SHARED_WINDOWS = ['five_hour', 'seven_day'];
/** The model week's key in a share map, the one key beside the two shared windows, undefined where the map has none. */
export const weekKeyOf = (shares: Record<string, unknown>): string | undefined => Object.keys(shares).find((k) => !SHARED_WINDOWS.includes(k));
/**
 * The model week's label for a share map's key: the window label of the key, so a second model week reads under its
 * own name, and the Fable week's where no map names one.
 */
export const weekLabelOf = (key: string | undefined): string => (key === undefined ? WINDOW_LABEL['7d_fable'] : (WINDOW_LABEL[windowKeyId(key) ?? ''] ?? key));
/** The share table's model week column, named by the week keys its groups carry, each distinct week once. */
export const weekColumnOf = (groups: Pick<AnalyticsGroup, 'costShares'>[]): string => {
  const labels = [...new Set(groups.map((g) => (g.costShares === null ? undefined : weekKeyOf(g.costShares))).filter((k): k is string => k !== undefined).map(weekLabelOf))];
  return labels.length ? labels.join(', ') : weekLabelOf(undefined);
};
/** A window's share as the map carries it: null where absent or carried as null. */
const windowShare = (shares: Record<string, number | null> | null, key: string | undefined): number | null => (shares === null || key === undefined || !own(shares, key) ? null : shares[key]);

/**
 * The share table's cells per group: the 5-hour, 7-day and model week shares, the 7-day share per plan-funded
 * request and per 1k plan-funded output tokens (`planRequests` and `planOutputTokens`, the rows the shares were
 * summed over, so a billed request never widens the denominator), each a label or null where the route answered
 * none. The headers name the windows, so a cell is the bare percent.
 */
export function shareRow(g: AnalyticsGroup): { fiveHour: string | null; perRequest: string | null; perThousand: string | null; sevenDay: string | null; week: string | null } {
  const sevenDay = windowShare(g.costShares, 'seven_day');
  const cell = (share: number | null) => (share === null ? null : fmtShare(share));
  const per = (count: number | null, scale: number) => (sevenDay === null || count === null || !(count > 0) ? null : fmtShare(sevenDay / (count / scale)));
  return {
    fiveHour: cell(windowShare(g.costShares, 'five_hour')),
    perRequest: per(g.planRequests, 1),
    perThousand: per(g.planOutputTokens, 1000),
    sevenDay: cell(sevenDay),
    week: cell(windowShare(g.costShares, g.costShares === null ? undefined : weekKeyOf(g.costShares))),
  };
}

/** The positions a dollar figure of the Analytics page reads under. */
export type DollarPosition = Exclude<UnitPosition, 'native' | 'share'>;
/**
 * Native reads both sides of one answer with no funding filter, so its dollar figures are the whole's, as the dollar
 * figures drawn under the shares are.
 */
export const dollarsOf = (unit: UnitPosition): DollarPosition => (unit === 'native' || unit === 'share' ? 'all' : unit);
/** A dollar position's requests, as the Cost chart's and table's subtitles and the first tile's note name them. */
export const UNIT_SUBSET: Record<DollarPosition, string> = { all: 'all requests', equivalent: "the subscription accounts' requests at the API price list", usd: 'the overage, fast mode and API key requests at the API price list' };
/**
 * Native's requests, each side in the unit its funding pays in, as the Cost table's subtitle and the first tile's note
 * name them.
 */
export const NATIVE_SUBSET = 'the subscription requests in 20x window equivalents and the billed requests in dollars';
/** The Cost section's meta line per dollar position, after the dollars in range. */
const UNIT_META: Record<DollarPosition, string> = { all: 'all requests', equivalent: 'equivalent', usd: 'billed' };
/** The first Cost tile's label per dollar position. */
const UNIT_TILE_LABEL: Record<DollarPosition, string> = { all: 'Estimated cost', equivalent: 'Dollar equivalent', usd: 'Billed dollars' };

/**
 * A 20x window equivalent with its window named and the tilde before it where an estimate priced it, or a dash where
 * the window holds none, as a dollar figure's `usdOrDash`. A dash is no figure, so no estimate marks it.
 */
export const equivalentOrDash = (windowKey: string, v: number | null | undefined, estimated = false): string => (v === null || v === undefined ? '-' : `${estimated ? '~' : ''}${fmtEquivalent(windowKey, v)}`);
/**
 * A native table cell, short enough that the table's columns fit its card: the 20x equivalent in its window's short
 * word ("0.244% 20x 7d"), the tilde before an estimated figure, and for a cell that sums both fundings the billed
 * dollars under it. `undefined` leaves out a side with no rows, and null reads a dash for a side with nothing priced.
 * The title carries the full reading and the estimate.
 */
export function nativeCellOf(windowKey: string, equivalent: number | null | undefined, estimated: boolean, billed?: number | null): { main: string; sub: string; title: string } {
  const marked = estimated && equivalent !== null && equivalent !== undefined;
  const sides = [
    ...(equivalent === undefined ? [] : [equivalent === null ? { full: null, short: '-' } : { full: fmtEquivalent(windowKey, equivalent), short: `${marked ? '~' : ''}${equivalentShort(windowKey, equivalent)}` }]),
    ...(billed === undefined ? [] : [billed === null ? { full: null, short: '-' } : { full: `${fmtUsd(billed)} billed`, short: fmtUsd(billed) }]),
  ];
  if (sides.length === 0) return { main: '-', sub: '', title: '' };
  const [main, second] = sides;
  return { main: main.short, sub: second === undefined ? '' : second.full ?? second.short, title: withEstimate(sides.flatMap((s) => (s.full === null ? [] : [s.full])).join(', '), marked) };
}
/**
 * The two sides of a sum over both fundings: the plan rows' 20x equivalent on a window and the billed rows' dollars,
 * a side with no rows null. The route counts the plan requests, and a fold of request rows that counted none tells the
 * sides apart by their figures. A sum with no row on either side reads both, so an empty range reads its zero in each
 * unit rather than nothing.
 */
export function sidesOf(g: Pick<AnalyticsGroup, 'billedUsd' | 'costEquivalent' | 'planRequests' | 'requests'>, window: string): { billed: number | null; equivalent: number | null } {
  const plan = g.planRequests === null ? Object.keys(g.costEquivalent).length > 0 : g.planRequests > 0;
  const billed = g.planRequests === null ? g.billedUsd > 0 : g.requests > g.planRequests;
  const equivalent = own(g.costEquivalent, window) ? g.costEquivalent[window] : 0;
  return plan || billed ? { billed: billed ? g.billedUsd : null, equivalent: plan ? equivalent : null } : { billed: g.billedUsd, equivalent };
}

/** One figure of a Cost tile, with the word naming its side where the tile stacks two. */
export interface CostFigure {
  text: string;
  word?: string;
}
/** The Cost section's head under a position, as `costHeadOf` reads it. */
export interface CostHead {
  /** Whether a figure of the head carries the estimated mark, so its element takes the estimate's title. */
  estimated: boolean;
  meta: string;
  /** The requests the Cost table's subtitle names, null under the shares, whose table names its own. */
  subset: string | null;
  /** The first tile, null under the shares, which read per account. */
  tile: { figures: CostFigure[]; label: string; note: string } | null;
}
/**
 * The Cost section's meta line, its first tile and the requests its table reads, from the position's totals. Native
 * pairs the plan rows' 20x 7-day equivalent with the billed dollars, a side with no rows left out, the equivalent
 * marked where an estimate priced a row of it. A dollar position reads its dollars under its own words, and the shares
 * read per account, so under them the head holds the meta alone.
 */
export function costHeadOf(unit: UnitPosition, totals: AnalyticsGroup): CostHead {
  if (unit === 'share') return { estimated: false, meta: 'shares per account', subset: null, tile: null };
  if (unit === 'native') {
    const { billed, equivalent } = sidesOf(totals, 'seven_day');
    const estimated = totals.estimated && equivalent !== null;
    const figures: CostFigure[] = [...(equivalent === null ? [] : [{ text: equivalentOrDash('seven_day', equivalent, estimated), word: 'subscription' }]), ...(billed === null ? [] : [{ text: fmtUsd(billed), word: 'billed' }])];
    return { estimated, meta: `${figures.map((f) => f.text).join(' and ')} in range`, subset: NATIVE_SUBSET, tile: { figures, label: 'Cost', note: NATIVE_SUBSET } };
  }
  const dollars = dollarsOf(unit);
  const figure = fmtUsd(totals.costUsd);
  return { estimated: false, meta: `${figure} in range, ${UNIT_META[dollars]}`, subset: UNIT_SUBSET[dollars], tile: { figures: [{ text: figure }], label: UNIT_TILE_LABEL[dollars], note: dollars === 'all' ? 'at the effective price list' : UNIT_SUBSET[dollars] } };
}

/**
 * Per account the 7-day share (solid) and the model week's (dashed) per bucket as `<account>:7d` and
 * `<account>:week`, the 5-hour share beside them as `<account>:5h` for the tooltip, a gap (null) where a bucket has
 * none, and per account the key its model week is charted under (`weeks`), undefined where no bucket names one. The
 * accounts are the groups carrying a share map, so an API key account draws no line.
 */
export function shareLines(points: AnalyticsPoint[]): { accounts: string[]; data: Record<string, number | null>[]; weeks: Record<string, string | undefined> } {
  const accounts = [...new Set(points.flatMap((p) => p.groups.filter((g) => g.costShares !== null).map((g) => g.key)))].sort((a, b) => a.localeCompare(b));
  const weeks: Record<string, string | undefined> = Object.fromEntries(accounts.map((a) => [a, undefined]));
  const data = points.map((p) => {
    const row: Record<string, number | null> = { at: Date.parse(p.at) };
    for (const a of accounts) for (const w of ['5h', '7d', 'week']) row[`${a}:${w}`] = null;
    for (const g of p.groups) {
      if (g.costShares === null) continue;
      const week = weekKeyOf(g.costShares);
      weeks[g.key] ??= week;
      row[`${g.key}:5h`] = windowShare(g.costShares, 'five_hour');
      row[`${g.key}:7d`] = windowShare(g.costShares, 'seven_day');
      row[`${g.key}:week`] = windowShare(g.costShares, week);
    }
    return row;
  });
  return { accounts, data, weeks };
}

const HOUR_MS = 3_600_000;
/** The windows the router reads, which the Accounts cards always offer, in the console's order. */
const BASE_WINDOWS = ['five_hour', 'seven_day', 'seven_day_fable'];
/** The extra-usage week, offered only while a row holds a sample of it, since the heads alone write it. */
const EXTRA_USAGE_WINDOW = 'seven_day_overage_included';
/** The window ids the Accounts cards offer, in order, the extra-usage week only while a row holds it. */
export function sampledWindows(rollups: AccountRollup[]): string[] {
  return rollups.some((r) => r.windows[EXTRA_USAGE_WINDOW] !== undefined) ? [...BASE_WINDOWS, EXTRA_USAGE_WINDOW] : [...BASE_WINDOWS];
}

/**
 * The Pace score beside a lead: the remaining share over the hours from the sample's instant to the reset, floored at
 * nothing remaining, the ratio the routing preview scores an account by (the preview adds its own deadline cap and
 * minute floor over the engine's prediction rather than a raw sample). Null without a reset or a utilization, and null
 * for a reset at or before the sample, since the window has reset since and left the sample stale.
 */
export const paceScoreOf = (sample: WindowSample): number | null => {
  if (sample.resetsAt === null || sample.utilization === null) return null;
  const hours = (Date.parse(sample.resetsAt) - Date.parse(sample.sampledAt)) / HOUR_MS;
  return hours > 0 ? Math.max(1 - sample.utilization, 0) / hours : null;
};

/**
 * Per account per bucket of the span the chosen window's utilization (`usage`) or lead over pace (`pace`) in percent,
 * null for a gap: a bucket no hour of which holds a sample of the window, or a sample without the figure (a
 * reset-only sample has neither). A bucket wider than an hour reads the latest hour inside it that holds a sample.
 * Under `pace` each row also carries `${account}:score`, the Pace score of the sample the lead was read from, null
 * where the lead is, so the tooltip reads both off the row.
 */
export function windowLines(rollups: AccountRollup[], window: string, read: 'pace' | 'usage', span: { fromMs: number; toMs: number }, bucketMs: number): { accounts: string[]; data: Record<string, number | null>[] } {
  const accounts = [...new Set(rollups.map((r) => r.accountId))].sort((a, b) => a.localeCompare(b));
  const start = Math.floor(span.fromMs / bucketMs) * bucketMs;
  const data: Record<string, number | null>[] = [];
  const byAt = new Map<number, Record<string, number | null>>();
  for (let t = start; t <= span.toMs; t += bucketMs) {
    const row: Record<string, number | null> = { at: t };
    for (const a of accounts) {
      row[a] = null;
      if (read === 'pace') row[`${a}:score`] = null;
    }
    data.push(row);
    byAt.set(t, row);
  }
  const drawn = new Map<string, number>();
  for (const r of rollups) {
    const sample = r.windows[window];
    if (sample === undefined) continue;
    const hourMs = Date.parse(r.hour);
    const at = Math.floor(hourMs / bucketMs) * bucketMs;
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

/** Each value weighed by its weight, so a bucket counts for the part of its width it covers. */
const weighedMean = (pairs: { value: number; weight: number }[]) => pairs.reduce((n, p) => n + p.value * p.weight, 0) / pairs.reduce((n, p) => n + p.weight, 0);
const hourCeil = (ms: number) => Math.ceil(ms / 3_600_000) * 3_600_000;
/** The share of its width a bucket covers before it sets the axis and the peak: 6 minutes of an hour, 36 of six. */
const SCALE_FLOOR = 0.1;

const LEGEND_ORDER: ClassLegend[] = ['conversation', 'overhead'];

/**
 * The burn route's points as chart lines and rows. The request-class grouping folds into its two legend entries;
 * every other grouping keeps the groups with the most tokens and sums the rest as other, whose shares stay empty
 * since shares of different windows do not add up. Under the account grouping the rest folds once per unit, since
 * a subscription account's window share and an API-key account's dollars chart on different axes. `sized` names
 * the accounts whose tier the cost model sizes, which chart in window share whatever their buckets priced.
 *
 * `rangeEndMs` is the end of the range the points were read for and `nowMs` the instant of that read, the end when
 * absent. The route reads whole hours through the hour ceiling of the end, so a bucket covers its width up to the hour
 * ceiling of the range's end, no later than the read's instant; the full width without an end.
 */
export function foldBurn(
  points: BurnPoint[],
  groupBy: BurnGroup,
  bucketSeconds: number,
  sized: ReadonlySet<string>,
  max = MAX_BURN_LINES,
  rangeEndMs?: number,
  nowMs = rangeEndMs,
): { lines: BurnLine[]; rows: BurnRow[] } {
  const lineOf = (group: string) => (groupBy === 'request_class' ? classLegendOf(group) : group);
  const tokens = new Map<string, number>();
  for (const p of points) tokens.set(lineOf(p.group), (tokens.get(lineOf(p.group)) ?? 0) + p.cachedTokens + p.uncachedTokens);
  // The category comes from the tier on file rather than from whether a bucket priced, so an account whose range
  // ran on unweighted models alone still charts as a plan line and its unpriced windows show.
  const byAccount = groupBy === 'account';
  const subscribed = new Set(byAccount ? points.filter((p) => p.unit === 'window_share').map((p) => p.group) : []);
  const unitOf = (group: string): BurnLineUnit => (subscribed.has(group) ? (sized.has(group) ? 'window_share' : 'unknown') : 'usd');
  const ranked = [...tokens.entries()].sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0])).map(([k]) => k);
  const kept: string[] = groupBy === 'request_class' ? LEGEND_ORDER : ranked.slice(0, max);
  const folded = groupBy === 'request_class' ? [] : ranked.slice(max);
  const rests = (['window_share', 'unknown', 'usd'] as const).filter((unit) => folded.some((g) => unitOf(g) === unit));
  // The rests take the slots after the kept lines, so a group whose id happens to read `other` stays its own line.
  const slots = kept.length + rests.length;
  const slotOf = (group: string) => (kept.includes(lineOf(group)) ? kept.indexOf(lineOf(group)) : kept.length + rests.indexOf(unitOf(group)));

  const byAt = new Map<number, Sum[]>();
  for (const p of points) {
    const at = Date.parse(p.at);
    const sums = byAt.get(at) ?? Array.from({ length: slots }, (): Sum => ({ cached: 0, fiveHour: null, sevenDay: null, tokens: 0, uncached: 0, usd: 0 }));
    byAt.set(at, sums);
    const slot = slotOf(p.group);
    const s = sums[slot];
    s.cached += p.cachedTokens;
    s.tokens += p.tokens;
    s.uncached += p.uncachedTokens;
    s.usd += p.usd;
    if (slot >= kept.length) continue;
    if (p.fiveHourShare !== null) s.fiveHour = (s.fiveHour ?? 0) + p.fiveHourShare;
    if (p.sevenDayShare !== null) s.sevenDay = (s.sevenDay ?? 0) + p.sevenDayShare;
  }
  const secondsOf = (at: number) => {
    if (rangeEndMs === undefined) return bucketSeconds;
    const covered = Math.min(at + bucketSeconds * 1_000, hourCeil(rangeEndMs), nowMs ?? rangeEndMs);
    return Math.max(1, (covered - at) / 1_000);
  };
  const rows = [...byAt.entries()]
    .sort((a, b) => a[0] - b[0])
    .map(([at, sums]): BurnRow => {
      const seconds = secondsOf(at);
      const minutes = seconds / 60;
      const hours = seconds / 3_600;
      return {
        at,
        cached: sums.map((s) => s.cached / minutes),
        fiveHour: sums.map((s) => (s.fiveHour === null ? null : s.fiveHour / hours)),
        partial: seconds < bucketSeconds,
        seconds,
        setsScale: seconds >= bucketSeconds * SCALE_FLOOR,
        sevenDay: sums.map((s) => (s.sevenDay === null ? null : s.sevenDay / hours)),
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
    // A window the line never priced traffic on stays null in every slot, idle ones included, so its legend reads a
    // dash; one it priced counts an idle slot (no token of any type) as zero, and a slot with tokens and no share
    // stays null. An idle slot's own share never makes the window priced: a failed request's row carries no token
    // and a listed model prices it at zero.
    const foldWindow = (read: (r: BurnRow) => (number | null)[]) => {
      const priced = rows.some((r) => r.tokens[i] > 0 && read(r)[i] !== null);
      for (const r of rows) if (r.tokens[i] === 0) read(r)[i] = priced ? 0 : null;
      const shares = rows.flatMap((r) => (read(r)[i] === null ? [] : [{ value: read(r)[i] as number, weight: weightOf(r) }]));
      return { perHour: priced ? weighedMean(shares) : null, unpriced: rows.some((r) => slotUnpriced(read(r)[i], r.tokens[i])) };
    };
    const fiveHour = foldWindow((r) => r.fiveHour);
    const sevenDay = foldWindow((r) => r.sevenDay);
    const shares = rows.filter((r) => r.setsScale).map((r) => r.fiveHour[i]).filter((v): v is number => v !== null);
    // Only a window-share line lacks a share for want of a row; every other line's slots read null by construction.
    const plan = rest === null && unitOf(kept[i]) === 'window_share';
    const figures = {
      fiveHourPeak: shares.length ? Math.max(...shares) : null,
      fiveHourPerHour: fiveHour.perHour,
      sevenDayPerHour: sevenDay.perHour,
      tokensPerMinute: mean((r) => r.cached[i] + r.uncached[i]),
      unpriced: { fiveHour: plan && fiveHour.unpriced, sevenDay: plan && sevenDay.unpriced },
      usdPerMinute: mean((r) => r.usd[i]),
    };
    if (rest === null) return { ...figures, key: kept[i], label: kept[i] === '-' ? 'unattributed' : kept[i], other: false, unit: unitOf(kept[i]) };
    const byUnit = groupBy === 'account';
    return { ...figures, key: byUnit ? `${OTHER_KEY}-${rest}` : OTHER_KEY, label: byUnit ? OTHER_LABEL[rest] : 'Other', other: true, unit: rest };
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
export const placementRungLabel = (rung: string): string => (own(PLACEMENT_RUNG_LABEL, rung) ? PLACEMENT_RUNG_LABEL[rung as Placement['rung']] : rung);

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
