import type { Api } from '@/api/client';
import type { Account, AccountRollup, AnalyticsGroup, AnalyticsPoint, AnalyticsQuery, BurnGroup, BurnPoint, BurnUnit, HeldPoint, PinMigrationReason, Placement, PlacementRow, Reconciliation, RewritesAnswer, SubscriptionCosts, WindowSample } from '@/api/types';
import { tierCapacity, tierRow, type CostWindow } from '@/lib/costs';
import { earlierHolderLabel, equivalentShort, fmtCompact, fmtEquivalent, fmtInt, fmtPct, fmtShare, fmtUsd, withEstimate } from '@/lib/format';
import { ageWord } from '@/lib/time';
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

/** The held bars: one per bucket with its milliseconds and request count, and the range's sum for the legend. */
export function heldBars(answer: RewritesAnswer): { bars: HeldPoint[]; totalMs: number; totalRequests: number } {
  const bars = answer.held.map((p) => ({ at: p.at, heldMs: p.heldMs, requests: p.requests }));
  return { bars, totalMs: bars.reduce((n, b) => n + b.heldMs, 0), totalRequests: bars.reduce((n, b) => n + b.requests, 0) };
}

/** The held time of a bucket or a range as its hover and legend read it, at least a second where a request waited. */
export const heldSpan = (ms: number): string => ageWord(Math.max(1_000, ms));

/** A held bar's hover, `12 requests held 14 min in all`, and `no request held` for a bucket that held none. */
export const heldHover = (bar: Pick<HeldPoint, 'heldMs' | 'requests'>): string =>
  bar.requests === 0 ? 'no request held' : `${fmtInt(bar.requests)} ${bar.requests === 1 ? 'request' : 'requests'} held ${heldSpan(bar.heldMs)} in all`;

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
/**
 * The figure the plan charts read off the burn points: each account's share of its own windows, which the route
 * answers where a group holds one account's rows, or the 20x window equivalents, which it answers under every grouping.
 */
export type BurnFigure = 'equivalent' | 'share';

export interface BurnLine {
  /** Whether any bucket of the line priced through the tier fallback, so the legend lists the estimated entry. */
  estimated: boolean;
  /** The highest 5-hour figure per hour the range reached, null for a line with no figure in any bucket. */
  fiveHourPeak: number | null;
  /** The range's 5-hour figure per hour over the buckets that read one, null on a window the line never priced. */
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
   * Per window, whether a bucket holds traffic the cost model could not price there: under the equivalents a bucket
   * the route flagged, and under the shares, which carry no flag, a bucket with tokens and no share (a model of no
   * known model tier, or a tier the cost model has no capacity for on that window). False on a line the plan charts do
   * not draw.
   */
  unpriced: { fiveHour: boolean; sevenDay: boolean };
  usdPerMinute: number;
  /**
   * The range's model week figure per hour. Under the equivalents a bucket off every model with a week of its own reads
   * zero and one the route flagged is left out, and under the shares, which carry no flag to tell the two apart, the
   * figure is the mean of the buckets that answered one. Null for a line that never ran on such a model.
   */
  weekPerHour: number | null;
}

/**
 * One bucket, indexed like the lines: the cached and uncached tokens and the dollars per minute of the time it covers,
 * its raw token count, and the shares per hour.
 */
export interface BurnRow {
  at: number;
  cached: number[];
  /** Per slot, whether a point of the bucket priced through the model tier's fallback weight. */
  estimated: boolean[];
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
  /** The model week's figure per hour, null where the route could not price one there, and where no row of the bucket
   *  ran on a model with a week of its own unless the line's fold reads that bucket as zero. */
  week: (number | null)[];
  /** Per slot, whether a point of the bucket carried a model week figure, so a zero the fold filled in or a gap marks
   *  no estimate. */
  weekAnswered: boolean[];
}

/** One window's running figure over a slot's points: null until a point carries one, and latched once a point reads
 *  unpriced, since a partial sum would read as the slot's figure. */
interface WindowFold {
  sum: number | null;
  unpriced: boolean;
}

