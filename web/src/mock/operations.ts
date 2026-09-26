import type { AlertEvent, AlertRule, BurnUnit, CompanionState, KeepaliveOverview, KeepaliveRecipe, LifecycleDelivery, RecipeReason, Session, Settings } from '@/api/types';
import { accountById } from './accounts';
import { PRICES } from './catalog';
import { keyById } from './pools';
import { DELETED_HOLDER_GENERATION, REQUESTS, SESSION_IDS } from './requests';
import { createRng } from './rng';
import { ALERT_EVENT_SAMPLE, ALERT_RULE_SAMPLE, DELIVERY_SAMPLE, KEEPALIVE_OVERVIEW_SAMPLE, LIFECYCLE_DELIVERY_SAMPLE, RECIPE_SAMPLE, SESSION_SAMPLE, SETTINGS_SAMPLE } from './samples';
import { DAY, HOUR, MINUTE, NOW, ago, ahead, daysAgo } from './time';

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
      : 'meter_tokens'
    : null;
  // Drawn whatever the spec says, so pinning or the account's kind never shifts the stream the rows after
  // this one read; a usd row's burn is not modeled here, since the backend prices it off a differently scaled
  // stored average (`burn_usd`), never this meter-token-sized roll.
  const burnRoll = rng.int(200, 4_000);
  const burnAtRoll = ago(rng.int(2, 40) * MINUTE);
  const lastInputRoll = rng.int(500, 20_000);
  const lastRefreshRoll = ago(rng.int(2, 40) * MINUTE);
  return {
    ...SESSION_SAMPLE,
    sessionId,
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
    burn: pinned && unit !== 'usd' ? burnRoll : null,
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
      netMeterTokens: null,
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
  historical: false,
  keepalive: { lastGuard: null, netMeterTokens: null, netUsd: null, nextProbeAt: null, state: null },
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
    // The unit a probe settles in follows the serving account's own funding: meter tokens on a subscription
    // upstream, dollars on an api_key one, the same split the engine's own net carries.
    const paid = state === 'armed' && reason === null ? -0.0004 * (j + 1) : 0;
    const onPlan = accountById.get(accountId ?? '')?.kind === 'oauth';
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
      netMeterTokens: onPlan ? paid * 1_000 : 0,
      netUsd: onPlan ? 0 : paid,
      nextProbeAt: state === 'armed' && reason === null ? ahead(rng.int(20, 280) * 1000) : null,
      poolId: keyById.get(pin.keyId)?.poolId ?? 'all',
      reason,
      sessionId: pin.sessionId,
      state,
      ttlSeconds: model.includes('haiku') ? 300 : 3_600,
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
  pin.keepalive.netMeterTokens = main?.netMeterTokens ?? null;
  pin.keepalive.netUsd = main?.netUsd ?? null;
}

