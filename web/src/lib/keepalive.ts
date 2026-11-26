import type { AgentContext, AgentRequests, KeepaliveRecipe, Lifecycle, RecipeCause, Session, SessionAgentFacts, SessionFacts, SessionSpend, SessionSpendPart } from '@/api/types';
import { ACTIVE_WITHIN, RANGE_PRESETS, type SessionGroupBy, type SessionsAddress, type SortDir } from '@/lib/address';
import { clientVersion } from '@/lib/client';
import { countOf, groupInto, lookup } from '@/lib/counts';
import { byteOrder, fmtCompact, fmtDuration, keyLifetime } from '@/lib/format';
import type { BadgeTone } from '@/lib/quota';
import { compareFigures, orderRows, type SortFigure } from '@/lib/sort';

export const CAUSE_LABEL: Record<RecipeCause, string> = {
  affinity_too_short: 'affinity too short',
  agent_limit: 'agent limit',
  agent_released: 'agent released',
  agent_unknown: 'agent unknown',
  awaiting_cache_evidence: 'awaiting cache evidence',
  awaiting_request: 'awaiting request',
  awaiting_selection: 'awaiting selection',
  budget: 'at budget',
  bus_gap: 'bus gap',
  compacted: 'compacted',
  cooled: 'cooled down',
  credits: 'credits blocked',
  evicted: 'evicted',
  expired: 'entry expired',
  expired_at_restart: 'expired at restart',
  fixed_thinking: 'fixed thinking',
  forced_tool: 'forced tool',
  headers_too_large: 'headers too large',
  horizon: 'wake too far',
  idle_limit: 'idle limit',
  ineligible_credential: 'ineligible credential',
  instance_mismatch: 'instance mismatch',
  instance_replaced: 'instance replaced',
  invalid_cache_control: 'invalid cache control',
  invalid_model: 'invalid model',
  invalid_probe_usage: 'invalid probe usage',
  invalid_request: 'invalid request',
  keepalive_off: 'keepalive off',
  key_opted_out: 'key permits no cache warming',
  lease_expired: 'lease expired',
  ledger_behind: 'ledger behind',
  missing_account: 'missing account',
  no_cache_evidence: 'no cache evidence',
  no_pin: 'no pin',
  no_probe_cache_evidence: 'no probe cache evidence',
  opted_out: 'cache warming not permitted',
  overage: 'overage',
  parked_limit: 'parked limit',
  partial_cache_coverage: 'partial cache coverage',
  paused: 'account paused',
  pin_moved: 'pin moved',
  pool_disabled: 'pool disabled',
  probe_authentication_error: 'probe authentication error',
  probe_pool_exhausted: 'pool exhausted',
  probe_quota_exhausted: 'quota exhausted',
  probe_retry_limit: 'retry limit',
  probe_transport_error: 'transport error',
  probing: 'probing',
  reauth: 'needs re-auth',
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
  session_limit: 'session limit',
  session_opted_out: 'cache warming off for the session',
  slow_probe: 'slow probe',
  spent: 'spend cap reached',
  storage_limit: 'storage limit',
  structured_output: 'structured output',
  unexpected_output: 'unexpected output',
  unsupported_probe: 'unsupported probe',
  unsupported_shape: 'unsupported shape',
};

