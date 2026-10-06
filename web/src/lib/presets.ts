import type { Account, BurnUnit, MemberEligibility, Pool, Session } from '@/api/types';
import { fmtPct } from './format';
import { windowOf } from './quota';
import { WINDOW_HOURS } from './windows';

export type SheddingPreset = 'balanced' | 'conservative' | 'custom' | 'fast';

/** The store's default is `balanced`. */
export const SHEDDING_PRESETS: Record<Exclude<SheddingPreset, 'custom'>, { burnWindowSeconds: number; shedShare: number; sourceDwellSeconds: number }> = {
  balanced: { burnWindowSeconds: 600, shedShare: 0.5, sourceDwellSeconds: 900 },
  conservative: { burnWindowSeconds: 900, shedShare: 0.3, sourceDwellSeconds: 1_800 },
  fast: { burnWindowSeconds: 300, shedShare: 0.5, sourceDwellSeconds: 600 },
};

export type FailoverPreset = 'balanced' | 'conservative' | 'custom' | 'fast';

/** balanced 16, 60 s, 5 % (the default); fast 8, 20 s, 10 %; conservative 16, 120 s, 5 %. */
export const FAILOVER_PRESETS: Record<Exclude<FailoverPreset, 'custom'>, { inFlightCap: number; paceFloor: number; retryInPlaceSeconds: number }> = {
  balanced: { inFlightCap: 16, paceFloor: 0.05, retryInPlaceSeconds: 60 },
  conservative: { inFlightCap: 16, paceFloor: 0.05, retryInPlaceSeconds: 120 },
  fast: { inFlightCap: 8, paceFloor: 0.1, retryInPlaceSeconds: 20 },
};

/**
 * The hysteresis band under the shedding margin: a shed session lands only on a subscription member below
 * `margin - MIGRATE_BAND`; an Anthropic-key member's bucket reads against 1.0.
 */
export const MIGRATE_BAND = 0.05;

/** The router's reading: a window's utilization less the share of it that has elapsed; 0 elapsed without a reset. */
export function aheadOf(window: { resetsAt: string | null; utilization: number }, lengthMs: number, now: number): number {
  if (window.resetsAt === null) return window.utilization;
  // Past its reset a window reads fully elapsed, where the router reads its own use since: an accepted approximation.
  const elapsed = (lengthMs - (Date.parse(window.resetsAt) - now)) / lengthMs;
  return window.utilization - Math.min(1, Math.max(0, elapsed));
}

// A share the store kept as a REAL can read back a rounding off the literal it was saved from.
const near = (a: number, b: number) => Math.abs(a - b) < 1e-9;
const cmp = (a: string, b: string) => (a < b ? -1 : a > b ? 1 : 0);

export function sheddingPresetOf(p: Pick<Pool, 'burnWindowSeconds' | 'shedShare' | 'sourceDwellSeconds'>): SheddingPreset {
  const names = Object.keys(SHEDDING_PRESETS) as Exclude<SheddingPreset, 'custom'>[];
  return (
    names.find((name) => {
      const v = SHEDDING_PRESETS[name];
      return v.burnWindowSeconds === p.burnWindowSeconds && near(v.shedShare, p.shedShare) && v.sourceDwellSeconds === p.sourceDwellSeconds;
    }) ?? 'custom'
  );
}

/** All three values must match a preset; balanced and conservative differ in the retry-in-place bound alone. */
export function failoverPresetOf(p: Pick<Pool, 'inFlightCap' | 'paceFloor' | 'retryInPlaceSeconds'>): FailoverPreset {
  const names = Object.keys(FAILOVER_PRESETS) as Exclude<FailoverPreset, 'custom'>[];
  return (
    names.find((name) => {
      const v = FAILOVER_PRESETS[name];
      return v.inFlightCap === p.inFlightCap && near(v.paceFloor, p.paceFloor) && v.retryInPlaceSeconds === p.retryInPlaceSeconds;
    }) ?? 'custom'
  );
}

/** The round's marking rule, the mirror of the router's: pins in the round's order with their decayed burns,
 *  marked until the marked burn reaches `target`, never the last unmarked pin, never an unpriced pin, none for a
 *  zero target. */
export function shedMarks(ordered: { id: string; burn: number | null }[], target: number): string[] {
  const marks: string[] = [];
  if (Number.isNaN(target) || target <= 0) return marks;
  let marked = 0;
  for (const pin of ordered) {
    // An indivisible pin may carry the marked burn past the target, and the round stops there.
    if (marked >= target || marks.length + 1 >= ordered.length) break;
    // An unpriced burn moves the marked sum nowhere, so marking the pin would shed a session on a guess: it stays,
    // and it still counts as one of the account's pins for the last-pin rule.
    if (pin.burn === null) continue;
    marks.push(pin.id);
    marked += pin.burn;
  }
  return marks;
}

