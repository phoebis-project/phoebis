import type { Account, AccountSlowDown, Cooldown, ExtraUsage, KnownWindowId, PLAN_WORDS, PlanWord, PollOutcome, QuotaPoll, RateLimitWindow, Spend, SubscriptionCosts, WindowId } from '@/api/types';
import type { SortDir } from '@/lib/address';
import { tierRow } from '@/lib/costs';
import { lookup } from '@/lib/counts';
import { fmtAbsolute, fmtPct, fmtRelative, instant, plural } from '@/lib/format';
import { orderRows } from '@/lib/sort';
import { HOUR, MINUTE, span } from '@/lib/time';

export type MeterTone = 'good' | 'low' | 'warn';

// The names claude.ai gives the plans, keyed by the tier ids, which stay the keys everywhere the router stores a tier.
const TIER_LABEL: Readonly<Record<string, string>> = {
  claude_enterprise: 'Enterprise',
  claude_max_20x: 'Ultra',
  claude_max_5x: 'Max',
  claude_pro: 'Pro',
  claude_team: 'Team',
  default_claude_ai: 'Pro',
  default_claude_enterprise: 'Enterprise',
  default_claude_max_20x: 'Ultra',
  default_claude_max_5x: 'Max',
  default_claude_team: 'Team',
};

/**
 * A tier's label, for the cost reference's tiers, the rings' anchor and reasons, and an older decision record's tier
 * word: `claude_max_20x` and `default_claude_max_20x` read as "Ultra", `claude_max_5x` and `default_claude_max_5x` as
 * "Max", `claude_pro` and `default_claude_ai`, the tier the profile reports for Pro, as "Pro", and the Team and
 * Enterprise ids with and without `default_` as "Team" and "Enterprise", every other id as sent. An account's own plan
 * reads the profile's display name first where it carries one (`planName`), since a record or a capacity row carries
 * the tier alone.
 */
export function planLabel(tier: string): string {
  return lookup(TIER_LABEL, tier) ?? tier;
}

/**
 * The label of each resolved plan word. The map is keyed by string, since `PlanWord` is a string the strict compiler
 * cannot index an eight-key Record with, and the literal still satisfies the eight words, so a word of `PLAN_WORDS`
 * without a label fails the build.
 */
export const PLAN_WORD_LABEL: Readonly<Record<string, string>> = {
  enterprise: 'Enterprise',
  free: 'Free',
  max: 'Max (tier unknown)',
  max_20x: 'Ultra',
  max_5x: 'Max',
  pro: 'Pro',
  team: 'Team',
  usage_based: 'Usage-based',
} satisfies Record<(typeof PLAN_WORDS)[number], string>;

/** A resolved plan word's label, a word the table lacks shown as sent. */
export const planWordLabel = (word: PlanWord): string => lookup(PLAN_WORD_LABEL, word) ?? word;

/** The plan words the router's `priced_tier` answers the stored tier for, the cost table deciding whether it holds
 *  that tier. */
const TIERED_PLANS: ReadonlySet<string> = new Set(['enterprise', 'max', 'max_20x', 'max_5x', 'pro', 'team']);

/** Why the gate prices no tier, the ring's and the sized set's reason word. */
export type UnpricedReason = 'no priced capacity for a free plan' | 'a usage-based seat' | 'tier unknown';

/**
 * The tier the router prices an account's capacity at, its snapshot's `priced_tier` and the capacity lookup behind it
 * folded into one answer: the stored tier under `pro`, `max`, `max_5x`, `max_20x`, `team` and `enterprise` while the
 * cost table holds a row for it, and none with the reason for `free`, `usage_based`, no plan, a word outside the eight,
 * no tier or a tier the table lacks.
 */
export function pricedTier(plan: PlanWord | null, tier: string | null, costs: SubscriptionCosts): { tier: string } | { reason: UnpricedReason; tier: null } {
  if (plan === 'free') return { reason: 'no priced capacity for a free plan', tier: null };
  if (plan === 'usage_based') return { reason: 'a usage-based seat', tier: null };
  if (plan === null || !TIERED_PLANS.has(plan) || tier === null || tierRow(costs, tier) === undefined) return { reason: 'tier unknown', tier: null };
  return { tier };
}

