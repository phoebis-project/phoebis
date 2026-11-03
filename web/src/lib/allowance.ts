import type { AllowanceAccount, AllowanceReset, AllowanceWindow, SubscriptionCosts, UnusedAllowanceAnswer } from '@/api/types';
import { HINT_WINDOW } from '@/lib/analytics';
import { anchorTier, pricedWindow, tierCapacity, weekScale, weekUnpricedTitle } from '@/lib/costs';
import { lookup } from '@/lib/counts';
import { byteOrder, fmtAbsolute, fmtShare, fmtShortTime, plural, weekShort } from '@/lib/format';
import { pricedTier } from '@/lib/quota';
import { DAY } from '@/lib/time';
import type { WeekPosition } from '@/lib/unit';
import { WINDOW_LABEL, windowKeyId, windowRank } from '@/lib/windows';

/** The weeks the Accounts block and the Analytics card read: the latest reset and the three before it. */
export const ALLOWANCE_WEEKS = 4;


/** The span the block and the card read: 29 days behind now to now, 28 days and a day of slack so the fourth weekly
 *  reset is never cut by the hour of the week or by a clock-walk reading up to a poll interval old. */
export function allowanceSpan(now: number): { from: string; to: string } {
  return { from: new Date(now - (7 * ALLOWANCE_WEEKS + 1) * DAY).toISOString(), to: new Date(now).toISOString() };
}

/** The newest `weeks` resets of a window with their total and count: the answer's figures where it lists that many or
 *  fewer, the console's own sum and count over the newest `weeks` otherwise. */
export function newestResets(window: AllowanceWindow, weeks = ALLOWANCE_WEEKS): { counted: number; resets: AllowanceReset[]; total: number } {
  if (window.resets.length <= weeks) return { counted: window.resetsCounted, resets: window.resets, total: window.totalShareLeft };
  const resets = window.resets.slice(0, weeks);
  return { counted: resets.length, resets, total: resets.reduce((sum, r) => sum + r.shareLeft, 0) };
}

/** A figure in the chosen week: the share of the account's own window priced through the tier's 7-day capacity over
 *  the chosen week's tier's (a model week at its capacity factor), `share` the figure in that week, and a dash where
 *  the router prices no capacity for the plan or the cost model sizes none for the tier on that window. */
export type AllowanceFigure = { kind: 'equivalent'; share: number; text: string } | { kind: 'unpriced'; text: '-'; title: string } | { kind: 'none'; text: '-'; title: string };

/** The dash's hover where the plan prices no capacity. Named apart from `lib/analytics.ts`'s `UNPRICED_TITLE`, whose
 *  words differ, so the test bundle's star exports keep both. */
export const ALLOWANCE_UNPRICED_TITLE = "Unpriced: the cost model holds no capacity for this account's plan";
export const NO_READING_TITLE = 'no reading at this reset';
export const FREE_NOTE = 'Free has no priced capacity in the cost model, so a reset the router recorded no tier for and the total read a dash, and a week with its own tier prices at it.';

/**
 * What one share of the account's own week weighs in Pro weeks: the tier's 7-day capacity times the window's capacity
 * factor (1 for the 7-day window, the model week's own for a model week) over the anchor's 7-day capacity, so a
 * Fable-week reset reads half its 7-day figure. Null with no cost model, for a plan the router prices no tier for, a
 * null tier, a window that is no week of the 7-day window, and where the cost model sizes either side at nothing.
 */
function capacityWeight(account: Pick<AllowanceAccount, 'plan' | 'tier'>, window: string, costs: SubscriptionCosts | null): number | null {
  if (costs === null) return null;
  const gate = pricedTier(account.plan, account.tier, costs);
  if (gate.tier === null) return null;
  const priced = pricedWindow(costs, window);
  if (priced === null || priced.priced !== 'seven_day') return null;
  const capacity = tierCapacity(costs, gate.tier, 'seven_day');
  const anchor = tierCapacity(costs, anchorTier(costs), 'seven_day');
  return capacity === null || anchor === null || anchor === 0 ? null : (capacity * priced.divisor) / anchor;
}

/**
 * The pair a reset prices at: the plan and the tier of the reading it replaced where the router recorded them, the
 * account's current pair where it did not. A missing key reads as null, since the wire writes both or neither.
 */
export const pairOf = (account: Pick<AllowanceAccount, 'plan' | 'tier'>, reset?: Pick<AllowanceReset, 'plan' | 'tier'>): Pick<AllowanceAccount, 'plan' | 'tier'> =>
  reset !== undefined && (reset.plan ?? null) !== null && (reset.tier ?? null) !== null ? reset : account;

