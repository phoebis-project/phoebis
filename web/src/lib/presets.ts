import type { Account, BurnUnit, Pool, Session } from '@/api/types';
import { windowOf } from './quota';

export type SheddingPreset = 'balanced' | 'conservative' | 'custom' | 'fast';

/** The store's default is `balanced`. */
export const SHEDDING_PRESETS: Record<Exclude<SheddingPreset, 'custom'>, { burnWindowSeconds: number; shedShare: number; sourceDwellSeconds: number }> = {
  balanced: { burnWindowSeconds: 600, shedShare: 0.5, sourceDwellSeconds: 900 },
  conservative: { burnWindowSeconds: 900, shedShare: 0.3, sourceDwellSeconds: 1_800 },
  fast: { burnWindowSeconds: 300, shedShare: 0.5, sourceDwellSeconds: 600 },
};

export type FailoverPreset = 'balanced' | 'conservative' | 'custom' | 'fast';

/** balanced 16, 10 s, 5 % (the default); fast 8, 5 s, 10 %; conservative 16, 20 s, 5 %. */
export const FAILOVER_PRESETS: Record<Exclude<FailoverPreset, 'custom'>, { inFlightCap: number; paceFloor: number; retryInPlaceSeconds: number }> = {
  balanced: { inFlightCap: 16, paceFloor: 0.05, retryInPlaceSeconds: 10 },
  conservative: { inFlightCap: 16, paceFloor: 0.05, retryInPlaceSeconds: 20 },
  fast: { inFlightCap: 8, paceFloor: 0.1, retryInPlaceSeconds: 5 },
};

/** The hysteresis band under the migrate threshold: a shed session lands only on a member below `threshold - MIGRATE_BAND`. */
export const MIGRATE_BAND = 0.05;

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
 *  marked until the marked burn reaches `target`, never the last unmarked pin, none for a zero target. */
export function shedMarks(ordered: { id: string; burn: number }[], target: number): string[] {
  const marks: string[] = [];
  if (Number.isNaN(target) || target <= 0) return marks;
  let marked = 0;
  for (const pin of ordered) {
    // An indivisible pin may carry the marked burn past the target, and the round stops there.
    if (marked >= target || marks.length + 1 >= ordered.length) break;
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
  /** The marked burn over the burn of the members that ran a round; 0 when they burn nothing. */
  burnShare: number;
  /** Members at or above the migrate threshold on either window, each of which runs a round. */
  members: number;
  sessions: number;
  /** The unit every replayed member's burn is in; null when none has a pin or two members differ. */
  unit: BurnUnit | null;
  warm: number;
}

/**
 * The shedding round's dry run: how many pins would be marked, how many of them warm, and their share of the burn.
 * Each member at or above the migrate threshold replays its round over its pins with each burn decayed to `now`,
 * as the router's `burn_decayed` does: cold pins first by burn, then warm ones by burn over their last input size,
 * bounded by the warm-sessions mode. A member's pins share one unit, so its round needs no conversion; members in
 * different units cannot be summed, so their shares are averaged instead.
 */
export function shedPreview(
  pool: Pick<Pool, 'burnWindowSeconds' | 'migrateAboveUtilization' | 'shedShare' | 'warmSessions'> & { members: { accountId: string }[] },
  accounts: Account[],
  sessions: Session[],
  now: number,
): ShedPreview {
  const threshold = pool.migrateAboveUtilization;
  if (threshold === null) return { burnShare: 0, members: 0, sessions: 0, unit: null, warm: 0 };
  const byId = new Map(accounts.map((a) => [a.id, a]));
  const hot = pool.members
    .map((m) => byId.get(m.accountId))
    .filter((a): a is Account => a !== undefined && (['5h', '7d'] as const).some((id) => (windowOf(a, id)?.utilization ?? -1) >= threshold));
  const windowMs = pool.burnWindowSeconds * 1_000;
  const rounds = hot.map((account) => {
    const pins = sessions
      .filter((s) => s.pinned && s.accountId === account.id && s.burn !== null)
      // The router walks an account's pins in key, session and model order, which settles ties in the sorts below.
      .sort((a, b) => cmp(a.keyId, b.keyId) || cmp(a.sessionId, b.sessionId) || cmp(a.model ?? '', b.model ?? ''))
      .map((s) => ({
        burn: burnDecayed(s.burn ?? 0, s.burnAt, now, windowMs),
        id: `${s.keyId}/${s.sessionId}/${s.model ?? ''}`,
        // A pin with no prompt size reported yet ranks as the cheapest rewrite.
        rewrite: Math.max(s.lastInputTokens ?? 0, 1),
        unit: s.burnUnit,
        warm: s.lastRefreshAt !== null && s.cacheTtlMs !== null && now - Date.parse(s.lastRefreshAt) < s.cacheTtlMs,
      }));
    const perRewrite = (p: (typeof pins)[number]) => p.burn / p.rewrite;
    const cold = pins.filter((p) => !p.warm).sort((a, b) => b.burn - a.burn);
    const warm = pins.filter((p) => p.warm).sort((a, b) => perRewrite(b) - perRewrite(a));
    const ordered = pool.warmSessions === 'always' ? [...cold, ...warm].sort((a, b) => perRewrite(b) - perRewrite(a)) : [...cold, ...warm];
    const total = pins.reduce((s, p) => s + p.burn, 0);
    let marks = shedMarks(ordered, pool.shedShare * total);
    // The warm pins stay in the order under `never`, so the last-pin rule still counts them among the member's pins.
    if (pool.warmSessions === 'never') marks = marks.slice(0, cold.length);
    const marked = pins.filter((p) => marks.includes(p.id));
    return { burn: marked.reduce((s, p) => s + p.burn, 0), sessions: marked.length, total, unit: pins[0]?.unit ?? null, warm: marked.filter((p) => p.warm).length };
  });
  const units = new Set(rounds.flatMap((r) => (r.unit === null ? [] : [r.unit])));
  const burning = rounds.filter((r) => r.total > 0);
  const total = burning.reduce((s, r) => s + r.total, 0);
  const burnShare = burning.length === 0 ? 0 : units.size <= 1 ? burning.reduce((s, r) => s + r.burn, 0) / total : burning.reduce((s, r) => s + r.burn / r.total, 0) / burning.length;
  return {
    burnShare,
    members: hot.length,
    sessions: rounds.reduce((s, r) => s + r.sessions, 0),
    unit: units.size === 1 ? [...units][0] : null,
    warm: rounds.reduce((s, r) => s + r.warm, 0),
  };
}
