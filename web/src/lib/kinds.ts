import type { DecisionKind } from '@/api/types';

// The routing log's kind tables, in a module that imports none of the console's own, so the address codecs read them
// without importing routingLog.ts, which imports the codecs.

export const DECISION_KIND_LABEL: Record<DecisionKind, string> = {
  exhausted: 'exhausted',
  held: 'held',
  moved: 'moved',
  placed: 'placed',
  refused: 'refused',
  renewed: 'renewed',
};

/** Every kind of the ledger's events table, in the console's words. */
export const EVENT_KIND_LABEL: Record<string, string> = {
  account_state_changed: 'account state changed',
  budget_period_ended: 'budget period ended',
  budget_reached: 'budget reached',
  catalog_changed: 'catalog changed',
  cooldown_cleared: 'cooldown cleared',
  cooldown_set: 'cooldown set',
  fast_credits_changed: 'fast credits changed',
  ledger_gap: 'ledger gap',
  limit_reset_claimed: 'limit reset claimed',
  mark_cleared: 'mark cleared',
  model_permitted: 'model permitted',
  model_rejected: 'model rejected',
  overage_ended: 'overage ended',
  overage_started: 'overage started',
  pin_created: 'pin created',
  pin_expired: 'pin expired',
  pin_migrated: 'pin moved',
  pin_released: 'pin released',
  plan_ended: 'plan ended',
  plan_restored: 'plan restored',
  poll_backoff_ended: 'poll backoff ended',
  poll_backoff_started: 'poll backoff started',
  poll_failed: 'poll failed',
  refresh_failed: 'refresh failed',
  refresh_succeeded: 'refresh succeeded',
  refresh_uncertain: 'refresh uncertain',
  registry_changed: 'registry changed',
  reservation_expired: 'reservation expired',
  router_started: 'router started',
  shedding_round: 'shedding round',
  slow_down_ended: 'slow-down ended',
  slow_down_started: 'slow-down started',
  stream_interrupted: 'stream interrupted',
  warm_prefix_expired: 'warm prefix expired',
  wind_down_started: 'wind-down started',
  window_cleared: 'window cleared',
  window_reset: 'window reset',
  window_spent: 'window spent',
};

/** Every kind the Kinds list offers: the decision kinds, `probe` and the event kinds. */
export const ALL_KINDS: readonly string[] = [...Object.keys(DECISION_KIND_LABEL), 'probe', ...Object.keys(EVENT_KIND_LABEL)];
