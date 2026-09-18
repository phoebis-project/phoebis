import { KEEPALIVE_TUNING_DEFAULTS } from '@/api/client';
import type { ApiKey, Pool, TrafficBucket } from '@/api/types';
import { sampleDriven } from '@/lib/selection';
import { createRng } from './rng';
import { DAY, HOUR, MINUTE, NOW, ago, ahead, daysAgo, iso, startOfHour } from './time';

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

/** What the route answers for a stored pool: the operator's flag, or the order a sample-driven strategy forces. */
export const effectiveSubscriptionPreference = (pool: { preferSubscription: boolean; strategy: Pool['strategy'] }) => pool.preferSubscription || sampleDriven(pool.strategy);

// The effective flag is computed rather than written, so no fixture can hold a pair the router would never serve.
const POOL_ROWS: Omit<Pool, 'preferSubscriptionEffective'>[] = [
  {
    id: 'all',
    name: 'all',
    status: 'degraded',
    strategy: 'least_utilized',
    apiKeySubStrategy: 'priority_random',
    preferSubscription: false,
    overagePolicy: 'prefer_plan',
    migrateAboveUtilization: null,
    affinityTtlSeconds: 600,
    keepalive: { ...KEEPALIVE_TUNING_DEFAULTS, enabled: true, costCapUsdPerDay: 4, warmSubagents: true },
    cacheTtl: {
      enabled: true,
      main: '1h',
      subagent: '5m',
      inOverage: '5m',
      models: [{ model: 'claude-haiku-4-5-20251001', main: '5m', subagent: '5m' }],
    },
    bodyCaptureHours: 24,
    defaultAllowedModels: null,
    members: [
      { accountId: 'business', eligible: 'eligible', priority: 0 },
      { accountId: 'mail', eligible: 'cooldown', priority: 0 },
      { accountId: 'personal', eligible: 'unavailable', priority: 0 },
      { accountId: 'console', eligible: 'eligible', priority: 1 },
    ],
    traffic24h: ALL_TRAFFIC,
    requests24h: ALL_TRAFFIC.reduce((s, b) => s + b.requests, 0),
    cost24h: 38.72,
    activePins: 10,
  },
  {
    id: 'codex',
    name: 'codex',
    status: 'active',
    strategy: 'round_robin',
    apiKeySubStrategy: 'round_robin',
    preferSubscription: false,
    overagePolicy: 'allow',
    migrateAboveUtilization: null,
    affinityTtlSeconds: 0,
    keepalive: { ...KEEPALIVE_TUNING_DEFAULTS, enabled: false, costCapUsdPerDay: null, warmSubagents: null },
    cacheTtl: null,
    bodyCaptureHours: 0,
    defaultAllowedModels: ['claude-haiku-4-5-20251001'],
    traffic24h: CODEX_TRAFFIC,
    requests24h: CODEX_TRAFFIC.reduce((s, b) => s + b.requests, 0),
    cost24h: 2.14,
    activePins: 2,
  },
];

export const POOLS: Pool[] = POOL_ROWS.map((p) => ({ ...p, preferSubscriptionEffective: effectiveSubscriptionPreference(p) }));

export const poolById = new Map(POOLS.map((p) => [p.id, p]));

export const KEYS: ApiKey[] = [
  {
    id: 'desktop',
    name: 'desktop',
    prefix: 'phoebis-8f2a',
    poolId: 'all',
    allowedModels: null,
    status: 'active',
    createdAt: daysAgo(91),
    expiresAt: null,
    lastUsedAt: ago(3 * MINUTE),
    notes: 'Main workstation. Claude Code launches through the shell profile.',
    bodyCaptureHours: null,
    keepaliveOptOut: false,
    usage7d: { cost: 182.4, requests: 9_812, tokens: 81_400_000 },
    lifetime: { cost: 2_318.9, requests: 121_407 },
  },
  {
    id: 'laptop',
    name: 'laptop',
    prefix: 'phoebis-c41d',
    poolId: 'all',
    allowedModels: ['claude-haiku-4-5-20251001', 'claude-sonnet-5'],
    status: 'active',
    createdAt: daysAgo(34),
    expiresAt: ahead(60 * DAY),
    lastUsedAt: ago(18 * MINUTE),
    notes: 'Travel laptop. Bodies captured while debugging the pre-commit hook.',
    bodyCaptureHours: 6,
    keepaliveOptOut: false,
    usage7d: { cost: 41.7, requests: 2_960, tokens: 22_100_000 },
    lifetime: { cost: 204.6, requests: 14_882 },
  },
  {
    id: 'batch',
    name: 'batch',
    prefix: 'phoebis-5b70',
    poolId: 'codex',
    allowedModels: null,
    status: 'disabled',
    createdAt: daysAgo(58),
    expiresAt: null,
    lastUsedAt: daysAgo(9),
    notes: 'Nightly evaluation runs. Switched off between campaigns rather than revoked.',
    bodyCaptureHours: 0,
    keepaliveOptOut: true,
    usage7d: { cost: 0, requests: 0, tokens: 0 },
    lifetime: { cost: 31.8, requests: 2_204 },
  },
  {
    id: 'ci',
    name: 'ci',
    prefix: 'phoebis-19e7',
    poolId: 'codex',
    allowedModels: ['claude-haiku-4-5-20251001'],
    status: 'revoked',
    createdAt: daysAgo(120),
    expiresAt: null,
    lastUsedAt: daysAgo(2),
    notes: 'Rotated out after the runner image moved to its own key.',
    bodyCaptureHours: 0,
    keepaliveOptOut: true,
    usage7d: { cost: 6.2, requests: 402, tokens: 3_900_000 },
    lifetime: { cost: 96.3, requests: 8_140 },
  },
];

export const keyById = new Map(KEYS.map((k) => [k.id, k]));
