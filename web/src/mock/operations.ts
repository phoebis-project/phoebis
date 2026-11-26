import type { AgentContext, AgentRequests, BurnFigures, BurnUnit, CompanionState, KeepaliveOverview, KeepaliveRecipe, Lifecycle, MoveFigures, PoolPin, RecipeCause, RecipeState, RequestLog, RoutingEvent, Session, SessionAgentFacts, SessionDay, SessionFacts, SessionSpendPart } from '@/api/types';
import { anchorTier, fundingOf, tierCapacity, weightOf, type CostWindow } from '@/lib/costs';
import { groupInto } from '@/lib/counts';
import { expiredBetweenTurns, servingAttempt } from '@/lib/dimensions';
import { byteOrder, tokensTotal } from '@/lib/format';
import { pinTtlMs, recipeFiler } from '@/lib/keepalive';
import { compareRequests } from '@/lib/requests';
import { regimeFitOf } from '@/lib/selection';
import { accountById, burnUnitOf, sharedWindowsOf } from './accounts';
import { API_PRICES_WIRE, SUBSCRIPTION_COSTS_WIRE, priceFor, promptOf } from './catalog';
import { FIXTURES } from './generated/fixtures';
import { affinityTtlMsOf, eligibilityOf, keyById, poolById, servedFigures } from './pools';
import { ACCOUNT_BOUND_SESSION_ID, BLOCKED_SESSION_ID, CLIENTS, CLIENT_SESSION_ID, DELETED_HOLDER_GENERATION, FINISHED_AGENTS, FINISHED_SESSION_ID, FLEET_SESSION_ID, HAIKU, MARKED_SESSION_IDS, MOVED_PIN_CALL_ID, MOVED_PIN_PROBE_DUE, MOVED_PIN_PROBE_ID, MOVED_PIN_TURN_ID, OPENING_SESSION_IDS, RELEASED_AT_REQUEST, RELEASED_SESSION_ID, REQUESTS, REQUEST_EVENTS, SESSION_IDS, SLOWED_SESSION_ID, STORY_AGENTS, STRETCH_ROUND, WORKSHOP_SESSION_ID, accountBoundAt, attemptEnd, fastTurn, pinCacheTtlOf, pinKeyFor, pinReadAt, pinStampOf, requestById } from './requests';
import { dropSizeOf, droppedThinkingOf, modelWeekRewriteOf, moveSizeOf } from './rewrite';
import { createRng } from './rng';
import { KEEPALIVE_OVERVIEW_SAMPLE, RECIPE_SAMPLE, SESSION_SAMPLE, SETTINGS_SAMPLE, TABLES, regimeFor, tablesFor } from './samples';
import { HOUR, MINUTE, NOW, ago, ahead, iso, startOfHour } from './time';

const rng = createRng(2_024);

/** What a fixed row writes out in place of the rolls, each time in minutes before the mock's now. */
interface WrittenFigures {
  /**
   * Main first, then its subagents: the id, the last request and the next probe in seconds ahead, null for none, and
   * the last probe where the row writes it.
   */
  agents: { agentId: string; lastProbeMinutes?: number; lastRequestMinutes: number; nextProbeSeconds: number | null }[];
  /** The pin's burn per hour as its last fold, `burnAtMinutes` back, left it, which the row serves decayed to now. */
  burn: number;
  burnAtMinutes: number;
  /** The pin's dollar burn per hour, which a plan pin answers beside its share and a billed pin as its burn. */
  burnUsd: number;
  createdMinutes: number;
  lastInputTokens: number;
  lastRefreshMinutes: number;
  /** The mod's last report in seconds: a finished row's end, after every agent's last probe, or a silent row's. */
  lastReportSeconds?: number;
}

/**
 * The five subagents the written session holds beyond its drawn pair, written so the session holds an agent in each
 * lifecycle word, three of them released, one by the session's end, hidden with the other two unless the view lists
 * released agents. Each keeps the id the story's agent table gives its place. The list's order (a released row first,
 * the parked row before the idle one, and the idle row's last request newer than the drawn busy row's) differs from
 * the lifecycle order main, busy, idle, parked, so lines drawn in the list's order read differently from lines sorted
 * into it. Each released row's last probe dates its release inside the pool's cache TTL, since the router lists a
 * released record for one TTL after the release and then not at all.
 */
const WRITTEN_SUBAGENTS: Partial<KeepaliveRecipe>[] = [
  { agentState: 'released', cause: 'agent_released', deadline: null, lastProbeAt: ago(4 * MINUTE), lastRequestAt: ago(9 * MINUTE), nextProbeAt: null, parkKind: null, state: 'ended', wakeAt: null },
  { agentState: 'parked', cause: 'scheduled', deadline: ahead(41 * MINUTE), lastProbeAt: ago(2 * MINUTE), lastRequestAt: ago(6 * MINUTE), nextProbeAt: ahead(150_000), parkKind: 'background', state: 'retained', wakeAt: null },
  { agentState: 'idle', cause: 'scheduled', deadline: null, lastProbeAt: ago(MINUTE), lastRequestAt: ago(30_000), nextProbeAt: ahead(200_000), parkKind: null, state: 'retained', wakeAt: null },
  { agentState: 'released', cause: 'agent_released', deadline: null, lastProbeAt: ago(7 * MINUTE), lastRequestAt: ago(12 * MINUTE), nextProbeAt: null, parkKind: null, state: 'ended', wakeAt: null },
  { agentState: 'released', cause: 'session_ended', deadline: null, lastProbeAt: ago(6 * MINUTE), lastRequestAt: ago(11 * MINUTE), nextProbeAt: null, parkKind: null, state: 'ended', wakeAt: null },
];
/**
 * The written session's idle subagent: its last turn stopped with `end_turn` and its next request has not arrived, so
 * the router holds its pin as ended while its line keeps the word the mod reports. It is not its row's display pin, and
 * every round of the story ran before that turn, so no round's record owes it a row.
 */
const ENDED_SUBAGENT = WRITTEN_SUBAGENTS[2];

// `state: null` marks a session with no cache-warming recipe: the `ci` key permits
// no cache warming and `jobs` keeps no caches warm.
// `companion` is independent of that: it says whether the companion mod ever
// registered for the session and is still reporting, and a session the mod
// ended reads none. A silent companion changes neither, because the retained prompts stay and are probed
// under the idle window. `subagents` pins the agents below main, which may run another
// model and, with that, land on another account. `idleMinutes` overrides the
// random idle on the few sessions that exist to straddle the expiry presets:
// without it every fixture is idle for under an hour and no preset in Settings
// would ever drop a row. `lastUseFromRows` reads the last use off the session's
// own requests instead, for the runner's sessions on `ci`: no pin keeps them in
// use, and their account has been at its daily budget since its last served
// request. Session 0 reads it so too, since its main still awaits the evidence of
// its last request, so no probe has renewed the pin since and the row and the line
// agree. A row of those whose pin a move of the story wrote (`pinnedBy`) reads the
// later of its last request and that move, since the write of a pin stamps its
// last use, and one whose pin a placement wrote (`placedBy`) the later of its last
// request and the renewal it names, since a renewal, a probe's among them, stamps it
// too. Every pinned row then takes its main recipe's last probe where the router sent
// that probe later still. `released` marks a session whose every agent, main included, the mod
// released. `sessionId` writes out the id of a session past the story's ids whose
// story row names it.
const sessionSpec: ({
  accountId: string;
  agents: number;
  /**
   * Why main's probe is held or its recipe ended. Without one, its next probe is scheduled, or in flight on a busy
   * session.
   */
  cause?: RecipeCause;
  /** The subagent whose compaction completed, by its place among the row's agents. */
  compactedAgent?: number;
  companion: CompanionState;
  /**
   * What the row's companion reports: the session's facts, its subagents' by their place among the row's agents, as
   * `compactedAgent` names one, and its earlier agents' by their id, since an agent with seed rows and no line has no
   * place there. Absent on a row with no reported fact.
   */
  facts?: { agents?: (Omit<SessionAgentFacts, 'agentId'> | null)[]; earlier?: readonly SessionAgentFacts[]; session: Partial<SessionFacts> };
  /**
   * The row is finished: the builder writes main's cause `session_ended`, every subagent's `agent_released` and every
   * agent in `released` state.
   */
  finished?: true;
  idleMinutes?: number;
  keyId: string;
  lastUseFromRows?: boolean;
  model: string;
  /** Main's requests named no effort, so its pin, the row and main's line read none. */
  noEffort?: true;
  /**
   * The story request whose move wrote the row's pin on its account. The row and main's line date their pin by the
   * move, and `stamped` gives the row the no-return stamp the move left. The row pairs it with `lastUseFromRows`, which
   * carries the rule that the later of the move and the row's last request is its last use.
   */
  pinnedBy?: { requestId: string; stamped?: true };
  /**
   * The story request whose placement wrote the row's pin where no move of the story did, and the one whose renewal
   * last stamped the pin's use. The row and main's line date their pin by the placement, and the row's last use reads
   * the later of its last request and the renewal.
   */
  placedBy?: { renewedBy: string; requestId: string };
  released?: true;
  sessionId?: string;
  state: Lifecycle | null;
  /** A held or paused subagent under a main agent that keeps probing, which is the case the state hints describe. */
  subagentHold?: RecipeCause;
  subagents?: { accountId?: string; model: string };
  /** The subagents the row holds beyond its drawn pair, written rather than rolled. */
  writtenSubagents?: Partial<KeepaliveRecipe>[];
} & (
  | { figures?: undefined; fixed?: undefined }
  | {
      figures: WrittenFigures;
      /** The row and its agents draw nothing from `rng`. Their ids, dates and figures are written in the row. */
      fixed: true;
      idleMinutes: number;
      sessionId: string;
    }
))[] = [
  // Its second subagent compacted, while its first stays paused on its cold probes. Its title runs past the Session
  // column at 1280 px and its second subagent's description past the Agent cell, so each label reads cut.
  {
    keyId: 'desktop', accountId: 'primary', model: 'claude-fable-5-1', state: 'busy', agents: 3, companion: 'reporting', lastUseFromRows: true, subagents: { model: 'claude-sonnet-5', accountId: 'nightly' }, cause: 'awaiting_cache_evidence', compactedAgent: 2, subagentHold: 'repeated_cold_probe',
    facts: {
      agents: [null, { agentType: 'code-reviewer', description: 'Review the retry loop', name: 'reviewer' }, { agentType: null, description: 'Trace the export job timeouts across the invoice and payout batch runs', name: null }],
      session: { branch: 'fix/checkout-retry', cwd: '/home/dev/work/storefront', project: '/home/dev/work/storefront', repository: 'git.example/acme/storefront', repositoryRoot: '/home/dev/work/storefront', title: 'Checkout retry rework for the storefront payment adapters and the order export job' },
    },
  },
  // A worktree of the first row's checkout: one repository and one root, another project.
  {
    keyId: 'desktop', accountId: 'primary', model: 'claude-fable-5-1', state: 'idle', agents: 1, companion: 'reporting', cause: 'expired',
    facts: { session: { branch: 'feature/tax-rules', cwd: '/home/dev/work/storefront-worktrees/tax-rules', project: '/home/dev/work/storefront-worktrees/tax-rules', repository: 'git.example/acme/storefront', repositoryRoot: '/home/dev/work/storefront' } },
  },
  // Its keepalive probe renewed the pin on secondary and met a 429 there, and secondary cools until 14:55, so the
  // router answers each later probe of main and of its subagent the cooldown, and both wait on the retry ladder.
  { keyId: 'desktop', accountId: 'secondary', model: 'claude-sonnet-4-5', state: 'idle', agents: 2, companion: 'silent', lastUseFromRows: true, cause: 'probe_quota_exhausted', subagentHold: 'probe_quota_exhausted', placedBy: { renewedBy: 'req_c41d0311-200', requestId: 'req_d4e8a7b2-916' } },
  // A parked main agent on a wakeup further away than the pool's horizon, held with its prompt kept. The other parked
  // session's agent stays retained, parked on a background command with its deadline. Its repository has no remote.
  {
    keyId: 'desktop', accountId: 'primary', model: 'claude-opus-5', state: 'parked', agents: 1, companion: 'reporting', cause: 'horizon',
    facts: { session: { branch: 'main', cwd: '/home/dev/work/notes', project: '/home/dev/work/notes', repositoryRoot: '/home/dev/work/notes' } },
  },
  // A subagent whose park ran past the pool's parked ceiling. Its companion reports no fact of the subagent and no
  // branch, and the session works in a folder below its project. The companion reported the earlier agent 0b7f, which
  // holds a seed row and no line, by a name and a type of its own.
  {
    keyId: 'desktop', accountId: 'nightly', model: 'claude-fable-5-1', state: 'idle', agents: 2, companion: 'reporting', subagents: { model: 'claude-sonnet-5' }, cause: 'overage', subagentHold: 'parked_limit',
    facts: { earlier: [{ agentId: '0b7f', agentType: 'repo-scout', description: null, name: 'scout' }], session: { cwd: '/home/dev/work/billing-service/api', project: '/home/dev/work/billing-service', repository: 'git.example/acme/billing-service', repositoryRoot: '/home/dev/work/billing-service' } },
  },
  // Its companion mod holds the cache warming off for the session, so the engine keeps no prompt of it and sends no
  // probe. Main's line reads ended, and the pin that a move of the story wrote stays.
  { keyId: 'desktop', accountId: 'primary', model: 'claude-sonnet-4-5', state: 'busy', agents: 1, companion: 'reporting', cause: 'session_opted_out', pinnedBy: { requestId: 'req_da52d141-a5a' } },
  // Idle for longer than the pool's one-hour max idle, so its recipe is held and its retained prompt is gone.
  { keyId: 'laptop', accountId: 'nightly', model: 'claude-sonnet-4-5', state: 'idle', agents: 1, companion: 'silent', idleMinutes: 95, cause: 'idle_limit' },
  // Its subagents run Haiku 4.5, which the laptop key's list allows beside the main agent's Sonnet 4.5. Their pin
  // stands on nightly, the one member open to Haiku 4.5 since primary refused the model at 14:24:10.
  { keyId: 'laptop', accountId: 'primary', model: 'claude-sonnet-4-5', state: 'busy', agents: 2, companion: 'reporting', subagents: { accountId: 'nightly', model: 'claude-haiku-4-5-20251001' }, cause: 'unsupported_shape', subagentHold: 'affinity_too_short' },
  // A client whose project read failed while its working directory read answered.
  {
    keyId: 'laptop', accountId: 'nightly', model: 'claude-haiku-4-5-20251001', state: 'parked', agents: 1, companion: 'reporting', noEffort: true,
    facts: { session: { cwd: '/home/dev/work/scratch' } },
  },
  // Five of its agents are written, so it holds an agent in each lifecycle word, three of them released. Holding its
  // first subagent moves no value a reader sees: the agents drawn after it take their fields from the written rows.
  { keyId: 'desktop', accountId: 'primary', model: 'claude-fable-5-1', state: 'idle', agents: 7, companion: 'reporting', subagentHold: 'probe_pool_exhausted', writtenSubagents: WRITTEN_SUBAGENTS },
  { keyId: 'ci', accountId: 'research', model: 'claude-haiku-4-5-20251001', state: null, agents: 1, companion: 'none', lastUseFromRows: true },
  { keyId: 'ci', accountId: 'research', model: 'claude-haiku-4-5-20251001', state: null, agents: 2, companion: 'none', lastUseFromRows: true },
  { keyId: 'laptop', accountId: 'secondary', model: 'claude-sonnet-4-5', state: null, agents: 1, companion: 'none' },
  // Only a report of the mod moves an agent off the busy word the engine sets on each request, and no mod reports it.
  { keyId: 'desktop', accountId: 'primary', model: 'claude-sonnet-4-5', state: 'busy', agents: 1, companion: 'none', lastUseFromRows: true, pinnedBy: { requestId: 'req_1d9c3e55-0b2', stamped: true } },
  // Seen by the router but with nothing in the ledger, so the per-row jump to
  // analytics has no traffic to land on.
  { keyId: 'laptop', accountId: 'nightly', model: 'claude-haiku-4-5-20251001', state: null, agents: 1, companion: 'none' },
  // A session the mod released whole, whose lines' last uses, minutes back, date the release inside all's one-hour
  // cache TTL, since the router lists a released record for one TTL after its release and then not at all.
  { keyId: 'desktop', accountId: 'primary', model: 'claude-sonnet-5', state: 'idle', agents: 3, companion: 'reporting', released: true, sessionId: RELEASED_SESSION_ID },
  { keyId: 'laptop', accountId: 'primary', model: 'claude-sonnet-4-5', state: 'busy', agents: 1, companion: 'reporting', sessionId: CLIENT_SESSION_ID },
  // One session the mod ended, whose companion reads none since its end, which keeps its lines, its released subagents'
  // last probes dating their release inside all's one-hour cache TTL, and its stored facts (a title, a project and
  // a7f3's name), and which the Sessions view's default filters hide, and one no companion reports, last used two hours
  // ago, past the one-hour non-companion timeout, so the route's list leaves it out. The ended session's companion word
  // reads the engine's record, which is gone, and its facts read the stored rows.
  {
    keyId: 'desktop',
    accountId: 'nightly',
    model: 'claude-opus-5',
    state: 'idle',
    agents: 3,
    companion: 'none',
    facts: { agents: [null, { agentType: null, description: null, name: 'drafter' }, null], session: { project: '/home/dev/work/mobile-app', title: 'Release notes draft for the mobile app' } },
    finished: true,
    fixed: true,
    idleMinutes: FINISHED_AGENTS[0].minutes,
    sessionId: FINISHED_SESSION_ID,
    figures: {
      agents: FINISHED_AGENTS.map(({ agentId, minutes }) => ({ agentId, lastRequestMinutes: minutes, nextProbeSeconds: null })),
      burn: 0.18,
      burnAtMinutes: 22,
      burnUsd: 0.47,
      createdMinutes: 140,
      lastInputTokens: 14_200,
      lastRefreshMinutes: 22,
      lastReportSeconds: 30,
    },
  },
  {
    keyId: 'laptop',
    accountId: 'secondary',
    model: 'claude-sonnet-4-5',
    state: 'idle',
    agents: 1,
    companion: 'none',
    // Idle for two hours, past all's one-hour max idle as the 95-minute laptop row is, so its recipe ends, and the
    // engine's figures keep it while the route's list leaves the row out. Its last probe is its last refresh, ahead of
    // its last request, since a probe stamps the pin and refreshes it at once.
    cause: 'idle_limit',
    fixed: true,
    idleMinutes: 120,
    sessionId: 'e5a17c3d-8f20-4b6e-9d41-2c7b0a9f6e58',
    figures: {
      agents: [{ agentId: 'main', lastProbeMinutes: 121, lastRequestMinutes: 120, nextProbeSeconds: null }],
      burn: 0.09,
      burnAtMinutes: 121,
      burnUsd: 0.21,
      createdMinutes: 310,
      lastInputTokens: 6_400,
      lastRefreshMinutes: 121,
    },
  },
  // A pin under the work pool's key on nightly, an account the pool held before, so the pool's figure counts it and
  // no member's does. Its mod went silent beside its idle recipe, as the engine sends such a session, at the presence
  // age its place gives a silent row, written so the row draws nothing. Its last probe is its last refresh, since a
  // probe stamps the pin and refreshes it at once.
  {
    keyId: 'notebook',
    accountId: 'nightly',
    model: 'claude-sonnet-5-5',
    state: 'idle',
    agents: 1,
    companion: 'silent',
    fixed: true,
    idleMinutes: 20,
    sessionId: '6d2f8b40-1e7a-4c95-a3d8-7b5e0c9f2a61',
    figures: {
      agents: [{ agentId: 'main', lastProbeMinutes: 21, lastRequestMinutes: 20, nextProbeSeconds: 240 }],
      burn: 0.04,
      burnAtMinutes: 21,
      burnUsd: 0.11,
      createdMinutes: 95,
      lastInputTokens: 10_800,
      lastRefreshMinutes: 21,
      lastReportSeconds: 34 * 60,
    },
  },
  // A session the mod released whole after a probe and one more turn, both past all's ten-minute affinity window, so
  // the router expired its pin and only the released line stays listed, the turn dating its release inside all's
  // one-hour cache TTL.
  {
    keyId: 'desktop',
    accountId: 'primary',
    model: 'claude-sonnet-5',
    state: 'idle',
    agents: 1,
    companion: 'reporting',
    released: true,
    fixed: true,
    idleMinutes: 25,
    sessionId: '1f7c3a9e-5b24-4d86-9e10-7a3c6b8d2f59',
    figures: {
      agents: [{ agentId: 'main', lastProbeMinutes: 31, lastRequestMinutes: 25, nextProbeSeconds: null }],
      burn: 0.05,
      burnAtMinutes: 26,
      burnUsd: 0.14,
      createdMinutes: 90,
      lastInputTokens: 9_400,
      lastRefreshMinutes: 26,
    },
  },
];

