import { decodeLimitResets, planName } from '@/api/decode';
import type { Account, BurnUnit, FeatureWord, PlanWord, QuotaHistory, QuotaSample, QuotaSnapshot, RateLimitWindow, Subscription, WindowId } from '@/api/types';
import { FEATURE_WORD_VALUES } from '@/api/types';
import { lookup } from '@/lib/counts';
import { endOf, isPaidSource, readWindows } from '@/lib/quota';
import { kindGroup } from '@/lib/selection';
import { periodHoursOf, windowKeyId, windowLabelOf } from '@/lib/windows';
import { leadOf } from './lead';
import { RESET_BLOCKS, limitResetsWire } from './resets';
import { createRng } from './rng';
import { round } from './round';
import { ACCOUNT_SAMPLE, QUOTA_SNAPSHOT_SAMPLE } from './samples';
import { DAY, HOUR, MINUTE, NIGHTLY_CAP_RAISED, NOW, ago, ahead, clock, endOfMonth, iso } from './time';

/** The windows the router projects, the 5-hour, the 7-day and the 7-day Fable window, as the route serves them. */
const PROJECTED: readonly string[] = ['5h', '7d', '7d_fable'];

/**
 * A window as the account routes answer it: `burn` is the share of the window the sessions on the account burn per
 * hour, and the projection runs it to the horizon, the reset on the 5-hour window and the earlier of the reset and five
 * hours out on a weekly one, five hours out on a window with no reset. A null burn is a window the router holds no
 * reading of, which projects nothing, and any other window, which the router does not project, carries none either.
 * The demand is the burn the sessions would reach unslowed, which equals the burn on an account no slow-down holds.
 */
function win(id: WindowId, utilization: number, resetsInMs: number | null, observedAgoMs = 40_000, given: number | null = 0): RateLimitWindow {
  const burn = PROJECTED.includes(id) ? given : null;
  const reset = resetsInMs ?? 5 * HOUR;
  const horizonInMs = id === '5h' ? reset : Math.min(5 * HOUR, reset);
  return {
    burn,
    demand: burn,
    horizon: burn === null ? null : ahead(horizonInMs),
    id,
    label: windowLabelOf(id),
    lead: null,
    observedAt: ago(observedAgoMs),
    periodHours: periodHoursOf(id),
    projection: burn === null ? null : round(utilization + (burn * horizonInMs) / HOUR, 3),
    resetsAt: resetsInMs === null ? null : ahead(resetsInMs),
    utilization,
  };
}

/** The fast lane's two buckets as the last answer to a fast request on the account reported them. */
function fastLane(input: number, output: number, resetsInMs: number): Account['fastLane'] {
  const reading = (utilization: number) => ({ observedAt: ago(40_000), resetsAt: ahead(resetsInMs), utilization });
  return { input: reading(input), output: reading(output) };
}

/**
 * How long before now spare's last poll read its windows, an instant its row, its snapshot and its samples share. It
 * lies an hour into the period each window stood in then, so the samples end inside it and every reset follows it.
 */
const SPARE_READ_AGO = 3 * DAY + 19 * HOUR;

/** How long before now legacy's last clean poll read its windows. */
const LEGACY_CLEAN_AGO = 2 * DAY + 3 * HOUR;
/** When legacy's refresh was refused, at the poll slot whose acquisition ran it, which lapsed the credential. */
export const LEGACY_LAPSED_AT = NOW - (2 * DAY + 2 * HOUR);
/** The poll that read legacy's 5-hour window at its limit, an hour and twenty minutes before its last clean poll. */
export const LEGACY_FIVE_HOUR_SPENT_AT = NOW - LEGACY_CLEAN_AGO - HOUR - 20 * MINUTE;

/** How long before now reserve's last clean poll read its windows, before its 5-hour window reset. */
const RESERVE_CLEAN_AGO = 35 * MINUTE;
/** Reserve's last poll, the sixth 503 at the poll's five-minute cadence since the clean one. */
const RESERVE_POLLED_AGO = 5 * MINUTE;

/** How long before now workshop's last clean poll read its windows. */
const WORKSHOP_CLEAN_AGO = 5 * MINUTE + 40_000;
/** Workshop's last poll, the one 503 at the poll's five-minute cadence since the clean one. */
const WORKSHOP_POLLED_AGO = 40_000;

/** How long before now sunset's saved end passed. */
export const SUNSET_END_AGO = 3 * DAY;
/**
 * Sunset's last clean poll, one poll interval before its end, so its reading is a sample strictly before the end and a
 * read of the last sample before the end finds it.
 */
export const SUNSET_LAST_POLL_AGO = SUNSET_END_AGO + 5 * MINUTE;
/** How long before now the profile read that found sunset's organization holding no plan set the flag. */
export const SUNSET_FLAGGED_AGO = 4 * HOUR;
/** When the 7-day window the end fell in resets: four days after the end, a day ahead of now, the window having
 *  started three days before the end. */
