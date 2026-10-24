import type { Account, SubscriptionCosts, WindowId } from '@/api/types';
import { anchorTier, tierCapacity, tierRow } from '@/lib/costs';
import { isExpired, isQuotaListed, planLabel, remainingOf, windowOf } from '@/lib/quota';

/** An account on a quota ring: its own remaining share of the window, 1 for an expired window, and its tier's capacity in windows of the anchor tier. */
export interface RingRow { color?: never; id: string; label: string; remaining: number; weight: number }

/** An account the ring cannot weigh, named under it with the reason. */
export interface RingLeftOut { id: string; label: string; reason: string }

/**
 * `capacity` is the sum of the weights and `left` the sum of weight times remaining, both in windows of the tier
 * `anchor` labels, the one the cost model counts its units in; `rows` is empty when every account is left out.
 */
export interface RingFigures { anchor: string; capacity: number; left: number; leftOut: RingLeftOut[]; rows: RingRow[] }

/**
 * The pooled capacity of one window across the routable subscription accounts, each weighed by its tier's capacity on
 * the window over the anchor tier's from the subscription cost model, so a Pro account's empty window does not count
 * as much as a Max 20x account's full one.
 */
export function ringFigures(accounts: Account[], windowId: WindowId, costs: SubscriptionCosts, now: number): RingFigures {
  const window = windowId === '5h' ? 'five_hour' : 'seven_day';
  const span = windowId === '5h' ? '5-hour' : '7-day';
  const anchor = anchorTier(costs);
  const anchorCapacity = tierCapacity(costs, anchor, window);
  const rows: RingRow[] = [];
  const leftOut: RingLeftOut[] = [];
  for (const a of accounts) {
    if (!isQuotaListed(a) || !a.routingEnabled) continue;
    // A usage-based seat has no plan window to weigh and the cost model sizes no tier for it, as for an API key.
    if (a.usageBased) continue;
    const w = windowOf(a, windowId);
    if (w === null) continue;
    if (a.subscription === null) {
      leftOut.push({ id: a.id, label: a.label, reason: 'no subscription profile' });
      continue;
    }
    const tier = a.subscription.rateLimitTier;
    if (tierRow(costs, tier) === undefined) {
      leftOut.push({ id: a.id, label: a.label, reason: `tier ${tier} is not in the cost model` });
      continue;
    }
    const capacity = tierCapacity(costs, tier, window);
    if (capacity === null || anchorCapacity === null) {
      // The reason names the side of the ratio that has no capacity, the account's own tier first.
      leftOut.push({ id: a.id, label: a.label, reason: `${planLabel(capacity === null ? tier : anchor)} ${span} capacity not measured` });
      continue;
    }
    // An expired window with no reading since is a full allowance, so it keeps its weight and is not named.
    rows.push({ id: a.id, label: a.label, remaining: isExpired(w, now) ? 1 : remainingOf(w), weight: capacity / anchorCapacity });
  }
  const capacity = rows.reduce((s, r) => s + r.weight, 0);
  const left = rows.reduce((s, r) => s + r.weight * r.remaining, 0);
  // The ring's slices and the list beside it read one order, the most of its own window left first, so the legend reads the ring.
  rows.sort((a, b) => b.remaining - a.remaining || a.label.localeCompare(b.label));
  return { anchor: planLabel(anchor), capacity, left, leftOut, rows };
}

/** A count of the anchor tier's windows: "0.39", "1.30". */
export const fmtWindows = (n: number) => n.toFixed(2);
