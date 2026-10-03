import type { BurnUnit, CompanionState, KeepaliveOverview, KeepaliveRecipe, PlanModelCells, RecipeReason, Session, Settings } from '@/api/types';
import { own } from '@/lib/analytics';
import { accountById } from './accounts';
import { PRICES } from './catalog';
import { keyById } from './pools';
import { DELETED_HOLDER_GENERATION, REQUESTS, SESSION_IDS } from './requests';
import { createRng } from './rng';
import { KEEPALIVE_OVERVIEW_SAMPLE, RECIPE_SAMPLE, SESSION_SAMPLE, SETTINGS_SAMPLE } from './samples';
import { MINUTE, ago, ahead } from './time';

const rng = createRng(2_024);

// `state: null` marks a session with no cache-warming recipe: the `ci` key opts
// out and `codex` keeps no caches warm.
// `companion` is independent of that: it says whether a cc-cache-keepalive
// companion ever registered for the session and is still reporting. A silent
// companion changes neither, because the retained prompts stay and are probed
// under the pool's idle limits. `subagents` pins the agents below main, which may run another
// model and, with that, land on another account. `idleMinutes` overrides the
// random idle on the few sessions that exist to straddle the expiry presets:
// without it every fixture is idle for under an hour and no preset in Settings
// would ever drop a row.
const sessionSpec: {
  accountId: string;
  agents: number;
  companion: CompanionState;
  idleMinutes?: number;
  keyId: string;
  model: string;
  state: Session['keepalive']['state'];
  subagents?: { accountId?: string; model: string };
}[] = [
  { keyId: 'desktop', accountId: 'business', model: 'claude-fable-5-1', state: 'busy', agents: 3, companion: 'reporting', subagents: { model: 'claude-sonnet-5', accountId: 'personal' } },
  { keyId: 'desktop', accountId: 'business', model: 'claude-fable-5-1', state: 'idle', agents: 1, companion: 'reporting' },
  { keyId: 'desktop', accountId: 'personal', model: 'claude-sonnet-5', state: 'idle', agents: 2, companion: 'silent' },
  { keyId: 'desktop', accountId: 'business', model: 'claude-opus-5', state: 'parked', agents: 1, companion: 'reporting' },
  { keyId: 'desktop', accountId: 'personal', model: 'claude-fable-5-1', state: 'idle', agents: 2, companion: 'reporting', subagents: { model: 'claude-sonnet-5' } },
  { keyId: 'desktop', accountId: 'business', model: 'claude-sonnet-5', state: 'busy', agents: 1, companion: 'reporting' },
  // Idle for longer than the pool's one-hour max idle, so its recipe is held and its retained prompt is gone.
  { keyId: 'laptop', accountId: 'personal', model: 'claude-sonnet-5', state: 'idle', agents: 1, companion: 'silent', idleMinutes: 95 },
  // The laptop key allows Haiku 4.5 and Sonnet 5 only, so its subagents stay inside that list.
  { keyId: 'laptop', accountId: 'business', model: 'claude-sonnet-5', state: 'busy', agents: 2, companion: 'reporting', subagents: { model: 'claude-haiku-4-5-20251001' } },
  { keyId: 'laptop', accountId: 'personal', model: 'claude-haiku-4-5-20251001', state: 'parked', agents: 1, companion: 'reporting' },
  { keyId: 'desktop', accountId: 'business', model: 'claude-fable-5-1', state: 'idle', agents: 2, companion: 'reporting' },
  { keyId: 'ci', accountId: 'evals', model: 'claude-haiku-4-5-20251001', state: null, agents: 1, companion: 'none' },
  { keyId: 'ci', accountId: 'evals', model: 'claude-haiku-4-5-20251001', state: null, agents: 2, companion: 'none', idleMinutes: 240 },
  { keyId: 'laptop', accountId: 'mail', model: 'claude-sonnet-5', state: null, agents: 1, companion: 'none' },
  { keyId: 'desktop', accountId: 'console', model: 'claude-opus-5', state: null, agents: 1, companion: 'none', idleMinutes: 570 },
  // Seen by the router but with nothing in the ledger, so the per-row jumps to
  // analytics and observe have no traffic to land on.
  { keyId: 'laptop', accountId: 'personal', model: 'claude-haiku-4-5-20251001', state: null, agents: 1, companion: 'none' },
];