/** How long the router lists a session after its last request, which the settings answer as well. */
const SESSION_EXPIRE_SECONDS = 86_400;
const NON_COMPANION_IDLE_SECONDS = 3_600;
/**
 * The router drops a session past `SESSION_EXPIRE_SECONDS`, so a ledger-only row is listed only while its last request
 * is inside the span.
 */
const insideSessionExpiry = (lastAt: string, now: number): boolean => Date.parse(lastAt) + SESSION_EXPIRE_SECONDS * 1000 > now;
/** Whether an instant is inside the non-companion timeout: a row or a line with no use on record is never older. */
const withinNonCompanionTimeout = (at: string | null, seconds: number, now: number): boolean => at === null || now - Date.parse(at) <= seconds * 1000;

// Grouped as the route's activity query groups a session's rows, by key, generation and session with probes left out,
// so the earlier holder's rows count for its own row below, never for the live session that shares their id. A refused
// request moves the session's last request too.
const activityOf = (keyId: string | null, keyGeneration: number, sessionId: string) => `${keyId}|${keyGeneration}|${sessionId}`;
const ledgered = new Map<string, string>();
for (const r of REQUESTS) {
  if (r.sessionId === null || r.kind === 'probe') continue;
  const id = activityOf(r.keyId, r.keyGeneration, r.sessionId);
  const seen = ledgered.get(id);
  if (seen === undefined || r.at > seen) ledgered.set(id, r.at);
}
/** Each agent's newest request of its session the router did not refuse, which re-armed the agent's recipe. */
const newestRequestAt = new Map<string, string>();
/**
 * Each agent's requests of its session that placed a pin, any of which the router may first have seen the agent at: an
 * inference request with an attempt, since the router writes the record at the first placement and a classifier call,
 * a count, a probe or a refusal before any attempt places none.
 */
const placingRequests = new Map<string, string[]>();
for (const r of REQUESTS) {
  if (r.sessionId === null || r.agentId === null) continue;
  const id = `${activityOf(r.keyId, r.keyGeneration, r.sessionId)}|${r.agentId}`;
  if (r.kind !== 'probe' && !r.refused) {
    const seen = newestRequestAt.get(id);
    if (seen === undefined || Date.parse(r.at) > Date.parse(seen)) newestRequestAt.set(id, r.at);
  }
  if (r.kind === 'inference' && r.classifier === null && r.attempts.length > 0) {
    const list = placingRequests.get(id);
    if (list) list.push(r.at);
    else placingRequests.set(id, [r.at]);
  }
}
/**
 * An agent's first sighting in its session's current life: its earliest placing request at or after the session's own
 * first sighting, since the router dates a session afresh once its last pin left and an agent's record with it.
 */
const earliestPlacingRequestAt = (id: string, sessionFirstSeen: string | null): string | undefined =>
  (placingRequests.get(id) ?? []).filter((at) => sessionFirstSeen === null || Date.parse(at) >= Date.parse(sessionFirstSeen)).reduce<string | undefined>((first, at) => (first === undefined || at < first ? at : first), undefined);

/**
 * A pin's burn as the Pro-week figure per hour: the account tier's own share of `window` scaled by the tier's capacity
 * there over the anchor tier's 7-day capacity, the Pro week the figure is a share of, each capacity the row of the
 * regime the pin's key's client label names.
 */
const burnEquivalentOf = (burn: number | null, accountId: string, window: CostWindow, keyId: string): number | null => {
  const tier = accountById.get(accountId)?.subscription?.rateLimitTier;
  const tables = tablesFor(CLIENTS[keyId] ?? null);
  const own = tier === undefined ? null : tierCapacity(tables, tier, window);
  const anchor = tierCapacity(tables, anchorTier(tables), 'seven_day');
  return burn === null || own === null || anchor === null ? null : (burn * own) / anchor;
};
/** The window a plan pin's burn is a share of: the 5-hour window where the account's set has it, else the 7-day one. */
const burnWindowOf = (accountId: string): CostWindow => (sharedWindowsOf(accountId).includes('five_hour') ? 'five_hour' : 'seven_day');
/** How a pin's burn reads on its account: a window share names its window and its Pro-week figure, dollars neither. */
const burnReadingOf = (accountId: string, keyId: string, burn: number | null, unit: BurnUnit | null = burnUnitOf(accountId)): Pick<Session, 'burnEquivalent' | 'burnUnit' | 'burnWindow'> => {
  const window = unit === 'window_share' ? burnWindowOf(accountId) : null;
  return { burnEquivalent: window === null ? null : burnEquivalentOf(burn, accountId, window, keyId), burnUnit: unit, burnWindow: window };
};
/**
 * A pin's plan copy in dollars per hour: the dollar burn its row drew, or wrote, beside its share, answered on a plan
 * pin where the story's price table in force at the clock lists a row for the pin's model. Null on a billed pin, which
 * answers its dollars as `burn`, on a row with no pin and for a model the table lists no row for. No subscription
 * weight is read, so a pin on a model the cost model does not weigh answers its dollars beside a null share.
 */
const burnUsdOf = (dollars: number | null, unit: BurnUnit | null, model: string): number | null => (unit !== 'window_share' || dollars === null || priceFor(model, NOW) === null ? null : dollars);
/** The burn averages after which the router reads a decayed burn as zero. */
const BURN_DECAY_CUTOFF_AVERAGES = 30;
/**
 * The factor a pin's burn decays by from its last fold at `foldAt` to the story's clock, as the sessions route decays
 * each pin to its read: the burn average of the pool the pin's key routes through is the time constant, and the burn
 * reads 0 once the fold lies more than 30 averages back. A fold past the clock decays nothing, and a burn never folded
 * reads as stored.
 */
const decayToNow = (foldAt: string | null, keyId: string): number => {
  if (foldAt === null) return 1;
  const averageMs = Math.max((poolById.get(keyById.get(keyId)?.poolId ?? '')?.burnAverageSeconds ?? 0) * 1_000, 1);
  const age = NOW - Date.parse(foldAt);
  return age > BURN_DECAY_CUTOFF_AVERAGES * averageMs ? 0 : Math.exp(-Math.max(age, 0) / averageMs);
};
/** The instant the route answers beside a decayed burn: its read, or a fold that lies past it. */
const servedBurnAt = (foldAt: string | null): string | null => (foldAt === null ? null : iso(Math.max(NOW, Date.parse(foldAt))));
/**
 * Each answer of `hasDestination` by account and key. Only the module's build asks, while the seed pools, the seed
 * accounts, the story's clock and the keys' client labels stand fixed, so an answer holds for every later ask.
 */
const DESTINATIONS = new Map<string, boolean>();
/**
 * Whether a pin on `accountId` under `keyId` has a destination: a sibling of the pin's pool, its own account apart,
 * with room for the pin. A sibling counts where the pools route reads it as eligible, and it has room where its entry
 * of `regimeFits` under the regime of the key's client label, the base entry for a key with no label, reads `fits`.
 * The router's move fit also books the pin's own rewrite and burn, which the served figures do not hold, so a fresh
 * session's fit stands in for it.
 */
const hasDestination = (accountId: string, keyId: string): boolean => {
  const memo = `${accountId}|${keyId}`;
  const known = DESTINATIONS.get(memo);
  if (known !== undefined) return known;
  const pool = poolById.get(keyById.get(keyId)?.poolId ?? '');
  const regime = regimeFor(CLIENTS[keyId] ?? null);
  const found =
    pool !== undefined &&
    pool.members.some(
      (m) =>
        m.accountId !== accountId &&
        eligibilityOf(accountById.get(m.accountId), pool.overagePolicy, m, NOW) === 'eligible' &&
        regimeFitOf(servedFigures(m.accountId, NOW, pool), regime)?.fit === 'fits',
    );
  DESTINATIONS.set(memo, found);
  return found;
};
/**
 * A pin's move as the mock's rounds read it, under the regime its key's client label names: the size the round divides
 * its burn by, the dropped thinking at the output's weight, and the cost of each that a warm move and a cold one pay,
 * each written to the cache at the pin's TTL `ttlMs` as its lane's write and priced as the Pro share of the week the
 * pin's model draws, the Fable week for a Fable pin. The size reads null where the pin holds no prompt size and the
 * drop where it drops nothing, each with its cost, and a cost reads null for a model without a weight on that week. A
 * prompt of 0 tokens is no size, as the route answers null where nothing is priced above zero, so a pin with a drop
 * and no size pays the drop's cost alone. A pin that no round can move reads no cost and keeps its other figures: one
 * that holds state on its account, whose flag the figures carry, and one whose pool holds no destination for it by
 * `hasDestination`.
 */
