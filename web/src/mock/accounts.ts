import type { Account, QuotaHistory, QuotaSample, QuotaSnapshot, RateLimitWindow, ServedModel, WindowId } from '@/api/types';
import { WINDOW_HOURS, WINDOW_LABEL } from '@/lib/windows';
import { CLAUDE_MODELS } from './catalog';
import { createRng } from './rng';
import { ACCOUNT_SAMPLE, QUOTA_SNAPSHOT_SAMPLE } from './samples';
import { DAY, HOUR, MINUTE, NOW, ago, ahead, hoursAgo, iso } from './time';

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
  return {
    id,
    label: WINDOW_LABEL[id],
    observedAt: ago(observedAgoMs),
    periodHours: WINDOW_HOURS[id],
    resetsAt: ahead(resetsInMs),
    utilization,
  };
}

export const ACCOUNTS: Account[] = [
  {
    ...ACCOUNT_SAMPLE,
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
    catalogError: null,
    catalogSyncedAt: hoursAgo(6),
    credential: {
      accessTokenExpiresAt: ahead(6 * HOUR + 22 * MINUTE),
      lastError: null,
      lastRefreshAt: ago(1 * HOUR + 38 * MINUTE),
      quarantined: false,
      refreshTokenExpiresAt: ahead(58 * DAY),
    },
    subscription: { billingType: 'annual', organizationType: 'claude_max', rateLimitTier: 'max_20x' },
    requests: { last24h: 1187, last7d: 7420, lifetime: 184_211 },
    cooldowns: [],
  },
  {
    ...ACCOUNT_SAMPLE,
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
      quarantined: false,
      refreshTokenExpiresAt: ahead(31 * DAY),
    },
    subscription: { billingType: 'monthly', organizationType: 'claude_pro', rateLimitTier: 'pro_5x' },
    requests: { last24h: 364, last7d: 2290, lifetime: 51_902 },
    cooldowns: [
      {
        reason: 'rate_limited (5h window exhausted)',
        scope: 'account',
        setAt: ago(28 * MINUTE),
        until: ahead(1 * HOUR + 12 * MINUTE),
        windowId: '5h',
      },
    ],
  },
  {
    ...ACCOUNT_SAMPLE,
    id: 'personal',
    kind: 'oauth',
    label: 'personal',
    email: 'personal@example.com',
    plan: 'Max 5x',
    status: 'funding_required',
    routingEnabled: true,
    pools: [{ poolId: 'all', priority: 0 }],
    windows: [win('5h', 0.97, 3 * HOUR + 41 * MINUTE), win('7d', 0.35, 1 * DAY + 18 * HOUR + 12 * MINUTE)],
    fableWindow: win('7d_fable', 0.41, 1 * DAY + 18 * HOUR + 12 * MINUTE),
    extraUsage: { disabledReason: 'Payment method declined on 02 Sep; overage paused by the provider', enabled: true, inUse: true, monthlyLimitCredits: 2000, usedCredits: 612 },
    servedModels: claudeModels({ syncedAgoHours: 6, vanished: [] }).concat([manualModel('claude-sonnet-4-5-20250929')]),
    catalogError: 'the models request answered 503',
    catalogSyncedAt: hoursAgo(6),
    overage: { inUse: true, observedAt: ago(40_000), reset: ahead(1 * DAY + 18 * HOUR + 12 * MINUTE), since: ago(3 * HOUR) },
    credential: {
      accessTokenExpiresAt: ahead(5 * HOUR + 50 * MINUTE),
      lastError: null,
      lastRefreshAt: ago(2 * HOUR + 10 * MINUTE),
      quarantined: true,
      refreshTokenExpiresAt: ahead(72 * DAY),
    },
    subscription: { billingType: 'monthly', organizationType: 'claude_max', rateLimitTier: 'max_5x' },
    requests: { last24h: 812, last7d: 4103, lifetime: 22_870 },
    cooldowns: [],
  },
  {
    ...ACCOUNT_SAMPLE,
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
    credential: { accessTokenExpiresAt: null, lastError: null, lastRefreshAt: null, quarantined: false, refreshTokenExpiresAt: null },
    subscription: null,
    requests: { last24h: 41, last7d: 233, lifetime: 3_104 },
    cooldowns: [],
  },
  {
    ...ACCOUNT_SAMPLE,
    id: 'evals',
    kind: 'api_key',
    label: 'Evals workspace',
    email: null,
    plan: null,
    status: 'ready',
    routingEnabled: true,
    pools: [{ poolId: 'codex', priority: 0 }],
    windows: [],
    fableWindow: null,
    extraUsage: null,
    servedModels: claudeModels({ syncedAgoHours: 1 }),
    catalogSyncedAt: hoursAgo(1),
    credential: { accessTokenExpiresAt: null, lastError: null, lastRefreshAt: null, quarantined: false, refreshTokenExpiresAt: null },
    subscription: null,
    requests: { last24h: 128, last7d: 902, lifetime: 6_517 },
    cooldowns: [],
  },
  {
    ...ACCOUNT_SAMPLE,
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
      quarantined: false,
      refreshTokenExpiresAt: ago(2 * DAY + 2 * HOUR),
    },
    subscription: { billingType: 'monthly', organizationType: 'claude_team', rateLimitTier: 'team' },
    requests: { last24h: 0, last7d: 96, lifetime: 12_406 },
    cooldowns: [],
  },
  // Added and never paired: no identity and no profile, so a page has to go by the kind to call it OAuth.
  {
    ...ACCOUNT_SAMPLE,
    id: 'spare',
    kind: 'oauth',
    label: 'spare',
    email: null,
    plan: null,
    status: 'unpaired',
    routingEnabled: true,
    pools: [],
    windows: [],
    fableWindow: null,
    extraUsage: null,
    servedModels: [],
    catalogError: null,
    catalogSyncedAt: null,
    credential: { accessTokenExpiresAt: null, lastError: null, lastRefreshAt: null, quarantined: false, refreshTokenExpiresAt: null },
    subscription: null,
    requests: { last24h: 0, last7d: 0, lifetime: 0 },
    cooldowns: [],
  },
];

export const accountById = new Map(ACCOUNTS.map((a) => [a.id, a]));

export const QUOTA_SNAPSHOTS: QuotaSnapshot[] = ACCOUNTS.map((a) => {
  if (a.id === 'spare') return { ...QUOTA_SNAPSHOT_SAMPLE, accountId: a.id, error: 'no credential is stored for this upstream; pair it to load quota', fetchedAt: null, status: 'error' };
  if (a.id === 'team') {
    return {
      ...QUOTA_SNAPSHOT_SAMPLE,
      accountId: a.id,
      error: 'Refresh token rejected (invalid_grant). Re-pair the account to load quota.',
      fetchedAt: ago(2 * DAY + 3 * HOUR),
      status: 'error',
    };
  }
  return { ...QUOTA_SNAPSHOT_SAMPLE, accountId: a.id, error: null, fetchedAt: ago(40_000), status: 'success' };
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
    samples: sawtooth(1000 + i * 7 + HISTORY_SEED[w.id], Date.parse(w.resetsAt ?? ''), w.periodHours * HOUR, w.utilization),
  })),
);
