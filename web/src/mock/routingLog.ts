import type {
  Account, AttemptWindow, CohortHold, DecisionKind, DecisionRecord, DecisionSummary, MoveCause, PinReason, Placement, PrefixEntry, RecordBudget, RecordCandidate, RecordCompletion, RecordCooldown, RecordOutcome,
  RecordPin, RecordRequest, RecordSettings, RequestLog, RoutingEvent, RoutingLogPage, RoutingLogQuery, Score, SummaryFact, WalkFact, WalkStep,
} from '@/api/types';
import { aheadOf, endOf } from '@/lib/quota';
import { ACCOUNTS } from './accounts';
import { KEY_REVOKED_AT, WORST_BUCKET, poolById, type PoolSeed } from './pools';
import { BUDGET_CROSSING_ID, FORCED_MOVE_ID, HAIKU, MARKED_SESSION_IDS, OPENING_SESSION_IDS, OVERAGE_FROM, REQUEST_EVENTS, REQUESTS, SHED_ROUND, SLOWED_SESSION_ID, STORY_HOUR_FROM, STORY_IDS, clock, failsOver, oauthWindows, requestById } from './requests';
import { createRng } from './rng';
import { DAY, HOUR, MINUTE, NOW, iso } from './time';

/** Why a move rule fired, as the trace's `moved` names it. A failover's reasons move the pin from the task instead. */
const RULES = ['spent', 'overage', 'return_to_plan', 'drain', 'shed'] as const;
const FAILOVER_REASONS = ['cooldown', 'fast_lane'];
const PLAN_WINDOWS = ['five_hour', 'seven_day'] as const;
const FIVE_HOURS = 5 * HOUR;
/** How long a booking lives before the purge expires it, the router's `IN_FLIGHT_RECLAIM_MS`. */
const RESERVATION_MS = HOUR;

/**
 * The share of each plan window one written token takes, as the story's tiers price a rewrite: a Fable token at the
 * window's own rate and the other families at their weight. A model without a weight of its own prices at its
 * family's, as the router estimates it, and only a Fable model draws its model week.
 */
const WRITE_SHARE: Record<string, number> = { five_hour: 1.2e-7, seven_day: 1.4e-8, seven_day_fable: 2.8e-8 };
const familyWeight = (model: string) => (model.includes('fable') ? 1 : model.includes('opus') ? 0.5 : model.includes('sonnet') ? 0.25 : 0.08);
/** The plan windows a model draws: both shared windows, and a Fable model its week as well. */
const drawnWindows = (model: string): string[] => (model.includes('fable') ? [...PLAN_WINDOWS, 'seven_day_fable'] : [...PLAN_WINDOWS]);
const round3 = (x: number) => Math.round(x * 1_000) / 1_000;
const round5 = (x: number) => Math.round(x * 100_000) / 100_000;
/** A rewrite of `tokens` per plan window the model draws, in window share. */
export const rewriteOf = (tokens: number, model: string): Record<string, number> =>
  Object.fromEntries(drawnWindows(model).map((w) => [w, round5(tokens * WRITE_SHARE[w] * familyWeight(model))]));
/** The weekly window the pick prices a pin's rewrite in: the model's week where it draws one, else the 7-day window. */
const rewriteWindowOf = (model: string): string => (drawnWindows(model).includes('seven_day_fable') ? 'seven_day_fable' : 'seven_day');
/**
 * The tokens a request's body carries, the relay's estimate a fresh session's rewrite is priced from. The mock holds no
 * estimate of its own and reads the usage the request reported, so a request that ended with no usage, which the
 * router books at its body's estimate, books and later expires a zero share here.
 */
const bodyTokensOf = (r: RequestLog) => r.usage.input + r.usage.cacheRead + r.usage.cacheWrite5m + r.usage.cacheWrite1h;
/**
 * A fresh session's burn per hour: the pool's warm pins' median as the story's hour reads it. The drawn day's
 * decisions read no burn at all, so they book the rewrite alone.
 */
export const PRIOR_BURN: Record<string, number> = { five_hour: 0.01, seven_day: 0.0012, seven_day_fable: 0.0024 };
const WINDOW_OF: Record<string, string> = { '5h': 'five_hour', '7d': 'seven_day', '7d_fable': 'seven_day_fable' };

// The in-flight counts draw from a stream of their own, so no other draw of the story moves.
const inFlightRng = createRng(5_303);
// A boundary's hash is drawn once from the mock's own stream and keyed by the boundary, so one boundary carries one
// hash on every member and in every request of its session, stable across reloads.
const hashRng = createRng(6_607);
const hashes = new Map<string, string>();
const hashOf = (boundary: string): string => {
  let hash = hashes.get(boundary);
  if (hash === undefined) {
    hash = Array.from({ length: 64 }, () => Math.floor(hashRng.next() * 16).toString(16)).join('');
    hashes.set(boundary, hash);
  }
  return hash;
};

const accountOf = new Map(ACCOUNTS.map((a) => [a.id, a]));
/** A static boundary is the client's own prefix, which every session of a main conversation or a subagent shares. */
const staticBoundary = (agentId: string | null, depth: number) => `static|claude-code-${agentId === 'main' ? 'main' : 'subagent'}|${depth}`;
const fact = (key: string, value: unknown, unit: string | null = null, accountId: string | null = null): WalkFact => ({ accountId, key, unit, value });
const step = (name: string, verdict: string, facts: WalkFact[] = []): WalkStep => ({ facts, step: name, verdict });
const summaryFact = (key: string, value: number | string, unit: SummaryFact['unit']): SummaryFact => ({ entity: null, key, unit, value });

/** One decision as the story writes it or the builder reads it off a request row and its trace. */
interface Spec {
  /** The account placed, renewed on, or held on, or null for an exhaustion and a refusal. */
  accountId: string | null;
  at: number;
  attempt: number;
  /** The accounts the request tried before this decision. */
  attempted: string[];
  /**
   * The share of each plan window the sessions on an account burn per hour, where the story's hand-written decision
   * reads another figure than the account routes answer now.
   */
  burns?: Record<string, Record<string, number>>;
  exhaustion?: RecordOutcome['exhaustion'];
  /** A summary fact the story names where the router's own reading is not what the row tells. */
  fact?: SummaryFact[];
  /** The pin event reason of a failover placement, which moves the pin from the task rather than by a rule. */
  failover?: string;
  /** The account the pin was on before a move or a failover. */
  from?: string;
  /** The holds before this attempt's placement, summed as the placement's row names them. */
  heldBefore?: CohortHold;
  /** A hold for a slot names its place, and one for a spent window's reset the reset it waits for. */
  hold?: { cohort: string | null; place: 'cohort' | 'reset' | 'session_pin' | 'slow_down' | 'strategy_pick'; until?: number };
  /** The holds the request waited through before this decision, each with the attempt that waited. */
  holds?: RecordRequest['holds'];
  /** The slots the story fills where a hold reads every member at its cap. */
  inFlight?: Record<string, number>;
  kind: DecisionKind;
  /** The pin as step 1 found it, or why none was looked at. */
  pin: { cause: string; idleMs: number; warmth: 'cold' | 'refresh' | 'untouched' } | 'none' | PinReason;
  /** The moving pin's last prompt, which its rewrite is priced from, and its burn per hour by plan window. */
  pinFigures?: { burn: Record<string, number>; tokens: number };
  /** The pin's pace entry where its turns gave it a cycle or the slow-down stood a deadline on it. */
  pinPace?: { cycleMs: number | null; slowDownUntil: number | null };
  /** Per member, the deepest entry per rung and its match, where the story sets a deeper refused entry beside one. */
  prefix?: Record<string, Partial<RecordCandidate['prefix']>>;
  /** The windows a member reads where the story's own readings are not the walk back's. */
  readings?: Record<string, AttemptWindow[]>;
  refusal?: { code: string; kind: string; reason: string };
  request: RequestLog;
  round?: ShedRound;
  rule?: string;
  rung?: Placement | null;
  seq: number;
  /** Whether the story wrote the decision by hand, so it reads the burns the accounts carry. */
  story?: boolean;
}

/**
 * A pin of a round in the round's order, with its burn per hour on the window that fired. A pin whose model does not
 * draw that window leaves the round before the order with no burn.
 */
interface RoundPin {
  agentId: string;
  burn: number | null;
  cold: boolean;
  keyId: string;
  lastInputTokens: number;
  marked: boolean;
  model: string;
  note: string | null;
  sessionId: string;
  skip: string | null;
}

/** A round the story writes on the decision that ran it: its pins, and the excess the window that fired reads. */
interface ShedRound {
  marks: { agentId: string; keyId: string; model: string; sessionId: string }[];
  pins: RoundPin[];
}

/** The pool's settings as the record names them, by the pool fields they are read from. */
function settingsOf(pool: PoolSeed): RecordSettings {
  return {
    affinityMode: pool.affinityMode,
    affinityTtlSeconds: pool.affinityTtlSeconds,
    burnAverageSeconds: pool.burnAverageSeconds,
    inFlightCap: pool.inFlightCap,
    legacy: null,
    loadShedding: pool.loadShedding,
    migrateWhenSpent: pool.migrateWhenSpent,
    minSharedPrefixTokens: pool.minSharedPrefixTokens,
    overageMoveGraceMinutes: pool.overageMoveGraceMinutes,
    overagePolicy: pool.overagePolicy,
    poolUpdatedAt: pool.updatedAt,
    preferSubscription: pool.preferSubscription,
    retryInPlaceSeconds: pool.retryInPlaceSeconds,
    returnToPlan: pool.returnToPlan,
    returnToPlanDwellSeconds: pool.returnToPlanDwellSeconds,
    sessionPlacement: pool.sessionPlacement,
    shedCap: pool.shedCap,
    shedDwellSeconds: pool.shedDwellSeconds,
    slowDown: pool.slowDown,
    slowDownHoldSeconds: pool.slowDownHoldSeconds,
    slowDownStretchSeconds: pool.slowDownStretchSeconds,
    strategy: pool.strategy,
    subagentPlacement: pool.subagentPlacement,
    warmSessions: pool.warmSessions,
    windDownMinutes: pool.windDownMinutes,
    windowCeiling: pool.windowCeiling,
  };
}

/**
 * The share an older window's reading walks back to where no attempt names it. A draw would move the story's
 * streams.
 */
const OLDER_FACTOR = 0.8;

/** The attempt a dispatching decision sent, whose answer the drawer shows. */
const dispatchedAttempt = (spec: Spec) => (spec.kind === 'placed' || spec.kind === 'renewed' || spec.kind === 'moved' ? spec.request.attempts[spec.attempt - 1] : undefined);

/**
 * The windows a decision read on a member: the attempt's own reading on the account it went to, so the card and the
 * drawer agree, and the walk back at the decision's time elsewhere. A 429's head reports the limit past the reading
 * the decision read, so a rate-limited attempt's account reads the walk back too.
 */
function readingsOf(spec: Spec, accountId: string): AttemptWindow[] {
  const story = spec.readings?.[accountId];
  if (story) return story;
  const attempt = dispatchedAttempt(spec);
  if (attempt?.accountId === accountId && attempt.upstreamStatus !== 429) return attempt.windows;
  return oauthWindows(accountId, spec.at, () => OLDER_FACTOR);
}

/** The window a 429's cooldown names: the spent plan window its head read, else the retry-after. */
const windowOfReason = (reason: string) => (reason.startsWith('429 on 5h') ? 'five_hour' : reason.startsWith('429 on 7d') ? 'seven_day' : 'retry_after');

/** A failover's cause from the cooldown that failed it over, in the words its cooldown row carries. */
const causeOf = (reason: string, model: string): MoveCause =>
  reason === 'model_rejected'
    ? { cause: null, reason, scope: `model:${model}`, state: null, window: 'model_rejection' }
    : reason === 'fast_rate_limit'
      ? { cause: null, reason, scope: 'fast', state: null, window: 'fast' }
      : { cause: null, reason: 'rate_limit', scope: 'account', state: null, window: windowOfReason(reason) };

/** A cooldown row as the routing state holds it: the accounts' own rows and every row an attempt's answer set. */
interface CooldownRow {
  accountId: string;
  reason: string;
  scope: string;
  setAt: number;
  until: number;
  window: string;
}

const cooldownRows: CooldownRow[] = [
  ...ACCOUNTS.flatMap((a) =>
    a.cooldowns.map((c) => ({ accountId: a.id, reason: c.reason.startsWith('rate_limit') ? 'rate_limit' : c.reason, scope: c.scope, setAt: Date.parse(c.setAt), until: Date.parse(c.until), window: c.scope === 'fast' ? 'fast' : c.windowId === '5h' ? 'five_hour' : c.scope.startsWith('model:') ? 'model_rejection' : (c.windowId ?? 'retry_after') })),
  ),
  ...REQUESTS.flatMap((r) =>
    r.attempts.flatMap((a) => {
      if (!a.cooldownSet) return [];
      const setAt = Date.parse(a.startedAt) + (a.durationMs ?? 0);
      const until = Date.parse(a.cooldownSet.until);
      const reason = a.cooldownSet.reason;
      if (reason === 'model_rejected') return [{ accountId: a.accountId, reason, scope: `model:${r.canonicalModel}`, setAt, until, window: 'model_rejection' }];
      if (reason === 'fast_rate_limit') return [{ accountId: a.accountId, reason, scope: 'fast', setAt, until, window: 'fast' }];
      return [{ accountId: a.accountId, reason: 'rate_limit', scope: 'account', setAt, until, window: windowOfReason(reason) }];
    }),
  ),
];
// A story attempt set the row its account carries now, so one cooldown is listed once.
const COOLDOWNS: CooldownRow[] = [...new Map(cooldownRows.map((c) => [`${c.accountId}|${c.scope}|${c.until}`, c])).values()];

/** Whether a live row keeps this request off its account: a plan-window row holds a standard request alone. */
const holds = (c: CooldownRow, model: string, fast: boolean) => (c.scope === 'account' ? !(fast && PLAN_WINDOWS.some((w) => w === c.window)) : c.scope === 'fast' ? fast : c.scope === `model:${model}`);

const startOfDay = (at: number) => Math.floor(at / DAY) * DAY;
const startOfMonth = (at: number) => { const d = new Date(at); return Date.UTC(d.getUTCFullYear(), d.getUTCMonth(), 1); };
const endOfMonth = (at: number) => { const d = new Date(at); return Date.UTC(d.getUTCFullYear(), d.getUTCMonth() + 1, 1); };

/** A member's spend as the ledger reported it at `at`: research crossed its day's budget at 12:41:07. */
function budgetsAt(pool: PoolSeed, accountId: string, at: number): RecordBudget[] {
  const member = pool.members.find((m) => m.accountId === accountId);
  if (!member) return [];
  const spentOf = (period: 'day' | 'month') => {
    const now = member.spends.find((s) => s.period === period)?.spentUsd ?? 0;
    return accountId === 'research' && period === 'day' && at < clock(12, 41, 7) ? 19.62 : now;
  };
  const rows: RecordBudget[] = [];
  if (member.budgetDailyUsd !== null) rows.push({ budgetUsd: member.budgetDailyUsd, observedAt: iso(at), period: 'day', periodEnd: iso(startOfDay(at) + DAY), periodStart: iso(startOfDay(at)), spentUsd: spentOf('day') });
  if (member.budgetMonthlyUsd !== null) rows.push({ budgetUsd: member.budgetMonthlyUsd, observedAt: iso(at), period: 'month', periodEnd: iso(endOfMonth(at)), periodStart: iso(startOfMonth(at)), spentUsd: spentOf('month') });
  return rows;
}

/** The account's state at `at`: the Overflow workspace's credit balance ran out at 11:30:12. */
const stateAt = (a: Account, at: number): string => {
  if (a.id === 'overflow') return at < clock(11, 30, 12) ? 'ready' : 'funding_required';
  return a.status === 'reauth_required' || a.status === 'funding_required' || a.status === 'unpaired' ? a.status : 'ready';
};

/**
 * The usage credits a fast request draws: nightly's spend cap was raised at 11:30:12, and secondary's plan has
 * none.
 */
const creditsAt = (accountId: string, at: number): RecordCandidate['credits'] => {
  if (accountId === 'primary') return { blocked: null, disabledReason: null, enabled: true, observedAt: iso(at - MINUTE), utilization: 0.3 };
  if (accountId === 'secondary') return { blocked: 'extra_usage_disabled', disabledReason: null, enabled: false, observedAt: iso(at - MINUTE), utilization: null };
  if (accountId !== 'nightly') return null;
  const capped = at < clock(11, 30, 12);
  return { blocked: capped ? 'org_spend_cap_reached' : null, disabledReason: null, enabled: true, observedAt: iso(at - MINUTE), utilization: capped ? 1 : 0.31 };
};

const inOverageAt = (accountId: string, at: number) => accountId === 'nightly' && at >= OVERAGE_FROM;

/**
 * What keeps a member out of the eligible set, in the engine's order: the operator's state, an ended subscription, a
 * cooldown that holds the request, the fast credits, the budget, the fast lane's own limit, and last the attempt this
 * request already made, so a member it tried reads the cooldown that answered rather than the attempt.
 */
function obstacleOf(a: Account, pool: PoolSeed, at: number, model: string, fast: boolean, attempted: string[]): RecordCandidate['obstacle'] {
  const state = stateAt(a, at);
  if (state !== 'ready' || !a.routingEnabled) return { name: 'operator', reason: null, until: null, window: null };
  const endsAt = a.subscription?.endsAt ? Date.parse(a.subscription.endsAt) : null;
  if (endsAt !== null && endsAt <= at) return { name: 'subscription_ended', reason: null, until: null, window: null };
  const live = COOLDOWNS.filter((c) => c.accountId === a.id && c.setAt <= at && at < c.until && holds(c, model, fast));
  const cooldown = (c: CooldownRow) => ({ name: 'cooldown', reason: c.reason, until: iso(c.until), window: c.window });
  const blocking = live.find((c) => c.scope !== 'fast');
  if (blocking) return cooldown(blocking);
  const credits = fast ? creditsAt(a.id, at) : null;
  if (credits?.blocked) return { name: 'credits', reason: credits.blocked, until: null, window: null };
  // The engine names the reached budget whose period ends last, the one that keeps the member out longest.
  const reached = budgetsAt(pool, a.id, at).filter((b) => b.spentUsd !== null && b.spentUsd >= b.budgetUsd).sort((x, y) => Date.parse(y.periodEnd ?? '') - Date.parse(x.periodEnd ?? ''))[0];
  if (reached) return { name: 'budget', reason: reached.period, until: reached.periodEnd, window: null };
  const fastLimited = live.find((c) => c.scope === 'fast');
  if (fastLimited) return cooldown(fastLimited);
  if (attempted.includes(a.id)) return { name: 'attempted', reason: null, until: null, window: null };
  return null;
}

/** The prefix TTL the pool's rules give a request: the plan's per class and model, the metered rules on a key. */
function ttlOf(pool: PoolSeed, kind: Account['kind'], subagent: boolean, model: string): number {
  if (!pool.cacheTtl.enabled) return 300_000;
  const rules = kind === 'api_key' ? pool.cacheTtl.metered : pool.cacheTtl.plan;
  const rule = rules.models.find((m) => m.model === model);
  const ttl = subagent ? (rule?.subagent ?? rules.subagent) : (rule?.main ?? rules.main);
  return ttl === '1h' ? 3_600_000 : 300_000;
}

const entry = (boundary: string, o: { at: number; depth: number; prefixTokens: number; refusedBy?: string | null; ttlMs: number; writtenAt: number }): PrefixEntry => ({
  depth: o.depth,
  hash: hashOf(boundary),
  live: o.writtenAt + o.ttlMs > o.at || o.ttlMs === 0,
  prefixTokens: o.prefixTokens,
  refusedBy: o.refusedBy ?? null,
  ttlMs: o.ttlMs,
  writtenAt: iso(o.writtenAt),
});

const NO_PREFIX: RecordCandidate['prefix'] = { content: null, contentMatch: null, static: null, staticMatch: null, withMain: false };
const WINDOW_MS: Record<string, number> = { five_hour: 5 * HOUR, seven_day: 7 * DAY, seven_day_fable: 7 * DAY };

/** The burn per hour the account routes answer on each plan window of an account. */
const ACCOUNT_BURNS = new Map(
  ACCOUNTS.map((a) => [
    a.id,
    Object.fromEntries(
      [...a.windows, ...(a.fableWindow ? [a.fableWindow] : [])].flatMap((w) => {
        const name = WINDOW_OF[w.id];
        return name !== undefined && w.burn ? [[name, w.burn]] : [];
      }),
    ) as Record<string, number>,
  ]),
);
/**
 * The burns a decision read on an account: the story's hand-written decisions read the sessions the accounts carry, and
 * a drawn decision reads none, so it projects its reading.
 */
