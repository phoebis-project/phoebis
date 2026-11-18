import type { Api } from '@/api/client';
import type { Account, AccountRollup, AnalyticsGroup, AnalyticsQuery, BurnGroup, BurnPoint, BurnUnit, FundedSum, HeldPoint, KeepaliveOverview, PinMigrationReason, PinReleaseReason, Placement, PlacementRow, Reconciliation, RewritesAnswer, SubscriptionCosts, WindowSample } from '@/api/types';
import type { AnalyticsAddress } from '@/lib/address';
import { MODEL_WEEK_TIERS, tierCapacity, type CostWindow, type FiguredPoint } from '@/lib/costs';
import { has, lookup } from '@/lib/counts';
import { earlierHolderLabel, fmtCompact, fmtInt, fmtPct, fmtShare, fmtUsd, fmtWeek, joinPresent, parseKeyLifetime, plural, weekShort } from '@/lib/format';
import { planLabel, pricedTier } from '@/lib/quota';
import { ageWord, HOUR, hourCeil, hourFloor } from '@/lib/time';
import { UNIT_LABEL, type UnitPosition, type WeekPosition } from '@/lib/unit';
import { windowHint } from '@/lib/windows';

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

/** Why the router released a pin, in the order the selection found them, and the cause of a move it forced. */
const PIN_RELEASE_LABEL: Record<PinReleaseReason, string> = {
  attempted: 'already tried by this request',
  budget: 'account at its budget',
  budget_unknown: 'account budget unknown',
  client_error: 'client error',
  cooldown: 'account cooled',
  deleted: 'account deleted',
  disconnected: 'account disconnected',
  expired: 'pin idle past the affinity window',
  fast_credits: 'usage credits unavailable for fast mode',
  fast_rate_limit: 'fast lane rate limited',
  funding_required: 'funding required',
  overage: 'in overage',
  paused: 'account paused',
  reauth_required: 'needs re-auth',
  released: 'nothing else stood against the account',
  spent: 'window spent',
  subscription_ended: 'subscription ended',
  unpaired: 'account unpaired',
  unserved: 'model no longer served here',
};

/** A reason the console has no words for reads as the router sent it. */
export const releaseLabel = (reason: string): string => lookup(PIN_RELEASE_LABEL, reason) ?? reason;

/**
 * The moves an account's state forced on a session: the cause words of a replaced pin's move that no rule and no
 * failover shares. `cooldown`, `overage` and `spent` stay in their lists, since a count keyed by the word alone
 * cannot tell a cause from the rule or the failover of the same word.
 */
export const CAUSE_MOVE_REASONS: readonly PinReleaseReason[] = ['attempted', 'budget', 'budget_unknown', 'deleted', 'disconnected', 'fast_credits', 'fast_rate_limit', 'funding_required', 'paused', 'reauth_required', 'released', 'subscription_ended', 'unpaired', 'unserved'];

/** A move's reason in words: a rule's or a failover's label, else the cause's words as a release reads them, else
 *  the word as the router sent it. */
export const moveLabel = (reason: string): string => lookup(PIN_MIGRATION_LABEL, reason) ?? releaseLabel(reason);

/** The held bars: one per bucket with its milliseconds and request count, and the range's sum for the legend. */
export function heldBars(answer: RewritesAnswer): { bars: HeldPoint[]; totalMs: number; totalRequests: number } {
  const bars = answer.held;
  return { bars, totalMs: bars.reduce((n, b) => n + b.heldMs, 0), totalRequests: bars.reduce((n, b) => n + b.requests, 0) };
}

/** The held time of a bucket or a range as its hover and legend read it, at least a second where a request waited. */
export const heldSpan = (ms: number): string => ageWord(Math.max(1_000, ms));

