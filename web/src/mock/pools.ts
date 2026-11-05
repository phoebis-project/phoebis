import { CACHE_TTL_DEFAULTS, KEEPALIVE_DEFAULTS, POOL_AFFINITY_DEFAULTS } from '@/api/client';
import type { AccountKind, ApiKey, BudgetPeriod, CacheTtl, Pool, PoolCacheTtl, PoolKeepalive, PoolMember, Range } from '@/api/types';
import { accountById } from './accounts';
import { paceDeadline, paceHours } from './lead';
import { round } from './round';
import { KEY_SAMPLE, POOL_SAMPLE } from './samples';
import { DAY, DELETED_HOLDER_END, MINUTE, NOW, ago, ahead, clock, daysAgo, endOfMonth, iso, startOfDay } from './time';

/**
 * A member as the story seeds it. The mock reads its kind, its eligibility, its burn, its Pace score and its speed off
 * the account, as the route does, and its pins off the recipes once they are drawn.
 */
type MemberSeed = Omit<PoolMember, 'activePins' | 'budgetUntil' | 'burn' | 'eligible' | 'kind' | 'paceScore' | 'speed'>;

/** The worst live bucket of each API-key account, which the router reads as its room. */
export const WORST_BUCKET: Readonly<Record<string, number>> = { annex: 0.02, overflow: 0.04, research: 0.1 };

/**
 * A member's three figures as the pools route serves them: the 7-day window's burn, the router's Pace score, the spare
 * rate on the 7-day window for an OAuth member (its remaining share per hour to the earlier of the reset and the end,
 * less the burn) and the remaining worst bucket for an API-key member, which burns no plan window, and the factor of
 * the account's slow-down where the member's pool has the switch on.
 */
export function servedFigures(accountId: string, now: number, slowDown: boolean): Pick<PoolMember, 'burn' | 'paceScore' | 'speed'> {
  const account = accountById.get(accountId);
  if (account === undefined) return { burn: null, paceScore: null, speed: null };
  if (account.kind === 'api_key') return { burn: null, paceScore: 1 - (WORST_BUCKET[accountId] ?? 0), speed: null };
  const speed = slowDown ? (account.slowDown?.factor ?? null) : null;
  const week = account.windows.find((w) => w.id === '7d');
  if (week === undefined) return { burn: null, paceScore: null, speed };
  const reset = week.resetsAt === null ? null : Date.parse(week.resetsAt);
  const hours = paceHours(paceDeadline(account, reset, now), now);
  const burn = week.burn ?? 0;
  return { burn: week.burn, paceScore: round((1 - week.utilization) / hours - burn, 5), speed };
}
/**
 * The route lists a pool's keys from the key registry, so the story seeds none on the pool itself. Its pins are its
 * members' sum, which the pool view reads, so the seed holds no figure of its own. Its guardrails are the attachments'
 * and its warnings the route's checks on its settings, both of which the pool view reads as the route does.
 */
export type PoolSeed = Omit<Pool, 'activePins' | 'guardrails' | 'keys' | 'members' | 'warnings'> & { members: MemberSeed[] };

const unbudgeted = (accountId: string, priority: number): MemberSeed => ({ accountId, budgetDailyUsd: null, budgetMonthlyUsd: null, priority, spends: [] });

/** The next UTC calendar boundary after `now`, which is where the ledger's spend period ends. */
function periodEndAfter(period: BudgetPeriod, now: number): string {
  return iso(period === 'day' ? startOfDay(now) + DAY : endOfMonth(now));
}

/** The instant the ci key was revoked, on the story's pinned day at 14:17:30. */
export const KEY_REVOKED_AT = clock(14, 17, 30);

type Resolved = NonNullable<Pool['keepaliveResolved']>;

const one = (value: number): Range => ({ max: value, min: value });
const span = (min: number, max: number): Range => ({ max, min });
/** The two ceilings left null read the break-even, as the router answers them under the default policy. */
const figures = (breakEven: Range, idle: Range, share: Range): Resolved => ({ breakEvenSeconds: breakEven, horizonSeconds: breakEven, idleSeconds: idle, idleShare: share, parkedSeconds: breakEven });

/**
 * The router's figures for one funding on one main TTL over the shipped tables, at the default policy of an hour of
 * idle window and both ceilings null. The mock holds no price, so they are written out here. A plan recipe breaks even
 * on the cost model's multipliers whatever its model: 44.1 hours on the hour, 3 hours 16 minutes on five minutes. The
 * dollar rows span the price table: Sonnet, Haiku and the older Opus rows break even first, Fable last, and on five
 * minutes the first rows' break-even lies under the hour of idle window, whose share reads above one there.
 */