const ledgered = new Map<string, { last: string; requests: number }>();
for (const r of REQUESTS) {
  if (r.sessionId === null) continue;
  const seen = ledgered.get(r.sessionId);
  if (!seen) ledgered.set(r.sessionId, { last: r.at, requests: 1 });
  else ledgered.set(r.sessionId, { last: r.at > seen.last ? r.at : seen.last, requests: seen.requests + 1 });
}

/** A 20k-token prefix read back at Fable 5.1's cache-read cell, 0.001675 of the Max 20x window per million, rounded. */
const PROBE_SHARE = 0.00003;

/**
 * Whether the cost table's 5-hour row for `model` (a dated id reads its undated name) prices every type in `types`:
 * a model with no row, or with a `null` cell among them, has no share for that usage.
 */
function sharePrices(model: string, types: (keyof PlanModelCells)[]): boolean {
  const rows = SETTINGS_SAMPLE.planTables.cost.windows.five_hour ?? {};
  const undated = model.replace(/-\d{8}$/, '');
  const row = own(rows, model) ? rows[model] : own(rows, undated) ? rows[undated] : undefined;
  return row !== undefined && types.every((t) => row[t] !== null);
}
const PIN_TYPES: (keyof PlanModelCells)[] = ['cacheRead', 'cacheWrite1h', 'cacheWrite5m', 'input', 'output'];

export const SESSIONS: Session[] = sessionSpec.map((spec, i) => {
  // The last entry deliberately falls outside the ledgered ids.
  const sessionId = i < SESSION_IDS.length ? SESSION_IDS[i] : rng.uuid();
  const seen = ledgered.get(sessionId) ?? null;
  // Both rolls are drawn whatever the spec says, so pinning one session's idle
  // does not shift the seeded stream the rest of these fixtures read from.
  const createdRoll = rng.int(8, 9 * 60) * MINUTE;
  const idleRoll = spec.state === 'busy' ? rng.int(5, 90) * 1000 : rng.int(2, 40) * MINUTE;
  const lastUsedAgo = spec.idleMinutes === undefined ? Math.min(createdRoll, idleRoll) : spec.idleMinutes * MINUTE;
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
  // A pin on a model whose row prices none of its types has no burn to rank by, as the route answers it.
  const burn = !pinned ? null : unit === 'usd' ? burnUsdRoll : PIN_TYPES.some((t) => sharePrices(spec.model, [t])) ? burnRoll : null;
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
    keyGeneration: Date.parse(keyById.get(spec.keyId)!.createdAt),
    createdAt: ago(createdAgo),
    lastUsedAt: ago(lastUsedAgo),
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
      lastGuard: null,
      netFiveHourShare: null,
      netUsd: null,
      nextProbeAt: null,
    },
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
  keepalive: { lastGuard: null, netFiveHourShare: null, netUsd: null, nextProbeAt: null, state: null },
  keyGeneration: Date.parse(keyById.get('laptop')!.createdAt),
  keyId: 'laptop',
  lastRequestAt: null,
  lastUsedAt: null,
  model: null,
  pinned: false,
  requests: 0,
  sessionId: '0f1d7c64-3b2a-4f58-9c7e-1a5b0e8d2f43',
});

// One recipe per agent on a pinned session, indexed by session. The reason
// explains why the main agent's probe is held; `null` means the next probe is
// scheduled.
const RECIPE_REASONS: (RecipeReason | null)[] = [
  'awaiting_cache_evidence',
  'net_negative',
  null,
  null,
  'overage',
  null,
  'idle_limit',
  'unsupported_shape',
  null,
  'cap_reached',
  null,
  null,
  null,
  null,
  null,
];