const moveFiguresOf = (pin: Parameters<typeof droppedThinkingOf>[0], tokens: number | null, ttlMs: number): Pick<AgentContext, 'accountBound' | 'moveCost' | 'moveDropCost' | 'moveDropSize' | 'moveSize'> => {
  const accountBound = accountBoundAt(pinKeyFor(pin.keyId, pin.sessionId, pin.agentId, pin.model), NOW);
  const dropped = droppedThinkingOf(pin, NOW);
  const client = CLIENTS[pin.keyId] ?? null;
  const drop = dropSizeOf(dropped, pin.model, client, ttlMs);
  const size = moveSizeOf(tokens, dropped, drop);
  const dropCost = drop === null ? null : modelWeekRewriteOf(drop, pin.model, client, ttlMs);
  return { accountBound, moveCost: accountBound || !hasDestination(pin.accountId, pin.keyId) ? null : size === null ? dropCost : modelWeekRewriteOf(size, pin.model, client, ttlMs), moveDropCost: dropCost, moveDropSize: drop, moveSize: size };
};
/** A row with no pin, which answers no flag of held account state and none of its move's figures. */
const NO_MOVE: Pick<Session, 'accountBound' | 'moveCost' | 'moveDropCost' | 'moveDropSize' | 'moveSize'> = { accountBound: null, moveCost: null, moveDropCost: null, moveDropSize: null, moveSize: null };
/** A pin's lane by the story's rule for its key's turns on its model, which the router stores on the pin. */
const fastPin = (keyId: string, model: string): boolean => fastTurn(keyId, model, 'inference');
/**
 * The served burn of a line beside its row's display pin, set from the row's figures rather than drawn, so the rows
 * after it keep their rolls: the display pin's burn scaled by the line's last prompt over the display pin's, none where
 * the display pin burns none or where a window share has no weight for the line's model to price it at. Its dollar
 * burn scales the display pin's dollars the same way and reads no weight. A pin whose last use is its session's first
 * sighting was never renewed with usage, so it burns 0. The display pin's own line reads the row's figures instead.
 */
const servedBeside = (
  display: Pick<Session, 'burn' | 'lastInputTokens'> & { dollars: number | null },
  line: Pick<AgentContext, 'accountId' | 'lastInputTokens' | 'lastUsedAt' | 'model'>,
  sessionFirstSeen: string | null,
): { burn: number | null; dollars: number | null } => {
  if (line.lastUsedAt !== null && line.lastUsedAt === sessionFirstSeen) return { burn: 0, dollars: 0 };
  const priced = burnUnitOf(line.accountId) !== 'window_share' || weightOf(TABLES, line.model, 'seven_day') !== null;
  const scaled = (figure: number | null) => (figure === null || display.lastInputTokens === null || line.lastInputTokens === null ? null : (figure * line.lastInputTokens) / display.lastInputTokens);
  return { burn: priced ? scaled(display.burn) : null, dollars: scaled(display.dollars) };
};

/**
 * A pinned row's runway, set from the row's index rather than drawn, so the rows after it keep their rolls: the pinned
 * rows take a span, none, unbounded and no projection in turn, the four readings the Runway cell draws.
 */
function runwayOf(i: number): Pick<Session, 'runwayBounded' | 'runwayMs'> {
  switch (i % 4) {
    case 0:
      return { runwayBounded: true, runwayMs: (47 + i * 23) * MINUTE };
    case 1:
      return { runwayBounded: true, runwayMs: 0 };
    case 2:
      return { runwayBounded: false, runwayMs: null };
    default:
      return { runwayBounded: null, runwayMs: null };
  }
}

/**
 * The words a pin's effort reads, taken by an index rather than drawn, so the rows after it keep their rolls. The four
 * are the levels the relay reads, in no order a reader relies on.
 */
const EFFORT_WORDS = ['medium', 'high', 'low', 'max'] as const;
const effortAt = (i: number) => EFFORT_WORDS[i % EFFORT_WORDS.length];

/** A pin's first cold instant: its last refresh plus its cache TTL, null where either is unknown. */
const warmUntilOf = (lastRefreshAt: string | null, cacheTtlMs: number | null): string | null => (lastRefreshAt === null || cacheTtlMs === null ? null : iso(Date.parse(lastRefreshAt) + cacheTtlMs));
/** Whether a pin still holds its cache at the story's clock: before its first cold instant. */
const warmAt = (warmUntil: string | null): boolean => pinTtlMs({ warmUntil }, NOW) !== null;
/** A line before `datedLines` dates it, with its pin's own instant where its caller writes one. */
type UndatedLine = Omit<AgentContext, 'pinnedAt'> & Partial<Pick<AgentContext, 'pinnedAt'>>;
/**
 * A pin's line as the route answers it: warm until its last refresh plus its cache TTL, the refresh its caller names
 * and the TTL its session's entries live for (`pinCacheTtlOf`), its burn and dollars as folded priced on the pin's own
 * account and model, in that account's unit and window, as the row's are for the display pin, and its lane, its flag of
 * held account state and its move's figures as the row's are. Its burn folded at its last use unless its caller names
 * the instant, as the display pin's line takes its row's, since the route keeps one fold per pin, and the line serves
 * it decayed to the story's clock with that instant beside it.
 */
const pinLine = (
  line: Omit<UndatedLine, 'accountBound' | keyof BurnFigures | 'cacheTtlMs' | 'fast' | keyof MoveFigures | 'warm' | 'warmUntil'> & Partial<Pick<AgentContext, 'burnAt'>>,
  keyId: string,
  sessionId: string,
  folded: { burn: number | null; dollars: number | null },
  lastRefreshAt: string | null,
): UndatedLine => {
  const cacheTtlMs = pinCacheTtlOf(sessionId);
  const unit = burnUnitOf(line.accountId);
  const foldAt = line.burnAt === undefined ? line.lastUsedAt : line.burnAt;
  const factor = decayToNow(foldAt, keyId);
  const burn = unit === null || folded.burn === null ? null : folded.burn * factor;
  const warmUntil = warmUntilOf(lastRefreshAt, cacheTtlMs);
  return {
    ...line,
    burn,
    burnAt: servedBurnAt(foldAt),
    ...burnReadingOf(line.accountId, keyId, burn, unit),
    burnUsd: burnUsdOf(folded.dollars === null ? null : folded.dollars * factor, unit, line.model),
    cacheTtlMs,
    fast: fastPin(keyId, line.model),
    ...moveFiguresOf({ accountId: line.accountId, agentId: line.agentId, keyId, model: line.model, sessionId }, line.lastInputTokens, cacheTtlMs),
    warm: warmAt(warmUntil),
    warmUntil,
  };
};
/** The later of two instants, either of which may be missing. */
const laterOf = (a: string | null, b: string | null): string | null => (a === null ? b : b === null || Date.parse(a) >= Date.parse(b) ? a : b);
/**
 * The newest use across a row's conversation pins, its display pin's and its lines', which the route answers as the
 * row's last use, so a subagent's turn keeps the row live.
 */
const newestUse = (own: string | null, lines: readonly Pick<AgentContext, 'lastUsedAt'>[]): string | null => [own, ...lines.map((l) => l.lastUsedAt)].reduce(laterOf, null);
/** The route's order of a session's lines: main first, then by agent id and model as bytes. */
const lineOrder = (a: Pick<AgentContext, 'agentId' | 'model'>, b: Pick<AgentContext, 'agentId' | 'model'>) =>
  (a.agentId === 'main') !== (b.agentId === 'main') ? (a.agentId === 'main' ? -1 : 1) : byteOrder(a.agentId, b.agentId) || byteOrder(a.model, b.model);
/**
 * A row's lines in the route's order, each pin written at its agent's first sighting unless its caller dates the pin
 * itself: main's at the session's, a subagent's at its earliest placing request (`earliestPlacingRequestAt`) of the
 * session's current life in the requests seed. A subagent no placing request of that life names reads the session's
 * sighting plus a minute per place among the row's subagents, a stand-in for a first sighting the story does not
 * write, and an agent pinned on two models takes one place, since it holds one first sighting.
 */
const datedLines = (firstSeen: string | null, session: string, lines: readonly UndatedLine[]): AgentContext[] => {
  const places = new Map<string, number>();
  const dated = (line: UndatedLine, seen: string | null): AgentContext => ({ ...line, pinnedAt: line.pinnedAt ?? seen });
  return [...lines].sort(lineOrder).map((line) => {
    if (line.agentId === 'main') return dated(line, firstSeen);
    if (!places.has(line.agentId)) places.set(line.agentId, places.size + 1);
    const place = places.get(line.agentId)!;
    const first = earliestPlacingRequestAt(`${session}|${line.agentId}`, firstSeen);
    return dated(line, first ?? (firstSeen === null ? null : iso(Date.parse(firstSeen) + place * MINUTE)));
  });
};
/** The summary of a row main holds no recipe for. */
const noRecipe = (): Session['keepalive'] => ({ nextProbeAt: null });
/** A pin no round marked, and a row with no pin. */
const NO_MARK: Pick<Session, 'mark'> = { mark: null };
/** A session whose companion reports no fact of it, or that no companion reports. */
const NO_FACTS: SessionFacts = { branch: null, cwd: null, project: null, repository: null, repositoryRoot: null, title: null };
/**
 * A pin with no no-return stamp, and a row with no pin. A row whose pin a failover of the story wrote reads that
 * failover's stamp instead (`pinnedBy`).
 */
const NO_STAMP: Pick<Session, 'shed'> = { shed: null };
/**
 * The instant the story's move for `requestId` wrote the pin of `sessionId` on `accountId`, and the no-return stamp it
 * leaves where the row keeps one, from the account the pin left under the move's reason. A story that writes no such
 * move fails the mock as it loads rather than reading a default.
 */
function storyPinWrite(requestId: string, accountId: string, sessionId: string, stamped: boolean): { at: string } & Pick<Session, 'shed'> {
  const move = REQUEST_EVENTS.get(requestId)?.find((e) => e.kind === 'pin_migrated' && e.accountId === accountId && e.sessionId === sessionId);
  const from = move?.detail?.from_account;
  const reason = move?.detail?.reason;
  if (move === undefined || typeof from !== 'string' || typeof reason !== 'string') throw new Error(`the story moves no pin of ${sessionId} onto ${accountId} at ${requestId}`);
  return { at: move.at, shed: stamped ? { at: move.at, from, reason } : null };
}

/**
 * The instants a placement of the story wrote a row's pin at and a renewal last stamped its use at, each its request's
 * own. A request the story does not hold fails the mock as it loads rather than reading a default.
 */
function storyPinPlaced(placedBy: { renewedBy: string; requestId: string }): { at: string; usedAt: string } {
  return { at: storyRequest(placedBy.requestId).at, usedAt: storyRequest(placedBy.renewedBy).at };
}

/** A request the story holds. One it does not hold fails the mock as it loads rather than reading a default. */
function storyRequest(id: string): RequestLog {
  const r = requestById.get(id);
  if (r === undefined) throw new Error(`the story holds no request ${id}`);
  return r;
}

/**
 * The end of the newest request that the main agent of a session had answered on an account. A probe is left out, since
 * it renews the pin and leaves the cache as it found it.
 */
function newestAnswerOn(keyId: string, sessionId: string, accountId: string): string | null {
  const ends = REQUESTS.flatMap((r) => {
    const last = servingAttempt(r);
    const answered = r.kind !== 'probe' && r.keyId === keyId && r.sessionId === sessionId && r.agentId === 'main' && last?.accountId === accountId && last.upstreamStatus === 200;
    return answered ? [attemptEnd(last)] : [];
  });
  return ends.length === 0 ? null : iso(Math.max(...ends));
}

/**
 * Each drawn row's first sighting, which the builders date its pins from: the router keeps it until the session's last
 * pin leaves, so an unpinned row holds none.
 */
const firstSighting = new Map<Session, string | null>();
/**
 * Each drawn row's display pin as its last fold left it, its burn, its dollar burn and the fold's instant, which its
 * lines scale and decay on their own, since the row serves the figures decayed to the story's clock.
 */
const lastFold = new Map<Session, { at: string | null; burn: number | null; dollars: number | null }>();

