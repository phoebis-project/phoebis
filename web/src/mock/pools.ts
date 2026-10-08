import { CACHE_TTL_DEFAULTS, KEEPALIVE_DEFAULTS, POOL_AFFINITY_DEFAULTS } from '@/api/client';
import type { ApiKey, BudgetPeriod, Pool, PoolKeepalive, PoolMember, Range } from '@/api/types';
import { KEY_SAMPLE, POOL_SAMPLE } from './samples';
import { DAY, DELETED_HOLDER_END, HOUR, MINUTE, NOW, ago, ahead, daysAgo, iso } from './time';

/** A member as the story seeds it; the mock reads its kind and its eligibility off the account, as the route does. */
type MemberSeed = Omit<PoolMember, 'budgetUntil' | 'eligible' | 'kind'>;
/** The route lists a pool's keys from the key registry, so the story seeds none on the pool itself. */
export type PoolSeed = Omit<Pool, 'keys' | 'members'> & { members: MemberSeed[] };

const unbudgeted = (accountId: string, priority: number): MemberSeed => ({ accountId, budgetDailyUsd: null, budgetMonthlyUsd: null, priority, spends: [] });

/** The next UTC calendar boundary after `now`, which is where the ledger's spend period ends. */
function periodEndAfter(period: BudgetPeriod, now: number): string {
  const d = new Date(now);
  return iso(period === 'day' ? Date.UTC(d.getUTCFullYear(), d.getUTCMonth(), d.getUTCDate() + 1) : Date.UTC(d.getUTCFullYear(), d.getUTCMonth() + 1, 1));
}

/** The instant the ci key was revoked, on the story's pinned day at 14:17:30. */
export const KEY_REVOKED_AT = NOW - (12 * MINUTE + 30_000);

const one = (value: number): Range => ({ max: value, min: value });

/**
 * The router's figures for a plan pool whose main turns write the one-hour entry: a break-even of 44.1 hours and the
 * default hour of idle window at 2 percent of one re-write. The mock holds no price, so every pool's figures are
 * fixed here rather than resolved from its members, and a pool created in the mock starts from these.
 */
export const PLAN_HOUR_RESOLVED: NonNullable<Pool['keepaliveResolved']> = {
  breakEvenSeconds: one(158_760),
  horizonSeconds: one(158_760),
  idleSeconds: one(3_600),
  idleShare: one(0.0222),
  parkedSeconds: one(158_760),
};

/**
 * What the route answers once a save sets `policy`: the three ceilings capped at the stored break-even, a null or a
 * zero reading as the break-even, and the idle share moved with the window, since a window's probes cost in
 * proportion to its length. The break-even itself depends on prices alone and stays as seeded.
 */
export function resolvedFor(resolved: Pool['keepaliveResolved'], policy: PoolKeepalive): Pool['keepaliveResolved'] {
  if (resolved === null) return null;
  const be = resolved.breakEvenSeconds;
  const capped = (seconds: number | null): Range => (seconds === null ? be : { max: Math.min(seconds, be.max), min: Math.min(seconds, be.min) });
  const idle = capped(policy.maxIdleSeconds === 0 ? null : policy.maxIdleSeconds);
  const share = (end: 'max' | 'min') => (resolved.idleSeconds[end] > 0 ? (resolved.idleShare[end] * idle[end]) / resolved.idleSeconds[end] : resolved.idleShare[end]);
  return { breakEvenSeconds: be, horizonSeconds: capped(policy.scheduledHorizonSeconds), idleSeconds: idle, idleShare: { max: share('max'), min: share('min') }, parkedSeconds: capped(policy.maxParkedSeconds) };
}

/** The day's traffic, which `requests.ts` derives from the request rows once they are drawn. */
const UNTIL_DRAWN: Pick<PoolSeed, 'cost24h' | 'requests24h' | 'traffic24h'> = { cost24h: 0, requests24h: 0, traffic24h: [] };

