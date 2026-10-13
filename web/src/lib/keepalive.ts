import type { KeepaliveRecipe, KeepaliveState, RecipeCause } from '@/api/types';
import { RANGE_PRESETS } from '@/lib/address';

export const CAUSE_LABEL: Record<RecipeCause, string> = {
  affinity_too_short: 'affinity too short',
  agent_released: 'agent released',
  agent_unknown: 'agent unknown',
  awaiting_cache_evidence: 'awaiting cache evidence',
  awaiting_request: 'awaiting request',
  awaiting_selection: 'awaiting selection',
  budget: 'at budget',
  bus_gap: 'bus gap',
  cooled: 'cooled down',
  evicted: 'evicted',
  expired: 'entry expired',
  expired_at_restart: 'expired at restart',
  fixed_thinking: 'fixed thinking',
  forced_tool: 'forced tool',
  headers_too_large: 'headers too large',
  horizon: 'wake too far',
  idle_limit: 'idle limit',
  ineligible_credential: 'ineligible credential',
  instance_replaced: 'instance replaced',
  invalid_cache_control: 'invalid cache control',
  invalid_model: 'invalid model',
  invalid_probe_usage: 'invalid probe usage',
  invalid_request: 'invalid request',
  keepalive_off: 'keepalive off',
  key_opted_out: 'key opted out',
  lease_expired: 'lease expired',
  ledger_behind: 'ledger behind',
  missing_account: 'missing account',
  no_cache_evidence: 'no cache evidence',
  no_probe_cache_evidence: 'no probe cache evidence',
  opted_out: 'opted out',
  overage: 'overage',
  owner_mismatch: 'owner mismatch',
  parked_limit: 'parked limit',
  partial_cache_coverage: 'partial cache coverage',
  paused: 'account paused',
  pool_disabled: 'pool disabled',
  probe_authentication_error: 'probe authentication error',
  probe_pool_exhausted: 'pool exhausted',
  probe_quota_exhausted: 'quota exhausted',
  probe_retry_limit: 'retry limit',
  probe_transport_error: 'transport error',
  probing: 'probing',
  reauth: 'needs re-auth',
  released: 'released',
  repeated_cold_probe: 'cold probes',
  request_canceled: 'request canceled',
  request_failed: 'request failed',
  request_limit: 'request limit',
  request_rejected: 'request rejected',
  request_too_large: 'request too large',
  reset: 'reset',
  retryable_probe_error: 'retrying',
  scheduled: 'scheduled',
  session_ended: 'session ended',
  session_expired: 'session expired',
  slow_probe: 'slow probe',
  storage_limit: 'storage limit',
  structured_output: 'structured output',
  unexpected_output: 'unexpected output',
  unsupported_probe: 'unsupported probe',
  unsupported_shape: 'unsupported shape',
};