export const SESSIONS: Session[] = sessionSpec.map((spec, i) => {
  // An entry past the story's session ids draws one the ledger never saw. The draw runs where the spec writes its id
  // out as well, so the rows after this one keep their rolls. A fixed row draws nothing, so it takes its own.
  const drawnId = spec.fixed ? spec.sessionId : i < SESSION_IDS.length ? SESSION_IDS[i] : rng.uuid();
  const written = spec.fixed ? spec.figures : null;
  const keyGeneration = Date.parse(keyById.get(spec.keyId)!.createdAt);
  const sessionId = spec.sessionId ?? drawnId;
  const seen = ledgered.get(activityOf(spec.keyId, keyGeneration, sessionId)) ?? null;
  const pinWrite = spec.pinnedBy === undefined ? null : storyPinWrite(spec.pinnedBy.requestId, spec.accountId, sessionId, spec.pinnedBy.stamped === true);
  const pinPlaced = spec.placedBy === undefined ? null : storyPinPlaced(spec.placedBy);
  const pinUsedAt = pinWrite?.at ?? pinPlaced?.usedAt ?? null;
  // Both rolls are drawn whatever the spec says, except on a fixed row, so pinning one session's idle
  // does not shift the seeded stream the rest of these fixtures read from.
  const createdRoll = (written ? written.createdMinutes : rng.int(8, 9 * 60)) * MINUTE;
  const idleRoll = spec.fixed ? spec.idleMinutes * MINUTE : spec.state === 'busy' ? rng.int(5, 90) * 1000 : rng.int(2, 40) * MINUTE;
  // Off the rows, a session the ledger holds no request of has no last use.
  const seenAt = seen === null ? null : Date.parse(seen);
  const lastFromRows = !spec.lastUseFromRows ? undefined : pinUsedAt === null ? seenAt : Math.max(seenAt ?? 0, Date.parse(pinUsedAt));
  const lastUsedAgo = typeof lastFromRows === 'number' ? NOW - lastFromRows : spec.idleMinutes === undefined ? Math.min(createdRoll, idleRoll) : spec.idleMinutes * MINUTE;
  const createdAgo = Math.max(createdRoll, lastUsedAgo + 5 * MINUTE);
  // A session the engine still keeps a recipe for is the one holding a pin on its account.
  const pinned = spec.state !== null;
  const unit = pinned ? burnUnitOf(spec.accountId) : null;
  // Drawn whatever the spec says, except on a fixed row, so pinning or the account's kind never shifts the stream the
  // rows after this one read. A row's burn is the per-hour figure the wire answers for its unit: dollars on the usd
  // roll, a share of the account's 5-hour window on the other. A plan pin answers the usd roll as its dollar burn.
  const burnRoll = written ? written.burn : rng.float(0.01, 0.9);
  const burnUsdRoll = written ? written.burnUsd : rng.float(0.01, 0.5);
  const burnAtRoll = ago((written ? written.burnAtMinutes : rng.int(2, 40)) * MINUTE);
  const lastInputRoll = written ? written.lastInputTokens : rng.int(500, 20_000);
  const lastRefreshRoll = ago((written ? written.lastRefreshMinutes : rng.int(2, 40)) * MINUTE);
  // A pin's cache was refreshed no earlier than the pin was written, so a roll that falls ahead of the write a move of
  // the story made reads the newest answer the pin's requests hold on its account instead.
  const lastRefresh = pinWrite !== null && Date.parse(lastRefreshRoll) < Date.parse(pinWrite.at) ? (newestAnswerOn(spec.keyId, sessionId, spec.accountId) ?? pinWrite.at) : lastRefreshRoll;
  // A pin on a model without a weight of its own has no burn to rank by, as the route answers it.
  const weighted = pinned && unit === 'window_share' && weightOf(TABLES, spec.model, 'seven_day') !== null;
  const folded = !pinned ? null : unit === 'usd' ? burnUsdRoll : weighted ? burnRoll : null;
  // The route serves the pin's burn decayed from its last fold to the read, its dollars by the same factor.
  const factor = pinned ? decayToNow(burnAtRoll, spec.keyId) : 1;
  const burn = folded === null ? null : folded * factor;
  // The ledger names a session's fork from its requests, so a session it never saw records none.
  const fork = REQUESTS.find((r) => r.sessionId === sessionId && r.forkedFrom !== null);
  // A drawn row's pin was written at its first sighting, unless a move or a placement of the story wrote it later.
  const firstSeen = pinned ? ago(createdAgo) : null;
  const row: Session = {
    ...SESSION_SAMPLE,
    sessionId,
    forkedFrom: fork?.forkedFrom ?? null,
    forkDepth: fork?.forkDepth ?? null,
    keyId: spec.keyId,
    accountId: spec.accountId,
    model: spec.model,
    historical: false,
    keyGeneration,
    pinnedAt: pinWrite?.at ?? pinPlaced?.at ?? firstSeen,
    // The placement the router's ledger holds for a pinned row, which the listing counts with the routing log's.
    placements: pinned ? 1 : 0,
    // An unpinned session's last use is what its own rows read.
    lastUsedAt: !pinned ? seen : lastFromRows === null ? null : ago(lastUsedAgo),
    lastRequestAt: seen,
    // Main's word, filled in from the recipes below, so a session whose main holds no recipe reads none.
    lifecycle: null,
    pinned,
    burn,
    burnAt: pinned ? servedBurnAt(burnAtRoll) : null,
    ...burnReadingOf(spec.accountId, spec.keyId, burn, unit),
    burnUsd: burnUsdOf(burnUsdRoll * factor, unit, spec.model),
    cacheTtlMs: pinned ? pinCacheTtlOf(sessionId) : null,
    effort: pinned && !spec.noEffort ? effortAt(i) : null,
    lastInputTokens: pinned ? lastInputRoll : null,
    lastRefreshAt: pinned ? lastRefresh : null,
    // Read off the display pin's line once the lines are written.
    ended: null,
    fast: pinned ? fastPin(spec.keyId, spec.model) : null,
    ...(pinned ? moveFiguresOf({ accountId: spec.accountId, agentId: 'main', keyId: spec.keyId, model: spec.model, sessionId }, lastInputRoll, pinCacheTtlOf(sessionId)) : NO_MOVE),
    ...NO_MARK,
    shed: pinWrite?.shed ?? null,
    ...(pinned ? runwayOf(i) : { runwayBounded: null, runwayMs: null }),
    // A row's lines are written once the recipes exist, so every row starts with none, and its agents' facts with them.
    agentContexts: [],
    agentFacts: [],
    facts: { ...NO_FACTS, ...spec.facts?.session },
    companion: spec.companion,
    // The companion's last report, as the engine dates one that stopped: a silent one's lapse, the session's end. A
    // fixed row writes its own, so it draws nothing.
    companionSince:
      written !== null
        ? written.lastReportSeconds === undefined
          ? null
          : ago(written.lastReportSeconds * 1000)
        : spec.companion === 'silent'
          ? ago(rng.int(12, 150) * MINUTE)
          : null,
    keepalive: {
      // Filled in from the recipes below, so a row and its agents cannot disagree.
      nextProbeAt: null,
    },
    // Set from the row's index rather than drawn, so the rows after this one keep their rolls: a reporting mod saw the
    // person seconds ago, a silent one when it last reported, and a session no mod registered has no presence.
    presenceAgeSeconds: spec.companion === 'reporting' ? 4 + ((i * 17) % 50) : spec.companion === 'silent' ? 900 + i * 60 : null,
    // Read off main's pin once the lines are written, before the row's last use moves to the newest across its pins.
    warm: null,
  };
  firstSighting.set(row, firstSeen);
  lastFold.set(row, { at: pinned ? burnAtRoll : null, burn: folded, dollars: unit === 'usd' ? folded : burnUsdOf(burnUsdRoll, unit, spec.model) });
  return row;
});

// A companion registered this session before its first request reached the router, so it holds no pin, no placement
// and no activity of its own. Its id is written out rather than drawn, which would shift every later roll in this file.
SESSIONS.push({
  ...SESSION_SAMPLE,
  accountId: null,
  agentContexts: [],
  agentFacts: [],
  burnUsd: null,
  companion: 'reporting',
  companionSince: null,
  effort: null,
  ended: null,
  facts: NO_FACTS,
  fast: null,
  forkDepth: null,
  forkedFrom: null,
  historical: false,
  keepalive: noRecipe(),
  keyGeneration: Date.parse(keyById.get('laptop')!.createdAt),
  keyId: 'laptop',
  lastRequestAt: null,
  lastUsedAt: null,
  lifecycle: 'registered',
  ...NO_MARK,
  model: null,
  ...NO_MOVE,
  pinned: false,
  pinnedAt: null,
  placements: 0,
  presenceAgeSeconds: 31,
  sessionId: '0f1d7c64-3b2a-4f58-9c7e-1a5b0e8d2f43',
  ...NO_STAMP,
  warm: null,
});

/** How long ago a probe still in flight went out. */
const PROBE_IN_FLIGHT_MS = 20_000;

// The state the engine reads a cause in, as `recipe_state` maps it with no pause.
const STATE_OF: Partial<Record<RecipeCause, RecipeState>> = {
  affinity_too_short: 'held',
  awaiting_cache_evidence: 'retained',
  compacted: 'ended',
  credits: 'held',
  expired: 'ended',
  horizon: 'held',
  idle_limit: 'ended',
  overage: 'held',
  parked_limit: 'ended',
  pin_moved: 'held',
  probe_pool_exhausted: 'held',
  probe_quota_exhausted: 'held',
  probing: 'retained',
  repeated_cold_probe: 'paused',
  scheduled: 'retained',
  session_opted_out: 'ended',
  spent: 'held',
  unsupported_shape: 'not_retained',
};
const stateOf = (cause: RecipeCause): RecipeState => STATE_OF[cause] ?? 'retained';

// The causes the story holds on the engine's retry ladder, each line `held`. A line held for its probe's 429 waits at
// least the answer's retry-after, and each cooldown of the story runs past the 300 s cap. A line held for its pool's
// untimed answer waits the doubled step, which reaches the cap from its fifth failure in a row. So each such line's
// next probe stands the cap after its own last one, ahead of the clock and inside the entry's TTL. The rung is read
// rather than drawn, so the rows after the line keep their rolls.
const LADDER_CAUSES: ReadonlySet<RecipeCause> = new Set<RecipeCause>(['probe_pool_exhausted', 'probe_quota_exhausted']);
const LADDER_CAP_MS = 300_000;

/**
 * What every line of a session the mod released whole reads, written over the drawn row. The row keeps its last request
 * and probe, the later of which dates the release inside the pool's cache TTL, since the router lists a released record
 * for one TTL after the release.
 */
const RELEASED_BY_THE_MOD: Partial<KeepaliveRecipe> = { agentState: 'released', cause: 'agent_released', deadline: null, nextProbeAt: null, parkKind: null, state: 'ended', wakeAt: null };
/**
 * What a subagent reads once its compaction completed: the engine stopped its recipe, and no probe goes out until the
 * agent's next main turn arms one for the new context. Written over the drawn row, so the rows after it keep their
 * rolls.
 */
const COMPACTED: Partial<KeepaliveRecipe> = { cause: 'compacted', lastGuard: 'compaction', nextProbeAt: null, state: 'ended' };
/**
 * What main reads on a session the mod ended, written over the drawn row. The end releases every agent, so main reads
 * released as an ended main does, and its subagents read as the mod released them.
 */
const ENDED_WITH_THE_SESSION: Partial<KeepaliveRecipe> = { agentState: 'released', cause: 'session_ended', deadline: null, nextProbeAt: null, parkKind: null, state: 'ended', wakeAt: null };

/**
 * A drawn story's pinned subagent at place `j` past main: the story's own, the one its request rows name. A uuid is
 * drawn and left unused, so the rolls after it stay, and a table short of the story's agents fails the mock as it
 * loads rather than reading a default.
 */
function storySubagent(i: number, j: number): string {
  rng.uuid();
  const agent = STORY_AGENTS[i]?.[j - 1];
  if (agent === undefined) throw new Error(`session story ${i} pins more subagents than its table names`);
  return agent.id;
}

/** A written session's subagent at place `j` past main, its list starting past the drawn pair. */
const writtenSubagentAt = (spec: Pick<(typeof sessionSpec)[number], 'writtenSubagents'>, j: number) => (j >= 2 ? spec.writtenSubagents?.[j - 2] : undefined);

export const RECIPES: KeepaliveRecipe[] = SESSIONS.flatMap((pin, i) => {
  // The registered session, listed after the spec's rows, holds no recipe.
  const mainState = i < sessionSpec.length ? sessionSpec[i].state : null;
  if (mainState === null) return [];
  const sub = sessionSpec[i].subagents;
  const spec = sessionSpec[i];
  // A fixed row's agents are written, so they draw nothing.
  const fixedAgents = spec.fixed ? spec.figures.agents : null;
  const agents = fixedAgents ? fixedAgents.map((a) => a.agentId) : Array.from({ length: spec.agents }, (_, j) => (j === 0 ? 'main' : storySubagent(i, j)));
  return agents.map((agentId, j) => {
    // A parked agent is probed like a busy one until one of the park's ceilings stops it.
    const held = j === 0 ? (spec.cause ?? null) : j === 1 ? (spec.subagentHold ?? null) : null;
    // A busy main has its probe in flight, and every other agent nothing holds waits for its scheduled one.
    const cause: RecipeCause = held ?? (mainState === 'busy' && j === 0 ? 'probing' : 'scheduled');
    const state = stateOf(cause);
    const model = j === 0 ? sessionSpec[i].model : sub?.model ?? sessionSpec[i].model;
    // The engine lists an agent once it has sent a request. The roll is drawn whatever it gives, so the rows after this
    // one keep theirs, and a subagent it leaves without one sent its request three and a half minutes ago, off the
    // whole minutes its last probe falls on.
    const rolled = fixedAgents ? ago(fixedAgents[j].lastRequestMinutes * MINUTE) : j === 0 ? pin.lastRequestAt : rng.int(0, 4) === 0 ? null : ago(rng.int(1, 40) * MINUTE);
    // A subagent's newest request in the seed re-armed its recipe where it came after the roll.
    const seeded = j === 0 ? undefined : newestRequestAt.get(`${activityOf(pin.keyId, pin.keyGeneration, pin.sessionId)}|${agentId}`);
    const drawnLast = rolled ?? ago(3 * MINUTE + 30_000);
    const lastRequestAt = seeded !== undefined && Date.parse(seeded) > Date.parse(drawnLast) ? seeded : drawnLast;
    const accountId = j === 0 ? pin.accountId : sub?.accountId ?? pin.accountId;
    const ttlSeconds = model.includes('haiku') ? 300 : 3_600;
    // Main reads its session's lifecycle word, a subagent whose park ran out is still parked, and the others are busy.
    const agentState: Lifecycle = j === 0 ? mainState : cause === 'parked_limit' ? 'parked' : 'busy';
    // The two parked mains: one on a background command with its deadline, one on a wakeup three days out. Neither
    // time is drawn, so the rows after them keep their rolls.
    const parkKind = agentState !== 'parked' ? null : cause === 'horizon' ? 'scheduled' : 'background';
    const deadline = agentState === 'parked' && j === 0 && cause !== 'horizon' ? ahead(72 * MINUTE + 30_000) : null;
    const wakeAt = parkKind === 'scheduled' ? ahead(3 * 24 * 60 * MINUTE + 30_000) : null;
    // An agent whose request still awaits its evidence has not been probed since, the engine probes no agent whose
    // session holds the cache warming off, a probe in flight was dispatched at its due time, which the engine records
    // as the last probe, and the rest were probed minutes ago.
    const writtenProbe = fixedAgents?.[j].lastProbeMinutes;
    const lastProbeAt = writtenProbe !== undefined ? ago(writtenProbe * MINUTE) : cause === 'awaiting_cache_evidence' || cause === 'session_opted_out' ? null : cause === 'probing' ? ago(PROBE_IN_FLIGHT_MS) : ago((((i * 3 + j) % 4) + 1) * MINUTE);
    const probesToday = lastProbeAt === null ? 0 : ((i * 7 + j * 3) % 9) + 1;
    // A line held for its probe's 429 was scheduled before that probe, so its draw is taken and set aside, and the rows
    // after it keep their rolls. A line held for its pool's answer takes none, since the stream takes no draw at its
    // place.
    const scheduledAt = !fixedAgents && (cause === 'scheduled' || cause === 'probe_quota_exhausted') ? ahead(rng.int(20, 280) * 1000) : null;
    // The written session's rows past its drawn pair are written, so a reader meets every lifecycle word. A session the
    // mod released whole keeps its draws, so the rows after it keep theirs.
    const written = sessionSpec[i].released
      ? RELEASED_BY_THE_MOD
      : sessionSpec[i].finished
        ? j === 0
          ? ENDED_WITH_THE_SESSION
          : RELEASED_BY_THE_MOD
        : writtenSubagentAt(spec, j) ?? (spec.compactedAgent === j ? COMPACTED : undefined);
    return {
      ...RECIPE_SAMPLE,
      accountId,
      agentId,
      agentState,
      cause,
      creditedToday: Math.floor(probesToday / 3),
      deadline,
      keyId: pin.keyId,
      // The last real request re-armed this recipe; whatever else holds its next probe now is the gate's
      // cause, not the guard's, which the mock has no event sequence to distinguish further.
      lastGuard: 'rearm',
      lastProbeAt,
      lastRequestAt,
      model,
      // A probe in flight leaves the due time it went out at, which the engine keeps showing until it lands.
      nextProbeAt: fixedAgents ? (fixedAgents[j].nextProbeSeconds === null ? null : ahead(fixedAgents[j].nextProbeSeconds * 1000)) : cause === 'scheduled' ? scheduledAt : cause === 'probing' ? ago(PROBE_IN_FLIGHT_MS) : LADDER_CAUSES.has(cause) && lastProbeAt !== null ? iso(Date.parse(lastProbeAt) + LADDER_CAP_MS) : null,
      parkKind,
      poolId: keyById.get(pin.keyId)?.poolId ?? 'all',
      probesToday,
      sessionId: pin.sessionId,
      state,
      ttlSeconds,
      wakeAt,
      ...written,
    };
  });
});

