import type { Account, MemberEligibility, Pool, PoolPin, RateLimitWindow } from '@/api/types';
import { groupInto } from './counts';
import { byteOrder, fmtDuration, instant, noun, percentText, plural } from './format';
import { openMark, pinTtlMs } from './keepalive';
import { endOf, largestShare, minuteSpan, remainingOf, windowOf } from './quota';
import { reservationRoundsRun, speedWord, windingDown } from './selection';
import { compareFigures } from './sort';
import { HOUR } from './time';
import { WINDOW_SHORT, windowKeyId } from './windows';

export type SheddingPreset = 'balanced' | 'conservative' | 'custom' | 'fast';

/**
 * The store's default is `balanced`. A preset sets the cap, the average and the dwell, and never the load-shedding
 * switch, so picking one leaves the round on or off as it was.
 */
export const SHEDDING_PRESETS: Record<Exclude<SheddingPreset, 'custom'>, { burnAverageSeconds: number; shedCap: number; shedDwellSeconds: number }> = {
  balanced: { burnAverageSeconds: 1_800, shedCap: 0.5, shedDwellSeconds: 900 },
  conservative: { burnAverageSeconds: 3_600, shedCap: 0.3, shedDwellSeconds: 1_800 },
  fast: { burnAverageSeconds: 900, shedCap: 0.5, shedDwellSeconds: 600 },
};

export type FailoverPreset = 'balanced' | 'conservative' | 'custom' | 'fast';

/** balanced 16 and 60 s (the default), fast 8 and 20 s, conservative 16 and 120 s. */
export const FAILOVER_PRESETS: Record<Exclude<FailoverPreset, 'custom'>, { inFlightCap: number; retryInPlaceSeconds: number }> = {
  balanced: { inFlightCap: 16, retryInPlaceSeconds: 60 },
  conservative: { inFlightCap: 16, retryInPlaceSeconds: 120 },
  fast: { inFlightCap: 8, retryInPlaceSeconds: 20 },
};

// A share the store kept as a REAL can read back a rounding off the literal it was saved from.
const near = (a: number, b: number) => Math.abs(a - b) < 1e-9;

/** The first preset of the table whose values `same` accepts, else custom. */
const presetOf = <K extends string, V>(presets: Record<K, V>, same: (v: V) => boolean): K | 'custom' => (Object.keys(presets) as K[]).find((name) => same(presets[name])) ?? 'custom';

export function sheddingPresetOf(p: Pick<Pool, 'burnAverageSeconds' | 'shedCap' | 'shedDwellSeconds'>): SheddingPreset {
  return presetOf(SHEDDING_PRESETS, (v) => v.burnAverageSeconds === p.burnAverageSeconds && near(v.shedCap, p.shedCap) && v.shedDwellSeconds === p.shedDwellSeconds);
}

/** Both values must match a preset. Balanced and conservative differ in the retry-in-place bound alone. */
export function failoverPresetOf(p: Pick<Pool, 'inFlightCap' | 'retryInPlaceSeconds'>): FailoverPreset {
  return presetOf(FAILOVER_PRESETS, (v) => v.inFlightCap === p.inFlightCap && v.retryInPlaceSeconds === p.retryInPlaceSeconds);
}

/** The share of a round's burn sum that its marks leave on the account's unmarked priced pins, as the router's. */
export const SHED_MARK_SHARE = 0.1;

/** The burn averages after which a decayed burn reads zero, as the router's. */
const BURN_DECAY_CUTOFF_AVERAGES = 30;

/**
 * The words a round that marks nothing answers, in the router's order: a walked pin's `no_runway` and `sliver`, a
 * left-out pin's `off_window`, `fable_model`, `ended`, `account_bound` and `no_burn`, then a walked pin's `unpriced`,
 * `last_pin` and `standing`.
 */
export const SHED_CAUSES = ['no_runway', 'sliver', 'off_window', 'fable_model', 'ended', 'account_bound', 'no_burn', 'unpriced', 'last_pin', 'standing'] as const;
type ShedCause = (typeof SHED_CAUSES)[number];
const firstCause = (read: readonly (ShedCause | null)[]): ShedCause | null => SHED_CAUSES.find((c) => read.includes(c)) ?? null;

/**
 * The round's marking rule, the mirror of the router's walk: pins in the round's order with their burns on the round's
 * window, walked until the removed burn reaches `target`, none for a zero target. Each position reads the router's
 * tests in its order. A pin under a standing mark is a move pending, so its burn counts as removed and it reads
 * `standing`. Past a met target, and at a pin flagged `pastCold`, a warm pin under `never` behind the cold ones, the
 * walk reads nothing. An unpriced pin reads `unpriced` and is never marked. At a priced pin the pick reads each pin
 * from the position on that covers the rest: the guard first, so a pin the guard keeps reads `last_pin` and never
 * `no_runway`, then its runway, so a pin with none reads `no_runway`, and the covering pin with runway whose move
 * costs the least is marked, the first in the order among equals, and the walk stops. With no pick the pin at the
 * position reads `sliver` under `SHED_MARK_SHARE` of the rest, `last_pin` where the guard refuses it, `no_runway` with
 * no runway, and is marked otherwise.
 *
 * The guard refuses a mark that would leave the walk's other priced pins, neither marked nor under a standing mark,
 * plus `stays.burn`, what stays on the account outside the walk, under `SHED_MARK_SHARE` of the walk's priced burn sum.
 * A burn with no price is unknown and not none, so an unpriced pin of the walk that stands unmarked and under no mark,
 * and `stays.unpriced`, turn the guard off. A walk that marks nothing answers the first word of `SHED_CAUSES` that one
 * of its pins read, and null where none read one. It answers null where a mark lands and where the target reads zero
 * or less.
 */