const burnsOf = (spec: Spec, accountId: string): Record<string, number> => (spec.story ? (spec.burns?.[accountId] ?? ACCOUNT_BURNS.get(accountId) ?? {}) : {});

/**
 * The instant a window's projection runs to: the 5-hour window's reset, or five hours out where no reset is read, and
 * the earlier of five hours out and the reset on a weekly window, each no later than the account's end.
 */
function horizonOf(name: string, resetsAt: string | null, end: number | null, at: number): number {
  const reset = resetsAt === null ? null : Date.parse(resetsAt);
  const bounds = name === 'five_hour' ? [reset ?? at + FIVE_HOURS, end] : [at + FIVE_HOURS, reset, end];
  return Math.max(at, Math.min(...bounds.filter((x): x is number => x !== null)));
}

/**
 * A read of an account's slow-down entry: its factor, the speed its holds realize, its stretch, when the stretch first
 * read over the threshold while it stays over, and the demand and the 5-hour reset it was read from.
 */
interface SlowRead {
  demand: number;
  factor: number;
  overSince: number | null;
  reset: number;
  speed: number;
  stretchMs: number;
}

const studioSlowed = accountOf.get('studio')?.slowDown ?? null;
const studioDemand = accountOf.get('studio')?.windows.find((w) => w.id === '5h')?.demand ?? 0;
/** The safety wait, the router's `SLOW_DOWN_WAIT_MS`: the stretch reads over the threshold this long before a round. */
const SLOW_DOWN_WAIT_MS = 120_000;

/**
 * The reads of studio's entry in work, each standing from its instant until the next. Each is a pinned selection's on
 * studio, its stretch the time to the reset less the time the demand takes from the utilization its decision reads to
 * the ceiling, and its factor one less the stretch over the time to the reset. The closed span reads under the
 * threshold in the last part of the window that resets at 10:29, its factor falling to 0.35 by the renewal after the
 * morning's hold, and the first turns after the reset read 1. The purge drops the entry an hour after the morning
 * session's last turn. The open span starts under the threshold at 13:48 and reads past it from the turn at 14:02:30,
 * and the renewal at 14:05 writes the read the account route answers now.
 */
const SLOW_READS: { accountId: string; from: number; poolId: string; read: SlowRead | null }[] = [
  { accountId: 'studio', from: clock(9, 12), poolId: 'work', read: { demand: 0.1286, factor: 0.91, overSince: null, reset: clock(10, 29), speed: 0.91, stretchMs: 420_000 } },
  { accountId: 'studio', from: clock(10, 11, 36), poolId: 'work', read: { demand: 0.1286, factor: 0.42, overSince: null, reset: clock(10, 29), speed: 0.42, stretchMs: 610_000 } },
  { accountId: 'studio', from: clock(10, 12), poolId: 'work', read: { demand: 0.1286, factor: 0.4, overSince: null, reset: clock(10, 29), speed: 0.4, stretchMs: 612_000 } },
  { accountId: 'studio', from: clock(10, 12, 36), poolId: 'work', read: { demand: 0.1286, factor: 0.35, overSince: null, reset: clock(10, 29), speed: 0.35, stretchMs: 639_600 } },
  { accountId: 'studio', from: clock(10, 38, 20), poolId: 'work', read: { demand: 0.1286, factor: 1, overSince: null, reset: clock(15, 29), speed: 1, stretchMs: 0 } },
  { accountId: 'studio', from: clock(10, 40, 36), poolId: 'work', read: { demand: 0.1286, factor: 1, overSince: null, reset: clock(15, 29), speed: 1, stretchMs: 0 } },
  { accountId: 'studio', from: clock(11, 41), poolId: 'work', read: null },
  { accountId: 'studio', from: clock(13, 48), poolId: 'work', read: { demand: 0.1, factor: 0.73, overSince: null, reset: clock(15, 29), speed: 0.73, stretchMs: 1_620_000 } },
  { accountId: 'studio', from: clock(14, 2, 30), poolId: 'work', read: { demand: 0.1286, factor: 0.5, overSince: clock(14, 2, 30), reset: clock(15, 29), speed: 0.5, stretchMs: 2_592_000 } },
  { accountId: 'studio', from: clock(14, 3, 36), poolId: 'work', read: { demand: 0.1286, factor: 0.5, overSince: clock(14, 2, 30), reset: clock(15, 29), speed: 0.5, stretchMs: 2_560_000 } },
  { accountId: 'studio', from: clock(14, 4, 18), poolId: 'work', read: { demand: 0.1286, factor: 0.5, overSince: clock(14, 2, 30), reset: clock(15, 29), speed: 0.5, stretchMs: 2_538_000 } },
  ...(studioSlowed === null
    ? []
    : [{ accountId: 'studio', from: clock(14, 5), poolId: 'work', read: { demand: studioDemand, factor: studioSlowed.factor, overSince: clock(14, 2, 30), reset: clock(15, 29), speed: studioSlowed.speed, stretchMs: studioSlowed.stretchMs } }]),
];

/**
 * The account's slow-down read on the shed line, on a pool that slows its pins and for an account it has read, and the
 * switch alone on a pool that does not.
 */
function slowFactsOf(source: RecordCandidate | undefined, pool: PoolSeed, at: number): WalkFact[] {
  if (!pool.slowDown) return [fact('slow_down', 'off')];
  const read = source ? slowReadAt(source.accountId, pool, at) : null;
  if (!source || read === null) return [];
  return [
    fact('slow_down', 'on'),
    fact('stretch_ms', read.stretchMs, 'ms'),
    fact('threshold_ms', pool.slowDownStretchSeconds * 1_000, 'ms'),
    fact('factor', read.factor),
    fact('speed', read.speed),
    fact('demand_five_hour', source.windows.five_hour?.demand ?? 0, 'percent_per_hour'),
    ...(read.overSince === null ? [] : [fact('over_since', read.overSince, 'time')]),
  ];
}

/** The account's slow-down read standing at `at`: none on a pool with the switch off or for an account it never slowed. */
const slowReadAt = (accountId: string, pool: PoolSeed, at: number): SlowRead | null =>
  pool.slowDown ? (SLOW_READS.filter((x) => x.accountId === accountId && x.poolId === pool.id && x.from <= at).pop()?.read ?? null) : null;

/** Every pool member's standing at the decision's time, eligible or not. */
function candidatesOf(spec: Spec, pool: PoolSeed): RecordCandidate[] {
  const r = spec.request;
  const fast = r.shape.requestedSpeed === 'fast';
  const at = spec.at;
  return pool.members.map((m) => {
    const a = accountOf.get(m.accountId)!;
    const overage = inOverageAt(a.id, at);
    const windows: RecordCandidate['windows'] = {};
    const entry = a.kind === 'oauth' ? slowReadAt(a.id, pool, at) : null;
    // Where an entry stands, the sessions burn the 5-hour window at the demand it read times the speed its holds
    // realize, so the window's demand reads the entry's own.
    const burn: Record<string, number> = a.kind === 'oauth' ? { ...burnsOf(spec, a.id), ...(spec.story && entry ? { five_hour: round5(entry.demand * entry.speed) } : {}) } : {};
    if (a.kind === 'oauth') {
      const head = r.attempts.some((x) => x.accountId === a.id) || a.id === 'primary';
      const ageMs = head ? 4_000 : 120_000;
      const end = endOf(a, at);
      for (const w of readingsOf(spec, a.id)) {
        const name = WINDOW_OF[w.id];
        if (name === undefined || w.utilization === null) continue;
        const ahead = aheadOf({ resetsAt: w.resetsAt, utilization: w.utilization }, WINDOW_MS[name], at, end);
        const horizon = horizonOf(name, w.resetsAt, end, at);
        windows[name] = {
          ageMs, ahead: name === 'seven_day_fable' ? null : Math.round(ahead * 1000) / 1000,
          // The 5-hour window's burn over the speed the account's holds realize, 1 with no entry in force.
          demand: name === 'five_hour' ? round5((burn[name] ?? 0) / (entry?.speed ?? 1)) : null,
          horizon: iso(horizon), observedAt: iso(at - ageMs),
          projection: round3(w.utilization + ((burn[name] ?? 0) * (horizon - at)) / HOUR), reserved: 0, reset: w.resetsAt, source: head ? 'head' : 'poll', utilization: w.utilization,
        };
      }
    } else {
      windows.worst_bucket = { ageMs: 4_000, ahead: null, demand: null, horizon: null, observedAt: iso(at - 4_000), projection: null, reserved: 0, reset: iso(at + 40_000), source: 'head', utilization: WORST_BUCKET[a.id] ?? 0.1 };
    }
    const cooldowns: RecordCooldown[] = COOLDOWNS.filter((c) => c.accountId === a.id && c.setAt <= at && at < c.until)
      .map((c) => ({ blocking: holds(c, r.canonicalModel, fast), reason: c.reason, scope: c.scope, until: iso(c.until), window: c.window }))
      .sort((x, y) => x.scope.localeCompare(y.scope));
    const budget = budgetsAt(pool, a.id, at);
    const sub = a.subscription;
    const endsAt = sub?.endsAt ? Date.parse(sub.endsAt) : null;
    // Drawn for every member, so a count the story sets moves no later draw. The account a placement took had a slot
    // free, since the cap passes a full one over. A token count takes no slot, so it lands on a full account too.
    const drawn = inFlightRng.int(0, 5);
    const drawnInFlight = spec.kind === 'placed' && r.kind !== 'count_tokens' && a.id === spec.accountId ? Math.min(drawn, pool.inFlightCap - 1) : drawn;
    const obstacle = obstacleOf(a, pool, at, r.canonicalModel, fast, spec.attempted);
    // Pace reads one window as the engine does, chosen by the model alone: the Fable week for a Fable request, else the
    // 7-day window, one the account has not read counting as untouched. An API key scores by its worst live bucket.
    const windowName = r.canonicalModel.includes('fable') ? 'seven_day_fable' : 'seven_day';
    const reading = a.kind === 'api_key' ? windows.worst_bucket : windows[windowName];
    const remaining = reading === undefined ? 1 : Math.max(0, 1 - reading.utilization);
    const reset = a.kind === 'api_key' ? null : (reading?.reset ?? null);
    const deadlineMs = [reset ? Date.parse(reset) : null, endsAt].filter((x): x is number => x !== null).reduce<number | null>((min, x) => (min === null || x < min ? x : min), null);
    const hours = a.kind === 'api_key' ? null : deadlineMs === null ? 168 : Math.max(1, (deadlineMs - at) / HOUR);
    const paceBurn = a.kind === 'api_key' ? 0 : (burn[windowName] ?? 0);
    return {
      accountId: a.id,
      allowanceKnown: a.kind === 'api_key' || windows.five_hour !== undefined,
      binding: null,
      budget,
      burn,
      cooldowns,
      credits: fast ? creditsAt(a.id, at) : null,
      enabled: a.routingEnabled,
      factor: entry?.factor ?? null,
      fit: null,
      inFlight: spec.inFlight?.[a.id] ?? drawnInFlight,
      inFlightCap: pool.inFlightCap,
      inOverage: overage,
      incarnation: 1,
      kind: a.kind,
      obstacle,
      overage: overage ? { observedAt: iso(at - 40_000), reset: a.overage?.reset ?? null, since: iso(OVERAGE_FROM), source: 'head' } : null,
      // The spare rate: the share left per hour to the deadline less the burn the account's sessions already spend.
      pace: pool.strategy === 'pace' ? { burn: paceBurn, deadline: deadlineMs === null ? null : iso(deadlineMs), end: endsAt === null ? null : iso(endsAt), hours: hours === null ? null : Math.round(hours * 10) / 10, remaining, reset, score: hours === null ? remaining : round5(remaining / hours - paceBurn), window: a.kind === 'api_key' ? null : windowName } : null,
      partition: null,
      planTier: sub?.rateLimitTier ?? null,
      prefix: { ...NO_PREFIX, ...spec.prefix?.[a.id] },
      priority: m.priority,
      rank: null,
      runwayMs: null,
      score: null,
      state: stateAt(a, at),
      term: a.kind === 'api_key' || !sub
        ? { cancelled: false, ended: false, endsAt: null, since: null, trialEndsAt: null, windingDown: false }
        : { cancelled: a.cancelled === true, ended: false, endsAt: sub.endsAt, since: sub.since, trialEndsAt: sub.trialEndsAt, windingDown: endsAt !== null && endsAt - at <= pool.windDownMinutes * MINUTE },
      tier: [pool.preferSubscription ? (a.kind === 'api_key' ? 2 : overage ? 1 : 0) : 0, m.priority, overage ? 1 : 0],
      verdict: obstacle ? 'out' : 'eligible',
      windows,
    };
  });
}

/** Primary's afternoon round, which finds no sibling with runway for either of its two pins. */
const NO_RUNWAY_ROUND = { accountId: 'primary', at: clock(14, 12, 30) };
/** Studio's round fired by the stretch, at the renewal its slowed request's hold led to. */
const STRETCH_ROUND = { accountId: 'studio', at: clock(14, 5) };
/** Nightly's morning round on its Fable week, fired while the 5-hour window is under the hold. */
const FABLE_WEEK_ROUND = { accountId: 'nightly', at: clock(8, 30) };
/** The story's four rounds, which a later decision on the account reads as within the dwell. */
const ROUNDS = [SHED_ROUND, NO_RUNWAY_ROUND, STRETCH_ROUND, FABLE_WEEK_ROUND];

/** What the gate read of one candidate for a pin: the verdict, the runway and its window, and the fit table's row. */
interface FitRead {
  binding: string | null;
  facts: WalkFact[];
  runwayMs: number | null;
  verdict: string;
}

const runwayOf = (c: Pick<RecordCandidate, 'runwayMs'>) => c.runwayMs ?? Number.POSITIVE_INFINITY;
/** The candidates with the longest runway, an unbounded one longest of all, ties kept in the set's order. */
const longestOf = <T extends Pick<RecordCandidate, 'runwayMs'>>(list: T[]): T[] => list.filter((c) => runwayOf(c) === Math.max(...list.map(runwayOf)));

/**
 * The router's fit of a pin on a candidate, over the plan windows the model draws: each window's runway is the time
 * until the prediction with the pin's rewrite, rising at the account's burn (its demand on the 5-hour window) and the
 * pin's, reaches the ceiling, zero where the rewrite alone does and unbounded with no burn. `fits` reaches the horizon
 * on every window, `rewrite_only` lands the rewrite but falls short on one, and `least_bad` is the rest, as is a window
 * a move cannot read. An account inside the pool's wind-down before its end reads `ending`, and an API-key account fits
 * while its worst bucket has room.
 */
function fitOf(c: RecordCandidate, at: number, pool: Pick<PoolSeed, 'windDownMinutes' | 'windowCeiling'>, model: string, tokens: number, pinBurn: Record<string, number>, move: boolean): FitRead {
  const ceiling = pool.windowCeiling;
  const read = (facts: WalkFact[], verdict: string, runwayMs: number | null = null, binding: string | null = null): FitRead => ({
    binding,
    facts: [...facts, fact('verdict', verdict)].map((f) => ({ ...f, accountId: c.accountId })),
    runwayMs,
    verdict,
  });
  const endsAt = c.term?.endsAt ? Date.parse(c.term.endsAt) : null;
  if (endsAt !== null && endsAt - at <= pool.windDownMinutes * MINUTE) return read([fact('end', endsAt, 'time')], 'ending');
  if (c.kind === 'api_key') return read([], (c.windows.worst_bucket?.utilization ?? 0) < 1 ? 'fits' : 'least_bad');
  const rewrite = rewriteOf(tokens, model);
  const projections: WalkFact[] = [];
  const rewrites: WalkFact[] = [];
  const reserved: WalkFact[] = [];
  const bounded: { runway: number; short: boolean; window: string }[] = [];
  let unread = false;
  let lands = true;
  let every = true;
  for (const w of drawnWindows(model)) {
    const x = c.windows[w];
    if (x === undefined || x.projection === null || x.horizon === null) {
      unread = unread || move;
      continue;
    }
    projections.push(fact(`projection_${w}`, x.projection, 'percent'));
    rewrites.push(fact(`rewrite_${w}`, rewrite[w] ?? 0, 'percent'));
    reserved.push(fact(`reserved_${w}`, x.reserved, 'percent'));
    const predicted = x.utilization + (x.reserved ?? 0);
    // The 5-hour window reads the demand, the burn over the account's speed, so a slowed account's holds lend it no
    // room. It reads the burn itself where no slow-down stands.
    const burn = (w === 'five_hour' && x.demand !== null ? x.demand : (c.burn[w] ?? 0)) + (pinBurn[w] ?? 0);
    const landed = predicted + (rewrite[w] ?? 0) < ceiling;
    lands = lands && landed;
    const runway = !landed ? 0 : burn <= 0 ? null : Math.round(((ceiling - predicted - (rewrite[w] ?? 0)) / burn) * HOUR);
    if (runway === null) continue;
    const short = runway < Date.parse(x.horizon) - at;
    every = every && !short;
    bounded.push({ runway, short, window: w });
  }
  const shortest = (list: typeof bounded) => [...list].sort((a, b) => a.runway - b.runway)[0];
  const binding = shortest(bounded.filter((b) => b.short)) ?? shortest(bounded);
  const verdict = unread ? 'least_bad' : every ? 'fits' : lands ? 'rewrite_only' : 'least_bad';
  return read([...projections, ...(binding ? [fact('runway_ms', binding.runway, 'ms')] : []), ...rewrites, ...reserved, ...(binding ? [fact('binding', binding.window)] : [])], verdict, binding?.runway ?? null, binding?.window ?? null);
}

/** The newest round any pool ran on the account by `at`. */
const newestRound = (accountId: string, at: number): number | null =>
  ROUNDS.filter((x) => x.accountId === accountId && x.at <= at).reduce<number | null>((newest, x) => (newest === null || x.at > newest ? x.at : newest), null);

/** A pin as the shed line names it in its marks and its skips. */
const pinText = (p: { agentId: string; keyId: string; model: string; sessionId: string }) => `${p.keyId}/${p.sessionId}/${p.agentId}/${p.model}`;

/** A walk of the five rules: the lines, the rule that fired, its destinations after the gate and before it. */
interface Move {
  destinations: RecordCandidate[];
  fired: string | null;
  /** Whether the spent rule held the request in place for the reset. */
  held: boolean;
  lines: WalkStep[];
  reached: RecordCandidate[];
}

/**
 * Step 4 as the engine walks it for a live pin, each rule on the facts the record holds: `no` with the fact that kept
 * it, or `fired` with its destinations and the gate's fit table after it, `not_run` for every rule after it. A rule
 * whose reason holds with no sibling to take the pin says so and the walk goes on. A destination is an eligible
 * sibling holding allowance, and the gate answers the fitting ones, else those whose rewrite lands with the longest
 * runway, else the whole set, a member at its cap in neither of the first two, each in the set's order for the
 * strategy to pick from, the longest runway breaking the strategy's tie among the fitting ones. The shed rule takes the
 * fitting destinations alone, and the spent rule holds a request whose spent window resets within the retry bound.
 */
