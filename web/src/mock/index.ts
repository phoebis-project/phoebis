import type { AccountPatch, Api, CreateKeyInput, KeyInput, PoolInput } from '@/api/client';
import { AFFINITY_MODE_VALUES, SESSION_PLACEMENT_VALUES, SUBAGENT_PLACEMENT_VALUES, WARM_SESSIONS_VALUES, decodeLiveDelta, decodeLiveSnapshot, decodeSettings } from '@/api/decode';
import { ApiError, catalogUnion, slug } from '@/api/http';
import type {
  Account, AccountCatalog, AccountInput, AnalyticsQuery, ApiKey, BurnGroup, CatalogModel, CreatedApiKey, KeepaliveOverview, KeyModels, LiveEvent, MemberEligibility, ModelEvent,
  ModelRule, ModelsState, PairingInput, PairingPreview, PairingSession, Pool, PoolMember, RequestDetail, RequestLog, RequestPage, RequestQuery, RuleCooldown, Session, SessionInfo, Settings, Subscription,
} from '@/api/types';
import { modelRulesRefusal, type NumericKeepaliveKey } from '@/lib/forms';
import { matchesPattern } from '@/lib/pattern';
import { matchesPattern as matchesModelPattern, processRules } from '@/lib/patterns';
import { isFastScope } from '@/lib/quota';
import { ACCOUNTS, QUOTA_HISTORY, usageOf } from './accounts';
import { breakdownOf, burnOf, placementsOf, rewritesOf, seriesOf } from './analytics';
import { ATTENTION } from './attention';
import { CATALOG } from './catalog';
import { liveFeed } from './live';
import { KEEPALIVE_OVERVIEW, RECIPES, SESSIONS, SETTINGS_WIRE } from './operations';
import { KEYS, POOLS } from './pools';
import { ACCOUNT_ROLLUPS, ALL_ROLLUPS, REQUEST_EVENTS, REQUESTS, capturedBodiesFor, requestById } from './requests';
import { createRng } from './rng';
import { ACCOUNT_SAMPLE, KEY_SAMPLE, PAIRING_SAMPLE, POOL_SAMPLE } from './samples';
import { DAY, MINUTE, NOW, ago, ahead, iso } from './time';

// Fixed, like the live feed's draw: a pass over the mock reads the same secrets every run.
const MOCK_SEED = 4_211;

// A short delay keeps loading states honest without making the mock feel slow.
const latency = <T,>(value: T, ms = 120): Promise<T> => new Promise((resolve) => setTimeout(() => resolve(value), ms));

// The routes' own wording, so a page draws the text the router would have sent.
const KEY_TAKEN = 'a key with this id exists';
const POOL_TAKEN = 'a pool with this id exists';

/**
 * The policy check's bounds in its own order and wording, written out apart from the form's table so a form that
 * drifts from the route is refused here rather than agreed with.
 */
const KEEPALIVE_RANGES: [NumericKeepaliveKey, string, number, number][] = [
  ['coldProbeLimit', 'cold_probe_limit', 1, 20],
  ['retrySeconds', 'retry_seconds', 1, 3_600],
  ['retryMaxSeconds', 'retry_max_seconds', 1, 604_800],
  ['maxIdleSeconds', 'max_idle_seconds', 0, 31_536_000],
  ['maxIdleTicks', 'max_idle_ticks', 0, 1_000_000],
  ['leadSeconds5m', 'lead_seconds_5m', 0, 299],
  ['leadSeconds1h', 'lead_seconds_1h', 0, 3_599],
  ['jitterSeconds5m', 'jitter_seconds_5m', 0, 299],
  ['jitterSeconds1h', 'jitter_seconds_1h', 0, 3_599],
  ['netNegativeGraceProbes', 'net_negative_grace_probes', 0, 100],
];

/**
 * The rule list's refusals in the routes' order and wording. A list the body leaves out is the routes' "keep
 * what is stored", never a refusal; a list that is present is judged the way the form's own editor judges it.
 */
const rulesRefusal = (rules: ModelRule[] | undefined): string | null =>
  rules === undefined ? null : modelRulesRefusal(rules);

/**
 * The key routes read the list, then the mode, so a body wrong in both is refused for the list. An absent field is the
 * stored one, or the default on a create.
 */
const keyRulesRefusal = (input: Partial<Pick<KeyInput, 'modelRules' | 'modelRulesMode'>>): string | null => {
  const mode = input.modelRulesMode;
  const rules = rulesRefusal(input.modelRules) ?? (mode === undefined || mode === 'extend' || mode === 'replace' ? null : 'model_rules_mode is extend or replace');
  if (rules) return rules;
  return null;
};

/** The shedding and failover refusals, in the order and words of the pool routes. */
const engineRefusal = (input: PoolInput): string | null => {
  const whole = (v: number, min: number, max: number) => Number.isInteger(v) && v >= min && v <= max;
  if (!(input.shedShare >= 0 && input.shedShare <= 1)) return 'shed_share is between 0 and 1';
  if (!whole(input.burnWindowSeconds, 60, Number.MAX_SAFE_INTEGER)) return 'burn_window_seconds is at least 60';
  if (!whole(input.sourceDwellSeconds, 60, Number.MAX_SAFE_INTEGER)) return 'source_dwell_seconds is at least 60';
  if (!WARM_SESSIONS_VALUES.some((mode) => mode === input.warmSessions)) return 'warm_sessions must be always, never, or when_needed';
  if (!whole(input.retryInPlaceSeconds, 0, 300)) return 'retry_in_place_seconds is 0 to 300';
  if (!whole(input.inFlightCap, 1, 4_294_967_295)) return 'in_flight_cap is at least 1';
  if (!(input.paceFloor >= 0 && input.paceFloor <= 1)) return 'pace_floor is between 0 and 1';
  if (!whole(input.overageMoveGraceMinutes, 0, 300)) return 'overage_move_grace_minutes is 0 to 300';
  if (!whole(input.windDownMinutes, 0, 10_080)) return 'wind_down_minutes is at most 10080 (one week)';
  return null;
};