function shedMarks(
  ordered: { burn: number | null; cost: number; id: string; pastCold: boolean; runway: boolean; standing: boolean }[],
  target: number,
  stays: { burn: number; unpriced: boolean },
): { marks: string[]; quiet: ShedCause | null } {
  type Pin = (typeof ordered)[number];
  const marks: string[] = [];
  if (Number.isNaN(target) || target <= 0) return { marks, quiet: null };
  const sum = ordered.reduce((n, pin) => n + (pin.burn ?? 0), 0);
  const unpricedStays = stays.unpriced || ordered.some((pin) => pin.burn === null && !pin.standing);
  // A standing mark is a move pending, so its burn counts as removed before the walk marks anything else.
  let removed = ordered.reduce((n, pin) => n + (pin.standing ? (pin.burn ?? 0) : 0), 0);
  // The guard sums the pins that stay, as the router does, since the round's sum less what leaves can round under a
  // floor those pins reach exactly.
  const refused = (candidate: Pin) => !unpricedStays && ordered.filter((q) => q !== candidate && !q.standing && !marks.includes(q.id)).reduce((n, q) => n + (q.burn ?? 0), 0) + stays.burn < SHED_MARK_SHARE * sum;
  // The walk asks the guard about a pin again at each later position, and a mark is the one thing that changes its
  // answer, so each answer stands until the next mark.
  const refusedMemo = new Map<Pin, boolean>();
  const refusedOf = (candidate: Pin): boolean => {
    const known = refusedMemo.get(candidate);
    if (known !== undefined) return known;
    const answer = refused(candidate);
    refusedMemo.set(candidate, answer);
    return answer;
  };
  // By position, as the router keeps them: a later position's pick reads a covering pin again.
  const words: (ShedCause | null)[] = ordered.map(() => null);
  let met = false;
  for (const [index, pin] of ordered.entries()) {
    if (pin.standing) {
      words[index] = 'standing';
      continue;
    }
    // An indivisible pin may carry the removed burn past the target, and the round stops there.
    if (met || removed >= target || pin.pastCold) continue;
    // An unpriced burn moves the removed sum nowhere, so marking the pin would shed a session on a guess.
    if (pin.burn === null) {
      words[index] = 'unpriced';
      continue;
    }
    const rest = target - removed;
    // A pin is indivisible, so once one from here on covers the rest, the cheapest such move meets the target.
    let pick: { burn: number; cost: number; id: string } | null = null;
    for (const [k, q] of ordered.entries()) {
      if (k < index || q.burn === null || q.burn < rest || q.standing || q.pastCold) continue;
      words[k] = refusedOf(q) ? 'last_pin' : q.runway ? null : 'no_runway';
      if (words[k] === null && (pick === null || q.cost < pick.cost)) pick = { burn: q.burn, cost: q.cost, id: q.id };
    }
    if (pick !== null) {
      marks.push(pick.id);
      refusedMemo.clear();
      removed += pick.burn;
      met = true;
    } else if (pin.burn < SHED_MARK_SHARE * rest) {
      // A pin under the floor relieves too little of the rest to be worth its move, so it stays.
      words[index] = 'sliver';
    } else if (pin.burn < rest) {
      // The guard refuses this pin alone: a later pin's smaller mark may still leave a tenth on the account.
      if (refusedOf(pin)) words[index] = 'last_pin';
      else if (!pin.runway) words[index] = 'no_runway';
      else {
        marks.push(pin.id);
        refusedMemo.clear();
        removed += pin.burn;
      }
    }
  }
  return { marks, quiet: marks.length > 0 ? null : firstCause(words) };
}

/**
 * A pin's served burn decayed on from `burnAt`, the instant the route decayed it to, to `now`, as the router's average
 * decays between folds, and 0 once `burnAt` lies more than `BURN_DECAY_CUTOFF_AVERAGES` averages before `now`, the
 * router's cutoff. The route already serves 0 past the cutoff at its read, so this holds the router's rule over the
 * span from the read to `now`, and over an older instant a reader hands it. A burn with no instant reads as served.
 * Without the fold's instant, which the route does not serve, its 0 can come later than the router's by the fold's age
 * at the read.
 */
function burnDecayed(burn: number, burnAt: string | null, now: number, windowMs: number): number {
  if (burnAt === null) return burn;
  const span = now - Date.parse(burnAt);
  if (span > BURN_DECAY_CUTOFF_AVERAGES * Math.max(windowMs, 1)) return 0;
  // An instant after `now` (another clock) decays nothing rather than growing the burn.
  return burn * Math.exp(-Math.max(span, 0) / Math.max(windowMs, 1));
}

/**
 * The 5-hour window's start: the reset less five hours while the reset is ahead, the reset itself once it has passed
 * with no new one, null with no reset.
 */