const FIGURES: Record<'dollars' | 'plan', Record<CacheTtl, Resolved>> = {
  dollars: {
    '1h': figures(span(61_560, 255_960), one(3_600), span(0.0139, 0.0556)),
    '5m': figures(span(2_760, 11_760), one(3_600), span(0.3, 1.2)),
  },
  plan: {
    '1h': figures(one(158_760), one(3_600), one(0.0222)),
    '5m': figures(one(11_760), one(3_600), one(0.3)),
  },
};

const widen = (a: Range, b: Range): Range => span(Math.min(a.min, b.min), Math.max(a.max, b.max));
const merged = (a: Resolved, b: Resolved): Resolved => ({
  breakEvenSeconds: widen(a.breakEvenSeconds, b.breakEvenSeconds),
  horizonSeconds: widen(a.horizonSeconds, b.horizonSeconds),
  idleSeconds: widen(a.idleSeconds, b.idleSeconds),
  idleShare: widen(a.idleShare, b.idleShare),
  parkedSeconds: widen(a.parkedSeconds, b.parkedSeconds),
});

/**
 * A pool's figures from its members' kinds as the router composes them: an OAuth member brings the plan's figures on
 * the plan set's main TTL, an API-key member the dollar rows' on the metered set's, and a policy that leaves the
 * markers as sent brings both TTLs. Per-model overrides and model rules are not read, so a pool that narrows its models
 * reads wider here than the router answers it. Null with no member.
 */
export function composedFor(kinds: AccountKind[], cacheTtl: PoolCacheTtl): Pool['keepaliveResolved'] {
  const ttls = (main: CacheTtl): CacheTtl[] => (cacheTtl.enabled ? [main] : ['5m', '1h']);
  const parts = [
    ...(kinds.includes('oauth') ? ttls(cacheTtl.plan.main).map((t) => FIGURES.plan[t]) : []),
    ...(kinds.includes('api_key') ? ttls(cacheTtl.metered.main).map((t) => FIGURES.dollars[t]) : []),
  ];
  return parts.length === 0 ? null : parts.reduce(merged);
}

/**
 * What the route answers once a save sets `policy`: a nonzero idle window at its figure, above the break-even too, and
 * 0 at the break-even, the parked and horizon ceilings capped at the stored break-even with a null reading as it, and
 * the idle share moved with the window, since a window's probes cost in proportion to its length. An approximation: the
 * router keeps each combination apart, while this recomputes the low and the high end alone, so where the two ends come
 * from different combinations (a window of 0 on `jobs`, the five minutes' break-even at the low end beside the hour's
 * share) a save answers a share range the router would not. The break-even depends on prices alone and stays.
 */
export function resolvedFor(resolved: Pool['keepaliveResolved'], policy: PoolKeepalive): Pool['keepaliveResolved'] {
  if (resolved === null) return null;
  const be = resolved.breakEvenSeconds;
  const capped = (seconds: number | null): Range => (seconds === null ? be : { max: Math.min(seconds, be.max), min: Math.min(seconds, be.min) });
  const idle = policy.maxIdleSeconds === 0 ? be : one(policy.maxIdleSeconds);
  const share = (end: 'max' | 'min') => (resolved.idleSeconds[end] > 0 ? (resolved.idleShare[end] * idle[end]) / resolved.idleSeconds[end] : resolved.idleShare[end]);
  return { breakEvenSeconds: be, horizonSeconds: capped(policy.scheduledHorizonSeconds), idleSeconds: idle, idleShare: { max: share('max'), min: share('min') }, parkedSeconds: capped(policy.maxParkedSeconds) };
}

/** The day's traffic, which `requests.ts` derives from the request rows once they are drawn. */
const UNTIL_DRAWN: Pick<PoolSeed, 'billed24h' | 'cost24h' | 'equivalent24h' | 'requests24h' | 'traffic24h'> = {
  billed24h: 0,
  cost24h: 0,
  equivalent24h: 0,
  requests24h: 0,
  traffic24h: [],
};

