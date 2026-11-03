import type { AgentContext, KeepaliveRecipe, KeepaliveState, RecipeCause, Session } from '@/api/types';
import { RANGE_PRESETS, type SortDir } from '@/lib/address';
import { fmtCompact } from '@/lib/format';

export const CAUSE_LABEL: Record<RecipeCause, string> = {
  affinity_too_short: 'affinity too short',
  agent_released: 'agent released',
  agent_unknown: 'agent unknown',
  awaiting_cache_evidence: 'awaiting cache evidence',
  awaiting_request: 'awaiting request',
  awaiting_selection: 'awaiting selection',
  budget: 'at budget',
  bus_gap: 'bus gap',
  compacted: 'compacted',
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
  compacted: 'A completed compaction replaced the context this recipe kept warm, so the recipe stopped and no probe is sent. The next main turn starts a recipe for the new context.',
  cooled: 'The account the session is pinned to is under a cooldown. The probe waits for it to end while the entry lasts.',
  evicted: 'The engine was at its retained-bytes cap and this recipe was the oldest idle one. The next real request starts a new recipe.',
  expired: 'The cache entry expired before its probe was due, so no probe was sent. The next real request starts a new recipe.',
  expired_at_restart: 'The cache entry expired while the router was restarting, so the recipe was dropped when the router read it back. The next real request starts a new recipe.',
  fixed_thinking: 'The request asked for a fixed thinking budget, which a probe cannot replay, so it was not kept.',
  forced_tool: 'The request forced a tool choice, which a probe cannot replay, so it was not kept.',
  headers_too_large: "The request's headers were larger than a recipe may hold, so it was not kept.",
  horizon:
    "The session is parked on a scheduled wakeup further away than the pool's horizon, so the recipe is held. Probing resumes once the wake is within the horizon while the entry is still warm, and the recipe ends as expired when the entry's TTL passes.",
  idle_limit: "The pool's idle window, the break-even where the pool sets none, ran out with no real request, so the retained prompt was discarded. The next real request starts a new recipe.",
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
  no_probe_cache_evidence: "The last probe's answer showed no cache read or write. The next attempt is scheduled on the retry ladder before the entry expires.",
  opted_out: "The session's key opts out of cache warming, so the request was not kept.",
  overage: 'The account is on extra usage and the pool policy skips probes there.',
  owner_mismatch: 'The request named another owner than the one the keepalive mod registered, so the engine kept nothing from it.',
  parked_limit: "The pool's parked ceiling ran out while the session waited on a background command or a subagent, so the retained prompt was discarded. The next real request starts a new recipe.",
  partial_cache_coverage: 'The last probe read only part of the prefix and wrote the rest, so it did not keep the whole entry warm.',
  paused: 'The pinned account is paused or has left the registry, so nothing probes this session.',
  pool_disabled: "The session's pool is disabled. Probing resumes once it is enabled again while the entry lasts.",
  probe_authentication_error: 'The last probe was refused for its credentials, or its account needs re-pairing.',
  probe_pool_exhausted: 'Every account of the pool was exhausted or cooled when the probe was selected. The next attempt is scheduled on the retry ladder before the entry expires.',
  probe_quota_exhausted: "The account's usage window was spent when the last probe was sent. The next attempt is scheduled on the retry ladder before the entry expires.",
  probe_retry_limit: "The last probes failed with retryable errors along the retry ladder until the entry's TTL, so the engine stopped retrying.",
  probe_transport_error: 'The last probe failed on the transport before an answer came back. The next attempt is scheduled on the retry ladder before the entry expires.',
  probing: 'A probe is in flight right now.',
  reauth: 'The account needs re-pairing or funding before anything can be sent on it.',
  released: 'An operator released this session, so every recipe on it was dropped.',
  repeated_cold_probe:
    'The probe read no cache for the prefix it was meant to keep warm, so that entry had already expired and the next turn writes regardless. Consecutive cold probes past the cold probe limit pause the recipe until the next real request.',
  request_canceled: "The agent's request was canceled before it finished, so the last answered request's recipe stays armed where one was, and the canceled body is never probed.",
  request_failed: "The agent's request failed, so the last answered request's recipe stays armed where one was, and the failed body is never probed.",
  request_limit: 'The engine was following as many open requests as its agent limit allows, so this one was not kept.',
  request_rejected: "The router rejected the agent's request, so it left nothing to keep.",
  request_too_large: 'The request body was larger than a recipe may hold, so it was not kept.',
  reset: 'The router could not open the prompt it saved for this recipe when it restarted, so the recipe was dropped. The next real request starts a new one.',
  retryable_probe_error: 'The last probe failed with a retryable error. The next attempt is scheduled on the retry ladder before the entry expires.',
  scheduled: 'The next probe is scheduled before the cache entry expires.',
  session_ended: 'The session ended, by the mod reporting it or by its key being deleted, so its recipes were dropped.',
  session_expired: 'The session passed the session expiry with no request and no report.',
  slow_probe: 'The last probe took its whole refresh interval, so the next one is scheduled on the retry ladder before the entry expires rather than starting at once.',
  storage_limit: 'The engine was at its retained-bytes cap with nothing idle to evict, so this recipe was not kept.',
  structured_output: 'The request asked for structured output, which a probe cannot replay, so it was not kept.',
  unexpected_output: "The last probe's answer carried output, which a probe never asks for.",
  unsupported_probe: "The upstream refused the last probe's shape.",
  unsupported_shape: 'The last request had no cacheable prefix the probe could replay.',
};