const windowStart = (w: RateLimitWindow, now: number): number | null => {
  if (w.resetsAt === null) return null;
  const reset = Date.parse(w.resetsAt);
  return reset > now ? reset - 5 * HOUR : reset;
};

/** A plan window the trigger reads, in the order the router's round walks them: the shared windows, then the week. */
export type ShedWindow = '5h' | '7d' | '7d_fable';
const TRIGGER_ORDER: ShedWindow[] = ['5h', '7d', '7d_fable'];
const SHARED_WINDOWS: ShedWindow[] = TRIGGER_ORDER.slice(0, 2);
/** A session's `burnWindow` under any of its spellings as the window the trigger reads, an unknown key as none. */
const burnWindowOf = (key: string | null): ShedWindow | null => {
  const id = windowKeyId(key ?? '');
  return TRIGGER_ORDER.find((w) => w === id) ?? null;
};

/**
 * The router's `MODEL_WINDOWS`: a model whose id names the Fable family draws the 7-day Fable window beside the two
 * shared windows.
 */
const drawsFable = (model: string | null) => (model ?? '').toLowerCase().includes('fable');
const windowsOf = (model: string | null): ShedWindow[] => (drawsFable(model) ? TRIGGER_ORDER : SHARED_WINDOWS);

/** A window's ceiling by the router's arm: the 5-hour ceiling for the 5-hour window, the weekly one for the 7-day
 *  window and each model week. */
const ceilingOf = (pool: Pick<Pool, 'fiveHourCeiling' | 'weeklyCeiling'>, id: string): number => (id === '5h' ? pool.fiveHourCeiling : pool.weeklyCeiling);
/** The line the round tests a projection against: the wall where the window's reset or the account's end sets the
 *  horizon, the window's own ceiling at the five-hour bound. */
const lineOf = (pool: Pick<Pool, 'fiveHourCeiling' | 'weeklyCeiling'>, id: string, resetAt: number | null, endAt: number | null, horizon: number, now: number): number => {
  const atReset = resetAt !== null && resetAt > now && (id === '5h' || resetAt <= now + 5 * HOUR);
  return atReset || (endAt !== null && endAt <= horizon) ? 1 : ceilingOf(pool, id);
};

/**
 * Whether an API-key account has room for a moved session: its worst live bucket under 1.0, since a bucket refills
 * within its minute. Its live buckets are its `rpm:`, `itpm:` and `otpm:` ones whose reset is ahead, or which carry
 * none and were read within the minute, as the router's `worst_bucket` reads them. A bucket with no share is one the
 * router holds no reading of, so it takes no part.
 */
const bucketRoom = (a: Account, now: number) =>
  (largestShare(a.windows.filter((w) => /^(rpm|itpm|otpm):/.test(w.id) && (w.resetsAt === null ? now - Date.parse(w.observedAt) < 60_000 : Date.parse(w.resetsAt) > now))) ?? 0) < 1;

/** A member whose projection crosses the round's line, on the first window in the trigger's order that does. */
export interface ShedHot {
  /** The burn a round removes to land the projection on its line at the horizon, in the window's share an hour. */
  excess: number;
  horizon: number;
  id: string;
  label: string;
  projection: number;
  window: ShedWindow;
}

/** A member whose 5-hour window is under the round's age hold: the window's age and the hold, both in milliseconds. */
export interface ShedHeld { ageMs: number; holdMs: number; id: string; label: string }

export interface ShedPreview {
  /**
   * With no member over its line, the window with the least room to its own ceiling, the first in the walk's order
   * among equals. Null with none to read. A 5-hour window under the hold is out of the reading, so the nearest is among
   * the windows a round would read.
   */
  closest: { horizon: number; label: string; projection: number; window: ShedWindow } | null;
  /** The excess the marks take off, in share an hour, each member's in the share of the window that fired it. */
  covered: number;
  /**
   * The members whose 5-hour window projects over the round's line under the hold, read on their next window, in the
   * members' order.
   */
  held: ShedHeld[];
  /** The members over the round's line, in the order the Members group lists them. */
  hot: ShedHot[];
  /** The pool's switch is off: no round runs, whatever the projection reads. */
  off: boolean;
  /**
   * Why the rounds the members' windows fire mark nothing, read as one round: the first word of `SHED_CAUSES` that a
   * walked pin of a round read. With none, `warm` where a walked pin burns and each walked pin that burns is warm
   * under `never`. Then the first word, in the same order, that a left-out pin carries on a round that walked no pin,
   * a pin on the fast lane naming none. Then `unpriced`, the router's own fallback. Null where the switch is off, where
   * no member is over its line and where a round marks a session.
   */
  quiet: ShedCause | 'warm' | null;
  /**
   * The Fable reservation's part, with the option on, under Pace and with load shedding on: in the members' order, the
   * members whose reservation round runs (`labels`) with what those rounds would mark, and the members under their
   * reservation that hold no pin a round could move (`idle`), which run no round.
   */
  reservation: { idle: string[]; labels: string[]; sessions: number; warm: number };
  sessions: number;
  warm: number;
}