function moveOf(spec: Spec, candidates: RecordCandidate[], pool: PoolSeed, source: RecordCandidate | undefined, pin: RecordPin): Move {
  const r = spec.request;
  const at = spec.at;
  const fast = r.shape.requestedSpeed === 'fast';
  const preferPlan = pool.overagePolicy === 'prefer_plan';
  const dwellMs = pool.returnToPlanDwellSeconds * 1_000;
  const windows = drawnWindows(r.canonicalModel);
  const holdsAllowance = (c: RecordCandidate) => c.kind === 'api_key' || windows.every((w) => (c.windows[w]?.utilization ?? 0) < 1);
  const allowanceKnown = (c: RecordCandidate) => c.kind === 'api_key' || windows.every((w) => c.windows[w] !== undefined);
  const siblings = candidates.filter((c) => c.obstacle === null && c.accountId !== source?.accountId);
  const lines: WalkStep[] = [];
  const no = (i: number, facts: WalkFact[]) => lines.push(step(`move:${RULES[i]}`, 'no', facts));
  // The moving pin's last prompt and burn, which the gate prices on every candidate.
  const tokens = spec.pinFigures?.tokens ?? bodyTokensOf(r);
  const pinBurn = spec.pinFigures?.burn ?? (spec.story ? PRIOR_BURN : {});
  const gate = (reached: RecordCandidate[], fitsOnly: boolean): { answered: RecordCandidate[]; step: WalkStep } => {
    const reads = reached.map((c) => {
      const read = fitOf(c, at, pool, r.canonicalModel, tokens, pinBurn, true);
      c.fit = read.verdict;
      c.runwayMs = read.runwayMs;
      c.binding = read.binding;
      return read;
    });
    // The story names where a move lands, so a rival the gate would answer ahead of it reads full, as the cap passes
    // it over: a better order, or a longer runway where the order narrows to the longest.
    const chosen = spec.kind === 'moved' ? reached.find((c) => c.accountId === spec.accountId) : undefined;
    if (chosen) {
      const order = (c: RecordCandidate) => (c.fit === 'fits' ? 0 : c.fit === 'rewrite_only' ? 1 : 2);
      const narrows = (c: RecordCandidate) => order(c) === 1;
      for (const c of reached) if (c !== chosen && (order(c) < order(chosen) || (order(c) === order(chosen) && narrows(c) && runwayOf(c) > runwayOf(chosen)))) c.inFlight = c.inFlightCap;
    }
    const open = (c: RecordCandidate) => c.inFlight < c.inFlightCap;
    // Each order keeps the set's order for the strategy to pick from. The fitting set goes on whole, the shed rule's
    // too, since the strategy ranks it and the runway only breaks its tie. The rewrite-only order narrows to the longest
    // runway, ties kept.
    const fits = reached.filter((c) => c.fit === 'fits' && open(c));
    const rewriteOnly = longestOf(reached.filter((c) => c.fit === 'rewrite_only' && open(c)));
    const left = reached.filter((c) => c.fit !== 'ending');
    const [verdict, answered] = fits.length ? ['fits', fits] : fitsOnly ? ['none', []] : rewriteOnly.length ? ['rewrite_only', rewriteOnly] : left.length ? ['least_bad', left] : ['none', []];
    return { answered, step: step('fit', verdict, reads.flatMap((x) => x.facts)) };
  };
  // The rule fires where the gate answers a sibling it keeps holding allowance. Otherwise its line says it found
  // none, with the members the end exclusion dropped.
  const tryRule = (i: number, facts: WalkFact[], keep: (c: RecordCandidate) => boolean, fitsOnly = false): Move | null => {
    const reached = siblings.filter((c) => keep(c) && holdsAllowance(c));
    const ending = () => reached.filter((c) => c.fit === 'ending').map((c) => c.accountId);
    if (reached.length === 0) {
      no(i, [...facts, fact('no_destination', true), fact('ending', [])]);
      return null;
    }
    const { answered, step: fitStep } = gate(reached, fitsOnly);
    if (answered.length === 0) {
      no(i, [...facts, fact('no_destination', true), fact('ending', ending())]);
      lines.push(fitStep);
      return null;
    }
    lines.push(step(`move:${RULES[i]}`, 'fired', [...facts, fact('destinations', answered.map((c) => c.accountId))]));
    for (const later of RULES.slice(i + 1)) lines.push(step(`move:${later}`, 'not_run'));
    lines.push(fitStep);
    return { destinations: answered, fired: RULES[i], held: false, lines, reached };
  };
  const none: Move = { destinations: [], fired: null, held: false, lines, reached: [] };

  if (fast) no(0, [fact('fast', true)]);
  else if (!pool.migrateWhenSpent) no(0, [fact('migrate_when_spent', false)]);
  else if (!source || holdsAllowance(source)) no(0, [fact('holds_allowance', true)]);
  else if (spec.hold?.place === 'reset' && spec.hold.until !== undefined) {
    // A reset inside the retry bound costs less than a cold rewrite, so the request waits for it in place.
    const spent = windows.find((w) => (source.windows[w]?.utilization ?? 0) >= 1) ?? 'five_hour';
    const reset = source.windows[spent]?.reset;
    lines.push(step('move:spent', 'held', [fact('holds_allowance', false), fact('reset', reset ? Date.parse(reset) : spec.hold.until, 'time'), fact('window', spent), fact('wait_ms', spec.hold.until - at, 'ms')]));
    for (const later of RULES.slice(1)) lines.push(step(`move:${later}`, 'not_run'));
    return { ...none, held: true };
  } else {
    const move = tryRule(0, [fact('holds_allowance', false)], (c) => allowanceKnown(c) && !(preferPlan && c.inOverage));
    if (move) return move;
  }

  if (fast) no(1, [fact('fast', true)]);
  else if (!preferPlan) no(1, [fact('overage_policy', pool.overagePolicy)]);
  else if (source?.kind !== 'oauth') no(1, [fact('kind', source?.kind ?? 'unknown')]);
  else if (!source.inOverage) no(1, [fact('in_overage', false)]);
  else {
    const reset = source.overage?.reset ? Date.parse(source.overage.reset) : null;
    const graceMs = pool.overageMoveGraceMinutes * MINUTE;
    if (reset !== null && reset - at <= graceMs) no(1, [fact('in_overage', true), fact('reset_in_grace', reset - at, 'ms'), fact('grace_ms', graceMs, 'ms')]);
    else {
      const move = tryRule(1, [fact('in_overage', true)], (c) => c.kind === 'oauth' && !c.inOverage);
      if (move) return move;
    }
  }

  const cold = typeof spec.pin === 'object' && spec.pin.warmth === 'cold';
  // The mode reads on every line: `never` keeps the pin with no age read, `when_cold` moves a cold pin alone, and
  // `always` moves a warm one too once it has stayed the return dwell.
  const mode = pool.returnToPlan;
  if (source?.kind !== 'api_key') no(2, [fact('kind', source?.kind ?? 'unknown'), fact('mode', mode)]);
  else {
    const dwellAge = at - Date.parse(pin.createdAt ?? iso(at));
    const read = [fact('mode', mode), fact('cold', cold), fact('dwell_age_ms', dwellAge, 'ms')];
    const facts = mode === 'never' ? [fact('mode', mode)] : mode === 'when_cold' ? read : [...read, fact('dwell_ms', dwellMs, 'ms')];
    const returns = mode === 'never' ? false : mode === 'when_cold' ? cold : cold || dwellAge >= dwellMs;
    if (!returns) no(2, facts);
    else {
      const move = tryRule(2, facts, (c) => c.kind === 'oauth' && !c.inOverage);
      if (move) return move;
    }
  }

  if (!cold) no(3, [fact('cold', false)]);
  else if (!source?.term?.windingDown) no(3, [fact('winding_down', false)]);
  else {
    const move = tryRule(3, [fact('cold', true), fact('winding_down', true)], (c) => !c.term?.windingDown);
    if (move) return move;
  }

  if (!pool.loadShedding) {
    lines.push(step('move:shed', 'not_run', [fact('round', 'off')]));
    return none;
  }
  if (!source) {
    no(4, []);
    return none;
  }
  // The one trigger: the first plan window, in the windows' order, whose projection at its horizon reads over the
  // ceiling, or on a pool that slows its pins the 5-hour window's stretch past the threshold once the safety wait has
  // passed, a 5-hour window under the hold yielding to the next. The line names every projection it read, the 5-hour
  // window's age and hold inside the hold, and how long the stretch has read over while the wait holds.
  const facts: WalkFact[] = [];
  for (const w of windows) {
    const x = source.windows[w];
    if (x?.projection === null || x?.projection === undefined || x.horizon === null) continue;
    facts.push(fact(`projection_${w}`, x.projection, 'percent'), fact(`horizon_${w}`, Date.parse(x.horizon), 'time'));
  }
  const held = fiveHourHeld(source, pool, at);
  if (held) facts.push(fact('age_five_hour', held.ageMs, 'ms'), fact('hold_five_hour', held.holdMs, 'ms'));
  facts.push(fact('ceiling', pool.windowCeiling, 'percent'));
  const overSince = slowReadAt(source.accountId, pool, at)?.overSince ?? null;
  const waiting = overSince !== null && at - overSince < SLOW_DOWN_WAIT_MS;
  if (waiting) facts.push(fact('over_ms', at - overSince, 'ms'));
  const trigger = pool.slowDown
    ? (stretchTriggerOf(source, pool, at) ?? triggerOf(source, windows.filter((w) => w !== 'five_hour'), pool.windowCeiling, at, pool))
    : triggerOf(source, windows, pool.windowCeiling, at, pool);
  // The hold holds back the round alone, as a wait does, so a held line goes on to the cooldown and the mark.
  if (trigger === null && !waiting && held === null) {
    no(4, facts);
    return none;
  }
  if (trigger !== null) {
    facts.push(fact('trigger', trigger.window), fact('fired_by', trigger.slowed ? 'slow_down' : 'projection'), fact('excess', trigger.excess, 'percent_per_hour'));
    if (spec.round) {
      const round = roundOf(spec.round, trigger.excess, pool);
      facts.push(fact('round', 'ran'), fact('target', round.target, 'percent_per_hour'), fact('marks', spec.round.marks.map(pinText)), fact('skipped', round.skipped));
    } else facts.push(fact('round', 'within_dwell'));
  }
  if (source.cooldowns.some((c) => c.blocking)) {
    no(4, [...facts, fact('cooldown', true)]);
    return none;
  }
  const marked = spec.kind === 'moved' && spec.rule === 'shed';
  facts.push(fact('marked', marked));
  if (!marked) {
    no(4, facts);
    return none;
  }
  // The no-return test keeps a shed pin off the account that shed it, which is the source here.
  return tryRule(4, facts, () => true, true) ?? none;
}

/**
 * The 5-hour window's start off the source's reading, as the router reads it, and whether `at` lies under the hold:
 * the reset less five hours where it lies ahead, the passed reset itself, and no hold where no reset is read. The burn
 * average still carries the previous window's burst over the reset, so no 5-hour round fires while the window is younger
 * than twice it.
 */
const fiveHourHeld = (source: RecordCandidate, pool: PoolSeed, at: number): { ageMs: number; holdMs: number } | null => {
  const reset = source.windows.five_hour?.reset;
  if (reset === null || reset === undefined) return null;
  const resetAt = Date.parse(reset);
  const start = resetAt > at ? resetAt - FIVE_HOURS : resetAt;
  const holdMs = 2 * pool.burnAverageSeconds * 1_000;
  // A start at or after `at` is no reading of a fresh window.
  return start < at && at - start < holdMs ? { ageMs: at - start, holdMs } : null;
};

/**
 * The 5-hour round under the slow-down: the stretch has read over the threshold for the safety wait, the horizon
 * moved to the reset less the threshold lies ahead, and the window is past the hold. The excess is the demand's past the
 * ceiling there, per hour to it.
 */
function stretchTriggerOf(source: RecordCandidate, pool: PoolSeed, at: number): { excess: number; slowed: true; window: string } | null {
  const overSince = slowReadAt(source.accountId, pool, at)?.overSince ?? null;
  const x = source.windows.five_hour;
  if (overSince === null || at - overSince < SLOW_DOWN_WAIT_MS || x?.reset === null || x?.reset === undefined || x.demand === null || fiveHourHeld(source, pool, at) !== null) return null;
  const hours = (Date.parse(x.reset) - pool.slowDownStretchSeconds * 1_000 - at) / HOUR;
  const excess = round5((x.utilization + x.demand * hours - pool.windowCeiling) / hours);
  return hours > 0 && excess > 0 ? { excess, slowed: true, window: 'five_hour' } : null;
}

/**
 * The window a round on `source` fires on, and the excess per hour that lands its projection on the ceiling. A 5-hour
 * window under the hold yields to the next window in the order.
 */
function triggerOf(source: RecordCandidate, windows: string[], ceiling: number, at: number, pool: PoolSeed): { excess: number; slowed?: true; window: string } | null {
  const held = fiveHourHeld(source, pool, at) !== null;
  for (const w of windows) {
    const x = source.windows[w];
    if (x?.projection === null || x?.projection === undefined || x.horizon === null || x.projection <= ceiling || (w === 'five_hour' && held)) continue;
    const hours = Math.max((Date.parse(x.horizon) - at) / HOUR, 1 / 60);
    return { excess: round5((x.projection - ceiling) / hours), window: w };
  }
  return null;
}

/**
 * A round's figures off its pins: the burn it could remove, its target under the cap, what its marks removed, the
 * pins it skipped as the shed line names them, and its outcome with the reason a round that marked nothing records.
 */
function roundOf(round: ShedRound, excess: number, pool: PoolSeed) {
  const burnSum = round5(round.pins.reduce((n, p) => n + (p.burn ?? 0), 0));
  const skips = round.pins.flatMap((p) => (p.skip === null ? [] : [p.skip]));
  const reason = round.marks.length > 0 ? null : (['no_runway', 'unpriced', 'last_pin', 'standing'].find((w) => skips.includes(w)) ?? 'unpriced');
  return {
    burnSum,
    outcome: round.marks.length > 0 ? `marked ${round.marks.length}` : 'nothing_to_mark',
    reason,
    removed: round5(round.pins.filter((p) => p.marked).reduce((n, p) => n + (p.burn ?? 0), 0)),
    skipped: round.pins.flatMap((p) => (p.skip === null ? [] : [`${pinText(p)}: ${p.skip}`])),
    target: round5(Math.min(excess, pool.shedCap * burnSum)),
  };
}

const tierKey = (t: [number, number, number]) => t.join('/');

/**
 * The pick among the members the partitions kept in the chosen account's tier, the set the engine's `choose` orders:
 * the chosen account first, the rest by score.
 */
function pickOf(candidates: RecordCandidate[], kept: RecordCandidate[], chosen: string | null, strategy: string, source: string | undefined): { runnerUp: Score | null; score: Score | null; tierSize: number } {
  const winner = candidates.find((c) => c.accountId === chosen);
  if (!winner) return { runnerUp: null, score: null, tierSize: 0 };
  const unit = (c: RecordCandidate) => (c.kind === 'api_key' ? 'percent' : 'percent_per_hour');
  const scoreOf = (c: RecordCandidate) => c.pace?.score ?? 0;
  const rest = kept.filter((c) => c !== winner && c.accountId !== source && tierKey(c.tier) === tierKey(winner.tier)).sort((x, y) => scoreOf(y) - scoreOf(x));
  winner.rank = 1;
  winner.score = strategy === 'pace' ? scoreOf(winner) : null;
  rest.forEach((c, i) => {
    c.rank = i + 2;
    c.score = strategy === 'pace' ? scoreOf(c) : null;
  });
  const tierSize = rest.length + 1;
  if (strategy !== 'pace') return { runnerUp: null, score: null, tierSize };
  const best = rest[0];
  return { runnerUp: best ? { accountId: best.accountId, score: scoreOf(best), unit: unit(best) } : null, score: { accountId: winner.accountId, score: scoreOf(winner), unit: unit(winner) }, tierSize };
}

/** Whether one tier ranks before another, compared member by member as the engine orders them. */
function tierBefore(x: [number, number, number], y: [number, number, number]): boolean {
  const i = x.findIndex((n, j) => n !== y[j]);
  return i >= 0 && x[i] < y[i];
}

/**
 * The story names the member each placement chose, drawn or written by hand, and the record reads the walk back's
 * windows, so the choice holds by the slots: where the member does not fit, every member that does is full, so the cap
 * passes it over before the no-fit partition, and where the strategy picks, every member of a better tier and under
 * Pace every tier mate scoring above the chosen member is full too. A token count takes no slot, so the request
 * generator places it on the engine's pick instead.
 */
function capTheRivals(spec: Spec, candidates: RecordCandidate[], pool: PoolSeed): void {
  const winner = candidates.find((c) => c.accountId === spec.accountId);
  if (!winner || spec.request.kind === 'count_tokens') return;
  const top = pool.strategy === 'pace' ? winner.pace?.score : undefined;
  const outscores = (c: RecordCandidate) => top !== undefined && c.pace !== null && c.pace.score > top;
  // A move's gate reads its own fits later in the walk, so only a placement's choice is held to the fresh fit here.
  const fits = (c: RecordCandidate) => spec.kind === 'placed' && freshFitOf(spec, c, pool).verdict === 'fits';
  const unfit = spec.kind === 'placed' && !fits(winner);
  for (const c of candidates) {
    if (c === winner || c.obstacle !== null || c.accountId === spec.from) continue;
    const picked = !spec.rung && (tierBefore(c.tier, winner.tier) || (tierKey(c.tier) === tierKey(winner.tier) && outscores(c)));
    if (picked || (unfit && fits(c))) c.inFlight = c.inFlightCap;
  }
}

/** The burn a fresh session brings: the prior on the story's hand-written decisions, none on a drawn one. */
const priorOf = (spec: Spec) => (spec.story ? PRIOR_BURN : {});

/** A fresh session's fit on a member, the body's rewrite with the prior's burn. */
const freshFitOf = (spec: Spec, c: RecordCandidate, pool: PoolSeed) => fitOf(c, spec.at, pool, spec.request.canonicalModel, bodyTokensOf(spec.request), priorOf(spec), false);

/**
 * The no-fit partition's reading of a placement's members, drawn or written by hand, as the engine reads it: every
 * member the spent and cap partitions left, or the whole set the cap holds for a slot, and none of the members they
 * dropped. A drawn token count and a drawn hold read none: the request generator draws their account without
 * the fit, and neither takes a slot the cap could keep that choice by.
 */
function readPlacementFits(spec: Spec, candidates: RecordCandidate[], pool: PoolSeed): void {
  const r = spec.request;
  if (!spec.story && (r.kind === 'count_tokens' || spec.kind === 'held')) return;
  const left = candidates.filter((c) => c.obstacle === null && keepsSpent(c, pool, r));
  const free = left.filter((c) => r.kind === 'count_tokens' || c.inFlight < c.inFlightCap);
  for (const c of free.length > 0 ? free : left) {
    const read = freshFitOf(spec, c, pool);
    c.fit = read.verdict;
    c.runwayMs = read.runwayMs;
    c.binding = read.binding;
  }
}

/** A Fable request draws on its own week and both shared windows, so a reading at or past 1 on any of them is spent. */
const spentForFable = (utilization: (window: string) => number | null | undefined) => [...PLAN_WINDOWS, 'seven_day_fable'].some((w) => (utilization(w) ?? 0) >= 1);

/** One reading of whether extra usage funds a spent member, so the partition and a run-out's servable set agree. */
const fundedBy = (pool: PoolSeed, credits: RecordCandidate['credits']) => pool.overagePolicy === 'allow' && credits !== null && credits.blocked === null;

/**
 * Whether the spent partition keeps a member: a request of a model with no week of its own never drops one, and a
 * Fable request keeps a spent member only where an `allow` pool reads its extra usage unblocked. A member without a
 * credits reading is not funded.
 */
const keepsSpent = (c: RecordCandidate, pool: PoolSeed, r: RequestLog) => !r.canonicalModel.includes('fable') || !spentForFable((w) => c.windows[w]?.utilization) || fundedBy(pool, c.credits);

/**
 * The four partitions' facts in the engine's order, and the members left after them, which the step's verdict reads.
 */
function narrowed(set: RecordCandidate[], pool: PoolSeed, r: RequestLog, moving: boolean): { facts: WalkFact[]; left: RecordCandidate[] } {
  const facts: WalkFact[] = [];
  let left = set;
  const narrow = (key: string, keep: (c: RecordCandidate) => boolean) => {
    const kept = left.filter(keep);
    if (kept.length > 0) {
      facts.push(fact(key, left.length - kept.length));
      left = kept;
    } else facts.push(left.length > 0 ? fact(key, 'fell_through') : fact(key, 0));
  };
  if (r.canonicalModel.includes('fable')) {
    // A spent member is out rather than passed over, so this partition never falls through.
    const kept = left.filter((c) => keepsSpent(c, pool, r));
    facts.push(fact('spent', left.length - kept.length));
    left = kept;
  } else facts.push(fact('spent', 'off'));
  // A token count takes no slot, so no cap holds it. With every member at its cap the request is held for a slot and
  // the set stays whole for the pick to name the account it waits on.
  const underCap = left.filter((c) => r.kind === 'count_tokens' || c.inFlight < c.inFlightCap);
  if (underCap.length === 0 && left.length > 0) facts.push(fact('at_cap', 'held'));
  else narrow('at_cap', (c) => underCap.includes(c));
  // A move's destinations are the set its gate ordered, read with the moving pin already, so they stay as it left
  // them. A member the partition read no fit for counts as fitting.
  narrow('no_fit', (c) => moving || c.fit === null || c.fit === 'fits');
  narrow('winding_down', (c) => !c.term?.windingDown);
  return { facts, left };
}

/**
 * The four partition facts over `set` as the engine narrows it: each partition keeps the members that pass and counts
 * the rest. A spent member drops even when none is left. Any other partition that would empty a non-empty set keeps it
 * whole and reads `fell_through` (`held` for the cap, as the engine words it), and the spent partition reads `off`
 * for a model with no week of its own. `moving` is a move's destination set, which the no-fit partition keeps whole.
 */
