import type { AgentContext, BurnUnit, CompanionState, KeepaliveOverview, KeepaliveRecipe, KeepaliveState, RecipeCause, RecipeState, Session } from '@/api/types';
import { anchorTier, tierCapacity, weightOrFallback } from '@/lib/costs';
import { accountById } from './accounts';
import { API_PRICES_WIRE, SUBSCRIPTION_COSTS_WIRE } from './catalog';
import { FIXTURES } from './generated/fixtures';
import { keyById, poolById } from './pools';
import { BLOCKED_SESSION_ID, CLIENT_SESSION_ID, DELETED_HOLDER_GENERATION, FLEET_SESSION_ID, HAIKU, MARKED_SESSION_IDS, OPENING_SESSION_IDS, RELEASED_SESSION_ID, REQUESTS, SESSION_IDS, SLOWED_SESSION_ID } from './requests';
import { createRng } from './rng';
import { KEEPALIVE_OVERVIEW_SAMPLE, RECIPE_SAMPLE, SESSION_SAMPLE, SETTINGS_SAMPLE, TABLES } from './samples';
import { HOUR, MINUTE, NOW, ago, ahead, iso } from './time';

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
// agree. `released` marks a session whose every agent, main included, the mod
// released. `sessionId` writes out the id of a session past the story's ids whose
// story row names it.
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
  /** Main's requests named no effort, so its pin, the row and main's line read none. */
  noEffort?: true;
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
  { keyId: 'laptop', accountId: 'nightly', model: 'claude-haiku-4-5-20251001', state: 'parked', agents: 1, companion: 'reporting', noEffort: true },
  // Five of its agents are written below, so it holds an agent in each lifecycle word, three of them released.
  { keyId: 'desktop', accountId: 'primary', model: 'claude-fable-5-1', state: 'idle', agents: 7, companion: 'reporting' },
  { keyId: 'ci', accountId: 'research', model: 'claude-haiku-4-5-20251001', state: null, agents: 1, companion: 'none', lastUseFromRows: true },
  { keyId: 'ci', accountId: 'research', model: 'claude-haiku-4-5-20251001', state: null, agents: 2, companion: 'none', lastUseFromRows: true },
  { keyId: 'laptop', accountId: 'secondary', model: 'claude-sonnet-4-6', state: null, agents: 1, companion: 'none' },
  { keyId: 'desktop', accountId: 'overflow', model: 'claude-opus-5', state: null, agents: 1, companion: 'none', idleMinutes: 570 },
  // Seen by the router but with nothing in the ledger, so the per-row jump to
  // analytics has no traffic to land on.
  { keyId: 'laptop', accountId: 'nightly', model: 'claude-haiku-4-5-20251001', state: null, agents: 1, companion: 'none' },
  // A session the mod released whole, whose lines' last uses, minutes back, date the release inside all's one-hour
  // cache TTL, since the router lists a released record for one TTL after its release and then not at all.
  { keyId: 'desktop', accountId: 'primary', model: 'claude-sonnet-5', state: 'idle', agents: 3, companion: 'reporting', released: true, sessionId: RELEASED_SESSION_ID },
  { keyId: 'laptop', accountId: 'primary', model: 'claude-sonnet-4-6', state: 'busy', agents: 1, companion: 'reporting', sessionId: CLIENT_SESSION_ID },
  // One session the mod ended, which keeps its lines, its released subagents' last probes dating their release inside
  // all's one-hour cache TTL, and which the Sessions view's default filters hide, and one no companion reports, last
  // used two hours ago, past the one-hour non-companion timeout, so the route's list leaves it out.
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
      createdMinutes: 95,
      lastInputTokens: 10_800,
      lastRefreshMinutes: 21,
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
 * is inside the span. Exported for the consistency test's two instants.
 */
export const insideSessionExpiry = (lastAt: string, now: number): boolean => Date.parse(lastAt) + SESSION_EXPIRE_SECONDS * 1000 > now;
/** Whether an instant is inside the non-companion timeout: a row or a line with no use on record is never older. */
export const withinNonCompanionTimeout = (at: string | null, seconds: number, now: number): boolean => at === null || now - Date.parse(at) <= seconds * 1000;
/** The later of a session's own last use and its last ledgered request, which the route's timeout reads. */
export const lastUseOf = (s: Pick<Session, 'lastRequestAt' | 'lastUsedAt'>): string | null => {
  const stamps = [s.lastUsedAt, s.lastRequestAt].filter((t): t is string => t !== null);
  return stamps.length === 0 ? null : stamps.reduce((a, b) => (Date.parse(b) > Date.parse(a) ? b : a));
};

