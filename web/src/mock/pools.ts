import { CACHE_TTL_DEFAULTS, KEEPALIVE_TUNING_DEFAULTS, POOL_AFFINITY_DEFAULTS } from '@/api/client';
import type { ApiKey, BudgetPeriod, Pool, PoolMember, TrafficBucket } from '@/api/types';
import { createRng } from './rng';
import { KEY_SAMPLE, POOL_SAMPLE } from './samples';
import { DAY, HOUR, MINUTE, NOW, ago, ahead, daysAgo, iso, startOfHour } from './time';

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

function traffic(seed: number, scale: number): TrafficBucket[] {
  const rng = createRng(seed);
  const start = startOfHour(NOW) - 23 * HOUR;
  return Array.from({ length: 24 }, (_, i) => {
    const at = start + i * HOUR;
    const hourOfDay = new Date(at).getHours();
    // Working hours carry the traffic; overnight is a trickle.
    const daylight = hourOfDay >= 8 && hourOfDay <= 19 ? 1 : 0.18;
    const requests = Math.round(scale * daylight * rng.float(0.55, 1.35));
    const failed = Math.round(requests * rng.float(0, 0.04));
    return { at: iso(at), costUsd: requests * rng.float(0.01, 0.05), failed, requests, tokens: requests * rng.int(9_000, 24_000) };
  });
}

const ALL_TRAFFIC = traffic(41, 62);
const CODEX_TRAFFIC = traffic(42, 7);

const POOL_ROWS: PoolSeed[] = [
  {
    ...POOL_SAMPLE,
    id: 'all',
    name: 'all',
    status: 'degraded',
    strategy: 'pace',
    preferSubscription: true,
    overagePolicy: 'prefer_plan',
    migrateAboveUtilization: null,
    selectBelowUtilization: null,
    affinityTtlSeconds: 600,
    // Pins only for a request that caches, and a fresh session placed where its static prefix is warm.
    affinityMode: 'when_cached',
    minSharedPrefixTokens: POOL_AFFINITY_DEFAULTS.minSharedPrefixTokens,
    sessionPlacement: 'by_warm_prefix',
    subagentPlacement: 'by_cohort',
    keepalive: { ...KEEPALIVE_TUNING_DEFAULTS, enabled: true, costCapUsdPerDay: 4, warmSubagents: true },
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
        accountId: 'business',
        budgetDailyUsd: null,
        budgetMonthlyUsd: 30,
        priority: 0,
        spends: [{ period: 'month', periodEnd: periodEndAfter('month', NOW), spentUsd: 12.5 }],
      },
      unbudgeted('mail', 0),
      unbudgeted('personal', 0),
      // Under both its budgets, half the month's spent, so the card has two spend lines that hold nothing back.
      {
        accountId: 'console',
        budgetDailyUsd: 5,
        budgetMonthlyUsd: 50,
        priority: 1,
        spends: [
          { period: 'day', periodEnd: periodEndAfter('day', NOW), spentUsd: 1.1 },
          { period: 'month', periodEnd: periodEndAfter('month', NOW), spentUsd: 25 },
        ],
      },
    ],
    traffic24h: ALL_TRAFFIC,
    requests24h: ALL_TRAFFIC.reduce((s, b) => s + b.requests, 0),
    cost24h: 38.72,
    activePins: 10,
    // The sample's shedding and failover settings are the store's defaults, so this pool reads balanced on both.
    // With migration off no round runs, so every move comes from the rules that need no threshold.
    movesPerHour: { overage: 0.5, spent: 2 },
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
    migrateAboveUtilization: 0.85,
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
    keepalive: { ...KEEPALIVE_TUNING_DEFAULTS, enabled: false, costCapUsdPerDay: null },
    autoModeServer: true,
    cacheDiagnostics: false,
    cacheTtl: { ...CACHE_TTL_DEFAULTS },
    bodyCaptureHours: 0,
    modelRules: [
      { not: false, pattern: 'claude-haiku-4-5*' },
      { not: true, pattern: '*' },
    ],
    members: [{ accountId: 'evals', budgetDailyUsd: 20, budgetMonthlyUsd: null, priority: 0, spends: [{ period: 'day', periodEnd: periodEndAfter('day', NOW), spentUsd: 20.4 }] }],
    traffic24h: CODEX_TRAFFIC,
    requests24h: CODEX_TRAFFIC.reduce((s, b) => s + b.requests, 0),
    cost24h: 2.14,
    activePins: 2,
  },
  {
    ...POOL_SAMPLE,
    id: 'studio',
    name: 'studio',
    status: 'active',
    strategy: 'priority',
    preferSubscription: true,
    overagePolicy: 'prefer_plan',
    migrateAboveUtilization: null,
    selectBelowUtilization: null,
    affinityTtlSeconds: 3_600,
    movesPerHour: {},
    keepalive: { ...KEEPALIVE_TUNING_DEFAULTS, enabled: false, costCapUsdPerDay: null },
    // Off, so the pool card's badge and the key dialog's Inherit label each read both states in the story.
    autoModeServer: false,
    cacheDiagnostics: false,
    cacheTtl: { ...CACHE_TTL_DEFAULTS },
    bodyCaptureHours: 0,
    modelRules: [],
    members: [unbudgeted('business', 0)],
    traffic24h: [],
    requests24h: 0,
    cost24h: 0,
    activePins: 0,
  },
];