export const SUNSET_WEEK_RESETS_IN = 4 * DAY - SUNSET_END_AGO;

/**
 * The poll snapshots the story writes by hand for its paired rows, by account id, each a function of the row. Above the
 * account list, which reads them while the module loads.
 */
const SNAPSHOT_OF: Readonly<Record<string, (a: Pick<Account, 'id'>) => QuotaSnapshot>> = {
  // The polls after the last clean read failed on the endpoint's 503 until the slot whose acquisition ran the refresh
  // the provider refused, and the poll asks a lapsed account nothing, so that attempt stands as the last, under the
  // words the usage service records for a failed acquisition, while the snapshot keeps the last clean read.
  legacy: (a) => failedSinceClean(a.id, LEGACY_CLEAN_AGO, iso(LEGACY_LAPSED_AT), 'acquire: account has to be paired again'),
  // A 503 is a passing fault, neither a 429 nor a 401, so the poll holds nothing back and asks again at its next slot,
  // while the windows of the last clean read still answer.
  reserve: (a) => failedSinceClean(a.id, RESERVE_CLEAN_AGO, ago(RESERVE_POLLED_AGO), 'usage request rejected with status 503'),
  // Past thirty minutes of 429s, so the poll_backoff condition holds for this one.
  secondary: (a) => {
    const nextAttemptAt = ahead(4 * MINUTE);
    // The router's hold sentence names the endpoint alone, and the deadline rides the snapshot as the poll's next
    // attempt.
    const lastError = 'the usage endpoint rate-limited the account';
    return {
      ...QUOTA_SNAPSHOT_SAMPLE,
      accountId: a.id,
      error: null,
      fetchedAt: ago(47 * MINUTE),
      heldUntil: nextAttemptAt,
      poll: { backoffSince: ago(42 * MINUTE), lastAttemptAt: ago(11 * MINUTE), lastCleanAt: ago(47 * MINUTE), lastError, lastOutcome: 'rate_limited', nextAttemptAt, rateLimitedInARow: 6 },
    };
  },
  // The poll sends an account whose plan ended no usage request, so it has asked sunset nothing since the flag: the
  // read that set it stands as the last attempt, under the words the usage service records for the pause, while the
  // snapshot keeps the reading of the last clean poll before the end.
  sunset: (a) => failedSinceClean(a.id, SUNSET_LAST_POLL_AGO, ago(SUNSET_FLAGGED_AGO), 'plan ended'),
  // Workshop's failed poll, a passing 503 as on reserve, at the one slot since its last clean read, while the windows
  // of that read still answer.
  workshop: (a) => failedSinceClean(a.id, WORKSHOP_CLEAN_AGO, ago(WORKSHOP_POLLED_AGO), 'usage request rejected with status 503'),
};

/** The reset the router estimates while no head has named the spend window's: 00:00 UTC on the first of next month. */
const SPEND_RESET_ESTIMATE = endOfMonth(NOW);

/**
 * A story account as written: the account view's shape, its subscription's plan word left to the final map, and the
 * scopes of its stored sign-in where they are not the login's, which no route answers and the grants read.
 */
type Seed = Omit<Account, 'subscription'> & { scopes?: readonly string[]; subscription: Omit<Subscription, 'plan'> | null };

/** The credential of a row that holds no token. */
export const NO_CREDENTIAL: Account['credential'] = { accessTokenExpiresAt: null, lastError: null, lastRefreshAt: null, quarantined: false, refreshTokenExpiresAt: null };

/** The scope each feature's calls need, as the router's table holds it. */
const FEATURE_SCOPE: Record<FeatureWord, string> = { artifacts: 'user:inference', connectors: 'user:mcp_servers' };
/** The scopes a pairing asks for, which cover both features. */
const LOGIN_SCOPES: readonly string[] = ['org:create_api_key', 'user:profile', 'user:inference', 'user:sessions:claude_code', 'user:mcp_servers', 'user:file_upload', 'user:plugins'];

/**
 * The features an account's stored scopes cover, by the route's rule: none for an OAuth account that stores no
 * credential, whose access expiry the route then answers null, and null for an API-key account. A stored credential
 * holds the login's scopes, which a new account's pairing stores, unless its seed names an older sign-in's.
 */
export const featureGrantsOf = (a: Pick<Account, 'credential' | 'kind'> & Pick<Seed, 'scopes'>): Account['featureGrants'] =>
  a.kind === 'api_key' ? null : a.credential.accessTokenExpiresAt === null ? [] : FEATURE_WORD_VALUES.filter((f) => (a.scopes ?? LOGIN_SCOPES).includes(FEATURE_SCOPE[f]));