export function partitionsOf(set: RecordCandidate[], pool: PoolSeed, r: RequestLog, moving = false): WalkFact[] {
  return narrowed(set, pool, r, moving).facts;
}

/** A pin as the routing state holds it while the builder walks the day in order. */
interface PinState {
  accountId: string;
  createdAt: number;
  epoch: number;
  lastRefreshAt: number;
  lastUsedAt: number;
}
const pins = new Map<string, PinState>();
let epochs = 0;
const pinKeyOf = (r: RequestLog) => `${r.keyId}|${r.sessionId}|${r.agentId}|${r.canonicalModel}`;
const laneOf = (r: RequestLog) => `${r.canonicalModel}|${r.shape.requestedSpeed === 'fast' ? 'fast' : 'standard'}|${r.shape.thinkingType ?? 'none'}|${r.shape.effort ?? '-'}`;

/**
 * What a placement or a move books on the account it chose until the usage replaces it: the rewrite of the pin's last
 * prompt, or of the body for a fresh session, and the burn it brings, per plan window the model draws. A count, a
 * probe and a fast request book nothing, and neither does an API-key account, which has no plan window.
 */
function reservationOf(spec: Spec, chosen: RecordCandidate | undefined): { reservation: NonNullable<RecordOutcome['reservation']>; step: WalkStep } | null {
  const r = spec.request;
  if ((spec.kind !== 'placed' && spec.kind !== 'moved') || chosen?.kind !== 'oauth' || r.kind !== 'inference' || r.shape.requestedSpeed === 'fast') return null;
  const id = chosen.accountId;
  const rewrite = rewriteOf(spec.kind === 'moved' ? (spec.pinFigures?.tokens ?? bodyTokensOf(r)) : bodyTokensOf(r), r.canonicalModel);
  const brought = spec.kind === 'moved' && spec.pinFigures ? spec.pinFigures.burn : spec.story ? PRIOR_BURN : {};
  const burn = Object.fromEntries(drawnWindows(r.canonicalModel).flatMap((w) => (brought[w] === undefined ? [] : [[w, brought[w]]])));
  const expiresAt = spec.at + RESERVATION_MS;
  const facts = [
    fact('account_id', id, null, id),
    ...Object.entries(rewrite).map(([w, x]) => fact(`reserved_${w}`, x, 'percent', id)),
    ...Object.entries(burn).map(([w, x]) => fact(`burn_${w}`, x, 'percent_per_hour', id)),
    fact('expires_at', expiresAt, 'time', id),
    fact('attempt', spec.attempt, 'count', id),
  ];
  return { reservation: { accountId: id, burn, expiresAt: iso(expiresAt), rewrite }, step: step('reserve', 'booked', facts) };
}

/** The mark the newest round on an account wrote by `at`: its time, its account, and the window that fired it. */
function markOf(accountId: string, at: number): { at: number; on: string; window: string } | null {
  const round = newestRound(accountId, at);
  return round === null ? null : { at: round, on: accountId, window: 'five_hour' };
}

/** What the router recorded for one decision: the record and the row the list route answers. */
function build(spec: Spec): { record: DecisionRecord; summary: DecisionSummary } {
  const r = spec.request;
  const pool = poolById.get(r.poolId ?? '')!;
  const fast = r.shape.requestedSpeed === 'fast';
  const candidates = spec.kind === 'refused' ? [] : candidatesOf(spec, pool);
  // A hold for a spent window's reset ends the walk at the spent rule, so it reads no placement's partitions.
  const resetHold = spec.kind === 'held' && spec.hold?.place === 'reset';
  // A slow-down hold waits on the pin the move rules left in place, before its renewal, so it reads no placement either.
  const slowHold = spec.kind === 'held' && spec.hold?.place === 'slow_down';
  if (spec.kind === 'placed' || spec.kind === 'moved') capTheRivals(spec, candidates, pool);
  if ((spec.kind === 'placed' || spec.kind === 'held' || spec.kind === 'exhausted') && !resetHold && !slowHold) readPlacementFits(spec, candidates, pool);
  const portable = r.client !== 'other';
  const pinKey = pinKeyOf(r);
  const pinning = pool.affinityTtlSeconds > 0 && r.sessionId !== null && (typeof spec.pin === 'object' || spec.pin === 'none');
  const held = pins.get(pinKey);
  const walk: WalkStep[] = [];
  let pin: RecordPin | null = null;
  let pinReason: PinReason | null = null;
  if (typeof spec.pin === 'string' && spec.pin !== 'none') {
    pinReason = spec.pin;
    if (spec.kind !== 'refused') walk.push(step('pin', 'none', [fact('reason', spec.pin)]));
  } else if (spec.pin === 'none') {
    pin = { accountId: null, createdAt: null, cycleMs: null, epoch: null, found: 'none', idleMs: null, incarnation: null, lane: null, lastRefreshAt: null, lastUsedAt: null, marked: false, markedAt: null, markedOn: null, markedWindow: null, refreshAgeMs: null, released: [], shed: null, slowDownUntil: null, warmth: null, warmthCause: null };
    walk.push(step('pin', 'none', [fact('found', 'none')]));
  } else {
    const on = spec.from ?? spec.accountId ?? '';
    // A drawn trace names where the pin lived. A pin the day's rows last left past its window was used by a request
    // the day does not hold, so the trace's own idle age stands.
    const recent = held !== undefined && held.accountId === on && spec.at - held.lastUsedAt <= pool.affinityTtlSeconds * 1_000;
    const found = recent ? held : { accountId: on, createdAt: held?.createdAt ?? spec.at - 40 * MINUTE, epoch: held?.epoch ?? (epochs += 1), lastRefreshAt: spec.at - spec.pin.idleMs, lastUsedAt: spec.at - spec.pin.idleMs };
    pins.set(pinKey, found);
    const idleMs = spec.at - found.lastUsedAt;
    const released = spec.failover ? [spec.failover === 'fast_lane' ? 'fast_rate_limit' : 'cooldown'] : [];
    // A shed move's pin carries the mark the round on its account wrote.
    const mark = spec.rule === 'shed' ? markOf(found.accountId, spec.at) : null;
    pin = {
      accountId: found.accountId, createdAt: iso(found.createdAt), cycleMs: spec.pinPace?.cycleMs ?? null, epoch: found.epoch, found: 'live', idleMs, incarnation: 1, lane: laneOf(r), lastRefreshAt: iso(spec.at - idleMs), lastUsedAt: iso(spec.at - idleMs),
      marked: mark !== null, markedAt: mark === null ? null : iso(mark.at), markedOn: mark?.on ?? null, markedWindow: mark?.window ?? null, refreshAgeMs: idleMs, released, shed: null,
      slowDownUntil: spec.pinPace?.slowDownUntil === null || spec.pinPace?.slowDownUntil === undefined ? null : iso(spec.pinPace.slowDownUntil), warmth: spec.pin.warmth, warmthCause: spec.pin.cause,
    };
    walk.push(step('pin', 'match', [fact('found', 'live'), fact('account_id', found.accountId, null, found.accountId), fact('epoch', found.epoch, 'count'), fact('idle_ms', idleMs, 'ms'), fact('refresh_age_ms', idleMs, 'ms'), fact('warmth', spec.pin.warmth), fact('cause', spec.pin.cause), fact('marked', pin.marked)]));
  }
  if (spec.kind === 'refused') {
    walk.push(step('admission', 'refused', [fact('reason', spec.refusal?.reason ?? '')]), step('selection', 'not_run'));
  } else {
    walk.push(step('eligible', candidates.some((c) => c.obstacle === null) ? 'match' : 'none', candidates.map((c) => fact('standing', c.obstacle?.name ?? 'eligible', null, c.accountId))));
  }
  const member = (id: string | null | undefined) => candidates.find((c) => c.accountId === id);
  // A probe warms the pin it was sent for: the line names that member's obstacle, if any, and whether this probe
  // already tried it.
  if (r.kind === 'probe') {
    const pinned = pin?.found === 'live' ? member(pin.accountId) : undefined;
    const facts = pinned ? [...(pinned.obstacle ? [fact('obstacle', pinned.obstacle.name)] : []), fact('tried', spec.attempted.includes(pinned.accountId))] : [];
    walk.push(step('probe', 'passed', facts));
  }
  // A live pin a portable request still reaches walks the five rules, and a fired rule names its destinations. A token
  // count follows its pin, since it writes none a move could leave, and a pin past its account's end is released at
  // renewal instead.
  let movedAmong: string[] | undefined;
  if (pin?.found === 'live' && portable && !spec.failover && (spec.kind === 'renewed' || spec.kind === 'moved' || resetHold || slowHold)) {
    const source = member(pin.accountId);
    const endsAt = source?.term?.endsAt ? Date.parse(source.term.endsAt) : null;
    const skip = r.kind === 'count_tokens' ? 'count' : endsAt !== null && endsAt <= spec.at ? 'subscription_ended' : null;
    if (skip !== null) {
      for (const rule of RULES) walk.push(step(`move:${rule}`, 'not_run', [fact('reason', skip)]));
    } else {
      let move = moveOf(spec, candidates, pool, source, pin);
      // With every destination at its cap no move is made and the pin renews in place, which is how a renewal reads
      // where a rule's reason holds.
      if (spec.kind === 'renewed' && move.fired !== null) {
        for (const c of move.reached) c.inFlight = c.inFlightCap;
        move = moveOf(spec, candidates, pool, source, pin);
      }
      // The shed line carries the account's read whichever rule answered, save a probe's, which reads none.
      const pace = r.kind === 'probe' ? [] : slowFactsOf(source, pool, spec.at);
      walk.push(...move.lines.map((line) => (line.step === 'move:shed' ? { ...line, facts: [...line.facts, ...pace] } : line)));
      if (move.fired !== null) {
        walk.push(step('cap_filter', move.destinations.some((c) => c.inFlight < c.inFlightCap) ? 'passed' : 'dropped', move.destinations.map((c) => fact('in_flight', `${c.inFlight}/${c.inFlightCap}`, 'count', c.accountId))));
        if (spec.kind === 'moved') {
          movedAmong = move.destinations.map((c) => c.accountId);
          for (const c of move.destinations) if (c.accountId !== spec.accountId) c.verdict = 'destination';
        }
      }
    }
  }
  // A renewal stamps the refresh on a turn that refreshes the pin, and the hashes on any turn that touches it and
  // carries a chain. A probe and a token count leave both.
  const warmth = typeof spec.pin === 'object' ? spec.pin.warmth : null;
  const stamps = { hashes: warmth !== 'untouched' && r.kind === 'inference', refresh: warmth === 'refresh' };
  if (spec.kind === 'renewed') {
    walk.push(step('renew', 'renewed', [fact('account_id', spec.accountId, null, spec.accountId), fact('lane', laneOf(r)), fact('refresh_stamped', stamps.refresh), fact('hashes_stamped', stamps.hashes)]));
  }
  let pick: ReturnType<typeof pickOf> = { runnerUp: null, score: null, tierSize: 0 };
  let tierSize = 0;
  const placing = (spec.kind === 'placed' || spec.kind === 'moved' || spec.kind === 'held' || spec.kind === 'exhausted') && !resetHold && !slowHold;
  if (placing) {
    const set = candidates.filter((c) => (movedAmong ? movedAmong.includes(c.accountId) : c.obstacle === null));
    const partitions = narrowed(set, pool, r, movedAmong !== undefined);
    walk.push(step('partitions', partitions.left.length > 0 ? 'match' : 'none', partitions.facts));
    const rung = spec.rung ?? null;
    const subagent = r.agentId !== 'main';
    const rungs: [string, Placement['rung']][] = [['rung:content', 'content'], ['rung:with_main', 'with_main'], ['rung:static', 'static']];
    let matched = false;
    for (const [name, which] of rungs) {
      if (matched) {
        walk.push(step(name, 'not_run'));
        continue;
      }
      if (rung?.rung === which) {
        matched = true;
        const chosen = member(spec.accountId);
        const entryOf = which === 'static' ? chosen?.prefix.staticMatch : chosen?.prefix.contentMatch;
        walk.push(step(name, 'match', [fact('depth', rung.depth, 'count'), fact('prefix_tokens', rung.prefixTokens, 'tokens'), fact('account_id', spec.accountId, null, spec.accountId), ...(entryOf ? [fact('hash', entryOf.hash)] : [])]));
        continue;
      }
      const reason = r.kind === 'count_tokens' ? 'count' : which === 'with_main' && !subagent ? 'class' : which === 'with_main' && pool.subagentPlacement !== 'with_main' ? 'policy_off' : 'no_entry';
      walk.push(step(name, 'none', [fact('reason', reason)]));
    }
    if (!matched && spec.kind === 'exhausted') walk.push(step('pick', 'none', [fact('strategy', pool.strategy)]));
    else if (!matched) {
      // A hold's pick ranks the members as a placement's does and names the account it waits on, while its row reads
      // the hold rather than a score.
      const ranked = pickOf(candidates, partitions.left, spec.accountId, pool.strategy, spec.kind === 'moved' ? spec.from : undefined);
      if (spec.kind !== 'held') pick = ranked;
      tierSize = ranked.tierSize;
      const tier = member(spec.accountId)?.tier ?? [0, 0, 0];
      // A burn fact follows each score, the burn the spare rate took off.
      const scores = candidates.filter((c) => c.score !== null).flatMap((c) => [fact('score', c.score, c.kind === 'api_key' ? 'percent' : 'percent_per_hour', c.accountId), fact('burn', c.pace?.burn ?? 0, 'percent_per_hour', c.accountId)]);
      walk.push(step('pick', 'chosen', [fact('strategy', pool.strategy), fact('tier', tier.join('/')), ...scores]));
    }
  }
  if (spec.kind === 'held' && spec.hold && !resetHold && !slowHold) {
    walk.push(step('hold', 'held', [fact('place', spec.hold.place), fact('account_id', spec.accountId, null, spec.accountId), fact('bound_ms', pool.retryInPlaceSeconds * 1_000, 'ms')]));
  }
  const exhaustion = spec.kind === 'exhausted' ? (spec.exhaustion ?? exhaustionOf(candidates)) : null;
  if (exhaustion) {
    walk.push(step('exhausted', 'exhausted', [fact('kind', exhaustion.kind), fact('deadline', exhaustion.deadline === null ? null : Date.parse(exhaustion.deadline), 'time'), fact('window', exhaustion.window), fact('obstacles', candidates.map((c) => c.obstacle?.name ?? 'eligible'))]));
  }
  // Every walked selection ends at the reserve station: a booking where a placement or a move booked, else the
  // account it landed or waits on alone, and nothing on an exhaustion.
  const booking = reservationOf(spec, member(spec.accountId));
  if (spec.kind !== 'refused') walk.push(booking?.step ?? step('reserve', 'none', spec.accountId === null || spec.kind === 'exhausted' ? [] : [fact('account_id', spec.accountId, null, spec.accountId)]));

  // Each member's verdict as the router reads it once the answer is in.
  for (const c of candidates) {
    if (c.accountId === spec.accountId) c.verdict = spec.kind === 'renewed' ? 'renewed' : spec.kind === 'held' ? 'held_on' : 'chosen';
    else if (c.accountId === spec.from && spec.kind === 'moved') c.verdict = 'source';
    else if (c.verdict === 'destination') continue;
    else if (c.obstacle !== null) c.verdict = 'out';
    else if (c.rank !== null) c.verdict = 'ranked';
    else if (tierSize > 0) c.verdict = 'eligible';
    else c.verdict = 'not_needed';
  }

  const chosen = member(spec.accountId);
  const dispatched = spec.kind === 'placed' || spec.kind === 'renewed' || spec.kind === 'moved';
  const attempt = dispatched ? r.attempts[spec.attempt - 1] : undefined;
  const answered = attempt?.upstreamStatus === 200 && spec.attempt === r.attempts.length;
  const tier = dispatched && !spec.rung && spec.kind !== 'renewed' ? (chosen?.tier ?? null) : null;
  const pinEvents = pinEventsOf(spec, pinning);
  let pinWritten: RecordOutcome['pinWritten'] = null;
  // Every renewal writes the pin back. A token count creates and moves none, so the session keeps the one it had.
  if (dispatched && pinning && (r.kind !== 'count_tokens' || spec.kind === 'renewed')) {
    const action = spec.kind === 'renewed' ? 'renewed' : pin?.found === 'live' || spec.failover ? 'moved' : 'created';
    const prior = pins.get(pinKey);
    const epoch = action === 'renewed' && prior ? prior.epoch : (epochs += 1);
    pins.set(pinKey, { accountId: spec.accountId ?? '', createdAt: action === 'renewed' && prior ? prior.createdAt : spec.at, epoch, lastRefreshAt: spec.at, lastUsedAt: spec.at });
    pinWritten = action === 'renewed' ? { action, epoch, hashesStamped: stamps.hashes, lane: laneOf(r), refreshStamped: stamps.refresh } : { action, epoch, hashesStamped: r.kind === 'inference', lane: laneOf(r), refreshStamped: true };
  }
  const usage = r.usage;
  const promised = spec.rung && spec.rung.rung !== 'with_main' ? spec.rung.prefixTokens : null;
  const hasUsage = answered && usage.input + usage.cacheRead + usage.cacheWrite5m + usage.cacheWrite1h > 0;
  const hit = !hasUsage ? null : spec.rung?.rung === 'content' ? usage.cacheRead >= (promised ?? 0) : usage.cacheRead > 0;
  const ttlMs = ttlOf(pool, chosen?.kind ?? 'oauth', r.agentId !== 'main', r.canonicalModel);
  const completion: RecordCompletion | null = dispatched
    ? {
        cacheMapWritten: answered && usage.cacheWrite5m + usage.cacheWrite1h > 0 ? [{ accountId: spec.accountId ?? '', depth: (spec.rung?.depth ?? 0) + 1, prefixTokens: usage.cacheRead + usage.cacheWrite5m + usage.cacheWrite1h, tier: 'message', ttlMs }] : null,
        cacheReadTokens: hasUsage ? usage.cacheRead : null,
        completedAt: attempt ? iso(Date.parse(attempt.startedAt) + (attempt.durationMs ?? 0)) : null,
        freed: spec.heldBefore !== undefined,
        heldMs: spec.heldBefore?.heldMs ?? null,
        hit,
        inputTokens: hasUsage ? usage.input : null,
        promisedTokens: promised,
        status: attempt?.upstreamStatus ?? null,
      }
    : null;
  // A slow-down hold copies the read it was answered from, its bound the pool's longest wait for the slow-down.
  const slowed = slowHold ? slowReadAt(spec.accountId ?? '', pool, spec.at) : null;
  const hold =
    spec.kind === 'held' && spec.hold
      ? {
          accountId: spec.accountId ?? '',
          boundMs: (slowHold ? pool.slowDownHoldSeconds : pool.retryInPlaceSeconds) * 1_000,
          cohort: spec.hold.cohort,
          cycleMs: slowHold ? (spec.pinPace?.cycleMs ?? null) : null,
          factor: slowed?.factor ?? null,
          reason: resetHold ? 'reset' : slowHold ? 'slow_down' : 'cap',
          speed: slowed?.speed ?? null,
          stretchMs: slowed?.stretchMs ?? null,
          thresholdMs: slowed ? pool.slowDownStretchSeconds * 1_000 : null,
          until: (resetHold || slowHold) && spec.hold.until !== undefined ? iso(spec.hold.until) : null,
        }
      : null;
  // The decision that lands a failover's session on a pin names the cooldown that failed it over.
  const failedOver = spec.failover && pinWritten !== null ? r.attempts[spec.attempt - 2]?.cooldownSet : undefined;
  const outcome: RecordOutcome = {
    accountId: spec.accountId,
    cause: failedOver ? causeOf(failedOver.reason, r.canonicalModel) : null,
    cohortHold: spec.heldBefore ?? null,
    completion,
    exhaustion,
    fromAccountId: spec.kind === 'moved' || spec.failover ? (spec.from ?? null) : null,
    hold,
    inOverage: chosen?.inOverage ?? false,
    kind: spec.kind,
    // A move takes the pin's mark with the row it leaves.
    markCleared: spec.kind === 'moved' && pin?.markedAt && pin.markedOn && pin.markedWindow ? { markedAt: pin.markedAt, markedOn: pin.markedOn, markedWindow: pin.markedWindow, projection: null, reason: 'moved' } : null,
    pinEvents,
    pinWritten,
    refusal: spec.refusal ?? null,
    reservation: booking?.reservation ?? null,
    rule: spec.kind === 'moved' ? (spec.rule ?? null) : null,
    rung: spec.rung ?? null,
    shedMarks: spec.round ? spec.round.marks : null,
    strategy: pool.strategy,
    tier,
    upstreamModel: dispatched ? r.canonicalModel : null,
  };
  const record: DecisionRecord = {
    candidates,
    outcome,
    pin,
    pinReason,
    raw: null,
    request: {
      agentId: r.agentId ?? 'main', at: iso(spec.at), attempt: spec.attempt, attempted: spec.attempted, bodyTokens: spec.kind === 'refused' ? null : bodyTokensOf(r), canonicalModel: r.canonicalModel,
      chain: { boundaries: r.kind === 'inference' ? Math.max(1, Math.round(r.shape.messageCount / 4)) : 0, staticBoundaries: r.kind === 'inference' ? 2 : 0, staticHash: r.kind === 'inference' },
      clientClass: r.client === 'other' ? 'other' : 'claude_code',
      flags: { contextCompacted: r.contextCompacted !== null, count: r.kind === 'count_tokens', fast, marker: r.kind === 'inference' && spec.pin !== 'pinning_off', portable, probe: r.kind === 'probe' },
      holds: spec.holds ?? [],
      keyGeneration: r.keyGeneration, keyId: r.keyId ?? '', lane: { effort: r.shape.effort ?? '-', speed: fast ? 'fast' : 'standard', thinking: r.shape.thinkingType ?? 'none' }, parentAgentId: r.agentId === 'main' ? null : 'main', poolId: r.poolId ?? '',
      registryRevision: 412, requestClass: r.requestClass, requestId: r.id, seq: spec.seq,
    },
    requestId: r.id,
    seq: spec.seq,
    settings: settingsOf(pool),
    version: 2,
    walk,
  };
  const { by, fact: facts } = byAndFact(spec, record, pick, pool);
  const summary: DecisionSummary = {
    type: 'decision',
    accountId: spec.accountId,
    agentId: r.agentId,
    at: iso(spec.at),
    attempt: spec.attempt,
    attemptCount: r.attempts.length,
    by,
    cache: hit === null ? null : { hit, inputTokens: usage.input, promisedTokens: promised, readTokens: usage.cacheRead },
    exhaustion: outcome.exhaustion,
    fact: spec.fact ?? facts,
    fast,
    fromAccountId: outcome.fromAccountId,
    hold,
    keyId: r.keyId,
    kind: spec.kind,
    model: r.canonicalModel,
    poolId: r.poolId,
    probe: r.kind === 'probe',
    refusal: outcome.refusal,
    requestId: r.id,
    rule: outcome.rule,
    rung: outcome.rung,
    runnerUp: pick.runnerUp,
    score: pick.score,
    seq: spec.seq,
    sessionId: r.sessionId,
    // A hold dispatches nothing, so its row counts the members kept out and none its pick ranked.
    skipped: spec.kind === 'renewed' ? 0 : candidates.filter((c) => c.accountId !== spec.accountId && (c.obstacle !== null || (spec.kind !== 'held' && c.rank !== null))).length,
    status: dispatched ? (attempt?.upstreamStatus ?? null) : null,
    strategy: pool.strategy,
    tier,
    warmth: pin?.warmth && pin.warmthCause ? { cause: pin.warmthCause, verdict: pin.warmth } : null,
  };
  return { record, summary };
}