/** The pin and placement spellings, in the order and words of the pool routes; an absent field is kept. */
const affinityRefusal = (input: Partial<PoolInput>): string | null => {
  if (input.affinityMode !== undefined && !AFFINITY_MODE_VALUES.some((mode) => mode === input.affinityMode)) return 'affinity_mode must be always or when_cached';
  if (input.subagentPlacement !== undefined && !SUBAGENT_PLACEMENT_VALUES.some((p) => p === input.subagentPlacement)) return 'subagent_placement must be by_cohort, by_strategy, or with_main';
  if (input.sessionPlacement !== undefined && !SESSION_PLACEMENT_VALUES.some((p) => p === input.sessionPlacement)) return 'session_placement must be by_strategy or by_warm_prefix';
  return null;
};

/** The member write's refusals, which the routes read once the pool's own fields have passed. */
const memberRefusal = (members: PoolInput['members']): string | null => {
  for (const m of members) {
    if (m.budgetDailyUsd != null && !(m.budgetDailyUsd > 0)) return 'budget_daily_usd is above 0';
    if (m.budgetMonthlyUsd != null && !(m.budgetMonthlyUsd > 0)) return 'budget_monthly_usd is above 0';
  }
  return null;
};

/** The affinity floor, the rule list, the keepalive policy's and the engine's refusals, first one first, as the pool routes answer them. */
const poolRefusal = (input: PoolInput): string | null => {
  // The route reads the threshold as an unsigned whole number while it parses the body, before any other check.
  const tokens = input.minSharedPrefixTokens;
  if (tokens !== undefined && !(Number.isSafeInteger(tokens) && tokens >= 0)) return 'body must be a JSON object';
  const affinity = input.affinityTtlSeconds;
  if (affinity !== 0 && affinity < 60) return 'affinity_ttl_seconds is 0 or at least 60';
  const rules = rulesRefusal(input.modelRules);
  if (rules) return rules;
  const k = input.keepalive;
  for (const [key, field, min, max] of KEEPALIVE_RANGES) {
    const value = k[key];
    if (value < min || value > max) return `keepalive.${field} must be between ${min} and ${max}, not ${value}`;
    if (key === 'retryMaxSeconds' && k.retryMaxSeconds < k.retrySeconds) return 'keepalive.retry_max_seconds must be at least keepalive.retry_seconds';
    // The route reads the two sums once every lead and jitter has passed its own bound.
    if (key === 'jitterSeconds1h') {
      if (k.leadSeconds5m + k.jitterSeconds5m >= 300) return 'keepalive.lead_seconds_5m plus keepalive.jitter_seconds_5m must stay under the cache TTL they apply to';
      if (k.leadSeconds1h + k.jitterSeconds1h >= 3_600) return 'keepalive.lead_seconds_1h plus keepalive.jitter_seconds_1h must stay under the cache TTL they apply to';
    }
  }
  if (k.costCapUsdPerDay !== null && !(k.costCapUsdPerDay > 0 && Number.isFinite(k.costCapUsdPerDay))) return 'keepalive.probe_cost_cap_usd_per_day must be null or a positive number';
  if (k.capWindowSharePerDay !== null && !(k.capWindowSharePerDay >= 0 && k.capWindowSharePerDay <= 1)) return 'keepalive.probe_cap_window_share_per_day must be null or between 0 and 1';
  const engine = engineRefusal(input) ?? affinityRefusal(input);
  if (engine) return engine;
  if (k.enabled && affinity < 300) {
    return 'keepalive.enabled requires affinity_ttl_seconds of at least 300: a probe follows the session pin, and a pin shorter than the shortest cache TTL cannot keep the session on the account it warmed';
  }
  return null;
};

// The registered redirect is a loopback port on the machine running Claude Code,
// so the callback the operator pastes carries both halves of the exchange.
const PAIRING_REDIRECT_URI = 'http://localhost:54545/callback';

const MODEL_SCOPE_PREFIX = 'model:';

/**
 * The Models view's newest rejections and permits, lifted off the request log's own events so each names a request
 * the drawer opens; the state route's live cooldowns come from the accounts.
 */
const modelEvents = (kind: 'model_permitted' | 'model_rejected'): ModelEvent[] =>
  [...REQUEST_EVENTS.values()]
    .flat()
    .filter((e) => e.kind === kind)
    .sort((a, b) => Date.parse(b.at) - Date.parse(a.at))
    .map((e) => ({
      // Every model_rejected and model_permitted event this mock produces sets both fields: an unnamed
      // account or model here is the fixture builder's own bug, worth a crash rather than a silent blank.
      accountId: e.accountId!,
      at: e.at,
      keyId: e.keyId,
      model: e.model!,
      rejectedBy: Array.isArray(e.detail?.rejected_by) ? e.detail.rejected_by.map(String) : [],
      requestId: typeof e.detail?.source_request_id === 'string' ? e.detail.source_request_id : null,
      sessionId: e.sessionId,
      upstreamRequestId: typeof e.detail?.request_id === 'string' ? e.detail.request_id : null,
      wireModel: String(e.detail?.wire_model ?? ''),
    }));
const MODEL_REJECTIONS = modelEvents('model_rejected');
const MODEL_PERMITS = modelEvents('model_permitted');

/**
 * The signed-in mark, which outlives a reload the way the router's session cookie does: the smoke runner writes
 * it so every mount starts signed in, and a login writes it so a browser pass that loads each view by URL stays
 * signed in.
 */
const SEEDED_SESSION_KEY = 'phoebis.mockSession';
const MOCK_SESSION: SessionInfo = { sessionId: 'mock-admin' };
const seededSession = (): boolean => {
  try {
    return localStorage.getItem(SEEDED_SESSION_KEY) !== null;
  } catch {
    return false;
  }
};
const seedSession = (signedIn: boolean) => {
  try {
    if (signedIn) localStorage.setItem(SEEDED_SESSION_KEY, 'admin');
    else localStorage.removeItem(SEEDED_SESSION_KEY);
  } catch {
    // No storage (the Node tests): the session lives on the instance alone.
  }
};