export const WARNING_CAUSES: RecipeCause[] = ['affinity_too_short', 'budget', 'evicted', 'expired', 'expired_at_restart', 'horizon', 'idle_limit', 'parked_limit', 'storage_limit', 'unsupported_shape'];

/** The engine's name for a session's main conversation. */
export const MAIN_AGENT = 'main';

/** The lifecycle words in the order a session's recipe lines run after main. `unknown` sits with `parked`, and
 * `released` closes the recipe lines wherever a released record is still listed. */
const STATE_ORDER: Record<KeepaliveState, number> = { busy: 0, idle: 1, parked: 2, released: 3, unknown: 2 };

/**
 * Two instants newest first under `desc` and oldest first under `asc`, an instant missing last under either, so the
 * direction flips the dated entries alone.
 */
const byInstant = (a: string | null, b: string | null, dir: SortDir): number => {
  if (a === null || b === null) return a === b ? 0 : a === null ? 1 : -1;
  const order = Date.parse(b) - Date.parse(a);
  return dir === 'desc' ? order : -order;
};

/** The later of a session's own last use and its last ledgered request, the `activity` key's value. */
export const lastActivity = (s: Pick<Session, 'lastRequestAt' | 'lastUsedAt'>): string | null => {
  if (s.lastUsedAt === null || s.lastRequestAt === null) return s.lastUsedAt ?? s.lastRequestAt;
  return Date.parse(s.lastRequestAt) > Date.parse(s.lastUsedAt) ? s.lastRequestAt : s.lastUsedAt;
};

/** The rows under a key and a direction, ties by session id, a row without the key's value last either way. */
export function orderRows<T extends { session: Pick<Session, 'createdAt' | 'lastRequestAt' | 'lastUsedAt' | 'sessionId'> }>(rows: readonly T[], sort: string, dir: SortDir): T[] {
  const valueOf = (s: T['session']) => (sort === 'activity' ? lastActivity(s) : s.createdAt);
  return [...rows].sort((a, b) => byInstant(valueOf(a.session), valueOf(b.session), dir) || a.session.sessionId.localeCompare(b.session.sessionId));
}

/** A line's group: main first, then the recipe lines by lifecycle word, then the pins no recipe stands on. */
const lineGroup = (line: SessionLine): number => (line.agentId === MAIN_AGENT ? -1 : line.recipe === null ? STATE_ORDER.released + 1 : STATE_ORDER[line.recipe.agentState]);

/**
 * The lines under the rows' key: main first, then the lifecycle groups of the recipe lines (`STATE_ORDER`), then the
 * pin-only lines, and inside a group by the line's own `createdAt` under `created` (a line without one last) or by
 * `byInstant` under `activity` (a pin-only line by its pin's last use), the direction applied inside the groups,
 * ties by agent id and then by model.
 */
