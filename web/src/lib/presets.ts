import type { Account, AgentContext, MemberEligibility, Pool, RateLimitWindow, Session } from '@/api/types';
import { byteOrder, fmtDuration, instant, noun, percentText, plural } from './format';
import { openMark } from './keepalive';
import { endOf, minuteSpan, windowOf } from './quota';
import { speedWord, windingDown } from './selection';
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

/**
 * The round's marking rule, the mirror of the router's walk: pins in the round's order with their burns on the round's
 * window, marked until the removed burn reaches `target`, none for a zero target. A pin under a standing mark is a move
 * pending, so its burn counts as removed and the walk skips it. Once one pin from the walk's position on covers what
 * the target still asks, the walk marks the covering pin whose move costs the least, the first in the order among
 * equals, and stops. Otherwise a pin under `SHED_MARK_SHARE` of that rest stays as a sliver, and the pin at the
 * position is marked. A mark that would leave the account's other priced pins, those neither marked nor under a
 * standing mark, under `SHED_MARK_SHARE` of the round's burn sum is refused, and the walk goes on. A pin flagged `skip`
 * stays unmarked and so counts among those pins, as the router counts a pin it passes over. An unpriced pin's burn is
 * unknown and not none, so while one stands unmarked and under no standing mark the round refuses no mark. An
 * unpriced pin and one flagged `skip` count nothing toward the target.
 */
export function shedMarks(ordered: { burn: number | null; cost?: number; id: string; skip?: boolean; standing?: boolean }[], target: number): string[] {
  type Pin = (typeof ordered)[number];
  const marks: string[] = [];
  if (Number.isNaN(target) || target <= 0) return marks;
  const sum = ordered.reduce((n, pin) => n + (pin.burn ?? 0), 0);
  const unpricedStays = ordered.some((pin) => pin.burn === null && pin.standing !== true);
  // A standing mark is a move pending, so its burn counts as removed before the walk marks anything else.
  let removed = ordered.reduce((n, pin) => n + (pin.standing === true ? (pin.burn ?? 0) : 0), 0);
  // The guard sums the pins that stay, as the router does, since the round's sum less what leaves can round under a
  // floor those pins reach exactly.
  const refused = (candidate: Pin) => !unpricedStays && ordered.filter((q) => q !== candidate && q.standing !== true && !marks.includes(q.id)).reduce((n, q) => n + (q.burn ?? 0), 0) < SHED_MARK_SHARE * sum;
  // An unpriced burn moves the removed sum nowhere, so marking the pin would shed a session on a guess. A pin with no
  // sibling to move to stays where it is, and its burn stays on the account.
  const open = (pin: Pin): pin is Pin & { burn: number } => pin.burn !== null && pin.skip !== true && pin.standing !== true;
  for (const [index, pin] of ordered.entries()) {
    // An indivisible pin may carry the removed burn past the target, and the round stops there.
    if (removed >= target) break;
    if (!open(pin)) continue;
    const rest = target - removed;
    // A pin is indivisible, so once one from here on covers the rest, the cheapest such move meets the target.
    const pick = ordered
      .slice(index)
      .filter(open)
      .filter((q) => q.burn >= rest && !refused(q))
      .reduce<Pin | null>((best, q) => (best !== null && (best.cost ?? 0) <= (q.cost ?? 0) ? best : q), null);
    if (pick !== null) {
      marks.push(pick.id);
      break;
    }
    // A pin under the floor relieves too little of the rest to be worth its move, so it stays.
    if (pin.burn < SHED_MARK_SHARE * rest) continue;
    // The guard refuses this pin alone: a later pin's smaller mark may still leave a tenth on the account.
    if (refused(pin)) continue;
    marks.push(pin.id);
    removed += pin.burn;
  }
  return marks;
}

/**
 * A pin's served burn decayed on from `burnAt`, the instant the route decayed it to, to `now`, as the router's average
 * decays between folds. A burn with no instant reads as served.
 */