export const CAUSE_HINT: Record<RecipeCause, string> = {
  affinity_too_short: "The pool's session affinity ends before this recipe's probe is due (the cache TTL less the lead), so the session could move away from the account the probe would warm.",
  agent_limit: "The session's first request found the engine's agent cap full, so the request was not kept.",
  agent_released: 'The companion mod reported this subagent finished, so its recipe was dropped.',
  agent_unknown: "The companion mod could not tell this agent's state, so the engine keeps no recipe for it.",
  awaiting_cache_evidence:
    "The agent's request went out and its stream has not yet reported cache usage. The first stream event carries it, within seconds. The probe timer arms from that event and probes then run while the response is still streaming, so a long turn does not go cold.",
  awaiting_request: 'No real request has reached the router from this agent since it started, so there is nothing to keep yet.',
  awaiting_selection: "The agent's request reached the router and waits for an account, and the recipe arms once one serves it.",
  budget: "The pinned member's spend reached its budget. Probes wait for the period to end or the budget to change while the entry lasts.",
  bus_gap: 'The keepalive engine missed bus events while this request was open, so it kept nothing from it. The next real request starts a new recipe.',
  compacted: 'A completed compaction replaced the context this recipe kept warm, so the recipe stopped and no probe is sent. The next main turn starts a recipe for the new context.',
  cooled: 'The account the session is pinned to is under a cooldown. The probe waits for it to end while the entry lasts.',
  credits: "The account's credits are blocked, so a recipe of the fast lane is not probed there. Probing resumes once the credits return.",
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
  instance_mismatch: 'The request named another instance of the companion mod than the one that holds the session, so the engine kept nothing from it.',
  instance_replaced: "A new instance of the companion mod took this session over, so the old instance's recipes were dropped.",
  invalid_cache_control: "The request's cache markers could not be read, so it was not kept.",
  invalid_model: 'The request named a model a recipe cannot carry, so it was not kept.',
  invalid_probe_usage: "The last probe's answer carried no usage the engine could read.",
  invalid_request: 'The request body could not be replayed as sent, so it was not kept.',
  keepalive_off: "Cache warming is off on the session's pool.",
  key_opted_out: "The session's key permits no cache warming, so its recipe was dropped.",
  lease_expired: 'The companion mod stopped renewing its lease. The recipe stays and is probed under the idle window from the last real request.',
  ledger_behind: "The router refused the probe while its ledger was catching up. The probe is sent again after the answer's wait.",
  missing_account: "The last probe's answer named no account, so the engine cannot tell where it landed.",
  no_cache_evidence: "The request's answer showed no cache read or write, so there was no entry to keep warm.",
  no_pin: 'The session held no pin when the probe came due. Probing resumes at its next real request.',
  no_probe_cache_evidence: "The last probe's answer showed no cache read or write. The next attempt is scheduled on the retry ladder before the entry expires.",
  opted_out: "The session's key permits no cache warming, so the request was not kept.",
  overage: 'The account is on extra usage and the pool policy skips probes there.',
  parked_limit: "The pool's parked ceiling ran out while the session waited on a background command or a subagent, so the retained prompt was discarded. The next real request starts a new recipe.",
  partial_cache_coverage: 'The last probe read only part of the prefix and wrote the rest, so it did not keep the whole entry warm.',
  paused: 'The pinned account is paused or has left the registry, so nothing probes this session.',
  pin_moved: "The session's pin stands on another account than the one that holds the recipe's cache entry. Probing resumes at its next real request.",
  pool_disabled: "The session's pool is disabled. Probing resumes once it is enabled again while the entry lasts.",
  probe_authentication_error: 'The last probe was refused for its credentials, or its account needs re-pairing.',
  probe_pool_exhausted: 'Every account of the pool was exhausted or cooled when the probe was selected. The next attempt is scheduled on the retry ladder before the entry expires.',
  probe_quota_exhausted: "The account's usage window was spent when the last probe was sent. The next attempt is scheduled on the retry ladder before the entry expires.",
  probe_retry_limit: "The last probes failed with retryable errors along the retry ladder until the entry's TTL, so the engine stopped retrying.",
  probe_transport_error: 'The last probe failed on the transport before an answer came back. The next attempt is scheduled on the retry ladder before the entry expires.',
  probing: 'A probe is in flight right now.',
  reauth: 'The account needs re-pairing or funding before anything can be sent on it.',
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
  session_limit: "The session's first request found the engine's session cap full, so the request was not kept.",
  session_opted_out: "Cache warming is off in the session's companion mod, so the router keeps no prompt of the session.",
  slow_probe: 'The last probe took its whole refresh interval, so the next one is scheduled on the retry ladder before the entry expires rather than starting at once.',
  spent: 'The account is at its spend cap, so nothing probes there until the cap lifts.',
  storage_limit: 'The engine was at its retained-bytes cap with nothing idle to evict, so this recipe was not kept.',
  structured_output: 'The request asked for structured output, which a probe cannot replay, so it was not kept.',
  unexpected_output: "The last probe's answer carried output, which a probe never asks for.",
  unsupported_probe: "The upstream refused the last probe's shape.",
  unsupported_shape: 'The last request had no cacheable prefix the probe could replay.',
};

export const WARNING_CAUSES: RecipeCause[] = ['affinity_too_short', 'budget', 'evicted', 'expired', 'expired_at_restart', 'horizon', 'idle_limit', 'parked_limit', 'storage_limit', 'unsupported_shape'];

/** The engine's name for a session's main conversation. */
export const MAIN_AGENT = 'main';

/** The lifecycle words in the order a session's lines run after main. `registered` sits with `busy`, `unknown` with
 * `parked`, and `released` closes the lines with a word wherever a released record is still listed. */
const STATE_ORDER: Record<Lifecycle, number> = { busy: 0, idle: 1, parked: 2, registered: 0, released: 3, unknown: 2 };

const instantFigure = (at: string | null): number | null => (at === null ? null : Date.parse(at));

/**
 * Two instants newest first under `desc` and oldest first under `asc`, an instant missing last under either, so the
 * direction flips the dated entries alone.
 */
const byInstant = (a: string | null, b: string | null, dir: SortDir): number => compareFigures(instantFigure(a), instantFigure(b), dir);

/** The session's last turn: its newest inference request, or its pin's last use while the ledger holds none, the
 *  case of a first turn. A probe and a token count stamp the pin and count as no turn. The `activity` key's value. */
