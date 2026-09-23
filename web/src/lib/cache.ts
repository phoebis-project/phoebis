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

/** A reason the console has no label for reads under the name the beta gave it. */
export const missLabel = (reason: CacheMissReason): string => CACHE_MISS_LABEL[reason] ?? reason;

/** The columns a breakdown draws: the known reasons in reading order, then every other name the groups carry, alphabetically. */
export function missReasonOrder(groups: readonly { cacheMissReasons: Record<CacheMissReason, number> }[]): CacheMissReason[] {
  const others = new Set<CacheMissReason>();
  for (const g of groups) for (const reason of Object.keys(g.cacheMissReasons)) if (!CACHE_MISS_REASONS.includes(reason)) others.add(reason);
  return [...CACHE_MISS_REASONS, ...[...others].sort()];
}

/** The `*_changed` reasons are the diagnosed divergences, each naming the part of the prompt the prefix broke at. */
export const isDivergence = (reason: CacheMissReason) => reason.endsWith('_changed');

export const WASTED_PROBE_REASONS: WastedProbeReason[] = ['idle_limit', 'released', 'replaced', 'session_ended'];

export const WASTED_PROBE_LABEL: Record<WastedProbeReason, string> = {
  idle_limit: 'Idle limit ran out',
  released: 'Released by an operator',
  replaced: 'Replaced by a new registration',
  session_ended: 'Session ended',
};

export const emptyWastedProbeReasons = (): Record<WastedProbeReason, number> => ({ idle_limit: 0, released: 0, replaced: 0, session_ended: 0 });

/** Seeds the known reasons at zero; a sum adds any other name it meets beside them. */
export const emptyCacheMissReasons = (): Record<CacheMissReason, number> => ({
  messages_changed: 0,
  model_changed: 0,
  none: 0,
  not_requested: 0,
  previous_message_not_found: 0,
  system_changed: 0,
  tools_changed: 0,
  unavailable: 0,
});