/** A pool pin as a round walks it, its own mark standing where the mark names the pin's account. */
type RoundPin = PoolPin & { standing: boolean };
/** A window as the trigger tests it: its projection against the line at the horizon, and the burn a round scales by. */
type WindowRead = { burn: number | null; horizon: number; id: ShedWindow; line: number; projection: number };
/** The words the router's predicates leave a pin out of a round under, in the order it tests them. */
type LeftOut = 'account_bound' | 'ended' | 'fable_model' | 'fast_lane' | 'off_window';
/**
 * The word the router's predicates leave `p` out of a round on `fired` under, the first that holds, null for a pin the
 * round walks: a pin on the fast lane, a pin whose model does not draw the fired window, a Fable pin of a reservation
 * round, an ended pin and a pin that holds state on its account.
 */
const leftOutAs = (p: Pick<PoolPin, 'accountBound' | 'ended' | 'fast' | 'model'>, fired: { id: ShedWindow; reservation: boolean }): LeftOut | null => {
  if (p.fast) return 'fast_lane';
  if (!windowsOf(p.model).includes(fired.id)) return 'off_window';
  if (fired.reservation && drawsFable(p.model)) return 'fable_model';
  if (p.ended) return 'ended';
  return p.accountBound ? 'account_bound' : null;
};

/**
 * The shedding round's dry run, replayed from the data the routes serve. Each member's served projection is read at its
 * served horizon on the windows its pins draw, in the trigger's order, and the first window over the round's line fires
 * its round: the wall, 1, where the window's served reset ahead of `now` or the account's end set the horizon, else the
 * window's own ceiling, 5-hour or weekly. On a pool with the slow-down on, the 5-hour window reads the router's stretch
 * test in its place: the `stretchProjection` of the account's slow-down entry against the 5-hour ceiling, at the
 * window's served horizon less the entry's threshold, and no 5-hour window for an account with no entry or no figure,
 * or where that instant lies at or before `now`, as the router fires nothing there. The excess runs from the line to
 * the horizon. A 5-hour window younger than twice the pool's burn average fires under neither test, and the member
 * reads its next window, as the router holds the round. With no member over its line, the closest window is the one
 * with the least room to its own ceiling.
 *
 * The round walks the pins the pool's pins route answers on the member, a pin the Sessions list hides among them, since
 * the router's round walks each pin under the pool's keys. A pin stands under a mark where its own mark names its
 * account, and it is warm before its first cold instant at `now`, by its served `warm` where it carries none. The
 * router's predicates leave a pin out by `leftOutAs`, and a priced pin that burns nothing on the window leaves under
 * `no_burn`, while a fast pin's model week still counts among the windows the trigger reads. The round orders the rest
 * by their served burn, decayed from the instant it was served to `now`: cold pins first, those whose move drops
 * nothing by burn and then those that drop thinking by burn over the weighted drop, then warm ones by burn over the
 * move's size, all by burn over the size under `always`, and walks them by `shedMarks` toward the excess under
 * `shedCap`. A move costs the pin's served move cost where the pin is warm or the mode reads `always`, and a cold pin
 * the share of the thinking its move drops. A member without `eligible` counts as eligible, as a member of a pool not
 * saved yet does.
 *
 * With the pool's Fable reservation option on, under Pace and with load shedding on, a member no window fired on runs a
 * round of its own where its 7-day room, 1 less its served 7-day utilization floored at 0, stands under its
 * `fableReservation`. A member whose pins that have not ended are each left out by the predicates, or that holds none,
 * runs no round and stands in `idle`, as the router cancels such a round. The round walks the member's pins on the
 * 7-day window toward `shedCap` of their burn with no excess to bound it, its marks counted apart from the rounds the
 * windows fire, and the Fable pins it leaves out whose agent has not ended stay on the account for its guard, at their
 * burns on the 7-day window. A null reservation, a missing 7-day window and one with no projection fire nothing. The
 * router waits before each round, and the preview reads no wait.
 *
 * The preview parts from the router's record in four places. A round whose walked pins that burn are each warm under
 * `never` reads `warm`, where the record reads `unpriced`. A target of zero or less reads no word, where the record
 * reads `standing` for a pin under a standing mark. A slowed member's target and marks read the served burn, where the
 * router divides each pin's burn by the speed its demand was read at. A pin whose move cost reads null prices at 0 in
 * the pick.
 */