function burnDecayed(burn: number, burnAt: string | null, now: number, windowMs: number): number {
  if (burnAt === null) return burn;
  // An instant after `now` (another clock) decays nothing rather than growing the burn.
  return burn * Math.exp(-Math.max(now - Date.parse(burnAt), 0) / Math.max(windowMs, 1));
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
const windowsOf = (model: string | null): ShedWindow[] => (drawsFable(model) ? TRIGGER_ORDER : TRIGGER_ORDER.slice(0, 2));

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
 * none and were read within the minute, as the router's `worst_bucket` reads them.
 */
const bucketRoom = (a: Account, now: number) =>
  Math.max(0, ...a.windows.filter((w) => /^(rpm|itpm|otpm):/.test(w.id) && (w.resetsAt === null ? now - Date.parse(w.observedAt) < 60_000 : Date.parse(w.resetsAt) > now)).map((w) => w.utilization)) < 1;

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
  /** The rounds mark nothing because no sibling has runway for any pin they could mark. */
  noRunway: boolean;
  /** The pool's switch is off: no round runs, whatever the projection reads. */
  off: boolean;
  /**
   * Why the rounds mark nothing though a pin there is priced, read over every member over the ceiling in the walk's
   * order: the priced pins burn nothing, so the target is zero (`burnless`), each priced pin is warm under `never`
   * (`warm`), or each pin a round could mark is its member's last (`last`). Null where the switch is off, where no
   * member is over the ceiling, where a round marks a session, where no pin is priced and where no sibling has runway.
   */
  quiet: 'burnless' | 'last' | 'warm' | null;
  sessions: number;
  /** The rounds mark nothing because no pin on the members over the ceiling is priced, or none is pinned there. */
  unpriced: boolean;
  warm: number;
}

/**
 * A conversation pin as the round walks it, its figures read off its source: the row for a display pin no line holds,
 * else the line. A line is warm before its first cold instant, and by its served `warm` where it carries none. The
 * row's mark serves its display pin alone, whichever source it walks as, since the route answers no mark per line.
 */
const walkedPin = (s: Session, line: AgentContext | null, now: number) => {
  const source = line ?? s;
  return {
    accountId: line === null ? s.accountId : line.accountId,
    agentId: line === null ? s.agentId : line.agentId,
    burn: source.burn,
    burnAt: source.burnAt,
    burnUnit: source.burnUnit,
    burnWindow: source.burnWindow,
    ended: source.ended,
    fast: source.fast,
    keyId: s.keyId,
    lastInputTokens: source.lastInputTokens,
    model: line === null ? s.model : line.model,
    moveCost: source.moveCost,
    sessionId: s.sessionId,
    standing: (line === null || (line.agentId === s.agentId && line.model === s.model)) && openMark(s) !== null,
    warm: line === null ? s.lastRefreshAt !== null && s.cacheTtlMs !== null && now - Date.parse(s.lastRefreshAt) < s.cacheTtlMs : line.warmUntil === null ? line.warm : now < Date.parse(line.warmUntil),
  };
};

/** Whether a walked pin's agent ended and its next request has not arrived, as the pin's own row or line reads it. */
const endedPin = (p: { ended: boolean | null }): boolean => p.ended === true;

/**
 * The shedding round's dry run, replayed from the data the routes serve. Each member's served projection is read at its
 * served horizon on the windows its pins draw, in the trigger's order, and the first window over the round's line fires
 * its round: the wall, 1, where the window's served reset ahead of `now` or the account's end set the horizon, else the
 * window's own ceiling, 5-hour or weekly. The excess runs from that line. A 5-hour window younger than twice the pool's
 * burn average does not fire, and the member reads its next window, as the router holds the round. With no member over
 * its line, the closest window is the one with the least room to its own ceiling. The router's round on a pool that
 * slows its pins tests the 5-hour window by the stretch, which the preview reads as it reads the other windows.
 *
 * The round walks the pool's own conversation pins on the member, the sessions under its keys, which the caller
 * filters, since the router's round marks the triggering pool's pins alone. They are the pins the sessions route lists:
 * each line on the member, and a row's display pin where the row is pinned there and no line holds its agent and model.
 * The router's round also walks a pin only auxiliary requests used and a pin whose line the route no longer lists,
 * which no served row holds. Each pin reads its burn, its prompt size, its lane, its ended mark and its move cost off
 * its own row or line, and two pins of one session are marked and counted apart. A pin on the fast lane or whose agent
 * ended is out of the round, its order, its sum and its counts, as the router's round leaves it out. The round orders
 * the pins by their served burn, decayed from the instant it was served to `now`: cold pins first by burn, then warm
 * ones by burn over their last input size, bounded by the warm-sessions mode. It marks until the removed burn covers
 * the excess under `shedCap`, passing over a pin with no sibling that has runway for it, by `shedMarks`. A pin under
 * its row's standing mark is a move pending, and a subagent's pending move still counts as burn to remove. A move costs
 * the pin's served move cost where the pin is warm or the mode reads `always`, and nothing for a cold pin. A member
 * without `eligible` counts as eligible, as a member of a pool not saved yet does.
 */
