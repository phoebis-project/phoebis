import type { AccountPatch, Api, CreateKeyInput, KeyInput, PoolInput } from '@/api/client';
import { AFFINITY_MODE_VALUES, RETURN_TO_PLAN_VALUES, SESSION_PLACEMENT_VALUES, SUBAGENT_PLACEMENT_VALUES, WARM_SESSIONS_VALUES, decodeSettings } from '@/api/decode';
import { ApiError, catalogUnion, slug } from '@/api/http';
import type {
  Account, AccountCatalog, AccountInput, AnalyticsQuery, ApiKey, BurnGroup, CatalogModel, CreatedApiKey, DecisionRecord, KeepaliveOverview, KeyModels, MemberEligibility,
  ModelRule, ModelsState, Overview, PairingInput, PairingPreview, PairingSession, PairingStart, Pool, PoolMember, RequestDetail, RequestLog, RequestPage, RequestQuery, RoutingLogPage, RoutingLogQuery, RuleCooldown, Session, SessionInfo, Settings,
  Subscription,
} from '@/api/types';
import { foldEffort, foldRequestClass } from '@/lib/dimensions';
import { modelRulesRefusal, poolWarnings, type NumericKeepaliveKey } from '@/lib/forms';
import { matchesPattern } from '@/lib/pattern';
import { matchesPattern as matchesModelPattern, processRules } from '@/lib/patterns';
import { isFastScope } from '@/lib/quota';
import { ACCOUNTS, QUOTA_HISTORY, usageOf } from './accounts';
import { RECONCILIATIONS, breakdownOf, burnOf, placementsOf, reconciliationsOf, rewritesOf, seriesOf } from './analytics';
import { ATTENTION } from './attention';
import { CATALOG } from './catalog';
import { KEEPALIVE_OVERVIEW, RECIPES, SESSIONS, SETTINGS_WIRE, memberPins } from './operations';
import { KEYS, POOLS, composedFor, resolvedFor, servedFigures } from './pools';
import { ACCOUNT_ROLLUPS, ALL_ROLLUPS, DAY_ROWS, REQUEST_EVENTS, REQUESTS, capturedBodiesFor, requestById, requestSidesOf } from './requests';
import { createRng } from './rng';
import { ROUTING_LOG_KINDS, getDecision, listRoutingLog } from './routingLog';
import { ACCOUNT_SAMPLE, KEY_SAMPLE, PAIRING_SAMPLE, POOL_SAMPLE } from './samples';
import { DAY, HOUR, MINUTE, NOW, ago, ahead, iso } from './time';

// Fixed, so a pass over the mock reads the same secrets every run.
const MOCK_SEED = 4_211;

// A short delay keeps loading states honest without making the mock feel slow.
const latency = <T,>(value: T, ms = 120): Promise<T> => new Promise((resolve) => setTimeout(() => resolve(value), ms));

// The routes' own wording, so a page draws the text the router would have sent.
const ACCOUNT_TAKEN = 'an account with this id exists';
const KEY_TAKEN = 'a key with this id exists';
const POOL_TAKEN = 'a pool with this id exists';

/** Strictly later than the stamp it replaces, as the route's fence stamps, so a save inside one millisecond still
 * moves it and an edit read before it cannot pass. */
const stamped = (previous: string) => iso(Math.max(Date.now(), Date.parse(previous) + 1));

/** One past the row's, as every store write of the account row moves the counter, so an edit read before it cannot
 * pass. */
const revised = (a: Account): Account => ({ ...a, revision: a.revision + 1 });

/** The route's edit token carries the account's incarnation above a 32-bit revision, so an id deleted and created
 * again starts past every token its predecessor answered. */
const INCARNATION = 2 ** 32;

/** The pool and key routes' edit token carries the row's incarnation above the 42 bits of its `updatedAt`
 * millisecond, so an id deleted and created again inside one millisecond still answers a token of its own. */
const INCARNATION_STAMP = 2 ** 42;
const stampToken = (incarnation: number, updatedAt: string) => incarnation * INCARNATION_STAMP + Date.parse(updatedAt);

/** A pool or key write's stamp and the token that carries it under the row's own incarnation. */
const restamp = (row: { revision: number; updatedAt: string }) => {
  const updatedAt = stamped(row.updatedAt);
  return { revision: stampToken(Math.floor(row.revision / INCARNATION_STAMP), updatedAt), updatedAt };
};

/**
 * The policy check's bounds in its own order and wording, written out apart from the form's table so a form that
 * drifts from the route is refused here rather than agreed with.
 */
