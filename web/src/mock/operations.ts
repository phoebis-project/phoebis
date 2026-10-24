import type { BurnUnit, CompanionState, KeepaliveOverview, KeepaliveRecipe, KeepaliveState, RecipeCause, RecipeState, Session } from '@/api/types';
import { anchorTier, tierCapacity, weightOrFallback } from '@/lib/costs';
import { accountById } from './accounts';
import { API_PRICES_WIRE, SUBSCRIPTION_COSTS_WIRE } from './catalog';
import { FIXTURES } from './generated/fixtures';
import { keyById, poolById } from './pools';
import { BLOCKED_SESSION_ID, CLIENT_SESSION_ID, DELETED_HOLDER_GENERATION, HAIKU, MARKED_SESSION_IDS, OPENING_SESSION_IDS, RELEASED_SESSION_ID, REQUESTS, SESSION_IDS, SLOWED_SESSION_ID } from './requests';
import { createRng } from './rng';
import { KEEPALIVE_OVERVIEW_SAMPLE, RECIPE_SAMPLE, SESSION_SAMPLE, SETTINGS_SAMPLE, TABLES } from './samples';
import { HOUR, MINUTE, NOW, ago, ahead } from './time';

const rng = createRng(2_024);

/** What a fixed row writes out in place of the rolls, each time in minutes before the mock's now. */
interface WrittenFigures {
  /**
   * Main first, then its subagents: the id, the last request and the next probe in seconds ahead, null for none, and
   * the last probe where the row writes it.
   */
  agents: { agentId: string; lastProbeMinutes?: number; lastRequestMinutes: number; nextProbeSeconds: number | null }[];
  burn: number;
  burnAtMinutes: number;
  /** The deepest cache-map entry's prefix of the main pin. */
  cachedPrefixTokens: number;
  createdMinutes: number;
  lastInputTokens: number;
  lastRefreshMinutes: number;
}