// Grouped as the route's activity query groups a session's rows, by key, generation and session with probes left out,
// so the earlier holder's rows count for its own row below, never for the live session that shares their id. A refused
// request moves the session's last request and adds to no count, as the query sums the unrefused rows alone.
const activityOf = (keyId: string | null, keyGeneration: number, sessionId: string) => `${keyId}|${keyGeneration}|${sessionId}`;
const ledgered = new Map<string, { last: string; requests: number }>();
for (const r of REQUESTS) {
  if (r.sessionId === null || r.kind === 'probe') continue;
  const id = activityOf(r.keyId, r.keyGeneration, r.sessionId);
  const seen = ledgered.get(id);
  const counted = r.refused ? 0 : 1;
  if (!seen) ledgered.set(id, { last: r.at, requests: counted });
  else ledgered.set(id, r.at > seen.last ? { last: r.at, requests: seen.requests + counted } : { ...seen, requests: seen.requests + counted });
}
/** Each agent's earliest request of its session, the instant the router first saw the agent there. */
const firstRequestAt = new Map<string, string>();
for (const r of REQUESTS) {
  if (r.sessionId === null || r.agentId === null || r.kind === 'probe') continue;
  const id = `${activityOf(r.keyId, r.keyGeneration, r.sessionId)}|${r.agentId}`;
  const seen = firstRequestAt.get(id);
  if (seen === undefined || r.at < seen) firstRequestAt.set(id, r.at);
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

/**
 * The words a pin's effort reads, taken by an index rather than drawn, so the rows after it keep their rolls. The four
 * are the levels the relay reads, in no order a reader relies on.
 */
const EFFORT_WORDS = ['medium', 'high', 'low', 'max'] as const;
const effortAt = (i: number) => EFFORT_WORDS[i % EFFORT_WORDS.length];

/** A pin's line as the route answers it: warm while its last use is inside its cache TTL at the story's clock. */
const pinLine = (line: Omit<AgentContext, 'createdAt' | 'warm'>): Omit<AgentContext, 'createdAt'> => ({
  ...line,
  warm: line.lastUsedAt !== null && line.cacheTtlMs !== null && NOW - Date.parse(line.lastUsedAt) < line.cacheTtlMs,
});
const byteOrder = (a: string, b: string) => (a < b ? -1 : a > b ? 1 : 0);
/** The route's order of a session's lines: main first, then by agent id and model as bytes. */
const lineOrder = (a: Pick<AgentContext, 'agentId' | 'model'>, b: Pick<AgentContext, 'agentId' | 'model'>) =>
  (a.agentId === 'main') !== (b.agentId === 'main') ? (a.agentId === 'main' ? -1 : 1) : byteOrder(a.agentId, b.agentId) || byteOrder(a.model, b.model);
/**
 * A row's lines in the route's order, each at its agent's first-seen time: main's at the session's, a subagent's at its
 * earliest request of the session in the requests seed. A subagent no request names reads the session's time plus a
 * minute per place among the row's subagent lines, a stand-in for a first sighting the story does not write.
 */
const datedLines = (createdAt: string | null, session: string, lines: readonly Omit<AgentContext, 'createdAt'>[]): AgentContext[] => {
  let place = 0;
  return [...lines].sort(lineOrder).map((line) => {
    if (line.agentId === 'main') return { ...line, createdAt };
    place += 1;
    const first = firstRequestAt.get(`${session}|${line.agentId}`);
    return { ...line, createdAt: first ?? (createdAt === null ? null : iso(Date.parse(createdAt) + place * MINUTE)) };
  });
};
/** A request's prompt as the route counts a pin's last turn: its uncached input, cache reads and cache writes. */
const promptOf = (r: { usage: { cacheRead: number; cacheWrite1h: number; cacheWrite5m: number; input: number } }) => r.usage.input + r.usage.cacheRead + r.usage.cacheWrite5m + r.usage.cacheWrite1h;

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
    // The router keeps a session's first-seen time until its last pin leaves, so an unpinned session answers none, and
    // its last use is what its own rows read.
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
    effort: pinned && !spec.noEffort ? effortAt(i) : null,
    lastInputTokens: pinned ? lastInputRoll : null,
    lastRefreshAt: pinned ? lastRefreshRoll : null,
    shedFrom: null,
    shedReason: null,
    ...(pinned ? runwayOf(i) : { runwayBounded: null, runwayMs: null }),
    // A row's lines are written once the recipes exist, so every row starts with none.
    agentContexts: [],
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
  agentContexts: [],
  agents: 0,
  companion: 'reporting',
  companionSince: null,
  createdAt: null,
  effort: null,
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
  // idle as the 95-minute `laptop` row did and keeps its recipe in the engine's figures while the route's list leaves
  // the row out under the non-companion timeout, the pin on the work pool's former member waits for its next probe,
  // and the released row's main is written over its own.
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
  probe_pool_exhausted: 'held',
  probing: 'retained',
  repeated_cold_probe: 'paused',
  scheduled: 'retained',
  unsupported_shape: 'not_retained',
};