export function shedPreview(
  pool: Pick<Pool, 'burnAverageSeconds' | 'fiveHourCeiling' | 'loadShedding' | 'shedCap' | 'warmSessions' | 'weeklyCeiling' | 'windDownMinutes'> & { members: { accountId: string; eligible?: MemberEligibility; priority: number }[] },
  accounts: Account[],
  sessions: Session[],
  now: number,
): ShedPreview {
  const byId = new Map(accounts.map((a) => [a.id, a]));
  const listed = pool.members
    .flatMap((m) => {
      const account = byId.get(m.accountId);
      return account === undefined ? [] : [{ account, eligible: (m.eligible ?? 'eligible') === 'eligible', priority: m.priority }];
    })
    .sort((a, b) => a.priority - b.priority || a.account.label.localeCompare(b.account.label));
  // The preview's reading of the gate: a sibling has runway for a pin while its served projection sits under the
  // window's own ceiling on every window the pin's model draws. The router also adds the pin's rewrite and burn and
  // counts the round's planned reservations, which the served figures cannot replay, so this is an approximation of its
  // read. A sibling whose end falls inside the pool's wind-down is no destination whatever it reads, an end exactly the
  // wind-down away included. A sibling whose windows hold the 7-day window and no 5-hour one, as an approved Ultra
  // account's set does, is read on the 7-day window alone, since the router's fit books the windows of the
  // destination's set and nothing on a window that set lacks.
  const hasRunway = (source: string, model: string | null) =>
    listed.some(
      ({ account, eligible }) =>
        account.id !== source &&
        eligible &&
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
  const windowMs = pool.burnAverageSeconds * 1_000;
  const hot: ShedHot[] = [];
  const held: ShedHeld[] = [];
  let closest: ShedPreview['closest'] = null;
  let covered = 0;
  let marked = 0;
  let warmMarked = 0;
  let walkable = 0;
  let stranded = 0;
  let priced = 0;
  let burning = 0;
  const conversation = sessions.flatMap((s) => {
    const display = s.pinned && !s.agentContexts.some((c) => c.agentId === s.agentId && c.model === s.model) ? [walkedPin(s, null, now)] : [];
    return [...display, ...s.agentContexts.map((c) => walkedPin(s, c, now))];
  });
  for (const { account } of listed) {
    if (account.kind === 'api_key') continue;
    const pins = conversation
      .filter((p) => p.accountId === account.id && !endedPin(p) && p.fast !== true)
      // The router walks an account's pins in key, session, agent and model order as bytes, which settles ties in the
      // sorts below.
      .sort((a, b) => byteOrder(a.keyId, b.keyId) || byteOrder(a.sessionId, b.sessionId) || byteOrder(a.agentId ?? '', b.agentId ?? '') || byteOrder(a.model ?? '', b.model ?? ''));
    const five = windowOf(account, '5h');
    const start = five === null ? null : windowStart(five, now);
    // A start at or after `now`, from a reset more than five hours ahead, is no reading of a fresh window, so it holds
    // nothing, as the router reads it.
    const heldNow = start !== null && start < now && now - start < 2 * windowMs;
    const end = endOf(account, now);
    const lineAt = (id: ShedWindow, w: RateLimitWindow, horizon: number) => lineOf(pool, id, w.resetsAt === null ? null : Date.parse(w.resetsAt), end, horizon, now);
    // A model week is read only where one of the pins draws it, as the router's trigger reads it.
    const read = TRIGGER_ORDER.filter((id) => id !== '7d_fable' || pins.some((p) => drawsFable(p.model)))
      .filter((id) => !(id === '5h' && heldNow))
      .flatMap((id) => {
        const w = windowOf(account, id);
        if (w === null || w.projection === null || w.horizon === null) return [];
        const horizon = Date.parse(w.horizon);
        return [{ burn: w.burn, horizon, id, line: lineAt(id, w, horizon), projection: w.projection }];
      });
    // A young 5-hour window under its line changes nothing in the round, so only one over it names its member: that
    // is the round the hold took away.
    if (heldNow && five !== null && five.projection !== null && five.horizon !== null && five.projection > lineAt('5h', five, Date.parse(five.horizon))) held.push({ ageMs: now - start, holdMs: 2 * windowMs, id: account.id, label: account.label });
    const fired = read.find((w) => w.projection > w.line);
    if (fired === undefined) {
      // The room to each window's own ceiling ranks them, since the two kinds of window read different ceilings.
      for (const w of read) if (closest === null || ceilingOf(pool, w.id) - w.projection < ceilingOf(pool, closest.window) - closest.projection) closest = { horizon: w.horizon, label: account.label, projection: w.projection, window: w.id };
      continue;
    }
    // A horizon at or behind `now` reads as a minute away, so the excess stays finite.
    const excess = (fired.projection - fired.line) / Math.max((fired.horizon - now) / HOUR, 1 / 60);
    hot.push({ excess, horizon: fired.horizon, id: account.id, label: account.label, projection: fired.projection, window: fired.id });
    if (!pool.loadShedding) continue;
    // The served burn is a pin's share of the window its `burnWindow` names: the 5-hour window, or the 7-day one on an
    // account without it. A round on another window reads each pin's burn scaled by the account's burn on the fired
    // window over its burn on the pin's own, the preview's approximation of the router's pricing of each pin on the
    // fired window. A pin whose model does not draw the fired window is out of the round before the order, as the
    // router's partition takes it, so it is in neither the order, the counts nor the quiet reading, and a pin served in
    // dollars has no share to scale, so it reads unpriced.
    const walked = pins
      .filter((p) => windowsOf(p.model).includes(fired.id))
      .map((p) => {
        const window = burnWindowOf(p.burnWindow);
        const own = window === null ? null : (windowOf(account, window)?.burn ?? null);
        const scale = fired.id === window ? 1 : own !== null && own > 0 && fired.burn !== null ? fired.burn / own : null;
        const served = p.burn === null || p.burnUnit !== 'window_share' ? null : burnDecayed(p.burn, p.burnAt, now, windowMs);
        return {
          burn: served === null || scale === null ? null : served * scale,
          id: `${p.keyId}/${p.sessionId}/${p.agentId ?? ''}/${p.model ?? ''}`,
          moveCost: p.moveCost,
          order: served,
          // A pin with no prompt size reported yet ranks as the cheapest rewrite.
          rewrite: Math.max(p.lastInputTokens ?? 0, 1),
          runway: hasRunway(account.id, p.model),
          standing: p.standing,
          warm: p.warm,
        };
      });
    type Pin = (typeof walked)[number];
    // A pin whose burn cannot be priced ranks after every priced pin of its group, as the router's round ranks it.
    const pricedFirst = (a: number | null, b: number | null) => compareFigures(a, b, 'desc');
    const perRewrite = (p: Pin) => (p.order === null ? null : p.order / p.rewrite);
    const byRewrite = (a: Pin, b: Pin) => pricedFirst(perRewrite(a), perRewrite(b));
    const cold = walked.filter((p) => !p.warm).sort((a, b) => pricedFirst(a.order, b.order));
    const warm = walked.filter((p) => p.warm).sort(byRewrite);
    const ordered = pool.warmSessions === 'always' ? [...cold, ...warm].sort(byRewrite) : [...cold, ...warm];
    // Under `never` the walk stops after the cold pins, so a warm pin stays in the order, where the burn guard still
    // counts its burn among the member's unmarked pins, and is never marked.
    const open = (p: Pin) => p.burn !== null && !(pool.warmSessions === 'never' && p.warm);
    const total = walked.reduce((sum, p) => sum + (p.burn ?? 0), 0);
    // A cold pin's move costs nothing, since its cache is rewritten wherever it runs next. A move the cost model sizes
    // none of reads 0, as the router's pick reads it.
    const marks = new Set(
      shedMarks(
        ordered.map((p) => ({ burn: p.burn, cost: pool.warmSessions === 'always' || p.warm ? (p.moveCost ?? 0) : 0, id: p.id, skip: !open(p) || !p.runway, standing: p.standing })),
        Math.min(excess, pool.shedCap * total),
      ),
    );
    const taken = walked.filter((p) => marks.has(p.id));
    covered += Math.min(taken.reduce((sum, p) => sum + (p.burn ?? 0), 0), excess);
    marked += taken.length;
    warmMarked += taken.filter((p) => p.warm).length;
    walkable += walked.filter(open).length;
    priced += walked.filter((p) => p.burn !== null).length;
    burning += total;
    stranded += walked.filter((p) => open(p) && !p.runway).length;
  }
  const noRunway = marked === 0 && walkable > 0 && stranded === walkable;
  const walkedQuiet = pool.loadShedding && hot.length > 0 && marked === 0 && priced > 0 && !noRunway;
  return {
    closest: hot.length === 0 ? closest : null,
    covered,
    held,
    hot,
    noRunway,
    off: !pool.loadShedding,
    quiet: !walkedQuiet ? null : burning === 0 ? 'burnless' : walkable === 0 ? 'warm' : 'last',
    sessions: marked,
    // A walk reads the pins' prices, so with the switch off or no member over the ceiling there is no price to lack.
    unpriced: pool.loadShedding && hot.length > 0 && marked === 0 && priced === 0,
    warm: warmMarked,
  };
}

const QUIET_REASON: Record<NonNullable<ShedPreview['quiet']>, string> = { burnless: 'none burns.', last: "each is its member's last.", warm: 'each is warm.' };

/**
 * The shedding group's dry run in up to three sentences, plus one per held member: the switch when it is off, the
 * members whose projection crosses the ceiling or the one closest to it, and the round they would run, or that no
 * sibling has runway for their sessions, or why a round would mark none of them, then each member whose 5-hour round
 * the hold keeps back. A 5-hour horizon reads its clock time, a weekly one the span to it.
 */
export function shedPreviewLine(preview: ShedPreview, now: number = Date.now()): string {
  const off = preview.off ? ['Load shedding is off: no round runs.'] : [];
  const { closest, hot } = preview;
  const held = preview.held.map((h) => `${h.label}'s 5-hour window is ${minuteSpan(h.ageMs)} old, under the hold of ${minuteSpan(h.holdMs)}, so its round reads the next window.`);
  if (hot.length === 0) {
    const at = closest === null ? '' : closest.window === '5h' ? `at ${instant(closest.horizon, now)}` : `in ${fmtDuration(closest.horizon - now)}`;
    const nearest = closest === null ? '' : `: the closest is ${closest.label}, ${WINDOW_SHORT[closest.window]} ${percentText(closest.projection)} ${at}`;
    return [...off, `No member projects over the ceiling on any window${nearest}.`, ...held].join(' ');
  }
  const over = `${plural(hot.length, 'member')} ${noun(hot.length, 'projects', 'project')} over the ceiling: ${hot.map((h) => h.label).join(', ')}.`;
  if (preview.off) return [...off, over, ...held].join(' ');
  const count = plural(preview.sessions, 'session');
  const round = preview.noRunway
    ? 'No sibling has runway for them, so nothing would be marked.'
    : preview.unpriced
      ? 'A round there would mark no session: none is priced.'
      : preview.quiet !== null
        ? `A round there would mark no session: ${QUIET_REASON[preview.quiet]}`
        : `A round there would mark ${count} (${preview.warm} warm), ${(preview.covered * 100).toFixed(1)} %/h of the excess, to move to a sibling with runway.`;
  return [over, round, ...held].join(' ');
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
