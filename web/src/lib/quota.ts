import type { Account, RateLimitWindow, WindowId } from '@/api/types';

export type MeterTone = 'good' | 'low' | 'warn';

/** Remaining share drives the color: green at 50% and above, amber at 20% and above, red below. */
export function meterTone(remaining: number): MeterTone {
  if (remaining >= 0.5) return 'good';
  if (remaining >= 0.2) return 'warn';
  return 'low';
}

export const remainingOf = (w: RateLimitWindow) => Math.max(0, Math.min(1, 1 - w.utilization));

export const windowOf = (account: Account, id: WindowId) => account.windows.find((w) => w.id === id) ?? null;

export const isSubscription = (account: Account) => account.kind === 'oauth';

/** The fast lane's two cooldown scopes, its rate limit's and its usage-credits refusal's: each holds fast-mode
 *  requests alone, so neither cools the upstream's standard traffic. */
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

export type BadgeTone = 'accent' | 'critical' | 'good' | 'neutral' | 'serious' | 'warning';

export const STATUS_TONE: Record<Account['status'], BadgeTone> = {
  cooldown: 'warning',
  funding_required: 'critical',
  paused: 'neutral',
  reauth_required: 'critical',
  ready: 'good',
  unpaired: 'neutral',
};