export function shedPreview(
  pool: Pick<Pool, 'burnAverageSeconds' | 'fableReservationShedding' | 'fiveHourCeiling' | 'loadShedding' | 'shedCap' | 'slowDown' | 'strategy' | 'warmSessions' | 'weeklyCeiling' | 'windDownMinutes'> & {
    members: { accountId: string; eligible?: MemberEligibility; fableReservation: number | null; priority: number }[];
  },
  accounts: Account[],
  pins: PoolPin[],
  now: number,
): ShedPreview {
  const byId = new Map(accounts.map((a) => [a.id, a]));
  const listed = pool.members
    .flatMap((m) => {
      const account = byId.get(m.accountId);
      return account === undefined ? [] : [{ account, eligible: (m.eligible ?? 'eligible') === 'eligible', priority: m.priority, reservation: m.fableReservation }];
    })
    .sort((a, b) => a.priority - b.priority || a.account.label.localeCompare(b.account.label));
  // The preview's reading of the gate: a sibling has runway for a pin while its served projection sits under the
  // window's own ceiling on every window the pin's model draws. The router also adds the pin's rewrite and burn and
  // counts the round's planned reservations, which the served figures cannot replay, so this is an approximation of its
  // read. A sibling whose end falls inside the pool's wind-down is no destination whatever it reads, an end exactly the
  // wind-down away included. A sibling whose windows hold the 7-day window and no 5-hour one, as an approved Ultra
  // account's set does, is read on the 7-day window alone, since the router's fit books the windows of the
  // destination's set and nothing on a window that set lacks. A reservation round moves no pin onto a sibling whose own
  // 7-day room reads under its `fableReservation`, as the router's leaves such a sibling out, and a sibling whose 7-day
  // window serves no share, or that serves no such window, reads a room of 1, as the router reads a window it holds no
  // reading of.
  const shortOfReservation = (account: Account, reservation: number | null) => {
    const week = windowOf(account, '7d');
    return reservation !== null && (week === null ? 1 : (remainingOf(week) ?? 1)) < reservation;
  };
  const hasRunway = (source: string, model: string | null, reservationRound: boolean) =>
    listed.some(
      ({ account, eligible, reservation }) =>
        account.id !== source &&
        eligible &&
        !(reservationRound && shortOfReservation(account, reservation)) &&
        !windingDown(pool, endOf(account, now), now) &&
        (account.kind === 'api_key'
          ? bucketRoom(account, now)
          : windowsOf(model)
              .filter((id) => id !== '5h' || windowOf(account, '5h') !== null || windowOf(account, '7d') === null)
              .every((id) => {
                const projection = windowOf(account, id)?.projection ?? null;
                return projection !== null && projection < ceilingOf(pool, id);
              })),
    );
  // `hasRunway` reads the model only through whether it draws the Fable week, so a member's pins share at most two
  // answers in each kind of round.
  const runwayMemo = new Map<string, boolean>();
  const runwayOf = (source: string, model: string | null, reservationRound: boolean): boolean => {
    const key = `${source}/${drawsFable(model)}/${reservationRound}`;
    const known = runwayMemo.get(key);
    if (known !== undefined) return known;
    const runway = hasRunway(source, model, reservationRound);
    runwayMemo.set(key, runway);
    return runway;
  };
  const windowMs = pool.burnAverageSeconds * 1_000;
  const hot: ShedHot[] = [];
  const held: ShedHeld[] = [];
  let closest: ShedPreview['closest'] = null;
  let covered = 0;
  let marked = 0;
  let warmMarked = 0;
  // The rounds the windows fire, read as one: the word each round's walk answered, the walked pins that burn and those
  // of them warm under `never`, and the words of the pins left out of a round that walked none.
  const walkedWords: (ShedCause | null)[] = [];
  const leftOutWords: ShedCause[] = [];
  let burning = 0;
  let burningWarm = 0;
  // Only Pace holds a reservation back, and the option adds a test to the round the switch runs.
  const reserving = reservationRoundsRun(pool);
  const reserved: ShedPreview['reservation'] = { idle: [], labels: [], sessions: 0, warm: 0 };
  const byAccount = groupInto(
    pins.map((p): RoundPin => ({ ...p, standing: openMark(p) !== null, warm: p.warmUntil === null ? p.warm : pinTtlMs(p, now) !== null })),
    (p) => p.accountId,
  );
  /**
   * One round's walk on `account` over its pins on the window `fired` names, toward the target `targetOf` reads off the
   * walked burn. The served burn is a pin's share of the window its `burnWindow` names: the 5-hour window, or the 7-day
   * one on an account without it. A round on another window reads each pin's burn scaled by the account's burn on the
   * fired window over its burn on the pin's own, the preview's approximation of the router's pricing of each pin on the
   * fired window, and a pin served in dollars has no share to scale, so it reads unpriced.
   */
  const walkOn = (account: Account, onAccount: RoundPin[], fired: { burn: number | null; id: ShedWindow; reservation: boolean }, targetOf: (total: number) => number) => {
    const burnOf = (p: RoundPin) => {
      const window = burnWindowOf(p.burnWindow);
      const own = window === null ? null : (windowOf(account, window)?.burn ?? null);
      const scale = fired.id === window ? 1 : own !== null && own > 0 && fired.burn !== null ? fired.burn / own : null;
      const served = p.burn === null || p.burnUnit !== 'window_share' ? null : burnDecayed(p.burn, p.burnAt, now, windowMs);
      return { burn: served === null || scale === null ? null : served * scale, served };
    };
    const left: (LeftOut | 'no_burn')[] = [];
    // The Fable pins a reservation round leaves out stay on the account, a pin under a standing mark among them, and
    // an ended agent's Fable pin counts nothing there, though it leaves under `fable_model` ahead of `ended`.
    const stays: (number | null)[] = [];
    const walked = onAccount.flatMap((p) => {
      const word = leftOutAs(p, fired);
      if (word !== null) {
        left.push(word);
        if (word === 'fable_model' && !p.ended) stays.push(burnOf(p).burn);
        return [];
      }
      const { burn, served } = burnOf(p);
      // A pin that burns nothing on the fired window has nothing to shed, so it leaves before the order and the guard's
      // sum, as the router's partition takes it.
      if (burn === 0) {
        left.push('no_burn');
        return [];
      }
      return [
        {
          burn,
          dropCost: p.moveDropCost,
          dropSize: p.moveDropSize,
          id: `${p.keyId}/${p.sessionId}/${p.agentId}/${p.model}`,
          moveCost: p.moveCost,
          order: served,
          runway: runwayOf(account.id, p.model, fired.reservation),
          // A pin with no prompt size reported yet ranks as the cheapest rewrite, at one token, as the round reads it.
          size: p.moveSize ?? 1,
          standing: p.standing,
          warm: p.warm,
        },
      ];
    });
    type Pin = (typeof walked)[number];
    // A pin whose burn cannot be priced ranks after every priced pin of its group, as the router's round ranks it.
    const pricedFirst = (a: number | null, b: number | null) => compareFigures(a, b, 'desc');
    const perRewrite = (p: Pin) => (p.order === null ? null : p.order / p.size);
    const byRewrite = (a: Pin, b: Pin) => pricedFirst(perRewrite(a), perRewrite(b));
    // A cold pin's cache is rewritten wherever it runs next, so the pins whose move drops nothing lead by burn, and
    // those that drop thinking, which the session pays again as output, follow by burn over the weighted drop. A pin
    // with no priced burn ranks after every priced cold pin, those that drop thinking among them.
    const perDrop = (p: Pin) => (p.order === null ? null : p.dropSize === null ? p.order : p.order / p.dropSize);
    const coldOrder = (a: Pin, b: Pin) => (a.order === null || b.order === null ? 0 : Number(a.dropSize !== null) - Number(b.dropSize !== null)) || pricedFirst(perDrop(a), perDrop(b));
    const cold = walked.filter((p) => !p.warm).sort(coldOrder);
    const warm = walked.filter((p) => p.warm).sort(byRewrite);
    const ordered = pool.warmSessions === 'always' ? [...cold, ...warm].sort(byRewrite) : [...cold, ...warm];
    const total = walked.reduce((sum, p) => sum + (p.burn ?? 0), 0);
    // A cold pin's cache is rewritten wherever it runs next, so its move pays the dropped thinking's share alone. A
    // move the cost model sizes none of reads 0, as the router's pick reads it. Under `never` the walk stops after the
    // cold pins, so a warm pin stays in the order, where the guard still counts its burn, and is never marked.
    const walk = shedMarks(
      ordered.map((p) => ({ burn: p.burn, cost: pool.warmSessions === 'always' || p.warm ? (p.moveCost ?? 0) : (p.dropCost ?? 0), id: p.id, pastCold: pool.warmSessions === 'never' && p.warm, runway: p.runway, standing: p.standing })),
      targetOf(total),
      { burn: stays.reduce((sum: number, burn) => sum + (burn ?? 0), 0), unpriced: stays.some((burn) => burn === null) },
    );
    const marks = new Set(walk.marks);
    return { left, quiet: walk.quiet, taken: walked.filter((p) => marks.has(p.id)), walked };
  };
  for (const { account, reservation } of listed) {
    if (account.kind === 'api_key') continue;
    const onAccount = [...(byAccount.get(account.id) ?? [])]
      // The router walks an account's pins in key, session, agent and model order as bytes, which settles ties in the
      // sorts below.
      .sort((a, b) => byteOrder(a.keyId, b.keyId) || byteOrder(a.sessionId, b.sessionId) || byteOrder(a.agentId, b.agentId) || byteOrder(a.model, b.model));
    // The router's trigger reads the pins the pool keeps on the account, a pin on the fast lane among them, and the
    // keep leaves an ended agent's pin out.
    const kept = onAccount.filter((p) => !p.ended);
    const five = windowOf(account, '5h');
    const start = five === null ? null : windowStart(five, now);
    // A start at or after `now`, from a reset more than five hours ahead, is no reading of a fresh window, so it holds
    // nothing, as the router reads it.
    const heldNow = start !== null && start < now && now - start < 2 * windowMs;
    const end = endOf(account, now);
    const lineAt = (id: ShedWindow, w: RateLimitWindow, horizon: number) => lineOf(pool, id, w.resetsAt === null ? null : Date.parse(w.resetsAt), end, horizon, now);
    const readOf = (id: ShedWindow): WindowRead | null => {
      const w = windowOf(account, id);
      if (w === null || w.horizon === null) return null;
      const horizon = Date.parse(w.horizon);
      if (id === '5h' && pool.slowDown) {
        const entry = account.slowDown;
        // The route serves the figure the last selection stored, and the router's test reads none once the horizon less
        // the threshold has come.
        if (entry === null || entry.stretchProjection === null || horizon - entry.thresholdMs <= now) return null;
        return { burn: w.burn, horizon: horizon - entry.thresholdMs, id, line: pool.fiveHourCeiling, projection: entry.stretchProjection };
      }
      if (w.projection === null) return null;
      return { burn: w.burn, horizon, id, line: lineAt(id, w, horizon), projection: w.projection };
    };
    // One 5-hour read feeds the trigger and the hold's test, so a slowed member under the hold is named by its stretch
    // figure as well.
    const fiveRead = readOf('5h');
    // A model week is read only where one of the pins draws it, a pin on the fast lane among them, as the router's
    // trigger reads every pin the pool keeps on the account.
    const read = TRIGGER_ORDER.filter((id) => id !== '7d_fable' || kept.some((p) => drawsFable(p.model)))
      .map((id) => (id === '5h' ? (heldNow ? null : fiveRead) : readOf(id)))
      .filter((r): r is WindowRead => r !== null);
    // A young 5-hour window under its line changes nothing in the round, so only one over it names its member: that
    // is the round the hold took away.
    if (heldNow && fiveRead !== null && fiveRead.projection > fiveRead.line) held.push({ ageMs: now - start, holdMs: 2 * windowMs, id: account.id, label: account.label });
    const fired = read.find((w) => w.projection > w.line);
    if (fired === undefined) {
      // The room to each window's own ceiling ranks them, since the two kinds of window read different ceilings.
      for (const w of read) if (closest === null || ceilingOf(pool, w.id) - w.projection < ceilingOf(pool, closest.window) - closest.projection) closest = { horizon: w.horizon, label: account.label, projection: w.projection, window: w.id };
      // The reservation's test runs once no window fired, as the router's does, and on a 7-day window with a share
      // alone, since the router's needs the projection of a window it holds a reading of.
      const week = windowOf(account, '7d');
      if (!reserving || week === null || week.utilization === null || week.projection === null || !shortOfReservation(account, reservation)) continue;
      // Only the Fable week's use is reserved, so the round would leave out each Fable pin. Where its predicates leave
      // out each pin the pool keeps on the account, the router cancels the round, whatever those pins burn.
      const round7d = { burn: week.burn, id: '7d' as const, reservation: true };
      if (kept.every((p) => leftOutAs(p, round7d) !== null)) {
        reserved.idle.push(account.label);
        continue;
      }
      const round = walkOn(account, onAccount, round7d, (total) => pool.shedCap * total);
      reserved.labels.push(account.label);
      reserved.sessions += round.taken.length;
      reserved.warm += round.taken.filter((p) => p.warm).length;
      continue;
    }
    // A horizon at or behind `now` reads as a minute away, so the excess stays finite.
    const excess = (fired.projection - fired.line) / Math.max((fired.horizon - now) / HOUR, 1 / 60);
    hot.push({ excess, horizon: fired.horizon, id: account.id, label: account.label, projection: fired.projection, window: fired.id });
    if (!pool.loadShedding) continue;
    const round = walkOn(account, onAccount, { burn: fired.burn, id: fired.id, reservation: false }, (total) => Math.min(excess, pool.shedCap * total));
    covered += Math.min(round.taken.reduce((sum, p) => sum + (p.burn ?? 0), 0), excess);
    marked += round.taken.length;
    warmMarked += round.taken.filter((p) => p.warm).length;
    walkedWords.push(round.quiet);
    const burns = round.walked.filter((p) => p.burn !== null);
    burning += burns.length;
    burningWarm += burns.filter((p) => pool.warmSessions === 'never' && p.warm).length;
    // The router reads a left-out pin's word only where its round walked no pin, and the fast lane names none.
    if (round.walked.length === 0) leftOutWords.push(...round.left.flatMap((word) => (word === 'fast_lane' ? [] : [word])));
  }
  // The router runs a round per member, each with its own word, and the preview draws one line for them all.
  const quiet = !pool.loadShedding || hot.length === 0 || marked > 0 ? null : (firstCause(walkedWords) ?? (burning > 0 && burningWarm === burning ? 'warm' : (firstCause(leftOutWords) ?? 'unpriced')));
  return {
    closest: hot.length === 0 ? closest : null,
    covered,
    held,
    hot,
    off: !pool.loadShedding,
    quiet,
    reservation: reserved,
    sessions: marked,
    warm: warmMarked,
  };
}