const KEEPALIVE_RANGES: [NumericKeepaliveKey, string, number, number][] = [
  ['maxIdleSeconds', 'max_idle_seconds', 0, 31_536_000],
  ['maxParkedSeconds', 'max_parked_seconds', 60, 604_800],
  ['scheduledHorizonSeconds', 'scheduled_horizon_seconds', 60, 604_800],
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

/** The ceiling, shedding, failover, slow-down and return-to-plan refusals, in the order and words of the pool routes. */
const engineRefusal = (input: PoolInput): string | null => {
  const whole = (v: number, min: number, max: number) => Number.isInteger(v) && v >= min && v <= max;
  if (!(input.windowCeiling >= 0.5 && input.windowCeiling <= 1)) return 'window_ceiling is between 0.5 and 1.0';
  if (!(input.shedCap >= 0.1 && input.shedCap <= 1)) return 'shed_cap is between 0.1 and 1.0';
  if (!whole(input.burnAverageSeconds, 600, 7_200)) return 'burn_average_seconds is between 600 and 7200';
  if (!whole(input.shedDwellSeconds, 300, 7_200)) return 'shed_dwell_seconds is between 300 and 7200';
  if (!WARM_SESSIONS_VALUES.some((mode) => mode === input.warmSessions)) return 'warm_sessions must be always, never, or when_needed';
  if (!whole(input.retryInPlaceSeconds, 0, 300)) return 'retry_in_place_seconds is 0 to 300';
  if (!whole(input.inFlightCap, 1, 4_294_967_295)) return 'in_flight_cap is at least 1';
  if (!whole(input.overageMoveGraceMinutes, 0, 300)) return 'overage_move_grace_minutes is 0 to 300';
  if (!whole(input.windDownMinutes, 0, 10_080)) return 'wind_down_minutes is at most 10080 (one week)';
  if (!whole(input.slowDownStretchSeconds, 60, 18_000)) return 'slow_down_stretch_seconds is between 60 and 18000';
  if (!whole(input.slowDownHoldSeconds, 1, 300)) return 'slow_down_hold_seconds is between 1 and 300';
  if (!RETURN_TO_PLAN_VALUES.some((mode) => mode === input.returnToPlan)) return 'return_to_plan must be always, when_cold, or never';
  if (!whole(input.returnToPlanDwellSeconds, 0, 86_400)) return 'return_to_plan_dwell_seconds is between 0 and 86400';
  return null;
};

/** The pin and placement spellings, in the order and words of the pool routes; an absent field is kept. */
const affinityRefusal = (input: Partial<PoolInput>): string | null => {
  if (input.affinityMode !== undefined && !AFFINITY_MODE_VALUES.some((mode) => mode === input.affinityMode)) return 'affinity_mode must be always or when_cached';
  if (input.subagentPlacement !== undefined && !SUBAGENT_PLACEMENT_VALUES.some((p) => p === input.subagentPlacement)) return 'subagent_placement must be by_cohort, by_strategy, or with_main';
  if (input.sessionPlacement !== undefined && !SESSION_PLACEMENT_VALUES.some((p) => p === input.sessionPlacement)) return 'session_placement must be by_strategy or by_warm_prefix';
  return null;
};

/** The account a member list names twice, which the routes refuse with 409 before any other member check. */
const repeatedMember = (members: PoolInput['members']): string | null => {
  const seen = new Set<string>();
  for (const m of members) {
    if (seen.has(m.accountId)) return m.accountId;
    seen.add(m.accountId);
  }
  return null;
};

/** The route's 409 for a member another pool holds, since an account belongs to one pool. */
const heldElsewhere = (members: PoolInput['members'], pools: Pick<Pool, 'id' | 'members'>[], poolId: string): string | null => {
  for (const m of members) {
    const holder = pools.find((p) => p.id !== poolId && p.members.some((x) => x.accountId === m.accountId));
    if (holder) return `account ${m.accountId} is a member of pool ${holder.id}`;
  }
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
    // A null ceiling is the recipe's break-even, which no bound applies to.
    if (value !== null && (value < min || value > max)) return `keepalive.${field} must be between ${min} and ${max}, not ${value}`;
  }
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

/**
 * Set to stand in for a ledger writer holding a batch past the replay route's bound: a replay answers the route's `503`
 * and lands nothing.
 */
export const HELD_WRITER_KEY = 'phoebis.mockHeldWriter';
const heldWriter = (): boolean => {
  try {
    return localStorage.getItem(HELD_WRITER_KEY) === '1';
  } catch {
    return false;
  }
};

/** The keepalive overview of a router with no session and no probe yet. */
const EMPTY_KEEPALIVE_OVERVIEW: KeepaliveOverview = {
  agents: { busy: 0, idle: 0, parked: 0 },
  busGaps: 0,
  expiredBetweenTurns24h: 0,
  limits: { agents: { count: 0, max: 1_024 }, retainedBytes: { count: 0, max: 256 * 1_048_576 }, sessions: { count: 0, max: 128 } },
  pools: [],
  probes24h: { failed: 0, held: 0, migrated: 0, ok: 0, skipped: 0 },
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
 * A written member as the route answers it: the kind, the burn, the Pace score and the speed read off the account, and
 * no spend, since the mock keeps no ledger to price one from. A member sent without a budget field has no budget over
 * that period. A save moves no pin, so a member the pool held keeps its pins and one it gains holds none yet.
 */
const writtenMember = (m: PoolInput['members'][number], account: Account | undefined, slowDown: boolean, eligible: (member: Omit<PoolMember, 'eligible'>) => MemberEligibility, activePins = 0): PoolMember => {
  const member = { accountId: m.accountId, activePins, budgetDailyUsd: m.budgetDailyUsd ?? null, budgetMonthlyUsd: m.budgetMonthlyUsd ?? null, budgetUntil: null, kind: account?.kind ?? 'oauth', priority: m.priority, spends: [], ...servedFigures(m.accountId, Date.now(), slowDown) };
  return { ...member, eligible: eligible(member) };
};

/** A pool's pins are its members', summed whenever read, so the figure and the members cannot disagree. */
const poolPins = (p: Pick<Pool, 'members'>): number => p.members.reduce((n, m) => n + m.activePins, 0);

/**
 * The route's `keys=` rule: `<id>@<generation>` names one lifetime and a bare id the live key's, so an earlier
 * holder's rows answer only to their own selector.
 */
const selectsKey = (selector: string, r: RequestLog, live: Map<string, number>) =>
  r.keyId !== null && (selector.includes('@') ? `${r.keyId}@${r.keyGeneration}` === selector : r.keyId === selector && live.get(selector) === r.keyGeneration);

function matchesQuery(r: RequestLog, q: RequestQuery, live: Map<string, number>): boolean {
  // A request the router never attributed to a key or a pool matches no filter on that field.
  if (q.keyIds?.length && !q.keyIds.some((k) => selectsKey(k, r, live))) return false;
  if (q.poolIds?.length && (r.poolId === null || !q.poolIds.includes(r.poolId))) return false;
  // The route files a request no attempt served under the unattributed id, as the rollups do.
  if (q.accountIds?.length && !q.accountIds.includes(r.accountId ?? '-')) return false;
  // The route compares the lowercased selection against the canonical id, never the client's spelling.
  if (q.models?.length && !q.models.some((m) => m.toLowerCase() === r.canonicalModel)) return false;
  if (q.statuses?.length && (r.status === null || !q.statuses.includes(r.status))) return false;
  if (q.outcomes?.length && !q.outcomes.includes(r.outcome)) return false;
  if (q.kind && r.kind !== q.kind) return false;
  if (q.from && Date.parse(r.at) < Date.parse(q.from)) return false;
  if (q.to && Date.parse(r.at) > Date.parse(q.to)) return false;
  if (q.request && r.id !== q.request) return false;
  if (q.session && r.sessionId !== q.session) return false;
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
  // The serving attempt is the last; a request no answer named a speed for reads as fast when it asked for fast and
  // as standard otherwise, as the route reads it.
  if (q.speeds?.length && !q.speeds.includes(r.attempts[r.attempts.length - 1]?.response.speed ?? (r.shape.requestedSpeed === 'fast' ? 'fast' : 'standard'))) return false;
  if (q.held && r.held === null) return false;
  // The serving attempt is the last, the one the rollups count a request's reason on. A request with no attempt or a
  // null serving reason matches nothing.
  if (q.cacheMissReasons?.length) {
    const reason = r.attempts[r.attempts.length - 1]?.cacheMissReason ?? null;
    if (reason === null || !q.cacheMissReasons.includes(reason)) return false;
  }
  if (q.efforts?.length && !q.efforts.includes(foldEffort(r.shape.effort))) return false;
  if (q.requestClasses?.length && !q.requestClasses.includes(foldRequestClass(r.requestClass))) return false;
  // Probes are off the log unless they are what the query asks for, as the route lists them.
  if (r.kind === 'probe' && !q.includeProbes && q.kind !== 'probe') return false;
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
  /** The incarnation each deleted id reached, which its next account starts above. */
  private buried = new Map<string, number>();
  /** The incarnation each deleted key id reached, which its next key starts above. */
  private buriedKeys = new Map<string, number>([['desktop', 1]]);
  /** The incarnation each deleted pool id reached, which its next pool starts above. */
  private buriedPools = new Map<string, number>();
  // The live desktop key was issued under the id its deleted earlier holder had, so it is that id's second; every other
  // key of the story is its id's first.
  private keys: ApiKey[] = KEYS.map((k) => ({ ...k, revision: stampToken((this.buriedKeys.get(k.id) ?? 0) + 1, k.updatedAt) }));
  /** `label` is set on a pairing that creates its row at the confirm. The row's id is `accountId` either way. */
  private pairings = new Map<string, { accountId: string; label?: string; state: string }>();
  /** Set by the first replay, which lands the story's parked records. */
  private parkedLanded = false;
  // The route reads every member's kind and verdict afresh on each answer; the story's are read once, at load.
  private pools: Omit<Pool, 'activePins' | 'keys' | 'warnings'>[] = POOLS.map((p) => ({
    ...p,
    // Every pool of the story is its id's first.
    revision: stampToken(1, p.updatedAt),
    members: p.members.map((m) => {
      const account = this.accounts.find((a) => a.id === m.accountId);
      return { ...m, activePins: memberPins(p.id, m.accountId), budgetUntil: budgetUntilOf(m, Date.now()), eligible: eligibilityOf(account, p.overagePolicy, m), kind: account?.kind ?? 'oauth', ...servedFigures(m.accountId, Date.now(), p.slowDown) };
    }),
  }));
  private secretRng = createRng(MOCK_SEED);
  private session: SessionInfo | null = null;
  private sessions: Session[] = [...SESSIONS];
  private settings: Settings = decodeSettings(SETTINGS_WIRE);

  private token(n: number) {
    return Array.from({ length: n }, () => 'abcdefghijklmnopqrstuvwxyz0123456789'[this.secretRng.int(0, 35)]).join('');
  }

  /**
   * The pool as the route answers it, with its keys listed from the key registry at the time of the read, and its
   * warnings the route's checks on the saved settings and the largest cohort among the pinned sessions under its keys.
   */
  private poolView(p: Omit<Pool, 'activePins' | 'keys' | 'warnings'>): Pool {
    const keys = this.keys.filter((k) => k.poolId === p.id).map((k) => ({ enabled: k.enabled, id: k.id, label: k.name, prefix: k.prefix }));
    const ids = new Set(keys.map((k) => k.id));
    const cohort = Math.max(0, ...this.sessions.filter((s) => s.pinned && ids.has(s.keyId)).map((s) => s.agents));
    return { ...p, activePins: poolPins(p), keys, warnings: poolWarnings(p, cohort) };
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

  /** The row the create route builds, and the row the confirm of a pairing started for a new account writes. */
  private newAccountRow(input: AccountInput, status: Account['status'], email: string | null): Account {
    return {
      ...ACCOUNT_SAMPLE,
      cooldowns: [],
      credential: { accessTokenExpiresAt: null, lastError: null, lastRefreshAt: null, quarantined: false, refreshTokenExpiresAt: null },
      email,
      extraUsage: null,
      fableWindow: null,
      fastLane: null,
      id: input.id,
      kind: input.kind,
      label: input.label,
      plan: null,
      pool: null,
      requests: { last24h: 0, last7d: 0, lifetime: 0 },
      revision: ((this.buried.get(input.id) ?? 0) + 1) * INCARNATION + 1,
      routingEnabled: true,
      status,
      subscription: null,
      usage: usageOf({ id: input.id, kind: input.kind, status }),
      windows: [],
    };
  }

  /** The account as the route answers it, its pool read off the member lists a pool write rewrites. */
  private withPool(a: Account): Account {
    const pool = this.pools.find((p) => p.members.some((m) => m.accountId === a.id));
    const member = pool?.members.find((m) => m.accountId === a.id);
    return { ...a, pool: pool && member ? { poolId: pool.id, priority: member.priority } : null };
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
    this.putAccount(revised({ ...a, extraUsage: a.extraUsage ? { ...a.extraUsage, disabledReason: null } : null, status: a.status === 'funding_required' ? 'ready' : a.status }));
    return latency(undefined, 200);
  }

  async completePairing(pairingId: string, input: PairingInput): Promise<PairingPreview> {
    const submitted = 'url' in input ? fromCallbackUrl(input.url) : input;
    if (!submitted) throw new Error('That URL carries no authorization code and state. Paste the whole address the browser landed on, or the code on its own.');
    // The router refuses a callback whose state does not belong to the pairing the id names.
    const pairing = this.pairings.get(pairingId);
    if (!pairing || pairing.state !== submitted.state) throw new Error('Pairing expired. Start again.');
    // Every subscription here is the operator's own, so its organization is the personal one the profile names, and a
    // login no row has paired yet is the operator's next alias.
    const existing = this.accounts.find((a) => a.id === pairing.accountId);
    return latency({ email: existing?.email ?? 'operator+new@example.com', organization: 'Personal', state: submitted.state }, 500);
  }

  async confirmPairing(pairingId: string) {
    const pairing = this.pairings.get(pairingId);
    // Deleted either way: the router fails the session on a refused confirm.
    this.pairings.delete(pairingId);
    if (pairing?.label !== undefined) {
      // The backstop for a create that landed between the start and the confirm.
      if (this.accounts.some((a) => a.id === pairing.accountId)) throw new ApiError(409, ACCOUNT_TAKEN, null);
      const row = this.newAccountRow({ id: pairing.accountId, kind: 'oauth', label: pairing.label, secret: null }, 'ready', 'operator+new@example.com');
      // The confirm stores the tokens it exchanged, so the row carries both their expiries, as the router answers it.
      this.accounts = [...this.accounts, { ...row, credential: { ...row.credential, accessTokenExpiresAt: ahead(8 * HOUR), refreshTokenExpiresAt: ahead(30 * DAY) } }];
    }
    return latency(undefined, 250);
  }

  async createAccount(input: AccountInput): Promise<Account> {
    const account = this.newAccountRow(input, input.kind === 'oauth' ? 'unpaired' : 'ready', null);
    this.accounts = [...this.accounts, account];
    return latency(this.withPool(account), 300);
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
      prefix: secret.slice(0, 16),
      poolId: input.poolId,
      // The route's defaults for a body that leaves these fields out, which a caller outside the console may.
      autoModeServer: input.autoModeServer ?? null,
      longContextTwins: input.longContextTwins ?? true,
      modelRules: input.modelRules ?? [],
      modelRulesMode: input.modelRulesMode ?? 'extend',
      status: 'active',
      createdAt: iso(NOW),
      updatedAt: iso(NOW),
      revision: stampToken((this.buriedKeys.get(id) ?? 0) + 1, iso(NOW)),
      expiresAt: input.expiresInDays ? iso(NOW + input.expiresInDays * DAY) : null,
      keepaliveOptOut: input.keepaliveOptOut,
      lastUsedAt: null,
      notes: input.notes,
      bodyCaptureHours: input.bodyCaptureHours,
      usage7d: { billedUsd: 0, cost: 0, equivalent: { fiveHour: 0, sevenDay: 0 }, estimated: false, requests: 0, tokens: 0 },
      lifetime: { billedUsd: 0, cost: 0, equivalent: { fiveHour: 0, sevenDay: 0 }, estimated: false, requests: 0 },
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
    const repeated = repeatedMember(input.members);
    if (repeated) throw new ApiError(409, `members name account ${repeated} twice`, null);
    const members = memberRefusal(input.members);
    if (members) throw new ApiError(400, members, null);
    const held = heldElsewhere(input.members, this.pools, id);
    if (held) throw new ApiError(409, held, null);
    const pool: Omit<Pool, 'activePins' | 'keys' | 'warnings'> = {
      ...POOL_SAMPLE,
      ...input,
      cost24h: 0,
      id,
      // From the members' kinds, as the router composes them, and nothing for a pool with no member.
      keepaliveResolved: resolvedFor(composedFor(input.members.map((m) => this.accounts.find((a) => a.id === m.accountId)?.kind ?? 'oauth'), input.cacheTtl), input.keepalive),
      members: input.members.map((m) => {
        const account = this.accounts.find((a) => a.id === m.accountId);
        return writtenMember(m, account, input.slowDown, (member) => eligibilityOf(account, input.overagePolicy, member));
      }),
      // The route starts a pool with no list when the body leaves it out, whatever the sample row holds.
      modelRules: input.modelRules ?? [],
      movesPerHour: {},
      requests24h: 0,
      revision: stampToken((this.buriedPools.get(id) ?? 0) + 1, iso(NOW)),
      traffic24h: [],
      updatedAt: iso(NOW),
    };
    this.pools = [...this.pools, pool];
    return latency(this.poolView(pool), 300);
  }

  async deleteAccount(accountId: string) {
    this.buried.set(accountId, Math.floor(this.account(accountId).revision / INCARNATION));
    this.accounts = this.accounts.filter((a) => a.id !== accountId);
    this.pools = this.pools.map((p) => {
      const members = p.members.filter((m) => m.accountId !== accountId);
      return { ...p, keepaliveResolved: members.length === 0 ? null : p.keepaliveResolved, members };
    });
    return latency(undefined, 250);
  }

  async deleteKey(id: string) {
    const key = this.keys.find((k) => k.id === id);
    if (key) this.buriedKeys.set(id, Math.floor(key.revision / INCARNATION_STAMP));
    this.keys = this.keys.filter((k) => k.id !== id);
    return latency(undefined, 200);
  }

  async deletePool(id: string) {
    // A key reaches only its own pool's members, so a pool cannot leave while keys still point at it.
    if (this.keys.some((k) => k.poolId === id)) return Promise.reject(new ApiError(409, 'pool still has keys', null));
    const pool = this.pools.find((p) => p.id === id);
    if (pool) this.buriedPools.set(id, Math.floor(pool.revision / INCARNATION_STAMP));
    this.pools = this.pools.filter((p) => p.id !== id);
    return latency(undefined, 200);
  }

  async disconnectAccount(accountId: string) {
    const a = this.account(accountId);
    // The route drops the credential and leaves the identity, so the row still says whose it was; with no credential
    // the poll's standing goes too.
    this.putAccount(revised({ ...a, credential: { accessTokenExpiresAt: null, lastError: null, lastRefreshAt: null, quarantined: false, refreshTokenExpiresAt: null }, status: 'unpaired', usage: usageOf({ ...a, status: 'unpaired' }) }));
    return latency(undefined, 250);
  }

  /** `refresh` re-stamps `fetchedAt` rather than answering a fresh fixture; every account answers the same
   * catalog except `nightly`, whose live fetch has been failing. */
  getAccountCatalog(accountId: string, refresh?: boolean): Promise<AccountCatalog> {
    const a = this.account(accountId);
    const fetchedAt = ago(refresh ? 0 : 40 * 1000);
    if (a.id === 'nightly') return latency({ error: 'the models request answered 503', fetchedAt, models: [] });
    return latency({ error: null, fetchedAt, models: CATALOG });
  }

  getAnalyticsBreakdown(query: AnalyticsQuery) {
    return latency(breakdownOf(emptyStore() ? [] : ALL_ROLLUPS, query));
  }

  getAnalyticsBurn(range: Pick<AnalyticsQuery, 'from' | 'to'>, groupBy: BurnGroup, filters?: Pick<AnalyticsQuery, 'accountIds' | 'userAgentGroups'>) {
    return latency(burnOf(emptyStore() ? [] : ALL_ROLLUPS, range, groupBy, filters));
  }

  getAnalyticsReconciliations(filters?: Pick<AnalyticsQuery, 'accountIds'>) {
    return latency(reconciliationsOf(emptyStore() ? [] : RECONCILIATIONS, filters));
  }

  getAnalyticsRewrites(range: Pick<AnalyticsQuery, 'from' | 'to'>) {
    return latency(rewritesOf(emptyStore() ? [] : ACCOUNT_ROLLUPS, emptyStore() ? [] : REQUESTS, range));
  }

  getAnalyticsSeries(query: AnalyticsQuery) {
    return latency(seriesOf(emptyStore() ? [] : ALL_ROLLUPS, query));
  }

  /** The story's conditions, or none for a fresh installation, the parked records' items gone once a replay landed them. */
  getAttention() {
    if (emptyStore()) return latency([]);
    return latency(this.parkedLanded ? ATTENTION.filter((i) => i.kind !== 'ledger_record_parked') : ATTENTION);
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
    if (emptyStore()) return latency({ cooldowns: [] });
    const byModel = new Map<string, RuleCooldown[]>();
    for (const row of this.modelCooldowns()) {
      const model = row.scope.slice(MODEL_SCOPE_PREFIX.length);
      byModel.set(model, [...(byModel.get(model) ?? []), row]);
    }
    const cooldowns = [...byModel.entries()].sort(([a], [b]) => a.localeCompare(b)).map(([model, rows]) => ({ model, rows }));
    return latency({ cooldowns });
  }

  getOverview() {
    const empty = emptyStore();
    // The route answers its 24 hourly buckets over an empty ledger too, at zero, so each spark draws a flat line.
    const last24h = empty ? [] : DAY_ROWS;
    const hourlyMap = new Map<string, Overview['hourly'][number]>();
    for (let i = 23; i >= 0; i -= 1) {
      const startMs = Math.floor((NOW - i * 3_600_000) / 3_600_000) * 3_600_000;
      const at = iso(startMs);
      const { billedUsd, equivalent } = requestSidesOf(last24h.filter((r) => Date.parse(r.at) >= startMs && Date.parse(r.at) < startMs + 3_600_000));
      hourlyMap.set(at, { at, billed: billedUsd, cost: 0, equivalent, errors: 0, requests: 0, tokens: 0 });
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
    const day = requestSidesOf(last24h);
    return latency({
      // Counted off the pools, so the dashboard and the Pools view cannot disagree on the router's pins.
      activePins: empty ? 0 : this.pools.reduce((n, p) => n + poolPins(p), 0),
      billed24h: day.billedUsd,
      cost24h: cost,
      equivalent24h: day.equivalent,
      errorRate24h: last24h.length ? last24h.filter((r) => r.status !== 200).length / last24h.length : null,
      estimated24h: day.estimated,
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

  getDecision(requestId: string, seq: number): Promise<DecisionRecord | null> {
    return latency(emptyStore() ? null : getDecision(requestId, seq));
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
    return latency(emptyStore() ? [] : this.accounts.map((a) => this.withPool(a)));
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

  listRoutingLog(query: RoutingLogQuery = {}): Promise<RoutingLogPage> {
    if (emptyStore()) return latency({ counts: query.request ? { decisions: 0, events: 0 } : null, nextCursor: null, rows: [] });
    // The route's refusals, in its order and words.
    const unknown = query.kinds?.find((k) => !ROUTING_LOG_KINDS.includes(k));
    if (unknown !== undefined) return Promise.reject(new ApiError(400, `unknown kind ${unknown}`, null));
    if (query.limit !== undefined && !(Number.isInteger(query.limit) && query.limit >= 1 && query.limit <= 200)) return Promise.reject(new ApiError(400, 'limit is 1 to 200', null));
    return latency(listRoutingLog(query, Date.now()));
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
    this.putAccount(revised({ ...a, routingEnabled: false, status: statusOf(a, false) }));
    return latency(undefined, 250);
  }

  refreshUsage(_accountId: string) {
    return latency(undefined, 400);
  }

  async releaseSession(keyId: string, sessionId: string) {
    this.sessions = this.sessions.filter((s) => !(s.keyId === keyId && s.sessionId === sessionId && !s.historical));
    return latency(undefined, 250);
  }

  async renameAccount(accountId: string, label: string, revision?: number) {
    const a = this.account(accountId);
    if (revision !== undefined && revision !== a.revision) {
      return Promise.reject(new ApiError(409, 'the account changed while the edit was being saved', 'stale_edit'));
    }
    return latency(this.withPool(this.putAccount(revised({ ...a, label }))), 250);
  }

  /** The first replay lands the story's parked count to zero, and a later one finds nothing to land. */
  async replayParked() {
    if (heldWriter()) return Promise.reject(new ApiError(503, 'the ledger writer is held, try again', null));
    const parked = emptyStore() || this.parkedLanded ? [] : ATTENTION.filter((i) => i.kind === 'ledger_record_parked');
    if (parked.length > 0) this.parkedLanded = true;
    return latency({ landed: parked.reduce((n, i) => n + (typeof i.detail.count === 'number' ? i.detail.count : 0), 0), remaining: 0 }, 250);
  }

  async resumeAccount(accountId: string) {
    const a = this.account(accountId);
    this.putAccount(revised({ ...a, routingEnabled: true, status: statusOf(a, true) }));
    return latency(undefined, 250);
  }

  async revokeKey(id: string) {
    // The route switches a revoked key off in the same write, and an edit sends that flag back.
    this.keys = this.keys.map((k) => (k.id === id ? { ...k, enabled: false, status: 'revoked' as const, ...restamp(k) } : k));
    return latency(undefined, 250);
  }

  async rotateKey(id: string): Promise<CreatedApiKey> {
    const key = this.keys.find((k) => k.id === id);
    if (!key) return Promise.reject(new ApiError(404, 'key not found', null));
    const secret = `phoebis-${this.token(40)}`;
    const next: ApiKey = { ...key, prefix: secret.slice(0, 16), retiredSecretLastUsedAt: null, sessionsOnRetiredSecret: 2, ...restamp(key) };
    this.keys = this.keys.map((k) => (k.id === id ? next : k));
    return latency({ key: next, secret }, 300);
  }

  async startPairing(start: PairingStart): Promise<PairingSession> {
    const accountId = 'accountId' in start ? start.accountId : start.id;
    // A new account's id is refused while a row holds it, as the route refuses it before the session starts.
    if ('id' in start && this.accounts.some((a) => a.id === start.id)) throw new ApiError(409, ACCOUNT_TAKEN, null);
    const state = this.token(24);
    // The router names the pairing with an id of its own; the state only ever travels through the browser.
    const id = `pair_${this.token(10)}`;
    this.pairings.set(id, 'id' in start ? { accountId, label: start.label, state } : { accountId, state });
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

  /** The account route's refusals in its order and words; the end is kept only while the flag stands. */
  async updateAccount(accountId: string, patch: AccountPatch, revision?: number): Promise<Account> {
    const a = this.account(accountId);
    if (patch.cancelled && a.kind === 'api_key') throw new ApiError(400, 'only an oauth account is cancelled', null);
    if (!patch.cancelled && patch.subscriptionEndsAt !== null) throw new ApiError(400, 'subscription_ends_at needs cancelled', null);
    if (patch.cancelled && patch.subscriptionEndsAt === null) throw new ApiError(400, 'cancelled needs subscription_ends_at', null);
    if (revision !== undefined && revision !== a.revision) {
      return Promise.reject(new ApiError(409, 'the account changed while the edit was being saved', 'stale_edit'));
    }
    // The route answers a subscription block for a cancelled OAuth row even before it pairs, so its end has a place.
    const blank: Subscription = { billingType: '', endsAt: null, organizationType: '', plan: null, rateLimitTier: '', since: null, trialEndsAt: null };
    const base = a.subscription ?? (patch.cancelled ? blank : null);
    const subscription = base ? { ...base, endsAt: patch.cancelled ? patch.subscriptionEndsAt : null } : null;
    return latency(this.withPool(this.putAccount(revised({ ...a, cancelled: patch.cancelled, subscription }))), 250);
  }

  async updateKey(id: string, input: KeyInput, revision?: number): Promise<ApiKey> {
    const idx = this.keys.findIndex((k) => k.id === id);
    if (idx < 0) return Promise.reject(new Error(`Unknown key ${id}`));
    const refusal = keyRulesRefusal(input);
    if (refusal) return Promise.reject(new ApiError(400, refusal, null));
    const { expiresAt: expiry, ...rest } = input;
    const current = this.keys[idx];
    if (revision !== undefined && revision !== current.revision) return Promise.reject(new ApiError(409, 'the key changed while it was being edited, so reload and try again', 'stale_edit'));
    const expiresAt = expiry === undefined ? current.expiresAt : expiry;
    // The status is the decoder's fold of the stored flags, so a key switched off or lapsed reads that way at once.
    const status = current.status === 'revoked' ? 'revoked' : expiresAt !== null && Date.parse(expiresAt) <= Date.now() ? 'expired' : rest.enabled ? 'active' : 'disabled';
    const next: ApiKey = { ...current, ...rest, expiresAt, status, ...restamp(current) };
    this.keys = this.keys.map((k, i) => (i === idx ? next : k));
    return latency(next, 250);
  }

  async updatePool(id: string, input: PoolInput, revision?: number): Promise<Pool> {
    const idx = this.pools.findIndex((p) => p.id === id);
    if (idx < 0) return Promise.reject(new Error(`Unknown pool ${id}`));
    const refusal = poolRefusal(input);
    if (refusal) return Promise.reject(new ApiError(400, refusal, null));
    if (input.bodyCaptureHours > this.settings.retention.bodyCaptureMaxHours) {
      return Promise.reject(new ApiError(400, 'body_capture_hours exceeds stats.body_capture_max_hours', null));
    }
    // The route refuses a repeat and a budget before the store looks any member up, and the store checks every member
    // inside the one transaction that writes the pool, so a refusal stores nothing.
    const repeated = repeatedMember(input.members);
    if (repeated) return Promise.reject(new ApiError(409, `members name account ${repeated} twice`, null));
    const members = memberRefusal(input.members);
    if (members) return Promise.reject(new ApiError(400, members, null));
    // In the route's order: the stamp after the list's own shape and ahead of the member lookups, so a stale edit that
    // names an unknown or a held member answers `stale_edit`.
    const current = this.pools[idx];
    if (revision !== undefined && revision !== current.revision) return Promise.reject(new ApiError(409, 'the pool changed while it was being edited, so reload and try again', 'stale_edit'));
    const unknown = input.members.find((m) => !this.accounts.some((a) => a.id === m.accountId));
    if (unknown) return Promise.reject(new ApiError(404, `account ${unknown.accountId} not found`, null));
    const held = heldElsewhere(input.members, this.pools, id);
    if (held) return Promise.reject(new ApiError(409, held, null));
    const verdict = new Map(current.members.map((m) => [m.accountId, m.eligible]));
    const pinned = new Map(current.members.map((m) => [m.accountId, m.activePins]));
    const next: Omit<Pool, 'activePins' | 'keys' | 'warnings'> = {
      ...current,
      ...input,
      // From the saved members' kinds, as the router composes them on every save.
      keepaliveResolved: resolvedFor(composedFor(input.members.map((m) => this.accounts.find((a) => a.id === m.accountId)?.kind ?? 'oauth'), input.cacheTtl), input.keepalive),
      // Eligibility is the router's reading of live cooldowns and overages, not something a save can set. A save
      // does rewrite the budget, and a write answers no spend here, so a budget verdict is read again.
      members: input.members.map((m) => {
        const account = this.accounts.find((a) => a.id === m.accountId);
        const kept = verdict.get(m.accountId) ?? 'eligible';
        return writtenMember(m, account, input.slowDown, (member) => (kept === 'budget' ? eligibilityOf(account, input.overagePolicy, member) : kept), pinned.get(m.accountId));
      }),
      ...restamp(current),
    };
    this.pools = this.pools.map((p, i) => (i === idx ? next : p));
    return latency(this.poolView(next), 250);
  }

  async verifyAccount(accountId: string) {
    this.account(accountId);
    return latency({ ok: true, status: 200 }, 400);
  }
}
