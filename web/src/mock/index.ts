import type { AccountPatch, Api, CreateGuardrailInput, CreateKeyInput, CreatePoolInput, KeyInput, PoolInput } from '@/api/client';
import type { GuardrailInput, GuardrailPatch, GuardrailRange, GuardrailTestInput, OverviewRange } from '@/api/client';
import { decodeLimitResets, decodeResetUse, decodeSettings, keyStatusOf, perFeature } from '@/api/decode';
import { decodeStreamCut } from '@/api/decode';
import { clientLeft } from '@/api/fold';
import { ApiError, catalogUnion, noteBuild, slug } from '@/api/http';
import type {
  Account, AccountCatalog, AccountInput, AccountRollup, AnalyticsQuery, ApiKey, BurnGroup, CatalogModel, CreatedApiKey, DecisionRecord, FeatureAccountIds, FeatureCall, FeatureCallPage, FeatureCallQuery, FeatureWord, KeepaliveOverview, KeyFeatureAccount, KeyModels, LimitResetsRead, MemberEligibility,
  ModelRule, Overview, PairingConfirmed, PairingInput, PairingPreview, PairingSession, PairingStart, Pool, PoolMember, RateLimitWindow, RequestDetail, RequestEvent, RequestLog, RequestOrder, RequestPage, RequestQuery, ResetUse, RoutingEvent, RoutingLogPage, RoutingLogQuery, RuleCooldown, Session, SessionInfo, Settings,
  Subscription,
} from '@/api/types';
import type { Guardrail, GuardrailAttachment, GuardrailCounts, GuardrailRef, GuardrailTestResult } from '@/api/types';
import { AFFINITY_MODE_VALUES, COMPANION_PERMISSION_VALUES, FEATURE_WORD_VALUES, GUARDRAIL_MODE_VALUES, REQUEST_ORDER_VALUES, RETURN_TO_PLAN_VALUES, SESSION_PLACEMENT_VALUES, SUBAGENT_PLACEMENT_VALUES, WARM_SESSIONS_VALUES } from '@/api/types';
import { keyFeaturesNaming, poolFeaturesNaming } from '@/lib/accountUses';
import { type Clock, wallClock } from '@/lib/clock';
import { has, lookup } from '@/lib/counts';
import { foldClassifier, foldEffort, foldRequestClass, foldSpeed, servedMissReason, servingResponse } from '@/lib/dimensions';
import { keyLifetime, tokensTotal } from '@/lib/format';
import { modelRulesRefusal, poolWarnings, type NumericKeepaliveKey } from '@/lib/forms';
import { matchesPattern } from '@/lib/pattern';
import { matchesPattern as matchesModelPattern, processRules } from '@/lib/patterns';
import { MODEL_SCOPE_PREFIX, accountStatusOf, flagOf, readWindows, withholdsWindows } from '@/lib/quota';
import { compareOrderKeys, compareRequests, orderKeyOf, requestOrder, type OrderKey } from '@/lib/requests';
import { movedFrom } from '@/lib/routingLog';
import { WINDOW_HOURS, WINDOW_KEY } from '@/lib/windows';
import { ACCOUNTS, NO_CREDENTIAL, QUOTA_HISTORY, accountById, blankAccountRow, cleanPollAt, featureGrantsOf, lastReadingOf, usageOf } from './accounts';
import { RECONCILIATIONS, breakdownOf, burnOf, placementsOf, reconciliationsOf, refusalCategoriesOf, rewritesOf, seriesOf, unusedAllowanceOf, wholeHoursOf } from './analytics';
import { ATTENTION, byKindThenSince, featureAccountUnavailableOf, pairingExpiringOf } from './attention';
import { CATALOG } from './catalog';
import { GUARDRAIL_RESERVED_IDS, GUARDRAIL_STALE, GUARDRAIL_TAKEN, GUARDRAILS, PRESETS, boundRefusal, conditionsRefusal, dryRunVerdict, repeatedId, sameConditions, withAttachments } from './guardrails';
import { KEEPALIVE_OVERVIEW, KEEPALIVE_OVERVIEW_30D, RECIPES, SESSIONS, SETTINGS_WIRE, largestCohort, memberPins, pinsUnderPool, poolPinsAt, rowsOfDeletedKey, sessionsListedAt } from './operations';
import { KEYS, POOLS, UNTIL_DRAWN, budgetUntilOf, composedFor, eligibilityOf, resolvedFor, servedFigures, type KeyRow, type PoolRow } from './pools';
import { ACCOUNT_ROLLUPS, ALL_ROLLUPS, DAY_ROWS, FEATURE_CALLS, REQUEST_EVENTS, REQUESTS, capturedBodiesFor, cooldownSecondsByHour, landedAt, requestById, requestSidesOf } from './requests';
import { GUARDRAIL_STORY_IDS } from './requests';
import { RESET_ANSWERS, RESET_BLOCKS, foldAnswer, grantStates, limitResetsWire, resetsRevision, type MockBlock } from './resets';
import { createRng } from './rng';
import { round } from './round';
import { ROUTING_LOG_KINDS, WINDOW_DROP_MARGIN, claimDetail, cursorInteger, dropDetail, getDecision, inList, listRoutingLog, routingCursorOf, subscriptionPair, wholePoints } from './routingLog';
import { ROUTING_EVENTS, SUMMARIES } from './routingLog';
import { ACCOUNT_ROLLUP_SAMPLE, INHERITED_FEATURE_ACCOUNTS, KEEPALIVE_OVERVIEW_SAMPLE, KEY_SAMPLE, NO_FEATURE_ACCOUNTS, PAIRING_SAMPLE, POOL_SAMPLE } from './samples';
import { DAY, HOUR, MINUTE, NOW, ago, ahead, hourCeil, iso, startOfHour } from './time';

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
const ACCOUNT_STALE = 'the account changed while it was being edited, so reload and try again';
const ACCOUNT_TAKEN = 'an account with this id exists';
const KEY_STALE = 'the key changed while it was being edited, so reload and try again';
const KEY_TAKEN = 'a key with this id exists';
const PAIRING_NOT_FOUND = 'pairing session not found';
const POLL_UNREAD = 'the poll has not read this account yet';
const POOL_STALE = 'the pool changed while it was being edited, so reload and try again';
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
const incarnationOf = (a: Account): number => Math.floor(a.revision / INCARNATION);

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
const KEY_INPUT_MEMBERS: ReadonlySet<string> = new Set(['autoModeServer', 'bodyCaptureHours', 'companionPermissions', 'enabled', 'expiresAt', 'featureAccounts', 'guardrails', 'longContextTwins', 'modelRules', 'modelRulesMode', 'name', 'notes', 'poolId']);
const unknownKeyMember = (input: object): string | null => (Object.keys(input).some((k) => !KEY_INPUT_MEMBERS.has(k)) ? 'body must be a JSON object' : null);

/**
 * The ceilings', shedding, failover, slow-down and return-to-plan refusals, in the order and words of the pool routes.
 */