/** The reason a round that marks no session reads for each word of `quiet`, `no_runway` keeping its own sentence. */
const QUIET_REASON: Record<Exclude<NonNullable<ShedPreview['quiet']>, 'no_runway'>, string> = {
  account_bound: 'each holds state on its account, which a move would lose.',
  ended: 'each agent has ended.',
  // No round of the preview reads this word: a window's round leaves no pin out under it, and the reservation's rounds
  // feed their own sentences.
  fable_model: 'each is a Fable session, which the reservation keeps.',
  last_pin: 'each would leave its member too little burn.',
  no_burn: 'none burns.',
  off_window: 'none draws that window.',
  sliver: 'each is too small to move.',
  standing: 'the sessions already marked cover it.',
  unpriced: 'none is priced.',
  warm: 'each is warm.',
};

/**
 * The shedding group's dry run in up to five sentences, plus one per idle and one per held member: the switch when it
 * is off, the members whose projection crosses the ceiling or the one closest to it, and the round they would run, or
 * that no sibling has runway for their sessions, or why a round would mark none of them, then the members whose 7-day
 * room stands under their Fable reservation and what their rounds would mark, then each member under its reservation
 * with no session of another model to move, then each member whose 5-hour round the hold keeps back. A 5-hour horizon
 * reads its clock time, a weekly one the span to it.
 */