/**
 * The failover session's subagent whose auxiliary call moved its pin to nightly. Its recipe keeps primary, the account
 * its last turn answered on, and the gate holds it with its schedule armed, so its next probe reads the due time that
 * the probe the move overtook went out at. Written behind the rolls, so every drawn row and recipe keeps its own.
 */
{
  const turn = storyRequest(MOVED_PIN_TURN_ID);
  const call = storyRequest(MOVED_PIN_CALL_ID);
  RECIPES.push({
    ...RECIPE_SAMPLE,
    accountId: servingAttempt(turn)!.accountId,
    agentId: call.agentId!,
    agentState: 'busy',
    cause: 'pin_moved',
    creditedToday: 0,
    deadline: null,
    keyId: call.keyId!,
    lastGuard: 'auxiliary',
    lastProbeAt: storyRequest(MOVED_PIN_PROBE_ID).at,
    lastRequestAt: call.at,
    model: call.canonicalModel,
    nextProbeAt: iso(MOVED_PIN_PROBE_DUE),
    parkKind: null,
    poolId: keyById.get(call.keyId!)?.poolId ?? 'all',
    probesToday: 1,
    sessionId: call.sessionId!,
    state: stateOf('pin_moved'),
    ttlSeconds: 300,
    wakeAt: null,
  });
}

/**
 * The account and the instant of the story's newest move of a recipe's pin key, for a recipe the gate holds under
 * `pin_moved`, whose pin stands off the account it holds. Every other recipe's pin stands on that account.
 */
function movedPinOf(r: Pick<KeepaliveRecipe, 'agentId' | 'cause' | 'keyId' | 'model' | 'sessionId'>): { accountId: string; at: string } | null {
  if (r.cause !== 'pin_moved') return null;
  const moves = [...REQUEST_EVENTS.values()].flat().filter((e) => e.kind === 'pin_migrated' && e.keyId === r.keyId && e.sessionId === r.sessionId && e.model === r.model && e.detail?.agent_id === r.agentId);
  const newest = moves.sort((a, b) => Date.parse(b.at) - Date.parse(a.at))[0];
  if (newest === undefined || newest.accountId === null) throw new Error(`the story moves no pin of ${r.sessionId} ${r.agentId} off its recipe`);
  return { accountId: newest.accountId, at: newest.at };
}

/**
 * A session's summary as the admin route reads it off the session's recipes. Its next probe is the soonest its agents
 * have scheduled: an agent awaiting cache evidence contributes no time even though it is armed, so a session left with
 * only those reads as pending rather than unscheduled.
 */
const summaryOf = (sessionId: string): Session['keepalive'] => {
  const own = RECIPES.filter((r) => r.sessionId === sessionId);
  const scheduled = own.flatMap((r) => (r.nextProbeAt === null ? [] : [r.nextProbeAt])).sort();
  return { nextProbeAt: scheduled[0] ?? null };
};

for (const pin of SESSIONS) {
  pin.keepalive = summaryOf(pin.sessionId);
  // The row's word is main's alone, so a row whose main holds no recipe keeps the word it was written with: the
  // registered-only session's `registered`, none elsewhere.
  pin.lifecycle = RECIPES.find((r) => r.sessionId === pin.sessionId && r.agentId === 'main')?.agentState ?? pin.lifecycle;
}

// The router stamps a pin at a probe's selection, in flight or answered, so a pinned row takes its main recipe's last
// probe where that came later than its last use, rolled or read off its rows. A line on the retry ladder took its last
// probe's answer from the selector, which sent nothing and stamped nothing, so such a row keeps its last use.
SESSIONS.forEach((pin) => {
  if (!pin.pinned || pin.lastUsedAt === null) return;
  const main = RECIPES.find((r) => r.keyId === pin.keyId && r.sessionId === pin.sessionId && r.agentId === 'main');
  if (!main?.lastProbeAt || LADDER_CAUSES.has(main.cause) || main.accountId !== pin.accountId) return;
  pin.lastUsedAt = laterOf(pin.lastUsedAt, main.lastProbeAt);
});

/**
 * A subagent pin's tokens, set from its session's index and its line's place rather than drawn, so the rows after it
 * keep their rolls.
 */
const agentTokens = (i: number, j: number) => 2_000 + ((i * 31 + j * 17) % 23) * 1_000;
/** A recipe that names its account, whose agent holds a pin there. */
const onAccount = (r: KeepaliveRecipe): r is KeepaliveRecipe & { accountId: string } => r.accountId !== null;
/**
 * Whether the router holds the pin of the agent that recipe `r` at place `j` of drawn row `i` names as ended. It marks
 * a pin ended on the mod's report that it released the agent and on a subagent's turn that stopped with `end_turn`. A
 * session's end releases each agent's recipe and sends the router no report, so its pins keep no ended mark.
 */
const pinEndedOf = (i: number, r: Pick<KeepaliveRecipe, 'cause'>, j: number): boolean =>
  r.cause === 'agent_released' || (i < sessionSpec.length && writtenSubagentAt(sessionSpec[i], j) === ENDED_SUBAGENT);
/**
 * The pin of the agent that recipe `r` at place `j` of drawn row `i` names, as the sessions route draws its line and
 * the pins route answers it, so the two routes read one `ended` and one set of move figures. Main's pin reads the row's
 * own figures. A subagent's pin takes the companion's five-minute TTL, its place's effort word and its last request as
 * its last use and its last refresh, and a moved pin's row is the one its move wrote.
 */
const agentPinOf = (row: Session, i: number, r: KeepaliveRecipe & { accountId: string }, j: number): UndatedLine => {
  const fold = lastFold.get(row);
  const display = { burn: fold?.burn ?? null, dollars: fold?.dollars ?? null, lastInputTokens: row.lastInputTokens };
  const moved = movedPinOf(r);
  const own = { accountId: moved?.accountId ?? r.accountId, agentId: r.agentId, model: r.model };
  const ended = pinEndedOf(i, r, j);
  if (r.agentId === 'main') {
    const line = { ...own, burnAt: fold?.at ?? null, effort: row.effort, ended, lastInputTokens: row.lastInputTokens, lastUsedAt: row.lastUsedAt ?? r.lastRequestAt, lifecycle: r.agentState, pinnedAt: row.pinnedAt };
    return pinLine(line, row.keyId, row.sessionId, display, row.lastRefreshAt);
  }
  const line = { ...own, effort: effortAt(j), ended, lastInputTokens: agentTokens(i, j), lastUsedAt: r.lastRequestAt, lifecycle: r.agentState, ...(moved === null ? {} : { pinnedAt: moved.at }) };
  return pinLine(line, row.keyId, row.sessionId, servedBeside(display, line, firstSighting.get(row) ?? null), line.lastUsedAt);
};
/**
 * The pins of each row that the router holds and the row lists no line for, which the pins route answers beside the
 * lines: a released agent's on a row a companion reports, and main's auxiliary pin.
 */
const UNLISTED_PINS = new Map<Session, UndatedLine[]>();
// The route lists one line per pin: on a row a companion reports, one of an agent the engine holds and has not
// released, main's reading the row's own pin, so a session the mod released whole lists none, and on any other row
// every pin the listing keeps inside the non-companion timeout, released or not, so the session the mod ended lists its
// released agents. Every recipe the story writes stands on a pin, and a session no companion ever reported holds main's
// alone. The row and main's line read warm off the row's last refresh, as the route reads both off one pin, before the
// row's last use moves to the newest across its pins.
SESSIONS.forEach((pin, i) => {
  const recipes = RECIPES.filter((r) => r.keyId === pin.keyId && r.sessionId === pin.sessionId);
  const firstSeen = firstSighting.get(pin) ?? null;
  const pins = recipes.flatMap((r, j) => (onAccount(r) ? [{ line: agentPinOf(pin, i, r, j), listed: r.agentState !== 'released' || pin.companion === 'none' }] : []));
  pin.agentContexts = datedLines(
    firstSeen,
    activityOf(pin.keyId, pin.keyGeneration, pin.sessionId),
    pins.flatMap((p) => (p.listed ? [p.line] : [])),
  );
  // An unlisted pin keeps its line's ended mark: the router holds the pin of an agent that the mod released as ended,
  // and a session's end, which releases an agent too, marks no pin.
  UNLISTED_PINS.set(
    pin,
    pins.flatMap((p) => (p.listed ? [] : [p.line])),
  );
  // The spec names a subagent's facts by its place, so they take the id the recipes drew for that place, and an earlier
  // agent's by its id, since no recipe draws one. The route answers one entry per agent with a stored set, in agent id
  // order.
  const reported = i < sessionSpec.length ? sessionSpec[i].facts : undefined;
  if (reported !== undefined) {
    const lined = recipes.flatMap((r, j) => {
      const facts = reported.agents?.[j];
      return facts ? [{ agentId: r.agentId, ...facts }] : [];
    });
    pin.agentFacts = [...lined, ...(reported.earlier ?? [])].sort((a, b) => byteOrder(a.agentId, b.agentId));
  }
  pin.warm = pin.pinned ? warmAt(warmUntilOf(pin.lastRefreshAt, pin.cacheTtlMs)) : null;
  // The row's ended mark is its display pin's line's. A row the mod released whole lists no line, and the router holds
  // the pin of an agent that the mod released as ended.
  const ownLine = pin.agentContexts.find((c) => c.agentId === pin.agentId && c.model === pin.model);
  pin.ended = !pin.pinned ? null : ownLine !== undefined ? ownLine.ended : recipes.some((r, j) => r.agentId === pin.agentId && pinEndedOf(i, r, j));
  pin.lastUsedAt = newestUse(pin.lastUsedAt, pin.agentContexts);
});

/**
 * A written row that names no fork, no cycle, no runway, no mark and no shed stamp: its lines priced as the route
 * prices a display pin's, the line its caller lists first at the row's burn and its dollar burn per hour as folded at
 * its last use, the burn itself on a dollar-billed account and `dollars` on a plan, and the others beside it, and dated
 * from its first sighting, its figures, its served burn and reading and its pin's instant off that first line, warm
 * off it and last used at the newest of its lines, each line refreshed at its last use unless it names its refresh in
 * `refreshedAt`, its summary off its session's recipes, and its last request off the activity fold. The engine holds no
 * record of its main, so it reads no lifecycle word. A row passes a companion or a presence only where a mod reports.
 */
const writtenRow = (
  { dollars, firstSeen, ...row }: Pick<Session, 'agentId' | 'burn' | 'keyId' | 'sessionId'> & Partial<Pick<Session, 'companion' | 'presenceAgeSeconds'>> & { accountId: string; dollars?: number; firstSeen: string },
  lines: (Parameters<typeof pinLine>[0] & { refreshedAt?: string })[],
): Session => {
  const keyGeneration = Date.parse(keyById.get(row.keyId)!.createdAt);
  const key = activityOf(row.keyId, keyGeneration, row.sessionId);
  const display = { burn: row.burn, dollars: burnUnitOf(row.accountId) === 'usd' ? row.burn : (dollars ?? null), lastInputTokens: lines[0].lastInputTokens };
  const priced = lines.map(({ refreshedAt, ...l }, j) => pinLine(l, row.keyId, row.sessionId, j === 0 ? display : servedBeside(display, l, firstSeen), refreshedAt ?? l.lastUsedAt));
  const line = priced[0];
  const agentContexts = datedLines(firstSeen, key, priced);
  const seen = ledgered.get(key);
  return {
    ...SESSION_SAMPLE,
    ...row,
    accountBound: line.accountBound,
    agentContexts,
    agentFacts: [],
    burn: line.burn,
    burnAt: line.burnAt,
    burnEquivalent: line.burnEquivalent,
    burnUnit: line.burnUnit,
    burnUsd: line.burnUsd,
    burnWindow: line.burnWindow,
    cacheTtlMs: line.cacheTtlMs,
    companion: row.companion ?? 'none',
    companionSince: null,
    cycleMs: null,
    effort: line.effort,
    ended: line.ended,
    factor: null,
    facts: NO_FACTS,
    fast: line.fast,
    forkDepth: null,
    forkedFrom: null,
    historical: false,
    keepalive: summaryOf(row.sessionId),
    keyGeneration,
    lastInputTokens: line.lastInputTokens,
    lastRefreshAt: lines[0].refreshedAt ?? line.lastUsedAt,
    lastRequestAt: seen ?? null,
    lastUsedAt: newestUse(null, priced),
    lifecycle: null,
    ...NO_MARK,
    model: line.model,
    moveCost: line.moveCost,
    moveDropCost: line.moveDropCost,
    moveDropSize: line.moveDropSize,
    moveSize: line.moveSize,
    pinned: true,
    pinnedAt: agentContexts.find((l) => l.agentId === line.agentId && l.model === line.model)!.pinnedAt,
    // The placement the router's ledger holds for the pin, which the listing counts with the routing log's.
    placements: 1,
    presenceAgeSeconds: row.presenceAgeSeconds ?? null,
    runwayBounded: null,
    runwayMs: null,
    ...NO_STAMP,
    speed: null,
    warm: line.warm,
  };
};