const POOL_ROWS: PoolSeed[] = [
  {
    ...POOL_SAMPLE,
    id: 'all',
    name: 'all',
    status: 'degraded',
    strategy: 'pace',
    preferSubscription: true,
    overagePolicy: 'prefer_plan',
    // A ceiling under 80 % with its shedding on the balanced preset: primary, secondary and nightly all project past
    // it, and the route warns about the unused fifth.
    windowCeiling: 0.75,
    loadShedding: true,
    shedCap: 0.5,
    burnAverageSeconds: 1_800,
    shedDwellSeconds: 900,
    // The slow-down off, so its 5-hour rounds fire on the projection. The return dwell is the shed dwell's, as the
    // registry's migration copied it.
    slowDown: false,
    slowDownStretchSeconds: 9_000,
    slowDownHoldSeconds: 120,
    returnToPlan: 'always',
    returnToPlanDwellSeconds: 900,
    affinityTtlSeconds: 600,
    // Pins only for a request that caches, and a fresh session placed where its static prefix is warm.
    affinityMode: 'when_cached',
    minSharedPrefixTokens: POOL_AFFINITY_DEFAULTS.minSharedPrefixTokens,
    sessionPlacement: 'by_warm_prefix',
    subagentPlacement: 'by_cohort',
    keepalive: { ...KEEPALIVE_DEFAULTS, enabled: true },
    // Three plan members on the hour (Haiku's five-minute override lands inside the range) and the API-key
    // `overflow` on the metered five minutes, which brings the low end: 46 minutes to 44.1 hours.
    keepaliveResolved: merged(FIGURES.plan['1h'], FIGURES.dollars['5m']),
    autoModeServer: true,
    cacheDiagnostics: true,
    cacheTtl: {
      enabled: true,
      injectWhenAbsent: true,
      plan: {
        main: '1h',
        subagent: '5m',
        models: [{ model: 'claude-haiku-4-5-20251001', main: '5m', subagent: '5m' }],
      },
      metered: { main: '5m', subagent: '5m', models: [] },
    },
    bodyCaptureHours: 24,
    modelRules: [{ not: true, pattern: 'claude-3*' }],
    members: [
      // A subscription member capped on its metered spend, so the card and the budgets card draw a metered line.
      {
        accountId: 'primary',
        budgetDailyUsd: null,
        budgetMonthlyUsd: 30,
        priority: 0,
        spends: [{ period: 'month', periodEnd: periodEndAfter('month', NOW), spentUsd: 12.5 }],
      },
      unbudgeted('secondary', 0),
      unbudgeted('nightly', 0),
      // Under both its budgets, half the month's spent, so the card has two spend lines that hold nothing back.
      {
        accountId: 'overflow',
        budgetDailyUsd: 5,
        budgetMonthlyUsd: 50,
        priority: 1,
        spends: [
          { period: 'day', periodEnd: periodEndAfter('day', NOW), spentUsd: 1.1 },
          { period: 'month', periodEnd: periodEndAfter('month', NOW), spentUsd: 25 },
        ],
      },
    ],
    ...UNTIL_DRAWN,
    // The 09:05:12 edit of its shared prefix floor and its slow-down hold, the registry change the routing log records.
    updatedAt: iso(clock(9, 5, 12)),
    // The sample's failover settings are the store's defaults, so this pool reads balanced on both groups. The last
    // hour's moves are the routing log's story: three failovers, a fast lane's and two by the spent rule.
    movesPerHour: { cooldown: 3, fast_lane: 1, spent: 2 },
  },
  {
    ...POOL_SAMPLE,
    id: 'jobs',
    name: 'jobs',
    // Its one member has spent its daily budget, which leaves the pool nothing to route to until the day ends.
    status: 'exhausted',
    strategy: 'round_robin',
    preferSubscription: false,
    overagePolicy: 'allow',
    windowCeiling: 0.95,
    loadShedding: true,
    // A batch pool tuned by hand, so neither setting group matches a preset and both read custom.
    shedCap: 0.4,
    burnAverageSeconds: 1_200,
    shedDwellSeconds: 1_800,
    migrateWhenSpent: false,
    warmSessions: 'never',
    // Its one member is an API-key account, which the slow-down never holds. The return dwell is the shed dwell's, as
    // the registry's migration copied it.
    slowDown: false,
    slowDownStretchSeconds: 9_000,
    slowDownHoldSeconds: 120,
    returnToPlan: 'always',
    returnToPlanDwellSeconds: 1_800,
    inFlightCap: 4,
    retryInPlaceSeconds: 30,
    overageMoveGraceMinutes: 0,
    windDownMinutes: 120,
    movesPerHour: {},
    affinityTtlSeconds: 0,
    keepalive: { ...KEEPALIVE_DEFAULTS, enabled: false, maxIdleSeconds: 86_400 },
    // Haiku alone in dollars on both TTLs, since the cache TTL policy leaves the markers as sent: 46 minutes on five
    // minutes to 17.1 hours on the hour. The idle window is set to a day, above the whole break-even, so it runs as set
    // and its share reads above one on both TTLs.
    keepaliveResolved: {
      breakEvenSeconds: { max: 61_560, min: 2_760 },
      horizonSeconds: { max: 61_560, min: 2_760 },
      idleSeconds: one(86_400),
      idleShare: { max: 28.8, min: 1.334 },
      parkedSeconds: { max: 61_560, min: 2_760 },
    },
    autoModeServer: true,
    cacheDiagnostics: false,
    cacheTtl: { ...CACHE_TTL_DEFAULTS },
    bodyCaptureHours: 0,
    modelRules: [
      { not: false, pattern: 'claude-haiku-4-5*' },
      { not: true, pattern: '*' },
    ],
    members: [{ accountId: 'research', budgetDailyUsd: 20, budgetMonthlyUsd: null, priority: 0, spends: [{ period: 'day', periodEnd: periodEndAfter('day', NOW), spentUsd: 20.4 }] }],
    ...UNTIL_DRAWN,
    updatedAt: daysAgo(12),
  },
  {
    ...POOL_SAMPLE,
    // A subscription member and the API-key annex a round moves studio's sessions to, behind a key that admits any
    // client.
    id: 'work',
    name: 'work',
    status: 'active',
    strategy: 'priority',
    preferSubscription: true,
    overagePolicy: 'prefer_plan',
    windowCeiling: 0.95,
    loadShedding: true,
    // Its own cap at the balanced preset's, where the sample's captured pool sets one under a quarter.
    shedCap: 0.5,
    // A quarter-hour burn average, which studio's afternoon span has outlasted by its slowed turn, so that turn's hold
    // applies the full factor.
    burnAverageSeconds: 900,
    // The slow-down on at the store's hold, so studio's sessions are held once they would pass the ceiling before the
    // reset. Its own stretch threshold of 30 minutes lets a round move sessions off studio while the store's two and a
    // half hours would still leave them to the slow-down alone.
    slowDown: true,
    slowDownStretchSeconds: 1_800,
    slowDownHoldSeconds: 120,
    returnToPlan: 'always',
    returnToPlanDwellSeconds: 900,
    affinityTtlSeconds: 3_600,
    movesPerHour: {},
    // Warming on with the plan's main turns on the hour, and the annex on the metered five minutes, which brings the
    // low end.
    keepalive: { ...KEEPALIVE_DEFAULTS, enabled: true },
    keepaliveResolved: merged(FIGURES.plan['1h'], FIGURES.dollars['5m']),
    // Off, so the pool card's badge and the key dialog's Inherit label each read both states in the story.
    autoModeServer: false,
    cacheDiagnostics: false,
    cacheTtl: { ...CACHE_TTL_DEFAULTS, enabled: true },
    bodyCaptureHours: 0,
    modelRules: [],
    members: [unbudgeted('studio', 0), unbudgeted('annex', 1)],
    ...UNTIL_DRAWN,
    updatedAt: daysAgo(40),
  },
  {
    ...POOL_SAMPLE,
    // The contract seat alone, a metered source the router never sheds: it reads no plan window for a ceiling to hold.
    id: 'contract',
    name: 'contract',
    status: 'active',
    strategy: 'pace',
    preferSubscription: true,
    overagePolicy: 'prefer_plan',
    loadShedding: false,
    slowDown: false,
    movesPerHour: {},
    // Off, since every write on the seat costs dollars. A usage-based member keeps the metered set, so the figures are
    // the dollar rows' on both TTLs, as a policy that leaves the markers as sent brings them.
    keepalive: { ...KEEPALIVE_DEFAULTS, enabled: false },
    keepaliveResolved: merged(FIGURES.dollars['5m'], FIGURES.dollars['1h']),
    bodyCaptureHours: 0,
    modelRules: [],
    members: [unbudgeted('contract', 1)],
    ...UNTIL_DRAWN,
    updatedAt: daysAgo(30),
  },
  {
    ...POOL_SAMPLE,
    // The approved Ultra seat alone, kept apart for the workshop's clients on a key of their own, so its week serves
    // their sessions and nobody else's. One member has nowhere to shed or slow a session into.
    id: 'workshop',
    name: 'workshop',
    status: 'active',
    strategy: 'pace',
    preferSubscription: true,
    overagePolicy: 'prefer_plan',
    loadShedding: false,
    slowDown: false,
    movesPerHour: {},
    // Off, so a session's cache lives as long as its own turns keep it. The figures are the plan's on both TTLs, as a
    // policy that leaves the markers as sent brings them.
    keepalive: { ...KEEPALIVE_DEFAULTS, enabled: false },
    keepaliveResolved: merged(FIGURES.plan['5m'], FIGURES.plan['1h']),
    bodyCaptureHours: 0,
    modelRules: [],
    members: [unbudgeted('workshop', 0)],
    ...UNTIL_DRAWN,
    updatedAt: daysAgo(20),
  },
];

