import type { AccountPatch, Api, CreateKeyInput, KeyInput, PoolInput } from '@/api/client';
import type { GuardrailInput, GuardrailRange, GuardrailTestInput, OverviewRange } from '@/api/client';
import { AFFINITY_MODE_VALUES, RETURN_TO_PLAN_VALUES, SESSION_PLACEMENT_VALUES, SUBAGENT_PLACEMENT_VALUES, WARM_SESSIONS_VALUES, decodeSettings } from '@/api/decode';
import { GUARDRAIL_MODE_VALUES, keyStatusOf } from '@/api/decode';
import { decodeStreamCut } from '@/api/decode';
import { ApiError, catalogUnion, noteBuild, slug } from '@/api/http';
import type {
  Account, AccountCatalog, AccountInput, AnalyticsQuery, ApiKey, BurnGroup, CatalogModel, CreatedApiKey, DecisionRecord, KeepaliveOverview, KeyModels, MemberEligibility,
  ModelRule, Overview, PairingConfirmed, PairingInput, PairingPreview, PairingSession, PairingStart, Pool, PoolMember, RequestDetail, RequestEvent, RequestLog, RequestPage, RequestQuery, RoutingEvent, RoutingLogPage, RoutingLogQuery, RuleCooldown, Session, SessionInfo, Settings,
  Subscription,
} from '@/api/types';
import type { Guardrail, GuardrailAttachment, GuardrailCounts, GuardrailRef, GuardrailTestResult } from '@/api/types';
import { type Clock, wallClock } from '@/lib/clock';
import { foldClassifier, foldEffort, foldRequestClass, foldSpeed, servedMissReason, servingResponse } from '@/lib/dimensions';
import { tokensTotal } from '@/lib/format';
import { modelRulesRefusal, poolWarnings, type NumericKeepaliveKey } from '@/lib/forms';
import { matchesPattern } from '@/lib/pattern';
import { matchesPattern as matchesModelPattern, processRules } from '@/lib/patterns';
import { MODEL_SCOPE_PREFIX, accountCooldown, accountStatusOf } from '@/lib/quota';
import { compareRequests } from '@/lib/requests';
import { ACCOUNTS, QUOTA_HISTORY, accountById, usageOf } from './accounts';
import { RECONCILIATIONS, breakdownOf, burnOf, placementsOf, reconciliationsOf, refusalCategoriesOf, rewritesOf, seriesOf, unusedAllowanceOf } from './analytics';
import { ATTENTION } from './attention';
import { CATALOG } from './catalog';
import { GUARDRAIL_RESERVED_IDS, GUARDRAIL_STALE, GUARDRAIL_TAKEN, GUARDRAILS, PRESETS, boundRefusal, conditionsRefusal, dryRunVerdict, repeatedId, sameConditions, withAttachments } from './guardrails';
import { KEEPALIVE_OVERVIEW, KEEPALIVE_OVERVIEW_30D, RECIPES, SESSIONS, SETTINGS_WIRE, memberPins, pinsUnderPool, sessionsListedAt } from './operations';
import { KEYS, POOLS, composedFor, resolvedFor, servedFigures } from './pools';
import { ACCOUNT_ROLLUPS, ALL_ROLLUPS, DAY_ROWS, REQUEST_EVENTS, REQUESTS, UNPRICED_REQUEST, UNPRICED_ROLLUPS, capturedBodiesFor, landedAt, requestById, requestSidesOf } from './requests';
import { GUARDRAIL_STORY_IDS } from './requests';
import { createRng } from './rng';
import { ROUTING_LOG_KINDS, WEEKLY_RESETS, getDecision, listRoutingLog } from './routingLog';
import { ROUTING_EVENTS } from './routingLog';
import { ACCOUNT_SAMPLE, KEY_SAMPLE, PAIRING_SAMPLE, POOL_SAMPLE } from './samples';
import { DAY, HOUR, MINUTE, NOW, ago, ahead, iso, startOfHour } from './time';

// Fixed, so a pass over the mock reads the same secrets every run.
const MOCK_SEED = 4_211;

/** The clock the mock's answers wait on and its landings read. */
type MockClock = Pick<Clock, 'now' | 'setTimeout'>;

let mockClock: MockClock = wallClock;

/**
 * The test seam: a harness that moves time hands the mock its clock, so a step of it lands the mock's answers and
 * moves its landings. `null` puts the wall clock back.
 */
export function installMockClock(next: MockClock | null): void {
  mockClock = next ?? wallClock;
}

let pending = 0;
// A short delay keeps loading states honest without making the mock feel slow. The answers in flight are counted so
// a harness settles on the mock's own signal rather than on a fixed wait.
const latency = <T,>(value: T, ms = 120): Promise<T> => {
  pending += 1;
  return new Promise((resolve) =>
    mockClock.setTimeout(() => {
      pending -= 1;
      // On each answer, as the transport reads each one's header, since the shell's hook may register after the first.
      if (buildMismatch()) noteBuild(OTHER_BUILD);
      resolve(value);
    }, ms),
  );
};
/** How many mock answers are still on their timer. */
export const inFlight = (): number => pending;

/** How long a confirm's first usage poll takes to land, on the mock's clock: the few seconds its answer names. */
const FIRST_POLL_MS = 2_000;
/**
 * The windows a paired account's first poll reads: the shape the story gives primary's plan windows, each stamped at
 * the read that lands it, as the router stamps a reading when it reads it, so none predates the confirm.
 */
const FIRST_READING = accountById.get('primary')?.windows ?? [];

// The routes' own wording, so a page draws the text the router would have sent.
const ACCOUNT_TAKEN = 'an account with this id exists';
const KEY_TAKEN = 'a key with this id exists';
const PAIRING_NOT_FOUND = 'pairing session not found';
const POOL_TAKEN = 'a pool with this id exists';

/** Strictly later than the stamp it replaces, as the route's fence stamps, so a save inside one millisecond still
 * moves it and an edit read before it cannot pass. */
const stamped = (previous: string) => iso(Math.max(mockClock.now(), Date.parse(previous) + 1));

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
  return rulesRefusal(input.modelRules) ?? (mode === undefined || mode === 'extend' || mode === 'replace' ? null : 'model_rules_mode is extend or replace');
};

/**
 * The key routes refuse a body with a member outside `KeyInput` before they read any member, so the mock answers the
 * pool routes' sentence first.
 */
const KEY_INPUT_MEMBERS: ReadonlySet<string> = new Set(['autoModeServer', 'bodyCaptureHours', 'enabled', 'expiresAt', 'guardrails', 'keepaliveOptOut', 'longContextTwins', 'modelRules', 'modelRulesMode', 'name', 'notes', 'poolId']);
const unknownKeyMember = (input: object, also: readonly string[] = []): string | null => (Object.keys(input).some((k) => !KEY_INPUT_MEMBERS.has(k) && !also.includes(k)) ? 'body must be a JSON object' : null);

/**
 * The ceiling, shedding, failover, slow-down and return-to-plan refusals, in the order and words of the pool routes.
 */
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

/** The guardrail routes' refusal of a pool or a key named twice, the pools read first. */
const subjectsTwice = (input: Pick<Partial<GuardrailInput>, 'keys' | 'pools'>): string | null => {
  const pool = repeatedId(input.pools);
  if (pool !== null) return `pools lists ${pool} twice`;
  const key = repeatedId(input.keys);
  return key === null ? null : `keys lists ${key} twice`;
};

