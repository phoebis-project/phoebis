import { planName } from '@/api/decode';
import type { Account, QuotaHistory, QuotaSample, QuotaSnapshot, RateLimitWindow, WindowId } from '@/api/types';
import { WINDOW_HOURS, WINDOW_LABEL, hoursOfKey } from '@/lib/windows';
import { createRng } from './rng';
import { ACCOUNT_SAMPLE, QUOTA_SNAPSHOT_SAMPLE } from './samples';
import { DAY, HOUR, MINUTE, NOW, ago, ahead, iso } from './time';

function win(id: WindowId, utilization: number, resetsInMs: number, observedAgoMs = 40_000): RateLimitWindow {
  return {
    id,
    label: WINDOW_LABEL[id] ?? id,
    observedAt: ago(observedAgoMs),
    periodHours: WINDOW_HOURS[id] ?? hoursOfKey(id),
    resetsAt: ahead(resetsInMs),
    utilization,
  };
}

/** The fast lane's two buckets as the last answer to a fast request on the account reported them. */
function fastLane(input: number, output: number, resetsInMs: number): Account['fastLane'] {
  const reading = (utilization: number) => ({ observedAt: ago(40_000), resetsAt: ahead(resetsInMs), utilization });
  return { input: reading(input), output: reading(output) };
}