export const CAUSE_HINT: Record<RecipeCause, string> = {
  affinity_too_short: "The pool's session affinity ends before this recipe's probe is due (the cache TTL less the lead), so the session could move away from the account the probe would warm.",
  agent_released: 'The keepalive mod reported this subagent finished, so its recipe was dropped.',
  agent_unknown: "The keepalive mod could not tell this agent's state, so the engine keeps no recipe for it.",
  awaiting_cache_evidence:
    "The agent's request went out and its stream has not yet reported cache usage. The first stream event carries it, within seconds. The probe timer arms from that event and probes then run while the response is still streaming, so a long turn does not go cold.",
  awaiting_request: 'No real request has reached the router from this agent since it started, so there is nothing to keep yet.',
  awaiting_selection: "The agent's request reached the router and waits for an account, and the recipe arms once one serves it.",
  budget: "The pinned member's spend reached its budget. Probes wait for the period to end or the budget to change while the entry lasts.",
  bus_gap: 'The keepalive engine missed bus events while this request was open, so it kept nothing from it. The next real request starts a new recipe.',
  cooled: 'The account the session is pinned to is under a cooldown. The probe waits for it to end while the entry lasts.',
  evicted: 'The engine was at its retained-bytes cap and this recipe was the oldest idle one. The next real request starts a new recipe.',
  expired: 'The cache entry expired before its probe was due, so no probe was sent. The next real request starts a new recipe.',
  expired_at_restart: 'The cache entry expired while the router was restarting, so the recipe was dropped when the router read it back. The next real request starts a new recipe.',
  fixed_thinking: 'The request asked for a fixed thinking budget, which a probe cannot replay, so it was not kept.',
  forced_tool: 'The request forced a tool choice, which a probe cannot replay, so it was not kept.',
  headers_too_large: "The request's headers were larger than a recipe may hold, so it was not kept.",
  horizon:
    "The session is parked on a scheduled wakeup further away than the pool's horizon, so the recipe is held. Probing resumes once the wake is within the horizon while the entry is still warm, and the recipe ends as expired when the entry's TTL passes.",
  idle_limit: "The pool's idle window, capped at the recipe's break-even, ran out with no real request, so the retained prompt was discarded. The next real request starts a new recipe.",
  ineligible_credential: 'The account this session would use left the registry or its subscription ended, so nothing is sent on it.',
  instance_replaced: "A new instance of the keepalive mod took this session over, so the old instance's recipes were dropped.",
  invalid_cache_control: "The request's cache markers could not be read, so it was not kept.",
  invalid_model: 'The request named a model a recipe cannot carry, so it was not kept.',
  invalid_probe_usage: "The last probe's answer carried no usage the engine could read.",
  invalid_request: 'The request body could not be replayed as sent, so it was not kept.',
  keepalive_off: "Cache warming is off on the session's pool.",
  key_opted_out: "The session's key opts out of cache warming, so its recipe was dropped.",
  lease_expired: 'The keepalive mod stopped renewing its lease. The recipe stays and is probed under the idle window from the last real request.',
  ledger_behind: "The router refused the probe while its ledger was catching up. The probe is sent again after the answer's wait.",
  missing_account: "The last probe's answer named no account, so the engine cannot tell where it landed.",
  no_cache_evidence: "The request's answer showed no cache read or write, so there was no entry to keep warm.",
  no_probe_cache_evidence: "The last probe's answer showed no cache read or write. The next attempt is scheduled with backoff.",
  opted_out: "The session's key opts out of cache warming, so the request was not kept.",
  overage: 'The account is on extra usage and the pool policy skips probes there.',
  owner_mismatch: 'The request named another owner than the one the keepalive mod registered, so the engine kept nothing from it.',
  parked_limit: "The pool's parked ceiling ran out while the session waited on a background command or a subagent, so the retained prompt was discarded. The next real request starts a new recipe.",
  partial_cache_coverage: 'The last probe read only part of the prefix and wrote the rest, so it did not keep the whole entry warm.',
  paused: 'The pinned account is paused or has left the registry, so nothing probes this session.',
  pool_disabled: "The session's pool is disabled. Probing resumes once it is enabled again while the entry lasts.",
  probe_authentication_error: 'The last probe was refused for its credentials, or its account needs re-pairing.',
  probe_pool_exhausted: 'Every account of the pool was exhausted or cooled when the probe was selected.',
  probe_quota_exhausted: "The account's usage window was spent when the last probe was sent.",
  probe_retry_limit: 'The last probes failed with retryable errors until the failure limit, so the engine stopped retrying.',
  probe_transport_error: 'The last probe failed on the transport before an answer came back.',
  probing: 'A probe is in flight right now.',
  reauth: 'The account needs re-pairing or funding before anything can be sent on it.',
  released: 'An operator released this session, so every recipe on it was dropped.',
  repeated_cold_probe:
    'The probe read no cache for the prefix it was meant to keep warm, so that entry had already expired and the next turn writes regardless. Consecutive cold probes past the cold probe limit pause the recipe until the next real request.',
  request_canceled: "The agent's request was canceled before it finished, so it left no evidence to arm a recipe on.",
  request_failed: "The agent's request failed, so it left no evidence to arm a recipe on.",
  request_limit: 'The engine was following as many open requests as its agent limit allows, so this one was not kept.',
  request_rejected: "The router rejected the agent's request, so it left nothing to keep.",
  request_too_large: 'The request body was larger than a recipe may hold, so it was not kept.',
  reset: 'The router could not open the prompt it saved for this recipe when it restarted, so the recipe was dropped. The next real request starts a new one.',
  retryable_probe_error: 'The last probe failed with a retryable error. The next attempt is scheduled with backoff.',
  scheduled: 'The next probe is scheduled before the cache entry expires.',
  session_ended: 'The session ended, by the mod reporting it or by its key being deleted, so its recipes were dropped.',
  session_expired: 'The session passed the session expiry with no request and no report.',
  slow_probe: 'The last probe took its whole refresh interval, so the next one waits out a backoff rather than starting at once.',
  storage_limit: 'The engine was at its retained-bytes cap with nothing idle to evict, so this recipe was not kept.',
  structured_output: 'The request asked for structured output, which a probe cannot replay, so it was not kept.',
  unexpected_output: "The last probe's answer carried output, which a probe never asks for.",
  unsupported_probe: "The upstream refused the last probe's shape.",
  unsupported_shape: 'The last request had no cacheable prefix the probe could replay.',
};

