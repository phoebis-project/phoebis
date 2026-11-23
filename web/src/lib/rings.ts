import type { Account, SubscriptionCosts } from '@/api/types';
import { tierCapacity } from '@/lib/costs';
import { has } from '@/lib/counts';
import { fmtPct } from '@/lib/format';
import { isExpired, isQuotaListed, planLabel, pricedTier, readWindows, remainingOf, windowOf } from '@/lib/quota';
import { WEEK_SHORT, WEEK_TIER, type WeekPosition } from '@/lib/unit';

/** The window a Dashboard ring pools: the 7-day window, or the Fable week, which the decoder holds as `fableWindow`. */
export type RingWindow = '7d' | '7d_fable';

/** An account on a quota ring: its own remaining share of the window, 1 for an expired window, and its weight in weeks of the chosen week's tier. */
export interface RingRow { color?: never; id: string; label: string; remaining: number; weight: number }

/** An account the ring cannot weigh, named under it with the reason. */
export interface RingLeftOut { id: string; label: string; reason: string }

/**
 * `capacity` and `left` in weeks of the chosen week's tier: on the 7-day ring each account weighs its tier's 7-day
 * capacity over the chosen tier's, on the Fable ring its tier's 7-day capacity times the Fable week's capacity factor
 * over the chosen tier's Fable-week capacity. `rows` is empty when every account is left out, and `carried` says
 * whether a listed account carries the window, so an empty ring tells a fleet without the window from one whose tiers
 * are not sized.
 */
export interface RingFigures { capacity: number; carried: boolean; left: number; leftOut: RingLeftOut[]; rows: RingRow[]; week: WeekPosition; window: RingWindow }

const SPAN: Record<RingWindow, string> = { '7d': '7-day', '7d_fable': 'Fable week' };
const ABSENT: Record<RingWindow, string> = { '7d': 'no 7-day window', '7d_fable': 'no Fable week' };

/**
 * The pooled capacity of one weekly window across the routable subscription accounts, each weighed by its tier's
 * capacity over the chosen week's tier's from the subscription cost model, so a Pro account's empty week does not count
 * as much as an Ultra account's full one. An account whose poll lacks the window, or whose window holds no share, is
 * named with the reason rather than read as spent or as full.
 */
export function ringFigures(accounts: Account[], windowId: RingWindow, costs: SubscriptionCosts, week: WeekPosition, now: number): RingFigures {
  const span = SPAN[windowId];
  const chosen = WEEK_TIER[week];
  const chosenCapacity = tierCapacity(costs, chosen, 'seven_day');
  // The cost model sizes the Fable week as one factor of every tier's 7-day window, so the factor cancels in the ratio
  // and the Fable ring weighs by the 7-day capacities, where the model sizes the week at all.
  const sized = windowId === '7d' || has(costs.modelWindows, 'seven_day_fable');
  const rows: RingRow[] = [];
  const leftOut: RingLeftOut[] = [];
  let carried = false;
  for (const a of accounts) {
    if (!isQuotaListed(a) || !a.routingEnabled) continue;
    const w = windowOf(a, windowId);
    // Counted before the gate, since a free plan's window stands though the gate weighs it at nothing.
    if (w !== null) carried = true;
    // Read before the window test, since a usage-based seat reports no plan window and is named all the same.
    const priced = a.subscription === null ? null : pricedTier(a.subscription.plan, a.subscription.rateLimitTier, costs);
    if (priced !== null && priced.tier === null) {
      leftOut.push({ id: a.id, label: a.label, reason: priced.reason });
      continue;
    }
    if (w === null) {
      leftOut.push({ id: a.id, label: a.label, reason: readWindows(a).length === 0 ? 'not read yet' : ABSENT[windowId] });
      continue;
    }
    if (priced === null) {
      leftOut.push({ id: a.id, label: a.label, reason: 'no subscription profile' });
      continue;
    }
    const tier = priced.tier;
    const capacity = sized ? tierCapacity(costs, tier, 'seven_day') : null;
    if (capacity === null || chosenCapacity === null) {
      // The reason names the side of the ratio that has no capacity, the account's own tier first.
      leftOut.push({ id: a.id, label: a.label, reason: `${planLabel(capacity === null ? tier : chosen)} ${span} capacity not measured` });
      continue;
    }
    // Tested ahead of the passed reset, since a window with no share is one the router holds no reading of, whatever
    // its reset says.
    const share = remainingOf(w);
    if (share === null) {
      leftOut.push({ id: a.id, label: a.label, reason: 'share not reported' });
      continue;
    }
    // An expired window is full only where it held a share, so it keeps its weight and is not named.
    const remaining = isExpired(w, now) ? 1 : share;
    rows.push({ id: a.id, label: a.label, remaining, weight: capacity / chosenCapacity });
  }
  const capacity = rows.reduce((s, r) => s + r.weight, 0);
  const left = rows.reduce((s, r) => s + r.weight * r.remaining, 0);
  // The ring's slices and the list beside it read one order, the most of its own window left first, so the legend reads the ring.
  rows.sort((a, b) => b.remaining - a.remaining || a.label.localeCompare(b.label));
  return { capacity, carried, left, leftOut, rows, week, window: windowId };
}

/** The weeks a ring counts in, "Ultra weeks" or "Max Fable weeks", and none on the Fable ring under the Pro week, since
 *  Pro has no Fable week. */
const weeksOf = (figures: RingFigures): string | null => (figures.window === '7d_fable' ? (figures.week === 'pro' ? null : `${WEEK_SHORT[figures.week]} Fable weeks`) : `${WEEK_SHORT[figures.week]} weeks`);

/** The line under a ring: "0.39 of 1.30 Ultra weeks left", null on the Fable ring under the Pro week. */
export function ringLine(figures: RingFigures): string | null {
  const weeks = weeksOf(figures);
  return weeks === null ? null : `${fmtWindows(figures.left)} of ${fmtWindows(figures.capacity)} ${weeks} left`;
}

/** A row's hover: "primary: 0.12 of 1.30 Ultra weeks, 9% of the pooled capacity", the share alone where the ring
 *  counts no weeks. */
export function ringRowTitle(figures: RingFigures, row: RingRow): string {
  const share = row.weight * row.remaining;
  const pooled = `${fmtPct(share / figures.capacity)} of the pooled capacity`;
  const weeks = weeksOf(figures);
  return weeks === null ? `${row.label}: ${pooled}` : `${row.label}: ${fmtWindows(share)} of ${fmtWindows(figures.capacity)} ${weeks}, ${pooled}`;
}

/** A count of the chosen week's weeks: "0.39", "1.30". */
export const fmtWindows = (n: number) => n.toFixed(2);