/** A row the router holds nothing for yet: no token, no plan, no window, no pool and no request. */
export const blankAccountRow = (input: Pick<Account, 'id' | 'kind' | 'label'>, status: Account['status'], email: string | null): Account => ({
  ...ACCOUNT_SAMPLE,
  cooldowns: [],
  credential: NO_CREDENTIAL,
  email,
  extraUsage: null,
  fableWindow: null,
  fastLane: null,
  featureGrants: featureGrantsOf({ credential: NO_CREDENTIAL, kind: input.kind }),
  id: input.id,
  kind: input.kind,
  label: input.label,
  limitResets: null,
  plan: null,
  pool: null,
  requests: { last24h: 0, last7d: 0, lifetime: 0 },
  routingEnabled: true,
  status,
  subscription: null,
  windows: [],
});

const CONTRACT_SUBSCRIPTION: Omit<Subscription, 'plan'> = { billingType: 'stripe_subscription_contracted', organizationType: 'claude_enterprise', planDisplayName: null, rateLimitTier: 'default_claude_zero', since: ago(45 * DAY), trialEndsAt: null, endsAt: null };

// Each plan label derives from the entry's subscription by the decoder's own rule, so the mock and the served
// console draw one name. The day's and the week's request counts derive from the request rows and the rollups in
// `requests.ts`; a lifetime reaches past both, so it is the story's own, raised there to what the rollups hold. The
// pools' member lists hold every membership and the mock reads an account's pool off them at each answer, so no
// entry seeds one and a member write moves it.
const SEEDS = [
  {
    ...ACCOUNT_SAMPLE,
    id: 'primary',
    companionId: '3c9e41a7',
    kind: 'oauth',
    label: 'primary',
    email: 'operator@example.com',
    status: 'cooldown',
    routingEnabled: true,
    // The poll's week for a model the family table has no row for: kept and shown under its key, read by no rule.
    // Its sessions burn the 5-hour window at the climb its heads read, 0.62 in the 2 h 46 min since the window opened,
    // so it projects past its wall at the reset.
    windows: [win('5h', 0.62, 2 * HOUR + 14 * MINUTE, 40_000, 0.224), win('7d', 0.41, 3 * DAY + 5 * HOUR + 40 * MINUTE, 40_000, 0.006), win('seven_day_opus', 0.08, 3 * DAY + 5 * HOUR + 40 * MINUTE)],
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
    subscription: { billingType: 'annual', organizationType: 'claude_max', planDisplayName: 'Ultra', rateLimitTier: 'default_claude_max_20x', since: ago(200 * DAY), trialEndsAt: null, endsAt: null },
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
    companionId: '8b2d06f5',
    kind: 'oauth',
    label: 'secondary',
    email: 'operator+second@example.com',
    status: 'cooldown',
    routingEnabled: true,
    // Canceled, with the end the operator saved before the trial's own, so the pace reads the saved one.
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
    subscription: { billingType: 'monthly', organizationType: 'claude_pro', planDisplayName: null, rateLimitTier: 'default_claude_ai', since: ago(30 * DAY), trialEndsAt: ahead(8 * DAY), endsAt: ahead(5 * DAY) },
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
    companionId: 'e4170c3b',
    kind: 'oauth',
    label: 'nightly',
    email: 'operator+nightly@example.com',
    status: 'ready',
    routingEnabled: true,
    // Past every pool's ceiling at its reset, 104%.
    windows: [win('5h', 0.97, 3 * HOUR + 41 * MINUTE, 40_000, 0.019), win('7d', 0.35, 1 * DAY + 18 * HOUR + 12 * MINUTE)],
    // The 7-day Fable window reads under all's ceiling, near enough for the morning's Fable session to project it past.
    fableWindow: win('7d_fable', 0.66, 1 * DAY + 18 * HOUR + 12 * MINUTE),
    // No polled row of the day carried the amounts, so the reading the heads wrote names its state alone.
    extraUsage: { disabledReason: null, enabled: true, inUse: true, monthlyLimitCredits: null, usedCredits: null },
    fastLane: null,
    // In plan overage from the first head after its spend cap was raised at 11:30:12, which `requests.ts` dates once
    // the day's requests are drawn. The raise stands in until then.
    overage: { inUse: true, observedAt: ago(40_000), reset: ahead(1 * DAY + 18 * HOUR + 12 * MINUTE), since: iso(NIGHTLY_CAP_RAISED) },
    credential: {
      accessTokenExpiresAt: ahead(5 * HOUR + 50 * MINUTE),
      lastError: null,
      lastRefreshAt: ago(2 * HOUR + 10 * MINUTE),
      quarantined: true,
      refreshTokenExpiresAt: ahead(72 * DAY),
    },
    subscription: { billingType: 'monthly', organizationType: 'claude_max', planDisplayName: null, rateLimitTier: 'default_claude_max_5x', since: ago(400 * DAY), trialEndsAt: null, endsAt: null },
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
    companionId: '5a8fd291',
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
    subscription: { billingType: 'monthly', organizationType: 'claude_max', planDisplayName: null, rateLimitTier: 'default_claude_max_5x', since: ago(16 * DAY), trialEndsAt: null, endsAt: null },
    requests: { last24h: 0, last7d: 0, lifetime: 48 },
    cooldowns: [],
    slowDown: { factor: 0.5, holdMs: 120_000, ramp: 1, since: iso(clock(13, 48)), speed: 0.5, stretchMs: 2_520_000, thresholdMs: 1_800_000 },
  },
  // The work pool's API-key member, which takes the sessions a round moves off studio.
  {
    ...ACCOUNT_SAMPLE,
    id: 'annex',
    companionId: 'c06b7e48',
    kind: 'api_key',
    label: 'Studio annex',
    email: null,
    status: 'ready',
    routingEnabled: true,
    windows: [],
    fableWindow: null,
    extraUsage: null,
    fastLane: null,
    credential: NO_CREDENTIAL,
    subscription: null,
    requests: { last24h: 0, last7d: 0, lifetime: 0 },
    cooldowns: [],
  },
  {
    ...ACCOUNT_SAMPLE,
    id: 'overflow',
    companionId: '71d4a95e',
    kind: 'api_key',
    label: 'Overflow workspace',
    email: null,
    status: 'funding_required',
    // The operator paused its routing at `OVERFLOW_PAUSED_AT`, inside the five minutes before its funding retry fell
    // due, so the router admits no retry to it. Its funding flag outranks the pause in its status.
    routingEnabled: false,
    windows: [],
    fableWindow: null,
    extraUsage: null,
    fastLane: fastLane(0.18, 0.05, 40_000),
    credential: NO_CREDENTIAL,
    subscription: null,
    requests: { last24h: 0, last7d: 0, lifetime: 3_104 },
    cooldowns: [],
  },
  {
    ...ACCOUNT_SAMPLE,
    id: 'research',
    companionId: '9f3a2c60',
    kind: 'api_key',
    label: 'Research workspace',
    email: null,
    status: 'ready',
    routingEnabled: true,
    windows: [],
    fableWindow: null,
    extraUsage: null,
    fastLane: null,
    credential: NO_CREDENTIAL,
    subscription: null,
    // The jobs pool's one member, so its lifetime holds what both of that pool's keys sent.
    requests: { last24h: 0, last7d: 0, lifetime: 3_400 },
    cooldowns: [],
  },
  {
    ...ACCOUNT_SAMPLE,
    id: 'legacy',
    companionId: '2e85b1d9',
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
    subscription: { billingType: 'monthly', organizationType: 'claude_pro', planDisplayName: null, rateLimitTier: 'default_claude_ai', since: null, trialEndsAt: null, endsAt: null },
    requests: { last24h: 0, last7d: 0, lifetime: 3_960 },
    cooldowns: [],
  },
  // Disconnected: the credential is gone and the identity it paired with stays, as a disconnect leaves the row, and
  // with no subscription block a page has to go by the kind to call it OAuth. The router records `operator` as the
  // row's `state_cause`, which the decoded row does not carry. The windows are the last poll's before the disconnect,
  // which the answer withholds as the route does while the sample history and the usage block read them, and none is
  // projected.
  {
    ...ACCOUNT_SAMPLE,
    id: 'spare',
    companionId: 'd7c0384f',
    kind: 'oauth',
    label: 'spare',
    email: 'operator+spare@example.com',
    status: 'unpaired',
    routingEnabled: true,
    windows: [win('5h', 0.4, -(3 * DAY + 15 * HOUR), SPARE_READ_AGO, null), win('7d', 0.02, 3 * DAY + 4 * HOUR, SPARE_READ_AGO, null)],
    fableWindow: win('7d_fable', 0, 3 * DAY + 4 * HOUR, SPARE_READ_AGO, null),
    extraUsage: null,
    fastLane: null,
    credential: NO_CREDENTIAL,
    subscription: null,
    requests: { last24h: 0, last7d: 0, lifetime: 0 },
    cooldowns: [],
  },
  // A stand-by login in no pool, so no request reaches it, whose usage polls have failed on the endpoint's 503 since
  // its last clean read. Its windows are that read's: the 5-hour one reset after it with no reading since, and the
  // 7-day one still runs.
  {
    ...ACCOUNT_SAMPLE,
    id: 'reserve',
    companionId: '46ea9b12',
    kind: 'oauth',
    label: 'reserve',
    email: 'operator+reserve@example.com',
    status: 'ready',
    routingEnabled: true,
    windows: [win('5h', 0.71, -(20 * MINUTE), RESERVE_CLEAN_AGO, null), win('7d', 0.33, 2 * DAY + 6 * HOUR, RESERVE_CLEAN_AGO)],
    fableWindow: null,
    extraUsage: null,
    fastLane: null,
    credential: {
      accessTokenExpiresAt: ahead(5 * HOUR + 10 * MINUTE),
      lastError: null,
      lastRefreshAt: ago(2 * HOUR + 50 * MINUTE),
      quarantined: false,
      // Inside the attention line's three-day lead, so the story words a refresh token's end before it passes.
      refreshTokenExpiresAt: ahead(2 * DAY),
    },
    subscription: { billingType: 'monthly', organizationType: 'claude_pro', planDisplayName: null, rateLimitTier: 'default_claude_ai', since: ago(90 * DAY), trialEndsAt: null, endsAt: null },
    requests: { last24h: 0, last7d: 0, lifetime: 0 },
    cooldowns: [],
  },
  // The story's plan-ended account, a Pro login in the sidelined pool, which no key routes to, whose operator canceled
  // it. Its saved end passed three days ago and a profile read four hours ago found its organization holding no plan,
  // so the router answers no window for it, its poll has asked it nothing since the flag, and its 7-day trend ends at
  // its last clean poll. The router stores the document that read found, a free organization on Pro's tier word billed
  // as none, before it flags the account, so the subscription block reads Free. The windows are that poll's reading,
  // which the final map empties as the route does: the end lies inside both, the 5-hour one three hours old at the end
  // and resetting two hours after it, the 7-day one three days old at the end and resetting four days after it.
  {
    ...ACCOUNT_SAMPLE,
    id: 'sunset',
    companionId: 'b5137f8c',
    kind: 'oauth',
    label: 'sunset',
    email: 'operator+sunset@example.com',
    status: 'ready',
    routingEnabled: true,
    canceled: true,
    planEndedAt: ago(SUNSET_FLAGGED_AGO),
    windows: [win('5h', 0.12, -(SUNSET_END_AGO - 2 * HOUR), SUNSET_LAST_POLL_AGO, null), win('7d', 0.46, SUNSET_WEEK_RESETS_IN, SUNSET_LAST_POLL_AGO, null)],
    fableWindow: null,
    extraUsage: null,
    fastLane: null,
    credential: {
      accessTokenExpiresAt: ahead(4 * HOUR + 50 * MINUTE),
      lastError: null,
      lastRefreshAt: ago(3 * HOUR + 10 * MINUTE),
      quarantined: false,
      refreshTokenExpiresAt: ahead(52 * DAY),
    },
    subscription: { billingType: 'none', organizationType: 'claude_free', planDisplayName: null, rateLimitTier: 'default_claude_ai', since: ago(200 * DAY), trialEndsAt: null, endsAt: ago(SUNSET_END_AGO) },
    requests: { last24h: 0, last7d: 0, lifetime: 1_180 },
    cooldowns: [],
  },
  // An Enterprise contract seat, which the router classes usage-based from its tier: no plan window, and its spend
  // billed in dollars at API rates against the monthly limit. The poll read the amounts ten minutes ago and a head
  // moved the share since, and with no head naming the reset yet the router estimates it.
  {
    ...ACCOUNT_SAMPLE,
    id: 'contract',
    companionId: '0a6cd5e3',
    kind: 'oauth',
    label: 'contract',
    email: 'operator+contract@example.com',
    status: 'ready',
    routingEnabled: true,
    usageBased: true,
    windows: [],
    fableWindow: null,
    spend: {
      amounts: { currency: 'USD', exponent: 2, limitMinor: 250_000, polledAt: ago(10 * MINUTE), usedMinor: 102_500 },
      observedAt: ago(5 * MINUTE),
      resetSource: 'estimated',
      resetsAt: iso(SPEND_RESET_ESTIMATE),
      utilization: 0.41,
    },
    extraUsage: { disabledReason: null, enabled: true, inUse: false, monthlyLimitCredits: 250_000, usedCredits: 102_500 },
    overage: null,
    fastLane: null,
    credential: {
      accessTokenExpiresAt: ahead(6 * HOUR + 40 * MINUTE),
      lastError: null,
      lastRefreshAt: ago(1 * HOUR + 20 * MINUTE),
      quarantined: false,
      refreshTokenExpiresAt: ahead(80 * DAY),
    },
    subscription: CONTRACT_SUBSCRIPTION,
    requests: { last24h: 0, last7d: 0, lifetime: 0 },
    cooldowns: [],
  },
  // The Ultra the upstream approved: its poll stopped carrying the 5-hour window, so its last clean poll, 5 min 40 s
  // ago, read the 7-day window, the Opus week and the Fable week alone, and the poll at the next slot failed on the
  // endpoint's 503. After the older seeds, so every seed above keeps its history's draws, and its id sorts after each
  // of theirs, since a color slot is the id's sorted place: it takes the API-key overflow's slot, which no ring and no
  // Quota lane draws, and every older account keeps its color.
  {
    ...ACCOUNT_SAMPLE,
    id: 'workshop',
    companionId: 'f8294a76',
    kind: 'oauth',
    label: 'workshop',
    email: 'operator+workshop@example.com',
    status: 'ready',
    routingEnabled: true,
    windows: [win('7d', 0.22, 4 * DAY + 2 * HOUR + 10 * MINUTE, WORKSHOP_CLEAN_AGO, 0.002), win('seven_day_opus', 0.05, 4 * DAY + 2 * HOUR + 10 * MINUTE, WORKSHOP_CLEAN_AGO)],
    fableWindow: win('7d_fable', 0.09, 4 * DAY + 2 * HOUR + 10 * MINUTE, WORKSHOP_CLEAN_AGO),
    extraUsage: null,
    fastLane: null,
    credential: {
      accessTokenExpiresAt: ahead(5 * HOUR + 35 * MINUTE),
      lastError: null,
      lastRefreshAt: ago(2 * HOUR + 25 * MINUTE),
      quarantined: false,
      refreshTokenExpiresAt: ahead(61 * DAY),
    },
    subscription: { billingType: 'annual', organizationType: 'claude_max', planDisplayName: 'Ultra', rateLimitTier: 'default_claude_max_20x', since: ago(60 * DAY), trialEndsAt: null, endsAt: null },
    requests: { last24h: 0, last7d: 0, lifetime: 640 },
    cooldowns: [],
  },
  // An API-key account created six minutes ago, which the router's registry snapshot predates. Its id sorts after
  // workshop's, so every account above keeps its color, and it takes primary's slot, which an API-key account draws on
  // no ring and no Quota lane.
  {
    ...ACCOUNT_SAMPLE,
    id: 'youngest',
    kind: 'api_key',
    label: 'New workspace',
    email: null,
    status: 'ready',
    routingEnabled: true,
    windows: [],
    fableWindow: null,
    extraUsage: null,
    fastLane: null,
    credential: NO_CREDENTIAL,
    subscription: null,
    requests: { last24h: 0, last7d: 0, lifetime: 0 },
    cooldowns: [],
  },
  // A second Enterprise contract seat, at its spend cap: the poll read its amounts at the limit ten minutes ago, and
  // with no head naming the reset the router estimates it. Its id sorts last, so it takes the API-key research's slot,
  // which no ring and no Quota lane draws, for the spend lane it draws.
  {
    ...ACCOUNT_SAMPLE,
    id: 'zenith',
    kind: 'oauth',
    label: 'zenith',
    email: 'operator+zenith@example.com',
    status: 'ready',
    routingEnabled: true,
    usageBased: true,
    windows: [],
    fableWindow: null,
    spend: {
      amounts: { currency: 'USD', exponent: 2, limitMinor: 100_000, polledAt: ago(10 * MINUTE), usedMinor: 100_000 },
      observedAt: ago(10 * MINUTE),
      resetSource: 'estimated',
      resetsAt: iso(SPEND_RESET_ESTIMATE),
      utilization: 1,
    },
    extraUsage: { disabledReason: null, enabled: true, inUse: false, monthlyLimitCredits: 100_000, usedCredits: 100_000 },
    overage: null,
    fastLane: null,
    credential: {
      accessTokenExpiresAt: ahead(7 * HOUR + 5 * MINUTE),
      lastError: null,
      lastRefreshAt: ago(55 * MINUTE),
      quarantined: false,
      refreshTokenExpiresAt: ahead(72 * DAY),
    },
    // Paired under a sign-in that never asked for the connectors' scope, so the dialogs list it disabled there.
    scopes: ['user:profile', 'user:inference', 'user:sessions:claude_code', 'user:file_upload'],
    subscription: CONTRACT_SUBSCRIPTION,
    requests: { last24h: 0, last7d: 0, lifetime: 0 },
    cooldowns: [],
  },
  // A Pro login in no pool whose 5-hour window has not opened, which the router answers with no reset and a projection
  // of 0 five hours out. After the older seeds, so every seed above keeps its history's draws, and its id sorts last,
  // so every older account keeps its color: the palette's eight slots put it on reserve's.
  {
    ...ACCOUNT_SAMPLE,
    id: 'zephyr',
    kind: 'oauth',
    label: 'zephyr',
    email: 'operator+zephyr@example.com',
    status: 'ready',
    routingEnabled: true,
    windows: [win('5h', 0, null), win('7d', 0.04, 5 * DAY + 3 * HOUR)],
    fableWindow: null,
    extraUsage: null,
    fastLane: null,
    credential: {
      accessTokenExpiresAt: ahead(6 * HOUR + 20 * MINUTE),
      lastError: null,
      lastRefreshAt: ago(1 * HOUR + 40 * MINUTE),
      quarantined: false,
      refreshTokenExpiresAt: ahead(60 * DAY),
    },
    subscription: { billingType: 'monthly', organizationType: 'claude_pro', planDisplayName: null, rateLimitTier: 'default_claude_ai', since: ago(20 * DAY), trialEndsAt: null, endsAt: null },
    requests: { last24h: 0, last7d: 0, lifetime: 0 },
    cooldowns: [],
  },
] satisfies Seed[];