/**
 * The router's answer when no member is left, read off the members' obstacles in the engine's precedence: the fast
 * lane's soonest reset, the usage credits where every obstacle is theirs, the soonest dated cooldown or budget, a model
 * refusal alone, or nothing dated.
 */
function exhaustionOf(candidates: RecordCandidate[]): NonNullable<RecordOutcome['exhaustion']> {
  const answer = (kind: string, o: Partial<NonNullable<RecordOutcome['exhaustion']>> = {}) => ({ deadline: null, kind, obstacle: null, reason: null, window: null, ...o });
  const obstacles = candidates.flatMap((c) => (c.obstacle ? [c.obstacle] : []));
  const untilOf = (until: string | null) => (until === null ? Number.POSITIVE_INFINITY : Date.parse(until));
  const fastLane = obstacles.filter((o) => o.name === 'cooldown' && o.reason === 'fast_rate_limit').map((o) => untilOf(o.until));
  if (fastLane.length) return answer('fast_lane', { deadline: iso(Math.min(...fastLane)) });
  const credits = obstacles.flatMap((o) => (o.name === 'credits' ? [o.reason] : o.name === 'budget' ? [null] : o.name === 'cooldown' ? [o.reason === 'fast_credits' ? o.window : null] : []));
  if (credits.length && credits.every((x) => x !== null)) return answer('fast_credits', { reason: credits[0] });
  const dated = obstacles.flatMap((o) => (o.name === 'budget' ? [{ reason: 'budget', until: untilOf(o.until), window: 'overage' }] : o.name === 'cooldown' ? [{ reason: o.reason, until: untilOf(o.until), window: o.reason === 'fast_credits' ? 'overage' : o.window }] : []));
  const timed = dated.filter((d) => d.reason !== 'model_rejected').sort((x, y) => x.until - y.until)[0];
  if (timed) return answer('timed', { deadline: iso(timed.until), window: timed.window });
  return answer(dated.length ? 'model_rejected' : 'untimed');
}

/** The pin events the routing task emits after the decision, in its order. */
function pinEventsOf(spec: Spec, pinning: boolean): RecordOutcome['pinEvents'] {
  const r = spec.request;
  const events = REQUEST_EVENTS.get(r.id) ?? [];
  const attempt = r.attempts[spec.attempt - 1];
  const moved = attempt && events.find((e) => e.kind === 'pin_migrated' && e.at === attempt.startedAt && e.accountId === attempt.accountId);
  if (moved && pinning && (spec.kind === 'placed' || spec.kind === 'moved')) return [{ kind: 'pin_migrated', reason: String(moved.detail?.reason ?? '') }];
  const expired = spec.seq === 1 ? events.find((e) => e.kind === 'pin_expired') : undefined;
  const reasons = expired && Array.isArray(expired.detail?.reasons) ? (expired.detail.reasons as unknown[]).map(String) : [];
  const created = pinning && r.kind === 'inference' && spec.kind === 'placed' && spec.pin === 'none';
  return [...(expired ? [{ kind: 'pin_expired', reason: reasons[0] ?? null }] : []), ...(created ? [{ kind: 'pin_created', reason: null }] : [])];
}

/**
 * A placement's summary fact, by the router's own rules. Exported so a test drives the strategies no pool of the story
 * contests a tier under.
 */
export function placementFactOf(record: DecisionRecord, pick: ReturnType<typeof pickOf>, strategy: string): SummaryFact[] {
  const rung = record.outcome.rung;
  if (rung?.rung === 'with_main') return [];
  if (rung) return [summaryFact('prefix', rung.prefixTokens, 'tokens'), summaryFact('depth', rung.depth, 'count')];
  const others = record.candidates.filter((c) => c.accountId !== record.outcome.accountId);
  if (others.length > 0 && others.every((c) => record.request.attempted.includes(c.accountId))) return [summaryFact('pick', 'last account', 'text')];
  if (pick.tierSize <= 1) return [summaryFact('pick', 'only candidate', 'text')];
  if (strategy === 'pace' && pick.score && pick.runnerUp) {
    return [summaryFact('gap', Math.round((pick.score.score - pick.runnerUp.score) * 100_000) / 100_000, pick.score.unit as SummaryFact['unit']), { entity: { id: pick.runnerUp.accountId, kind: 'account' }, key: 'over', unit: null, value: pick.runnerUp.accountId }];
  }
  // A round-robin pick reads its turn as the engine names it. The story keeps no cursor, so the chosen member reads
  // first, as a rank does.
  return [summaryFact(strategy === 'round_robin' ? 'turn' : 'rank', `1 of ${pick.tierSize}`, 'text')];
}

/** The row's By and Fact, by the router's own rules for the summary it writes beside the record. */
function byAndFact(spec: Spec, record: DecisionRecord, pick: ReturnType<typeof pickOf>, pool: PoolSeed): { by: DecisionSummary['by']; fact: SummaryFact[] } {
  const by = (kind: DecisionSummary['by']['kind'], value: string, tier: [number, number, number] | null = null) => ({ kind, tier, value });
  const r = spec.request;
  const placementFact = () => placementFactOf(record, pick, pool.strategy);
  const inFlight = (): SummaryFact[] => {
    const m = record.candidates.find((c) => c.accountId === spec.accountId);
    return m ? [summaryFact('in_flight', `${m.inFlight}/${m.inFlightCap}`, 'text')] : [];
  };
  switch (spec.kind) {
    case 'renewed': {
      // The wait explains the renewal more than the pin's idle age does: the row names the slow-down it followed and
      // how long it lasted.
      const slowWait = spec.heldBefore?.waits.find((w) => w.reason === 'slow_down');
      if (slowWait) return { by: by('hold', 'slow_down'), fact: [summaryFact('held', slowWait.heldMs, 'ms')] };
      if (r.kind === 'probe') return { by: by('none', ''), fact: [summaryFact('epoch', record.pin?.epoch ?? 0, 'count')] };
      if (record.pin?.warmth === 'cold') return { by: by('none', ''), fact: [summaryFact('cause', record.pin.warmthCause ?? '', 'text')] };
      // The first window, in the trigger's order, whose projection at its horizon reaches the ceiling names its reading.
      const m = record.candidates.find((c) => c.accountId === spec.accountId);
      const hottest = m === undefined ? undefined : drawnWindows(r.canonicalModel).find((w) => (m.windows[w]?.projection ?? -1) >= pool.windowCeiling);
      if (m !== undefined && hottest !== undefined) return { by: by('none', ''), fact: [summaryFact(hottest, m.windows[hottest].utilization, 'percent')] };
      return { by: by('none', ''), fact: [summaryFact('idle', record.pin?.idleMs ?? 0, 'ms')] };
    }
    case 'moved': {
      // The runway first, a zero one as none and one with no end as unbounded, then the order the gate fell back to.
      const runway = record.candidates.find((c) => c.accountId === spec.accountId)?.runwayMs ?? null;
      const runwayFact = runway === null ? summaryFact('runway', 'unbounded', 'text') : runway === 0 ? summaryFact('runway', 'none', 'text') : summaryFact('runway', runway, 'ms');
      const order = record.walk.filter((w) => w.step === 'fit').pop()?.verdict;
      const verdict = order === 'least_bad' ? [summaryFact('fit', 'least bad', 'text')] : order === 'rewrite_only' ? [summaryFact('fit', 'rewrite only', 'text')] : placementFact();
      return { by: by('rule', record.outcome.rule ?? ''), fact: [runwayFact, ...verdict] };
    }
    case 'placed':
      // The hold the placement followed names its reason, a slot's wait its cohort or the pick it waited on.
      if (spec.heldBefore) {
        const { waits } = spec.heldBefore;
        const reason = waits[waits.length - 1]?.reason;
        const place = reason === 'slow_down' || reason === 'reset' ? reason : spec.heldBefore.cohort ? 'cohort' : 'strategy_pick';
        return { by: by('hold', place), fact: [summaryFact('held', spec.heldBefore.heldMs, 'ms')] };
      }
      return { by: record.outcome.rung ? by('rung', record.outcome.rung.rung) : by('strategy', pool.strategy, record.outcome.tier), fact: placementFact() };
    case 'held':
      // A slowed pin's wait names its end and the factor it was sized under: no slot, pick or reset explains it.
      if (spec.hold?.place === 'slow_down' && record.outcome.hold?.until) {
        const factor = record.outcome.hold.factor;
        return { by: by('hold', 'slow_down'), fact: [summaryFact('until', Date.parse(record.outcome.hold.until), 'time'), ...(factor === null ? [] : [{ entity: null, key: 'factor', unit: null, value: factor }])] };
      }
      // A wait for a reset names its end, since no slot and no pick explain it.
      if (spec.hold?.place === 'reset' && spec.hold.until !== undefined) return { by: by('hold', 'reset'), fact: [summaryFact('until', spec.hold.until, 'time')] };
      return { by: by('hold', spec.hold?.place ?? 'strategy_pick'), fact: inFlight() };
    case 'exhausted': {
      const x = record.outcome.exhaustion;
      if (!x) return { by: by('exhaustion', ''), fact: [] };
      const f = x.reason !== null ? summaryFact('cause', x.reason, 'text') : x.deadline !== null ? summaryFact('retry', Date.parse(x.deadline), 'time') : summaryFact('retry', 'none', 'text');
      return { by: by('exhaustion', x.kind), fact: [f] };
    }
    default: {
      const kind = record.outcome.refusal?.kind ?? '';
      if (kind === 'client') return { by: by('refusal', kind), fact: [summaryFact('client', r.userAgent?.match(/\/([0-9.]+)/)?.[1] ?? 'unknown', 'text')] };
      return { by: by('refusal', kind), fact: [{ entity: { id: r.keyId ?? '', kind: 'key' }, key: 'key', unit: null, value: r.keyId ?? '' }] };
    }
  }
}

/** The decisions of a drawn request, read off its attempts and their traces. */
function derivedSpecs(r: RequestLog): Spec[] {
  const specs: Spec[] = [];
  const base = { attempted: [] as string[], request: r };
  const pool = poolById.get(r.poolId ?? '');
  const pinning = (pool?.affinityTtlSeconds ?? 0) > 0 && r.sessionId !== null;
  const noPin: Spec['pin'] = pinning ? 'none' : 'pinning_off';
  const at = Date.parse(r.at);
  // The engine holds a request only with every member at its cap, whether it waits on its cohort's account or on the
  // strategy's pick, and a request the bound ran out on waited so too.
  const allAtCap = () => Object.fromEntries((pool?.members ?? []).map((m) => [m.accountId, pool?.inFlightCap ?? 0]));
  const holds: RecordRequest['holds'] = [];
  let waitsRecorded = 0;
  // Every member was at its cap until the bound ran out, so the router answered the request without a dispatch.
  const ranOut = (seq: number, attempt: number, onAt: number, attempted: string[]) => {
    // The row's hold sums every wait, and a wait before the first attempt already has its one record, however many
    // accounts it named.
    const waitedOn = (r.held?.waitedOn ?? []).slice(waitsRecorded);
    const bound = (pool?.retryInPlaceSeconds ?? 60) * 1_000;
    // One held record per re-selection the hold made, each carrying the in-flight facts a held row reads.
    waitedOn.forEach((accountId, i) => {
      specs.push({ ...base, accountId, at: onAt + i * bound, attempt, attempted, hold: { cohort: null, place: 'strategy_pick' }, holds: [...holds], inFlight: allAtCap(), kind: 'held', pin: noPin, seq: seq + i });
      holds.push({ accountId, attempt, cohort: null, heldMs: bound, ranOut: i === waitedOn.length - 1 });
    });
    const answeredAt = onAt + waitedOn.length * bound;
    // Past the bound the relay passes each account it could hold on over and selects again, so the selection that
    // answers finds every one of them attempted and nothing left to place on. A Fable request's spent member drops
    // before the cap, so it takes no hold and is never passed over. The windows are the ones the answering record
    // reads.
    const fast = r.shape.requestedSpeed === 'fast';
    const spent = (id: string) => r.canonicalModel.includes('fable') && spentForFable((w) => oauthWindows(id, answeredAt, () => OLDER_FACTOR).find((x) => WINDOW_OF[x.id] === w)?.utilization);
    // A spent Fable member stays servable where the pool allows overage and the credits the record reads are unblocked.
    const funded = (id: string) => pool !== undefined && fundedBy(pool, fast ? creditsAt(id, answeredAt) : null);
    const servable = pool === undefined ? [] : pool.members.map((m) => m.accountId).filter((id) => obstacleOf(accountOf.get(id)!, pool, answeredAt, r.canonicalModel, fast, []) === null && (!spent(id) || funded(id)));
    const passedOver = [...new Set([...attempted, ...waitedOn, ...servable])];
    // The builder names the exhaustion off the members' obstacles, which a block among them dates or explains.
    specs.push({ ...base, accountId: null, at: answeredAt, attempt, attempted: passedOver, fact: [summaryFact('held', bound, 'ms'), summaryFact('answer', 429, 'count')], holds: [...holds], inFlight: allAtCap(), kind: 'exhausted', pin: noPin, seq: seq + waitedOn.length });
  };
  if (r.attempts.length === 0) {
    if (r.errorCode === 'accounts_at_cap') ranOut(1, 1, at, []);
    else specs.push({ ...base, accountId: null, at, attempt: 1, kind: 'exhausted', pin: noPin, seq: 1 });
    return specs;
  }
  let seq = 0;
  let pinOn: string | null = null;
  r.attempts.forEach((a, i) => {
    const s = a.selection;
    const start = Date.parse(a.startedAt);
    const attempted = r.attempts.slice(0, i).map((x) => x.accountId);
    // The row holds a request only before its first attempt, from its arrival.
    const hold = i === 0 ? (s?.cohortHold ?? null) : null;
    if (hold) {
      const heldOn = hold.waitedOn[hold.waitedOn.length - 1] ?? a.accountId;
      seq += 1;
      specs.push({ ...base, accountId: heldOn, at: start - hold.heldMs, attempt: i + 1, attempted, hold: { cohort: hold.cohort, place: hold.cohort ? 'cohort' : 'strategy_pick' }, holds: [...holds], inFlight: allAtCap(), kind: 'held', pin: noPin, seq });
      holds.push({ accountId: heldOn, attempt: i + 1, cohort: hold.cohort, heldMs: hold.heldMs, ranOut: false });
      waitsRecorded += hold.waitedOn.length;
    }
    seq += 1;
    const moved = s?.moved ?? null;
    const failover = moved !== null && FAILOVER_REASONS.includes(moved) ? moved : undefined;
    const rule = moved !== null && failover === undefined ? moved : undefined;
    // A request that can hold no pin renews none and moves none: it is placed.
    const kind: DecisionKind = !pinning ? 'placed' : s?.renewed ? 'renewed' : rule ? 'moved' : 'placed';
    const from = failover ? attempted[attempted.length - 1] : rule ? (String(REQUEST_EVENTS.get(r.id)?.find((e) => e.kind === 'pin_migrated')?.detail?.from_account ?? '') || undefined) : undefined;
    const live = kind === 'renewed' || kind === 'moved' || failover !== undefined;
    // A probe and a token count leave the pin's warmth untouched.
    const untouched = r.kind === 'probe' || r.kind === 'count_tokens';
    const pinSpec: Spec['pin'] = live && pinning ? { cause: r.kind === 'probe' ? 'probe' : r.kind === 'count_tokens' ? 'count' : 'continues_pin', idleMs: 20_000 + inFlightRng.int(0, 280) * 1_000, warmth: untouched ? 'untouched' : 'refresh' } : noPin;
    const prefix: Spec['prefix'] = {};
    const rung = s?.placement ?? null;
    if (rung && rung.rung !== 'with_main') {
      const ttl = pool ? ttlOf(pool, accountOf.get(a.accountId)?.kind ?? 'oauth', r.agentId !== 'main', r.canonicalModel) : 300_000;
      const boundary = rung.rung === 'static' ? staticBoundary(r.agentId, rung.depth) : `message|${r.sessionId ?? r.id}|${rung.depth}`;
      const found = entry(boundary, { at: start, depth: rung.depth, prefixTokens: rung.prefixTokens, ttlMs: ttl, writtenAt: start - 2 * MINUTE });
      prefix[a.accountId] = rung.rung === 'static' ? { static: found, staticMatch: found } : { content: found, contentMatch: found };
    }
    const heldBefore = holds.length ? { cohort: holds[holds.length - 1].cohort, heldMs: holds.reduce((n, h) => n + h.heldMs, 0), waitedOn: holds.map((h) => h.accountId), waits: holds.map((h) => ({ heldMs: h.heldMs, reason: 'cap' })) } : undefined;
    specs.push({ ...base, accountId: a.accountId, at: start, attempt: i + 1, attempted, failover, from: from ?? pinOn ?? undefined, heldBefore: hold ? heldBefore : undefined, holds: [...holds], kind, pin: pinSpec, prefix, rule, rung, seq });
    pinOn = a.accountId;
  });
  const last = r.attempts[r.attempts.length - 1];
  const after = Date.parse(last.startedAt) + (last.durationMs ?? 0);
  const tried = r.attempts.map((x) => x.accountId);
  if (r.errorCode === 'accounts_at_cap') ranOut(seq + 1, r.attempts.length + 1, after, tried);
  // A 429 or a refused model fails over, so a request whose last attempt answered one found nothing left at its
  // reselect and the router answered it.
  else if (failsOver(last)) specs.push({ ...base, accountId: null, at: after, attempt: r.attempts.length + 1, attempted: tried, holds: [...holds], kind: 'exhausted', pin: noPin, seq: seq + 1 });
  return specs;
}

