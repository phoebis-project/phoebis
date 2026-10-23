import { planName } from '@/api/decode';
import type { Account, QuotaHistory, QuotaSample, QuotaSnapshot, RateLimitWindow, WindowId } from '@/api/types';
import { endOf } from '@/lib/quota';
import { WINDOW_HOURS, WINDOW_LABEL, hoursOfKey } from '@/lib/windows';
import { leadOf } from './lead';
import { createRng } from './rng';
import { ACCOUNT_SAMPLE, QUOTA_SNAPSHOT_SAMPLE } from './samples';
import { DAY, HOUR, MINUTE, NOW, ago, ahead, iso } from './time';

/** The windows the router projects, the 5-hour, the 7-day and the 7-day Fable window, as the route serves them. */
const PROJECTED: readonly string[] = ['5h', '7d', '7d_fable'];

/**
 * A window as the account routes answer it: `burn` is the share of the window the sessions on the account burn per
 * hour, and the projection runs it to the horizon, the reset on the 5-hour window and the earlier of the reset and five
 * hours out on a weekly one. A null burn is a window the router holds no reading of, which projects nothing, and any
 * other window, which the router does not project, carries none either. The demand is the burn the sessions would
 * reach unslowed, which equals the burn on an account no slow-down holds.
 */
function win(id: WindowId, utilization: number, resetsInMs: number, observedAgoMs = 40_000, given: number | null = 0): RateLimitWindow {
  const burn = PROJECTED.includes(id) ? given : null;
  const horizonInMs = id === '5h' ? resetsInMs : Math.min(5 * HOUR, resetsInMs);
  return {
    burn,
    demand: burn,
    horizon: burn === null ? null : ahead(horizonInMs),
    id,
    label: WINDOW_LABEL[id] ?? id,
    lead: null,
    observedAt: ago(observedAgoMs),
    periodHours: WINDOW_HOURS[id] ?? hoursOfKey(id),
    projection: burn === null ? null : Math.round((utilization + (burn * horizonInMs) / HOUR) * 1_000) / 1_000,
    resetsAt: ahead(resetsInMs),
    utilization,
  };
}

/** The fast lane's two buckets as the last answer to a fast request on the account reported them. */
function fastLane(input: number, output: number, resetsInMs: number): Account['fastLane'] {
  const reading = (utilization: number) => ({ observedAt: ago(40_000), resetsAt: ahead(resetsInMs), utilization });
  return { input: reading(input), output: reading(output) };
}

/** How long before now spare's last poll read its windows, an instant its row, its snapshot and its samples share. */
const SPARE_READ_AGO = 4 * DAY + 2 * HOUR;

/** How long before now legacy's last clean poll read its windows. */
const LEGACY_CLEAN_AGO = 2 * DAY + 3 * HOUR;
/** When legacy's refresh was refused, at the poll slot whose acquisition ran it, which lapsed the credential. */
export const LEGACY_LAPSED_AT = NOW - (2 * DAY + 2 * HOUR);
/** The poll that read legacy's 5-hour window at its limit, an hour and twenty minutes before its last clean poll. */
export const LEGACY_FIVE_HOUR_SPENT_AT = NOW - LEGACY_CLEAN_AGO - HOUR - 20 * MINUTE;