/**
 * A row the router lists off the ledger's rows alone: no pin, no recipe and no line, the activity fold's last request
 * its own. Its last use is that request, as the decoder reads a row the route answers with no pin.
 */
const ledgerOnlyRow = (fields: Pick<Session, 'accountId' | 'historical' | 'keyGeneration' | 'keyId' | 'lastRequestAt' | 'model' | 'sessionId'>): Session => ({
  ...SESSION_SAMPLE,
  agentContexts: [],
  agentFacts: [],
  agentId: null,
  burn: null,
  burnAt: null,
  burnEquivalent: null,
  burnUnit: null,
  burnUsd: null,
  burnWindow: null,
  cacheTtlMs: null,
  companion: 'none',
  companionSince: null,
  effort: null,
  ended: null,
  facts: NO_FACTS,
  fast: null,
  forkDepth: null,
  forkedFrom: null,
  keepalive: noRecipe(),
  lastInputTokens: null,
  lastRefreshAt: null,
  lastUsedAt: fields.lastRequestAt,
  lifecycle: null,
  ...NO_MARK,
  ...NO_MOVE,
  pinned: false,
  pinnedAt: null,
  placements: 0,
  presenceAgeSeconds: null,
  ...NO_STAMP,
  warm: null,
  ...fields,
});
/**
 * A session's ledger-only row off `rows`, its account, its model and its last request those of its newest request that
 * is no probe under its key, generation and session, and none where `rows` hold no such request.
 */
const ledgerRowOf = (rows: readonly RequestLog[], s: Pick<Session, 'keyGeneration' | 'keyId' | 'sessionId'>, historical: boolean): Session | null => {
  // The seed runs newest first, and the written rows keep its order.
  const newest = rows.find((r) => r.kind !== 'probe' && r.keyId === s.keyId && r.keyGeneration === s.keyGeneration && r.sessionId === s.sessionId);
  return newest === undefined ? null : ledgerOnlyRow({ accountId: newest.accountId, historical, keyGeneration: s.keyGeneration, keyId: s.keyId, lastRequestAt: newest.at, model: newest.canonicalModel, sessionId: s.sessionId });
};

/**
 * A session's row once the operator deleted its key, read off `written`, the requests the ledger had written by the
 * delete: the store deletes the key's pins with the key, so the route lists the row off the ledger alone, its account
 * and model those of its newest request, and a session the ledger holds no request of leaves the list. The story writes
 * no request of a deleted key behind its delete, so these are the requests the list reads at each later instant, though
 * the router's delete does not stop a request already in flight. The engine holds the key's sessions until its next
 * tick, up to a second behind the delete, and for that second the route also lists each of them that the ledger holds
 * no request of, as a historical row with no account. The mock ends them at the delete, so for that second it leaves
 * those rows out.
 */
export const rowsOfDeletedKey = (s: Session, written: readonly RequestLog[]): Session[] => {
  const row = ledgerRowOf(written, s, true);
  // The ledger names the fork off the session's requests, which outlive the key.
  return row === null ? [] : [{ ...row, forkDepth: s.forkDepth, forkedFrom: s.forkedFrom }];
};

/**
 * Studio's notebook sessions in work, pinned there while the slow-down holds it: the slowed one with its subagent,
 * whose main pin has the cycle its turns since the placement give, the two the round marked, the one whose turn opened
 * the span and the account-bound one placed after the round, whose pins have none yet. Each burn is the main pin's
 * demand at the account's half speed, and each `burnUsd` that pin's dollars an hour. No recipe warms them. They are
 * written after the recipes rather than rolled, so every drawn row and recipe keeps its rolls.
 */
const SLOWED_PINS: { agents: { agentId: string; model: string }[]; burn: number; burnUsd: number; cycleMs: number | null; sessionId: string }[] = [
  { agents: [{ agentId: 'main', model: 'claude-sonnet-5-5' }, { agentId: '7b2e', model: HAIKU }], burn: 0.0125, burnUsd: 0.31, cycleMs: 42_000, sessionId: SLOWED_SESSION_ID },
  { agents: [{ agentId: 'main', model: 'claude-sonnet-5-5' }], burn: 0.0135, burnUsd: 0.33, cycleMs: null, sessionId: MARKED_SESSION_IDS[0] },
  { agents: [{ agentId: 'main', model: 'claude-sonnet-5-5' }], burn: 0.014, burnUsd: 0.35, cycleMs: null, sessionId: MARKED_SESSION_IDS[1] },
  { agents: [{ agentId: 'main', model: 'claude-sonnet-5-5' }], burn: 0.0113, burnUsd: 0.28, cycleMs: null, sessionId: OPENING_SESSION_IDS[1] },
  { agents: [{ agentId: 'main', model: 'claude-sonnet-5-5' }], burn: 0.0102, burnUsd: 0.25, cycleMs: null, sessionId: ACCOUNT_BOUND_SESSION_ID },
];
// Every pin of the account reads the window's runway: until its burn carries the utilization to the pool's ceiling.
const studioFive = accountById.get('studio')?.windows.find((w) => w.id === '5h');
const studioRunwayMs = studioFive?.burn && studioFive.utilization !== null ? Math.round(((poolById.get('work')!.fiveHourCeiling - studioFive.utilization) / studioFive.burn) * HOUR) : null;
const studioSlowed = servedFigures('studio', NOW, poolById.get('work')!);
/**
 * The mark studio's stretch round set on the two sessions it marked, the round of 14:05 that the 5-hour window fired.
 * Neither session sent a request since, so the mark stands.
 */
const STRETCH_MARK: Pick<Session, 'mark'> = { mark: { at: iso(STRETCH_ROUND.at), on: STRETCH_ROUND.accountId, window: 'five_hour' } };
SLOWED_PINS.forEach((pin, k) => {
  // The rows run newest first, and the main agent's newest turn is the pin's last use.
  const rows = REQUESTS.filter((r) => r.kind !== 'probe' && r.keyId === 'notebook' && r.sessionId === pin.sessionId);
  const main = rows.filter((r) => r.agentId === 'main');
  const effort = effortAt(k);
  const firstSeen = main[main.length - 1].at;
  // Each agent's pin reads its own newest turn, which wrote the hour's cache.
  const lines = pin.agents.map(({ agentId, model }, j) => {
    const own = rows.filter((r) => r.agentId === agentId);
    const newest = own[0];
    return {
      accountId: 'studio',
      agentId,
      effort: agentId === 'main' ? effort : effortAt(k + j),
      ended: false,
      lastInputTokens: promptOf(newest.usage),
      lastUsedAt: servingAttempt(newest)!.startedAt,
      lifecycle: null,
      model,
    };
  });
  SESSIONS.push({
    ...writtenRow({ accountId: 'studio', agentId: 'main', burn: pin.burn, dollars: pin.burnUsd, firstSeen, keyId: 'notebook', sessionId: pin.sessionId }, lines),
    cycleMs: pin.cycleMs,
    factor: studioSlowed.factor,
    lastRequestAt: rows[0].at,
    runwayBounded: studioRunwayMs !== null,
    runwayMs: studioRunwayMs,
    speed: studioSlowed.speed,
    ...(MARKED_SESSION_IDS.some((id) => id === pin.sessionId) ? STRETCH_MARK : {}),
  });
});

/**
 * The contract seat's batch session on fleet, whose pool keeps no cache warm, so no recipe warms it. Its main agent
 * holds one pin per model it sent, and a pin stands while its last turn is inside the pool's affinity window: the Opus
 * pin of the newest turn, where the earlier Sonnet turns' pin has lapsed. Written after the recipes rather than rolled,
 * so every drawn row and recipe keeps its rolls, the burn a figure of its own in dollars, as a usage-based seat's burn
 * reads.
 */
const fleetRows = REQUESTS.filter((r) => r.kind !== 'probe' && r.keyId === 'fleet' && r.sessionId === FLEET_SESSION_ID);
const fleetPool = keyById.get('fleet')!.poolId;
const fleetTtlMs = affinityTtlMsOf(fleetPool);
// The rows run newest first, so each model's first row is its pin's last turn.
const FLEET_PINS = [...new Set(fleetRows.map((r) => r.canonicalModel))]
  .map((model) => fleetRows.find((r) => r.canonicalModel === model)!)
  .filter((last) => NOW - Date.parse(servingAttempt(last)!.startedAt) <= fleetTtlMs);
/** The fleet session's burn, in dollars per hour as a usage-based seat's reads. */
const FLEET_BURN = 0.36;
// The newest model's pin is the one the route lists the session's main agent under.
if (FLEET_PINS.length > 0) {
  const last = FLEET_PINS[0];
  const lastAttempt = servingAttempt(last)!;
  // The oldest turn of the last unbroken run of turns, newest first: a turn that came once the pin before it had passed
  // the affinity window found no pin, so the router wrote one again from it.
  const runStart = (turns: readonly RequestLog[]): RequestLog => {
    let first = turns[0];
    for (const r of turns.slice(1)) {
      if (Date.parse(first.at) - Date.parse(servingAttempt(r)!.startedAt) > fleetTtlMs) break;
      first = r;
    }
    return first;
  };
  // The session is first seen at its run's oldest turn on any model, and each model's pin was written at that model's.
  const firstSeen = runStart(fleetRows);
  // One line per standing pin, each read off its model's last turn as the row reads the newest one.
  const lines = FLEET_PINS.map((pin, j) => {
    const attempt = servingAttempt(pin)!;
    const pinnedAt = runStart(fleetRows.filter((r) => r.canonicalModel === pin.canonicalModel)).at;
    return { accountId: attempt.accountId, agentId: 'main', effort: effortAt(j + 1), ended: false, lastInputTokens: promptOf(pin.usage), lastUsedAt: attempt.startedAt, lifecycle: null, model: pin.canonicalModel, pinnedAt };
  });
  SESSIONS.push(writtenRow({ accountId: lastAttempt.accountId, agentId: 'main', burn: FLEET_BURN, firstSeen: firstSeen.at, keyId: 'fleet', sessionId: FLEET_SESSION_ID }, lines));
}

/**
 * The approved Ultra's session on bench, pinned on its newest turn, a Sonnet one inside the pool's affinity window:
 * its poll names no 5-hour window, so its burn is a share of the 7-day window. The workshop pool keeps no cache warm,
 * so no recipe warms it. Written after the recipes rather than rolled, so every drawn row and recipe keeps its rolls,
 * the burn and its dollars figures of their own.
 */
const workshopRows = REQUESTS.filter((r) => r.kind !== 'probe' && r.keyId === 'bench' && r.sessionId === WORKSHOP_SESSION_ID);
const WORKSHOP_PIN = workshopRows[0];
const WORKSHOP_BURN = 0.0021;
const WORKSHOP_BURN_USD = 0.19;
{
  const lastAttempt = servingAttempt(WORKSHOP_PIN)!;
  const firstSeen = workshopRows[workshopRows.length - 1].at;
  // A pin holds one model, so the line's pin was written at the session's oldest turn on that model.
  const pinnedAt = workshopRows.filter((r) => r.canonicalModel === WORKSHOP_PIN.canonicalModel).at(-1)!.at;
  const line = { accountId: 'workshop', agentId: 'main', effort: effortAt(1), ended: false, lastInputTokens: promptOf(WORKSHOP_PIN.usage), lastUsedAt: lastAttempt.startedAt, lifecycle: null, model: WORKSHOP_PIN.canonicalModel, pinnedAt };
  SESSIONS.push(writtenRow({ accountId: 'workshop', agentId: 'main', burn: WORKSHOP_BURN, dollars: WORKSHOP_BURN_USD, firstSeen, keyId: 'bench', sessionId: WORKSHOP_SESSION_ID }, [line]));
}

// The earlier holder of the `desktop` key id, which the analytics story draws as `desktop (deleted, created ...)`
// with the creation minute in the viewer's zone: the router lists a holder's session while its last request is inside
// the session expiry and the non-companion timeout, with that holder's own requests and no pin or recipe. The holder's
// last request ran on the session the live key holds now, so the row carries that id, and it joins the list after the
// probe times are filled in, which would otherwise hand it that session's next probe.
const holderRow = ledgerRowOf(REQUESTS, { keyGeneration: DELETED_HOLDER_GENERATION, keyId: 'desktop', sessionId: SESSION_IDS[0] }, true);
if (holderRow?.lastRequestAt && insideSessionExpiry(holderRow.lastRequestAt, NOW)) SESSIONS.push(holderRow);

// The desktop session whose every request the key's own guardrail refused before any account was asked. The router
// lists it off the ledger's rows with no pin and no recipe, so a drifting or blocked client stays visible, and the
// activity fold gives it the newest refusal as its last request, and only while that refusal is inside the router's
// session expiry, the non-companion timeout leaving it out of the route's list sooner.
const blockedGeneration = Date.parse(keyById.get('desktop')!.createdAt);
const blockedLast = REQUESTS.find((r) => r.kind !== 'probe' && r.keyId === 'desktop' && r.keyGeneration === blockedGeneration && r.sessionId === BLOCKED_SESSION_ID);
function refusalOnlyRows(): Session[] {
  if (!blockedLast || !insideSessionExpiry(blockedLast.at, NOW)) return [];
  const row = ledgerOnlyRow({ accountId: null, historical: false, keyGeneration: blockedGeneration, keyId: 'desktop', lastRequestAt: blockedLast.at, model: blockedLast.canonicalModel, sessionId: BLOCKED_SESSION_ID });
  // The fold over its rows, every one refused, answers what the listing answers for it, its newest refusal's user agent
  // among it, so the builder's row is the one listed.
  return [{ ...row, ...foldSessionSpend(REQUESTS, row, NOW) }];
}
SESSIONS.push(...refusalOnlyRows());

/**
 * Two sessions written after the recipes rather than rolled, so every drawn row and recipe keeps its rolls, and neither
 * has reached the ledger. On the laptop, a companion session whose main has sent auxiliary calls alone while its
 * subagent works: main's auxiliary pin draws no line and holds no recipe, so the row falls back to the subagent's
 * conversation pin and the mod's one recipe is the subagent's. On fleet, a session no mod reports whose main and
 * subagent each hold a pin on the contract seat, the subagent's last used past its five-minute cache TTL.
 */
