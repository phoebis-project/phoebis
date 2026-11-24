import { fmtCompactTick } from '@/lib/format';

// Chart colors are read from the CSS custom properties so both themes stay in
// one place. Recharts needs literal values, so they are resolved at render time.
// The ninth slot, `--series-muted`, draws a series no account answered, so the eye passes over it.
export const SERIES_VARS = ['--series-1', '--series-2', '--series-3', '--series-4', '--series-5', '--series-6', '--series-7', '--series-8', '--series-muted'] as const;
/** One-hue ordinal ramp, light to dark, for ordered series such as percentiles and request phases. */
export const RAMP_VARS = ['--ramp-1', '--ramp-2', '--ramp-3', '--ramp-4', '--ramp-5'] as const;

export function cssVar(name: string): string {
  if (typeof window === 'undefined') return '#888';
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim() || '#888';
}

export interface ChartInk {
  accent: string;
  axis: string;
  critical: string;
  good: string;
  grid: string;
  ink2: string;
  ink3: string;
  muted: string;
  ramp: string[];
  serious: string;
  series: string[];
  surface: string;
  warning: string;
}

export function readChartInk(): ChartInk {
  return {
    accent: cssVar('--accent'),
    axis: cssVar('--axis'),
    critical: cssVar('--critical'),
    good: cssVar('--good'),
    grid: cssVar('--grid'),
    ink2: cssVar('--ink-2'),
    ink3: cssVar('--ink-3'),
    muted: cssVar('--series-muted'),
    ramp: RAMP_VARS.map(cssVar),
    serious: cssVar('--serious'),
    series: SERIES_VARS.map(cssVar),
    surface: cssVar('--surface'),
    warning: cssVar('--warning'),
  };
}

/** The series slots an account can take: every entry of SERIES_VARS but the muted ninth. */
const ACCOUNT_SLOT_COUNT = SERIES_VARS.length - 1;

/**
 * Color follows the entity: an account's slot is its index in the registry's order (every account sorted by id, as
 * `GET /admin/accounts` answers) modulo the eight series colors, so one account keeps one color across the
 * Dashboard, the Quota view and the Analytics whatever each view filters. An id outside the list answers `null`.
 */
export function accountSlots(ids: readonly string[]): (id: string) => number | null {
  const slots = new Map([...ids].sort().map((id, index) => [id, index % ACCOUNT_SLOT_COUNT] as const));
  return (id) => slots.get(id) ?? null;
}

/** The slot's series ink, or the muted ink for an account the registry no longer lists. */
export const accountColor = (ink: ChartInk, slot: number | null) => (slot === null ? ink.muted : ink.series[slot]);

/** Ordered by how an outcome degrades rather than alphabetically, like the token and probe stacks below. */
export const OUTCOME_SERIES = [
  { key: 'succeeded', label: 'OK', slot: 0 },
  { key: 'rateLimited', label: 'Rate limited', slot: 3 },
  { key: 'failed', label: 'Failed', slot: 7 },
  { key: 'incomplete', label: 'Incomplete', slot: 2 },
  { key: 'clientCanceled', label: 'Client canceled', slot: 4 },
  { key: 'exhausted', label: 'Exhausted', slot: 1 },
  // No account answered a rejected request, so it takes the muted slot and the muted series closes the stack.
  { key: 'rejected', label: 'Rejected', slot: 8 },
] as const;

export const TOKEN_SERIES = [
  { key: 'uncachedInput', label: 'Uncached input', slot: 0 },
  { key: 'cacheRead', label: 'Cache read', slot: 2 },
  { key: 'cacheWrite5m', label: 'Cache write 5m', slot: 3 },
  { key: 'cacheWrite1h', label: 'Cache write 1h', slot: 1 },
  { key: 'thinking', label: 'Thinking', slot: 5 },
  { key: 'plainOutput', label: 'Output', slot: 6 },
] as const;

const HELD_PROBE_HINT = 'The router refused the probe while its ledger caught up. It was sent again and never adds to the cold streak.';

/** Ordered best outcome first rather than alphabetically, so the stack reads top to bottom as it degrades. */
export const PROBE_SERIES: { hint?: string; key: 'failed' | 'held' | 'ok' | 'skipped'; label: string; slot: number }[] = [
  { key: 'ok', label: 'OK', slot: 0 },
  { key: 'held', label: 'Held', slot: 8, hint: HELD_PROBE_HINT },
  { key: 'skipped', label: 'Skipped', slot: 3 },
  { key: 'failed', label: 'Failed', slot: 7 },
];

/** Percentile lines read from the ramp: p50 lightest, p99 darkest and emphasized. */
export const PERCENTILES = [
  { key: 'p50', label: 'p50', p: 0.5, rampIndex: 0 },
  { key: 'p90', label: 'p90', p: 0.9, rampIndex: 1 },
  { key: 'p95', label: 'p95', p: 0.95, rampIndex: 2 },
  { key: 'p99', label: 'p99', p: 0.99, rampIndex: 4 },
] as const;

/** A chart card's plot height, its skeleton's too, so a card keeps its height as the answer lands. */
export const CHART_HEIGHT = 220;

export const PCT_AXIS = { axisLine: false, domain: [0, 100], tickFormatter: (v: number) => `${v}%`, tickLine: false, ticks: [0, 25, 50, 75, 100], width: 40 };
const BAR_GAP_PERCENT = 14;
/** The gap on each side of a bucket's bar as a share of the bucket's width, so that a bar keeps a width at each
 *  bucket count. 14% is the 2 px of a desktop's plot under 24 hours and under 7 days. */
export const BAR_GAP = `${BAR_GAP_PERCENT}%`;
/** The gaps of a chart with two bars a bucket: twice the side gap, and one gap between the bars. That gap is a share
 *  too, since Recharts keeps a gap in pixels until the bucket holds no more than the gaps, which leaves the bars a
 *  fraction of a pixel just above that width. */
export const PAIRED_BAR_GAPS = { barCategoryGap: `${2 * BAR_GAP_PERCENT}%`, barGap: BAR_GAP };
/** A count axis: compact ticks in the token axes' width, since the chart's box clips a tick wider than its axis. */
export const COUNT_AXIS = { allowDecimals: false, axisLine: false, tickFormatter: (v: number) => fmtCompactTick(v), tickLine: false, width: 44 };