/**
 * Set to stand in for a fresh installation: every registry list and every ledger read answers nothing, while the
 * seeded store stays in memory for the next read without the switch.
 */
export const EMPTY_STORE_KEY = 'phoebis.mockEmpty';
export const emptyStore = (): boolean => {
  try {
    return localStorage.getItem(EMPTY_STORE_KEY) === '1';
  } catch {
    return false;
  }
};

/** The keepalive overview of a router with no session and no probe yet. */
const EMPTY_KEEPALIVE_OVERVIEW: KeepaliveOverview = {
  agents: { busy: 0, idle: 0, parked: 0 },
  busGaps: 0,
  costToday: [],
  expiredBetweenTurns24h: 0,
  netSaved7dUsd: 0,
  probes24h: { failed: 0, migrated: 0, ok: 0, skipped: 0 },
};

type WireMetric = { quality: string; series: number[]; value: number | null };
/** The feed's metrics that need a finished request to read anything, which an idle router answers as null. */
const NEEDS_FINISHED = new Set(['error_rate', 'ttfb_p50', 'ttfb_p95', 'ttfb_p99']);
/**
 * A feed tick as a router with no traffic measures it. The simulator's ticks read its own requests, which an empty
 * store would never have run.
 */
const idleTick = (tick: unknown) => {
  const t = tick as { metrics: Record<string, WireMetric> };
  const metrics = Object.fromEntries(Object.entries(t.metrics).map(([key, m]) => [key, { ...m, series: m.series.map(() => 0), value: NEEDS_FINISHED.has(key) ? null : 0 }]));
  return { ...t, metrics };
};

function fromCallbackUrl(url: string): { code: string; state: string } | null {
  let params: URLSearchParams;
  try {
    params = new URL(url).searchParams;
  } catch {
    return null;
  }
  const code = params.get('code');
  const state = params.get('state');
  return code && state ? { code, state } : null;
}

/** A flag outranks the enabled bit and the cooldowns in the status the account view folds, so pausing never hides one. */
const FLAGGED: Account['status'][] = ['funding_required', 'reauth_required', 'unpaired'];
/** A fast-lane row holds fast-mode requests alone, so the account cools only under another scope's row. */
const cooling = (cooldowns: Account['cooldowns']) => cooldowns.some((c) => !isFastScope(c.scope));
const statusOf = (a: Account, enabled: boolean): Account['status'] => {
  if (FLAGGED.includes(a.status)) return a.status;
  if (!enabled) return 'paused';
  return cooling(a.cooldowns) ? 'cooldown' : 'ready';
};

type BudgetedSeed = Pick<PoolMember, 'budgetDailyUsd' | 'budgetMonthlyUsd' | 'spends'>;

/**
 * The router's end for a member at a budget, which the route answers as `budget_until`: among the budgets whose
 * report, its period not yet over, has reached the budget, the latest period end, since a day's end clears
 * nothing while the month's budget stands; null while none is reached.
 */
const budgetUntilOf = (member: BudgetedSeed, now: number): string | null =>
  member.spends
    .filter((s) => {
      const budget = s.period === 'day' ? member.budgetDailyUsd : member.budgetMonthlyUsd;
      return budget !== null && s.spentUsd >= budget && s.periodEnd !== null && Date.parse(s.periodEnd) > now;
    })
    .reduce<string | null>((latest, s) => (latest === null || Date.parse(s.periodEnd ?? '') > Date.parse(latest) ? s.periodEnd : latest), null);

/**
 * The pool route's reading of a member, in its order: paused or not ready is unavailable, then an account-wide
 * cooldown, then a spend at or past either of the member's budgets inside the period it was reported for, then
 * overage the router observed under a policy that admits none.
 */
const eligibilityOf = (a: Account | undefined, policy: PoolInput['overagePolicy'], member: BudgetedSeed): MemberEligibility => {
  if (!a || !a.routingEnabled || FLAGGED.includes(a.status)) return 'unavailable';
  if (a.cooldowns.some((c) => c.scope === 'account' && Date.parse(c.until) > Date.now())) return 'cooldown';
  if (budgetUntilOf(member, Date.now()) !== null) return 'budget';
  if (policy === 'never' && a.overage?.inUse) return 'overage';
  return 'eligible';
};

/**
 * A written member as the route answers it: the kind read off the account, and no spend, since the mock keeps no
 * ledger to price one from. A member sent without a budget field has no budget over that period.
 */
const writtenMember = (m: PoolInput['members'][number], account: Account | undefined, eligible: (member: Omit<PoolMember, 'eligible'>) => MemberEligibility): PoolMember => {
  const member = { accountId: m.accountId, budgetDailyUsd: m.budgetDailyUsd ?? null, budgetMonthlyUsd: m.budgetMonthlyUsd ?? null, budgetUntil: null, kind: account?.kind ?? 'oauth', priority: m.priority, spends: [] };
  return { ...member, eligible: eligible(member) };
};

/**
 * The route's `keys=` rule: `<id>@<generation>` names one lifetime and a bare id the live key's, so an earlier
 * holder's rows answer only to their own selector.
 */
const selectsKey = (selector: string, r: RequestLog, live: Map<string, number>) =>
  r.keyId !== null && (selector.includes('@') ? `${r.keyId}@${r.keyGeneration}` === selector : r.keyId === selector && live.get(selector) === r.keyGeneration);

