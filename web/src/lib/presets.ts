import type { Account, MemberEligibility, Pool, RateLimitWindow, Session } from '@/api/types';
import { fmtDuration, instant } from './format';
import { windowOf } from './quota';

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
const cmp = (a: string, b: string) => (a < b ? -1 : a > b ? 1 : 0);

export function sheddingPresetOf(p: Pick<Pool, 'burnAverageSeconds' | 'shedCap' | 'shedDwellSeconds'>): SheddingPreset {
  const names = Object.keys(SHEDDING_PRESETS) as Exclude<SheddingPreset, 'custom'>[];
  return (
    names.find((name) => {
      const v = SHEDDING_PRESETS[name];
      return v.burnAverageSeconds === p.burnAverageSeconds && near(v.shedCap, p.shedCap) && v.shedDwellSeconds === p.shedDwellSeconds;
    }) ?? 'custom'
  );
}

/** Both values must match a preset. Balanced and conservative differ in the retry-in-place bound alone. */
export function failoverPresetOf(p: Pick<Pool, 'inFlightCap' | 'retryInPlaceSeconds'>): FailoverPreset {
  const names = Object.keys(FAILOVER_PRESETS) as Exclude<FailoverPreset, 'custom'>[];
  return (
    names.find((name) => {
      const v = FAILOVER_PRESETS[name];
      return v.inFlightCap === p.inFlightCap && v.retryInPlaceSeconds === p.retryInPlaceSeconds;
    }) ?? 'custom'
  );
}

/**
 * The round's marking rule, the mirror of the router's walk: pins in the round's order with their burns on the round's
 * window, marked until the marked burn reaches `target`, never the last unmarked pin, none for a zero target. An
 * unpriced pin and one flagged `skip` stay unmarked and count nothing toward the target.
 */