// `state: null` marks a session with no cache-warming recipe: the `ci` key opts
// out and `jobs` keeps no caches warm.
// `companion` is independent of that: it says whether the keepalive mod ever
// registered for the session and is still reporting. A silent
// companion changes neither, because the retained prompts stay and are probed
// under the idle window. `subagents` pins the agents below main, which may run another
// model and, with that, land on another account. `idleMinutes` overrides the
// random idle on the few sessions that exist to straddle the expiry presets:
// without it every fixture is idle for under an hour and no preset in Settings
// would ever drop a row. `lastUseFromRows` reads the last use off the session's
// own requests instead, for the runner's sessions on `ci`: no pin keeps them in
// use, and their account has been at its daily budget since its last served
// request. Session 0 reads it so too, since its main still awaits the evidence of
// its last request, so no probe has renewed the pin since and the row and the line
// agree.
// `released` marks a session whose every
// agent, main included, the mod released. `sessionId` writes out the id of a
// session past the story's ids whose story row names it.
const sessionSpec: ({
  accountId: string;
  agents: number;
  companion: CompanionState;
  /**
   * The row is finished: the builder writes main's cause `session_ended` and every subagent `agent_released` in
   * `released` state.
   */
  finished?: true;
  idleMinutes?: number;
  keyId: string;
  lastUseFromRows?: boolean;
  model: string;
  released?: true;
  sessionId?: string;
  state: Session['keepalive']['state'];
  subagents?: { accountId?: string; model: string };
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
  { keyId: 'desktop', accountId: 'primary', model: 'claude-fable-5-1', state: 'busy', agents: 3, companion: 'reporting', lastUseFromRows: true, subagents: { model: 'claude-sonnet-5', accountId: 'nightly' } },
  { keyId: 'desktop', accountId: 'primary', model: 'claude-fable-5-1', state: 'idle', agents: 1, companion: 'reporting' },
  { keyId: 'desktop', accountId: 'nightly', model: 'claude-sonnet-4-6', state: 'idle', agents: 2, companion: 'silent' },
  { keyId: 'desktop', accountId: 'primary', model: 'claude-opus-5', state: 'parked', agents: 1, companion: 'reporting' },
  { keyId: 'desktop', accountId: 'nightly', model: 'claude-fable-5-1', state: 'idle', agents: 2, companion: 'reporting', subagents: { model: 'claude-sonnet-5' } },
  { keyId: 'desktop', accountId: 'primary', model: 'claude-sonnet-5', state: 'busy', agents: 1, companion: 'reporting' },
  // Idle for longer than the pool's one-hour max idle, so its recipe is held and its retained prompt is gone.
  { keyId: 'laptop', accountId: 'nightly', model: 'claude-sonnet-4-6', state: 'idle', agents: 1, companion: 'silent', idleMinutes: 95 },
  // The laptop key allows Haiku 4.5 and Sonnet 4.6 only, so its subagents stay inside that list.
  { keyId: 'laptop', accountId: 'primary', model: 'claude-sonnet-4-6', state: 'busy', agents: 2, companion: 'reporting', subagents: { model: 'claude-haiku-4-5-20251001' } },
  { keyId: 'laptop', accountId: 'nightly', model: 'claude-haiku-4-5-20251001', state: 'parked', agents: 1, companion: 'reporting' },
  // Five of its agents are written below, so it holds an agent in each lifecycle word, three of them released.
  { keyId: 'desktop', accountId: 'primary', model: 'claude-fable-5-1', state: 'idle', agents: 7, companion: 'reporting' },
  { keyId: 'ci', accountId: 'research', model: 'claude-haiku-4-5-20251001', state: null, agents: 1, companion: 'none', lastUseFromRows: true },
  { keyId: 'ci', accountId: 'research', model: 'claude-haiku-4-5-20251001', state: null, agents: 2, companion: 'none', lastUseFromRows: true },
  { keyId: 'laptop', accountId: 'secondary', model: 'claude-sonnet-4-6', state: null, agents: 1, companion: 'none' },
  { keyId: 'desktop', accountId: 'overflow', model: 'claude-opus-5', state: null, agents: 1, companion: 'none', idleMinutes: 570 },
  // Seen by the router but with nothing in the ledger, so the per-row jump to
  // analytics has no traffic to land on.
  { keyId: 'laptop', accountId: 'nightly', model: 'claude-haiku-4-5-20251001', state: null, agents: 1, companion: 'none' },
  { keyId: 'desktop', accountId: 'primary', model: 'claude-sonnet-5', state: 'idle', agents: 3, companion: 'reporting', released: true, sessionId: RELEASED_SESSION_ID },
  { keyId: 'laptop', accountId: 'primary', model: 'claude-sonnet-4-6', state: 'busy', agents: 1, companion: 'reporting', sessionId: CLIENT_SESSION_ID },
  // The two sessions the Sessions view's default filters hide: one the mod ended, which keeps its lines, and one no
  // companion reports, last used two hours ago, past the view's one-hour age.
  {
    keyId: 'desktop',
    accountId: 'nightly',
    model: 'claude-opus-5',
    state: 'idle',
    agents: 3,
    companion: 'reporting',
    finished: true,
    fixed: true,
    idleMinutes: 21,
    sessionId: '9b4e2f71-6c3d-4a85-b0e2-5d7f8a1c3e96',
    figures: {
      agents: [
        { agentId: 'main', lastRequestMinutes: 21, nextProbeSeconds: null },
        { agentId: 'a7f3', lastRequestMinutes: 34, nextProbeSeconds: null },
        { agentId: '5d0e', lastRequestMinutes: 47, nextProbeSeconds: null },
      ],
      burn: 0.18,
      burnAtMinutes: 22,
      cachedPrefixTokens: 12_000,
      createdMinutes: 140,
      lastInputTokens: 14_200,
      lastRefreshMinutes: 22,
    },
  },
  {
    keyId: 'laptop',
    accountId: 'secondary',
    model: 'claude-sonnet-4-6',
    state: 'idle',
    agents: 1,
    companion: 'none',
    fixed: true,
    idleMinutes: 120,
    sessionId: 'e5a17c3d-8f20-4b6e-9d41-2c7b0a9f6e58',
    figures: {
      agents: [{ agentId: 'main', lastRequestMinutes: 120, nextProbeSeconds: null }],
      burn: 0.09,
      burnAtMinutes: 121,
      cachedPrefixTokens: 5_000,
      createdMinutes: 310,
      lastInputTokens: 6_400,
      lastRefreshMinutes: 121,
    },
  },
  // A pin under the work pool's key on nightly, an account the pool held before, so the pool's figure counts it and
  // no member's does.
  {
    keyId: 'notebook',
    accountId: 'nightly',
    model: 'claude-sonnet-5-5',
    state: 'idle',
    agents: 1,
    companion: 'none',
    fixed: true,
    idleMinutes: 20,
    sessionId: '6d2f8b40-1e7a-4c95-a3d8-7b5e0c9f2a61',
    figures: {
      agents: [{ agentId: 'main', lastRequestMinutes: 20, nextProbeSeconds: 240 }],
      burn: 0.04,
      burnAtMinutes: 21,
      cachedPrefixTokens: 9_000,
      createdMinutes: 95,
      lastInputTokens: 10_800,
      lastRefreshMinutes: 21,
    },
  },
  // A session the mod released whole after a probe and one more turn, both past all's ten-minute affinity window, so
  // the router expired its pin and only the released line stays listed.
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
      cachedPrefixTokens: 8_000,
      createdMinutes: 90,
      lastInputTokens: 9_400,
      lastRefreshMinutes: 26,
    },
  },
];

/** How long the router lists a session after its last request, which the settings answer as well. */
const SESSION_EXPIRE_SECONDS = 86_400;
/**
 * The router drops a session past `SESSION_EXPIRE_SECONDS`, so a ledger-only row is listed only while its last request
 * is inside the span. Exported for the consistency test's two instants.
 */
export const insideSessionExpiry = (lastAt: string, now: number): boolean => Date.parse(lastAt) + SESSION_EXPIRE_SECONDS * 1000 > now;

// Grouped as the route's activity query groups a session's rows, by key, generation and session with probes left out,
// so the earlier holder's rows count for its own row below, never for the live session that shares their id. A refused
// request moves the session's last request and adds to no count, as the query sums the unrefused rows alone.
const activityOf = (keyId: string | null, keyGeneration: number, sessionId: string) => `${keyId}|${keyGeneration}|${sessionId}`;
// The newest request's model is kept as sent, since a `[1m]` spelling there names the session's context window.
const ledgered = new Map<string, { last: string; model: string; requests: number }>();
for (const r of REQUESTS) {
  if (r.sessionId === null || r.kind === 'probe') continue;
  const id = activityOf(r.keyId, r.keyGeneration, r.sessionId);
  const seen = ledgered.get(id);
  const counted = r.refused ? 0 : 1;
  if (!seen) ledgered.set(id, { last: r.at, model: r.model, requests: counted });
  else ledgered.set(id, r.at > seen.last ? { last: r.at, model: r.model, requests: seen.requests + counted } : { ...seen, requests: seen.requests + counted });
}