export const lastActivity = (s: Pick<Session, 'lastInferenceAt' | 'lastUsedAt'>): string | null => s.lastInferenceAt ?? s.lastUsedAt;

/** A row's ledgered requests: its spend's count, 0 for a row with no spend. */
export const requestCount = (session: Pick<Session, 'spend'>): number => session.spend?.requests ?? 0;

type OrderedSession = Pick<Session, 'firstRequestAt' | 'lastInferenceAt' | 'lastUsedAt' | 'sessionId' | 'spend'>;

/** A row's figure under each key of the Sessions view's order. */
const ROW_FIGURE: Record<SessionsAddress['sort'], (s: OrderedSession) => SortFigure> = {
  activity: (s) => instantFigure(lastActivity(s)),
  cost: (s) => s.spend?.usd ?? null,
  created: (s) => instantFigure(s.firstRequestAt),
  requests: requestCount,
};

/**
 * The rows under `created`, `activity`, `cost` (`spend.usd`, the list dollars, which add across units) or `requests`,
 * ties by session id, a row without the value last either way.
 */
export function orderSessionRows<T extends { session: OrderedSession }>(rows: readonly T[], sort: SessionsAddress['sort'], dir: SortDir): T[] {
  return orderRows(rows, (row) => ROW_FIGURE[sort](row.session), dir, (row) => row.session.sessionId);
}

/** A line's group: main first, then the lines by lifecycle word, then the lines with no word, whose agent the engine
 *  holds no record of. */
const lineGroup = (line: SessionLine): number => (line.agentId === MAIN_AGENT ? -1 : line.lifecycle === null ? STATE_ORDER.released + 1 : STATE_ORDER[line.lifecycle]);

const firstRequestOf = (line: SessionLine): string | null => line.firstRequestAt;

const LINE_VALUE: Record<SessionsAddress['sort'], (line: SessionLine) => string | null> = {
  activity: (line) => (line.recipe !== null ? line.recipe.lastRequestAt : (line.pin?.lastUsedAt ?? null)),
  cost: firstRequestOf,
  created: firstRequestOf,
  requests: firstRequestOf,
};

/**
 * The lines under the rows' key: main first, then the lifecycle groups (`STATE_ORDER`), then the lines with no word,
 * and inside a group by the agent's first request under `created`, `cost` and `requests` (a line without one last) or
 * by `byInstant` under `activity` (a pin-only line by its pin's last use), the direction applied inside the groups,
 * ties by agent id and then by model.
 */
export function orderLines(lines: readonly SessionLine[], sort: SessionsAddress['sort'], dir: SortDir): SessionLine[] {
  const valueOf = LINE_VALUE[sort];
  return [...lines].sort((a, b) => lineGroup(a) - lineGroup(b) || byInstant(valueOf(a), valueOf(b), dir) || a.agentId.localeCompare(b.agentId) || (a.model ?? '').localeCompare(b.model ?? ''));
}

export type RequestsRange = (typeof RANGE_PRESETS)[number]['name'];

/**
 * A minute under each width: the request log measures its window from its own load, a moment after the click that
 * chose the range, so a request right at a width's edge would fall out of the window it was picked for.
 */
const RANGE_HEADROOM_MS = 60_000;

/**
 * The narrowest of the request log's ranges that covers an instant, measured back from `now` (the instant of the
 * click, never a sampled one) with the headroom, or null past the widest range.
 */
function requestsRange(at: string, now: number): RequestsRange | null {
  const age = now - Date.parse(at);
  return RANGE_PRESETS.find((r) => age <= r.ms - RANGE_HEADROOM_MS)?.name ?? null;
}

/** The range "View in requests" opens: the narrowest preset that covers the first request, a custom span from the
 *  first request to a minute past the last one where it is older than the widest preset, and null with no first
 *  request, where the page's default applies. */
type RequestsSpan = { preset: RequestsRange } | { from: number; to: number };
export function requestsSpan(firstRequestAt: string | null, lastRequestAt: string | null, now: number): RequestsSpan | null {
  if (firstRequestAt === null) return null;
  const preset = requestsRange(firstRequestAt, now);
  if (preset !== null) return { preset };
  return { from: Date.parse(firstRequestAt), to: (lastRequestAt === null ? now : Date.parse(lastRequestAt)) + RANGE_HEADROOM_MS };
}

/** Whether the mod released a line's agent, which a live session's row draws only under the Released agents filter. */
function isReleasedLine(line: Pick<SessionLine, 'lifecycle'>): boolean {
  return line.lifecycle === 'released';
}

/**
 * A session the engine is done with: it draws a line and every line it draws is a released one. Such a session keeps
 * its lines, so its probe facts stay in view once it is shown.
 */
export function isFinished(lines: readonly Pick<SessionLine, 'lifecycle'>[]): boolean {
  return lines.length > 0 && lines.every(isReleasedLine);
}