const AUXILIARY_ONLY_SESSION_ID = '3e9b7c21-4d58-4a06-8f1e-6c2a9d0b5e74';
const NO_MOD_SESSION_ID = 'b7e2d5a9-6c13-4f80-a4d2-9e1f3c7b0a68';
const auxiliaryOnlyRecipe: KeepaliveRecipe = {
  ...RECIPE_SAMPLE,
  accountId: 'overflow',
  agentId: 'd41c',
  agentState: 'busy',
  cause: 'scheduled',
  creditedToday: 0,
  deadline: null,
  keyId: 'laptop',
  lastGuard: 'rearm',
  lastProbeAt: null,
  lastRequestAt: ago(3 * MINUTE),
  model: 'claude-sonnet-4-5',
  nextProbeAt: ahead(75_000),
  parkKind: null,
  poolId: 'all',
  probesToday: 0,
  sessionId: AUXILIARY_ONLY_SESSION_ID,
  state: 'retained',
  ttlSeconds: 300,
  wakeAt: null,
};
RECIPES.push(auxiliaryOnlyRecipe);
const AUXILIARY_ONLY_CREATED_AT = ago(38 * MINUTE);
const AUXILIARY_ONLY_BURN = 0.05;
const auxiliaryOnlyLine = {
  accountId: 'overflow',
  agentId: auxiliaryOnlyRecipe.agentId,
  effort: 'high',
  ended: false,
  lastInputTokens: 23_400,
  lastUsedAt: auxiliaryOnlyRecipe.lastRequestAt,
  lifecycle: auxiliaryOnlyRecipe.agentState,
  model: auxiliaryOnlyRecipe.model,
};
const NO_MOD_CREATED_AT = ago(47 * MINUTE);
const NO_MOD_BURN = 0.22;
const noModMain = { accountId: 'contract', agentId: 'main', effort: 'max', ended: false, lastInputTokens: 41_200, lastUsedAt: ago(2 * MINUTE), lifecycle: null, model: 'claude-sonnet-5-5' };
const noModSubagent = { accountId: 'contract', agentId: '5c8e', effort: 'low', ended: false, lastInputTokens: 7_900, lastUsedAt: ago(25 * MINUTE), lifecycle: null, model: HAIKU };
const auxiliaryOnlyRow = writtenRow(
  {
    accountId: 'overflow',
    agentId: auxiliaryOnlyLine.agentId,
    burn: AUXILIARY_ONLY_BURN,
    companion: 'reporting',
    firstSeen: AUXILIARY_ONLY_CREATED_AT,
    keyId: 'laptop',
    presenceAgeSeconds: 9,
    sessionId: AUXILIARY_ONLY_SESSION_ID,
  },
  [auxiliaryOnlyLine],
);
// Main's auxiliary calls ride beside its subagent's turns, so its pin was last used at the subagent's last request. An
// auxiliary call reports no prompt size, so the pin's burn and its move's size read none.
const auxiliaryMain = { accountId: 'overflow', agentId: 'main', effort: null, ended: false, lastInputTokens: null, lastUsedAt: auxiliaryOnlyRecipe.lastRequestAt, lifecycle: null, model: HAIKU };
UNLISTED_PINS.set(auxiliaryOnlyRow, [pinLine(auxiliaryMain, 'laptop', AUXILIARY_ONLY_SESSION_ID, { burn: null, dollars: null }, auxiliaryMain.lastUsedAt)]);
const WRITTEN_SESSIONS = [
  auxiliaryOnlyRow,
  writtenRow({ accountId: 'contract', agentId: 'main', burn: NO_MOD_BURN, firstSeen: NO_MOD_CREATED_AT, keyId: 'fleet', sessionId: NO_MOD_SESSION_ID }, [noModMain, noModSubagent]),
];
SESSIONS.push(...WRITTEN_SESSIONS);

/**
 * Two laptop sessions the route lists off the ledger and the pins alone, written after the recipes rather than rolled,
 * so every drawn row and recipe keeps its rolls. No mod reports either, and the engine holds a session from a mod's
 * start report alone, so neither holds a recipe. The compacted conversation's pin was swept once all's affinity window
 * passed its last turn, so its row is the ledger's. The failover's subagent holds the pin its landing wrote on nightly,
 * live inside that window, and main's pins were swept hours before, so that pin is the row's. The landing stamps the
 * pin with its source, the turn's end renews it with no refresh, and the turn stopped with `end_turn`, so the router
 * holds the pin as ended. Its burn and its dollars are written figures, since the mock holds no fold of the pin.
 */
const LEDGER_TURN_ID = 'req_31f0b7c2-6e1';
const FAILOVER_TURN_ID = 'req_2e7c90a1-6d4';
const FAILOVER_PIN_BURN = 0.012;
const FAILOVER_PIN_BURN_USD = 0.03;
{
  const of = (id: string) => {
    const r = storyRequest(id);
    if (r.keyId === null || r.sessionId === null) throw new Error(`the story holds no session request ${id}`);
    return { ...r, keyId: r.keyId, sessionId: r.sessionId };
  };
  const turn = of(LEDGER_TURN_ID);
  SESSIONS.push(ledgerRowOf(REQUESTS, turn, false)!);
  const failover = of(FAILOVER_TURN_ID);
  const landing = servingAttempt(failover)!;
  const write = storyPinWrite(failover.id, landing.accountId, failover.sessionId, true);
  const line = {
    accountId: landing.accountId,
    agentId: failover.agentId!,
    effort: failover.shape.effort,
    ended: true,
    lastInputTokens: promptOf(failover.usage),
    lastUsedAt: iso(pinStampOf(failover, false)),
    lifecycle: null,
    model: failover.canonicalModel,
    pinnedAt: write.at,
    refreshedAt: write.at,
  };
  const row = writtenRow({ accountId: landing.accountId, agentId: line.agentId, burn: FAILOVER_PIN_BURN, dollars: FAILOVER_PIN_BURN_USD, firstSeen: failover.at, keyId: failover.keyId, sessionId: failover.sessionId }, [line]);
  SESSIONS.push({ ...row, shed: write.shed });
}
// Each row carries its own fold of the request seed at the story's clock, the spend its count lives in among it, so a
// read of the seed past the route's list finds what the route answers for the row.
for (const s of SESSIONS) Object.assign(s, foldSessionSpend(REQUESTS, s, NOW));

/**
 * A session's `spend`, `day`, `agentRequests`, `userAgent`, `firstRequestAt` and `lastInferenceAt` folded from the
 * request seed's rows on it, as the route folds the ledger's: a refused row passed over, a probe's figures apart in the
 * probes, every other row a client request priced as the request log prices it (`costUsd`, `costShares`,
 * `costEquivalent`) under its serving attempt's funding and one with no token priced nothing, the day the client
 * requests of the hours from the one holding `now - 24 h` by class, one entry per agent, and the user agent the newest
 * client request's, or the newest refusal's where every request was refused. An agent's entry sums every model it ran
 * and reads the model, the account, the type and the instant of its newest client request, or of an agent with probes
 * alone its newest probe's model and none of the other three. The two instants and each entry's first request read the
 * refused rows too: the first request that is no probe and the newest of kind `inference`, null where none stands.
 */
export function foldSessionSpend(rows: readonly RequestLog[], session: Pick<Session, 'keyGeneration' | 'keyId' | 'sessionId'>, now: number): Pick<Session, 'agentRequests' | 'day' | 'firstRequestAt' | 'lastInferenceAt' | 'spend' | 'userAgent'> {
  const emptyPart = (): SessionSpendPart => ({ billedUsd: {}, billedUsdByAccount: {}, equivalent: 0, requests: 0, shares: {}, usd: 0 });
  // A plan-funded request adds its Pro-week figure, and one that priced none leaves the sum unknown. Any other funding
  // bills its dollars, a request that cost nothing filing none. It answers whether it filed the dollars as billed, so
  // the map by account reads that one condition, as the ledger's does.
  const addMoney = (into: Pick<SessionSpendPart, 'billedUsd' | 'equivalent' | 'usd'>, r: RequestLog): boolean => {
    const funding = fundingOf(servingAttempt(r));
    into.usd += r.costUsd;
    if (funding === 'plan') {
      into.equivalent = into.equivalent === null || r.costEquivalent === null ? null : into.equivalent + r.costEquivalent;
      return false;
    }
    if (r.costUsd <= 0) return false;
    into.billedUsd[funding] = (into.billedUsd[funding] ?? 0) + r.costUsd;
    return true;
  };
  // The route prices no usage without a token, so such a request, a failed one among them, adds its count alone.
  const priced = (r: RequestLog) => tokensTotal(r.usage) > 0;
  // Every plan-funded request names its serving account under `shares`, a free one with no share in it.
  const add = (into: SessionSpendPart, r: RequestLog) => {
    into.requests += 1;
    if (!priced(r)) return;
    const serving = servingAttempt(r);
    // A request no attempt served costs nothing (`fundingOf`), so a billed one always names its account.
    if (addMoney(into, r) && serving !== null) into.billedUsdByAccount[serving.accountId] = (into.billedUsdByAccount[serving.accountId] ?? 0) + r.costUsd;
    if (serving === null || fundingOf(serving) !== 'plan') return;
    const windows = (into.shares[serving.accountId] ??= {});
    for (const [window, share] of Object.entries(r.costShares ?? {})) if (share !== null) windows[window] = (windows[window] ?? 0) + share;
  };
  // The newest of two requests by their receipt, the later id where two share an instant, as the route folds them in
  // that order.
  const newer = (r: RequestLog, than: RequestLog | null) => than === null || compareRequests(r, than) < 0;
  const spend = emptyPart();
  const probes = emptyPart();
  // `addMoney` writes the list dollars as well, which the route's day leaves out.
  const day: SessionDay & { usd: number } = { billedUsd: {}, equivalent: 0, requests: {}, usd: 0 };
  const dayFrom = startOfHour(now - 24 * HOUR);
  const agents = new Map<string, { agentId: string; newest: RequestLog | null; newestProbe: RequestLog | null; probes: SessionSpendPart; spend: SessionSpendPart }>();
  const agentOf = (agentId: string) => {
    const opened = agents.get(agentId) ?? { agentId, newest: null, newestProbe: null, probes: emptyPart(), spend: emptyPart() };
    agents.set(agentId, opened);
    return opened;
  };
  let newest: RequestLog | null = null;
  let newestRefused: RequestLog | null = null;
  let firstRequestAt: string | null = null;
  let lastInferenceAt: string | null = null;
  // An agent's entry opens on a served request alone, while its first request counts its refused ones too.
  const firstOfAgent = new Map<string, string>();
  const earlier = (at: string, than: string | null | undefined) => than === null || than === undefined || Date.parse(at) < Date.parse(than);
  for (const r of rows) {
    if (r.keyId !== session.keyId || r.keyGeneration !== session.keyGeneration || r.sessionId !== session.sessionId) continue;
    if (r.kind !== 'probe') {
      if (earlier(r.at, firstRequestAt)) firstRequestAt = r.at;
      if (r.agentId !== null && earlier(r.at, firstOfAgent.get(r.agentId))) firstOfAgent.set(r.agentId, r.at);
    }
    if (r.kind === 'inference') lastInferenceAt = laterOf(lastInferenceAt, r.at);
    if (r.refused) {
      if (r.kind !== 'probe' && newer(r, newestRefused)) newestRefused = r;
      continue;
    }
    const agent = r.agentId === null ? undefined : agentOf(r.agentId);
    if (r.kind === 'probe') {
      add(probes, r);
      if (agent) {
        add(agent.probes, r);
        if (newer(r, agent.newestProbe)) agent.newestProbe = r;
      }
      continue;
    }
    add(spend, r);
    if (newer(r, newest)) newest = r;
    if (agent) {
      add(agent.spend, r);
      if (newer(r, agent.newest)) agent.newest = r;
    }
    if (Date.parse(r.at) >= dayFrom) {
      const requestClass = r.requestClass ?? 'other';
      day.requests[requestClass] = (day.requests[requestClass] ?? 0) + 1;
      if (priced(r)) addMoney(day, r);
    }
  }
  const agentRequests: AgentRequests[] = [...agents.values()]
    .sort((a, b) => byteOrder(a.agentId, b.agentId))
    .map((a) => ({
      accountId: a.newest === null ? null : (servingAttempt(a.newest)?.accountId ?? null),
      agentId: a.agentId,
      agentType: a.newest?.agentType ?? null,
      firstRequestAt: firstOfAgent.get(a.agentId) ?? null,
      lastRequestAt: a.newest?.at ?? null,
      // An entry opens on a row of the agent, so one of the two stands.
      model: (a.newest ?? a.newestProbe)!.canonicalModel,
      spend: { ...a.spend, probes: a.probes },
    }));
  return {
    agentRequests,
    day: { billedUsd: day.billedUsd, equivalent: day.equivalent, requests: day.requests },
    firstRequestAt,
    lastInferenceAt,
    spend: spend.requests > 0 || probes.requests > 0 ? { ...spend, probes } : null,
    userAgent: (newest ?? newestRefused)?.userAgent ?? null,
  };
}

/** The latest of a row's last use, its last request and its companion's end, the instants the route's list reads. */
const listedThrough = (s: Pick<Session, 'companionSince' | 'lastRequestAt' | 'lastUsedAt'>): string | null => [s.lastUsedAt, s.lastRequestAt, s.companionSince].reduce(laterOf, null);

/** The routing log's rows a placement count reads. */
type PinEvent = Pick<RoutingEvent, 'agentId' | 'at' | 'keyId' | 'kind' | 'sessionId'>;

/**
 * Main's placements as the route counts them: its `pin_created` and `pin_migrated` events under the row's key and
 * session at or after the row's key generation, and on an earlier holder's row before the live key's. Where the story
 * seeds the row's session with a pin of main's whose creation the log does not hold, the count adds that creation,
 * since a router's ledger holds it. A seeded pin of a subagent adds none, since the route counts main's placements
 * alone. The added placement follows the seed rather than the listed row's pin, because the ledger keeps the event when
 * a write takes the pin, as a key's delete does.
 */
const SEEDED_PINS: ReadonlySet<string> = new Set(SESSIONS.filter((x) => x.pinned && x.agentId === 'main').map((x) => activityOf(x.keyId, x.keyGeneration, x.sessionId)));
const placementsOf = (s: Session, events: readonly PinEvent[]): number => {
  const live = keyById.get(s.keyId);
  // The story's key map keeps a key the operator deleted, whose own rows no live key bounds, so only a later
  // generation's creation ends a historical row's count.
  const until = s.historical && live !== undefined && Date.parse(live.createdAt) > s.keyGeneration ? Date.parse(live.createdAt) : Number.POSITIVE_INFINITY;
  const own = events.filter((e) => Date.parse(e.at) >= s.keyGeneration && Date.parse(e.at) < until);
  // An earlier holder's row can carry the key id and the session of a live key's pinned row, so the generation tells
  // the two apart.
  const seeded = SEEDED_PINS.has(activityOf(s.keyId, s.keyGeneration, s.sessionId));
  return own.length + (seeded && !own.some((e) => e.kind === 'pin_created') ? 1 : 0);
};