const ONE_HOUR = 3_600_000;
const FIVE_MINUTES = 300_000;
const sessionId = (prefix: string) => REQUESTS.find((r) => r.sessionId?.startsWith(prefix))?.sessionId ?? prefix;

/**
 * The 11:00 round's four pins on secondary in its order: the cold one, then the warm ones by burn over their prompt.
 * Each holds Sonnet 4.6, the one model secondary serves, and burns its share of the 5-hour window per hour, which sum
 * to the account's burn. The first two cover the target under the cap and plan their moves to primary, where they fit,
 * and the walk stops before the current pin.
 */
const ROUND_PINS: RoundPin[] = [
  { agentId: 'main', burn: 0.0034, cold: true, keyId: 'desktop', lastInputTokens: 22_000, marked: true, model: 'claude-sonnet-4-6', note: null, sessionId: sessionId('0d588de5'), skip: null },
  { agentId: 'e90b', burn: 0.0142, cold: false, keyId: 'desktop', lastInputTokens: 88_000, marked: true, model: 'claude-sonnet-4-6', note: null, sessionId: sessionId('c64358b5'), skip: null },
  { agentId: 'e90b', burn: 0.0052, cold: false, keyId: 'desktop', lastInputTokens: 41_000, marked: false, model: 'claude-sonnet-4-6', note: 'current', sessionId: sessionId('407dd39b'), skip: null },
  { agentId: 'c21e', burn: 0.0071, cold: false, keyId: 'desktop', lastInputTokens: 64_000, marked: false, model: 'claude-sonnet-4-6', note: null, sessionId: sessionId('76ddb35b'), skip: null },
];

/**
 * The afternoon round's two pins on primary, cold first: no sibling has runway for either, since nightly projects
 * past the ceiling, secondary is spent and cooling, and the Overflow workspace needs funding. Their burns sum to
 * primary's.
 */
const NO_RUNWAY_PINS: RoundPin[] = [
  { agentId: 'main', burn: 0.052, cold: true, keyId: 'desktop', lastInputTokens: 43_000, marked: false, model: 'claude-sonnet-4-6', note: null, sessionId: sessionId('bcc805b6'), skip: 'no_runway' },
  { agentId: 'main', burn: 0.033, cold: false, keyId: 'desktop', lastInputTokens: 52_300, marked: false, model: 'claude-sonnet-5-5', note: 'current', sessionId: sessionId('d7167642'), skip: 'no_runway' },
];

/**
 * Studio's round fired by the stretch, its five pins in the round's order: the warm ones by their demand over their
 * prompt, each burn the pin's rate over the account's speed, which sum to its demand. The target is more than any one
 * pin's burn, so the order marks the first, and the rest is less than each pin left, so the pick marks the covering pin
 * whose move rewrites least, the first among equals on the API-key annex, and the walk ends there.
 */
const STRETCH_PINS: RoundPin[] = [
  { agentId: 'main', burn: 0.028, cold: false, keyId: 'notebook', lastInputTokens: 30_000, marked: true, model: 'claude-sonnet-5-5', note: null, sessionId: MARKED_SESSION_IDS[1], skip: null },
  { agentId: 'main', burn: 0.027, cold: false, keyId: 'notebook', lastInputTokens: 40_000, marked: true, model: 'claude-sonnet-5-5', note: 'cheapest', sessionId: MARKED_SESSION_IDS[0], skip: null },
  { agentId: '7b2e', burn: 0.026, cold: false, keyId: 'notebook', lastInputTokens: 48_000, marked: false, model: HAIKU, note: null, sessionId: SLOWED_SESSION_ID, skip: null },
  { agentId: 'main', burn: 0.0226, cold: false, keyId: 'notebook', lastInputTokens: 52_000, marked: false, model: 'claude-sonnet-5-5', note: null, sessionId: OPENING_SESSION_IDS[1], skip: null },
  { agentId: 'main', burn: 0.025, cold: false, keyId: 'notebook', lastInputTokens: 72_000, marked: false, model: 'claude-sonnet-5-5', note: 'current', sessionId: SLOWED_SESSION_ID, skip: null },
];

/**
 * Nightly's Fable-week round, its two pins in the round's order: the Fable session's pin, then an Opus pin, whose model
 * draws no Fable week and so burns nothing there. The Opus pin leaves the round before the order and counts toward no
 * total, which leaves the Fable pin its member's last on the week, so the walk marks nothing.
 */
const FABLE_WEEK_PINS: RoundPin[] = [
  { agentId: 'main', burn: 0.03, cold: false, keyId: 'desktop', lastInputTokens: 81_000, marked: false, model: 'claude-fable-5-1', note: 'current', sessionId: sessionId('89cd1c92'), skip: 'last_pin' },
  { agentId: 'main', burn: null, cold: false, keyId: 'desktop', lastInputTokens: 61_000, marked: false, model: 'claude-opus-5', note: null, sessionId: 'e3b8d1f6-4a29-4c71-9d05-6b2a8f3c7e14', skip: 'off_window' },
];

const roundOfPins = (pins: RoundPin[]): ShedRound => ({ marks: pins.filter((p) => p.marked).map((p) => ({ agentId: p.agentId, keyId: p.keyId, model: p.model, sessionId: p.sessionId })), pins });

/**
 * The story hour and the morning's round, written by hand so every card the routing log draws is on the page: each
 * decision names its kind, its account and what the engine read where the walk back alone would not tell it.
 */
function storySpecs(): Spec[] {
  const t = clock;
  const at = (id: string, seq: number, attempt: number, when: number, kind: DecisionKind, accountId: string | null, extra: Partial<Spec> = {}): Spec => ({ accountId, at: when, attempt, attempted: [], kind, pin: 'none', request: requestById.get(id)!, seq, story: true, ...extra });
  const live = (cause = 'continues_pin', idleMs = 60_000, warmth: 'cold' | 'refresh' | 'untouched' = 'refresh') => ({ cause, idleMs, warmth });
  const mainStatic = (when: number, writtenAt: number, refusedBy: string | null = null) => entry(staticBoundary('main', 2), { at: when, depth: 2, prefixTokens: 31_200, refusedBy, ttlMs: ONE_HOUR, writtenAt });
  const cohort = (when: number, refusedBy: string | null = null) => entry(staticBoundary('e90b', 1), { at: when, depth: 1, prefixTokens: 18_400, refusedBy, ttlMs: FIVE_MINUTES, writtenAt: t(14, 19, 52) });
  const message = (session: string, depth: number, prefixTokens: number, when: number, writtenAt: number, ttlMs: number) => entry(`message|${sessionId(session)}|${depth}`, { at: when, depth, prefixTokens, ttlMs, writtenAt });
  const placedStatic: Placement = { depth: 2, prefixTokens: 31_200, rung: 'static' };
  // Secondary's sessions burn its 5-hour window at their sum while the round runs and its marks are pending.
  const roundBurn = { secondary: { five_hour: round5(ROUND_PINS.reduce((n, p) => n + (p.burn ?? 0), 0)) } };
  // The Overflow workspace sits at its in-flight cap through the round and the move. With a free slot an API-key
  // sibling fits with no end to its runway, the longest of all, so the shed rule would plan and move both marks there.
  const overflowFull = { overflow: 16 };
  // Nightly's last minute before its spent window resets, as the reset hold reads it.
  const spentNightly = (when: number) => oauthWindows('nightly', when, () => OLDER_FACTOR).map((w) => (w.id === '5h' ? { ...w, utilization: 1 } : w));
  // Studio's 5-hour reading where a hold reads it before any answer's head: the utilization the slow-down read.
  const studioAt = (when: number, utilization: number) => ({ studio: oauthWindows('studio', when, () => OLDER_FACTOR).map((w) => (w.id === '5h' ? { ...w, utilization } : w)) });
  // Nightly's Fable week where the morning's Fable session runs it ahead of the walk back's even climb.
  const nightlyAt = (when: number, utilization: number) => ({ nightly: oauthWindows('nightly', when, () => OLDER_FACTOR).map((w) => (w.id === '7d_fable' ? { ...w, utilization } : w)) });
  // A pin's cycle after one more gap, the time-weighted average over the work pool's burn average.
  const averageMs = poolById.get('work')!.burnAverageSeconds * 1_000;
  const foldCycle = (cycleMs: number, gapMs: number) => {
    const w = Math.exp(-gapMs / averageMs);
    return Math.round(cycleMs * w + gapMs * (1 - w));
  };
  return [
    // Two of the round's sessions placed on secondary just before it. Every pin a beat renews or moves is a request's.
    at('req_e5a1c093-4d7', 1, 1, t(10, 58, 59), 'placed', 'secondary'),
    at('req_f3b6d218-85a', 1, 1, t(10, 59, 10), 'placed', 'secondary'),
    at('req_0b7c2d94-e61', 1, 1, t(10, 59, 30), 'renewed', 'secondary', { pin: live('probe', 31_000, 'untouched') }),
    // Secondary's 5-hour window opened at 09:55 and runs hot, so its projection to the reset reads past the pool's
    // ceiling: this renewal runs the round, which marks two pins.
    at('req_1eaa9bc1-a18', 1, 1, t(11, 0), 'renewed', 'secondary', { burns: roundBurn, inFlight: overflowFull, pin: live('continues_pin', 30_000), round: roundOfPins(ROUND_PINS) }),
    // A marked pin moves at its next request inside the dwell, to the sibling where it fits.
    at('req_868264bc-611', 1, 1, t(11, 8, 30), 'moved', 'primary', {
      burns: roundBurn,
      from: 'secondary',
      inFlight: overflowFull,
      pin: live('continues_pin', 560_000),
      pinFigures: { burn: { five_hour: 0.0142, seven_day: 0.0016 }, tokens: 88_000 },
      prefix: { secondary: { content: message('c64358b5', 10, 72_000, t(11, 8, 30), t(11, 6, 2), FIVE_MINUTES), contentMatch: null } },
      rule: 'shed',
    }),
    // The hour's sessions placed on secondary before its spent edge.
    at('req_a1f04c2e-7b3', 1, 1, t(14, 0, 10), 'placed', 'secondary'),
    at('req_b72e9d05-c14', 1, 1, t(14, 1, 30), 'placed', 'secondary'),
    at('req_c90d3f61-a28', 1, 1, t(14, 1, 35), 'placed', 'secondary'),
    at('req_d4e8a7b2-916', 1, 1, t(14, 1, 40), 'placed', 'secondary'),
    // A token count follows its subagent's pin without walking the rules, to secondary before its window is spent.
    at('req_3b8e51f4-0a9', 1, 1, t(14, 1, 50), 'renewed', 'secondary', { pin: live('count', 20_000, 'untouched') }),
    at('req_1d9c3e55-0b2', 1, 1, t(14, 2, 10), 'renewed', 'secondary', { pin: live('continues_pin', 120_000) }),
    at('req_1d9c3e55-0b2', 2, 2, t(14, 2, 11), 'placed', 'primary', {
      attempted: ['secondary'],
      failover: 'cooldown',
      from: 'secondary',
      pin: live(),
      prefix: { primary: { static: mainStatic(t(14, 2, 11), t(13, 58, 2)), staticMatch: mainStatic(t(14, 2, 11), t(13, 58, 2)) }, secondary: { content: message('bcc805b6', 8, 58_900, t(14, 2, 11), t(14, 0, 24), ONE_HOUR), contentMatch: null } },
      rung: placedStatic,
    }),
    at('req_7a2d4c18-e95', 1, 1, t(14, 3), 'placed', 'primary'),
    // Secondary reads its 5-hour window spent from 14:02:11 and cools for its retry-after alone, so the session's next
    // request finds it spent and free: the spent rule moves the pin to primary.
    at('req_da52d141-a5a', 1, 1, t(14, 5, 12), 'moved', 'primary', {
      from: 'secondary',
      pin: live('continues_pin', 217_000),
      prefix: { primary: { static: mainStatic(t(14, 5, 12), t(13, 58, 2)), staticMatch: mainStatic(t(14, 5, 12), t(13, 58, 2)) }, secondary: { content: message('0d588de5', 5, 44_600, t(14, 5, 12), t(14, 1, 47), ONE_HOUR), contentMatch: null } },
      rule: 'spent',
      rung: placedStatic,
    }),
    // The keepalive's probe walks the move rules like any request on its pin, so the spent rule moves it too.
    at('req_c41d0311-200', 1, 1, t(14, 6, 30), 'moved', 'primary', { from: 'secondary', pin: live('probe', 290_000, 'untouched'), rule: 'spent' }),
    at('req_ef099b34-453', 1, 1, t(14, 9, 41), 'placed', 'primary'),
    // The member placed by its static prefix holds a deeper entry the rung refused as expired beside the one it took.
    // A sibling's entry reads the gate that refused it.
    at('req_b2bed817-a58', 1, 1, t(14, 11, 44), 'placed', 'primary', {
      prefix: {
        nightly: { static: mainStatic(t(14, 11, 44), t(13, 30), 'band'), staticMatch: null },
        primary: { static: entry(staticBoundary('main', 3), { at: t(14, 11, 44), depth: 3, prefixTokens: 33_900, refusedBy: 'expired', ttlMs: ONE_HOUR, writtenAt: t(13, 5, 10) }), staticMatch: mainStatic(t(14, 11, 44), t(13, 58, 2)) },
      },
      rung: placedStatic,
    }),
    at('req_6c3a9e27-5b1', 1, 1, t(14, 18, 5), 'refused', null, { pin: 'not_looked_up', refusal: { code: 'revoked', kind: 'key', reason: 'the key was revoked' } }),
    at('req_e0a4c6f2-118', 1, 1, t(14, 14, 2), 'refused', null, { pin: 'not_looked_up', refusal: { code: 'drift', kind: 'client', reason: 'hash' } }),
    at('req_51d0e2b8-7a0', 1, 1, t(14, 16, 40), 'exhausted', null, { exhaustion: { deadline: iso(startOfDay(t(14, 16, 40)) + DAY), kind: 'timed', obstacle: null, reason: null, window: 'overage' }, pin: 'pinning_off' }),
    // The compacted conversation renews its pin cold.
    at('req_31f0b7c2-6e1', 1, 1, t(14, 17, 52), 'renewed', 'primary', { pin: live('compacted', 491_000, 'cold') }),
    at('req_9a07c4e1-d55', 1, 1, t(14, 19, 2), 'renewed', 'primary', { pin: live('continues_pin', 438_000) }),
    // A subagent request that carries no cache marker of its own pins nothing. Its cohort's static entry places it.
    at('req_69e30548-32e', 1, 1, t(14, 21, 12), 'held', 'primary', { hold: { cohort: 'primary', place: 'cohort' }, inFlight: { nightly: 16, primary: 16 }, pin: 'pinning_off', prefix: { primary: { static: cohort(t(14, 21, 12), 'at_cap'), staticMatch: null } } }),
    at('req_69e30548-32e', 2, 1, t(14, 21, 14), 'placed', 'primary', {
      heldBefore: { cohort: 'primary', heldMs: 2_000, waitedOn: ['primary'], waits: [{ heldMs: 2_000, reason: 'cap' }] },
      holds: [{ accountId: 'primary', attempt: 1, cohort: 'primary', heldMs: 2_000, ranOut: false }],
      inFlight: { nightly: 16, primary: 15 },
      pin: 'pinning_off',
      prefix: { primary: { static: cohort(t(14, 21, 14)), staticMatch: cohort(t(14, 21, 14)) } },
      rung: { depth: 1, prefixTokens: 18_400, rung: 'static' },
    }),
    // The router keyed the laptop turn by the root its content matched and placed it on primary, whose head arrived
    // before the relay found the stream past it undecodable.
    at('req_5e2f7a19-b03', 1, 1, t(14, 22, 48), 'placed', 'primary'),
    at('req_a6c19e73-5d2', 1, 1, t(14, 23, 30), 'placed', 'primary'),
    at('req_2e7c90a1-6d4', 1, 1, t(14, 24, 9), 'placed', 'primary'),
    at('req_2e7c90a1-6d4', 2, 2, t(14, 24, 10), 'placed', 'nightly', { attempted: ['primary'], failover: 'cooldown', from: 'primary', pin: live() }),
    at('req_4b0e9d17-c31', 1, 1, t(14, 26, 58), 'placed', 'primary'),
    at('req_4b0e9d17-c31', 2, 2, t(14, 27, 1), 'placed', 'nightly', { attempted: ['primary'], failover: 'fast_lane', from: 'primary', pin: live() }),
    at('req_f8085653-422', 1, 1, t(14, 27, 28), 'renewed', 'primary', { pin: live('continues_pin', 506_000) }),
    // A conversation the Overflow workspace answered, cold there nine minutes on, returns to a plan with its
    // 640k-token context. Primary fits but is full, and the rest fall short of the ceiling, so the gate answers the
    // least bad set and Pace takes nightly, whose window the conversation spends within the hour.
    at('req_4e8b1d36-f72', 1, 1, t(10, 52, 30), 'placed', 'overflow'),
    at(FORCED_MOVE_ID, 1, 1, t(11, 1, 50), 'moved', 'nightly', {
      from: 'overflow',
      inFlight: { primary: 16 },
      pin: live('continues_pin', 0, 'cold'),
      pinFigures: { burn: { five_hour: 0.21, seven_day: 0.025 }, tokens: 640_000 },
      rule: 'return_to_plan',
    }),
    // A new conversation lands on nightly with primary full, and nightly's spent window answers 429 40 s before it
    // resets: the reselect finds the pin on a spent account whose reset falls inside the retry bound, so the spent
    // rule holds the request there for the reset, and it renews on the fresh window.
    at('req_9d4c1a73-e58', 1, 1, t(13, 10, 20), 'placed', 'nightly', { inFlight: { primary: 16 } }),
    at('req_9d4c1a73-e58', 2, 2, t(13, 10, 21), 'held', 'nightly', {
      attempted: ['nightly'],
      hold: { cohort: null, place: 'reset', until: t(13, 11) },
      pin: live('continues_pin', 1_000),
      readings: { nightly: spentNightly(t(13, 10, 21)) },
    }),
    at('req_9d4c1a73-e58', 3, 2, t(13, 11), 'renewed', 'nightly', { holds: [{ accountId: 'nightly', attempt: 2, cohort: null, heldMs: 39_000, ranOut: false }], pin: live('continues_pin', 40_000) }),
    // The Fable session's first turn places it on nightly, and its next turn's renewal runs nightly's Fable-week round,
    // before the overage begins at the first head after 11:30:12: from then on the pool's overage rule fires on the
    // account's pins ahead of the shed rule. The 5-hour window the story spends at 13:10:21 opened at 08:11, so it is
    // 19 minutes old at the round, under all's hold of an hour, and its projection past the ceiling rides a burn that
    // still carries the previous window's: the hold keeps it from firing, and the Fable week fires in its place. The
    // Opus pin draws no Fable week, so the round lists it and counts nothing of it. The burns replace the account's, so
    // the 5-hour burn stands beside the Fable pin's.
    at('req_3e2b9d47-c18', 1, 1, t(8, 21, 40), 'placed', 'nightly'),
    at('req_3e7a5c19-f84', 1, 1, t(8, 30), 'renewed', 'nightly', {
      burns: { nightly: { five_hour: 0.019, seven_day_fable: 0.03 } },
      pin: live('continues_pin', 500_000),
      readings: nightlyAt(t(8, 30), 0.65),
      round: roundOfPins(FABLE_WEEK_PINS),
    }),
    // A fresh session placed by the spare rate: secondary falls short of the ceiling, so the no-fit partition passes it
    // over, and its sessions' burn outruns its pace, so its spare rate reads negative.
    at('req_2c7d9f15-b63', 1, 1, t(13, 45), 'placed', 'primary', { burns: { secondary: { five_hour: 0.032, seven_day: 0.004 } } }),
    // A fresh placement that books its rewrite on primary until the answer's usage replaces it.
    at('req_7a3f0c26-d81', 1, 1, t(13, 41, 15), 'placed', 'primary'),
    // Primary projects past the ceiling, so this renewal runs a round, which finds no sibling with runway.
    at('req_6e1f3b98-a24', 1, 1, t(14, 12, 30), 'renewed', 'primary', { pin: live('continues_pin', 46_000), round: roundOfPins(NO_RUNWAY_PINS) }),
    // A session placed on secondary before its spent edge. Its next turn finds the spent rule's one destination full,
    // so it renews on secondary, whose 429 on the spent 5-hour window fails it over to primary once a slot frees.
    at('req_d2a8f6c1-d50', 1, 1, t(14, 1, 45), 'placed', 'secondary'),
    at('req_b8c3e9d4-e61', 1, 1, t(14, 7, 50), 'renewed', 'secondary', { pin: live('continues_pin', 365_000) }),
    at('req_b8c3e9d4-e61', 2, 2, t(14, 7, 51), 'placed', 'primary', { attempted: ['secondary'], failover: 'cooldown', from: 'secondary', pin: live() }),
    // Studio's closed span in work: a placement reads no entry, so the session placed at 09:11:24 opens the span at
    // its first turn after the placement, which reads the factor at 0.91 in the last 77 minutes of the window that
    // resets at 10:29.
    at('req_5c7e2a9d-b14', 1, 1, t(9, 11, 24), 'placed', 'studio'),
    at('req_a3d8f1b6-c25', 1, 1, t(9, 12), 'renewed', 'studio', { pin: live('continues_pin', 36_000) }),
    // The morning session's first turn after its placement sets its pin's dispatch, and the next one, 24 s later,
    // gives the pin its cycle: at 0.40 it waits 36 s, and the renewal after the wait reads 0.35 and runs no round,
    // since the stretch reads under the threshold. Its turns after the reset read 1, and the second, past the wait,
    // ends the span. Each turn after the hold folds its gap into the cycle over the pool's burn average.
    at('req_3d7b9e2f-a41', 1, 1, t(10, 11), 'placed', 'studio'),
    at('req_2b9e4c7f-d36', 1, 1, t(10, 11, 36), 'renewed', 'studio', { pin: live('continues_pin', 36_000) }),
    at('req_8e1c4a6d-b52', 1, 1, t(10, 12), 'held', 'studio', {
      hold: { cohort: null, place: 'slow_down', until: t(10, 12, 36) },
      pin: live('continues_pin', 24_000),
      pinPace: { cycleMs: 24_000, slowDownUntil: t(10, 12, 36) },
      readings: studioAt(t(10, 12), 0.9354),
    }),
    at('req_8e1c4a6d-b52', 2, 1, t(10, 12, 36), 'renewed', 'studio', {
      heldBefore: { cohort: null, heldMs: 36_000, waitedOn: ['studio'], waits: [{ heldMs: 36_000, reason: 'slow_down' }] },
      holds: [{ accountId: 'studio', attempt: 1, cohort: null, heldMs: 36_000, ranOut: false }],
      pin: live('continues_pin', 60_000),
      pinPace: { cycleMs: 24_000, slowDownUntil: t(10, 12, 36) },
    }),
    at('req_f6a1d3e8-e47', 1, 1, t(10, 38, 20), 'renewed', 'studio', { pin: live('continues_pin', 1_544_000), pinPace: { cycleMs: foldCycle(24_000, t(10, 38, 20) - t(10, 12, 36)), slowDownUntil: null } }),
    at('req_8d4b7f2c-f58', 1, 1, t(10, 40, 36), 'renewed', 'studio', {
      pin: live('continues_pin', 136_000),
      pinPace: { cycleMs: foldCycle(foldCycle(24_000, t(10, 38, 20) - t(10, 12, 36)), t(10, 40, 36) - t(10, 38, 20)), slowDownUntil: null },
    }),
    // Studio's open span: a session placed at 13:47:24 opens it at its first turn after the placement, under the
    // threshold at 0.73, and the sessions placed after it are the round's other four pins.
    at('req_b1e7c4a9-a69', 1, 1, t(13, 47, 24), 'placed', 'studio'),
    at('req_4f2a8d6e-b7a', 1, 1, t(13, 48), 'renewed', 'studio', { pin: live('continues_pin', 36_000) }),
    at('req_6b3e8d1a-c73', 1, 1, t(13, 50, 30), 'placed', 'studio'),
    at('req_2f9a6c4e-d84', 1, 1, t(13, 53, 10), 'placed', 'studio'),
    at('req_c5d8e2b7-e95', 1, 1, t(13, 58, 40), 'placed', 'studio'),
    at('req_7e4f1a9c-f16', 1, 1, t(13, 59, 5), 'placed', 'studio'),
    // The first turns after the placements: no pin has a cycle yet, so neither is held, and the first reads the stretch
    // past the threshold, which starts the safety wait.
    at('req_4a9c3e7b-a27', 1, 1, t(14, 2, 30), 'renewed', 'studio', { pin: live('continues_pin', 720_000) }),
    at('req_e1b6d4f8-b38', 1, 1, t(14, 3, 36), 'renewed', 'studio', { pin: live('continues_pin', 296_000) }),
    // The slowed request: its pin's 42 s cycle at half speed waits 42 s inside the safety wait, and the renewal after
    // it, past the wait, runs the round the stretch fires, which marks two of studio's pins for the annex.
    at('req_9f2e7c5a-c49', 1, 1, t(14, 4, 18), 'held', 'studio', {
      hold: { cohort: null, place: 'slow_down', until: t(14, 5) },
      pin: live('continues_pin', 42_000),
      pinPace: { cycleMs: 42_000, slowDownUntil: t(14, 5) },
      readings: studioAt(t(14, 4, 18), 0.8591),
    }),
    at('req_9f2e7c5a-c49', 2, 1, t(14, 5), 'renewed', 'studio', {
      heldBefore: { cohort: null, heldMs: 42_000, waitedOn: ['studio'], waits: [{ heldMs: 42_000, reason: 'slow_down' }] },
      holds: [{ accountId: 'studio', attempt: 1, cohort: null, heldMs: 42_000, ranOut: false }],
      pin: live('continues_pin', 84_000),
      pinPace: { cycleMs: 42_000, slowDownUntil: t(14, 5) },
      round: roundOfPins(STRETCH_PINS),
    }),
  ];
}