/** With no cost model a figure reads the unpriced dash, an arm the pages never draw, since they hold the skeleton
 *  while the model loads. */
export function allowanceFigure(share: number | null, window: string, account: Pick<AllowanceAccount, 'plan' | 'tier'>, costs: SubscriptionCosts | null, week: WeekPosition, reset?: Pick<AllowanceReset, 'plan' | 'tier'>): AllowanceFigure {
  if (share === null) return { kind: 'none', text: '-', title: NO_READING_TITLE };
  const scale = weekScale(costs, week);
  // No figure reads in a week whose tier the cost model does not size, so the dash names that week, not the plan.
  if (costs !== null && scale === null) return { kind: 'unpriced', text: '-', title: weekUnpricedTitle(week) };
  const weight = capacityWeight(pairOf(account, reset), window, costs);
  if (weight === null || scale === null) return { kind: 'unpriced', text: '-', title: ALLOWANCE_UNPRICED_TITLE };
  const figure = share * weight * scale;
  return { kind: 'equivalent', share: figure, text: weekShort(figure, week) };
}

/**
 * A window's four weeks' figure: each reset's figure at its own pair added, so a week before a tier change weighs at
 * the tier it ran under. A reset that prices nothing leaves the total unpriced, since a sum without that week would
 * count fewer weeks than its label names.
 */
export function weeksFigure(window: AllowanceWindow, account: Pick<AllowanceAccount, 'plan' | 'tier'>, costs: SubscriptionCosts | null, week: WeekPosition): AllowanceFigure {
  const { resets, total } = newestResets(window);
  if (resets.length === 0) return allowanceFigure(total, window.window, account, costs, week);
  let sum = 0;
  for (const reset of resets) {
    const figure = allowanceFigure(reset.shareLeft, window.window, account, costs, week, reset);
    if (figure.kind !== 'equivalent') return figure;
    sum += figure.share;
  }
  return { kind: 'equivalent', share: sum, text: weekShort(sum, week) };
}

/** The block's subtitle, which the week control's position leaves alike. */
export const ALLOWANCE_SUBTITLE = 'Each weekly window left unused when it reset, as a share of the chosen week, the last four weeks';

/** The account's weekly windows in the console's order, the 7-day window first and then each model week, a week the
 *  console does not know after the known ones in the answer's order. */
export function allowanceWindows(account: AllowanceAccount): AllowanceWindow[] {
  return [...account.windows].sort((a, b) => windowRank(a.window) - windowRank(b.window));
}

/** The weekly windows the Analytics card draws a pair of columns for: the 7-day window and the Fable week always, then
 *  each further model week the accounts name, in the console's order. */
export function weeklyKeys(accounts: AllowanceAccount[]): string[] {
  const keys = ['seven_day', 'seven_day_fable'];
  for (const a of accounts) for (const w of a.windows) if (!keys.includes(w.window)) keys.push(w.window);
  return keys.sort((a, b) => windowRank(a) - windowRank(b));
}

/** A window's word in the card's column heads: "7-day", "Fable", a model week the console does not know by its key. */
export const allowanceColumnWord = (window: string): string => (window === 'seven_day_fable' ? 'Fable' : (lookup(WINDOW_LABEL, windowKeyId(window) ?? '') ?? window));

/** The block's own words for where a reading came from, since the routing log's `window reset` panel names none. */
const SOURCE_WORD = new Map([
  ['clock', 'the clock walk'],
  ['sample', 'a usage sample'],
  ['term_ended', "the term's end"],
]);

/** A reset's hover: its absolute instant and where its reading came from, a word the router adds read as sent. */
export const resetHover = (reset: AllowanceReset): string => `${fmtAbsolute(reset.resetAt)}, read from ${SOURCE_WORD.get(reset.source) ?? reset.source}`;

/** A reset's own share for the week's hover, in the hint's words: "Ultra: 39% of the 7-day window left", "Max: 12% of
 *  the Fable week left", a week the console does not know under its key. */
export const ownShareHover = (plan: string, window: string, shareLeft: number): string => `${plan}: ${fmtShare(shareLeft)} of ${lookup(HINT_WINDOW, window) ?? window} left`;

/** A reset's day, "Sep 12": a week's reset lies days back, so the day is always written. */
export const resetDate = (isoTime: string): string => new Date(isoTime).toLocaleDateString('en-US', { day: 'numeric', month: 'short' });

