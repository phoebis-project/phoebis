import { encodeMembers, encodeModelRules, encodeTtlRules, isoOrNull } from '@/api/decode';
import type {
  Account, AllowanceReset, Attempt, AttemptWindow, CohortHold, DecisionKind, DecisionRecord, DecisionSummary, Exhaustion, MoveCause, PinReason, Placement, PrefixEntry, RecordBudget, RecordCandidate, RecordCompletion, RecordCooldown, RecordHold, RecordOutcome,
  PoolKeepalive, RecordPin, RecordRequest, RecordSettings, Refusal, RequestEvent, RequestLog, ResetOutcome, RoutingEvent, RoutingLogPage, RoutingLogQuery, Score, Subscription, SummaryFact, WalkFact, WalkStep,
} from '@/api/types';
import { DECISION_KIND_VALUES } from '@/api/types';
import { modelWeeks } from '@/lib/costs';
import { lookup } from '@/lib/counts';
import { servingAttempt } from '@/lib/dimensions';
import { placementHit } from '@/lib/format';
import { SHED_CAUSES, SHED_MARK_SHARE } from '@/lib/presets';
import { MODEL_SCOPE_PREFIX, endOf, flagOf, isPaidSource, readWindows, windowOf } from '@/lib/quota';
import { compareRows, movedFrom } from '@/lib/routingLog';
import { reservationRoundsRun } from '@/lib/selection';
import { WINDOW_KEY, hoursOfKey } from '@/lib/windows';
import { ACCOUNTS, LEGACY_FIVE_HOUR_SPENT_AT, LEGACY_LAPSED_AT, SHARED_WINDOWS, SUNSET_WEEK_RESETS_IN, accountById, lastReadingOf, rankTier } from './accounts';
import { promptOf } from './catalog';
import { PRIOR_BURN, bindingOf, ceilingOn, runwaysOf, shownOf, stretchProjectionOf, windowFitOf, type Runway } from './fit';
import { GUARDRAILS } from './guardrails';
import { leadOf, leftPerHour, paceDeadline, paceOf } from './lead';
import { SESSIONS } from './operations';
import { DELETED_ACCOUNTS, KEY_REVOKED_AT, RETIRED, WORST_BUCKET, affinityTtlMsOf, keyById, poolById, storyAccountOf, type PoolSeed } from './pools';
import { ACCOUNT_BOUND_CALL_ID, ACCOUNT_BOUND_TURN_IDS, ATTEMPT_COOLDOWNS, BUDGET_CROSSING_ID, CLIENTS, CLIMB_SESSION_ID, COUNTED_BODY_TOKENS, FORCED_MOVE_ID, HAIKU, JUDGED_TURN_IDS, MARKED_SESSION_IDS, MOVED_PIN_CALL_ID, MOVED_PIN_PROBE_DUE, MOVED_PIN_PROBE_ID, MOVED_PIN_TURN_ID, NO_PIN_DRAWS, OPENING_SESSION_IDS, OPUS_MORNING_SESSION_ID, OVERAGE_FROM, RELEASED_SESSION_ID, RENEWED_ON_PIN, REQUEST_EVENTS, REQUESTS, RESERVATION_FABLE_SESSION_ID, RESERVATION_MARKED_SESSION_ID, REWRITE_HELD_ID, ROUTER_RESTARTED_AT, SECONDARY_WEEK_OVER_AT, SESSION_IDS, SLOWED_SESSION_ID, STORY_HOUR_FROM, STORY_IDS, TIE_STORY_ID, UNAVAILABLE_TURN_ID, UNSEEDED_IDS, accountBoundAt, attemptEnd, carriesConversation, cooldownOf, failsOver, holds, inOverageAt, migrationAt, oauthWindows, pinCacheTtlOf, pinKeyOf, pinReadAt, pinStampOf, requestById, resentInPlace, sweepAfter, type CooldownRow } from './requests';
import { cooldownRefusal, FABLE_USE, RESET_BLOCKS, type MockGrant, type MockUse } from './resets';
import { drawnWindows, dropSizeOf, droppedThinkingOf, moveSizeOf, proWeekRewriteOf, rewriteTokensOf, rewriteWindowOf } from './rewrite';
import { createRng } from './rng';
import { round } from './round';
import { TABLES } from './samples';
import { DAY, HOUR, MINUTE, NIGHTLY_CAP_RAISED, NOW, OVERFLOW_LAPSED_AT, OVERFLOW_PAUSED_AT, RESEARCH_BUDGET_AT, clock, endOfMonth, iso, startOfDay, startOfMonth } from './time';

/** Why a move rule fired, as the trace's `moved` names it. A failover's reasons move the pin from the task instead. */
const RULES = ['spent', 'overage', 'return_to_plan', 'drain', 'shed'] as const;
const FAILOVER_REASONS = ['cooldown', 'fast_lane'];
const FIVE_HOURS = 5 * HOUR;
const FIVE_MINUTES = 5 * MINUTE;
/** How long a booking lives before the purge expires it, the router's `IN_FLIGHT_RECLAIM_MS`. */
const RESERVATION_MS = HOUR;

/**
 * The share of each plan window one written token takes, as the story's tiers price a rewrite: a Fable token at the
 * window's own rate and the other families at their weight. A model without a weight of its own prices at its
 * family's, as the router estimates it, and only a Fable model draws its model week.
 */
const WRITE_SHARE: Record<string, number> = { five_hour: 1.2e-7, seven_day: 1.4e-8, seven_day_fable: 2.8e-8 };
const familyWeight = (model: string) => (model.includes('fable') ? 1 : model.includes('opus') ? 0.5 : model.includes('sonnet') ? 0.25 : 0.08);
const round3 = (x: number) => round(x, 3);
const round5 = (x: number) => round(x, 5);
/** A rewrite of `tokens` per plan window the model draws, in window share. */
const rewriteOf = (tokens: number, model: string): Record<string, number> =>
  Object.fromEntries(drawnWindows(model).map((w) => [w, round5(tokens * WRITE_SHARE[w] * familyWeight(model))]));
/**
 * The tokens a request's body carries, the relay's estimate a fresh session's rewrite is priced from. The mock reads
 * the usage the request reported, so a request that ended with no usage, which the router books at its body's
 * estimate, books and later expires a zero share here. The router stores no usage for a token count, so a count reads
 * the estimate the mock keeps beside its row.
 */
const bodyTokensOf = (r: RequestLog) => (r.kind === 'count_tokens' ? (COUNTED_BODY_TOKENS.get(r.id) ?? 0) : promptOf(r.usage));

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
  /**
   * The rows a long session's answer writes where its boundaries are new to the account, in place of the one row the
   * builder derives: the message chain's depth, and the size of each static boundary another key held there.
   */
  cacheMap?: { messages: number; staticSizes: number[] };
  /**
   * The selections whose in-flight draws the decision takes, set for a drawn probe that sent nothing, whose one answer
   * takes the draws of every selection it drew. Unset, the count is one, and none for a probe's pinned answer after its
   * leg.
   */
  drawnSelections?: number;
  /** An exhaustion the story names, which the builder gives the status the router answers it with. */
  exhaustion?: Omit<Exhaustion, 'status'> | null;
  /** A summary fact the story names where the router's own reading is not what the row tells. */
  fact?: SummaryFact[];
  /** The pin event reason of a failover placement, which moves the pin from the task rather than by a rule. */
  failover?: string;
  /** The account the pin was on before a move or a failover, or the one a probe's pinned answer names. */
  from?: string;
  /** The holds before this attempt's placement, summed as the placement's row names them. */
  heldBefore?: CohortHold;
  /**
   * A hold for a slot names its place, one for a spent window's reset the reset it waits for, and one for a rewrite in
   * flight the request whose head it waits for.
   */
  hold?: { cohort: string | null; place: 'cohort' | 'reset' | 'rewrite' | 'session_pin' | 'slow_down' | 'strategy_pick'; until?: number; waitsFor?: string };
  /** The holds the request waited through before this decision, each with the attempt that waited. */
  holds?: RecordRequest['holds'];
  /** The slots the story fills by hand: every member at its cap under a hold, or a member it holds full. */
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
  /**
   * The live pin's whole list of reasons where the state of its account forced the placement to move it, as the
   * move's event carries them. The decision stays a placement that names the account it left.
   */
  reasons?: string[];
  refusal?: Refusal;
  /** The expired row a first placement released, with the reasons its release names. */
  replaced?: { accountId: string; reasons: string[] };
  request: RequestLog;
  round?: ShedRound;
  rule?: string;
  rung?: Placement | null;
  seq: number;
  /** Whether the story wrote the decision by hand, so it reads the burns the accounts carry. */
  story?: boolean;
}

/**
 * A pin of a round in the round's order, with its burn per hour on the window that fired. A pin on the fast lane, one
 * whose model does not draw that window, one whose agent ended and one with no priced burn there leave the round before
 * the order, and the record names none of them a burn.
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

/**
 * A pool's edit, the registry change the routing log records: its instant, and the settings and the keepalive policy
 * it moved, each at its value before the edit, which the change carries as each pair's old value, and the instant the
 * settings before it took force. The story's other rows read neither, so no row before the edit contradicts them, and
 * a record carries no policy.
 */
const BEFORE_EDIT: Record<string, { at: number; keepalive: Partial<PoolKeepalive>; settings: Partial<RecordSettings> }> = {
  all: { at: clock(9, 5, 12), keepalive: { maxIdleSeconds: 43_200 }, settings: { minSharedPrefixTokens: 10_000, poolUpdatedAt: iso(NOW - 9 * DAY), slowDownHoldSeconds: 60 } },
};

/**
 * The stamp a pool's row held at `at`: its last write by then, the edit or the members write that placed a deleted
 * account, else the stamp the edit replaced, else the row's own stamp. The delete of an account moves none, as its
 * member row leaves by the cascade.
 */
function stampAt(pool: PoolSeed, at: number): string {
  const edit = BEFORE_EDIT[pool.id];
  const writes = [edit?.at, ...DELETED_ACCOUNTS.filter((d) => d.poolId === pool.id).map((d) => d.createdAt)].filter((w): w is number => w !== undefined && w <= at);
  return writes.length === 0 ? (edit?.settings.poolUpdatedAt ?? pool.updatedAt) : iso(Math.max(...writes));
}

/**
 * The decisions an earlier build of the router recorded, keyed `${requestId}/${seq}`. One build wrote a decision's
 * record and its round, so both lack each member that build did not write: the record's settings hold no Fable
 * reservation option, and `roundEvent` names what the round lacks.
 */
const EARLIER_BUILD: ReadonlySet<string> = new Set(['req_1eaa9bc1-a18/1', 'req_3e7a5c19-f84/1']);

/**
 * The pool's settings as the record decided at `at` names them, by the pool fields they are read from, under the stamp
 * the pool held then: a record decided before the pool's edit carries the values the edit moved, and a record of the
 * earlier build no Fable reservation option.
 */
function settingsOf(pool: PoolSeed, at: number, decision: string): RecordSettings {
  const edit = BEFORE_EDIT[pool.id];
  return {
    ...settingsNow(pool),
    ...(EARLIER_BUILD.has(decision) ? { fableReservationShedding: null } : {}),
    poolUpdatedAt: stampAt(pool, at),
    ...(edit !== undefined && at < edit.at ? edit.settings : {}),
  };
}

function settingsNow(pool: PoolSeed): RecordSettings {
  return {
    affinityMode: pool.affinityMode,
    affinityTtlSeconds: pool.affinityTtlSeconds,
    burnAverageSeconds: pool.burnAverageSeconds,
    fableReservationShedding: pool.fableReservationShedding,
    fiveHourCeiling: pool.fiveHourCeiling,
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
    weeklyCeiling: pool.weeklyCeiling,
    windDownMinutes: pool.windDownMinutes,
  };
}

/**
 * The share an older window's reading walks back to where no attempt names it. A draw would move the story's
 * streams.
 */
const OLDER_FACTOR = 0.8;

/** Whether the decision sent an attempt: a placement, a renewal or a move. */
const isDispatched = (spec: Spec) => spec.kind === 'placed' || spec.kind === 'renewed' || spec.kind === 'moved';

/** The attempt a dispatching decision sent, whose answer the drawer shows. */
const dispatchedAttempt = (spec: Spec) => (isDispatched(spec) ? spec.request.attempts[spec.attempt - 1] : undefined);

/**
 * Where a held decision holds in place, null for every other decision. A hold for a spent window's reset ends the walk
 * at the spent rule, so it reads no placement's partitions. A slow-down hold waits on the pin the move rules left in
 * place, before its renewal, so it reads no placement either, and so does a moved pin's wait for the head of the
 * request that carried the move.
 */
const inPlaceOf = (spec: Spec): 'reset' | 'rewrite' | 'slow_down' | null => {
  const place = spec.hold?.place;
  return spec.kind === 'held' && (place === 'reset' || place === 'rewrite' || place === 'slow_down') ? place : null;
};

/** Whether the decision left a live pin: a rule's move, or a placement the pin's account state forced. */
const leftPin = (spec: Spec) => spec.kind === 'moved' || spec.reasons !== undefined;

/**
 * Whether the decision is a probe's pinned answer: the router answers the probe its pinned member's obstacle at the
 * probe step, so it reads no fit and walks no placement behind it.
 */
const pinnedAnswerOf = (spec: Spec) => spec.exhaustion?.kind === 'pinned';

/** Whether the decision walks the placement: a placement, a move, a hold or an exhaustion, none in place or pinned. */
const walksPlacement = (spec: Spec) => (spec.kind === 'placed' || spec.kind === 'moved' || spec.kind === 'held' || spec.kind === 'exhausted') && inPlaceOf(spec) === null && !pinnedAnswerOf(spec);

/** Whether the router held the request out at the cap, which the story does only where a hold ran out. */
const heldOut = (spec: Spec) => (spec.holds ?? []).some((h) => h.ranOut);

/**
 * The windows a decision read on a member: the attempt's own reading on the account it went to, so the card and the
 * drawer agree, and the walk back at the decision's time elsewhere. A 429's head reports the limit past the reading
 * the decision read, and an attempt with no head stores no reading, so a rate-limited or a headless attempt's account
 * reads the walk back too.
 */
function readingsOf(spec: Spec, accountId: string): AttemptWindow[] {
  const story = spec.readings?.[accountId];
  if (story) return story;
  const attempt = dispatchedAttempt(spec);
  if (attempt?.accountId === accountId && attempt.windows.length > 0 && attempt.upstreamStatus !== 429) return attempt.windows;
  return oauthWindows(accountId, spec.at, () => OLDER_FACTOR);
}

/** A failover's cause from the cooldown that failed it over, in the words its cooldown row carries. */
const causeOf = (reason: string, model: string): MoveCause => {
  const c = cooldownOf(reason, model);
  return { cause: null, reason: c.reason, scope: c.scope, state: null, window: c.window };
};

const cooldownRows: CooldownRow[] = [
  ...ACCOUNTS.flatMap((a) =>
    a.cooldowns.map((c) => ({ accountId: a.id, reason: c.reason.startsWith('rate_limit') ? 'rate_limit' : c.reason, scope: c.scope, setAt: Date.parse(c.setAt), until: Date.parse(c.until), window: c.scope === 'fast' ? 'fast' : c.windowId === '5h' ? 'five_hour' : c.scope.startsWith(MODEL_SCOPE_PREFIX) ? 'model_rejection' : (c.windowId ?? 'retry_after') })),
  ),
  ...ATTEMPT_COOLDOWNS.map(({ model: _model, requestId: _requestId, ...row }) => row),
];
// A story attempt set the row its account carries now, so one cooldown is listed once.
const COOLDOWNS: CooldownRow[] = [...new Map(cooldownRows.map((c) => [`${c.accountId}|${c.scope}|${c.until}`, c])).values()];

/** A member's spend as the ledger reported it at `at`: research crossed its day's budget at 12:41:07. */
function budgetsAt(pool: PoolSeed, accountId: string, at: number): RecordBudget[] {
  const member = pool.members.find((m) => m.accountId === accountId);
  if (!member) return [];
  const spentOf = (period: 'day' | 'month') => {
    const now = member.spends.find((s) => s.period === period)?.spentUsd ?? 0;
    return accountId === 'research' && period === 'day' && at < RESEARCH_BUDGET_AT ? 19.62 : now;
  };
  const rows: RecordBudget[] = [];
  if (member.budgetDailyUsd !== null) rows.push({ budgetUsd: member.budgetDailyUsd, observedAt: iso(at), period: 'day', periodEnd: iso(startOfDay(at) + DAY), periodStart: iso(startOfDay(at)), spentUsd: spentOf('day') });
  if (member.budgetMonthlyUsd !== null) rows.push({ budgetUsd: member.budgetMonthlyUsd, observedAt: iso(at), period: 'month', periodEnd: iso(endOfMonth(at)), periodStart: iso(startOfMonth(at)), spentUsd: spentOf('month') });
  return rows;
}

/** The account's state at `at`: the Overflow workspace's credit balance ran out at 11:30:12. */
const stateAt = (a: Account, at: number): string => {
  if (a.id === 'overflow') return at < OVERFLOW_LAPSED_AT ? 'ready' : 'funding_required';
  return flagOf(a.status) ?? 'ready';
};

/** Whether the account's routing was on at `at`: the operator paused the Overflow workspace at 11:33:00. */
const enabledAt = (a: Account, at: number): boolean => (a.id === 'overflow' ? at < OVERFLOW_PAUSED_AT : a.routingEnabled);

/**
 * The usage credits a fast request draws: nightly's spend cap was raised at 11:30:12, and secondary's plan has
 * none.
 */
const creditsAt = (accountId: string, at: number): RecordCandidate['credits'] => {
  if (accountId === 'primary') return { blocked: null, disabledReason: null, enabled: true, observedAt: iso(at - MINUTE), utilization: 0.3 };
  if (accountId === 'secondary') return { blocked: 'extra_usage_disabled', disabledReason: null, enabled: false, observedAt: iso(at - MINUTE), utilization: null };
  if (accountId !== 'nightly') return null;
  const capped = at < NIGHTLY_CAP_RAISED;
  return { blocked: capped ? 'org_spend_cap_reached' : null, disabledReason: null, enabled: true, observedAt: iso(at - MINUTE), utilization: capped ? 1 : 0.31 };
};

/**
 * What keeps a member out of the eligible set, in the engine's order: the operator's state, an ended subscription, a
 * cooldown that holds the request, the fast credits, the budget, the fast lane's own limit, and last the attempt this
 * request already made, so a member it tried reads the cooldown that answered rather than the attempt.
 */
function obstacleOf(a: Account, pool: PoolSeed, at: number, model: string, fast: boolean, attempted: string[]): RecordCandidate['obstacle'] {
  const plain = (name: string, reason: string | null = null, until: string | null = null) => ({ name, reason, scope: null, until, window: null });
  const state = stateAt(a, at);
  if (state !== 'ready' || !enabledAt(a, at)) return plain('operator');
  const endsAt = a.subscription?.endsAt ? Date.parse(a.subscription.endsAt) : null;
  if (endsAt !== null && endsAt <= at) return plain('subscription_ended');
  const live = COOLDOWNS.filter((c) => c.accountId === a.id && c.setAt <= at && at < c.until && holds(c, model, fast));
  const cooldown = (c: CooldownRow) => ({ name: 'cooldown', reason: c.reason, scope: c.scope, until: iso(c.until), window: c.window });
  const blocking = live.find((c) => c.scope !== 'fast');
  if (blocking) return cooldown(blocking);
  const credits = fast ? creditsAt(a.id, at) : null;
  if (credits?.blocked) return plain('credits', credits.blocked);
  // The engine names the reached budget whose period ends last, the one that keeps the member out longest.
  const reached = budgetsAt(pool, a.id, at).filter((b) => b.spentUsd !== null && b.spentUsd >= b.budgetUsd).sort((x, y) => Date.parse(y.periodEnd ?? '') - Date.parse(x.periodEnd ?? ''))[0];
  if (reached) return plain('budget', reached.period, reached.periodEnd);
  const fastLimited = live.find((c) => c.scope === 'fast');
  if (fastLimited) return cooldown(fastLimited);
  if (attempted.includes(a.id)) return plain('attempted');
  return null;
}