/** Remaining share drives the color: green at 50% and above, amber at 20% and above, red below. */
export function meterTone(remaining: number): MeterTone {
  if (remaining >= 0.5) return 'good';
  if (remaining >= 0.2) return 'warn';
  return 'low';
}

const shareLeft = (utilization: number): number => Math.max(0, Math.min(1, 1 - utilization));
export const remainingOf = (w: RateLimitWindow) => shareLeft(w.utilization);
/** A spend reading's share left, clamped as a window's is, and null without a share or under no limit. */
export const spendLeftOf = (spend: Spend): number | null =>
  spend.utilization === null || (spend.amounts !== null && spend.amounts.limitMinor === null) ? null : shareLeft(spend.utilization);

/** An account's window by its id, the Fable week from `fableWindow`, which the decoder files apart from `windows`. */
export const windowOf = (account: Account, id: WindowId): RateLimitWindow | null => (id === '7d_fable' ? account.fableWindow : (account.windows.find((w) => w.id === id) ?? null));

/** Every window an account's poll read, the Fable week from `fableWindow` among them. */
export const readWindows = (account: Pick<Account, 'fableWindow' | 'windows'>): RateLimitWindow[] => (account.fableWindow === null ? account.windows : [...account.windows, account.fableWindow]);

/**
 * The account's extra usage as its plan bills it: none for a usage-based seat, which always pays in dollars, so none
 * of its spend is overage, and whose spend bar already shows the figure.
 */
export const planExtraUsage = (account: Pick<Account, 'extraUsage' | 'usageBased'>): ExtraUsage | null => (account.usageBased ? null : account.extraUsage);

/** The router's overage reading as the plan bills it: none for a usage-based seat, which always pays in dollars. */
export const planOverage = (account: Pick<Account, 'overage' | 'usageBased'>): Account['overage'] => (account.usageBased ? null : account.overage);

/** A window whose reset has passed with no reading since: the card and the timeline agree on this one test. */
export const isExpired = (w: RateLimitWindow, now: number) => w.resetsAt !== null && Date.parse(w.resetsAt) < now;

/** The slow-down a page draws the account's speed chip from: its entry while the speed the holds realized is under 1
 *  and the 5-hour window the entry was read from has not reset since, null otherwise. */
export function slowedOf(account: Account, now: number): AccountSlowDown | null {
  const slowDown = account.slowDown ?? null;
  const w5 = windowOf(account, '5h');
  return slowDown !== null && slowDown.speed < 1 && w5 !== null && !isExpired(w5, now) ? slowDown : null;
}

/**
 * The end the router's pace reads (`term::end_of`): the saved end while the account is canceled, else a trial end
 * still ahead.
 */
export function endOf(account: Pick<Account, 'canceled' | 'subscription'>, now: number): number | null {
  const term = account.subscription;
  if (!term) return null;
  if (account.canceled) return term.endsAt === null ? null : Date.parse(term.endsAt);
  const trial = term.trialEndsAt === null ? Number.NaN : Date.parse(term.trialEndsAt);
  return trial > now ? trial : null;
}

/** The operator's saved end as stored, standing only while the account is canceled, with no trial end in its place. */
export const canceledEndOf = (account: Pick<Account, 'canceled' | 'subscription'>): string | null => (account.canceled ? (account.subscription?.endsAt ?? null) : null);

/** The account's `end` where a window's pace runs to it, an end before the reset (one at the reset is the reset), and
 *  null otherwise. */
export const paceEnd = (resetsAt: string | null, end: number | null): number | null => (resetsAt !== null && end !== null && end < Date.parse(resetsAt) ? end : null);

/** " to its end at 15:42" for the end a pace runs to (`paceEnd`), as the decision card's Pace hover names it, and empty
 *  for none. */