export const POOLS: PoolSeed[] = POOL_ROWS;

export const poolById = new Map(POOLS.map((p) => [p.id, p]));

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
    createdAt: daysAgo(91),
    expiresAt: null,
    lastUsedAt: ago(3 * MINUTE),
    // Rotated inside the last 15 minutes, which is the only window in which the route reports a session still
    // holding the previous secret.
    retiredSecretLastUsedAt: ago(2 * MINUTE),
    sessionsOnRetiredSecret: 1,
    notes: 'Main workstation. Claude Code launches through the shell profile.',
    bodyCaptureHours: null,
    keepaliveOptOut: false,
    usage7d: { cost: 182.4, requests: 9_812, tokens: 81_400_000 },
    lifetime: { cost: 2_318.9, requests: 121_407 },
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
    expiresAt: ahead(60 * DAY),
    lastUsedAt: ago(18 * MINUTE),
    retiredSecretLastUsedAt: null,
    sessionsOnRetiredSecret: 0,
    notes: 'Travel laptop. Bodies captured while debugging the pre-commit hook.',
    bodyCaptureHours: 6,
    keepaliveOptOut: false,
    usage7d: { cost: 41.7, requests: 2_960, tokens: 22_100_000 },
    lifetime: { cost: 204.6, requests: 14_882 },
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
    expiresAt: null,
    lastUsedAt: daysAgo(9),
    retiredSecretLastUsedAt: null,
    sessionsOnRetiredSecret: 0,
    notes: 'Nightly evaluation runs. Switched off between campaigns rather than revoked.',
    bodyCaptureHours: 0,
    keepaliveOptOut: true,
    usage7d: { cost: 0, requests: 0, tokens: 0 },
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
    expiresAt: null,
    lastUsedAt: daysAgo(2),
    retiredSecretLastUsedAt: null,
    sessionsOnRetiredSecret: 0,
    notes: 'Rotated out after the runner image moved to its own key.',
    bodyCaptureHours: 0,
    keepaliveOptOut: true,
    usage7d: { cost: 6.2, requests: 402, tokens: 3_900_000 },
    lifetime: { cost: 96.3, requests: 8_140 },
  },
  {
    ...KEY_SAMPLE,
    id: 'notebook',
    // Off under a pool that is off, so the key dialog reads an explicit key value beside the pool's.
    autoModeServer: false,
    name: 'notebook',
    prefix: 'phoebis-a07c',
    poolId: 'studio',
    modelRules: [],
    modelRulesMode: 'extend',
    longContextTwins: true,
    enabled: true,
    status: 'active',
    createdAt: daysAgo(3),
    expiresAt: null,
    // The request log's one notebook row sets the last use and the usage below, since its time and cost are drawn.
    lastUsedAt: null,
    retiredSecretLastUsedAt: null,
    sessionsOnRetiredSecret: 0,
    notes: 'Handed out for the data notebooks, which call the Messages API through the SDK.',
    bodyCaptureHours: null,
    keepaliveOptOut: false,
    usage7d: { cost: 0, requests: 0, tokens: 0 },
    lifetime: { cost: 0, requests: 0 },
  },
];

export const keyById = new Map(KEYS.map((k) => [k.id, k]));
