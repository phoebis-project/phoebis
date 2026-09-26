import type { BurnGroup, BurnPoint, BurnUnit, PinMigrationReason } from '@/api/types';

/** Why a session's pin moved, as the request drawer, its selection trace and the rewrites legend word it. */
export const PIN_MIGRATION_LABEL: Record<PinMigrationReason, string> = {
  cooldown: 'rate limited',
  drain: 'winding down',
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
const OTHER_LABEL: Record<BurnUnit, string> = { meter_tokens: 'Other (plan)', usd: 'Other (dollars)' };

export interface BurnLine {
  key: string;
  label: string;
  meterPerMinute: number;
  /** Whether this is the folded rest rather than one group. */
  other: boolean;
  /** The highest plan share per hour the range reached; null for a line with no share in any bucket. */
  planPeak: number | null;
  tokensPerMinute: number;
  /**
   * The unit the line is charted in: under the upstream grouping a subscription upstream burns meter tokens and an
   * API-key one dollars; every other grouping mixes both, so it charts dollars.
   */
  unit: BurnUnit;
  usdPerMinute: number;
}

/** One bucket, each figure per minute of the bucket and the plan share per hour, indexed like the lines. */
export interface BurnRow {
  at: number;
  cached: number[];
  meter: number[];
  plan: (number | null)[];
  uncached: number[];
  usd: number[];
}

interface Sum {
  cached: number;
  meter: number;
  plan: number | null;
  uncached: number;
  usd: number;
}

const LEGEND_ORDER: ClassLegend[] = ['conversation', 'overhead'];

/**
 * The burn route's points as chart lines and rows. The request-class grouping folds into its two legend entries;
 * every other grouping keeps the groups with the most tokens and sums the rest as other, whose plan share stays
 * empty since shares of different windows do not add up. Under the upstream grouping the rest folds once per unit,
 * since a subscription upstream's meter tokens and an API-key upstream's dollars chart on different axes.
 */
export function foldBurn(points: BurnPoint[], groupBy: BurnGroup, bucketSeconds: number, max = MAX_BURN_LINES): { lines: BurnLine[]; rows: BurnRow[] } {
  const lineOf = (group: string) => (groupBy === 'request_class' ? classLegendOf(group) : group);
  const tokens = new Map<string, number>();
  for (const p of points) tokens.set(lineOf(p.group), (tokens.get(lineOf(p.group)) ?? 0) + p.cachedTokens + p.uncachedTokens);
  // A plan share or meter tokens mark a subscription upstream; the route answers neither for an API-key one.
  const metered = new Set(groupBy === 'account' ? points.filter((p) => p.meterTokens > 0 || p.planPercent !== null).map((p) => p.group) : []);
  const unitOf = (group: string): BurnUnit => (metered.has(group) ? 'meter_tokens' : 'usd');
  const ranked = [...tokens.entries()].sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0])).map(([k]) => k);
  const kept: string[] = groupBy === 'request_class' ? LEGEND_ORDER : ranked.slice(0, max);
  const folded = groupBy === 'request_class' ? [] : ranked.slice(max);
  const rests = (['meter_tokens', 'usd'] as const).filter((unit) => folded.some((g) => unitOf(g) === unit));
  // The rests take the slots after the kept lines, so a group whose id happens to read `other` stays its own line.
  const slots = kept.length + rests.length;
  const slotOf = (group: string) => (kept.includes(lineOf(group)) ? kept.indexOf(lineOf(group)) : kept.length + rests.indexOf(unitOf(group)));

  const byAt = new Map<number, Sum[]>();
  for (const p of points) {
    const at = Date.parse(p.at);
    const sums = byAt.get(at) ?? Array.from({ length: slots }, (): Sum => ({ cached: 0, meter: 0, plan: null, uncached: 0, usd: 0 }));
    byAt.set(at, sums);
    const slot = slotOf(p.group);
    const s = sums[slot];
    s.cached += p.cachedTokens;
    s.uncached += p.uncachedTokens;
    s.meter += p.meterTokens;
    s.usd += p.usd;
    if (p.planPercent !== null && slot < kept.length) s.plan = (s.plan ?? 0) + p.planPercent;
  }
  const minutes = bucketSeconds / 60;
  const hours = bucketSeconds / 3_600;
  const rows = [...byAt.entries()]
    .sort((a, b) => a[0] - b[0])
    .map(([at, sums]) => ({
      at,
      cached: sums.map((s) => s.cached / minutes),
      meter: sums.map((s) => s.meter / minutes),
      plan: sums.map((s) => (s.plan === null ? null : s.plan / hours)),
      uncached: sums.map((s) => s.uncached / minutes),
      usd: sums.map((s) => s.usd / minutes),
    }));

  const mean = (read: (r: BurnRow) => number) => (rows.length ? rows.reduce((n, r) => n + read(r), 0) / rows.length : 0);
  const lines = Array.from({ length: slots }, (_, i): BurnLine => {
    const rest = i >= kept.length ? rests[i - kept.length] : null;
    const shares = rows.map((r) => r.plan[i]).filter((v): v is number => v !== null);
    const figures = { meterPerMinute: mean((r) => r.meter[i]), planPeak: shares.length ? Math.max(...shares) : null, tokensPerMinute: mean((r) => r.cached[i] + r.uncached[i]), usdPerMinute: mean((r) => r.usd[i]) };
    if (rest === null) return { ...figures, key: kept[i], label: kept[i] === '-' ? 'unattributed' : kept[i], other: false, unit: unitOf(kept[i]) };
    const byUnit = groupBy === 'account';
    return { ...figures, key: byUnit ? `${OTHER_KEY}-${rest}` : OTHER_KEY, label: byUnit ? OTHER_LABEL[rest] : 'Other', other: true, unit: rest };
  });
  return { lines, rows };
}

/** How the fits route names a window, as the tables label it. */
export const FIT_WINDOW_LABEL: Record<string, string> = { five_hour: '5-hour', seven_day: '7-day', seven_day_fable: '7-day Fable' };

/** Past this share off the table's prediction, the fit table colours a drift. */
export const DRIFT_TOLERANCE = 0.2;
/** The fits route's own provisional tolerance on the band factor, named on the table's hint. */
export const BAND_TOLERANCE = 0.15;

/** The fitted scale against the prediction as a signed share: 1.25 reads +25.0%, 0.5 reads -50.0%. */
export const driftLabel = (drift: number) => {
  const off = drift - 1;
  return `${off > 0 ? '+' : ''}${(off * 100).toFixed(1)}%`;
};
export const driftWide = (drift: number | null) => drift !== null && Math.abs(drift - 1) > DRIFT_TOLERANCE;