/** A reset's day and time, "Sep 12 20:10", the day written even within a day of now. */
export const resetInstant = (isoTime: string): string => `${resetDate(isoTime)} ${fmtShortTime(isoTime)}`;

/** One account of the Analytics card: each weekly window's last and four weeks' figures with the resets counted,
 *  keyed by the wire key, the 7-day window's count, the instant of a latest reset that is the term's end, and whether
 *  one of the newest 7-day weeks prices no capacity at the pair it ran under, the account's own pair standing in where
 *  it holds no 7-day reset. */
export interface AllowanceRow {
  accountId: string;
  figures: Record<string, { counted: number; last: AllowanceFigure; weeks: AllowanceFigure }>;
  resets: number;
  termEndedAt: string | null;
  unpriced: boolean;
}

/** A figure's value to rank and add by, none for a dash. */
const valueOf = (figure: AllowanceFigure | undefined): number | null => (figure === undefined || figure.kind === 'none' || figure.kind === 'unpriced' ? null : figure.share);

/** One row of the Analytics card per account: its resets count, each weekly window's last and four weeks' figures, the
 *  term-ended mark of its latest reset where one is, sorted with an unpriced account last, then most left first on the
 *  four weeks' 7-day figure in the chosen week, then the Fable figure, then the id. */
export function allowanceRows(answer: UnusedAllowanceAnswer, costs: SubscriptionCosts | null, week: WeekPosition): AllowanceRow[] {
  const rows = answer.accounts.map((account): AllowanceRow => {
    const figures: AllowanceRow['figures'] = {};
    let termEndedAt: string | null = null;
    for (const w of allowanceWindows(account)) {
      const { counted, resets } = newestResets(w);
      if (resets.length === 0) continue;
      figures[w.window] = { counted, last: allowanceFigure(resets[0].shareLeft, w.window, account, costs, week, resets[0]), weeks: weeksFigure(w, account, costs, week) };
      if (termEndedAt === null && resets[0].source === 'term_ended') termEndedAt = resets[0].resetAt;
    }
    // Read off each newest 7-day week's own pair, so the fleet's note names every account the sum leaves out.
    const sevenDay = account.windows.find((w) => w.window === 'seven_day');
    const pairs = sevenDay === undefined || sevenDay.resets.length === 0 ? [account] : newestResets(sevenDay).resets.map((r) => pairOf(account, r));
    const unpriced = pairs.some((pair) => capacityWeight(pair, 'seven_day', costs) === null);
    return { accountId: account.accountId, figures, resets: figures.seven_day?.counted ?? 0, termEndedAt, unpriced };
  });
  const lead = (r: AllowanceRow, window: string) => valueOf(r.figures[window]?.weeks) ?? Number.NEGATIVE_INFINITY;
  // A dash ranks under every figure, so an equal pair of dashes compares as a tie rather than as NaN.
  const by = (a: number, b: number) => (a === b ? 0 : b - a);
  return rows.sort(
    (a, b) =>
      Number(a.unpriced) - Number(b.unpriced) ||
      by(lead(a, 'seven_day'), lead(b, 'seven_day')) ||
      by(lead(a, 'seven_day_fable'), lead(b, 'seven_day_fable')) ||
      byteOrder(a.accountId, b.accountId),
  );
}


/** The two fleet figures: the priced accounts' figures in the chosen week added, with the unpriced accounts named, and
 *  where the week's `scale` is null, every figure a dash, the note naming the week the cost model does not size. */
export function fleetFigures(rows: AllowanceRow[], week: WeekPosition, scale: number | null): { fable: string; note: string; sevenDay: string } {
  const sum = (window: string) => {
    const values = rows.map((r) => valueOf(r.figures[window]?.weeks)).filter((v): v is number => v !== null);
    return values.length === 0 ? '-' : weekShort(values.reduce((a, b) => a + b, 0), week);
  };
  // The rows' flags read each account's own pair, which would count accounts as priced over a column of dashes.
  if (scale === null) return { fable: sum('seven_day_fable'), note: weekUnpricedTitle(week), sevenDay: sum('seven_day') };
  const priced = rows.filter((r) => !r.unpriced).length;
  const unpriced = rows
    .filter((r) => r.unpriced)
    .map((r) => r.accountId)
    .sort();
  return { fable: sum('seven_day_fable'), note: `${plural(priced, 'account')} priced${unpriced.length === 0 ? '' : `, ${unpriced.join(', ')} left out unpriced`}`, sevenDay: sum('seven_day') };
}