/** A burn average decayed from its last fold to `now`, as the router reads it; one never folded reads as stored. */
function burnDecayed(burn: number, burnAt: string | null, now: number, windowMs: number): number {
  if (burnAt === null) return burn;
  // A fold dated after `now` (another clock) decays nothing rather than growing the burn.
  return burn * Math.exp(-Math.max(now - Date.parse(burnAt), 0) / Math.max(windowMs, 1));
}

export interface ShedPreview {
  /** The members that ran a round burn in different units, so `burnShare` is the mean of their own shares. */
  averaged: boolean;
  /** The marked burn over the burn of the members that ran a round; 0 when they burn nothing. */
  burnShare: number;
  /**
   * Whether an eligible member reads cool (a subscription member under the band, an Anthropic-key member with room in
   * its bucket), where the router lands a marked session; without one the marks stand and nothing moves. The router
   * also passes over a member at its in-flight cap or one any pool shed from within the dwell, which the pool view
   * does not carry.
   */
  cool: boolean;
  /** Members whose 5-hour or 7-day window runs ahead of its pace by the shedding margin, each of which runs a round. */
  members: number;
  sessions: number;
  /** The unit every replayed member's burn is in; null when none has a pin or two members differ. */
  unit: BurnUnit | null;
  /** Pins whose burn the table cannot price, left in the order unmarked. */
  unpriced: number;
  warm: number;
}

/**
 * The shedding round's dry run: how many pins would be marked, how many of them warm, and their share of the burn.
 * Each member ahead of its pace by the shedding margin replays its round over the pool's pins on it: `sessions` is the
 * pool's slice, the sessions under its keys, which the caller filters, since the router's round marks the triggering
 * pool's pins alone. The round decays each burn to `now`, as the router's `burn_decayed` does, and orders cold pins
 * first by burn, then warm ones by burn over their last input size, bounded by the warm-sessions mode. A member's
 * pins share one unit, so its round needs no conversion; members in different units cannot be summed, so their shares
 * are averaged instead. A member without `eligible` counts as eligible, as a member of a pool not saved yet does.
 * `sized` names the accounts whose tier the cost table sizes.
 */