function matchesQuery(r: RequestLog, q: RequestQuery, live: Map<string, number>): boolean {
  // A request the router never attributed matches no filter on that field.
  if (q.keyIds?.length && !q.keyIds.some((k) => selectsKey(k, r, live))) return false;
  if (q.poolIds?.length && (r.poolId === null || !q.poolIds.includes(r.poolId))) return false;
  if (q.accountIds?.length && (r.accountId === null || !q.accountIds.includes(r.accountId))) return false;
  if (q.models?.length && !q.models.includes(r.model)) return false;
  if (q.statuses?.length && (r.status === null || !q.statuses.includes(r.status))) return false;
  if (q.outcomes?.length && !q.outcomes.includes(r.outcome)) return false;
  if (q.kind && r.kind !== q.kind) return false;
  if (q.from && Date.parse(r.at) < Date.parse(q.from)) return false;
  if (q.to && Date.parse(r.at) > Date.parse(q.to)) return false;
  if (q.search) {
    const needle = q.search.toLowerCase();
    const hay = [r.id, r.sessionId ?? '', r.keyId ?? '', r.accountId ?? '', r.model, r.errorCode ?? '', r.agentId ?? '', ...r.attempts.flatMap((a) => [a.upstreamRequestId ?? '', a.upstreamMessageId ?? ''])].join(' ').toLowerCase();
    if (!hay.includes(needle)) return false;
  }
  // The route matches the same wildcard grammar through `LIKE ... ESCAPE`, so a row with no user agent matches none.
  if (q.userAgent && !matchesPattern(q.userAgent, r.userAgent ?? '')) return false;
  // The route reads a row with no class as `-`, the marker the rollups file it under.
  if (q.client && !matchesPattern(q.client, r.client ?? '-')) return false;
  if (q.agentTypes?.length && !q.agentTypes.includes(r.agentType ?? '')) return false;
  if (q.turnOrigins?.length && !q.turnOrigins.includes(r.turnOrigin ?? '')) return false;
  // The serving attempt is the last; an answer that named no speed reads as standard, as the route reads it.
  if (q.speeds?.length && !q.speeds.includes(r.attempts[r.attempts.length - 1]?.response.speed ?? 'standard')) return false;
  if (q.held && r.held === null) return false;
  return true;
}

/**
 * The route encodes a cursor as `<received_at>:<id>` and resumes strictly past that row in
 * `received_at DESC, id DESC` order; the id breaks the tie so rows sharing a millisecond are neither
 * repeated nor skipped.
 */
function isAfterCursor(r: RequestLog, cursor: string): boolean {
  const sep = cursor.indexOf(':');
  const at = Number(cursor.slice(0, sep));
  const id = cursor.slice(sep + 1);
  const rowAt = Date.parse(r.at);
  return rowAt < at || (rowAt === at && r.id < id);
}

export class MockApi implements Api {
  private accounts = ACCOUNTS.map((a) => ({ ...a }));
  private keys: ApiKey[] = [...KEYS];
  private memberWriteFailure: string | null = null;
  private pairings = new Map<string, { accountId: string; state: string }>();
  // The route reads every member's kind and verdict afresh on each answer; the story's are read once, at load.
  private pools: Omit<Pool, 'keys'>[] = POOLS.map((p) => ({
    ...p,
    members: p.members.map((m) => {
      const account = this.accounts.find((a) => a.id === m.accountId);
      return { ...m, budgetUntil: budgetUntilOf(m, Date.now()), eligible: eligibilityOf(account, p.overagePolicy, m), kind: account?.kind ?? 'oauth' };
    }),
  }));
  private secretRng = createRng(MOCK_SEED);
  private session: SessionInfo | null = null;
  private sessions: Session[] = [...SESSIONS];
  private settings: Settings = decodeSettings(SETTINGS_WIRE);

  private token(n: number) {
    return Array.from({ length: n }, () => 'abcdefghijklmnopqrstuvwxyz0123456789'[this.secretRng.int(0, 35)]).join('');
  }

  /** The pool as the route answers it, with its keys listed from the key registry at the time of the read. */
  private poolView(p: Omit<Pool, 'keys'>): Pool {
    const keys = this.keys.filter((k) => k.poolId === p.id).map((k) => ({ enabled: k.enabled, id: k.id, label: k.name, prefix: k.prefix }));
    return { ...p, keys };
  }

  private account(accountId: string): Account {
    const a = this.accounts.find((x) => x.id === accountId);
    if (!a) throw new ApiError(404, 'account not found', null);
    return a;
  }

  private putAccount(next: Account): Account {
    this.accounts = this.accounts.map((a) => (a.id === next.id ? next : a));
    return next;
  }

  analyticsBudgets() {
    if (emptyStore()) return latency([]);
    // Read off the members as they stand, so a budget set on the Pools form shows here; one row per budgeted
    // period, and the route orders by pool, then account, then the day ahead of the month, which the stable sort
    // keeps from the order each member's rows are built in.
    const members = this.pools.flatMap((p) =>
      p.members.flatMap((m) =>
        (
          [
            ['day', m.budgetDailyUsd],
            ['month', m.budgetMonthlyUsd],
          ] as const
        ).flatMap(([budgetPeriod, budgetUsd]) => {
          if (budgetUsd === null) return [];
          const spend = m.spends.find((s) => s.period === budgetPeriod);
          return [{ accountId: m.accountId, budgetPeriod, budgetUsd, kind: m.kind, periodEnd: spend?.periodEnd ?? null, poolId: p.id, spentUsd: spend?.spentUsd ?? null }];
        }),
      ),
    );
    return latency(members.sort((a, b) => a.poolId.localeCompare(b.poolId) || a.accountId.localeCompare(b.accountId)));
  }

  analyticsPlacements(query: Pick<AnalyticsQuery, 'from' | 'keyIds' | 'poolIds' | 'to'>) {
    return latency(placementsOf(emptyStore() ? [] : REQUESTS, query));
  }

  async cancelPairing(pairingId: string) {
    this.pairings.delete(pairingId);
    return latency(undefined, 150);
  }

  async clearCooldown(accountId: string, scope: string) {
    const a = this.account(accountId);
    const cooldowns = a.cooldowns.filter((c) => c.scope !== scope);
    this.putAccount({ ...a, cooldowns, status: cooling(cooldowns) ? a.status : 'ready' });
    return latency(undefined, 200);
  }