export function orderLines(lines: readonly SessionLine[], sort: string, dir: SortDir): SessionLine[] {
  const valueOf = (line: SessionLine) => (sort === 'activity' ? (line.recipe !== null ? line.recipe.lastRequestAt : (line.pin?.lastUsedAt ?? null)) : (line.pin?.createdAt ?? null));
  return [...lines].sort((a, b) => lineGroup(a) - lineGroup(b) || byInstant(valueOf(a), valueOf(b), dir) || a.agentId.localeCompare(b.agentId) || a.model.localeCompare(b.model));
}

export type RequestsRange = (typeof RANGE_PRESETS)[number]['name'];

/**
 * A minute under each width: the request log measures its window from its own load, a moment after the click that
 * chose the range, so a last request right at a width's edge would fall out of the window it was picked for.
 */
const RANGE_HEADROOM_MS = 60_000;

/**
 * The narrowest of the request log's ranges that covers a last request, a session's or one of its lines', measured
 * back from `now` (the instant of the click, never a sampled one) with the headroom, or null past the widest range or
 * without a last request, where the page's own default applies.
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

/**
 * A session the engine is done with: main's recipe reads the cause `session_ended` or `released`, or every line it
 * holds is a released one. Such a session keeps its lines, so its probe facts stay in view once it is shown.
 */
export function isFinished(session: Pick<Session, 'keepalive'>, agents: readonly Pick<KeepaliveRecipe, 'agentState' | 'cause'>[]): boolean {
  const { cause } = session.keepalive;
  return cause === 'session_ended' || cause === 'released' || (agents.length > 0 && agents.every(isReleasedLine));
}

/** The Context cell's text: the tokens compact, a dash with none or before the first turn. */
export const contextCell = (tokens: number | null): string => (tokens === null || tokens === 0 ? '-' : fmtCompact(tokens));

/** A model as the route keys a pin: the relay folds the long-context suffix out case-blind before it pins. */
const pinModel = (model: string): string => model.toLowerCase().replace(/\[1m\]$/, '');

/**
 * The route's entry for an agent's pin on a model, none where it lists none (a released agent's among them). The route
 * answers one entry per pin and an agent can hold pins on two models, so the key is the agent id and the model.
 */
export const lineFigures = (s: Pick<Session, 'agentContexts'>, agentId: string, model: string): AgentContext | null =>
  s.agentContexts.find((c) => c.agentId === agentId && pinModel(c.model) === pinModel(model)) ?? null;

/** One drawn line: a recipe with its pin, a recipe without one, or a pin without a recipe. */
export interface SessionLine {
  agentId: string;
  model: string;
  pin: AgentContext | null;
  recipe: KeepaliveRecipe | null;
}

/**
 * The union of a session's entries and its recipes, keyed by agent id and pin model: the recipes first in the order
 * handed, then the pins no recipe claims in the route's order, which `orderLines` sorts into the view's.
 */
export function sessionLines(s: Pick<Session, 'agentContexts'>, recipes: readonly KeepaliveRecipe[]): SessionLine[] {
  const claimed = new Set<AgentContext>();
  const lines: SessionLine[] = recipes.map((recipe) => {
    const pin = lineFigures(s, recipe.agentId, recipe.model);
    if (pin !== null) claimed.add(pin);
    return { agentId: recipe.agentId, model: recipe.model, pin, recipe };
  });
  for (const pin of s.agentContexts) if (!claimed.has(pin)) lines.push({ agentId: pin.agentId, model: pin.model, pin, recipe: null });
  return lines;
}

/** A pin-only line's TTL: the time left to the pin's last use plus its cache TTL, null once passed or unknown. */
export const pinTtlMs = (pin: Pick<AgentContext, 'cacheTtlMs' | 'lastUsedAt'>, now: number): number | null => {
  if (pin.cacheTtlMs === null || pin.lastUsedAt === null) return null;
  const left = Date.parse(pin.lastUsedAt) + pin.cacheTtlMs - now;
  return left > 0 ? left : null;
};

/** A read a tile's figure needs: the word the notice names it by, its error, and whether it has ever answered. */
export interface TileRead {
  error: string | null;
  loaded: boolean;
  name: string;
}

/**
 * The first read, in the order handed, that failed before it ever answered, so a tile shows one error and the notice
 * under the tiles names the read whose Try again reads it again. A refresh that fails after a first answer keeps the
 * figures it drew, which `loaded` carries.
 */
export const firstFailedRead = (reads: readonly TileRead[]): TileRead | null => reads.find((r) => !r.loaded && r.error !== null) ?? null;
