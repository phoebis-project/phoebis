import { planLabel } from '@/api/decode';
import type { Account, ExtraUsage, PollOutcome, QuotaPoll, RateLimitWindow, WindowId } from '@/api/types';
import { fmtAbsolute, fmtRelative } from '@/lib/format';

export type MeterTone = 'good' | 'low' | 'warn';

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
  if (poll === null) return { lines: [{ text: 'not yet polled', tone: 'muted' }], title: '' };
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