/** A selection trace as the attempt row keeps it, read off the decision that dispatched the attempt. */
function traceOf(record: DecisionRecord, summary: DecisionSummary, held: CohortHold | null): RequestLog['attempts'][number]['selection'] {
  const pinEvent = record.outcome.pinEvents.find((e) => e.kind === 'pin_migrated');
  return {
    cacheRead: record.outcome.completion?.cacheReadTokens ?? null,
    cohortHold: held,
    moved: record.outcome.rule ?? pinEvent?.reason ?? null,
    placement: record.outcome.rung,
    renewed: record.outcome.kind === 'renewed',
    skipped: record.outcome.kind === 'renewed' ? [] : record.candidates.filter((c) => c.accountId !== record.outcome.accountId && (c.obstacle !== null || c.rank !== null)).map((c) => ({ accountId: c.accountId, obstacle: c.obstacle?.name ?? 'ranked' })),
    strategy: summary.strategy,
    tier: record.outcome.tier,
  };
}

/** Every decision of every request, built in time order so each pin's epoch and idle age read its last write. */
function buildDecisions() {
  const specs = [...storySpecs(), ...REQUESTS.filter((r) => !STORY_IDS.has(r.id)).flatMap(derivedSpecs)].sort((a, b) => a.at - b.at || a.request.id.localeCompare(b.request.id) || a.seq - b.seq);
  const decisions = new Map<string, DecisionRecord>();
  const summaries: DecisionSummary[] = [];
  for (const spec of specs) {
    const { record, summary } = build(spec);
    // The mock answers the record as it built it, so the card's copy is the record itself.
    const { raw: _, ...answered } = record;
    decisions.set(`${record.requestId}/${record.seq}`, { ...record, raw: answered });
    summaries.push(summary);
  }
  // The story's attempt rows carry the trace of the decision that dispatched them, so the drawer and the card agree.
  for (const id of STORY_IDS) {
    const r = requestById.get(id);
    if (!r) continue;
    for (const [i, a] of r.attempts.entries()) {
      const dispatched = summaries.filter((s) => s.requestId === id && s.attempt === i + 1 && s.kind !== 'held').pop();
      const record = dispatched && decisions.get(`${id}/${dispatched.seq}`);
      if (dispatched && record) a.selection = traceOf(record, dispatched, record.outcome.cohortHold);
    }
  }
  return { decisions, summaries: summaries.sort((a, b) => Date.parse(b.at) - Date.parse(a.at)) };
}

const built = buildDecisions();

/** One version 2 record per decision of every mock request, keyed `${requestId}/${seq}`. */
export const DECISIONS: Map<string, DecisionRecord> = built.decisions;
/** One summary per record, the routing log's decision rows, newest first. */
export const SUMMARIES: DecisionSummary[] = built.summaries;

type EventSeed = Omit<RoutingEvent, 'agentId' | 'actor' | 'id' | 'keyId' | 'model' | 'poolId' | 'requestId' | 'sessionId' | 'type'> & Partial<Pick<RoutingEvent, 'actor' | 'agentId' | 'keyId' | 'model' | 'poolId' | 'requestId' | 'sessionId'>>;

/**
 * The story hour's cache entry that lapsed unread: the Haiku subagent's write on nightly at 14:24:12, which its
 * session never read again within the entry's five minutes.
 */
function expiries(): EventSeed[] {
  const writer = DECISIONS.get('req_2e7c90a1-6d4/2');
  const r = requestById.get('req_2e7c90a1-6d4');
  const written = writer?.outcome.completion?.cacheMapWritten?.[0];
  const writtenAt = writer?.outcome.completion?.completedAt;
  if (!r || !written || !writtenAt) return [];
  const at = Date.parse(writtenAt);
  return [
    {
      accountId: written.accountId,
      at: iso(at + written.ttlMs),
      detail: { cause: 'ttl', deepest: { depth: written.depth, prefix_tokens: written.prefixTokens, tier: written.tier, ttl_ms: written.ttlMs, written_at: at }, entries: 1, lane: laneOf(r), root: r.sessionId },
      keyId: r.keyId,
      kind: 'warm_prefix_expired',
      model: r.canonicalModel,
      sessionId: r.sessionId,
    },
  ];
}

const ttlRulesDiff = (r: PoolSeed['cacheTtl']['plan']) => ({ main: r.main, models: r.models.map((m) => ({ main: m.main, model: m.model, subagent: m.subagent })), subagent: r.subagent });

/** A pool edit's diff as the store's `pool_diff` writes it: the whole row after the save, under its column names. */
function poolDiff(p: PoolSeed): Record<string, unknown> {
  return {
    affinity_mode: p.affinityMode,
    affinity_ttl_seconds: p.affinityTtlSeconds,
    auto_mode_server: p.autoModeServer,
    body_capture_hours: p.bodyCaptureHours,
    burn_average_seconds: p.burnAverageSeconds,
    cache_diagnostics: p.cacheDiagnostics,
    cache_ttl: { enabled: p.cacheTtl.enabled, inject_when_absent: p.cacheTtl.injectWhenAbsent, metered: ttlRulesDiff(p.cacheTtl.metered), plan: ttlRulesDiff(p.cacheTtl.plan) },
    enabled: p.status !== 'disabled',
    in_flight_cap: p.inFlightCap,
    label: p.name,
    load_shedding: p.loadShedding,
    migrate_when_spent: p.migrateWhenSpent,
    min_shared_prefix_tokens: p.minSharedPrefixTokens,
    model_rules: p.modelRules.map((r) => ({ not: r.not, pattern: r.pattern })),
    overage_move_grace_minutes: p.overageMoveGraceMinutes,
    overage_policy: p.overagePolicy,
    prefer_subscription: p.preferSubscription,
    retry_in_place_seconds: p.retryInPlaceSeconds,
    return_to_plan: p.returnToPlan,
    return_to_plan_dwell_seconds: p.returnToPlanDwellSeconds,
    session_placement: p.sessionPlacement,
    shed_cap: p.shedCap,
    shed_dwell_seconds: p.shedDwellSeconds,
    slow_down: p.slowDown,
    slow_down_hold_seconds: p.slowDownHoldSeconds,
    slow_down_stretch_seconds: p.slowDownStretchSeconds,
    strategy: p.strategy,
    subagent_placement: p.subagentPlacement,
    warm_sessions: p.warmSessions,
    wind_down_minutes: p.windDownMinutes,
    window_ceiling: p.windowCeiling,
  };
}

/**
 * A round's `shedding_round` event off the decision that ran it: the window that fired with its projection, horizon
 * and ceiling from the shed line, the round's figures, and each pin with the rewrite and the runway it would take on
 * the destination planned for it, each planned move counted against the next one's runway there. The rewrite is the
 * story's 20x equivalent of the pin's weekly window, a figure of the pin alone, so an API-key destination reads it too.
 */
function roundEvent(decision: string, pins: RoundPin[]): EventSeed | null {
  const record = DECISIONS.get(decision);
  const line = record?.walk.find((w) => w.step === 'move:shed');
  const value = (key: string) => line?.facts.find((f) => f.key === key)?.value;
  const window = value('trigger');
  if (!record || typeof window !== 'string' || record.outcome.accountId === null) return null;
  const accountId = record.outcome.accountId;
  const at = Date.parse(record.request.at);
  const pool = poolById.get(record.request.poolId)!;
  const source = record.candidates.find((c) => c.accountId === accountId)?.windows[window];
  const round = roundOf(roundOfPins(pins), Number(value('excess')), pool);
  // A round the stretch fired reads the demand to the reset less the threshold, the horizon the slow-down moved.
  const slowed = value('fired_by') === 'slow_down' ? slowReadAt(accountId, pool, at) : null;
  const threshold = pool.slowDownStretchSeconds * 1_000;
  const moved = slowed && source?.reset ? Date.parse(source.reset) - threshold : null;
  const horizon = moved ?? (source?.horizon ? Date.parse(source.horizon) : null);
  const projection = moved !== null && source ? round3(source.utilization + (source.demand ?? 0) * ((moved - at) / HOUR)) : (source?.projection ?? null);
  // A planned move books its rewrite and its burn on the destination for the rest of the walk, as the router's plan does.
  const planned = new Map<string, RecordCandidate>();
  const scoreOf = (c: RecordCandidate) => c.pace?.score ?? 0;
  // A mark's destination as its own move would read it: a free eligible sibling holding allowance that the pin fits
  // to the horizon, ranked by the pool's strategy, by tier and under Pace by score, the longest runway breaking the
  // strategy's tie. Round robin's cursor names one turn, so the runway never speaks there.
  const read = (p: RoundPin) => {
    if (!p.marked) return { planned: null, rewrite: null, runway: null };
    const windows = drawnWindows(p.model);
    const siblings = record.candidates.filter((c) => c.obstacle === null && c.accountId !== accountId && c.inFlight < c.inFlightCap && (c.kind === 'api_key' || windows.every((w) => (c.windows[w]?.utilization ?? 0) < 1)));
    const reads = siblings.map((c) => {
      const base = planned.get(c.accountId) ?? structuredClone(c);
      return { base, fit: fitOf(base, at, pool, p.model, p.lastInputTokens, { [window]: p.burn ?? 0 }, true) };
    });
    const fitting = reads.filter((x) => x.fit.verdict === 'fits').map((x) => ({ ...x, runwayMs: x.fit.runwayMs }));
    const ranked = [...fitting].sort((x, y) => (tierBefore(x.base.tier, y.base.tier) ? -1 : tierBefore(y.base.tier, x.base.tier) ? 1 : pool.strategy === 'pace' ? scoreOf(y.base) - scoreOf(x.base) : 0));
    const ties = ranked.filter((x) => tierKey(x.base.tier) === tierKey(ranked[0].base.tier) && (pool.strategy !== 'pace' || scoreOf(x.base) === scoreOf(ranked[0].base)));
    const chosen = pool.strategy === 'round_robin' ? ranked[0] : longestOf(ties)[0];
    if (!chosen) return { planned: null, rewrite: null, runway: null };
    const rewrite = rewriteOf(p.lastInputTokens, p.model);
    for (const [w, x] of Object.entries(rewrite)) if (chosen.base.windows[w]) chosen.base.windows[w].reserved = round5((chosen.base.windows[w].reserved ?? 0) + x);
    chosen.base.burn = { ...chosen.base.burn, [window]: (chosen.base.burn[window] ?? 0) + (p.burn ?? 0) };
    planned.set(chosen.base.accountId, chosen.base);
    return { planned: chosen.base.accountId, rewrite: rewrite[rewriteWindowOf(p.model)] ?? null, runway: chosen.fit.runwayMs };
  };
  return {
    accountId,
    agentId: record.request.agentId,
    at: iso(at),
    detail: {
      burn_average_seconds: pool.burnAverageSeconds,
      burn_sum: round.burnSum,
      ceiling: pool.windowCeiling,
      dwell_seconds: pool.shedDwellSeconds,
      dwell_until: at + pool.shedDwellSeconds * 1_000,
      excess: Number(value('excess')),
      factor: slowed?.factor ?? null,
      horizon,
      marks: pins.filter((p) => p.marked).map((p) => ({ agent_id: p.agentId, key_id: p.keyId, model: p.model, session_id: p.sessionId })),
      outcome: round.outcome,
      persisted: true,
      pins: pins.map((p) => {
        const figures = read(p);
        return {
          agent_id: p.agentId, burn: p.burn, cold: p.cold, key_id: p.keyId, last_input_tokens: p.lastInputTokens, marked: p.marked, model: p.model, note: p.note,
          per_rewrite: p.burn === null ? null : Number((p.burn / p.lastInputTokens).toPrecision(7)), planned: figures.planned, rewrite: figures.rewrite,
          rewrite_window: figures.rewrite === null ? null : rewriteWindowOf(p.model), runway_ms: figures.runway, session_id: p.sessionId, skip: p.skip,
        };
      }),
      projection,
      reason: round.reason,
      removed: round.removed,
      shed_cap: pool.shedCap,
      stretch_ms: slowed?.stretchMs ?? null,
      target: round.target,
      threshold_ms: slowed ? threshold : null,
      trigger: slowed ? 'slow_down' : 'projection',
      unit: 'window_share',
      utilization: source?.utilization ?? null,
      warm_sessions: pool.warmSessions,
      window,
    },
    keyId: record.request.keyId,
    kind: 'shedding_round',
    model: record.request.canonicalModel,
    poolId: record.request.poolId,
    requestId: record.requestId,
  };
}

/** A move's mark leaving with the row the move took off the account, beside its `pin_migrated`. */
function markCleared(): EventSeed[] {
  return [...DECISIONS.values()].flatMap((record) => {
    const m = record.outcome.markCleared;
    if (m === null) return [];
    const detail = { agent_id: record.request.agentId, marked_at: Date.parse(m.markedAt), marked_on: m.markedOn, marked_window: m.markedWindow, projection: m.projection, reason: m.reason };
    return [{ accountId: m.markedOn, agentId: record.request.agentId, at: record.request.at, detail, keyId: record.request.keyId, kind: 'mark_cleared', model: record.request.canonicalModel }];
  });
}

/**
 * The slow-down spans the reads of each account's entry open and close, as the router emits them from its reads: a
 * start where the factor first reads under 1 with the read's own figures, and an end at the read that finds it back at
 * 1 for the safety wait, its span running from the start to the read that first found 1, its slowest factor the lowest
 * read in between and its held time the slow-down waits of the requests that arrived in it, as the router sums the
 * holds it answered. Studio's closed span ended in the window after the one it slowed, and its open one is the one the
 * afternoon's slowed request and its round stand in.
 */
function slowDownSpans(): EventSeed[] {
  const seeds: EventSeed[] = [];
  const keys = [...new Set(SLOW_READS.map((x) => `${x.accountId}|${x.poolId}`))];
  for (const key of keys) {
    const [accountId, poolId] = key.split('|');
    const pool = poolById.get(poolId)!;
    let span: { factorMin: number; since: number } | null = null;
    let clearSince: number | null = null;
    let readAt = 0;
    const close = (at: number, end: number) => {
      if (span === null) return;
      const since = span.since;
      const heldMs = REQUESTS.filter((r) => r.poolId === poolId && r.accountId === accountId && Date.parse(r.at) >= since && Date.parse(r.at) < end)
        .flatMap((r) => r.held?.waits ?? [])
        .filter((w) => w.reason === 'slow_down')
        .reduce((n, w) => n + w.heldMs, 0);
      seeds.push({ accountId, at: iso(at), detail: { duration_ms: end - since, factor_min: span.factorMin, held_ms: heldMs, since }, kind: 'slow_down_ended', poolId });
      span = null;
      clearSince = null;
    };
    for (const { from, read } of SLOW_READS.filter((x) => x.accountId === accountId && x.poolId === poolId).sort((x, y) => x.from - y.from)) {
      // A dropped entry closes its span at once, at the instant the factor last returned to 1, else at its last read.
      if (read === null) {
        close(from, clearSince ?? readAt);
        continue;
      }
      readAt = from;
      if (read.factor < 1) {
        if (span === null) {
          span = { factorMin: read.factor, since: from };
          const detail = { demand: read.demand, factor: read.factor, reset: read.reset, stretch_ms: read.stretchMs, threshold_ms: pool.slowDownStretchSeconds * 1_000 };
          seeds.push({ accountId, at: iso(from), detail, kind: 'slow_down_started', poolId });
        } else span.factorMin = Math.min(span.factorMin, read.factor);
        clearSince = null;
        continue;
      }
      if (span === null) continue;
      clearSince ??= from;
      if (from - clearSince >= SLOW_DOWN_WAIT_MS) close(from, clearSince);
    }
  }
  return seeds;
}