/** The word an organization type resolves to, the Max type's read by its tier. */
const PLAN_OF_TYPE = new Map<string, PlanWord>([
  ['claude_enterprise', 'enterprise'],
  ['claude_free', 'free'],
  ['claude_pro', 'pro'],
  ['claude_team', 'team'],
]);
/** The sizes a Max organization's tier names, a tier naming neither reading `max`. */
const MAX_OF_TIER = new Map<string, PlanWord>([
  ['default_claude_max_20x', 'max_20x'],
  ['default_claude_max_5x', 'max_5x'],
]);
/** The word a tier resolves to on its own, read for an organization type outside the five. */
const PLAN_OF_TIER = new Map<string, PlanWord>([
  ['default_claude_ai', 'pro'],
  ['default_claude_max_20x', 'max_20x'],
  ['default_claude_max_5x', 'max_5x'],
  ['default_claude_zero', 'usage_based'],
]);

/** The resolved plan as the account view answers it: the usage-based class first, the organization type second, the
 *  tier third, as the router resolves it. Null without a profile. */
function resolvedPlan(a: Pick<Seed, 'subscription' | 'usageBased'>): PlanWord | null {
  const sub = a.subscription;
  if (sub === null) return null;
  if (a.usageBased) return 'usage_based';
  if (sub.organizationType === 'claude_max') return MAX_OF_TIER.get(sub.rateLimitTier) ?? 'max';
  return PLAN_OF_TYPE.get(sub.organizationType) ?? PLAN_OF_TIER.get(sub.rateLimitTier) ?? null;
}