export function shedPreviewLine(preview: ShedPreview, now: number = Date.now()): string {
  const off = preview.off ? ['Load shedding is off: no round runs.'] : [];
  const { closest, hot } = preview;
  const held = preview.held.map((h) => `${h.label}'s 5-hour window is ${minuteSpan(h.ageMs)} old, under the hold of ${minuteSpan(h.holdMs)}, so its round reads the next window.`);
  const { idle, labels, sessions, warm } = preview.reservation;
  const reserved = [
    ...(labels.length === 0
      ? []
      : [
          `${plural(labels.length, 'member')} ${noun(labels.length, 'holds', 'hold')} less 7-day room than ${noun(labels.length, 'its', 'their')} Fable reservation: ${labels.join(', ')}.`,
          sessions === 0 ? 'A round there would mark no non-Fable session.' : `A round there would mark ${plural(sessions, 'non-Fable session')} (${warm} warm), to move to a sibling with runway.`,
        ]),
    ...idle.map((label) => `${label} is under its Fable reservation, and no session of another model is there to move.`),
  ];
  if (hot.length === 0) {
    const at = closest === null ? '' : closest.window === '5h' ? `at ${instant(closest.horizon, now)}` : `in ${fmtDuration(closest.horizon - now)}`;
    const nearest = closest === null ? '' : `: the closest is ${closest.label}, ${WINDOW_SHORT[closest.window]} ${percentText(closest.projection)} ${at}`;
    return [...off, `No member projects over the ceiling on any window${nearest}.`, ...reserved, ...held].join(' ');
  }
  const over = `${plural(hot.length, 'member')} ${noun(hot.length, 'projects', 'project')} over the ceiling: ${hot.map((h) => h.label).join(', ')}.`;
  if (preview.off) return [...off, over, ...held].join(' ');
  const count = plural(preview.sessions, 'session');
  const round =
    preview.quiet === 'no_runway'
      ? 'No sibling has runway for them, so nothing would be marked.'
      : preview.quiet !== null
        ? `A round there would mark no session: ${QUIET_REASON[preview.quiet]}`
        : `A round there would mark ${count} (${preview.warm} warm), ${(preview.covered * 100).toFixed(1)} %/h of the excess, to move to a sibling with runway.`;
  return [over, round, ...reserved, ...held].join(' ');
}

