import type {
  Account, AttemptWindow, CohortHold, DecisionKind, DecisionRecord, DecisionSummary, PinReason, Placement, PrefixEntry, RecordBudget, RecordCandidate, RecordCompletion, RecordCooldown, RecordOutcome,
  RecordPin, RecordRequest, RecordSettings, RequestLog, RoutingEvent, RoutingLogPage, RoutingLogQuery, Score, SummaryFact, WalkFact, WalkStep,
} from '@/api/types';
import { ACCOUNTS } from './accounts';
import { KEY_REVOKED_AT, poolById, type PoolSeed } from './pools';
import { BUDGET_CROSSING_ID, HAIKU, OVERAGE_FROM, REQUEST_EVENTS, REQUESTS, SHED_ROUND, STORY_HOUR_FROM, STORY_IDS, clock, failsOver, oauthWindows, requestById } from './requests';
import { createRng } from './rng';
import { DAY, HOUR, MINUTE, NOW, iso } from './time';

/** Why a move rule fired, as the trace's `moved` names it. A failover's reasons move the pin from the task instead. */
const RULES = ['spent', 'overage', 'return_to_plan', 'drain', 'shed'] as const;
const FAILOVER_REASONS = ['cooldown', 'fast_lane'];
/** The line under which a destination of a fired rule reads cool. */
const MIGRATE_BAND = 0.05;
const PLAN_WINDOWS = ['five_hour', 'seven_day'] as const;
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
   * Set where every member held the request at its cap until the bound ran out: the account it waited on, or null
   * for none. The router answers it on the bound path rather than walking a placement.
   */
  boundOn?: string | null;
  exhaustion?: RecordOutcome['exhaustion'];
  /** A summary fact the story names where the router's own reading is not what the row tells. */
  fact?: SummaryFact[];
  /** The pin event reason of a failover placement, which moves the pin from the task rather than by a rule. */
  failover?: string;
  /** The account the pin was on before a move or a failover. */
  from?: string;
  /** The holds before this attempt's placement, summed as the placement's row names them. */
  heldBefore?: CohortHold;
  hold?: { cohort: string | null; place: 'cohort' | 'session_pin' | 'strategy_pick' };
  /** The holds the request waited through before this decision, each with the attempt that waited. */
  holds?: RecordRequest['holds'];
  /** The slots the story fills where a hold reads every member at its cap. */
  inFlight?: Record<string, number>;
  kind: DecisionKind;
  /** The pin as step 1 found it, or why none was looked at. */
  pin: { cause: string; idleMs: number; warmth: 'cold' | 'refresh' | 'untouched' } | 'none' | PinReason;
  /** Per member, the deepest entry per rung and its match, where the story sets a deeper refused entry beside one. */
  prefix?: Record<string, Partial<RecordCandidate['prefix']>>;
  refusal?: { kind: string; reason: string };
  request: RequestLog;
  round?: ShedRound;
  rule?: string;
  rung?: Placement | null;
  seq: number;
}

interface ShedRound {
  marks: { agentId: string; keyId: string; model: string; sessionId: string }[];
  pins: { agentId: string; burn: number; cold: boolean; lastInputTokens: number; marked: boolean; model: string; note: string | null; sessionId: string }[];
}