/**
 * A pin's context figures as the route answers them off its tokens: the window by the client's rule, a million where
 * the session's newest request named a `[1m]` twin and 200 000 otherwise, and the share of it to three decimals. The
 * mock keeps no cache map, so unless a fixed row writes its own, the deepest entry's prefix stands at nine tenths of
 * the tokens floored to the thousand, none below a thousand. A pin with no turn yet reads no figure.
 */
function pinContext(tokens: number | null, newestModel: string | null, writtenPrefix?: number): Pick<Session, 'cachedPrefixTokens' | 'contextShare' | 'contextWindowTokens'> {
  if (tokens === null || tokens === 0) return { cachedPrefixTokens: null, contextShare: null, contextWindowTokens: null };
  const window = newestModel?.endsWith('[1m]') ? 1_000_000 : 200_000;
  const prefix = writtenPrefix ?? Math.floor((tokens * 0.9) / 1_000) * 1_000;
  return { cachedPrefixTokens: prefix >= 1_000 ? prefix : null, contextShare: Math.round((tokens / window) * 1_000) / 1_000, contextWindowTokens: window };
}

/** The weight `model` prices at on the 5-hour window (a dated id reads its undated name), its model tier's latest
 *  weighed model's where it has none of its own. Null for a model of no weighted tier, which has no share for any
 *  usage. */
const pinWeight = (model: string) => weightOrFallback(TABLES, model, 'five_hour');