// MockApi restamps revision at construction, so the served figure is not this seed's. The sample's guardrails and
// warnings are the captured pool's, so the seed drops them: the mock answers its own.
export const POOLS: PoolSeed[] = POOL_ROWS.map(({ guardrails: _g, warnings: _w, ...seed }: PoolSeed & { guardrails?: unknown; warnings?: unknown }) => seed);

export const poolById = new Map(POOLS.map((p) => [p.id, p]));

// Each key's 7-day usage derives from the rollups in `requests.ts`. A lifetime is the story's own where the key is
// older than the month the rollups hold, raised there to their sum; a key created inside that month carries none,
// so its lifetime is theirs.
// MockApi restamps revision at construction, so the served figure is not this seed's. A key's guardrails and its pool's
// are the attachments', which the key view reads.
export const KEYS: Omit<ApiKey, 'guardrails' | 'poolGuardrails'>[] = [
  {
    ...KEY_SAMPLE,
    id: 'desktop',
    autoModeServer: null,
    name: 'desktop',
    prefix: 'phoebis-8f2a1c4e',
    poolId: 'all',
    modelRules: [],
    modelRulesMode: 'extend',
    longContextTwins: true,
    enabled: true,
    status: 'active',
    // Issued under the id an earlier key held, the hour that key's rows end.
    createdAt: iso(DELETED_HOLDER_END),
    // Its rotation, the key's last write.
    updatedAt: ago(12 * MINUTE),
    expiresAt: null,
    lastUsedAt: ago(3 * MINUTE),
    // Rotated inside the last 15 minutes, which is the only window in which the route reports a session still
    // holding the previous secret.
    retiredSecretLastUsedAt: ago(2 * MINUTE),
    sessionsOnRetiredSecret: 1,
    notes: 'Main workstation. Claude Code launches through the shell profile.',
    bodyCaptureHours: null,
    keepaliveOptOut: false,
    lifetime: { billedUsd: 0, cost: 0, equivalent: 0, requests: 0 },
  },
  {
    ...KEY_SAMPLE,
    id: 'laptop',
    autoModeServer: null,
    name: 'laptop',
    prefix: 'phoebis-c41d7b09',
    poolId: 'all',
    modelRules: [
      { not: false, pattern: 'claude-haiku-4-5-20251001' },
      { not: false, pattern: 'claude-sonnet-4-5' },
      { not: true, pattern: '*' },
    ],
    modelRulesMode: 'extend',
    longContextTwins: true,
    enabled: true,
    status: 'active',
    createdAt: daysAgo(34),
    updatedAt: daysAgo(5),
    expiresAt: ahead(60 * DAY),
    lastUsedAt: ago(18 * MINUTE),
    retiredSecretLastUsedAt: null,
    sessionsOnRetiredSecret: 0,
    notes: 'Travel laptop. Bodies captured while debugging the pre-commit hook.',
    bodyCaptureHours: 6,
    keepaliveOptOut: false,
    // Plan-funded, so its dollars bill nothing. Its equivalents before the month the rollups hold are not drawn, so
    // they read the rollups' sums.
    lifetime: { billedUsd: 0, cost: 38.6, equivalent: 0, requests: 1_012 },
  },
  {
    ...KEY_SAMPLE,
    id: 'batch',
    autoModeServer: null,
    name: 'batch',
    prefix: 'phoebis-5b70e3a2',
    poolId: 'jobs',
    modelRules: [],
    modelRulesMode: 'extend',
    longContextTwins: true,
    enabled: false,
    status: 'disabled',
    createdAt: daysAgo(58),
    // When it was switched off.
    updatedAt: daysAgo(20),
    expiresAt: null,
    // The last nightly run before the switch-off.
    lastUsedAt: daysAgo(21),
    retiredSecretLastUsedAt: null,
    sessionsOnRetiredSecret: 0,
    notes: 'Nightly evaluation runs. Switched off between campaigns rather than revoked.',
    bodyCaptureHours: 0,
    keepaliveOptOut: true,
    // The jobs pool's one member is an API key account, so every dollar of its life billed.
    lifetime: { billedUsd: 31.8, cost: 31.8, equivalent: 0, requests: 2_204 },
  },
  {
    ...KEY_SAMPLE,
    id: 'ci',
    autoModeServer: null,
    name: 'ci',
    prefix: 'phoebis-19e7f6d4',
    poolId: 'jobs',
    modelRules: [
      { not: false, pattern: 'claude-haiku-4-5-20251001' },
      { not: true, pattern: '*' },
    ],
    modelRulesMode: 'extend',
    longContextTwins: true,
    // A revocation switches the key off with it.
    enabled: false,
    status: 'revoked',
    createdAt: daysAgo(120),
    // Its revocation, after the last request it reached selection with at 14:16:40.
    updatedAt: iso(KEY_REVOKED_AT),
    expiresAt: null,
    // The request log's newest row the key was let through with sets the last use, since the rows are drawn.
    lastUsedAt: null,
    retiredSecretLastUsedAt: null,
    sessionsOnRetiredSecret: 0,
    notes: 'Rotated out after the runner image moved to its own key.',
    bodyCaptureHours: 0,
    keepaliveOptOut: true,
    lifetime: { billedUsd: 21.4, cost: 21.4, equivalent: 0, requests: 960 },
  },
  {
    ...KEY_SAMPLE,
    id: 'notebook',
    // Off under a pool that is off, so the key dialog reads an explicit key value beside the pool's.
    autoModeServer: false,
    name: 'notebook',
    prefix: 'phoebis-a07c2e58',
    poolId: 'work',
    modelRules: [],
    modelRulesMode: 'extend',
    longContextTwins: true,
    enabled: true,
    status: 'active',
    createdAt: daysAgo(3),
    updatedAt: daysAgo(3),
    expiresAt: null,
    // The request log's newest notebook row sets the last use.
    lastUsedAt: null,
    retiredSecretLastUsedAt: null,
    sessionsOnRetiredSecret: 0,
    notes: 'Handed out for the data notebooks, which call the Messages API through the SDK.',
    bodyCaptureHours: null,
    keepaliveOptOut: false,
    lifetime: { billedUsd: 0, cost: 0, equivalent: 0, requests: 0 },
  },
  {
    ...KEY_SAMPLE,
    id: 'fleet',
    autoModeServer: null,
    name: 'fleet',
    prefix: 'phoebis-6d2e9a14',
    poolId: 'contract',
    modelRules: [],
    modelRulesMode: 'extend',
    longContextTwins: true,
    enabled: true,
    status: 'active',
    createdAt: daysAgo(30),
    updatedAt: daysAgo(30),
    expiresAt: null,
    // The request log's newest fleet row sets the last use.
    lastUsedAt: null,
    retiredSecretLastUsedAt: null,
    sessionsOnRetiredSecret: 0,
    notes: 'Batch workers on the Enterprise contract seat, billed per token at API rates.',
    bodyCaptureHours: 0,
    keepaliveOptOut: false,
    lifetime: { billedUsd: 0, cost: 0, equivalent: 0, requests: 0 },
  },
  {
    ...KEY_SAMPLE,
    id: 'bench',
    autoModeServer: null,
    name: 'bench',
    prefix: 'phoebis-3b8d5f71',
    poolId: 'workshop',
    modelRules: [],
    modelRulesMode: 'extend',
    longContextTwins: true,
    enabled: true,
    status: 'active',
    createdAt: daysAgo(20),
    updatedAt: daysAgo(20),
    expiresAt: null,
    // The request log's newest bench row sets the last use.
    lastUsedAt: null,
    retiredSecretLastUsedAt: null,
    sessionsOnRetiredSecret: 0,
    notes: "The workshop's clients, on the approved Ultra seat.",
    bodyCaptureHours: 0,
    keepaliveOptOut: false,
    lifetime: { billedUsd: 0, cost: 0, equivalent: 0, requests: 0 },
  },
];

export const keyById = new Map(KEYS.map((k) => [k.id, k]));