/** The route's 409 for a member another pool holds, since an account belongs to one pool. */
const heldElsewhere = (members: PoolInput['members'], pools: { id: string; members: { accountId: string }[] }[], poolId: string): string | null => {
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

/**
 * The affinity floor, the rule list, the keepalive policy's and the engine's refusals, first one first, as the pool
 * routes answer them.
 */
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

/**
 * The signed-in mark, which outlives a reload the way the router's session cookie does: the smoke runner writes
 * it so every mount starts signed in, and a login writes it so a browser pass that loads each view by URL stays
 * signed in.
 */
const SEEDED_SESSION_KEY = 'phoebis.mockSession';
const MOCK_SESSION: SessionInfo = { persistent: false, sessionId: 'mock-admin' };
/**
 * A login's mark carries the Stay logged in choice, so a reload reads it, and the bare `admin` the smoke's own writers
 * store reads as a login left unchecked.
 */
const persistentOf = (mark: string): boolean => {
  try {
    return (JSON.parse(mark) as { persistent?: unknown } | null)?.persistent === true;
  } catch {
    return false;
  }
};
const seededSession = (): SessionInfo | null => {
  try {
    const mark = localStorage.getItem(SEEDED_SESSION_KEY);
    return mark === null ? null : { ...MOCK_SESSION, persistent: persistentOf(mark) };
  } catch {
    return null;
  }
};
/** `null` signs out. */
const seedSession = (persistent: boolean | null) => {
  try {
    if (persistent !== null) localStorage.setItem(SEEDED_SESSION_KEY, JSON.stringify({ persistent, user: 'admin' }));
    else localStorage.removeItem(SEEDED_SESSION_KEY);
  } catch {
    // No storage (the Node tests): the session lives on the instance alone.
  }
};

/** Whether a stand-in switch is set. Without storage (the Node tests) none is. */
const switchSet = (key: string): boolean => {
  try {
    return localStorage.getItem(key) === '1';
  } catch {
    return false;
  }
};

/**
 * Set to stand in for a fresh installation: every registry list and every ledger read answers nothing, while the
 * seeded store stays in memory for the next read without the switch.
 */
export const EMPTY_STORE_KEY = 'phoebis.mockEmpty';
export const emptyStore = (): boolean => switchSet(EMPTY_STORE_KEY);

/**
 * Set to stand in for a ledger writer holding a batch past the replay route's bound: a replay answers the route's `503`
 * and lands nothing.
 */
export const HELD_WRITER_KEY = 'phoebis.mockHeldWriter';
const heldWriter = (): boolean => switchSet(HELD_WRITER_KEY);

/** Set to stand in for a sessions read the store refuses: the list answers a `503` and draws nothing. */
export const SESSIONS_FAIL_KEY = 'phoebis.mockSessionsFail';
const sessionsFail = (): boolean => switchSet(SESSIONS_FAIL_KEY);

/**
 * Set to stand in for a router that runs another build: each answer names a build other than the bundle's, as an
 * answer's header names it to the transport's `call`, which the mock never goes through, so the shell draws its reload
 * banner.
 */
export const BUILD_MISMATCH_KEY = 'phoebis.mockBuildMismatch';
const buildMismatch = (): boolean => switchSet(BUILD_MISMATCH_KEY);
/** A build no bundle is made from. */
const OTHER_BUILD = '0'.repeat(40);

/**
 * Set to add the one plan request on a model the cost model lists in no tier: the dashboard's day and the analytics'
 * folds read it beside the story, so the figures it moves read a window priced nothing, while without the switch the
 * served story's figures stay as they are.
 */
export const UNPRICED_STORY_KEY = 'phoebis.mockUnpriced';
const unpricedStory = (): boolean => switchSet(UNPRICED_STORY_KEY);
/**
 * The ledger's read sets as the routes fold them: the story's, the unpriced request's rows beside the story's under
 * its switch, and an empty form of each for a fresh installation, so a route that reads its rows here answers that
 * switch with no test of its own.
 */
const ledger = () => {
  const empty = emptyStore();
  const unpriced = !empty && unpricedStory();
  return {
    accountRollups: empty ? [] : ACCOUNT_ROLLUPS,
    attention: empty ? [] : ATTENTION,
    catalog: empty ? [] : CATALOG,
    dayRows: empty ? [] : unpriced ? [...DAY_ROWS, UNPRICED_REQUEST] : DAY_ROWS,
    decisionOf: (requestId: string, seq: number): DecisionRecord | null => (empty ? null : getDecision(requestId, seq)),
    keepalive: empty ? EMPTY_KEEPALIVE_OVERVIEW : KEEPALIVE_OVERVIEW,
    keepalive30d: empty ? EMPTY_KEEPALIVE_OVERVIEW : KEEPALIVE_OVERVIEW_30D,
    quotaHistory: empty ? [] : QUOTA_HISTORY,
    reconciliations: empty ? [] : RECONCILIATIONS,
    recipes: empty ? [] : RECIPES,
    requestById: empty ? new Map<string, RequestLog>() : requestById,
    requests: empty ? [] : REQUESTS,
    rollups: empty ? [] : unpriced ? [...ALL_ROLLUPS, ...UNPRICED_ROLLUPS] : ALL_ROLLUPS,
    weeklyResets: empty ? [] : WEEKLY_RESETS,
  };
};
/**
 * The key the unpriced request lands on, its lifetime and its week read null under the switch, since the request
 * prices nothing and falls inside the week. The keys' fold runs once at load, so the null lands at the read, and the
 * key's other figures stay the story's.
 */
const unpricedKey = (k: Omit<ApiKey, 'guardrails' | 'poolGuardrails'>): Omit<ApiKey, 'guardrails' | 'poolGuardrails'> => {
  if (!unpricedStory() || k.id !== UNPRICED_REQUEST.keyId || Date.parse(k.createdAt) !== UNPRICED_REQUEST.keyGeneration) return k;
  const nulled = <S extends Pick<ApiKey['lifetime'], 'equivalent'>>(s: S): S => ({ ...s, equivalent: null });
  return { ...k, lifetime: nulled(k.lifetime), usage7d: nulled(k.usage7d) };
};
/**
 * The request's `stream_interrupted` rows, which the detail lists as its events and lifts its cut off, the newest
 * one's, as the route does.
 */
const cutsOf = (requestId: string) => ROUTING_EVENTS.filter((e) => e.kind === 'stream_interrupted' && e.requestId === requestId);

/** The keepalive overview of a router with no session and no probe yet. */
const EMPTY_KEEPALIVE_OVERVIEW: KeepaliveOverview = {
  agents: { busy: 0, idle: 0, parked: 0 },
  busGaps: 0,
  expiredBetweenTurns24h: 0,
  // The caps are the bootstrap file's defaults, which the story's overview carries.
  limits: { agents: { ...KEEPALIVE_OVERVIEW.limits.agents, count: 0 }, retainedBytes: { ...KEEPALIVE_OVERVIEW.limits.retainedBytes, count: 0 }, sessions: { ...KEEPALIVE_OVERVIEW.limits.sessions, count: 0 } },
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

/**
 * A flag outranks the enabled bit and the cooldowns in the status the account view folds, so pausing never hides one.
 */
const FLAGGED: Account['status'][] = ['funding_required', 'reauth_required', 'unpaired'];
const statusOf = (a: Account, enabled: boolean): Account['status'] =>
  accountStatusOf(FLAGGED.includes(a.status) ? (a.status as 'funding_required' | 'reauth_required' | 'unpaired') : null, enabled, a.cooldowns);

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

/** Whether the router reads the account's term as over: canceled with its saved end passed, or flagged plan ended. */
const termEnded = (a: Account, now: number): boolean => (a.canceled && a.subscription?.endsAt != null && Date.parse(a.subscription.endsAt) <= now) || (a.planEndedAt !== null && Date.parse(a.planEndedAt) <= now);

/**
 * The pool route's reading of a member, in its order: paused, not ready or past its term is unavailable, then an
 * account-wide cooldown, then a spend at or past either of the member's budgets inside the period it was reported for,
 * then overage the router observed under a policy that admits none.
 */
const eligibilityOf = (a: Account | undefined, policy: PoolInput['overagePolicy'], member: BudgetedSeed): MemberEligibility => {
  if (!a || !a.routingEnabled || FLAGGED.includes(a.status) || termEnded(a, Date.now())) return 'unavailable';
  if (a.cooldowns.some((c) => c.scope === 'account' && Date.parse(c.until) > Date.now())) return 'cooldown';
  if (budgetUntilOf(member, Date.now()) !== null) return 'budget';
  if (policy === 'never' && a.overage?.inUse) return 'overage';
  return 'eligible';
};

/**
 * A written member as the route answers it: the kind, the burn, the Pace score and the speed read off the account, and
 * no spend, since the mock keeps no ledger to price one from. A member sent without a budget field has no budget over
 * that period.
 */
const writtenMember = (m: PoolInput['members'][number], account: Account | undefined, slowDown: boolean, eligible: (member: Omit<PoolMember, 'activePins' | 'eligible'>) => MemberEligibility): Omit<PoolMember, 'activePins'> => {
  const member = { accountId: m.accountId, budgetDailyUsd: m.budgetDailyUsd ?? null, budgetMonthlyUsd: m.budgetMonthlyUsd ?? null, budgetUntil: null, kind: account?.kind ?? 'oauth', priority: m.priority, spends: [], ...servedFigures(m.accountId, Date.now(), slowDown) };
  return { ...member, eligible: eligible(member) };
};

/**
 * A pool's pins are every pin under the keys it holds at the read, so a pool created again under a deleted one's id
 * reads none of the old keys' pins. A pin on a member that left the pool counts here and in no member's cell.
 */
const poolPins = (p: Pick<Pool, 'id'>, keys: readonly Pick<ApiKey, 'id' | 'poolId'>[]): number => pinsUnderPool(new Set(keys.filter((k) => k.poolId === p.id).map((k) => k.id)));

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
  if (q.agentId && r.agentId !== q.agentId) return false;
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
  if (q.speeds?.length && !q.speeds.includes(foldSpeed(r))) return false;
  if (q.held && r.held === null) return false;
  // The serving attempt is the last, the one the rollups count a request's reason on. A request with no attempt or a
  // null serving reason matches nothing.
  if (q.cacheMissReasons?.length) {
    const reason = servedMissReason(r);
    if (reason === null || !q.cacheMissReasons.includes(reason)) return false;
  }
  // The stop reason and the category are the serving attempt's too, and `-` keeps a request whose serving attempt
  // carries none or that no attempt served.
  const serving = servingResponse(r);
  if (q.stopReasons?.length && !q.stopReasons.includes(serving?.stopReason ?? '-')) return false;
  if (q.refusalCategories?.length && !q.refusalCategories.includes(serving?.refusalCategory ?? '-')) return false;
  if (q.efforts?.length && !q.efforts.includes(foldEffort(r.shape.effort))) return false;
  if (q.requestClasses?.length && !q.requestClasses.includes(foldRequestClass(r.requestClass))) return false;
  if (q.classifiers?.length && !q.classifiers.includes(foldClassifier(r.classifier))) return false;
  // `-` keeps a request no guardrail matched, the spelling the class filter uses for none.
  if (q.guardrails?.length) {
    const listed = q.guardrails;
    if (!r.guardrails.some((m) => listed.includes(m.id)) && !(listed.includes('-') && r.guardrails.length === 0)) return false;
  }
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
  /** The incarnation each deleted guardrail id reached, which its next guardrail starts above. */
  private buriedGuardrails = new Map<string, number>();
  /** The incarnation each deleted key id reached, which its next key starts above. */
  private buriedKeys = new Map<string, number>([['desktop', 1]]);
  /** The incarnation each deleted pool id reached, which its next pool starts above. */
  private buriedPools = new Map<string, number>();
  /** The instant each confirm started its account's first poll, held until a list read lands that reading. */
  private firstPolls = new Map<string, number>();
  // The attachments live on the guardrails alone, as one table holds them in the registry, and every pool and key view
  // reads its lists from here.
  private guardrails: Guardrail[] = GUARDRAILS.map((g) => ({ ...g, revision: stampToken(1, g.updatedAt) }));
  // The live desktop key was issued under the id its deleted earlier holder had, so it is that id's second; every other
  // key of the story is its id's first.
  private keys: Omit<ApiKey, 'guardrails' | 'poolGuardrails'>[] = KEYS.map((k) => ({ ...k, revision: stampToken((this.buriedKeys.get(k.id) ?? 0) + 1, k.updatedAt) }));
  /** `label` is set on a pairing that creates its row at the confirm. The row's id is `accountId` either way. */
  private pairings = new Map<string, { accountId: string; label?: string; state: string }>();
  /** Set by the first replay, which lands the story's parked records. */
  private parkedLanded = false;
  // The route reads every member's kind and verdict afresh on each answer; the story's are read once, at load.
  private pools: (Omit<Pool, 'activePins' | 'guardrails' | 'keys' | 'members' | 'warnings'> & { members: Omit<PoolMember, 'activePins'>[] })[] = POOLS.map((p) => ({
    ...p,
    // Every pool of the story is its id's first.
    revision: stampToken(1, p.updatedAt),
    members: p.members.map((m) => {
      const account = this.accounts.find((a) => a.id === m.accountId);
      return { ...m, budgetUntil: budgetUntilOf(m, Date.now()), eligible: eligibilityOf(account, p.overagePolicy, m), kind: account?.kind ?? 'oauth', ...servedFigures(m.accountId, Date.now(), p.slowDown) };
    }),
  }));
  private secretRng = createRng(MOCK_SEED);
  private session: SessionInfo | null = null;
  private sessions: Session[] = [...SESSIONS];
  // The settings name the bundle's own commit as a router built from it would. The banner reads the build off an
  // answer's header through `call` alone, which the mock never goes through, so it draws none whatever this holds.
  private settings: Settings = decodeSettings({ ...SETTINGS_WIRE, about: { ...SETTINGS_WIRE.about, build: __PHOEBIS_BUILD__ } });
  /** The routing log rows this mock's writes recorded, after the story's, as the ledger appends them. */
  private written: RoutingEvent[] = [];

  private token(n: number) {
    return Array.from({ length: n }, () => 'abcdefghijklmnopqrstuvwxyz0123456789'[this.secretRng.int(0, 35)]).join('');
  }

  /** The registry's lists as the routes read them, or none for a fresh installation. */
  private registry() {
    const empty = emptyStore();
    return { accounts: empty ? [] : this.accounts, guardrails: empty ? [] : this.guardrails, keys: empty ? [] : this.keys, pools: empty ? [] : this.pools, sessions: empty ? [] : this.sessions };
  }

  /**
   * The pool as the route answers it, with its keys listed from the key registry at the time of the read, and its
   * warnings the route's checks on the saved settings and the largest cohort among the pinned sessions under its keys.
   */
  private poolView(p: Omit<Pool, 'activePins' | 'guardrails' | 'keys' | 'members' | 'warnings'> & { members: Omit<PoolMember, 'activePins'>[] }): Pool {
    const keys = this.keys.filter((k) => k.poolId === p.id).map((k) => ({ enabled: k.enabled, id: k.id, label: k.name, prefix: k.prefix }));
    const ids = new Set(keys.map((k) => k.id));
    const cohort = Math.max(0, ...this.sessions.filter((s) => s.pinned && ids.has(s.keyId)).map((s) => s.agents));
    // A member's pins are read under the keys the pool holds at the read, as its figure is, so a key that moved here
    // counts its pins on the member cells too.
    const members = p.members.map((m) => ({ ...m, activePins: memberPins(ids, m.accountId) }));
    return { ...p, activePins: poolPins(p, this.keys), guardrails: this.guardrailRefs('pools', p.id), keys, members, warnings: poolWarnings(p, cohort) };
  }

  /**
   * The key as the route answers it, its own guardrails and its pool's read from the attachments at the time of the
   * read.
   */
  private keyView(k: Omit<ApiKey, 'guardrails' | 'poolGuardrails'>): ApiKey {
    return { ...k, guardrails: this.guardrailRefs('keys', k.id), poolGuardrails: this.guardrailRefs('pools', k.poolId) };
  }

  /** A pool's or a key's guardrails in the order it runs them. */
  private guardrailRefs(kind: 'keys' | 'pools', subjectId: string): GuardrailRef[] {
    return this.guardrails
      .flatMap((g) => g[kind].filter((a) => a.id === subjectId).map((a) => ({ enabled: g.enabled, id: g.id, mode: g.mode, name: g.name, position: a.position })))
      .sort((a, b) => a.position - b.position);
  }

  /** The guardrail as the route answers it, each attachment labeled by its pool's or key's name at the read. */
  private guardrailView(g: Guardrail): Guardrail {
    const labeled = (list: GuardrailAttachment[], nameOf: (id: string) => string | undefined) => list.map((a) => ({ ...a, label: nameOf(a.id) ?? a.id })).sort((a, b) => a.position - b.position || a.id.localeCompare(b.id));
    return { ...g, keys: labeled(g.keys, (id) => this.keys.find((k) => k.id === id)?.name), pools: labeled(g.pools, (id) => this.pools.find((p) => p.id === id)?.name) };
  }

  /** A subject's list replaced whole from its own side, positions 0 to n-1 in the given order. */
  private setSubjectGuardrails(kind: 'keys' | 'pools', subjectId: string, ids: string[], moveStamps = true) {
    this.guardrails = this.guardrails.map((g) => {
      const others = g[kind].filter((a) => a.id !== subjectId);
      const at = ids.indexOf(g.id);
      const held = others.length < g[kind].length;
      const next = withAttachments(g, kind, at < 0 ? others : [...others, { id: subjectId, label: subjectId, position: at }]);
      // A guardrail dialog opened before a save presents a stamp the row no longer carries, as the routes refuse it. A
      // reorder leaves the guardrail's membership, and a subject delete removes its attachments as the store's delete
      // does, so neither moves a stamp.
      return !moveStamps || held === (at >= 0) ? next : { ...next, ...restamp(g) };
    });
  }

  /**
   * The bound of a write from the guardrail's side over the lists it would leave, pool by pool in id order: a named
   * pool takes the guardrail at the end of its list, a named key at the end of its own, and every key of a named pool
   * and every named key is counted against its pool's list. An absent list leaves that kind's lists as they stand.
   */
  private sideBoundRefusal(id: string, pools: string[] | undefined, keys: string[] | undefined): string | null {
    const place = (list: string[], attached: boolean) => (attached ? (list.includes(id) ? list : [...list, id]) : list.filter((x) => x !== id));
    const listOf = (kind: 'keys' | 'pools', subjectId: string) => this.guardrailRefs(kind, subjectId).map((r) => r.id);
    const affected = new Set([...(pools ?? []), ...(keys ?? []).flatMap((k) => this.keys.find((x) => x.id === k)?.poolId ?? [])]);
    for (const poolId of [...affected].sort()) {
      const poolNamed = pools?.includes(poolId) ?? false;
      const poolList = pools === undefined ? listOf('pools', poolId) : place(listOf('pools', poolId), poolNamed);
      const keyLists = this.keys
        .filter((k) => k.poolId === poolId && (poolNamed || (keys?.includes(k.id) ?? false)))
        .sort((a, b) => a.id.localeCompare(b.id))
        .map((k): [string, string[]] => [k.id, keys === undefined ? listOf('keys', k.id) : place(listOf('keys', k.id), keys.includes(k.id))]);
      const refusal = boundRefusal(poolId, poolList, keyLists);
      if (refusal) return refusal;
    }
    return null;
  }

  /**
   * From the guardrail's side: a subject newly named joins the end of its list, one already attached keeps its place.
   */
  private attached(g: Guardrail, kind: 'keys' | 'pools', ids: string[] | undefined): GuardrailAttachment[] {
    if (ids === undefined) return g[kind];
    const kept = g[kind].filter((a) => ids.includes(a.id));
    const next = (subjectId: string) => Math.max(-1, ...this.guardrails.filter((x) => x.id !== g.id).flatMap((x) => x[kind].filter((a) => a.id === subjectId).map((a) => a.position))) + 1;
    return [...kept, ...ids.filter((s) => !kept.some((a) => a.id === s)).map((s) => ({ id: s, label: s, position: next(s) }))];
  }

  /** The route's checks of a guardrail's own members, in its order: the name, the description, the mode. */
  private guardrailFieldRefusal(input: Partial<GuardrailInput>): string | null {
    const bytes = (text: string) => new TextEncoder().encode(text).length;
    if (input.name !== undefined) {
      if (bytes(input.name) > 256) return 'name is at most 256 bytes';
      if (input.name === '') return 'name is required';
      // The route refuses each phrase the client reads specially in an error's message, matched in any case and
      // answered as its list spells it.
      const lowered = input.name.toLowerCase();
      const phrase = ['model:', 'OAuth token has been revoked', 'prompt is too long'].find((p) => lowered.includes(p.toLowerCase()));
      if (phrase !== undefined) return `name must not contain "${phrase}"`;
    }
    if (input.description !== undefined && bytes(input.description) > 256) return 'description is at most 256 bytes';
    if (input.mode !== undefined && !GUARDRAIL_MODE_VALUES.some((m) => m === input.mode)) return 'mode is observe or enforce';
    return null;
  }

  /** The pools and keys a guardrail-side write names that the registry lacks, which the store refuses by name. */
  private missingSubject(pools: string[] | undefined, keys: string[] | undefined): string | null {
    const pool = (pools ?? []).find((id) => !this.pools.some((p) => p.id === id));
    if (pool !== undefined) return `pool ${pool} not found`;
    const key = (keys ?? []).find((id) => !this.keys.some((k) => k.id === id));
    return key === undefined ? null : `key ${key} not found`;
  }

  /**
   * The stamp as the route reads it against the shipped presets: customized where the conditions left the stamped
   * version's.
   */
  private presetStateOf(preset: Guardrail['preset'], conditions: Guardrail['conditions']): Guardrail['preset'] {
    if (preset === null) return null;
    const stamped = PRESETS.find((p) => p.id === preset.id && p.version === preset.version);
    return { ...preset, currentVersion: PRESETS.find((p) => p.id === preset.id)?.version ?? preset.version, customized: stamped === undefined || !sameConditions(conditions, stamped.conditions) };
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

  /**
   * A list read at least `FIRST_POLL_MS` after a confirm lands that confirm's first reading on its row, where the row
   * reads no window yet. Keyed on the clock rather than a count of reads, since the confirm's reload and the refresh
   * the closing dialog resumes read the list at once on a live clock.
   */
  private landFirstPolls() {
    const at = mockClock.now();
    for (const [id, confirmedAt] of this.firstPolls) {
      if (at - confirmedAt < FIRST_POLL_MS) continue;
      this.firstPolls.delete(id);
      const a = this.accounts.find((x) => x.id === id);
      if (a && a.windows.length === 0 && a.fableWindow === null) this.putAccount({ ...a, windows: FIRST_READING.map((w) => ({ ...w, observedAt: iso(at) })) });
    }
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
    // Read off the members as they stand, so a budget set on the Pools form shows here; one row per budgeted
    // period, and the route orders by pool, then account, then the day ahead of the month, which the stable sort
    // keeps from the order each member's rows are built in.
    const members = this.registry().pools.flatMap((p) =>
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
    return latency(placementsOf(ledger().requests, query));
  }

  async cancelPairing(pairingId: string) {
    this.pairings.delete(pairingId);
    return latency(undefined, 150);
  }

  async clearCooldown(accountId: string, scope: string) {
    const a = this.account(accountId);
    const cooldowns = a.cooldowns.filter((c) => c.scope !== scope);
    this.putAccount({ ...a, cooldowns, status: accountCooldown(cooldowns) !== null ? a.status : 'ready' });
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

  async confirmPairing(pairingId: string): Promise<PairingConfirmed> {
    const pairing = this.pairings.get(pairingId);
    // Deleted either way: the router fails the session on a refused confirm.
    this.pairings.delete(pairingId);
    if (pairing === undefined) throw new ApiError(404, PAIRING_NOT_FOUND, null);
    // The story seals legacy's stored credential under an earlier storage key, so the route refuses a re-pair's confirm
    // until a disconnect deletes that credential.
    const held = pairing.label === undefined ? this.accounts.find((a) => a.id === pairing.accountId) : undefined;
    if (held?.id === 'legacy' && held.credential.refreshTokenExpiresAt !== null) throw new ApiError(409, `${held.label} holds a credential sealed under another storage key, so disconnect it before pairing again`, 'sealed_under_another_key');
    if (pairing.label !== undefined) {
      // The backstop for a create that landed between the start and the confirm.
      if (this.accounts.some((a) => a.id === pairing.accountId)) throw new ApiError(409, ACCOUNT_TAKEN, null);
      const row = this.newAccountRow({ id: pairing.accountId, kind: 'oauth', label: pairing.label, secret: null }, 'ready', 'operator+new@example.com');
      // The confirm stores the tokens it exchanged, so the row carries both their expiries, as the router answers it.
      this.accounts = [...this.accounts, { ...row, credential: { ...row.credential, accessTokenExpiresAt: ahead(8 * HOUR), refreshTokenExpiresAt: ahead(30 * DAY) } }];
    }
    // The router's confirm clears a plan-ended flag, since the confirmed profile holds a plan, and records the plan
    // found again under the pairing's source. The mock's profile is the stored document, so the row carries its words.
    if (held !== undefined && held.planEndedAt !== null) {
      this.putAccount({ ...held, planEndedAt: null });
      const detail = { organization_type: held.subscription?.organizationType ?? null, rate_limit_tier: held.subscription?.rateLimitTier ?? null, source: 'pairing', subscription_status: 'active' };
      this.written.push({ type: 'event', accountId: held.id, actor: null, agentId: null, at: iso(mockClock.now()), detail, id: ROUTING_EVENTS.length + this.written.length + 1, keyId: null, kind: 'plan_restored', model: null, poolId: null, requestId: null, sessionId: null });
    }
    this.firstPolls.set(pairing.accountId, mockClock.now());
    return latency({ accountId: pairing.accountId, firstPollInFlight: true, status: 'confirmed' }, 250);
  }

  async createAccount(input: AccountInput): Promise<Account> {
    const account = this.newAccountRow(input, input.kind === 'oauth' ? 'unpaired' : 'ready', null);
    this.accounts = [...this.accounts, account];
    return latency(this.withPool(account), 300);
  }

  /**
   * The create route's checks in its order. A preset supplies the name, the mode and the conditions the input leaves
   * out, never the description, and stamps the row whatever the input changed.
   */
  async createGuardrail(input: GuardrailInput & { presetVersion?: never }): Promise<Guardrail> {
    if (input.presetVersion !== undefined) throw new ApiError(400, 'preset_version applies on edit', null);
    const preset = input.preset === undefined ? undefined : PRESETS.find((p) => p.id === input.preset);
    // The client sends the id it derives from the name, so a name already taken is an id already taken.
    const id = slug(input.name ?? preset?.name ?? '');
    if (GUARDRAIL_RESERVED_IDS.includes(id)) throw new ApiError(400, 'id is reserved', null);
    if (this.guardrails.some((g) => g.id === id)) throw new ApiError(409, GUARDRAIL_TAKEN, null);
    if (input.preset !== undefined && preset === undefined) throw new ApiError(400, `unknown preset ${input.preset}`, null);
    const name = input.name ?? preset?.name;
    if (name === undefined) throw new ApiError(400, 'name is required', null);
    const fields = this.guardrailFieldRefusal({ ...input, name });
    if (fields !== null) throw new ApiError(400, fields, null);
    // The shipped table's own tests hold every preset to the rules, so a preset's conditions stand as they are.
    const refusal = (input.conditions === undefined && preset !== undefined ? null : conditionsRefusal(input.conditions)) ?? subjectsTwice(input);
    if (refusal !== null) throw new ApiError(400, refusal, null);
    const conditions = input.conditions ?? preset?.conditions ?? [];
    const attach = this.sideBoundRefusal(id, input.pools, input.keys);
    if (attach !== null) throw new ApiError(400, attach, null);
    const missing = this.missingSubject(input.pools, input.keys);
    if (missing !== null) throw new ApiError(404, missing, null);
    const at = iso(NOW);
    const blank: Guardrail = {
      action: 'block',
      conditions: conditions.map((c) => ({ name: c.name, operator: c.operator, path: c.path, selector: c.selector, subject: c.subject, value: c.value })),
      createdAt: at,
      description: input.description ?? '',
      enabled: input.enabled ?? true,
      id,
      keys: [],
      mode: input.mode ?? preset?.suggestedMode ?? 'observe',
      name,
      pools: [],
      preset: null,
      revision: stampToken((this.buriedGuardrails.get(id) ?? 0) + 1, at),
      updatedAt: at,
    };
    const stamp = preset === undefined ? null : { currentVersion: preset.version, customized: false, id: preset.id, version: preset.version };
    const row: Guardrail = { ...blank, keys: this.attached(blank, 'keys', input.keys), pools: this.attached(blank, 'pools', input.pools), preset: this.presetStateOf(stamp, blank.conditions) };
    this.guardrails = [...this.guardrails, row];
    // A pool or key dialog opened before the create presents a stamp the row no longer carries, as the routes
    // refuse it.
    const movedPools = new Set(row.pools.map((a) => a.id));
    const movedKeys = new Set(row.keys.map((a) => a.id));
    this.pools = this.pools.map((p) => (movedPools.has(p.id) ? { ...p, ...restamp(p) } : p));
    this.keys = this.keys.map((k) => (movedKeys.has(k.id) ? { ...k, ...restamp(k) } : k));
    return latency(this.guardrailView(row), 300);
  }

  async createKey(input: CreateKeyInput): Promise<CreatedApiKey> {
    const shape = unknownKeyMember(input, ['expiresInDays']);
    if (shape) throw new ApiError(400, shape, null);
    // A new key starts with none of its own, so once the body's shape passes, the route refuses the member before it
    // reads anything else.
    if (input.guardrails !== undefined) throw new ApiError(400, 'guardrails applies on edit', null);
    // The client sends the id it derives from the name, so a name already taken is an id already taken.
    const id = slug(input.name);
    if (this.keys.some((k) => k.id === id)) throw new ApiError(409, KEY_TAKEN, null);
    if (!this.pools.some((p) => p.id === input.poolId)) throw new ApiError(404, 'pool not found', null);
    const refusal = keyRulesRefusal(input);
    if (refusal) throw new ApiError(400, refusal, null);
    const body = this.token(40);
    const secret = `phoebis-${body}`;
    const key: Omit<ApiKey, 'guardrails' | 'poolGuardrails'> = {
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
      usage7d: { billedUsd: 0, cost: 0, equivalent: 0, requests: 0, tokens: 0 },
      lifetime: { billedUsd: 0, cost: 0, equivalent: 0, requests: 0 },
    };
    this.keys = [key, ...this.keys];
    return latency({ key: this.keyView(key), secret }, 300);
  }

  async createPool(input: PoolInput & { guardrails?: never }): Promise<Pool> {
    // A new pool starts with no guardrail, so the route refuses the member before it reads anything else.
    if (input.guardrails !== undefined) throw new ApiError(400, 'guardrails applies on edit', null);
    const id = slug(input.name);
    if (this.pools.some((p) => p.id === id)) throw new ApiError(409, POOL_TAKEN, null);
    const refusal = poolRefusal(input);
    if (refusal) throw new ApiError(400, refusal, null);
    if (input.bodyCaptureHours > this.settings.retention.bodyCaptureMaxHours) throw new ApiError(400, 'body_capture_hours exceeds stats.body_capture_max_hours', null);
    const repeated = repeatedId(input.members.map((m) => m.accountId));
    if (repeated) throw new ApiError(409, `members name account ${repeated} twice`, null);
    const members = memberRefusal(input.members);
    if (members) throw new ApiError(400, members, null);
    const held = heldElsewhere(input.members, this.pools, id);
    if (held) throw new ApiError(409, held, null);
    const pool: Omit<Pool, 'activePins' | 'guardrails' | 'keys' | 'members' | 'warnings'> & { members: Omit<PoolMember, 'activePins'>[] } = {
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

  async deleteGuardrail(id: string) {
    const row = this.guardrails.find((g) => g.id === id);
    if (!row) throw new ApiError(404, 'guardrail not found', null);
    this.buriedGuardrails.set(id, Math.floor(row.revision / INCARNATION_STAMP));
    // The attachments live on the row, so they leave with it and every other guardrail keeps its positions.
    this.guardrails = this.guardrails.filter((g) => g.id !== id);
    // A pool or key dialog opened before the delete presents a stamp the row no longer carries, as the routes
    // refuse it.
    const movedPools = new Set(row.pools.map((a) => a.id));
    const movedKeys = new Set(row.keys.map((a) => a.id));
    this.pools = this.pools.map((p) => (movedPools.has(p.id) ? { ...p, ...restamp(p) } : p));
    this.keys = this.keys.map((k) => (movedKeys.has(k.id) ? { ...k, ...restamp(k) } : k));
    return latency(undefined, 200);
  }

  async deleteKey(id: string) {
    const key = this.keys.find((k) => k.id === id);
    if (key) this.buriedKeys.set(id, Math.floor(key.revision / INCARNATION_STAMP));
    this.keys = this.keys.filter((k) => k.id !== id);
    // The key's attachments go in the delete's own transaction, since one subject column cannot cascade from two
    // tables.
    this.setSubjectGuardrails('keys', id, [], false);
    return latency(undefined, 200);
  }

  async deletePool(id: string) {
    // A key reaches only its own pool's members, so a pool cannot leave while keys still point at it.
    if (this.keys.some((k) => k.poolId === id)) return Promise.reject(new ApiError(409, 'pool still has keys', null));
    const pool = this.pools.find((p) => p.id === id);
    if (pool) this.buriedPools.set(id, Math.floor(pool.revision / INCARNATION_STAMP));
    this.pools = this.pools.filter((p) => p.id !== id);
    this.setSubjectGuardrails('pools', id, [], false);
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
    return latency(breakdownOf(ledger().rollups, query));
  }

  getAnalyticsBurn(range: Pick<AnalyticsQuery, 'from' | 'to'>, groupBy: BurnGroup, filters?: Pick<AnalyticsQuery, 'accountIds' | 'userAgentGroups'>) {
    return latency(burnOf(ledger().rollups, range, groupBy, filters));
  }

  /**
   * The counts route over the story's rows: every match in the range, the blocks' captured request bytes, and the name
   * the registry holds now, none for a guardrail deleted since it matched. The trailing day ends at the story's own
   * instant, as the dashboard's does.
   */
  getAnalyticsGuardrails(range: GuardrailRange): Promise<GuardrailCounts> {
    const from = range === '24h' ? NOW - DAY : Date.parse(range.from);
    const to = range === '24h' ? NOW : Date.parse(range.to);
    const counts = new Map<string, { blocked: number; blockedBytes: number; observed: number }>();
    for (const r of ledger().requests) {
      const at = Date.parse(r.at);
      if (at < from || at > to) continue;
      for (const m of r.guardrails) {
        const c = counts.get(m.id) ?? { blocked: 0, blockedBytes: 0, observed: 0 };
        if (m.blocked) {
          c.blocked += 1;
          c.blockedBytes += capturedBodiesFor(r)?.requestBytesSeen ?? 0;
        } else c.observed += 1;
        counts.set(m.id, c);
      }
    }
    const guardrails = [...counts]
      .sort(([a], [b]) => a.localeCompare(b))
      .map(([id, c]) => ({ ...c, id, name: this.guardrails.find((g) => g.id === id)?.name ?? null }));
    return latency({ from: iso(from), guardrails, to: iso(to) });
  }

  getAnalyticsReconciliations(filters?: Pick<AnalyticsQuery, 'accountIds'>) {
    return latency(reconciliationsOf(ledger().reconciliations, filters));
  }

  getAnalyticsRefusalCategories(query: AnalyticsQuery) {
    return latency(refusalCategoriesOf(ledger().requests, query));
  }

  getAnalyticsRewrites(range: Pick<AnalyticsQuery, 'from' | 'to'>) {
    const { accountRollups, requests } = ledger();
    return latency(rewritesOf(accountRollups, requests, range));
  }

  getAnalyticsSeries(query: AnalyticsQuery) {
    return latency(seriesOf(ledger().rollups, query));
  }

  /** The story's weekly resets, or none for a fresh installation, whose registry holds no account to end a term on. */
  getAnalyticsUnusedAllowance(span: Pick<AnalyticsQuery, 'from' | 'to'>, filters?: Pick<AnalyticsQuery, 'accountIds'>) {
    const { quotaHistory, weeklyResets } = ledger();
    return latency(unusedAllowanceOf(weeklyResets, this.registry().accounts, quotaHistory, span, filters));
  }

  /**
   * The story's conditions, or none for a fresh installation, the parked records' items gone once a replay landed them.
   */
  getAttention() {
    const { attention } = ledger();
    return latency(this.parkedLanded ? attention.filter((i) => i.kind !== 'ledger_record_parked') : attention);
  }

  async getCapturedBodies(requestId: string) {
    const r = requestById.get(requestId);
    return latency(r ? capturedBodiesFor(r) : null);
  }

  getGuardrail(id: string): Promise<Guardrail> {
    const row = this.registry().guardrails.find((g) => g.id === id);
    if (!row) return Promise.reject(new ApiError(404, 'guardrail not found', null));
    return latency(this.guardrailView(row));
  }

  getKeepaliveOverview(q: { range?: OverviewRange } = {}) {
    if (q.range !== undefined && q.range !== '7d' && q.range !== '30d') return Promise.reject(new ApiError(400, 'range is 7d or 30d', null));
    const { keepalive, keepalive30d } = ledger();
    return latency(q.range === '30d' ? keepalive30d : keepalive);
  }

  /** Every live model-scoped cooldown across the accounts, folded to the shape the key models route answers. */
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

  getOverview() {
    // The route answers its 24 hourly buckets over an empty ledger too, at zero, so each spark draws a flat line.
    const last24h = ledger().dayRows;
    const hourlyMap = new Map<string, Overview['hourly'][number]>();
    for (let i = 23; i >= 0; i -= 1) {
      const startMs = startOfHour(NOW - i * HOUR);
      const at = iso(startMs);
      const { billedUsd, equivalent } = requestSidesOf(last24h.filter((r) => Date.parse(r.at) >= startMs && Date.parse(r.at) < startMs + HOUR));
      hourlyMap.set(at, { at, billed: billedUsd, cost: 0, equivalent, errors: 0, refused: 0, requests: 0, tokens: 0 });
    }
    let tokens = 0;
    let cached = 0;
    let cost = 0;
    const errorCodes = new Map<string, number>();
    const refusalCodes = new Map<string, number>();
    const rated = last24h.filter((r) => !r.refused);
    for (const r of last24h) {
      const at = iso(startOfHour(Date.parse(r.at)));
      const bucket = hourlyMap.get(at);
      // The ledger's fold books a refused request under its refusals map and nothing else.
      if (r.refused) {
        if (r.errorCode) refusalCodes.set(r.errorCode, (refusalCodes.get(r.errorCode) ?? 0) + 1);
        if (bucket) bucket.refused += 1;
        continue;
      }
      const total = tokensTotal(r.usage);
      tokens += total;
      cached += r.usage.cacheRead;
      cost += r.costUsd;
      if (r.errorCode) errorCodes.set(r.errorCode, (errorCodes.get(r.errorCode) ?? 0) + 1);
      if (bucket) {
        bucket.requests += 1;
        bucket.tokens += total;
        bucket.cost += r.costUsd;
        // The ledger's rate: a request that did not succeed is an error, a canceled one answered with a 200 among them.
        if (r.outcome !== 'succeeded') bucket.errors += 1;
      }
    }
    const top = [...errorCodes.entries()].sort((a, b) => b[1] - a[1])[0];
    const topRefusal = [...refusalCodes.entries()].sort((a, b) => b[1] - a[1])[0];
    const day = requestSidesOf(last24h);
    return latency({
      // Counted off the pools, so the dashboard and the Pools view cannot disagree on the router's pins.
      activePins: this.registry().pools.reduce((n, p) => n + poolPins(p, this.keys), 0),
      billed24h: day.billedUsd,
      cost24h: cost,
      equivalent24h: day.equivalent,
      errorRate24h: rated.length ? rated.filter((r) => r.outcome !== 'succeeded').length / rated.length : null,
      hourly: [...hourlyMap.values()],
      refused24h: last24h.length - rated.length,
      requests24h: rated.length,
      tokens24h: tokens,
      tokensCached24h: cached,
      topErrorCode: top ? top[0] : null,
      topRefusalCode: topRefusal ? topRefusal[0] : null,
    });
  }

  getQuotaHistory(accountId: string) {
    return latency(ledger().quotaHistory.filter((h) => h.accountId === accountId));
  }

  getDecision(requestId: string, seq: number): Promise<DecisionRecord | null> {
    return latency(ledger().decisionOf(requestId, seq));
  }

  getRequest(id: string): Promise<RequestDetail | null> {
    const request = ledger().requestById.get(id);
    const events = REQUEST_EVENTS.get(id) ?? [];
    const rows = cutsOf(id);
    const cuts: RequestEvent[] = rows.map((e) => ({ accountId: e.accountId, at: e.at, detail: e.detail, keyId: e.keyId, kind: e.kind, model: e.model, sessionId: e.sessionId }));
    const newest = rows[rows.length - 1];
    return latency(request ? { events: cuts.length === 0 ? events : [...events, ...cuts].sort((a, b) => Date.parse(a.at) - Date.parse(b.at)), request, streamCut: newest ? decodeStreamCut({ ...newest.detail, at: Date.parse(newest.at) }) : null } : null);
  }

  getSession(): Promise<SessionInfo | null> {
    if (this.session) return latency(this.session, 60);
    return latency(seededSession(), 60);
  }

  getSettings() {
    return latency(this.settings);
  }

  listAccountRollups(from: string, to: string) {
    // As the route reads a range: from the hour holding the start up to the hour ceiling of the end, exclusive.
    const fromHour = startOfHour(Date.parse(from));
    const toCeiling = Math.ceil(Date.parse(to) / HOUR) * HOUR;
    return latency(ledger().accountRollups.filter((r) => Date.parse(r.hour) >= fromHour && Date.parse(r.hour) < toCeiling));
  }

  listAccounts() {
    this.landFirstPolls();
    return latency(this.registry().accounts.map((a) => this.withPool(a)));
  }

  listCatalog() {
    return latency(ledger().catalog);
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

  /** The presets are the binary's, so a fresh installation lists them too. */
  listGuardrailPresets() {
    return latency(PRESETS.map((p) => ({ ...p, conditions: p.conditions.map((c) => ({ ...c })) })));
  }

  listGuardrails() {
    return latency([...this.registry().guardrails].sort((a, b) => a.id.localeCompare(b.id)).map((g) => this.guardrailView(g)));
  }

  listKeepaliveRecipes() {
    return latency(ledger().recipes);
  }

  listKeys() {
    return latency(this.registry().keys.map((k) => this.keyView(unpricedKey(k))));
  }

  listPools() {
    return latency(this.registry().pools.map((p) => this.poolView(p)));
  }

  listRequests(query: RequestQuery = {}): Promise<RequestPage> {
    if (emptyStore()) return latency({ landedThrough: 0, nextCursor: null, requests: [] });
    const live = new Map(this.keys.map((k) => [k.id, Date.parse(k.createdAt)]));
    // The ledger lists a request once it has written it, so a row whose landing the mock's clock has not reached
    // stays out. The newest stamp is over the rows the query's filters admit, its `to` included, with its
    // `landedAfter` as the floor and capped at its `to` unless the query marks a fixed end, as the route reads it.
    const now = mockClock.now();
    const written = REQUESTS.filter((r) => landedAt(r) <= now);
    const since = query.landedAfter;
    const matched = written.filter((r) => matchesQuery(r, query, live) && (since === undefined || landedAt(r) > since)).sort(compareRequests);
    const newest = matched.reduce((stamp, r) => Math.max(stamp, landedAt(r)), since ?? 0);
    const landedThrough = query.to && !query.fixedEnd ? Math.min(newest, Date.parse(query.to)) : newest;
    const after = query.cursor ? matched.filter((r) => isAfterCursor(r, query.cursor!)) : matched;
    const limit = query.limit ?? 50;
    const page = after.slice(0, limit);
    const last = page[page.length - 1];
    // A cursor only rides along while rows are left, which is how the route signals the end of the list.
    return latency({ landedThrough, nextCursor: after.length > limit && last ? `${Date.parse(last.at)}:${last.id}` : null, requests: page });
  }

  listRoutingLog(query: RoutingLogQuery = {}): Promise<RoutingLogPage> {
    if (emptyStore()) return latency({ counts: query.request ? { decisions: 0, events: 0 } : null, nextCursor: null, rows: [] });
    // The route's refusals, in its order and words.
    const unknown = query.kinds?.find((k) => !ROUTING_LOG_KINDS.includes(k));
    if (unknown !== undefined) return Promise.reject(new ApiError(400, `unknown kind ${unknown}`, null));
    if (query.limit !== undefined && !(Number.isInteger(query.limit) && query.limit >= 1 && query.limit <= 200)) return Promise.reject(new ApiError(400, 'limit is 1 to 200', null));
    return latency(listRoutingLog(query, Date.now(), this.written));
  }

  listSessions() {
    if (sessionsFail()) return Promise.reject(new ApiError(503, 'store busy', null));
    return latency(sessionsListedAt(NOW, this.registry().sessions, this.settings.sessions.nonCompanionIdleSeconds));
  }

  login(secret: string, stayLoggedIn = false): Promise<SessionInfo> {
    if (secret !== 'admin') return Promise.reject(new ApiError(401, 'that secret does not match', null));
    this.session = { ...MOCK_SESSION, persistent: stayLoggedIn };
    seedSession(stayLoggedIn);
    return latency(this.session, 200);
  }

  logout() {
    this.session = null;
    seedSession(null);
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
    const parked = this.parkedLanded ? [] : ledger().attention.filter((i) => i.kind === 'ledger_record_parked');
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
    const next: Omit<ApiKey, 'guardrails' | 'poolGuardrails'> = { ...key, prefix: secret.slice(0, 16), retiredSecretLastUsedAt: null, sessionsOnRetiredSecret: 2, ...restamp(key) };
    this.keys = this.keys.map((k) => (k.id === id ? next : k));
    return latency({ key: this.keyView(next), secret }, 300);
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

  /**
   * The dry run over the story's captured rows of the pool or the key, newest first, no further back than the longest
   * capture window and never a probe, each verdict as `dryRunVerdict` reads it. The route's refusals come in its order.
   */
  async testGuardrail(input: GuardrailTestInput): Promise<GuardrailTestResult> {
    if (input.guardrail !== undefined && input.conditions !== undefined) throw new ApiError(400, 'guardrail and conditions are exclusive', null);
    if (input.guardrail === undefined && input.conditions === undefined) throw new ApiError(400, 'guardrail or conditions is required', null);
    if (input.poolId !== undefined && input.keyId !== undefined) throw new ApiError(400, 'pool_id and key_id are exclusive', null);
    if (input.poolId === undefined && input.keyId === undefined) throw new ApiError(400, 'pool_id or key_id is required', null);
    const limit = input.limit ?? 200;
    if (!(Number.isInteger(limit) && limit >= 1 && limit <= 500)) throw new ApiError(400, 'limit is 1 to 500', null);
    const windowHours = this.settings.retention.bodyCaptureMaxHours;
    const sample = { keyId: input.keyId ?? null, limit, poolId: input.poolId ?? null, windowHours };
    if (emptyStore()) return latency({ requests: [], sample, totals: { indeterminate: 0, matched: 0, sampled: 0, unmatched: 0 } });
    let conditions = input.conditions ?? [];
    if (input.guardrail !== undefined) {
      const saved = this.guardrails.find((g) => g.id === input.guardrail);
      if (!saved) throw new ApiError(404, 'no such guardrail', null);
      conditions = saved.conditions;
    } else {
      const refusal = conditionsRefusal(input.conditions);
      if (refusal !== null) throw new ApiError(400, refusal, null);
    }
    if (input.poolId !== undefined && !this.pools.some((p) => p.id === input.poolId)) throw new ApiError(404, 'no such pool', null);
    if (input.keyId !== undefined && !this.keys.some((k) => k.id === input.keyId)) throw new ApiError(404, 'no such key', null);
    const since = NOW - windowHours * HOUR;
    const rows = REQUESTS.filter((r) => r.kind !== 'probe' && r.capturedBodies && Date.parse(r.at) >= since && (input.poolId !== undefined ? r.poolId === input.poolId : r.keyId === input.keyId))
      .sort(compareRequests)
      .slice(0, limit);
    const requests = rows.map((r) => ({ ...dryRunVerdict(conditions, r, GUARDRAIL_STORY_IDS.has(r.id)), id: r.id, receivedAt: r.at }));
    const count = (verdict: string) => requests.filter((r) => r.verdict === verdict).length;
    return latency({ requests, sample, totals: { indeterminate: count('indeterminate'), matched: count('matched'), sampled: requests.length, unmatched: count('unmatched') } }, 300);
  }

  /** The account route's refusals in its order and words; the end is kept only while the flag stands. */
  async updateAccount(accountId: string, patch: AccountPatch, revision?: number): Promise<Account> {
    const a = this.account(accountId);
    if (patch.canceled && a.kind === 'api_key') throw new ApiError(400, 'only an oauth account is canceled', null);
    if (!patch.canceled && patch.subscriptionEndsAt !== null) throw new ApiError(400, 'subscription_ends_at needs canceled', null);
    if (patch.canceled && patch.subscriptionEndsAt === null) throw new ApiError(400, 'canceled needs subscription_ends_at', null);
    if (revision !== undefined && revision !== a.revision) {
      return Promise.reject(new ApiError(409, 'the account changed while the edit was being saved', 'stale_edit'));
    }
    // The route answers a subscription block for a canceled OAuth row even before it pairs, so its end has a place.
    const blank: Subscription = { billingType: '', endsAt: null, organizationType: '', plan: null, planDisplayName: null, rateLimitTier: '', since: null, trialEndsAt: null };
    const base = a.subscription ?? (patch.canceled ? blank : null);
    const subscription = base ? { ...base, endsAt: patch.canceled ? patch.subscriptionEndsAt : null } : null;
    return latency(this.withPool(this.putAccount(revised({ ...a, canceled: patch.canceled, subscription }))), 250);
  }

  /**
   * The edit route's checks in its order. An edit stamps the guardrail and every pool and key it attaches or detaches,
   * so a dialog of either side opened before it is refused as stale.
   */
  async updateGuardrail(id: string, input: Partial<GuardrailInput> & { preset?: never }, revision?: number): Promise<Guardrail> {
    const current = this.guardrails.find((g) => g.id === id);
    if (!current) throw new ApiError(404, 'guardrail not found', null);
    if (input.preset !== undefined) throw new ApiError(400, 'preset applies on create', null);
    let stamp = current.preset;
    if (input.presetVersion !== undefined) {
      if (stamp === null) throw new ApiError(400, 'guardrail has no preset', null);
      const presetId = stamp.id;
      if (!PRESETS.some((p) => p.id === presetId && p.version === input.presetVersion)) throw new ApiError(400, `preset_version is not a shipped version of ${presetId}`, null);
      stamp = { ...stamp, version: input.presetVersion };
    }
    const refusal = this.guardrailFieldRefusal(input) ?? (input.conditions === undefined ? null : conditionsRefusal(input.conditions)) ?? subjectsTwice(input);
    if (refusal !== null) throw new ApiError(400, refusal, null);
    if (revision !== undefined && revision !== current.revision) throw new ApiError(409, GUARDRAIL_STALE, 'stale_edit');
    const attach = input.pools === undefined && input.keys === undefined ? null : this.sideBoundRefusal(id, input.pools, input.keys);
    if (attach !== null) throw new ApiError(400, attach, null);
    const missing = this.missingSubject(input.pools, input.keys);
    if (missing !== null) throw new ApiError(404, missing, null);
    const conditions = input.conditions ?? current.conditions;
    const next: Guardrail = {
      ...current,
      conditions: conditions.map((c) => ({ name: c.name, operator: c.operator, path: c.path, selector: c.selector, subject: c.subject, value: c.value })),
      description: input.description ?? current.description,
      enabled: input.enabled ?? current.enabled,
      keys: this.attached(current, 'keys', input.keys),
      mode: input.mode ?? current.mode,
      name: input.name ?? current.name,
      pools: this.attached(current, 'pools', input.pools),
      preset: this.presetStateOf(stamp, conditions),
      ...restamp(current),
    };
    this.guardrails = this.guardrails.map((g) => (g.id === id ? next : g));
    const changed = (kind: 'keys' | 'pools') => {
      const before = new Set(current[kind].map((a) => a.id));
      const after = new Set(next[kind].map((a) => a.id));
      return new Set([...before, ...after].filter((s) => before.has(s) !== after.has(s)));
    };
    const movedPools = changed('pools');
    const movedKeys = changed('keys');
    this.pools = this.pools.map((p) => (movedPools.has(p.id) ? { ...p, ...restamp(p) } : p));
    this.keys = this.keys.map((k) => (movedKeys.has(k.id) ? { ...k, ...restamp(k) } : k));
    return latency(this.guardrailView(next), 250);
  }

  /**
   * The refusals in the route's order: the unknown member, then after the mock's key lookup the pool 404, the rules
   * refusal, the doubled guardrail, the stale stamp, the bound refusal, the unknown guardrail.
   */
  async updateKey(id: string, input: KeyInput, revision?: number): Promise<ApiKey> {
    const shape = unknownKeyMember(input);
    if (shape) return Promise.reject(new ApiError(400, shape, null));
    const idx = this.keys.findIndex((k) => k.id === id);
    if (idx < 0) return Promise.reject(new Error(`Unknown key ${id}`));
    const { expiresAt: expiry, guardrails: ownIds, ...rest } = input;
    const current = this.keys[idx];
    // The route reads the pool before the rule list and the guardrail list, and before it compares the edit's stamp, so
    // an edit onto a missing pool answers 404 ahead of their refusals.
    if (!this.pools.some((p) => p.id === rest.poolId)) return Promise.reject(new ApiError(404, 'pool not found', null));
    const refusal = keyRulesRefusal(input);
    if (refusal) return Promise.reject(new ApiError(400, refusal, null));
    const twice = repeatedId(ownIds);
    if (twice !== null) return Promise.reject(new ApiError(400, `guardrails lists ${twice} twice`, null));
    if (revision !== undefined && revision !== current.revision) return Promise.reject(new ApiError(409, 'the key changed while it was being edited, so reload and try again', 'stale_edit'));
    // The pool the key lands in, a new one on a move, with the key's own list as the edit leaves it.
    const own = ownIds ?? this.guardrailRefs('keys', id).map((r) => r.id);
    const bound = boundRefusal(rest.poolId, this.guardrailRefs('pools', rest.poolId).map((r) => r.id), [[id, own]]);
    if (bound !== null) return Promise.reject(new ApiError(400, bound, null));
    const unknown = (ownIds ?? []).find((g) => !this.guardrails.some((x) => x.id === g));
    if (unknown !== undefined) return Promise.reject(new ApiError(404, `guardrail ${unknown} not found`, null));
    const expiresAt = expiry === undefined ? current.expiresAt : expiry;
    // The status is the decoder's fold of the stored flags, so a key switched off or lapsed reads that way at once.
    const status = keyStatusOf(current.status === 'revoked', expiresAt, rest.enabled, Date.now());
    const next: Omit<ApiKey, 'guardrails' | 'poolGuardrails'> = { ...current, ...rest, expiresAt, status, ...restamp(current) };
    this.keys = this.keys.map((k, i) => (i === idx ? next : k));
    if (ownIds !== undefined) this.setSubjectGuardrails('keys', id, ownIds);
    return latency(this.keyView(next), 250);
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
    const repeated = repeatedId(input.members.map((m) => m.accountId));
    if (repeated) return Promise.reject(new ApiError(409, `members name account ${repeated} twice`, null));
    const members = memberRefusal(input.members);
    if (members) return Promise.reject(new ApiError(400, members, null));
    const { guardrails: guardrailIds, ...fields } = input;
    const twice = repeatedId(guardrailIds);
    if (twice !== null) return Promise.reject(new ApiError(400, `guardrails lists ${twice} twice`, null));
    // In the route's order: the stamp after the list's own shape and ahead of the member lookups, so a stale edit that
    // names an unknown or a held member answers `stale_edit`.
    const current = this.pools[idx];
    if (revision !== undefined && revision !== current.revision) return Promise.reject(new ApiError(409, 'the pool changed while it was being edited, so reload and try again', 'stale_edit'));
    // The store counts the guardrail list against every key of the pool first in its transaction, then writes the
    // members and looks each guardrail up last.
    if (guardrailIds !== undefined) {
      const keyLists = this.keys
        .filter((k) => k.poolId === id)
        .sort((a, b) => a.id.localeCompare(b.id))
        .map((k): [string, string[]] => [k.id, this.guardrailRefs('keys', k.id).map((r) => r.id)]);
      const bound = boundRefusal(id, guardrailIds, keyLists);
      if (bound !== null) return Promise.reject(new ApiError(400, bound, null));
    }
    const unknown = input.members.find((m) => !this.accounts.some((a) => a.id === m.accountId));
    if (unknown) return Promise.reject(new ApiError(404, `account ${unknown.accountId} not found`, null));
    const held = heldElsewhere(input.members, this.pools, id);
    if (held) return Promise.reject(new ApiError(409, held, null));
    const missing = guardrailIds?.find((g) => !this.guardrails.some((x) => x.id === g));
    if (missing !== undefined) return Promise.reject(new ApiError(404, `guardrail ${missing} not found`, null));
    const verdict = new Map(current.members.map((m) => [m.accountId, m.eligible]));
    const next: Omit<Pool, 'activePins' | 'guardrails' | 'keys' | 'members' | 'warnings'> & { members: Omit<PoolMember, 'activePins'>[] } = {
      ...current,
      ...fields,
      // From the saved members' kinds, as the router composes them on every save.
      keepaliveResolved: resolvedFor(composedFor(input.members.map((m) => this.accounts.find((a) => a.id === m.accountId)?.kind ?? 'oauth'), input.cacheTtl), input.keepalive),
      // Eligibility is the router's reading of live cooldowns and overages, not something a save can set. A save
      // does rewrite the budget, and a write answers no spend here, so a budget verdict is read again.
      members: input.members.map((m) => {
        const account = this.accounts.find((a) => a.id === m.accountId);
        const kept = verdict.get(m.accountId) ?? 'eligible';
        return writtenMember(m, account, input.slowDown, (member) => (kept === 'budget' ? eligibilityOf(account, input.overagePolicy, member) : kept));
      }),
      ...restamp(current),
    };
    this.pools = this.pools.map((p, i) => (i === idx ? next : p));
    if (guardrailIds !== undefined) this.setSubjectGuardrails('pools', id, guardrailIds);
    return latency(this.poolView(next), 250);
  }

  async verifyAccount(accountId: string) {
    this.account(accountId);
    return latency({ ok: true, status: 200 }, 400);
  }
}