/**
 * Whether a row's state holds no credential the router reads: not paired, or awaiting re-authentication. The route
 * withholds such a row's windows, and the poll reads no such account.
 */
export const withholdsWindows = (a: Pick<Account, 'status'>): boolean => a.status === 'unpaired' || a.status === 'reauth_required';

/** Each row's last reading as the final map builds it, ahead of the state's withholding. */
const READINGS = new Map<string, Pick<Account, 'fableWindow' | 'windows'>>();

export const ACCOUNTS: Account[] = SEEDS.map(({ scopes, ...a }: Seed) => {
  const subscription: Subscription | null = a.subscription && { ...a.subscription, plan: resolvedPlan(a) };
  // Standing in for the router, which answers each window's lead toward the earlier of its reset and the account's end.
  const end = endOf({ ...a, subscription }, NOW);
  const led = (w: RateLimitWindow): RateLimitWindow => ({ ...w, lead: leadOf(w, w.periodHours * HOUR, NOW, end) });
  // The poll no longer asks an account whose plan ended, so the route shows no reset of it.
  const limitResets = a.planEndedAt === null ? decodeLimitResets(limitResetsWire(a.id, RESET_BLOCKS, NOW)) : null;
  const reading = { fableWindow: a.planEndedAt === null && a.fableWindow !== null ? led(a.fableWindow) : null, windows: a.planEndedAt === null ? a.windows.map(led) : [] };
  READINGS.set(a.id, reading);
  const answered = withholdsWindows(a) ? { fableWindow: null, windows: [] } : reading;
  return { ...a, fableWindow: answered.fableWindow, featureGrants: featureGrantsOf({ ...a, scopes }), limitResets, plan: planName(subscription), pool: null, subscription, usage: usageOf(a), windows: answered.windows };
});

