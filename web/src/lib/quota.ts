import type { Account, AccountSlowDown, ExtraUsage, PollOutcome, QuotaPoll, RateLimitWindow, WindowId } from '@/api/types';
import { fmtAbsolute, fmtPct, fmtRelative, instant } from '@/lib/format';

export type MeterTone = 'good' | 'low' | 'warn';

/** `default_claude_max_5x` reads as "Max 5x" and `default_claude_ai`, the tier the profile reports for Pro, as "Pro";
 *  anything else is shown as sent. */
export function planLabel(tier: string): string {
  const m = /^(?:default_)?claude_(ai|max|pro|team|enterprise)(?:_(\d+)x)?$/.exec(tier);
  if (!m) return tier;
  const name = m[1] === 'ai' ? 'Pro' : m[1][0].toUpperCase() + m[1].slice(1);
  return m[2] ? `${name} ${m[2]}x` : name;
}

/** Remaining share drives the color: green at 50% and above, amber at 20% and above, red below. */
export function meterTone(remaining: number): MeterTone {
  if (remaining >= 0.5) return 'good';
  if (remaining >= 0.2) return 'warn';
  return 'low';
}

export const remainingOf = (w: RateLimitWindow) => Math.max(0, Math.min(1, 1 - w.utilization));

export const windowOf = (account: Account, id: WindowId) => account.windows.find((w) => w.id === id) ?? null;

/** A window whose reset has passed with no reading since: the card and the timeline agree on this one test. */
export const isExpired = (w: RateLimitWindow, now: number) => w.resetsAt !== null && Date.parse(w.resetsAt) < now;

/**
 * The slow-down a page draws the account's speed chip from: its entry while the factor is under 1 and the 5-hour window
 * the entry was read from has not reset since, null otherwise.
 */
export function slowedOf(account: Account, now: number): AccountSlowDown | null {
  const slowDown = account.slowDown ?? null;
  const w5 = windowOf(account, '5h');
  return slowDown !== null && slowDown.factor < 1 && w5 !== null && !isExpired(w5, now) ? slowDown : null;
}

const HOUR = 3_600_000;
const MINUTE = 60_000;

/** The end the router's pace reads (`term::end_of`): the saved end while the account is cancelled, else a trial end still ahead. */
export function endOf(account: Pick<Account, 'cancelled' | 'subscription'>, now: number): number | null {
  const term = account.subscription;
  if (!term) return null;
  if (account.cancelled) return term.endsAt === null ? null : Date.parse(term.endsAt);
  const trial = term.trialEndsAt === null ? Number.NaN : Date.parse(term.trialEndsAt);
  return trial > now ? trial : null;
}

/** The account's `end` where a window's pace runs to it, an end before the reset (one at the reset is the reset); else null. */
export const paceEnd = (resetsAt: string | null, end: number | null): number | null => (resetsAt !== null && end !== null && end < Date.parse(resetsAt) ? end : null);

/** " to its end at 15:42" for the end a pace runs to (`paceEnd`), as the decision card's Pace hover names it; empty for none. */
export const toEndWords = (end: number | null, now: number): string => (end === null ? '' : ` to its end at ${instant(end, now)}`);

/** The share of a window elapsed from its start to the earlier of its `reset` and the account's `end`, clamped to 0 to 1. */
function elapsedShare(reset: number, lengthMs: number, now: number, end: number | null): number {
  const start = reset - lengthMs;
  const span = Math.min(reset, end ?? reset) - start;
  // Past its reset a window reads fully elapsed, where the router reads its own use since: an accepted approximation.
  const elapsed = span <= 0 ? 1 : (now - start) / span;
  return Math.min(1, Math.max(0, elapsed));
}

/**
 * The router's reading: a window's utilization less the share of it that has elapsed, from its start to the earlier
 * of its reset and the account's `end` (`endOf`), clamped to 0 to 1; 0 elapsed without a reset.
 */
export function aheadOf(window: { resetsAt: string | null; utilization: number }, lengthMs: number, now: number, end: number | null): number {
  if (window.resetsAt === null) return window.utilization;
  return window.utilization - elapsedShare(Date.parse(window.resetsAt), lengthMs, now, end);
}

/**
 * The share of a window's period still to come toward the earlier of its reset and the account's `end`, 0 to 1, so
 * the pace mark and `paceWords` read one deadline. Null without a reset time.
 */
export function remainingTimeShare(w: RateLimitWindow, now: number, end: number | null): number | null {
  if (w.resetsAt === null) return null;
  return 1 - elapsedShare(Date.parse(w.resetsAt), w.periodHours * HOUR, now, end);
}

/** The lead over pace in words, as `aheadOf` reads it: "12% ahead of pace", "5% behind pace", or "on pace" within
 *  one point (a lead under 0.01), with " to its end at 15:42" where the pace runs to the account's `end`. Null
 *  without a reset time. */
