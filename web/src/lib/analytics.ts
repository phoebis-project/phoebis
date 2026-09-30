import type { BurnGroup, BurnPoint, BurnUnit, PinMigrationReason, Placement, PlacementRow } from '@/api/types';
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
 * The unit a burn line charts in. `unknown` is a subscription upstream the route priced no share for, its tier
 * missing or one the tier factor table does not size: it has no share to draw, and dollars only for what its
 * fast-mode answers spent.
 */
export type BurnLineUnit = BurnUnit | 'unknown';
const OTHER_LABEL: Record<BurnLineUnit, string> = { unknown: 'Other (tier unknown)', usd: 'Other (dollars)', window_share: 'Other (plan)' };

export interface BurnLine {
  /** The highest 5-hour share per hour the range reached; null for a line with no share in any bucket. */
  fiveHourPeak: number | null;
  /** The range's 5-hour share per hour, a bucket without a share counting as none; null with no share at all. */
  fiveHourPerHour: number | null;
  /** Whether a bucket's 5-hour share was priced through a cell or tier factor no measurement has filled. */
  fiveHourUnmeasured: boolean;
  key: string;
  label: string;
  /** Whether this is the folded rest rather than one group. */
  other: boolean;
  sevenDayPerHour: number | null;
  sevenDayUnmeasured: boolean;
  tokensPerMinute: number;
  /**
   * The unit the line is charted in: under the upstream grouping a subscription upstream burns window share and an
   * API-key one dollars; every other grouping mixes both, so it charts dollars.
   */
  unit: BurnLineUnit;
  usdPerMinute: number;
}

/** One bucket, the token and dollar figures per minute of the bucket and the shares per hour, indexed like the lines. */
export interface BurnRow {
  at: number;
  cached: number[];
  /** Null where no priced row landed in the slot, so a missing figure never reads as a zero share. */
  fiveHour: (number | null)[];
  fiveHourUnmeasured: boolean[];
  sevenDay: (number | null)[];
  sevenDayUnmeasured: boolean[];
  uncached: number[];
  usd: number[];
}

interface Sum {
  cached: number;
  fiveHour: number | null;
  fiveHourUnmeasured: boolean;
  sevenDay: number | null;
  sevenDayUnmeasured: boolean;
  uncached: number;
  usd: number;
}

const LEGEND_ORDER: ClassLegend[] = ['conversation', 'overhead'];

/**
 * The burn route's points as chart lines and rows. The request-class grouping folds into its two legend entries;
 * every other grouping keeps the groups with the most tokens and sums the rest as other, whose shares stay empty
 * since shares of different windows do not add up. Under the upstream grouping the rest folds once per unit, since
 * a subscription upstream's window share and an API-key upstream's dollars chart on different axes.
 */
export function foldBurn(points: BurnPoint[], groupBy: BurnGroup, bucketSeconds: number, max = MAX_BURN_LINES): { lines: BurnLine[]; rows: BurnRow[] } {
  const lineOf = (group: string) => (groupBy === 'request_class' ? classLegendOf(group) : group);
  const tokens = new Map<string, number>();
  for (const p of points) tokens.set(lineOf(p.group), (tokens.get(lineOf(p.group)) ?? 0) + p.cachedTokens + p.uncachedTokens);
  // A share marks a subscription upstream the tables price; the route answers the unit for one they cannot, whose
  // points carry no share, and dollars only for its fast-mode answers.
  const byUpstream = groupBy === 'account';
  const priced = new Set(byUpstream ? points.filter((p) => p.fiveHourShare !== null).map((p) => p.group) : []);
  const subscribed = new Set(byUpstream ? points.filter((p) => p.unit === 'window_share').map((p) => p.group) : []);
  const unitOf = (group: string): BurnLineUnit => (priced.has(group) ? 'window_share' : subscribed.has(group) ? 'unknown' : 'usd');
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
    const sums = byAt.get(at) ?? Array.from({ length: slots }, (): Sum => ({ cached: 0, fiveHour: null, fiveHourUnmeasured: false, sevenDay: null, sevenDayUnmeasured: false, uncached: 0, usd: 0 }));
    byAt.set(at, sums);
    const slot = slotOf(p.group);
    const s = sums[slot];
    s.cached += p.cachedTokens;
    s.uncached += p.uncachedTokens;
    s.usd += p.usd;
    if (slot >= kept.length) continue;
    if (p.fiveHourShare !== null) {
      s.fiveHour = (s.fiveHour ?? 0) + p.fiveHourShare;
      s.fiveHourUnmeasured ||= p.fiveHourUnmeasured;
    }
    if (p.sevenDayShare !== null) {
      s.sevenDay = (s.sevenDay ?? 0) + p.sevenDayShare;
      s.sevenDayUnmeasured ||= p.sevenDayUnmeasured;
    }
  }
  const minutes = bucketSeconds / 60;
  const hours = bucketSeconds / 3_600;
  const rows = [...byAt.entries()]
    .sort((a, b) => a[0] - b[0])
    .map(([at, sums]) => ({
      at,
      cached: sums.map((s) => s.cached / minutes),
      fiveHour: sums.map((s) => (s.fiveHour === null ? null : s.fiveHour / hours)),
      fiveHourUnmeasured: sums.map((s) => s.fiveHourUnmeasured),
      sevenDay: sums.map((s) => (s.sevenDay === null ? null : s.sevenDay / hours)),
      sevenDayUnmeasured: sums.map((s) => s.sevenDayUnmeasured),
      uncached: sums.map((s) => s.uncached / minutes),
      usd: sums.map((s) => s.usd / minutes),
    }));

  const mean = (read: (r: BurnRow) => number) => (rows.length ? rows.reduce((n, r) => n + read(r), 0) / rows.length : 0);
  const perHour = (read: (r: BurnRow) => number | null) => (rows.some((r) => read(r) !== null) ? mean((r) => read(r) ?? 0) : null);
  const lines = Array.from({ length: slots }, (_, i): BurnLine => {
    const rest = i >= kept.length ? rests[i - kept.length] : null;
    const shares = rows.map((r) => r.fiveHour[i]).filter((v): v is number => v !== null);
    const figures = {
      fiveHourPeak: shares.length ? Math.max(...shares) : null,
      fiveHourPerHour: perHour((r) => r.fiveHour[i]),
      fiveHourUnmeasured: rows.some((r) => r.fiveHourUnmeasured[i]),
      sevenDayPerHour: perHour((r) => r.sevenDay[i]),
      sevenDayUnmeasured: rows.some((r) => r.sevenDayUnmeasured[i]),
      tokensPerMinute: mean((r) => r.cached[i] + r.uncached[i]),
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