/**
 * The sessions the route lists at `now`, each folded from the request rows handed in and counting its placements in
 * the routing log's events handed in: a session no mod reports leaves once the latest of its last use, its last request
 * and its companion's end is older than the timeout, and each of its lines once its pin's last use is. The clock, the
 * rows and the events are handed in, since the story is built against `NOW` and a
 * test reads the list at any instant without moving `Date.now`, the mock's route hands the rows its ledger has written
 * by then, and the mock's routing log reads this module's sessions.
 */
export const sessionsListedAt = (now: number, sessions: readonly Session[], seconds: number, rows: readonly RequestLog[], events: readonly PinEvent[]): Session[] => {
  const bySession = groupInto(rows, (r) => (r.sessionId === null ? null : activityOf(r.keyId, r.keyGeneration, r.sessionId)));
  const mainPins = groupInto(events, (e) => (e.agentId === 'main' && (e.kind === 'pin_created' || e.kind === 'pin_migrated') ? `${e.keyId}|${e.sessionId}` : null));
  return sessions
    .filter((s) => s.companion !== 'none' || withinNonCompanionTimeout(listedThrough(s), seconds, now))
    .map((s) => {
      const folded = { ...s, ...foldSessionSpend(bySession.get(activityOf(s.keyId, s.keyGeneration, s.sessionId)) ?? [], s, now), placements: placementsOf(s, mainPins.get(`${s.keyId}|${s.sessionId}`) ?? []) };
      return s.companion !== 'none' ? folded : { ...folded, agentContexts: s.agentContexts.filter((c) => withinNonCompanionTimeout(c.lastUsedAt, seconds, now)) };
    });
};

// A pin is the engine's: one per key, session, agent and model, so a session's subagents hold pins of their own, on
// another account where they run there. The router renews a pin on each request and each keepalive probe, and the
// mod's release ends the keepalive record alone, so a released agent keeps its pin while its last request or probe is
// inside its pool's affinity window, whether or not the ledger holds the request that wrote it. The pool view reads
// every pin under the pool's keys as its figure and each member's own on the member, so a pin on an account that left
// the pool counts in the figure and the dashboard's sum alone.
const recipeLastUse = (r: Pick<KeepaliveRecipe, 'lastProbeAt' | 'lastRequestAt'>): number | null => {
  const at = laterOf(r.lastRequestAt, r.lastProbeAt);
  return at === null ? null : Date.parse(at);
};
const pinKept = (r: KeepaliveRecipe, poolId: string): boolean => {
  const last = recipeLastUse(r);
  return last !== null && NOW - last <= affinityTtlMsOf(poolId);
};

/**
 * The pins under a pool's keys as `GET /admin/pools/{id}/pins` answers them at `now`, in the order of their keys,
 * sessions, agents and models as bytes: each line of the rows and each pin the rows list no line for, with none of the
 * sessions route's filters. A pin's mark is its row's where it is the row's display pin, since the story marks no
 * other. The router's sweep takes a pin past the pool's affinity TTL, so a pin that `pinReadAt` reads as swept at `now`
 * is out: its last use is the later of its recipe's last request and last probe, which each renew it, the last use of
 * main's own line where its recipe holds no last probe, and a line's own last use where no recipe names the pin.
 */
export const poolPinsAt = (now: number, sessions: readonly Session[], keyIds: ReadonlySet<string>, ttlMs: number): PoolPin[] => {
  const recipes = new Map(RECIPES.map((r) => [pinKeyFor(r.keyId, r.sessionId, r.agentId, r.model), r]));
  return sessions
    .filter((s) => keyIds.has(s.keyId))
    .flatMap((s) =>
      [...s.agentContexts, ...(UNLISTED_PINS.get(s) ?? [])].flatMap((line): PoolPin[] => {
        const recipe = recipes.get(pinKeyFor(s.keyId, s.sessionId, line.agentId, line.model));
        // A recipe dates its agent's pin by the later of its last request and its last probe. A main whose recipe
        // holds no last probe has no probe to renew its pin, so its pin is dated as the sessions route dates its line,
        // by the row's last use, which stands for a request of the agent that the story's sample of requests can lack.
        const unprobedMain = recipe !== undefined && line.agentId === 'main' && recipe.lastProbeAt === null;
        const lastUse = recipe !== undefined && !unprobedMain ? recipeLastUse(recipe) : line.lastUsedAt === null ? null : Date.parse(line.lastUsedAt);
        if (lastUse === null || pinReadAt(lastUse, ttlMs, now) === 'none') return [];
        const mark = line.agentId === s.agentId && line.model === s.model ? s.mark : null;
        return [
          {
            accountBound: line.accountBound,
            accountId: line.accountId,
            agentId: line.agentId,
            burn: line.burn,
            burnAt: line.burnAt,
            burnUnit: line.burnUnit,
            burnWindow: line.burnWindow,
            ended: line.ended,
            fast: line.fast,
            keyId: s.keyId,
            mark,
            model: line.model,
            moveCost: line.moveCost,
            moveDropCost: line.moveDropCost,
            moveDropSize: line.moveDropSize,
            moveSize: line.moveSize,
            sessionId: s.sessionId,
            warm: line.warm,
            warmUntil: line.warmUntil,
          },
        ];
      }),
    )
    .sort((a, b) => byteOrder(a.keyId, b.keyId) || byteOrder(a.sessionId, b.sessionId) || byteOrder(a.agentId, b.agentId) || byteOrder(a.model, b.model));
};
type Pin = { agentId: string; keyId: string; sessionId: string };
/** The pins by their account, keyed `key|session|agent|model` so a pin a recipe and a line name counts once. */
const PINS = new Map<string | null, Map<string, Pin>>();
const pinOn = (accountId: string | null, keyId: string, sessionId: string, agentId: string, model: string) =>
  PINS.set(accountId, (PINS.get(accountId) ?? new Map<string, Pin>()).set(pinKeyFor(keyId, sessionId, agentId, model), { agentId, keyId, sessionId }));
for (const r of RECIPES) {
  const poolId = keyById.get(r.keyId)?.poolId;
  if (poolId === undefined || (r.agentState === 'released' && !pinKept(r, poolId))) continue;
  pinOn(movedPinOf(r)?.accountId ?? r.accountId, r.keyId, r.sessionId, r.agentId, r.model);
}
// Every listed line is a pin. The recipe loop above stays for the released agents the router still holds, which draw
// no line.
for (const s of SESSIONS) for (const line of s.agentContexts) pinOn(line.accountId, s.keyId, s.sessionId, line.agentId, line.model);
// Main's auxiliary pin draws no line, but the router holds it, so the pools count it as they count every pin.
for (const line of UNLISTED_PINS.get(auxiliaryOnlyRow) ?? []) pinOn(line.accountId, auxiliaryOnlyRow.keyId, auxiliaryOnlyRow.sessionId, line.agentId, line.model);
const underKeys = (pins: ReadonlyMap<string, Pin>, keyIds: ReadonlySet<string>) => [...pins.values()].filter((pin) => keyIds.has(pin.keyId));
const pinsUnder = (keyIds: ReadonlySet<string>) => [...PINS.values()].flatMap((pins) => underKeys(pins, keyIds));
/**
 * The pins under a pool's current keys on one member's account, the one membership `pinsUnderPool` reads too, so a
 * key that moved pools counts its pins on the member cells of the pool that holds it now.
 */
export const memberPins = (keyIds: ReadonlySet<string>, accountId: string): number => underKeys(PINS.get(accountId) ?? new Map(), keyIds).length;
/**
 * Every pin under a pool's current keys, one per key, session, agent and model as the route counts them, on a member's
 * account or not. A pin on an account that left the pool counts here and in no member's figure.
 */
export const pinsUnderPool = (keyIds: ReadonlySet<string>): number => pinsUnder(keyIds).length;
/**
 * The largest cohort under a pool's current keys, which its in-flight cap check reads: the most agents of one session
 * among the pins, as the route counts them, so an agent pinned on two models counts once.
 */
export const largestCohort = (keyIds: ReadonlySet<string>): number => {
  const agents = new Map<string, Set<string>>();
  for (const { agentId, keyId, sessionId } of pinsUnder(keyIds)) {
    const session = `${keyId}|${sessionId}`;
    agents.set(session, (agents.get(session) ?? new Set()).add(agentId));
  }
  return Math.max(0, ...[...agents.values()].map((a) => a.size));
};

// The ledger rows' expiries between turns over the last 24 hours, which the tile and the attention item count. A
// released agent's return stays out, since no probe could have kept its entry warm, while the Cache section's rollups
// keep it.
export const EXPIRED_BETWEEN_TURNS = REQUESTS.filter((r) => r.status === 200 && expiredBetweenTurns(r) && !RELEASED_AT_REQUEST.has(r.id));

// The mock's engine holds a session from its registration: the mod's, on every row a mod reports or went silent on,
// the registration-only row among them, and the first request it kept a recipe for on a spec row no mod registered.
// SESSIONS lists the spec's rows first, in its order, and the one written row a recipe warms is one the mod reports.
const ENGINE_SESSIONS = SESSIONS.filter((s, i) => s.companion !== 'none' || (i < sessionSpec.length && sessionSpec[i].state !== null));

// The router counts a session against `max_sessions` while it holds a slot, from its first request until its end
// (`Session::holds_slot` in the session engine). A session that registered and sent nothing holds none, an ended one
// holds none while its entry stays, and the mod's release of each agent is no end. In the story a session sent a
// request where the engine keeps a recipe of it, and it ended where its main's recipe reads the session's end.
const recipesOf = recipeFiler(RECIPES);
const SLOT_HOLDERS = ENGINE_SESSIONS.filter((s) => {
  const own = recipesOf(s);
  return own.length > 0 && !own.some((r) => r.agentId === 'main' && r.cause === 'session_ended');
});

// Router-wide, so every probe counted here belongs to a pool that keeps caches
// warm, and `jobs` has warming off and contributes none. Hand-set at a busier
// scale than the logged rows. The probe whose pin moved files under failed, and the gate's hold behind it withholds
// the same due probe once, under skipped.
const PROBES_24H = { ok: 225, held: 4, skipped: 46, failed: 4 };

// Each bucket reaches fewer windows than the one before it left unreturned.
const IDLE_REACHED = [96, 70, 50, 36, 22, 13, 8, 5, 2, 1];
const IDLE_RETURNED = [24, 16, 11, 12, 7, 4, 2, 2, 1, 0];
// The 30 days hold more of each, at rates within a few points of the 7 days' where a bucket counts enough windows.
const IDLE_REACHED_30D = [430, 315, 230, 170, 105, 64, 40, 26, 14, 7];
const IDLE_RETURNED_30D = [107, 72, 51, 57, 33, 20, 10, 10, 7, 1];

export const KEEPALIVE_OVERVIEW: KeepaliveOverview = {
  ...KEEPALIVE_OVERVIEW_SAMPLE,
  // Three gaps since the start, the last two hours back, so the Dashboard's card and the bus gaps item read one story.
  busGaps: 3,
  expiredBetweenTurns24h: EXPIRED_BETWEEN_TURNS.length,
  lastBusGapAt: ago(2 * HOUR),
  // The caps are the bootstrap file's defaults but the sessions cap, which the story sets to 18, so that the 17
  // sessions that hold a slot stand at 94 percent and the session engine's item holds beside the cache warming's. The
  // retained bytes are hand-set like the probe counts, at 92 percent of their cap. Every agent record the story's
  // engine holds carries a recipe, the released ones among them, so the records count the recipes.
  limits: {
    agents: { count: RECIPES.length, max: KEEPALIVE_OVERVIEW_SAMPLE.limits.agents.max },
    retainedBytes: { count: Math.round(0.92 * KEEPALIVE_OVERVIEW_SAMPLE.limits.retainedBytes.max), max: KEEPALIVE_OVERVIEW_SAMPLE.limits.retainedBytes.max },
    sessions: { count: SLOT_HOLDERS.length, max: 18 },
  },
  // `all` is the one pool that keeps caches warm, so its probes are the router-wide count. Its buckets keep the bounds
  // the router answered in the capture.
  pools: [
    {
      credited24h: 162,
      idleReturns: KEEPALIVE_OVERVIEW_SAMPLE.pools[0].idleReturns.map((b, i) => ({ ...b, reached: IDLE_REACHED[i], returned: IDLE_RETURNED[i] })),
      poolId: 'all',
      probes24h: PROBES_24H.ok,
      wasted24h: 31,
      wastedReasons24h: { cold_return: 7, evicted: 0, expired: 4, horizon: 2, idle_limit: 12, parked_limit: 3, released: 1, replaced: 0, session_ended: 2 },
    },
  ],
  probes24h: PROBES_24H,
};

/** The overview under the 30-day range, which moves the idle returns alone, as the route's answer keeps its shape. */
export const KEEPALIVE_OVERVIEW_30D: KeepaliveOverview = {
  ...KEEPALIVE_OVERVIEW,
  pools: KEEPALIVE_OVERVIEW.pools.map((p) => ({ ...p, idleReturns: p.idleReturns.map((b, i) => ({ ...b, reached: IDLE_REACHED_30D[i], returned: IDLE_RETURNED_30D[i] })) })),
};

const settingsFixture = FIXTURES.settings.body as Record<string, Record<string, unknown>>;

/**
 * The settings answer in the route's own shape, so the mock's settings reach the pages through the decoder the router's
 * answer goes through. The route answers the price table and the cost model the ledger ships, here with their
 * revisions at the story's dates, so the dialog, the About card and the drawer read the dates the story's requests
 * price at. A price revision the captured answer holds as the next one keeps a date after the story's clock.
 */
export const SETTINGS_WIRE = {
  ...settingsFixture,
  about: { profile: 'claude-code-2.1.310-linux-x64', version: SETTINGS_SAMPLE.about.version },
  access: { admin_configured: true },
  api_prices: API_PRICES_WIRE,
  listeners: { metrics: '127.0.0.1:9464', proxy: '127.0.0.1:8080', proxy_tls: false },
  retention: { body_capture_max_hours: 72, retention_days: 365, rollup_retention_days: 365 },
  sessions: { expire_after_seconds: SESSION_EXPIRE_SECONDS, non_companion_idle_seconds: NON_COMPANION_IDLE_SECONDS },
  storage: { api_prices_path: './api_prices.yaml', ledger_path: './data/ledger.sqlite', path: './data/router.sqlite', subscription_costs_path: './subscription_costs.yaml' },
  subscription_costs: SUBSCRIPTION_COSTS_WIRE,
  usage: { limit_reset_warning_days: 5 },
};
