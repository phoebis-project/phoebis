import type { RecipeReason } from '@/api/types';

export const REASON_LABEL: Record<RecipeReason, string> = {
  affinity_too_short: 'affinity too short',
  awaiting_cache_evidence: 'awaiting cache evidence',
  budget: 'at budget',
  cap_reached: 'cap reached',
  cold_probes: 'paused: cold probes',
  cooled: 'cooled down',
  idle_limit: 'idle limit',
  net_negative: 'net negative',
  oauth_unverified: 'subscription probes off',
  overage: 'overage',
  paused: 'paused',
  pool_exhausted: 'pool exhausted',
  probe_error: 'probe error',
  quota_exhausted: 'quota exhausted',
  reauth: 'needs re-auth',
  retrying: 'retrying',
  storage_limit: 'storage limit',
  unsupported_shape: 'unsupported shape',
};

export const REASON_HINT: Record<RecipeReason, string> = {
  affinity_too_short: "The pool's session affinity expires before the cache this probe would warm, so the session could move away from it.",
  awaiting_cache_evidence: "The agent's request went out and its stream has not yet reported cache usage. The first stream event carries it, within seconds; the probe timer arms from that event and probes then run while the response is still streaming, so a long turn does not go cold.",
  budget: "The pinned member's spend reached its budget; probes wait for the period to end or the budget to change.",
  cap_reached: 'The pool spent its daily probe cap, or a subscription upstream spent its share of the 5-hour window; probes resume at midnight UTC.',
  cold_probes: "The probe read no cache for the prefix it was meant to keep warm, so that entry had already expired and the next turn writes regardless. Consecutive cold probes past the pool's cold probe limit pause the recipe until the next real request.",
  cooled: 'The upstream the session is pinned to is under a cooldown; the probe waits for it to end.',
  idle_limit: "The pool's max idle time or idle probe budget ran out since the last real request, so the retained prompt was discarded. The next real request starts a new recipe.",
  net_negative: "Probes on this session have cost more than the cache reads they bought. The recipe gets the pool's grace probes after each real request and then holds until a request lands warm again.",
  oauth_unverified: 'Probes on subscription upstreams stay off until the expiry-crossing experiment has passed and oauth_prewarm_verified is set in the bootstrap file.',
  overage: 'The upstream is on extra usage and the pool policy skips probes there.',
  paused: 'The upstream is paused, the pool is disabled, or the key opted out, so nothing probes this session.',
  pool_exhausted: 'Every account of the pool was exhausted or cooled when the probe was selected; the recipe resumes with the next real request.',
  probe_error: 'The last probe failed on the transport or was refused, and the recipe holds until the next real request.',
  quota_exhausted: "The upstream's usage window is spent; probes resume when it resets.",
  reauth: 'The upstream needs re-pairing or funding before anything can be sent on it.',
  retrying: 'The last probe failed with a retryable error; the next attempt is scheduled with backoff.',
  storage_limit: 'The engine is at its retained-request or session limit, so this recipe was not kept.',
  unsupported_shape: 'The last request had no cacheable prefix the probe could replay.',
};

export const WARNING_REASONS: RecipeReason[] = ['affinity_too_short', 'budget', 'cap_reached', 'idle_limit', 'net_negative', 'oauth_unverified', 'storage_limit', 'unsupported_shape'];