// The cause the story holds on the engine's retry ladder: its line reads `held` with the next rung ahead of the clock,
// inside the ladder's 300 s cap and the entry's TTL. The rung is written rather than drawn, so the rows after the line
// keep their rolls.
const LADDER_NEXT_PROBE: Partial<Record<RecipeCause, string>> = { probe_pool_exhausted: ahead(3 * MINUTE) };

/**
 * The index in `sessionSpec` of the session whose rows past the drawn pair are written, not rolled.
 */
const WRITTEN_SESSION = 9;
/**
 * The five subagents the written session holds beyond its drawn pair, written so the session holds an agent in each
 * lifecycle word, three of them released, one by the session's end, which the view keeps listed, and an id of the
 * length Claude Code gives a subagent. The list's order (a released row first, the parked row before the idle one, and
 * the idle row's last request newer than the drawn busy row's) differs from the lifecycle order main, busy, idle,
 * parked, so lines drawn in the list's order read differently from lines sorted into it. Each released row's last probe
 * dates its release inside the pool's cache TTL, since the router lists a released record for one TTL after the
 * release and then not at all.
 */
const WRITTEN_SUBAGENTS: Partial<KeepaliveRecipe>[] = [
  { agentId: 'e7c1', agentState: 'released', cause: 'agent_released', deadline: null, lastProbeAt: ago(4 * MINUTE), lastRequestAt: ago(9 * MINUTE), nextProbeAt: null, parkKind: null, state: 'ended', wakeAt: null },
  { agentId: '4f0c2a9e7b61d3c58', agentState: 'parked', cause: 'scheduled', deadline: ahead(41 * MINUTE), lastProbeAt: ago(2 * MINUTE), lastRequestAt: ago(6 * MINUTE), nextProbeAt: ahead(150_000), parkKind: 'background', state: 'retained', wakeAt: null },
  { agentId: 'b2d8', agentState: 'idle', cause: 'scheduled', deadline: null, lastProbeAt: ago(MINUTE), lastRequestAt: ago(30_000), nextProbeAt: ahead(200_000), parkKind: null, state: 'retained', wakeAt: null },
  { agentId: '91aa', agentState: 'released', cause: 'agent_released', deadline: null, lastProbeAt: ago(7 * MINUTE), lastRequestAt: ago(12 * MINUTE), nextProbeAt: null, parkKind: null, state: 'ended', wakeAt: null },
  { agentId: 'c3a9', agentState: 'released', cause: 'session_ended', deadline: null, lastProbeAt: ago(6 * MINUTE), lastRequestAt: ago(11 * MINUTE), nextProbeAt: null, parkKind: null, state: 'ended', wakeAt: null },
];

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
      nextProbeAt: fixedAgents ? (fixedAgents[j].nextProbeSeconds === null ? null : ahead(fixedAgents[j].nextProbeSeconds * 1000)) : cause === 'scheduled' ? ahead(rng.int(20, 280) * 1000) : cause === 'probing' ? ago(PROBE_IN_FLIGHT_MS) : (LADDER_NEXT_PROBE[cause] ?? null),
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
// The route lists one line per pin of an agent the engine holds and has not released, main's reading the row's own pin,
// so a session the mod released whole lists none. Every recipe the story writes stands on a pin, and a session no
// companion reports holds main's alone. The subagents' pins take the companion's five-minute TTL, their place's effort
// word and their last request as their last use.
SESSIONS.forEach((pin, i) => {
  pin.agentContexts = datedLines(
    pin.createdAt,
    activityOf(pin.keyId, pin.keyGeneration, pin.sessionId),
    RECIPES.filter((r) => r.keyId === pin.keyId && r.sessionId === pin.sessionId).flatMap((r, j) => {
      if (r.agentState === 'released' || r.accountId === null) return [];
      const own = { accountId: r.accountId, agentId: r.agentId, model: r.model };
      if (r.agentId === 'main') return [pinLine({ ...own, cacheTtlMs: pin.cacheTtlMs, effort: pin.effort, lastInputTokens: pin.lastInputTokens, lastUsedAt: pin.lastUsedAt ?? r.lastRequestAt })];
      return [pinLine({ ...own, cacheTtlMs: 5 * MINUTE, effort: effortAt(j), lastInputTokens: agentTokens(i, j), lastUsedAt: r.lastRequestAt })];
    }),
  );
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
SLOWED_PINS.forEach((pin, k) => {
  // The rows run newest first, and the main agent's newest turn is the pin's last use.
  const rows = REQUESTS.filter((r) => r.kind !== 'probe' && r.keyId === 'notebook' && r.sessionId === pin.sessionId);
  const main = rows.filter((r) => r.agentId === 'main');
  const last = main[0];
  const lastAttempt = last.attempts[last.attempts.length - 1];
  const lastInputTokens = promptOf(last);
  const effort = effortAt(k);
  const createdAt = main[main.length - 1].at;
  // Each agent's pin reads its own newest turn, which wrote the hour's cache.
  const lines = pin.agents.map(({ agentId, model }, j) => {
    const own = rows.filter((r) => r.agentId === agentId);
    const newest = own[0];
    return pinLine({
      accountId: 'studio',
      agentId,
      cacheTtlMs: HOUR,
      effort: agentId === 'main' ? effort : effortAt(k + j),
      lastInputTokens: promptOf(newest),
      lastUsedAt: newest.attempts[newest.attempts.length - 1].startedAt,
      model,
    });
  });
  SESSIONS.push({
    ...SESSION_SAMPLE,
    accountId: 'studio',
    agentContexts: datedLines(createdAt, activityOf('notebook', Date.parse(keyById.get('notebook')!.createdAt), pin.sessionId), lines),
    agentId: 'main',
    agents: pin.agents.length,
    burn: pin.burn,
    burnAt: lastAttempt.startedAt,
    burnEquivalent: burnEquivalentOf(pin.burn, 'studio'),
    burnUnit: 'window_share',
    cacheTtlMs: HOUR,
    companion: 'none',
    companionSince: null,
    createdAt,
    cycleMs: pin.cycleMs,
    effort,
    epoch: 1,
    estimated: false,
    forkDepth: null,
    forkedFrom: null,
    historical: false,
    keepalive: { cause: null, lastGuard: null, nextProbeAt: null, state: null },
    keyGeneration: Date.parse(keyById.get('notebook')!.createdAt),
    keyId: 'notebook',
    lastInputTokens,
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
const fleetTtlMs = (poolById.get(fleetPool)?.affinityTtlSeconds ?? 0) * 1_000;
// The rows run newest first, so each model's first row is its pin's last turn.
const FLEET_PINS = [...new Set(fleetRows.map((r) => r.canonicalModel))]
  .map((model) => fleetRows.find((r) => r.canonicalModel === model)!)
  .filter((last) => NOW - Date.parse(last.attempts[last.attempts.length - 1].startedAt) <= fleetTtlMs);
// The newest model's pin is the one the route lists the session's main agent under.
if (FLEET_PINS.length > 0) {
  const last = FLEET_PINS[0];
  const lastAttempt = last.attempts[last.attempts.length - 1];
  const lastInputTokens = promptOf(last);
  const keyGeneration = Date.parse(keyById.get('fleet')!.createdAt);
  const seen = ledgered.get(activityOf('fleet', keyGeneration, FLEET_SESSION_ID)) ?? null;
  // The session is first seen at the oldest turn of its last unbroken run: a turn that came once the pin before it had
  // passed the affinity window found the session holding no pin, so the router dated the session again from it.
  let firstSeen = fleetRows[0];
  for (const r of fleetRows.slice(1)) {
    if (Date.parse(firstSeen.at) - Date.parse(r.attempts[r.attempts.length - 1].startedAt) > fleetTtlMs) break;
    firstSeen = r;
  }
  // One line per standing pin, each read off its model's last turn as the row reads the newest one.
  const lines = FLEET_PINS.map((pin, j) => {
    const attempt = pin.attempts[pin.attempts.length - 1];
    return pinLine({ accountId: attempt.accountId, agentId: 'main', cacheTtlMs: 5 * MINUTE, effort: effortAt(j + 1), lastInputTokens: promptOf(pin), lastUsedAt: attempt.startedAt, model: pin.canonicalModel });
  });
  SESSIONS.push({
    ...SESSION_SAMPLE,
    accountId: lastAttempt.accountId,
    agentContexts: datedLines(firstSeen.at, activityOf('fleet', keyGeneration, FLEET_SESSION_ID), lines),
    agentId: 'main',
    agents: 1,
    burn: 0.36,
    burnAt: lastAttempt.startedAt,
    burnEquivalent: null,
    burnUnit: 'usd',
    cacheTtlMs: 5 * MINUTE,
    companion: 'none',
    companionSince: null,
    createdAt: firstSeen.at,
    cycleMs: null,
    effort: lines[0].effort,
    epoch: 1,
    estimated: false,
    forkDepth: null,
    forkedFrom: null,
    historical: false,
    keepalive: { cause: null, lastGuard: null, nextProbeAt: null, state: null },
    keyGeneration,
    keyId: 'fleet',
    lastInputTokens,
    lastRefreshAt: lastAttempt.startedAt,
    lastRequestAt: seen?.last ?? null,
    lastUsedAt: lastAttempt.startedAt,
    model: last.canonicalModel,
    pinned: true,
    presenceAgeSeconds: null,
    requests: seen?.requests ?? 0,
    runwayBounded: null,
    runwayMs: null,
    sessionId: FLEET_SESSION_ID,
    shedFrom: null,
    shedReason: null,
    speed: null,
  });
}

// The earlier holder of the `desktop` key id, which the analytics story draws as `desktop (deleted, created ...)`
// with the creation minute in the viewer's zone: the router lists a holder's session while its last request is inside
// the session expiry and the non-companion timeout, with that holder's own requests and no pin or recipe. The holder's
// last request ran on the session the live key holds now, so the row carries that id, and it joins the list after the
// probe times are filled in, which would otherwise hand it that session's next probe.
const holderRows = REQUESTS.filter((r) => r.kind !== 'probe' && r.keyId === 'desktop' && r.keyGeneration === DELETED_HOLDER_GENERATION && r.sessionId === SESSION_IDS[0]);
const holderLast = holderRows[0];
if (holderLast && insideSessionExpiry(holderLast.at, NOW)) {
  SESSIONS.push({
    ...SESSION_SAMPLE,
    accountId: holderLast.accountId,
    agentContexts: [],
    agentId: null,
    agents: 0,
    burn: null,
    burnAt: null,
    burnEquivalent: null,
    burnUnit: null,
    cacheTtlMs: null,
    companion: 'none',
    companionSince: null,
    createdAt: null,
    effort: null,
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
// inside the router's session expiry, the non-companion timeout leaving it out of the route's list sooner. The row is
// built at an instant since the mock's now is fixed at load, so the consistency test reads it inside the expiry and
// past it.
const blockedGeneration = Date.parse(keyById.get('desktop')!.createdAt);
const blocked = ledgered.get(activityOf('desktop', blockedGeneration, BLOCKED_SESSION_ID));
const blockedLast = REQUESTS.find((r) => r.kind !== 'probe' && r.keyId === 'desktop' && r.keyGeneration === blockedGeneration && r.sessionId === BLOCKED_SESSION_ID);
export function refusalOnlyRows(now: number): Session[] {
  if (!blocked || !blockedLast || !insideSessionExpiry(blockedLast.at, now)) return [];
  const row: Session = {
    ...SESSION_SAMPLE,
    accountId: null,
    agentContexts: [],
    agentId: null,
    agents: 0,
    burn: null,
    burnAt: null,
    burnEquivalent: null,
    burnUnit: null,
    cacheTtlMs: null,
    companion: 'none',
    companionSince: null,
    createdAt: null,
    effort: null,
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
  model: 'claude-sonnet-4-6',
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
const auxiliaryOnlyLine = pinLine({
  accountId: 'overflow',
  agentId: auxiliaryOnlyRecipe.agentId,
  cacheTtlMs: 5 * MINUTE,
  effort: 'high',
  lastInputTokens: 23_400,
  lastUsedAt: auxiliaryOnlyRecipe.lastRequestAt,
  model: auxiliaryOnlyRecipe.model,
});
const AUXILIARY_ONLY_CREATED_AT = ago(38 * MINUTE);
const noModLines = [
  pinLine({ accountId: 'contract', agentId: 'main', cacheTtlMs: 5 * MINUTE, effort: 'max', lastInputTokens: 41_200, lastUsedAt: ago(2 * MINUTE), model: 'claude-sonnet-5-5' }),
  pinLine({ accountId: 'contract', agentId: '5c8e', cacheTtlMs: 5 * MINUTE, effort: 'low', lastInputTokens: 7_900, lastUsedAt: ago(25 * MINUTE), model: HAIKU }),
];
const NO_MOD_CREATED_AT = ago(47 * MINUTE);
/** A written row that names no fork, no cycle, no runway and no shed stamp, and has reached no ledger. */
const writtenRow = (row: Pick<Session, 'accountId' | 'agentContexts' | 'agentId' | 'agents' | 'burn' | 'companion' | 'createdAt' | 'keepalive' | 'keyId' | 'presenceAgeSeconds' | 'sessionId'>, line: Omit<AgentContext, 'createdAt'>): Session => ({
  ...SESSION_SAMPLE,
  ...row,
  burnAt: line.lastUsedAt,
  burnEquivalent: null,
  burnUnit: 'usd',
  cacheTtlMs: line.cacheTtlMs,
  companionSince: null,
  cycleMs: null,
  effort: line.effort,
  epoch: 1,
  estimated: false,
  forkDepth: null,
  forkedFrom: null,
  historical: false,
  keyGeneration: Date.parse(keyById.get(row.keyId)!.createdAt),
  lastInputTokens: line.lastInputTokens,
  lastRefreshAt: line.lastUsedAt,
  lastRequestAt: null,
  lastUsedAt: line.lastUsedAt,
  model: line.model,
  pinned: true,
  requests: 0,
  runwayBounded: null,
  runwayMs: null,
  shedFrom: null,
  shedReason: null,
  speed: null,
});
const WRITTEN_SESSIONS = [
  writtenRow(
    {
      accountId: 'overflow',
      agentContexts: datedLines(AUXILIARY_ONLY_CREATED_AT, activityOf('laptop', Date.parse(keyById.get('laptop')!.createdAt), AUXILIARY_ONLY_SESSION_ID), [auxiliaryOnlyLine]),
      agentId: auxiliaryOnlyLine.agentId,
      agents: 1,
      burn: 0.05,
      companion: 'reporting',
      createdAt: AUXILIARY_ONLY_CREATED_AT,
      // Main holds no recipe, so the summary names no state, cause or guard, and its next probe is the subagent's.
      keepalive: { cause: null, lastGuard: null, nextProbeAt: auxiliaryOnlyRecipe.nextProbeAt, state: null },
      keyId: 'laptop',
      presenceAgeSeconds: 9,
      sessionId: AUXILIARY_ONLY_SESSION_ID,
    },
    auxiliaryOnlyLine,
  ),
  writtenRow(
    {
      accountId: 'contract',
      agentContexts: datedLines(NO_MOD_CREATED_AT, activityOf('fleet', Date.parse(keyById.get('fleet')!.createdAt), NO_MOD_SESSION_ID), noModLines),
      agentId: 'main',
      agents: noModLines.length,
      burn: 0.22,
      companion: 'none',
      createdAt: NO_MOD_CREATED_AT,
      keepalive: { cause: null, lastGuard: null, nextProbeAt: null, state: null },
      keyId: 'fleet',
      presenceAgeSeconds: null,
      sessionId: NO_MOD_SESSION_ID,
    },
    noModLines[0],
  ),
];
SESSIONS.push(...WRITTEN_SESSIONS);

/**
 * The sessions the route lists at `now`: a session no mod reports leaves once its last use is older than the timeout,
 * and each of its lines once its pin's last use is. The clock is handed in, as `refusalOnlyRows` takes it, since the
 * story is built against `NOW` and a test reads the list at any instant without moving `Date.now`. A row whose lines
 * all stand is answered as the seed holds it, so two reads of one instant answer the same rows.
 */
export const sessionsListedAt = (now: number, sessions: readonly Session[] = SESSIONS, seconds = NON_COMPANION_IDLE_SECONDS): Session[] =>
  sessions
    .filter((s) => s.companion !== 'none' || withinNonCompanionTimeout(lastUseOf(s), seconds, now))
    .map((s) => {
      if (s.companion !== 'none') return s;
      const standing = s.agentContexts.filter((c) => withinNonCompanionTimeout(c.lastUsedAt, seconds, now));
      return standing.length === s.agentContexts.length ? s : { ...s, agentContexts: standing };
    });

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
/** The pins by the account each sits on, each written `key|session|agent|model`. */
const PINS = new Map<string | null, Set<string>>();
const pinOn = (accountId: string | null, pin: string) => PINS.set(accountId, (PINS.get(accountId) ?? new Set()).add(pin));
for (const r of RECIPES) {
  const poolId = keyById.get(r.keyId)?.poolId;
  if (poolId === undefined || (r.agentState === 'released' && !pinKept(r, poolId))) continue;
  pinOn(r.accountId, `${r.keyId}|${r.sessionId}|${r.agentId}|${r.model}`);
}
for (const pin of SLOWED_PINS) for (const a of pin.agents) pinOn('studio', `notebook|${pin.sessionId}|${a.agentId}|${a.model}`);
for (const last of FLEET_PINS) pinOn(last.attempts[last.attempts.length - 1].accountId, `fleet|${FLEET_SESSION_ID}|main|${last.canonicalModel}`);
for (const row of WRITTEN_SESSIONS) for (const line of row.agentContexts) pinOn(line.accountId, `${row.keyId}|${row.sessionId}|${line.agentId}|${line.model}`);
// Main's auxiliary pin draws no line, but the router holds it, so the pools count it as they count every pin.
pinOn('overflow', `laptop|${AUXILIARY_ONLY_SESSION_ID}|main|${HAIKU}`);
const underKeys = (pins: Iterable<string>, keyIds: ReadonlySet<string>) => [...pins].filter((pin) => keyIds.has(pin.slice(0, pin.indexOf('|'))));
/**
 * The pins under a pool's current keys on one member's account, the one membership `pinsUnderPool` reads too, so a
 * key that moved pools counts its pins on the member cells of the pool that holds it now.
 */
export const memberPins = (keyIds: ReadonlySet<string>, accountId: string): number => underKeys(PINS.get(accountId) ?? [], keyIds).length;
/**
 * Every pin under a pool's current keys, one per key, session, agent and model as the route counts them, on a member's
 * account or not. A pin on an account that left the pool counts here and in no member's figure.
 */
export const pinsUnderPool = (keyIds: ReadonlySet<string>): number => [...PINS.values()].reduce((n, pins) => n + underKeys(pins, keyIds).length, 0);

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
  about: { profile: 'claude-code-2.1.310-linux-x64', version: SETTINGS_SAMPLE.about.version },
  access: { admin_configured: true },
  api_prices: API_PRICES_WIRE,
  listeners: { metrics: '127.0.0.1:9464', proxy: '127.0.0.1:8080', proxy_tls: false },
  retention: { body_capture_max_hours: 72, retention_days: 365, rollup_retention_days: 365 },
  sessions: { expire_after_seconds: SESSION_EXPIRE_SECONDS, non_companion_idle_seconds: NON_COMPANION_IDLE_SECONDS },
  storage: { api_prices_path: './api_prices.yaml', ledger_path: './data/ledger.sqlite', path: './data/router.sqlite', subscription_costs_path: './subscription_costs.yaml' },
  subscription_costs: SUBSCRIPTION_COSTS_WIRE,
};
