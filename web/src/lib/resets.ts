import type { LimitResets, RateLimitWindow, ResetGrant, WindowId } from '@/api/types';
import { lookup } from './counts';
import { fmtAbsolute, fmtEndsIn, fmtPct, instant, listWords } from './format';
import { isExpired, remainingOf } from './quota';
import { DAY } from './time';

const BLOCKED_WORDS: Readonly<Record<string, string>> = {
  blocked_five_hour: 'blocked by the 5-hour limit',
  blocked_opus: 'blocked by the Opus limit',
  blocked_other: 'blocked by another limit',
  blocked_seven_day: 'blocked by the 7-day limit',
  blocked_sonnet: 'blocked by the Sonnet limit',
  cannot_clear: 'nothing it can clear now',
  needs_limit: 'usable once at a limit it clears',
};

/** Why a grant with a reset left cannot be used now, lower case as the chip draws it. */
export function resetReason(grant: Pick<ResetGrant, 'reason' | 'state'>, cooldownUntil: string | null, now: number): string {
  switch (grant.state) {
    case 'cooldown':
      return cooldownUntil === null ? 'on a cooldown' : `on a cooldown until ${instant(Date.parse(cooldownUntil), now)}`;
    case 'paused':
      return 'paused';
    case 'waiting':
      return 'waits for the other reset';
    case 'blocked':
      return lookup(BLOCKED_WORDS, grant.reason ?? '') ?? 'not usable now';
    default:
      return 'not usable now';
  }
}

/** The card's Resets chip. */
export interface ResetChip {
  /** The member's `left`. */
  count: number;
  /** "ends in 4 days", or "first ends in 4 days" for several, beside a usable reset inside the warning's days. */
  ends: string | null;
  /** The words after the count under a narrow row. */
  short: string;
  /** "available", or the reason of the first grant with a reset left where none is usable now. */
  state: string;
  /** The hover: the whole words, the labels of the resets left and, where the end is near, its instant. */
  title: string;
  tone: 'plain' | 'warning';
  /** The first usable grant in the block's order, null where none is usable now. */
  usable: ResetGrant | null;
}

const holdsReset = (g: ResetGrant) => g.state !== 'ended' && g.state !== 'used';

/** The chip of an account's limit resets at `now`, null where it has no reset left. `warningDays` is null until the
 *  settings are read, which draws the plain tone. */
export function resetChip(resets: LimitResets | null, now: number, warningDays: number | null): ResetChip | null {
  const open = resets === null || resets.left === 0 ? [] : resets.grants.filter(holdsReset);
  const first = open[0];
  if (resets === null || first === undefined) return null;
  const usable = resets.grants.find((g) => g.state === 'usable') ?? null;
  const earliest = open.reduce((a, b) => (Date.parse(b.endsAt) < Date.parse(a.endsAt) ? b : a));
  // The edge itself warns, as the Dashboard's item holds from that instant on.
  const near = warningDays !== null && warningDays > 0 && Date.parse(earliest.endsAt) - now <= warningDays * DAY;
  const state = usable === null ? resetReason(first, resets.cooldownUntil, now) : 'available';
  const ends = near && usable !== null ? `${resets.left > 1 ? 'first ' : ''}${fmtEndsIn(earliest.endsAt, now)}` : null;
  const short = ends ?? (usable === null && first.state === 'cooldown' ? state.slice('on a '.length) : state);
  const labels = listWords(open.map((g) => g.label));
  const whole = `Resets ${resets.left} ${state}${ends === null ? '' : ` ${ends}`}`;
  return { count: resets.left, ends, short, state, title: `${whole}: ${labels}${near ? `, ends ${fmtAbsolute(earliest.endsAt)}` : ''}`, tone: near ? 'warning' : 'plain', usable };
}

/** The windows a reset usable now clears, which the card marks on their chips. */
export const resetMarks = (resets: LimitResets | null): WindowId[] => [...new Set((resets?.grants ?? []).filter((g) => g.state === 'usable').flatMap((g) => g.clears))];

// The printed figure decides, so a window the chip prints as full carries no mark and no line that promises a change.
/** Whether a window's chip prints "100% left" now: live, and full by the printed figure. */
export const readsFull = (window: RateLimitWindow | null, now: number): boolean => window !== null && !isExpired(window, now) && fmtPct(remainingOf(window)) === '100%';