export const toEndWords = (end: number | null, now: number): string => (end === null ? '' : ` to its end at ${instant(end, now)}`);

/** One less the share elapsed, read off the field as the utilization less the lead, so the mark and the words read one
 *  figure. Null without a reset or without a lead. */
export function remainingTimeShare(w: RateLimitWindow): number | null {
  if (w.resetsAt === null || w.lead === null) return null;
  return Math.min(1, Math.max(0, 1 - (w.utilization - w.lead)));
}

/** The lead in words off the answer's field: "12% ahead of pace", "5% behind pace", "on pace" within a point, with
 *  " to its end at 15:42" where the pace runs to the account's end. The lead is as of the answer, not aged on the
 *  page's clock between reads. Null without a reset or without a lead. */
export function paceWords(w: RateLimitWindow, now: number, end: number | null): string | null {
  if (w.resetsAt === null || w.lead === null) return null;
  const lead = w.lead;
  const words = Math.abs(lead) < 0.01 ? 'on pace' : `${fmtPct(Math.abs(lead))} ${lead > 0 ? 'ahead of' : 'behind'} pace`;
  return `${words}${toEndWords(paceEnd(w.resetsAt, end), now)}`;
}

/**
 * A span rounded to the minute in the routing log's spelling: `27 min`, `5 h`, `4 h 59 min`, `3 d 5 h`. A horizon five
 * hours out reads `5 h` though the page reads it seconds after the route computed it, and a positive span under a
 * minute reads `1 min`, so `0 min` never stands beside a zero runway's `none`.
 */
export function minuteSpan(ms: number): string {
  const minutes = ms > 0 ? Math.max(1, Math.round(ms / MINUTE)) : 0;
  return minutes === 0 ? '0 min' : span(minutes * MINUTE);
}

/**
 * The share left at the window's horizon: `19% left at the reset` where the horizon is the reset (within a minute),
 * `56% left in 5 h` where it comes earlier, `59% left now` once it has come, the share `1 - projection` floored at
 * zero, and `19% left at reset` in the compact form the bars draw. A window the projection spends before its horizon
 * (`projection` at 1 or more) reads the instant it runs out instead, `spent in 2 h 10 min` in both forms, the span
 * `(horizon - now) - (projection - 1) / burn` hours through `minuteSpan`, and `0% left now` once that instant has
 * come. With `burn` null or zero it reads `0% left` in the horizon's form, and a utilization already at 1 or more reads
 * `0% left now`. Null without a projection.
 */
export function projectionWords(w: Pick<RateLimitWindow, 'burn' | 'horizon' | 'projection' | 'resetsAt' | 'utilization'>, now: number, compact = false): string | null {
  const parts = projectionParts(w, now, compact);
  return parts === null ? null : parts.join(' ');
}

/** `projectionWords` as its two parts, the share left (or `spent`) and the horizon, so a window bar too narrow for the
 *  words on one line breaks between the two and keeps each whole. */
export function projectionParts(w: Pick<RateLimitWindow, 'burn' | 'horizon' | 'projection' | 'resetsAt' | 'utilization'>, now: number, compact = false): [string, string] | null {
  if (w.projection === null || w.horizon === null) return null;
  if (w.utilization >= 1) return ['0% left', 'now'];
  const horizon = Date.parse(w.horizon);
  // The sessions spend the window before its horizon. The projection is the router's forecast, which carries the
  // later charges and the reservations the last sample does not, so the instant the share reaches 1 is read back
  // from the horizon along the burn rather than forward from the sampled utilization.
  if (w.projection >= 1 && w.burn !== null && w.burn > 0) {
    const span = horizon - now - ((w.projection - 1) / w.burn) * HOUR;
    return span > 0 ? ['spent', `in ${minuteSpan(span)}`] : ['0% left', 'now'];
  }
  const left = `${fmtPct(Math.max(0, 1 - w.projection))} left`;
  if (horizon <= now) return [left, 'now'];
  // The route reads the horizon off the router's latest reading and the reset off the ledger's newest sample, whole
  // seconds on one path and milliseconds on the other, so a horizon within a minute of the reset is the reset.
  const atReset = w.resetsAt !== null && Math.abs(Date.parse(w.resetsAt) - horizon) < MINUTE;
  return [left, atReset ? (compact ? 'at reset' : 'at the reset') : `in ${minuteSpan(horizon - now)}`];
}

