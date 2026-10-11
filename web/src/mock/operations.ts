import type { BurnUnit, CompanionState, KeepaliveOverview, KeepaliveRecipe, KeepaliveState, RecipeCause, RecipeState, Session } from '@/api/types';
import { anchorTier, ratesOf } from '@/lib/costs';
import { accountById } from './accounts';
import { FIXTURES } from './generated/fixtures';
import { POOLS, keyById } from './pools';
import { DELETED_HOLDER_GENERATION, REQUESTS, SESSION_IDS } from './requests';
import { createRng } from './rng';
import { KEEPALIVE_OVERVIEW_SAMPLE, RECIPE_SAMPLE, SESSION_SAMPLE, SETTINGS_SAMPLE, TABLES } from './samples';
import { MINUTE, NOW, ago, ahead } from './time';

const rng = createRng(2_024);

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
// request.
const sessionSpec: {
  accountId: string;
  agents: number;
  companion: CompanionState;
  idleMinutes?: number;
  keyId: string;
  lastUseFromRows?: boolean;
  model: string;
  state: Session['keepalive']['state'];
  subagents?: { accountId?: string; model: string };
}[] = [
  { keyId: 'desktop', accountId: 'primary', model: 'claude-fable-5-1', state: 'busy', agents: 3, companion: 'reporting', subagents: { model: 'claude-sonnet-5', accountId: 'nightly' } },
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
  // The last session with a recipe: four of its agents are written below, so it holds an agent in each lifecycle
  // word, two of them released.
  { keyId: 'desktop', accountId: 'primary', model: 'claude-fable-5-1', state: 'idle', agents: 6, companion: 'reporting' },
  { keyId: 'ci', accountId: 'research', model: 'claude-haiku-4-5-20251001', state: null, agents: 1, companion: 'none', lastUseFromRows: true },
  { keyId: 'ci', accountId: 'research', model: 'claude-haiku-4-5-20251001', state: null, agents: 2, companion: 'none', lastUseFromRows: true },
  { keyId: 'laptop', accountId: 'secondary', model: 'claude-sonnet-4-6', state: null, agents: 1, companion: 'none' },
  { keyId: 'desktop', accountId: 'overflow', model: 'claude-opus-5', state: null, agents: 1, companion: 'none', idleMinutes: 570 },
  // Seen by the router but with nothing in the ledger, so the per-row jump to
  // analytics has no traffic to land on.
  { keyId: 'laptop', accountId: 'nightly', model: 'claude-haiku-4-5-20251001', state: null, agents: 1, companion: 'none' },
];

/** How long the router lists a session after its last request, which the settings answer as well. */
const SESSION_EXPIRE_SECONDS = 86_400;

// Grouped as the route's activity query groups a session's rows, by key, generation and session with probes left out,
// so the earlier holder's rows count for its own row below, never for the live session that shares their id.
const activityOf = (keyId: string | null, keyGeneration: number, sessionId: string) => `${keyId}|${keyGeneration}|${sessionId}`;
const ledgered = new Map<string, { last: string; requests: number }>();
for (const r of REQUESTS) {
  if (r.sessionId === null || r.kind === 'probe') continue;
  const id = activityOf(r.keyId, r.keyGeneration, r.sessionId);
  const seen = ledgered.get(id);
  if (!seen) ledgered.set(id, { last: r.at, requests: 1 });
  else ledgered.set(id, { last: r.at > seen.last ? r.at : seen.last, requests: seen.requests + 1 });
}

/** Whether the cost model weighs `model` on the 5-hour window (a dated id reads its undated name): a weighted model
 *  prices every type, and one without a weight has no share for any usage. */
const sharePrices = (model: string): boolean => ratesOf(TABLES, 'five_hour', anchorTier(TABLES), model) !== null;