/**
 * The windows of a row's last reading. The route withholds them from the answer on a row that is not paired or needs
 * re-auth, while the ledger keeps the hours and the events its polls wrote, so the history reads the reading the final
 * map built for such a row, and every other row's own.
 */
export const lastReadingOf = (a: Account): Pick<Account, 'fableWindow' | 'windows'> => (withholdsWindows(a) ? (READINGS.get(a.id) ?? a) : a);

export const accountById = new Map(ACCOUNTS.map((a) => [a.id, a]));
/**
 * The unit a pin's burn reads on the account it sits on: dollars on an API key or a usage-based seat, which has no plan
 * window, else a share of the account's own window, and none on an account the registry lacks.
 */
export const burnUnitOf = (accountId: string): BurnUnit | null => {
  const account = accountById.get(accountId);
  return account === undefined ? null : isPaidSource(account) ? 'usd' : 'window_share';
};

/**
 * The kind tier the engine files a member in where its pool prefers subscriptions: the plan-funded subscriptions, the
 * ones in overage, then the paid sources. Each caller reads the overage its own way, the engine's at the instant it
 * ranks. A pool that prefers none files every member in one tier.
 */
export const rankTier = (preferSubscription: boolean, paid: boolean, overage: boolean, priority: number): [number, number, number] => [preferSubscription ? kindGroup(paid, overage) : 0, priority, overage ? 1 : 0];