  async clearFunding(accountId: string) {
    const a = this.account(accountId);
    this.putAccount({ ...a, extraUsage: a.extraUsage ? { ...a.extraUsage, disabledReason: null } : null, status: a.status === 'funding_required' ? 'ready' : a.status });
    return latency(undefined, 200);
  }

  async completePairing(pairingId: string, input: PairingInput): Promise<PairingPreview> {
    const submitted = 'url' in input ? fromCallbackUrl(input.url) : input;
    if (!submitted) throw new Error('That URL carries no authorization code and state. Paste the whole address the browser landed on, or the code on its own.');
    // The router refuses a callback whose state does not belong to the pairing the id names.
    const pairing = this.pairings.get(pairingId);
    if (!pairing || pairing.state !== submitted.state) throw new Error('Pairing expired. Start again.');
    const existing = this.accounts.find((a) => a.id === pairing.accountId);
    return latency({ email: existing?.email ?? 'new.login@example.com', organization: existing ? `${existing.label} workspace` : 'Personal', state: submitted.state }, 500);
  }

  async confirmPairing(pairingId: string) {
    this.pairings.delete(pairingId);
    return latency(undefined, 250);
  }

  async createAccount(input: AccountInput): Promise<Account> {
    const status: Account['status'] = input.kind === 'oauth' ? 'unpaired' : 'ready';
    const account: Account = {
      ...ACCOUNT_SAMPLE,
      cooldowns: [],
      credential: { accessTokenExpiresAt: null, lastError: null, lastRefreshAt: null, quarantined: false, refreshTokenExpiresAt: null },
      email: null,
      extraUsage: null,
      fableWindow: null,
      fastLane: null,
      id: input.id,
      kind: input.kind,
      label: input.label,
      plan: null,
      pools: [],
      requests: { last24h: 0, last7d: 0, lifetime: 0 },
      routingEnabled: true,
      status,
      subscription: null,
      usage: usageOf({ id: input.id, kind: input.kind, status }),
      windows: [],
    };
    this.accounts = [...this.accounts, account];
    return latency(account, 300);
  }

  async createKey(input: CreateKeyInput): Promise<CreatedApiKey> {
    // The client sends the id it derives from the name, so a name already taken is an id already taken.
    const id = slug(input.name);
    if (this.keys.some((k) => k.id === id)) throw new ApiError(409, KEY_TAKEN, null);
    const refusal = keyRulesRefusal(input);
    if (refusal) throw new ApiError(400, refusal, null);
    const body = this.token(40);
    const secret = `phoebis-${body}`;
    const key: ApiKey = {
      ...KEY_SAMPLE,
      id,
      name: input.name,
      prefix: secret.slice(0, 12),
      poolId: input.poolId,
      // The route's defaults for a body that leaves these fields out, which a caller outside the console may.
      autoModeServer: input.autoModeServer ?? null,
      longContextTwins: input.longContextTwins ?? true,
      modelRules: input.modelRules ?? [],
      modelRulesMode: input.modelRulesMode ?? 'extend',
      status: 'active',
      createdAt: iso(NOW),
      expiresAt: input.expiresInDays ? iso(NOW + input.expiresInDays * DAY) : null,
      keepaliveOptOut: input.keepaliveOptOut,
      lastUsedAt: null,
      notes: input.notes,
      bodyCaptureHours: input.bodyCaptureHours,
      usage7d: { cost: 0, requests: 0, tokens: 0 },
      lifetime: { cost: 0, requests: 0 },
    };
    this.keys = [key, ...this.keys];
    return latency({ key, secret }, 300);
  }

  async createPool(input: PoolInput): Promise<Pool> {
    const id = slug(input.name);
    if (this.pools.some((p) => p.id === id)) throw new ApiError(409, POOL_TAKEN, null);
    const refusal = poolRefusal(input);
    if (refusal) throw new ApiError(400, refusal, null);
    if (input.bodyCaptureHours > this.settings.retention.bodyCaptureMaxHours) throw new ApiError(400, 'body_capture_hours exceeds stats.body_capture_max_hours', null);
    const members = memberRefusal(input.members);
    if (members) throw new ApiError(400, members, null);
    const pool: Omit<Pool, 'keys'> = {
      ...POOL_SAMPLE,
      ...input,
      activePins: 0,
      cost24h: 0,
      id,
      members: input.members.map((m) => {
        const account = this.accounts.find((a) => a.id === m.accountId);
        return writtenMember(m, account, (member) => eligibilityOf(account, input.overagePolicy, member));
      }),
      // The route starts a pool with no list when the body leaves it out, whatever the sample row holds.
      modelRules: input.modelRules ?? [],
      movesPerHour: {},
      requests24h: 0,
      traffic24h: [],
    };
    this.pools = [...this.pools, pool];
    return latency(this.poolView(pool), 300);
  }

  async deleteAccount(accountId: string) {
    this.account(accountId);
    this.accounts = this.accounts.filter((a) => a.id !== accountId);
    this.pools = this.pools.map((p) => ({ ...p, members: p.members.filter((m) => m.accountId !== accountId) }));
    return latency(undefined, 250);
  }

  async deleteKey(id: string) {
    this.keys = this.keys.filter((k) => k.id !== id);
    return latency(undefined, 200);
  }

  async deletePool(id: string) {
    // A key reaches only its own pool's members, so a pool cannot leave while keys still point at it.
    if (this.keys.some((k) => k.poolId === id)) return Promise.reject(new ApiError(409, 'pool still has keys', null));
    this.pools = this.pools.filter((p) => p.id !== id);
    return latency(undefined, 200);
  }

  async disconnectAccount(accountId: string) {
    const a = this.account(accountId);
    // The route drops the credential and leaves the identity, so the row still says whose it was; with no credential
    // the poll's standing goes too.
    this.putAccount({ ...a, credential: { accessTokenExpiresAt: null, lastError: null, lastRefreshAt: null, quarantined: false, refreshTokenExpiresAt: null }, status: 'unpaired', usage: usageOf({ ...a, status: 'unpaired' }) });
    return latency(undefined, 250);
  }

