import type { Account, QuotaHistory, QuotaSample, QuotaSnapshot, RateLimitWindow, ServedModel, WindowId } from '@/api/types';
import { CLAUDE_MODELS } from './catalog';
import { createRng } from './rng';
import { DAY, HOUR, MINUTE, NOW, ago, ahead, daysAgo, hoursAgo, iso } from './time';

const claudeModels = (opts: { discovered?: string[]; syncedAgoHours: number; vanished?: string[] }): ServedModel[] =>
  CLAUDE_MODELS.map((id) => {
    const isNew = (opts.discovered ?? []).includes(id);
    return {
      canonicalId: id,
      id,
      inCatalog: !(opts.vanished ?? []).includes(id),
      isNew,
      longContext: id !== 'claude-haiku-4-5-20251001',
      manual: false,
      selected: !isNew,
      upstreamId: id,
    };
  });

const manualModel = (id: string): ServedModel => ({ canonicalId: id, id, inCatalog: false, isNew: false, longContext: false, manual: true, selected: true, upstreamId: id });

function win(id: WindowId, utilization: number, resetsInMs: number, observedAgoMs = 40_000): RateLimitWindow {
  const labels: Record<WindowId, string> = { '5h': '5-hour', '7d': '7-day', '7d_fable': '7-day Fable' };
  const periods: Record<WindowId, number> = { '5h': 5, '7d': 168, '7d_fable': 168 };
  return {
    id,
    label: labels[id],
    observedAt: ago(observedAgoMs),
    periodHours: periods[id],
    resetsAt: ahead(resetsInMs),
    utilization,
  };
}

export const ACCOUNTS: Account[] = [
  {
    id: 'business',
    kind: 'oauth',
    label: 'business',
    email: 'business@example.com',
    plan: 'Max 20x',
    status: 'ready',
    routingEnabled: true,
    pools: [{ poolId: 'all', priority: 0 }],
    windows: [win('5h', 0.62, 2 * HOUR + 14 * MINUTE), win('7d', 0.41, 3 * DAY + 5 * HOUR + 40 * MINUTE)],
    fableWindow: win('7d_fable', 0.12, 3 * DAY + 5 * HOUR + 40 * MINUTE),
    extraUsage: { disabledReason: null, enabled: true, inUse: false, monthlyLimitCredits: 5000, usedCredits: 1234 },
    servedModels: claudeModels({ syncedAgoHours: 6, discovered: ['claude-opus-5'] }),
    catalogSyncedAt: hoursAgo(6),
    credential: {
      accessTokenExpiresAt: ahead(6 * HOUR + 22 * MINUTE),
      lastError: null,
      lastRefreshAt: ago(1 * HOUR + 38 * MINUTE),
      refreshTokenExpiresAt: ahead(58 * DAY),
    },
    subscription: { billingType: 'annual', rateLimitTier: 'max_20x', since: daysAgo(412), status: 'active', trialEndsAt: null },
    requests: { last24h: 1187, last7d: 7420, lifetime: 184_211 },
    cooldowns: [],
  },
  {
    id: 'mail',
    kind: 'oauth',
    label: 'mail',
    email: 'mail@example.com',
    plan: 'Pro 5x',
    status: 'cooldown',
    routingEnabled: true,
    pools: [{ poolId: 'all', priority: 0 }],
    windows: [win('5h', 1, 1 * HOUR + 12 * MINUTE), win('7d', 0.88, 5 * DAY + 2 * HOUR + 5 * MINUTE)],
    fableWindow: null,
    extraUsage: null,
    servedModels: claudeModels({ syncedAgoHours: 6 }),
    catalogSyncedAt: hoursAgo(6),
    credential: {
      accessTokenExpiresAt: ahead(3 * HOUR + 2 * MINUTE),
      lastError: null,
      lastRefreshAt: ago(4 * HOUR + 58 * MINUTE),
      refreshTokenExpiresAt: ahead(31 * DAY),
    },
    subscription: { billingType: 'monthly', rateLimitTier: 'pro_5x', since: daysAgo(203), status: 'active', trialEndsAt: null },
    requests: { last24h: 364, last7d: 2290, lifetime: 51_902 },
    cooldowns: [
      {
        reason: 'rate_limited (5h window exhausted)',
        setAt: ago(28 * MINUTE),
        until: ahead(1 * HOUR + 12 * MINUTE),
        windowId: '5h',
      },
    ],
  },
  {
    id: 'personal',
    kind: 'oauth',
    label: 'personal',
    email: 'personal@example.com',
    plan: 'Max 5x',
    status: 'ready',
    routingEnabled: true,
    pools: [{ poolId: 'all', priority: 0 }],
    windows: [win('5h', 0.97, 3 * HOUR + 41 * MINUTE), win('7d', 0.35, 1 * DAY + 18 * HOUR + 12 * MINUTE)],
    fableWindow: win('7d_fable', 0.41, 1 * DAY + 18 * HOUR + 12 * MINUTE),
    extraUsage: { disabledReason: 'Payment method declined on 02 Sep; overage paused by the provider', enabled: true, inUse: true, monthlyLimitCredits: 2000, usedCredits: 612 },
    servedModels: claudeModels({ syncedAgoHours: 6, vanished: [] }).concat([manualModel('claude-sonnet-4-5-20250929')]),
    catalogSyncedAt: hoursAgo(6),
    credential: {
      accessTokenExpiresAt: ahead(5 * HOUR + 50 * MINUTE),
      lastError: null,
      lastRefreshAt: ago(2 * HOUR + 10 * MINUTE),
      refreshTokenExpiresAt: ahead(72 * DAY),
    },
    subscription: { billingType: 'monthly', rateLimitTier: 'max_5x', since: daysAgo(96), status: 'active', trialEndsAt: null },
    requests: { last24h: 812, last7d: 4103, lifetime: 22_870 },
    cooldowns: [],
  },
  {
    id: 'console',
    kind: 'api_key',
    label: 'Console workspace',
    email: null,
    plan: null,
    status: 'ready',
    routingEnabled: true,
    pools: [{ poolId: 'all', priority: 1 }],
    windows: [],
    fableWindow: null,
    extraUsage: null,
    servedModels: claudeModels({ syncedAgoHours: 6 }),
    catalogSyncedAt: hoursAgo(6),
    credential: { accessTokenExpiresAt: null, lastError: null, lastRefreshAt: null, refreshTokenExpiresAt: null },
    subscription: null,
    requests: { last24h: 41, last7d: 233, lifetime: 3_104 },
    cooldowns: [],
  },
  {
    id: 'team',
    kind: 'oauth',
    label: 'team',
    email: 'team@example.com',
    plan: 'Team',
    status: 'reauth_required',
    routingEnabled: false,
    pools: [],
    windows: [win('5h', 0.18, -2 * DAY, 2 * DAY + 3 * HOUR), win('7d', 0.52, 2 * DAY, 2 * DAY + 3 * HOUR)],
    fableWindow: null,
    extraUsage: null,
    servedModels: claudeModels({ syncedAgoHours: 51 }),
    catalogSyncedAt: hoursAgo(51),
    credential: {
      accessTokenExpiresAt: ago(2 * DAY + 2 * HOUR),
      lastError: 'refresh_token rejected: invalid_grant',
      lastRefreshAt: ago(2 * DAY + 3 * HOUR),
      refreshTokenExpiresAt: ago(2 * DAY + 2 * HOUR),
    },
    subscription: { billingType: 'monthly', rateLimitTier: 'team', since: daysAgo(280), status: 'trialing', trialEndsAt: ahead(9 * DAY) },
    requests: { last24h: 0, last7d: 96, lifetime: 12_406 },
    cooldowns: [],
  },
];