export function shedPreview(
  pool: Pick<Pool, 'burnWindowSeconds' | 'shedAheadOfPace' | 'shedShare' | 'warmSessions'> & { members: { accountId: string; eligible?: MemberEligibility }[] },
  accounts: Account[],
  sessions: Session[],
  now: number,
  sized: ReadonlySet<string>,
): ShedPreview {
  const threshold = pool.shedAheadOfPace;
  if (threshold === null) return { averaged: false, burnShare: 0, cool: false, members: 0, sessions: 0, unit: null, unpriced: 0, warm: 0 };
  const byId = new Map(accounts.map((a) => [a.id, a]));
  const band = threshold - MIGRATE_BAND;
  const lead = (a: Account, id: '5h' | '7d') => {
    const w = windowOf(a, id);
    return w === null ? null : aheadOf(w, WINDOW_HOURS[id] * 3_600_000, now);
  };
  // The router's destination readings: a subscription account's two shared windows by their lead over pace against the
  // band, an Anthropic-key account's worst live bucket against 1.0 (a bucket refills within its minute); a window the
  // row does not carry reads as under.
  const under = (a: Account) =>
    a.kind === 'api_key'
      ? Math.max(0, ...a.windows.filter((w) => w.resetsAt === null || Date.parse(w.resetsAt) > now).map((w) => w.utilization)) < 1
      : (['5h', '7d'] as const).every((id) => (lead(a, id) ?? Number.NEGATIVE_INFINITY) < band);
  const cool = pool.members.some((m) => {
    const account = byId.get(m.accountId);
    return account !== undefined && (m.eligible ?? 'eligible') === 'eligible' && under(account);
  });
  const hot = pool.members
    .map((m) => byId.get(m.accountId))
    .filter((a): a is Account => a !== undefined && (['5h', '7d'] as const).some((id) => (lead(a, id) ?? Number.NEGATIVE_INFINITY) >= threshold));
  const windowMs = pool.burnWindowSeconds * 1_000;
  const rounds = hot.map((account) => {
    const pins = sessions
      // The route reads no burn for a window-share pin of an account whose tier the table does not size, while the
      // round prices it on the reference tier and sheds it; the preview cannot replay that order, so it leaves those
      // pins out rather than count them as pins the table cannot price.
      .filter((s) => s.pinned && s.accountId === account.id && (s.burnUnit !== 'window_share' || sized.has(account.id)))
      // The router walks an account's pins in key, session and model order, which settles ties in the sorts below.
      .sort((a, b) => cmp(a.keyId, b.keyId) || cmp(a.sessionId, b.sessionId) || cmp(a.model ?? '', b.model ?? ''))
      .map((s) => ({
        burn: s.burn === null ? null : burnDecayed(s.burn, s.burnAt, now, windowMs),
        id: `${s.keyId}/${s.sessionId}/${s.model ?? ''}`,
        // A pin with no prompt size reported yet ranks as the cheapest rewrite.
        rewrite: Math.max(s.lastInputTokens ?? 0, 1),
        unit: s.burnUnit,
        warm: s.lastRefreshAt !== null && s.cacheTtlMs !== null && now - Date.parse(s.lastRefreshAt) < s.cacheTtlMs,
      }));
    type Pin = (typeof pins)[number];
    // A pin whose burn cannot be priced ranks after every priced pin of its group, as the router's round ranks it.
    const pricedFirst = (a: number | null, b: number | null) => (a === null ? (b === null ? 0 : 1) : b === null ? -1 : b - a);
    const perRewrite = (p: Pin) => (p.burn === null ? null : p.burn / p.rewrite);
    const byRewrite = (a: Pin, b: Pin) => pricedFirst(perRewrite(a), perRewrite(b));
    const cold = pins.filter((p) => !p.warm).sort((a, b) => pricedFirst(a.burn, b.burn));
    const warm = pins.filter((p) => p.warm).sort(byRewrite);
    const ordered = pool.warmSessions === 'always' ? [...cold, ...warm].sort(byRewrite) : [...cold, ...warm];
    const total = pins.reduce((s, p) => s + (p.burn ?? 0), 0);
    let marks = shedMarks(ordered, pool.shedShare * total);
    // The warm pins stay in the order under `never`, so the last-pin rule still counts them among the member's pins;
    // the marks are priced pins alone, so the cold group's marks are its priced count at most, as the router's
    // `cold_priced` truncation reads them.
    if (pool.warmSessions === 'never') marks = marks.slice(0, cold.filter((p) => p.burn !== null).length);
    const marked = pins.filter((p) => marks.includes(p.id));
    return {
      burn: marked.reduce((s, p) => s + (p.burn ?? 0), 0),
      sessions: marked.length,
      total,
      unit: pins[0]?.unit ?? null,
      unpriced: pins.filter((p) => p.burn === null).length,
      warm: marked.filter((p) => p.warm).length,
    };
  });
  const units = new Set(rounds.flatMap((r) => (r.unit === null ? [] : [r.unit])));
  const burning = rounds.filter((r) => r.total > 0);
  const total = burning.reduce((s, r) => s + r.total, 0);
  const averaged = units.size > 1;
  const burnShare = burning.length === 0 ? 0 : averaged ? burning.reduce((s, r) => s + r.burn / r.total, 0) / burning.length : burning.reduce((s, r) => s + r.burn, 0) / total;
  return {
    averaged,
    burnShare,
    cool,
    members: hot.length,
    sessions: rounds.reduce((s, r) => s + r.sessions, 0),
    unit: units.size === 1 ? [...units][0] : null,
    unpriced: rounds.reduce((s, r) => s + r.unpriced, 0),
    warm: rounds.reduce((s, r) => s + r.warm, 0),
  };
}

/**
 * The shedding card's dry run in one sentence, naming the unit its share was taken in. Until the cost table is read no
 * window-share pin enters the round, so a count then would read short; `table` says whether it has been read.
 */
export function shedPreviewLine(preview: ShedPreview, shedAheadOn: boolean, table: 'loading' | 'missing' | 'ready' = 'ready'): string {
  if (!shedAheadOn) return 'No round runs while shed ahead of pace is off.';
  if (table === 'loading') return 'The cost table is loading; window-share pins are counted once it is read.';
  if (table === 'missing') return 'The cost table did not load, so window-share pins cannot be counted.';
  if (preview.members === 0) return 'No member is ahead of pace by the margin.';
  const of = preview.averaged
    ? "of each account's burn on average"
    : preview.unit === 'window_share'
      ? 'of the burn in window share'
      : preview.unit === 'usd'
        ? 'of the burn in dollars'
        : 'of the burn';
  const across = preview.members > 1 ? `, across ${preview.members} accounts` : '';
  const what = `${preview.sessions} ${preview.sessions === 1 ? 'session' : 'sessions'}, ${preview.warm} warm, ${fmtPct(preview.burnShare)} ${of}${across}`;
  const n = preview.unpriced;
  const unpriced = n > 0 ? ` ${n} pin${n === 1 ? '' : 's'} the table cannot price stay${n === 1 ? 's' : ''}.` : '';
  return `${preview.cool ? `Would move ${what}.` : `Would mark ${what}, but no member reads under the band to take them.`}${unpriced}`;
}