export const WARNING_CAUSES: RecipeCause[] = ['affinity_too_short', 'budget', 'evicted', 'expired', 'expired_at_restart', 'horizon', 'idle_limit', 'parked_limit', 'storage_limit', 'unsupported_shape'];

/** The engine's name for a session's main conversation. */
export const MAIN_AGENT = 'main';

/** The lifecycle words in the order a session's lines run after main. `unknown` sits with `parked`, and `released`
 * closes the list wherever a released record is still listed. */
const STATE_ORDER: Record<KeepaliveState, number> = { busy: 0, idle: 1, parked: 2, released: 3, unknown: 2 };

/** Newest last request first, an agent without one last. */
function byLastRequest(a: KeepaliveRecipe, b: KeepaliveRecipe): number {
  if (a.lastRequestAt === null || b.lastRequestAt === null) return a.lastRequestAt === b.lastRequestAt ? 0 : a.lastRequestAt === null ? 1 : -1;
  return Date.parse(b.lastRequestAt) - Date.parse(a.lastRequestAt);
}

/**
 * A session's agents in the order the Sessions view draws them: main first, then by lifecycle word, inside a word the
 * newest last request first, an agent without one last, and ties by agent id, so two reads of one list draw one
 * order. The list handed in is left as it is.
 */
export function orderRecipes(agents: readonly KeepaliveRecipe[]): KeepaliveRecipe[] {
  return [...agents].sort((a, b) => {
    if (a.agentId === MAIN_AGENT || b.agentId === MAIN_AGENT) return a.agentId === b.agentId ? 0 : a.agentId === MAIN_AGENT ? -1 : 1;
    return STATE_ORDER[a.agentState] - STATE_ORDER[b.agentState] || byLastRequest(a, b) || a.agentId.localeCompare(b.agentId);
  });
}

export type RequestsRange = (typeof RANGE_PRESETS)[number]['name'];

/**
 * A minute under each width: the request log measures its window from its own load, a moment after the click that
 * chose the range, so a last request right at a width's edge would fall out of the window it was picked for.
 */
const RANGE_HEADROOM_MS = 60_000;

/**
 * The narrowest of the request log's ranges that covers a session's last request, measured back from `now` (the
 * instant of the click, never a sampled one) with the headroom, or null past the widest range or without a last
 * request, where the page's own default applies.
 */
export function requestsRange(lastRequestAt: string | null, now: number): RequestsRange | null {
  if (lastRequestAt === null) return null;
  const age = now - Date.parse(lastRequestAt);
  return RANGE_PRESETS.find((r) => age <= r.ms - RANGE_HEADROOM_MS)?.name ?? null;
}

/**
 * Whether a recipe line is one the mod released under a live session, which the view hides at its next re-read. A
 * line of a session the mod ended, or one the operator released, keeps its ended state and stays listed, so an
 * ended session keeps its lines.
 */
export function isReleasedLine(agent: Pick<KeepaliveRecipe, 'agentState' | 'cause'>): boolean {
  return agent.agentState === 'released' && agent.cause === 'agent_released';
}

/**
 * How many of a list's lines the view hides, which the Agents column and the Agents tile take off the engine's counts.
 */
export function releasedCount(agents: readonly Pick<KeepaliveRecipe, 'agentState' | 'cause'>[]): number {
  return agents.filter(isReleasedLine).length;
}