/** A held bar's hover, `12 requests held 14 min in all`, and `no request held` for a bucket that held none. */
export const heldHover = (bar: Pick<HeldPoint, 'heldMs' | 'requests'>): string =>
  bar.requests === 0 ? 'no request held' : `${plural(bar.requests, 'request')} held ${heldSpan(bar.heldMs)} in all`;

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
  /** The highest plan dollars per hour a bucket that sets the scale reached, null for a line with no such bucket. */
  planUsdPeak: number | null;
  /** The range's mean of the plan side's list dollars per hour. */
  planUsdPerHour: number;
  tokensPerMinute: number;
  /**
   * The unit the line is charted in: under the account grouping a subscription account burns plan share and an
   * API-key one dollars; every other grouping mixes both, so it charts dollars.
   */
  unit: BurnLineUnit;
  /** Whether a bucket of the line holds the model's gap the card handed in: traffic no tier priced, or on the week card
   *  traffic the route flagged as priced on no weight. False on a folded rest. */
  unpriced: boolean;
  /** Whether a bucket of the line holds the tier's gap the card handed in: traffic a cost revision priced with no
   *  capacity for the chosen tier on the card's window. False on a folded rest. */
  unsized: boolean;
  usdPerMinute: number;
}

/**
 * One bucket, indexed like the lines: the cached and uncached tokens and the dollars per minute of the time it covers,
 * its raw token count, and the card's figure per hour.
 */
export interface BurnRow {
  at: number;
  cached: number[];
  /**
   * The figure per hour the card handed in under the chosen tier. Null where the slot holds a gap, on a line that never
   * priced traffic and on a folded rest. An idle slot reads zero on a line that priced.
   */
  equivalent: (number | null)[];
  /** The bucket covers less than its width, so its rate rests on that part alone. */
  partial: boolean;
  /** The plan side's list dollars per hour of the time the bucket covers, indexed like the lines. */
  planUsd: number[];
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
  /** Whether the slot holds a bucket with the tier's gap, so its tooltip names that cause. False on a folded rest. */
  unsized: boolean[];
  usd: number[];
}

/** A slot's running figure over its points: null until a point carries one, and latched once a point reads a gap of
 *  either cause, since a partial sum would read as the slot's figure. */
interface FigureFold {
  sum: number | null;
  unpriced: boolean;
  unsized: boolean;
}

interface Sum {
  cached: number;
  equivalent: FigureFold;
  planUsd: number;
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
  filters: Pick<AnalyticsQuery, 'accountIds' | 'keyIds' | 'poolIds' | 'userAgentGroups'> = {},
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
    const words = `${fmtShare(share)} of ${windowHint(key)}`;
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
/** The failed share of a group's requests, null for a group with no request. The refused requests are in neither
 *  count, and the client's cancels leave the failures, since a client that hangs up is no fault of the route. */
export const errorRateOf = (g: Pick<AnalyticsGroup, 'clientCanceled' | 'requests' | 'succeeded'>): number | null =>
  g.requests > 0 ? (g.requests - g.succeeded - g.clientCanceled) / g.requests : null;
/**
 * The answered and the failed probes, the count a side's probe cost latches on and the Cache warming notes print as
 * the probes sent. The wire's `failed` count also holds the exhausted and rejected probes the relay refuses itself. A
 * held or skipped probe never ran, so a group whose probes were all held or skipped spent nothing on probes and its
 * saving stands.
 */
export const upstreamProbes = (g: Pick<AnalyticsGroup, 'probes'>): number => g.probes.ok + g.probes.failed;
/**
 * The five drawn probe counts by the keys of `PROBE_SERIES`. The route counts a migrated probe in `ok` and in
 * `migrated`, so the OK part is `ok` less `migrated`, floored at zero for a bucket whose mark landed apart from its
 * request row.
 */
export const probeParts = <T extends KeepaliveOverview['probes24h']>(c: T): T => ({ ...c, ok: Math.max(0, c.ok - c.migrated) });
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
/**
 * The cost label per dollar position. The Analytics page's first Cost tile reads it under a dollar position, the
 * Dashboard's cost tile under every position through `dollarsOf`, and the Keys and Pools cost tiles and a pool card's
 * cost row under the two positions that name one side, through `namedCostLabel`.
 */
export const UNIT_TILE_LABEL: Record<DollarPosition, string> = { all: 'Estimated cost', equivalent: UNIT_LABEL.equivalent, usd: UNIT_LABEL.usd };
/** The cost label of a position that names one side, and null under the others. */
export const namedCostLabel = (unit: UnitPosition): string | null => (unit === 'equivalent' || unit === 'usd' ? UNIT_TILE_LABEL[unit] : null);

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
/** Parts of a title as sentences, an absent or empty part left out. */
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
/**
 * A dollar position's dollars of a sum: every row's, the billed rows', or the plan rows' at the API price list. The
 * routes add a row's dollars to both sums in one pass, so their difference needs no tolerance, where the console adds
 * a session spend's billed dollars again from a map per funding.
 */
export const dollarsUnder = (unit: DollarPosition, s: FundedSum): number => (unit === 'all' ? s.cost : unit === 'usd' ? s.billedUsd : s.cost - s.billedUsd);
/** The figure a funded sum ranks by in the chosen unit: under native its dollars at the API price list, the one
 *  measure a billed-only key and a key with unpriced plan rows both have, under a dollar position its dollars. */
export const fundedRankOf = (unit: UnitPosition, sum: FundedSum): number => dollarsUnder(dollarsOf(unit), sum);
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
  return `${named} unpriced in ${plural(buckets, 'bucket, drawn as a gap', 'buckets, drawn as gaps')}`;
}
/** The Cost head's sentence above a figure of the routes' sums over rows that can hold a billed probe while the Include
 *  probes toggle is off: the routes add a billed probe's dollars to the cost and billed sums whatever the flag says. */
