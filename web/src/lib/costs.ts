import type { SubscriptionCosts, TypeMultiplier } from '@/api/types';
import { undatedModel } from '@/lib/format';

/**
 * The share rule of the subscription cost model, the console's one copy of what the ledger prices: a token costs the
 * model's weight times its type's multiplier in units, and a share is those units over the tier's capacity on the
 * window, over the peak band's capacity factor inside the band and a model week's divisor.
 */

export type CostWindow = 'five_hour' | 'seven_day';
export type TokenType = keyof TypeMultiplier;
export interface Usage { cacheRead: number; cacheWrite1h: number; cacheWrite5m: number; input: number; output: number }

const TOKEN_TYPES: TokenType[] = ['cacheRead', 'cacheWrite1h', 'cacheWrite5m', 'input', 'output'];

// A local copy rather than `lib/analytics`'s, which reads `tierRow` from this module: the two would import each other.
const own = (map: object, key: string): boolean => Object.prototype.hasOwnProperty.call(map, key);

export const anchorTier = (costs: SubscriptionCosts): string => costs.unit.definedBy.tier;

/** The tier's capacity row as stored, then with `default_` taken off or put on; undefined for a tier the model lacks. */
export const tierRow = (costs: SubscriptionCosts, tier: string): SubscriptionCosts['capacity'][string] | undefined => {
  const bare = tier.startsWith('default_') ? tier.slice('default_'.length) : null;
  const key = [tier, bare, `default_${tier}`].find((k): k is string => k !== null && own(costs.capacity, k));
  return key === undefined ? undefined : costs.capacity[key];
};

export const tierCapacity = (costs: SubscriptionCosts, tier: string, window: CostWindow): number | null => tierRow(costs, tier)?.[window === 'five_hour' ? 'fiveHour' : 'sevenDay'] ?? null;

/** The shared window `window` prices on and the model-week divisor, as the ledger reads it; null for no plan window. */
export const pricedWindow = (costs: SubscriptionCosts, window: string): { divisor: number; priced: CostWindow } | null => {
  if (window === 'five_hour' || window === 'seven_day') return { divisor: 1, priced: window };
  if (own(costs.modelWindows, window)) {
    const row = costs.modelWindows[window];
    return row.of === 'seven_day' ? { divisor: row.capacityFactor, priced: 'seven_day' } : null;
  }
  // Any other week prices as the 7-day share once, as the ledger's window claims allow.
  return window.startsWith('seven_day') ? { divisor: 1, priced: 'seven_day' } : null;
};

/** The model's row key: its id as sent, else its undated name, since the price rows carry dated ids. */
const keyOf = (costs: SubscriptionCosts, model: string): string | null => (own(costs.modelWeight, model) ? model : own(costs.modelWeight, undatedModel(model)) ? undatedModel(model) : null);

export const weightOf = (costs: SubscriptionCosts, model: string, window: string): number | null => {
  const key = keyOf(costs, model);
  if (key === null) return null;
  const byWindow = costs.overrides[key]?.weightByWindow;
  return byWindow !== undefined && own(byWindow, window) ? byWindow[window] : costs.modelWeight[key];
};

const multipliersOf = (costs: SubscriptionCosts, model: string): TypeMultiplier => {
  const key = keyOf(costs, model);
  return { ...costs.typeMultiplier, ...(key === null ? {} : costs.overrides[key]?.typeMultiplier ?? {}) };
};

/** Whether `at` falls inside the peak band's hours in its zone, on a weekday when the band covers weekdays alone. */
export const inBand = (costs: SubscriptionCosts, at: number): boolean => {
  const { fromHour, toHour, weekdays, zone } = costs.peak;
  if (zone === '') return false;
  let parts: Intl.DateTimeFormatPart[];
  try {
    parts = new Intl.DateTimeFormat('en-US', { hour: 'numeric', hourCycle: 'h23', timeZone: zone, weekday: 'short' }).formatToParts(at);
  } catch {
    // A zone the browser does not know prices out of band, as the ledger reads a zone it cannot resolve.
    return false;
  }
  const hour = Number(parts.find((p) => p.type === 'hour')?.value);
  const weekend = ['Sat', 'Sun'].includes(parts.find((p) => p.type === 'weekday')?.value ?? '');
  return (!weekdays || !weekend) && fromHour <= hour && hour < toHour;
};

/** The weight the band puts on a charge landing on `window` at `at`: the reciprocal of the priced window's capacity
 *  factor inside the band, else 1. */
export const bandFactor = (costs: SubscriptionCosts, window: string, at: number): number => {
  const w = pricedWindow(costs, window);
  if (w === null || !inBand(costs, at)) return 1;
  return 1 / costs.peak.capacityFactor[w.priced === 'five_hour' ? 'fiveHour' : 'sevenDay'];
};

/** Per million tokens of each type, the share of the tier's window out of band; null for an unpriced model or tier. */
export const ratesOf = (costs: SubscriptionCosts, window: string, tier: string, model: string): Record<TokenType, number> | null => {
  const w = pricedWindow(costs, window);
  const weight = weightOf(costs, model, window);
  const capacity = w === null ? null : tierCapacity(costs, tier, w.priced);
  if (w === null || weight === null || capacity === null) return null;
  const m = multipliersOf(costs, model);
  const per = (k: TokenType) => (weight * m[k] * 1_000_000) / capacity / w.divisor;
  return { cacheRead: per('cacheRead'), cacheWrite1h: per('cacheWrite1h'), cacheWrite5m: per('cacheWrite5m'), input: per('input'), output: per('output') };
};

/** The share of the tier's own window `usage` spends for a request completing at `at`; null where unpriced. */
export const shareOf = (costs: SubscriptionCosts, window: string, tier: string, model: string, usage: Usage, at: number): number | null => {
  const rates = ratesOf(costs, window, tier, model);
  if (rates === null) return null;
  const units = TOKEN_TYPES.reduce((s, k) => s + (usage[k] * rates[k]) / 1_000_000, 0);
  return units * bandFactor(costs, window, at);
};