/** The prefix TTL the pool's rules give a request: the plan's per class and model, the metered rules on a key. */
function ttlOf(pool: PoolSeed, kind: Account['kind'], subagent: boolean, model: string): number {
  if (!pool.cacheTtl.enabled) return FIVE_MINUTES;
  const rules = kind === 'api_key' ? pool.cacheTtl.metered : pool.cacheTtl.plan;
  const rule = rules.models.find((m) => m.model === model);
  const ttl = subagent ? (rule?.subagent ?? rules.subagent) : (rule?.main ?? rules.main);
  return ttl === '1h' ? HOUR : FIVE_MINUTES;
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

/** The burn per hour the account routes answer on each plan window of an account. */
const ACCOUNT_BURNS = new Map(
  ACCOUNTS.map((a) => [
    a.id,
    Object.fromEntries(
      readWindows(a).flatMap((w) => {
        const name = WINDOW_KEY[w.id];
        return name !== undefined && w.burn ? [[name, w.burn]] : [];
      }),
    ) as Record<string, number>,
  ]),
);
/**
 * Primary's 5-hour burn before the afternoon round's current session lands there, the burn of the sessions it carries.
 */
const PRIMARY_EARLIER_BURN = 0.085;
/**
 * The 7-day Fable window's burn on primary while a Fable pin of all stands there through the morning, the pins' demand
 * as the reservation's test reads it. The desktop Fable conversation's turns are few, so no clock hour of primary's
 * Fable record holds that rate, and the demand leaves with the pin.
 */
const RESERVATION_FABLE_BURN = 0.008;
/** The pin key of the desktop Fable conversation, whose pin on primary draws the Fable week through the morning. */
const FABLE_PIN_KEY = pinKeyOf(requestById.get('req_2d8f6b14-a79')!);
/** The instant of a story row, read off its request row. The build fails where the story lacks the row. */
const storyAt = (requestId: string, lacks: string): number => {
  const row = requestById.get(requestId);
  if (!row) throw new Error(`the story lacks ${lacks}`);
  return Date.parse(row.at);
};
/** The placement that lands the afternoon round's current session on primary, read off its row. */
const CURRENT_SESSION_LANDED_AT = storyAt('req_b2bed817-a58', 'the placement of the session the afternoon round reads as current');
/**
 * The burns a decision read on an account: the story's hand-written decisions read the sessions the accounts carry, and
 * a drawn decision reads none, so it projects its reading. The router sums the pins an account holds at the decision,
 * and a placement reads the sum before its own booking, so primary's 5-hour window reads the round's current session
 * after its placement alone, and its Fable week the morning's Fable pins behind the first one's landing, while one
 * stands there. A burn the decision writes wins.
 */
const burnsOf = (spec: Spec, accountId: string): Record<string, number> => {
  if (!spec.story) return {};
  const written = spec.burns?.[accountId];
  if (written) return written;
  const row = ACCOUNT_BURNS.get(accountId) ?? {};
  if (accountId !== 'primary') return row;
  const p = pins.get(FABLE_PIN_KEY);
  const fable: Record<string, number> = p !== undefined && p.accountId === 'primary' && pinReadOf(p, spec.at) !== 'none' ? { seven_day_fable: RESERVATION_FABLE_BURN } : {};
  return spec.at <= CURRENT_SESSION_LANDED_AT ? { ...row, ...fable, five_hour: PRIMARY_EARLIER_BURN } : { ...row, ...fable };
};

/**
 * The instant a window's projection runs to, and what set it, as the router's `horizon_of` reads them: the 5-hour
 * window's reset, or five hours out where no reset is read, and the earlier of five hours out and the reset on a weekly
 * window, each no later than the account's end. A reset or an end discards the quota left at it, so either bounds the
 * horizon at the wall, a tie with the five-hour bound included, and the five-hour bound alone at the span.
 */
function horizonOf(name: string, resetsAt: string | null, end: number | null, at: number): { at: number; bound: 'span' | 'wall' } {
  const reset = resetsAt === null ? null : Date.parse(resetsAt);
  const span = at + FIVE_HOURS;
  const byReset = reset !== null && (name === 'five_hour' || reset <= span);
  const horizon = byReset ? reset : span;
  const byEnd = end !== null && end <= horizon;
  return { at: Math.max(at, byEnd ? end : horizon), bound: byReset || byEnd ? 'wall' : 'span' };
}

/**
 * The account a candidate names: the registry's, or a deleted account's as it stood, which a record decided before its
 * delete still lists.
 */
const accountOf = (id: string): Account => {
  const account = storyAccountOf(id);
  if (account === undefined) throw new Error(`a candidate names an account the story lacks: ${id}`);
  return account;
};

/**
 * The line a window's projection is tested against, as the router's `HorizonBound::limit` reads it: the wall, 1, at a
 * horizon a reset or the account's end set, and the window's own ceiling at the five-hour bound.
 */
const lineOf = (c: RecordCandidate, window: string, pool: Pick<PoolSeed, 'fiveHourCeiling' | 'weeklyCeiling'>, at: number): number =>
  horizonOf(window, c.windows[window]?.reset ?? null, endOf(accountOf(c.accountId), at), at).bound === 'wall' ? 1 : ceilingOn(pool, window);

/**
 * The line a round tests a window against: the 5-hour ceiling for the 5-hour window of a pool that slows its pins,
 * since the stretch tests that window there at every horizon, and the line its horizon's bound sets for any other.
 */
const triggerLineOf = (c: RecordCandidate, w: string, pool: Pick<PoolSeed, 'fiveHourCeiling' | 'slowDown' | 'weeklyCeiling'>, at: number): number =>
  pool.slowDown && w === 'five_hour' ? ceilingOn(pool, w) : lineOf(c, w, pool, at);

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

const studioSlowed = accountById.get('studio')?.slowDown ?? null;
const studioDemand = accountById.get('studio')?.windows.find((w) => w.id === '5h')?.demand ?? 0;
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
function slowFactsOf(source: RecordCandidate | undefined, pool: PoolSeed, at: number, cycled: boolean): WalkFact[] {
  if (!pool.slowDown) return [fact('slow_down', 'off')];
  const read = source ? slowReadAt(source.accountId, pool, at) : null;
  if (!source || read === null) return [];
  return [
    fact('slow_down', 'on'),
    fact('stretch_ms', read.stretchMs, 'ms'),
    fact('threshold_ms', pool.slowDownStretchSeconds * 1_000, 'ms'),
    fact('factor', read.factor),
    // The engine's share of the factor the holds apply: the time since the span's first read under 1 over the pool's
    // burn average.
    fact('ramp', rampAt(source.accountId, pool, at), 'percent'),
    // The speed the account's pins realize under the holds, 1 while none of them holds a cycle to be held by.
    fact('speed', cycled ? read.speed : 1),
    fact('demand_five_hour', source.windows.five_hour?.demand ?? 0, 'percent_per_hour'),
    ...(read.overSince === null ? [] : [fact('over_since', read.overSince, 'time')]),
  ];
}

/**
 * The account's slow-down read standing at `at`: none on a pool with the switch off or for an account it never slowed.
 */
const slowReadAt = (accountId: string, pool: PoolSeed, at: number): SlowRead | null =>
  pool.slowDown ? (SLOW_READS.filter((x) => x.accountId === accountId && x.poolId === pool.id && x.from <= at).pop()?.read ?? null) : null;

/**
 * The ramp standing at `at`, 0 at a factor of 1 and capped at 1. A read at 1 or the purge ends the span, so the next
 * read under 1 starts the ramp afresh, as the engine's stamp does. No story read stands at or past the ceiling, where
 * the engine's ramp reads 1 at once, and no span under 1 crosses a reset, where the engine drops the stamp, so the
 * helper reads neither case.
 */
function rampAt(accountId: string, pool: PoolSeed, at: number): number {
  let since: number | null = null;
  for (const x of SLOW_READS) if (x.accountId === accountId && x.poolId === pool.id && x.from <= at) since = x.read === null || x.read.factor >= 1 ? null : (since ?? x.from);
  return since === null ? 0 : round3(Math.min(1, (at - since) / (pool.burnAverageSeconds * 1_000)));
}

/**
 * The seat's first record, which carries no spend entry, as a record does where the seat's share lapsed at its reset or
 * was not yet read, so the Pace hover's sentence rests on the paid pace's shape alone.
 */
const SPEND_UNREAD: ReadonlySet<string> = new Set(['req_c94d1e07-b35']);

/**
 * A candidate's Pace facts off the readings and the burns its record holds, with the Fable reservation where `reserve`
 * says the member stands in the pick's first tier.
 */
function paceFactsOf(a: Account, at: number, model: string, c: Pick<RecordCandidate, 'burn' | 'windows'>, reserve: boolean): NonNullable<RecordCandidate['pace']> {
  const p = paceOf(a, at, model, { burns: c.burn, readings: c.windows, reserve });
  return { burn: p.burn, deadline: isoOrNull(p.deadline), end: isoOrNull(p.end), fableReservation: p.fableReservation, hours: p.hours === null ? null : round(p.hours, 1), remaining: p.remaining, reset: isoOrNull(p.reset), score: p.score, window: p.window };
}

/**
 * The pool's members at `at` with their accounts: today's, and each account the story deleted that stood in the pool
 * from its creation up to its delete.
 */
const membersAt = (pool: PoolSeed, at: number) => [
  ...pool.members.map((member) => ({ account: accountById.get(member.accountId)!, member })),
  ...DELETED_ACCOUNTS.filter((d) => d.poolId === pool.id && d.createdAt <= at && at < d.deletedAt),
];

/** Every pool member's standing at the decision's time, eligible or not. */
function candidatesOf(spec: Spec, pool: PoolSeed): RecordCandidate[] {
  const r = spec.request;
  const fast = r.shape.requestedSpeed === 'fast';
  const at = spec.at;
  const members = membersAt(pool, at);
  const selections = spec.drawnSelections ?? (pinnedAnswerOf(spec) ? 0 : 1);
  // A probe that sent nothing takes the draws of each selection its attempts drew, past the one its answer reads.
  for (let k = 1; k < selections; k += 1) members.forEach(() => inFlightRng.int(0, 5));
  return members.map(({ account: a, member: m }) => {
    const overage = inOverageAt(a.id, at);
    const windows: RecordCandidate['windows'] = {};
    const entry = a.kind === 'oauth' ? slowReadAt(a.id, pool, at) : null;
    // Where an entry stands, the sessions burn the 5-hour window at the demand it read times the speed its holds
    // realize, so the window's demand reads the entry's own.
    const burn: Record<string, number> = a.kind === 'oauth' ? { ...burnsOf(spec, a.id), ...(spec.story && entry ? { five_hour: round5(entry.demand * entry.speed) } : {}) } : {};
    const end = endOf(a, at);
    if (a.kind === 'oauth') {
      const head = r.attempts.some((x) => x.accountId === a.id) || a.id === 'primary';
      const ageMs = head ? 4_000 : 120_000;
      for (const w of readingsOf(spec, a.id)) {
        const name = WINDOW_KEY[w.id];
        if (name === undefined || w.utilization === null) continue;
        const ahead = leadOf({ resetsAt: w.resetsAt, utilization: w.utilization }, hoursOfKey(name) * HOUR, at, end);
        const horizon = horizonOf(name, w.resetsAt, end, at).at;
        windows[name] = {
          ageMs, ahead: name === 'seven_day_fable' ? null : round3(ahead),
          // The 5-hour window's burn over the speed the account's holds realize, 1 with no entry in force.
          demand: name === 'five_hour' ? round5((burn[name] ?? 0) / (entry?.speed ?? 1)) : null,
          horizon: iso(horizon), observedAt: iso(at - ageMs),
          projection: round3(w.utilization + ((burn[name] ?? 0) * (horizon - at)) / HOUR), reserved: 0, reset: w.resetsAt, source: head ? 'head' : 'poll', utilization: w.utilization,
        };
      }
      // The engine writes a usage-based seat's spend window while a share is held, its source the reset's.
      const spend = a.usageBased ? a.spend : null;
      if (spend !== null && spend.utilization !== null && !SPEND_UNREAD.has(r.id)) {
        windows.spend = { ageMs, ahead: null, demand: null, horizon: null, observedAt: iso(at - ageMs), projection: null, reserved: 0, reset: spend.resetsAt, source: spend.resetSource, utilization: spend.utilization };
      }
    } else {
      windows.worst_bucket = { ageMs: 4_000, ahead: null, demand: null, horizon: null, observedAt: iso(at - 4_000), projection: null, reserved: 0, reset: iso(at + 40_000), source: 'head', utilization: lookup(WORST_BUCKET, a.id) ?? 0.1 };
    }
    const cooldowns: RecordCooldown[] = COOLDOWNS.filter((c) => c.accountId === a.id && c.setAt <= at && at < c.until)
      .map((c) => ({ blocking: holds(c, r.canonicalModel, fast), reason: c.reason, scope: c.scope, until: iso(c.until), window: c.window }))
      .sort((x, y) => x.scope.localeCompare(y.scope));
    const budget = budgetsAt(pool, a.id, at);
    const sub = a.subscription;
    const endsAt = sub?.endsAt ? Date.parse(sub.endsAt) : null;
    // Drawn for every member, so a count the story sets moves no later draw. A drawn count stands under the cap, so a
    // member reads full only where the story's own pins or the marks of `fillDrawnRivals` hold it there. A row outside
    // the seeded streams draws none, and neither does a probe's pinned answer after its leg, which reads the counts its
    // leg's decision drew (`buildDecisions`), so the decisions after it keep their draws.
    const drawn = UNSEEDED_IDS.has(r.id) || selections === 0 ? 0 : inFlightRng.int(0, 5);
    const drawnInFlight = Math.min(drawn, pool.inFlightCap - 1);
    const obstacle = obstacleOf(a, pool, at, r.canonicalModel, fast, spec.attempted);
    const paid = isPaidSource(a);
    return {
      accountId: a.id,
      // A paid source, an API key or a usage-based seat, knows its allowance, as the engine's paid-source arm answers,
      // since its reading is the spend or the worst bucket, never a plan window.
      allowanceKnown: isPaidSource(a) || windows.five_hour !== undefined,
      binding: null,
      budget,
      burn,
      cooldowns,
      credits: fast ? creditsAt(a.id, at) : null,
      enabled: enabledAt(a, at),
      factor: entry?.factor ?? null,
      fit: null,
      inFlight: spec.inFlight?.[a.id] ?? drawnInFlight,
      inFlightCap: pool.inFlightCap,
      inOverage: overage,
      incarnation: 1,
      kind: a.kind,
      obstacle,
      overage: overage ? { observedAt: iso(at - 40_000), reset: a.overage?.reset ?? null, since: iso(OVERAGE_FROM), source: 'head' } : null,
      // The first tier is known once the partitions have run, so the pick gives its members their reservation.
      pace: pool.strategy === 'pace' ? paceFactsOf(a, at, r.canonicalModel, { burn, windows }, false) : null,
      partition: null,
      planTier: sub?.plan ?? null,
      prefix: { ...NO_PREFIX, ...spec.prefix?.[a.id] },
      priority: m.priority,
      rank: null,
      runwayMs: null,
      score: null,
      state: stateAt(a, at),
      term: a.kind === 'api_key' || !sub
        ? { canceled: false, ended: false, endsAt: null, since: null, trialEndsAt: null, windingDown: false }
        : { canceled: a.canceled === true, ended: false, endsAt: sub.endsAt, since: sub.since, trialEndsAt: sub.trialEndsAt, windingDown: endsAt !== null && endsAt - at <= pool.windDownMinutes * MINUTE },
      tier: rankTier(pool.preferSubscription, paid, overage, m.priority),
      verdict: obstacle ? 'out' : 'eligible',
      windows,
    };
  });
}

/**
 * The story's rounds, which a later decision on the account reads as within the dwell, one per story spec that runs a
 * round. `buildDecisions` sets them before its first decision.
 */
let ROUNDS: { accountId: string; at: number }[] = [];

/**
 * What the gate read of one candidate for a pin: the verdict, the runways to the ceiling and to 1.0 that the ranks
 * read, each over every bounded window, one past its reset included, the ladder's third rung ranking a `least_bad`
 * member by the one to 1.0, the pair of each that a row states, and the fit table's row.
 */
interface FitRead {
  facts: WalkFact[];
  runwayMs: number | null;
  /** The runway to the ceiling and its window as a row states them: `shownOf` over the windows' runways. */
  shown: Runway | undefined;
  verdict: string;
  wallRunwayMs: number | null;
  /** The runway to 1.0 and its window as a placement's `least_bad` row states them, read the same way. */
  wallShown: Runway | undefined;
}

const runwayOf = (c: Pick<RecordCandidate, 'runwayMs'>) => c.runwayMs ?? Number.POSITIVE_INFINITY;
/**
 * Whether a drawn choice's rival stands ahead of it, each by its place in the order and its runway: a better place, or
 * the same place with a longer runway where that place narrows to the longest.
 */
const outranks = (rival: { index: number; runwayMs: number | null }, chosen: { index: number; runwayMs: number | null }, narrows: boolean) =>
  rival.index < chosen.index || (rival.index === chosen.index && narrows && runwayOf(rival) > runwayOf(chosen));
/** The candidates with the longest runway, an unbounded one longest of all, ties kept in the set's order. */
const longestOf = <T>(list: T[], runway: (x: T) => number | null): T[] => {
  const key = (x: T) => runway(x) ?? Number.POSITIVE_INFINITY;
  return list.filter((c) => key(c) === Math.max(...list.map(key)));
};
/**
 * The strategy's ties the longest runway keeps, by the runway of those `read` says the selection read a fit for, a tie
 * with no read keeping its place, and none where the runway narrows nothing, so the pick line names `by_runway` only
 * where it broke the tie.
 */
function runwayTiesOf<T>(ties: T[], read: (c: T) => boolean, runway: (c: T) => number | null): T[] {
  const key = (c: T) => runway(c) ?? Number.POSITIVE_INFINITY;
  const reads = ties.filter(read);
  if (reads.length === 0) return [];
  const longest = Math.max(...reads.map(key));
  const kept = ties.filter((c) => !read(c) || key(c) === longest);
  return kept.length === ties.length ? [] : kept;
}

/**
 * The router's fit of a pin on a candidate, over the plan windows the model draws: each window's runway is the time
 * until the prediction with the pin's rewrite, rising at the account's burn (its demand on the 5-hour window) and the
 * pin's, reaches the window's own ceiling, zero where the rewrite alone does and unbounded with no burn. `fits` reaches
 * the horizon on every window, `rewrite_only` lands the rewrite but falls short on one, and `least_bad` is the rest, as
 * is a window a move cannot read. An account inside the pool's wind-down before its end reads `ending`, and an API-key
 * account fits while its worst bucket has room. The runway to 1.0 reads every window with a projection, whether the
 * rewrite lands there or not. Each runway the ranks read binds on the shortest window short of its horizon, else the
 * shortest of all, a window past its reset included, and a row and the fit table state the same pick over the windows
 * whose runway ends by their reset, none where each one passes it. An API key whose full bucket refuses the request at
 * this instant reads zero to 1.0 on no window, the pair its row states too.
 */
function fitOf(c: RecordCandidate, at: number, pool: Pick<PoolSeed, 'fiveHourCeiling' | 'weeklyCeiling' | 'windDownMinutes'>, model: string, tokens: number, pinBurn: Record<string, number>, move: boolean): FitRead {
  const read = (facts: WalkFact[], verdict: string, bounded: Runway[] = [], walls: Runway[] = []): FitRead => ({
    facts: [...facts, fact('verdict', verdict)].map((f) => ({ ...f, accountId: c.accountId })),
    runwayMs: bindingOf(bounded)?.runway ?? null,
    shown: shownOf(bounded),
    verdict,
    wallRunwayMs: bindingOf(walls)?.runway ?? null,
    wallShown: shownOf(walls),
  });
  const endsAt = c.term?.endsAt ? Date.parse(c.term.endsAt) : null;
  if (endsAt !== null && endsAt - at <= pool.windDownMinutes * MINUTE) return read([fact('end', endsAt, 'time')], 'ending');
  if (c.kind === 'api_key') return (c.windows.worst_bucket?.utilization ?? 0) < 1 ? read([], 'fits') : read([], 'least_bad', [], [{ pastReset: false, runway: 0, short: false, window: null }]);
  const rewrite = rewriteOf(tokens, model);
  const projections: WalkFact[] = [];
  const rewrites: WalkFact[] = [];
  const reserved: WalkFact[] = [];
  const bounded: Runway[] = [];
  const walls: Runway[] = [];
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
    // The 5-hour window reads the demand, the burn over the account's speed, so a slowed account's holds lend it no
    // room. It reads the burn itself where no slow-down stands.
    const burn = (w === 'five_hour' && x.demand !== null ? x.demand : (c.burn[w] ?? 0)) + (pinBurn[w] ?? 0);
    const window = windowFitOf(x, ceilingOn(pool, w), rewrite[w] ?? 0, burn);
    lands = lands && window.lands;
    const { ceiling, wall } = runwaysOf(window, w, x.horizon, x.reset, at);
    if (wall !== null) walls.push(wall);
    if (ceiling === null) continue;
    every = every && !ceiling.short;
    bounded.push(ceiling);
  }
  const shown = shownOf(bounded);
  const verdict = unread ? 'least_bad' : every ? 'fits' : lands ? 'rewrite_only' : 'least_bad';
  return read([...projections, ...(shown ? [fact('runway_ms', shown.runway, 'ms')] : []), ...rewrites, ...reserved, ...(shown ? [fact('binding', shown.window)] : [])], verdict, bounded, walls);
}

/** The newest round any pool ran on the account by `at`. */
const newestRound = (accountId: string, at: number): number | null =>
  ROUNDS.filter((x) => x.accountId === accountId && x.at <= at).reduce<number | null>((newest, x) => (newest === null || x.at > newest ? x.at : newest), null);

/** A pin as the shed line names it in its marks and its skips. */
const pinText = (p: { agentId: string; keyId: string; model: string; sessionId: string }) => `${p.keyId}/${p.sessionId}/${p.agentId}/${p.model}`;

/**
 * Whether a member holds allowance for a request of `model`. An API key reads no plan window, so it always holds it. A
 * usage-based seat answers by its spend alone, since the spend funds every model: it holds none where its row's spend
 * entry reads 1 or more, and holds it below that or with no entry, whatever a plan window on its row reads. Any other
 * member holds it where no plan window the model draws, its own week where it has one and both shared windows, reads 1
 * or more.
 */
const holdsAllowance = (c: RecordCandidate, model: string) => {
  if (c.kind === 'api_key') return true;
  if (accountOf(c.accountId).usageBased) return (c.windows.spend?.utilization ?? 0) < 1;
  return drawnWindows(model).every((w) => (c.windows[w]?.utilization ?? 0) < 1);
};

/** The ladder's rungs from the best: the words of a landing's `ladder` fact. */
const LADDER = ['fits', 'rewrite_only', 'least_bad', 'ending', 'every'];
/** Whether a rung narrows its members to the longest runway. */
const narrowsByRunway = (rung: string) => rung === 'rewrite_only' || rung === 'least_bad';
/** The runway a read ranks by: the one to 1.0 under `least_bad`, the one to the ceiling otherwise. */
const rankedRunwayOf = (read: FitRead): number | null => (read.verdict === 'least_bad' ? read.wallRunwayMs : read.runwayMs);

/**
 * The rung a member's verdict stands on, a member read no fit for among the fitting ones. A `least_bad` or winding-down
 * member with no allowance left stands on the last rung alone.
 */
const rungOf = (verdict: string | null, holds: boolean): string => (verdict === null ? 'fits' : verdict === 'fits' || verdict === 'rewrite_only' || holds ? verdict : 'every');

/** A walk of the five rules: the lines, the rule that fired, its destinations after the gate and before it. */
interface Move {
  destinations: RecordCandidate[];
  fired: string | null;
  /** Whether the spent rule held the request in place for the reset. */
  held: boolean;
  lines: WalkStep[];
  reached: RecordCandidate[];
  /**
   * The gate's last read of each candidate it read. A row states the runway over the windows whose runway ends by
   * their reset, so a move's tie reads this read of each destination for the runway it ranks by.
   */
  reads: Map<RecordCandidate, FitRead>;
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
  const holds = (c: RecordCandidate) => holdsAllowance(c, r.canonicalModel);
  const allowanceKnown = (c: RecordCandidate) => isPaidSource(accountOf(c.accountId)) || windows.every((w) => c.windows[w] !== undefined);
  const siblings = candidates.filter((c) => c.obstacle === null && c.accountId !== source?.accountId);
  const lines: WalkStep[] = [];
  const gateReads = new Map<RecordCandidate, FitRead>();
  const no = (i: number, facts: WalkFact[]) => lines.push(step(`move:${RULES[i]}`, 'no', facts));
  // The moving pin's last prompt and burn, which the gate prices on every candidate.
  const tokens = spec.pinFigures?.tokens ?? bodyTokensOf(r);
  const pinBurn = spec.pinFigures?.burn ?? priorOf(spec);
  const gate = (reached: RecordCandidate[], fitsOnly: boolean): { answered: RecordCandidate[]; step: WalkStep } => {
    const reads = reached.map((c) => {
      const read = fitOf(c, at, pool, r.canonicalModel, tokens, pinBurn, true);
      c.fit = read.verdict;
      c.runwayMs = read.shown?.runway ?? null;
      c.binding = read.shown?.window ?? null;
      gateReads.set(c, read);
      return read;
    });
    // A drawn move lands where its draw put it, so a rival the gate would answer ahead of it reads full, as the cap
    // passes it over: a better order, or a longer runway where the order narrows to the longest. A story move lands
    // where its histories send it.
    const chosen = spec.kind === 'moved' && !spec.story ? reached.find((c) => c.accountId === spec.accountId) : undefined;
    if (chosen) {
      const order = (c: RecordCandidate) => (c.fit === 'fits' ? 0 : c.fit === 'rewrite_only' ? 1 : 2);
      const mine = { index: order(chosen), runwayMs: chosen.runwayMs };
      for (const c of reached) if (c !== chosen && outranks({ index: order(c), runwayMs: c.runwayMs }, mine, mine.index === 1)) c.inFlight = c.inFlightCap;
    }
    const open = (c: RecordCandidate) => c.inFlight < c.inFlightCap;
    // Each order keeps the set's order for the strategy to pick from. The fitting set goes on whole, the shed rule's
    // too, since the strategy ranks it and the runway only breaks its tie. The rewrite-only order narrows to the
    // longest runway, ties kept.
    const fits = reached.filter((c) => c.fit === 'fits' && open(c));
    const rewriteOnly = longestOf(reached.filter((c) => c.fit === 'rewrite_only' && open(c)), (c) => gateReads.get(c)!.runwayMs);
    const left = reached.filter((c) => c.fit !== 'ending');
    const [verdict, answered] = fits.length ? ['fits', fits] : fitsOnly ? ['none', []] : rewriteOnly.length ? ['rewrite_only', rewriteOnly] : left.length ? ['least_bad', left] : ['none', []];
    return { answered, step: step('fit', verdict, reads.flatMap((x) => x.facts)) };
  };
  // The rule fires where the gate answers a sibling it keeps holding allowance. Otherwise its line says it found
  // none, with the members the end exclusion dropped.
  const tryRule = (i: number, facts: WalkFact[], keep: (c: RecordCandidate) => boolean, fitsOnly = false): Move | null => {
    const reached = siblings.filter((c) => keep(c) && holds(c));
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
    return { destinations: answered, fired: RULES[i], held: false, lines, reached, reads: gateReads };
  };
  const none: Move = { destinations: [], fired: null, held: false, lines, reached: [], reads: gateReads };

