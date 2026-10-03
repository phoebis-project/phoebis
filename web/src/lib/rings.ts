import { planLabel } from '@/api/decode';
import type { Account, CostTable, WindowId } from '@/api/types';
import { tierFactors } from '@/lib/analytics';
import { isExpired, isSubscription, remainingOf, windowOf } from '@/lib/quota';

/** An account on a quota ring: its own remaining share of the window, 1 for an expired window, and its tier's capacity in Max 20x windows. */
export interface RingRow { color?: never; id: string; label: string; remaining: number; weight: number }

/** An account the ring cannot weigh, named under it with the reason. */
export interface RingLeftOut { id: string; label: string; reason: string }

/** `capacity` is the sum of the weights and `left` the sum of weight times remaining; `rows` is empty when every account is left out. */
export interface RingFigures { capacity: number; left: number; leftOut: RingLeftOut[]; rows: RingRow[] }

/**
 * The pooled capacity of one window across the routable subscription accounts, each weighed by its tier's factor
 * from the cost table, so a Pro account's empty window does not count as much as a Max 20x account's full one.
 */
export function ringFigures(accounts: Account[], windowId: WindowId, tiers: CostTable['tiers'], now: number): RingFigures {
  const rows: RingRow[] = [];
  const leftOut: RingLeftOut[] = [];
  for (const a of accounts) {
    if (!isSubscription(a) || !a.routingEnabled) continue;
    const w = windowOf(a, windowId);
    if (w === null) continue;
    if (a.subscription === null) {
      leftOut.push({ id: a.id, label: a.label, reason: 'no subscription profile' });
      continue;
    }
    const factors = tierFactors(tiers, a.subscription.rateLimitTier);
    if (factors === undefined) {
      leftOut.push({ id: a.id, label: a.label, reason: `tier ${a.subscription.rateLimitTier} is not in the cost table` });
      continue;
    }
    const weight = windowId === '5h' ? factors.fiveHour : factors.sevenDay;
    if (weight === null) {
      leftOut.push({ id: a.id, label: a.label, reason: `${planLabel(a.subscription.rateLimitTier)} ${windowId === '5h' ? '5-hour' : '7-day'} capacity not measured` });
      continue;
    }
    // An expired window with no reading since is a full allowance, so it keeps its weight and is not named.
    rows.push({ id: a.id, label: a.label, remaining: isExpired(w, now) ? 1 : remainingOf(w), weight });
  }
  const capacity = rows.reduce((s, r) => s + r.weight, 0);
  const left = rows.reduce((s, r) => s + r.weight * r.remaining, 0);
  return { capacity, left, leftOut, rows };
}

/** A count of Max 20x windows: "0.39", "1.30". */
export const fmtWindows = (n: number) => n.toFixed(2);