/** The two shared windows by the ledger's id. */
export const SHARED_WINDOWS = ['five_hour', 'seven_day'] as const;
/**
 * The shared windows an account's newest clean poll names, as the router holds its window set: the ones its reading
 * carries, and both before any reading names one. The model weeks are no part of the set.
 */
export function sharedWindowsOf(accountId: string): ('five_hour' | 'seven_day')[] {
  const ids: string[] = accountById.get(accountId)?.windows.map((w) => w.id) ?? [];
  const held = SHARED_WINDOWS.filter((w) => ids.includes(windowKeyId(w) ?? ''));
  return held.length === 0 ? [...SHARED_WINDOWS] : held;
}

/**
 * A snapshot whose polls failed since its last clean read, which it keeps. A declaration, since the account list above
 * reads the snapshots while the module loads.
 */
function failedSinceClean(accountId: string, cleanAgo: number, lastAttemptAt: string, lastError: string): QuotaSnapshot {
  return {
    ...QUOTA_SNAPSHOT_SAMPLE,
    accountId,
    error: null,
    fetchedAt: ago(cleanAgo),
    heldUntil: null,
    poll: { backoffSince: null, lastAttemptAt, lastCleanAt: ago(cleanAgo), lastError, lastOutcome: 'failed', nextAttemptAt: null, rateLimitedInARow: 0 },
  };
}

