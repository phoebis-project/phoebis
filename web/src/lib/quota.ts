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

export function soonestReset(account: Account, now: number): WindowId | null {
  let best: RateLimitWindow | null = null;
  for (const w of account.windows) {
    if (Date.parse(w.resetsAt) <= now) continue;
    if (!best || Date.parse(w.resetsAt) < Date.parse(best.resetsAt)) best = w;
  }
  return best ? best.id : null;
}

export const STATUS_LABEL: Record<Account['status'], string> = {
  cooldown: 'Cooling down',
  disabled: 'Disabled',
  paused: 'Paused',
  reauth_required: 'Needs re-auth',
  ready: 'Ready',
};

export type BadgeTone = 'accent' | 'critical' | 'good' | 'neutral' | 'serious' | 'warning';

export const STATUS_TONE: Record<Account['status'], BadgeTone> = {
  cooldown: 'warning',
  disabled: 'neutral',
  paused: 'neutral',
  reauth_required: 'critical',
  ready: 'good',
};