// Each plan label derives from the entry's subscription by the decoder's own rule, so the mock and the served
// console draw one name. The day's and the week's request counts derive from the request rows and the rollups in
// `requests.ts`; a lifetime reaches past both, so it is the story's own, raised there to what the rollups hold.
export const ACCOUNTS: Account[] = ([
  {
    ...ACCOUNT_SAMPLE,
    id: 'primary',
    kind: 'oauth',
    label: 'primary',
    email: 'operator@example.com',
    status: 'cooldown',
    routingEnabled: true,
    pools: [
      { poolId: 'all', priority: 0 },
      { poolId: 'work', priority: 0 },
    ],
    // The poll's week for a model the family table has no row for: kept and shown under its key, read by no rule.
    windows: [win('5h', 0.62, 2 * HOUR + 14 * MINUTE), win('7d', 0.41, 3 * DAY + 5 * HOUR + 40 * MINUTE), win('seven_day_opus', 0.08, 3 * DAY + 5 * HOUR + 40 * MINUTE)],
    fableWindow: win('7d_fable', 0.12, 3 * DAY + 5 * HOUR + 40 * MINUTE),
    extraUsage: { disabledReason: null, enabled: true, inUse: false, monthlyLimitCredits: 5000, usedCredits: 1234 },
    fastLane: fastLane(1, 0.64, 12 * MINUTE),
    credential: {
      accessTokenExpiresAt: ahead(6 * HOUR + 22 * MINUTE),
      lastError: null,
      lastRefreshAt: ago(1 * HOUR + 38 * MINUTE),
      quarantined: false,
      refreshTokenExpiresAt: ahead(58 * DAY),
    },
    subscription: { billingType: 'annual', organizationType: 'claude_max', plan: 'max', rateLimitTier: 'default_claude_max_20x', since: ago(200 * DAY), trialEndsAt: null, endsAt: null, derivedEndsAt: ahead(160 * DAY) },
    requests: { last24h: 0, last7d: 0, lifetime: 5_380 },
    // A live model-scoped cooldown, which the pool eligibility check leaves alone (that reads the account-wide
    // scope alone) but which the Models view and this account's own status fold in like any other cooldown. The
    // fast-lane row ahead of it holds fast-mode requests alone, so neither the status nor the header's cooldown
    // badge reads it, and it lifts sooner, so a page reading the first row would name the wrong reset.
    cooldowns: [
      {
        reason: 'fast_rate_limit',
        scope: 'fast',
        setAt: ago(3 * MINUTE),
        until: ahead(12 * MINUTE),
        windowId: null,
      },
      {
        reason: 'model_rejected',
        scope: 'model:claude-haiku-4-5-20251001',
        setAt: ago(6 * MINUTE),
        until: ahead(54 * MINUTE),
        windowId: null,
      },
    ],
  },
  {
    ...ACCOUNT_SAMPLE,
    id: 'secondary',
    kind: 'oauth',
    label: 'secondary',
    email: 'operator+second@example.com',
    status: 'cooldown',
    routingEnabled: true,
    // Cancelled, with both ends set: the stored one the operator saved, the derived one the form would
    // otherwise have prefilled.
    cancelled: true,
    pools: [{ poolId: 'all', priority: 0 }],
    windows: [win('5h', 1, 1 * HOUR + 12 * MINUTE), win('7d', 0.88, 5 * DAY + 2 * HOUR + 5 * MINUTE)],
    fableWindow: null,
    extraUsage: null,
    fastLane: null,
    credential: {
      accessTokenExpiresAt: ahead(3 * HOUR + 2 * MINUTE),
      lastError: null,
      lastRefreshAt: ago(4 * HOUR + 58 * MINUTE),
      quarantined: false,
      refreshTokenExpiresAt: ahead(31 * DAY),
    },
    subscription: { billingType: 'monthly', organizationType: 'claude_pro', plan: 'pro', rateLimitTier: 'default_claude_pro', since: ago(20 * DAY), trialEndsAt: ahead(8 * DAY), endsAt: ahead(5 * DAY), derivedEndsAt: ahead(35 * DAY) },
    requests: { last24h: 0, last7d: 0, lifetime: 1_140 },
    cooldowns: [
      {
        reason: 'rate_limited (5h window exhausted)',
        scope: 'account',
        setAt: ago(28 * MINUTE),
        until: ahead(1 * HOUR + 12 * MINUTE),
        windowId: '5h',
      },
      // A second scope, one model, so the account's cooling-down block has an account row and a model row.
      {
        reason: 'model_rejected',
        scope: 'model:claude-opus-5',
        setAt: ago(22 * MINUTE),
        until: ahead(38 * MINUTE),
        windowId: null,
      },
    ],
  },
  {
    ...ACCOUNT_SAMPLE,
    id: 'nightly',
    kind: 'oauth',
    label: 'nightly',
    email: 'operator+nightly@example.com',
    status: 'ready',
    routingEnabled: true,
    pools: [{ poolId: 'all', priority: 0 }],
    windows: [win('5h', 0.97, 3 * HOUR + 41 * MINUTE), win('7d', 0.35, 1 * DAY + 18 * HOUR + 12 * MINUTE)],
    fableWindow: win('7d_fable', 0.41, 1 * DAY + 18 * HOUR + 12 * MINUTE),
    extraUsage: { disabledReason: 'Payment method declined on 02 Sep; overage paused by the provider', enabled: true, inUse: true, monthlyLimitCredits: 2000, usedCredits: 612 },
    fastLane: null,
    overage: { inUse: true, observedAt: ago(40_000), reset: ahead(1 * DAY + 18 * HOUR + 12 * MINUTE), since: ago(3 * HOUR) },
    credential: {
      accessTokenExpiresAt: ahead(5 * HOUR + 50 * MINUTE),
      lastError: null,
      lastRefreshAt: ago(2 * HOUR + 10 * MINUTE),
      quarantined: true,
      refreshTokenExpiresAt: ahead(72 * DAY),
    },
    subscription: { billingType: 'monthly', organizationType: 'claude_max', plan: 'max', rateLimitTier: 'default_claude_max_5x', since: ago(400 * DAY), trialEndsAt: null, endsAt: null, derivedEndsAt: ahead(20 * DAY) },
    requests: { last24h: 0, last7d: 0, lifetime: 7_420 },
    cooldowns: [],
  },
  {
    ...ACCOUNT_SAMPLE,
    id: 'overflow',
    kind: 'api_key',
    label: 'Overflow workspace',
    email: null,
    status: 'funding_required',
    routingEnabled: true,
    pools: [{ poolId: 'all', priority: 1 }],
    windows: [],
    fableWindow: null,
    extraUsage: null,
    fastLane: fastLane(0.18, 0.05, 40_000),
    credential: { accessTokenExpiresAt: null, lastError: null, lastRefreshAt: null, quarantined: false, refreshTokenExpiresAt: null },
    subscription: null,
    requests: { last24h: 0, last7d: 0, lifetime: 3_104 },
    cooldowns: [],
  },
  {
    ...ACCOUNT_SAMPLE,
    id: 'research',
    kind: 'api_key',
    label: 'Research workspace',
    email: null,
    status: 'ready',
    routingEnabled: true,
    pools: [{ poolId: 'codex', priority: 0 }],
    windows: [],
    fableWindow: null,
    extraUsage: null,
    fastLane: null,
    credential: { accessTokenExpiresAt: null, lastError: null, lastRefreshAt: null, quarantined: false, refreshTokenExpiresAt: null },
    subscription: null,
    requests: { last24h: 0, last7d: 0, lifetime: 1_480 },
    cooldowns: [],
  },
  {
    ...ACCOUNT_SAMPLE,
    id: 'legacy',
    kind: 'oauth',
    label: 'legacy',
    email: 'operator+old@example.com',
    status: 'reauth_required',
    routingEnabled: false,
    pools: [],
    windows: [win('5h', 0.18, -2 * DAY, 2 * DAY + 3 * HOUR), win('7d', 0.52, 2 * DAY, 2 * DAY + 3 * HOUR)],
    fableWindow: null,
    extraUsage: null,
    fastLane: null,
    credential: {
      accessTokenExpiresAt: ago(2 * DAY + 2 * HOUR),
      lastError: 'refresh_token rejected: invalid_grant',
      lastRefreshAt: ago(2 * DAY + 3 * HOUR),
      quarantined: false,
      refreshTokenExpiresAt: ago(2 * DAY + 2 * HOUR),
    },
    subscription: { billingType: 'monthly', organizationType: 'claude_pro', plan: 'pro', rateLimitTier: 'default_claude_pro', since: null, trialEndsAt: null, endsAt: null, derivedEndsAt: null },
    requests: { last24h: 0, last7d: 0, lifetime: 3_960 },
    cooldowns: [],
  },
  // Disconnected: the credential is gone and the identity it paired with stays, as a disconnect leaves the row, and
  // with no subscription block a page has to go by the kind to call it OAuth.
  {
    ...ACCOUNT_SAMPLE,
    id: 'spare',
    kind: 'oauth',
    label: 'spare',
    email: 'operator+spare@example.com',
    status: 'unpaired',
    routingEnabled: true,
    pools: [],
    windows: [],
    fableWindow: null,
    extraUsage: null,
    fastLane: null,
    credential: { accessTokenExpiresAt: null, lastError: null, lastRefreshAt: null, quarantined: false, refreshTokenExpiresAt: null },
    subscription: null,
    requests: { last24h: 0, last7d: 0, lifetime: 0 },
    cooldowns: [],
  },
] satisfies Account[]).map((a) => ({ ...a, plan: planName(a.subscription), usage: usageOf(a) }));