/** A row's or a line's Status word: a lifecycle word, `finished` for a session the engine is done with, `unreported`
 *  where no mod reports the agent. */
export type StatusWord = Lifecycle | 'finished' | 'unreported';

export const STATUS_TONE: Record<Lifecycle, BadgeTone> = { busy: 'accent', idle: 'good', parked: 'neutral', registered: 'neutral', released: 'neutral', unknown: 'neutral' };

/**
 * The row's Status word and tone: `finished` first where the row's `isFinished` holds, `unreported` where the route
 * answers no word, else the lifecycle word. `muted` is the ink `unreported` takes.
 */
export function statusOf(session: Pick<Session, 'lifecycle'>, finished: boolean): { tone: BadgeTone | 'muted'; word: StatusWord } {
  if (finished) return { tone: 'neutral', word: 'finished' };
  const word = session.lifecycle;
  if (word === null) return { tone: 'muted', word: 'unreported' };
  return { tone: STATUS_TONE[word], word };
}

/**
 * The Status sub line: `seen {ago}` while the mod reports (`presenceAgeSeconds`), `silent {span}` in the warning tone
 * while it is silent (the span since `companionSince`), null where no mod reports and where the age or the instant
 * the line reads is absent, so no age is made up.
 */
export function presenceOf(session: Pick<Session, 'companion' | 'companionSince' | 'presenceAgeSeconds'>, now: number): { text: string; tone: 'muted' | 'warning' } | null {
  if (session.companion === 'reporting') return session.presenceAgeSeconds === null ? null : { text: `seen ${fmtDuration(session.presenceAgeSeconds * 1000)} ago`, tone: 'muted' };
  if (session.companion === 'silent') return session.companionSince === null ? null : { text: `silent ${fmtDuration(now - Date.parse(session.companionSince))}`, tone: 'warning' };
  return null;
}

/**
 * The pin's open shedding mark, which the `leaving` badge draws: a mark stands where the route names the row's own
 * account as marked. A no-return stamp alone is no mark, since it records a move that already happened.
 */
export const openMark = (session: Pick<Session, 'accountId' | 'mark'>): Session['mark'] => (session.mark !== null && session.mark.on === session.accountId ? session.mark : null);

/**
 * A line's word as the Status filter, the Agents cell's split and the Agents table read it: the line's own lifecycle
 * word, and `unreported` where the route answers none.
 */
export const lineWordOf = (line: Pick<SessionLine, 'lifecycle'>): Exclude<StatusWord, 'finished'> => line.lifecycle ?? 'unreported';

/** The lines less the released ones, the Agents cell's and the Dashboard's count. */
export const agentCount = (lines: readonly Pick<SessionLine, 'lifecycle'>[]): number => lines.filter((line) => !isReleasedLine(line)).length;

/**
 * The shown lines by `lineWordOf`'s word, `busy`, `idle`, `parked`, `registered`, `unknown`, `unreported`, the
 * released ones out.
 */
export function lifecycleSplit(lines: readonly Pick<SessionLine, 'lifecycle'>[]): Record<Exclude<StatusWord, 'finished' | 'released'>, number> {
  const split = { busy: 0, idle: 0, parked: 0, registered: 0, unknown: 0, unreported: 0 };
  for (const line of lines) {
    const word = lineWordOf(line);
    if (word !== 'released') split[word] += 1;
  }
  return split;
}

/** The rows' lines counted as the Agents cell counts them, with the busy, idle and parked ones of `lifecycleSplit`. */
export function lineTotals(rows: readonly { lines: readonly Pick<SessionLine, 'lifecycle'>[] }[]): { agents: number; busy: number; idle: number; parked: number } {
  const totals = { agents: 0, busy: 0, idle: 0, parked: 0 };
  for (const { lines } of rows) {
    const split = lifecycleSplit(lines);
    totals.agents += agentCount(lines);
    totals.busy += split.busy;
    totals.idle += split.idle;
    totals.parked += split.parked;
  }
  return totals;
}

/** The Context cell's text: the tokens compact, a dash with none or before the first turn. */
export const contextCell = (tokens: number | null): string => (tokens === null || tokens === 0 ? '-' : fmtCompact(tokens));

/** A model as the route keys a pin: the relay folds the long-context suffix out case-blind before it pins. */
const pinModel = (model: string): string => model.toLowerCase().replace(/\[1m\]$/, '');

/**
 * The route's entry for an agent's pin on a model, none where it lists none (a released agent's among them). The route
 * answers one entry per pin and an agent can hold pins on two models, so the key is the agent id and the model.
 */
const lineFigures = (s: Pick<Session, 'agentContexts'>, agentId: string, model: string): AgentContext | null =>
  s.agentContexts.find((c) => c.agentId === agentId && pinModel(c.model) === pinModel(model)) ?? null;