/** A pin's burn as a 20x 5-hour equivalent: the account tier's own share scaled by its capacity over the anchor's. */
const burnEquivalentOf = (burn: number | null, accountId: string): number | null => {
  const tier = accountById.get(accountId)?.subscription?.rateLimitTier;
  const own = tier === undefined ? null : tierCapacity(TABLES, tier, 'five_hour');
  const anchor = tierCapacity(TABLES, anchorTier(TABLES), 'five_hour');
  return burn === null || own === null || anchor === null ? null : (burn * own) / anchor;
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

export const SESSIONS: Session[] = sessionSpec.map((spec, i) => {
  // An entry past the story's session ids draws one the ledger never saw. The draw runs where the spec writes its id
  // out as well, so the rows after this one keep their rolls. A fixed row draws nothing, so it takes its own.
  const drawnId = spec.fixed ? spec.sessionId : i < SESSION_IDS.length ? SESSION_IDS[i] : rng.uuid();
  const written = spec.fixed ? spec.figures : null;
  const keyGeneration = Date.parse(keyById.get(spec.keyId)!.createdAt);
  const sessionId = spec.sessionId ?? drawnId;
  const seen = ledgered.get(activityOf(spec.keyId, keyGeneration, sessionId)) ?? null;
  // Both rolls are drawn whatever the spec says, except on a fixed row, so pinning one session's idle
  // does not shift the seeded stream the rest of these fixtures read from.
  const createdRoll = (written ? written.createdMinutes : rng.int(8, 9 * 60)) * MINUTE;
  const idleRoll = spec.fixed ? spec.idleMinutes * MINUTE : spec.state === 'busy' ? rng.int(5, 90) * 1000 : rng.int(2, 40) * MINUTE;
  // Off the rows, a session the ledger holds no request of has no last use.
  const lastFromRows = spec.lastUseFromRows ? (seen ? Date.parse(seen.last) : null) : undefined;
  const lastUsedAgo = typeof lastFromRows === 'number' ? NOW - lastFromRows : spec.idleMinutes === undefined ? Math.min(createdRoll, idleRoll) : spec.idleMinutes * MINUTE;
  const createdAgo = Math.max(createdRoll, lastUsedAgo + 5 * MINUTE);
  // A session the engine still keeps a recipe for is the one holding a pin on its account.
  const pinned = spec.state !== null;
  const unit: BurnUnit | null = pinned
    ? accountById.get(spec.accountId)?.kind === 'api_key'
      ? 'usd'
      : 'window_share'
    : null;
  // Drawn whatever the spec says, except on a fixed row, so pinning or the account's kind never shifts the stream the
  // rows after this one read. A row's burn is the per-hour figure the wire answers for its unit: dollars on the usd
  // roll, a share of the account's 5-hour window on the other.
  const burnRoll = written ? written.burn : rng.float(0.01, 0.9);
  const burnUsdRoll = written ? written.burn : rng.float(0.01, 0.5);
  const burnAtRoll = ago((written ? written.burnAtMinutes : rng.int(2, 40)) * MINUTE);
  const lastInputRoll = written ? written.lastInputTokens : rng.int(500, 20_000);
  const lastRefreshRoll = ago((written ? written.lastRefreshMinutes : rng.int(2, 40)) * MINUTE);
  // A pin on a model of no weighted tier has no burn to rank by, as the route answers it.
  const weight = pinned && unit === 'window_share' ? pinWeight(spec.model) : null;
  const burn = !pinned ? null : unit === 'usd' ? burnUsdRoll : weight !== null ? burnRoll : null;
  // The ledger names a session's fork from its requests, so a session it never saw records none.
  const fork = REQUESTS.find((r) => r.sessionId === sessionId && r.forkedFrom !== null);
  return {
    ...SESSION_SAMPLE,
    sessionId,
    forkedFrom: fork?.forkedFrom ?? null,
    forkDepth: fork?.forkDepth ?? null,
    keyId: spec.keyId,
    accountId: spec.accountId,
    model: spec.model,
    epoch: i < 3 ? 2 : 1,
    historical: false,
    keyGeneration,
    // An unpinned session has no pin to date, so the route answers no creation and the last use its own rows read.
    createdAt: pinned ? ago(createdAgo) : null,
    lastUsedAt: !pinned ? (seen ? seen.last : null) : lastFromRows === null ? null : ago(lastUsedAgo),
    lastRequestAt: seen ? seen.last : null,
    pinned,
    requests: seen ? seen.requests : 0,
    burn,
    burnAt: pinned ? burnAtRoll : null,
    burnEquivalent: unit === 'window_share' ? burnEquivalentOf(burn, spec.accountId) : null,
    burnUnit: unit,
    cacheTtlMs: pinned ? 5 * MINUTE : null,
    lastInputTokens: pinned ? lastInputRoll : null,
    ...pinContext(pinned ? lastInputRoll : null, seen?.model ?? null, written?.cachedPrefixTokens),
    lastRefreshAt: pinned ? lastRefreshRoll : null,
    shedFrom: null,
    shedReason: null,
    ...(pinned ? runwayOf(i) : { runwayBounded: null, runwayMs: null }),
    // A companion row's lines are written once the recipes exist, so every row starts with none.
    agentContexts: null,
    agents: spec.agents,
    companion: spec.companion,
    companionSince: spec.companion === 'silent' ? ago(rng.int(12, 150) * MINUTE) : null,
    estimated: weight?.estimated ?? false,
    keepalive: {
      state: spec.state,
      // Filled in from the recipes below, so a row and its agents cannot disagree.
      cause: null,
      lastGuard: null,
      nextProbeAt: null,
    },
    // Set from the row's index rather than drawn, so the rows after this one keep their rolls: a reporting mod saw the
    // person seconds ago, a silent one when it last reported, and a session no mod registered has no presence.
    presenceAgeSeconds: spec.companion === 'reporting' ? 4 + ((i * 17) % 50) : spec.companion === 'silent' ? 900 + i * 60 : null,
  };
});

// A companion registered this session before its first request reached the router, so it holds no pin, no epoch and
// no activity of its own. Its id is written out rather than drawn, which would shift every later roll in this file.
SESSIONS.push({
  ...SESSION_SAMPLE,
  accountId: null,
  agentContexts: null,
  agents: 0,
  cachedPrefixTokens: null,
  companion: 'reporting',
  companionSince: null,
  contextShare: null,
  contextWindowTokens: null,
  createdAt: null,
  epoch: null,
  forkDepth: null,
  forkedFrom: null,
  historical: false,
  keepalive: { cause: null, lastGuard: null, nextProbeAt: null, state: null },
  keyGeneration: Date.parse(keyById.get('laptop')!.createdAt),
  keyId: 'laptop',
  lastRequestAt: null,
  lastUsedAt: null,
  model: null,
  pinned: false,
  presenceAgeSeconds: 31,
  requests: 0,
  sessionId: '0f1d7c64-3b2a-4f58-9c7e-1a5b0e8d2f43',
});

// One recipe per agent on a pinned session, indexed by session. The cause
// explains why the main agent's probe is held or its recipe ended, and `null` means
// the next probe is scheduled.
const RECIPE_CAUSES: (RecipeCause | null)[] = [
  'awaiting_cache_evidence',
  'expired',
  null,
  null,
  'overage',
  null,
  'idle_limit',
  'unsupported_shape',
  null,
  null,
  null,
  null,
  null,
  null,
  null,
  // One row for each session past the story's ids, so the modulo wraps none of them onto the first causes. The
  // finished row's main is written over its own, the row idle for two hours ran past the `all` pool's one-hour max
  // idle as the 95-minute `laptop` row did, the pin on the work pool's former member waits for its next probe, and the
  // released row's main is written over its own.
  null,
  null,
  null,
  'idle_limit',
  null,
  null,
];

// A held or paused subagent under a main agent that keeps probing, which is the
// case the state hints describe.
const SUBAGENT_HOLDS = new Map<number, RecipeCause>([
  [0, 'repeated_cold_probe'],
  // A subagent whose park ran past the pool's parked ceiling.
  [4, 'parked_limit'],
  [7, 'affinity_too_short'],
  // Holding it moves no value a reader sees: the agents drawn after it take their fields from the written rows.
  [9, 'probe_pool_exhausted'],
]);

// A parked main agent on a wakeup further away than the pool's horizon, held with its prompt kept. The other parked
// session's agent stays retained, parked on a background command with its deadline.
const PARKED_HOLDS = new Map<number, RecipeCause>([[3, 'horizon']]);

/** How long ago a probe still in flight went out. */
const PROBE_IN_FLIGHT_MS = 20_000;

// The state the engine reads each cause the story uses in, as `recipe_state` maps it with no pause.
const STATE_OF: Partial<Record<RecipeCause, RecipeState>> = {
  affinity_too_short: 'held',
  awaiting_cache_evidence: 'retained',
  compacted: 'ended',
  expired: 'ended',
  horizon: 'held',
  idle_limit: 'ended',
  overage: 'held',
  parked_limit: 'ended',
  probe_pool_exhausted: 'paused',
  probing: 'retained',
  repeated_cold_probe: 'paused',
  scheduled: 'retained',
  unsupported_shape: 'not_retained',
};

/**
 * The index in `sessionSpec` of the session whose rows past the drawn pair are written, not rolled.
 */
const WRITTEN_SESSION = 9;
/**
 * The five subagents the written session holds beyond its drawn pair, written so the session holds an agent in each
 * lifecycle word, three of them released, one by the session's end, which the view keeps listed, and an id of the
 * length Claude Code gives a subagent. The list's order (a released row first, the parked row before the idle one, and
 * the idle row's last request newer than the drawn busy row's) differs from the lifecycle order main, busy, idle,
 * parked, so lines drawn in the list's order read differently from lines sorted into it.
 */
const WRITTEN_SUBAGENTS: Partial<KeepaliveRecipe>[] = [
  { agentId: 'e7c1', agentState: 'released', cause: 'agent_released', deadline: null, lastProbeAt: ago(4 * MINUTE), lastRequestAt: ago(9 * MINUTE), nextProbeAt: null, parkKind: null, state: 'ended', wakeAt: null },
  { agentId: '4f0c2a9e7b61d3c58', agentState: 'parked', cause: 'scheduled', deadline: ahead(41 * MINUTE), lastProbeAt: ago(2 * MINUTE), lastRequestAt: ago(6 * MINUTE), nextProbeAt: ahead(150_000), parkKind: 'background', state: 'retained', wakeAt: null },
  { agentId: 'b2d8', agentState: 'idle', cause: 'scheduled', deadline: null, lastProbeAt: ago(MINUTE), lastRequestAt: ago(30_000), nextProbeAt: ahead(200_000), parkKind: null, state: 'retained', wakeAt: null },
  { agentId: '91aa', agentState: 'released', cause: 'agent_released', deadline: null, lastProbeAt: ago(7 * MINUTE), lastRequestAt: ago(12 * MINUTE), nextProbeAt: null, parkKind: null, state: 'ended', wakeAt: null },
  { agentId: 'c3a9', agentState: 'released', cause: 'session_ended', deadline: null, lastProbeAt: ago(6 * MINUTE), lastRequestAt: ago(11 * MINUTE), nextProbeAt: null, parkKind: null, state: 'ended', wakeAt: null },
];

/** What every line of a session the mod released whole reads, written over the drawn row. */
const RELEASED_BY_THE_MOD: Partial<KeepaliveRecipe> = { agentState: 'released', cause: 'agent_released', deadline: null, nextProbeAt: null, parkKind: null, state: 'ended', wakeAt: null };
/**
 * What a subagent reads once its compaction completed: the engine stopped its recipe, and no probe goes out until the
 * agent's next main turn arms one for the new context. Written over the drawn row, so the rows after it keep their
 * rolls.
 */
const COMPACTED: Partial<KeepaliveRecipe> = { cause: 'compacted', lastGuard: 'compaction', nextProbeAt: null, state: 'ended' };
/** The first session's second subagent compacted, while its first stays paused on its cold probes. */
const COMPACTED_LINE = { agent: 2, session: 0 };
/**
 * What main reads on a session the mod ended, written over the drawn row. Its lifecycle word stays its session's, as
 * an ended main's does, and its subagents read as the mod released them.
 */
const ENDED_WITH_THE_SESSION: Partial<KeepaliveRecipe> = { cause: 'session_ended', deadline: null, nextProbeAt: null, parkKind: null, state: 'ended', wakeAt: null };

export const RECIPES: KeepaliveRecipe[] = SESSIONS.flatMap((pin, i) => {
  const mainState = pin.keepalive.state;
  if (mainState === null) return [];
  const sub = sessionSpec[i].subagents;
  const spec = sessionSpec[i];
  // A fixed row's agents are written, so they draw nothing.
  const fixedAgents = spec.fixed ? spec.figures.agents : null;
  const agents = fixedAgents ? fixedAgents.map((a) => a.agentId) : Array.from({ length: pin.agents }, (_, j) => (j === 0 ? 'main' : rng.uuid().slice(0, 4)));
  return agents.map((agentId, j) => {
    // A parked agent is probed like a busy one until one of the park's ceilings stops it.
    const held = mainState === 'parked' ? (j === 0 ? PARKED_HOLDS.get(i) ?? null : null) : j === 0 ? RECIPE_CAUSES[i % RECIPE_CAUSES.length] : j === 1 ? SUBAGENT_HOLDS.get(i) ?? null : null;
    // A busy main has its probe in flight, and every other agent nothing holds waits for its scheduled one.
    const cause: RecipeCause = held ?? (mainState === 'busy' && j === 0 ? 'probing' : 'scheduled');
    const state = STATE_OF[cause] ?? 'retained';
    const model = j === 0 ? sessionSpec[i].model : sub?.model ?? sessionSpec[i].model;
    // The engine lists an agent once it has sent a request. The roll is drawn whatever it gives, so the rows after this
    // one keep theirs, and a subagent it leaves without one sent its request three and a half minutes ago, off the
    // whole minutes its last probe falls on.
    const rolled = fixedAgents ? ago(fixedAgents[j].lastRequestMinutes * MINUTE) : j === 0 ? pin.lastRequestAt : rng.int(0, 4) === 0 ? null : ago(rng.int(1, 40) * MINUTE);
    const lastRequestAt = rolled ?? ago(3 * MINUTE + 30_000);
    const accountId = j === 0 ? pin.accountId : sub?.accountId ?? pin.accountId;
    const ttlSeconds = model.includes('haiku') ? 300 : 3_600;
    // Main reads its session's lifecycle word, a subagent whose park ran out is still parked, and the others are busy.
    const agentState: KeepaliveState = j === 0 ? mainState : cause === 'parked_limit' ? 'parked' : 'busy';
    // The two parked mains: one on a background command with its deadline, one on a wakeup three days out. Neither
    // time is drawn, so the rows after them keep their rolls.
    const parkKind = agentState !== 'parked' ? null : cause === 'horizon' ? 'scheduled' : 'background';
    const deadline = agentState === 'parked' && j === 0 && cause !== 'horizon' ? ahead(72 * MINUTE + 30_000) : null;
    const wakeAt = parkKind === 'scheduled' ? ahead(3 * 24 * 60 * MINUTE + 30_000) : null;
    // An agent whose request still awaits its evidence has not been probed since, a probe in flight was dispatched at
    // its due time, which the engine records as the last probe, and the rest were probed minutes ago.
    const writtenProbe = fixedAgents?.[j].lastProbeMinutes;
    const lastProbeAt = writtenProbe !== undefined ? ago(writtenProbe * MINUTE) : cause === 'awaiting_cache_evidence' ? null : cause === 'probing' ? ago(PROBE_IN_FLIGHT_MS) : ago((((i * 3 + j) % 4) + 1) * MINUTE);
    const probesToday = lastProbeAt === null ? 0 : ((i * 7 + j * 3) % 9) + 1;
    // The written session's rows past its drawn pair are written, so a reader meets every lifecycle word. A session the
    // mod released whole keeps its draws, so the rows after it keep theirs.
    const written = sessionSpec[i].released
      ? RELEASED_BY_THE_MOD
      : sessionSpec[i].finished
        ? j === 0
          ? ENDED_WITH_THE_SESSION
          : RELEASED_BY_THE_MOD
        : i === WRITTEN_SESSION && j >= 2
          ? WRITTEN_SUBAGENTS[j - 2]
          : i === COMPACTED_LINE.session && j === COMPACTED_LINE.agent
            ? COMPACTED
            : undefined;
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
      nextProbeAt: fixedAgents ? (fixedAgents[j].nextProbeSeconds === null ? null : ahead(fixedAgents[j].nextProbeSeconds * 1000)) : cause === 'scheduled' ? ahead(rng.int(20, 280) * 1000) : cause === 'probing' ? ago(PROBE_IN_FLIGHT_MS) : null,
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

// A session's next probe is the soonest its agents have scheduled. An agent
// awaiting cache evidence contributes no time even though it is armed, so a
// session left with only those reads as pending rather than unscheduled.
for (const pin of SESSIONS) {
  const scheduled = RECIPES.filter((r) => r.sessionId === pin.sessionId && r.nextProbeAt !== null)
    .map((r) => r.nextProbeAt as string)
    .sort();
  pin.keepalive.nextProbeAt = scheduled[0] ?? null;
  // The session's own summary reads the main agent's lifecycle word, guard note and cause, the way the admin route
  // does, so a session the mod released whole reads released.
  const main = RECIPES.find((r) => r.sessionId === pin.sessionId && r.agentId === 'main');
  pin.keepalive.state = main?.agentState ?? null;
  pin.keepalive.lastGuard = main?.lastGuard ?? null;
  pin.keepalive.cause = main?.cause ?? null;
}

/**
 * A subagent pin's tokens, set from its session's index and its line's place rather than drawn, so the rows after it
 * keep their rolls.
 */
const agentTokens = (i: number, j: number) => 2_000 + ((i * 31 + j * 17) % 23) * 1_000;
// The route lists a companion session's other pins whose agent the mod has not released, ordered by agent id as
// bytes, empty where every other agent is released, and none on a session no companion reports.
SESSIONS.forEach((pin, i) => {
  if (pin.companion === 'none') return;
  const newest = ledgered.get(activityOf(pin.keyId, pin.keyGeneration, pin.sessionId))?.model ?? null;
  pin.agentContexts = RECIPES.filter((r) => r.keyId === pin.keyId && r.sessionId === pin.sessionId)
    .map((r, j) => ({ r, tokens: agentTokens(i, j) }))
    .filter(({ r }) => r.agentId !== 'main' && r.agentState !== 'released')
    .sort(({ r: a }, { r: b }) => (a.agentId < b.agentId ? -1 : a.agentId > b.agentId ? 1 : 0))
    .map(({ r, tokens }) => ({ agentId: r.agentId, lastInputTokens: tokens, model: r.model, ...pinContext(tokens, newest) }));
});

/**
 * Studio's notebook sessions in work, pinned there while the slow-down holds it: the slowed one with its subagent, whose
 * main pin has the cycle its turns since the placement give, the two the round marked and the one whose turn opened the
 * span, whose pins have none yet. Each burn is the main pin's demand at the account's half speed. No recipe warms them.
 * They are written after the recipes rather than rolled, so every drawn row and recipe keeps its rolls.
 */
const SLOWED_PINS: { agents: { agentId: string; model: string }[]; burn: number; cycleMs: number | null; sessionId: string }[] = [
  { agents: [{ agentId: 'main', model: 'claude-sonnet-5-5' }, { agentId: '7b2e', model: HAIKU }], burn: 0.0125, cycleMs: 42_000, sessionId: SLOWED_SESSION_ID },
  { agents: [{ agentId: 'main', model: 'claude-sonnet-5-5' }], burn: 0.0135, cycleMs: null, sessionId: MARKED_SESSION_IDS[0] },
  { agents: [{ agentId: 'main', model: 'claude-sonnet-5-5' }], burn: 0.014, cycleMs: null, sessionId: MARKED_SESSION_IDS[1] },
  { agents: [{ agentId: 'main', model: 'claude-sonnet-5-5' }], burn: 0.0113, cycleMs: null, sessionId: OPENING_SESSION_IDS[1] },
];
// Every pin of the account reads the window's runway: until its burn carries the utilization to the pool's ceiling.
const studioFive = accountById.get('studio')?.windows.find((w) => w.id === '5h');
const studioRunwayMs = studioFive?.burn ? Math.round(((poolById.get('work')!.windowCeiling - studioFive.utilization) / studioFive.burn) * HOUR) : null;
for (const pin of SLOWED_PINS) {
  // The rows run newest first, and the main agent's newest turn is the pin's last use.
  const rows = REQUESTS.filter((r) => r.kind !== 'probe' && r.keyId === 'notebook' && r.sessionId === pin.sessionId);
  const main = rows.filter((r) => r.agentId === 'main');
  const last = main[0];
  const lastAttempt = last.attempts[last.attempts.length - 1];
  const lastInputTokens = last.usage.input + last.usage.cacheRead + last.usage.cacheWrite5m + last.usage.cacheWrite1h;
  SESSIONS.push({
    ...SESSION_SAMPLE,
    accountId: 'studio',
    agentContexts: null,
    agentId: 'main',
    agents: pin.agents.length,
    burn: pin.burn,
    burnAt: lastAttempt.startedAt,
    burnEquivalent: burnEquivalentOf(pin.burn, 'studio'),
    burnUnit: 'window_share',
    cacheTtlMs: HOUR,
    companion: 'none',
    companionSince: null,
    createdAt: main[main.length - 1].at,
    cycleMs: pin.cycleMs,
    epoch: 1,
    estimated: false,
    forkDepth: null,
    forkedFrom: null,
    historical: false,
    keepalive: { cause: null, lastGuard: null, nextProbeAt: null, state: null },
    keyGeneration: Date.parse(keyById.get('notebook')!.createdAt),
    keyId: 'notebook',
    lastInputTokens,
    ...pinContext(lastInputTokens, rows[0].model),
    lastRefreshAt: lastAttempt.startedAt,
    lastRequestAt: rows[0].at,
    lastUsedAt: lastAttempt.startedAt,
    model: 'claude-sonnet-5-5',
    pinned: true,
    presenceAgeSeconds: null,
    requests: rows.length,
    runwayBounded: studioRunwayMs !== null,
    runwayMs: studioRunwayMs,
    sessionId: pin.sessionId,
    shedFrom: null,
    shedReason: null,
    speed: accountById.get('studio')?.slowDown?.factor ?? null,
  });
}

// The earlier holder of the `desktop` key id, which the analytics story draws as `desktop (deleted, created ...)`
// with the creation minute in the viewer's zone: the router lists a holder's session while its last request is inside
// the session expiry, with that holder's own requests and no pin or recipe. The holder's last request ran on the
// session the live key holds now, so the row carries that id, and it joins the list after the probe times are filled
// in, which would otherwise hand it that session's next probe.
const holderRows = REQUESTS.filter((r) => r.kind !== 'probe' && r.keyId === 'desktop' && r.keyGeneration === DELETED_HOLDER_GENERATION && r.sessionId === SESSION_IDS[0]);
const holderLast = holderRows[0];
if (holderLast && insideSessionExpiry(holderLast.at, NOW)) {
  SESSIONS.push({
    ...SESSION_SAMPLE,
    accountId: holderLast.accountId,
    agentContexts: null,
    agentId: null,
    agents: 0,
    burn: null,
    burnAt: null,
    burnEquivalent: null,
    burnUnit: null,
    cachedPrefixTokens: null,
    cacheTtlMs: null,
    companion: 'none',
    companionSince: null,
    contextShare: null,
    contextWindowTokens: null,
    createdAt: null,
    epoch: null,
    forkDepth: null,
    forkedFrom: null,
    historical: true,
    keepalive: { cause: null, lastGuard: null, nextProbeAt: null, state: null },
    keyGeneration: DELETED_HOLDER_GENERATION,
    keyId: 'desktop',
    lastInputTokens: null,
    lastRefreshAt: null,
    lastRequestAt: holderLast.at,
    lastUsedAt: null,
    model: holderLast.canonicalModel,
    pinned: false,
    presenceAgeSeconds: null,
    requests: holderRows.length,
    sessionId: SESSION_IDS[0],
    shedFrom: null,
    shedReason: null,
  });
}

// The desktop session whose every request the key's own guardrail refused before any account was asked. The router
// lists it off the ledger's rows with no pin and no recipe, so a drifting or blocked client stays visible, and the
// activity fold gives it a count of none and the newest refusal as its last request, and only while that refusal is
// inside the router's session expiry. The row is built at an instant since the mock's now is fixed at load, so the
// consistency test reads it inside the expiry and past it.
const blockedGeneration = Date.parse(keyById.get('desktop')!.createdAt);
const blocked = ledgered.get(activityOf('desktop', blockedGeneration, BLOCKED_SESSION_ID));
const blockedLast = REQUESTS.find((r) => r.kind !== 'probe' && r.keyId === 'desktop' && r.keyGeneration === blockedGeneration && r.sessionId === BLOCKED_SESSION_ID);
export function refusalOnlyRows(now: number): Session[] {
  if (!blocked || !blockedLast || !insideSessionExpiry(blockedLast.at, now)) return [];
  const row: Session = {
    ...SESSION_SAMPLE,
    accountId: null,
    agentContexts: null,
    agentId: null,
    agents: 0,
    burn: null,
    burnAt: null,
    burnEquivalent: null,
    burnUnit: null,
    cachedPrefixTokens: null,
    cacheTtlMs: null,
    companion: 'none',
    companionSince: null,
    contextShare: null,
    contextWindowTokens: null,
    createdAt: null,
    epoch: null,
    forkDepth: null,
    forkedFrom: null,
    historical: false,
    keepalive: { cause: null, lastGuard: null, nextProbeAt: null, state: null },
    keyGeneration: blockedGeneration,
    keyId: 'desktop',
    lastInputTokens: null,
    lastRefreshAt: null,
    lastRequestAt: blocked.last,
    lastUsedAt: null,
    model: blockedLast.canonicalModel,
    pinned: false,
    presenceAgeSeconds: null,
    requests: blocked.requests,
    sessionId: BLOCKED_SESSION_ID,
    shedFrom: null,
    shedReason: null,
  };
  return [row];
}
SESSIONS.push(...refusalOnlyRows(NOW));

// A pin is the engine's: one per key, session, agent and model, so a session's subagents hold pins of their own, on
// another account where they run there. The router renews a pin on each request and each keepalive probe, and the
// mod's release ends the keepalive record alone, so a released agent keeps its pin while its last request or probe is
// inside its pool's affinity window, whether or not the ledger holds the request that wrote it. The pool view reads
// every pin under the pool's keys as its figure and each member's own on the member, so a pin on an account that left
// the pool counts in the figure and the dashboard's sum alone.
const pinKept = (r: KeepaliveRecipe, poolId: string): boolean => {
  const lastUse = Math.max(...[r.lastRequestAt, r.lastProbeAt].flatMap((at) => (at === null ? [] : [Date.parse(at)])));
  return NOW - lastUse <= (poolById.get(poolId)?.affinityTtlSeconds ?? 0) * 1_000;
};
const PINS = new Map<string, Set<string>>();
for (const r of RECIPES) {
  const poolId = keyById.get(r.keyId)?.poolId;
  const pin = `${r.keyId}|${r.sessionId}|${r.agentId}|${r.model}`;
  if (poolId === undefined || (r.agentState === 'released' && !pinKept(r, poolId))) continue;
  const member = `${poolId}|${r.accountId}`;
  PINS.set(member, (PINS.get(member) ?? new Set()).add(pin));
}
for (const pin of SLOWED_PINS) {
  const member = `${keyById.get('notebook')!.poolId}|studio`;
  for (const a of pin.agents) PINS.set(member, (PINS.get(member) ?? new Set()).add(`notebook|${pin.sessionId}|${a.agentId}|${a.model}`));
}
/** The pins under a pool's keys on one member's account. */
export const memberPins = (poolId: string, accountId: string): number => PINS.get(`${poolId}|${accountId}`)?.size ?? 0;
/**
 * Every pin under a pool's current keys, one per key, session, agent and model as the route counts them, on a member's
 * account or not. A pin on an account that left the pool counts here and in no member's figure.
 */
export const pinsUnderPool = (keyIds: ReadonlySet<string>): number => new Set([...PINS.values()].flatMap((pins) => [...pins].filter((pin) => keyIds.has(pin.slice(0, pin.indexOf('|')))))).size;

// Read off the ledger rows, which cover the last 24 hours, so the tile and the
// analytics Cache section agree on what "expired between turns" counts.
const EXPIRED_BETWEEN_TURNS_24H = REQUESTS.filter((r) => r.status === 200 && r.usage.cacheRead === 0 && r.attempts[r.attempts.length - 1].cacheMissReason === 'none').length;

// The keepalive engine holds a session once the mod registered it or a request retained a recipe for it.
const ENGINE_SESSIONS = SESSIONS.filter((s) => s.companion !== 'none' || s.keepalive.state !== null);

// Router-wide, so every probe counted here belongs to a pool that keeps caches
// warm, and `jobs` has warming off and contributes none. Hand-set at a busier
// scale than the logged rows.
const PROBES_24H = { ok: 225, migrated: 18, held: 4, skipped: 45, failed: 3 };

// Each bucket reaches fewer windows than the one before it left unreturned.
const IDLE_REACHED = [96, 70, 50, 36, 22, 13, 8, 5, 2, 1];
const IDLE_RETURNED = [24, 16, 11, 12, 7, 4, 2, 2, 1, 0];

export const KEEPALIVE_OVERVIEW: KeepaliveOverview = {
  ...KEEPALIVE_OVERVIEW_SAMPLE,
  // Counted off the recipes so the tiles cannot drift from the table below them.
  agents: {
    busy: RECIPES.filter((r) => r.agentState === 'busy').length,
    idle: RECIPES.filter((r) => r.agentState === 'idle').length,
    parked: RECIPES.filter((r) => r.agentState === 'parked').length,
  },
  busGaps: 0,
  expiredBetweenTurns24h: EXPIRED_BETWEEN_TURNS_24H,
  // The caps are the bootstrap file's defaults, and the retained bytes are hand-set like the probe counts.
  limits: {
    agents: { count: ENGINE_SESSIONS.reduce((n, s) => n + s.agents, 0), max: 1_024 },
    retainedBytes: { count: 40 * 1_048_576, max: 256 * 1_048_576 },
    sessions: { count: ENGINE_SESSIONS.length, max: 128 },
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

const settingsFixture = FIXTURES.settings.body as Record<string, Record<string, unknown>>;

/**
 * The settings answer in the route's own shape, so the mock's settings reach the pages through the decoder the router's
 * answer goes through. The route answers the price table and the cost model the ledger ships, here with their
 * revisions dated into the story's past, so the dialog, the About card and the drawer read the dates the story's
 * requests price at.
 */
export const SETTINGS_WIRE = {
  ...settingsFixture,
  about: { profile: 'claude-code-2.1.269-linux-x64', version: SETTINGS_SAMPLE.about.version },
  access: { admin_configured: true },
  api_prices: API_PRICES_WIRE,
  listeners: { metrics: '127.0.0.1:9464', proxy: '127.0.0.1:8080', proxy_tls: false },
  retention: { body_capture_max_hours: 72, retention_days: 365, rollup_retention_days: 365 },
  sessions: { expire_after_seconds: SESSION_EXPIRE_SECONDS },
  storage: { api_prices_path: './api_prices.yaml', ledger_path: './data/ledger.sqlite', path: './data/router.sqlite', subscription_costs_path: './subscription_costs.yaml' },
  subscription_costs: SUBSCRIPTION_COSTS_WIRE,
};