// A held subagent under a main agent that keeps probing, which is the case the
// state hints describe.
const SUBAGENT_HOLDS = new Map<number, RecipeReason>([
  [0, 'cold_probes'],
  [7, 'affinity_too_short'],
  // The last agent the story draws, so holding it moves no other roll.
  [9, 'pool_exhausted'],
]);

export const RECIPES: KeepaliveRecipe[] = SESSIONS.flatMap((pin, i) => {
  if (pin.keepalive.state === null) return [];
  const sub = sessionSpec[i].subagents;
  const agents = Array.from({ length: pin.agents }, (_, j) => (j === 0 ? 'main' : rng.uuid().slice(0, 4)));
  return agents.map((agentId, j) => {
    // A parked agent is probed like a busy one, so it never carries a hold reason.
    const reason = pin.keepalive.state === 'parked' ? null : j === 0 ? RECIPE_REASONS[i % RECIPE_REASONS.length] : j === 1 ? SUBAGENT_HOLDS.get(i) ?? null : null;
    // Cache evidence arrives with the first stream event and the probe timer
    // arms from it, so that agent is armed without a time rather than held.
    const state: KeepaliveRecipe['state'] =
      reason === 'awaiting_cache_evidence' ? 'armed' : reason ? 'held' : pin.keepalive.state === 'busy' && j === 0 ? 'probing' : 'armed';
    const model = j === 0 ? sessionSpec[i].model : sub?.model ?? sessionSpec[i].model;
    // A subagent spawned in the last turn may not have sent a request yet.
    const lastRequestAt = j === 0 ? pin.lastRequestAt : rng.int(0, 4) === 0 ? null : ago(rng.int(1, 40) * MINUTE);
    const accountId = j === 0 ? pin.accountId : sub?.accountId ?? pin.accountId;
    const ttlSeconds = model.includes('haiku') ? 300 : 3_600;
    // The unit a probe settles in follows the serving account's own funding and the probe's typed usage: a share of
    // the 5-hour window on a subscription account whose row for the model prices every type the probe spends,
    // dollars on an api_key one or on a model the table cannot price, the same decision the engine makes. A probe
    // replays a prefix of about 20k tokens at the cache-read rate: about 0.016% of the window, or $0.004.
    const probes = state === 'armed' && reason === null ? j + 1 : 0;
    const probeTypes: (keyof PlanModelCells)[] = ['cacheRead', 'cacheWrite5m', 'input', 'output', ...(ttlSeconds === 3_600 ? (['cacheWrite1h'] as const) : [])];
    const inShare = accountById.get(accountId ?? '')?.kind === 'oauth' && sharePrices(model, probeTypes);
    // Held for overage after probing, so an agent on plan quota settled in both units and carries a balance in each;
    // one whose probes the share could not price carries its dollar balance alone.
    const crossed = reason === 'overage';
    return {
      ...RECIPE_SAMPLE,
      accountId,
      agentId,
      keyId: pin.keyId,
      // The last real request re-armed this recipe; whatever else holds its next probe now is the gate's
      // reason, not the guard's, which the mock has no event sequence to distinguish further.
      lastGuard: 'rearm',
      lastRequestAt,
      model,
      // A freshly-armed agent has paid one probe's cost and earned no credit back yet; a held one has paid
      // nothing further since its last hold began.
      netFiveHourShare: inShare ? (crossed ? 3 * PROBE_SHARE : -PROBE_SHARE * probes) : 0,
      netUsd: crossed ? -0.012 : inShare ? 0 : -0.004 * probes,
      nextProbeAt: state === 'armed' && reason === null ? ahead(rng.int(20, 280) * 1000) : null,
      poolId: keyById.get(pin.keyId)?.poolId ?? 'all',
      reason,
      sessionId: pin.sessionId,
      state,
      ttlSeconds,
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
  // The session's own summary reads the main agent's guard note and balances, the way the admin route does.
  const main = RECIPES.find((r) => r.sessionId === pin.sessionId && r.agentId === 'main');
  pin.keepalive.lastGuard = main?.lastGuard ?? null;
  pin.keepalive.netFiveHourShare = main?.netFiveHourShare ?? null;
  pin.keepalive.netUsd = main?.netUsd ?? null;
}

// The earlier holder of the `desktop` key id, which the analytics story already draws as
// `desktop (deleted, created ... UTC)`: the ledger keeps its sessions until they expire, with no recipe of their
// own. It joins the list after the probe times are filled in because it carries the live session's id, which would
// otherwise hand it that session's next probe.
const firstDesktop = SESSIONS[0];
SESSIONS.push({
  ...firstDesktop,
  agents: 0,
  companion: 'none',
  companionSince: null,
  createdAt: null,
  historical: true,
  keepalive: { lastGuard: null, netFiveHourShare: null, netUsd: null, nextProbeAt: null, state: null },
  keyGeneration: DELETED_HOLDER_GENERATION,
  lastUsedAt: null,
  pinned: false,
});

// Counted off the recipes so the tiles cannot drift from the table below them.
const WARMED = SESSIONS.filter((pin) => pin.keepalive.state !== null);

// Read off the ledger rows, which cover the last 24 hours, so the tile and the
// analytics Cache section agree on what "expired between turns" counts.
const EXPIRED_BETWEEN_TURNS_24H = REQUESTS.filter((r) => r.status === 200 && r.usage.cacheRead === 0 && r.attempts[r.attempts.length - 1].cacheMissReason === 'none').length;

export const KEEPALIVE_OVERVIEW: KeepaliveOverview = {
  ...KEEPALIVE_OVERVIEW_SAMPLE,
  // Every session row, as the route counts them; the page counts the ones holding a recipe itself.
  sessions: SESSIONS.length,
  agents: {
    busy: WARMED.filter((p) => p.keepalive.state === 'busy').reduce((n, p) => n + p.agents, 0),
    idle: WARMED.filter((p) => p.keepalive.state === 'idle').reduce((n, p) => n + p.agents, 0),
    parked: WARMED.filter((p) => p.keepalive.state === 'parked').reduce((n, p) => n + p.agents, 0),
  },
  expiredBetweenTurns24h: EXPIRED_BETWEEN_TURNS_24H,
  // Router-wide, so every probe counted here belongs to a pool that keeps caches
  // warm; `codex` has warming off and contributes none. Hand-set at a busier
  // scale than the logged rows, at the same cost per probe as `costToday`.
  probes24h: { ok: 225, migrated: 18, skipped: 45, failed: 3 },
  costToday: [
    { poolId: 'all', usd: 2.61, capUsd: 4 },
    { poolId: 'codex', usd: 0, capUsd: null },
  ],
  netSaved7dUsd: 41.8,
  sessionsWithCompanion: SESSIONS.filter((pin) => pin.companion !== 'none').length,
};

export const SETTINGS: Settings = {
  ...SETTINGS_SAMPLE,
  about: { profile: 'claude-code-2.1.269-linux-x64', version: SETTINGS_SAMPLE.about.version },
  access: { adminConfigured: true },
  keepalive: { maxAgents: 1024, maxRetainedBytes: 268_435_456, maxSessions: 128 },
  listeners: { metrics: '127.0.0.1:9464', proxy: '127.0.0.1:8080' },
  // The route answers the table the ledger ships, so the captured answer is the table itself.
  planTables: SETTINGS_SAMPLE.planTables,
  pricing: { effectiveDate: '2026-09-30', models: PRICES, source: 'shipped' },
  retention: { bodyCaptureMaxHours: 72 },
  sessions: { expireAfterSeconds: 86_400 },
  storage: { ledgerPath: './data/ledger.sqlite', path: './data/router.sqlite', pricingPath: './pricing.yaml' },
  usage: { cacheTtlSeconds: 60 },
};
