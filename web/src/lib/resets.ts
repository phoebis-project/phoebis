import { ApiError } from '@/api/http';
import type { LimitResets, RateLimitWindow, ResetGrant, ResetUse, WindowId } from '@/api/types';
import { lookup } from './counts';
import { fmtAbsolute, fmtDate, fmtEndsIn, fmtPct, instant, listWords, messageOf, upperFirst } from './format';
import { isExpired, remainingOf, shareLeft } from './quota';
import { DAY } from './time';
import { windowTag } from './windows';

const BLOCKED_WORDS: Readonly<Record<string, string>> = {
  blocked_five_hour: 'blocked by the 5-hour limit',
  blocked_opus: 'blocked by the Opus limit',
  blocked_other: 'blocked by another limit',
  blocked_seven_day: 'blocked by the 7-day limit',
  blocked_sonnet: 'blocked by the Sonnet limit',
  cannot_clear: 'nothing it can clear now',
  needs_limit: 'usable once at a limit it clears',
};

const cooldownWords = (until: string | null, now: number) => (until === null ? 'cooldown' : `cooldown until ${instant(Date.parse(until), now)}`);

/** Why a grant with a reset left cannot be used now, lower case as the chip draws it. */
function resetReason(grant: Pick<ResetGrant, 'reason' | 'state'>, cooldownUntil: string | null, now: number): string {
  switch (grant.state) {
    case 'cooldown':
      return `on a ${cooldownWords(cooldownUntil, now)}`;
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
  if (resets === null || resets.left === 0) return null;
  const open = resets.grants.filter(holdsReset);
  const first = open[0];
  if (first === undefined) return null;
  const usable = resets.grants.find((g) => g.state === 'usable') ?? null;
  const earliest = open.reduce((a, b) => (Date.parse(b.endsAt) < Date.parse(a.endsAt) ? b : a));
  // The edge itself warns, as the Dashboard's item holds from that instant on.
  const near = warningDays !== null && warningDays > 0 && Date.parse(earliest.endsAt) - now <= warningDays * DAY;
  const state = usable === null ? resetReason(first, resets.cooldownUntil, now) : 'available';
  const ends = near && usable !== null ? `${resets.left > 1 ? 'first ' : ''}${fmtEndsIn(earliest.endsAt, now)}` : null;
  const short = ends ?? (usable === null && first.state === 'cooldown' ? cooldownWords(resets.cooldownUntil, now) : state);
  const labels = listWords(open.map((g) => g.label));
  const whole = `Resets ${resets.left} ${state}${ends === null ? '' : ` ${ends}`}`;
  return { count: resets.left, ends, short, state, title: `${whole}: ${labels}${near ? `, ends ${fmtAbsolute(earliest.endsAt)}` : ''}`, tone: near ? 'warning' : 'plain', usable };
}

/** The windows a reset usable now clears, which the card marks on their chips. */
export const resetMarks = (resets: LimitResets | null): WindowId[] => [...new Set((resets?.grants ?? []).filter((g) => g.state === 'usable').flatMap((g) => g.clears))];

/** Whether a window's chip prints "100% left" now: live, and full by the printed figure. The printed figure decides, so
 *  a window the chip prints as full carries no mark, and a reset that clears only such windows draws the dialog's
 *  warning that it has nothing to clear. */
export const readsFull = (window: RateLimitWindow | null, now: number): boolean => window !== null && !isExpired(window, now) && fmtPct(remainingOf(window)) === '100%';

/** The first page's rows: the usable grants first, each part in the block's order. */
export function resetRows(resets: LimitResets): ResetGrant[] {
  return [...resets.grants.filter((g) => g.state === 'usable'), ...resets.grants.filter((g) => g.state !== 'usable')];
}

/** What stands beside a row that cannot be used: the reason's words with a capital, "Used at 13:00", "Used" or
 *  "Ended Mar 09, 2027". Null for a usable row. */
export function resetRowNote(grant: ResetGrant, cooldownUntil: string | null, now: number): string | null {
  switch (grant.state) {
    case 'usable':
      return null;
    case 'used':
      return grant.usedAt === null ? 'Used' : `Used at ${instant(Date.parse(grant.usedAt), now)}`;
    case 'ended':
      return `Ended ${fmtDate(grant.endsAt)}`;
    default:
      return upperFirst(resetReason(grant, cooldownUntil, now));
  }
}

/** The confirmation's line on the 7-day window. `figure` is the share left after the use as `fmtPct` prints it,
 *  null where the row carries no share. Null where the line leaves the page. */
export function resetEstimate(grant: Pick<ResetGrant, 'clears' | 'kind'>, fableWeekShare: number | null, week: RateLimitWindow | null, fable: RateLimitWindow | null, now: number): { figure: string | null } | null {
  if (grant.kind !== 'fable' || !grant.clears.includes('7d_fable') || grant.clears.includes('7d')) return null;
  const live = (w: RateLimitWindow | null) => (w !== null && !isExpired(w, now) ? w : null);
  const sevenDay = live(week);
  // An unused Fable week holds no share of the 7-day window.
  if (sevenDay === null || (live(fable)?.utilization ?? 0) <= 0) return null;
  if (fableWeekShare === null) return { figure: null };
  const figure = fmtPct(shareLeft(sevenDay.utilization - fableWeekShare));
  // The printed figures decide: a share that rounds away would print the reading as its own estimate.
  return figure === fmtPct(remainingOf(sevenDay)) ? null : { figure };
}

/** How the dialog files a use's answer: `used` closes it, and the other two keep it open under `sentence`. */
export interface ResetAnswer {
  kind: 'refused' | 'unconfirmed' | 'used';
  /** The callout's sentence, empty for `used`. */
  sentence: string;
}

/** Files what `api.useLimitReset` answered (`use`) or threw (`thrown`), by the router's outcome and its codes.
 *  `account` is the account's label and `grant` the claimed grant as the dialog confirmed it. */
export function resetAnswer(result: { use: ResetUse } | { thrown: unknown }, account: string, grant: Pick<ResetGrant, 'id' | 'resetsTotal'>, now: number): ResetAnswer {
  const refused = (sentence: string): ResetAnswer => ({ kind: 'refused', sentence: `Refused: ${sentence}` });
  const open = (sentence: string): ResetAnswer => ({ kind: 'unconfirmed', sentence: `Not confirmed: ${sentence}` });
  if ('thrown' in result) {
    const e = result.thrown;
    const message = messageOf(e);
    const code = e instanceof ApiError ? e.code : null;
    if (code === 'limit_reset_not_sent') return refused(`the claim did not reach the upstream, so the reset was not used (${message}).`);
    if (code === 'limit_reset_refused') return refused(`the upstream turned the claim away, so the reset was not used (${message}).`);
    if (code === 'limit_reset_unanswered') return open(`the upstream gave no usable answer (${message}), so the reset may have been used.`);
    if (e instanceof ApiError && e.status >= 400 && e.status < 500) return refused(`${message}, so the reset was not used.`);
    // A call that got no answer, and a 5xx without a code, may follow a claim that left.
    return open(`the router did not answer (${message}), so the reset may have been used.`);
  }
  const { use } = result;
  if (use.outcome === 'used') return { kind: 'used', sentence: '' };
  const reason = use.reason === null ? '' : ` (${use.reason})`;
  if (use.outcome === 'unsettled') {
    if (use.result === 'unavailable') return open(`the upstream answered that this reset is not available now${reason}.`);
    if (use.result === 'cooldown') return open('the upstream answered a cooldown to a repeated claim, so the first attempt may have used the reset.');
    return open(`the upstream answered ${use.result}, which this console does not know.`);
  }
  const left = use.limitResets?.grants.find((g) => g.id === grant.id)?.resetsLeft ?? use.resetsLeft ?? 0;
  const until = use.cooldownUntil === null ? '' : ` until ${instant(Date.parse(use.cooldownUntil), now)}`;
  switch (use.result) {
    case 'already_used':
      return refused(`this reset was already used, and ${account} holds ${left} of ${grant.resetsTotal} now.`);
    case 'cooldown':
      return refused(`${account} is on a cooldown${until}, so the reset was not used.`);
    case 'ineligible':
      return refused(`${account} cannot take this reset${reason}, so it was not used.`);
    case 'not_limited':
      return refused(`${account} stands at no limit this reset clears, so it was not used.`);
    default:
      return refused(`the upstream answered ${use.result}, so the reset was not used.`);
  }
}

/** The line after a "Not confirmed" sentence: the claimed grant's count in the member read after the claim, or
 *  what will show it where none is at hand. */
export const resetHolds = (account: string, after: Pick<ResetGrant, 'resetsLeft' | 'resetsTotal'> | null): string =>
  after === null ? `The next read of ${account} shows whether it was used.` : `${account} holds ${after.resetsLeft} of ${after.resetsTotal} now.`;

// One window with use left is something to clear, so the warning waits for every one of them to read full.
/** Whether a use would clear nothing yet: the grant names a window, and every window it clears reads 100% left now.
 *  A grant that names none answers false, since no window shows that it has nothing to clear. `cleared` holds one
 *  entry per window of the grant's `clears`, null where the account reads none. */
export const resetClearsNothing = (cleared: readonly (RateLimitWindow | null)[], now: number): boolean => cleared.length > 0 && cleared.every((window) => readsFull(window, now));

/** The windows a grant clears as joined tags ("5h and 7d"), null for a grant that names none. */
export const clearsWords = (clears: readonly string[]): string | null => (clears.length === 0 ? null : listWords(clears.map((w) => windowTag(w))));
