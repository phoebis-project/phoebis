// The window test that the routing log's records and the pools route's members share. It stands below both modules,
// since the routing log imports the pools.
import type { Pool } from '@/api/types';
import { HOUR } from './time';

/**
 * A fresh session's burn per hour: the pool's warm pins' median as the story's hour reads it. The drawn day's
 * decisions read no burn at all, so they book the rewrite alone.
 */
export const PRIOR_BURN: Record<string, number> = { five_hour: 0.01, seven_day: 0.0012, seven_day_fable: 0.0024 };

/** The pool setting that holds a window's own ceiling, by the window's wire key. */
export const ceilingSettingOf = (window: string): 'five_hour_ceiling' | 'weekly_ceiling' => (window === 'five_hour' ? 'five_hour_ceiling' : 'weekly_ceiling');

const CEILING_FIELD = { five_hour_ceiling: 'fiveHourCeiling', weekly_ceiling: 'weeklyCeiling' } as const;

/** A window's own ceiling on a pool by its wire key. */
export const ceilingOn = (pool: Pick<Pool, 'fiveHourCeiling' | 'weeklyCeiling'>, window: string): number => pool[CEILING_FIELD[ceilingSettingOf(window)]];

/** A bounded runway on a window, and whether it ends short of the window's horizon. */
export interface Runway {
  /** Whether the runway ends past its window's reset, which takes the line away first. False with no reset. */
  pastReset: boolean;
  runway: number;
  short: boolean;
  window: string | null;
}

/** One plan window's reading for a request with `rewrite` and `burn` on it: whether the rewrite lands under the
 *  window's ceiling, the runway to the ceiling, the runway to 1.0, and whether the window holds no allowance. */
interface WindowFit {
  lands: boolean;
  runwayMs: number | null;
  spent: boolean;
  wallRunwayMs: number | null;
}

/**
 * Each runway is the time until the prediction with the rewrite, rising at `burn`, reaches its line: zero where it
 * stands there already and unbounded with no burn, as the router's `time_to` reads it.
 */
export function windowFitOf(reading: { reserved: number | null; utilization: number }, ceiling: number, rewrite: number, burn: number): WindowFit {
  const predicted = reading.utilization + (reading.reserved ?? 0);
  const runwayTo = (line: number) => (predicted + rewrite >= line ? 0 : burn <= 0 ? null : Math.round(((line - predicted - rewrite) / burn) * HOUR));
  return { lands: predicted + rewrite < ceiling, runwayMs: runwayTo(ceiling), spent: reading.utilization >= 1, wallRunwayMs: runwayTo(1) };
}

/**
 * A window's two runways off its fit read at `at`, to the ceiling and to 1.0, each short where it ends before the
 * window's `horizon`, past its reset where it ends after the window's `reset`, as the router's `passes_reset` reads it,
 * and none where it is unbounded.
 */
export function runwaysOf(read: WindowFit, window: string, horizon: string, reset: string | null, at: number): { ceiling: Runway | null; wall: Runway | null } {
  const span = Date.parse(horizon) - at;
  const resetSpan = reset === null ? null : Date.parse(reset) - at;
  const of = (ms: number | null): Runway | null => (ms === null ? null : { pastReset: resetSpan !== null && ms > resetSpan, runway: ms, short: ms < span, window });
  return { ceiling: of(read.runwayMs), wall: of(read.wallRunwayMs) };
}

/**
 * The binding window among `runways`, as the router's `binding_of` picks it for the rank and the displayed pair alike:
 * the shortest runway of those short of their horizon, else the shortest of all.
 */
export function bindingOf(runways: Runway[]): Runway | undefined {
  const shortest = (list: Runway[]) => [...list].sort((a, b) => a.runway - b.runway)[0];
  return shortest(runways.filter((b) => b.short)) ?? shortest(runways);
}

/** The pair a row states among `runways`: `bindingOf` over those that do not pass their window's reset, and none
 *  where every one of them does. */
export function shownOf(runways: Runway[]): Runway | undefined {
  return bindingOf(runways.filter((r) => !r.pastReset));
}