interface Sum {
  cached: number;
  estimated: boolean;
  fiveHour: WindowFold;
  sevenDay: WindowFold;
  tokens: number;
  uncached: number;
  usd: number;
  week: WindowFold;
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
 * read lacked, and the plan charts size each account from the tiers of the read that answered its buckets. Only a
 * read whose groups each hold one account's rows answers shares to size (the account grouping, or the Accounts filter
 * naming exactly one account), so any other reads no account and a failed account read leaves it whole.
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
 * A group's label: the route's under every grouping, the model grouping's empty key included, since the ledger labels
 * that key itself.
 */
export const modelGroupLabel = (g: Pick<AnalyticsGroup, 'key' | 'label'>, _groupBy: BurnGroup): string => g.label;

/** An earlier holder of a key id groups under `<id>@<generation>`, which the breakdown labels by its creation. */
const EARLIER_HOLDER = /^(.+)@(\d+)$/;
/**
 * A burn group's label. A burn point carries no label of its own, so the words are the breakdown's: an earlier key
 * holder's, `no model` for the model grouping's empty key and `unattributed` for what the router never attributed.
 */
function burnLabelOf(groupBy: BurnGroup, key: string): string {
  if (key === '-') return 'unattributed';
  if (groupBy === 'model' && key === '') return 'no model';
  const holder = groupBy === 'key' ? EARLIER_HOLDER.exec(key) : null;
  return holder === null ? key : earlierHolderLabel(holder[1], Number(holder[2]));
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
/**
 * The note under Window shares of a surface that reads one account's own windows, such as a tile or the cache cost
 * card, with no account named: the route answers a named account's shares under any grouping, so the filter alone is
 * needed, whatever the grouping.
 */
export const ONE_ACCOUNT_SHARE_NOTE = "Shares of one account's windows alone: name one account in the Accounts filter.";
/**
 * The note under Window shares of a surface that reads a share per group, such as the Cost chart or a Burn plan card,
 * where no group holds one account's rows: either half makes each group one account's.
 */
export const GROUPED_SHARE_NOTE = "Shares of one account's windows alone: group by account, or name one account in the Accounts filter.";

const SHARED_WINDOWS = ['five_hour', 'seven_day'];

/**
 * One named account's shares over the groups the route answered them on, the Accounts filter naming that account:
 * each window summed over the groups, since every group is the one account's. A window carried as null is left out,
 * and so is a shared window that a group `required` to carry it lacks, since a partial sum would read as the
 * account's. A model week sums where a group carries one, a group off such a model carrying none. Null where no group
 * answered a map, and where a required group answered none, whatever the others priced.
 */
export function oneAccountShares<T>(groups: AnalyticsGroup[], read: (g: AnalyticsGroup) => Record<string, T | null> | null, add: (a: T, b: T) => T, required: (g: AnalyticsGroup) => boolean = () => false): Record<string, T> | null {
  // The required test runs over every group before a missing map drops one, or the others would read as the whole.
  const answered = groups.map((g) => ({ map: read(g), required: required(g) }));
  if (answered.some((a) => a.map === null && a.required)) return null;
  const maps = answered.flatMap((a) => (a.map === null ? [] : [{ map: a.map, required: a.required }]));
  if (maps.length === 0) return null;
  const out: Record<string, T> = {};
  const left = new Set<string>();
  for (const { map, required: carries } of maps) {
    if (carries) for (const window of SHARED_WINDOWS) if (!own(map, window)) left.add(window);
    for (const [window, v] of Object.entries(map)) {
      if (v === null) left.add(window);
      else out[window] = own(out, window) ? add(out[window], v) : v;
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
 * group that carries no plan count has plan rows where it carries an equivalent window at all, as `sidesOf` reads it.
 */
export const planRowsRequired = (g: AnalyticsGroup): boolean => (g.planRequests === null ? Object.keys(g.costEquivalent).length > 0 : g.planRequests > 0 && g.planOutputTokens !== 0);
export const addReadWrite = (a: { read: number; write: number }, b: { read: number; write: number }) => ({ read: a.read + b.read, write: a.write + b.write });
/** The requests that did not succeed over the requests. The refused requests are in neither, since the fold never
 *  counted them. Zero where nothing was rated. */
export const errorRateOf = (g: Pick<AnalyticsGroup, 'requests' | 'succeeded'>): number => (g.requests > 0 ? (g.requests - g.succeeded) / g.requests : 0);
/**
 * Whether a group's probes oblige it to carry both shared probe windows on its shares: answered probes a plan account
 * served, since a held or skipped probe never ran. A group whose answered probes are all billed carries no plan window
 * to lack, so it adds no probe window and drops none.
 */
export const probesRequired = (g: Pick<AnalyticsGroup, 'probes'>): boolean => g.probes.planOk > 0;
/**
 * The answered and the failed probes, the count a side's probe cost latches on. The wire's `failed` count also holds
 * the exhausted and rejected probes the relay refuses itself. A held or skipped probe never ran, so a group whose
 * probes were all held or skipped spent nothing on probes and its saving stands.
 */
export const upstreamProbes = (g: Pick<AnalyticsGroup, 'probes'>): number => g.probes.ok + g.probes.failed;
/**
 * Whether a group's kept-warm credits oblige it to carry both shared saving windows: credits whose request a plan
 * account served, a credit-only group's among them (a request received before the bucket and completed inside it). A
 * group whose probes kept nothing warm, or whose credits are all billed, carries no plan saving to lack, so it adds no
 * saving window and drops none.
 */
export const savingRequired = (g: Pick<AnalyticsGroup, 'keptWarm'>): boolean => (g.keptWarm?.planRequests ?? 0) > 0;
/**
 * The named account's whole: the position's totals with its shares summed over the groups the route answered them on.
 */
export const oneAccountGroupOf = (totals: AnalyticsGroup, groups: AnalyticsGroup[]): AnalyticsGroup => ({
  ...totals,
  cacheShares: oneAccountShares(groups, (g) => g.cacheShares, addReadWrite, planRowsRequired),
  costShares: oneAccountShares(groups, (g) => g.costShares, (a, b) => a + b, planRowsRequired),
  probeShares: oneAccountShares(groups, (g) => g.probeShares, (a, b) => ({ cost: a.cost + b.cost, wasted: a.wasted === null || b.wasted === null ? null : a.wasted + b.wasted }), probesRequired),
  savingShares: oneAccountShares(groups, (g) => g.savingShares, (a, b) => a + b, savingRequired),
});

/** The two counts summed, the one figure the charts and the by-pool table print per group. */
export const keptWarmTokens = (k: { tokens1h: number; tokens5m: number }): number => k.tokens1h + k.tokens5m;
/** The net saving, the gross less the probe cost, in whatever unit both figures came in. */
export const netSaving = (gross: number, probeCost: number): number => gross - probeCost;
/** The two counts' note: "12,345 tokens read warm, 8,000 at five minutes and 4,345 at an hour". */
export function keptWarmNote(k: { requests: number; tokens1h: number; tokens5m: number }): string {
  return `${fmtInt(keptWarmTokens(k))} tokens read warm, ${fmtInt(k.tokens5m)} at five minutes and ${fmtInt(k.tokens1h)} at an hour`;
}

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
/** Native's requests where a surface reads the 7-day window alone, each side in the unit its funding pays in. */
export const NATIVE_7D_SUBSET = 'the subscription requests in 20x 7-day equivalents and the billed requests in dollars';
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
 * in `nativeCellOf`'s vocabulary: undefined leaves out a side with no rows, and a null equivalent reads a dash for plan
 * rows the route priced nothing on the window, since the ledger leaves a window out when it cannot price a plan row
 * there. The route and the session fold count the plan requests, and a group that carries no count tells the sides
 * apart by their figures. A sum with no row on either side reads both, so an empty range reads its zero in each unit
 * rather than nothing.
 */
export function sidesOf(g: Pick<AnalyticsGroup, 'billedUsd' | 'costEquivalent' | 'planRequests' | 'requests'>, window: string): { billed: number | undefined; equivalent: number | null | undefined } {
  const plan = g.planRequests === null ? Object.keys(g.costEquivalent).length > 0 : g.planRequests > 0;
  const billed = g.planRequests === null ? g.billedUsd > 0 : g.requests > g.planRequests;
  const equivalent = own(g.costEquivalent, window) ? g.costEquivalent[window] : null;
  return plan || billed ? { billed: billed ? g.billedUsd : undefined, equivalent: plan ? equivalent : undefined } : { billed: g.billedUsd, equivalent: equivalent ?? 0 };
}

/** One figure of a Cost tile, with the word naming its side where the tile stacks two. */
export interface CostFigure {
  text: string;
  word?: string;
}
/** The dash's title where a window's `unpriced` flag holds. */
export const UNPRICED_TITLE = 'No subscription request priced on this window: its model has no tier the cost model weighs';
/**
 * A sum's native figures on `window`, each with its funding's word: the plan rows' 20x equivalent, the tilde before it
 * where an estimate priced it, then the billed rows' dollars. The sides read as `sidesOf` answers them: a side
 * undefined for want of rows is left out, a null equivalent reads a dash under the subscription word, and a sum with no
 * row on either side reads both, so it shows its zero in each unit rather than nothing. `unpriced` is the sum's flag
 * on the window: a plan row there priced nothing, so the subscription side reads the dash whatever the other rows
 * summed, since a partial sum would read as the whole. `marked` says whether the equivalent carries the tilde, so the
 * figure's element takes the estimate's title, and the answer's `unpriced` whether it reads the flag's dash, so it
 * takes `UNPRICED_TITLE`.
 */
export function nativeFiguresOf(window: string, sides: { billed: number | undefined; equivalent: number | null | undefined; unpriced: boolean }, estimated: boolean): { figures: CostFigure[]; marked: boolean; unpriced: boolean } {
  const none = sides.billed === undefined && sides.equivalent === undefined && !sides.unpriced;
  const equivalent = sides.unpriced ? null : none ? 0 : sides.equivalent;
  const billed = none ? 0 : sides.billed;
  const marked = estimated && !none && equivalent !== null && equivalent !== undefined;
  return {
    figures: [...(equivalent === undefined ? [] : [{ text: equivalentOrDash(window, equivalent, marked), word: 'subscription' }]), ...(billed === undefined ? [] : [{ text: fmtUsd(billed), word: 'billed' }])],
    marked,
    unpriced: sides.unpriced,
  };
}
/**
 * The Cost by card's note: `{n} groups unpriced in {m} buckets, drawn as gaps`, singular `1 group unpriced in 1 bucket,
 * drawn as a gap`, each count read on its own, and null where no drawn group read unpriced in the range.
 */
export function unpricedNote(groups: number, buckets: number): string | null {
  if (groups === 0 || buckets === 0) return null;
  return `${fmtInt(groups)} ${groups === 1 ? 'group' : 'groups'} unpriced in ${fmtInt(buckets)} ${buckets === 1 ? 'bucket, drawn as a gap' : 'buckets, drawn as gaps'}`;
}
/**
 * Whether a probe figure's plan side carries the estimate's mark: a plan probe row priced through the model tier's
 * fallback, which a request row's fallback never stands in for, on a side with plan probes that priced a figure, since a
 * dash or a side with no plan probe is no estimate.
 */
export const probeMarked = (whole: Pick<AnalyticsGroup, 'probeEstimated'>, planProbes: number, priced: number | null): boolean => whole.probeEstimated && planProbes > 0 && priced !== null;
/** The Cost section's head under a position, as `costHeadOf` reads it. */
export interface CostHead {
  /** Whether a figure of the head carries the estimated mark, so its element takes the estimate's title. */
  estimated: boolean;
  meta: string;
  /** The requests the Cost table's subtitle names, null under the shares, whose table names its own. */
  subset: string | null;
  /** The first tile, null under the shares, which read per account. */
  tile: { figures: CostFigure[]; label: string; note: string } | null;
  /** Whether the subscription side reads the dash of a 7-day window the totals flag unpriced, so the head's elements
   *  take `UNPRICED_TITLE`. */
  unpriced: boolean;
}
/**
 * The Cost section's meta line, its first tile and the requests its table reads, from the position's totals. Native
 * pairs the plan rows' 20x 7-day equivalent with the billed dollars, a side with no rows left out, the equivalent
 * marked where an estimate priced a row of it and a dash where the route priced none or flags the window unpriced. A
 * dollar position reads its dollars under its own words, and the shares read per account, so under them the head holds
 * the meta alone.
 */
export function costHeadOf(unit: UnitPosition, totals: AnalyticsGroup): CostHead {
  if (unit === 'share') return { estimated: false, meta: 'shares per account', subset: null, tile: null, unpriced: false };
  if (unit === 'native') {
    const { figures, marked, unpriced } = nativeFiguresOf('seven_day', { ...sidesOf(totals, 'seven_day'), unpriced: totals.unpriced.seven_day === true }, totals.estimated);
    return { estimated: marked, meta: `${figures.map((f) => f.text).join(' and ')} in range`, subset: NATIVE_SUBSET, tile: { figures, label: 'Cost', note: NATIVE_SUBSET }, unpriced };
  }
  const dollars = dollarsOf(unit);
  const figure = fmtUsd(totals.costUsd);
  return { estimated: false, meta: `${figure} in range, ${UNIT_META[dollars]}`, subset: UNIT_SUBSET[dollars], tile: { figures: [{ text: figure }], label: UNIT_TILE_LABEL[dollars], note: dollars === 'all' ? 'at the effective price list' : UNIT_SUBSET[dollars] }, unpriced: false };
}

/**
 * A group's color slot among the range's keys, the empty key skipped so the first named key takes slot 0. The empty
 * key, a key not listed and any key past the eighth take slot 7.
 */
export function slotIndex(keys: readonly string[], key: string): number {
  const i = key === '' ? -1 : keys.filter((k) => k !== '').indexOf(key);
  return Math.min(7, i < 0 ? 7 : i);
}

/**
 * Per group the 7-day share (solid) and the model week's (dashed) per bucket as `<group>:7d` and `<group>:week`, the
 * 5-hour share beside them as `<group>:5h` for the tooltip, a gap (null) where a bucket has none, per group the key its
 * model week is charted under (`weeks`), undefined where no bucket names one, and the group's label under `groupBy`,
 * which names the line under the page's grouping. The groups are the ones carrying a share map: each account under the
 * account grouping, an API key account drawing no line, or each group of the one account the Accounts filter names.
 */
export function shareLines(points: AnalyticsPoint[], groupBy: BurnGroup): { accounts: string[]; data: Record<string, number | null>[]; labels: Record<string, string>; weeks: Record<string, string | undefined> } {
  const accounts = [...new Set(points.flatMap((p) => p.groups.filter((g) => g.costShares !== null).map((g) => g.key)))].sort((a, b) => a.localeCompare(b));
  const weeks: Record<string, string | undefined> = Object.fromEntries(accounts.map((a) => [a, undefined]));
  const labels: Record<string, string> = {};
  for (const p of points) for (const g of p.groups) if (g.costShares !== null) labels[g.key] ??= modelGroupLabel(g, groupBy);
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
  return { accounts, data, labels, weeks };
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
 * every other grouping keeps the groups with the most tokens and sums the rest as other, which draws no plan line, so
 * its plan figures stay empty. Under the account grouping the rest folds once per unit, since a subscription account's
 * window share and an API-key account's dollars chart on different axes. Where the shares answer (that grouping, or
 * `account`, the one account the Accounts filter names) a line's unit is its account's. `sized` names the accounts
 * whose tier the cost model sizes, which chart in window share whatever their buckets priced. `figure` picks what the
 * plan rows carry: the shares, or the 20x window equivalents, which add across accounts and tiers.
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
  options: { account?: string | null; figure?: BurnFigure } = {},
): { lines: BurnLine[]; rows: BurnRow[] } {
  const figure = options.figure ?? 'share';
  const account = options.account ?? null;
  const lineOf = (group: string) => (groupBy === 'request_class' ? classLegendOf(group) : group);
  const tokens = new Map<string, number>();
  for (const p of points) tokens.set(lineOf(p.group), (tokens.get(lineOf(p.group)) ?? 0) + p.cachedTokens + p.uncachedTokens);
  // The category comes from the tier on file rather than from whether a bucket priced, so an account whose range
  // ran on unweighted models alone still charts as a plan line and its unpriced windows show.
  const byAccount = groupBy === 'account';
  const shared = byAccount || account !== null;
  // Keyed by the line, so the request class grouping's two folded lines read their classes' unit.
  const subscribed = new Set(shared ? points.filter((p) => p.unit === 'window_share').map((p) => lineOf(p.group)) : []);
  const unitOf = (group: string): BurnLineUnit => (subscribed.has(group) ? (sized.has(byAccount ? group : (account ?? '')) ? 'window_share' : 'unknown') : 'usd');
  const ranked = [...tokens.entries()].sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0])).map(([k]) => k);
  const kept: string[] = groupBy === 'request_class' ? LEGEND_ORDER : ranked.slice(0, max);
  const folded = groupBy === 'request_class' ? [] : ranked.slice(max);
  const rests = (['window_share', 'unknown', 'usd'] as const).filter((unit) => folded.some((g) => unitOf(g) === unit));
  // The rests take the slots after the kept lines, so a group whose id happens to read `other` stays its own line.
  const slots = kept.length + rests.length;
  const slotOf = (group: string) => (kept.includes(lineOf(group)) ? kept.indexOf(lineOf(group)) : kept.length + rests.indexOf(unitOf(group)));
  const equivalents = figure === 'equivalent';
  const figures = (p: BurnPoint) => (figure === 'share' ? [p.fiveHourShare, p.sevenDayShare, p.modelWeekShare] : [p.fiveHourEquivalent, p.sevenDayEquivalent, p.modelWeekEquivalent]);
  const add = (into: WindowFold, value: number | null, unpriced: boolean) => {
    if (value !== null) into.sum = (into.sum ?? 0) + value;
    if (unpriced) into.unpriced = true;
  };

  const byAt = new Map<number, Sum[]>();
  for (const p of points) {
    const at = Date.parse(p.at);
    const sums = byAt.get(at) ?? Array.from({ length: slots }, (): Sum => ({ cached: 0, estimated: false, fiveHour: { sum: null, unpriced: false }, sevenDay: { sum: null, unpriced: false }, tokens: 0, uncached: 0, usd: 0, week: { sum: null, unpriced: false } }));
    byAt.set(at, sums);
    const slot = slotOf(p.group);
    const s = sums[slot];
    s.cached += p.cachedTokens;
    s.tokens += p.tokens;
    s.uncached += p.uncachedTokens;
    s.usd += p.usd;
    if (slot >= kept.length) continue;
    const [fiveHour, sevenDay, week] = figures(p);
    // The equivalents carry the route's flags. The shares carry none, so a null share with tokens behind it reads
    // unpriced, and a null model week share, which the route answers alike for a bucket off every such model, adds
    // nothing and is drawn as a gap.
    add(s.fiveHour, fiveHour, equivalents ? p.fiveHourUnpriced : fiveHour === null && p.tokens > 0);
    add(s.sevenDay, sevenDay, equivalents ? p.sevenDayUnpriced : sevenDay === null && p.tokens > 0);
    add(s.week, week, equivalents && p.modelWeekUnpriced);
    if (p.estimated) s.estimated = true;
  }
  const secondsOf = (at: number) => {
    if (rangeEndMs === undefined) return bucketSeconds;
    const covered = Math.min(at + bucketSeconds * 1_000, hourCeil(rangeEndMs), nowMs ?? rangeEndMs);
    return Math.max(1, (covered - at) / 1_000);
  };
  const perHour = (w: WindowFold, hours: number) => (w.unpriced || w.sum === null ? null : w.sum / hours);
  const buckets = [...byAt.entries()].sort((a, b) => a[0] - b[0]);
  const rows = buckets
    .map(([at, sums]): BurnRow => {
      const seconds = secondsOf(at);
      const minutes = seconds / 60;
      const hours = seconds / 3_600;
      return {
        at,
        cached: sums.map((s) => s.cached / minutes),
        estimated: sums.map((s) => s.estimated),
        fiveHour: sums.map((s) => perHour(s.fiveHour, hours)),
        partial: seconds < bucketSeconds,
        seconds,
        setsScale: seconds >= bucketSeconds * SCALE_FLOOR,
        sevenDay: sums.map((s) => perHour(s.sevenDay, hours)),
        tokens: sums.map((s) => s.tokens),
        uncached: sums.map((s) => s.uncached / minutes),
        usd: sums.map((s) => s.usd / minutes),
        week: sums.map((s) => perHour(s.week, hours)),
        weekAnswered: sums.map((s) => perHour(s.week, hours) !== null),
      };
    });
  // Weighed by the share of its width a bucket covers rather than its seconds: the same mean, and a range of full
  // buckets reads the plain mean to the last bit.
  const weightOf = (r: BurnRow) => r.seconds / bucketSeconds;

  const mean = (read: (r: BurnRow) => number) => (rows.length ? weighedMean(rows.map((r) => ({ value: read(r), weight: weightOf(r) }))) : 0);
  const lines = Array.from({ length: slots }, (_, i): BurnLine => {
    const rest = i >= kept.length ? rests[i - kept.length] : null;
    // A window the line never priced traffic on stays null in every slot, idle ones included, so its legend reads a
    // dash. One it priced counts an idle slot as zero and keeps an unpriced one null. An idle slot's own figure never
    // makes the window priced: a failed request's row carries no token and a listed model prices it at zero. Under the
    // equivalents the route flags a slot it could not price, so a slot with no token or with no figure and no flag is
    // idle, the model week's among them (a bucket off every model with a week of its own). The shares carry no flag, so
    // under them a slot with no token is idle and one with tokens and no figure is unpriced, and the model week fills
    // no slot, since a null there is either a bucket off every such model or one the table could not price.
    const foldWindow = (read: (r: BurnRow) => (number | null)[], fold: (s: Sum) => WindowFold, gaps: 'none' | 'idle') => {
      const flagged = (k: number) => equivalents && fold(buckets[k][1][i]).unpriced;
      const idle = (r: BurnRow, k: number) => r.tokens[i] === 0 || (equivalents && read(r)[i] === null && !flagged(k));
      const priced = rows.some((r) => (gaps === 'none' || r.tokens[i] > 0) && read(r)[i] !== null);
      if (gaps === 'idle') {
        rows.forEach((r, k) => {
          if (idle(r, k)) read(r)[i] = priced ? 0 : null;
        });
      }
      const figures = rows.flatMap((r) => (read(r)[i] === null ? [] : [{ value: read(r)[i] as number, weight: weightOf(r) }]));
      return { perHour: priced ? weighedMean(figures) : null, unpriced: equivalents ? rows.some((_, k) => flagged(k)) : rows.some((r) => slotUnpriced(read(r)[i], r.tokens[i])) };
    };
    const fiveHour = foldWindow((r) => r.fiveHour, (s) => s.fiveHour, 'idle');
    const sevenDay = foldWindow((r) => r.sevenDay, (s) => s.sevenDay, 'idle');
    const week = foldWindow((r) => r.week, (s) => s.week, equivalents ? 'idle' : 'none');
    const peaks = rows.filter((r) => r.setsScale).map((r) => r.fiveHour[i]).filter((v): v is number => v !== null);
    // Under the shares only a window-share line can lack a figure for want of a row, and under the equivalents every
    // line can.
    const plan = rest === null && (figure === 'equivalent' || unitOf(kept[i]) === 'window_share');
    const figuresOf = {
      estimated: rest === null && rows.some((r) => r.estimated[i]),
      fiveHourPeak: peaks.length ? Math.max(...peaks) : null,
      fiveHourPerHour: fiveHour.perHour,
      sevenDayPerHour: sevenDay.perHour,
      tokensPerMinute: mean((r) => r.cached[i] + r.uncached[i]),
      unpriced: { fiveHour: plan && fiveHour.unpriced, sevenDay: plan && sevenDay.unpriced },
      usdPerMinute: mean((r) => r.usd[i]),
      weekPerHour: week.perHour,
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

/** The least rise a ratio counts after: the readings move in whole percents, so a smaller rise makes a coarse ratio. */
export const MIN_RISE = 0.05;
/** The band a completed window's ratio agrees with the table inside, symmetric in log terms, as the ledger reads it. */
export const RATIO_HIGH = 1.25;
export const RATIO_LOW = 0.8;
/** A ratio the condition marks: outside the band after a rise the readings' whole-percent steps can carry. */
export const ratioOff = (row: Pick<Reconciliation, 'observedRise' | 'ratio'>): boolean => row.observedRise >= MIN_RISE && (row.ratio < RATIO_LOW || row.ratio > RATIO_HIGH);