const engineRefusal = (input: PoolInput): string | null => {
  const whole = (v: number, min: number, max: number) => Number.isInteger(v) && v >= min && v <= max;
  if (!(input.fiveHourCeiling >= 0.5 && input.fiveHourCeiling <= 1)) return 'five_hour_ceiling is between 0.5 and 1.0';
  if (!(input.weeklyCeiling >= 0.5 && input.weeklyCeiling <= 1)) return 'weekly_ceiling is between 0.5 and 1.0';
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

/** The accounts a key's choices name, in the shape the routes' check of a selection reads. */
const namedFeatureAccounts = (words: Record<FeatureWord, KeyFeatureAccount>): FeatureAccountIds => perFeature((f) => words[f].accountId);

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
 * The account hours the route answers. The drawn hours stand for the ledger's folds on an account that holds a window
 * reading. An API-key account, a deleted one and a subscription row with no window reading take the folds of the
 * routing log's events instead, each booked to its hour as the ledger books it: a pin created, a pin moved in on its
 * destination and out on the account it left, and the time under the account's cooldowns. An ended plan's samples
 * already stand in its hours, so its folds join those rows.
 */
function accountHours(): AccountRollup[] {
  const folds = (id: string | null): id is string => {
    if (id === null) return false;
    const a = accountById.get(id);
    return a === undefined || a.kind !== 'oauth' || readWindows(lastReadingOf(a)).length === 0;
  };
  const hours = new Map(ACCOUNT_ROLLUPS.map((r) => [`${r.accountId}|${Date.parse(r.hour)}`, r]));
  const fold = (accountId: string | null, at: number, d: { cooldownSeconds?: number; created?: number; migratedIn?: number; migratedOut?: number }) => {
    if (!folds(accountId)) return;
    const hour = startOfHour(at);
    const key = `${accountId}|${hour}`;
    const row = hours.get(key) ?? { ...ACCOUNT_ROLLUP_SAMPLE, accountId, cooldownSeconds: 0, hour: iso(hour), overageSeconds: 0, peak: { '5h': null, '7d': null, '7d_fable': null }, pins: { created: 0, migratedIn: 0, migratedOut: 0 }, windows: {} };
    const pins = { created: row.pins.created + (d.created ?? 0), migratedIn: row.pins.migratedIn + (d.migratedIn ?? 0), migratedOut: row.pins.migratedOut + (d.migratedOut ?? 0) };
    hours.set(key, { ...row, cooldownSeconds: row.cooldownSeconds + (d.cooldownSeconds ?? 0), pins });
  };
  const spans = new Map<string, { setAt: number; until: number }[]>();
  for (const e of ROUTING_EVENTS) {
    const at = Date.parse(e.at);
    if (e.kind === 'pin_created') fold(e.accountId, at, { created: 1 });
    if (e.kind === 'pin_migrated') {
      fold(e.accountId, at, { migratedIn: 1 });
      fold(movedFrom(e), at, { migratedOut: 1 });
    }
    if (e.kind === 'cooldown_set' && folds(e.accountId) && typeof e.detail.until === 'number') spans.set(e.accountId, [...(spans.get(e.accountId) ?? []), { setAt: at, until: e.detail.until }]);
  }
  // The ledger stores no account hour that holds nothing.
  for (const [accountId, rows] of spans) for (const [hour, seconds] of cooldownSecondsByHour(rows)) if (seconds > 0) fold(accountId, hour, { cooldownSeconds: seconds });
  return [...hours.values()];
}
const ACCOUNT_HOURS = accountHours();

const NO_REQUESTS = new Map<string, RequestLog>();
/**
 * The ledger's read sets as the routes fold them: the story's, and an empty form of each for a fresh installation, so
 * a route that reads its rows here answers the empty store's switch with no test of its own.
 */
const ledger = () => {
  const empty = emptyStore();
  return {
    accountHours: empty ? [] : ACCOUNT_HOURS,
    accountRollups: empty ? [] : ACCOUNT_ROLLUPS,
    attention: empty ? [] : ATTENTION,
    catalog: empty ? [] : CATALOG,
    dayRows: empty ? [] : DAY_ROWS,
    decisionOf: (requestId: string, seq: number): DecisionRecord | null => (empty ? null : getDecision(requestId, seq)),
    featureCalls: empty ? [] : FEATURE_CALLS,
    keepalive: empty ? EMPTY_KEEPALIVE_OVERVIEW : KEEPALIVE_OVERVIEW,
    keepalive30d: empty ? EMPTY_KEEPALIVE_OVERVIEW : KEEPALIVE_OVERVIEW_30D,
    quotaHistory: empty ? [] : QUOTA_HISTORY,
    reconciliations: empty ? [] : RECONCILIATIONS,
    recipes: empty ? [] : RECIPES,
    requestById: empty ? NO_REQUESTS : requestById,
    requests: empty ? [] : REQUESTS,
    rollups: empty ? [] : ALL_ROLLUPS,
    routingEvents: empty ? [] : ROUTING_EVENTS,
    summaries: empty ? [] : SUMMARIES,
  };
};
/**
 * The rows the ledger has written by `now`: it lists a request once it has written it, so a row whose landing the
 * clock has not reached stays out of the request log and of the sessions' folds alike.
 */
const writtenBy = (now: number): RequestLog[] => ledger().requests.filter((r) => landedAt(r) <= now);
/**
 * The request's `stream_interrupted` rows, which the detail lists as its events and lifts its cut off, the newest
 * one's, as the route does.
 */
const cutsOf = (requestId: string) => ROUTING_EVENTS.filter((e) => e.kind === 'stream_interrupted' && e.requestId === requestId);
/** The kinds of the events a pin writes, which a request's detail reads off the routing log. */
const PIN_EVENT_KINDS: readonly string[] = ['pin_created', 'pin_expired', 'pin_migrated', 'pin_released'];

/** The keepalive overview of a router with no session and no probe yet. */
const EMPTY_KEEPALIVE_OVERVIEW: KeepaliveOverview = {
  busGaps: 0,
  expiredBetweenTurns24h: 0,
  lastBusGapAt: null,
  // The captured overview carries the bootstrap file's defaults.
  limits: { agents: { ...KEEPALIVE_OVERVIEW_SAMPLE.limits.agents, count: 0 }, retainedBytes: { ...KEEPALIVE_OVERVIEW_SAMPLE.limits.retainedBytes, count: 0 }, sessions: { ...KEEPALIVE_OVERVIEW_SAMPLE.limits.sessions, count: 0 } },
  pools: [],
  probes24h: { failed: 0, held: 0, ok: 0, skipped: 0 },
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
const statusOf = (a: Account, enabled: boolean, flag = flagOf(a.status)): Account['status'] => accountStatusOf(flag, enabled, a.cooldowns);

/**
 * Whether the stored document reports a plan, as the router's `reports_no_plan` reads a profile: every organization
 * type but the free one, so a row with no document reports one too.
 */
const reportsPlan = (a: Pick<Account, 'subscription'>): boolean => a.subscription?.organizationType !== 'claude_free';

/**
 * Whether the router holds a reading of the row, which the account view answers from while it holds no usage entry: a
 * window the row reads, or a sample the story's poll wrote, since the router keeps an account's last reading past a
 * disconnect and past an ended plan.
 */
const holdsReading = (a: Account): boolean => readWindows(a).length > 0 || ledger().quotaHistory.some((h) => h.accountId === a.id && h.samples.length > 0);

/**
 * A written member as the route answers it: the kind, the burn, the Pace score, the speed and the fit read off the
 * account under the saved pool's settings, and no spend, since the mock keeps no ledger to price one from. A member sent
 * without a budget field has no budget over that period.
 */
const writtenMember = (m: PoolInput['members'][number], account: Account | undefined, pool: Pick<PoolInput, 'fiveHourCeiling' | 'slowDown' | 'weeklyCeiling' | 'windDownMinutes'>, eligible: (member: Omit<PoolMember, 'activePins' | 'eligible'>) => MemberEligibility): Omit<PoolMember, 'activePins'> => {
  const member = { accountId: m.accountId, budgetDailyUsd: m.budgetDailyUsd ?? null, budgetMonthlyUsd: m.budgetMonthlyUsd ?? null, budgetUntil: null, kind: account?.kind ?? 'oauth', priority: m.priority, spends: [], ...servedFigures(m.accountId, Date.now(), pool) };
  return { ...member, eligible: eligible(member) };
};

const keyIdsOf = (keys: readonly Pick<ApiKey, 'id' | 'poolId'>[], poolId: string): Set<string> => new Set(keys.filter((k) => k.poolId === poolId).map((k) => k.id));

/**
 * A pool's pins are every pin under the keys it holds at the read, so a pool created again under a deleted one's id
 * reads none of the old keys' pins. A pin on a member that left the pool counts here and in no member's cell.
 */
const poolPins = (p: Pick<Pool, 'id'>, keys: readonly Pick<ApiKey, 'id' | 'poolId'>[]): number => pinsUnderPool(keyIdsOf(keys, p.id));

/**
 * The route's `keys=` rule: `<id>@<generation>` names one lifetime and a bare id the live key's, so an earlier
 * holder's rows answer only to their own selector, and the reserved id `-` reaches the requests that hold no key at
 * any generation.
 */
const selectsKey = (selector: string, r: RequestLog, live: Map<string, number>) =>
  selector === '-' ? r.keyId === null : r.keyId !== null && (selector.includes('@') ? keyLifetime(r.keyId, r.keyGeneration) === selector : r.keyId === selector && live.get(selector) === r.keyGeneration);

function matchesQuery(r: RequestLog, q: RequestQuery, live: Map<string, number>): boolean {
  // A request the router never attributed to a key matches the reserved id alone.
  if (q.keyIds?.length && !q.keyIds.some((k) => selectsKey(k, r, live))) return false;
  // The route reaches a request filed under no pool through the `-` selector, as the rollups file it.
  if (q.poolIds?.length && !q.poolIds.includes(r.poolId ?? '-')) return false;
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
  if (q.fallback !== undefined && (r.fallbackDigest !== null) !== q.fallback) return false;
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
  // The ledger reads the serving answer's own keys through `json_each`, so a null map and an empty one match no key,
  // and an inherited name such as `constructor` matches none either.
  if (q.inputTransformations?.length) {
    const dropped = serving?.inputTransformations ?? {};
    if (!q.inputTransformations.some((key) => has(dropped, key))) return false;
  }
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

/** The route's cursor under a key: `<figure>:<id>`, the figure empty where the row carries none. */
const cursorText = (k: OrderKey): string => `${k.figure ?? ''}:${k.id}`;
const keyedPage = <R>(sorted: R[], cursor: OrderKey | undefined, limit: number, key: (r: R) => OrderKey, compare: (a: OrderKey, b: OrderKey) => number): { nextCursor: string | null; page: R[] } => {
  // The route resumes strictly past the cursor's row in the list's order, so rows sharing a figure are neither
  // repeated nor skipped.
  const after = cursor === undefined ? sorted : sorted.filter((r) => compare(key(r), cursor) > 0);
  const page = after.slice(0, limit);
  const last = page[page.length - 1];
  // A cursor only rides along while rows are left, which is how the route signals the end of the list.
  return { nextCursor: after.length > limit && last ? cursorText(key(last)) : null, page };
};
/**
 * The keys a cursor names, or null for a cursor the route refuses: no separator, a figure its integer parse does not
 * read, or an empty figure under `received_at` or `attempts`, whose rows all carry one.
 */
const cursorKeyOf = (cursor: string, order: RequestOrder): OrderKey | null => {
  const sep = cursor.indexOf(':');
  if (sep < 0) return null;
  const value = cursor.slice(0, sep);
  const id = cursor.slice(sep + 1);
  if (value === '') return order === 'received_at' || order === 'attempts' ? null : { figure: null, id };
  const figure = cursorInteger(value);
  return figure === null ? null : { figure, id };
};

/** The pair the route orders the feature call list by and its cursor names: the receipt, then the id. */
const featureCallKey = (c: FeatureCall): OrderKey => ({ figure: Date.parse(c.receivedAt), id: c.id });

/** As the route reads its query: an absent or empty list narrows nothing, and each end of the range holds its instant. */
const matchesFeatureCall = (c: FeatureCall, q: FeatureCallQuery): boolean =>
  inList(q.accounts, c.accountId) &&
  inList(q.features, c.feature) &&
  inList(q.keys, c.keyId) &&
  inList(q.outcomes, c.outcome) &&
  (q.from === undefined || Date.parse(c.receivedAt) >= q.from) &&
  (q.to === undefined || Date.parse(c.receivedAt) <= q.to);

/** A window as the poll after a reset reads it: `by` taken off its use, and its lead and projection with it. A window
 *  with no share holds no use to take off, so it reads as it stood. */
const lowered = (w: RateLimitWindow, by: number, at: number): RateLimitWindow => {
  if (w.utilization === null) return w;
  const drop = Math.min(by, w.utilization);
  const less = (v: number | null) => (v === null ? null : round(v - drop, 3));
  return { ...w, lead: less(w.lead), observedAt: iso(at), projection: less(w.projection), utilization: round(w.utilization - drop, 3) };
};

/**
 * An account's readings after a reset, as the poll that follows the claim reads them: each window the grant clears
 * empty, and where the grant gives the Fable week back without clearing the 7-day window, that window less the week's
 * share of it, as the one measured use read.
 */
function readingsAfterReset(a: Account, clears: readonly string[], kind: string, at: number): Pick<Account, 'fableWindow' | 'windows'> {
  const share = kind === 'fable' && !clears.includes('7d') ? (a.limitResets?.fableWeekShare ?? null) : null;
  const read = (w: RateLimitWindow) => (clears.includes(w.id) ? lowered(w, 1, at) : w.id === '7d' && share !== null ? lowered(w, share, at) : w);
  return { fableWindow: a.fableWindow && read(a.fableWindow), windows: a.windows.map(read) };
}

/**
 * The drops the poll after a use reads, as the router writes them: one per weekly window whose figure fell by more than
 * the margin under its unchanged reset, from the stored reading's figure to the poll's, with the stored pair.
 */
function dropsAfterReset(a: Account, after: Pick<Account, 'fableWindow' | 'windows'>): Record<string, unknown>[] {
  const pair = subscriptionPair(a.subscription);
  const read = new Map(readWindows(after).map((w) => [w.id, w.utilization]));
  return readWindows(a).flatMap((w) => {
    const next = read.get(w.id) ?? null;
    if (w.periodHours !== WINDOW_HOURS['7d'] || w.resetsAt === null || w.utilization === null || next === null) return [];
    return wholePoints(w.utilization) - wholePoints(next) > wholePoints(WINDOW_DROP_MARGIN) ? [dropDetail(WINDOW_KEY[w.id] ?? w.id, w.utilization, next, Date.parse(w.resetsAt), pair)] : [];
  });
}

type StoredPool = PoolRow<Omit<PoolMember, 'activePins'>>;

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
  /** The request ids the claims send, drawn apart from the secrets so a use moves no token a later write draws. */
  private claimIds = createRng(7_919);
  /** The instant each confirm started its account's first poll, held until a list read lands that reading. */
  private firstPolls = new Map<string, number>();
  // The attachments live on the guardrails alone, as one table holds them in the registry, and every pool and key view
  // reads its lists from here.
  private guardrails: Guardrail[] = GUARDRAILS.map((g) => ({ ...g, revision: stampToken(1, g.updatedAt) }));
  // The live desktop key was issued under the id its deleted earlier holder had, so it is that id's second; every other
  // key of the story is its id's first.
  private keys: KeyRow[] = KEYS.map((k) => ({ ...k, revision: stampToken((this.buriedKeys.get(k.id) ?? 0) + 1, k.updatedAt) }));
  /** `label` is set on a pairing that creates its row at the confirm. The row's id is `accountId` either way. */
  private pairings = new Map<string, { accountId: string; label?: string; state: string }>();
  /** Set by the first replay, which lands the story's parked records. */
  private parkedLanded = false;
  // The route reads every member's kind and verdict afresh on each answer; the story's are read once, at load.
  private pools: StoredPool[] = POOLS.map((p) => ({
    ...p,
    // Every pool of the story is its id's first.
    revision: stampToken(1, p.updatedAt),
    members: p.members.map((m) => {
      const account = this.accounts.find((a) => a.id === m.accountId);
      return { ...m, budgetUntil: budgetUntilOf(m, Date.now()), eligible: eligibilityOf(account, p.overagePolicy, m, Date.now()), kind: account?.kind ?? 'oauth', ...servedFigures(m.accountId, Date.now(), p) };
    }),
  }));
  /** The story's limit reset blocks, each use's answer folded in as the router's store folds it. */
  private resetBlocks: Record<string, MockBlock> = { ...RESET_BLOCKS };
  private secretRng = createRng(MOCK_SEED);
  private session: SessionInfo | null = null;
  private sessions: Session[] = SESSIONS;
  // The settings name the bundle's own commit as a router built from it would. The banner reads the build off an
  // answer's header through `call` alone, which the mock never goes through, so it draws none whatever this holds.
  private settings: Settings = decodeSettings({ ...SETTINGS_WIRE, about: { ...SETTINGS_WIRE.about, build: __PHOEBIS_BUILD__ } });
  /** The routing log rows this mock's writes recorded, after the story's, as the ledger appends them. */
  private written: RoutingEvent[] = [];

  private token(n: number) {
    return Array.from({ length: n }, () => 'abcdefghijklmnopqrstuvwxyz0123456789'[this.secretRng.int(0, 35)]).join('');
  }

  /** The registry's lists and the routing log rows the writes recorded, as the routes read them, or none for a fresh
   *  installation. */
  private registry() {
    const empty = emptyStore();
    return { accounts: empty ? [] : this.accounts, guardrails: empty ? [] : this.guardrails, keys: empty ? [] : this.keys, pools: empty ? [] : this.pools, sessions: empty ? [] : this.sessions, written: empty ? [] : this.written };
  }

  /**
   * The refusals of a pool body the create and the edit share, the first in the route's order: the fields, the body
   * capture bound, a doubled member and the member writes. Null where none applies.
   */
  private poolBodyRefusal(input: PoolInput): ApiError | null {
    const refusal = poolRefusal(input);
    if (refusal) return new ApiError(400, refusal, null);
    if (input.bodyCaptureHours > this.settings.retention.bodyCaptureMaxHours) return new ApiError(400, 'body_capture_hours exceeds stats.body_capture_max_hours', null);
    const repeated = repeatedId(input.members.map((m) => m.accountId));
    if (repeated) return new ApiError(409, `members name account ${repeated} twice`, null);
    const members = memberRefusal(input.members);
    return members ? new ApiError(400, members, null) : null;
  }

  /**
   * The route's refusal of a feature account it cannot hold: an id no account holds, which a later account of that id
   * would take over, or an API-key account, which holds no claude.ai token. A word the body leaves out is not read.
   */
  private featureAccountRefusal(words: Partial<FeatureAccountIds> | undefined): ApiError | null {
    for (const feature of FEATURE_WORD_VALUES) {
      const id = words?.[feature];
      if (id == null) continue;
      const account = this.accounts.find((a) => a.id === id);
      if (!account) return new ApiError(400, `feature_accounts names account ${id}, which does not exist`, null);
      if (account.kind === 'api_key') return new ApiError(400, `feature_accounts names account ${id}, which is an API-key account`, null);
    }
    return null;
  }

  /** The keepalive figures a saved body resolves to, composed from its members' kinds as the router composes them. */
  private resolvedOf(input: PoolInput): Pool['keepaliveResolved'] {
    return resolvedFor(composedFor(input.members.map((m) => this.accounts.find((a) => a.id === m.accountId)?.kind ?? 'oauth'), input.cacheTtl), input.keepalive);
  }

  /**
   * The pool as the route answers it, with its keys listed from the key registry at the time of the read, and its
   * warnings the route's checks on the saved settings and the largest cohort among the pins under its keys.
   */
  private poolView(p: StoredPool): Pool {
    const keys = this.keys.filter((k) => k.poolId === p.id).map((k) => ({ enabled: k.enabled, id: k.id, label: k.name, prefix: k.prefix }));
    const ids = keyIdsOf(this.keys, p.id);
    const cohort = largestCohort(ids);
    // A member's pins are read under the keys the pool holds at the read, as its figure is, so a key that moved here
    // counts its pins on the member cells too.
    const members = p.members.map((m) => ({ ...m, activePins: memberPins(ids, m.accountId) }));
    // The route answers `exhausted` for an enabled pool with no member, whatever status the pool was saved with.
    return { ...p, activePins: pinsUnderPool(ids), guardrails: this.guardrailRefs('pools', p.id), keys, members, status: p.status !== 'disabled' && p.members.length === 0 ? 'exhausted' : p.status, warnings: poolWarnings(p, cohort) };
  }

  /**
   * The key as the route answers it, its own guardrails and its pool's read from the attachments at the time of the
   * read.
   */
  private keyView(k: KeyRow): ApiKey {
    return {
      ...k,
      guardrails: this.guardrailRefs('keys', k.id),
      poolGuardrails: this.guardrailRefs('pools', k.poolId),
    };
  }

  /**
   * Restamps the pools and keys another write changed: those a guardrail's create, edit or delete attaches or detaches,
   * and those whose feature selection an account's deletion clears. A pool or key dialog opened before that write then
   * presents a stamp the row no longer carries, as the routes refuse it.
   */
  private restampSubjects(pools: ReadonlySet<string>, keys: ReadonlySet<string>) {
    this.pools = this.pools.map((p) => (pools.has(p.id) ? { ...p, ...restamp(p) } : p));
    this.keys = this.keys.map((k) => (keys.has(k.id) ? { ...k, ...restamp(k) } : k));
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
   * A list read at least `FIRST_POLL_MS` after a confirm lands that confirm's first poll on its row: its clean answer
   * as the row's poll standing and its snapshot, and its reading, the first reading where the row reads no window yet
   * and else the row's windows read at the landing, as the router records a clean answer's reading at its instant.
   * Keyed on the clock rather than a count of reads, since the confirm's reload and the refresh the closing dialog
   * resumes read the list at once on a live clock. A row that lost its credential since the confirm takes none, since
   * the router's poll and its token manager read no such account.
   */
  private landFirstPolls() {
    const at = mockClock.now();
    for (const [id, confirmedAt] of this.firstPolls) {
      if (at - confirmedAt < FIRST_POLL_MS) continue;
      this.firstPolls.delete(id);
      const a = this.accounts.find((x) => x.id === id);
      if (a === undefined || withholdsWindows(a)) continue;
      const read = iso(at);
      const usage = a.usage && { ...a.usage, error: null, fetchedAt: read, heldUntil: null, poll: cleanPollAt(read) };
      const windows = (readWindows(a).length === 0 ? FIRST_READING : a.windows).map((w) => ({ ...w, observedAt: read }));
      this.putAccount({ ...a, fableWindow: a.fableWindow && { ...a.fableWindow, observedAt: read }, usage, windows });
    }
  }

  /** The row the create route builds, and the row the confirm of a pairing started for a new account writes. */
  private newAccountRow(input: AccountInput, status: Account['status'], email: string | null): Account {
    return {
      ...blankAccountRow(input, status, email),
      revision: ((this.buried.get(input.id) ?? 0) + 1) * INCARNATION + 1,
      usage: usageOf({ id: input.id, kind: input.kind, status, windows: [] }),
    };
  }

  /** The account as the route answers it, its pool read off the member lists a pool write rewrites. */
  private withPool(a: Account): Account {
    const pool = this.pools.find((p) => p.members.some((m) => m.accountId === a.id));
    const member = pool?.members.find((m) => m.accountId === a.id);
    return { ...a, pool: pool && member ? { poolId: pool.id, priority: member.priority } : null };
  }

  private writeEvent(kind: string, accountId: string, actor: string | null, at: number, detail: Record<string, unknown>): void {
    this.written.push({ type: 'event', accountId, actor, agentId: null, at: iso(at), detail, id: ROUTING_EVENTS.length + this.written.length + 1, keyId: null, kind, model: null, poolId: null, requestId: null, sessionId: null });
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

  analyticsPlacements(query: Pick<AnalyticsQuery, 'accountIds' | 'clients' | 'from' | 'keyIds' | 'models' | 'poolIds' | 'to' | 'userAgents'>) {
    return latency(placementsOf(ledger().requests, query));
  }

  async cancelPairing(pairingId: string) {
    this.pairings.delete(pairingId);
    return latency(undefined, 150);
  }

  async clearCooldown(accountId: string, scope: string) {
    const a = this.account(accountId);
    const cooldowns = a.cooldowns.filter((c) => c.scope !== scope);
    this.putAccount({ ...a, cooldowns, status: statusOf({ ...a, cooldowns }, a.routingEnabled) });
    return latency(undefined, 200);
  }

  async clearFunding(accountId: string) {
    const a = this.account(accountId);
    const flag = flagOf(a.status);
    this.putAccount(revised({ ...a, extraUsage: a.extraUsage ? { ...a.extraUsage, disabledReason: null } : null, status: statusOf(a, a.routingEnabled, flag === 'funding_required' ? null : flag) }));
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
    // until a disconnect deletes that credential. A credential a later pairing stored is sealed under the key in use,
    // so the refusal holds only while the row carries the seed's end.
    const held = pairing.label === undefined ? this.accounts.find((a) => a.id === pairing.accountId) : undefined;
    if (held?.id === 'legacy' && held.credential.refreshTokenExpiresAt === accountById.get('legacy')?.credential.refreshTokenExpiresAt) throw new ApiError(409, `${held.label} holds a credential sealed under another storage key, so disconnect it before pairing again`, 'sealed_under_another_key');
    // The confirm stores the tokens it exchanged, so the row carries both their expiries, as the router answers it.
    const tokens = { accessTokenExpiresAt: ahead(8 * HOUR), refreshTokenExpiresAt: ahead(30 * DAY) };
    // The router's confirm drops the usage service's entry for the account, so until the first poll answers the row
    // reads no poll standing and answers from what the router still holds: no error and the instant of a reading it
    // holds, or the poll's unread sentence and no instant where it holds none.
    const inFlight = (usage: Account['usage'], read: boolean): Account['usage'] => usage && { ...usage, ...(read ? { error: null } : { error: POLL_UNREAD, fetchedAt: null }), heldUntil: null, poll: null };
    if (pairing.label !== undefined) {
      // The backstop for a create that landed between the start and the confirm.
      if (this.accounts.some((a) => a.id === pairing.accountId)) throw new ApiError(409, ACCOUNT_TAKEN, null);
      const row = this.newAccountRow({ id: pairing.accountId, kind: 'oauth', label: pairing.label, secret: null }, 'ready', 'operator+new@example.com');
      const credential = { ...row.credential, ...tokens };
      // The router holds no reading of an account the confirm creates.
      this.accounts = [...this.accounts, { ...row, credential, featureGrants: featureGrantsOf({ credential, kind: row.kind }), usage: inFlight(row.usage, false) }];
    }
    // The router's confirm clears a plan-ended flag and records the plan found again only where the confirmed profile
    // reports a plan. The mock's profile is the stored document, so a document that names a free organization keeps
    // the flag, and a confirm that keeps it starts no first poll, since the poll's schedule reads no flagged account.
    const flagged = held !== undefined && held.planEndedAt !== null ? held : null;
    const kept = flagged !== null && !reportsPlan(flagged);
    if (held !== undefined) {
      // The router's pairing clears the stored reset block and moves its epoch, so the row reads no limit resets until
      // the next read, whose member no token from before the pairing matches. The upstream's grants and uses stay in
      // the block, since a pairing changes nothing upstream. Its confirm sets the account's state ready, which the
      // account view folds with the enabled bit and the cooldowns.
      const credential = { ...held.credential, ...tokens };
      this.putAccount({ ...held, credential, featureGrants: featureGrantsOf({ credential, kind: held.kind }), limitResets: null, status: statusOf(held, held.routingEnabled, null), usage: inFlight(held.usage, holdsReading(held)), ...(flagged !== null && !kept ? { planEndedAt: null } : {}) });
      const block = lookup(this.resetBlocks, held.id);
      if (block !== undefined) this.resetBlocks = { ...this.resetBlocks, [held.id]: { ...block, folds: block.folds + 1 } };
    }
    if (flagged !== null && !kept) {
      const detail = { organization_type: flagged.subscription?.organizationType ?? null, rate_limit_tier: flagged.subscription?.rateLimitTier ?? null, source: 'pairing', subscription_status: 'active' };
      this.writeEvent('plan_restored', flagged.id, null, mockClock.now(), detail);
    }
    if (!kept) this.firstPolls.set(pairing.accountId, mockClock.now());
    return latency({ accountId: pairing.accountId, firstPollInFlight: !kept, status: 'confirmed' }, 250);
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
  async createGuardrail(input: CreateGuardrailInput): Promise<Guardrail> {
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
    this.restampSubjects(new Set(row.pools.map((a) => a.id)), new Set(row.keys.map((a) => a.id)));
    return latency(this.guardrailView(row), 300);
  }

  async createKey(input: CreateKeyInput): Promise<CreatedApiKey> {
    const shape = unknownKeyMember(input);
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
    // A new key starts at inherit for each word the body leaves out.
    const featureAccounts = { ...INHERITED_FEATURE_ACCOUNTS, ...input.featureAccounts };
    const unselectable = this.featureAccountRefusal(namedFeatureAccounts(featureAccounts));
    if (unselectable) throw unselectable;
    const body = this.token(40);
    const secret = `phoebis-${body}`;
    const key: KeyRow = {
      ...KEY_SAMPLE,
      id,
      name: input.name,
      prefix: secret.slice(0, 16),
      poolId: input.poolId,
      // The route's defaults for a body that leaves these fields out, which a caller outside the console may.
      autoModeServer: input.autoModeServer ?? null,
      companionPermissions: input.companionPermissions ?? [...COMPANION_PERMISSION_VALUES],
      featureAccounts,
      longContextTwins: input.longContextTwins ?? true,
      modelRules: input.modelRules ?? [],
      modelRulesMode: input.modelRulesMode ?? 'extend',
      status: 'active',
      createdAt: iso(NOW),
      updatedAt: iso(NOW),
      revision: stampToken((this.buriedKeys.get(id) ?? 0) + 1, iso(NOW)),
      expiresAt: input.expiresAt ?? null,
      lastUsedAt: null,
      notes: input.notes,
      bodyCaptureHours: input.bodyCaptureHours,
      usage7d: { billedUsd: 0, cost: 0, equivalent: 0, requests: 0, tokens: 0 },
      lifetime: { billedUsd: 0, cost: 0, equivalent: 0, requests: 0 },
    };
    this.keys = [key, ...this.keys];
    return latency({ key: this.keyView(key), secret }, 300);
  }

  async createPool(input: CreatePoolInput): Promise<Pool> {
    // A new pool starts with no guardrail, so the route refuses the member before it reads anything else.
    if (input.guardrails !== undefined) throw new ApiError(400, 'guardrails applies on edit', null);
    const id = slug(input.name);
    if (this.pools.some((p) => p.id === id)) throw new ApiError(409, POOL_TAKEN, null);
    const refused = this.poolBodyRefusal(input);
    if (refused) throw refused;
    const unselectable = this.featureAccountRefusal(input.featureAccounts);
    if (unselectable) throw unselectable;
    const held = heldElsewhere(input.members, this.pools, id);
    if (held) throw new ApiError(409, held, null);
    const pool: StoredPool = {
      ...POOL_SAMPLE,
      ...input,
      ...UNTIL_DRAWN,
      // The route's member is optional per word, and a new pool starts at none for each word the body leaves out.
      featureAccounts: { ...NO_FEATURE_ACCOUNTS, ...input.featureAccounts },
      id,
      // From the members' kinds, as the router composes them, and nothing for a pool with no member.
      keepaliveResolved: this.resolvedOf(input),
      members: input.members.map((m) => {
        const account = this.accounts.find((a) => a.id === m.accountId);
        return writtenMember(m, account, input, (member) => eligibilityOf(account, input.overagePolicy, member, Date.now()));
      }),
      // The route starts a pool with no list when the body leaves it out, whatever the sample row holds.
      modelRules: input.modelRules ?? [],
      movesPerHour: {},
      revision: stampToken((this.buriedPools.get(id) ?? 0) + 1, iso(NOW)),
      updatedAt: iso(NOW),
    };
    this.pools = [...this.pools, pool];
    return latency(this.poolView(pool), 300);
  }

  async deleteAccount(accountId: string) {
    this.buried.set(accountId, Math.floor(this.account(accountId).revision / INCARNATION));
    this.accounts = this.accounts.filter((a) => a.id !== accountId);
    // Each selection of the account goes to none, a key's never to inherit, which would act under an account the
    // operator did not choose for it. A cleared row takes a new stamp, so a dialog read before the deletion cannot save
    // the old selection onto an account created again under the id.
    const cleared = { keys: new Set<string>(), pools: new Set<string>() };
    this.pools = this.pools.map((p) => {
      const members = p.members.filter((m) => m.accountId !== accountId);
      const named = poolFeaturesNaming(p, accountId);
      if (named.length > 0) cleared.pools.add(p.id);
      const featureAccounts = named.length === 0 ? p.featureAccounts : perFeature((f) => (named.includes(f) ? null : p.featureAccounts[f]));
      return { ...p, featureAccounts, keepaliveResolved: members.length === 0 ? null : p.keepaliveResolved, members };
    });
    this.keys = this.keys.map((k) => {
      const named = keyFeaturesNaming(k, accountId);
      if (named.length === 0) return k;
      cleared.keys.add(k.id);
      return { ...k, featureAccounts: perFeature((f) => (named.includes(f) ? { accountId: null, mode: 'none' as const } : k.featureAccounts[f])) };
    });
    this.restampSubjects(cleared.pools, cleared.keys);
    return latency(undefined, 250);
  }

  async deleteGuardrail(id: string) {
    const row = this.guardrails.find((g) => g.id === id);
    if (!row) throw new ApiError(404, 'guardrail not found', null);
    this.buriedGuardrails.set(id, Math.floor(row.revision / INCARNATION_STAMP));
    // The attachments live on the row, so they leave with it and every other guardrail keeps its positions.
    this.guardrails = this.guardrails.filter((g) => g.id !== id);
    this.restampSubjects(new Set(row.pools.map((a) => a.id)), new Set(row.keys.map((a) => a.id)));
    return latency(undefined, 200);
  }

  async deleteKey(id: string) {
    const key = this.keys.find((k) => k.id === id);
    if (key) this.buriedKeys.set(id, Math.floor(key.revision / INCARNATION_STAMP));
    this.keys = this.keys.filter((k) => k.id !== id);
    const written = writtenBy(mockClock.now());
    this.sessions = this.sessions.flatMap((s) => (s.keyId === id ? rowsOfDeletedKey(s, written) : [s]));
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
    // the poll's standing goes too. The usage block keeps the instant of the last reading, which the answer of a row
    // needing re-auth already withholds, and the stored row holds no window, as the answer withholds them.
    this.putAccount(revised({ ...a, credential: NO_CREDENTIAL, fableWindow: null, featureGrants: featureGrantsOf({ credential: NO_CREDENTIAL, kind: a.kind }), status: 'unpaired', usage: usageOf({ ...a, ...lastReadingOf(a), status: 'unpaired' }), windows: [] }));
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

  getAnalyticsBurn(range: Pick<AnalyticsQuery, 'from' | 'to'>, groupBy: BurnGroup, filters?: Pick<AnalyticsQuery, 'accountIds' | 'keyIds' | 'poolIds' | 'userAgentGroups'>) {
    return latency(burnOf(ledger().rollups, range, groupBy, filters));
  }

  /**
   * The counts route over the story's rows: every match in the whole hours the range touches, the blocks' captured
   * request bytes, and the name the registry holds now, none for a guardrail deleted since it matched. The route turns
   * the trailing day into two instants, which end at the story's own instant here as the dashboard's day does, counts
   * the whole hours they touch as it does a custom range's, and echoes both instants.
   */
  getAnalyticsGuardrails(range: GuardrailRange): Promise<GuardrailCounts> {
    const from = range === '24h' ? NOW - DAY : Date.parse(range.from);
    const to = range === '24h' ? NOW : Date.parse(range.to);
    const fromMs = startOfHour(from);
    const toMs = hourCeil(to);
    const counts = new Map<string, { blocked: number; blockedBytes: number; observed: number }>();
    for (const r of ledger().requests) {
      const at = Date.parse(r.at);
      if (at < fromMs || at >= toMs) continue;
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

  getAnalyticsRewrites(range: Pick<AnalyticsQuery, 'from' | 'to'>, filters?: Pick<AnalyticsQuery, 'accountIds' | 'poolIds'>) {
    // The fold reads the drawn hours alone: it gives each of their moves a drawn reason, fit and cache write, which a
    // move of the routing log holds of its own.
    const { accountRollups, requests, routingEvents } = ledger();
    return latency(rewritesOf(accountRollups, requests, routingEvents, range, filters));
  }

  getAnalyticsSeries(query: AnalyticsQuery) {
    return latency(seriesOf(ledger().rollups, query));
  }

  /**
   * The story's weekly resets and drops with the claims they join, the uses written since among them, or none for a
   * fresh installation, whose registry holds no account to end a term on.
   */
  getAnalyticsUnusedAllowance(span: Pick<AnalyticsQuery, 'from' | 'to'>, filters?: Pick<AnalyticsQuery, 'accountIds'>) {
    const { quotaHistory, routingEvents } = ledger();
    const { accounts, written } = this.registry();
    return latency(unusedAllowanceOf([...routingEvents, ...written], accounts, quotaHistory, span, filters));
  }

  /**
   * The story's conditions, or none for a fresh installation, the parked records' items gone once a replay landed them.
   * The refresh tokens near their end are read off the live accounts, so a re-pair's new end clears its item, and the
   * feature accounts that cannot serve off the live pools, keys and accounts at the read's instant, the one the dialogs'
   * options read, so a moved selection or a new pairing clears its item. Both stand at their kind's rank, as the route
   * ranks the kinds.
   */
  getAttention() {
    const { attention } = ledger();
    const listed = this.parkedLanded ? attention.filter((i) => i.kind !== 'ledger_record_parked') : attention;
    const registry = this.registry();
    return latency([...listed, ...pairingExpiringOf(registry.accounts, NOW), ...featureAccountUnavailableOf(registry, mockClock.now())].sort(byKindThenSince));
  }

  async getCapturedBodies(requestId: string) {
    const r = ledger().requestById.get(requestId);
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
    const { keys, pools } = this.registry();
    const key = keys.find((k) => k.id === keyId);
    if (!key) throw new ApiError(404, 'key not found', null);
    const pool = pools.find((p) => p.id === key.poolId);
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
    // The route answers its 25 hourly points, the 24 whole hours before the current one and the current hour so far,
    // over an empty ledger too, at zero, so each spark draws a flat line.
    const last24h = ledger().dayRows;
    const byHour = new Map<number, RequestLog[]>();
    for (const r of last24h) {
      const hour = startOfHour(Date.parse(r.at));
      const rows = byHour.get(hour);
      if (rows) rows.push(r);
      else byHour.set(hour, [r]);
    }
    const hourlyMap = new Map<number, Overview['hourly'][number]>();
    for (let i = 24; i >= 0; i -= 1) {
      const startMs = startOfHour(NOW - i * HOUR);
      const { billedUsd, equivalent } = requestSidesOf(byHour.get(startMs) ?? []);
      hourlyMap.set(startMs, { at: iso(startMs), billed: billedUsd, cost: 0, equivalent, errors: 0, refused: 0, requests: 0, tokens: 0 });
    }
    let tokens = 0;
    let cached = 0;
    let cost = 0;
    let requests = 0;
    let errors = 0;
    let refused = 0;
    const errorCodes = new Map<string, number>();
    const refusalCodes = new Map<string, number>();
    // The ledger's rate: a request that did not succeed is an error, the router's cut answered with a 200 among them,
    // and a client that hung up is none, so its code leaves "Most common" too.
    const erred = (r: RequestLog) => r.outcome !== 'succeeded' && !clientLeft(r);
    for (const r of last24h) {
      const bucket = hourlyMap.get(startOfHour(Date.parse(r.at)));
      if (!r.refused) {
        cost += r.costUsd;
        if (bucket) bucket.cost += r.costUsd;
      }
      // The day's rows hold a billed probe for its dollars, and nothing else of it counts, as the route skips a probe
      // ahead of its refusals.
      if (r.kind === 'probe') continue;
      // The ledger's fold books a refused request under its refusals map and nothing else.
      if (r.refused) {
        refused += 1;
        if (r.errorCode) refusalCodes.set(r.errorCode, (refusalCodes.get(r.errorCode) ?? 0) + 1);
        if (bucket) bucket.refused += 1;
        continue;
      }
      const total = tokensTotal(r.usage);
      tokens += total;
      cached += r.usage.cacheRead;
      if (r.errorCode && !clientLeft(r)) errorCodes.set(r.errorCode, (errorCodes.get(r.errorCode) ?? 0) + 1);
      const failed = erred(r);
      requests += 1;
      if (failed) errors += 1;
      if (bucket) {
        bucket.requests += 1;
        bucket.tokens += total;
        if (failed) bucket.errors += 1;
      }
    }
    const top = [...errorCodes.entries()].sort((a, b) => b[1] - a[1])[0];
    const topRefusal = [...refusalCodes.entries()].sort((a, b) => b[1] - a[1])[0];
    const day = requestSidesOf(last24h);
    return latency({
      // Counted off the pools, so the dashboard and the Pools view cannot disagree on the router's pins.
      activePins: this.registry().pools.reduce((n, p) => n + poolPins(p, this.keys), 0),
      day24h: { billedUsd: day.billedUsd, cost, equivalent: day.equivalent },
      errorRate24h: requests ? errors / requests : null,
      hourly: [...hourlyMap.values()],
      refused24h: refused,
      requests24h: requests,
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

  /**
   * The request with the events of its window, from its arrival to its finish, under its session or naming the
   * request. Its pin events read as the route reads its pin events, off the routing log, which holds each placement's
   * creation, and the request's own table adds its other kinds, so no event lists twice. A row the story gives no
   * finish ended when the ledger wrote it, since the router records a finish on every request it writes.
   */
  getRequest(id: string): Promise<RequestDetail | null> {
    const request = ledger().requestById.get(id);
    if (request === undefined) return latency(null);
    const from = Date.parse(request.at);
    const to = request.phases.finishedAt === null ? landedAt(request) : Date.parse(request.phases.finishedAt);
    const pins = ROUTING_EVENTS.filter((e) => PIN_EVENT_KINDS.includes(e.kind) && Date.parse(e.at) >= from && Date.parse(e.at) <= to && ((request.sessionId !== null && e.sessionId === request.sessionId) || e.requestId === id));
    const own = (REQUEST_EVENTS.get(id) ?? []).filter((e) => !PIN_EVENT_KINDS.includes(e.kind));
    const rows = cutsOf(id);
    const asEvent = (e: RoutingEvent): RequestEvent => ({ accountId: e.accountId, at: e.at, detail: e.detail, keyId: e.keyId, kind: e.kind, model: e.model, sessionId: e.sessionId });
    // The route orders by the instant and then by the row's place in the ledger. At one instant the stable sort keeps
    // the order the lists join in: the request's own events, then the pin events, then the cuts.
    const events = [...own, ...pins.map(asEvent), ...rows.map(asEvent)].sort((a, b) => Date.parse(a.at) - Date.parse(b.at));
    const newest = rows[rows.length - 1];
    return latency({ events, request, streamCut: newest ? decodeStreamCut({ ...newest.detail, at: Date.parse(newest.at) }) : null });
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
    const { fromMs, toMs } = wholeHoursOf({ from, to });
    return latency(
      ledger().accountHours.filter((r) => {
        const at = Date.parse(r.hour);
        return at >= fromMs && at < toMs;
      }),
    );
  }

  listAccounts() {
    this.landFirstPolls();
    return latency(this.registry().accounts.map((a) => this.withPool(a)));
  }

  listCatalog() {
    return latency(ledger().catalog);
  }

  async listCatalogOf(accountIds: string[]): Promise<CatalogModel[]> {
    const listed = new Set(this.registry().accounts.map((a) => a.id));
    const fetched = await Promise.all(
      accountIds.filter((id) => listed.has(id)).map(async (id) => {
        try {
          return await this.getAccountCatalog(id);
        } catch {
          return null;
        }
      }),
    );
    return catalogUnion(fetched);
  }

  listFeatureCalls(query: FeatureCallQuery = {}): Promise<FeatureCallPage> {
    const cursor = query.cursor === undefined ? undefined : cursorKeyOf(query.cursor, 'received_at');
    if (cursor === null) return Promise.reject(new ApiError(400, 'cursor is <received_at>:<id>', null));
    const compare = compareOrderKeys('desc');
    const matched = ledger()
      .featureCalls.filter((c) => matchesFeatureCall(c, query))
      .sort((a, b) => compare(featureCallKey(a), featureCallKey(b)));
    const { nextCursor, page } = keyedPage(matched, cursor, 50, featureCallKey, compare);
    return latency({ featureCalls: page, nextCursor });
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
    return latency(this.registry().keys.map((k) => this.keyView(k)));
  }

  /** The pins under the pool's keys at the read, the clock the sessions route reads, swept at the pool's affinity
   *  TTL. */
  listPoolPins(poolId: string) {
    const { keys, pools, sessions } = this.registry();
    const pool = pools.find((p) => p.id === poolId);
    if (pool === undefined) return Promise.reject(new ApiError(404, 'pool not found', null));
    return latency(poolPinsAt(NOW, sessions, keyIdsOf(keys, poolId), pool.affinityTtlSeconds * 1_000));
  }

  listPools() {
    return latency(this.registry().pools.map((p) => this.poolView(p)));
  }

  listRequests(query: RequestQuery = {}): Promise<RequestPage> {
    // The route's query extractor refuses a key outside its list before the store is read, in the parser's words behind
    // the parameter's name.
    const order = query.order ?? 'received_at';
    if (!REQUEST_ORDER_VALUES.includes(order)) return Promise.reject(new ApiError(400, `order: unknown variant \`${order}\`, expected one of ${REQUEST_ORDER_VALUES.map((w) => `\`${w}\``).join(', ')}`, null));
    const dir = query.dir ?? 'desc';
    const cursor = query.cursor === undefined ? undefined : cursorKeyOf(query.cursor, order);
    if (cursor === null) return Promise.reject(new ApiError(400, 'cursor is <value>:<id>', null));
    const live = new Map(this.registry().keys.map((k) => [k.id, Date.parse(k.createdAt)]));
    // The newest stamp is over the rows the query's filters admit, its `to` included, with its `landedAfter` as the
    // floor and capped at its `to` unless the query marks a fixed end, as the route reads it.
    const now = mockClock.now();
    const written = writtenBy(now);
    const since = query.landedAfter;
    const matched = written.filter((r) => matchesQuery(r, query, live) && (since === undefined || landedAt(r) > since)).sort(requestOrder(order, dir));
    const newest = matched.reduce((stamp, r) => Math.max(stamp, landedAt(r)), since ?? 0);
    const landedThrough = query.to && !query.fixedEnd ? Math.min(newest, Date.parse(query.to)) : newest;
    const { nextCursor, page } = keyedPage(matched, cursor, query.limit ?? 50, (r) => orderKeyOf(r, order), compareOrderKeys(dir));
    return latency({ landedThrough, nextCursor, requests: page });
  }

  listRoutingLog(query: RoutingLogQuery = {}): Promise<RoutingLogPage> {
    // The route's refusals, in its order and words.
    const unknown = query.kinds?.find((k) => !ROUTING_LOG_KINDS.includes(k));
    if (unknown !== undefined) return Promise.reject(new ApiError(400, `unknown kind ${unknown}`, null));
    if (query.limit !== undefined && !(Number.isInteger(query.limit) && query.limit >= 1 && query.limit <= 200)) return Promise.reject(new ApiError(400, 'limit is 1 to 200', null));
    const cursor = query.cursor === undefined ? undefined : routingCursorOf(query.cursor);
    if (cursor === null) return Promise.reject(new ApiError(400, 'cursor is <at>:<rank>:<id>', null));
    const { routingEvents, summaries } = ledger();
    // The writes' rows are the ledger's too, so a fresh installation lists none of them.
    return latency(listRoutingLog(query, Date.now(), { events: routingEvents, summaries, written: this.registry().written }, cursor));
  }

  listSessions() {
    if (sessionsFail()) return Promise.reject(new ApiError(503, 'store busy', null));
    return latency(sessionsListedAt(NOW, this.registry().sessions, this.settings.sessions.nonCompanionIdleSeconds, writtenBy(mockClock.now()), ledger().routingEvents));
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

  async readLimitResets(accountId: string): Promise<LimitResetsRead> {
    const a = this.account(accountId);
    const at = mockClock.now();
    // The usage service refuses a read while the poll waits out a backoff, and the route answers the stored block,
    // which holds nothing once a pairing cleared it.
    const next = a.usage?.poll?.nextAttemptAt;
    const held = next !== null && next !== undefined && Date.parse(next) > at;
    const error = held ? 'the account is waiting out a rate limit' : null;
    if (held && a.limitResets === null) return latency({ error, limitResets: null }, 400);
    const limitResets = decodeLimitResets(limitResetsWire(accountId, this.resetBlocks, at, incarnationOf(a)));
    this.putAccount({ ...a, limitResets });
    return latency({ error, limitResets }, 400);
  }

  refreshUsage(_accountId: string) {
    return latency(undefined, 400);
  }

  async renameAccount(accountId: string, label: string, revision?: number) {
    const a = this.account(accountId);
    if (revision !== undefined && revision !== a.revision) {
      return Promise.reject(new ApiError(409, ACCOUNT_STALE, 'stale_edit'));
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
    const next: KeyRow = { ...key, prefix: secret.slice(0, 16), retiredSecretLastUsedAt: null, sessionsOnRetiredSecret: 2, ...restamp(key) };
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
    const { guardrails, keys, pools } = this.registry();
    let conditions = input.conditions ?? [];
    if (input.guardrail !== undefined) {
      const saved = guardrails.find((g) => g.id === input.guardrail);
      if (!saved) throw new ApiError(404, 'no such guardrail', null);
      conditions = saved.conditions;
    } else {
      const refusal = conditionsRefusal(input.conditions);
      if (refusal !== null) throw new ApiError(400, refusal, null);
    }
    if (input.poolId !== undefined && !pools.some((p) => p.id === input.poolId)) throw new ApiError(404, 'no such pool', null);
    if (input.keyId !== undefined && !keys.some((k) => k.id === input.keyId)) throw new ApiError(404, 'no such key', null);
    const since = NOW - windowHours * HOUR;
    const rows = ledger().requests.filter((r) => r.kind !== 'probe' && r.capturedBodies && Date.parse(r.at) >= since && (input.poolId !== undefined ? r.poolId === input.poolId : r.keyId === input.keyId))
      .sort(compareRequests)
      .slice(0, limit);
    const requests = rows.map((r) => ({ ...dryRunVerdict(conditions, r, GUARDRAIL_STORY_IDS.has(r.id)), id: r.id, receivedAt: r.at }));
    const count = (verdict: string) => requests.filter((r) => r.verdict === verdict).length;
    return latency({ requests, sample, totals: { indeterminate: count('indeterminate'), matched: count('matched'), sampled: requests.length, unmatched: count('unmatched') } }, 300);
  }

  /** The account route's refusals in its order and words. The end is kept only while the flag stands. */
  async updateAccount(accountId: string, patch: AccountPatch, revision?: number): Promise<Account> {
    const a = this.account(accountId);
    if (patch.canceled && a.kind === 'api_key') throw new ApiError(400, 'only an oauth account is canceled', null);
    if (revision !== undefined && revision !== a.revision) {
      return Promise.reject(new ApiError(409, ACCOUNT_STALE, 'stale_edit'));
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
  async updateGuardrail(id: string, input: GuardrailPatch, revision?: number): Promise<Guardrail> {
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
    this.restampSubjects(changed('pools'), changed('keys'));
    return latency(this.guardrailView(next), 250);
  }

  /**
   * The refusals in the route's order: the unknown member, then after the mock's key lookup the pool 404, the rules
   * refusal, a feature word's id that does not go with its mode, the doubled guardrail, the stale stamp, a feature
   * account that cannot be selected, the bound refusal, the unknown guardrail.
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
    if (revision !== undefined && revision !== current.revision) return Promise.reject(new ApiError(409, KEY_STALE, 'stale_edit'));
    // A word the body leaves out keeps its stored choice, and a move keeps both.
    const featureAccounts = { ...current.featureAccounts, ...rest.featureAccounts };
    // Behind the stamp, over the choices the write would store, so a save from before an account's deletion answers
    // stale_edit, never this refusal.
    const unselectable = this.featureAccountRefusal(namedFeatureAccounts(featureAccounts));
    if (unselectable) return Promise.reject(unselectable);
    // The pool the key lands in, a new one on a move, with the key's own list as the edit leaves it.
    const own = ownIds ?? this.guardrailRefs('keys', id).map((r) => r.id);
    const bound = boundRefusal(rest.poolId, this.guardrailRefs('pools', rest.poolId).map((r) => r.id), [[id, own]]);
    if (bound !== null) return Promise.reject(new ApiError(400, bound, null));
    const unknown = (ownIds ?? []).find((g) => !this.guardrails.some((x) => x.id === g));
    if (unknown !== undefined) return Promise.reject(new ApiError(404, `guardrail ${unknown} not found`, null));
    const expiresAt = expiry === undefined ? current.expiresAt : expiry;
    // The status is the decoder's fold of the stored flags, so a key switched off or lapsed reads that way at once.
    const status = keyStatusOf(current.status === 'revoked', expiresAt, rest.enabled, Date.now());
    const next: KeyRow = { ...current, ...rest, expiresAt, featureAccounts, status, ...restamp(current) };
    this.keys = this.keys.map((k, i) => (i === idx ? next : k));
    if (ownIds !== undefined) this.setSubjectGuardrails('keys', id, ownIds);
    return latency(this.keyView(next), 250);
  }

  async updatePool(id: string, input: PoolInput, revision?: number): Promise<Pool> {
    const idx = this.pools.findIndex((p) => p.id === id);
    if (idx < 0) return Promise.reject(new Error(`Unknown pool ${id}`));
    // The route refuses a repeat and a budget before the store looks any member up, and the store checks every member
    // inside the one transaction that writes the pool, so a refusal stores nothing.
    const refused = this.poolBodyRefusal(input);
    if (refused) return Promise.reject(refused);
    const { guardrails: guardrailIds, ...fields } = input;
    const twice = repeatedId(guardrailIds);
    if (twice !== null) return Promise.reject(new ApiError(400, `guardrails lists ${twice} twice`, null));
    // In the route's order: the stamp after the list's own shape and ahead of the member lookups, so a stale edit that
    // names an unknown or a held member answers `stale_edit`.
    const current = this.pools[idx];
    if (revision !== undefined && revision !== current.revision) return Promise.reject(new ApiError(409, POOL_STALE, 'stale_edit'));
    // Behind the stamp, so a save from before an account's deletion answers stale_edit, never this refusal.
    const unselectable = this.featureAccountRefusal(input.featureAccounts);
    if (unselectable) return Promise.reject(unselectable);
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
    const next: StoredPool = {
      ...current,
      ...fields,
      // A word the body leaves out keeps its stored account, as the route keeps it, and a member change moves none.
      featureAccounts: { ...current.featureAccounts, ...fields.featureAccounts },
      // From the saved members' kinds, as the router composes them on every save.
      keepaliveResolved: this.resolvedOf(input),
      // Eligibility is the router's reading of live cooldowns and overages, not something a save can set. A save
      // does rewrite the budget, and a write answers no spend here, so a budget verdict is read again.
      members: input.members.map((m) => {
        const account = this.accounts.find((a) => a.id === m.accountId);
        const kept = verdict.get(m.accountId) ?? 'eligible';
        return writtenMember(m, account, input, (member) => (kept === 'budget' ? eligibilityOf(account, input.overagePolicy, member, Date.now()) : kept));
      }),
      ...restamp(current),
    };
    this.pools = this.pools.map((p, i) => (i === idx ? next : p));
    if (guardrailIds !== undefined) this.setSubjectGuardrails('pools', id, guardrailIds);
    return latency(this.poolView(next), 250);
  }

  async useLimitReset(accountId: string, grantId: string, revision?: number): Promise<ResetUse> {
    const path = `/admin/accounts/${encodeURIComponent(accountId)}/limit-resets/${encodeURIComponent(grantId)}/use`;
    const a = this.account(accountId);
    const at = mockClock.now();
    const block = lookup(this.resetBlocks, accountId);
    const index = block?.grants.findIndex((g) => g.id === grantId) ?? -1;
    // The router checks the grant against its stored block before anything leaves, so the table never answers a use
    // the router refuses, a grant that waits among them. A row's null member stands for a store that holds no block,
    // as a pairing leaves it until the next read.
    if (a.limitResets === null || block === undefined || index < 0 || grantStates(block, at)[index].state !== 'usable') throw new ApiError(409, 'this reset is not usable now', 'limit_reset_not_usable', path);
    // The router compares the token once the grant read usable, before the claim's token is acquired.
    if (revision !== undefined && revision !== resetsRevision(incarnationOf(a), block)) throw new ApiError(409, 'the account changed while its reset was being used', 'limit_reset_account_changed', path);
    const entry = lookup(RESET_ANSWERS, grantId);
    if (entry === undefined) throw new Error(`the story answers no use of ${grantId}`);
    // Every claim that left writes its row, one the upstream left unanswered included, as the router's task does.
    const sent = 'refusal' in entry ? { answer: null, status: entry.upstream } : { answer: entry.answer, status: 200 };
    const detail = claimDetail(block.grants[index], this.claimIds.uuid(), sent.status, sent.answer, incarnationOf(a));
    this.writeEvent('limit_reset_claimed', a.id, 'admin', at, detail);
    if ('refusal' in entry) throw new ApiError(entry.refusal.status, entry.refusal.message, entry.refusal.code, path);
    this.resetBlocks = { ...this.resetBlocks, [accountId]: foldAnswer(block, grantId, entry.answer, at) };
    // Built after the fold, so the claimed grant reads its count and its use from this claim.
    const wire = limitResetsWire(accountId, this.resetBlocks, at, incarnationOf(a));
    const limitResets = decodeLimitResets(wire);
    const claimed = limitResets?.grants.find((g) => g.id === grantId);
    const readings = entry.answer.outcome === 'used' && claimed ? readingsAfterReset(a, claimed.clears, claimed.kind, at) : null;
    if (readings !== null) for (const drop of dropsAfterReset(a, readings)) this.writeEvent('window_dropped', a.id, null, at, drop);
    this.putAccount({ ...a, ...readings, limitResets });
    return latency(decodeResetUse({ ...entry.answer, limit_resets: wire }), 400);
  }

  async verifyAccount(accountId: string) {
    this.account(accountId);
    return latency({ ok: true, status: 200 }, 400);
  }
}