export const accountById = new Map(ACCOUNTS.map((a) => [a.id, a]));

/** The poll block the route answers on a row: none for an Anthropic-key account, which the poll never reads. */
export function usageOf(a: Pick<Account, 'id' | 'kind' | 'status'>): QuotaSnapshot | null {
  if (a.kind === 'api_key') return null;
  if (a.status === 'unpaired') return { ...QUOTA_SNAPSHOT_SAMPLE, accountId: a.id, error: 'no credential is stored for this account; pair it to load quota', fetchedAt: null, poll: null, status: 'error' };
  // The refresh keeps failing, so every poll fails while the windows of the last clean read still answer.
  if (a.id === 'legacy') {
    return {
      ...QUOTA_SNAPSHOT_SAMPLE,
      accountId: a.id,
      error: null,
      fetchedAt: ago(2 * DAY + 3 * HOUR),
      poll: { backoffSince: null, lastAttemptAt: ago(3 * MINUTE), lastCleanAt: ago(2 * DAY + 3 * HOUR), lastError: 'usage request rejected with status 503', lastOutcome: 'failed', nextAttemptAt: null, rateLimitedInARow: 0 },
      status: 'success',
    };
  }
  // Past thirty minutes of 429s, so the poll_backoff condition holds for this one.
  if (a.id === 'secondary') {
    const nextAttemptAt = ahead(4 * MINUTE);
    // The hold sentence as the router writes it, its instant cut to whole seconds.
    const lastError = `the usage endpoint rate-limited the account; next attempt at ${nextAttemptAt.slice(0, 10)} ${nextAttemptAt.slice(11, 19)} UTC`;
    return {
      ...QUOTA_SNAPSHOT_SAMPLE,
      accountId: a.id,
      error: null,
      fetchedAt: ago(47 * MINUTE),
      poll: { backoffSince: ago(42 * MINUTE), lastAttemptAt: ago(11 * MINUTE), lastCleanAt: ago(47 * MINUTE), lastError, lastOutcome: 'rate_limited', nextAttemptAt, rateLimitedInARow: 6 },
      status: 'success',
    };
  }
  const poll = QUOTA_SNAPSHOT_SAMPLE.poll && { ...QUOTA_SNAPSHOT_SAMPLE.poll, lastAttemptAt: ago(40_000), lastCleanAt: ago(40_000) };
  return { ...QUOTA_SNAPSHOT_SAMPLE, accountId: a.id, error: null, fetchedAt: ago(40_000), poll, status: 'success' };
}

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

const HISTORY_SEED: Partial<Record<WindowId, number>> = { '5h': 1, '7d': 2, '7d_fable': 3 };

export const QUOTA_HISTORY: QuotaHistory[] = ACCOUNTS.flatMap((account, i) =>
  [...account.windows, ...(account.fableWindow ? [account.fableWindow] : [])].map((w) => ({
    accountId: account.id,
    windowId: w.id,
    samples: sawtooth(1000 + i * 7 + (HISTORY_SEED[w.id] ?? 4), Date.parse(w.resetsAt ?? ''), w.periodHours * HOUR, w.utilization),
  })),
);