/** The poll block the route answers on a row: none for an API-key account, the general arm for a row without a
 *  credential (its newest reading's instant, no error and no poll where it holds windows, the no-credential error
 *  where it holds none), the story's own snapshot where the table names the row, else the default poll. */
export function usageOf(a: Pick<Account, 'id' | 'kind' | 'status' | 'windows'>): QuotaSnapshot | null {
  if (a.kind === 'api_key') return null;
  if (a.status === 'unpaired') {
    // The polls stop with the credential, while the router keeps the last reading past a disconnect.
    const read = a.windows.map((w) => Date.parse(w.observedAt));
    const fetchedAt = read.length === 0 ? null : iso(Math.max(...read));
    return { ...QUOTA_SNAPSHOT_SAMPLE, accountId: a.id, error: fetchedAt === null ? 'no credential is stored for this account, so pair it to load quota' : null, fetchedAt, heldUntil: null, poll: null };
  }
  const own = lookup(SNAPSHOT_OF, a.id);
  if (own !== undefined) return own(a);
  const poll = QUOTA_SNAPSHOT_SAMPLE.poll && { ...QUOTA_SNAPSHOT_SAMPLE.poll, lastAttemptAt: ago(40_000), lastCleanAt: ago(40_000) };
  return { ...QUOTA_SNAPSHOT_SAMPLE, accountId: a.id, error: null, fetchedAt: ago(40_000), heldUntil: null, poll };
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
  for (let t = start; t < endMs; t += SAMPLE_STEP) {
    const windowIndex = Math.floor((t - currentStart) / periodMs);
    const windowStart = currentStart + windowIndex * periodMs;
    let peak = peakFor.get(windowIndex);
    if (peak === undefined) {
      // The live window is the one the samples end in. Its last point is the reading itself at `endMs`, as the router's
      // last reading is, and the points before it climb a peak scaled to meet the reading there, floored so a window
      // read early in its period does not scale its earlier points past what they could reach. Older windows get their
      // own random shape.
      if (windowIndex === 0) {
        const phaseEnd = (endMs - currentStart) / periodMs;
        peak = target / Math.max(0.05, climb(phaseEnd));
      } else {
        peak = rng.float(0.25, 1.0) * (rng.chance(0.15) ? 0.35 : 1);
      }
      peakFor.set(windowIndex, peak);
    }
    const phase = (t - windowStart) / periodMs;
    const jitter = rng.float(-0.012, 0.012);
    const value = Math.max(0, Math.min(1, peak * climb(phase) + jitter));
    samples.push({ at: iso(t), utilization: round(value, 3) });
  }
  samples.push({ at: iso(endMs), utilization: round(target, 3) });
  return samples;
}

const HISTORY_SEED: Partial<Record<WindowId, number>> = { '5h': 1, '7d': 2, '7d_fable': 3 };

/**
 * Where an account's samples end: at its last reading, the last clean poll where every poll after it failed (legacy's,
 * reserve's and workshop's), an unpaired row's last reading before its disconnect, and the rest now.
 */
const samplesEndOf = (account: (typeof ACCOUNTS)[number]): number => {
  const last = account.usage?.poll?.lastOutcome === 'failed' ? account.usage.poll.lastCleanAt : account.status === 'unpaired' ? account.usage?.fetchedAt : null;
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

// The samples are built from the literal's windows, since a flagged account answers none. The ledger writes a sample of
// each polled window whatever its reset, and a window with no reset has no period to place earlier samples in, so it
// answers the one its last poll wrote, at the reading.
export const QUOTA_HISTORY: QuotaHistory[] = SEEDS.flatMap((seed, i) =>
  readWindows(seed).map((w) => ({
    accountId: seed.id,
    windowId: w.id,
    samples:
      w.resetsAt === null
        ? [{ at: iso(samplesEndOf(ACCOUNTS[i])), utilization: round(w.utilization, 3) }]
        : carryLegacyEdges(ACCOUNTS[i], w, sawtooth(1000 + i * 7 + (HISTORY_SEED[w.id] ?? 4), Date.parse(w.resetsAt), w.periodHours * HOUR, w.utilization, samplesEndOf(ACCOUNTS[i]))),
  })),
);