/** The burn behind a projection, `the sessions on this account burn 7.3% an hour`. Null without a burn. */
export function burnWords(w: Pick<RateLimitWindow, 'burn'>): string | null {
  return w.burn === null ? null : `the sessions on this account burn ${fmtPct(w.burn, 1)} an hour`;
}

export const isSubscription = (account: Pick<Account, 'kind'>) => account.kind === 'oauth';
/** A paid source, as the engine reads one: an API-key account or a usage-based seat, neither with plan capacity. */
export const isPaidSource = (a: Pick<Account, 'kind' | 'usageBased'>): boolean => a.kind === 'api_key' || a.usageBased;
/**
 * Whether the Quota page lists the account. It lists a subscription account that is paired, not awaiting
 * re-authentication and whose plan has not ended. The flag alone decides the third: a saved end that has passed counts
 * for nothing on its own, since the router sets the flag once a profile read finds no plan.
 */
export const isQuotaListed = (account: Pick<Account, 'kind' | 'planEndedAt' | 'status'>): boolean =>
  isSubscription(account) && account.status !== 'reauth_required' && account.status !== 'unpaired' && account.planEndedAt === null;

/** The no-window sentence of an account whose plan ended: the long form, or the compact one the account cards and the
 *  Accounts list's rows draw in their window bars' place. */
export const planEndedSentence = (planEndedAt: string, now: number, compact = false): string =>
  compact ? `No window since the plan ended ${fmtRelative(planEndedAt, now)}` : `No quota window is read since the plan ended ${fmtRelative(planEndedAt, now)}.`;

/** The fast lane's two cooldown scopes, its rate limit's and its usage-credits refusal's: each holds fast-mode
 *  requests alone, so neither cools the account's standard traffic. */
export const isFastScope = (scope: string) => scope === 'fast' || scope === 'fast_credits';
/** The router's spelling of a cooldown scoped to one model, `model:<id>`. */
export const MODEL_SCOPE_PREFIX = 'model:';
/** The model id a `model:<id>` cooldown scope names, null for a scope of any other kind. */
export const scopeModel = (scope: string): string | null => (scope.startsWith(MODEL_SCOPE_PREFIX) ? scope.slice(MODEL_SCOPE_PREFIX.length) : null);
/** `Whole account` for the account scope, `Fast mode` and `Fast mode credits` for the fast lane's rate limit and
 *  usage-credits refusal, the model id for `model:<id>`. An identifier, the model id or a scope this build does not
 *  know shown as sent, draws in the mono face, and the worded scopes do not. */
export function scopeLabel(scope: string): { label: string; mono: boolean } {
  if (scope === 'account') return { label: 'Whole account', mono: false };
  if (scope === 'fast') return { label: 'Fast mode', mono: false };
  if (scope === 'fast_credits') return { label: 'Fast mode credits', mono: false };
  return { label: scopeModel(scope) ?? scope, mono: true };
}

/** The row that cools the account: the first on a scope beyond the fast lane's, null where none does. */
export const accountCooldown = (cooldowns: Cooldown[]): Cooldown | null => cooldowns.find((c) => !isFastScope(c.scope)) ?? null;

/** The statuses an account's flag sets. */
export const ACCOUNT_FLAGS = ['funding_required', 'reauth_required', 'unpaired'] as const;
export type AccountFlag = (typeof ACCOUNT_FLAGS)[number];
/** The flag an account's status carries, null for a status the enabled bit or the cooldowns decide. */
export const flagOf = (status: Account['status']): AccountFlag | null => {
  const flag = status as AccountFlag;
  return ACCOUNT_FLAGS.includes(flag) ? flag : null;
};