/**
 * The last segment of a path behind its trailing separators, `/` and `\` alike. Null where the path has none, a drive
 * root such as `C:\` among them, which is the Windows form of `/`.
 */
function baseName(path: string): string | null {
  const trimmed = path.replace(/[/\\]+$/, '');
  if (/^[A-Za-z]:$/.test(trimmed)) return null;
  const segment = trimmed.slice(Math.max(trimmed.lastIndexOf('/'), trimmed.lastIndexOf('\\')) + 1);
  return segment === '' ? null : segment;
}

/** The short form of a path or of a `host/path`: its last segment, or the whole value where it has none. */
export const shortPlace = (value: string): string => baseName(value) ?? value;

/** A session's repository: its remote, and a repository with no remote reads its root. */
export const repositoryOf = (facts: Pick<SessionFacts, 'repository' | 'repositoryRoot'>): string | null => facts.repository ?? facts.repositoryRoot;

/**
 * A session's project: the one its companion reports, else its working directory, since a client's project read can
 * fail while its working directory read answers.
 */
export const projectOf = (facts: Pick<SessionFacts, 'cwd' | 'project'>): string | null => facts.project ?? facts.cwd;

/**
 * A session's label: its title, else the base name of the project, for which the working directory stands in. Null
 * where neither names one or the label is the session id, so a caller draws the id alone.
 */
export function sessionLabel(session: Pick<Session, 'facts' | 'sessionId'>): string | null {
  const { facts } = session;
  const place = projectOf(facts);
  const label = facts.title ?? (place === null ? null : baseName(place));
  return label === session.sessionId ? null : label;
}

/**
 * An agent's label: its name, else its description. Null where neither names one or the label is the agent id, so a
 * caller draws the id alone.
 */
export function agentLabel(agentId: string, facts: Pick<SessionAgentFacts, 'description' | 'name'> | null): string | null {
  const label = facts?.name ?? facts?.description ?? null;
  return label === agentId ? null : label;
}

export interface SessionGroup<R> {
  /**
   * The project path or the working directory that stands in for it, the `host/path` or the repository's root. Null for
   * the rows without the fact.
   */
  key: string | null;
  rows: R[];
  /** The key's short form, or `No project` or `No repository` where the key is null. */
  short: string;
}

const NO_FACT_GROUP: Record<Exclude<SessionGroupBy, 'none'>, string> = { project: 'No project', repository: 'No repository' };

/** The rows in groups by their session's project or repository, the groups in name order and the rows as handed. */
export function groupSessions<R extends { session: Pick<Session, 'facts'> }>(rows: readonly R[], by: Exclude<SessionGroupBy, 'none'>): SessionGroup<R>[] {
  // A worktree's remote and root are its checkout's, so by repository the two share a group with or without a remote.
  const keyOf = ({ facts }: Pick<Session, 'facts'>): string | null => (by === 'project' ? projectOf(facts) : repositoryOf(facts));
  // Code-unit compares, so the five-second re-read and a second browser order the groups alike.
  const named = [...groupInto(rows, (row) => keyOf(row.session))]
    .map(([key, held]) => {
      const short = shortPlace(key);
      return { fold: short.toLowerCase(), group: { key, rows: held, short } };
    })
    .sort((a, b) => byteOrder(a.fold, b.fold) || byteOrder(a.group.key, b.group.key))
    .map(({ group }) => group);
  const without = rows.filter((row) => keyOf(row.session) === null);
  return without.length === 0 ? named : [...named, { key: null, rows: without, short: NO_FACT_GROUP[by] }];
}

/**
 * One drawn line: a recipe with its pin, a recipe without one, a pin without a recipe, or main's drawn from the row
 * where no line names it, with `registered` among its words.
 */
export interface SessionLine {
  agentId: string;
  /** The agent's type its companion reports, else its ledger entry's, null where neither names one. Main's line reads
   *  the ledger's alone. */
  agentType: string | null;
  /** What the companion reports of the line's agent, null where it reports none. */
  facts: SessionAgentFacts | null;
  /** The agent's first request from its ledger entry, null on an agent without one. */
  firstRequestAt: string | null;
  /** The pin's word where a pin stands, the engine's record, else the recipe's, and on main's drawn line the row's. */
  lifecycle: Lifecycle | null;
  /** Null on main's line drawn from a row that names no model. */
  model: string | null;
  pin: AgentContext | null;
  recipe: KeepaliveRecipe | null;
}

/** A line's account: its pin's, and its recipe's only where no pin stands. Null where neither names one. */
export const lineAccountOf = (line: Pick<SessionLine, 'pin' | 'recipe'>): string | null => (line.pin !== null ? line.pin.accountId : (line.recipe?.accountId ?? null));

/**
 * The union of a session's entries and its recipes, keyed by agent id and pin model: main's line drawn from the row
 * where no line names main, then the recipes in the order handed, then the pins no recipe claims in the route's order,
 * which `orderLines` sorts into the view's.
 */