// The earlier holder of the `desktop` key id, which the analytics story already draws as `desktop (deleted)`:
// the ledger keeps its sessions until they expire, with no recipe of their own. It joins the list after the
// probe times are filled in because it carries the live session's id, which would otherwise hand it that
// session's next probe.
const firstDesktop = SESSIONS[0];
SESSIONS.push({
  ...firstDesktop,
  agents: 0,
  companion: 'none',
  companionSince: null,
  createdAt: null,
  historical: true,
  keepalive: { lastGuard: null, netMeterTokens: null, netUsd: null, nextProbeAt: null, state: null },
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

export const ALERT_RULES: AlertRule[] = [
  {
    ...ALERT_RULE_SAMPLE,
    createdAt: daysAgo(46),
    id: 'rule_question',
    kind: 'ask_user_question',
    scope: { type: 'all', id: null },
    threshold: null,
    windowId: null,
    webhookUrl: 'https://hooks.example.com/phoebis/question',
    enabled: true,
    hasSecret: true,
    updatedAt: daysAgo(46),
  },
  {
    ...ALERT_RULE_SAMPLE,
    createdAt: daysAgo(46),
    id: 'rule_drift',
    kind: 'client_drift',
    scope: { type: 'all', id: null },
    threshold: null,
    windowId: null,
    webhookUrl: 'https://hooks.example.com/phoebis/drift',
    enabled: true,
    hasSecret: false,
    updatedAt: daysAgo(46),
  },
  {
    ...ALERT_RULE_SAMPLE,
    createdAt: daysAgo(46),
    id: 'rule_reauth',
    kind: 'reauth_required',
    scope: { type: 'any_account', id: null },
    threshold: null,
    windowId: null,
    webhookUrl: 'https://hooks.example.com/phoebis/reauth',
    enabled: true,
    hasSecret: true,
    updatedAt: daysAgo(46),
  },
  {
    ...ALERT_RULE_SAMPLE,
    createdAt: daysAgo(46),
    id: 'rule_7d',
    kind: 'utilization_above',
    scope: { type: 'pool', id: 'all' },
    threshold: 0.9,
    windowId: '7d',
    webhookUrl: 'https://hooks.example.com/phoebis/quota',
    enabled: true,
    hasSecret: true,
    updatedAt: daysAgo(46),
  },
  {
    ...ALERT_RULE_SAMPLE,
    createdAt: daysAgo(46),
    id: 'rule_exhausted',
    kind: 'pool_exhausted',
    scope: { type: 'pool', id: 'all' },
    threshold: null,
    windowId: null,
    webhookUrl: 'https://hooks.example.com/phoebis/pool',
    enabled: true,
    hasSecret: false,
    updatedAt: daysAgo(46),
  },
  {
    ...ALERT_RULE_SAMPLE,
    createdAt: daysAgo(46),
    id: 'rule_funding',
    kind: 'funding_required',
    scope: { type: 'any_account', id: null },
    threshold: null,
    windowId: null,
    webhookUrl: 'https://hooks.example.com/phoebis/funding',
    enabled: true,
    hasSecret: true,
    updatedAt: daysAgo(46),
  },
  {
    ...ALERT_RULE_SAMPLE,
    createdAt: daysAgo(46),
    id: 'rule_cap',
    kind: 'keepalive_cost_cap',
    scope: { type: 'pool', id: 'all' },
    threshold: null,
    windowId: null,
    webhookUrl: 'https://hooks.example.com/phoebis/cap',
    enabled: true,
    hasSecret: false,
    updatedAt: daysAgo(46),
  },
  {
    ...ALERT_RULE_SAMPLE,
    createdAt: daysAgo(46),
    id: 'rule_idle',
    kind: 'session_idle',
    scope: { type: 'key', id: 'desktop' },
    threshold: 30,
    windowId: null,
    webhookUrl: 'https://hooks.example.com/phoebis/idle',
    enabled: true,
    hasSecret: true,
    updatedAt: daysAgo(46),
  },
  {
    ...ALERT_RULE_SAMPLE,
    createdAt: daysAgo(46),
    id: 'rule_active',
    kind: 'session_active',
    scope: { type: 'key', id: 'desktop' },
    threshold: null,
    windowId: null,
    webhookUrl: 'https://hooks.example.com/phoebis/active',
    enabled: true,
    hasSecret: false,
    updatedAt: daysAgo(46),
  },
  {
    ...ALERT_RULE_SAMPLE,
    createdAt: daysAgo(46),
    id: 'rule_parked',
    kind: 'session_parked',
    scope: { type: 'all', id: null },
    threshold: null,
    windowId: null,
    webhookUrl: 'https://hooks.example.com/phoebis/parked',
    enabled: true,
    hasSecret: false,
    updatedAt: daysAgo(46),
  },
  {
    ...ALERT_RULE_SAMPLE,
    createdAt: daysAgo(46),
    id: 'rule_subagent',
    kind: 'subagent_active',
    scope: { type: 'key', id: 'desktop' },
    threshold: null,
    windowId: null,
    webhookUrl: 'https://hooks.example.com/phoebis/subagent',
    enabled: true,
    hasSecret: false,
    updatedAt: daysAgo(46),
  },
  {
    ...ALERT_RULE_SAMPLE,
    createdAt: daysAgo(46),
    id: 'rule_refresh',
    kind: 'refresh_failed',
    scope: { type: 'account', id: 'team' },
    threshold: null,
    windowId: null,
    webhookUrl: 'https://hooks.example.com/phoebis/refresh',
    enabled: false,
    hasSecret: true,
    updatedAt: daysAgo(12),
  },
];

export const ALERT_EVENTS: AlertEvent[] = [
  {
    ...ALERT_EVENT_SAMPLE,
    id: 'evt_09',
    ruleId: 'rule_question',
    kind: 'ask_user_question',
    subject: `desktop, session ${SESSION_IDS[0].slice(0, 8)}`,
    detail: {
      agent: 'main',
      key: 'desktop',
      question: 'The ledger migration can rewrite rows in place or build a new table and swap it. In place is one step but locks writes for about 40 seconds; the swap needs twice the disk. Which do you want?',
      question_expires_at: NOW + 72 * HOUR - 18 * MINUTE,
      session: SESSION_IDS[0],
    },
    state: 'open',
    openedAt: ago(18 * MINUTE),
    resolvedAt: null,
    deliveries: [{ ...DELIVERY_SAMPLE, at: ago(18 * MINUTE), attempt: 1, error: null, state: 'opened', status: 200 }],
  },
  {
    ...ALERT_EVENT_SAMPLE,
    id: 'evt_10',
    ruleId: 'rule_drift',
    kind: 'client_drift',
    subject: 'client 2.1.274',
    detail: { client_version: '2.1.274', key: 'laptop', trigger: 'hash' },
    state: 'open',
    openedAt: ago(3 * HOUR + 40 * MINUTE),
    resolvedAt: null,
    deliveries: [
      { ...DELIVERY_SAMPLE, at: ago(3 * HOUR + 40 * MINUTE), attempt: 1, error: 'refused', state: 'opened', status: null },
      { ...DELIVERY_SAMPLE, at: ago(3 * HOUR + 39 * MINUTE), attempt: 2, error: null, state: 'opened', status: 200 },
    ],
  },
  {
    ...ALERT_EVENT_SAMPLE,
    id: 'evt_11',
    ruleId: 'rule_question',
    kind: 'ask_user_question',
    subject: `laptop, session ${SESSION_IDS[6].slice(0, 8)}`,
    detail: {
      agent: 'c21e',
      key: 'laptop',
      question: 'Two tests fail on the new cooldown clock. Should I relax the assertions or keep them and fix the clock?',
      question_expires_at: NOW + 72 * HOUR - 9 * HOUR,
      session: SESSION_IDS[6],
    },
    state: 'resolved',
    openedAt: ago(9 * HOUR),
    resolvedAt: ago(8 * HOUR + 41 * MINUTE),
    deliveries: [{ ...DELIVERY_SAMPLE, at: ago(9 * HOUR), attempt: 1, error: null, state: 'opened', status: 200 }],
  },
  // The store still holds the text; its body's capture window closed, so the route blanks it for every role.
  {
    ...ALERT_EVENT_SAMPLE,
    id: 'evt_14',
    ruleId: 'rule_question',
    kind: 'ask_user_question',
    subject: `laptop, session ${SESSION_IDS[6].slice(0, 8)}`,
    detail: {
      agent: 'main',
      key: 'laptop',
      question: 'The retention job overlaps the backup window. Move it to 04:00 or shorten the backup?',
      question_expires_at: NOW - 2 * HOUR,
      session: SESSION_IDS[6],
    },
    state: 'resolved',
    openedAt: ago(3 * DAY + 2 * HOUR),
    resolvedAt: ago(3 * DAY + 1 * HOUR),
    deliveries: [{ ...DELIVERY_SAMPLE, at: ago(3 * DAY + 2 * HOUR), attempt: 1, error: null, state: 'opened', status: 200 }],
  },
  {
    ...ALERT_EVENT_SAMPLE,
    id: 'evt_01',
    ruleId: 'rule_reauth',
    kind: 'reauth_required',
    subject: 'team (team@example.com)',
    detail: null,
    state: 'open',
    openedAt: ago(2 * DAY + 2 * HOUR),
    resolvedAt: null,
    deliveries: [{ ...DELIVERY_SAMPLE, at: ago(2 * DAY + 2 * HOUR), attempt: 1, error: null, state: 'opened', status: 200 }],
  },
  {
    ...ALERT_EVENT_SAMPLE,
    id: 'evt_02',
    ruleId: 'rule_7d',
    kind: 'utilization_above',
    subject: 'mail 7-day window at 91%',
    detail: null,
    state: 'open',
    openedAt: ago(6 * HOUR + 12 * MINUTE),
    resolvedAt: null,
    deliveries: [
      { ...DELIVERY_SAMPLE, at: ago(6 * HOUR + 12 * MINUTE), attempt: 1, error: 'connect timeout after 10s', state: 'opened', status: null },
      { ...DELIVERY_SAMPLE, at: ago(6 * HOUR + 11 * MINUTE), attempt: 2, error: null, state: 'opened', status: 200 },
    ],
  },
  {
    ...ALERT_EVENT_SAMPLE,
    id: 'evt_03',
    ruleId: 'rule_exhausted',
    kind: 'pool_exhausted',
    subject: 'pool all: 3 of 4 members cooling down',
    detail: null,
    state: 'resolved',
    openedAt: ago(1 * DAY + 4 * HOUR),
    resolvedAt: ago(1 * DAY + 3 * HOUR + 22 * MINUTE),
    deliveries: [{ ...DELIVERY_SAMPLE, at: ago(1 * DAY + 4 * HOUR), attempt: 1, error: null, state: 'opened', status: 200 }],
  },
  {
    ...ALERT_EVENT_SAMPLE,
    id: 'evt_04',
    ruleId: 'rule_7d',
    kind: 'utilization_above',
    subject: 'personal 7-day window at 92%',
    detail: null,
    state: 'resolved',
    openedAt: daysAgo(3),
    resolvedAt: daysAgo(2),
    deliveries: [{ ...DELIVERY_SAMPLE, at: daysAgo(3), attempt: 1, error: null, state: 'opened', status: 200 }],
  },
  {
    ...ALERT_EVENT_SAMPLE,
    id: 'evt_05',
    ruleId: 'rule_7d',
    kind: 'utilization_above',
    subject: 'business 7-day window at 90%',
    detail: null,
    state: 'resolved',
    openedAt: daysAgo(5),
    resolvedAt: daysAgo(4),
    deliveries: [{ ...DELIVERY_SAMPLE, at: daysAgo(5), attempt: 1, error: 'HTTP 500 from webhook', state: 'opened', status: 500 }, { ...DELIVERY_SAMPLE, at: daysAgo(5), attempt: 2, error: null, state: 'opened', status: 200 }],
  },
  {
    ...ALERT_EVENT_SAMPLE,
    id: 'evt_06',
    ruleId: 'rule_reauth',
    kind: 'reauth_required',
    subject: 'mail (mail@example.com)',
    detail: null,
    state: 'resolved',
    openedAt: daysAgo(9),
    resolvedAt: daysAgo(9),
    deliveries: [{ ...DELIVERY_SAMPLE, at: daysAgo(9), attempt: 1, error: null, state: 'opened', status: 200 }],
  },
  {
    ...ALERT_EVENT_SAMPLE,
    id: 'evt_07',
    ruleId: 'rule_exhausted',
    kind: 'pool_exhausted',
    subject: 'pool all: every member cooling down',
    detail: null,
    state: 'resolved',
    openedAt: daysAgo(12),
    resolvedAt: daysAgo(12),
    deliveries: [{ ...DELIVERY_SAMPLE, at: daysAgo(12), attempt: 1, error: null, state: 'opened', status: 200 }],
  },
  {
    ...ALERT_EVENT_SAMPLE,
    id: 'evt_08',
    ruleId: 'rule_7d',
    kind: 'utilization_above',
    subject: 'mail 7-day window at 94%',
    detail: null,
    state: 'resolved',
    openedAt: daysAgo(14),
    resolvedAt: daysAgo(13),
    deliveries: [{ ...DELIVERY_SAMPLE, at: daysAgo(14), attempt: 1, error: null, state: 'opened', status: 200 }],
  },
  {
    ...ALERT_EVENT_SAMPLE,
    id: 'evt_12',
    ruleId: 'rule_cap',
    kind: 'keepalive_cost_cap',
    subject: 'all',
    detail: { cap_usd: 4, pool: 'all', spent_usd: 4.02 },
    state: 'open',
    openedAt: ago(5 * HOUR + 6 * MINUTE),
    resolvedAt: null,
    deliveries: [{ ...DELIVERY_SAMPLE, at: ago(5 * HOUR + 6 * MINUTE), attempt: 1, error: null, state: 'opened', status: 200 }],
  },
  {
    ...ALERT_EVENT_SAMPLE,
    id: 'evt_16',
    ruleId: 'rule_idle',
    kind: 'session_idle',
    subject: `desktop, session ${SESSION_IDS[2].slice(0, 8)}`,
    // Written by a router that still opened and resolved the session kinds; the lifecycle log is where they
    // are read now, so the events card drops a row like this one rather than drawing it twice.
    detail: { key: 'desktop', pool: 'all', session: SESSION_IDS[2] },
    state: 'resolved',
    openedAt: daysAgo(16),
    resolvedAt: daysAgo(15),
    deliveries: [
      { ...DELIVERY_SAMPLE, at: daysAgo(16), attempt: 1, error: null, state: 'opened', status: 200 },
      { ...DELIVERY_SAMPLE, at: daysAgo(15), attempt: 1, error: null, state: 'resolved', status: 200 },
    ],
  },
  {
    ...ALERT_EVENT_SAMPLE,
    id: 'evt_13',
    ruleId: 'rule_refresh',
    kind: 'refresh_failed',
    subject: 'team',
    detail: { account: 'team', failures: 2, last_error: 'invalid_grant', outcome: 'failed' },
    state: 'resolved',
    openedAt: daysAgo(13),
    resolvedAt: daysAgo(12),
    deliveries: [
      { ...DELIVERY_SAMPLE, at: daysAgo(13), attempt: 1, error: null, state: 'opened', status: 200 },
      { ...DELIVERY_SAMPLE, at: daysAgo(12), attempt: 1, error: null, state: 'resolved', status: 200 },
    ],
  },
];

/**
 * One session told end to end, plus a second session starting as it closes: the four session kinds post a
 * delivery per event and open nothing, so this log is where they are read. Ids climb with `at`, which is the
 * order the router hands them out and the order the list is sorted back into.
 */
export const LIFECYCLE_DELIVERIES: LifecycleDelivery[] = [
  {
    ...LIFECYCLE_DELIVERY_SAMPLE,
    id: 101,
    at: ago(52 * MINUTE),
    ruleId: 'rule_active',
    kind: 'session_active',
    event: 'session_started',
    subject: `desktop, session ${SESSION_IDS[0].slice(0, 8)}`,
    detail: { event: 'session_started', key: 'desktop', pool: 'all', session: SESSION_IDS[0], source: 'request' },
    attempt: 1,
    error: null,
    status: 200,
  },
  {
    ...LIFECYCLE_DELIVERY_SAMPLE,
    id: 102,
    at: ago(48 * MINUTE),
    ruleId: 'rule_subagent',
    kind: 'subagent_active',
    event: 'subagent_started',
    subject: `desktop, session ${SESSION_IDS[0].slice(0, 8)}, agent a7f3`,
    detail: { agent: 'a7f3', event: 'subagent_started', key: 'desktop', parent_agent: 'main', pool: 'all', session: SESSION_IDS[0] },
    attempt: 1,
    error: null,
    status: 200,
  },
  {
    ...LIFECYCLE_DELIVERY_SAMPLE,
    id: 103,
    at: ago(35 * MINUTE),
    ruleId: 'rule_idle',
    kind: 'session_idle',
    event: 'session_idle',
    subject: `desktop, session ${SESSION_IDS[2].slice(0, 8)}`,
    detail: { event: 'session_idle', idle_minutes: 30, key: 'desktop', pool: 'all', session: SESSION_IDS[2] },
    attempt: 1,
    error: null,
    status: 200,
  },
  {
    ...LIFECYCLE_DELIVERY_SAMPLE,
    id: 104,
    at: ago(30 * MINUTE),
    ruleId: 'rule_parked',
    kind: 'session_parked',
    event: 'session_parked',
    subject: `desktop, session ${SESSION_IDS[1].slice(0, 8)}`,
    detail: { event: 'session_parked', key: 'desktop', pool: 'all', session: SESSION_IDS[1] },
    attempt: 1,
    error: null,
    status: 200,
  },
  {
    ...LIFECYCLE_DELIVERY_SAMPLE,
    id: 105,
    at: ago(22 * MINUTE),
    ruleId: 'rule_parked',
    kind: 'session_parked',
    event: 'session_resumed',
    subject: `desktop, session ${SESSION_IDS[1].slice(0, 8)}`,
    detail: { event: 'session_resumed', key: 'desktop', pool: 'all', session: SESSION_IDS[1] },
    attempt: 1,
    error: null,
    status: 200,
  },
  {
    ...LIFECYCLE_DELIVERY_SAMPLE,
    id: 106,
    at: ago(20 * MINUTE),
    ruleId: 'rule_idle',
    kind: 'session_idle',
    event: 'session_idle',
    subject: `desktop, session ${SESSION_IDS[1].slice(0, 8)}`,
    // The router always writes idle_minutes; the type leaves it optional, and the row must read without it.
    detail: { event: 'session_idle', key: 'desktop', pool: 'all', session: SESSION_IDS[1] },
    attempt: 1,
    error: null,
    status: 200,
  },
  {
    ...LIFECYCLE_DELIVERY_SAMPLE,
    id: 107,
    at: ago(11 * MINUTE),
    ruleId: 'rule_subagent',
    kind: 'subagent_active',
    event: 'subagent_ended',
    subject: `desktop, session ${SESSION_IDS[0].slice(0, 8)}, agent a7f3`,
    detail: { agent: 'a7f3', event: 'subagent_ended', key: 'desktop', parent_agent: 'main', pool: 'all', session: SESSION_IDS[0] },
    attempt: 1,
    error: null,
    status: 200,
  },
  {
    ...LIFECYCLE_DELIVERY_SAMPLE,
    id: 108,
    at: ago(9 * MINUTE),
    ruleId: 'rule_active',
    kind: 'session_active',
    event: 'session_ended',
    subject: `desktop, session ${SESSION_IDS[0].slice(0, 8)}`,
    detail: { event: 'session_ended', key: 'desktop', pool: 'all', session: SESSION_IDS[0], source: 'companion' },
    attempt: 1,
    error: null,
    status: 200,
  },
  {
    ...LIFECYCLE_DELIVERY_SAMPLE,
    id: 109,
    at: ago(4 * MINUTE),
    ruleId: 'rule_active',
    kind: 'session_active',
    event: 'session_started',
    subject: `desktop, session ${SESSION_IDS[3].slice(0, 8)}`,
    detail: { event: 'session_started', key: 'desktop', pool: 'all', session: SESSION_IDS[3], source: 'request' },
    attempt: 1,
    error: 'refused',
    status: null,
  },
];

export const SETTINGS: Settings = {
  ...SETTINGS_SAMPLE,
  about: { profile: 'claude-code-2.1.269-linux-x64', publicUrl: 'https://router.home.example', version: SETTINGS_SAMPLE.about.version },
  access: { adminConfigured: true, viewerConfigured: true },
  compatibility: { cacheDiagnostics: false, outboundGzip: false, profile: 'claude-code-2.1.269-linux-x64', usageEntrypoint: 'cli' },
  keepalive: { leaseSeconds: 90, maxAgents: 1024, maxRequestBytes: 8_388_608, maxRetainedBytes: 268_435_456, maxSessions: 128, maxWorkers: 4, oauthPrewarmVerified: false },
  listeners: { admin: '127.0.0.1:8081', metrics: '127.0.0.1:9464', proxy: '127.0.0.1:8080' },
  pricing: { effectiveDate: '2026-09-08', models: PRICES, source: 'shipped table' },
  quota: { pollIntervalSeconds: 3_600, sampleRetentionDays: 30 },
  retention: { bodyCaptureMaxHours: 72, ledgerDays: 90, quotaSamplesDays: 30, rollupDays: 400 },
  routing: { catalogCacheSeconds: 60, catalogRefreshSeconds: 21_600, cooldownFallbackSeconds: 60, maxAttempts: 3, modelRejectionSeconds: 3_600, startupFetchSeconds: 5 },
  sessions: { expireAfterSeconds: 86_400 },
  storage: { ledgerPath: './data/ledger.sqlite', path: './data/router.sqlite', pricingPath: './pricing.yaml' },
  usage: { cacheTtlSeconds: 60, refreshConcurrency: 2 },
};