const BILLED_PROBE_NOTE = "A billed probe's dollars count whatever the Include probes toggle says";
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
 * draws as its skeleton. A dollar position reads its dollars under its own words, and reads no scale. `billedProbeNote`
 * says the figure is the routes' sums read with probes off over rows that can hold a billed probe, so a dollar
 * position's meta closes on `BILLED_PROBE_NOTE`.
 */
export function costHeadOf(unit: UnitPosition, totals: AnalyticsGroup, week: WeekPosition, scale: number | null, billedProbeNote: boolean): CostHead | null {
  if (unit === 'native') {
    if (scale === null) return null;
    const { figures, unpriced } = nativeFiguresOf(sidesOf(totals), week, scale);
    return { meta: `${figures.map((f) => f.text).join(' and ')} in range`, subset: NATIVE_SUBSET, tile: { figures, label: 'Cost', note: NATIVE_SUBSET }, unpriced };
  }
  const dollars = dollarsOf(unit);
  const figure = fmtUsd(totals.costUsd);
  return { meta: `${figure} in range, ${UNIT_META[dollars]}${billedProbeNote ? `. ${BILLED_PROBE_NOTE}` : ''}`, subset: UNIT_SUBSET[dollars], tile: { figures: [{ text: figure }], label: UNIT_TILE_LABEL[dollars], note: dollars === 'all' ? 'at the effective price list' : UNIT_SUBSET[dollars] }, unpriced: false };
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
 * The Pace score beside a lead: the remaining share, floored at nothing remaining, over the hours from the sample's
 * instant to its deadline, the earlier of the window's reset and the account's end. It is the ratio the routing preview
 * scores an account by (the preview adds its own minute floor over the engine's prediction rather than a raw sample).
 * Null without a deadline or a utilization, and null for a deadline at or before the sample, since the window has reset
 * or the account's end has passed since and left the sample stale.
 */
export const paceScoreOf = (sample: WindowSample): number | null => {
  if (sample.deadline === null || sample.utilization === null) return null;
  const hours = (Date.parse(sample.deadline) - Date.parse(sample.sampledAt)) / HOUR;
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
 * where the lead is, so the tooltip reads both off the row. The accounts are those that draw a point, a bucket with the
 * read's figure (a utilization under `usage`, a lead under `pace`), and the rows hold their keys alone.
 */
export function windowLines(rollups: AccountRollup[], window: string, read: 'pace' | 'usage', span: { fromMs: number; toMs: number }, bucketMs: number): { accounts: string[]; data: Record<string, number | null>[] } {
  const listed = [...new Set(rollups.map((r) => r.accountId))].sort((a, b) => a.localeCompare(b));
  const keysOf = read === 'pace' ? (a: string) => [a, `${a}:score`] : (a: string) => [a];
  const { bucketOf, byAt, data } = bucketRows(listed, span, bucketMs, keysOf);
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
  // Read after the fold, since a reset-only sample in a bucket's later hour leaves the bucket without a point.
  const accounts = listed.filter((a) => data.some((row) => row[a] !== null));
  // With no account dropped, the rows already hold the keys the rebuild keeps, in its order.
  if (accounts.length === listed.length) return { accounts, data };
  const keys = ['at', ...accounts.flatMap(keysOf)];
  return { accounts, data: data.map((row) => Object.fromEntries(keys.map((k) => [k, row[k]]))) };
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
 * The named groups the Burn tab's plan cards draw, in the one order every card's colors read: the sorted union of
 * each fold's lines less its rests, so a group one card draws while another folds it into its rest keeps a color.
 */
export const burnColorOrder = (folds: readonly { lines: readonly BurnLine[] }[]): string[] => [...new Set(folds.flatMap((f) => f.lines.filter((l) => !l.other).map((l) => l.key)))].sort();

const coveredEnd = (rangeEndMs: number, readAt: number, wholeHours: boolean): number => Math.min(wholeHours ? hourCeil(rangeEndMs) : rangeEndMs, readAt);

/**
 * The burn route's points, each with the figure a plan card hands in, as chart lines and rows. The request-class
 * grouping folds into its two legend entries, and every other grouping keeps the groups with the most tokens and sums
 * the rest as other, which draws no line of the card's share, so its figure stays empty while its dollars and plan
 * dollars sum. Under the account grouping the rest folds once per unit, since a subscription account's plan share and
 * an API-key account's dollars chart on different axes. Where each group holds one account's rows (that grouping, or
 * `account`, the one account the Accounts filter names) a line's unit is its account's. `sized` names the accounts
 * whose plan the router prices on a tier the cost model sizes (`pricedTier`, so a free plan and a usage-based seat are
 * out). Each slot carries the card's figure per hour under the chosen tier, which adds across accounts, the cause of a
 * gap it holds, and the plan side's list dollars per hour.
 *
 * `rangeEndMs` is the end of the range the points were read for and `nowMs` the instant of that read, the end when
 * absent. The route reads whole hours through the hour ceiling of the end, so a bucket covers its width up to the hour
 * ceiling of the range's end, no later than the read's instant, and its full width without an end.
 */
export function foldBurn(
  points: FiguredPoint[],
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
  // A figure is a number under any tier of the week map, whichever card folds, so a line's unit reads no week position.
  const figured = new Set(points.filter((p) => Object.values(p.equivalentByTier).some((v) => v !== null)).map((p) => lineOf(p.group)));
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
    const sums = byAt.get(at) ?? Array.from({ length: slots }, (): Sum => ({ cached: 0, equivalent: { sum: null, unpriced: false, unsized: false }, planUsd: 0, tokens: 0, uncached: 0, usd: 0 }));
    byAt.set(at, sums);
    const slot = slotOf(p.group);
    const s = sums[slot];
    s.cached += p.cachedTokens;
    s.planUsd += p.planUsd;
    s.tokens += p.tokens;
    s.uncached += p.uncachedTokens;
    s.usd += p.usd;
    if (slot >= kept.length) continue;
    if (p.figure !== null) s.equivalent.sum = (s.equivalent.sum ?? 0) + p.figure;
    if (p.gap === 'unpriced') s.equivalent.unpriced = true;
    if (p.gap === 'unsized') s.equivalent.unsized = true;
  }
  const secondsOf = (at: number) => {
    if (rangeEndMs === undefined) return bucketSeconds;
    const covered = Math.min(at + bucketSeconds * 1_000, coveredEnd(rangeEndMs, nowMs ?? rangeEndMs, true));
    return Math.max(1, (covered - at) / 1_000);
  };
  const perHour = (f: FigureFold, hours: number) => (f.unpriced || f.unsized || f.sum === null ? null : f.sum / hours);
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
        planUsd: sums.map((s) => s.planUsd / hours),
        seconds,
        setsScale: seconds >= bucketSeconds * SCALE_FLOOR,
        tokens: sums.map((s) => s.tokens),
        uncached: sums.map((s) => s.uncached / minutes),
        unsized: sums.map((s) => s.equivalent.unsized),
        usd: sums.map((s) => s.usd / minutes),
      };
    });
  // Weighed by the share of its width a bucket covers rather than its seconds: the same mean, and a range of full
  // buckets reads the plain mean to the last bit.
  const weightOf = (r: BurnRow) => r.seconds / bucketSeconds;

  const mean = (read: (r: BurnRow) => number) => (rows.length ? weighedMean(rows.map((r) => ({ value: read(r), weight: weightOf(r) }))) : 0);
  const scaleRows = rows.filter((r) => r.setsScale);
  const lines = Array.from({ length: slots }, (_, i): BurnLine => {
    const rest = i >= kept.length ? rests[i - kept.length] : null;
    // A line that never priced traffic stays null in every slot, idle ones included, so its legend reads a dash. One
    // that priced counts an idle slot as zero and keeps a gap null. An idle slot's own figure never makes the line
    // priced: a failed request's row carries no token and a listed model prices it at zero. The card names the cause of
    // each gap, so a slot with no token or with no figure and neither cause is idle.
    const fold = (k: number) => buckets[k][1][i].equivalent;
    const idle = (r: BurnRow, k: number) => r.tokens[i] === 0 || (r.equivalent[i] === null && !fold(k).unpriced && !fold(k).unsized);
    const priced = rows.some((r) => r.tokens[i] > 0 && r.equivalent[i] !== null);
    rows.forEach((r, k) => {
      if (idle(r, k)) r.equivalent[i] = priced ? 0 : null;
    });
    const figures = rows.flatMap((r) => (r.equivalent[i] === null ? [] : [{ value: r.equivalent[i] as number, weight: weightOf(r) }]));
    const peaks = scaleRows.map((r) => r.equivalent[i]).filter((v): v is number => v !== null);
    const planPeaks = scaleRows.map((r) => r.planUsd[i]);
    const figuresOf = {
      peak: peaks.length ? Math.max(...peaks) : null,
      perHour: priced ? weighedMean(figures) : null,
      planUsdPeak: planPeaks.length ? Math.max(...planPeaks) : null,
      planUsdPerHour: mean((r) => r.planUsd[i]),
      tokensPerMinute: mean((r) => r.cached[i] + r.uncached[i]),
      unpriced: rest === null && rows.some((_, k) => fold(k).unpriced),
      unsized: rest === null && rows.some((_, k) => fold(k).unsized),
      usdPerMinute: mean((r) => r.usd[i]),
    };
    if (rest === null) return { ...figuresOf, key: kept[i], label: burnLabelOf(groupBy, kept[i]), other: false, unit: unitOf(kept[i]) };
    return { ...figuresOf, key: byAccount ? `${OTHER_KEY}-${rest}` : OTHER_KEY, label: byAccount ? OTHER_LABEL[rest] : 'Other', other: true, unit: rest };
  });
  return { lines, rows };
}