/** The pool's settings as the record names them, by the pool fields they are read from. */
function settingsOf(pool: PoolSeed): RecordSettings {
  return {
    affinityMode: pool.affinityMode,
    affinityTtlSeconds: pool.affinityTtlSeconds,
    burnWindowSeconds: pool.burnWindowSeconds,
    inFlightCap: pool.inFlightCap,
    migrateBand: MIGRATE_BAND,
    migrateWhenSpent: pool.migrateWhenSpent,
    minSharedPrefixTokens: pool.minSharedPrefixTokens,
    overageMoveGraceMinutes: pool.overageMoveGraceMinutes,
    overagePolicy: pool.overagePolicy,
    paceFloor: pool.paceFloor,
    poolUpdatedAt: pool.updatedAt,
    preferSubscription: pool.preferSubscription,
    retryInPlaceSeconds: pool.retryInPlaceSeconds,
    selectBelowUtilization: pool.selectBelowUtilization,
    sessionPlacement: pool.sessionPlacement,
    shedAheadOfPace: pool.shedAheadOfPace,
    shedShare: pool.shedShare,
    sourceDwellSeconds: pool.sourceDwellSeconds,
    strategy: pool.strategy,
    subagentPlacement: pool.subagentPlacement,
    warmSessions: pool.warmSessions,
    windDownMinutes: pool.windDownMinutes,
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
  const attempt = dispatchedAttempt(spec);
  if (attempt?.accountId === accountId && attempt.upstreamStatus !== 429) return attempt.windows;
  return oauthWindows(accountId, spec.at, () => OLDER_FACTOR);
}

/** The window a 429's cooldown names: the spent plan window its head read, else the retry-after. */
const windowOfReason = (reason: string) => (reason.startsWith('429 on 5h') ? 'five_hour' : reason.startsWith('429 on 7d') ? 'seven_day' : 'retry_after');

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
    a.cooldowns.map((c) => ({ accountId: a.id, reason: c.reason.startsWith('rate_limited') ? 'rate_limited' : c.reason, scope: c.scope, setAt: Date.parse(c.setAt), until: Date.parse(c.until), window: c.scope === 'fast' ? 'fast' : c.windowId === '5h' ? 'five_hour' : c.scope.startsWith('model:') ? 'model_rejection' : (c.windowId ?? 'retry_after') })),
  ),
  ...REQUESTS.flatMap((r) =>
    r.attempts.flatMap((a) => {
      if (!a.cooldownSet) return [];
      const setAt = Date.parse(a.startedAt) + (a.durationMs ?? 0);
      const until = Date.parse(a.cooldownSet.until);
      const reason = a.cooldownSet.reason;
      if (reason === 'model_rejected') return [{ accountId: a.accountId, reason, scope: `model:${r.canonicalModel}`, setAt, until, window: 'model_rejection' }];
      if (reason === 'fast_rate_limit') return [{ accountId: a.accountId, reason, scope: 'fast', setAt, until, window: 'fast' }];
      return [{ accountId: a.accountId, reason: 'rate_limited', scope: 'account', setAt, until, window: windowOfReason(reason) }];
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
  if (state !== 'ready' || !a.routingEnabled) return { name: 'operator', reason: state, until: null, window: null };
  const endsAt = a.subscription?.endsAt ? Date.parse(a.subscription.endsAt) : null;
  if (endsAt !== null && endsAt <= at) return { name: 'subscription_ended', reason: null, until: null, window: null };
  const live = COOLDOWNS.filter((c) => c.accountId === a.id && c.setAt <= at && at < c.until && holds(c, model, fast));
  const cooldown = (c: CooldownRow) => ({ name: 'cooldown', reason: c.reason, until: iso(c.until), window: c.window });
  const blocking = live.find((c) => c.scope !== 'fast');
  if (blocking) return cooldown(blocking);
  const credits = fast ? creditsAt(a.id, at) : null;
  if (credits?.blocked) return { name: 'credits', reason: credits.blocked, until: null, window: null };
  const reached = budgetsAt(pool, a.id, at).find((b) => b.spentUsd !== null && b.spentUsd >= b.budgetUsd);
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

/** Every pool member's standing at the decision's time, eligible or not. */
function candidatesOf(spec: Spec, pool: PoolSeed): RecordCandidate[] {
  const r = spec.request;
  const fast = r.shape.requestedSpeed === 'fast';
  const at = spec.at;
  return pool.members.map((m) => {
    const a = accountOf.get(m.accountId)!;
    const overage = inOverageAt(a.id, at);
    const windows: RecordCandidate['windows'] = {};
    if (a.kind === 'oauth') {
      const head = r.attempts.some((x) => x.accountId === a.id) || a.id === 'primary';
      const ageMs = head ? 4_000 : 120_000;
      for (const w of readingsOf(spec, a.id)) {
        const name = WINDOW_OF[w.id];
        if (name === undefined || w.utilization === null) continue;
        const reset = w.resetsAt === null ? null : Date.parse(w.resetsAt);
        const elapsed = reset === null ? 0 : Math.min(1, Math.max(0, 1 - (reset - at) / WINDOW_MS[name]));
        windows[name] = { ageMs, ahead: name === 'seven_day_fable' ? null : Math.round((w.utilization - elapsed) * 1000) / 1000, observedAt: iso(at - ageMs), reset: w.resetsAt, source: head ? 'head' : 'poll', utilization: w.utilization };
      }
    } else {
      windows.worst_bucket = { ageMs: 4_000, ahead: null, observedAt: iso(at - 4_000), reset: iso(at + 40_000), source: 'head', utilization: a.id === 'overflow' ? 0.04 : 0.1 };
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
    return {
      accountId: a.id,
      allowanceKnown: a.kind === 'api_key' || windows.five_hour !== undefined,
      budget,
      cooldowns,
      credits: fast ? creditsAt(a.id, at) : null,
      enabled: a.routingEnabled,
      inFlight: spec.inFlight?.[a.id] ?? drawnInFlight,
      inFlightCap: pool.inFlightCap,
      inOverage: overage,
      incarnation: 1,
      kind: a.kind,
      obstacle,
      overage: overage ? { observedAt: iso(at - 40_000), reset: a.overage?.reset ?? null, since: iso(OVERAGE_FROM), source: 'head' } : null,
      pace: pool.strategy === 'pace' ? { deadline: deadlineMs === null ? null : iso(deadlineMs), end: endsAt === null ? null : iso(endsAt), hours: hours === null ? null : Math.round(hours * 10) / 10, remaining, reset, score: hours === null ? remaining : Math.round((remaining / hours) * 100_000) / 100_000, window: a.kind === 'api_key' ? null : windowName } : null,
      partition: null,
      planTier: sub?.rateLimitTier ?? null,
      prefix: { ...NO_PREFIX, ...spec.prefix?.[a.id] },
      priority: m.priority,
      rank: null,
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

/** The story's one shedding round, on secondary at 11:00, which every cool line reads its last round from. */
const ROUNDS = [SHED_ROUND];
/** The newest round any pool ran on the account by `at`, as the cool line's `last_shed_age_ms` reads it. */
const newestRound = (accountId: string, at: number): number | null =>
  ROUNDS.filter((x) => x.accountId === accountId && x.at <= at).reduce<number | null>((newest, x) => (newest === null || x.at > newest ? x.at : newest), null);

/** The two shared windows' leads over pace with their elapsed shares, and the larger lead's margin over `against`. */
function leadFacts(c: RecordCandidate, against: number): WalkFact[] {
  const facts: WalkFact[] = [];
  const leads: number[] = [];
  for (const [name, short] of [['five_hour', 'five_hour'], ['seven_day', 'seven_day']] as const) {
    const w = c.windows[name];
    if (!w || w.ahead === null) continue;
    leads.push(w.ahead);
    facts.push(fact(`ahead_${short}`, w.ahead, 'percent'), fact(`elapsed_${short}`, Math.round((w.utilization - w.ahead) * 1000) / 1000, 'percent'));
  }
  if (leads.length) facts.push(fact('margin', Math.round((Math.max(...leads) - against) * 1000) / 1000, 'percent'));
  return facts;
}

/** A walk of the five rules: the lines, the rule that fired, its destinations after the cool preference and before. */
interface Move {
  destinations: RecordCandidate[];
  fired: string | null;
  lines: WalkStep[];
  reached: RecordCandidate[];
}

/**
 * Step 4 as the engine walks it for a live pin, each rule on the facts the record holds: `no` with the fact that kept
 * it, or `fired` with its destinations, `not_run` for every rule after it and the cool reading the destinations were
 * preferred by. A rule whose reason holds with no sibling to take the pin says so and the walk goes on. A destination
 * is an eligible sibling holding allowance. A cool one reads both leads under the band, no round within the dwell and
 * room under the cap, and a move prefers the cool ones where any is.
 */
function moveOf(spec: Spec, candidates: RecordCandidate[], pool: PoolSeed, source: RecordCandidate | undefined, pin: RecordPin): Move {
  const r = spec.request;
  const at = spec.at;
  const fast = r.shape.requestedSpeed === 'fast';
  const preferPlan = pool.overagePolicy === 'prefer_plan';
  const threshold = pool.shedAheadOfPace;
  const band = threshold === null ? null : threshold - MIGRATE_BAND;
  const dwellMs = pool.sourceDwellSeconds * 1_000;
  // A model with a week of its own draws on it and on both shared windows.
  const windows: string[] = r.canonicalModel.includes('fable') ? [...PLAN_WINDOWS, 'seven_day_fable'] : [...PLAN_WINDOWS];
  const holdsAllowance = (c: RecordCandidate) => c.kind === 'api_key' || windows.every((w) => (c.windows[w]?.utilization ?? 0) < 1);
  const allowanceKnown = (c: RecordCandidate) => c.kind === 'api_key' || windows.every((w) => c.windows[w] !== undefined);
  const rested = (c: RecordCandidate) => {
    const newest = newestRound(c.accountId, at);
    return newest === null || at - newest >= dwellMs;
  };
  const under = (c: RecordCandidate) => (c.kind === 'api_key' ? (c.windows.worst_bucket?.utilization ?? 0) < 1 : band === null || PLAN_WINDOWS.every((w) => (c.windows[w]?.ahead ?? -1) < band));
  const cool = (c: RecordCandidate) => under(c) && rested(c) && c.inFlight < c.inFlightCap;
  const coolFacts = (c: RecordCandidate) => {
    const newest = newestRound(c.accountId, at);
    const readings = c.kind === 'api_key' ? [fact('worst_bucket', c.windows.worst_bucket?.utilization ?? null, 'percent')] : leadFacts(c, band ?? 0);
    return [...readings, fact('band', c.kind === 'api_key' ? 1 : band, 'percent'), ...(newest === null ? [] : [fact('last_shed_age_ms', at - newest, 'ms')]), fact('in_flight', `${c.inFlight}/${c.inFlightCap}`, 'count'), fact('cool', cool(c))].map((f) => ({ ...f, accountId: c.accountId }));
  };
  const siblings = candidates.filter((c) => c.obstacle === null && c.accountId !== source?.accountId);
  const lines: WalkStep[] = [];
  const no = (i: number, facts: WalkFact[]) => lines.push(step(`move:${RULES[i]}`, 'no', facts));
  // The rule fires where any sibling it keeps holds allowance. Otherwise its line says it found none.
  const tryRule = (i: number, facts: WalkFact[], keep: (c: RecordCandidate) => boolean): Move | null => {
    const reached = siblings.filter((c) => keep(c) && holdsAllowance(c));
    if (reached.length === 0) {
      no(i, [...facts, fact('no_destination', true)]);
      return null;
    }
    const preferred = threshold === null ? [] : reached.filter(cool);
    const destinations = preferred.length ? preferred : reached;
    lines.push(step(`move:${RULES[i]}`, 'fired', [...facts, fact('destinations', destinations.map((c) => c.accountId))]));
    for (const later of RULES.slice(i + 1)) lines.push(step(`move:${later}`, 'not_run'));
    if (threshold !== null) lines.push(step('cool', preferred.length ? 'preferred' : 'fell_through', reached.flatMap(coolFacts)));
    return { destinations, fired: RULES[i], lines, reached };
  };
  const none: Move = { destinations: [], fired: null, lines, reached: [] };

  if (fast) no(0, [fact('fast', true)]);
  else if (!pool.migrateWhenSpent) no(0, [fact('migrate_when_spent', false)]);
  else if (!source || holdsAllowance(source)) no(0, [fact('holds_allowance', true)]);
  else {
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
  if (source?.kind !== 'api_key') no(2, [fact('kind', source?.kind ?? 'unknown')]);
  else {
    const dwellAge = at - Date.parse(pin.createdAt ?? iso(at));
    const facts = [fact('cold', cold), fact('dwell_age_ms', dwellAge, 'ms')];
    if (!(cold || dwellAge >= dwellMs)) no(2, facts);
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

  if (threshold === null) {
    no(4, [fact('threshold', 'off')]);
    return none;
  }
  const facts = [...(source ? leadFacts(source, threshold) : []), fact('threshold', threshold, 'percent')];
  if (!source || !PLAN_WINDOWS.some((w) => (source.windows[w]?.ahead ?? -1) >= threshold)) {
    no(4, facts);
    return none;
  }
  facts.push(...(spec.round ? [fact('round', 'ran'), fact('marks', spec.round.marks.map((m) => `${m.keyId}/${m.sessionId}/${m.agentId}/${m.model}`))] : [fact('round', 'within_dwell')]));
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
  return tryRule(4, facts, cool) ?? none;
}

const tierKey = (t: [number, number, number]) => t.join('/');

/**
 * The pick among the eligible members of the best tier, or among a move's destinations: the chosen account first, the
 * rest by score.
 */
function pickOf(candidates: RecordCandidate[], chosen: string | null, strategy: string, source: string | undefined, among?: string[]): { runnerUp: Score | null; score: Score | null; tierSize: number } {
  const eligible = candidates.filter((c) => c.obstacle === null && c.accountId !== source && (among === undefined || among.includes(c.accountId)));
  const winner = candidates.find((c) => c.accountId === chosen);
  if (!winner) return { runnerUp: null, score: null, tierSize: 0 };
  const tier = eligible.filter((c) => tierKey(c.tier) === tierKey(winner.tier));
  const unit = (c: RecordCandidate) => (c.kind === 'api_key' ? 'percent' : 'percent_per_hour');
  const scoreOf = (c: RecordCandidate) => c.pace?.score ?? 0;
  const rest = tier.filter((c) => c !== winner).sort((x, y) => scoreOf(y) - scoreOf(x));
  // The trace names who the strategy chose, so the record reads the chosen account's score at the top of its tier.
  const top = Math.max(scoreOf(winner), ...rest.map((c) => scoreOf(c) + 0.0001));
  winner.rank = 1;
  winner.score = strategy === 'pace' ? top : null;
  rest.forEach((c, i) => {
    c.rank = i + 2;
    c.score = strategy === 'pace' ? scoreOf(c) : null;
  });
  if (strategy !== 'pace') return { runnerUp: null, score: null, tierSize: tier.length };
  const best = rest[0];
  return { runnerUp: best ? { accountId: best.accountId, score: scoreOf(best), unit: unit(best) } : null, score: { accountId: winner.accountId, score: top, unit: unit(winner) }, tierSize: tier.length };
}

/** The five partitions' facts in the engine's order, and the members left after them, which the step's verdict reads. */
function narrowed(set: RecordCandidate[], pool: PoolSeed, r: RequestLog): { facts: WalkFact[]; left: RecordCandidate[] } {
  const facts: WalkFact[] = [];
  let left = set;
  const narrow = (key: string, keep: (c: RecordCandidate) => boolean) => {
    const kept = left.filter(keep);
    if (kept.length > 0) {
      facts.push(fact(key, left.length - kept.length, 'count'));
      left = kept;
    } else facts.push(left.length > 0 ? fact(key, 'fell_through') : fact(key, 0, 'count'));
  };
  if (r.canonicalModel.includes('fable')) {
    // A model with a week of its own draws on both shared windows too. A member spent on any of them is out rather
    // than passed over, so this partition never falls through. An `allow` pool keeps one whose extra usage the record
    // reads as unblocked, and a member without a credits reading is not funded.
    const spent = (c: RecordCandidate) => [...PLAN_WINDOWS, 'seven_day_fable'].some((w) => (c.windows[w]?.utilization ?? 0) >= 1);
    const funded = (c: RecordCandidate) => pool.overagePolicy === 'allow' && c.credits !== null && c.credits.blocked === null;
    const kept = left.filter((c) => !spent(c) || funded(c));
    facts.push(fact('spent', left.length - kept.length, 'count'));
    left = kept;
  } else facts.push(fact('spent', 'off'));
  // A token count takes no slot, so no cap holds it. With every member at its cap the request is held for a slot and
  // the set stays whole for the pick to name the account it waits on.
  const underCap = left.filter((c) => r.kind === 'count_tokens' || c.inFlight < c.inFlightCap);
  if (underCap.length === 0 && left.length > 0) facts.push(fact('at_cap', 'held'));
  else narrow('at_cap', (c) => underCap.includes(c));
  const ceiling = pool.selectBelowUtilization;
  if (ceiling === null) facts.push(fact('above_ceiling', 'off'));
  else narrow('above_ceiling', (c) => c.windows.five_hour === undefined || c.windows.five_hour.utilization < ceiling);
  narrow('winding_down', (c) => !c.term?.windingDown);
  if (pool.strategy !== 'pace') facts.push(fact('floor', 'off'));
  else narrow('floor', (c) => (c.pace?.remaining ?? 1) > pool.paceFloor);
  return { facts, left };
}

/**
 * The five partition facts over `set` as the engine narrows it: each partition keeps the members that pass and counts
 * the rest. A spent member drops even when none is left. Any other partition that would empty a non-empty set keeps it
 * whole and reads `fell_through` (`held` for the cap, as the engine words it), and a partition the pool's settings
 * switch off reads `off`.
 */
export function partitionsOf(set: RecordCandidate[], pool: PoolSeed, r: RequestLog): WalkFact[] {
  return narrowed(set, pool, r).facts;
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

/** What the router recorded for one decision: the record and the row the list route answers. */
function build(spec: Spec): { record: DecisionRecord; summary: DecisionSummary } {
  const r = spec.request;
  const pool = poolById.get(r.poolId ?? '')!;
  const fast = r.shape.requestedSpeed === 'fast';
  const candidates = spec.kind === 'refused' ? [] : candidatesOf(spec, pool);
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
    pin = { accountId: null, createdAt: null, epoch: null, found: 'none', idleMs: null, incarnation: null, lane: null, lastRefreshAt: null, lastUsedAt: null, marked: false, refreshAgeMs: null, released: [], shed: null, warmth: null, warmthCause: null };
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
    pin = {
      accountId: found.accountId, createdAt: iso(found.createdAt), epoch: found.epoch, found: 'live', idleMs, incarnation: 1, lane: laneOf(r), lastRefreshAt: iso(spec.at - idleMs), lastUsedAt: iso(spec.at - idleMs),
      marked: spec.rule === 'shed', refreshAgeMs: idleMs, released, shed: null, warmth: spec.pin.warmth, warmthCause: spec.pin.cause,
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
  if (spec.boundOn !== undefined) walk.push(step('bound', 'exhausted', spec.boundOn === null ? [] : [fact('account_id', spec.boundOn, null, spec.boundOn)]));
  // A live pin a portable request still reaches walks the five rules, and a fired rule names its destinations. A token
  // count follows its pin, since it writes none a move could leave, and a pin past its account's end is released at
  // renewal instead.
  let movedAmong: string[] | undefined;
  if (pin?.found === 'live' && portable && !spec.failover && (spec.kind === 'renewed' || spec.kind === 'moved')) {
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
      walk.push(...move.lines);
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
  const placing = (spec.kind === 'placed' || spec.kind === 'moved' || spec.kind === 'held' || spec.kind === 'exhausted') && spec.boundOn === undefined;
  if (placing) {
    const set = candidates.filter((c) => (movedAmong ? movedAmong.includes(c.accountId) : c.obstacle === null));
    const partitions = narrowed(set, pool, r);
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
      const ranked = pickOf(candidates, spec.accountId, pool.strategy, spec.kind === 'moved' ? spec.from : undefined, movedAmong);
      if (spec.kind !== 'held') pick = ranked;
      tierSize = ranked.tierSize;
      const tier = member(spec.accountId)?.tier ?? [0, 0, 0];
      walk.push(step('pick', 'chosen', [fact('strategy', pool.strategy), fact('tier', tier.join('/')), ...candidates.filter((c) => c.score !== null).map((c) => fact('score', c.score, c.kind === 'api_key' ? 'percent' : 'percent_per_hour', c.accountId))]));
    }
  }
  if (spec.kind === 'held' && spec.hold) {
    walk.push(step('hold', 'held', [fact('place', spec.hold.place), fact('account_id', spec.accountId, null, spec.accountId), fact('bound_ms', pool.retryInPlaceSeconds * 1_000, 'ms')]));
  }
  const exhaustion = spec.kind === 'exhausted' ? (spec.exhaustion ?? exhaustionOf(candidates)) : null;
  if (exhaustion) {
    walk.push(step('exhausted', 'exhausted', [fact('kind', exhaustion.kind), fact('deadline', exhaustion.deadline === null ? null : Date.parse(exhaustion.deadline), 'time'), fact('window', exhaustion.window), fact('obstacles', candidates.map((c) => c.obstacle?.name ?? 'eligible'))]));
  }

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
  const hold = spec.kind === 'held' && spec.hold ? { accountId: spec.accountId ?? '', boundMs: pool.retryInPlaceSeconds * 1_000, cohort: spec.hold.cohort } : null;
  const outcome: RecordOutcome = {
    accountId: spec.accountId,
    cohortHold: spec.heldBefore ?? null,
    completion,
    exhaustion,
    fromAccountId: spec.kind === 'moved' || spec.failover ? (spec.from ?? null) : null,
    hold,
    inOverage: chosen?.inOverage ?? false,
    kind: spec.kind,
    pinEvents,
    pinWritten,
    refusal: spec.refusal ?? null,
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
      agentId: r.agentId ?? 'main', at: iso(spec.at), attempt: spec.attempt, attempted: spec.attempted, canonicalModel: r.canonicalModel,
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
    version: 1,
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

/** The row's By and Fact, by the router's own rules for the summary it writes beside the record. */
function byAndFact(spec: Spec, record: DecisionRecord, pick: ReturnType<typeof pickOf>, pool: PoolSeed): { by: DecisionSummary['by']; fact: SummaryFact[] } {
  const by = (kind: DecisionSummary['by']['kind'], value: string, tier: [number, number, number] | null = null) => ({ kind, tier, value });
  const r = spec.request;
  const placementFact = (): SummaryFact[] => {
    const rung = record.outcome.rung;
    if (rung?.rung === 'with_main') return [];
    if (rung) return [summaryFact('prefix', rung.prefixTokens, 'tokens'), summaryFact('depth', rung.depth, 'count')];
    const others = record.candidates.filter((c) => c.accountId !== spec.accountId);
    if (others.length > 0 && others.every((c) => record.request.attempted.includes(c.accountId))) return [summaryFact('pick', 'last account', 'text')];
    if (pick.tierSize <= 1) return [summaryFact('pick', 'only candidate', 'text')];
    if (pool.strategy === 'pace' && pick.score && pick.runnerUp) {
      return [summaryFact('gap', Math.round((pick.score.score - pick.runnerUp.score) * 100_000) / 100_000, pick.score.unit as SummaryFact['unit']), { entity: { id: pick.runnerUp.accountId, kind: 'account' }, key: 'over', unit: null, value: pick.runnerUp.accountId }];
    }
    return [summaryFact('rank', `1 of ${pick.tierSize}`, 'text')];
  };
  const inFlight = (): SummaryFact[] => {
    const m = record.candidates.find((c) => c.accountId === spec.accountId);
    return m ? [summaryFact('in_flight', `${m.inFlight}/${m.inFlightCap}`, 'text')] : [];
  };
  switch (spec.kind) {
    case 'renewed': {
      if (r.kind === 'probe') return { by: by('none', ''), fact: [summaryFact('epoch', record.pin?.epoch ?? 0, 'count')] };
      if (record.pin?.warmth === 'cold') return { by: by('none', ''), fact: [summaryFact('cause', record.pin.warmthCause ?? '', 'text')] };
      const m = record.candidates.find((c) => c.accountId === spec.accountId);
      const threshold = pool.shedAheadOfPace;
      const hottest = threshold === null || !m ? undefined : PLAN_WINDOWS.map((w) => [w, m.windows[w]] as const).filter(([, w]) => w?.ahead !== null && w?.ahead !== undefined && w.ahead >= threshold).sort((x, y) => (y[1]?.ahead ?? 0) - (x[1]?.ahead ?? 0))[0];
      if (hottest?.[1]) return { by: by('none', ''), fact: [summaryFact(hottest[0], hottest[1].utilization, 'percent')] };
      return { by: by('none', ''), fact: [summaryFact('idle', record.pin?.idleMs ?? 0, 'ms')] };
    }
    case 'moved': {
      const noneCool = record.walk.some((w) => w.step === 'cool' && w.verdict === 'fell_through');
      return { by: by('rule', record.outcome.rule ?? ''), fact: noneCool ? [summaryFact('pick', 'none cool', 'text')] : placementFact() };
    }
    case 'placed':
      if (spec.heldBefore) return { by: by('hold', spec.heldBefore.cohort ? 'cohort' : 'strategy_pick'), fact: [summaryFact('held', spec.heldBefore.heldMs, 'ms')] };
      return { by: record.outcome.rung ? by('rung', record.outcome.rung.rung) : by('strategy', pool.strategy, record.outcome.tier), fact: placementFact() };
    case 'held':
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
  // A hold on the cohort's account waits on that account at its cap. One on the strategy's pick, and a request the
  // bound ran out on, waited with every member at its cap.
  const atCap = (accountId: string | null) => (accountId === null || pool === undefined ? {} : { [accountId]: pool.inFlightCap });
  const allAtCap = () => Object.fromEntries((pool?.members ?? []).map((m) => [m.accountId, pool?.inFlightCap ?? 0]));
  const holds: RecordRequest['holds'] = [];
  // Every member was at its cap until the bound ran out, so the router answered the request without a dispatch.
  const ranOut = (seq: number, attempt: number, onAt: number, attempted: string[]) => {
    const waitedOn = r.held?.waitedOn[r.held.waitedOn.length - 1] ?? null;
    const bound = (pool?.retryInPlaceSeconds ?? 60) * 1_000;
    if (waitedOn !== null) {
      specs.push({ ...base, accountId: waitedOn, at: onAt, attempt, attempted, fact: [summaryFact('answer', 429, 'count'), summaryFact('held', bound, 'ms')], hold: { cohort: null, place: 'strategy_pick' }, holds: [...holds], inFlight: allAtCap(), kind: 'held', pin: noPin, seq });
      holds.push({ accountId: waitedOn, attempt, cohort: null, heldMs: bound });
    }
    specs.push({ ...base, accountId: null, at: onAt + bound, attempt, attempted: waitedOn ? [...attempted, waitedOn] : attempted, boundOn: waitedOn, exhaustion: { deadline: null, kind: 'untimed', obstacle: null, reason: null, window: null }, holds: [...holds], inFlight: allAtCap(), kind: 'exhausted', pin: noPin, seq: seq + 1 });
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
      specs.push({ ...base, accountId: heldOn, at: start - hold.heldMs, attempt: i + 1, attempted, hold: { cohort: hold.cohort, place: hold.cohort ? 'cohort' : 'strategy_pick' }, holds: [...holds], inFlight: hold.cohort ? atCap(heldOn) : allAtCap(), kind: 'held', pin: noPin, seq });
      holds.push({ accountId: heldOn, attempt: i + 1, cohort: hold.cohort, heldMs: hold.heldMs });
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
    const heldBefore = holds.length ? { cohort: holds[holds.length - 1].cohort, heldMs: holds.reduce((n, h) => n + h.heldMs, 0), waitedOn: holds.map((h) => h.accountId) } : undefined;
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
 * The 11:00 round's four pins on secondary in its order: the cold ones by burn, the warm ones, the last unmarked. Each
 * holds Sonnet 5, the one model secondary serves. A burn is a share of the 5-hour window a second, as the engine reads
 * a pin's: the first pin's is 7.1% an hour.
 */
const ROUND_PINS: ShedRound['pins'] = [
  { agentId: 'c21e', burn: 0.071 / 3_600, cold: true, lastInputTokens: 64_000, marked: true, model: 'claude-sonnet-5', note: null, sessionId: sessionId('76ddb35b') },
  { agentId: 'main', burn: 0.034 / 3_600, cold: true, lastInputTokens: 22_000, marked: true, model: 'claude-sonnet-5', note: null, sessionId: sessionId('0d588de5') },
  { agentId: 'e90b', burn: 0.142 / 3_600, cold: false, lastInputTokens: 88_000, marked: true, model: 'claude-sonnet-5', note: null, sessionId: sessionId('c64358b5') },
  { agentId: 'e90b', burn: 0.052 / 3_600, cold: false, lastInputTokens: 41_000, marked: false, model: 'claude-sonnet-5', note: 'current', sessionId: sessionId('407dd39b') },
];

/**
 * The story hour and the morning's round, written by hand so every card the routing log draws is on the page: each
 * decision names its kind, its account and what the engine read where the walk back alone would not tell it.
 */
function storySpecs(): Spec[] {
  const t = clock;
  const at = (id: string, seq: number, attempt: number, when: number, kind: DecisionKind, accountId: string | null, extra: Partial<Spec> = {}): Spec => ({ accountId, at: when, attempt, attempted: [], kind, pin: 'none', request: requestById.get(id)!, seq, ...extra });
  const live = (cause = 'continues_pin', idleMs = 60_000, warmth: 'cold' | 'refresh' | 'untouched' = 'refresh') => ({ cause, idleMs, warmth });
  const mainStatic = (when: number, writtenAt: number, refusedBy: string | null = null) => entry(staticBoundary('main', 2), { at: when, depth: 2, prefixTokens: 31_200, refusedBy, ttlMs: ONE_HOUR, writtenAt });
  const cohort = (when: number, refusedBy: string | null = null) => entry(staticBoundary('e90b', 1), { at: when, depth: 1, prefixTokens: 18_400, refusedBy, ttlMs: FIVE_MINUTES, writtenAt: t(14, 19, 52) });
  const message = (session: string, depth: number, prefixTokens: number, when: number, writtenAt: number, ttlMs: number) => entry(`message|${sessionId(session)}|${depth}`, { at: when, depth, prefixTokens, ttlMs, writtenAt });
  const placedStatic: Placement = { depth: 2, prefixTokens: 31_200, rung: 'static' };
  const round: ShedRound = { marks: ROUND_PINS.filter((p) => p.marked).map((p) => ({ agentId: p.agentId, keyId: 'desktop', model: p.model, sessionId: p.sessionId })), pins: ROUND_PINS };
  return [
    // Two of the round's sessions placed on secondary just before it. Every pin a beat renews or moves is a request's.
    at('req_e5a1c093-4d7', 1, 1, t(10, 58, 59), 'placed', 'secondary'),
    at('req_f3b6d218-85a', 1, 1, t(10, 59, 10), 'placed', 'secondary'),
    at('req_0b7c2d94-e61', 1, 1, t(10, 59, 30), 'renewed', 'secondary', { pin: live('probe', 31_000, 'untouched') }),
    // Secondary's 5-hour window opened at 10:42 and leads its pace past the pool's margin: this renewal runs the round.
    at('req_1eaa9bc1-a18', 1, 1, t(11, 0), 'renewed', 'secondary', { pin: live('continues_pin', 30_000), round }),
    // A marked pin moves at its next request inside the dwell, while the window still leads its pace by the margin.
    at('req_868264bc-611', 1, 1, t(11, 8, 30), 'moved', 'primary', {
      from: 'secondary',
      pin: live('continues_pin', 560_000),
      prefix: { secondary: { content: message('c64358b5', 10, 72_000, t(11, 8, 30), t(11, 6, 2), FIVE_MINUTES), contentMatch: null } },
      rule: 'shed',
    }),
    // The hour's sessions placed on secondary before its spent edge.
    at('req_a1f04c2e-7b3', 1, 1, t(14, 0, 10), 'placed', 'secondary'),
    at('req_b72e9d05-c14', 1, 1, t(14, 1, 30), 'placed', 'secondary'),
    at('req_c90d3f61-a28', 1, 1, t(14, 1, 35), 'placed', 'secondary'),
    at('req_d4e8a7b2-916', 1, 1, t(14, 1, 40), 'placed', 'secondary'),
    at('req_1d9c3e55-0b2', 1, 1, t(14, 2, 10), 'renewed', 'secondary', { pin: live('continues_pin', 120_000) }),
    at('req_1d9c3e55-0b2', 2, 2, t(14, 2, 11), 'placed', 'primary', {
      attempted: ['secondary'],
      failover: 'cooldown',
      from: 'secondary',
      pin: live(),
      prefix: { primary: { static: mainStatic(t(14, 2, 11), t(13, 58, 2)), staticMatch: mainStatic(t(14, 2, 11), t(13, 58, 2)) }, secondary: { content: message('bcc805b6', 8, 58_900, t(14, 2, 11), t(14, 0, 24), ONE_HOUR), contentMatch: null } },
      rung: placedStatic,
    }),
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
    // A token count follows its pin without walking the rules, to the spent secondary, whose 429 cools it until the
    // window resets. The count fails over to primary and writes no pin there.
    at('req_3b8e51f4-0a9', 1, 1, t(14, 9, 30), 'renewed', 'secondary', { pin: live('count', 480_000, 'untouched') }),
    at('req_3b8e51f4-0a9', 2, 2, t(14, 9, 31), 'placed', 'primary', { attempted: ['secondary'], failover: 'cooldown', from: 'secondary', pin: live('count', 1_000, 'untouched') }),
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
    at('req_6c3a9e27-5b1', 1, 1, t(14, 18, 5), 'refused', null, { pin: 'not_looked_up', refusal: { kind: 'key', reason: 'the key was revoked' } }),
    at('req_e0a4c6f2-118', 1, 1, t(14, 14, 2), 'refused', null, { pin: 'not_looked_up', refusal: { kind: 'client', reason: 'hash' } }),
    at('req_51d0e2b8-7a0', 1, 1, t(14, 16, 40), 'exhausted', null, { exhaustion: { deadline: iso(startOfDay(t(14, 16, 40)) + DAY), kind: 'timed', obstacle: null, reason: null, window: 'overage' }, pin: 'pinning_off' }),
    // The compacted conversation renews its pin cold.
    at('req_31f0b7c2-6e1', 1, 1, t(14, 17, 52), 'renewed', 'primary', { pin: live('compacted', 491_000, 'cold') }),
    at('req_9a07c4e1-d55', 1, 1, t(14, 19, 2), 'renewed', 'primary', { pin: live('continues_pin', 438_000) }),
    // A subagent request that carries no cache marker of its own pins nothing. Its cohort's static entry places it.
    at('req_69e30548-32e', 1, 1, t(14, 21, 12), 'held', 'primary', { hold: { cohort: 'primary', place: 'cohort' }, inFlight: { nightly: 16, primary: 16 }, pin: 'pinning_off', prefix: { primary: { static: cohort(t(14, 21, 12), 'at_cap'), staticMatch: null } } }),
    at('req_69e30548-32e', 2, 1, t(14, 21, 14), 'placed', 'primary', {
      heldBefore: { cohort: 'primary', heldMs: 2_000, waitedOn: ['primary'] },
      holds: [{ accountId: 'primary', attempt: 1, cohort: 'primary', heldMs: 2_000 }],
      inFlight: { nightly: 16, primary: 15 },
      pin: 'pinning_off',
      prefix: { primary: { static: cohort(t(14, 21, 14)), staticMatch: cohort(t(14, 21, 14)) } },
      rung: { depth: 1, prefixTokens: 18_400, rung: 'static' },
    }),
    at('req_2e7c90a1-6d4', 1, 1, t(14, 24, 9), 'placed', 'primary'),
    at('req_2e7c90a1-6d4', 2, 2, t(14, 24, 10), 'placed', 'nightly', { attempted: ['primary'], failover: 'cooldown', from: 'primary', pin: live() }),
    at('req_4b0e9d17-c31', 1, 1, t(14, 26, 58), 'placed', 'primary'),
    at('req_4b0e9d17-c31', 2, 2, t(14, 27, 1), 'placed', 'nightly', { attempted: ['primary'], failover: 'fast_lane', from: 'primary', pin: live() }),
    at('req_f8085653-422', 1, 1, t(14, 27, 28), 'renewed', 'primary', { pin: live('continues_pin', 506_000) }),
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

/** One Record v1 per decision of every mock request, keyed `${requestId}/${seq}`. */
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
  // A window's spent edge is the first head of any answer that reads it at its limit, once per account and window.
  const heads = REQUESTS.flatMap((r) => r.attempts.map((a) => ({ a, at: Date.parse(a.startedAt) + (a.durationMs ?? 0), r }))).sort((x, y) => x.at - y.at);
  const spent = new Set<string>();
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
      if (!a.cooldownSet || a.cooldownSet.reason === 'model_rejected') continue;
      const fast = a.cooldownSet.reason === 'fast_rate_limit';
      const until = Date.parse(a.cooldownSet.until);
      seeds.push({
        accountId: a.accountId,
        at: iso(answeredAt),
        detail: { reason: fast ? 'fast_rate_limit' : 'rate_limited', scope: fast ? 'fast' : 'account', source_request_id: r.id, until, window: fast ? 'fast' : windowOfReason(a.cooldownSet.reason) },
        kind: 'cooldown_set',
        requestId: r.id,
      });
    }
  }
  // A pin event names the epoch the record wrote, so the event and the card read one number.
  for (const seed of seeds) {
    if (seed.kind !== 'pin_migrated') continue;
    const s = SUMMARIES.find((x) => x.requestId === seed.requestId || (x.sessionId === seed.sessionId && x.at === seed.at && x.accountId === seed.accountId));
    const epoch = s && DECISIONS.get(`${s.requestId}/${s.seq}`)?.outcome.pinWritten?.epoch;
    if (epoch !== undefined && epoch !== null) seed.detail.epoch = epoch;
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
  const round = DECISIONS.get('req_1eaa9bc1-a18/1');
  const roundReading = round?.candidates.find((c) => c.accountId === 'secondary')?.windows.five_hour;
  const beforeReset = oauthWindows('primary', fiveHourReset - 1, () => OLDER_FACTOR).find((w) => w.id === '5h')?.utilization ?? null;
  const elapsed = roundReading ? Math.round((roundReading.utilization - (roundReading.ahead ?? 0)) * 1000) / 1000 : 0;
  const burnSum = 0.299 / 3_600;
  seeds.push(
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
    {
      accountId: 'secondary',
      agentId: 'e90b',
      at: iso(t(11, 0)),
      detail: {
        ahead: roundReading?.ahead ?? null, burn_sum: burnSum, burn_window_seconds: pool.burnWindowSeconds, dwell_seconds: pool.sourceDwellSeconds, dwell_until: t(11, 0) + pool.sourceDwellSeconds * 1_000, elapsed_share: elapsed,
        margin: roundReading?.ahead === null || roundReading?.ahead === undefined ? null : Math.round((roundReading.ahead - (pool.shedAheadOfPace ?? 0)) * 1000) / 1000,
        marks: round?.outcome.shedMarks?.map((m) => ({ agent_id: m.agentId, key_id: m.keyId, model: m.model, session_id: m.sessionId })) ?? [],
        persisted: true,
        pins: ROUND_PINS.map((p) => ({ agent_id: p.agentId, burn: p.burn, cold: p.cold, key_id: 'desktop', last_input_tokens: p.lastInputTokens, marked: p.marked, model: p.model, note: p.note, per_rewrite: p.burn / (p.lastInputTokens / 1_000), session_id: p.sessionId })),
        shed_share: pool.shedShare, target: burnSum * pool.shedShare, threshold: pool.shedAheadOfPace, unit: 'window_share', utilization: roundReading?.utilization ?? null, warm_sessions: pool.warmSessions, window: 'five_hour',
      },
      keyId: 'desktop',
      kind: 'shedding_round',
      model: 'claude-sonnet-5',
      poolId: 'all',
      requestId: 'req_1eaa9bc1-a18',
      sessionId: sessionId('407dd39b'),
    },
    { accountId: null, actor: 'admin', at: iso(t(9, 5, 12)), detail: { diff: { shed_ahead_of_pace: { from: null, to: pool.shedAheadOfPace } }, kind: 'pool_updated', target: 'all' }, kind: 'registry_changed', poolId: 'all' },
    // The ci key's revocation, after the last request it reached selection with.
    { accountId: null, actor: 'admin', at: iso(KEY_REVOKED_AT), detail: { diff: {}, kind: 'key_revoked', target: 'ci' }, keyId: 'ci', kind: 'registry_changed' },
    { accountId: 'primary', at: iso(t(9, 0, 41)), detail: { added: ['claude-opus-5-5'], count: 12, removed: [] }, kind: 'catalog_changed' },
    { accountId: null, at: iso(t(6, 12, 40)), detail: { affinity_entries_loaded: 34, down_since: t(6, 11, 58), holds_lost: true, pins_loaded: 10, shedding_marks_loaded: 0, shedding_rounds_stale: 0, version: '0.1.0', windows_loaded: 5 }, kind: 'router_started' },
    // Once each outside the hour.
    { accountId: 'primary', agentId: 'main', at: iso(t(13, 13, 40)), detail: { agent_id: 'main', reasons: ['expired'] }, keyId: 'laptop', kind: 'pin_expired', model: HAIKU, sessionId: sessionId('5eeed6ce') },
    { accountId: 'nightly', actor: 'admin', agentId: 'e90b', at: iso(t(12, 40, 5)), detail: { agent_id: 'e90b' }, keyId: 'desktop', kind: 'pin_released', model: 'claude-sonnet-5-5', sessionId: sessionId('407dd39b') },
    { accountId: 'primary', at: iso(Date.parse('2026-09-12T20:10:00Z') - Date.parse('2026-09-16T14:30:00Z') + NOW), detail: { since: Date.parse('2026-09-11T16:10:00Z') - Date.parse('2026-09-16T14:30:00Z') + NOW }, kind: 'overage_ended' },
    { accountId: 'legacy', at: iso(NOW - 2 * DAY - 2 * HOUR), detail: {}, kind: 'reauth_required' },
    { accountId: 'legacy', at: iso(NOW - 2 * DAY - 2 * HOUR), detail: { cause: 'refresh_failed', detail: 'refresh_token rejected: invalid_grant', from: 'ready', to: 'reauth_required' }, kind: 'account_state_changed' },
    { accountId: 'primary', at: iso(t(8, 15, 3)), detail: { attempts: 3, duration_ms: 25 * MINUTE, resumed_at: t(8, 15, 3) }, kind: 'poll_backoff_ended' },
    { accountId: 'research', at: iso(startOfDay(t(14, 30))), detail: { period: 'day', period_start: startOfDay(t(14, 30)) - DAY, previous_spent_usd: 18.2 }, kind: 'budget_period_ended', poolId: 'jobs' },
    // Drawn at the instant it fires: the subscription's end less the pool's wind-down, which is still ahead.
    { accountId: 'secondary', at: iso(endsAt - pool.windDownMinutes * MINUTE), detail: { end_source: 'cancelled', ends_at: endsAt, wind_down_minutes: pool.windDownMinutes }, kind: 'wind_down_started', poolId: 'all' },
  );
  if (oldestCooldown) seeds.push({ accountId: oldestCooldown.accountId, at: iso(oldestCooldown.until), detail: { cause: 'deadline', scope: oldestCooldown.scope }, kind: 'cooldown_cleared' });
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
  'account_state_changed', 'budget_period_ended', 'budget_reached', 'catalog_changed', 'cooldown_cleared', 'cooldown_set', 'fast_credits_changed', 'ledger_gap', 'model_permitted', 'model_rejected',
  'overage_ended', 'overage_started', 'pin_created', 'pin_expired', 'pin_migrated', 'pin_released', 'poll_backoff_ended', 'poll_backoff_started', 'reauth_required', 'refresh_failed',
  'refresh_succeeded', 'refresh_uncertain', 'registry_changed', 'router_started', 'shedding_round', 'warm_prefix_expired', 'wind_down_started', 'window_reset', 'window_spent',
];
/** The one event kind the route's default set leaves out beside the probe decisions. */
const HIDDEN_EVENT_KIND = 'warm_prefix_expired';

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
 * The merged page by the route's rules. The range runs from an hour back by default. The default kinds are every
 * decision but a probe's and every event but a warm prefix's expiry, and `kinds` names exactly what it lists, `probe`
 * for a probe's decision of any kind. `accounts` matches a decision's source too. `limit + 1` rows are read so a cursor
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
      (kinds === undefined ? !s.probe : (kinds.includes(s.kind) && !s.probe) || (kinds.includes('probe') && s.probe)),
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
      (kinds === undefined ? e.kind !== HIDDEN_EVENT_KIND : kinds.includes(e.kind)),
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