/** The one badge an account's flag, enabled bit and cooldowns fold to. A flag outranks the rest, so pausing never
 *  hides one. */
export const accountStatusOf = (flag: AccountFlag | null, enabled: boolean, cooldowns: Cooldown[]): Account['status'] =>
  flag ?? (!enabled ? 'paused' : accountCooldown(cooldowns) !== null ? 'cooldown' : 'ready');

/** The soonest future reset over every window the poll read and a seat's spend window, with every window at that
 *  instant, so the card marks each. Null where no reset lies ahead. */
export function soonestResetAt(account: Account, now: number): { at: number; ids: WindowId[] } | null {
  let at = Number.POSITIVE_INFINITY;
  let ids: WindowId[] = [];
  const take = (id: WindowId, resetsAt: string | null | undefined) => {
    // A window the sampler reported without a reset time can never be the soonest to recover.
    const t = resetsAt ? Date.parse(resetsAt) : Number.NaN;
    if (!(t > now) || t > at) return;
    ids = t === at ? [...ids, id] : [id];
    at = t;
  };
  for (const w of readWindows(account)) take(w.id, w.resetsAt);
  // A usage-based seat recovers at its spend window's reset, the one reset it carries.
  take('spend', account.spend?.resetsAt);
  return ids.length === 0 ? null : { at, ids };
}

/** One window's future reset, or null where the window is absent, expired or carries none. `'7d_fable'` reads
 *  `fableWindow`. */
const resetOf = (account: Account, windowId: KnownWindowId, now: number): number | null => {
  const resetsAt = windowOf(account, windowId)?.resetsAt;
  const at = resetsAt ? Date.parse(resetsAt) : Number.NaN;
  return at > now ? at : null;
};

/** One window's lead, or null where the window is absent, expired or carries none. `'7d_fable'` reads
 *  `fableWindow`. */
const leadOf = (account: Account, windowId: KnownWindowId, now: number): number | null => {
  const w = windowOf(account, windowId);
  return w === null || isExpired(w, now) ? null : w.lead;
};

/** The accounts by one window's reset, the soonest first under `asc`, an account without one and a usage-based seat
 *  after every dated one under either direction, ties by label. */
export const sortByReset = (accounts: Account[], windowId: KnownWindowId, now: number, dir: SortDir = 'asc'): Account[] => orderRows(accounts, (a) => resetOf(a, windowId, now), dir, (a) => a.label);

/** The accounts by one window's lead, the largest first under `desc`, an account whose window is absent, expired or
 *  without a lead after every one with a lead under either direction, ties by label. */
export const sortByPace = (accounts: Account[], windowId: KnownWindowId, now: number, dir: SortDir = 'desc'): Account[] => orderRows(accounts, (a) => leadOf(a, windowId, now), dir, (a) => a.label);

/**
 * A spend window's reset on hover: the instant, and where the router estimated it, why the line marks it, since no
 * head has named the window's reset yet.
 */
export const spendResetTitle = (spend: Pick<Spend, 'resetSource' | 'resetsAt'>): string =>
  `resets ${fmtAbsolute(spend.resetsAt)}${spend.resetSource === 'estimated' ? ' (estimated: no reset header has been seen for this window yet)' : ''}`;

/**
 * Whether a seat's spend holds a reading: the router answers the `spend` object on every seat, with every member null
 * before the first reading, so an object with neither amounts nor a share reads as no reading.
 */
export const hasSpendReading = (spend: Spend | null): spend is Spend => spend !== null && (spend.amounts !== null || spend.utilization !== null);

/**
 * Whether the Quota windows timeline draws a lane for the account: a plan window, or a spend reading. The page's
 * filter and the lane builder read this one rule, since the builder never sees an account the filter leaves out.
 */
export const onTimeline = (account: Pick<Account, 'spend' | 'windows'>): boolean => account.windows.length > 0 || hasSpendReading(account.spend);

/**
 * The timeline's lanes under one zoom, in the order the accounts come: a plan account's by the zoom's window, and a
 * usage-based seat's by its spend window, which stands in for every plan window it lacks.
 */