/**
 * Whether a bucket chart's last bucket is partial: the covered end, the end of the window `coveredWindow` answers for
 * the read, lies before the bucket's own end.
 */
export const lastBucketPartial = (buckets: readonly { at: number }[], bucketMs: number, coveredToMs: number): boolean => {
  const last = buckets[buckets.length - 1];
  return last !== undefined && coveredToMs < last.at + bucketMs;
};

/** How a read covers its range: a rollup read sums whole hours, the burn read starts at its first bucket, and a
 *  session view folds the request log over the range itself. */
type ReadSource = 'burn' | 'rollup' | 'session';
/** The window a read covers. Its end is the hour ceiling of the range's end and no later than the read's instant,
 *  the range's own end in a session view. Its start is the hour that holds the range's start for a rollup read,
 *  the first bucket's start for the burn read, and the range's own start in a session view. A burn answer with no
 *  bucket reaches back to nothing, so it starts at the range's own start. */
export function coveredWindow(source: ReadSource, range: { fromMs: number; toMs: number }, readAt: number, firstBucketAt: number | null): { fromMs: number; toMs: number } {
  const fromMs = source === 'rollup' ? hourFloor(range.fromMs) : source === 'burn' && firstBucketAt !== null ? firstBucketAt : range.fromMs;
  return { fromMs, toMs: coveredEnd(range.toMs, readAt, source !== 'session') };
}
/** Whether the first bucket holds only part of its span: the covered start lies after the bucket's own start. */
export const firstBucketPartial = (buckets: readonly { at: number }[], coveredFromMs: number): boolean => buckets.length > 0 && coveredFromMs > buckets[0].at;

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