export function sessionLines(s: Pick<Session, 'agentContexts' | 'agentFacts' | 'agentId' | 'agentRequests' | 'lifecycle' | 'model'>, recipes: readonly KeepaliveRecipe[]): SessionLine[] {
  // The route answers one entry per agent, which carries its newest client request's type and model and its first
  // request.
  const newest = new Map(s.agentRequests.map((entry) => [entry.agentId, entry]));
  // Main reads no reported fact: its label is fixed and its type is the ledger's.
  const reported = new Map(s.agentFacts.filter((entry) => entry.agentId !== MAIN_AGENT).map((entry) => [entry.agentId, entry]));
  const entryOf = (agentId: string) => ({
    agentType: reported.get(agentId)?.agentType ?? newest.get(agentId)?.agentType ?? null,
    facts: reported.get(agentId) ?? null,
    firstRequestAt: newest.get(agentId)?.firstRequestAt ?? null,
  });
  const claimed = new Set<AgentContext>();
  const lines: SessionLine[] = recipes.map((recipe) => {
    const pin = lineFigures(s, recipe.agentId, recipe.model);
    if (pin !== null) claimed.add(pin);
    return { agentId: recipe.agentId, ...entryOf(recipe.agentId), lifecycle: pin !== null ? pin.lifecycle : recipe.agentState, model: recipe.model, pin, recipe };
  });
  for (const pin of s.agentContexts) if (!claimed.has(pin)) lines.push({ agentId: pin.agentId, ...entryOf(pin.agentId), lifecycle: pin.lifecycle, model: pin.model, pin, recipe: null });
  if (lines.some((line) => line.agentId === MAIN_AGENT)) return lines;
  // A row whose pin is another agent's carries that agent's model, so main's line reads main's ledger entry.
  const model = s.agentId === null || s.agentId === MAIN_AGENT ? s.model : (newest.get(MAIN_AGENT)?.model ?? null);
  return [{ agentId: MAIN_AGENT, ...entryOf(MAIN_AGENT), lifecycle: s.lifecycle, model, pin: null, recipe: null }, ...lines];
}

/** Files the recipes under their rows on the route's flag. A recipe names no generation and belongs to the live
 *  key alone, and the route marks an earlier holder of the key id `historical`, so such a row reads none. */
export function recipeFiler(recipes: readonly KeepaliveRecipe[]): (session: Pick<Session, 'historical' | 'keyId' | 'sessionId'>) => KeepaliveRecipe[] {
  const bySession = groupInto(recipes, (r) => `${r.keyId}:${r.sessionId}`);
  return (session) => (session.historical ? [] : (bySession.get(`${session.keyId}:${session.sessionId}`) ?? []));
}

/** An agent's entry without its id: its sums over every model it ran, with its newest client request's account, model
 *  and type. */
export type AgentFigures = Omit<AgentRequests, 'agentId'>;

/** Entries summed over a list of one entry or more, the agents the earlier agents' line counts, with the newest's
 *  account, model and type and the earliest first request, null where no entry holds one. */
export function figuresOf(entries: readonly AgentRequests[]): AgentFigures {
  const [first, ...rest] = entries;
  const newest = rest.reduce((n, entry) => (byInstant(entry.lastRequestAt, n.lastRequestAt, 'desc') < 0 ? entry : n), first);
  const earliest = rest.reduce((e, entry) => (byInstant(entry.firstRequestAt, e.firstRequestAt, 'asc') < 0 ? entry : e), first);
  return {
    accountId: newest.accountId,
    agentType: newest.agentType,
    firstRequestAt: earliest.firstRequestAt,
    lastRequestAt: newest.lastRequestAt,
    model: newest.model,
    spend: rest.reduce((sum, entry) => addSpend(sum, entry.spend), first.spend),
  };
}

/** An agent's one entry, null for an agent with none. */
export function agentFigures(session: Pick<Session, 'agentRequests'>, agentId: string): AgentFigures | null {
  return session.agentRequests.find((entry) => entry.agentId === agentId) ?? null;
}

export const keysOf = (a: object, b: object): string[] => [...new Set([...Object.keys(a), ...Object.keys(b)])];

/** Two maps keyed by data summed by key, built as fresh entries so no key the wire sends reaches a prototype. */
const addMap = (a: Record<string, number>, b: Record<string, number>): Record<string, number> => Object.fromEntries(keysOf(a, b).map((k) => [k, countOf(a, k) + countOf(b, k)]));

const addPart = (a: SessionSpendPart, b: SessionSpendPart): SessionSpendPart => ({
  billedUsd: addMap(a.billedUsd, b.billedUsd),
  billedUsdByAccount: addMap(a.billedUsdByAccount, b.billedUsdByAccount),
  equivalent: a.equivalent === null || b.equivalent === null ? null : a.equivalent + b.equivalent,
  requests: a.requests + b.requests,
  shares: Object.fromEntries(keysOf(a.shares, b.shares).map((id) => [id, addMap(lookup(a.shares, id) ?? {}, lookup(b.shares, id) ?? {})])),
  usd: a.usd + b.usd,
});