  /**
   * A test seam outside the client's interface: the next pool save stores its settings, then fails its member write
   * with `reason`, the route's partial save, which the mock's own store cannot reach otherwise.
   */
  failMemberWriteOnce(reason: string): void {
    this.memberWriteFailure = reason;
  }

  /** `refresh` re-stamps `fetchedAt` rather than answering a fresh fixture; every account answers the same
   * catalog except `personal`, whose live fetch has been failing. */
  getAccountCatalog(accountId: string, refresh?: boolean): Promise<AccountCatalog> {
    const a = this.account(accountId);
    const fetchedAt = ago(refresh ? 0 : 40 * 1000);
    if (a.id === 'personal') return latency({ error: 'the models request answered 503', fetchedAt, models: [] });
    return latency({ error: null, fetchedAt, models: CATALOG });
  }

  getAnalyticsBreakdown(query: AnalyticsQuery) {
    return latency(breakdownOf(emptyStore() ? [] : ALL_ROLLUPS, query));
  }

  getAnalyticsBurn(range: Pick<AnalyticsQuery, 'from' | 'to'>, groupBy: BurnGroup) {
    return latency(burnOf(emptyStore() ? [] : ALL_ROLLUPS, range, groupBy));
  }

  getAnalyticsRewrites(range: Pick<AnalyticsQuery, 'from' | 'to'>) {
    return latency(rewritesOf(emptyStore() ? [] : ACCOUNT_ROLLUPS, range));
  }

  getAnalyticsSeries(query: AnalyticsQuery) {
    return latency(seriesOf(emptyStore() ? [] : ALL_ROLLUPS, query));
  }

  /** The story's conditions, or none for a fresh installation. */
  getAttention() {
    return latency(emptyStore() ? [] : ATTENTION);
  }

  async getCapturedBodies(requestId: string) {
    const r = requestById.get(requestId);
    return latency(r ? capturedBodiesFor(r) : null);
  }

  getKeepaliveOverview() {
    return latency(emptyStore() ? EMPTY_KEEPALIVE_OVERVIEW : KEEPALIVE_OVERVIEW);
  }

  /** Every live model-scoped cooldown across the accounts, folded to the shape the two model routes answer. */
  private modelCooldowns(): RuleCooldown[] {
    return this.accounts.flatMap((a) =>
      a.cooldowns
        .filter((c) => c.scope.startsWith(MODEL_SCOPE_PREFIX) && Date.parse(c.until) > Date.now())
        .map((c) => ({ accountId: a.id, reason: c.reason, scope: c.scope, setAt: c.setAt, until: c.until, window: c.reason === 'model_rejected' ? 'model_rejection' : c.reason })),
    );
  }

  async getKeyModels(keyId: string): Promise<KeyModels> {
    // A store without a key processes no rule for any key, so the answer carries none and no pool.
    if (emptyStore()) return latency({ keyId, longContextTwins: false, modelRulesMode: 'extend', poolId: '', rules: [] });
    const key = this.keys.find((k) => k.id === keyId);
    if (!key) throw new ApiError(404, 'key not found', null);
    const pool = this.pools.find((p) => p.id === key.poolId);
    const poolRules = pool?.modelRules ?? [];
    const memberIds = new Set((pool?.members ?? []).map((m) => m.accountId));
    const cooldowns = this.modelCooldowns().filter((c) => memberIds.has(c.accountId));
    const rules = processRules(key.modelRules, key.modelRulesMode, poolRules).map((r) => ({
      // A denied model is never routed, so its cooldowns say nothing about the key.
      cooldowns: r.not ? [] : cooldowns.filter((c) => matchesModelPattern(r.pattern, c.scope.slice(MODEL_SCOPE_PREFIX.length))),
      level: r.level,
      not: r.not,
      pattern: r.pattern,
    }));
    return latency({ keyId: key.id, longContextTwins: key.longContextTwins, modelRulesMode: key.modelRulesMode, poolId: key.poolId, rules });
  }

  getModelsState(): Promise<ModelsState> {
    if (emptyStore()) return latency({ cooldowns: [], permits: [], rejections: [] });
    const byModel = new Map<string, RuleCooldown[]>();
    for (const row of this.modelCooldowns()) {
      const model = row.scope.slice(MODEL_SCOPE_PREFIX.length);
      byModel.set(model, [...(byModel.get(model) ?? []), row]);
    }
    const cooldowns = [...byModel.entries()].sort(([a], [b]) => a.localeCompare(b)).map(([model, rows]) => ({ model, rows }));
    return latency({ cooldowns, permits: MODEL_PERMITS, rejections: MODEL_REJECTIONS });
  }

  getOverview() {
    const empty = emptyStore();
    // The route answers its 24 hourly buckets over an empty ledger too, at zero, so each spark draws a flat line.
    const last24h = empty ? [] : REQUESTS.filter((r) => Date.parse(r.at) >= NOW - DAY);
    const hourlyMap = new Map<string, { at: string; cost: number; errors: number; requests: number; tokens: number }>();
    for (let i = 23; i >= 0; i -= 1) {
      const at = iso(Math.floor((NOW - i * 3_600_000) / 3_600_000) * 3_600_000);
      hourlyMap.set(at, { at, cost: 0, errors: 0, requests: 0, tokens: 0 });
    }
    let tokens = 0;
    let cached = 0;
    let cost = 0;
    const errorCodes = new Map<string, number>();
    for (const r of last24h) {
      const at = iso(Math.floor(Date.parse(r.at) / 3_600_000) * 3_600_000);
      const bucket = hourlyMap.get(at);
      const total = r.usage.input + r.usage.cacheRead + r.usage.cacheWrite5m + r.usage.cacheWrite1h + r.usage.output;
      tokens += total;
      cached += r.usage.cacheRead;
      cost += r.costUsd;
      if (r.errorCode) errorCodes.set(r.errorCode, (errorCodes.get(r.errorCode) ?? 0) + 1);
      if (bucket) {
        bucket.requests += 1;
        bucket.tokens += total;
        bucket.cost += r.costUsd;
        if (r.status !== 200) bucket.errors += 1;
      }
    }
    const top = [...errorCodes.entries()].sort((a, b) => b[1] - a[1])[0];
    return latency({
      // Counted off the pools, so the dashboard and the Pools view cannot disagree on the router's pins.
      activePins: empty ? 0 : this.pools.reduce((n, p) => n + p.activePins, 0),
      cost24h: cost,
      errorRate24h: last24h.length ? last24h.filter((r) => r.status !== 200).length / last24h.length : null,
      hourly: [...hourlyMap.values()],
      requests24h: last24h.length,
      tokens24h: tokens,
      tokensCached24h: cached,
      topErrorCode: top ? top[0] : null,
    });
  }