const POOL_ROWS: PoolSeed[] = [
  {
    ...POOL_SAMPLE,
    id: 'all',
    name: 'all',
    status: 'degraded',
    strategy: 'pace',
    preferSubscription: true,
    overagePolicy: 'prefer_plan',
    shedAheadOfPace: 0.9,
    selectBelowUtilization: null,
    affinityTtlSeconds: 600,
    // Pins only for a request that caches, and a fresh session placed where its static prefix is warm.
    affinityMode: 'when_cached',
    minSharedPrefixTokens: POOL_AFFINITY_DEFAULTS.minSharedPrefixTokens,
    sessionPlacement: 'by_warm_prefix',
    subagentPlacement: 'by_cohort',
    keepalive: { ...KEEPALIVE_DEFAULTS, enabled: true },
    keepaliveResolved: PLAN_HOUR_RESOLVED,
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
    activePins: 10,
    // The 09:05:12 edit that set its shedding margin, the registry change the routing log records.
    updatedAt: iso(NOW - (5 * HOUR + 24 * MINUTE + 48_000)),
    // The sample's shedding and failover settings are the store's defaults, so this pool reads balanced on both. The
    // last hour's moves are the routing log's story: two failovers, a fast lane's and two by the spent rule.
    movesPerHour: { cooldown: 2, fast_lane: 1, spent: 2 },
  },
  {
    ...POOL_SAMPLE,
    id: 'codex',
    name: 'codex',
    // Its one member has spent its daily budget, which leaves the pool nothing to route to until the day ends.
    status: 'exhausted',
    strategy: 'round_robin',
    preferSubscription: false,
    overagePolicy: 'allow',
    shedAheadOfPace: 0.85,
    selectBelowUtilization: null,
    // A batch pool tuned by hand, so neither setting group matches a preset and both read custom.
    shedShare: 0.4,
    burnWindowSeconds: 1_200,
    sourceDwellSeconds: 1_800,
    migrateWhenSpent: false,
    warmSessions: 'never',
    inFlightCap: 4,
    retryInPlaceSeconds: 30,
    paceFloor: 0.05,
    overageMoveGraceMinutes: 0,
    windDownMinutes: 120,
    movesPerHour: {},
    affinityTtlSeconds: 0,
    keepalive: { ...KEEPALIVE_DEFAULTS, enabled: false },
    // Haiku alone in dollars on both TTLs, since the cache TTL policy leaves the markers as sent: 46 minutes on five
    // minutes, which also caps the hour of idle window, to 17.1 hours on the hour.
    keepaliveResolved: {
      breakEvenSeconds: { max: 61_560, min: 2_760 },
      horizonSeconds: { max: 61_560, min: 2_760 },
      idleSeconds: { max: 3_600, min: 2_760 },
      idleShare: { max: 0.92, min: 0.0556 },
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
    activePins: 2,
    updatedAt: daysAgo(12),
  },
  {
    ...POOL_SAMPLE,
    id: 'work',
    name: 'work',
    status: 'active',
    strategy: 'priority',
    preferSubscription: true,
    overagePolicy: 'prefer_plan',
    shedAheadOfPace: null,
    selectBelowUtilization: null,
    affinityTtlSeconds: 3_600,
    movesPerHour: {},
    keepalive: { ...KEEPALIVE_DEFAULTS, enabled: false },
    // The plan on both TTLs, the markers going as sent: 3 hours 16 minutes on five minutes to 44.1 hours on the hour.
    keepaliveResolved: {
      breakEvenSeconds: { max: 158_760, min: 11_760 },
      horizonSeconds: { max: 158_760, min: 11_760 },
      idleSeconds: one(3_600),
      idleShare: { max: 0.3, min: 0.0222 },
      parkedSeconds: { max: 158_760, min: 11_760 },
    },
    // Off, so the pool card's badge and the key dialog's Inherit label each read both states in the story.
    autoModeServer: false,
    cacheDiagnostics: false,
    cacheTtl: { ...CACHE_TTL_DEFAULTS },
    bodyCaptureHours: 0,
    modelRules: [],
    members: [unbudgeted('primary', 0)],
    ...UNTIL_DRAWN,
    activePins: 0,
    updatedAt: daysAgo(40),
  },
];

export const POOLS: PoolSeed[] = POOL_ROWS;

export const poolById = new Map(POOLS.map((p) => [p.id, p]));

// Each key's 7-day usage derives from the rollups in `requests.ts`. A lifetime is the story's own where the key is
// older than the month the rollups hold, raised there to their sum; a key created inside that month carries none,
// so its lifetime is theirs.
export const KEYS: ApiKey[] = [
  {
    ...KEY_SAMPLE,
    id: 'desktop',
    autoModeServer: null,
    name: 'desktop',
    prefix: 'phoebis-8f2a',
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
    lifetime: { cost: 0, requests: 0 },
  },
  {
    ...KEY_SAMPLE,
    id: 'laptop',
    autoModeServer: null,
    name: 'laptop',
    prefix: 'phoebis-c41d',
    poolId: 'all',
    modelRules: [
      { not: false, pattern: 'claude-haiku-4-5-20251001' },
      { not: false, pattern: 'claude-sonnet-5' },
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
    lifetime: { cost: 38.6, requests: 1_012 },
  },
  {
    ...KEY_SAMPLE,
    id: 'batch',
    autoModeServer: null,
    name: 'batch',
    prefix: 'phoebis-5b70',
    poolId: 'codex',
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
    lifetime: { cost: 31.8, requests: 2_204 },
  },
  {
    ...KEY_SAMPLE,
    id: 'ci',
    autoModeServer: null,
    name: 'ci',
    prefix: 'phoebis-19e7',
    poolId: 'codex',
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
    lastUsedAt: iso(NOW - (13 * MINUTE + 20_000)),
    retiredSecretLastUsedAt: null,
    sessionsOnRetiredSecret: 0,
    notes: 'Rotated out after the runner image moved to its own key.',
    bodyCaptureHours: 0,
    keepaliveOptOut: true,
    lifetime: { cost: 21.4, requests: 960 },
  },
  {
    ...KEY_SAMPLE,
    id: 'notebook',
    // Off under a pool that is off, so the key dialog reads an explicit key value beside the pool's.
    autoModeServer: false,
    name: 'notebook',
    prefix: 'phoebis-a07c',
    poolId: 'work',
    modelRules: [],
    modelRulesMode: 'extend',
    longContextTwins: true,
    enabled: true,
    status: 'active',
    createdAt: daysAgo(3),
    updatedAt: daysAgo(3),
    expiresAt: null,
    // The request log's one notebook row sets the last use, since its time is drawn.
    lastUsedAt: null,
    retiredSecretLastUsedAt: null,
    sessionsOnRetiredSecret: 0,
    notes: 'Handed out for the data notebooks, which call the Messages API through the SDK.',
    bodyCaptureHours: null,
    keepaliveOptOut: false,
    lifetime: { cost: 0, requests: 0 },
  },
];

export const keyById = new Map(KEYS.map((k) => [k.id, k]));