/** Two spends summed: the maps merged by key, `equivalent` null where either is null. */
function addSpend(a: SessionSpend, b: SessionSpend): SessionSpend {
  return { ...addPart(a, b), probes: addPart(a.probes, b.probes) };
}

/** The entry of every agent with ledgered requests and no line, main never among them. */
function earlierEntries(session: Pick<Session, 'agentRequests'>, lines: readonly Pick<SessionLine, 'agentId'>[]): AgentRequests[] {
  const named = new Set(lines.map((line) => line.agentId));
  return session.agentRequests.filter((entry) => entry.agentId !== MAIN_AGENT && !named.has(entry.agentId));
}

/** Every agent with ledgered requests and no line, main never among them, in id order, each with its figures. */
export function earlierAgents(session: Pick<Session, 'agentRequests'>, lines: readonly Pick<SessionLine, 'agentId'>[]): AgentRequests[] {
  return earlierEntries(session, lines).sort((a, b) => (a.agentId < b.agentId ? -1 : 1));
}

/** The filters the Sessions view matches a row against, the address's values less its sort and its grouping. */
export interface SessionFilters {
  account: string[];
  /** `15m`, `1h`, `6h`, `24h` or empty. */
  active: '' | (typeof ACTIVE_WITHIN)[number]['id'];
  agentType: string[];
  /** `cold`, `no recipe`, `warm`. */
  cache: string[];
  /** Folded client versions, `clientVersion`'s words. */
  client: string[];
  companion: string[];
  effort: string[];
  finished: boolean;
  forked: boolean;
  key: string[];
  model: string[];
  pool: string[];
  released: boolean;
  /** A session id's prefix, or a part of a fact the session's companion reports, whatever the case. */
  search: string;
  /** A session id, matched whole. Empty for none. */
  session: string;
  slowed: boolean;
  /** The lifecycle words, `released`, `unknown` and `unreported` among them. */
  status: string[];
}

/** What the matcher reads of a row: the session, its lines (the released ones among them), its key's pool. */
export interface MatchRow {
  finished: boolean;
  lines: readonly SessionLine[];
  poolId: string | null;
  session: Session;
}

/** The two settings that bound how long the route lists a row no companion reports. */
export type ListingLimits = { expireAfterSeconds: number; nonCompanionIdleSeconds: number };

/**
 * The instant the route stops listing a session, as a label. A row no companion reports leaves once the latest of its
 * pin use, its last request and its companion's end is older than the lesser of the non-companion timeout and the
 * session expiry. A companion row has none, since the route keeps it whether its mod reports or went silent. Null too
 * while the settings are unknown or the row holds no instant.
 */
export function expiryOf(session: Pick<Session, 'companion' | 'companionSince' | 'lastRequestAt' | 'lastUsedAt'>, limits: ListingLimits | null): number | null {
  if (limits === null || session.companion !== 'none') return null;
  const instants = [session.lastUsedAt, session.lastRequestAt, session.companionSince].flatMap((at) => (at === null ? [] : [Date.parse(at)]));
  return instants.length === 0 ? null : Math.max(...instants) + Math.min(limits.nonCompanionIdleSeconds, limits.expireAfterSeconds) * 1000;
}

/** The row's last turn (`lastActivity`) is inside `spanMs` of `now`. */
function activeWithin(session: Pick<Session, 'lastInferenceAt' | 'lastUsedAt'>, spanMs: number, now: number): boolean {
  const last = lastActivity(session);
  return last !== null && now - Date.parse(last) <= spanMs;
}

/** Whether a list filter turns a value away: an empty one passes every value, and a standing one turns null away. */
const misses = (values: readonly string[], value: string | null): boolean => values.length > 0 && (value === null || !values.includes(value));

/** The Cache words a line matches: its pin's `warm` or `cold`, and `no recipe` on a pin no recipe stands on. A line
 *  with no pin matches none. */
const cacheWords = (line: SessionLine): string[] => (line.pin === null ? [] : [line.pin.warm ? 'warm' : 'cold', ...(line.recipe === null ? ['no recipe'] : [])]);

const present = (v: string | null): string[] => (v === null ? [] : [v]);

/**
 * The words a line offers each Agents filter, so a line that offers none misses a standing one. The Released agents
 * flag draws lines and matches none, so it stays out.
 */
const AGENT_FILTER_WORDS = {
  // The account the Agents table's cell shows, so the filter keeps the line the cell names.
  account: (line) => present(lineAccountOf(line)),
  agentType: (line) => present(line.agentType),
  cache: (line) => cacheWords(line),
  effort: (line) => present(line.pin?.effort ?? null),
  model: (line) => present(line.model),
  status: (line) => [lineWordOf(line)],
} satisfies Partial<Record<keyof SessionFilters, (line: SessionLine) => string[]>>;