  getQuotaHistory(accountId: string) {
    return latency(emptyStore() ? [] : QUOTA_HISTORY.filter((h) => h.accountId === accountId));
  }

  getRequest(id: string): Promise<RequestDetail | null> {
    const request = emptyStore() ? undefined : requestById.get(id);
    return latency(request ? { events: REQUEST_EVENTS.get(id) ?? [], request } : null);
  }

  getSession(): Promise<SessionInfo | null> {
    if (this.session) return latency(this.session, 60);
    return latency(seededSession() ? MOCK_SESSION : null, 60);
  }

  getSettings() {
    return latency(this.settings);
  }

  listAccountRollups(from: string, to: string) {
    if (emptyStore()) return latency([]);
    const fromMs = Date.parse(from);
    const toMs = Date.parse(to);
    return latency(ACCOUNT_ROLLUPS.filter((r) => Date.parse(r.hour) >= fromMs && Date.parse(r.hour) <= toMs));
  }

  listAccounts() {
    return latency(emptyStore() ? [] : this.accounts);
  }

  listCatalog() {
    return latency(emptyStore() ? [] : CATALOG);
  }

  async listCatalogOf(accountIds: string[]): Promise<CatalogModel[]> {
    if (emptyStore()) return latency([]);
    const fetched = await Promise.all(
      accountIds.map(async (id) => {
        try {
          return await this.getAccountCatalog(id);
        } catch {
          return null;
        }
      }),
    );
    return catalogUnion(fetched);
  }

  listKeepaliveRecipes() {
    return latency(emptyStore() ? [] : RECIPES);
  }

  listKeys() {
    return latency(emptyStore() ? [] : this.keys);
  }

  listPools() {
    return latency(emptyStore() ? [] : this.pools.map((p) => this.poolView(p)));
  }

  listRequests(query: RequestQuery = {}): Promise<RequestPage> {
    if (emptyStore()) return latency({ nextCursor: null, requests: [] });
    const live = new Map(this.keys.map((k) => [k.id, Date.parse(k.createdAt)]));
    const matched = REQUESTS.filter((r) => matchesQuery(r, query, live)).sort((a, b) => Date.parse(b.at) - Date.parse(a.at) || (a.id < b.id ? 1 : a.id > b.id ? -1 : 0));
    const after = query.cursor ? matched.filter((r) => isAfterCursor(r, query.cursor!)) : matched;
    const limit = query.limit ?? 50;
    const page = after.slice(0, limit);
    const last = page[page.length - 1];
    // A cursor only rides along while rows are left, which is how the route signals the end of the list.
    return latency({ nextCursor: after.length > limit && last ? `${Date.parse(last.at)}:${last.id}` : null, requests: page });
  }

  listSessions() {
    return latency(emptyStore() ? [] : this.sessions);
  }

  login(secret: string): Promise<SessionInfo> {
    if (secret !== 'admin') return Promise.reject(new ApiError(401, 'that secret does not match', null));
    this.session = MOCK_SESSION;
    seedSession(true);
    return latency(this.session, 200);
  }

  logout() {
    this.session = null;
    seedSession(false);
    return latency(undefined, 150);
  }

  async pauseAccount(accountId: string) {
    const a = this.account(accountId);
    this.putAccount({ ...a, routingEnabled: false, status: statusOf(a, false) });
    return latency(undefined, 250);
  }

  refreshUsage(_accountId: string) {
    return latency(undefined, 400);
  }

  async releaseSession(keyId: string, sessionId: string) {
    this.sessions = this.sessions.filter((s) => !(s.keyId === keyId && s.sessionId === sessionId && !s.historical));
    return latency(undefined, 250);
  }

  async renameAccount(accountId: string, label: string) {
    return latency(this.putAccount({ ...this.account(accountId), label }), 250);
  }

  async resumeAccount(accountId: string) {
    const a = this.account(accountId);
    this.putAccount({ ...a, routingEnabled: true, status: statusOf(a, true) });
    return latency(undefined, 250);
  }

  async revokeKey(id: string) {
    // The route switches a revoked key off in the same write, and an edit sends that flag back.
    this.keys = this.keys.map((k) => (k.id === id ? { ...k, enabled: false, status: 'revoked' as const } : k));
    return latency(undefined, 250);
  }

  async rotateKey(id: string): Promise<CreatedApiKey> {
    const key = this.keys.find((k) => k.id === id);
    if (!key) return Promise.reject(new ApiError(404, 'key not found', null));
    const secret = `phoebis-${this.token(40)}`;
    const next: ApiKey = { ...key, prefix: secret.slice(0, 12), retiredSecretLastUsedAt: null, sessionsOnRetiredSecret: 2 };
    this.keys = this.keys.map((k) => (k.id === id ? next : k));
    return latency({ key: next, secret }, 300);
  }