/** The slow-down group's dry run, from the slow-down the routes serve on each member. */
export interface SlowDownPreview {
  holdSeconds: number;
  /** The pool's switch is off: no request is held. */
  off: boolean;
  /**
   * The members read at a factor under 1, in the order the Members group lists them. The stretch is null on a member
   * whose account did not load, which counts by the factor the pools route serves on it alone.
   */
  slowed: { factor: number; label: string; stretchMs: number | null }[];
  thresholdMs: number;
}

/**
 * The slow-down group's dry run, replayed from the data the routes serve: a loaded account reads the factor and the
 * stretch the router last read for it at a pinned selection, and a member whose account did not load reads the factor
 * the pools route serves on it. The threshold and the hold are the pool's as the dialog holds them.
 */
export function slowDownPreview(
  pool: Pick<Pool, 'slowDown' | 'slowDownHoldSeconds' | 'slowDownStretchSeconds'> & { members: { accountId: string; factor: number | null; priority: number }[] },
  accounts: Account[],
): SlowDownPreview {
  const thresholdMs = pool.slowDownStretchSeconds * 1_000;
  const byId = new Map(accounts.map((a) => [a.id, a]));
  const read = pool.members
    .map((m) => {
      const account = byId.get(m.accountId);
      if (account === undefined) return { factor: m.factor, label: m.accountId, priority: m.priority, stretchMs: null };
      const slowDown = account.slowDown ?? null;
      return { factor: slowDown?.factor ?? null, label: account.label, priority: m.priority, stretchMs: slowDown?.stretchMs ?? null };
    })
    .sort((a, b) => a.priority - b.priority || a.label.localeCompare(b.label));
  return {
    holdSeconds: pool.slowDownHoldSeconds,
    off: !pool.slowDown,
    slowed: read.flatMap(({ factor, label, stretchMs }) => (factor !== null && factor < 1 ? [{ factor, label, stretchMs }] : [])),
    thresholdMs,
  };
}

/**
 * The slow-down group's dry run in up to two sentences: the switch when it is off, or the members slowed with their
 * factors and stretches and the wait a session on the slowest member takes. A member that is not slowed reads a
 * stretch of 0, so a pool with none slowed has no nearest member to name.
 */
export function slowDownPreviewLine(preview: SlowDownPreview): string {
  if (preview.off) return 'Slow-down is off: no request is held.';
  const { slowed } = preview;
  if (slowed.length === 0) return 'No member is slowed.';
  const threshold = minuteSpan(preview.thresholdMs);
  const named = slowed.map(({ factor, label, stretchMs }) => `${label} at ${speedWord(factor)}${stretchMs === null ? '' : ` (${minuteSpan(stretchMs)} of ${threshold})`}`);
  // The first of the lowest factors, so a tie names the member listed first.
  const slowest = slowed.reduce((low, m) => (m.factor < low.factor ? m : low));
  const hold = `${preview.holdSeconds} s`;
  const head = `${plural(slowed.length, 'member')} ${noun(slowed.length, 'is', 'are')} slowed: ${named.join(', ')}.`;
  const who = slowed.length === 1 ? 'A session there' : `A session on ${slowest.label}`;
  // A factor of 0 is a window at its ceiling, where every held request waits the whole hold.
  const wait = slowest.factor > 0 ? `waits its cycle times ${(1 / slowest.factor - 1).toFixed(1)}, up to ${hold}` : `waits the full ${hold}`;
  return `${head} ${who} ${wait}.`;
}