/** The filters that read each agent line. */
const AGENT_FILTER_FIELDS = Object.keys(AGENT_FILTER_WORDS) as readonly (keyof typeof AGENT_FILTER_WORDS)[];

/** One line passes every Agents filter that stands, its Status read as `lineWordOf` reads it. */
function lineMatches(line: SessionLine, filters: SessionFilters): boolean {
  return AGENT_FILTER_FIELDS.every((f) => filters[f].length === 0 || AGENT_FILTER_WORDS[f](line).some((w) => filters[f].includes(w)));
}

/**
 * The lines a row draws, the ones the Agents filters read: a live row's released lines only under `released`, and
 * every line of a finished row, released ones among them, so a session the engine is done with keeps its probe facts
 * in view once it is shown.
 */
function drawnLines(row: Pick<MatchRow, 'finished' | 'lines'>, filters: Pick<SessionFilters, 'released'>): SessionLine[] {
  return row.lines.filter((line) => row.finished || filters.released || !isReleasedLine(line));
}

/**
 * The id by its start, or one of the session's reported facts by any part, whatever the case. No agent's fact is
 * read: a match on a line belongs to the Agents filters and their fold.
 */
function searchFinds(session: Pick<Session, 'facts' | 'sessionId'>, search: string): boolean {
  const term = search.toLowerCase();
  if (session.sessionId.toLowerCase().startsWith(term)) return true;
  const { branch, cwd, project, repository, repositoryRoot, title } = session.facts;
  return [branch, cwd, project, repository, repositoryRoot, title].some((value) => value !== null && value.toLowerCase().includes(term));
}

/**
 * The row passes every Session filter and, where an Agents filter stands, one of the lines it draws passes them all. A
 * finished row is out unless `finished` stands. The search matches the id's start or a part of a reported fact,
 * whatever its case. The session key keeps the row whose id it is, matched whole. Key reads the Requests view's
 * selectors: `id@generation` names one lifetime and a bare id the live key's rows, so an earlier holder's rows answer
 * to their own lifetime alone.
 */
export function matches(row: MatchRow, filters: SessionFilters, now: number): boolean {
  const { session } = row;
  if (row.finished && !filters.finished) return false;
  if (filters.search !== '' && !searchFinds(session, filters.search)) return false;
  if (filters.session !== '' && session.sessionId !== filters.session) return false;
  const active = ACTIVE_WITHIN.find((a) => a.id === filters.active);
  if (active !== undefined && !activeWithin(session, active.ms, now)) return false;
  if (misses(filters.client, clientVersion(session.userAgent)) || misses(filters.companion, session.companion) || misses(filters.pool, row.poolId)) return false;
  if (filters.forked && session.forkedFrom === null) return false;
  if (filters.key.length > 0 && !filters.key.some((k) => (k.includes('@') ? k === keyLifetime(session.keyId, session.keyGeneration) : k === session.keyId && !session.historical))) return false;
  if (filters.slowed && !(session.speed !== null && session.speed < 1)) return false;
  return !agentFiltersStand(filters) || drawnLines(row, filters).some((line) => lineMatches(line, filters));
}

/**
 * The lines the row draws (`drawn`) and, under Agents filters, the ones that match (`shown`) with how many the fold
 * line counts: the other drawn lines and the earlier agents, so the count leaves out a released line the row does not
 * draw.
 */
export function hiddenLines(row: MatchRow, filters: SessionFilters): { drawn: SessionLine[]; hidden: number; shown: SessionLine[] } {
  const drawn = drawnLines(row, filters);
  if (!agentFiltersStand(filters)) return { drawn, hidden: 0, shown: drawn };
  const shown = drawn.filter((line) => lineMatches(line, filters));
  return { drawn, hidden: drawn.length - shown.length + earlierEntries(row.session, row.lines).length, shown };
}

/**
 * The agents the bar's count names over the kept rows' `hiddenLines` answers, each row's matching lines counted as the
 * Agents cell counts.
 */
export const matchingAgents = (drawn: Iterable<Pick<ReturnType<typeof hiddenLines>, 'shown'>>): number => [...drawn].reduce((n, row) => n + agentCount(row.shown), 0);

/** Whether any Agents filter stands. */
export const agentFiltersStand = (filters: SessionFilters): boolean => AGENT_FILTER_FIELDS.some((f) => filters[f].length > 0);

/** A pin-only line's TTL: the time left to the instant its cache goes cold, null once passed or unknown. */
export const pinTtlMs = (pin: Pick<AgentContext, 'warmUntil'>, now: number): number | null => {
  if (pin.warmUntil === null) return null;
  const left = Date.parse(pin.warmUntil) - now;
  return left > 0 ? left : null;
};