  if (fast) no(0, [fact('fast', true)]);
  else if (!pool.migrateWhenSpent) no(0, [fact('migrate_when_spent', false)]);
  else if (!source || holds(source)) no(0, [fact('holds_allowance', true)]);
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
  // The one trigger: the first plan window, in the windows' order, whose projection at its horizon reads over the line
  // that horizon's bound sets, or on a pool that slows its pins the 5-hour window's stretch past the threshold once the
  // safety wait has passed, a 5-hour window under the hold yielding to the next. A round the story writes fires where
  // its window has read over its ceiling for the projection's wait, counted from the first reading `OVER_CEILING_SINCE`
  // dates, and the build fails on one inside it. A decision inside a round's dwell reads the same trigger as the round,
  // the first window over its line. Any other window over its line is inside its wait, which holds the round back and
  // yields to the next window. The line names every projection it read with its horizon, its wall and its wait, the
  // 5-hour window's age and hold inside the hold, and how long the stretch has read over while the safety wait holds.
  const newest = newestRound(source.accountId, at);
  const fires = spec.round !== undefined || (newest !== null && at - newest < pool.shedDwellSeconds * 1_000);
  const stretch = pool.slowDown ? stretchTriggerOf(source, pool, at) : null;
  // The trigger reads the account's pins, so a request of a model with no week reads a week its account's pins draw.
  // The allowance reads above keep the request's own windows.
  const tested = triggerWindowsOf(source.accountId, pool, at);
  const { fired: trigger, waits } = stretch === null ? triggerOf(source, pool.slowDown ? tested.filter((w) => w !== 'five_hour') : tested, at, pool, fires) : { fired: stretch, waits: [] };
  // The stretch's safety wait is read apart, so a stretch past it fires a round the story has to write.
  if (trigger !== null && !fires) throw new Error(`the story writes no round for ${source.accountId}'s ${trigger.window} window at ${iso(at)}, outside any dwell`);
  // The Fable reservation's test runs once no window fired and reads off the 7-day projection, as the router's does.
  // Its wait runs from the first reading of the room under the reservation, which `OVER_CEILING_SINCE` dates under
  // `FABLE_RESERVATION`: outside a dwell a short room inside the wait holds the round back and names its stamp and its
  // wait, and one past it would fire a round the story has to write.
  const short = reservationShort(source, pool, at);
  const shortRead = trigger === null && short !== null && (source.windows.seven_day?.projection ?? null) !== null;
  const reservationWait = shortRead && spec.round === undefined && !fires ? reservationWaitOf(source, at, pool) : null;
  const reservationWaits = reservationWait !== null && at - reservationWait.overSince < reservationWait.waitMs;
  // A round on the reservation walks the source's pins of another model alone, so over Fable pins alone its test fires
  // nothing, and the line says why behind the reservation's figure. The stamp still reads the room short.
  const noOtherModel = shortRead && !reservationWaits && fablePinsAlone(source.accountId, pool, at);
  const reserved = shortRead && !reservationWaits && !noOtherModel;
  if (reserved && !fires) throw new Error(`the story writes no round for ${source.accountId}'s Fable reservation at ${iso(at)}, outside any dwell`);
  // A round the story writes is one the readings fire, as the router runs none on a selection whose tests fire nothing.
  if (spec.round !== undefined && trigger === null && !reserved) throw new Error(`the story writes a round for ${source.accountId} at ${iso(at)} that no window's test and no Fable reservation fires`);
  // The router stamps a test at the first selection that reads it over, a window past its own ceiling or the room
  // under the reservation, and keeps the stamp while the account holds a pin of the pool at each sweep.
  for (const [test, date] of Object.entries(OVER_CEILING_SINCE[source.accountId] ?? {})) {
    const reading = source.windows[test];
    const over = test === FABLE_RESERVATION ? shortRead : tested.includes(test) && (reading?.projection ?? 0) > ceilingOn(pool, test);
    if (date === at && over) readOver.add(`${source.accountId}|${test}|${date}`);
  }
  // The stretch's round waits the safety wait, read above, so every other story round is held to its test's wait and
  // to the stamp the wait runs from.
  const roundTest = spec.round === undefined ? null : trigger !== null ? (trigger.slowed ? null : trigger.window) : reserved ? FABLE_RESERVATION : null;
  if (roundTest !== null) {
    const words = roundTest === FABLE_RESERVATION ? 'Fable reservation' : `${roundTest} window`;
    const dated = firstReadingOf(source.accountId, roundTest);
    const { overSince, waitMs } = roundTest === FABLE_RESERVATION ? reservationWaitOf(source, at, pool) : { overSince: dated ?? at, waitMs: projectionWaitOf(source, roundTest, dated ?? at, at, pool) };
    if (at - overSince < waitMs) throw new Error(roundInsideWait(source.accountId, words, at, overSince, waitMs));
    if (dated !== undefined) stampHolds(source.accountId, roundTest, words, dated, at, pool);
  }
  // The reservation's test names no rate, so its round writes no excess.
  const fired: { by: string; excess: number | null; window: string } | null =
    trigger !== null ? { by: trigger.slowed ? 'slow_down' : 'projection', excess: trigger.excess, window: trigger.window } : reserved ? { by: 'fable_reservation', excess: null, window: 'seven_day' } : null;
  const facts: WalkFact[] = [];
  for (const w of tested) {
    const x = source.windows[w];
    if (x?.projection === null || x?.projection === undefined || x.horizon === null) continue;
    facts.push(fact(`projection_${w}`, x.projection, 'percent'), fact(`horizon_${w}`, Date.parse(x.horizon), 'time'));
    // The instant the projection's burn takes the window to its wall, in whole milliseconds as the engine stores it,
    // and none where the window burns nothing.
    const burn = source.burn[w] ?? 0;
    if (burn > 0) facts.push(fact(`wall_${w}`, wallOf(x, burn, at), 'time'));
    const wait = waits.find((entry) => entry.window === w);
    if (wait) facts.push(fact(`over_since_${w}`, wait.overSince, 'time'), fact(`wait_${w}`, wait.waitMs, 'ms'));
  }
  const held = fiveHourHeld(source, pool, at);
  if (held) facts.push(fact('age_five_hour', held.ageMs, 'ms'), fact('hold_five_hour', held.holdMs, 'ms'));
  // The router names the reservation wherever the room reads short, whether or not a window fired, and the stamp and
  // the wait beside it while the room reads short inside the wait.
  if (short !== null) facts.push(fact('fable_reservation', short, 'percent'));
  if (noOtherModel) facts.push(fact('fable_reservation', 'no_other_model'));
  if (reservationWaits && reservationWait !== null) facts.push(fact('over_since_fable_reservation', reservationWait.overSince, 'time'), fact('wait_fable_reservation', reservationWait.waitMs, 'ms'));
  // The line tests windows of both kinds, so the router writes both of the pool's ceilings here, whatever line the
  // trigger reads.
  facts.push(fact('five_hour_ceiling', pool.fiveHourCeiling, 'percent'), fact('weekly_ceiling', pool.weeklyCeiling, 'percent'));
  const overSince = slowReadAt(source.accountId, pool, at)?.overSince ?? null;
  const stretchWaits = overSince !== null && at - overSince < SLOW_DOWN_WAIT_MS;
  if (stretchWaits) facts.push(fact('over_ms', at - overSince, 'ms'));
  // The hold holds back the round alone, as a wait does, so a held line goes on to the cooldown and the mark.
  if (fired === null && waits.length === 0 && !stretchWaits && held === null && !reservationWaits) {
    no(4, facts);
    return none;
  }
  if (fired !== null) {
    facts.push(fact('trigger', fired.window), fact('fired_by', fired.by), ...(fired.excess === null ? [] : [fact('excess', fired.excess, 'percent_per_hour')]));
    if (spec.round) {
      const round = roundOf(spec.round, fired.excess, pool);
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
 * average still carries the previous window's burst over the reset, so no 5-hour round fires while the window is
 * younger than twice it.
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
 * The 5-hour round under the slow-down: the stretch has read over the threshold for the safety wait, the window's
 * horizon (its reset cut at the account's end) moved back by the threshold lies ahead, and the window is past the hold.
 * The excess is the demand's past the 5-hour ceiling there, per hour to it.
 */
function stretchTriggerOf(source: RecordCandidate, pool: PoolSeed, at: number): { excess: number; slowed: true; window: string } | null {
  const overSince = slowReadAt(source.accountId, pool, at)?.overSince ?? null;
  const x = source.windows.five_hour;
  if (overSince === null || at - overSince < SLOW_DOWN_WAIT_MS || x?.horizon === null || x?.horizon === undefined || x.demand === null || fiveHourHeld(source, pool, at) !== null) return null;
  const threshold = pool.slowDownStretchSeconds * 1_000;
  const projected = stretchProjectionOf({ at, demand: x.demand, horizon: Date.parse(x.horizon), utilization: x.utilization }, threshold);
  if (projected === null) return null;
  const hours = (Date.parse(x.horizon) - threshold - at) / HOUR;
  const excess = round5((projected - pool.fiveHourCeiling) / hours);
  return excess > 0 ? { excess, slowed: true, window: 'five_hour' } : null;
}

/**
 * The Fable reservation's test on the source, on a Pace pool that sheds on it with load shedding on: the reservation
 * `paceOf` holds back for the account's Fable week off the record's readings and burns, where the 7-day room it reads,
 * floored at 0, stands strictly under it. Null where the room holds the whole reservation, where none applies and on
 * any other pool.
 */
function reservationShort(source: RecordCandidate, pool: PoolSeed, at: number): number | null {
  if (!reservationRoundsRun(pool)) return null;
  const pace = paceOf(accountOf(source.accountId), at, null, { burns: source.burn, readings: source.windows, reserve: true });
  return pace.fableReservation !== null && pace.remaining < pace.fableReservation ? pace.fableReservation : null;
}

/**
 * Whether each pin the routing state keeps on the account under the pool's keys at `at` draws a week of its own, the
 * Fable use a round on the reservation leaves in place. The router also leaves a pin out of that round for its lane,
 * its agent's end and its session's account state, and drops a pin that a release took. Of those, this state can read
 * the account state alone, through `accountBoundAt`, and keeps neither the lane, the end nor the release.
 */
function fablePinsAlone(accountId: string, pool: PoolSeed, at: number): boolean {
  return heldModelsOf(accountId, pool, at).every((m) => modelWeeks(TABLES, m).length > 0);
}

/**
 * The windows a round on the account reads its trigger over at `at`, in the trigger's order: both shared windows, then
 * the week of each model that a pin of the pool on the account draws, whatever the request's model. The router also
 * leaves out a pin whose agent ended and drops a pin that a release took, and this state records neither the end nor
 * the release.
 */
function triggerWindowsOf(accountId: string, pool: PoolSeed, at: number): string[] {
  const drawn = new Set(heldModelsOf(accountId, pool, at).flatMap((m) => modelWeeks(TABLES, m)));
  return [...SHARED_WINDOWS, ...Object.keys(TABLES.modelWindows).filter((w) => drawn.has(w))];
}

/** The instant a window's projected burn takes it to its wall, in whole milliseconds as the engine stores it. */
const wallOf = (x: Pick<RecordCandidate['windows'][string], 'utilization'>, burn: number, at: number): number => Math.round(at + ((1 - x.utilization) / burn) * HOUR);

/** The key `OVER_CEILING_SINCE` dates the Fable reservation's stamp under, beside the plan windows' names. */
const FABLE_RESERVATION = 'fable_reservation';

/** The instant of the selection that set a stamp, read off its request row. */
const selectionAt = (requestId: string): number => storyAt(requestId, `${requestId}, the selection that first read a test over`);

/**
 * The stamp of each test the story waits on, by account and by window, as the router's `note_projection` and
 * `note_reservation` set it: the instant of the first selection on the account that read the window's projection over
 * its own ceiling, or under `FABLE_RESERVATION` the 7-day room under the Fable reservation. Each is a renewal the day
 * holds, read off its row, and secondary's week takes the reading the story dates. The router keeps a stamp only while
 * the account holds a pin of the pool at every purge sweep, so the rows from each stamp to its round keep one there. A
 * round on a window with no date here waits from its own instant, as the router's does on a missing stamp.
 */
const OVER_CEILING_SINCE: Readonly<Record<string, Readonly<Record<string, number>>>> = {
  nightly: { seven_day_fable: selectionAt('req_4b9e2c71-a35') },
  primary: { [FABLE_RESERVATION]: selectionAt('req_8e1d5a36-c92'), five_hour: selectionAt('req_c3a71e58-b46') },
  secondary: { five_hour: selectionAt('req_5f2c8b19-d73'), seven_day: SECONDARY_WEEK_OVER_AT },
};

/** The stamp the story dates for a test on an account, and none where it dates no first reading. */
const firstReadingOf = (accountId: string, window: string): number | undefined => lookup(OVER_CEILING_SINCE[accountId] ?? {}, window);

/**
 * A test's wait: a quarter of the time from the stamp to `end`, at most twice the pool's burn average, and the cap
 * alone with no `end`.
 */
const quarterWaitMs = (end: number | null, overSince: number, pool: PoolSeed): number => {
  const cap = 2 * pool.burnAverageSeconds * 1_000;
  return end === null ? cap : Math.min(Math.floor((end - overSince) / 4), cap);
};

/**
 * The wait of a window over its line, as the router's `trigger_of` reads it: a quarter of the time from the stamp to
 * the wall instant its burn reaches now, at most twice the pool's burn average, and the cap alone where its burn reads
 * nothing.
 */
function projectionWaitOf(source: RecordCandidate, window: string, overSince: number, at: number, pool: PoolSeed): number {
  const reading = source.windows[window];
  const burn = source.burn[window] ?? 0;
  return quarterWaitMs(burn > 0 && reading !== undefined ? wallOf(reading, burn, at) : null, overSince, pool);
}

/** What the build says of a round the story writes before its test has read over for the wait. */
const roundInsideWait = (accountId: string, test: string, at: number, overSince: number, waitMs: number): string =>
  `the story writes the round on ${accountId}'s ${test} at ${iso(at)}, inside the wait of ${waitMs} ms that runs from its first reading over at ${iso(overSince)}`;

/**
 * The stamp and the wait of a window over its line that fired no round. A window the story dates no first reading for,
 * or one past its wait, would fire a round the story does not write, so the build fails on it. A 5-hour window under
 * the hold reads its wait first, as the router does: inside the wait the line names it beside the age, and past it the
 * hold alone keeps the round back, so the window names no wait and the build goes on.
 */
function waitOf(source: RecordCandidate, window: string, at: number, pool: PoolSeed, held: boolean): { overSince: number; waitMs: number; window: string } | null {
  const overSince = firstReadingOf(source.accountId, window);
  if (overSince === undefined) throw new Error(`the story dates no reading over the ceiling for ${source.accountId}'s ${window} window, over its line at ${iso(at)}`);
  const waitMs = projectionWaitOf(source, window, overSince, at, pool);
  if (at - overSince < waitMs) return { overSince, waitMs, window };
  if (held) return null;
  throw new Error(`${source.accountId}'s ${window} window is past its wait at ${iso(at)} with no round the story writes`);
}

/**
 * The stamp and the wait of the Fable reservation's test on a source whose room reads short, as the router's
 * `reservation_trigger` reads them: a quarter of the time from the stamp to the 7-day window's reset, at most twice the
 * pool's burn average, and the cap alone where the reading names no reset. A source the story dates no first short
 * reading for waits from `at`, as the router's does on a missing stamp.
 */
function reservationWaitOf(source: RecordCandidate, at: number, pool: PoolSeed): { overSince: number; waitMs: number } {
  const overSince = firstReadingOf(source.accountId, FABLE_RESERVATION) ?? at;
  const reset = source.windows.seven_day?.reset ?? null;
  return { overSince, waitMs: quarterWaitMs(reset === null ? null : Date.parse(reset), overSince, pool) };
}

/**
 * The window a round on `source` fires on where `fires` holds, and the excess per hour that lands its projection on the
 * line its horizon read: the wall at a reset or an end horizon, the window's own ceiling at the five-hour bound. Where
 * it does not hold, each window over its line is inside its wait and yields to the next. A 5-hour window under the hold
 * yields to the next window in the order, behind its wait where it reads over its line.
 */
function triggerOf(source: RecordCandidate, windows: string[], at: number, pool: PoolSeed, fires: boolean): { fired: { excess: number; slowed?: true; window: string } | null; waits: NonNullable<ReturnType<typeof waitOf>>[] } {
  const held = fiveHourHeld(source, pool, at) !== null;
  const waits: NonNullable<ReturnType<typeof waitOf>>[] = [];
  for (const w of windows) {
    const x = source.windows[w];
    if (x?.projection === null || x?.projection === undefined || x.horizon === null) continue;
    const line = lineOf(source, w, pool, at);
    if (x.projection <= line) continue;
    const young = w === 'five_hour' && held;
    if (!fires) {
      const wait = waitOf(source, w, at, pool, young);
      if (wait !== null) waits.push(wait);
      continue;
    }
    if (young) continue;
    const hours = Math.max((Date.parse(x.horizon) - at) / HOUR, 1 / 60);
    return { fired: { excess: round5((x.projection - line) / hours), window: w }, waits };
  }
  return { fired: null, waits };
}

/** The skip words of a pin that leaves the round before the walk. */
const LEFT_OUT_SKIPS = ['account_bound', 'ended', 'fable_model', 'fast_lane', 'no_burn', 'off_window'];
/** Whether the round's walk reads the pin: one with no skip word, or a skip word the walk sets itself. */
const walkedOf = (p: RoundPin) => p.skip === null || !LEFT_OUT_SKIPS.includes(p.skip);

/**
 * A round's figures off its pins: the burn it could remove, its target under the cap, what its marks removed, the
 * pins it skipped as the shed line names them, and its outcome with the reason a round that marked nothing records.
 * The reason is the deepest class a walked pin's skip names, where no pin was walked a left-out pin's own word, and
 * `unpriced` where neither names one, as the router ranks them. A round with no excess, as the Fable reservation's,
 * targets the cap's share of its burn alone.
 */
function roundOf(round: ShedRound, excess: number | null, pool: PoolSeed) {
  const burnSum = round5(round.pins.reduce((n, p) => n + (p.burn ?? 0), 0));
  const walked = round.pins.filter(walkedOf);
  const skips = (walked.length > 0 ? walked : round.pins).flatMap((p) => (p.skip === null ? [] : [p.skip]));
  const reason = round.marks.length > 0 ? null : (SHED_CAUSES.find((w) => skips.includes(w)) ?? 'unpriced');
  return {
    burnSum,
    outcome: round.marks.length > 0 ? `marked ${round.marks.length}` : 'nothing_to_mark',
    reason,
    removed: round5(round.pins.filter((p) => p.marked).reduce((n, p) => n + (p.burn ?? 0), 0)),
    skipped: round.pins.flatMap((p) => (p.skip === null ? [] : [`${pinText(p)}: ${p.skip}`])),
    target: round5(excess === null ? pool.shedCap * burnSum : Math.min(excess, pool.shedCap * burnSum)),
  };
}

const tierKey = (t: [number, number, number]) => t.join('/');
/** The unit a member's score reads in: a paid source's remaining share, any other's share per hour. */
const scoreUnitOf = (c: RecordCandidate) => (isPaidSource(accountOf(c.accountId)) ? 'percent' : 'percent_per_hour');

/**
 * The pick among the members the partitions kept in the chosen account's tier, the set the engine's `choose` orders:
 * the chosen account first, the rest by score. Under Pace each of them ranks by the figures `reserve` gives it, which
 * hold the Fable reservation, since that tier is the first one and is known here alone.
 */
function pickOf(candidates: RecordCandidate[], kept: RecordCandidate[], chosen: string | null, strategy: string, source: string | undefined, reserve: (c: RecordCandidate) => RecordCandidate['pace']): { runnerUp: Score | null; score: Score | null; tierSize: number } {
  const winner = candidates.find((c) => c.accountId === chosen);
  if (!winner) return { runnerUp: null, score: null, tierSize: 0 };
  const scoreOf = (c: RecordCandidate) => c.pace?.score ?? 0;
  const rest = kept.filter((c) => c !== winner && c.accountId !== source && tierKey(c.tier) === tierKey(winner.tier));
  if (strategy === 'pace') for (const c of [winner, ...rest]) c.pace = reserve(c);
  rest.sort((x, y) => scoreOf(y) - scoreOf(x));
  winner.rank = 1;
  winner.score = strategy === 'pace' ? scoreOf(winner) : null;
  rest.forEach((c, i) => {
    c.rank = i + 2;
    c.score = strategy === 'pace' ? scoreOf(c) : null;
  });
  const tierSize = rest.length + 1;
  if (strategy !== 'pace') return { runnerUp: null, score: null, tierSize };
  const best = rest[0];
  return { runnerUp: best ? { accountId: best.accountId, score: scoreOf(best), unit: scoreUnitOf(best) } : null, score: { accountId: winner.accountId, score: scoreOf(winner), unit: scoreUnitOf(winner) }, tierSize };
}

/** Whether one tier ranks before another, compared member by member as the engine orders them. */
function tierBefore(x: [number, number, number], y: [number, number, number]): boolean {
  const i = x.findIndex((n, j) => n !== y[j]);
  return i >= 0 && x[i] < y[i];
}

/**
 * A drawn placement or move takes the account its request drew, so its rivals read full where the engine would have
 * picked them: on a placement, every member the ladder would take ahead of it, one of a better rung or one of the same
 * rung with a longer runway where that rung narrows to the longest, so the cap passes them over before the no-fit
 * partition, and where the strategy picks, every member of a better tier and under Pace every tier mate scoring above
 * the chosen member. A rung places among the members the partitions keep, so the ladder's marks stand on a placement a
 * rung made too. A token count takes no slot, so the request generator places it on the engine's pick instead. The
 * drawn picks read so until the request generator draws them in the engine's order.
 */
function fillDrawnRivals(spec: Spec, candidates: RecordCandidate[], pool: PoolSeed): void {
  const winner = candidates.find((c) => c.accountId === spec.accountId);
  if (!winner || spec.request.kind === 'count_tokens') return;
  const top = pool.strategy === 'pace' ? winner.pace?.score : undefined;
  const outscores = (c: RecordCandidate) => top !== undefined && c.pace !== null && c.pace.score > top;
  // A move's gate reads its own fits later in the walk, so only a placement's choice is held to the ladder here. Each
  // member's step is its rung and the runway its read ranks by, which a rung compares only where it narrows.
  const stepOf = (c: RecordCandidate) => {
    const read = freshFitOf(spec, c, pool);
    const rung = rungOf(read.verdict, holdsAllowance(c, spec.request.canonicalModel));
    return { index: LADDER.indexOf(rung), runwayMs: rankedRunwayOf(read) };
  };
  const placed = spec.kind === 'placed' ? stepOf(winner) : null;
  const ahead = (c: RecordCandidate) => placed !== null && outranks(stepOf(c), placed, narrowsByRunway(LADDER[placed.index]));
  for (const c of candidates) {
    if (c === winner || c.obstacle !== null || c.accountId === (spec.from ?? spec.replaced?.accountId)) continue;
    const picked = !spec.rung && (tierBefore(c.tier, winner.tier) || (tierKey(c.tier) === tierKey(winner.tier) && outscores(c)));
    if (picked || ahead(c)) c.inFlight = c.inFlightCap;
  }
}

/** The burn a fresh session brings: the prior on the story's hand-written decisions, none on a drawn one. */
const priorOf = (spec: Spec) => (spec.story ? PRIOR_BURN : {});

// The fit's inputs do not move between the rivals' marks and the placement's read, so one fit serves both.
const FRESH_FITS = new WeakMap<RecordCandidate, FitRead>();
/** A fresh session's fit on a member, the body's rewrite with the prior's burn. */
const freshFitOf = (spec: Spec, c: RecordCandidate, pool: PoolSeed): FitRead => {
  const known = FRESH_FITS.get(c);
  if (known) return known;
  const read = fitOf(c, spec.at, pool, spec.request.canonicalModel, bodyTokensOf(spec.request), priorOf(spec), false);
  FRESH_FITS.set(c, read);
  return read;
};

/**
 * The no-fit partition's reading of a placement's members, drawn or written by hand, as the engine reads it: every
 * member the spent and cap partitions left, or the whole set the cap holds for a slot, and none of the members they
 * dropped. A `least_bad` or winding-down member with no allowance left reads `spent` with no runway and no window. A
 * drawn token count and a drawn hold read none: the request generator draws their account without the fit, and
 * neither takes a slot the cap could keep that choice by.
 */
function readPlacementFits(spec: Spec, candidates: RecordCandidate[], pool: PoolSeed): void {
  const r = spec.request;
  if (!spec.story && (r.kind === 'count_tokens' || spec.kind === 'held')) return;
  const left = candidates.filter((c) => c.obstacle === null && keepsSpent(c, pool, r));
  const free = left.filter((c) => r.kind === 'count_tokens' || c.inFlight < c.inFlightCap);
  for (const c of free.length > 0 ? free : left) {
    const read = freshFitOf(spec, c, pool);
    // The ladder refuses a `least_bad` or winding-down member with no allowance its rung, so its row reads `spent` with
    // no runway toward the rung it was refused. A `least_bad` row states the runway to 1.0 and each other row the one
    // to the ceiling, each over the windows whose runway ends by their reset, where the rung ranks by every bounded
    // window.
    const spent = (read.verdict === 'least_bad' || read.verdict === 'ending') && !holdsAllowance(c, r.canonicalModel);
    const wall = read.verdict === 'least_bad';
    const row = spent ? undefined : wall ? read.wallShown : read.shown;
    c.fit = spent ? 'spent' : read.verdict;
    c.runwayMs = row?.runway ?? null;
    c.binding = row?.window ?? null;
  }
}

/** A Fable request draws on its own week and both shared windows, so a reading at or past 1 on any of them is spent. */
const spentForFable = (model: string, utilization: (window: string) => number | null | undefined) => drawnWindows(model).some((w) => (utilization(w) ?? 0) >= 1);

/** One reading of whether extra usage funds a spent member, so the partition and a run-out's servable set agree. */
const fundedBy = (pool: PoolSeed, credits: RecordCandidate['credits']) => pool.overagePolicy === 'allow' && credits !== null && credits.blocked === null;

/**
 * Whether the spent partition keeps a member: a request of a model with no week of its own never drops one, and a
 * Fable request keeps a spent member only where an `allow` pool reads its extra usage unblocked. A member without a
 * credits reading is not funded.
 */
const keepsSpent = (c: RecordCandidate, pool: PoolSeed, r: RequestLog) => !r.canonicalModel.includes('fable') || holdsAllowance(c, r.canonicalModel) || fundedBy(pool, c.credits);

/**
 * The four partitions' facts in the engine's order, with the ladder's rung after the no-fit count on every landing that
 * reaches the fit with a member, the members left after them, which the step's verdict reads, and each member a
 * partition dropped with that partition's label: `spent`, `at_cap`, `no_fit` or `winding_down`. A dropped member
 * leaves the set, so the first partition that drops it is the one that names it.
 */
function narrowed(set: RecordCandidate[], pool: PoolSeed, r: RequestLog, moving: boolean): { dropped: Map<RecordCandidate, string>; facts: WalkFact[]; left: RecordCandidate[] } {
  const facts: WalkFact[] = [];
  const dropped = new Map<RecordCandidate, string>();
  let left = set;
  const drop = (key: string, kept: RecordCandidate[]) => {
    for (const c of left) if (!kept.includes(c)) dropped.set(c, key);
    facts.push(fact(key, left.length - kept.length));
    left = kept;
  };
  const narrow = (key: string, keep: (c: RecordCandidate) => boolean) => {
    const kept = left.filter(keep);
    if (kept.length > 0) drop(key, kept);
    else facts.push(left.length > 0 ? fact(key, 'fell_through') : fact(key, 0));
  };
  // A spent member is out rather than passed over, so this partition never falls through.
  if (r.canonicalModel.includes('fable')) drop('spent', left.filter((c) => keepsSpent(c, pool, r)));
  else facts.push(fact('spent', 'off'));
  // A token count takes no slot, so no cap holds it. With every member at its cap the request is held for a slot and
  // the set stays whole for the pick to name the account it waits on.
  const underCap = left.filter((c) => r.kind === 'count_tokens' || c.inFlight < c.inFlightCap);
  if (underCap.length === 0 && left.length > 0) facts.push(fact('at_cap', 'held'));
  else narrow('at_cap', (c) => underCap.includes(c));
  // A move's destinations are the set its gate ordered, read with the moving pin already, so they stay as it left
  // them. Every other landing walks the ladder: the fitting members, else the rewrite-only ones at the longest runway,
  // else the least-bad ones holding allowance at the longest runway to 1.0, else the ones inside their wind-down
  // holding allowance, else none, which falls through to the whole set. Each rung ranks by the runway over every
  // bounded window that the member's fit read, which a row leaves out where the window passes its reset. A walk over
  // no member takes no rung.
  if (moving) narrow('no_fit', () => true);
  else {
    const reached = left.length > 0;
    const rungs = new Map(left.map((c) => [c, rungOf(c.fit, holdsAllowance(c, r.canonicalModel))]));
    const rung = LADDER.find((x) => [...rungs.values()].includes(x)) ?? 'every';
    const onRung = left.filter((c) => rungs.get(c) === rung);
    const kept = rung === 'every' ? [] : narrowsByRunway(rung) ? longestOf(onRung, (c) => rankedRunwayOf(FRESH_FITS.get(c)!)) : onRung;
    narrow('no_fit', (c) => kept.includes(c));
    if (reached) facts.push(fact('ladder', rung));
  }
  narrow('winding_down', (c) => !c.term?.windingDown);
  return { dropped, facts, left };
}

/** A pin as the routing state holds it while the builder walks the day in order. */
interface PinState {
  accountId: string;
  createdAt: number;
  /** The cycle the pin's pace holds, which the slow-down's holds are sized from. */
  cycleMs: number | null;
  /**
   * Where the turn that last wrote the row ended its stream, the engine's stamp of the row's last use. The story's
   * live reads stamp the selection instead, and only the read of a row the purge has not yet taken reads this.
   */
  endedAt?: number;
  epoch: number;
  lastRefreshAt: number;
  lastUsedAt: number;
  /** The canonical model of the requests that write the row, which names the weeks the pin draws. */
  model: string;
  /** The pool of the key that wrote the row, whose sweeps a dated stamp reads. */
  poolId: string;
  /** The source a shed move or a failover's landing left, which a renewal keeps and any other write clears. */
  shed: { at: number; from: string; reason: string } | null;
  /** The last use the request walks read the row by (`pinStampOf`), which decides a placement's word. */
  stamp: number;
}
const pins = new Map<string, PinState>();
let epochs = 0;
/** A row as a selection reads it at `at`, by its stamp and its own pool's affinity window. */
const pinReadOf = (p: PinState, at: number) => pinReadAt(p.stamp, affinityTtlMsOf(p.poolId), at);
/**
 * The models of the pins the routing state keeps on the account under the pool's keys at `at`. A pin counts where no
 * sweep has taken it, `live` or `expired_idle` under `pinReadAt`.
 */
const heldModelsOf = (accountId: string, pool: PoolSeed, at: number): string[] =>
  [...pins.values()].filter((p) => p.accountId === accountId && p.poolId === pool.id && pinReadOf(p, at) !== 'none').map((p) => p.model);

/**
 * The newest purge sweep that found an account with no pin of a pool, by pool and account, read on each account that
 * `OVER_CEILING_SINCE` dates a stamp for. The router drops an account's stamps at such a sweep, so a round that rests
 * on a stamp older than the sweep is one the router would not fire.
 */
const bareSweeps = new Map<string, number>();
/** The next sweep `noteSweeps` reads, the first behind the earliest date the story keeps. */
let nextSweep = sweepAfter(Math.min(...Object.values(OVER_CEILING_SINCE).flatMap((tests) => Object.values(tests))));

/**
 * Reads each sweep ahead of `until` against the pins the walk holds at it. The decisions run in time order, so the
 * pins stand as the decisions before the sweep left them, and a sweep keeps a pin whose age is at most its pool's TTL.
 */
function noteSweeps(until: number): void {
  const pools = [...poolById.values()].filter((p) => p.affinityTtlSeconds > 0);
  for (; nextSweep < until; nextSweep += MINUTE) {
    const at = nextSweep;
    const held = new Set([...pins.values()].filter((p) => pinReadOf(p, at) === 'live').map((p) => `${p.poolId}|${p.accountId}`));
    for (const accountId of Object.keys(OVER_CEILING_SINCE)) {
      for (const pool of pools) if (!held.has(`${pool.id}|${accountId}`)) bareSweeps.set(`${pool.id}|${accountId}`, at);
    }
  }
}

/** The dated stamps whose selection read their test over, each as account, test and date. */
const readOver = new Set<string>();

/**
 * Fails the build where a story round rests on a dated stamp that the router would not hold: a date whose selection
 * did not read the test over, or a purge sweep between the date and the round that found the account with no pin of
 * the round's pool.
 */
function stampHolds(accountId: string, test: string, words: string, since: number, at: number, pool: PoolSeed): void {
  if (!readOver.has(`${accountId}|${test}|${since}`)) throw new Error(`the story dates ${accountId}'s ${words} stamp at ${iso(since)}, where no selection on ${accountId} read that test over`);
  const bare = bareSweeps.get(`${pool.id}|${accountId}`);
  if (bare !== undefined && bare > since) throw new Error(`the story keeps ${accountId}'s ${words} stamp of ${iso(since)} to its round at ${iso(at)}, though the sweep at ${iso(bare)} finds ${accountId} with no pin of ${pool.id}`);
}
const recordShedOf = (shed: PinState['shed']): RecordPin['shed'] => (shed === null ? null : { at: iso(shed.at), from: shed.from, reason: shed.reason });
/** The pin step's facts on the row it read, which follow its `found` fact. */
const pinFacts = (state: PinState, idleMs: number, refreshAgeMs: number) => [fact('account_id', state.accountId, null, state.accountId), fact('epoch', state.epoch, 'count'), fact('idle_ms', idleMs, 'ms'), fact('refresh_age_ms', refreshAgeMs, 'ms')];
/** The reasons a `pin_expired` event's release names, none where the event carries no list. */
const releasedReasons = (e: Pick<RequestEvent, 'detail'> | undefined): string[] => {
  const reasons = e?.detail?.reasons;
  return Array.isArray(reasons) ? reasons.map(String) : [];
};
/** The pin's list a `pin_migrated` event carries where the state of the pin's account forced the move, else null. */
export const forcedReasons = (e: Pick<RequestEvent, 'detail' | 'kind'>): string[] | null => (e.kind === 'pin_migrated' && Array.isArray(e.detail?.reasons) ? releasedReasons(e) : null);
const laneOf = (r: RequestLog) => `${r.canonicalModel}|${r.shape.requestedSpeed === 'fast' ? 'fast' : 'standard'}|${r.shape.thinkingType ?? 'none'}|${r.shape.effort ?? '-'}`;

/**
 * Whether a pin of the account holds a cycle at the decision: the request's own by the pace the story names, or one
 * the routing state keeps live on the account. Until one does, the slow-down holds no pin, so the account's pins
 * realize full speed, as on the first turns after a placement.
 */
function cycleStands(spec: Spec, accountId: string, pool: PoolSeed): boolean {
  if ((spec.pinPace?.cycleMs ?? null) !== null) return true;
  return [...pins.values()].some((p) => p.accountId === accountId && p.cycleMs !== null && spec.at - p.lastUsedAt <= pool.affinityTtlSeconds * 1_000);
}

/**
 * What a placement or a move books on the account it chose until the usage replaces it: the rewrite of the pin's last
 * prompt, or of the body for a fresh session, and the burn it brings, per plan window the model draws. A count, a
 * probe, a fast request and a client classifier call book nothing, and neither does a paid source, which has no plan
 * capacity.
 */
function reservationOf(spec: Spec, chosen: RecordCandidate | undefined): { reservation: NonNullable<RecordOutcome['reservation']>; step: WalkStep } | null {
  const r = spec.request;
  const account = chosen === undefined ? undefined : accountById.get(chosen.accountId);
  if ((spec.kind !== 'placed' && spec.kind !== 'moved') || chosen === undefined || account === undefined || isPaidSource(account) || r.kind !== 'inference' || r.shape.requestedSpeed === 'fast' || r.classifier !== null) return null;
  const id = chosen.accountId;
  const rewrite = rewriteOf(spec.kind === 'moved' ? (spec.pinFigures?.tokens ?? bodyTokensOf(r)) : bodyTokensOf(r), r.canonicalModel);
  const brought = spec.kind === 'moved' && spec.pinFigures ? spec.pinFigures.burn : priorOf(spec);
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

/**
 * The turn each of the story's classifier calls judges, the session's turn under the call's key whose stream ended
 * last before it, and where the router sent the call. The two `client` calls of the laptop session judge their own
 * subagents' turns on primary and follow them there. The `client_off` call judges main's turn that ended on secondary
 * at 14:01:47, whose pin stood there until 14:05:12, and secondary read its 5-hour window spent at 14:02:11, so the call
 * follows neither and the strategy places it.
 */
const JUDGED_TURNS: Record<string, { direction: 'pin_account' | 'strategy' | 'turn_account'; turn: string }> = {
  'req_4d9a1e6b-c20': { direction: 'turn_account', turn: JUDGED_TURN_IDS[0] },
  'req_63a8d2f5-b17': { direction: 'turn_account', turn: JUDGED_TURN_IDS[1] },
  'req_7c2e9f41-d03': { direction: 'strategy', turn: 'req_c90d3f61-a28' },
};

/** Whether the decision is a classifier call the router directs to its judged turn's account or that turn's pin's. */
const directedOf = (spec: Spec): boolean => spec.pin === 'classifier' && JUDGED_TURNS[spec.request.id].direction !== 'strategy';

/** A classifier call's judged agent, model and account, read off its judged turn's row, the account the one its
 *  stream ended on. */
function judgedTurnOf(callId: string): { account: string; agent: string; direction: (typeof JUDGED_TURNS)[string]['direction']; model: string } {
  const { direction, turn } = JUDGED_TURNS[callId];
  const r = requestById.get(turn);
  const ended = r ? servingAttempt(r) : null;
  if (!r || !ended || r.agentId === null) throw new Error(`the classifier call ${callId} judges a turn the story lacks: ${turn}`);
  return { account: ended.accountId, agent: r.agentId, direction, model: r.canonicalModel };
}

/**
 * The pin step: the pin the request found, or the reason it read none. A live pin is the one the routing state holds
 * where the day's rows last left it inside the pool's window, else the trace's own, which the walk writes back with a
 * fresh epoch where the state held none.
 */
function pinStepOf(spec: Spec, pool: PoolSeed, pinKey: string): { pin: RecordPin | null; pinReason: PinReason | null; steps: WalkStep[] } {
  const r = spec.request;
  const held = pins.get(pinKey);
  if (spec.pin === 'classifier') {
    const turn = judgedTurnOf(r.id);
    const facts = [fact('reason', 'classifier', 'text'), fact('judged_agent', turn.agent, 'text'), fact('judged_model', turn.model, 'text'), fact('judged', turn.account, 'text', turn.account), fact('direction', turn.direction, 'text')];
    return { pin: null, pinReason: 'classifier', steps: [step('pin', 'none', facts)] };
  }
  if (typeof spec.pin === 'string' && spec.pin !== 'none') return { pin: null, pinReason: spec.pin, steps: spec.kind === 'refused' ? [] : [step('pin', 'none', [fact('reason', spec.pin)])] };
  if (spec.pin === 'none') {
    // The row's stamp decides the word, as the request walks read it: a row idle past the pool's window that the purge
    // has not yet taken, with the reasons its release names, or none behind the sweep. The engine writes no warmth and
    // no mark for a row it releases.
    if (held !== undefined && pinReadOf(held, spec.at) === 'expired_idle') {
      const lastUsedAt = held.endedAt ?? held.lastUsedAt;
      const idleMs = spec.at - lastUsedAt;
      const pin: RecordPin = {
        accountId: held.accountId, createdAt: iso(held.createdAt), cycleMs: null, epoch: held.epoch, found: 'expired_idle', idleMs, incarnation: 1, lane: laneOf(r), lastRefreshAt: iso(held.lastRefreshAt), lastUsedAt: iso(lastUsedAt),
        marked: false, markedAt: null, markedOn: null, markedWindow: null, refreshAgeMs: spec.at - held.lastRefreshAt, released: spec.replaced?.reasons ?? [],
        shed: recordShedOf(held.shed), slowDownUntil: null, warmth: null, warmthCause: null,
      };
      return { pin, pinReason: null, steps: [step('pin', 'none', [fact('found', 'expired_idle'), ...pinFacts(held, idleMs, spec.at - held.lastRefreshAt)])] };
    }
    const pin: RecordPin = { accountId: null, createdAt: null, cycleMs: null, epoch: null, found: 'none', idleMs: null, incarnation: null, lane: null, lastRefreshAt: null, lastUsedAt: null, marked: false, markedAt: null, markedOn: null, markedWindow: null, refreshAgeMs: null, released: [], shed: null, slowDownUntil: null, warmth: null, warmthCause: null };
    return { pin, pinReason: null, steps: [step('pin', 'none', [fact('found', 'none')])] };
  }
  const on = spec.from ?? spec.accountId ?? '';
  // A drawn trace names where the pin lived. A pin the day's rows last left past its window was used by a request
  // the day does not hold, so the trace's own idle age stands.
  const recent = held !== undefined && held.accountId === on && spec.at - held.lastUsedAt <= pool.affinityTtlSeconds * 1_000;
  const found = recent ? held : { accountId: on, createdAt: held?.createdAt ?? spec.at - 40 * MINUTE, cycleMs: null, epoch: held?.epoch ?? (epochs += 1), lastRefreshAt: spec.at - spec.pin.idleMs, lastUsedAt: spec.at - spec.pin.idleMs, model: r.canonicalModel, poolId: pool.id, shed: null, stamp: spec.at - spec.pin.idleMs };
  pins.set(pinKey, found);
  const idleMs = spec.at - found.lastUsedAt;
  const released = spec.failover ? [spec.failover === 'fast_lane' ? 'fast_rate_limit' : 'cooldown'] : (spec.reasons ?? []);
  // A shed move's pin carries the mark the round on its account wrote.
  const mark = spec.rule === 'shed' ? markOf(found.accountId, spec.at) : null;
  const pin: RecordPin = {
    accountId: found.accountId, createdAt: iso(found.createdAt), cycleMs: spec.pinPace?.cycleMs ?? null, epoch: found.epoch, found: 'live', idleMs, incarnation: 1, lane: laneOf(r), lastRefreshAt: iso(spec.at - idleMs), lastUsedAt: iso(spec.at - idleMs),
    marked: mark !== null, markedAt: mark === null ? null : iso(mark.at), markedOn: mark?.on ?? null, markedWindow: mark?.window ?? null, refreshAgeMs: idleMs, released,
    shed: recordShedOf(found.shed),
    slowDownUntil: spec.pinPace?.slowDownUntil === null || spec.pinPace?.slowDownUntil === undefined ? null : iso(spec.pinPace.slowDownUntil), warmth: spec.pin.warmth, warmthCause: spec.pin.cause,
  };
  return { pin, pinReason: null, steps: [step('pin', 'match', [fact('found', 'live'), ...pinFacts(found, idleMs, idleMs), fact('warmth', spec.pin.warmth), fact('cause', spec.pin.cause), fact('marked', pin.marked)])] };
}

/**
 * The move rules a live pin a portable request still reaches walks, the destinations a fired rule names, whether the
 * move gate answered the fitting order, which tells a move's runway ties apart, and the gate's last read of each
 * candidate over every walk the decision ran. A token count follows its pin, since it writes none a move could leave,
 * a probe warms the pin it was sent for and never moves it, a request that carries no conversation follows a pin that
 * holds account state, since the session's next turn finds its references on that account alone, and a pin past its
 * account's end is released at renewal instead.
 */
function moveWalkOf(spec: Spec, candidates: RecordCandidate[], pool: PoolSeed, pin: RecordPin | null, portable: boolean): { gateFits: boolean; gateReads: Map<RecordCandidate, FitRead>; movedAmong: string[] | undefined; steps: WalkStep[] } {
  const r = spec.request;
  const steps: WalkStep[] = [];
  const gateReads = new Map<RecordCandidate, FitRead>();
  let movedAmong: string[] | undefined;
  if (pin?.found === 'live' && portable && !spec.failover && (spec.kind === 'renewed' || leftPin(spec) || inPlaceOf(spec) !== null)) {
    const source = candidates.find((c) => c.accountId === pin.accountId);
    const endsAt = source?.term?.endsAt ? Date.parse(source.term.endsAt) : null;
    const skip =
      r.kind === 'count_tokens'
        ? 'count'
        : r.kind === 'probe'
          ? 'probe'
          : !carriesConversation(r) && accountBoundAt(pinKeyOf(r), spec.at)
            ? 'account_bound'
            : endsAt !== null && endsAt <= spec.at
              ? 'subscription_ended'
              : null;
    if (skip !== null) {
      for (const rule of RULES) steps.push(step(`move:${rule}`, 'not_run', [fact('reason', skip)]));
    } else {
      const walkRules = () => {
        const m = moveOf(spec, candidates, pool, source, pin);
        for (const [c, read] of m.reads) gateReads.set(c, read);
        return m;
      };
      let move = walkRules();
      // A drawn renewal stays where its draw put it, so where a rule's reason holds every destination reads at its cap,
      // no move is made and the pin renews in place. A story renewal reads the slots the story fills.
      if (spec.kind === 'renewed' && move.fired !== null && !spec.story) {
        for (const c of move.reached) c.inFlight = c.inFlightCap;
        move = walkRules();
      }
      // The shed line carries the account's read whichever rule answered.
      const pace = slowFactsOf(source, pool, spec.at, cycleStands(spec, source?.accountId ?? '', pool));
      steps.push(...move.lines.map((line) => (line.step === 'move:shed' ? { ...line, facts: [...line.facts, ...pace] } : line)));
      if (move.fired !== null) {
        steps.push(step('cap_filter', move.destinations.some((c) => c.inFlight < c.inFlightCap) ? 'passed' : 'dropped', move.destinations.map((c) => fact('in_flight', `${c.inFlight}/${c.inFlightCap}`, 'count', c.accountId))));
        if (spec.kind === 'moved') {
          movedAmong = move.destinations.map((c) => c.accountId);
          for (const c of move.destinations) if (c.accountId !== spec.accountId) c.verdict = 'destination';
        }
      }
    }
  }
  return { gateFits: steps.some((w) => w.step === 'fit' && w.verdict === 'fits'), gateReads, movedAmong, steps };
}

/** The rung stations in the walk's order, each with the rung it places on. */
const RUNG_STEPS: [string, Placement['rung']][] = [['rung:content', 'content'], ['rung:with_main', 'with_main'], ['rung:static', 'static']];

/**
 * The placement walk: the partitions, the three rungs and the pick among the members they kept, or an exhaustion's
 * empty pick. `gateFits` is whether the move gate answered the fitting order, which tells a move's runway ties apart,
 * and `gateReads` the gate's last read of each destination, whose runway a move's tie ranks by. A decision that holds
 * in place or renews walks none, and neither does a probe's pinned answer.
 */
function placingWalkOf(spec: Spec, candidates: RecordCandidate[], pool: PoolSeed, movedAmong: string[] | undefined, gateFits: boolean, gateReads: Map<RecordCandidate, FitRead>): { pick: ReturnType<typeof pickOf>; steps: WalkStep[]; tierSize: number } {
  const r = spec.request;
  const member = (id: string | null | undefined) => candidates.find((c) => c.accountId === id);
  const steps: WalkStep[] = [];
  let pick: ReturnType<typeof pickOf> = { runnerUp: null, score: null, tierSize: 0 };
  let tierSize = 0;
  const placing = walksPlacement(spec);
  if (placing) {
    const set = candidates.filter((c) => (movedAmong ? movedAmong.includes(c.accountId) : c.obstacle === null));
    const partitions = narrowed(set, pool, r, movedAmong !== undefined);
    // A row carries the label of the first partition that dropped its member, from the run the step's facts record.
    for (const [c, label] of partitions.dropped) c.partition = label;
    steps.push(step('partitions', partitions.left.length > 0 ? 'match' : 'none', partitions.facts));
    // A classifier call that follows its judged turn goes ahead of the rungs and the strategy, so every station names
    // the call as its reason and the pick ranks nobody.
    if (directedOf(spec)) {
      for (const [name] of RUNG_STEPS) steps.push(step(name, 'none', [fact('reason', 'classifier')]));
      steps.push(step('pick', 'chosen', [fact('account_id', spec.accountId, null, spec.accountId), fact('reason', 'classifier')]));
      return { pick, steps, tierSize };
    }
    const rung = spec.rung ?? null;
    const subagent = r.agentId !== 'main';
    let matched = false;
    for (const [name, which] of RUNG_STEPS) {
      if (matched) {
        steps.push(step(name, 'not_run'));
        continue;
      }
      if (rung?.rung === which) {
        matched = true;
        const chosen = member(spec.accountId);
        const entryOf = which === 'static' ? chosen?.prefix.staticMatch : chosen?.prefix.contentMatch;
        steps.push(step(name, 'match', [fact('depth', rung.depth, 'count'), fact('prefix_tokens', rung.prefixTokens, 'tokens'), fact('account_id', spec.accountId, null, spec.accountId), ...(entryOf ? [fact('hash', entryOf.hash)] : [])]));
        continue;
      }
      const reason = r.kind === 'count_tokens' ? 'count' : which === 'with_main' && !subagent ? 'class' : which === 'with_main' && pool.subagentPlacement !== 'with_main' ? 'policy_off' : 'no_entry';
      steps.push(step(name, 'none', [fact('reason', reason)]));
    }
    if (!matched && spec.kind === 'exhausted') steps.push(step('pick', 'none', [fact('strategy', pool.strategy)]));
    else if (!matched) {
      // A hold's pick ranks the members as a placement's does and names the account it waits on, while its row reads
      // the hold rather than a score.
      const reserve = (c: RecordCandidate) => paceFactsOf(accountOf(c.accountId), spec.at, r.canonicalModel, c, true);
      const ranked = pickOf(candidates, partitions.left, spec.accountId, pool.strategy, spec.kind === 'moved' ? spec.from : undefined, reserve);
      if (spec.kind !== 'held') pick = ranked;
      tierSize = ranked.tierSize;
      const tier = member(spec.accountId)?.tier ?? [0, 0, 0];
      // A burn fact follows each score, the burn the spare rate took off, and a member that holds a Fable reservation
      // names it after its burn.
      const scores = candidates
        .filter((c) => c.score !== null)
        .flatMap((c) => {
          const reserved = c.pace?.fableReservation ?? null;
          return [fact('score', c.score, scoreUnitOf(c), c.accountId), fact('burn', c.pace?.burn ?? 0, 'percent_per_hour', c.accountId), ...(reserved === null ? [] : [fact('fable_reservation', reserved, 'percent', c.accountId)])];
        });
      // The strategy's ties are the ranked tier's best scores under Pace and the whole tier under Priority. The runway
      // breaks them for a move whose gate answered the fitting order and for a failover by its fitting members' reads,
      // and round robin's cursor picks alone. Each tie ranks by its read's runway to the ceiling over every bounded
      // window, the gate's on a move and the placement's on a failover.
      const ranks = candidates.filter((c) => c.rank !== null);
      const ties = pool.strategy === 'pace' ? ranks.filter((c) => c.score === Math.max(...ranks.map((x) => x.score ?? 0))) : ranks;
      const byRunway = pool.strategy === 'round_robin' ? [] : runwayTiesOf(ties, (c) => (movedAmong ? gateFits : spec.failover !== undefined && c.fit === 'fits'), (c) => (movedAmong ? gateReads : FRESH_FITS).get(c)?.runwayMs ?? null);
      // The draw is left to the runway's ties where they narrowed the strategy's, else to the strategy's own. Round
      // robin's cursor names the chosen member's turn among the ranked tier, so it leaves no draw.
      const draw =
        pool.strategy === 'round_robin'
          ? [fact('turn', `${member(spec.accountId)?.rank ?? 1} of ${ranks.length}`), fact('drawn', false)]
          : [fact('ties', ties.map((c) => c.accountId)), ...(byRunway.length > 0 ? [fact('by_runway', byRunway.map((c) => c.accountId))] : []), fact('drawn', (byRunway.length > 0 ? byRunway : ties).length > 1)];
      // The tier as the engine writes it, a list of three strings.
      steps.push(step('pick', 'chosen', [fact('strategy', pool.strategy), fact('tier', tier.map(String)), ...scores, ...draw]));
    }
  }
  return { pick, steps, tierSize };
}

/**
 * The pin a dispatched decision writes back: every renewal, keeping its epoch and its stamp, and a placement or a move
 * with a fresh epoch. A token count creates and moves none, so the session keeps the one it had.
 */
function pinWriteOf(spec: Spec, pinning: boolean, pinKey: string, stamps: { hashes: boolean; refresh: boolean }): RecordOutcome['pinWritten'] {
  const r = spec.request;
  if (!isDispatched(spec) || !pinning || (r.kind === 'count_tokens' && spec.kind !== 'renewed')) return null;
  // A rule's move alone writes `moved`: a failover's landing and a placement the pin's account state forced leave the
  // pin by no rule, so each writes its row afresh.
  const action = spec.kind === 'renewed' ? 'renewed' : spec.kind === 'moved' ? 'moved' : 'created';
  const prior = pins.get(pinKey);
  const kept = action === 'renewed' ? prior : undefined;
  const epoch = kept?.epoch ?? (epochs += 1);
  // The engine stamps the row a shed move or a failover's landing writes with its source, a renewal keeps the row's
  // stamp, and every other write clears it.
  const source = spec.from ?? '';
  const shed = action === 'renewed' ? (prior?.shed ?? null) : spec.failover ? { at: spec.at, from: source, reason: spec.failover } : spec.kind === 'moved' && spec.rule === 'shed' ? { at: spec.at, from: source, reason: 'round' } : null;
  const dispatched = dispatchedAttempt(spec);
  // A renewal's stamp never moves back, where it arrives while an earlier request of its pin still streams. A write
  // whose request the relay ended before any attempt stamps its selection.
  const own = r.attempts.length > 0 ? pinStampOf(r, action === 'renewed') : spec.at;
  const stamp = kept ? Math.max(kept.stamp, own) : own;
  pins.set(pinKey, { accountId: spec.accountId ?? '', createdAt: kept?.createdAt ?? spec.at, cycleMs: spec.pinPace?.cycleMs ?? null, endedAt: dispatched ? attemptEnd(dispatched) : spec.at, epoch, lastRefreshAt: spec.at, lastUsedAt: spec.at, model: r.canonicalModel, poolId: r.poolId ?? '', shed, stamp });
  return action === 'renewed' ? { action, epoch, hashesStamped: stamps.hashes, lane: laneOf(r), refreshStamped: stamps.refresh } : { action, epoch, hashesStamped: r.kind === 'inference', lane: laneOf(r), refreshStamped: true };
}

/**
 * A dispatched decision's completion: the cache map its answer wrote, the reads it reported and its attempt's status.
 * The router writes the message chain's rows first, each at the answer's one prefix, then the static chain's, each
 * numbered by its place in that chain.
 */
function completionOf(spec: Spec, pool: PoolSeed, chosen: RecordCandidate | undefined, attempt: Attempt | undefined, cache: { answered: boolean; hasUsage: boolean; hit: boolean | null; promised: number | null }): RecordCompletion {
  const r = spec.request;
  const usage = r.usage;
  const ttlMs = ttlOf(pool, chosen?.kind ?? 'oauth', r.agentId !== 'main', r.canonicalModel);
  const prefixTokens = usage.cacheRead + usage.cacheWrite5m + usage.cacheWrite1h;
  const written = spec.cacheMap
    ? [
        ...Array.from({ length: spec.cacheMap.messages }, (_, i) => ({ depth: i + 1, prefixTokens, tier: 'message' })),
        ...spec.cacheMap.staticSizes.map((size, i) => ({ depth: i + 1, prefixTokens: size, tier: 'static' })),
      ]
    : [{ depth: (spec.rung?.depth ?? 0) + 1, prefixTokens, tier: 'message' }];
  return {
    cacheMapWritten: cache.answered && usage.cacheWrite5m + usage.cacheWrite1h > 0 ? written.map((e) => ({ accountId: spec.accountId ?? '', depth: e.depth, prefixTokens: e.prefixTokens, tier: e.tier, ttlMs })) : null,
    cacheReadTokens: cache.hasUsage ? usage.cacheRead : null,
    cacheWriteTokens: cache.hasUsage ? usage.cacheWrite5m + usage.cacheWrite1h : null,
    completedAt: attempt ? iso(attemptEnd(attempt)) : null,
    freed: spec.heldBefore !== undefined,
    heldMs: spec.heldBefore?.heldMs ?? null,
    hit: cache.hit,
    inputTokens: cache.hasUsage ? usage.input : null,
    inputTransformations: attempt?.response.inputTransformations ?? null,
    promisedTokens: cache.promised,
    status: attempt?.upstreamStatus ?? null,
  };
}

/**
 * A held decision's hold. A slow-down hold copies the read it was answered from, its bound the pool's longest wait for
 * the slow-down, which bounds a wait for a rewrite in flight too.
 */
function holdOf(spec: Spec, pool: PoolSeed): RecordHold | null {
  const inPlace = inPlaceOf(spec);
  const slowed = inPlace === 'slow_down' ? slowReadAt(spec.accountId ?? '', pool, spec.at) : null;
  if (spec.kind !== 'held' || !spec.hold) return null;
  return {
    accountId: spec.accountId ?? '',
    boundMs: (inPlace === 'slow_down' || inPlace === 'rewrite' ? pool.slowDownHoldSeconds : pool.retryInPlaceSeconds) * 1_000,
    cohort: spec.hold.cohort,
    cycleMs: inPlace === 'slow_down' ? (spec.pinPace?.cycleMs ?? null) : null,
    // The factor the hold was sized from: the read's, pulled toward 1 by the ramp.
    factor: slowed ? 1 - (1 - slowed.factor) * rampAt(spec.accountId ?? '', pool, spec.at) : null,
    reason: inPlace ?? 'cap',
    // The speed the account's pins realize, as the shed line reads it.
    speed: slowed ? (cycleStands(spec, spec.accountId ?? '', pool) ? slowed.speed : 1) : null,
    stretchMs: slowed?.stretchMs ?? null,
    thresholdMs: slowed ? pool.slowDownStretchSeconds * 1_000 : null,
    until: inPlace !== null && spec.hold.until !== undefined ? iso(spec.hold.until) : null,
    waitsFor: spec.hold.waitsFor ?? null,
  };
}

/**
 * What the router recorded for one decision: the record and the row the list route answers. `messageRows` is the
 * request's most message rows any of its answers writes, which its chain's boundary count reads.
 */
function build(spec: Spec, messageRows: number): { record: DecisionRecord; summary: DecisionSummary } {
  const r = spec.request;
  const pool = poolById.get(r.poolId ?? '')!;
  const fast = r.shape.requestedSpeed === 'fast';
  const candidates = spec.kind === 'refused' ? [] : candidatesOf(spec, pool);
  const inPlace = inPlaceOf(spec);
  if (!spec.story && (spec.kind === 'placed' || spec.kind === 'moved')) fillDrawnRivals(spec, candidates, pool);
  if (spec.kind !== 'moved' && walksPlacement(spec)) readPlacementFits(spec, candidates, pool);
  // A turn that names state its account holds is no portable request, whatever its client.
  const portable = r.client !== 'other' && !ACCOUNT_BOUND_TURN_IDS.has(r.id);
  const pinKey = pinKeyOf(r);
  const pinning = pool.affinityTtlSeconds > 0 && r.sessionId !== null && (typeof spec.pin === 'object' || spec.pin === 'none');
  const { pin, pinReason, steps: pinSteps } = pinStepOf(spec, pool, pinKey);
  const walk: WalkStep[] = [...pinSteps];
  if (spec.kind === 'refused') {
    walk.push(step('admission', 'refused', [fact('reason', spec.refusal?.reason ?? '')]), step('selection', 'not_run'));
  } else {
    walk.push(step('eligible', candidates.some((c) => c.obstacle === null) ? 'match' : 'none', candidates.map((c) => fact('standing', c.obstacle?.name ?? 'eligible', null, c.accountId))));
  }
  const member = (id: string | null | undefined) => candidates.find((c) => c.accountId === id);
  // A probe warms the pin it was sent for: the line names that member's obstacle, if any, and whether this probe
  // already tried it, `no_pin` where the session holds no live pin, or `pin_moved` alone where the pin stands on
  // another account than the recipe's, which the selector answers before it reads the member. A pinned answer ends the
  // walk there, the reserve station aside.
  if (r.kind === 'probe') {
    const pinned = pin?.found === 'live' ? member(pin.accountId) : undefined;
    const ownWord = spec.exhaustion?.obstacle === 'no_pin' || spec.exhaustion?.obstacle === 'pin_moved' ? spec.exhaustion.obstacle : null;
    const facts = ownWord !== null ? [fact('obstacle', ownWord)] : pinned ? [...(pinned.obstacle ? [fact('obstacle', pinned.obstacle.name)] : []), fact('tried', spec.attempted.includes(pinned.accountId))] : [];
    walk.push(step('probe', pinnedAnswerOf(spec) ? 'answered' : 'passed', facts));
  }
  const { gateFits, gateReads, movedAmong, steps: moveSteps } = moveWalkOf(spec, candidates, pool, pin, portable);
  walk.push(...moveSteps);
  // A renewal stamps the refresh on a turn that refreshes the pin, and the hashes on any turn that touches it and
  // carries a chain. A probe and a token count leave both.
  const warmth = typeof spec.pin === 'object' ? spec.pin.warmth : null;
  const stamps = { hashes: warmth !== 'untouched' && r.kind === 'inference', refresh: warmth === 'refresh' };
  if (spec.kind === 'renewed') {
    walk.push(step('renew', 'renewed', [fact('account_id', spec.accountId, null, spec.accountId), fact('lane', laneOf(r)), fact('refresh_stamped', stamps.refresh), fact('hashes_stamped', stamps.hashes)]));
  }
  const { pick, steps: placingSteps, tierSize } = placingWalkOf(spec, candidates, pool, movedAmong, gateFits, gateReads);
  walk.push(...placingSteps);
  if (spec.kind === 'held' && spec.hold && inPlace === null) {
    walk.push(step('hold', 'held', [fact('place', spec.hold.place), fact('account_id', spec.accountId, null, spec.accountId), fact('bound_ms', pool.retryInPlaceSeconds * 1_000, 'ms')]));
  }
  const named = spec.kind === 'exhausted' ? (spec.exhaustion ?? exhaustionOf(candidates)) : null;
  const exhaustion = named && { ...named, status: exhaustionStatus(spec, named) };
  if (exhaustion && walksPlacement(spec)) {
    walk.push(step('exhausted', 'exhausted', [fact('kind', exhaustion.kind), fact('deadline', exhaustion.deadline === null ? null : Date.parse(exhaustion.deadline), 'time'), fact('window', exhaustion.window), fact('obstacles', candidates.map((c) => c.obstacle?.name ?? 'eligible'))]));
  }
  // Every walked selection ends at the reserve station: a booking where a placement or a move booked, else the
  // account it landed or waits on alone, and nothing on an exhaustion.
  const booking = reservationOf(spec, member(spec.accountId));
  if (spec.kind !== 'refused') walk.push(booking?.step ?? step('reserve', 'none', spec.accountId === null || spec.kind === 'exhausted' ? [] : [fact('account_id', spec.accountId, null, spec.accountId)]));

  // The router names the account a decision left on a move or a failover's landing alone, so a placement that
  // replaced an idle row names none. The member loop and the outcome read this one name, so the row that reads
  // `source` is the account the outcome names.
  const left = leftPin(spec) || spec.failover ? (spec.from ?? null) : null;
  // Each member's verdict as the router reads it once the answer is in.
  for (const c of candidates) {
    if (c.accountId === spec.accountId) c.verdict = spec.kind === 'renewed' ? 'renewed' : spec.kind === 'held' ? 'held_on' : 'chosen';
    else if (c.accountId === left) c.verdict = 'source';
    else if (c.verdict === 'destination') continue;
    else if (c.obstacle !== null || c.partition !== null) c.verdict = 'out';
    else if (c.rank !== null) c.verdict = 'ranked';
    else if (tierSize > 0) c.verdict = 'eligible';
    else c.verdict = 'not_needed';
  }

  const chosen = member(spec.accountId);
  const dispatched = isDispatched(spec);
  const attempt = dispatchedAttempt(spec);
  const answered = attempt?.upstreamStatus === 200 && spec.attempt === r.attempts.length;
  // A dispatch names the tier its pick ranked, none where the walk reached no pick.
  const tier = dispatched && tierSize > 0 ? (chosen?.tier ?? null) : null;
  const pinEvents = pinEventsOf(spec, pinning);
  const pinWritten = pinWriteOf(spec, pinning, pinKey, stamps);
  const usage = r.usage;
  const promised = spec.rung && spec.rung.rung !== 'with_main' ? spec.rung.prefixTokens : null;
  // The router stores no usage for a token count's answer, so its completion reads no cache figure, as the route
  // answers an attempt with no usage.
  const hasUsage = answered && r.kind !== 'count_tokens' && bodyTokensOf(r) > 0;
  const hit = !hasUsage ? null : spec.rung ? placementHit(spec.rung, usage.cacheRead) : usage.cacheRead > 0;
  const completion = dispatched ? completionOf(spec, pool, chosen, attempt, { answered, hasUsage, hit, promised }) : null;
  const hold = holdOf(spec, pool);
  // The decision that lands a failover's session on a pin names the cooldown that failed it over.
  const failedOver = spec.failover && pinWritten !== null ? r.attempts[spec.attempt - 2]?.cooldownSet : undefined;
  const outcome: RecordOutcome = {
    accountId: spec.accountId,
    cause: failedOver ? causeOf(failedOver.reason, r.canonicalModel) : null,
    cohortHold: spec.heldBefore ?? null,
    completion,
    exhaustion,
    fromAccountId: left,
    hold,
    inOverage: chosen?.inOverage ?? false,
    kind: spec.kind,
    // A move takes the pin's mark with the row it leaves.
    markCleared: spec.kind === 'moved' && pin?.markedAt && pin.markedOn && pin.markedWindow ? { markedAt: pin.markedAt, markedOn: pin.markedOn, markedWindow: pin.markedWindow, projection: null, reason: 'moved' } : null,
    pinEvents,
    pinWritten,
    refusal: spec.refusal ?? null,
    reservation: booking?.reservation ?? null,
    rule: leftPin(spec) ? (spec.rule ?? null) : null,
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
      chain: { boundaries: r.kind === 'inference' ? Math.max(1, messageRows, Math.round(r.shape.messageCount / 4)) : 0, staticBoundaries: r.kind === 'inference' ? 2 : 0, staticHash: r.kind === 'inference' },
      clientClass: r.client === 'other' ? 'other' : 'claude_code',
      flags: { contextCompacted: r.contextCompacted !== null, count: r.kind === 'count_tokens', fast, marker: r.cacheMarker === true, portable, probe: r.kind === 'probe' },
      holds: spec.holds ?? [],
      keyGeneration: r.keyGeneration, keyId: r.keyId ?? '', lane: { effort: r.shape.effort ?? '-', speed: fast ? 'fast' : 'standard', thinking: r.shape.thinkingType ?? 'none' }, parentAgentId: r.agentId === 'main' ? null : 'main', poolId: r.poolId ?? '',
      registryRevision: 412, requestClass: r.requestClass, requestId: r.id, seq: spec.seq, sessionId: r.sessionId,
      ...(spec.refusal?.kind === 'guardrail' || spec.refusal?.kind === 'client' ? refusedFacts(r, spec.refusal.code === 'drift') : {}),
    },
    requestId: r.id,
    seq: spec.seq,
    settings: settingsOf(pool, spec.at, `${r.id}/${spec.seq}`),
    version: 3,
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
    // The router adds the status it answered with where it held the request out, from the function that gives the
    // record its status.
    fact: [...(spec.fact ?? facts), ...(exhaustion !== null && heldOut(spec) ? [summaryFact('answer', exhaustion.status, 'count')] : [])],
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
function exhaustionOf(candidates: RecordCandidate[]): Omit<Exhaustion, 'status'> {
  const answer = (kind: string, o: Partial<Omit<Exhaustion, 'status'>> = {}) => ({ deadline: null, kind, obstacle: null, reason: null, window: null, ...o });
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

const CLEARED_BY_WAIT = new Set(['cooldown', 'credits', 'overage', 'spent']);

/**
 * The status the relay answers an exhaustion with, by the router's rule. The router reads an untimed exhaustion as the
 * cap, which frees on its own, only where it held the request at the cap with no leg dialed. The relay dials a leg for
 * each attempt, so a decision past attempt 1 has one behind it. A leg that fails over ahead of the attempt's count
 * leaves attempt 1 with a leg dialed, and the story draws none.
 */
function exhaustionStatus(spec: Spec, x: Omit<Exhaustion, 'status'>): number {
  if (x.kind === 'untimed') return heldOut(spec) && spec.attempt === 1 ? 429 : 503;
  if (x.kind === 'model_rejected') return 404;
  if (x.kind === 'pinned') return x.obstacle !== null && CLEARED_BY_WAIT.has(x.obstacle) ? 429 : 503;
  return 429;
}

/** The pin events the routing task emits after the decision, in its order. */
function pinEventsOf(spec: Spec, pinning: boolean): RecordOutcome['pinEvents'] {
  const r = spec.request;
  const events = REQUEST_EVENTS.get(r.id) ?? [];
  const attempt = r.attempts[spec.attempt - 1];
  const moved = attempt && migrationAt(r, attempt);
  if (moved && pinning && (spec.kind === 'placed' || spec.kind === 'moved')) return [{ kind: 'pin_migrated', reason: String(moved.detail?.reason ?? '') }];
  // A release the selection made lands at its instant. One the request's end wrote, a 400's, follows the decision.
  const expired = spec.seq === 1 ? events.find((e) => e.kind === 'pin_expired' && Date.parse(e.at) === spec.at) : undefined;
  const reasons = releasedReasons(expired);
  const created = pinning && r.kind === 'inference' && spec.kind === 'placed' && spec.pin === 'none';
  return [...(expired ? [{ kind: 'pin_expired', reason: reasons[0] ?? null }] : []), ...(created ? [{ kind: 'pin_created', reason: null }] : [])];
}

/** A placement's summary fact, by the router's own rules. */
function placementFactOf(record: DecisionRecord, pick: ReturnType<typeof pickOf>, strategy: string): SummaryFact[] {
  const rung = record.outcome.rung;
  if (rung?.rung === 'with_main') return [];
  if (rung) return [summaryFact('prefix', rung.prefixTokens, 'tokens'), summaryFact('depth', rung.depth, 'count')];
  const others = record.candidates.filter((c) => c.accountId !== record.outcome.accountId);
  if (others.length > 0 && others.every((c) => record.request.attempted.includes(c.accountId))) return [summaryFact('pick', 'last account', 'text')];
  if (pick.tierSize <= 1) return [summaryFact('pick', 'only candidate', 'text')];
  if (strategy === 'pace' && pick.score && pick.runnerUp) {
    return [summaryFact('gap', round5(pick.score.score - pick.runnerUp.score), pick.score.unit as SummaryFact['unit']), { entity: { id: pick.runnerUp.accountId, kind: 'account' }, key: 'over', unit: null, value: pick.runnerUp.accountId }];
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
      // The wait explains the renewal more than the pin's idle age does: the row names the slow-down or the rewrite in
      // flight it followed and how long it lasted.
      const heldWait = spec.heldBefore?.waits.find((w) => w.reason === 'slow_down' || w.reason === 'rewrite');
      if (heldWait) return { by: by('hold', heldWait.reason), fact: [summaryFact('held', heldWait.heldMs, 'ms')] };
      if (r.kind === 'probe') return { by: by('none', ''), fact: [summaryFact('epoch', record.pin?.epoch ?? 0, 'count')] };
      if (record.pin?.warmth === 'cold') return { by: by('none', ''), fact: [summaryFact('cause', record.pin.warmthCause ?? '', 'text')] };
      // The first window, in the trigger's order, whose projection at its horizon reads over the round's line names its
      // reading. The test is the round's own, strict, so the row names no window that no round fires on.
      const m = record.candidates.find((c) => c.accountId === spec.accountId);
      const hottest = m === undefined ? undefined : triggerWindowsOf(m.accountId, pool, spec.at).find((w) => (m.windows[w]?.projection ?? -1) > triggerLineOf(m, w, pool, spec.at));
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
        const place = reason === 'slow_down' || reason === 'reset' || reason === 'rewrite' ? reason : spec.heldBefore.cohort ? 'cohort' : 'strategy_pick';
        return { by: by('hold', place), fact: [summaryFact('held', spec.heldBefore.heldMs, 'ms')] };
      }
      // A call the judged turn directed names that, since the strategy ranked nobody.
      if (directedOf(spec)) return { by: by('strategy', pool.strategy, record.outcome.tier), fact: [summaryFact('pick', 'classifier', 'text')] };
      return { by: record.outcome.rung ? by('rung', record.outcome.rung.rung) : by('strategy', pool.strategy, record.outcome.tier), fact: placementFact() };
    case 'held':
      // A slowed pin's wait names its end and the factor it was sized under: no slot, pick or reset explains it.
      if (spec.hold?.place === 'slow_down' && record.outcome.hold?.until) {
        const factor = record.outcome.hold.factor;
        return { by: by('hold', 'slow_down'), fact: [summaryFact('until', Date.parse(record.outcome.hold.until), 'time'), ...(factor === null ? [] : [{ entity: null, key: 'factor', unit: null, value: factor }])] };
      }
      // A wait for a reset names its end, since no slot and no pick explain it, and so does a wait for a rewrite in
      // flight.
      if (spec.hold?.place === 'reset' && spec.hold.until !== undefined) return { by: by('hold', 'reset'), fact: [summaryFact('until', spec.hold.until, 'time')] };
      if (spec.hold?.place === 'rewrite' && spec.hold.until !== undefined) return { by: by('hold', 'rewrite'), fact: [summaryFact('until', spec.hold.until, 'time')] };
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
  // A classifier call holds no pin of its own, so it creates, renews and moves none and its word names why.
  const classifier = r.classifier !== null;
  const pinning = !classifier && (pool?.affinityTtlSeconds ?? 0) > 0 && r.sessionId !== null;
  const noPin: Spec['pin'] = classifier ? 'classifier' : pinning ? 'none' : 'pinning_off';
  const at = Date.parse(r.at);
  // The engine holds a request only with every member at its cap, whether it waits on its cohort's account or on the
  // strategy's pick, and a request the bound ran out on waited so too.
  const allAtCap = () => Object.fromEntries((pool?.members ?? []).map((m) => [m.accountId, pool?.inFlightCap ?? 0]));
  const holds: RecordRequest['holds'] = [];
  let waitsRecorded = 0;
  // Every member was at its cap until the bound ran out, so the router answered the request with no dispatch behind the
  // hold.
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
    const spent = (id: string) => r.canonicalModel.includes('fable') && spentForFable(r.canonicalModel, (w) => oauthWindows(id, answeredAt, () => OLDER_FACTOR).find((x) => WINDOW_KEY[x.id] === w)?.utilization);
    // A spent Fable member stays servable where the pool allows overage and the credits the record reads are unblocked.
    const funded = (id: string) => pool !== undefined && fundedBy(pool, fast ? creditsAt(id, answeredAt) : null);
    const servable = pool === undefined ? [] : pool.members.map((m) => m.accountId).filter((id) => obstacleOf(accountById.get(id)!, pool, answeredAt, r.canonicalModel, fast, []) === null && (!spent(id) || funded(id)));
    const passedOver = [...new Set([...attempted, ...waitedOn, ...servable])];
    // The builder names the exhaustion off the members' obstacles, which a block among them dates or explains.
    specs.push({ ...base, accountId: null, at: answeredAt, attempt, attempted: passedOver, fact: [summaryFact('held', bound, 'ms')], holds: [...holds], inFlight: allAtCap(), kind: 'exhausted', pin: noPin, seq: seq + waitedOn.length });
  };
  // A probe that found no live pin sent nothing: the selector answers the pinned obstacle `no_pin` at its selection.
  const noPinDraws = NO_PIN_DRAWS.get(r.id);
  if (noPinDraws !== undefined) {
    const drawnSelections = noPinDraws.filter((_, i) => !resentInPlace({ attempts: noPinDraws }, i)).length + (failsOver(noPinDraws[noPinDraws.length - 1]) ? 1 : 0);
    specs.push({ ...base, accountId: null, at, attempt: 1, drawnSelections, exhaustion: { deadline: null, kind: 'pinned', obstacle: 'no_pin', reason: null, window: null }, kind: 'exhausted', pin: noPin, seq: 1 });
    return specs;
  }
  if (r.attempts.length === 0) {
    if (r.errorCode === 'accounts_at_cap') ranOut(1, 1, at, []);
    else specs.push({ ...base, accountId: null, at, attempt: 1, kind: 'exhausted', pin: noPin, seq: 1 });
    return specs;
  }
  let seq = 0;
  let pinOn: string | null = null;
  r.attempts.forEach((a, i) => {
    if (resentInPlace(r, i)) return;
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
    // A move the state of the pin's account forced carries the pin's list on its event, and its word can be a rule's
    // or a failover's, so the list decides it ahead of the word: the decision stays a placement.
    const migration = migrationAt(r, a);
    const reasons = (migration && forcedReasons(migration)) ?? undefined;
    const failover = moved !== null && reasons === undefined && FAILOVER_REASONS.includes(moved) ? moved : undefined;
    const rule = moved !== null && failover === undefined ? moved : undefined;
    // A request that can hold no pin renews none and moves none: it is placed.
    const kind: DecisionKind = !pinning ? 'placed' : s?.renewed ? 'renewed' : rule && reasons === undefined ? 'moved' : 'placed';
    // A first placement that released an expired pin at its selection carries that account and the release's reasons
    // for its Pin step. A 400's release follows the answer, so it is none of these.
    const expired = i === 0 && kind === 'placed' ? REQUEST_EVENTS.get(r.id)?.find((e) => e.kind === 'pin_expired' && Date.parse(e.at) <= start) : undefined;
    const replacedOn = expired?.accountId ?? null;
    const replaced = replacedOn === null ? undefined : { accountId: replacedOn, reasons: releasedReasons(expired) };
    const from = failover ? attempted[attempted.length - 1] : rule ? (String(REQUEST_EVENTS.get(r.id)?.find((e) => e.kind === 'pin_migrated')?.detail?.from_account ?? '') || undefined) : undefined;
    // The reload's purge takes the pin of an account the registry no longer holds before any selection reads it, so the
    // move off such an account lands on the trace the purge kept and finds no pin.
    const purged = reasons !== undefined && from !== undefined && !accountById.has(from);
    const live = kind === 'renewed' || kind === 'moved' || failover !== undefined || (reasons !== undefined && !purged);
    // A probe and a token count leave the pin's warmth untouched. A written row takes no draw, and neither does a drawn
    // row moved onto its live pin, whose drawn placement took none: its live pin is the row its session's last turn
    // wrote, whose idle age the Pin step reads.
    const untouched = r.kind === 'probe' || r.kind === 'count_tokens';
    const pinSpec: Spec['pin'] = live && pinning ? { cause: r.kind === 'probe' ? 'probe' : r.kind === 'count_tokens' ? 'count' : 'continues_pin', idleMs: UNSEEDED_IDS.has(r.id) || RENEWED_ON_PIN.has(r.id) ? 0 : 20_000 + inFlightRng.int(0, 280) * 1_000, warmth: untouched ? 'untouched' : 'refresh' } : noPin;
    const prefix: Spec['prefix'] = {};
    const rung = s?.placement ?? null;
    if (rung && rung.rung !== 'with_main') {
      const ttl = pool ? ttlOf(pool, accountById.get(a.accountId)?.kind ?? 'oauth', r.agentId !== 'main', r.canonicalModel) : FIVE_MINUTES;
      const boundary = rung.rung === 'static' ? staticBoundary(r.agentId, rung.depth) : `message|${r.sessionId ?? r.id}|${rung.depth}`;
      const found = entry(boundary, { at: start, depth: rung.depth, prefixTokens: rung.prefixTokens, ttlMs: ttl, writtenAt: start - 2 * MINUTE });
      prefix[a.accountId] = rung.rung === 'static' ? { static: found, staticMatch: found } : { content: found, contentMatch: found };
    }
    const heldBefore = holds.length ? { cohort: holds[holds.length - 1].cohort, heldMs: holds.reduce((n, h) => n + h.heldMs, 0), waitedOn: holds.map((h) => h.accountId), waits: holds.map((h) => ({ heldMs: h.heldMs, reason: 'cap' })) } : undefined;
    specs.push({ ...base, accountId: a.accountId, at: start, attempt: i + 1, attempted, failover, from: from ?? pinOn ?? undefined, heldBefore: hold ? heldBefore : undefined, holds: [...holds], kind, pin: pinSpec, prefix, reasons, replaced, rule, rung, seq });
    pinOn = a.accountId;
  });
  const last = r.attempts[r.attempts.length - 1];
  const after = attemptEnd(last);
  const tried = r.attempts.map((x) => x.accountId);
  if (r.errorCode === 'accounts_at_cap') ranOut(seq + 1, r.attempts.length + 1, after, tried);
  // A 429 or a refused model fails over, so a request whose last attempt answered one found nothing left at its
  // reselect and the router answered it.
  else if (failsOver(last)) specs.push({ ...base, accountId: null, at: after, attempt: r.attempts.length + 1, attempted: tried, holds: [...holds], kind: 'exhausted', pin: noPin, seq: seq + 1 });
  return specs;
}

const sessionId = (prefix: string) => REQUESTS.find((r) => r.sessionId?.startsWith(prefix))?.sessionId ?? prefix;

/**
 * The 11:00 round's five walked pins on secondary in its order: the cold one, then the warm ones by burn over their
 * prompt. Each holds Sonnet 4.5, the one model secondary serves, and burns its share of the 5-hour window per hour,
 * which sum to the account's burn. The first two cover the target under the cap and plan their moves to primary, where
 * they fit, and the walk stops before the current pin. The fourth is the laptop conversation's, whose renewal of
 * 10:39:30 stamped the window and whose last turn left its pin on secondary. The fifth, the subagent `f2c8`'s, which
 * its first turn placed on secondary at 10:59:40, reads warm under its fresh five-minute cache and holds no prompt size
 * and a burn of zero while that turn runs through the round. The round is an earlier build's record, which priced
 * Sonnet 4.5 through its fallback weight, so its burns and marks stand as it stored them. That build walked a pin with
 * a burn of zero where a later one leaves it out, and its walk stopped at the met target ahead of the fifth pin, so
 * that pin reads no skip word. It wrote `cheapest` on the pick's mark, the second pin's.
 */
const ROUND_PINS: RoundPin[] = [
  { agentId: 'main', burn: 0.0034, cold: true, keyId: 'desktop', lastInputTokens: 22_000, marked: true, model: 'claude-sonnet-4-5', note: null, sessionId: sessionId('0d588de5'), skip: null },
  { agentId: 'e90b', burn: 0.0142, cold: false, keyId: 'desktop', lastInputTokens: 88_000, marked: true, model: 'claude-sonnet-4-5', note: 'cheapest', sessionId: sessionId('c64358b5'), skip: null },
  { agentId: 'e90b', burn: 0.0052, cold: false, keyId: 'desktop', lastInputTokens: 41_000, marked: false, model: 'claude-sonnet-4-5', note: 'current', sessionId: sessionId('407dd39b'), skip: null },
  { agentId: 'main', burn: 0.0071, cold: false, keyId: 'laptop', lastInputTokens: 64_000, marked: false, model: 'claude-sonnet-4-5', note: null, sessionId: CLIMB_SESSION_ID, skip: null },
  { agentId: 'f2c8', burn: 0, cold: false, keyId: 'desktop', lastInputTokens: 0, marked: false, model: 'claude-sonnet-4-5', note: null, sessionId: sessionId('407dd39b'), skip: null },
];

/**
 * The parked desktop session's Opus pin on primary, whose stored lane is fast since fast mode serves that key's Opus
 * turns.
 */
const parkedOpus = SESSIONS.find((x) => x.sessionId === SESSION_IDS[3] && x.keyId === 'desktop');

/**
 * The afternoon round's pins on primary, each pin of all that a request row holds there at 14:12:30, cold first. The
 * three Sonnet 4.5 pins, the cold one of the failover at 14:02:11 and the two that the spent move of 14:05:12 and the
 * failover of 14:07:51 brought over from secondary, hold a model with no weight of its own, so the round prices no burn
 * for them and skips each as unpriced rather than shed a session on a guess. The two priced pins share the climb
 * primary's heads draw on its 5-hour window, 0.62 in the 2 h 46 min since the window opened at 11:44, which is
 * primary's own 5-hour burn: the current pin and the laptop session's Haiku 4.5 pin, placed at 14:09:41. No sibling
 * has runway for either, since nightly projects past the ceiling, secondary is spent and cooling, and the Overflow
 * workspace is paused. Two pins leave the round before the order: the parked session's Opus pin, which its turn of
 * 14:06 wrote, rides the fast lane, so it holds no share of a plan window, and the pin of the session's main agent
 * that the mod released after its one turn at 14:03 belongs to an agent that ended.
 */
const NO_RUNWAY_PINS: RoundPin[] = [
  { agentId: 'main', burn: null, cold: true, keyId: 'desktop', lastInputTokens: 43_000, marked: false, model: 'claude-sonnet-4-5', note: null, sessionId: sessionId('bcc805b6'), skip: 'unpriced' },
  { agentId: 'main', burn: 0.212, cold: false, keyId: 'desktop', lastInputTokens: 52_300, marked: false, model: 'claude-sonnet-5-5', note: 'current', sessionId: sessionId('d7167642'), skip: 'no_runway' },
  { agentId: 'main', burn: 0.012, cold: false, keyId: 'laptop', lastInputTokens: 40_800, marked: false, model: HAIKU, note: null, sessionId: sessionId('4ef130f1'), skip: 'no_runway' },
  { agentId: 'main', burn: null, cold: false, keyId: 'desktop', lastInputTokens: 47_100, marked: false, model: 'claude-sonnet-4-5', note: null, sessionId: sessionId('0d588de5'), skip: 'unpriced' },
  { agentId: 'main', burn: null, cold: false, keyId: 'desktop', lastInputTokens: 56_700, marked: false, model: 'claude-sonnet-4-5', note: null, sessionId: 'd6f1a3c8-2e94-4b07-8a5d-3c9e7f2b1a64', skip: 'unpriced' },
  { agentId: 'main', burn: null, cold: false, keyId: 'desktop', lastInputTokens: parkedOpus?.lastInputTokens ?? 0, marked: false, model: parkedOpus?.model ?? 'claude-opus-5', note: null, sessionId: SESSION_IDS[3], skip: 'fast_lane' },
  { agentId: 'main', burn: null, cold: false, keyId: 'desktop', lastInputTokens: 18_100, marked: false, model: 'claude-sonnet-5', note: null, sessionId: RELEASED_SESSION_ID, skip: 'ended' },
];

/**
 * Studio's round fired by the stretch, its six pins in the round's order: a cold pin first, then the warm ones by their
 * demand over the size of their move, each burn the pin's rate over the account's speed, the six summing to its demand.
 * A move's size is its prompt, but the first marked pin's move drops the 480 thinking tokens of its turn on studio: it
 * rewrites 29,520 tokens and weighs the drop at four prompt tokens apiece, 31,440 in all, so its 0.028 still leads the
 * next pin's 0.027 over 40,000. The target is more than any one pin's burn, so no pin covers it at the cold pin, whose
 * burn is under a tenth of the target, and that pin stays warm as a sliver. The order marks the first warm pin, and the
 * rest is less than each pin left, so the pick marks the covering pin whose move costs least, the cost the pin's own
 * Pro share of its rewrite and the same on every destination, and the walk ends there. The current pin reads the
 * prompt its turn of 14:03:36 stored, since the slowed request's own answer lands after the round.
 */
const STRETCH_PINS: RoundPin[] = [
  { agentId: '3c9a', burn: 0.002, cold: true, keyId: 'notebook', lastInputTokens: 26_000, marked: false, model: HAIKU, note: null, sessionId: OPENING_SESSION_IDS[1], skip: 'sliver' },
  { agentId: 'main', burn: 0.028, cold: false, keyId: 'notebook', lastInputTokens: 30_000, marked: true, model: 'claude-sonnet-5-5', note: null, sessionId: MARKED_SESSION_IDS[1], skip: null },
  { agentId: 'main', burn: 0.027, cold: false, keyId: 'notebook', lastInputTokens: 40_000, marked: true, model: 'claude-sonnet-5-5', note: 'cheapest', sessionId: MARKED_SESSION_IDS[0], skip: null },
  { agentId: '7b2e', burn: 0.026, cold: false, keyId: 'notebook', lastInputTokens: 61_000, marked: false, model: HAIKU, note: null, sessionId: SLOWED_SESSION_ID, skip: null },
  { agentId: 'main', burn: 0.0206, cold: false, keyId: 'notebook', lastInputTokens: 52_000, marked: false, model: 'claude-sonnet-5-5', note: null, sessionId: OPENING_SESSION_IDS[1], skip: null },
  { agentId: 'main', burn: 0.025, cold: false, keyId: 'notebook', lastInputTokens: 69_000, marked: false, model: 'claude-sonnet-5-5', note: 'current', sessionId: SLOWED_SESSION_ID, skip: null },
];

/** The 7-day Fable window's burn on nightly at the round, all the Fable session's, since no other pin draws it. */
const FABLE_WEEK_BURN = 0.03;

/**
 * Nightly's Fable-week round, its two pins as the round lists them: the Fable session's pin, the current one, whose
 * renewal runs standard, so it walks the order on the 7-day Fable window with the window's whole burn, and as the one
 * pin walked it is the last pin, which the engine never marks. Then the laptop conversation's Opus pin, which its turn
 * of 08:19:05 wrote and whose model draws no 7-day Fable window, so it leaves the round before the order with no burn.
 * The round marks nothing for the last pin.
 */
const FABLE_WEEK_PINS: RoundPin[] = [
  { agentId: 'main', burn: FABLE_WEEK_BURN, cold: false, keyId: 'desktop', lastInputTokens: 81_000, marked: false, model: 'claude-fable-5-1', note: 'current', sessionId: sessionId('89cd1c92'), skip: 'last_pin' },
  { agentId: 'main', burn: null, cold: false, keyId: 'laptop', lastInputTokens: 61_000, marked: false, model: 'claude-opus-5', note: null, sessionId: OPUS_MORNING_SESSION_ID, skip: 'off_window' },
];

/** The 7-day Fable window's burn on nightly through the Fable conversation's morning, all that conversation's. */
const FABLE_MORNING_BURN = 0.033;
/** Secondary's 5-hour burn at the two renewals that read its window over all's ceiling ahead of the 11:00 round. */
const SECONDARY_CLIMB_BURN = 0.09;
/**
 * Primary's 5-hour burn at the renewal that stamps its window ahead of the afternoon round: the earlier sessions' and
 * the purged conversation's pin, whose pace the sweep of 13:32:40 takes with it.
 */
const PRIMARY_STAMP_BURN = 0.115;

/**
 * Primary's Fable reservation round, its pins as the round lists them. The round walks the pins whose model draws no
 * Fable week on the 7-day window: a cold Sonnet 5.5 pin, whose 0.0016 an hour covers the cap's half of the walked burn,
 * and the current pin, whose Sonnet 4.5 has no weight of its own and so no priced burn, which lifts the guard. The
 * router's walk tests a met target ahead of a pin with no priced burn, so the current pin behind the pick reads no skip
 * word. The Fable pin leaves the round before the order with no figure, though its 0.008 an hour on the Fable week
 * spends the room the reservation holds. Each pin stands on a row: the cold pin on its session's one turn of 11:05:10,
 * the current pin on the move of 11:08:30 and the Fable pin on its conversation's renewal of 11:02:40.
 */
const RESERVATION_PINS: RoundPin[] = [
  { agentId: 'main', burn: 0.0016, cold: true, keyId: 'desktop', lastInputTokens: 36_000, marked: true, model: 'claude-sonnet-5-5', note: 'cheapest', sessionId: RESERVATION_MARKED_SESSION_ID, skip: null },
  { agentId: 'e90b', burn: null, cold: false, keyId: 'desktop', lastInputTokens: 88_000, marked: false, model: 'claude-sonnet-4-5', note: 'current', sessionId: sessionId('c64358b5'), skip: null },
  { agentId: 'main', burn: null, cold: false, keyId: 'desktop', lastInputTokens: 52_000, marked: false, model: 'claude-fable-5-1', note: null, sessionId: RESERVATION_FABLE_SESSION_ID, skip: 'fable_model' },
];

/**
 * The last round's pins on primary, each pin of all that a request row holds there at 14:29:00, in the router's order:
 * the cold pins by burn, then the warm ones by burn over their prompt, a pin with no priced burn last in its group. The
 * priced pins share primary's 5-hour burn of 0.224 an hour by the 5-hour shares their turns took in the half hour of
 * all's burn average. The two Sonnet 4.5 pins, the failover session's main and the laptop session's main whose 502 and
 * retry ran at 14:22, hold a model with no weight of its own, so the round skips each as unpriced. No sibling has
 * runway for a priced pin, since nightly projects past the ceiling, secondary is spent and cooling, and the Overflow
 * workspace is paused. Three pins leave the round before the order, each of a subagent whose last turn ended: `e90b`'s
 * of 14:21:12 and the two of the session the mod released whole. The record lists them last, in key order.
 */
const MOVED_PIN_ROUND_PINS: RoundPin[] = [
  { agentId: 'main', burn: 0.1165, cold: true, keyId: 'desktop', lastInputTokens: 57_800, marked: false, model: 'claude-fable-5-1', note: null, sessionId: sessionId('4ef130f1'), skip: 'no_runway' },
  { agentId: 'main', burn: null, cold: true, keyId: 'desktop', lastInputTokens: 63_000, marked: false, model: 'claude-sonnet-4-5', note: null, sessionId: sessionId('bcc805b6'), skip: 'unpriced' },
  { agentId: 'main', burn: null, cold: true, keyId: 'laptop', lastInputTokens: 3_200, marked: false, model: 'claude-sonnet-4-5', note: null, sessionId: sessionId('7d3e1c20'), skip: 'unpriced' },
  { agentId: '6c2b', burn: 0.0301, cold: false, keyId: 'desktop', lastInputTokens: 19_600, marked: false, model: 'claude-sonnet-5', note: 'current', sessionId: sessionId('bcc805b6'), skip: 'no_runway' },
  { agentId: 'main', burn: 0.0774, cold: false, keyId: 'desktop', lastInputTokens: 54_100, marked: false, model: 'claude-sonnet-5-5', note: null, sessionId: sessionId('d7167642'), skip: 'no_runway' },
  { agentId: '2f9d', burn: null, cold: false, keyId: 'desktop', lastInputTokens: 12_600, marked: false, model: 'claude-sonnet-5', note: null, sessionId: RELEASED_SESSION_ID, skip: 'ended' },
  { agentId: '8c1a', burn: null, cold: true, keyId: 'desktop', lastInputTokens: 14_100, marked: false, model: 'claude-sonnet-5', note: null, sessionId: RELEASED_SESSION_ID, skip: 'ended' },
  { agentId: 'e90b', burn: null, cold: true, keyId: 'desktop', lastInputTokens: 19_600, marked: false, model: 'claude-sonnet-5-5', note: null, sessionId: sessionId('89cd1c92'), skip: 'ended' },
];

const roundOfPins = (pins: RoundPin[]): ShedRound => ({ marks: pins.filter((p) => p.marked).map((p) => ({ agentId: p.agentId, keyId: p.keyId, model: p.model, sessionId: p.sessionId })), pins });

/**
 * A refusal at admission (a guardrail's block, a client's drift or contract) comes before any selection, so its record
 * leaves the parent agent, the lane, the chains, the accounts attempted and the holds empty. The block and the contract
 * keep the class and the count and probe flags the request carried, the drift keeps none of them, and each writes the
 * marker flag false whatever the body, as the relay's three refusal records do. A key refusal is the selection's own
 * and keeps the request's facts.
 */
const refusedFacts = (r: RequestLog, drift: boolean): Pick<RecordRequest, 'attempted' | 'chain' | 'flags' | 'holds' | 'lane' | 'parentAgentId' | 'requestClass'> => ({
  attempted: [],
  chain: { boundaries: 0, staticBoundaries: 0, staticHash: false },
  flags: { contextCompacted: false, count: !drift && r.kind === 'count_tokens', fast: false, marker: false, portable: false, probe: !drift && r.kind === 'probe' },
  holds: [],
  lane: { effort: '', speed: '', thinking: '' },
  parentAgentId: null,
  requestClass: drift ? null : r.requestClass,
});

/**
 * A guardrail's block as the relay answers it: the stored name of the guardrail the request's matches mark blocking.
 */
function blockOf(id: string): NonNullable<Spec['refusal']> {
  const blocking = requestById.get(id)?.guardrails.find((m) => m.blocked)?.id ?? '';
  return { code: 'guardrail_blocked', kind: 'guardrail', reason: GUARDRAILS.find((g) => g.id === blocking)?.name ?? blocking };
}

/**
 * The story hour and the morning's round, written by hand so every card the routing log draws is on the page: each
 * decision names its kind, its account and what the engine read where the walk back alone would not tell it.
 */
function storySpecs(): Spec[] {
  const t = clock;
  const withReading = (windows: AttemptWindow[], id: string, utilization: number) => windows.map((w) => (w.id === id ? { ...w, utilization } : w));
  const at =(id: string, seq: number, attempt: number, when: number, kind: DecisionKind, accountId: string | null, extra: Partial<Spec> = {}): Spec => ({ accountId, at: when, attempt, attempted: [], kind, pin: 'none', request: requestById.get(id)!, seq, story: true, ...extra });
  const live = (cause = 'continues_pin', idleMs = 60_000, warmth: 'cold' | 'refresh' | 'untouched' = 'refresh') => ({ cause, idleMs, warmth });
  const mainStatic = (when: number, writtenAt: number, refusedBy: string | null = null) => entry(staticBoundary('main', 2), { at: when, depth: 2, prefixTokens: 31_200, refusedBy, ttlMs: HOUR, writtenAt });
  const cohort = (when: number, refusedBy: string | null = null) => entry(staticBoundary('e90b', 1), { at: when, depth: 1, prefixTokens: 18_400, refusedBy, ttlMs: FIVE_MINUTES, writtenAt: t(14, 19, 52) });
  const message = (session: string, depth: number, prefixTokens: number, when: number, writtenAt: number, ttlMs: number) => entry(`message|${sessionId(session)}|${depth}`, { at: when, depth, prefixTokens, ttlMs, writtenAt });
  const placedStatic: Placement = { depth: 2, prefixTokens: 31_200, rung: 'static' };
  // Secondary's sessions burn its 5-hour window at their sum while the round runs and its marks are pending.
  const roundBurn = { secondary: { five_hour: round5(ROUND_PINS.reduce((n, p) => n + (p.burn ?? 0), 0)) } };
  // Secondary's 5-hour burn while the laptop conversation works there, ahead of the round's sessions.
  const climbBurn = { secondary: { five_hour: SECONDARY_CLIMB_BURN } };
  // Primary's burns while its Fable pin draws the Fable week: the 7-day burn is the walked pin's and the Fable pin's
  // share of that window, and the 5-hour burn the sessions' before the afternoon.
  const reservationBurns = { primary: { five_hour: PRIMARY_EARLIER_BURN, seven_day: 0.0056, seven_day_fable: RESERVATION_FABLE_BURN } };
  // Nightly's burns through the Fable conversation's morning, the 7-day Fable window's all that conversation's. The
  // window reads 0.62 through the hour, so the burn takes its projection at the five-hour bound to 0.785.
  const morningBurns = { nightly: { five_hour: 0.03, seven_day_fable: FABLE_MORNING_BURN } };
  const fableWeekBurns = { nightly: { five_hour: 0.03, seven_day_fable: FABLE_WEEK_BURN } };
  // The Overflow workspace sits at its in-flight cap through the round and the move. With a free slot an API-key
  // sibling fits with no end to its runway, the longest of all, so the shed rule would plan and move both marks there.
  const overflowFull = { overflow: 16 };
  // Primary and nightly sit at their caps while the hour's first sessions land on secondary. Each lands its rewrite,
  // and a free one would take a fresh session on the ladder ahead of secondary, which falls short of the ceiling.
  const secondaryOnly = { nightly: 16, primary: 16 };
  const nightlyOnly = { overflow: 16, primary: 16, secondary: 16 };
  // Nightly's last minute before its spent window resets, as the reset hold reads it.
  const spentNightly = (when: number) => withReading(oauthWindows('nightly', when, () => OLDER_FACTOR), '5h', 1);
  // Studio's 5-hour reading where a hold reads it before any answer's head: the utilization the slow-down read.
  const studioAt = (when: number, utilization: number) => ({ studio: withReading(oauthWindows('studio', when, () => OLDER_FACTOR), '5h', utilization) });
  // Nightly's 7-day Fable window where the morning's Fable session runs it ahead of the walk back's even climb.
  const nightlyAt = (when: number, utilization: number) => ({ nightly: withReading(oauthWindows('nightly', when, () => OLDER_FACTOR), '7d_fable', utilization) });
  // A pin's pace after one more gap. A gap at or over the work pool's burn average is a pause that folds nothing, and
  // any other moves the cycle toward the gap by one over the number of gaps folded with it, capped at eight, so the
  // cycle reads about the mean of the pin's last eight gaps.
  const averageMs = poolById.get('work')!.burnAverageSeconds * 1_000;
  const foldCycle = (pace: { cycleMs: number; gaps: number }, gapMs: number) => {
    if (gapMs >= averageMs) return pace;
    const gaps = Math.min(pace.gaps + 1, 8);
    return { cycleMs: Math.round(pace.cycleMs + (gapMs - pace.cycleMs) / gaps), gaps };
  };
  // Studio's morning pin holds one gap at 10:12:36, from its turn of 10:11:36 to 10:12: the placement before it and
  // the renewal that carries the hold fold nothing.
  const morningPace = { cycleMs: 24_000, gaps: 1 };
  // The tie's failover picks before nightly's overage, where primary and nightly share the plan tier. Each account's
  // sessions burn its 7-day window at what leaves both one spare rate, and the 5-hour burns leave both fitting a fresh
  // session, primary to the longer runway.
  const tieAt = t(8, 16, 21);
  const spareOf = (id: string): number => {
    const week = oauthWindows(id, tieAt, () => OLDER_FACTOR).find((w) => w.id === '7d');
    return leftPerHour(week?.utilization ?? 0, paceDeadline(accountById.get(id)!, week?.resetsAt ? Date.parse(week.resetsAt) : null, tieAt), tieAt);
  };
  const TIE_SCORE = 0.003;
  const tieBurns = { nightly: { five_hour: 0.02, seven_day: spareOf('nightly') - TIE_SCORE }, primary: { five_hour: 0.03, seven_day: spareOf('primary') - TIE_SCORE } };
  const overflowStatic = (when: number) => entry(staticBoundary('main', 2), { at: when, depth: 2, prefixTokens: 31_200, ttlMs: FIVE_MINUTES, writtenAt: when - 2 * MINUTE });
  return [
    // Two of the round's sessions placed on secondary just before it. Every pin a beat renews or moves is a request's.
    at('req_e5a1c093-4d7', 1, 1, t(10, 58, 59), 'placed', 'secondary'),
    at('req_f3b6d218-85a', 1, 1, t(10, 59, 10), 'placed', 'secondary'),
    at('req_0b7c2d94-e61', 1, 1, t(10, 59, 30), 'renewed', 'secondary', { pin: live('probe', 31_000, 'untouched') }),
    // The laptop conversation's two renewals on secondary while its 5-hour window is under the hold. The first reads
    // the projection over all's ceiling and under the wall, which stamps the window and names the hold alone. The
    // second reads it past the wall inside the wait, so its line names the stamp and the wait beside the age and the
    // hold.
    at('req_5f2c8b19-d73', 1, 1, t(10, 39, 30), 'renewed', 'secondary', { burns: climbBurn, pin: live('continues_pin', 590_000) }),
    at('req_b7d43a90-e26', 1, 1, t(10, 49, 10), 'renewed', 'secondary', { burns: climbBurn, pin: live('continues_pin', 580_000) }),
    // Secondary's 5-hour window opened at 09:55 and runs hot, so its projection to the reset reads past the wall, and
    // it has read over all's ceiling since the laptop conversation's renewal of 10:39:30, a quarter of the time to its
    // wall instant and more: this renewal runs the round, which marks two pins.
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
    // The desktop Fable conversation's morning on primary, whose week leaves a room of about 0.31 all morning. While
    // the conversation's pin draws the Fable week at 0.008 an hour, Pace holds back 0.32 of that room for it, so each
    // of its renewals on primary reads the room under the reservation: the one of 10:13:30 is the first, which stamps
    // the reservation's test, and it and the five after it name the reservation, the stamp and the hour's wait. The
    // conversation's landing and the marked session's one turn are placements, which run no shed rule.
    at('req_2d8f6b14-a79', 1, 1, t(10, 4), 'placed', 'primary'),
    at('req_8e1d5a36-c92', 1, 1, t(10, 13, 30), 'renewed', 'primary', { burns: reservationBurns, pin: live('continues_pin', 570_000) }),
    at('req_3f8c1d94-b57', 1, 1, t(10, 23, 20), 'renewed', 'primary', { burns: reservationBurns, pin: live('continues_pin', 590_000) }),
    at('req_a2c94e7b-f15', 1, 1, t(10, 33, 10), 'renewed', 'primary', { burns: reservationBurns, pin: live('continues_pin', 590_000) }),
    at('req_d06a4e73-c29', 1, 1, t(10, 43), 'renewed', 'primary', { burns: reservationBurns, pin: live('continues_pin', 590_000) }),
    at('req_71b5f2c8-e40', 1, 1, t(10, 52, 50), 'renewed', 'primary', { burns: reservationBurns, pin: live('continues_pin', 590_000) }),
    at('req_f15c7a28-d63', 1, 1, t(11, 2, 40), 'renewed', 'primary', { burns: reservationBurns, pin: live('continues_pin', 590_000) }),
    at('req_a96e3d52-b18', 1, 1, t(11, 5, 10), 'placed', 'primary'),
    // The moved session's next turn renews its pin on primary, whose row carries the source the move left. Primary's
    // 7-day room has read under the Fable reservation since the Fable conversation's renewal of 10:13:30, the hour of
    // the test's wait and ten seconds more, while every window reads under its line, so this renewal runs the
    // reservation's round. The 7-day burn is the walked pin's and the Fable pin's, its Fable week burn in the 7-day
    // window's share, and the 5-hour burn the sessions' before the afternoon. The Fable pin leaves with the sweep of
    // 11:14:40 and takes its demand with it, so every later selection reads a room that holds the reservation.
    at('req_5d1f8c3a-b27', 1, 1, t(11, 13, 40), 'renewed', 'primary', {
      burns: reservationBurns,
      pin: live('continues_pin', 310_000),
      round: roundOfPins(RESERVATION_PINS),
    }),
    // The hour's sessions placed on secondary before its spent edge.
    at('req_a1f04c2e-7b3', 1, 1, t(14, 0, 10), 'placed', 'secondary', { inFlight: secondaryOnly }),
    at('req_b72e9d05-c14', 1, 1, t(14, 1, 30), 'placed', 'secondary', { inFlight: secondaryOnly }),
    at('req_c90d3f61-a28', 1, 1, t(14, 1, 35), 'placed', 'secondary', { inFlight: secondaryOnly }),
    at('req_d4e8a7b2-916', 1, 1, t(14, 1, 40), 'placed', 'secondary', { inFlight: secondaryOnly }),
    // A token count follows its subagent's pin without walking the rules, to secondary before its window is spent.
    at('req_3b8e51f4-0a9', 1, 1, t(14, 1, 50), 'renewed', 'secondary', { pin: live('count', 20_000, 'untouched') }),
    at('req_1d9c3e55-0b2', 1, 1, t(14, 2, 10), 'renewed', 'secondary', { pin: live('continues_pin', 120_000) }),
    at('req_1d9c3e55-0b2', 2, 2, t(14, 2, 11), 'placed', 'primary', {
      attempted: ['secondary'],
      // The failover lands the long conversation on primary, where each of its eleven message boundaries is new, and
      // primary held the main static chain the rung placed on under another key, so the answer writes both of its
      // boundaries again under this one.
      cacheMap: { messages: 11, staticSizes: [26_800, 31_200] },
      failover: 'cooldown',
      from: 'secondary',
      pin: live(),
      prefix: { primary: { static: mainStatic(t(14, 2, 11), t(13, 58, 2)), staticMatch: mainStatic(t(14, 2, 11), t(13, 58, 2)) }, secondary: { content: message('bcc805b6', 8, 58_900, t(14, 2, 11), t(14, 0, 24), HOUR), contentMatch: null } },
      rung: placedStatic,
    }),
    at('req_7a2d4c18-e95', 1, 1, t(14, 3), 'placed', 'primary'),
    // Secondary reads its 5-hour window spent from 14:02:11 and cools for its retry-after alone, so the session's next
    // request finds it spent and free: the spent rule moves the pin to primary.
    at('req_da52d141-a5a', 1, 1, t(14, 5, 12), 'moved', 'primary', {
      from: 'secondary',
      pin: live('continues_pin', 217_000),
      prefix: { primary: { static: mainStatic(t(14, 5, 12), t(14, 2, 11)), staticMatch: mainStatic(t(14, 5, 12), t(14, 2, 11)) }, secondary: { content: message('0d588de5', 5, 44_600, t(14, 5, 12), t(14, 1, 47), HOUR), contentMatch: null } },
      rule: 'spent',
      rung: placedStatic,
    }),
    // The keepalive's probe warms its pin on secondary, whose spent window is no obstacle to a probe, so it renews
    // there with every move rule unrun. Its leg's 429 cools secondary, and the selection behind that answer meets the
    // cooldown on the pinned member, which the router answers the probe rather than moving the pin.
    at('req_c41d0311-200', 1, 1, t(14, 6, 30), 'renewed', 'secondary', { pin: live('probe', 290_000, 'untouched') }),
    at('req_c41d0311-200', 2, 2, t(14, 6, 31), 'exhausted', null, {
      attempted: ['secondary'],
      exhaustion: { deadline: requestById.get('req_c41d0311-200')!.attempts[0].cooldownSet!.until, kind: 'pinned', obstacle: 'cooldown', reason: null, window: null },
      from: 'secondary',
      pin: live('probe', 1_000, 'untouched'),
    }),
    at('req_ef099b34-453', 1, 1, t(14, 9, 41), 'placed', 'primary'),
    // The member placed by its static prefix holds a deeper entry the rung refused as expired beside the one it took.
    // A sibling's entry reads the gate that refused it.
    at('req_b2bed817-a58', 1, 1, t(14, 11, 44), 'placed', 'primary', {
      prefix: {
        nightly: { static: mainStatic(t(14, 11, 44), t(13, 30), 'band'), staticMatch: null },
        primary: { static: entry(staticBoundary('main', 3), { at: t(14, 11, 44), depth: 3, prefixTokens: 33_900, refusedBy: 'expired', ttlMs: HOUR, writtenAt: t(13, 5, 10) }), staticMatch: mainStatic(t(14, 11, 44), t(14, 2, 11)) },
      },
      rung: placedStatic,
    }),
    at('req_6c3a9e27-5b1', 1, 1, t(14, 18, 5), 'refused', null, { pin: 'not_looked_up', refusal: { code: 'revoked', kind: 'key', reason: 'the key was revoked' } }),
    at('req_e0a4c6f2-118', 1, 1, t(14, 14, 2), 'refused', null, { pin: 'not_looked_up', refusal: { code: 'drift', kind: 'client', reason: 'hash' } }),
    at('req_f1c8e3a5-29d', 1, 1, t(14, 14, 20), 'refused', null, { pin: 'not_looked_up', refusal: { code: 'contract', kind: 'client', reason: 'class_header' } }),
    // The desktop key's own guardrail refused the day's last auxiliary fork on it.
    at('req_a8f3c6d0-e17', 1, 1, t(12, 52, 20), 'refused', null, { pin: 'not_looked_up', refusal: blockOf('req_a8f3c6d0-e17') }),
    at('req_51d0e2b8-7a0', 1, 1, t(14, 16, 40), 'exhausted', null, { exhaustion: { deadline: iso(startOfDay(t(14, 16, 40)) + DAY), kind: 'timed', obstacle: null, reason: null, window: 'overage' }, pin: 'pinning_off' }),
    // The router placed the laptop turn of the day before on secondary, and the relay rejected it there as it built
    // the body, so the placement sent no attempt and its completion reads no answer.
    at(UNAVAILABLE_TURN_ID, 1, 1, storyAt(UNAVAILABLE_TURN_ID, 'the laptop turn of the day before that the relay rejected as it built the body'), 'placed', 'secondary'),
    // The keepalive's probes of the session that failed over to primary renew its pin and walk past no rule.
    at('req_6d0b3f85-a14', 1, 1, t(14, 11, 40), 'renewed', 'primary', { pin: live('probe', 569_000, 'untouched') }),
    at('req_e4a92c17-b60', 1, 1, t(14, 20, 40), 'renewed', 'primary', { pin: live('probe', 540_000, 'untouched') }),
    // The compacted conversation renews its pin cold.
    at('req_31f0b7c2-6e1', 1, 1, t(14, 17, 52), 'renewed', 'primary', { pin: live('compacted', 491_000, 'cold') }),
    at('req_9a07c4e1-d55', 1, 1, t(14, 19, 2), 'renewed', 'primary', { pin: live('continues_pin', 438_000) }),
    // A subagent's first request under all, whose cache markers the router injects, so it carries one whatever its
    // body marked. Its session and agent hold no pin: the held selection finds none, its cohort's static entry places
    // it once a slot frees, and the placement writes its pin.
    at('req_69e30548-32e', 1, 1, t(14, 21, 12), 'held', 'primary', { hold: { cohort: 'primary', place: 'cohort' }, inFlight: { nightly: 16, primary: 16 }, prefix: { primary: { static: cohort(t(14, 21, 12), 'at_cap'), staticMatch: null } } }),
    at('req_69e30548-32e', 2, 1, t(14, 21, 14), 'placed', 'primary', {
      heldBefore: { cohort: 'primary', heldMs: 2_000, waitedOn: ['primary'], waits: [{ heldMs: 2_000, reason: 'cap' }] },
      holds: [{ accountId: 'primary', attempt: 1, cohort: 'primary', heldMs: 2_000, ranOut: false }],
      inFlight: { nightly: 16, primary: 15 },
      prefix: { primary: { static: cohort(t(14, 21, 14)), staticMatch: cohort(t(14, 21, 14)) } },
      rung: { depth: 1, prefixTokens: 18_400, rung: 'static' },
    }),
    // The router keyed the laptop turn by the root its content matched and placed it on primary, whose head arrived
    // before the relay found the stream past it undecodable.
    at('req_5e2f7a19-b03', 1, 1, t(14, 22, 48), 'placed', 'primary'),
    // The client's retry renews the pin the 502 placed, and primary's head goes out before the relay finds the stream
    // past part of the body undecodable.
    at('req_5e3c8a41-d27', 1, 1, t(14, 22, 53), 'renewed', 'primary', { pin: live('continues_pin', 5_000) }),
    at('req_a6c19e73-5d2', 1, 1, t(14, 23, 30), 'placed', 'primary'),
    at('req_2e7c90a1-6d4', 1, 1, t(14, 24, 9), 'placed', 'primary'),
    at('req_2e7c90a1-6d4', 2, 2, t(14, 24, 10), 'placed', 'nightly', { attempted: ['primary'], failover: 'cooldown', from: 'primary', pin: live() }),
    at('req_4b0e9d17-c31', 1, 1, t(14, 26, 58), 'placed', 'primary'),
    at('req_4b0e9d17-c31', 2, 2, t(14, 27, 1), 'placed', 'nightly', { attempted: ['primary'], failover: 'fast_lane', from: 'primary', pin: live() }),
    at('req_f8085653-422', 1, 1, t(14, 27, 28), 'renewed', 'primary', { pin: live('continues_pin', 506_000) }),
    // The failover session's subagent lands beside its main on primary. Its auxiliary call renews the pin there and
    // leaves its warmth, and the call's 429 fails it over to nightly, which moves the pin off the recipe's holder. The
    // probe the move overtook meets the pin on nightly and answers that standing with no attempt.
    at(MOVED_PIN_TURN_ID, 1, 1, t(14, 25), 'placed', 'primary'),
    at(MOVED_PIN_CALL_ID, 1, 1, storyAt(MOVED_PIN_CALL_ID, 'the auxiliary call whose renewal runs the last round on primary'), 'renewed', 'primary', { pin: live('continues_pin', 236_000, 'untouched'), round: roundOfPins(MOVED_PIN_ROUND_PINS) }),
    at(MOVED_PIN_CALL_ID, 2, 2, t(14, 29, 1), 'placed', 'nightly', { attempted: ['primary'], failover: 'cooldown', from: 'primary', pin: live() }),
    at(MOVED_PIN_PROBE_ID, 1, 1, MOVED_PIN_PROBE_DUE + 400, 'exhausted', null, {
      exhaustion: { deadline: null, kind: 'pinned', obstacle: 'pin_moved', reason: null, window: null },
      from: 'nightly',
      pin: live('probe', 300, 'untouched'),
    }),
    // The client's two classifier calls follow their judged turns to primary. A classifier call books nothing and holds
    // no pin.
    at('req_4d9a1e6b-c20', 1, 1, t(12, 41, 8), 'placed', 'primary', { pin: 'classifier' }),
    at('req_63a8d2f5-b17', 1, 1, t(12, 41, 10), 'placed', 'primary', { pin: 'classifier' }),
    // A conversation the Overflow workspace takes at 10:52:30, with primary full and secondary's window too far along
    // for its rewrite, returns to a plan with its 640k-token context, cold there nine minutes on. Primary fits but is
    // full, and the rest fall short of the ceiling, so the gate answers the least bad set and Pace takes nightly, whose
    // window the conversation spends within the hour.
    at('req_4e8b1d36-f72', 1, 1, t(10, 52, 30), 'placed', 'overflow', { inFlight: { primary: 16 } }),
    at(FORCED_MOVE_ID, 1, 1, t(11, 1, 50), 'moved', 'nightly', {
      from: 'overflow',
      inFlight: { primary: 16 },
      pin: live('continues_pin', 0, 'cold'),
      pinFigures: { burn: { five_hour: 0.21, seven_day: 0.025 }, tokens: 640_000 },
      rule: 'return_to_plan',
    }),
    // The conversation's next request a second later finds the move's head still out, so it waits on nightly for that
    // head rather than write the 640k-token prefix a second time, bounded by the pool's hold cap from the move, and
    // renews a second later, when the head comes.
    at(REWRITE_HELD_ID, 1, 1, t(11, 1, 51), 'held', 'nightly', {
      hold: { cohort: null, place: 'rewrite', until: t(11, 3, 50), waitsFor: FORCED_MOVE_ID },
      pin: live('continues_pin', 1_000),
    }),
    at(REWRITE_HELD_ID, 2, 1, t(11, 1, 52), 'renewed', 'nightly', {
      heldBefore: { cohort: null, heldMs: 1_000, waitedOn: ['nightly'], waits: [{ heldMs: 1_000, reason: 'rewrite' }] },
      holds: [{ accountId: 'nightly', attempt: 1, cohort: null, heldMs: 1_000, ranOut: false }],
      pin: live('continues_pin', 2_000),
    }),
    // A new conversation lands on nightly with primary and secondary full, as its renewal at 13:11 finds them, and
    // nightly's spent window answers 429 40 s before it resets: the reselect finds the pin on a spent account whose
    // reset falls inside the retry bound, so the spent rule holds the request there for the reset, and it renews on the
    // fresh window, where the overage rule finds primary and secondary full.
    at('req_9d4c1a73-e58', 1, 1, t(13, 10, 20), 'placed', 'nightly', { inFlight: { primary: 16, secondary: 16 } }),
    at('req_9d4c1a73-e58', 2, 2, t(13, 10, 21), 'held', 'nightly', {
      attempted: ['nightly'],
      hold: { cohort: null, place: 'reset', until: t(13, 11) },
      pin: live('continues_pin', 1_000),
      readings: { nightly: spentNightly(t(13, 10, 21)) },
    }),
    at('req_9d4c1a73-e58', 3, 2, t(13, 11), 'renewed', 'nightly', { holds: [{ accountId: 'nightly', attempt: 2, cohort: null, heldMs: 39_000, ranOut: false }], inFlight: { primary: 16, secondary: 16 }, pin: live('continues_pin', 40_000) }),
    // A Fable conversation works on nightly through the hour ahead of the Fable session. Its first turn lands there
    // while its three siblings sit at their caps: nightly's old 5-hour window reads past all's ceiling, so primary and
    // the Overflow workspace would take the turn on the ladder, and secondary, which reads past the ceiling too, on
    // its Pace score. Its renewal of 07:29:30 is the first selection that reads the 7-day Fable window over all's
    // weekly ceiling at the five-hour bound, which stamps the window, and each renewal after it reads the window over
    // inside the hour's wait and names the stamp and the wait.
    at('req_9c4a7e12-f08', 1, 1, t(7, 20), 'placed', 'nightly', { inFlight: nightlyOnly }),
    at('req_4b9e2c71-a35', 1, 1, t(7, 29, 30), 'renewed', 'nightly', { burns: morningBurns, pin: live('continues_pin', 570_000) }),
    at('req_d27f5a83-e14', 1, 1, t(7, 39, 20), 'renewed', 'nightly', { burns: morningBurns, pin: live('continues_pin', 590_000) }),
    at('req_6a1c9e47-b90', 1, 1, t(7, 49, 10), 'renewed', 'nightly', { burns: morningBurns, pin: live('continues_pin', 590_000) }),
    at('req_e83b2d65-c57', 1, 1, t(7, 59), 'renewed', 'nightly', { burns: morningBurns, pin: live('continues_pin', 590_000) }),
    at('req_c8f2a61d-b74', 1, 1, t(8, 8, 50), 'renewed', 'nightly', { burns: morningBurns, pin: live('continues_pin', 590_000) }),
    // The Fable session's first turn places it on nightly, and its next turn's renewal runs nightly's Fable-week round,
    // an hour and 30 seconds behind the stamp of 07:29:30, which is the cap of the window's wait, and before the
    // overage begins at the first head after 11:30:12: from then on the pool's overage rule fires on the
    // account's pins ahead of the shed rule. The 5-hour window the story spends at 13:10:21 opened at 08:11, so it is
    // 19 minutes old at the round, under all's hold of an hour, while its burn still carries the previous window's, and
    // its projection reads under the wall at its reset besides: the 7-day Fable window fires in its place, over all's
    // ceiling at the five-hour bound. The laptop conversation's Opus pin draws no 7-day Fable window, so the round
    // lists it and counts nothing of it. The burns replace the account's, so the 5-hour burn stands beside the 7-day
    // Fable window's.
    at('req_3e2b9d47-c18', 1, 1, t(8, 21, 40), 'placed', 'nightly'),
    at('req_3e7a5c19-f84', 1, 1, t(8, 30), 'renewed', 'nightly', {
      burns: fableWeekBurns,
      pin: live('continues_pin', 500_000),
      readings: nightlyAt(t(8, 30), 0.65),
      round: roundOfPins(FABLE_WEEK_PINS),
    }),
    // The laptop conversation's next three turns, inside the round's dwell. The first lands on nightly with its three
    // siblings at their caps. Both renewals read nightly's Fable week at 0.65. At the first the Fable session's pin
    // stands past its window ahead of the sweep and burns the week as at the round's renewal, so the week projects 0.80
    // over all's weekly ceiling of 0.78 and the round's trigger reads it. The sweep takes that pin ahead of the second,
    // so nothing burns the week there.
    at('req_5a8e3c07-d19', 1, 1, t(8, 35), 'placed', 'nightly', { inFlight: nightlyOnly }),
    at('req_b4d29e61-a73', 1, 1, t(8, 40, 20), 'renewed', 'nightly', {
      burns: fableWeekBurns,
      pin: live('continues_pin', 320_000),
      readings: nightlyAt(t(8, 40, 20), 0.65),
    }),
    at('req_e61f0b48-c25', 1, 1, t(8, 41, 30), 'renewed', 'nightly', {
      burns: { nightly: { five_hour: 0.03 } },
      pin: live('continues_pin', 70_000),
      readings: nightlyAt(t(8, 41, 30), 0.65),
    }),
    // A fresh session the Overflow workspace's static prefix places there answers 429, and its failover ties primary
    // and nightly on Pace with both fitting: the runway keeps primary, so the pick leaves no draw.
    at(TIE_STORY_ID, 1, 1, t(8, 16, 20), 'placed', 'overflow', { burns: tieBurns, prefix: { overflow: { static: overflowStatic(t(8, 16, 20)), staticMatch: overflowStatic(t(8, 16, 20)) } }, rung: { depth: 2, prefixTokens: 31_200, rung: 'static' } }),
    at(TIE_STORY_ID, 2, 2, tieAt, 'placed', 'primary', { attempted: ['overflow'], burns: tieBurns, failover: 'cooldown', from: 'overflow', pin: live() }),
    // A fresh session placed by the spare rate: secondary falls short of the ceiling, so the no-fit partition passes it
    // over, and its sessions' burn outruns its pace, so its spare rate reads negative.
    at('req_2c7d9f15-b63', 1, 1, t(13, 45), 'placed', 'primary', { burns: { secondary: { five_hour: 0.032, seven_day: 0.004 } } }),
    // A fresh placement that books its rewrite on primary until the answer's usage replaces it.
    at('req_7a3f0c26-d81', 1, 1, t(13, 41, 15), 'placed', 'primary'),
    // The held conversation renews its pin on primary while the purged conversation's pin still stands there, a minute
    // ahead of the sweep that takes that pin and its pace: primary's sessions burn 0.115 an hour, its 5-hour window
    // projects over all's ceiling and under the wall at the 16:44 reset, and the reading stamps the window. The
    // conversation's next turn finds its pin swept and lands on primary again.
    at('req_c3a71e58-b46', 1, 1, t(13, 31, 30), 'renewed', 'primary', { burns: { primary: { five_hour: PRIMARY_STAMP_BURN, seven_day: 0.006 } }, pin: live('continues_pin', 591_000) }),
    at('req_7e2b9f40-c81', 1, 1, t(13, 52, 30), 'placed', 'primary'),
    // Primary's sessions burn its 5-hour window at 0.224 an hour from its reading of 0.55, so it projects past the wall
    // at the 16:44 reset. The window has read over all's ceiling since the held conversation's renewal of 13:31:30, a
    // quarter of the time from there to its wall instant of 16:13:02 and 37 seconds more, and no selection on primary
    // read it in between, so this renewal runs a round, which finds no sibling with runway.
    at('req_6e1f3b98-a24', 1, 1, t(14, 12, 30), 'renewed', 'primary', { pin: live('continues_pin', 46_000), round: roundOfPins(NO_RUNWAY_PINS) }),
    // A session placed on secondary before its spent edge. Its next turn finds the spent rule's one destination full,
    // so it renews on secondary, whose 429 on the spent 5-hour window fails it over to primary once a slot frees.
    at('req_d2a8f6c1-d50', 1, 1, t(14, 1, 45), 'placed', 'secondary', { inFlight: secondaryOnly }),
    at('req_b8c3e9d4-e61', 1, 1, t(14, 7, 50), 'renewed', 'secondary', { inFlight: { primary: 16 }, pin: live('continues_pin', 365_000) }),
    at('req_b8c3e9d4-e61', 2, 2, t(14, 7, 51), 'placed', 'primary', { attempted: ['secondary'], failover: 'cooldown', from: 'secondary', pin: live() }),
    // Its next turn renews the pin the landing wrote on primary, whose row carries the source the failover left.
    at('req_e2c7a4f9-d13', 1, 1, t(14, 13, 5), 'renewed', 'primary', { pin: live('continues_pin', 314_000) }),
    // Studio's closed span in work: a placement reads no entry, so the session placed at 09:11:24 opens the span at
    // its first turn after the placement, which reads the factor at 0.91 in the last 77 minutes of the window that
    // resets at 10:29.
    at('req_5c7e2a9d-b14', 1, 1, t(9, 11, 24), 'placed', 'studio'),
    at('req_a3d8f1b6-c25', 1, 1, t(9, 12), 'renewed', 'studio', { pin: live('continues_pin', 36_000) }),
    // The morning session's first turn after its placement sets its pin's dispatch, and the next one, 24 s later,
    // gives the pin its cycle: at 0.40 it waits 36 s, and the renewal after the wait reads 0.35 and runs no round,
    // since the stretch reads under the threshold. Its turns after the reset read 1, and the second, past the wait,
    // ends the span. The first turn after the hold comes past the pool's burn average, a pause that folds nothing, and
    // the next folds its gap into the cycle.
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
    at('req_f6a1d3e8-e47', 1, 1, t(10, 38, 20), 'renewed', 'studio', { pin: live('continues_pin', 1_544_000), pinPace: { cycleMs: foldCycle(morningPace, t(10, 38, 20) - t(10, 12, 36)).cycleMs, slowDownUntil: null } }),
    at('req_8d4b7f2c-f58', 1, 1, t(10, 40, 36), 'renewed', 'studio', {
      pin: live('continues_pin', 136_000),
      pinPace: { cycleMs: foldCycle(foldCycle(morningPace, t(10, 38, 20) - t(10, 12, 36)), t(10, 40, 36) - t(10, 38, 20)).cycleMs, slowDownUntil: null },
    }),
    // Studio's open span: a session placed at 13:47:24 opens it at its first turn after the placement, under the
    // threshold at 0.73, and the sessions placed after it are the round's other four pins.
    at('req_b1e7c4a9-a69', 1, 1, t(13, 47, 24), 'placed', 'studio'),
    at('req_4f2a8d6e-b7a', 1, 1, t(13, 48), 'renewed', 'studio', { pin: live('continues_pin', 36_000) }),
    // The annex stands at its cap from 13:50:30 to 13:59:05, so the slowed studio, which a fresh session no longer
    // fits, stays the engine's pick for the four sessions the stretch's round reads at 14:05.
    at('req_6b3e8d1a-c73', 1, 1, t(13, 50, 30), 'placed', 'studio', { inFlight: { annex: 16 } }),
    at('req_2f9a6c4e-d84', 1, 1, t(13, 53, 10), 'placed', 'studio', { inFlight: { annex: 16 } }),
    at('req_c5d8e2b7-e95', 1, 1, t(13, 58, 40), 'placed', 'studio', { inFlight: { annex: 16 } }),
    at('req_7e4f1a9c-f16', 1, 1, t(13, 59, 5), 'placed', 'studio', { inFlight: { annex: 16 } }),
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
    // The account-bound session lands on studio inside the round's dwell, and its auxiliary call follows the bound pin
    // there, leaving its warmth, with no rule run and no slow-down read. The annex stands at its cap again at 14:08,
    // so the slowed studio, which a fresh session no longer fits, stays the engine's pick for the session.
    at('req_7c4a1e86-d39', 1, 1, t(14, 8), 'placed', 'studio', { inFlight: { annex: 16 } }),
    at(ACCOUNT_BOUND_CALL_ID, 1, 1, t(14, 12), 'renewed', 'studio', { pin: live('class_auxiliary', 234_000, 'untouched') }),
  ];
}

/** The members a decision's selection passed over, out under an obstacle or ranked by the pick, none on a renewal. */
const skippedOf = (record: DecisionRecord) =>
  record.outcome.kind === 'renewed' ? [] : record.candidates.filter((c) => c.accountId !== record.outcome.accountId && (c.obstacle !== null || c.rank !== null)).map((c) => ({ accountId: c.accountId, obstacle: c.obstacle?.name ?? 'ranked' }));

/** A selection trace as the attempt row keeps it, read off the decision that dispatched the attempt. */
function traceOf(record: DecisionRecord, summary: DecisionSummary, held: CohortHold | null): RequestLog['attempts'][number]['selection'] {
  const pinEvent = record.outcome.pinEvents.find((e) => e.kind === 'pin_migrated');
  return {
    cacheRead: record.outcome.completion?.cacheReadTokens ?? null,
    cohortHold: held,
    moved: record.outcome.rule ?? pinEvent?.reason ?? null,
    placement: record.outcome.rung,
    renewed: record.outcome.kind === 'renewed',
    skipped: skippedOf(record),
    strategy: summary.strategy,
    tier: record.outcome.tier,
  };
}

/** Every decision of every request, built in time order so each pin's epoch and idle age read its last write. */
function buildDecisions() {
  const story = storySpecs();
  ROUNDS = story.flatMap((s) => (s.round ? [{ accountId: s.accountId!, at: s.at }] : []));
  const specs = [...story, ...REQUESTS.filter((r) => !STORY_IDS.has(r.id)).flatMap(derivedSpecs)].sort((a, b) => a.at - b.at || a.request.id.localeCompare(b.request.id) || a.seq - b.seq);
  const decisions = new Map<string, DecisionRecord>();
  const summaries: DecisionSummary[] = [];
  // The router writes the request's message boundary count on every record of it, and each message row an answer
  // writes is a boundary new to the account, so the count holds at least the most rows any of its answers writes.
  const messageRows = new Map<string, number>();
  for (const spec of specs) messageRows.set(spec.request.id, Math.max(messageRows.get(spec.request.id) ?? 1, spec.cacheMap?.messages ?? 1));
  for (const spec of specs) {
    noteSweeps(spec.at);
    // The router reads its live counters at both of a probe's selections, a second apart. It wrote the leg's record
    // ahead of the charge of the probe's own slot, and the leg's 429 dropped that slot again, so a pinned answer
    // carries the counts its leg's decision drew whole.
    const leg = pinnedAnswerOf(spec) ? decisions.get(`${spec.request.id}/${spec.seq - 1}`) : undefined;
    const counted = leg === undefined ? spec : { ...spec, inFlight: Object.fromEntries(leg.candidates.map((c) => [c.accountId, c.inFlight])) };
    const { record, summary } = build(counted, messageRows.get(spec.request.id) ?? 1);
    // The mock answers the record as it built it, so the card's copy is the record itself.
    const { raw: _, ...answered } = record;
    decisions.set(`${record.requestId}/${record.seq}`, { ...record, raw: answered });
    summaries.push(summary);
  }
  // The decision that dispatched a request's attempt, the last of the attempt's that is no hold, with its record.
  const dispatchingOf = (id: string, attempt: number) => {
    const summary = summaries.filter((s) => s.requestId === id && s.attempt === attempt && s.kind !== 'held').pop();
    const record = summary && decisions.get(`${id}/${summary.seq}`);
    return summary && record ? { record, summary } : null;
  };
  // The story's attempt rows carry the trace of the decision that dispatched them, so the drawer and the card agree.
  for (const id of STORY_IDS) {
    const r = requestById.get(id);
    if (!r) continue;
    for (const [i, a] of r.attempts.entries()) {
      const dispatching = dispatchingOf(id, i + 1);
      if (dispatching) a.selection = traceOf(dispatching.record, dispatching.summary, dispatching.record.outcome.cohortHold);
    }
  }
  // A written row's attempt names every member its selection passed over, out under its obstacle or passed over by the
  // pick, as the record that dispatched the attempt holds them. A resend carries the trace of the attempt it resends.
  for (const id of UNSEEDED_IDS) {
    const r = requestById.get(id);
    if (!r) continue;
    for (const [i, a] of r.attempts.entries()) {
      const record = dispatchingOf(id, resentInPlace(r, i) ? i : i + 1)?.record;
      if (record && a.selection) a.selection = { ...a.selection, skipped: skippedOf(record) };
    }
  }
  return { decisions, summaries: summaries.sort((a, b) => Date.parse(b.at) - Date.parse(a.at)) };
}

const built = buildDecisions();

/** One version 3 record per decision of every mock request, keyed `${requestId}/${seq}`. */
export const DECISIONS: Map<string, DecisionRecord> = built.decisions;
/** One summary per record, the routing log's decision rows, newest first. */
export const SUMMARIES: DecisionSummary[] = built.summaries;

export type EventSeed = Omit<RoutingEvent, 'agentId' | 'actor' | 'id' | 'keyId' | 'model' | 'poolId' | 'requestId' | 'sessionId' | 'type'> & Partial<Pick<RoutingEvent, 'actor' | 'agentId' | 'keyId' | 'model' | 'poolId' | 'requestId' | 'sessionId'>>;

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
      poolId: r.poolId,
      sessionId: r.sessionId,
    },
  ];
}

/**
 * A pool edit's diff as the store writes it: each field the save changed under its column name, with the value
 * `before` names and the one the pool holds now.
 */
function poolDiff(p: PoolSeed, before: Record<string, unknown>): Record<string, unknown> {
  const row = poolRow(p);
  return Object.fromEntries(Object.entries(before).map(([field, from]) => [field, { from, to: row[field] }]));
}

/** A pool's row as the store's `pool_diff` writes it on a creation, under its column names, its policy dotted. */
function poolRow(p: PoolSeed): Record<string, unknown> {
  return {
    affinity_mode: p.affinityMode,
    affinity_ttl_seconds: p.affinityTtlSeconds,
    auto_mode_server: p.autoModeServer,
    body_capture_hours: p.bodyCaptureHours,
    burn_average_seconds: p.burnAverageSeconds,
    cache_diagnostics: p.cacheDiagnostics,
    cache_ttl: { enabled: p.cacheTtl.enabled, inject_when_absent: p.cacheTtl.injectWhenAbsent, metered: encodeTtlRules(p.cacheTtl.metered), plan: encodeTtlRules(p.cacheTtl.plan) },
    enabled: p.status !== 'disabled',
    fable_reservation_shedding: p.fableReservationShedding,
    five_hour_ceiling: p.fiveHourCeiling,
    in_flight_cap: p.inFlightCap,
    'keepalive.enabled': p.keepalive.enabled,
    'keepalive.max_idle_seconds': p.keepalive.maxIdleSeconds,
    'keepalive.max_parked_seconds': p.keepalive.maxParkedSeconds,
    'keepalive.scheduled_horizon_seconds': p.keepalive.scheduledHorizonSeconds,
    label: p.name,
    load_shedding: p.loadShedding,
    migrate_when_spent: p.migrateWhenSpent,
    min_shared_prefix_tokens: p.minSharedPrefixTokens,
    model_rules: encodeModelRules(p.modelRules),
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
    thinking_diagnostics: p.thinkingDiagnostics,
    warm_sessions: p.warmSessions,
    weekly_ceiling: p.weeklyCeiling,
    wind_down_minutes: p.windDownMinutes,
  };
}

/**
 * A round's `shedding_round` event off the decision that ran it: the window that fired with its projection and horizon
 * from the shed line and the line its trigger read as the ceiling, the wall where the projection fired at a reset or an
 * end horizon, the round's figures, and each pin that the pick's walk read, a mark or not, with the rewrite and the
 * runway it would take on the destination that read found, each move planned before the read counted against its
 * runway there. A mark alone names its destination. The rewrite is the pin's Pro-week figure under `rewrite_unit`, a
 * figure of the pin alone, so an API-key destination reads it too, with the weekly window the pick compared on named,
 * and the share floor the round applied. Each pin names the thinking its move drops, null on a kept pair and with no
 * destination. A round the Fable reservation fired names the reservation, which any other round holds null. An event
 * of an earlier build (`EARLIER_BUILD`) stored the rewrite as the story's 20x equivalent of that window and names no
 * rewrite unit, no floor, no dropped thinking, no cold pin's order key (`per_drop`) and no reservation.
 */
function roundEvent(decision: string, pins: RoundPin[]): EventSeed | null {
  const earlier = EARLIER_BUILD.has(decision);
  const record = DECISIONS.get(decision);
  const line = record?.walk.find((w) => w.step === 'move:shed');
  const value = (key: string) => line?.facts.find((f) => f.key === key)?.value;
  const window = value('trigger');
  if (!record || typeof window !== 'string' || record.outcome.accountId === null) return null;
  const accountId = record.outcome.accountId;
  const at = Date.parse(record.request.at);
  const pool = poolById.get(record.request.poolId)!;
  const candidate = record.candidates.find((c) => c.accountId === accountId);
  const source = candidate?.windows[window];
  const firedBy = String(value('fired_by'));
  const reserved = firedBy === 'fable_reservation';
  // The Fable reservation's test names no rate, so its line carries no excess and its round records the burn of the
  // pins it walked in the excess's place.
  const excess = value('excess') === undefined ? null : Number(value('excess'));
  const round = roundOf(roundOfPins(pins), excess, pool);
  // A round the stretch fired reads the demand to the window's horizon less the threshold, the slow-down's horizon.
  const slowed = firedBy === 'slow_down' ? slowReadAt(accountId, pool, at) : null;
  // A round with no candidate to read a line from reads the window's own ceiling.
  const ceiling = candidate === undefined ? ceilingOn(pool, window) : triggerLineOf(candidate, window, pool, at);
  const threshold = pool.slowDownStretchSeconds * 1_000;
  const moved = slowed && source?.horizon ? Date.parse(source.horizon) - threshold : null;
  const horizon = moved ?? (source?.horizon ? Date.parse(source.horizon) : null);
  // The stretch fired with its moved horizon ahead of the round, so the projection there reads a figure.
  const projection = moved !== null && source?.horizon ? round3(stretchProjectionOf({ at, demand: source.demand ?? 0, horizon: Date.parse(source.horizon), utilization: source.utilization }, threshold)!) : (source?.projection ?? null);
  // A planned move books its rewrite and its burn on the destination for the rest of the walk, as the router's plan
  // does.
  const planned = new Map<string, RecordCandidate>();
  const scoreOf = (c: RecordCandidate) => c.pace?.score ?? 0;
  // A pin's destination as its own move would read it: a free eligible sibling holding allowance that the pin fits
  // to the horizon, ranked by the pool's strategy, by tier and under Pace by score, the longest runway breaking the
  // strategy's tie. Round robin's cursor names one turn, so the runway never speaks there. The walk reads it with the
  // moves booked so far, and the order with none (`order`). The fit and the booking price the write at the
  // destination, the last prompt less the thinking a bound pair drops (`bound`). Null where nothing fits.
  const read = (p: RoundPin, bound: number | null, order = false) => {
    const tokens = rewriteTokensOf(p.lastInputTokens, bound);
    const siblings = record.candidates.filter((c) => c.obstacle === null && c.accountId !== accountId && c.inFlight < c.inFlightCap && holdsAllowance(c, p.model));
    const reads = siblings.map((c) => {
      const base = (order ? undefined : planned.get(c.accountId)) ?? c;
      return { base, fit: fitOf(base, at, pool, p.model, tokens, { [window]: p.burn ?? 0 }, true) };
    });
    const fitting = reads.filter((x) => x.fit.verdict === 'fits');
    const ranked = [...fitting].sort((x, y) => (tierBefore(x.base.tier, y.base.tier) ? -1 : tierBefore(y.base.tier, x.base.tier) ? 1 : pool.strategy === 'pace' ? scoreOf(y.base) - scoreOf(x.base) : 0));
    const ties = ranked.filter((x) => tierKey(x.base.tier) === tierKey(ranked[0].base.tier) && (pool.strategy !== 'pace' || scoreOf(x.base) === scoreOf(ranked[0].base)));
    const chosen = pool.strategy === 'round_robin' ? ranked[0] : longestOf(ties, (x) => x.fit.runwayMs)[0];
    if (!chosen) return null;
    const rewrite = rewriteOf(tokens, p.model);
    // A pin with no prompt size reported yet is unsized, so its rewrite reads nothing, and the Pro-week figure is null
    // on a model without a weight of its own. It is the write alone, at the destination, which keeps none of the
    // thinking the move drops. The engine names the window the pick compared on beside a figure alone.
    const figure = p.lastInputTokens === 0 ? null : earlier ? (rewrite[rewriteWindowOf(p.model)] ?? null) : proWeekRewriteOf(tokens, p.model, CLIENTS[p.keyId] ?? null, pinCacheTtlOf(p.sessionId));
    // The booking writes to the destination's planned copy, taken at its first booking, so the record's candidates
    // stay as the decision left them.
    const book = () => {
      const booked = planned.get(chosen.base.accountId) ?? structuredClone(chosen.base);
      for (const [w, x] of Object.entries(rewrite)) if (booked.windows[w]) booked.windows[w].reserved = round5((booked.windows[w].reserved ?? 0) + x);
      booked.burn = { ...booked.burn, [window]: (booked.burn[window] ?? 0) + (p.burn ?? 0) };
      planned.set(booked.accountId, booked);
    };
    return { book, figures: { destination: chosen.base.accountId, rewrite: figure, runway: chosen.fit.shown?.runway ?? null, window: figure === null ? null : rewriteWindowOf(p.model) } };
  };
  // An earlier build read no dropped thinking. The account and the instant hold for the whole round, so each pin's
  // bound is read once.
  const bounds = new Map(pins.map((p) => [p, earlier ? null : droppedThinkingOf({ accountId, agentId: p.agentId, keyId: p.keyId, model: p.model, sessionId: p.sessionId }, at)]));
  const boundOf = (p: RoundPin) => bounds.get(p) ?? null;
  // An order read skips the booked moves and books none, so one read per pin serves both of its readers.
  const orderReads = new Map<RoundPin, ReturnType<typeof read>>();
  const orderReadOf = (p: RoundPin) => {
    if (!orderReads.has(p)) orderReads.set(p, read(p, boundOf(p), true));
    return orderReads.get(p) ?? null;
  };
  const unread = { destination: null, rewrite: null, runway: null, window: null };
  /** The round's pick as the router plans it. The walk takes the pins in the order's sequence. At each position
   *  it reads the pins from there on whose burn covers the rest, each with the earlier marks' destinations
   *  booked, and the pick's own booking joins behind those reads. With no covering pin to pick, it reads the pin
   *  at the position with the same bookings where its seed marks it or names it `no_runway`. A pin that the walk
   *  never reads keeps the order's read. */
  const pickReplayOf = (pins: RoundPin[]) => {
    const replay = new Map<RoundPin, { destination: string | null; rewrite: number | null; runway: number | null; window: string | null }>();
    const walk = pins.filter(walkedOf);
    const look = (p: RoundPin) => {
      const found = read(p, boundOf(p));
      replay.set(p, found?.figures ?? unread);
      return found;
    };
    // A standing mark is a move pending, and `never` walks the cold pins alone, which the order puts first.
    const reachable = (p: RoundPin) => p.skip !== 'standing' && !(pool.warmSessions === 'never' && !p.cold);
    let removed = walk.filter((p) => p.skip === 'standing').reduce((n, p) => n + (p.burn ?? 0), 0);
    for (const [i, p] of walk.entries()) {
      if (removed >= round.target) break;
      if (!reachable(p) || p.burn === null) continue;
      const rest = round.target - removed;
      // The guard reads no destination for a pin it keeps, and the seeds name the pin it kept `last_pin`.
      const covering = walk.slice(i).filter((q) => reachable(q) && q.skip !== 'last_pin' && q.burn !== null && q.burn >= rest);
      const compared = covering.map((q) => [q, look(q)] as const);
      const pick = compared.find(([q]) => q.marked);
      if (pick) {
        pick[1]?.book();
        break;
      }
      if (p.burn < rest && (p.marked || p.skip === 'no_runway')) {
        const own = look(p);
        if (p.marked) {
          own?.book();
          removed += p.burn;
        }
      }
    }
    for (const p of walk) if (!replay.has(p)) replay.set(p, { ...unread, destination: orderReadOf(p)?.figures.destination ?? null });
    return replay;
  };
  const replay = pickReplayOf(pins);
  return {
    accountId,
    agentId: record.request.agentId,
    at: iso(at),
    detail: {
      burn_average_seconds: pool.burnAverageSeconds,
      burn_sum: round.burnSum,
      ceiling,
      dwell_seconds: pool.shedDwellSeconds,
      dwell_until: at + pool.shedDwellSeconds * 1_000,
      excess: excess ?? round.burnSum,
      ...(earlier ? {} : { fable_reservation: reserved ? Number(value('fable_reservation')) : null }),
      factor: slowed?.factor ?? null,
      horizon,
      ...(earlier ? {} : { mark_share: SHED_MARK_SHARE }),
      marks: pins.filter((p) => p.marked).map((p) => ({ agent_id: p.agentId, key_id: p.keyId, model: p.model, session_id: p.sessionId })),
      outcome: round.outcome,
      persisted: true,
      pins: pins.map((p) => {
        // A pin names the drop where the read its figures follow found a destination, and a pin that leaves before
        // the walk has none. The record's destination is a mark's of this round alone, since a standing mark's move
        // was planned by an earlier round.
        const bound = boundOf(p);
        const figures = replay.get(p) ?? unread;
        const dropped = bound !== null && figures.destination !== null ? bound : null;
        // The router sizes both keys at the order, which read the pin before the round booked a destination.
        const orderDropped = bound !== null && walkedOf(p) && orderReadOf(p) !== null ? bound : null;
        const orderDrop = dropSizeOf(orderDropped, p.model, CLIENTS[p.keyId] ?? null, pinCacheTtlOf(p.sessionId));
        // The burn per token of the move's size, the rewrite plus the dropped thinking at the output's weight. A pin
        // with no prompt size divides by one token, the cheapest rewrite.
        const size = moveSizeOf(p.lastInputTokens, orderDropped, orderDrop) ?? 1;
        return {
          agent_id: p.agentId, burn: p.burn, cold: p.cold, ...(earlier ? {} : { dropped_thinking_tokens: dropped }), key_id: p.keyId, last_input_tokens: p.lastInputTokens, marked: p.marked, model: p.model, note: p.note,
          ...(earlier ? {} : { per_drop: p.burn === null || orderDrop === null ? null : Number((p.burn / orderDrop).toPrecision(7)) }),
          per_rewrite: p.burn === null ? null : Number((p.burn / size).toPrecision(7)), planned: p.marked && p.skip !== 'standing' ? figures.destination : null, rewrite: figures.rewrite,
          rewrite_window: figures.window, runway_ms: figures.runway, session_id: p.sessionId, skip: p.skip,
        };
      }),
      projection,
      reason: round.reason,
      removed: round.removed,
      ...(earlier ? {} : { rewrite_unit: 'pro_week' }),
      shed_cap: pool.shedCap,
      stretch_ms: slowed?.stretchMs ?? null,
      target: round.target,
      threshold_ms: slowed ? threshold : null,
      trigger: firedBy,
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
    sessionId: record.request.sessionId,
  };
}

/**
 * A move's mark leaving with the row the move took off the account, beside its `pin_migrated`. Its `limit` is null,
 * since a heal alone reads a projection under a line.
 */
function markCleared(): EventSeed[] {
  return [...DECISIONS.values()].flatMap((record) => {
    const m = record.outcome.markCleared;
    if (m === null) return [];
    const detail = { agent_id: record.request.agentId, limit: null, marked_at: Date.parse(m.markedAt), marked_on: m.markedOn, marked_window: m.markedWindow, projection: m.projection, reason: m.reason };
    return [{ accountId: m.markedOn, agentId: record.request.agentId, at: record.request.at, detail, keyId: record.request.keyId, kind: 'mark_cleared', model: record.request.canonicalModel, poolId: record.request.poolId, sessionId: record.request.sessionId }];
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
      const waits = REQUESTS.filter((r) => r.poolId === poolId && r.accountId === accountId && Date.parse(r.at) >= since && Date.parse(r.at) < end)
        .flatMap((r) => r.held?.waits ?? [])
        .filter((w) => w.reason === 'slow_down');
      const heldMs = waits.reduce((n, w) => n + w.heldMs, 0);
      seeds.push({ accountId, at: iso(at), detail: { duration_ms: end - since, factor_min: span.factorMin, held_ms: heldMs, holds: waits.length, since }, kind: 'slow_down_ended', poolId });
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
 * or move, a failover's among them, replaces the request's booking, and a renewal books none and leaves it. A request
 * the relay rejected before any attempt ends at its placement.
 */
function expiredReservations(): EventSeed[] {
  const booked = new Map<string, DecisionRecord | null>();
  for (const record of [...DECISIONS.values()].sort((x, y) => x.seq - y.seq)) {
    if (record.outcome.kind === 'placed' || record.outcome.kind === 'moved') booked.set(record.requestId, record.outcome.reservation === null ? null : record);
  }
  return [...booked.values()].flatMap((record) => {
    const booking = record?.outcome.reservation;
    const r = record ? requestById.get(record.requestId) : undefined;
    if (!record || !booking || !r || r.outcome === 'succeeded') return [];
    const last = servingAttempt(r);
    const ended = last === null ? Date.parse(record.request.at) : attemptEnd(last);
    const detail = { age_ms: ended - Date.parse(record.request.at), attempt: record.request.attempt, cause: endCauseOf(r), shares: booking.rewrite };
    return [{ accountId: booking.accountId, at: iso(ended), detail, kind: 'reservation_expired', requestId: record.requestId }];
  });
}

/**
 * The story's late event, studio's token refresh, which the ledger writes a few seconds after the mock loads: past the
 * first auto reload poll of a log read as the mock loads, so that poll lands nothing and a later one lands it on top.
 */
const LATE_EVENT_AT = NOW + 6_000;

/** The detail a usage poll writes for the endpoint's 503, a fault it passes over. */
const USAGE_503 = { endpoint: 'usage', error: 'usage request rejected with status 503', status: 503, transport: false };

/** How often the router's usage poll asks every polled account, its `POLL_INTERVAL`. */
const POLL_INTERVAL_MS = 5 * MINUTE;

/** An account's failed polls after its last clean read, one 503 at each slot before `endMs`. */
function failedPolls(accountId: string, cleanAt: string, endMs: number): EventSeed[] {
  const slots: number[] = [];
  for (let at = Date.parse(cleanAt) + POLL_INTERVAL_MS; at < endMs; at += POLL_INTERVAL_MS) slots.push(at);
  return slots.map((at) => ({ accountId, at: iso(at), detail: { ...USAGE_503 }, kind: 'poll_failed' }));
}

/**
 * Legacy's polls after its last clean read: the endpoint answered 503 at every slot until the one whose acquisition ran
 * the refresh the provider refused, and the poll asks an account it has to pair again nothing, so none follows.
 */
function legacyPolls(): EventSeed[] {
  const clean = accountById.get('legacy')?.usage?.poll?.lastCleanAt;
  return clean ? failedPolls('legacy', clean, LEGACY_LAPSED_AT) : [];
}

/**
 * An account's polls after its last clean read, up to and including its last attempt, whose whole millisecond the
 * bound passes, each answered 503. A 503 holds nothing back, so reserve's poll asked at every slot, and workshop's one
 * poll is its last attempt.
 */
function lastAttemptPolls(accountId: string): EventSeed[] {
  const poll = accountById.get(accountId)?.usage?.poll;
  if (!poll?.lastCleanAt || !poll.lastAttemptAt) return [];
  return failedPolls(accountId, poll.lastCleanAt, Date.parse(poll.lastAttemptAt) + 1);
}

/**
 * Primary's plan read as ended by the profile read a usage 403 made due, and found again by the next profile read a
 * minute later. While the flag stands the routing reads the account as past its end, so the two sit in a minute of
 * the morning no decision places on it.
 */
function planRows(account: Account): EventSeed[] {
  const sub = account.subscription;
  if (!sub) return [];
  const busy = (at: number) => SUMMARIES.some((s) => (s.accountId === account.id || s.fromAccountId === account.id) && Date.parse(s.at) >= at && Date.parse(s.at) <= at + MINUTE);
  let at = clock(6, 40);
  while (busy(at)) at -= MINUTE;
  return [
    { accountId: account.id, at: iso(at), detail: { organization_type: 'claude_free', rate_limit_tier: 'default_claude_ai', source: 'usage_403', subscription_status: 'canceled' }, kind: 'plan_ended' },
    { accountId: account.id, at: iso(at + MINUTE), detail: { organization_type: sub.organizationType, rate_limit_tier: sub.rateLimitTier, source: 'profile', subscription_status: 'active' }, kind: 'plan_restored' },
  ];
}

/** The plan-ended account's two rows, at the instant the account's flag names. */
function planEndedRows(account: Account): EventSeed[] {
  if (account.planEndedAt === null) return [];
  const at = Date.parse(account.planEndedAt);
  return [
    // The profile read found the free organization its canceled plan left, and no read since found a plan, so no
    // `plan_restored` follows.
    { accountId: account.id, at: account.planEndedAt, detail: { organization_type: 'claude_free', rate_limit_tier: 'default_claude_ai', source: 'profile', subscription_status: 'canceled' }, kind: 'plan_ended' },
    // The backoff close: a run one rate limit opened six minutes earlier stood open when the profile read paused the
    // poll, and the pause closes it at its own instant, since no clean answer will come.
    { accountId: account.id, at: account.planEndedAt, detail: { attempts: 1, cause: 'plan_ended', duration_ms: 6 * MINUTE, resumed_at: at }, kind: 'poll_backoff_ended' },
  ];
}

/** The detail `claimDetail` writes, a type alias so a routing event's detail map takes it. */
export type ClaimDetail = {
  cleared: string[];
  cooldown_until: number | null;
  error: null;
  grant_id: string;
  incarnation: number;
  label: string;
  outcome: ResetOutcome;
  reason: string | null;
  request_id: string;
  resets_left: number | null;
  result: string | null;
  status: number;
  weekly_resets_at: number | null;
};

/**
 * A claim's detail as the router writes it: every key on every row, null where the claim has no such fact, and the
 * instants in epoch milliseconds. The claim's request id travels here alone, since the row's own column names a relayed
 * request. A claim the upstream answered without a `result` carries no answer. `incarnation` names the account row
 * that held the id when the claim left, since an id deleted and created again starts a new one.
 */
export function claimDetail(grant: MockGrant, requestId: string, status: number, answer: MockUse | null, incarnation: number): ClaimDetail {
  return {
    cleared: answer?.cleared ?? [],
    cooldown_until: answer?.cooldown_until ?? null,
    error: null,
    grant_id: grant.id,
    incarnation,
    label: grant.label,
    // With no answer, the status alone classes the claim as the client classes it.
    outcome: answer?.outcome ?? ([401, 403, 429].includes(status) ? 'refused' : 'unsettled'),
    reason: answer?.reason ?? null,
    request_id: requestId,
    resets_left: answer?.resets_left ?? null,
    result: answer?.result ?? null,
    status,
    weekly_resets_at: answer?.weekly_resets_at ?? null,
  };
}

/** The fall past which the router writes a weekly window's drop, which also clears a drop no answered claim joins
 *  where the poll after it reads under it. The router reads both in whole points. */
export const WINDOW_DROP_MARGIN = 0.05;
export const wholePoints = (utilization: number): number => Math.round(utilization * 100);

/** The detail `dropDetail` writes, a type alias so a routing event's detail map takes it. */
export type DropDetail = { pair_changed: boolean; plan: ReadingPair['plan']; reset: number; tier: ReadingPair['tier']; utilization: number; utilization_before: number; window: string };

/**
 * A drop's detail as the router writes it: the stored reading's figure before the fall, the figure of the poll that
 * writes the event, the week's unchanged reset in epoch milliseconds, the stored reading's pair, and whether the
 * writing poll's pair (`writing`, the stored one by default) differs from it.
 */
export const dropDetail = (window: string, before: number, after: number, reset: number, pair: ReadingPair, writing: ReadingPair = pair): DropDetail => ({
  pair_changed: writing.plan !== pair.plan || writing.tier !== pair.tier,
  plan: pair.plan,
  reset,
  tier: pair.tier,
  utilization: after,
  utilization_before: before,
  window,
});

/**
 * The claims the story's blocks rest on: studio's use at the instant its block stamps, with the drop of the Fable week
 * it cleared from 0.75 to 0.02, and workshop's refusal 20 minutes back, the claim its block's cooldown comes from. The
 * use leaves the 7-day window as it stood, so it drops no other window. A seed is its id's first account, so each
 * claim's holder is incarnation 1.
 */
function claimRows(): EventSeed[] {
  const studio = RESET_BLOCKS.studio;
  const workshop = RESET_BLOCKS.workshop;
  const used = studio.grants[0];
  const usedAt = studio.uses[used.id];
  const cooled = workshop.grants[0];
  const cooldown = workshop.cooldown_until === null ? null : Date.parse(workshop.cooldown_until);
  return [
    {
      accountId: 'studio',
      actor: 'admin',
      at: iso(usedAt),
      detail: claimDetail(used, '8c1f5a3e-2d74-4b09-9e6a-5f3b7c0d1e82', 200, FABLE_USE, 1),
      kind: 'limit_reset_claimed',
    },
    { accountId: 'studio', at: iso(usedAt), detail: dropDetail('seven_day_fable', 0.75, 0.02, resetOf('studio', '7d_fable'), readingPairAt('studio', usedAt)), kind: 'window_dropped' },
    {
      accountId: 'workshop',
      actor: 'admin',
      at: iso(NOW - 20 * MINUTE),
      detail: claimDetail(cooled, '3a9e6d1c-7b52-4f80-a4c3-9d2e5b8f0c17', 200, cooldownRefusal(cooldown), 1),
      kind: 'limit_reset_claimed',
    },
  ];
}

const WEEK = 7 * DAY;

/** The plan word and the tier a `window_reset` stores, those of the reading it replaced. */
type ReadingPair = Pick<AllowanceReset, 'plan' | 'tier'>;

/** The pair a subscription reads, the one a reading taken under it stores. */
export const subscriptionPair = (s: Subscription | null): ReadingPair => ({ plan: s?.plan ?? null, tier: s?.rateLimitTier ?? null });

/** The account's pair as its subscription reads now. */
const currentPair = (accountId: string): ReadingPair => subscriptionPair(accountById.get(accountId)?.subscription ?? null);

/**
 * The pair an account held up to an instant before it changed: a reset at or before `through` replaced a reading taken
 * under `pair`, whatever window it closes, since one poll's readings name one plan.
 */
interface EarlierPair {
  pair: ReadingPair;
  through: number;
}

const PRO_READING: ReadingPair = { plan: 'pro', tier: 'default_claude_ai' };

/**
 * A weekly window's resets over the four weeks before the story's, a week apart back from `resetsAtMs`, its current
 * reset, each storing the reading before it so the share left is one less it, newest first, with the pair
 * `readingPairAt` gives that reading at the reset. The clock walk announces a reset no poll read after it, which names
 * no next reset, as the engine writes it.
 */
function weeklyResets(accountId: string, window: string, resetsAtMs: number, shares: number[], clockAt = -1): EventSeed[] {
  return shares.map((share, i) => {
    const reset = resetsAtMs - (i + 1) * WEEK;
    const clockWalk = i === clockAt;
    const { plan, tier } = readingPairAt(accountId, reset);
    return { accountId, at: iso(reset), detail: { next_reset: clockWalk ? null : reset + WEEK, observed: clockWalk ? 'clock' : 'sample', plan, reset_at: reset, tier, utilization_before: round3(1 - share), window }, kind: 'window_reset' };
  });
}

const resetOf = (accountId: string, window: '7d' | '7d_fable'): number => {
  const a = accountById.get(accountId);
  return Date.parse((a ? windowOf(a, window) : null)?.resetsAt ?? '');
};

/**
 * Studio upgraded from Pro to Max 5x at its subscription's start, inside its second-newest week, so its third and
 * fourth weeks ran under Pro on both its windows. Sunset was a Pro login until its plan ended, and the router stores a
 * reading with the pair of the profile it held when it took the reading, which names Pro up to the read that set the
 * plan-ended flag, so its readings before the flag carry Pro's pair.
 */
const STUDIO_BEFORE_UPGRADE: EarlierPair = { pair: PRO_READING, through: Date.parse(accountById.get('studio')?.subscription?.since ?? '') };
const SUNSET_BEFORE_FLAG: EarlierPair = { pair: PRO_READING, through: Date.parse(accountById.get('sunset')?.planEndedAt ?? '') };
const EARLIER_PAIRS = new Map<string, EarlierPair>([
  ['studio', STUDIO_BEFORE_UPGRADE],
  ['sunset', SUNSET_BEFORE_FLAG],
]);

/**
 * The pair the story stores for a reading of the account taken at `at`: null for both at or after a plan-ended flag,
 * since the stored profile names the plan up to the flag, else the account's earlier pair for an instant at or before
 * its `through`, else its current one. A term's end reads it beside the weekly resets, since its pair is the one the
 * window's stored reading carries.
 */
export function readingPairAt(accountId: string, at: number): ReadingPair {
  const flagged = accountById.get(accountId)?.planEndedAt ?? null;
  if (flagged !== null && at >= Date.parse(flagged)) return { plan: null, tier: null };
  const earlier = EARLIER_PAIRS.get(accountId);
  return earlier !== undefined && at <= earlier.through ? earlier.pair : currentPair(accountId);
}

/**
 * The story's weekly resets, the routing log's rows the unused allowance route answers from: each weekly window of the
 * subscription accounts with traffic, and sunset's 7-day window, which its plan's end left unreported, so its three
 * resets run back from the reset of the week the end cut short. Nightly's third 7-day reset fell between two polls and
 * the clock walk announced it.
 */
const WEEKLY_RESETS: EventSeed[] = [
  ...weeklyResets('primary', 'seven_day', resetOf('primary', '7d'), [0.32, 0.41, 0.27, 0.38]),
  ...weeklyResets('primary', 'seven_day_fable', resetOf('primary', '7d_fable'), [0.05, 0.12, 0.08, 0.03]),
  ...weeklyResets('secondary', 'seven_day', resetOf('secondary', '7d'), [0.61, 0.55, 0.7, 0.58]),
  ...weeklyResets('nightly', 'seven_day', resetOf('nightly', '7d'), [0.18, 0.22, 0.15, 0.29], 2),
  ...weeklyResets('nightly', 'seven_day_fable', resetOf('nightly', '7d_fable'), [0.31, 0.36, 0.28, 0.4]),
  ...weeklyResets('studio', 'seven_day', resetOf('studio', '7d'), [0.44, 0.39, 0.5, 0.47]),
  ...weeklyResets('studio', 'seven_day_fable', resetOf('studio', '7d_fable'), [0.79, 0.84, 0.76, 0.82]),
  ...weeklyResets('sunset', 'seven_day', NOW + SUNSET_WEEK_RESETS_IN, [0.52, 0.48, 0.57]),
];

/**
 * Studio's upgrade at its subscription's start, as the router reads it: the same use against five times the allowance
 * cut its Fable week from 0.14 to 0.03 under the reset of the week that stood open then. The poll at the start read
 * the fall first, so the poll behind it writes the drop with the pair before the fall, Pro's, and its own pair named
 * another, and no claim joins the drop, so its row reads as a plan change. The router writes a drop per window and only
 * past the margin, and the 7-day window fell by no more than the margin, so the Fable week's drop stands alone.
 */
const STUDIO_PLAN_CHANGE: EventSeed = {
  accountId: 'studio',
  at: iso(STUDIO_BEFORE_UPGRADE.through + POLL_INTERVAL_MS),
  detail: dropDetail('seven_day_fable', 0.14, 0.03, resetOf('studio', '7d_fable') - 2 * WEEK, readingPairAt('studio', STUDIO_BEFORE_UPGRADE.through), readingPairAt('studio', STUDIO_BEFORE_UPGRADE.through + POLL_INTERVAL_MS)),
  kind: 'window_dropped',
};

/**
 * The events beside the decisions: every pin event and model answer the requests' windows hold, the pins the
 * placements created, the cooldowns the attempts' answers set, and the story's own rows, at least one of every kind.
 */
function buildEvents(): RoutingEvent[] {
  const t = clock;
  const seeds: EventSeed[] = [];
  for (const [id, events] of REQUEST_EVENTS) {
    const poolId = requestById.get(id)?.poolId ?? null;
    for (const e of events) {
      const detail = e.detail ?? {};
      seeds.push({ accountId: e.accountId, agentId: typeof detail.agent_id === 'string' ? detail.agent_id : null, at: e.at, detail, keyId: e.keyId, kind: e.kind, model: e.model, poolId, requestId: typeof detail.source_request_id === 'string' ? detail.source_request_id : null, sessionId: e.sessionId });
    }
  }
  for (const s of SUMMARIES) {
    const record = DECISIONS.get(`${s.requestId}/${s.seq}`);
    if (!record?.outcome.pinEvents.some((e) => e.kind === 'pin_created')) continue;
    seeds.push({ accountId: s.accountId, agentId: s.agentId, at: s.at, detail: { agent_id: s.agentId, epoch: record.outcome.pinWritten?.epoch ?? null }, keyId: s.keyId, kind: 'pin_created', model: s.model, poolId: s.poolId, sessionId: s.sessionId });
  }
  const heads = REQUESTS.flatMap((r) => r.attempts.map((a) => ({ a, at: attemptEnd(a), r }))).sort((x, y) => x.at - y.at);
  // The story's own spent edge, which the poll read before any answer's head: nightly's 5-hour window at its limit
  // within the hour of the least-bad move onto it. Its first reading under the limit after that, an answer's head or
  // the reading a decision met, clears it, so the 429's head that reads the limit again before the reset is a crossing
  // of its own.
  const nightlyReset = clock(13, 11);
  const nightlySpentAt = clock(12, 1, 5);
  seeds.push({ accountId: 'nightly', at: iso(nightlySpentAt), detail: { reset: nightlyReset, source: 'poll', source_request_id: null, utilization: 1, window: 'five_hour' }, kind: 'window_spent' });
  const under = (at: number, reset: string | null, utilization: number | null) => at > nightlySpentAt && reset === iso(nightlyReset) && utilization !== null && utilization < 1;
  const nightlyReads = [
    ...heads.flatMap(({ a, at, r }) => {
      const w = a.accountId === 'nightly' ? a.windows.find((x) => x.id === '5h') : undefined;
      return w && under(at, w.resetsAt, w.utilization) ? [{ at, requestId: r.id, source: 'head', utilization: w.utilization as number }] : [];
    }),
    ...[...DECISIONS.values()].flatMap((record) => {
      const w = record.candidates.find((c) => c.accountId === 'nightly')?.windows.five_hour;
      const at = w ? Date.parse(w.observedAt) : Number.NaN;
      return w && under(at, w.reset, w.utilization) ? [{ at, requestId: w.source === 'head' ? record.requestId : null, source: w.source, utilization: w.utilization }] : [];
    }),
  ].sort((x, y) => x.at - y.at);
  const nightlyClear = nightlyReads[0];
  if (nightlyClear) {
    const detail = { reset: nightlyReset, source: nightlyClear.source, source_request_id: nightlyClear.requestId, utilization: nightlyClear.utilization, utilization_before: 1, window: 'five_hour' };
    seeds.push({ accountId: 'nightly', at: iso(nightlyClear.at), detail, kind: 'window_cleared', requestId: nightlyClear.requestId });
  }
  // Legacy's 5-hour window, spent at a poll and read back under its limit under the same reset by its last clean poll,
  // the reading the ledger keeps while its account answers no window, so no reading at the limit follows the clear. Its
  // history carries both readings, and the clock announced the reset that passed after the last clean poll, as no poll
  // read it again.
  const legacyRow = accountById.get('legacy');
  const legacy = legacyRow && lastReadingOf(legacyRow).windows.find((w) => w.id === '5h');
  if (legacy?.resetsAt && legacy.utilization !== null && legacy.utilization < 1) {
    const reset = Date.parse(legacy.resetsAt);
    const read = { reset, source: 'poll', source_request_id: null, window: 'five_hour' };
    seeds.push(
      { accountId: 'legacy', at: iso(LEGACY_FIVE_HOUR_SPENT_AT), detail: { ...read, utilization: 1 }, kind: 'window_spent' },
      { accountId: 'legacy', at: legacy.observedAt, detail: { ...read, utilization: legacy.utilization, utilization_before: 1 }, kind: 'window_cleared' },
      { accountId: 'legacy', at: iso(reset), detail: { next_reset: null, observed: 'clock', ...currentPair('legacy'), reset_at: reset, utilization_before: legacy.utilization, window: 'five_hour' }, kind: 'window_reset' },
    );
  }
  // Studio's wall after the round its stretch fired at 14:05: a poll read the 5-hour window at its limit, and the
  // reading the account answers now reads it under again before the reset.
  const studio = accountById.get('studio')?.windows.find((w) => w.id === '5h');
  if (studio?.resetsAt && studio.utilization !== null && studio.utilization < 1) {
    const read = { reset: Date.parse(studio.resetsAt), source: 'poll', source_request_id: null, window: 'five_hour' };
    seeds.push(
      { accountId: 'studio', at: iso(clock(14, 20)), detail: { ...read, utilization: 1 }, kind: 'window_spent' },
      { accountId: 'studio', at: studio.observedAt, detail: { ...read, utilization: studio.utilization, utilization_before: 1 }, kind: 'window_cleared' },
    );
  }
  // A window's spent edge is the first head of any answer that reads it at its limit, once per account, window and
  // reset until a reading under the limit clears it.
  const announced = new Map<string, number>([[`nightly|five_hour|${iso(nightlyReset)}`, nightlyClear?.at ?? Number.POSITIVE_INFINITY]]);
  for (const { a, at, r } of heads) {
    for (const w of a.windows) {
      const window = WINDOW_KEY[w.id];
      const edge = `${a.accountId}|${window}|${w.resetsAt}`;
      if ((window !== 'five_hour' && window !== 'seven_day') || (w.utilization ?? 0) < 1 || at < (announced.get(edge) ?? Number.NEGATIVE_INFINITY)) continue;
      announced.set(edge, Number.POSITIVE_INFINITY);
      seeds.push({ accountId: a.accountId, at: iso(at), detail: { reset: w.resetsAt === null ? null : Date.parse(w.resetsAt), source: 'head', source_request_id: r.id, utilization: 1, window }, kind: 'window_spent', requestId: r.id });
    }
  }
  seeds.push(
    ...ATTEMPT_COOLDOWNS.map(({ accountId, model, reason, requestId, scope, setAt, until, window }): EventSeed => ({
      accountId,
      at: iso(setAt),
      detail: { reason, scope, source_request_id: requestId, until, window },
      kind: 'cooldown_set',
      ...(scope.startsWith(MODEL_SCOPE_PREFIX) ? { model } : {}),
      requestId,
    })),
  );
  // A pin event names the epoch the record wrote, so the event and the card read one number, and a rule's move the
  // fit, the runway and the window that binds on the destination its gate read. A move an account's state forced reads
  // the placement's fit there.
  for (const seed of seeds) {
    if (seed.kind !== 'pin_migrated') continue;
    const s = SUMMARIES.find((x) => x.requestId === seed.requestId || (x.sessionId === seed.sessionId && x.at === seed.at && x.accountId === seed.accountId));
    const record = s && DECISIONS.get(`${s.requestId}/${s.seq}`);
    const epoch = record?.outcome.pinWritten?.epoch;
    if (epoch !== undefined && epoch !== null) seed.detail.epoch = epoch;
    const landed = record?.outcome.kind === 'moved' || forcedReasons(seed) !== null ? record?.candidates.find((c) => c.accountId === record.outcome.accountId) : undefined;
    if (landed?.fit) Object.assign(seed.detail, { binding: landed.binding, fit: landed.fit, runway_ms: landed.runwayMs });
    // A failover's event carries the cooldown that caused it, in the shape the router serializes, and a rule's move
    // none.
    const cause = record?.outcome.cause;
    if (record) seed.detail.cause = cause ? { reason: cause.reason, scope: cause.scope, window: cause.window } : null;
  }

  const primary = accountById.get('primary')!;
  const primaryAccess = primary.credential.accessTokenExpiresAt === null ? null : Date.parse(primary.credential.accessTokenExpiresAt);
  const primaryRefresh = primary.credential.refreshTokenExpiresAt === null ? null : Date.parse(primary.credential.refreshTokenExpiresAt);
  const fiveHourReset = Date.parse(primary.windows.find((w) => w.id === '5h')?.resetsAt ?? '') - 5 * HOUR;
  const oldestCooldown = COOLDOWNS.filter((c) => c.scope === 'account' && c.until < STORY_HOUR_FROM && c.until > NOW - DAY).sort((a, b) => a.until - b.until)[0];
  const gap = REQUESTS.filter((r) => r.responseTruncated && Date.parse(r.at) < STORY_HOUR_FROM)[0];
  const secondary = accountById.get('secondary')!;
  const endsAt = Date.parse(secondary.subscription?.endsAt ?? '');
  const pool = poolById.get('all')!;
  const annex = accountById.get('annex')!;
  const beforeReset = oauthWindows('primary', fiveHourReset - 1, () => OLDER_FACTOR).find((w) => w.id === '5h')?.utilization ?? null;
  const undecodable = requestById.get('req_5e3c8a41-d27')!;
  const cutAttempt = undecodable.attempts[undecodable.attempts.length - 1];
  seeds.push(
    // The 08:30 and 11:00 rounds are an earlier build's records, so they name no unit and keep the rewrites they
    // stored, priced through that build's fallback weight: the 11:00 round's planned moves of Sonnet 4.5 pins read
    // their figures, which the request story's moves onto primary at 11:00 follow.
    ...[
      roundEvent('req_1eaa9bc1-a18/1', ROUND_PINS),
      roundEvent('req_9f2e7c5a-c49/2', STRETCH_PINS),
      roundEvent('req_6e1f3b98-a24/1', NO_RUNWAY_PINS),
      roundEvent('req_3e7a5c19-f84/1', FABLE_WEEK_PINS),
      roundEvent('req_5d1f8c3a-b27/1', RESERVATION_PINS),
      roundEvent(`${MOVED_PIN_CALL_ID}/1`, MOVED_PIN_ROUND_PINS),
    ].flatMap((e) => (e === null ? [] : [e])),
    ...markCleared(),
    ...expiredReservations(),
    ...slowDownSpans(),
    // The story hour.
    ...expiries(),
    { accountId: 'secondary', at: iso(t(13, 48, 20)), detail: { attempts: 1, endpoint: 'usage', last_clean_poll_at: t(13, 43), next_attempt_at: t(13, 53), reason: '429' }, kind: 'poll_backoff_started' },
    // Primary's head went out on the client's retry and the relay could not decode the stream past part of the body, so
    // the attempt's completion records the cut with the request's error word and the body bytes the client got.
    {
      accountId: cutAttempt.accountId,
      at: iso(attemptEnd(cutAttempt)),
      detail: { attempt: cutAttempt.index + 1, bytes_relayed: 846, code: undecodable.errorCode, text: 'error decoding response body', upstream_request_id: cutAttempt.upstreamRequestId },
      keyId: undecodable.keyId,
      kind: 'stream_interrupted',
      model: undecodable.canonicalModel,
      poolId: undecodable.poolId,
      requestId: undecodable.id,
      sessionId: undecodable.sessionId,
    },
    // Earlier the same day.
    // The refresh rows in the router's shape: primary's commit names its credential's expiry, and nightly's two
    // failures name their class beside the text the manager stored.
    { accountId: 'primary', at: iso(t(12, 52, 10)), detail: { access_expires_at: primaryAccess, refresh_expires_at: primaryRefresh, until: primaryAccess }, kind: 'refresh_succeeded' },
    { accountId: 'nightly', at: iso(t(12, 19, 40)), detail: { cause: 'transport', error: 'transport: connection refused (os error 111)' }, kind: 'refresh_failed' },
    { accountId: 'nightly', at: iso(t(12, 20)), detail: { cause: 'refresh_response_lost', error: 'refresh response lost: connection reset by peer' }, kind: 'refresh_uncertain' },
    // A usage poll the endpoint answered 503: neither its 200, its 429 nor its 401, so it passes as a fault and the
    // next poll reads clean.
    { accountId: 'nightly', at: iso(t(12, 26, 40)), detail: USAGE_503, kind: 'poll_failed' },
    ...legacyPolls(),
    ...lastAttemptPolls('reserve'),
    ...lastAttemptPolls('workshop'),
    { accountId: 'research', at: iso(RESEARCH_BUDGET_AT), detail: { budget_usd: 20, period: 'day', period_end: startOfDay(RESEARCH_BUDGET_AT) + DAY, source_request_id: BUDGET_CROSSING_ID, spent_usd: 20.4 }, kind: 'budget_reached', poolId: 'jobs', requestId: BUDGET_CROSSING_ID },
    { accountId: 'primary', at: iso(fiveHourReset), detail: { next_reset: fiveHourReset + 5 * HOUR, observed: 'sample', ...currentPair('primary'), reset_at: fiveHourReset, utilization_before: beforeReset, window: 'five_hour' }, kind: 'window_reset' },
    ...WEEKLY_RESETS,
    STUDIO_PLAN_CHANGE,
    { accountId: 'overflow', at: iso(OVERFLOW_LAPSED_AT), detail: { cause: 'funding_refused', detail: 'Your credit balance is too low to access the Anthropic API.', from: 'ready', to: 'funding_required' }, kind: 'account_state_changed' },
    {
      accountId: 'nightly',
      at: iso(NIGHTLY_CAP_RAISED),
      detail: { after: { disabled_reason: null, enabled: true, utilization: 0.31 }, before: { disabled_reason: null, enabled: true, utilization: 1 }, blocked_after: null, blocked_before: 'org_spend_cap_reached', observed_at: NIGHTLY_CAP_RAISED },
      kind: 'fast_credits_changed',
    },
    { accountId: 'nightly', at: iso(OVERAGE_FROM), detail: {}, kind: 'overage_started' },
    // The edit that raised the pool's shared prefix floor and its slow-down hold and cut its idle window.
    {
      accountId: null,
      actor: 'admin',
      at: iso(BEFORE_EDIT.all.at),
      detail: {
        diff: poolDiff(pool, { min_shared_prefix_tokens: BEFORE_EDIT.all.settings.minSharedPrefixTokens, slow_down_hold_seconds: BEFORE_EDIT.all.settings.slowDownHoldSeconds, 'keepalive.max_idle_seconds': BEFORE_EDIT.all.keepalive.maxIdleSeconds }),
        kind: 'pool_updated',
        target: 'all',
      },
      kind: 'registry_changed',
      poolId: 'all',
    },
    // The annex unpaused and renamed with its secret replaced: the store records the secret as its flag alone. The
    // ledger's row names each change's subject in the column of its kind, as its target names it.
    {
      accountId: 'annex',
      actor: 'admin',
      at: iso(t(9, 8, 30)),
      detail: { diff: { enabled: { from: false, to: annex.routingEnabled }, label: { from: 'annex', to: annex.label }, secret_replaced: true }, kind: 'account_updated', target: 'annex' },
      kind: 'registry_changed',
    },
    // The operator's pause of the Overflow workspace after its lapse, which the store records as the switch's pair alone.
    { accountId: 'overflow', actor: 'admin', at: iso(OVERFLOW_PAUSED_AT), detail: { diff: { enabled: { from: true, to: false } }, kind: 'account_updated', target: 'overflow' }, kind: 'registry_changed' },
    // The ci key's revocation, after the last request it reached selection with.
    { accountId: null, actor: 'admin', at: iso(KEY_REVOKED_AT), detail: { diff: {}, kind: 'key_revoked', target: 'ci' }, keyId: 'ci', kind: 'registry_changed' },
    // The retired workspace's life: the store's creation, which names its kind and label, and the members write that
    // places it in its pool, which stores the whole list. An API key account is born ready, so no state row opens its
    // history. At its delete, the store's change, which stores no diff, and the account's last state row.
    { accountId: RETIRED.account.id, actor: 'admin', at: iso(RETIRED.createdAt), detail: { diff: { kind: RETIRED.account.kind, label: RETIRED.account.label }, kind: 'account_created', target: RETIRED.account.id }, kind: 'registry_changed' },
    {
      accountId: null,
      actor: 'admin',
      at: iso(RETIRED.createdAt),
      detail: {
        diff: { members: encodeMembers([...(poolById.get(RETIRED.poolId)?.members ?? []), RETIRED.member]) },
        kind: 'pool_members_set',
        target: RETIRED.poolId,
      },
      kind: 'registry_changed',
      poolId: RETIRED.poolId,
    },
    { accountId: RETIRED.account.id, actor: 'admin', at: iso(RETIRED.deletedAt), detail: { diff: {}, kind: 'account_deleted', target: RETIRED.account.id }, kind: 'registry_changed' },
    { accountId: RETIRED.account.id, at: iso(RETIRED.deletedAt), detail: { cause: 'deleted', detail: null, from: 'ready', to: 'deleted' }, kind: 'account_state_changed' },
    { accountId: 'primary', at: iso(t(9, 0, 41)), detail: { added: ['claude-opus-5-5'], count: 12, removed: [] }, kind: 'catalog_changed' },
    { accountId: null, at: iso(ROUTER_RESTARTED_AT), detail: { affinity_entries_loaded: 34, down_since: t(6, 11, 58), holds_lost: true, pins_loaded: 10, shedding_marks_loaded: 0, shedding_rounds_stale: 0, version: '0.1.0', windows_loaded: 5 }, kind: 'router_started' },
    // Once each outside the hour.
    // The sweep's expiry names no request, so it reads its pool off its key.
    { accountId: 'primary', agentId: 'main', at: iso(t(13, 13, 40)), detail: { agent_id: 'main', reasons: ['expired'] }, keyId: 'laptop', kind: 'pin_expired', model: HAIKU, poolId: keyById.get('laptop')?.poolId ?? null, sessionId: sessionId('5eeed6ce') },
    { accountId: 'nightly', actor: 'admin', agentId: 'e90b', at: iso(t(12, 40, 5)), detail: { agent_id: 'e90b' }, keyId: 'desktop', kind: 'pin_released', model: 'claude-sonnet-5-5', sessionId: sessionId('407dd39b') },
    { accountId: 'primary', at: iso(Date.parse('2026-09-12T20:10:00Z') - Date.parse('2026-09-16T14:30:00Z') + NOW), detail: { since: Date.parse('2026-09-11T16:10:00Z') - Date.parse('2026-09-16T14:30:00Z') + NOW }, kind: 'overage_ended' },
    { accountId: 'legacy', at: iso(LEGACY_LAPSED_AT), detail: { cause: 'refresh_failed', detail: 'refresh_token rejected: invalid_grant', from: 'ready', to: 'reauth_required' }, kind: 'account_state_changed' },
    // The Overflow workspace's balance ran out once before, three days back, and the router's own retry past its
    // five minutes found it funded again.
    { accountId: 'overflow', at: iso(NOW - 3 * DAY - 5 * HOUR), detail: { cause: 'funding_refused', detail: 'Your credit balance is too low to access the Anthropic API.', from: 'ready', to: 'funding_required' }, kind: 'account_state_changed' },
    { accountId: 'overflow', at: iso(NOW - 3 * DAY - 5 * HOUR + 5 * MINUTE + 40_000), detail: { cause: 'funding_retry', detail: null, from: 'funding_required', to: 'ready' }, kind: 'account_state_changed' },
    { accountId: 'primary', at: iso(t(8, 15, 3)), detail: { attempts: 3, cause: 'clean_poll', duration_ms: 25 * MINUTE, resumed_at: t(8, 15, 3) }, kind: 'poll_backoff_ended' },
    { accountId: 'research', at: iso(startOfDay(t(14, 30))), detail: { period: 'day', period_start: startOfDay(t(14, 30)) - DAY, previous_spent_usd: 18.2 }, kind: 'budget_period_ended', poolId: 'jobs' },
    // Drawn at the instant it fires: the subscription's end less the pool's wind-down, which is still ahead.
    { accountId: 'secondary', at: iso(endsAt - pool.windDownMinutes * MINUTE), detail: { end_source: 'canceled', ends_at: endsAt, wind_down_minutes: pool.windDownMinutes }, kind: 'wind_down_started', poolId: 'all' },
    ...planRows(primary),
    ...planEndedRows(accountById.get('sunset')!),
    ...claimRows(),
  );
  if (oldestCooldown) seeds.push({ accountId: oldestCooldown.accountId, at: iso(oldestCooldown.until), detail: { cause: 'expired', scope: oldestCooldown.scope }, kind: 'cooldown_cleared' });
  if (gap) seeds.push({ accountId: null, at: iso(Date.parse(gap.at) + (gap.durationMs ?? 0)), detail: { reason: 'chunk_skipped', request_id: gap.id }, kind: 'ledger_gap', requestId: gap.id });
  // The story's access tokens run eight hours from their refresh.
  const studioRefresh = accountById.get('studio')?.credential.refreshTokenExpiresAt ?? null;
  seeds.push({ accountId: 'studio', at: iso(LATE_EVENT_AT), detail: { access_expires_at: LATE_EVENT_AT + 8 * HOUR, refresh_expires_at: studioRefresh === null ? null : Date.parse(studioRefresh), until: LATE_EVENT_AT + 8 * HOUR }, kind: 'refresh_succeeded' });
  // At one instant the kinds read alphabetically, save that a placement writes the release of the row it replaces
  // before the pin it creates, so the ledger numbers the release first, and that the ledger's mirror records a
  // registry change before the sweep that writes the changed account's state row.
  const instantKey = (kind: string) => (kind === 'pin_expired' || kind === 'pin_released' ? `pin_0${kind.slice(4)}` : kind === 'registry_changed' ? 'account_registry_changed' : kind);
  return seeds
    .sort((a, b) => Date.parse(a.at) - Date.parse(b.at) || instantKey(a.kind).localeCompare(instantKey(b.kind)))
    .map((e, i) => ({ type: 'event', accountId: e.accountId, actor: e.actor ?? null, agentId: e.agentId ?? null, at: e.at, detail: e.detail, id: i + 1, keyId: e.keyId ?? null, kind: e.kind, model: e.model ?? null, poolId: e.poolId ?? null, requestId: e.requestId ?? null, sessionId: e.sessionId ?? null }));
}

/** Every event kind of the ledger at least once, oldest first by id. */
export const ROUTING_EVENTS: RoutingEvent[] = buildEvents();
/** The late event's id: the route lists it once its clock reaches the event, as the ledger has written it by then. */
const LATE_EVENT_ID = ROUTING_EVENTS.find((e) => Date.parse(e.at) === LATE_EVENT_AT)?.id;

/** Every event kind the ledger writes, as the route validates `kinds` against them. */
const EVENT_KINDS: readonly string[] = [
  'account_state_changed', 'budget_period_ended', 'budget_reached', 'catalog_changed', 'cooldown_cleared', 'cooldown_set', 'fast_credits_changed', 'ledger_gap', 'limit_reset_claimed', 'mark_cleared', 'model_permitted', 'model_rejected',
  'overage_ended', 'overage_started', 'pin_created', 'pin_expired', 'pin_migrated', 'pin_released', 'plan_ended', 'plan_restored', 'poll_backoff_ended', 'poll_backoff_started', 'poll_failed',
  'refresh_failed', 'refresh_succeeded', 'refresh_uncertain', 'registry_changed', 'reservation_expired', 'router_started', 'shedding_round', 'slow_down_ended', 'slow_down_started',
  'stream_interrupted', 'warm_prefix_expired', 'wind_down_started', 'window_cleared', 'window_dropped', 'window_reset', 'window_spent',
];

/**
 * The route's merged order: newest first, a decision before an event at one instant, and the whole order reversed
 * under `dir: 'asc'`, so a cursor resumes past its row by the same comparisons flipped.
 */
const cursorOf = (row: RoutingLogPage['rows'][number]) => (row.type === 'decision' ? `${Date.parse(row.at)}:1:${row.requestId}/${row.seq}` : `${Date.parse(row.at)}:0:${row.id}`);
/**
 * A cursor's integer as the routes parse one, an optional sign and digits alone, or null. An integer past what a
 * number holds exactly reads null too, where the routes read one up to their 64-bit bound.
 */
export const cursorInteger = (text: string): number | null => (/^[+-]?\d+$/.test(text) && Number.isSafeInteger(Number(text)) ? Number(text) : null);
/**
 * The fields of the row a cursor names, as `compareRows` reads them, or null for a cursor the route refuses. The
 * request id may hold a `/` or a `:` of its own, so the sequence, the route's 32-bit unsigned figure, is read off the
 * last `/`.
 */
export const routingCursorOf = (cursor: string) => {
  const first = cursor.indexOf(':');
  const second = first < 0 ? -1 : cursor.indexOf(':', first + 1);
  if (second < 0) return null;
  const ms = cursorInteger(cursor.slice(0, first));
  // A `Date` holds a narrower range than the route's 64-bit instant, so an instant past it is refused where the route
  // answers a page.
  if (ms === null || Number.isNaN(new Date(ms).getTime())) return null;
  const rank = cursor.slice(first + 1, second);
  const id = cursor.slice(second + 1);
  if (rank === '0') {
    const rowid = cursorInteger(id);
    return rowid === null ? null : { at: iso(ms), id: rowid, type: 'event' as const };
  }
  const slash = id.lastIndexOf('/');
  const seq = id.slice(slash + 1);
  // Below 1, the slash is missing or leaves the request id empty.
  if (rank !== '1' || slash < 1 || !/^\+?\d+$/.test(seq) || Number(seq) > 0xffff_ffff) return null;
  return { at: iso(ms), requestId: id.slice(0, slash), seq: Number(seq), type: 'decision' as const };
};

/** As the routes read a list filter: an absent or empty list narrows nothing, and a row without the value matches
 * no list that narrows. */
export const inList = (list: string[] | undefined, value: string | null): boolean =>
  !list?.length || (value !== null && list.includes(value));

/**
 * The merged page by the route's rules, newest first and oldest first under `dir: 'asc'`. The range runs from an hour
 * back by default. An absent `kinds` is every decision and event, the probe decisions among them, and `kinds` names
 * exactly what it lists, `probe` for a probe's decision of any kind. `accounts` matches a decision's source too.
 * `limit + 1` rows are read so a cursor is offered only when a next page exists, and the counts come under a request
 * filter. The rows are the ledger's as the caller reads them: its decision summaries, its events, and the rows a mock's
 * writes recorded since, which `written` holds.
 */
export function listRoutingLog(query: RoutingLogQuery, now: number, rows: { events: readonly RoutingEvent[]; summaries: readonly DecisionSummary[]; written: readonly RoutingEvent[] }, cursor: NonNullable<ReturnType<typeof routingCursorOf>> | undefined): RoutingLogPage {
  const from = query.from ? Date.parse(query.from) : now - HOUR;
  const to = query.to ? Date.parse(query.to) : now;
  // An empty list is no filter, as the client omits it from the query.
  const kinds = query.kinds?.length ? query.kinds : undefined;
  const within = (at: string) => Date.parse(at) >= from && Date.parse(at) <= to;
  const session = (value: string | null) => !query.session || (value !== null && value.startsWith(query.session));
  // An event that names no agent matches none, as the route leaves a session-level event out.
  const agent = (value: string | null) => !query.agentId || value === query.agentId;
  const decisions = rows.summaries.filter(
    (s) =>
      within(s.at) &&
      (!query.request || s.requestId === query.request) &&
      session(s.sessionId) &&
      agent(s.agentId) &&
      inList(query.keyIds, s.keyId) &&
      inList(query.poolIds, s.poolId) &&
      inList(query.models, s.model) &&
      (!query.accountIds?.length || query.accountIds.some((id) => id === s.accountId || id === s.fromAccountId)) &&
      (kinds === undefined || (kinds.includes(s.kind) && !s.probe) || (kinds.includes('probe') && s.probe)),
  );
  const keep = (e: RoutingEvent) =>
    within(e.at) &&
    (e.id !== LATE_EVENT_ID || now >= LATE_EVENT_AT) &&
    (!query.request || e.requestId === query.request) &&
    session(e.sessionId) &&
    agent(e.agentId) &&
    inList(query.keyIds, e.keyId) &&
    inList(query.poolIds, e.poolId) &&
    inList(query.models, e.model) &&
    // A move lists under the account it left too, as a decision does under its source.
    (inList(query.accountIds, e.accountId) || inList(query.accountIds, movedFrom(e))) &&
    (kinds === undefined || kinds.includes(e.kind));
  const events = [...rows.events.filter(keep), ...rows.written.filter(keep)];
  const dir = query.dir ?? 'desc';
  const merged = [...decisions, ...events].sort(compareRows);
  if (dir === 'asc') merged.reverse();
  // The route resumes strictly past the cursor's row in the page's order.
  const sign = dir === 'asc' ? -1 : 1;
  const rest = cursor === undefined ? merged : merged.filter((row) => sign * compareRows(row, cursor) > 0);
  const limit = query.limit ?? 25;
  const page = rest.slice(0, limit);
  const last = page[page.length - 1];
  return {
    counts: query.request ? { decisions: decisions.length, events: events.length } : null,
    nextCursor: rest.length > limit && last ? cursorOf(last) : null,
    rows: page,
  };
}

/** The kinds the route accepts in `kinds`: every decision and event kind, and the probe pseudo-kind. */
export const ROUTING_LOG_KINDS: readonly string[] = [...DECISION_KIND_VALUES, 'probe', ...EVENT_KINDS];

/** The record route's answer, or null for a decision no request of the mock made. */
export function getDecision(requestId: string, seq: number): DecisionRecord | null {
  return DECISIONS.get(`${requestId}/${seq}`) ?? null;
}
