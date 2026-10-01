import type { Account, BurnGroup, BurnPoint, BurnUnit, CostTable, PinMigrationReason, Placement, PlacementRow } from '@/api/types';
import { fmtCompact, fmtInt, fmtPct } from '@/lib/format';

/** Why a session's pin moved, as the request drawer, its selection trace and the rewrites legend word it. */
export const PIN_MIGRATION_LABEL: Record<PinMigrationReason, string> = {
  cooldown: 'rate limited',
  drain: 'winding down',
  fast_lane: 'fast lane cooling',
  mark: 'upstream marked',
  overage: 'overage',
  return_to_plan: 'returned to plan',
  shed: 'shed',
  spent: 'window spent',
};
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
/** The key the folded rest carries, suffixed by its unit under the upstream grouping; no group is looked up by it. */
export const OTHER_KEY = 'other';
/**
 * The unit a burn line charts in. `unknown` is a subscription upstream the cost table does not size, its tier missing
 * or one the table has no factors for: it has no share to draw, and dollars only for what its fast-mode answers spent.
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
   * The unit the line is charted in: under the upstream grouping a subscription upstream burns window share and an
   * API-key one dollars; every other grouping mixes both, so it charts dollars.
   */
  unit: BurnLineUnit;
  /**
   * Per window, whether a bucket with tokens has no share on it: a model the table has no row for, or a cell it
   * leaves unmeasured. False on a line that does not chart window share.
   */
  unpriced: { fiveHour: boolean; sevenDay: boolean };
  usdPerMinute: number;
}

/** One bucket, the token and dollar figures per minute of the bucket and the shares per hour, indexed like the lines. */
export interface BurnRow {
  at: number;
  cached: number[];
  /**
   * Null where the slot's traffic could not be priced, or on a window the line never priced; an idle slot reads zero
   * on a window the line priced.
   */
  fiveHour: (number | null)[];
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
 * The cost table's factors for a stored tier, matched as the ledger matches it: as stored, then with the `default_`
 * prefix taken off or put on, since only one spelling has been seen on the wire. Undefined for a tier it does not size.
 */
export function tierFactors(tiers: CostTable['tiers'], tier: string): CostTable['tiers'][string] | undefined {
  const bare = tier.startsWith('default_') ? tier.slice('default_'.length) : null;
  const key = [tier, bare, `default_${tier}`].find((k): k is string => k !== null && own(tiers, k));
  return key === undefined ? undefined : tiers[key];
}

/**
 * The upstreams whose stored tier the cost table sizes, matched as the ledger matches it: as stored, then with the
 * `default_` prefix taken off or put on.
 */
export function sizedUpstreams(accounts: Account[], tiers: CostTable['tiers']): Set<string> {
  return new Set(accounts.filter((a) => a.subscription !== null && tierFactors(tiers, a.subscription.rateLimitTier) !== undefined).map((a) => a.id));
}

const meanOf = (values: number[]) => values.reduce((n, v) => n + v, 0) / values.length;

const LEGEND_ORDER: ClassLegend[] = ['conversation', 'overhead'];

/**
 * The burn route's points as chart lines and rows. The request-class grouping folds into its two legend entries;
 * every other grouping keeps the groups with the most tokens and sums the rest as other, whose shares stay empty
 * since shares of different windows do not add up. Under the upstream grouping the rest folds once per unit, since
 * a subscription upstream's window share and an API-key upstream's dollars chart on different axes. `sized` names
 * the upstreams whose tier the cost table sizes, which chart in window share whatever their buckets priced.
 */
export function foldBurn(points: BurnPoint[], groupBy: BurnGroup, bucketSeconds: number, sized: ReadonlySet<string>, max = MAX_BURN_LINES): { lines: BurnLine[]; rows: BurnRow[] } {
  const lineOf = (group: string) => (groupBy === 'request_class' ? classLegendOf(group) : group);
  const tokens = new Map<string, number>();
  for (const p of points) tokens.set(lineOf(p.group), (tokens.get(lineOf(p.group)) ?? 0) + p.cachedTokens + p.uncachedTokens);
  // The category comes from the tier on file rather than from whether a bucket priced, so an upstream whose range
  // ran on unmeasured models alone still charts as a plan line and its unpriced windows show.
  const byUpstream = groupBy === 'account';
  const subscribed = new Set(byUpstream ? points.filter((p) => p.unit === 'window_share').map((p) => p.group) : []);
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
  const minutes = bucketSeconds / 60;
  const hours = bucketSeconds / 3_600;
  const rows = [...byAt.entries()]
    .sort((a, b) => a[0] - b[0])
    .map(([at, sums]) => ({
      at,
      cached: sums.map((s) => s.cached / minutes),
      fiveHour: sums.map((s) => (s.fiveHour === null ? null : s.fiveHour / hours)),
      sevenDay: sums.map((s) => (s.sevenDay === null ? null : s.sevenDay / hours)),
      tokens: sums.map((s) => s.tokens),
      uncached: sums.map((s) => s.uncached / minutes),
      usd: sums.map((s) => s.usd / minutes),
    }));

  const mean = (read: (r: BurnRow) => number) => (rows.length ? rows.reduce((n, r) => n + read(r), 0) / rows.length : 0);
  const lines = Array.from({ length: slots }, (_, i): BurnLine => {
    const rest = i >= kept.length ? rests[i - kept.length] : null;
    // A window the line never priced traffic on stays null in every slot, idle ones included, so its legend reads a
    // dash; one it priced counts an idle slot (no token of any type) as zero, and a slot with tokens and no share
    // stays null. An idle slot's own share never makes the window priced: a failed request's row carries no token
    // and a listed model prices it at zero.
    const foldWindow = (read: (r: BurnRow) => (number | null)[]) => {
      const priced = rows.some((r) => r.tokens[i] > 0 && read(r)[i] !== null);
      for (const r of rows) if (r.tokens[i] === 0) read(r)[i] = priced ? 0 : null;
      return { perHour: priced ? meanOf(rows.map((r) => read(r)[i]).filter((v): v is number => v !== null)) : null, unpriced: rows.some((r) => slotUnpriced(read(r)[i], r.tokens[i])) };
    };
    const fiveHour = foldWindow((r) => r.fiveHour);
    const sevenDay = foldWindow((r) => r.sevenDay);
    const shares = rows.map((r) => r.fiveHour[i]).filter((v): v is number => v !== null);
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
