import type { CacheMissReason, WastedProbeReason } from '@/api/types';

/** Reading order for the panels: the no-divergence answer, the diagnosed divergences, then the two non-answers. */
export const CACHE_MISS_REASONS: CacheMissReason[] = ['none', 'model_changed', 'system_changed', 'tools_changed', 'messages_changed', 'previous_message_not_found', 'unavailable', 'not_requested'];

export const CACHE_MISS_LABEL: Record<CacheMissReason, string> = {
  messages_changed: 'History edited',
  model_changed: 'Model changed',
  none: 'No divergence',
  not_requested: 'Not requested',
  previous_message_not_found: 'Previous message not found',
  system_changed: 'System prompt changed',
  tools_changed: 'Tools changed',
  unavailable: 'Unavailable',
};

// The route passes any name the beta sent, so a lookup by that name sees own keys only: `__proto__` or
// `constructor` would otherwise answer a member every object inherits. The lib target predates `Object.hasOwn`.
const hasOwn = (o: object, key: string) => Object.prototype.hasOwnProperty.call(o, key);

/** A reason the console has no label for reads under the name the beta gave it. */
export const missLabel = (reason: CacheMissReason): string => (hasOwn(CACHE_MISS_LABEL, reason) ? CACHE_MISS_LABEL[reason] : reason);

export const missCount = (reasons: Record<CacheMissReason, number>, reason: CacheMissReason): number => (hasOwn(reasons, reason) ? reasons[reason] : 0);

/** The columns a breakdown draws: the known reasons in reading order, then every other name the groups carry, alphabetically. */
export function missReasonOrder(groups: readonly { cacheMissReasons: Record<CacheMissReason, number> }[]): CacheMissReason[] {
  const others = new Set<CacheMissReason>();
  for (const g of groups) for (const reason of Object.keys(g.cacheMissReasons)) if (!CACHE_MISS_REASONS.includes(reason)) others.add(reason);
  return [...CACHE_MISS_REASONS, ...[...others].sort()];
}

/** The `*_changed` reasons are the diagnosed divergences, each naming the part of the prompt the prefix broke at. */
export const isDivergence = (reason: CacheMissReason) => reason.endsWith('_changed');

export const WASTED_PROBE_REASONS: WastedProbeReason[] = ['cold_return', 'evicted', 'expired', 'horizon', 'idle_limit', 'parked_limit', 'released', 'replaced', 'session_ended'];

export const WASTED_PROBE_LABEL: Record<WastedProbeReason, string> = {
  cold_return: 'Returned cold',
  evicted: 'Evicted at the byte cap',
  expired: 'Entry expired',
  horizon: 'Horizon held the recipe',
  idle_limit: 'Idle limit ran out',
  parked_limit: 'Parked limit ran out',
  released: 'Released by an operator',
  replaced: 'Replaced by a new registration',
  session_ended: 'Session ended',
};

export const emptyWastedProbeReasons = (): Record<WastedProbeReason, number> => ({ cold_return: 0, evicted: 0, expired: 0, horizon: 0, idle_limit: 0, parked_limit: 0, released: 0, replaced: 0, session_ended: 0 });

/**
 * Seeds the known reasons at zero; a sum adds any other name it meets beside them. The map has no prototype, so
 * a name such as `__proto__` or `constructor` is a key like any other: it neither reads an inherited member as
 * its count nor, assigned, replaces the map's prototype.
 */
export const emptyCacheMissReasons = (): Record<CacheMissReason, number> =>
  Object.assign(Object.create(null) as Record<CacheMissReason, number>, {
    messages_changed: 0,
    model_changed: 0,
    none: 0,
    not_requested: 0,
    previous_message_not_found: 0,
    system_changed: 0,
    tools_changed: 0,
    unavailable: 0,
  });
