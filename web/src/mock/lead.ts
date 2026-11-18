import type { Account } from '@/api/types';
import { modelWeeks, pricedWindow } from '@/lib/costs';
import { endOf, isPaidSource } from '@/lib/quota';
import { HOUR } from '@/lib/time';
import { round } from './round';
import { TABLES } from './samples';

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

/** A window's share used and its reset as a Pace read takes them, off a record's reading or an account's window. */
export interface PaceReading {
  reset: string | null;
  utilization: number;
}

/**
 * What a Pace read takes beside the account, the instant and the model: each window's reading and the burn per hour the
 * account's sessions spend on it, both by the window's wire key with `worst_bucket` for an API key, and whether the
 * member stands in the pick's first tier, the one set the router holds a Fable reservation on.
 */
export interface PaceReads {
  burns: Record<string, number>;
  readings: Record<string, PaceReading | undefined>;
  reserve: boolean;
}

/**
 * A member's Pace figures for a request of `model` at `at`, as the router's `pace_of` works them. A model with a week of
 * its own reads that week and any other the 7-day window, one the account has not read counting as untouched. A paid
 * source scores by its remaining allowance with no hours, no window and no burn: an API key its worst live bucket, and
 * a usage-based seat, which reads no plan window, the whole of it. A null model is the pools route's ask, which reads
 * as a request of a model without a week of its own.
 */
export function paceOf(
  account: Account,
  at: number,
  model: string | null,
  reads: PaceReads,
): { burn: number; deadline: number | null; end: number | null; fableReservation: number | null; hours: number | null; remaining: number; reset: number | null; score: number; window: string | null } {
  const window = (model === null ? undefined : modelWeeks(TABLES, model)[0]) ?? 'seven_day';
  const paid = isPaidSource(account);
  const reading = account.kind === 'api_key' ? reads.readings.worst_bucket : paid ? undefined : reads.readings[window];
  const remaining = reading === undefined ? 1 : Math.max(0, 1 - reading.utilization);
  if (paid) return { burn: 0, deadline: null, end: null, fableReservation: null, hours: null, remaining, reset: null, score: remaining, window: null };
  const reset = reading?.reset ? Date.parse(reading.reset) : null;
  const deadline = paceDeadline(account, reset, at);
  const hours = paceHours(deadline, at);
  const whole = reads.burns[window] ?? 0;
  // The cost model's divisor of the Fable week, which turns a share of that week into one of the 7-day window.
  const factor = pricedWindow(TABLES, 'seven_day_fable')?.divisor ?? null;
  // The mock keeps no record of Fable use per hour, so the account's Fable week burn stands both for its strongest Fable
  // hour and for its pins' current Fable burn, the two rates the router takes the larger of.
  const fableBurn = reads.burns.seven_day_fable ?? 0;
  const fable = reads.readings.seven_day_fable;
  const fableReservation = !reads.reserve || window !== 'seven_day' || fable === undefined || factor === null ? null : round(factor * Math.min(Math.max(0, 1 - fable.utilization), fableBurn * hours), 5);
  // Beside a reservation the score takes off only the burn of the pins whose model draws no Fable week. The mock keeps
  // one 7-day burn per account, so the Fable pins' part of it is their Fable week burn in the 7-day window's share.
  const burn = fableReservation === null ? whole : round(Math.max(0, whole - factor! * fableBurn), 5);
  return { burn, deadline, end: endOf(account, at), fableReservation, hours, remaining, reset, score: round((remaining - (fableReservation ?? 0)) / hours - burn, 5), window };
}

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