/**
 * The words a request's end reports for its booking when no usage replaced it: a client gone before the answer, the
 * router out of accounts, a stream that broke, or an answer the upstream refused.
 */
const endCauseOf = (r: RequestLog) => (r.outcome === 'canceled' ? 'canceled' : r.outcome === 'exhausted' ? 'exhausted' : r.outcome === 'incomplete' || r.errorCode === 'stream_undecodable' ? 'transport_error' : 'refused');

/**
 * The bookings a request's end released with no usage to replace them, one per request at its end. A later placement
 * or move, a failover's among them, replaces the request's booking, and a renewal books none and leaves it.
 */
function expiredReservations(): EventSeed[] {
  const booked = new Map<string, DecisionRecord | null>();
  for (const record of [...DECISIONS.values()].sort((x, y) => x.seq - y.seq)) {
    if (record.outcome.kind === 'placed' || record.outcome.kind === 'moved') booked.set(record.requestId, record.outcome.reservation === null ? null : record);
  }
  return [...booked.values()].flatMap((record) => {
    const booking = record?.outcome.reservation;
    const r = record ? requestById.get(record.requestId) : undefined;
    const last = r?.attempts[r.attempts.length - 1];
    if (!record || !booking || !r || !last || r.outcome === 'succeeded') return [];
    const ended = Date.parse(last.startedAt) + (last.durationMs ?? 0);
    const detail = { age_ms: ended - Date.parse(record.request.at), attempt: record.request.attempt, cause: endCauseOf(r), shares: booking.rewrite };
    return [{ accountId: booking.accountId, at: iso(ended), detail, kind: 'reservation_expired', requestId: record.requestId }];
  });
}

/**
 * The events beside the decisions: every pin event and model answer the requests' windows hold, the pins the
 * placements created, the cooldowns the attempts' answers set, and the story's own rows, at least one of every kind.
 */
function buildEvents(): RoutingEvent[] {
  const t = clock;
  const seeds: EventSeed[] = [];
  for (const events of REQUEST_EVENTS.values()) {
    for (const e of events) {
      const detail = e.detail ?? {};
      seeds.push({ accountId: e.accountId, agentId: typeof detail.agent_id === 'string' ? detail.agent_id : null, at: e.at, detail, keyId: e.keyId, kind: e.kind, model: e.model, requestId: typeof detail.source_request_id === 'string' ? detail.source_request_id : null, sessionId: e.sessionId });
    }
  }
  for (const s of SUMMARIES) {
    const record = DECISIONS.get(`${s.requestId}/${s.seq}`);
    if (!record?.outcome.pinEvents.some((e) => e.kind === 'pin_created')) continue;
    seeds.push({ accountId: s.accountId, agentId: s.agentId, at: s.at, detail: { agent_id: s.agentId, epoch: record.outcome.pinWritten?.epoch ?? null }, keyId: s.keyId, kind: 'pin_created', model: s.model, sessionId: s.sessionId });
  }
  // The story's own spent edge, which the poll read before any answer's head: nightly's 5-hour window at its limit
  // within the hour of the least-bad move onto it.
  const nightlyReset = clock(13, 11);
  seeds.push({ accountId: 'nightly', at: iso(clock(12, 1, 5)), detail: { reset: nightlyReset, source: 'poll', source_request_id: null, utilization: 1, window: 'five_hour' }, kind: 'window_spent' });
  // A window's spent edge is the first head of any answer that reads it at its limit, once per account and window.
  const heads = REQUESTS.flatMap((r) => r.attempts.map((a) => ({ a, at: Date.parse(a.startedAt) + (a.durationMs ?? 0), r }))).sort((x, y) => x.at - y.at);
  const spent = new Set<string>([`nightly|five_hour|${iso(nightlyReset)}`]);
  for (const { a, at, r } of heads) {
    for (const w of a.windows) {
      const window = WINDOW_OF[w.id];
      const edge = `${a.accountId}|${window}|${w.resetsAt}`;
      if ((window !== 'five_hour' && window !== 'seven_day') || (w.utilization ?? 0) < 1 || spent.has(edge)) continue;
      spent.add(edge);
      seeds.push({ accountId: a.accountId, at: iso(at), detail: { reset: w.resetsAt === null ? null : Date.parse(w.resetsAt), source: 'head', source_request_id: r.id, utilization: 1, window }, kind: 'window_spent', requestId: r.id });
    }
  }
  for (const r of REQUESTS) {
    for (const a of r.attempts) {
      const answeredAt = Date.parse(a.startedAt) + (a.durationMs ?? 0);
      if (!a.cooldownSet) continue;
      const rejected = a.cooldownSet.reason === 'model_rejected';
      const fast = a.cooldownSet.reason === 'fast_rate_limit';
      const until = Date.parse(a.cooldownSet.until);
      seeds.push({
        accountId: a.accountId,
        at: iso(answeredAt),
        detail: {
          reason: rejected ? 'model_rejected' : fast ? 'fast_rate_limit' : 'rate_limit',
          scope: rejected ? `model:${r.canonicalModel}` : fast ? 'fast' : 'account',
          source_request_id: r.id,
          until,
          window: rejected ? 'model_rejection' : fast ? 'fast' : windowOfReason(a.cooldownSet.reason),
        },
        kind: 'cooldown_set',
        ...(rejected ? { model: r.canonicalModel } : {}),
        requestId: r.id,
      });
    }
  }
  // A pin event names the epoch the record wrote, so the event and the card read one number, and a rule's move the
  // fit, the runway and the window that binds on the destination its gate read.
  for (const seed of seeds) {
    if (seed.kind !== 'pin_migrated') continue;
    const s = SUMMARIES.find((x) => x.requestId === seed.requestId || (x.sessionId === seed.sessionId && x.at === seed.at && x.accountId === seed.accountId));
    const record = s && DECISIONS.get(`${s.requestId}/${s.seq}`);
    const epoch = record?.outcome.pinWritten?.epoch;
    if (epoch !== undefined && epoch !== null) seed.detail.epoch = epoch;
    const landed = record?.outcome.kind === 'moved' ? record.candidates.find((c) => c.accountId === record.outcome.accountId) : undefined;
    if (landed?.fit) Object.assign(seed.detail, { binding: landed.binding, fit: landed.fit, runway_ms: landed.runwayMs });
    // A failover's event carries the cooldown that caused it, in the shape the router serializes, and a rule's move
    // none.
    const cause = record?.outcome.cause;
    if (record) seed.detail.cause = cause ? { reason: cause.reason, scope: cause.scope, window: cause.window } : null;
  }

  const primary = accountOf.get('primary')!;
  const primaryAccess = primary.credential.accessTokenExpiresAt === null ? null : Date.parse(primary.credential.accessTokenExpiresAt);
  const primaryRefresh = primary.credential.refreshTokenExpiresAt === null ? null : Date.parse(primary.credential.refreshTokenExpiresAt);
  const fiveHourReset = Date.parse(primary.windows.find((w) => w.id === '5h')?.resetsAt ?? '') - 5 * HOUR;
  const oldestCooldown = COOLDOWNS.filter((c) => c.scope === 'account' && c.until < STORY_HOUR_FROM && c.until > NOW - DAY).sort((a, b) => a.until - b.until)[0];
  const gap = REQUESTS.filter((r) => r.responseTruncated && Date.parse(r.at) < STORY_HOUR_FROM)[0];
  const secondary = accountOf.get('secondary')!;
  const endsAt = Date.parse(secondary.subscription?.endsAt ?? '');
  const pool = poolById.get('all')!;
  const beforeReset = oauthWindows('primary', fiveHourReset - 1, () => OLDER_FACTOR).find((w) => w.id === '5h')?.utilization ?? null;
  seeds.push(
    ...[roundEvent('req_1eaa9bc1-a18/1', ROUND_PINS), roundEvent('req_9f2e7c5a-c49/2', STRETCH_PINS), roundEvent('req_6e1f3b98-a24/1', NO_RUNWAY_PINS), roundEvent('req_3e7a5c19-f84/1', FABLE_WEEK_PINS)].flatMap((e) => (e === null ? [] : [e])),
    ...markCleared(),
    ...expiredReservations(),
    ...slowDownSpans(),
    // The story hour.
    ...expiries(),
    { accountId: 'secondary', at: iso(t(13, 48, 20)), detail: { attempts: 1, endpoint: 'usage', last_clean_poll_at: t(13, 43), next_attempt_at: t(13, 53), reason: '429' }, kind: 'poll_backoff_started' },
    // Earlier the same day.
    // The refresh rows in the router's shape: primary's commit names its credential's expiry, and nightly's two
    // failures name their class beside the text the manager stored.
    { accountId: 'primary', at: iso(t(12, 52, 10)), detail: { access_expires_at: primaryAccess, refresh_expires_at: primaryRefresh, until: primaryAccess }, kind: 'refresh_succeeded' },
    { accountId: 'nightly', at: iso(t(12, 19, 40)), detail: { cause: 'transport', error: 'transport: connection refused (os error 111)' }, kind: 'refresh_failed' },
    { accountId: 'nightly', at: iso(t(12, 20)), detail: { cause: 'refresh_response_lost', error: 'refresh response lost: connection reset by peer' }, kind: 'refresh_uncertain' },
    { accountId: 'research', at: iso(t(12, 41, 7)), detail: { budget_usd: 20, period: 'day', period_end: startOfDay(t(12, 41, 7)) + DAY, source_request_id: BUDGET_CROSSING_ID, spent_usd: 20.4 }, kind: 'budget_reached', poolId: 'jobs', requestId: BUDGET_CROSSING_ID },
    { accountId: 'primary', at: iso(fiveHourReset), detail: { next_reset: fiveHourReset + 5 * HOUR, observed: 'sample', reset_at: fiveHourReset, utilization_before: beforeReset, window: 'five_hour' }, kind: 'window_reset' },
    { accountId: 'overflow', at: iso(t(11, 30, 12)), detail: { cause: 'funding_refused', detail: 'Your credit balance is too low to access the Anthropic API.', from: 'ready', to: 'funding_required' }, kind: 'account_state_changed' },
    {
      accountId: 'nightly',
      at: iso(t(11, 30, 12)),
      detail: { after: { disabled_reason: null, enabled: true, utilization: 0.31 }, before: { disabled_reason: null, enabled: true, utilization: 1 }, blocked_after: null, blocked_before: 'org_spend_cap_reached', observed_at: t(11, 30, 12) },
      kind: 'fast_credits_changed',
    },
    { accountId: 'nightly', at: iso(OVERAGE_FROM), detail: {}, kind: 'overage_started' },
    { accountId: null, actor: 'admin', at: iso(t(9, 5, 12)), detail: { diff: poolDiff(pool), kind: 'pool_updated', target: 'all' }, kind: 'registry_changed', poolId: 'all' },
    // The ci key's revocation, after the last request it reached selection with.
    { accountId: null, actor: 'admin', at: iso(KEY_REVOKED_AT), detail: { diff: {}, kind: 'key_revoked', target: 'ci' }, keyId: 'ci', kind: 'registry_changed' },
    { accountId: 'primary', at: iso(t(9, 0, 41)), detail: { added: ['claude-opus-5-5'], count: 12, removed: [] }, kind: 'catalog_changed' },
    { accountId: null, at: iso(t(6, 12, 40)), detail: { affinity_entries_loaded: 34, down_since: t(6, 11, 58), holds_lost: true, pins_loaded: 10, shedding_marks_loaded: 0, shedding_rounds_stale: 0, version: '0.1.0', windows_loaded: 5 }, kind: 'router_started' },
    // Once each outside the hour.
    { accountId: 'primary', agentId: 'main', at: iso(t(13, 13, 40)), detail: { agent_id: 'main', reasons: ['expired'] }, keyId: 'laptop', kind: 'pin_expired', model: HAIKU, sessionId: sessionId('5eeed6ce') },
    { accountId: 'nightly', actor: 'admin', agentId: 'e90b', at: iso(t(12, 40, 5)), detail: { agent_id: 'e90b' }, keyId: 'desktop', kind: 'pin_released', model: 'claude-sonnet-5-5', sessionId: sessionId('407dd39b') },
    { accountId: 'primary', at: iso(Date.parse('2026-09-12T20:10:00Z') - Date.parse('2026-09-16T14:30:00Z') + NOW), detail: { since: Date.parse('2026-09-11T16:10:00Z') - Date.parse('2026-09-16T14:30:00Z') + NOW }, kind: 'overage_ended' },
    { accountId: 'legacy', at: iso(NOW - 2 * DAY - 2 * HOUR), detail: { cause: 'refresh_failed', detail: 'refresh_token rejected: invalid_grant', from: 'ready', to: 'reauth_required' }, kind: 'account_state_changed' },
    // The Overflow workspace's balance ran out once before, three days back, and the router's own retry past its
    // five minutes found it funded again.
    { accountId: 'overflow', at: iso(NOW - 3 * DAY - 5 * HOUR), detail: { cause: 'funding_refused', detail: 'Your credit balance is too low to access the Anthropic API.', from: 'ready', to: 'funding_required' }, kind: 'account_state_changed' },
    { accountId: 'overflow', at: iso(NOW - 3 * DAY - 5 * HOUR + 5 * MINUTE + 40_000), detail: { cause: 'funding_retry', detail: null, from: 'funding_required', to: 'ready' }, kind: 'account_state_changed' },
    { accountId: 'primary', at: iso(t(8, 15, 3)), detail: { attempts: 3, duration_ms: 25 * MINUTE, resumed_at: t(8, 15, 3) }, kind: 'poll_backoff_ended' },
    { accountId: 'research', at: iso(startOfDay(t(14, 30))), detail: { period: 'day', period_start: startOfDay(t(14, 30)) - DAY, previous_spent_usd: 18.2 }, kind: 'budget_period_ended', poolId: 'jobs' },
    // Drawn at the instant it fires: the subscription's end less the pool's wind-down, which is still ahead.
    { accountId: 'secondary', at: iso(endsAt - pool.windDownMinutes * MINUTE), detail: { end_source: 'cancelled', ends_at: endsAt, wind_down_minutes: pool.windDownMinutes }, kind: 'wind_down_started', poolId: 'all' },
  );
  if (oldestCooldown) seeds.push({ accountId: oldestCooldown.accountId, at: iso(oldestCooldown.until), detail: { cause: 'expired', scope: oldestCooldown.scope }, kind: 'cooldown_cleared' });
  if (gap) seeds.push({ accountId: null, at: iso(Date.parse(gap.at) + (gap.durationMs ?? 0)), detail: { reason: 'chunk_skipped', request_id: gap.id }, kind: 'ledger_gap', requestId: gap.id });
  return seeds
    .sort((a, b) => Date.parse(a.at) - Date.parse(b.at) || a.kind.localeCompare(b.kind))
    .map((e, i) => ({ type: 'event', accountId: e.accountId, actor: e.actor ?? null, agentId: e.agentId ?? null, at: e.at, detail: e.detail, id: i + 1, keyId: e.keyId ?? null, kind: e.kind, model: e.model ?? null, poolId: e.poolId ?? null, requestId: e.requestId ?? null, sessionId: e.sessionId ?? null }));
}

/** Every event kind of the ledger at least once, oldest first by id. */
export const ROUTING_EVENTS: RoutingEvent[] = buildEvents();

const DECISION_KINDS: readonly string[] = ['exhausted', 'held', 'moved', 'placed', 'refused', 'renewed'];
/** Every event kind the ledger writes, as the route validates `kinds` against them. */
const EVENT_KINDS: readonly string[] = [
  'account_state_changed', 'budget_period_ended', 'budget_reached', 'catalog_changed', 'cooldown_cleared', 'cooldown_set', 'fast_credits_changed', 'ledger_gap', 'mark_cleared', 'model_permitted', 'model_rejected',
  'overage_ended', 'overage_started', 'pin_created', 'pin_expired', 'pin_migrated', 'pin_released', 'poll_backoff_ended', 'poll_backoff_started', 'refresh_failed',
  'refresh_succeeded', 'refresh_uncertain', 'registry_changed', 'reservation_expired', 'router_started', 'shedding_round', 'slow_down_ended', 'slow_down_started', 'warm_prefix_expired', 'wind_down_started',
  'window_reset', 'window_spent',
];

/** The route's merged order: newest first, a decision before an event at one instant. */
const cursorOf = (row: RoutingLogPage['rows'][number]) => (row.type === 'decision' ? `${Date.parse(row.at)}:1:${row.requestId}/${row.seq}` : `${Date.parse(row.at)}:0:${row.id}`);
function after(row: RoutingLogPage['rows'][number], cursor: string): boolean {
  const [at, rank, ...rest] = cursor.split(':');
  const id = rest.join(':');
  const rowAt = Date.parse(row.at);
  if (rowAt !== Number(at)) return rowAt < Number(at);
  if (row.type === 'decision') {
    if (rank !== '1') return false;
    const slash = id.lastIndexOf('/');
    const requestId = id.slice(0, slash);
    const seq = Number(id.slice(slash + 1));
    return row.requestId < requestId || (row.requestId === requestId && row.seq < seq);
  }
  return rank === '1' || row.id < Number(id);
}
const order = (a: RoutingLogPage['rows'][number], b: RoutingLogPage['rows'][number]) => {
  const at = Date.parse(b.at) - Date.parse(a.at);
  if (at !== 0) return at;
  if (a.type !== b.type) return a.type === 'decision' ? -1 : 1;
  if (a.type === 'decision' && b.type === 'decision') return a.requestId === b.requestId ? b.seq - a.seq : a.requestId < b.requestId ? 1 : -1;
  return (b as RoutingEvent).id - (a as RoutingEvent).id;
};

/**
 * The merged page by the route's rules. The range runs from an hour back by default. An absent `kinds` is every
 * decision and event, the probe decisions among them, and `kinds` names exactly what it lists, `probe` for a probe's
 * decision of any kind. `accounts` matches a decision's source too. `limit + 1` rows are read so a cursor
 * is offered only when a next page exists, and the counts come under a request filter.
 */
export function listRoutingLog(query: RoutingLogQuery, now: number): RoutingLogPage {
  const from = query.from ? Date.parse(query.from) : now - HOUR;
  const to = query.to ? Date.parse(query.to) : now;
  // An empty list is no filter, as the client omits it from the query.
  const kinds = query.kinds?.length ? query.kinds : undefined;
  const within = (at: string) => Date.parse(at) >= from && Date.parse(at) <= to;
  const inList = (list: string[] | undefined, value: string | null) => !list?.length || (value !== null && list.includes(value));
  const session = (value: string | null) => !query.session || (value !== null && value.startsWith(query.session));
  const decisions = SUMMARIES.filter(
    (s) =>
      within(s.at) &&
      (!query.request || s.requestId === query.request) &&
      session(s.sessionId) &&
      inList(query.keyIds, s.keyId) &&
      inList(query.poolIds, s.poolId) &&
      inList(query.models, s.model) &&
      (!query.accountIds?.length || query.accountIds.some((id) => id === s.accountId || id === s.fromAccountId)) &&
      (kinds === undefined || (kinds.includes(s.kind) && !s.probe) || (kinds.includes('probe') && s.probe)),
  );
  const events = ROUTING_EVENTS.filter(
    (e) =>
      within(e.at) &&
      (!query.request || e.requestId === query.request) &&
      session(e.sessionId) &&
      inList(query.keyIds, e.keyId) &&
      inList(query.poolIds, e.poolId) &&
      inList(query.models, e.model) &&
      inList(query.accountIds, e.accountId) &&
      (kinds === undefined || kinds.includes(e.kind)),
  );
  const merged = [...decisions, ...events].sort(order);
  const rest = query.cursor ? merged.filter((row) => after(row, query.cursor!)) : merged;
  const limit = query.limit ?? 25;
  const rows = rest.slice(0, limit);
  const last = rows[rows.length - 1];
  return {
    counts: query.request ? { decisions: decisions.length, events: events.length } : null,
    nextCursor: rest.length > limit && last ? cursorOf(last) : null,
    rows,
  };
}

/** The kinds the route accepts in `kinds`: every decision and event kind, and the probe pseudo-kind. */
export const ROUTING_LOG_KINDS: readonly string[] = [...DECISION_KINDS, 'probe', ...EVENT_KINDS];

/** The record route's answer, or null for a decision no request of the mock made. */
export function getDecision(requestId: string, seq: number): DecisionRecord | null {
  return DECISIONS.get(`${requestId}/${seq}`) ?? null;
}
