import type { Reconciliation } from '@/api/types';
import { judged, RATIO_HIGH, RATIO_LOW, ratioOff } from './analytics';
import { byteOrder } from './format';
import { DAY } from './time';
import { windowRank } from './windows';

export const METER_HINT =
  "Each subscription account's completed windows of the last 30 days, grouped by account and window kind: the rise the upstream's readings showed against the share the router priced for its own requests in it. 1.1 means the meter charged 1.1 times the table. A ratio under 0.8 or over 1.25 after a rise of at least 5% is marked.";
export const UNJUDGED_TITLE = 'the rise or the priced share is under 5%, so the band does not judge this ratio';

/** The span the route answers and the spark draws: the trailing 30 days. */
const SPAN_MS = 30 * DAY;

/** The windows' order: the 5-hour window, the 7-day one, the known model week, then any other key by its spelling. */
export const byWindow = (a: string, b: string) => windowRank(a) - windowRank(b) || byteOrder(a, b);
const newestFirst = (a: Reconciliation, b: Reconciliation) => Date.parse(b.windowEnd) - Date.parse(a.windowEnd);

export interface MeterGroup {
  accountId: string;
  judged: number;
  latest: Reconciliation;
  leftOut: number;
  off: number;
  rows: Reconciliation[];
  window: string;
}

/** The weekly windows are the 7-day one and every model week, the route's `seven_day` family. */
const isWeekly = (window: string) => window.startsWith('seven_day');

/** The rows grouped by account and window kind, the weekly windows apart from the 5-hour ones, each group's rows newest
 *  first, the groups by account id and inside an account the 7-day window before the model weeks. */
export function meterGroups(rows: Reconciliation[]): { fiveHour: MeterGroup[]; weekly: MeterGroup[] } {
  const byKey = new Map<string, Reconciliation[]>();
  for (const r of rows) {
    const key = `${r.accountId}\u0000${r.window}`;
    const list = byKey.get(key);
    if (list) list.push(r);
    else byKey.set(key, [r]);
  }
  const groups = [...byKey.values()]
    .map((list): MeterGroup => {
      const sorted = [...list].sort(newestFirst);
      const judgedCount = sorted.filter(judged).length;
      return { accountId: sorted[0].accountId, judged: judgedCount, latest: sorted[0], leftOut: sorted.length - judgedCount, off: sorted.filter(ratioOff).length, rows: sorted, window: sorted[0].window };
    })
    .sort((a, b) => byteOrder(a.accountId, b.accountId) || byWindow(a.window, b.window));
  return { fiveHour: groups.filter((g) => !isWeekly(g.window)), weekly: groups.filter((g) => isWeekly(g.window)) };
}

/** How many windows of a group an open group lists at first. */
export const GROUP_PAGE = { fiveHour: 10, weekly: 25 } as const;
/** How many more windows each "Show 25 more" lists. */
export const GROUP_STEP = 25;

export const SPARK = { height: 24, width: 120, floor: 0.5, ceiling: 2, band: [RATIO_LOW, RATIO_HIGH] } as const;
/** A point keeps 3 px from either side, so an off window's 2.25 px dot at an edge stays inside the 120 px the column
 *  is sized to, and 2 px from the top and the bottom. */
const INSET_X = 3;
const INSET_Y = 2;

export interface SparkPoint {
  off: boolean;
  x: number;
  y: number;
}

/** A ratio's height in the spark: its base-2 log clipped to the floor and the ceiling, the ceiling at the top. */
export function sparkY(ratio: number): number {
  const low = Math.log2(SPARK.floor);
  const high = Math.log2(SPARK.ceiling);
  const v = Math.min(high, Math.max(low, Math.log2(ratio)));
  return INSET_Y + ((high - v) / (high - low)) * (SPARK.height - 2 * INSET_Y);
}

/** The spark's points over the trailing 30 days to the newest reconciliation, the y axis the ratio's base-2 log clipped
 *  to the floor and the ceiling, the judged windows alone, oldest first, each marked off where the band rejects it,
 *  with the title counting the judged, the off and the left-out windows. `now` is the answer's newest window end, so
 *  every line of the view reads one axis. The judged windows are dots too at eight or fewer. */
export function sparkOf(group: MeterGroup, now: number): { dots: boolean; points: SparkPoint[]; title: string } {
  const since = now - SPAN_MS;
  const span = SPARK.width - 2 * INSET_X;
  const points = group.rows
    .filter(judged)
    .map((r) => ({ at: Date.parse(r.windowEnd), r }))
    .sort((a, b) => a.at - b.at)
    .map(({ at, r }) => ({ off: ratioOff(r), x: INSET_X + Math.min(1, Math.max(0, (at - since) / SPAN_MS)) * span, y: sparkY(r.ratio) }));
  return { dots: points.length <= 8, points, title: `${group.judged} judged, ${group.off} off, ${group.leftOut} unjudged` };
}