export function paceWords(w: RateLimitWindow, now: number, end: number | null): string | null {
  if (w.resetsAt === null) return null;
  const lead = aheadOf(w, w.periodHours * HOUR, now, end);
  const words = Math.abs(lead) < 0.01 ? 'on pace' : `${Math.round(Math.abs(lead) * 100)}% ${lead > 0 ? 'ahead of' : 'behind'} pace`;
  return `${words}${toEndWords(paceEnd(w.resetsAt, end), now)}`;
}

/**
 * A span rounded to the minute in the routing log's spelling: `27 min`, `5 h`, `4 h 59 min`, `3 d 5 h`. A horizon five
 * hours out reads `5 h` though the page reads it seconds after the route computed it, and a positive span under a
 * minute reads `1 min`, so `0 min` never stands beside a zero runway's `none`.
 */
export function minuteSpan(ms: number): string {
  const minutes = ms > 0 ? Math.max(1, Math.round(ms / MINUTE)) : 0;
  const pair = (major: number, majorUnit: string, minor: number, minorUnit: string) => (minor > 0 ? `${major} ${majorUnit} ${minor} ${minorUnit}` : `${major} ${majorUnit}`);
  if (minutes < 60) return `${minutes} min`;
  if (minutes < 24 * 60) return pair(Math.floor(minutes / 60), 'h', minutes % 60, 'min');
  return pair(Math.floor(minutes / (24 * 60)), 'd', Math.floor((minutes % (24 * 60)) / 60), 'h');
}

/**
 * The projection at the window's horizon, `104% at the reset` where the horizon is the reset and `44% in 5 h` where it
 * falls earlier (five hours out on a weekly window, or the account's end), with `at reset` in the compact form the
 * window bars draw, and `41% now` once the horizon has come. Null without a projection.
 */
export function projectionWords(w: Pick<RateLimitWindow, 'horizon' | 'projection' | 'resetsAt'>, now: number, compact = false): string | null {
  if (w.projection === null || w.horizon === null) return null;
  const horizon = Date.parse(w.horizon);
  if (horizon <= now) return `${fmtPct(w.projection)} now`;
  // The route reads the horizon off the router's latest reading and the reset off the ledger's newest sample, whole
  // seconds on one path and milliseconds on the other, so a horizon within a minute of the reset is the reset.
  const atReset = w.resetsAt !== null && Math.abs(Date.parse(w.resetsAt) - horizon) < MINUTE;
  return `${fmtPct(w.projection)} ${atReset ? (compact ? 'at reset' : 'at the reset') : `in ${minuteSpan(horizon - now)}`}`;
}

/** The burn behind a projection, `the sessions on this account burn 7.3% an hour`. Null without a burn. */
export function burnWords(w: Pick<RateLimitWindow, 'burn'>): string | null {
  return w.burn === null ? null : `the sessions on this account burn ${fmtPct(w.burn, 1)} an hour`;
}

export const isSubscription = (account: Account) => account.kind === 'oauth';

/** The fast lane's two cooldown scopes, its rate limit's and its usage-credits refusal's: each holds fast-mode
 *  requests alone, so neither cools the account's standard traffic. */
export const isFastScope = (scope: string) => scope === 'fast' || scope === 'fast_credits';

export function soonestReset(account: Account, now: number): WindowId | null {
  let best: RateLimitWindow | null = null;
  let bestAt = Number.POSITIVE_INFINITY;
  for (const w of account.windows) {
    // A window the sampler reported without a reset time can never be the soonest to recover.
    const at = w.resetsAt === null ? Number.NaN : Date.parse(w.resetsAt);
    if (!(at > now) || at >= bestAt) continue;
    best = w;
    bestAt = at;
  }
  return best ? best.id : null;
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

export type BadgeTone = 'accent' | 'critical' | 'good' | 'neutral' | 'serious' | 'warning';

export const STATUS_TONE: Record<Account['status'], BadgeTone> = {
  cooldown: 'warning',
  funding_required: 'critical',
  paused: 'neutral',
  reauth_required: 'critical',
  ready: 'good',
  unpaired: 'neutral',
};

/** The Plan chip's word: the tier's name with its multiple where `planLabel` parses the tier, else the plan's own
 *  name from the profile's flag, so an unparsed tier still reads "Max" or "Pro". Null with no plan at all. */
export function planChip(account: Pick<Account, 'plan' | 'subscription'>): string | null {
  const tier = account.subscription?.rateLimitTier ?? '';
  // `planLabel` answers a tier its pattern does not match unchanged, so a changed answer is the parsed one.
  const parsed = tier === '' ? tier : planLabel(tier);
  return parsed !== tier ? parsed : account.plan;
}

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
    title.push(`backing off since ${fmtAbsolute(poll.backoffSince)}`, `${n} rate-limited ${n === 1 ? 'answer' : 'answers'} in a row`, ...(next ? [`next attempt ${fmtAbsolute(next)}`] : []));
  }
  return { lines, title: title.join('\n') };
}