// Each plan label derives from the entry's subscription by the decoder's own rule, so the mock and the served
// console draw one name. The day's and the week's request counts derive from the request rows and the rollups in
// `requests.ts`; a lifetime reaches past both, so it is the story's own, raised there to what the rollups hold. The
// pools' member lists hold every membership and the mock reads an account's pool off them at each answer, so no
// entry seeds one and a member write moves it.
export const ACCOUNTS: Account[] = ([
  {
    ...ACCOUNT_SAMPLE,
    id: 'primary',
    kind: 'oauth',
    label: 'primary',
    email: 'operator@example.com',
    status: 'cooldown',
    routingEnabled: true,
    // The poll's week for a model the family table has no row for: kept and shown under its key, read by no rule.
    // Its sessions burn the 5-hour window fast enough to read 19% left at the reset, so the 81% it projects to use
    // passes the all pool's ceiling.
    windows: [win('5h', 0.62, 2 * HOUR + 14 * MINUTE, 40_000, 0.085), win('7d', 0.41, 3 * DAY + 5 * HOUR + 40 * MINUTE, 40_000, 0.006), win('seven_day_opus', 0.08, 3 * DAY + 5 * HOUR + 40 * MINUTE)],
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
    subscription: { billingType: 'annual', organizationType: 'claude_max', plan: 'max', rateLimitTier: 'default_claude_max_20x', since: ago(200 * DAY), trialEndsAt: null, endsAt: null },
    requests: { last24h: 0, last7d: 0, lifetime: 5_380 },
    // A live model-scoped cooldown, which the pool eligibility check leaves alone (that reads the account-wide
    // scope alone) but which this account's own status folds in like any other cooldown. The fast-lane row ahead of
    // it holds fast-mode requests alone, so neither the status nor the header's cooldown badge reads it, and it lifts
    // sooner, so a page reading the first row would name the wrong reset.
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
        setAt: ago(5 * MINUTE + 50_000),
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
    // Cancelled, with the end the operator saved before the trial's own, so the pace reads the saved one.
    canceled: true,
    // The 7-day reading crosses 80% between the story's 14:01:45 and 14:03 placements, so Pace takes secondary over
    // primary for the hour's sessions before the crossing and primary over it after.
    windows: [win('5h', 1, 25 * MINUTE), win('7d', 0.808, 5 * DAY + 2 * HOUR + 5 * MINUTE)],
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
    subscription: { billingType: 'monthly', organizationType: 'claude_pro', plan: 'pro', rateLimitTier: 'default_claude_ai', since: ago(30 * DAY), trialEndsAt: ahead(8 * DAY), endsAt: ahead(5 * DAY) },
    requests: { last24h: 0, last7d: 0, lifetime: 1_140 },
    cooldowns: [
      {
        reason: 'rate_limit (5h window exhausted)',
        scope: 'account',
        setAt: ago(20 * MINUTE + 29_000),
        until: ahead(25 * MINUTE),
        windowId: '5h',
      },
      // A second scope, one model, so the account's cooling-down block has an account row and a model row. Set
      // before the account cooled, when a request could still reach it.
      {
        reason: 'model_rejected',
        scope: 'model:claude-opus-5',
        setAt: ago(34 * MINUTE),
        until: ahead(26 * MINUTE),
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
    // Past every pool's ceiling at its reset, 104%.
    windows: [win('5h', 0.97, 3 * HOUR + 41 * MINUTE, 40_000, 0.019), win('7d', 0.35, 1 * DAY + 18 * HOUR + 12 * MINUTE)],
    // The 7-day Fable window reads under all's ceiling, near enough for the morning's Fable session to project it past.
    fableWindow: win('7d_fable', 0.66, 1 * DAY + 18 * HOUR + 12 * MINUTE),
    extraUsage: { disabledReason: null, enabled: true, inUse: true, monthlyLimitCredits: 2000, usedCredits: 612 },
    fastLane: null,
    // In plan overage from the first head after its spend cap was raised at 11:30:12, which `requests.ts` dates once
    // the day's requests are drawn. The raise stands in until then.
    overage: { inUse: true, observedAt: ago(40_000), reset: ahead(1 * DAY + 18 * HOUR + 12 * MINUTE), since: ago(2 * HOUR + 59 * MINUTE + 48_000) },
    credential: {
      accessTokenExpiresAt: ahead(5 * HOUR + 50 * MINUTE),
      lastError: null,
      lastRefreshAt: ago(2 * HOUR + 10 * MINUTE),
      quarantined: true,
      refreshTokenExpiresAt: ahead(72 * DAY),
    },
    subscription: { billingType: 'monthly', organizationType: 'claude_max', plan: 'max', rateLimitTier: 'default_claude_max_5x', since: ago(400 * DAY), trialEndsAt: null, endsAt: null },
    requests: { last24h: 0, last7d: 0, lifetime: 7_420 },
    cooldowns: [],
  },
  // The work pool's Max 5x login, paired a fortnight ago. At 14:05 its 5-hour window read 0.86 with 84 min to the
  // reset at 15:29, where the pace stood at 0.72, and its sessions' demand of 0.1286 an hour would pass the ceiling 42
  // min before the reset, so the pool's slow-down, slowing them since 13:48, holds them at half speed: their burn is
  // half their demand, and the window reads 0.887 now, the burn since 14:05 added, which lands it on the ceiling at the
  // reset.
  {
    ...ACCOUNT_SAMPLE,
    id: 'studio',
    kind: 'oauth',
    label: 'studio',
    email: 'operator+studio@example.com',
    status: 'ready',
    routingEnabled: true,
    windows: [{ ...win('5h', 0.887, 59 * MINUTE, 40_000, 0.0643), demand: 0.1286 }, win('7d', 0.27, 4 * DAY + 9 * HOUR + 20 * MINUTE)],
    fableWindow: win('7d_fable', 0.19, 4 * DAY + 9 * HOUR + 20 * MINUTE),
    extraUsage: null,
    fastLane: null,
    credential: {
      accessTokenExpiresAt: ahead(4 * HOUR + 35 * MINUTE),
      lastError: null,
      lastRefreshAt: ago(3 * HOUR + 25 * MINUTE),
      quarantined: false,
      refreshTokenExpiresAt: ahead(64 * DAY),
    },
    subscription: { billingType: 'monthly', organizationType: 'claude_max', plan: 'max', rateLimitTier: 'default_claude_max_5x', since: ago(16 * DAY), trialEndsAt: null, endsAt: null },
    requests: { last24h: 0, last7d: 0, lifetime: 48 },
    cooldowns: [],
    slowDown: { factor: 0.5, holdMs: 120_000, since: ago(42 * MINUTE), speed: 0.5, stretchMs: 2_520_000, thresholdMs: 1_800_000 },
  },
  // The work pool's API-key member, which takes the sessions a round moves off studio.
  {
    ...ACCOUNT_SAMPLE,
    id: 'annex',
    kind: 'api_key',
    label: 'Studio annex',
    email: null,
    status: 'ready',
    routingEnabled: true,
    windows: [],
    fableWindow: null,
    extraUsage: null,
    fastLane: null,
    credential: { accessTokenExpiresAt: null, lastError: null, lastRefreshAt: null, quarantined: false, refreshTokenExpiresAt: null },
    subscription: null,
    requests: { last24h: 0, last7d: 0, lifetime: 0 },
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
    windows: [],
    fableWindow: null,
    extraUsage: null,
    fastLane: null,
    credential: { accessTokenExpiresAt: null, lastError: null, lastRefreshAt: null, quarantined: false, refreshTokenExpiresAt: null },
    subscription: null,
    // The jobs pool's one member, so its lifetime holds what both of that pool's keys sent.
    requests: { last24h: 0, last7d: 0, lifetime: 3_400 },
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
    // Read before their resets and never since, so the router projects neither.
    windows: [win('5h', 0.18, -2 * DAY, 2 * DAY + 3 * HOUR, null), win('7d', 0.52, 2 * DAY, 2 * DAY + 3 * HOUR, null)],
    fableWindow: null,
    // Off since the provider stopped funding it, as the last clean poll read it.
    extraUsage: { disabledReason: 'Payment method declined on 02 Sep. Overage paused by the provider', enabled: false, inUse: false, monthlyLimitCredits: 0, usedCredits: 0 },
    fastLane: null,
    credential: {
      accessTokenExpiresAt: ago(2 * DAY + 2 * HOUR),
      lastError: 'refresh_token rejected: invalid_grant',
      lastRefreshAt: ago(2 * DAY + 3 * HOUR),
      quarantined: false,
      refreshTokenExpiresAt: ago(2 * DAY + 2 * HOUR),
    },
    subscription: { billingType: 'monthly', organizationType: 'claude_pro', plan: 'pro', rateLimitTier: 'default_claude_ai', since: null, trialEndsAt: null, endsAt: null },
    requests: { last24h: 0, last7d: 0, lifetime: 3_960 },
    cooldowns: [],
  },
  // Disconnected: the credential is gone and the identity it paired with stays, as a disconnect leaves the row, and
  // with no subscription block a page has to go by the kind to call it OAuth. The router records `operator` as the
  // row's `state_cause`, which the decoded row does not carry. The windows are the last poll's before the disconnect,
  // standing in for a router that still answers them for a disconnected row, and none is projected.
  {
    ...ACCOUNT_SAMPLE,
    id: 'spare',
    kind: 'oauth',
    label: 'spare',
    email: 'operator+spare@example.com',
    status: 'unpaired',
    routingEnabled: true,
    windows: [win('5h', 0.4, -(3 * DAY + 20 * HOUR), SPARE_READ_AGO, null), win('7d', 0.02, 3 * DAY + 4 * HOUR, SPARE_READ_AGO, null)],
    fableWindow: win('7d_fable', 0, 3 * DAY + 4 * HOUR, SPARE_READ_AGO, null),
    extraUsage: null,
    fastLane: null,
    credential: { accessTokenExpiresAt: null, lastError: null, lastRefreshAt: null, quarantined: false, refreshTokenExpiresAt: null },
    subscription: null,
    requests: { last24h: 0, last7d: 0, lifetime: 0 },
    cooldowns: [],
  },
] satisfies Account[]).map((a) => {
  // Standing in for the router, which answers each window's lead toward the earlier of its reset and the account's end.
  const end = endOf(a, NOW);
  const led = (w: RateLimitWindow): RateLimitWindow => ({ ...w, lead: leadOf(w, w.periodHours * HOUR, NOW, end) });
  return { ...a, fableWindow: a.fableWindow === null ? null : led(a.fableWindow), plan: planName(a.subscription), pool: null, usage: usageOf(a), windows: a.windows.map(led) };
});

export const accountById = new Map(ACCOUNTS.map((a) => [a.id, a]));

/** The poll block the route answers on a row: none for an API-key account, which the poll never reads. */
export function usageOf(a: Pick<Account, 'id' | 'kind' | 'status'>): QuotaSnapshot | null {
  if (a.kind === 'api_key') return null;
  // The polls stop with the credential, so the snapshot keeps the reading from before the disconnect and no poll.
  if (a.id === 'spare') return { ...QUOTA_SNAPSHOT_SAMPLE, accountId: a.id, error: null, fetchedAt: ago(SPARE_READ_AGO), heldUntil: null, poll: null, status: 'success' };
  if (a.status === 'unpaired') return { ...QUOTA_SNAPSHOT_SAMPLE, accountId: a.id, error: 'no credential is stored for this account, so pair it to load quota', fetchedAt: null, heldUntil: null, poll: null, status: 'error' };
  // The polls after the last clean read failed on the endpoint's 503 until the slot whose acquisition ran the refresh
  // the provider refused, and the poll asks a lapsed account nothing, so that attempt stands as the last, under the
  // words the usage service records for a failed acquisition, while the windows of the last clean read still answer.
  if (a.id === 'legacy') {
    return {
      ...QUOTA_SNAPSHOT_SAMPLE,
      accountId: a.id,
      error: null,
      fetchedAt: ago(LEGACY_CLEAN_AGO),
      heldUntil: null,
      poll: { backoffSince: null, lastAttemptAt: iso(LEGACY_LAPSED_AT), lastCleanAt: ago(LEGACY_CLEAN_AGO), lastError: 'acquire: account has to be paired again', lastOutcome: 'failed', nextAttemptAt: null, rateLimitedInARow: 0 },
      status: 'success',
    };
  }
  // Past thirty minutes of 429s, so the poll_backoff condition holds for this one.
  if (a.id === 'secondary') {
    const nextAttemptAt = ahead(4 * MINUTE);
    // The router's hold sentence names the endpoint alone, and the deadline rides the snapshot as the poll's next attempt.
    const lastError = 'the usage endpoint rate-limited the account';
    return {
      ...QUOTA_SNAPSHOT_SAMPLE,
      accountId: a.id,
      error: null,
      fetchedAt: ago(47 * MINUTE),
      heldUntil: nextAttemptAt,
      poll: { backoffSince: ago(42 * MINUTE), lastAttemptAt: ago(11 * MINUTE), lastCleanAt: ago(47 * MINUTE), lastError, lastOutcome: 'rate_limited', nextAttemptAt, rateLimitedInARow: 6 },
      status: 'success',
    };
  }
  const poll = QUOTA_SNAPSHOT_SAMPLE.poll && { ...QUOTA_SNAPSHOT_SAMPLE.poll, lastAttemptAt: ago(40_000), lastCleanAt: ago(40_000) };
  return { ...QUOTA_SNAPSHOT_SAMPLE, accountId: a.id, error: null, fetchedAt: ago(40_000), heldUntil: null, poll, status: 'success' };
}

const SAMPLE_STEP = 5 * MINUTE;
const HISTORY_SPAN = 7 * DAY;

// Concave climb: usage lands early in a window and tapers as the session cools.
const climb = (phase: number) => Math.min(1, Math.pow(Math.max(0, phase), 0.75));

function sawtooth(seed: number, resetsAtMs: number, periodMs: number, target: number, endMs: number): QuotaSample[] {
  const rng = createRng(seed);
  const samples: QuotaSample[] = [];
  const start = NOW - HISTORY_SPAN;
  const currentStart = resetsAtMs - periodMs;
  const peakFor = new Map<number, number>();
  for (let t = start; t <= endMs; t += SAMPLE_STEP) {
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

/**
 * Where an account's samples end: at its last reading, legacy's last clean poll, since every poll after it failed,
 * spare's reading before its disconnect, and the rest now.
 */
const samplesEndOf = (account: (typeof ACCOUNTS)[number]): number => {
  const last = account.id === 'legacy' ? account.usage?.poll?.lastCleanAt : account.id === 'spare' ? account.usage?.fetchedAt : null;
  return last ? Date.parse(last) : NOW;
};

/**
 * Legacy's 5-hour samples carry the readings its spent and cleared rows name: the poll at the limit, the polls after it
 * at the limit too, and the last clean poll back under it at the reading its account answers.
 */
function carryLegacyEdges(account: (typeof ACCOUNTS)[number], w: RateLimitWindow, samples: QuotaSample[]): QuotaSample[] {
  if (account.id !== 'legacy' || w.id !== '5h') return samples;
  const clean = samplesEndOf(account);
  return samples.map((x) => {
    const at = Date.parse(x.at);
    return at === clean ? { ...x, utilization: w.utilization } : at >= LEGACY_FIVE_HOUR_SPENT_AT && at < clean ? { ...x, utilization: 1 } : x;
  });
}

export const QUOTA_HISTORY: QuotaHistory[] = ACCOUNTS.flatMap((account, i) =>
  [...account.windows, ...(account.fableWindow ? [account.fableWindow] : [])].map((w) => ({
    accountId: account.id,
    windowId: w.id,
    samples: carryLegacyEdges(account, w, sawtooth(1000 + i * 7 + (HISTORY_SEED[w.id] ?? 4), Date.parse(w.resetsAt ?? ''), w.periodHours * HOUR, w.utilization, samplesEndOf(account))),
  })),
);