export const SESSIONS: Session[] = sessionSpec.map((spec, i) => {
  // The last entry deliberately falls outside the ledgered ids.
  const sessionId = i < SESSION_IDS.length ? SESSION_IDS[i] : rng.uuid();
  const keyGeneration = Date.parse(keyById.get(spec.keyId)!.createdAt);
  const seen = ledgered.get(activityOf(spec.keyId, keyGeneration, sessionId)) ?? null;
  // Both rolls are drawn whatever the spec says, so pinning one session's idle
  // does not shift the seeded stream the rest of these fixtures read from.
  const createdRoll = rng.int(8, 9 * 60) * MINUTE;
  const idleRoll = spec.state === 'busy' ? rng.int(5, 90) * 1000 : rng.int(2, 40) * MINUTE;
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
  // Drawn whatever the spec says, so pinning or the account's kind never shifts the stream the rows after this
  // one read. A row's burn is the per-hour figure the wire answers for its unit: dollars on the usd roll, a share
  // of the account's 5-hour window on the other.
  const burnRoll = rng.float(0.01, 0.9);
  const burnUsdRoll = rng.float(0.01, 0.5);
  const burnAtRoll = ago(rng.int(2, 40) * MINUTE);
  const lastInputRoll = rng.int(500, 20_000);
  const lastRefreshRoll = ago(rng.int(2, 40) * MINUTE);
  // A pin on a model the cost model does not weigh has no burn to rank by, as the route answers it.
  const burn = !pinned ? null : unit === 'usd' ? burnUsdRoll : sharePrices(spec.model) ? burnRoll : null;
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
    createdAt: ago(createdAgo),
    lastUsedAt: lastFromRows === null ? null : ago(lastUsedAgo),
    lastRequestAt: seen ? seen.last : null,
    pinned,
    requests: seen ? seen.requests : 0,
    burn,
    burnAt: pinned ? burnAtRoll : null,
    burnUnit: unit,
    cacheTtlMs: pinned ? 5 * MINUTE : null,
    lastInputTokens: pinned ? lastInputRoll : null,
    lastRefreshAt: pinned ? lastRefreshRoll : null,
    shedFrom: null,
    agents: spec.agents,
    companion: spec.companion,
    companionSince: spec.companion === 'silent' ? ago(rng.int(12, 150) * MINUTE) : null,
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
  agents: 0,
  companion: 'reporting',
  companionSince: null,
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
 * The index in `sessionSpec` of the last session with a recipe: its rows past the drawn pair are written, not rolled.
 */
const WRITTEN_SESSION = 9;
/**
 * The four subagents the last warmed session holds beyond its drawn pair, written so the session holds an agent in
 * each lifecycle word, two of them released, and an id of the length Claude Code gives a subagent. The list's order (a
 * released row first, the parked row before the idle one, and the idle row's last request newer than the drawn busy
 * row's) differs from the lifecycle order main, busy, idle, parked, so lines drawn in the list's order read differently
 * from lines sorted into it.
 */
const WRITTEN_SUBAGENTS: Partial<KeepaliveRecipe>[] = [
  { agentId: 'e7c1', agentState: 'released', cause: 'agent_released', deadline: null, lastProbeAt: ago(4 * MINUTE), lastRequestAt: ago(9 * MINUTE), nextProbeAt: null, parkKind: null, state: 'ended', wakeAt: null },
  { agentId: '4f0c2a9e7b61d3c58', agentState: 'parked', cause: 'scheduled', deadline: ahead(41 * MINUTE), lastProbeAt: ago(2 * MINUTE), lastRequestAt: ago(6 * MINUTE), nextProbeAt: ahead(150_000), parkKind: 'background', state: 'retained', wakeAt: null },
  { agentId: 'b2d8', agentState: 'idle', cause: 'scheduled', deadline: null, lastProbeAt: ago(MINUTE), lastRequestAt: ago(30_000), nextProbeAt: ahead(200_000), parkKind: null, state: 'retained', wakeAt: null },
  { agentId: '91aa', agentState: 'released', cause: 'agent_released', deadline: null, lastProbeAt: ago(7 * MINUTE), lastRequestAt: ago(12 * MINUTE), nextProbeAt: null, parkKind: null, state: 'ended', wakeAt: null },
];

export const RECIPES: KeepaliveRecipe[] = SESSIONS.flatMap((pin, i) => {
  const mainState = pin.keepalive.state;
  if (mainState === null) return [];
  const sub = sessionSpec[i].subagents;
  const agents = Array.from({ length: pin.agents }, (_, j) => (j === 0 ? 'main' : rng.uuid().slice(0, 4)));
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
    const rolled = j === 0 ? pin.lastRequestAt : rng.int(0, 4) === 0 ? null : ago(rng.int(1, 40) * MINUTE);
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
    const lastProbeAt = cause === 'awaiting_cache_evidence' ? null : cause === 'probing' ? ago(PROBE_IN_FLIGHT_MS) : ago((((i * 3 + j) % 4) + 1) * MINUTE);
    const probesToday = lastProbeAt === null ? 0 : ((i * 7 + j * 3) % 9) + 1;
    // The last warmed session's rows past its drawn pair are written, so a reader meets every lifecycle word.
    const written = i === WRITTEN_SESSION && j >= 2 ? WRITTEN_SUBAGENTS[j - 2] : undefined;
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
      nextProbeAt: cause === 'scheduled' ? ahead(rng.int(20, 280) * 1000) : cause === 'probing' ? ago(PROBE_IN_FLIGHT_MS) : null,
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
  // The session's own summary reads the main agent's guard note and cause, the way the admin route does.
  const main = RECIPES.find((r) => r.sessionId === pin.sessionId && r.agentId === 'main');
  pin.keepalive.lastGuard = main?.lastGuard ?? null;
  pin.keepalive.cause = main?.cause ?? null;
}

// The earlier holder of the `desktop` key id, which the analytics story draws as `desktop (deleted, created ...)`
// with the creation minute in the viewer's zone: the router lists a holder's session while its last request is inside
// the session expiry, with that holder's own requests and no pin or recipe. The holder's last request ran on the
// session the live key holds now, so the row carries that id, and it joins the list after the probe times are filled
// in, which would otherwise hand it that session's next probe.
const holderRows = REQUESTS.filter((r) => r.kind !== 'probe' && r.keyId === 'desktop' && r.keyGeneration === DELETED_HOLDER_GENERATION && r.sessionId === SESSION_IDS[0]);
const holderLast = holderRows[0];
if (holderLast && Date.parse(holderLast.at) + SESSION_EXPIRE_SECONDS * 1000 > NOW) {
  SESSIONS.push({
    ...SESSION_SAMPLE,
    accountId: holderLast.accountId,
    agentId: null,
    agents: 0,
    burn: null,
    burnAt: null,
    burnUnit: null,
    cacheTtlMs: null,
    companion: 'none',
    companionSince: null,
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
  });
}

// A pool's active pins are the pinned sessions under its keys, so the pool's figure, its members' pins and the
// dashboard's sum read one count.
for (const p of POOLS) p.activePins = SESSIONS.filter((x) => x.pinned && keyById.get(x.keyId)?.poolId === p.id).length;

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
 * answer goes through. The route answers the price table and the cost model the ledger ships, so the captured
 * `api_prices` and `subscription_costs` envelopes stand as they are.
 */
export const SETTINGS_WIRE = {
  ...settingsFixture,
  about: { profile: 'claude-code-2.1.269-linux-x64', version: SETTINGS_SAMPLE.about.version },
  access: { admin_configured: true },
  listeners: { metrics: '127.0.0.1:9464', proxy: '127.0.0.1:8080' },
  retention: { body_capture_max_hours: 72 },
  sessions: { expire_after_seconds: SESSION_EXPIRE_SECONDS },
  storage: { api_prices_path: './api_prices.yaml', ledger_path: './data/ledger.sqlite', path: './data/router.sqlite', subscription_costs_path: './subscription_costs.yaml' },
};