export const accountById = new Map(ACCOUNTS.map((a) => [a.id, a]));

export const QUOTA_SNAPSHOTS: QuotaSnapshot[] = ACCOUNTS.map((a) => {
  if (a.id === 'team') {
    return {
      accountId: a.id,
      error: 'Refresh token rejected (invalid_grant). Re-pair the account to load quota.',
      fetchedAt: ago(2 * DAY + 3 * HOUR),
      status: 'error',
    };
  }
  return { accountId: a.id, error: null, fetchedAt: ago(40_000), status: 'success' };
});

const SAMPLE_STEP = 5 * MINUTE;
const HISTORY_SPAN = 7 * DAY;

// Concave climb: usage lands early in a window and tapers as the session cools.
const climb = (phase: number) => Math.min(1, Math.pow(Math.max(0, phase), 0.75));

function sawtooth(seed: number, resetsAtMs: number, periodMs: number, target: number): QuotaSample[] {
  const rng = createRng(seed);
  const samples: QuotaSample[] = [];
  const start = NOW - HISTORY_SPAN;
  const currentStart = resetsAtMs - periodMs;
  const peakFor = new Map<number, number>();
  for (let t = start; t <= NOW; t += SAMPLE_STEP) {
    const windowIndex = Math.floor((t - currentStart) / periodMs);
    const windowStart = currentStart + windowIndex * periodMs;
    let peak = peakFor.get(windowIndex);
    if (peak === undefined) {
      // The live window must end exactly at the reported utilization; older
      // windows get their own random shape.
      if (windowIndex === 0) {
        const phaseNow = (NOW - currentStart) / periodMs;
        peak = target / Math.max(0.05, climb(phaseNow));
      } else {
        peak = rng.float(0.25, 1.0) * (rng.chance(0.15) ? 0.35 : 1);
      }
      peakFor.set(windowIndex, peak);
    }
    const phase = (t - windowStart) / periodMs;
    const jitter = windowIndex === 0 && t > NOW - SAMPLE_STEP ? 0 : rng.float(-0.012, 0.012);
    const value = Math.max(0, Math.min(1, peak * climb(phase) + jitter));
    samples.push({ at: iso(t), utilization: Math.round(value * 1000) / 1000 });
  }
  return samples;
}

const HISTORY_SEED: Record<WindowId, number> = { '5h': 1, '7d': 2, '7d_fable': 3 };

export const QUOTA_HISTORY: QuotaHistory[] = ACCOUNTS.flatMap((account, i) =>
  [...account.windows, ...(account.fableWindow ? [account.fableWindow] : [])].map((w) => ({
    accountId: account.id,
    windowId: w.id,
    samples: sawtooth(1000 + i * 7 + HISTORY_SEED[w.id], Date.parse(w.resetsAt), w.periodHours * HOUR, w.utilization),
  })),
);
