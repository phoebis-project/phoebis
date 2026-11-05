import type { Account } from '@/api/types';
import { endOf } from '@/lib/quota';
import { HOUR } from '@/lib/time';

/** The earliest of the instants given, null where none is. */
export const earliest = (...at: (number | null)[]): number | null => at.reduce<number | null>((min, x) => (x === null || (min !== null && min <= x) ? min : x), null);

/**
 * The hours the router's Pace score spreads a window's share left over: to the deadline, the earlier of the reset and
 * the account's end, at least one, and a week where neither is read.
 */
export const paceHours = (deadline: number | null, at: number): number => (deadline === null ? 168 : Math.max(1, (deadline - at) / HOUR));

/** The Pace deadline: the earlier of the window's reset and the end the router's pace reads (`endOf`). */
export const paceDeadline = (account: Pick<Account, 'canceled' | 'subscription'>, reset: number | null, at: number): number | null => earliest(reset, endOf(account, at));

/** The share left per hour to the deadline, the ratio the router's Pace strategy scores a window by. */
export const leftPerHour = (utilization: number, deadline: number | null, at: number): number => Math.max(0, 1 - utilization) / paceHours(deadline, at);

/**
 * The router's lead over pace, as the account answer carries it, computed by the mock in the router's place: the
 * window's utilization less the share of it elapsed from its start to the earlier of its reset and the account's end
 * (`endOf`), clamped to 0 to 1, and the utilization alone without a reset. The production bundle reads the field.
 */
export function leadOf(window: { resetsAt: string | null; utilization: number }, lengthMs: number, now: number, end: number | null): number {
  if (window.resetsAt === null) return window.utilization;
  const reset = Date.parse(window.resetsAt);
  const start = reset - lengthMs;
  const span = Math.min(reset, end ?? reset) - start;
  // Past its reset a window reads fully elapsed, where the router reads its own use since: an accepted approximation.
  const elapsed = span <= 0 ? 1 : (now - start) / span;
  return window.utilization - Math.min(1, Math.max(0, elapsed));
}