export function shedMarks(ordered: { burn: number | null; id: string; skip?: boolean }[], target: number): string[] {
  const marks: string[] = [];
  if (Number.isNaN(target) || target <= 0) return marks;
  let marked = 0;
  for (const pin of ordered) {
    // An indivisible pin may carry the marked burn past the target, and the round stops there.
    if (marked >= target || marks.length + 1 >= ordered.length) break;
    // An unpriced burn moves the marked sum nowhere, so marking the pin would shed a session on a guess. A pin with
    // no sibling to move to stays where it is. Either still counts as one of the account's pins for the last-pin rule.
    if (pin.burn === null || pin.skip === true) continue;
    marks.push(pin.id);
    marked += pin.burn;
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

const HOUR_MS = 3_600_000;

/** A plan window the trigger reads, in the order the router's round walks them: the shared windows, then the week. */
export type ShedWindow = '5h' | '7d' | '7d_fable';
const TRIGGER_ORDER: ShedWindow[] = ['5h', '7d', '7d_fable'];
const PREVIEW_WINDOW: Record<ShedWindow, string> = { '5h': '5h', '7d': '7d', '7d_fable': 'Fable week' };

const readingOf = (account: Account, id: ShedWindow): RateLimitWindow | null => (id === '7d_fable' ? account.fableWindow : windowOf(account, id));
/** The router's `MODEL_WINDOWS`: a model whose id names the Fable family draws the Fable week beside the shared two. */
const drawsFable = (model: string | null) => (model ?? '').toLowerCase().includes('fable');
const windowsOf = (model: string | null): ShedWindow[] => (drawsFable(model) ? TRIGGER_ORDER : TRIGGER_ORDER.slice(0, 2));

/**
 * Whether an API-key account has room for a moved session: its worst live bucket under 1.0, since a bucket refills
 * within its minute. Its live buckets are its `rpm:`, `itpm:` and `otpm:` ones whose reset is ahead, or which carry none
 * and were read within the minute, as the router's `worst_bucket` reads them.
 */
const bucketRoom = (a: Account, now: number) =>
  Math.max(0, ...a.windows.filter((w) => /^(rpm|itpm|otpm):/.test(w.id) && (w.resetsAt === null ? now - Date.parse(w.observedAt) < 60_000 : Date.parse(w.resetsAt) > now)).map((w) => w.utilization)) < 1;

/** A member whose projection crosses the ceiling, on the first window in the trigger's order that does. */
export interface ShedHot {
  /** The burn a round removes to land the projection on the ceiling at the horizon, in the window's share an hour. */
  excess: number;
  horizon: number;
  id: string;
  label: string;
  projection: number;
  window: ShedWindow;
}

export interface ShedPreview {
  /** With no member over the ceiling, the window whose projection reads nearest under it. Null with none to read. */
  closest: { horizon: number; label: string; projection: number; window: ShedWindow } | null;
  /** The excess the marks take off, in share an hour, each member's in the share of the window that fired it. */
  covered: number;
  /** The members over the ceiling, in the order the Members group lists them. */
  hot: ShedHot[];
  /** The rounds mark nothing because no sibling has runway for any pin they could mark. */
  noRunway: boolean;
  /** The pool's switch is off: no round runs, whatever the projection reads. */
  off: boolean;
  sessions: number;
  warm: number;
}

/**
 * The shedding round's dry run, replayed from the data the routes serve. Each member's served projection is read at
 * its served horizon on the windows its pins draw, in the trigger's order, and the first window over the ceiling fires
 * its round. The round walks the pool's own pins on the member, the sessions under its keys, which the caller filters,
 * since the router's round marks the triggering pool's pins alone. It orders them by their served 5-hour burn, decayed
 * from the instant it was served to `now`: cold pins first by burn, then warm ones by burn over their last input size,
 * bounded by the warm-sessions mode. It marks until the removed burn covers the excess under `shedCap`, passing over a
 * pin with no sibling that has runway for it. A member without `eligible` counts as eligible, as a member of a pool not
 * saved yet does.
 */
export function shedPreview(
  pool: Pick<Pool, 'burnAverageSeconds' | 'loadShedding' | 'shedCap' | 'warmSessions' | 'windowCeiling'> & { members: { accountId: string; eligible?: MemberEligibility; priority: number }[] },
  accounts: Account[],
  sessions: Session[],
  now: number,
): ShedPreview {
  const ceiling = pool.windowCeiling;
  const byId = new Map(accounts.map((a) => [a.id, a]));
  const listed = pool.members
    .flatMap((m) => {
      const account = byId.get(m.accountId);
      return account === undefined ? [] : [{ account, eligible: (m.eligible ?? 'eligible') === 'eligible', priority: m.priority }];
    })
    .sort((a, b) => a.priority - b.priority || a.account.label.localeCompare(b.account.label));
  // The preview's reading of the gate: a sibling has runway for a pin while its served projection sits under the
  // ceiling on every window the pin's model draws. The router also adds the pin's rewrite and burn and counts the
  // round's planned reservations, which the served figures cannot replay, so this is an approximation of its read.
  const hasRunway = (source: string, model: string | null) =>
    listed.some(
      ({ account, eligible }) =>
        account.id !== source &&
        eligible &&
        (account.kind === 'api_key'
          ? bucketRoom(account, now)
          : windowsOf(model).every((id) => {
              const projection = readingOf(account, id)?.projection ?? null;
              return projection !== null && projection < ceiling;
            })),
    );
  const windowMs = pool.burnAverageSeconds * 1_000;
  const hot: ShedHot[] = [];
  let closest: ShedPreview['closest'] = null;
  let covered = 0;
  let marked = 0;
  let warmMarked = 0;
  let walkable = 0;
  let stranded = 0;
  for (const { account } of listed) {
    if (account.kind === 'api_key') continue;
    const pins = sessions
      .filter((s) => s.pinned && s.accountId === account.id)
      // The router walks an account's pins in key, session and model order, which settles ties in the sorts below.
      .sort((a, b) => cmp(a.keyId, b.keyId) || cmp(a.sessionId, b.sessionId) || cmp(a.model ?? '', b.model ?? ''));
    // A model week is read only where one of the pins draws it, as the router's trigger reads it.
    const read = TRIGGER_ORDER.filter((id) => id !== '7d_fable' || pins.some((s) => drawsFable(s.model))).flatMap((id) => {
      const w = readingOf(account, id);
      return w === null || w.projection === null || w.horizon === null ? [] : [{ burn: w.burn, horizon: Date.parse(w.horizon), id, projection: w.projection }];
    });
    const fired = read.find((w) => w.projection > ceiling);
    if (fired === undefined) {
      for (const w of read) if (closest === null || w.projection > closest.projection) closest = { horizon: w.horizon, label: account.label, projection: w.projection, window: w.id };
      continue;
    }
    // A horizon at or behind `now` reads as a minute away, so the excess stays finite.
    const excess = (fired.projection - ceiling) / Math.max((fired.horizon - now) / HOUR_MS, 1 / 60);
    hot.push({ excess, horizon: fired.horizon, id: account.id, label: account.label, projection: fired.projection, window: fired.id });
    if (!pool.loadShedding) continue;
    // The served burn is a pin's share of the account's 5-hour window. A round on a weekly window reads each pin's
    // burn scaled by the account's burn on that window over its 5-hour burn, the preview's approximation of the
    // router's pricing of each pin on the fired window. A pin served in dollars has no share to scale.
    const fiveHour = readingOf(account, '5h')?.burn ?? null;
    const scale = fired.id === '5h' ? 1 : fiveHour !== null && fiveHour > 0 && fired.burn !== null ? fired.burn / fiveHour : null;
    const walked = pins.map((s) => {
      const served = s.burn === null || s.burnUnit !== 'window_share' ? null : burnDecayed(s.burn, s.burnAt, now, windowMs);
      return {
        burn: served === null || scale === null ? null : served * scale,
        id: `${s.keyId}/${s.sessionId}/${s.model ?? ''}`,
        order: served,
        // A pin with no prompt size reported yet ranks as the cheapest rewrite.
        rewrite: Math.max(s.lastInputTokens ?? 0, 1),
        runway: hasRunway(account.id, s.model),
        warm: s.lastRefreshAt !== null && s.cacheTtlMs !== null && now - Date.parse(s.lastRefreshAt) < s.cacheTtlMs,
      };
    });
    type Pin = (typeof walked)[number];
    // A pin whose burn cannot be priced ranks after every priced pin of its group, as the router's round ranks it.
    const pricedFirst = (a: number | null, b: number | null) => (a === null ? (b === null ? 0 : 1) : b === null ? -1 : b - a);
    const perRewrite = (p: Pin) => (p.order === null ? null : p.order / p.rewrite);
    const byRewrite = (a: Pin, b: Pin) => pricedFirst(perRewrite(a), perRewrite(b));
    const cold = walked.filter((p) => !p.warm).sort((a, b) => pricedFirst(a.order, b.order));
    const warm = walked.filter((p) => p.warm).sort(byRewrite);
    const ordered = pool.warmSessions === 'always' ? [...cold, ...warm].sort(byRewrite) : [...cold, ...warm];
    // Under `never` the walk stops after the cold pins, so a warm pin stays in the order, where the last-pin rule
    // still counts it among the member's pins, and is never marked.
    const open = (p: Pin) => p.burn !== null && !(pool.warmSessions === 'never' && p.warm);
    const total = walked.reduce((sum, p) => sum + (p.burn ?? 0), 0);
    const marks = new Set(shedMarks(ordered.map((p) => ({ burn: p.burn, id: p.id, skip: !open(p) || !p.runway })), Math.min(excess, pool.shedCap * total)));
    const taken = walked.filter((p) => marks.has(p.id));
    covered += Math.min(taken.reduce((sum, p) => sum + (p.burn ?? 0), 0), excess);
    marked += taken.length;
    warmMarked += taken.filter((p) => p.warm).length;
    walkable += walked.filter(open).length;
    stranded += walked.filter((p) => open(p) && !p.runway).length;
  }
  return { closest: hot.length === 0 ? closest : null, covered, hot, noRunway: marked === 0 && walkable > 0 && stranded === walkable, off: !pool.loadShedding, sessions: marked, warm: warmMarked };
}

/** A share as a whole percent with the space before the sign, as the pool card's badge writes it. */
const percentText = (share: number) => `${Math.round(share * 100)} %`;

/**
 * The shedding group's dry run in up to three sentences: the switch when it is off, the members whose projection
 * crosses the ceiling or the one closest to it, and the round they would run, or that no sibling has runway for their
 * sessions. A 5-hour horizon reads its clock time, a weekly one the span to it.
 */
export function shedPreviewLine(preview: ShedPreview, now: number = Date.now()): string {
  const off = preview.off ? ['Load shedding is off: no round runs.'] : [];
  const { closest, hot } = preview;
  if (hot.length === 0) {
    const at = closest === null ? '' : closest.window === '5h' ? `at ${instant(closest.horizon, now)}` : `in ${fmtDuration(closest.horizon - now)}`;
    const nearest = closest === null ? '' : `: the closest is ${closest.label}, ${PREVIEW_WINDOW[closest.window]} ${percentText(closest.projection)} ${at}`;
    return [...off, `No member projects over the ceiling on any window${nearest}.`].join(' ');
  }
  const over = hot.length === 1 ? `1 member projects over the ceiling: ${hot[0].label}.` : `${hot.length} members project over the ceiling: ${hot.map((h) => h.label).join(', ')}.`;
  if (preview.off) return [...off, over].join(' ');
  const count = `${preview.sessions} ${preview.sessions === 1 ? 'session' : 'sessions'}`;
  const round = preview.noRunway ? 'No sibling has runway for them, so nothing would be marked.' : `A round there would mark ${count} (${preview.warm} warm), ${(preview.covered * 100).toFixed(1)} %/h of the excess, to move to a sibling with runway.`;
  return `${over} ${round}`;
}
