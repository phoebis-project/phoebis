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
/** A reason the console has no label for reads as the router sent it. */
export const moveLabel = (reason: string): string => (Object.prototype.hasOwnProperty.call(PIN_MIGRATION_LABEL, reason) ? PIN_MIGRATION_LABEL[reason as PinMigrationReason] : reason);

/** The classes that refresh a pin's warmth, the routing crate's `REFRESHING_CLASSES`: the conversation itself. */
export const CONVERSATION_CLASSES = ['compaction', 'main', 'subagent'] as const;
/** The classes the client documents; a class it adds later still reads as sent. */
export const REQUEST_CLASS_VALUES = ['auxiliary', 'compaction', 'main', 'subagent', 'workflow'] as const;

export type ClassLegend = 'conversation' | 'overhead';

/** Conversation or the overhead around it; `-`, the rollups' name for a request that sent no class, is overhead. */
export const classLegendOf = (requestClass: string): ClassLegend => ((CONVERSATION_CLASSES as readonly string[]).includes(requestClass) ? 'conversation' : 'overhead');

/** Lines a burn chart draws before the rest fold into one, the stacked cost chart's limit. */
export const MAX_BURN_LINES = 7;
/** The key the folded rest carries; colours and React keys read it, and no group is ever looked up by it. */
export const OTHER_KEY = 'other';

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
 * empty since shares of different windows do not add up.
 */
export function foldBurn(points: BurnPoint[], groupBy: BurnGroup, bucketSeconds: number, max = MAX_BURN_LINES): { lines: BurnLine[]; rows: BurnRow[] } {
  const lineOf = (group: string) => (groupBy === 'request_class' ? classLegendOf(group) : group);
  const tokens = new Map<string, number>();
  for (const p of points) tokens.set(lineOf(p.group), (tokens.get(lineOf(p.group)) ?? 0) + p.cachedTokens + p.uncachedTokens);
  const ranked = [...tokens.entries()].sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0])).map(([k]) => k);
  const kept: string[] = groupBy === 'request_class' ? LEGEND_ORDER : ranked.slice(0, max);
  // The rest takes the slot after the kept lines, so a group whose id happens to read `other` stays its own line.
  const slots = kept.length + (groupBy !== 'request_class' && ranked.length > max ? 1 : 0);
  const slotOf = (group: string) => (kept.includes(lineOf(group)) ? kept.indexOf(lineOf(group)) : kept.length);

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
    const other = i === kept.length;
    const shares = rows.map((r) => r.plan[i]).filter((v): v is number => v !== null);
    const meterPerMinute = mean((r) => r.meter[i]);
    return {
      key: other ? OTHER_KEY : kept[i],
      label: other ? 'Other' : kept[i] === '-' ? 'unattributed' : kept[i],
      meterPerMinute,
      other,
      planPeak: shares.length ? Math.max(...shares) : null,
      tokensPerMinute: mean((r) => r.cached[i] + r.uncached[i]),
      unit: groupBy === 'account' && (meterPerMinute > 0 || shares.length > 0) ? 'meter_tokens' : 'usd',
      usdPerMinute: mean((r) => r.usd[i]),
    };
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