  async startPairing(accountId: string): Promise<PairingSession> {
    const state = this.token(24);
    // The router names the pairing with an id of its own; the state only ever travels through the browser.
    const id = `pair_${this.token(10)}`;
    this.pairings.set(id, { accountId, state });
    const params = new URLSearchParams({
      client_id: '9d1c250a-e61b-44d9-88ed-5944d1962f5e',
      code_challenge: this.token(43),
      code_challenge_method: 'S256',
      redirect_uri: PAIRING_REDIRECT_URI,
      response_type: 'code',
      scope: 'org:create_api_key user:profile user:inference',
      state,
    });
    return latency({ ...PAIRING_SAMPLE, authorizeUrl: `https://claude.ai/oauth/authorize?${params.toString()}`, expiresAt: ahead(10 * MINUTE), id, redirectUri: PAIRING_REDIRECT_URI, state }, 200);
  }

  subscribeLive(handler: (event: LiveEvent) => void) {
    // An empty store runs no request, so its feed is the snapshot without rows and the idle ticks after it.
    const empty = emptyStore();
    // Through the decoders the socket's frames take, so every page reads the feed the way the served console does.
    return liveFeed().subscribe((frame) => {
      if (frame.kind === 'snapshot') {
        const snapshot = empty ? { ...frame.snapshot, in_flight: [], recent: [], tick: idleTick(frame.snapshot.tick) } : frame.snapshot;
        return handler({ kind: 'snapshot', snapshot: decodeLiveSnapshot(snapshot) });
      }
      if (empty && frame.delta.type !== 'tick') return;
      const delta = decodeLiveDelta(empty ? { ...frame.delta, tick: idleTick(frame.delta.tick) } : frame.delta);
      if (delta) handler({ delta, kind: 'delta' });
    });
  }

  /** The account route's refusals in its order and words; the end is kept only while the flag stands. */
  async updateAccount(accountId: string, patch: AccountPatch): Promise<Account> {
    const a = this.account(accountId);
    if (patch.cancelled && a.kind === 'api_key') throw new ApiError(400, 'only an oauth account is cancelled', null);
    if (!patch.cancelled && patch.subscriptionEndsAt !== null) throw new ApiError(400, 'subscription_ends_at needs cancelled', null);
    if (patch.cancelled && patch.subscriptionEndsAt === null) throw new ApiError(400, 'cancelled needs subscription_ends_at', null);
    // The route answers a subscription block for a cancelled OAuth row even before it pairs, so its end has a place.
    const blank: Subscription = { billingType: '', derivedEndsAt: null, endsAt: null, organizationType: '', plan: null, rateLimitTier: '', since: null, trialEndsAt: null };
    const base = a.subscription ?? (patch.cancelled ? blank : null);
    const subscription = base ? { ...base, endsAt: patch.cancelled ? patch.subscriptionEndsAt : null } : null;
    return latency(this.putAccount({ ...a, cancelled: patch.cancelled, subscription }), 250);
  }

  async updateKey(id: string, input: KeyInput): Promise<ApiKey> {
    const idx = this.keys.findIndex((k) => k.id === id);
    if (idx < 0) return Promise.reject(new Error(`Unknown key ${id}`));
    const refusal = keyRulesRefusal(input);
    if (refusal) return Promise.reject(new ApiError(400, refusal, null));
    const { expiresAt: expiry, ...rest } = input;
    const current = this.keys[idx];
    const expiresAt = expiry === undefined ? current.expiresAt : expiry;
    // The status is the decoder's fold of the stored flags, so a key switched off or lapsed reads that way at once.
    const status = current.status === 'revoked' ? 'revoked' : expiresAt !== null && Date.parse(expiresAt) <= Date.now() ? 'expired' : rest.enabled ? 'active' : 'disabled';
    const next: ApiKey = { ...current, ...rest, expiresAt, status };
    this.keys = this.keys.map((k, i) => (i === idx ? next : k));
    return latency(next, 250);
  }

  async updatePool(id: string, input: PoolInput): Promise<Pool> {
    const idx = this.pools.findIndex((p) => p.id === id);
    if (idx < 0) return Promise.reject(new Error(`Unknown pool ${id}`));
    const refusal = poolRefusal(input);
    if (refusal) return Promise.reject(new ApiError(400, refusal, null));
    if (input.bodyCaptureHours > this.settings.retention.bodyCaptureMaxHours) {
      return Promise.reject(new ApiError(400, 'body_capture_hours exceeds stats.body_capture_max_hours', null));
    }
    // The route checks every member against the registry before it stores anything, and before the member write's own
    // refusals.
    const unknown = input.members.find((m) => !this.accounts.some((a) => a.id === m.accountId));
    if (unknown) return Promise.reject(new ApiError(404, `account ${unknown.accountId} not found`, null));
    const members = memberRefusal(input.members);
    if (members) return Promise.reject(new ApiError(400, members, null));
    const current = this.pools[idx];
    const reason = this.memberWriteFailure;
    if (reason !== null) {
      this.memberWriteFailure = null;
      this.pools = this.pools.map((p, i) => (i === idx ? { ...current, ...input, members: current.members } : p));
      return Promise.reject(new ApiError(409, `the pool's settings were saved; its members were not: ${reason}`, null));
    }
    const verdict = new Map(current.members.map((m) => [m.accountId, m.eligible]));
    const next: Omit<Pool, 'keys'> = {
      ...current,
      ...input,
      // Eligibility is the router's reading of live cooldowns and overages, not something a save can set. A save
      // does rewrite the budget, and a write answers no spend here, so a budget verdict is read again.
      members: input.members.map((m) => {
        const account = this.accounts.find((a) => a.id === m.accountId);
        const kept = verdict.get(m.accountId) ?? 'eligible';
        return writtenMember(m, account, (member) => (kept === 'budget' ? eligibilityOf(account, input.overagePolicy, member) : kept));
      }),
    };
    this.pools = this.pools.map((p, i) => (i === idx ? next : p));
    return latency(this.poolView(next), 250);
  }

  async verifyAccount(accountId: string) {
    this.account(accountId);
    return latency({ ok: true, status: 200 }, 400);
  }
}