export function timelineLanes(accounts: readonly Account[], id: '5h' | '7d'): { account: Account; spend: Spend | null; window: RateLimitWindow | null }[] {
  return accounts
    .map((account) => (account.usageBased ? { account, spend: account.spend, window: null } : { account, spend: null, window: windowOf(account, id) }))
    .filter((l) => l.window !== null || l.spend !== null);
}

export const STATUS_LABEL: Record<Account['status'], string> = {
  cooldown: 'Cooling down',
  funding_required: 'Needs funding',
  paused: 'Paused',
  reauth_required: 'Needs re-auth',
  ready: 'Ready',
  unpaired: 'Not paired',
};
/** The statuses the router reports, in the labels' order, which the Accounts address and its status filter read. */
export const ACCOUNT_STATUSES = Object.keys(STATUS_LABEL) as Account['status'][];

/** How the usage poll last answered, in the words the Quota page and the routing log share. */
export const POLL_OUTCOME_LABEL: Record<PollOutcome, string> = { clean: 'clean', failed: 'failed', rate_limited: 'rate limited' };

export type BadgeTone = 'accent' | 'critical' | 'good' | 'info' | 'neutral' | 'serious' | 'warning';

export const STATUS_TONE: Record<Account['status'], BadgeTone> = {
  cooldown: 'warning',
  funding_required: 'critical',
  paused: 'neutral',
  reauth_required: 'critical',
  ready: 'good',
  unpaired: 'neutral',
};

/** The Extra usage line's state: the serious `in_use`, or one of three quiet states drawn in the muted ink. */
export type ExtraUsageState = 'disabled' | 'enabled' | 'in_use' | 'unreported';

export function extraUsageState(extra: ExtraUsage | null): ExtraUsageState {
  if (extra === null) return 'unreported';
  if (!extra.enabled) return 'disabled';
  return extra.inUse ? 'in_use' : 'enabled';
}

/** The footer's essential reading: one or two lines that never wrap, each in the muted or the serious tone, a
 *  failed poll's reason cut inside its line, and a title carrying every fact the lines leave (the attempt's
 *  instant, when the snapshot was read, since when the account backs off, the run, the next attempt). */
export interface PollReading {
  lines: { reason?: string; text: string; tone: 'muted' | 'serious' }[];
  title: string;
}

export function pollReading(poll: QuotaPoll | null, fetchedAt: string | null, now: number): PollReading {
  // After a start the poll has not asked yet while the snapshot still holds the last run's windows, so their age
  // stays in the title.
  if (poll === null) return { lines: [{ text: 'not yet polled', tone: 'muted' }], title: fetchedAt ? `checked ${fmtAbsolute(fetchedAt)}` : '' };
  const polled = `polled ${fmtRelative(poll.lastAttemptAt, now)}: `;
  const reason = poll.lastOutcome === 'failed' && poll.lastError ? poll.lastError : null;
  const lines: PollReading['lines'] = [reason === null ? { text: `${polled}${POLL_OUTCOME_LABEL[poll.lastOutcome]}`, tone: 'muted' } : { reason, text: `${polled}failed: `, tone: 'muted' }];
  const title = [...(poll.lastAttemptAt ? [fmtAbsolute(poll.lastAttemptAt)] : []), ...(reason === null ? [] : [reason]), ...(fetchedAt ? [`checked ${fmtAbsolute(fetchedAt)}`] : [])];
  if (poll.backoffSince !== null) {
    const next = poll.nextAttemptAt;
    lines.push({ text: next ? `backing off, next attempt ${fmtRelative(next, now)}` : 'backing off', tone: 'serious' });
    const n = poll.rateLimitedInARow;
    title.push(`backing off since ${fmtAbsolute(poll.backoffSince)}`, `${plural(n, 'rate-limited answer')} in a row`, ...(next ? [`next attempt ${fmtAbsolute(next)}`] : []));
  }
  return { lines, title: title.join('\n') };
}
